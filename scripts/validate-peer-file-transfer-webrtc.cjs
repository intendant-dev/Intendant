#!/usr/bin/env node
'use strict';

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn, spawnSync } = require('child_process');
const { httpStatus, launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_PRIMARY_PORT = 18965;
const DEFAULT_PEER_PORT = 18966;
const START_TIMEOUT_MS = 45000;
const PEER_CONNECT_TIMEOUT_MS = 60000;
const BROWSER_TIMEOUT_MS = 90000;

function usage() {
  console.log(`Usage:
  node scripts/validate-peer-file-transfer-webrtc.cjs [options]

Options:
  --binary <path>          Intendant binary to launch. Default: target/release/intendant
  --primary-port <port>    Primary daemon dashboard port. Default: ${DEFAULT_PRIMARY_PORT}
  --peer-port <port>       Peer daemon dashboard port. Default: ${DEFAULT_PEER_PORT}
  --keep-temp              Keep the isolated test directory after completion.

Environment:
  PLAYWRIGHT_NODE_PATH     Optional node_modules directory containing playwright.
  CHROME_PATH/CHROME_BIN   Optional Chromium executable for the CDP fallback.
`);
}

function parseArgs(argv) {
  const repoRoot = path.resolve(__dirname, '..');
  const options = {
    repoRoot,
    binary: path.join(repoRoot, 'target', 'release', 'intendant'),
    primaryPort: DEFAULT_PRIMARY_PORT,
    peerPort: DEFAULT_PEER_PORT,
    keepTemp: false,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') {
      usage();
      process.exit(0);
    }
    if (arg === '--binary' || arg === '--dashboard-binary') {
      options.binary = path.resolve(argv[++i]);
      continue;
    }
    if (arg === '--primary-port') {
      options.primaryPort = Number(argv[++i]);
      continue;
    }
    if (arg === '--peer-port') {
      options.peerPort = Number(argv[++i]);
      continue;
    }
    if (arg === '--keep-temp') {
      options.keepTemp = true;
      continue;
    }
    throw new Error(`unknown argument: ${arg}`);
  }
  assert(Number.isInteger(options.primaryPort) && options.primaryPort > 0, 'invalid primary port');
  assert(Number.isInteger(options.peerPort) && options.peerPort > 0, 'invalid peer port');
  assert.notStrictEqual(options.primaryPort, options.peerPort, 'primary and peer ports must differ');
  if (!fs.existsSync(options.binary)) {
    throw new Error(`intendant binary not found: ${options.binary}`);
  }
  return options;
}

function delay(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

function trimLog(text, max = 30000) {
  if (text.length <= max) return text;
  return text.slice(text.length - max);
}

async function waitFor(predicate, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  let lastValue = null;
  while (Date.now() < deadline) {
    try {
      lastValue = await predicate();
      if (lastValue) return lastValue;
    } catch (err) {
      lastError = err;
    }
    await delay(250);
  }
  const suffix = lastError
    ? `: ${lastError.message || lastError}`
    : lastValue
      ? `: last=${JSON.stringify(lastValue)}`
      : '';
  throw new Error(`timed out waiting for ${label}${suffix}`);
}

function runChecked(binary, args, opts) {
  const result = spawnSync(binary, args, {
    cwd: opts.cwd,
    env: opts.env,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
    timeout: opts.timeoutMs || 30000,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(
      `${opts.label || args.join(' ')} exited ${result.status}\n` +
      trimLog(result.stdout || '', 12000) +
      trimLog(result.stderr || '', 12000)
    );
  }
  return {
    stdout: result.stdout || '',
    stderr: result.stderr || '',
  };
}

function spawnDaemon(binary, args, opts) {
  const logs = [];
  const child = spawn(binary, args, {
    cwd: opts.cwd,
    env: opts.env,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  child.stdout.on('data', chunk => logs.push(`[stdout] ${chunk.toString()}`));
  child.stderr.on('data', chunk => logs.push(`[stderr] ${chunk.toString()}`));
  child.once('error', err => logs.push(`[error] ${err.message || err}`));
  child.logs = () => trimLog(logs.join(''), 30000);
  return child;
}

async function stopDaemon(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise(resolve => child.once('exit', resolve));
  child.kill('SIGTERM');
  await Promise.race([exited, delay(3000)]).catch(() => {});
  if (child.exitCode === null && child.signalCode === null) {
    child.kill('SIGKILL');
    await Promise.race([exited, delay(1000)]).catch(() => {});
  }
}

function testEnv(home) {
  return {
    ...process.env,
    HOME: home,
    NO_COLOR: '1',
    RUST_BACKTRACE: process.env.RUST_BACKTRACE || '1',
  };
}

function localNonLoopbackIpv4() {
  const interfaces = os.networkInterfaces();
  for (const entries of Object.values(interfaces)) {
    for (const entry of entries || []) {
      if (entry && (entry.family === 'IPv4' || entry.family === 4) && !entry.internal && entry.address) {
        return entry.address;
      }
    }
  }
  return '';
}

function setPeerBrowserTcpViaUrl(projectDir, url) {
  const configPath = path.join(projectDir, 'intendant.toml');
  const text = fs.readFileSync(configPath, 'utf8');
  if (/browser_tcp_via_url\s*=/.test(text)) {
    fs.writeFileSync(
      configPath,
      text.replace(/browser_tcp_via_url\s*=\s*"[^"]*"/, `browser_tcp_via_url = ${JSON.stringify(url)}`)
    );
    return configPath;
  }
  const updated = text.replace(
    /(\[\[peer\]\][\s\S]*?card_url\s*=\s*"[^"]*"\s*)/,
    `$1browser_tcp_via_url = ${JSON.stringify(url)}\n`
  );
  assert.notStrictEqual(updated, text, `failed to inject browser_tcp_via_url into ${configPath}`);
  fs.writeFileSync(configPath, updated);
  return configPath;
}

function createFixture(root) {
  const dir = path.join(root, 'peer-files');
  fs.mkdirSync(dir, { recursive: true });
  const chunks = [];
  for (let i = 0; i < 640; i += 1) {
    chunks.push(`peer direct file transfer fixture line ${String(i).padStart(4, '0')}`);
  }
  const text = `${chunks.join('\n')}\n`;
  const filePath = path.join(dir, 'direct-peer-download.txt');
  fs.writeFileSync(filePath, text);
  return { dir, filePath, text };
}

function extractInvite(stdout) {
  const match = String(stdout || '').match(/intendant-peer-v1\.[A-Za-z0-9_-]+/);
  if (!match) {
    throw new Error(`could not find invite in peer invite output:\n${stdout}`);
  }
  return match[0];
}

function grantPeerReadRoot(peerHome, readRoot) {
  const identitiesDir = path.join(peerHome, '.intendant', 'access-certs', 'peer-access-identities');
  const files = fs.existsSync(identitiesDir)
    ? fs.readdirSync(identitiesDir).filter(name => name.endsWith('.json'))
    : [];
  assert.strictEqual(files.length, 1, `expected one peer identity in ${identitiesDir}, found ${files.length}`);
  const identityPath = path.join(identitiesDir, files[0]);
  const record = JSON.parse(fs.readFileSync(identityPath, 'utf8'));
  assert.strictEqual(record.status, 'approved', 'test peer identity is not approved');
  record.filesystem = {
    read_roots: [readRoot],
    write_roots: [],
  };
  fs.writeFileSync(identityPath, `${JSON.stringify(record, null, 2)}\n`);
  return {
    identityPath,
    fingerprint: record.fingerprint,
    label: record.label,
    profile: record.profile,
  };
}

function connectedPeerFromLog(home, label) {
  const logsRoot = path.join(home, '.intendant', 'logs');
  if (!fs.existsSync(logsRoot)) return null;
  const dirs = fs.readdirSync(logsRoot)
    .map(name => path.join(logsRoot, name))
    .filter(dir => fs.existsSync(path.join(dir, 'peers.jsonl')));
  for (const dir of dirs) {
    const file = path.join(dir, 'peers.jsonl');
    const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/).filter(Boolean);
    for (const line of lines) {
      let event;
      try {
        event = JSON.parse(line);
      } catch (_) {
        continue;
      }
      const card = event?.payload?.card || {};
      if (
        event?.payload?.event === 'connected' &&
        String(card.label || '').includes(label)
      ) {
        return {
          peer: String(event.peer || ''),
          label: String(card.label || ''),
          id: String(card.id || ''),
          log: file,
        };
      }
    }
  }
  return null;
}

async function waitForBrowserReady(page) {
  await page.waitForFunction(() => (
    Boolean(window.intendantDashboardControl) &&
    Boolean(window.intendantDashboardFiles) &&
    typeof window.intendantDashboardFiles._debugProbePeerDownloadPath === 'function'
  ), { timeout: BROWSER_TIMEOUT_MS });
}

async function waitForPeerInBrowser(page, label) {
  const deadline = Date.now() + PEER_CONNECT_TIMEOUT_MS;
  let last = null;
  while (Date.now() < deadline) {
    last = await page.evaluate(`(async () => {
      const status = window.intendantDashboardControl?.status?.() || null;
      try {
        if (!window.intendantDashboardFiles || typeof window.intendantDashboardFiles._debugRefreshPeerList !== 'function') {
          return {
            status,
            error: 'peer debug helper is unavailable',
            peers: [],
          };
        }
        const peers = await window.intendantDashboardFiles._debugRefreshPeerList();
        const match = peers.find(peer => String(peer.label || '').includes(${JSON.stringify(label)})) || peers[0] || null;
        return { status, peers, match };
      } catch (err) {
        return {
          status,
          error: err?.message || String(err),
          peers: [],
        };
      }
    })()`).catch(err => ({ error: err?.message || String(err) }));
    if (last?.match?.id && last.match.connected !== false) return last.match;
    await page.waitForTimeout(250);
  }
  throw new Error(`timed out waiting for connected peer in browser: ${JSON.stringify(last)}`);
}

async function probePeerDownload(page, peer, fixture) {
  await page.evaluate(`window.__intendantPeerTransferFixture = ${JSON.stringify({
    peerId: peer.id,
    peerLabel: peer.label,
    filePath: fixture.filePath,
    expectedText: fixture.text,
  })}`);
  return page.evaluate(`(async () => {
    const fixture = window.__intendantPeerTransferFixture;
    const result = await window.intendantDashboardFiles._debugProbePeerDownloadPath(
      fixture.peerId,
      fixture.filePath,
      {
        peerLabel: fixture.peerLabel,
        chunkBytes: 2048,
        maxBytes: 1024 * 1024,
      }
    );
    return {
      ok: result.ok === true,
      peerId: result.peerId,
      path: result.path,
      filename: result.filename,
      size: result.size,
      totalSize: result.totalSize,
      rangeCount: result.rangeCount,
      textMatches: result.text === fixture.expectedText,
      textLength: result.text.length,
      statusText: result.statusText,
      transfer: result.transfer,
      progressCount: Array.isArray(result.progress) ? result.progress.length : 0,
    };
  })()`);
}

async function main() {
  const options = parseArgs(process.argv);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-peer-file-transfer-'));
  const primaryHome = path.join(root, 'primary-home');
  const peerHome = path.join(root, 'peer-home');
  const primaryProject = path.join(root, 'primary-project');
  const peerProject = path.join(root, 'peer-project');
  fs.mkdirSync(primaryHome, { recursive: true });
  fs.mkdirSync(peerHome, { recursive: true });
  fs.mkdirSync(primaryProject, { recursive: true });
  fs.mkdirSync(peerProject, { recursive: true });

  const primaryEnv = testEnv(primaryHome);
  const peerEnv = testEnv(peerHome);
  const browserTcpHost = localNonLoopbackIpv4();
  if (!browserTcpHost) {
    throw new Error('no non-loopback IPv4 address found for browser-to-peer ICE-TCP smoke test');
  }
  const primaryOrigin = `https://127.0.0.1:${options.primaryPort}`;
  const peerOrigin = `https://127.0.0.1:${options.peerPort}`;
  const peerBrowserTcpViaUrl = `wss://${browserTcpHost}:${options.peerPort}/ws`;
  const peerCardUrl = `${peerOrigin}/.well-known/agent-card.json`;
  const fixture = createFixture(root);
  let primary = null;
  let peer = null;
  let browser = null;

  try {
    runChecked(options.binary, [
      'access',
      'setup',
      '--name',
      'e2e-primary',
      '--ip',
      '127.0.0.1',
      '--host',
      'localhost',
      '--port',
      String(options.primaryPort),
      '--no-serve-certs',
      '--force',
    ], {
      cwd: primaryProject,
      env: primaryEnv,
      label: 'primary access setup',
    });
    runChecked(options.binary, [
      'access',
      'setup',
      '--name',
      'e2e-peer',
      '--ip',
      '127.0.0.1',
      '--host',
      'localhost',
      '--port',
      String(options.peerPort),
      '--no-serve-certs',
      '--force',
    ], {
      cwd: peerProject,
      env: peerEnv,
      label: 'peer access setup',
    });

    const inviteOutput = runChecked(options.binary, [
      'peer',
      'invite',
      '--card-url',
      peerCardUrl,
      '--label',
      'e2e-peer',
      '--client-name',
      'e2e-primary',
    ], {
      cwd: peerProject,
      env: peerEnv,
      label: 'peer invite',
    });
    const invite = extractInvite(inviteOutput.stdout);
    runChecked(options.binary, [
      'peer',
      'join',
      invite,
      '--label',
      'e2e-peer',
    ], {
      cwd: primaryProject,
      env: primaryEnv,
      label: 'primary peer join',
    });
    const primaryConfigPath = setPeerBrowserTcpViaUrl(primaryProject, peerBrowserTcpViaUrl);
    const grant = grantPeerReadRoot(peerHome, fixture.dir);

    peer = spawnDaemon(options.binary, [
      '--no-tui',
      '--mtls',
      '--bind',
      '0.0.0.0',
      '--web',
      String(options.peerPort),
      '--advertise-url',
      `wss://127.0.0.1:${options.peerPort}/ws`,
    ], {
      cwd: peerProject,
      env: peerEnv,
    });
    await waitFor(async () => {
      const status = await httpStatus(peerCardUrl, {
        ignoreHTTPSErrors: true,
        timeoutMs: 2000,
      });
      return status > 0 ? status : null;
    }, START_TIMEOUT_MS, 'peer HTTPS listener');

    primary = spawnDaemon(options.binary, [
      '--no-tui',
      '--tls',
      '--bind',
      '127.0.0.1',
      '--web',
      String(options.primaryPort),
      '--advertise-url',
      `wss://127.0.0.1:${options.primaryPort}/ws`,
    ], {
      cwd: primaryProject,
      env: primaryEnv,
    });
    await waitFor(async () => {
      const status = await httpStatus(`${primaryOrigin}/config`, {
        ignoreHTTPSErrors: true,
        timeoutMs: 2000,
      });
      return status === 200 ? status : null;
    }, START_TIMEOUT_MS, 'primary dashboard config');
    await waitFor(async () => {
      return connectedPeerFromLog(primaryHome, 'e2e-peer');
    }, PEER_CONNECT_TIMEOUT_MS, 'primary peer registry connection');

    browser = await launchBrowser({
      headless: true,
      ignoreHTTPSErrors: true,
      browserArgs: [
        '--disable-features=WebRtcHideLocalIpsWithMdns',
        '--force-webrtc-ip-handling-policy=default_public_and_private_interfaces',
        '--allow-loopback-in-peer-connection',
      ],
    });
    const page = await browser.newPage();
    page.on('console', msg => console.log(`[browser:${msg.type()}] ${msg.text()}`));
    const response = await page.goto(`${primaryOrigin}/`, {
      waitUntil: 'domcontentloaded',
      timeout: BROWSER_TIMEOUT_MS,
    });
    assert(response, 'dashboard produced no response');
    assert.strictEqual(response.status(), 200, `dashboard returned ${response.status()}`);
    await waitForBrowserReady(page);
    const browserPeer = await waitForPeerInBrowser(page, 'e2e-peer');
    const result = await probePeerDownload(page, browserPeer, fixture);

    assert.strictEqual(result.ok, true, 'peer download probe did not report ok');
    assert.strictEqual(result.textMatches, true, 'downloaded text did not match peer fixture');
    assert.strictEqual(result.size, Buffer.byteLength(fixture.text), 'downloaded size mismatch');
    assert(result.rangeCount > 1, `expected ranged download, got rangeCount=${result.rangeCount}`);
    assert(result.transfer && result.transfer.peerId === browserPeer.id, 'transfer snapshot did not preserve peer id');

    console.log(JSON.stringify({
      ok: true,
      browser: browser.kind,
      primaryOrigin,
      peerOrigin,
      peerBrowserTcpViaUrl,
      peer: browserPeer,
      grant: {
        primaryConfigPath,
        identityPath: grant.identityPath,
        fingerprint: grant.fingerprint,
        label: grant.label,
        profile: grant.profile,
        readRoot: fixture.dir,
      },
      download: {
        filename: result.filename,
        bytes: result.size,
        rangeCount: result.rangeCount,
        progressCount: result.progressCount,
        statusText: result.statusText,
      },
    }, null, 2));
  } catch (err) {
    if (primary) {
      console.error('\n--- primary daemon log ---');
      console.error(primary.logs());
    }
    if (peer) {
      console.error('\n--- peer daemon log ---');
      console.error(peer.logs());
    }
    throw err;
  } finally {
    if (browser) await browser.close().catch(() => {});
    await stopDaemon(primary);
    await stopDaemon(peer);
    if (options.keepTemp) {
      console.log(`kept temp root: ${root}`);
    } else {
      fs.rmSync(root, { recursive: true, force: true });
    }
  }
}

main().catch(err => {
  console.error(err && err.stack || err);
  process.exit(1);
});

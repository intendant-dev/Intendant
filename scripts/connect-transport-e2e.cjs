#!/usr/bin/env node
'use strict';

// Hosted-Connect transport E2E: asserts the OUTCOME the claim ceremony
// exists for — a browser that signed up at the rendezvous, claimed a fresh
// daemon with its twelve-word bootstrap phrase, and opened the hosted
// dashboard actually gets an OPEN dashboard-control DataChannel to the
// daemon. The fresh-VPS ceremony validation stopped at enrollment, and
// three real transport bugs survived it undetected:
//
//   1. the reverse proxy dropped X-Forwarded-For, so the register echo's
//      observed_ip was null and the daemon never learned its public address;
//   2. the daemon then advertised no ICE-TCP candidate at all;
//   3. the WebRTC engine stamped DTLS/SCTP transmits as UDP, the IO glue
//      misrouted them off the nominated TCP pair, and DTLS timed out at 30s.
//
// This rig reproduces the topology locally with no cloud resources and no
// real accounts:
//
//   browser (headless Chromium) ── http ──> intendant-connect (127.0.0.1)
//   daemon ── http ──> XFF-injecting forward proxy ──> intendant-connect
//   browser ── WebRTC (ICE-TCP + DTLS + SCTP) ──> daemon gateway port
//
// The proxy plays the production reverse proxy: it stamps every daemon->
// service request with `X-Forwarded-For: <this machine's LAN IP>`, so the
// register echo carries a non-loopback observed_ip and the daemon
// advertises `<LAN-IP>:<gateway-port>` as its ICE-TCP candidate — locally
// reachable, so the browser can really dial it. (A daemon polling the
// service directly on 127.0.0.1 gets observed_ip=null and never advertises
// the candidate: exactly the bug class under test, and untestable without
// the proxy.)
//
// Stages (each printed, each asserted):
//   1. service + proxy + fresh-HOME daemon come up; register echo carries
//      observed_ip == LAN IP (bug class 1);
//   2. headless Chromium creates a passkey account (CDP virtual
//      authenticator), enters the daemon-minted phrase, and the first-owner
//      bootstrap enrolls the browser key as role:root;
//   3. hosted /app dashboard connects: control DataChannel OPEN, binding
//      verified, status RPC answered (grant, not refusal);
//   4. TCP-forced pass: a fresh /app load with every UDP candidate stripped
//      from the daemon's answer (what a cloud box's filtered network does),
//      so ICE must nominate the advertised ICE-TCP candidate and DTLS+SCTP
//      must flow over it (bug classes 2 and 3). Asserts the selected ICE
//      pair is protocol=tcp at <LAN-IP>:<gateway-port> and the daemon logs
//      a second "data channel open".
//
// Prerequisites (this script builds nothing):
//   cargo build --bin intendant --bin intendant-runtime --bin intendant-connect
//   plus a Chromium (Playwright's, or CHROME_PATH/CHROME_BIN, or a system
//   install — see scripts/lib/browser-automation.cjs).
//
// Not in CI: needs a Chromium and a routable (non-loopback) local IP. Part
// of the operator battery next to validate-connect-rendezvous.cjs.

const assert = require('assert');
const fs = require('fs');
const http = require('http');
const net = require('net');
const os = require('os');
const path = require('path');
const { spawn, spawnSync } = require('child_process');
const { launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_SERVICE_PORT = 9891;
const DEFAULT_PROXY_PORT = 9892;
const DEFAULT_DAEMON_PORT = 8891;
const DEFAULT_DAEMON_ID = 'connect-transport-e2e';
const START_TIMEOUT_MS = 60000;
const CLAIM_TIMEOUT_MS = 75000;
const TRANSPORT_TIMEOUT_MS = 45000;

function usage() {
  console.log(`Usage:
  node scripts/connect-transport-e2e.cjs [options]

Options:
  --connect-binary <path>   intendant-connect binary. Default target/debug/intendant-connect.
  --daemon-binary <path>    intendant daemon binary. Default target/debug/intendant.
  --release                 Use target/release binaries instead of target/debug.
  --service-port <port>     intendant-connect listen port. Default ${DEFAULT_SERVICE_PORT}.
  --proxy-port <port>       XFF forward-proxy port (daemon's rendezvous). Default ${DEFAULT_PROXY_PORT}.
  --daemon-port <port>      Daemon web/gateway port (also the ICE-TCP port). Default ${DEFAULT_DAEMON_PORT}.
  --daemon-id <id>          Connect daemon id. Default ${DEFAULT_DAEMON_ID}.
  --lan-ip <ip>             Address to inject as X-Forwarded-For (must be a
                            reachable local interface address). Default: auto-detect.
  --keep                    Keep the scratch dir on success too.
  --help                    This message.

Binaries are NOT built by this script:
  cargo build --bin intendant --bin intendant-runtime --bin intendant-connect
`);
}

function parseArgs(argv) {
  const repoRoot = path.resolve(__dirname, '..');
  const out = {
    repoRoot,
    profile: 'debug',
    connectBinary: null,
    daemonBinary: null,
    servicePort: DEFAULT_SERVICE_PORT,
    proxyPort: DEFAULT_PROXY_PORT,
    daemonPort: DEFAULT_DAEMON_PORT,
    daemonId: DEFAULT_DAEMON_ID,
    lanIp: null,
    keep: false,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--connect-binary') out.connectBinary = path.resolve(argv[++i]);
    else if (arg === '--daemon-binary') out.daemonBinary = path.resolve(argv[++i]);
    else if (arg === '--release') out.profile = 'release';
    else if (arg === '--service-port') out.servicePort = Number(argv[++i]);
    else if (arg === '--proxy-port') out.proxyPort = Number(argv[++i]);
    else if (arg === '--daemon-port') out.daemonPort = Number(argv[++i]);
    else if (arg === '--daemon-id') out.daemonId = String(argv[++i] || '').trim();
    else if (arg === '--lan-ip') out.lanIp = String(argv[++i] || '').trim();
    else if (arg === '--keep') out.keep = true;
    else if (arg === '--help' || arg === '-h') {
      usage();
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  if (!out.connectBinary) {
    out.connectBinary = path.join(repoRoot, 'target', out.profile, 'intendant-connect');
  }
  if (!out.daemonBinary) {
    out.daemonBinary = path.join(repoRoot, 'target', out.profile, 'intendant');
  }
  for (const port of [out.servicePort, out.proxyPort, out.daemonPort]) {
    assert(Number.isInteger(port) && port > 0 && port < 65536, `invalid port ${port}`);
  }
  assert(out.daemonId, 'daemon id is required');
  return out;
}

function stage(name, detail) {
  console.log(`[stage] ${name}${detail ? `: ${detail}` : ''}`);
}

/// First non-internal, non-link-local IPv4 on this machine — the address
/// the proxy injects as X-Forwarded-For, and therefore the address the
/// daemon advertises as its ICE-TCP candidate. Must be locally dialable.
function detectLanIp() {
  const candidates = [];
  for (const [name, addrs] of Object.entries(os.networkInterfaces())) {
    for (const addr of addrs || []) {
      if (addr.internal) continue;
      if (addr.family !== 'IPv4' && addr.family !== 4) continue;
      if (addr.address.startsWith('169.254.')) continue;
      candidates.push({ name, address: addr.address });
    }
  }
  // Prefer conventional primary interfaces over tunnels/bridges.
  const score = ({ name }) => {
    if (/^(en0|eth0|wlan0)$/.test(name)) return 0;
    if (/^(en|eth|wlan|wl)/.test(name)) return 1;
    if (/^(utun|tun|tap|bridge|vnic|docker|veth|awdl|llw)/.test(name)) return 3;
    return 2;
  };
  candidates.sort((a, b) => score(a) - score(b));
  return candidates.length ? candidates[0].address : null;
}

async function waitFor(fn, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (err) {
      if (err && err.fatal) throw err;
      lastError = err;
    }
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  throw new Error(`timed out waiting for ${label}${lastError ? `: ${lastError.message}` : ''}`);
}

async function fetchJson(url, options = {}) {
  const resp = await fetch(url, options);
  const body = await resp.json().catch(() => ({}));
  if (!resp.ok || body.ok === false) {
    throw new Error(`${url} returned ${resp.status}: ${body.error || JSON.stringify(body)}`);
  }
  return body;
}

async function httpStatus(url) {
  const resp = await fetch(url).catch(() => null);
  return resp ? resp.status : 0;
}

function portIsFree(port) {
  return new Promise(resolve => {
    const probe = net
      .createServer()
      .once('error', () => resolve(false))
      .once('listening', () => probe.close(() => resolve(true)));
    probe.listen(port, '0.0.0.0');
  });
}

function tcpDialOk(host, port, timeoutMs = 3000) {
  return new Promise(resolve => {
    const sock = net.connect({ host, port });
    const done = ok => {
      sock.destroy();
      resolve(ok);
    };
    sock.setTimeout(timeoutMs, () => done(false));
    sock.once('connect', () => done(true));
    sock.once('error', () => done(false));
  });
}

/// The ~30-line reverse-proxy stand-in: forward every request to the
/// service, stamping X-Forwarded-For with the LAN IP, and capture the
/// /api/daemon/register response body so the driver can assert the
/// observed_ip echo (bug class 1) without touching product code.
function startXffProxy(servicePort, lanIp, registerEchoes) {
  const server = http.createServer((req, res) => {
    const headers = { ...req.headers };
    delete headers['accept-encoding']; // keep register responses parseable
    headers.host = `127.0.0.1:${servicePort}`;
    headers['x-forwarded-for'] = lanIp;
    const upstream = http.request(
      { host: '127.0.0.1', port: servicePort, path: req.url, method: req.method, headers },
      upstreamRes => {
        const captured = [];
        const capture = String(req.url || '').startsWith('/api/daemon/register');
        res.writeHead(upstreamRes.statusCode || 502, upstreamRes.headers);
        upstreamRes.on('data', chunk => {
          if (capture) captured.push(chunk);
          res.write(chunk);
        });
        upstreamRes.on('end', () => {
          res.end();
          if (capture) {
            try {
              registerEchoes.push(JSON.parse(Buffer.concat(captured).toString('utf8')));
            } catch (_) {
              registerEchoes.push({ unparseable: true });
            }
          }
        });
      }
    );
    upstream.on('error', err => {
      res.writeHead(502, { 'content-type': 'text/plain' });
      res.end(`xff proxy upstream error: ${err.message}`);
    });
    req.pipe(upstream);
  });
  // The daemon long-polls /api/daemon/next; do not reap those sockets.
  server.requestTimeout = 0;
  server.headersTimeout = 60000;
  return server;
}

function prepareDaemonAccessCerts(binary, homeDir, repoRoot, label) {
  const result = spawnSync(
    binary,
    ['access', 'setup', '--no-serve-certs', '--force', '--name', label, '--ip', '127.0.0.1', '--host', 'localhost'],
    {
      cwd: repoRoot,
      env: { ...process.env, HOME: homeDir, USERPROFILE: homeDir },
      encoding: 'utf8',
    }
  );
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`access setup failed: ${result.stderr || result.stdout || `exit ${result.status}`}`);
  }
}

async function addVirtualAuthenticator(browser, page) {
  const options = {
    protocol: 'ctap2',
    transport: 'internal',
    hasResidentKey: true,
    hasUserVerification: true,
    isUserVerified: true,
    automaticPresenceSimulation: true,
  };
  if (browser.kind === 'playwright' && page.context) {
    const client = await page.context().newCDPSession(page);
    await client.send('WebAuthn.enable');
    await client.send('WebAuthn.addVirtualAuthenticator', { options });
    return;
  }
  if (page.connection && page.sessionId) {
    await page.connection.send('WebAuthn.enable', {}, page.sessionId);
    await page.connection.send('WebAuthn.addVirtualAuthenticator', { options }, page.sessionId);
    return;
  }
  throw new Error('browser driver does not expose CDP WebAuthn controls');
}

/// Fault injection for the TCP-forced pass, installed before the page
/// loads: make UDP unroutable between browser and daemon, the exact
/// topology of a cloud daemon whose UDP is filtered, so ICE can only
/// nominate the daemon's advertised ICE-TCP candidate. Three legs, all
/// needed:
///   - strip UDP candidates from the daemon's answer SDP (and any remote
///     trickle) so the browser never dials daemon UDP;
///   - swallow the browser's own UDP trickle candidates before the page
///     signals them, or the daemon would send UDP checks back and the
///     browser would mint a working peer-reflexive UDP pair (observed:
///     same-host prflx quietly un-forced an earlier version of this rig);
///   - register every RTCPeerConnection so the driver can read getStats()
///     and assert which pair actually got nominated.
const TCP_ONLY_INIT_SCRIPT = `(() => {
  if (window.__rigTcpOnlyInstalled) return;
  window.__rigTcpOnlyInstalled = true;
  window.__rigStrippedUdpLines = 0;
  window.__rigDroppedRemoteUdpTrickle = 0;
  window.__rigDroppedLocalUdpTrickle = 0;
  window.__rigPCs = [];
  const udpCandidateLine = line => /^(a=)?candidate:\\S+ \\d+ udp /i.test(String(line).trim());
  const stripSdp = sdp => String(sdp)
    .split(/\\r\\n|\\n/)
    .filter(line => {
      if (udpCandidateLine(line)) {
        window.__rigStrippedUdpLines += 1;
        return false;
      }
      return true;
    })
    .join('\\r\\n');
  const NativePC = window.RTCPeerConnection;
  const origSetRemote = NativePC.prototype.setRemoteDescription;
  NativePC.prototype.setRemoteDescription = function (desc, ...rest) {
    let next = desc;
    try {
      if (desc && typeof desc.sdp === 'string') next = { type: desc.type, sdp: stripSdp(desc.sdp) };
    } catch (_) {
      next = desc;
    }
    return origSetRemote.call(this, next, ...rest);
  };
  const origAddIce = NativePC.prototype.addIceCandidate;
  NativePC.prototype.addIceCandidate = function (candidate, ...rest) {
    try {
      const line = candidate && typeof candidate.candidate === 'string' ? candidate.candidate : '';
      if (udpCandidateLine(line)) {
        window.__rigDroppedRemoteUdpTrickle += 1;
        return Promise.resolve();
      }
    } catch (_) {}
    return origAddIce.call(this, candidate, ...rest);
  };
  const accessor = Object.getOwnPropertyDescriptor(NativePC.prototype, 'onicecandidate');
  if (accessor && accessor.set) {
    Object.defineProperty(NativePC.prototype, 'onicecandidate', {
      configurable: true,
      enumerable: accessor.enumerable,
      get() {
        return accessor.get.call(this);
      },
      set(handler) {
        const wrapped = typeof handler === 'function'
          ? function (ev) {
              const line = ev && ev.candidate && typeof ev.candidate.candidate === 'string'
                ? ev.candidate.candidate
                : '';
              if (line && udpCandidateLine(line)) {
                window.__rigDroppedLocalUdpTrickle += 1;
                return undefined;
              }
              return handler.call(this, ev);
            }
          : handler;
        return accessor.set.call(this, wrapped);
      },
    });
  }
  window.RTCPeerConnection = new Proxy(NativePC, {
    construct(target, args) {
      const pc = new target(...args);
      window.__rigPCs.push(pc);
      return pc;
    },
  });
})();`;

async function addInitScript(browser, page, source) {
  if (browser.kind === 'playwright' && typeof page.addInitScript === 'function') {
    await page.addInitScript(source);
    return;
  }
  if (page.connection && page.sessionId) {
    await page.connection.send('Page.addScriptToEvaluateOnNewDocument', { source }, page.sessionId);
    return;
  }
  throw new Error('browser driver does not expose init-script injection');
}

async function click(page, selector) {
  if (typeof page.locator === 'function') {
    await page.locator(selector).click();
    return;
  }
  const point = await page.evaluate(`(() => {
    const el = document.querySelector(${JSON.stringify(selector)});
    if (!el) throw new Error('missing selector ${selector}');
    el.scrollIntoView({ block: 'center' });
    const r = el.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  })()`);
  for (const type of ['mousePressed', 'mouseReleased']) {
    await page.connection.send(
      'Input.dispatchMouseEvent',
      { type, x: point.x, y: point.y, button: 'left', clickCount: 1 },
      page.sessionId
    );
  }
}

async function goto(page, url, opts = {}) {
  const response = await page.goto(url, opts);
  if (response && response.status && response.status() >= 400) {
    throw new Error(`${url} returned ${response.status()}`);
  }
  return response;
}

/// Wait for the hosted dashboard control transport to be fully live:
/// channel OPEN, daemon binding verified, hosted signaling, and a status
/// RPC answered over the channel (grantKind is only ever set from the
/// daemon's status response — a refused session never reaches it).
async function waitForHostedTransport(page, label) {
  let last = null;
  try {
    return await waitFor(async () => {
      last = await page.evaluate(() => window.intendantDashboardControl?.status?.() || null);
      if (
        last &&
        last.connected === true &&
        last.channelState === 'open' &&
        last.signalingMode === 'connect-rendezvous' &&
        last.verifiedBinding &&
        last.verifiedBinding.ok === true &&
        last.grantKind
      ) {
        return last;
      }
      return null;
    }, TRANSPORT_TIMEOUT_MS, label);
  } catch (err) {
    throw new Error(`${err.message}; last dashboard status: ${JSON.stringify(last)}`);
  }
}

async function selectedIcePair(page) {
  return page.evaluate(async () => {
    const out = {
      found: false,
      pcCount: (window.__rigPCs || []).length,
      strippedUdpLines: window.__rigStrippedUdpLines || 0,
      droppedRemoteUdpTrickle: window.__rigDroppedRemoteUdpTrickle || 0,
      droppedLocalUdpTrickle: window.__rigDroppedLocalUdpTrickle || 0,
    };
    for (const pc of window.__rigPCs || []) {
      if (pc.connectionState !== 'connected') continue;
      const stats = await pc.getStats();
      let pair = null;
      stats.forEach(report => {
        if (report.type === 'transport' && report.selectedCandidatePairId) {
          pair = stats.get(report.selectedCandidatePairId) || pair;
        }
      });
      if (!pair) {
        stats.forEach(report => {
          if (
            report.type === 'candidate-pair' &&
            (report.selected || report.nominated) &&
            (!report.state || report.state === 'succeeded')
          ) {
            pair = report;
          }
        });
      }
      if (!pair) continue;
      const local = stats.get(pair.localCandidateId) || {};
      const remote = stats.get(pair.remoteCandidateId) || {};
      const view = c => ({
        protocol: c.protocol || '',
        address: c.address || c.ip || '',
        port: c.port ?? null,
        candidateType: c.candidateType || '',
        tcpType: c.tcpType || '',
      });
      return { ...out, found: true, local: view(local), remote: view(remote) };
    }
    return out;
  });
}

async function main() {
  const options = parseArgs(process.argv);
  for (const binary of [options.connectBinary, options.daemonBinary]) {
    if (!fs.existsSync(binary)) {
      throw new Error(
        `missing binary ${binary}; run: cargo build --bin intendant --bin intendant-runtime --bin intendant-connect`
      );
    }
  }
  const lanIp = options.lanIp || detectLanIp();
  if (!lanIp) {
    throw new Error(
      'no routable non-loopback IPv4 interface found (offline box?); pass --lan-ip <address> of a reachable local interface'
    );
  }
  for (const [label, port] of [
    ['service', options.servicePort],
    ['proxy', options.proxyPort],
    ['daemon', options.daemonPort],
  ]) {
    if (!(await portIsFree(port))) {
      throw new Error(`${label} port ${port} is already in use; pass --${label}-port`);
    }
  }
  stage('preflight', `lan_ip=${lanIp} service=${options.servicePort} proxy=${options.proxyPort} daemon=${options.daemonPort}`);

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-connect-transport-e2e-'));
  const serviceOrigin = `http://localhost:${options.servicePort}`;
  const serviceApi = `http://127.0.0.1:${options.servicePort}`;
  const serviceLogs = [];
  const daemonLogs = [];
  const registerEchoes = [];
  const children = [];
  let proxy = null;
  let browser = null;
  let failed = false;

  const daemonLog = () => daemonLogs.join('');
  const countMatches = (text, needle) => text.split(needle).length - 1;

  function spawnLogged(command, args, spawnOptions, logs) {
    const child = spawn(command, args, spawnOptions);
    children.push(child);
    child.stdout?.on('data', chunk => logs.push(String(chunk)));
    child.stderr?.on('data', chunk => logs.push(String(chunk)));
    child.once('error', err => logs.push(String((err && err.message) || err)));
    return child;
  }

  try {
    // ── Rendezvous service ──────────────────────────────────────────────
    spawnLogged(
      options.connectBinary,
      [
        '--listen', `127.0.0.1:${options.servicePort}`,
        '--origin', serviceOrigin,
        '--rp-id', 'localhost',
        '--static-root', path.join(options.repoRoot, 'static'),
        '--data-file', path.join(tmp, 'connect-state.json'),
        '--open-registration',
      ],
      { cwd: options.repoRoot, stdio: ['ignore', 'pipe', 'pipe'] },
      serviceLogs
    );
    await waitFor(async () => (await httpStatus(`${serviceApi}/healthz`)) === 200, START_TIMEOUT_MS, 'intendant-connect health');
    stage('service up', serviceOrigin);

    // ── XFF forward proxy (the daemon's rendezvous URL) ────────────────
    proxy = startXffProxy(options.servicePort, lanIp, registerEchoes);
    await new Promise((resolve, reject) => {
      proxy.once('error', reject);
      proxy.listen(options.proxyPort, '127.0.0.1', resolve);
    });
    stage('xff proxy up', `127.0.0.1:${options.proxyPort} injecting X-Forwarded-For: ${lanIp}`);

    // ── Fresh daemon: scratch HOME, empty IAM, Connect via the proxy ───
    const daemonHome = path.join(tmp, 'daemon-home');
    const daemonProject = path.join(tmp, 'daemon-project');
    fs.mkdirSync(daemonHome, { recursive: true });
    fs.mkdirSync(daemonProject, { recursive: true });
    // Minimal project marker so the daemon roots itself in the scratch dir.
    fs.writeFileSync(path.join(daemonProject, 'intendant.toml'), '');
    // Keyless: the scripted mock provider, with a trivial script (no
    // session ever starts here; the daemon just needs a valid provider).
    const mockScript = path.join(tmp, 'mock-script.json');
    fs.writeFileSync(
      mockScript,
      JSON.stringify({ model: 'mock-1', profiles: [{ match: '', steps: [] }] })
    );
    prepareDaemonAccessCerts(options.daemonBinary, daemonHome, options.repoRoot, options.daemonId);

    const daemonEnv = { ...process.env };
    for (const name of [
      'OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'MODEL_NAME',
      'PRESENCE_PROVIDER', 'PRESENCE_MODEL', 'CU_PROVIDER', 'CU_MODEL',
      'INTENDANT_HOME', 'INTENDANT_MCP_URL', 'INTENDANT_PORT', 'INTENDANT_SESSION_ID',
      'INTENDANT_MANAGED_CONTEXT', 'INTENDANT_APP_HTML_PATH',
      'HTTP_PROXY', 'HTTPS_PROXY', 'http_proxy', 'https_proxy', 'ALL_PROXY', 'all_proxy', 'NO_PROXY', 'no_proxy',
    ]) {
      delete daemonEnv[name];
    }
    Object.assign(daemonEnv, {
      HOME: daemonHome,
      USERPROFILE: daemonHome,
      PROVIDER: 'mock',
      INTENDANT_MOCK_SCRIPT: mockScript,
      INTENDANT_CONNECT_RENDEZVOUS_URL: `http://127.0.0.1:${options.proxyPort}`,
      INTENDANT_CONNECT_DAEMON_ID: options.daemonId,
      DISPLAY: ':99',
    });
    // Default bind (0.0.0.0) on purpose: the ICE-TCP candidate advertises
    // <LAN-IP>:<gateway-port>, which a loopback-only bind could not serve.
    spawnLogged(
      options.daemonBinary,
      ['--web', String(options.daemonPort), '--no-tui'],
      { cwd: daemonProject, env: daemonEnv, stdio: ['ignore', 'pipe', 'pipe'] },
      daemonLogs
    );

    await waitFor(
      () => daemonLog().includes(`[web_gateway] ICE-TCP candidates advertise port ${options.daemonPort}`),
      START_TIMEOUT_MS,
      'daemon gateway startup (ICE-TCP advertise line)'
    );
    stage('daemon gateway up', `port ${options.daemonPort}`);

    const registered = await waitFor(async () => {
      const status = await fetchJson(`${serviceApi}/api/status?daemon_id=${encodeURIComponent(options.daemonId)}`);
      return status.registered && status.daemon_public_key ? status : null;
    }, START_TIMEOUT_MS, 'daemon registration at the rendezvous');
    assert.strictEqual(registered.claimed, false, 'fresh daemon must start unclaimed');

    // Bug class 1: the register echo must carry the proxy-observed IP.
    const echo = await waitFor(
      () => registerEchoes.find(body => body && typeof body.observed_ip === 'string' && body.observed_ip) || null,
      START_TIMEOUT_MS,
      'register response echoing observed_ip'
    );
    assert.strictEqual(
      echo.observed_ip,
      lanIp,
      `register echoed observed_ip=${JSON.stringify(echo.observed_ip)}, expected the injected X-Forwarded-For ${lanIp}`
    );
    assert.strictEqual(echo.claim_code_daemon_minted, true, `fresh daemon did not mint its own claim phrase: ${JSON.stringify(echo)}`);
    stage('register echo ok', `observed_ip=${echo.observed_ip} (from X-Forwarded-For), daemon-minted claim phrase`);

    const claimPhrase = await waitFor(() => {
      const match = daemonLog().match(/first-owner bootstrap: claim this daemon at [^\s]*claim_code=([^\s"'<>]+)/);
      return match ? decodeURIComponent(match[1]) : null;
    }, START_TIMEOUT_MS, 'daemon-minted bootstrap claim phrase in the daemon log');
    stage('bootstrap phrase minted', `${claimPhrase.split('-').length} words`);

    // Sanity: the ICE-TCP candidate the daemon will advertise must be
    // dialable from this machine, or a failure below would be ambiguous.
    assert(
      await tcpDialOk(lanIp, options.daemonPort),
      `cannot dial ${lanIp}:${options.daemonPort} (local firewall?) — the advertised ICE-TCP candidate would be unreachable`
    );
    stage('gateway reachable at advertised address', `${lanIp}:${options.daemonPort}`);

    // ── Browser: account, claim, first-owner bootstrap ──────────────────
    browser = await launchBrowser({ headless: true, ignoreHTTPSErrors: true });
    const page = await browser.newPage();
    page.on('console', msg => {
      const text = msg.text();
      if (/error|warn|\[dashboard-control\]/i.test(text)) console.log(`[browser:${msg.type()}] ${text}`);
    });
    await addVirtualAuthenticator(browser, page);
    await goto(page, `${serviceOrigin}/connect`, { timeout: START_TIMEOUT_MS });

    const accountName = `transport-e2e-${Date.now().toString(36)}`;
    await page.evaluate(`document.getElementById('account').value = ${JSON.stringify(accountName)}`);
    await click(page, '#register');
    await waitFor(
      () => page.evaluate("!document.getElementById('manage').classList.contains('hidden')"),
      START_TIMEOUT_MS,
      'account registration (signed-in manage view)'
    );
    stage('account created', `@${accountName} (passkey via CDP virtual authenticator)`);

    await page.evaluate(`document.getElementById('claim-code').value = ${JSON.stringify(claimPhrase)}`);
    await click(page, '#claim');
    await waitFor(async () => {
      const text = await page.evaluate("document.getElementById('claim-status').textContent || ''");
      if (/rejected|timed out|error/i.test(text)) {
        const err = new Error(`claim failed on the page: ${text}`);
        err.fatal = true;
        throw err;
      }
      return text.includes('Claimed') && text.includes('first owner') ? text : null;
    }, CLAIM_TIMEOUT_MS, 'first-owner bootstrap claim on the /connect page');
    stage('claim ok', 'page reports first-owner bootstrap claim');

    await waitFor(
      () => daemonLog().includes('first-owner bootstrap: enrolled client key'),
      START_TIMEOUT_MS,
      'daemon-side bootstrap enrollment log'
    );
    await waitFor(
      () => daemonLog().includes('claim acknowledged — this daemon co-signed'),
      START_TIMEOUT_MS,
      'daemon-side co-signed claim log'
    );
    const enrolled = daemonLog().match(/first-owner bootstrap: enrolled client key (\S+) as role:root[^\n]*/);
    stage('bootstrap enrolled', enrolled ? enrolled[0].replace(/^.*enrolled/, 'enrolled') : 'role:root');

    const daemons = await page.evaluate(() => fetch('/api/daemons').then(r => r.json()));
    assert.strictEqual(daemons.daemons?.length, 1, `expected one claimed daemon: ${JSON.stringify(daemons)}`);
    assert.strictEqual(daemons.daemons[0].daemon_id, options.daemonId);

    // ── Pass A: hosted dashboard, unmodified network ────────────────────
    const appUrl = `${serviceOrigin}/app?connect=1&daemon_id=${encodeURIComponent(options.daemonId)}`;
    await goto(page, appUrl, { timeout: START_TIMEOUT_MS });
    const baseline = await waitForHostedTransport(page, 'hosted dashboard control transport (baseline)');
    assert.strictEqual(baseline.claimedDaemonPublicKey, registered.daemon_public_key, 'baseline binding key mismatch');
    assert(baseline.sessionGrantSha256, 'baseline session did not bind a Connect session grant');
    await waitFor(
      () => countMatches(daemonLog(), '[dashboard/control] data channel open:') >= 1,
      START_TIMEOUT_MS,
      'daemon "data channel open" log (baseline)'
    );
    stage(
      'transport ready (baseline)',
      `channel open, binding verified, grant=${baseline.grantKind}, ice=${baseline.iceCandidatePair || baseline.iceRoute || 'n/a'}`
    );

    // ── Pass B: TCP-forced — the cloud-daemon topology ──────────────────
    const page2 = await browser.newPage();
    page2.on('console', msg => {
      const text = msg.text();
      if (/error|warn|\[dashboard-control\]/i.test(text)) console.log(`[browser2:${msg.type()}] ${text}`);
    });
    await addInitScript(browser, page2, TCP_ONLY_INIT_SCRIPT);
    await goto(page2, appUrl, { timeout: START_TIMEOUT_MS });
    const forced = await waitForHostedTransport(page2, 'hosted dashboard control transport (TCP-forced)');
    assert.strictEqual(forced.claimedDaemonPublicKey, registered.daemon_public_key, 'TCP-forced binding key mismatch');

    // Bug class 2: the daemon must have advertised the observed address as
    // an ICE-TCP candidate on the gateway port.
    const iceTcpEnabled = `[dashboard/control] ICE-TCP enabled on ${lanIp}:${options.daemonPort}`;
    assert(
      daemonLog().includes(iceTcpEnabled),
      `daemon log never announced "${iceTcpEnabled}" — no ICE-TCP candidate was advertised`
    );

    const pair = await selectedIcePair(page2);
    assert(pair.strippedUdpLines > 0, `TCP-forcing never engaged (no UDP candidates stripped): ${JSON.stringify(pair)}`);
    assert(
      pair.droppedLocalUdpTrickle > 0,
      `TCP-forcing did not suppress the browser's UDP trickle (prflx would un-force the pair): ${JSON.stringify(pair)}`
    );
    assert(pair.found, `no selected ICE candidate pair on the TCP-forced connection: ${JSON.stringify(pair)}`);
    // Bug class 3: DTLS+SCTP actually flowed over the nominated TCP pair.
    assert.strictEqual(
      pair.remote.protocol,
      'tcp',
      `TCP-forced session selected a non-TCP pair: ${JSON.stringify(pair)}`
    );
    if (pair.remote.port !== null) {
      assert.strictEqual(Number(pair.remote.port), options.daemonPort, `remote ICE-TCP port mismatch: ${JSON.stringify(pair)}`);
    }
    if (pair.remote.address) {
      assert.strictEqual(pair.remote.address, lanIp, `remote ICE-TCP address mismatch: ${JSON.stringify(pair)}`);
    }
    await waitFor(
      () => countMatches(daemonLog(), '[dashboard/control] data channel open:') >= 2,
      START_TIMEOUT_MS,
      'daemon "data channel open" log (TCP-forced)'
    );
    stage(
      'channel open over ICE-TCP',
      `selected pair ${pair.local.protocol}/${pair.local.candidateType} -> ${pair.remote.protocol}/${pair.remote.candidateType} @ ${pair.remote.address || '<hidden>'}:${pair.remote.port ?? '?'}, grant=${forced.grantKind}`
    );

    console.log(JSON.stringify({
      ok: true,
      lan_ip: lanIp,
      daemon_id: options.daemonId,
      daemon_public_key: registered.daemon_public_key,
      observed_ip_echo: echo.observed_ip,
      account: accountName,
      baseline: {
        session_id: baseline.sessionId,
        grant_kind: baseline.grantKind,
        ice_candidate_pair: baseline.iceCandidatePair || null,
      },
      tcp_forced: {
        session_id: forced.sessionId,
        grant_kind: forced.grantKind,
        stripped_udp_candidate_lines: pair.strippedUdpLines,
        dropped_local_udp_trickle: pair.droppedLocalUdpTrickle,
        selected_pair: pair,
      },
      data_channel_open_logs: countMatches(daemonLog(), '[dashboard/control] data channel open:'),
    }, null, 2));
  } catch (err) {
    failed = true;
    console.error(`\nFAILED: ${(err && err.stack) || err}`);
    const tailOf = logs => logs.join('').split(/\r?\n/).filter(Boolean).slice(-40).join('\n');
    console.error(`\n--- intendant-connect log tail ---\n${tailOf(serviceLogs)}`);
    console.error(`\n--- daemon log tail ---\n${tailOf(daemonLogs)}`);
    console.error(`\nscratch dir kept for inspection: ${tmp}`);
    process.exitCode = 1;
  } finally {
    // A page mid-WebRTC-reconnect can wedge a graceful browser close;
    // bound it and fall through to child cleanup regardless.
    if (browser) {
      await Promise.race([
        browser.close().catch(() => {}),
        new Promise(resolve => setTimeout(resolve, 8000)),
      ]);
    }
    // Children first: the daemon holds a long-poll open through the proxy,
    // so a graceful proxy close would wait on it forever.
    for (const child of children.reverse()) {
      if (child.exitCode === null && !child.killed) child.kill('SIGTERM');
    }
    await new Promise(resolve => setTimeout(resolve, 500));
    for (const child of children) {
      if (child.exitCode === null && !child.killed) child.kill('SIGKILL');
    }
    if (proxy) {
      proxy.close(() => {});
      if (typeof proxy.closeAllConnections === 'function') proxy.closeAllConnections();
    }
    if (!failed && !options.keep) {
      fs.rmSync(tmp, { recursive: true, force: true });
    } else if (!failed) {
      console.log(`scratch dir kept: ${tmp}`);
    }
  }
}

main().then(() => {
  if (!process.exitCode) process.exit(0);
  process.exit(process.exitCode);
}).catch(err => {
  console.error((err && err.stack) || err);
  process.exit(1);
});

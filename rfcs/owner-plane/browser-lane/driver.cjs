#!/usr/bin/env node
'use strict';

// The browser execution lane driver (§13.2 `browser` column,
// execution-lanes-plan lane 1): builds the wasm package, serves the
// fixture page + the committed corpus over loopback HTTP, launches
// headless Chromium over raw CDP (launch/WebSocket mechanics after
// scripts/smoke-dashboard-boot.cjs — the CI-proven minimal recipe,
// no npm deps), and gates on the harness's all_green shape: EVERY
// browser-annotated vector must report clean structural layers AND
// semantics=PASS, or the driver exits 1.
//
// Honesty caveat printed on every run: family 13's §13.2 browser
// cell is the IndexedDB Txn subset; until that shim (work item 3)
// lands, f13 vectors execute the reducer's engine lanes IN-MEMORY
// inside Chromium — real wasm execution, not yet the IndexedDB
// substrate.
//
// Usage:  node driver.cjs [--dev] [--skip-build] [--browser PATH]
//                         [--timeout MS] [--artifact-dir DIR]

const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const net = require('net');
const os = require('os');
const path = require('path');
const { EventEmitter } = require('events');
const { spawn, spawnSync } = require('child_process');

const LANE_DIR = __dirname;
// LANE_VECTORS_DIR exists for NEGATIVE CONTROLS (pointing the driver
// at a deliberately corrupted corpus copy to prove the gate goes
// red); the committed corpus is the default and the CI job never
// overrides it.
const VECTORS_DIR = process.env.LANE_VECTORS_DIR || path.join(LANE_DIR, '..', 'vectors');
const DEFAULT_TIMEOUT_MS = 180000;
const CDP_READY_TIMEOUT_MS = 20000;
const POLL_INTERVAL_MS = 250;
// Every §13.2 browser-required family must appear in the manifest —
// an annotation-filter bug cannot silently shrink the run.
const REQUIRED_FAMILIES = [1, 2, 3, 4, 5, 8, 13];

const PATH_BROWSER_NAMES = [
  'chromium',
  'chromium-browser',
  'google-chrome',
  'google-chrome-stable',
  'chrome',
];
const BROWSER_ENV_OVERRIDES = ['CHROME_PATH', 'CHROME_BIN'];
const DARWIN_BROWSER_PATHS = [
  '/Applications/Chromium.app/Contents/MacOS/Chromium',
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
];

function parseArgs(argv) {
  const opts = {
    dev: false,
    skipBuild: false,
    browser: '',
    timeoutMs: DEFAULT_TIMEOUT_MS,
    artifactDir: '',
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const value = () => {
      i += 1;
      if (i >= argv.length) throw new Error(`${arg} requires a value`);
      return argv[i];
    };
    if (arg === '--dev') opts.dev = true;
    else if (arg === '--skip-build') opts.skipBuild = true;
    else if (arg === '--browser') opts.browser = value();
    else if (arg === '--timeout') opts.timeoutMs = Number(value());
    else if (arg === '--artifact-dir') opts.artifactDir = value();
    else if (arg === '--help' || arg === '-h') {
      console.log('node driver.cjs [--dev] [--skip-build] [--browser PATH] [--timeout MS] [--artifact-dir DIR]');
      process.exit(0);
    } else throw new Error(`unknown argument: ${arg}`);
  }
  if (!Number.isFinite(opts.timeoutMs) || opts.timeoutMs <= 0) {
    throw new Error('--timeout must be a positive number');
  }
  return opts;
}

// --------------------------------------------------------- wasm build

function buildWasm(opts) {
  if (opts.skipBuild) {
    if (!fs.existsSync(path.join(LANE_DIR, 'pkg', 'owner_plane_browser_lane.js'))) {
      throw new Error('--skip-build but pkg/ is not built');
    }
    return;
  }
  const args = ['build', '--target', 'web', '--out-dir', 'pkg', opts.dev ? '--dev' : '--release'];
  console.log(`[driver] wasm-pack ${args.join(' ')}`);
  const run = spawnSync('wasm-pack', args, { cwd: LANE_DIR, stdio: 'inherit' });
  if (run.status !== 0) {
    throw new Error(`wasm-pack failed (${run.error ? run.error.message : `exit ${run.status}`})`);
  }
}

// ------------------------------------------------------------ corpus

function loadManifest() {
  const entries = [];
  for (const name of fs.readdirSync(VECTORS_DIR).sort()) {
    if (!name.endsWith('.json')) continue;
    const vector = JSON.parse(fs.readFileSync(path.join(VECTORS_DIR, name), 'utf8'));
    const surfaces = Array.isArray(vector.surfaces) ? vector.surfaces : [];
    if (surfaces.includes('browser')) entries.push({ file: name, vector });
  }
  if (entries.length === 0) throw new Error(`no browser-annotated vectors under ${VECTORS_DIR}`);
  const families = new Set(entries.map((e) => e.vector.family));
  for (const fam of REQUIRED_FAMILIES) {
    if (!families.has(fam)) {
      throw new Error(`no browser-annotated family-${fam} vectors — annotation filter broken?`);
    }
  }
  return entries;
}

// ------------------------------------------------------------ server

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.wasm': 'application/wasm',
  '.json': 'application/json',
};

function startServer(manifest) {
  const manifestBody = JSON.stringify(manifest);
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1');
    const fail = (code, msg) => {
      res.writeHead(code, { 'content-type': 'text/plain' });
      res.end(msg);
    };
    if (url.pathname === '/' || url.pathname === '/index.html') {
      res.writeHead(200, { 'content-type': MIME['.html'] });
      res.end(fs.readFileSync(path.join(LANE_DIR, 'fixture', 'index.html')));
      return;
    }
    if (url.pathname === '/manifest.json') {
      res.writeHead(200, { 'content-type': MIME['.json'] });
      res.end(manifestBody);
      return;
    }
    if (url.pathname.startsWith('/pkg/')) {
      const rel = url.pathname.slice('/pkg/'.length);
      if (rel.includes('..') || rel.includes('/')) return fail(400, 'bad path');
      const file = path.join(LANE_DIR, 'pkg', rel);
      if (!fs.existsSync(file)) return fail(404, 'not found');
      res.writeHead(200, {
        'content-type': MIME[path.extname(file)] || 'application/octet-stream',
      });
      res.end(fs.readFileSync(file));
      return;
    }
    fail(404, 'not found');
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({ server, port: server.address().port }));
  });
}

// -------------------------------------------- browser discovery/launch
// (after scripts/smoke-dashboard-boot.cjs)

function isExecutableFile(candidate) {
  try {
    const stat = fs.statSync(candidate);
    return stat.isFile() && Boolean(stat.mode & 0o111);
  } catch (_) {
    return false;
  }
}

function resolveBrowserExecutable(explicit) {
  const candidates = [];
  if (explicit) candidates.push(explicit);
  for (const name of BROWSER_ENV_OVERRIDES) {
    if (process.env[name]) candidates.push(process.env[name]);
  }
  const dirs = (process.env.PATH || '').split(path.delimiter).filter(Boolean);
  for (const dir of dirs) {
    for (const name of PATH_BROWSER_NAMES) candidates.push(path.join(dir, name));
  }
  if (process.platform === 'darwin') candidates.push(...DARWIN_BROWSER_PATHS);
  for (const candidate of candidates) {
    if (!candidate || !isExecutableFile(candidate)) continue;
    const probe = spawnSync(candidate, ['--version'], { stdio: 'ignore', timeout: 5000 });
    if (probe.status === 0) return candidate;
  }
  throw new Error(
    `no working Chromium/Chrome executable found (searched ${PATH_BROWSER_NAMES.join('/')} on PATH,`
    + ` $${BROWSER_ENV_OVERRIDES.join(', $')}${process.platform === 'darwin' ? ', /Applications bundles' : ''}).`
    + ' Install chromium or pass --browser PATH.',
  );
}

function browserArgs(userDataDir, url) {
  return [
    '--remote-debugging-port=0',
    `--user-data-dir=${userDataDir}`,
    '--headless=new',
    // CI runs unprivileged; the page only visits its own loopback
    // server.
    '--no-sandbox',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-background-networking',
    '--disable-dev-shm-usage',
    '--disable-extensions',
    url,
  ];
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitUntil(fn, timeoutMs, message) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await fn();
    if (value) return value;
    if (Date.now() >= deadline) throw new Error(message);
    await delay(POLL_INTERVAL_MS);
  }
}

async function waitForDevToolsPort(userDataDir, child, timeoutMs) {
  const activePortPath = path.join(userDataDir, 'DevToolsActivePort');
  return waitUntil(() => {
    if (child.exitCode !== null) {
      throw new Error(`browser exited before CDP was ready (code ${child.exitCode})`);
    }
    if (fs.existsSync(activePortPath)) {
      const lines = fs.readFileSync(activePortPath, 'utf8').trim().split(/\r?\n/);
      const port = Number(lines[0]);
      if (Number.isFinite(port) && port > 0) return { port };
    }
    return null;
  }, timeoutMs, `browser CDP endpoint was not ready within ${timeoutMs}ms`);
}

function httpGetJson(url) {
  return new Promise((resolve, reject) => {
    const req = http.get(url, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => { body += chunk; });
      res.on('end', () => {
        try {
          resolve(JSON.parse(body));
        } catch (e) {
          reject(e);
        }
      });
    });
    req.on('error', reject);
    req.setTimeout(5000, () => req.destroy(new Error(`GET ${url} timed out`)));
  });
}

// ------------------------------------------------- CDP transport
// (after scripts/smoke-dashboard-boot.cjs: Node's global WebSocket
// when present, raw RFC 6455 fallback otherwise)

async function openWebSocket(wsUrl, timeoutMs) {
  const GlobalWs = globalThis.WebSocket;
  if (typeof GlobalWs === 'function') {
    try {
      return await openGlobalWebSocket(GlobalWs, wsUrl, timeoutMs);
    } catch (_) { /* fall through */ }
  }
  return openMinimalWebSocket(wsUrl, timeoutMs);
}

function openGlobalWebSocket(GlobalWs, wsUrl, timeoutMs) {
  return new Promise((resolve, reject) => {
    const ws = new GlobalWs(wsUrl);
    const timer = setTimeout(() => {
      ws.close();
      reject(new Error(`CDP WebSocket did not open within ${timeoutMs}ms`));
    }, timeoutMs);
    ws.addEventListener('open', () => {
      clearTimeout(timer);
      const adapter = new EventEmitter();
      adapter.send = (text) => ws.send(text);
      adapter.close = () => ws.close();
      ws.addEventListener('message', (event) => {
        if (typeof event.data === 'string') adapter.emit('message', event.data);
        else if (Buffer.isBuffer(event.data)) adapter.emit('message', event.data.toString('utf8'));
        else if (event.data instanceof ArrayBuffer) adapter.emit('message', Buffer.from(event.data).toString('utf8'));
      });
      ws.addEventListener('close', () => adapter.emit('close'));
      ws.addEventListener('error', (event) => adapter.emit('error', event.error || new Error('WebSocket error')));
      resolve(adapter);
    });
    ws.addEventListener('error', (event) => {
      clearTimeout(timer);
      reject(event.error || new Error('WebSocket connect failed'));
    });
  });
}

function openMinimalWebSocket(wsUrl, timeoutMs) {
  const url = new URL(wsUrl);
  const port = Number(url.port) || 80;
  const key = crypto.randomBytes(16).toString('base64');
  const socket = net.connect({ host: url.hostname, port });
  const adapter = new MinimalWebSocket(socket);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`CDP WebSocket did not open within ${timeoutMs}ms`));
    }, timeoutMs);
    socket.once('connect', () => {
      socket.write([
        `GET ${url.pathname}${url.search || ''} HTTP/1.1`,
        `Host: ${url.host}`,
        'Upgrade: websocket',
        'Connection: Upgrade',
        `Sec-WebSocket-Key: ${key}`,
        'Sec-WebSocket-Version: 13',
        '',
        '',
      ].join('\r\n'));
    });
    adapter.once('open', () => { clearTimeout(timer); resolve(adapter); });
    adapter.once('error', (error) => { clearTimeout(timer); reject(error); });
  });
}

class MinimalWebSocket extends EventEmitter {
  constructor(socket) {
    super();
    this.socket = socket;
    this.buffer = Buffer.alloc(0);
    this.opened = false;
    socket.on('data', (chunk) => this.handleData(chunk));
    socket.on('close', () => this.emit('close'));
    socket.on('error', (error) => this.emit('error', error));
  }

  handleData(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    if (!this.opened) {
      const marker = this.buffer.indexOf('\r\n\r\n');
      if (marker === -1) return;
      const headers = this.buffer.slice(0, marker).toString('utf8');
      this.buffer = this.buffer.slice(marker + 4);
      if (!/^HTTP\/1\.[01] 101/.test(headers)) {
        this.emit('error', new Error(`WebSocket handshake failed: ${headers.split(/\r?\n/)[0]}`));
        return;
      }
      this.opened = true;
      this.emit('open');
    }
    this.readFrames();
  }

  readFrames() {
    while (this.buffer.length >= 2) {
      const second = this.buffer[1];
      const opcode = this.buffer[0] & 0x0f;
      let offset = 2;
      let length = second & 0x7f;
      if (length === 126) {
        if (this.buffer.length < offset + 2) return;
        length = this.buffer.readUInt16BE(offset);
        offset += 2;
      } else if (length === 127) {
        if (this.buffer.length < offset + 8) return;
        length = this.buffer.readUInt32BE(offset) * 2 ** 32 + this.buffer.readUInt32BE(offset + 4);
        offset += 8;
      }
      let mask;
      if (second & 0x80) {
        if (this.buffer.length < offset + 4) return;
        mask = this.buffer.slice(offset, offset + 4);
        offset += 4;
      }
      if (this.buffer.length < offset + length) return;
      let payload = this.buffer.slice(offset, offset + length);
      this.buffer = this.buffer.slice(offset + length);
      if (mask) payload = maskBytes(payload, mask);
      if (opcode === 0x1) this.emit('message', payload.toString('utf8'));
      else if (opcode === 0x8) this.close();
      else if (opcode === 0x9) this.writeFrame(0xA, payload);
    }
  }

  send(text) {
    this.writeFrame(0x1, Buffer.from(text, 'utf8'));
  }

  writeFrame(opcode, payload) {
    const mask = crypto.randomBytes(4);
    let header;
    if (payload.length < 126) {
      header = Buffer.alloc(2);
      header[1] = 0x80 | payload.length;
    } else if (payload.length < 65536) {
      header = Buffer.alloc(4);
      header[1] = 0x80 | 126;
      header.writeUInt16BE(payload.length, 2);
    } else {
      header = Buffer.alloc(10);
      header[1] = 0x80 | 127;
      header.writeUInt32BE(0, 2);
      header.writeUInt32BE(payload.length, 6);
    }
    header[0] = 0x80 | opcode;
    this.socket.write(Buffer.concat([header, mask, maskBytes(payload, mask)]));
  }

  close() {
    if (!this.socket.destroyed) this.socket.end();
  }
}

function maskBytes(payload, mask) {
  const out = Buffer.alloc(payload.length);
  for (let i = 0; i < payload.length; i += 1) out[i] = payload[i] ^ mask[i % 4];
  return out;
}

class Cdp extends EventEmitter {
  constructor(socket) {
    super();
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    socket.on('message', (raw) => {
      let message;
      try {
        message = JSON.parse(raw);
      } catch (_) {
        return;
      }
      if (message.id && this.pending.has(message.id)) {
        const { resolve, reject } = this.pending.get(message.id);
        this.pending.delete(message.id);
        if (message.error) reject(new Error(`${message.error.message || 'CDP error'}`));
        else resolve(message.result);
      } else if (message.method) {
        this.emit(message.method, message.params || {});
      }
    });
    socket.on('close', () => this.rejectAll(new Error('CDP WebSocket closed')));
    socket.on('error', (error) => this.rejectAll(error));
  }

  rejectAll(error) {
    for (const { reject } of this.pending.values()) reject(error);
    this.pending.clear();
  }

  send(method, params = {}, timeoutMs = 10000) {
    const id = this.nextId;
    this.nextId += 1;
    this.socket.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP ${method} timed out`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (v) => { clearTimeout(timer); resolve(v); },
        reject: (e) => { clearTimeout(timer); reject(e); },
      });
    });
  }
}

// --------------------------------------------------------------- gate

function rowGreen(row) {
  return row.pairs === 'ok' && row.decode === 'ok' && row.convergence === 'ok'
    && row.semantics === 'PASS';
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  buildWasm(opts);
  const manifest = loadManifest();

  const { server, port } = await startServer(manifest);
  const fixtureUrl = `http://127.0.0.1:${port}/`;
  const userDataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'owner-plane-lane-'));
  const browserPath = resolveBrowserExecutable(opts.browser);
  console.log(`[driver] serving ${manifest.length} vector(s) at ${fixtureUrl}`);
  console.log(`[driver] browser: ${browserPath}`);

  const stderrChunks = [];
  const child = spawn(browserPath, browserArgs(userDataDir, fixtureUrl), {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  child.stderr.on('data', (chunk) => stderrChunks.push(chunk));

  const diagnostics = [];
  let exitCode = 1;
  try {
    const { port: cdpPort } = await waitForDevToolsPort(userDataDir, child, CDP_READY_TIMEOUT_MS);
    const version = await httpGetJson(`http://127.0.0.1:${cdpPort}/json/version`);
    console.log(`[driver] ${version.Browser || 'unknown browser'} over CDP`);
    const targets = await waitUntil(async () => {
      const list = await httpGetJson(`http://127.0.0.1:${cdpPort}/json/list`);
      const page = list.find((t) => t.type === 'page' && (t.url || '').startsWith(fixtureUrl));
      return page || null;
    }, CDP_READY_TIMEOUT_MS, 'fixture page target never appeared');

    const socket = await openWebSocket(targets.webSocketDebuggerUrl, CDP_READY_TIMEOUT_MS);
    const cdp = new Cdp(socket);
    cdp.on('Runtime.exceptionThrown', (params) => {
      const d = params.exceptionDetails || {};
      diagnostics.push(`exception: ${d.text || ''} ${(d.exception && d.exception.description) || ''}`);
    });
    await cdp.send('Runtime.enable');

    const raw = await waitUntil(async () => {
      const result = await cdp.send('Runtime.evaluate', {
        expression: 'window.__laneReport && window.__laneReport.done ? JSON.stringify(window.__laneReport) : ""',
        returnByValue: true,
      });
      const value = result && result.result && result.result.value;
      return typeof value === 'string' && value ? value : null;
    }, opts.timeoutMs, `lane report not done within ${opts.timeoutMs}ms`);

    const report = JSON.parse(raw);
    if (report.error) throw new Error(`fixture fatal: ${report.error}`);
    if (report.rows.length !== manifest.length || report.total !== manifest.length) {
      throw new Error(
        `row count mismatch: served ${manifest.length}, page saw ${report.total}, ran ${report.rows.length}`,
      );
    }
    const red = report.rows.filter((row) => !rowGreen(row));
    console.log(`[driver] ${report.rows.length - red.length}/${report.rows.length} green under ${report.ua}`);
    console.log('[driver] caveat: f13 rows ran the engine lanes in-memory in Chromium; the IndexedDB Txn substrate is execution-lanes work item 3.');
    if (red.length) {
      for (const row of red) {
        console.error(`[driver] RED ${row.file}: pairs=${row.pairs} decode=${row.decode} convergence=${row.convergence} semantics=${row.semantics}`);
      }
    } else {
      exitCode = 0;
    }
  } catch (error) {
    console.error(`[driver] ${error.message}`);
  } finally {
    for (const line of diagnostics.slice(0, 20)) console.error(`[browser] ${line}`);
    if (exitCode !== 0 && opts.artifactDir) {
      fs.mkdirSync(opts.artifactDir, { recursive: true });
      fs.writeFileSync(path.join(opts.artifactDir, 'browser-stderr.log'), Buffer.concat(stderrChunks));
      fs.writeFileSync(path.join(opts.artifactDir, 'driver-diagnostics.log'), diagnostics.join('\n'));
    }
    child.kill('SIGTERM');
    await Promise.race([new Promise((r) => child.once('exit', r)), delay(3000)]);
    if (child.exitCode === null) child.kill('SIGKILL');
    server.close();
    fs.rmSync(userDataDir, { recursive: true, force: true });
  }
  process.exit(exitCode);
}

main().catch((error) => {
  console.error(`[driver] ${error.message}`);
  process.exit(1);
});

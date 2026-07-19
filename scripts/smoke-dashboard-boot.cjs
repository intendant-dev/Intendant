#!/usr/bin/env node
'use strict';

// Dashboard boot smoke — keyless, CI-grade, deliberately tiny.
//
// Loads the dashboard SPA from an already-running daemon in headless
// Chromium over raw CDP (no npm deps) and asserts the page actually BOOTED:
//
//   (a) late-module window exposures exist — every JS fragment in
//       static/app/ is assembled into ONE <script type="module">
//       (30-module-open.html … 59-module-close.html), so a single eval
//       error anywhere kills every fragment after it. On 2026-07-09 a
//       cross-fragment TDZ ReferenceError did exactly that, silently, and
//       every CI gate stayed green because nothing boots the SPA in a real
//       browser. `window.retryFilesTransfer` (55-files-ide.js) and
//       `window.createVirtualDisplay` (58-shortcuts-boot.js) are
//       unconditional top-level assignments near the END of that module —
//       they exist iff module eval ran to completion.
//   (b) zero page-error-level events during boot (filter documented at
//       FATAL/ALLOWED below — network-level noise from a keyless daemon is
//       expected and allowed, page-integrity errors are not).
//   (c) the static shell rendered (#files-transfer-list, from
//       20-shell.html).
//   (bonus) if `window.__intendantModuleAlive` exists it must be truthy;
//       its absence is fine — assertion (a) stands alone.
//   (d) the session-window jump-to-bottom button is state-derived: on a
//       synthetic window it must appear whenever the reader is scrolled
//       off the tail (including on a QUIET window with no appends in
//       flight), stay put without flicker while output streams in, and
//       hide at the tail / while minimized / after a restore-from-minimize
//       rebuild. This is the one interaction-state assertion in the smoke:
//       the visibility predicate regressed silently in production
//       (2026-07-19 — event-starved by an append-only flag) because no CI
//       harness executes SPA interaction paths; the probe is keyless,
//       deterministic (synthetic DOM only, no daemon sessions), and adds
//       one Runtime.evaluate.
//
// This script builds and launches NOTHING but the browser: point it at a
// running daemon. Keyless local recipe:
//
//   PROVIDER=mock INTENDANT_MOCK_SCRIPT=/tmp/mock.json \
//     target/debug/intendant --web 0 --bind 127.0.0.1 --no-tls
//   node scripts/smoke-dashboard-boot.cjs http://127.0.0.1:<port>/
//
// scripts/validate-dashboard.cjs is the deep QA battery (Station probes,
// perf evals, workflows); this file is the minimal boot gate that can run
// on every PR. Keep it lean — new dashboard assertions belong in the
// battery unless they are boot-integrity signals.

const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const https = require('https');
const net = require('net');
const os = require('os');
const path = require('path');
const { EventEmitter } = require('events');
const { spawn, spawnSync } = require('child_process');

const DEFAULT_TIMEOUT_MS = 45000;
const CDP_READY_TIMEOUT_MS = 15000;
const CDP_COMMAND_TIMEOUT_MS = 10000;
// Errors thrown during module eval fire before the late exposures appear,
// so they are always caught. The settle window additionally scoops up
// immediately-following async failures (first WS message handlers, first
// snapshot render) before the ledger verdict.
const POST_BOOT_SETTLE_MS = 750;
const POLL_INTERVAL_MS = 250;
const MAX_REPORT_LINES = 40;
const LINE_LIMIT = 300;

// Browser executables searched on PATH, in order. Explicit --browser and
// the env overrides win (same env names validate-dashboard.cjs honors).
const PATH_BROWSER_NAMES = [
  'chromium',
  'chromium-browser',
  'google-chrome',
  'google-chrome-stable',
  'chrome',
];
const BROWSER_ENV_OVERRIDES = [
  'INTENDANT_BROWSER_WORKSPACE_EXECUTABLE',
  'INTENDANT_BROWSER_EXECUTABLE',
  'CHROME_PATH',
  'CHROME_BIN',
];
const DARWIN_BROWSER_PATHS = [
  '/Applications/Chromium.app/Contents/MacOS/Chromium',
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
];

function usage() {
  console.log(`Usage: node scripts/smoke-dashboard-boot.cjs <daemon-url> [options]

  <daemon-url>          e.g. http://127.0.0.1:41234/ (the SPA is served at /)

Options:
  --url URL             Alternative to the positional daemon URL
  --browser PATH        Chromium/Chrome executable (default: $CHROME_BIN et al,
                        then chromium/chromium-browser/google-chrome on PATH)
  --timeout MS          Overall boot deadline (default: ${DEFAULT_TIMEOUT_MS})
  --artifact-dir DIR    On failure, write boot-failure.png, boot-errors.log,
                        and browser-stderr.log here`);
}

function parseArgs(argv) {
  const opts = { url: '', browser: '', timeoutMs: DEFAULT_TIMEOUT_MS, artifactDir: '' };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const value = () => {
      i += 1;
      if (i >= argv.length) throw new Error(`${arg} requires a value`);
      return argv[i];
    };
    if (arg === '--url') opts.url = value();
    else if (arg === '--browser') opts.browser = value();
    else if (arg === '--timeout') opts.timeoutMs = Number(value());
    else if (arg === '--artifact-dir') opts.artifactDir = value();
    else if (arg === '--help' || arg === '-h') { usage(); process.exit(0); }
    else if (!arg.startsWith('-') && !opts.url) opts.url = arg;
    else throw new Error(`unknown argument: ${arg}`);
  }
  if (!opts.url) throw new Error('daemon URL is required (see --help)');
  if (!/^https?:\/\//.test(opts.url)) throw new Error(`daemon URL must be http(s): ${opts.url}`);
  if (!Number.isFinite(opts.timeoutMs) || opts.timeoutMs <= 0) throw new Error('--timeout must be a positive number');
  return opts;
}

// ---------------------------------------------------------------------------
// Browser discovery + launch

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
  const rejected = [];
  for (const candidate of candidates) {
    if (!candidate || !isExecutableFile(candidate)) continue;
    // `--version` exits 0 without opening a window; it weeds out broken
    // launcher shims (e.g. a Homebrew cask stub whose target app was
    // removed) so resolution falls through to the next real browser
    // instead of dying at launch.
    const probe = spawnSync(candidate, ['--version'], { stdio: 'ignore', timeout: 5000 });
    if (probe.status === 0) return candidate;
    rejected.push(`${candidate} (--version failed: ${probe.error ? probe.error.message : `exit ${probe.status}`})`);
  }
  const rejectedNote = rejected.length ? ` Rejected broken candidates: ${rejected.join('; ')}.` : '';
  throw new Error(
    `no working Chromium/Chrome executable found (searched ${PATH_BROWSER_NAMES.join('/')} on PATH`
    + `${process.platform === 'darwin' ? ', /Applications bundles' : ''}, and `
    + `$${BROWSER_ENV_OVERRIDES.join(', $')}). Install chromium or pass --browser PATH.${rejectedNote}`,
  );
}

function browserArgs(userDataDir, url) {
  const args = [
    '--remote-debugging-port=0',
    `--user-data-dir=${userDataDir}`,
    '--headless=new',
    // CI runs as an unprivileged user on kernels that may disable
    // unprivileged user namespaces; the sandbox adds nothing to a smoke
    // that only visits its own loopback daemon.
    '--no-sandbox',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-background-networking',
    '--disable-dev-shm-usage',
    '--disable-extensions',
    '--window-size=1440,1000',
  ];
  // Allow pointing at a TLS daemon with a self-signed cert for local use.
  // (mTLS-gated daemons still refuse certless browsers — run those against
  // --no-tls instead; the CI job always uses plain HTTP on loopback.)
  if (url.startsWith('https://')) args.push('--ignore-certificate-errors');
  args.push('about:blank');
  return args;
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
      if (Number.isFinite(port) && port > 0) {
        return { port, path: lines[1] || '/devtools/browser' };
      }
    }
    return null;
  }, timeoutMs, `browser CDP endpoint was not ready within ${timeoutMs}ms`);
}

function httpGet(url) {
  return new Promise((resolve, reject) => {
    const client = url.startsWith('https:') ? https : http;
    const req = client.get(url, { rejectUnauthorized: false }, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => { body += chunk; });
      res.on('end', () => resolve({ status: res.statusCode, body }));
    });
    req.on('error', reject);
    req.setTimeout(5000, () => req.destroy(new Error(`GET ${url} timed out`)));
  });
}

async function httpJson(url) {
  const { status, body } = await httpGet(url);
  if (status < 200 || status >= 300) throw new Error(`GET ${url} returned ${status}`);
  return JSON.parse(body);
}

// ---------------------------------------------------------------------------
// Minimal CDP transport: prefer the Node global WebSocket (Node >= 22),
// fall back to a raw RFC 6455 client socket — same tiering as
// scripts/validate-dashboard.cjs minus its optional `ws` package layer.

async function openWebSocket(wsUrl, timeoutMs) {
  const GlobalWs = globalThis.WebSocket;
  if (typeof GlobalWs === 'function') {
    try {
      return await openGlobalWebSocket(GlobalWs, wsUrl, timeoutMs);
    } catch (_) {
      // Older/incomplete global implementations: fall through.
    }
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

class CdpConnection extends EventEmitter {
  constructor(socket) {
    super();
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    socket.on('message', (raw) => this.handleMessage(raw));
    socket.on('close', () => this.rejectAll(new Error('CDP WebSocket closed')));
    socket.on('error', (error) => this.rejectAll(error));
  }

  send(method, params = {}, sessionId, timeoutMs = CDP_COMMAND_TIMEOUT_MS) {
    const id = this.nextId;
    this.nextId += 1;
    const payload = { id, method, params };
    if (sessionId) payload.sessionId = sessionId;
    this.socket.send(JSON.stringify(payload));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.pending.delete(id)) reject(new Error(`CDP ${method} timed out`));
      }, timeoutMs);
      if (typeof timer.unref === 'function') timer.unref();
      this.pending.set(id, { resolve, reject, timer });
    });
  }

  handleMessage(raw) {
    let message;
    try {
      message = JSON.parse(String(raw));
    } catch (_) {
      return;
    }
    if (message.id && this.pending.has(message.id)) {
      const entry = this.pending.get(message.id);
      this.pending.delete(message.id);
      clearTimeout(entry.timer);
      if (message.error) entry.reject(new Error(message.error.message || JSON.stringify(message.error)));
      else entry.resolve(message.result || {});
      return;
    }
    this.emit('event', message);
  }

  rejectAll(error) {
    for (const entry of this.pending.values()) {
      clearTimeout(entry.timer);
      entry.reject(error);
    }
    this.pending.clear();
  }

  close() {
    this.socket.close();
  }
}

// ---------------------------------------------------------------------------
// Boot-error ledger.
//
// FATAL (any single entry fails the smoke):
//   - Runtime.exceptionThrown: uncaught exceptions, unhandled promise
//     rejections, and module-evaluation errors. This is exactly the
//     2026-07-09 incident class.
//   - Log.entryAdded at level "error" from any source EXCEPT "network":
//     source "javascript" mirrors page errors; "security"/"deprecation"/
//     "other" errors mean the page itself is broken.
//   - Runtime.consoleAPICalled with type "error"/"assert" whose message is
//     not recognizably network noise: the SPA console.error()ing during
//     boot is a boot-integrity signal.
//   - A JavaScript dialog opening during boot (auto-dismissed so the run
//     can finish, but recorded as fatal — nothing in a healthy boot opens
//     dialogs).
//
// ALLOWED (recorded, reported, never fatal):
//   - Log.entryAdded with source "network" (any level): a keyless mock
//     daemon legitimately fails optional fetches, and Chromium reports
//     every failed resource load here ("Failed to load resource: ...").
//     These are environmental, not SPA-integrity, signals.
//   - console.error text that matches NETWORK_NOISE_RE: transport-level
//     failure messages the SPA logs for optional endpoints (fetch/
//     WebSocket teardown noise). Kept deliberately narrow — anything
//     ambiguous stays fatal so real regressions cannot hide behind the
//     allowlist.
const NETWORK_NOISE_RE = /failed to fetch|networkerror|network error|load resource|websocket|net::err_|err_connection|xhr|cors/i;

class BootLedger {
  constructor() {
    this.fatal = [];
    this.allowed = [];
  }

  record(kind, text, { fatal }) {
    const compact = `[${kind}] ${String(text || '').replace(/\s+/g, ' ').trim()}`.slice(0, LINE_LIMIT);
    (fatal ? this.fatal : this.allowed).push(compact);
  }

  onCdpEvent(message, sessionId) {
    if (message.sessionId !== sessionId || !message.method) return;
    const params = message.params || {};
    if (message.method === 'Runtime.exceptionThrown') {
      this.record('exception', exceptionText(params.exceptionDetails || {}), { fatal: true });
    } else if (message.method === 'Log.entryAdded') {
      const entry = params.entry || {};
      if (entry.source === 'network') {
        this.record(`log.network.${entry.level}`, `${entry.text || ''} ${entry.url || ''}`, { fatal: false });
      } else if (entry.level === 'error') {
        this.record(`log.${entry.source}.error`, `${entry.text || ''} ${entry.url || ''}`, { fatal: true });
      }
    } else if (message.method === 'Runtime.consoleAPICalled') {
      const type = params.type || 'log';
      if (type !== 'error' && type !== 'assert') return;
      const text = (params.args || []).map(remoteObjectText).filter(Boolean).join(' ');
      this.record(`console.${type}`, text, { fatal: !NETWORK_NOISE_RE.test(text) });
    } else if (message.method === 'Page.javascriptDialogOpening') {
      this.record('dialog', `${params.type || 'dialog'}: ${params.message || ''}`, { fatal: true });
    }
  }
}

// ---------------------------------------------------------------------------
// Session-window jump-button behavior probe (assertion (d)).
//
// Runs entirely inside the page against a synthetic session window built
// through the SPA's own creation/append paths — no daemon session exists on
// the keyless smoke daemon, and none is needed: the probe pins the
// client-side visibility state machine (scroll → follow recompute → button)
// that broke silently when it was keyed on an append-only flag. Module-scope
// SPA functions are unreachable from page evals (one shared module scope),
// so state mutations go through the window.qa.sessionWindowSweeps facade
// (41-session-window-actions.js) and everything else is plain DOM. Every
// wait is a double-rAF settle (scroll events from programmatic scrollTop
// writes and the SPA's rAF-coalesced follow scroll both land within one
// frame).
const JUMP_BUTTON_PROBE_EXPRESSION = `(async () => {
  const out = { failures: [], steps: [] };
  const check = (cond, label) => {
    out.steps.push((cond ? 'pass' : 'FAIL') + ' ' + label);
    if (!cond) out.failures.push(label);
  };
  const settle = () => new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  });
  const sid = 'boot-smoke-jump-probe';
  const qa = window.qa && window.qa.sessionWindowSweeps;
  for (const name of ['build', 'append', 'remove', 'setMinimized', 'windowState']) {
    if (!qa || typeof qa[name] !== 'function') {
      out.failures.push('window.qa.sessionWindowSweeps.' + name + ' missing');
      return out;
    }
  }
  try {
    if (!qa.build(sid, { task: 'jump-button smoke probe' })) {
      out.failures.push('synthetic session window did not materialize');
      return out;
    }
    const root = document.querySelector('.session-window[data-session-id="' + sid + '"]');
    const log = root && root.querySelector('.session-window-log');
    const button = root && root.querySelector('.session-window-jump-bottom');
    if (!root || !log || !button) {
      out.failures.push('probe window DOM (log / jump button) missing');
      return out;
    }
    // Deterministic scroll geometry: max-height caps the flex-stretched log
    // (header-collapsed windows have no stylesheet max-height), so 40 seed
    // entries always overflow.
    log.style.maxHeight = '120px';
    log.style.overflowY = 'auto';
    const visible = () => !button.classList.contains('hidden');
    const atTail = () => {
      const state = qa.windowState(sid);
      return !!(state && state.logAtBottom);
    };
    for (let i = 0; i < 40; i += 1) qa.append(sid, 'probe seed ' + i + ' ' + 'x'.repeat(48));
    await settle();
    check(log.scrollHeight > log.clientHeight + 60, 'probe window is scrollable');
    // Anchor at the tail through the scroller itself (a user drag to the
    // bottom). The seed burst compresses create+append into one task, so
    // the SPA's follow flush can race the grid-fit rAF that first gives the
    // log its overflow — live sessions re-arm the flush on every later
    // append, but the probe must not depend on that ordering.
    log.scrollTop = log.scrollHeight;
    await settle();
    check(atTail(), 'seeded and anchored at the tail');
    check(!visible(), 'at tail: button hidden');
    // (1) QUIET window scrolled up: nothing appends afterwards — the scroll
    // recompute alone must reveal the button.
    log.scrollTop = 0;
    await settle();
    check(visible(), 'scrolled-up quiet window: button visible');
    // (4) streaming appends while scrolled up: stays visible with no
    // hidden-flicker (any class mutation whose OLD value contains "hidden"
    // means the button was hidden at some point mid-burst) and no follow
    // steal of the reader's scroll position.
    let flickered = false;
    const observer = new MutationObserver((records) => {
      for (const record of records) {
        if (/(^|\\s)hidden(\\s|$)/.test(record.oldValue || '')) flickered = true;
      }
    });
    observer.observe(button, { attributes: true, attributeFilter: ['class'], attributeOldValue: true });
    const heldScrollTop = log.scrollTop;
    for (let i = 0; i < 12; i += 1) qa.append(sid, 'probe stream ' + i + ' ' + 'y'.repeat(48));
    await settle();
    observer.disconnect();
    check(visible(), 'streaming append while scrolled up: button still visible');
    check(!flickered, 'no hidden-flicker during streamed appends');
    check(Math.abs(log.scrollTop - heldScrollTop) < 1, 'no follow steal while scrolled up');
    // (2) the button's own click path returns to the tail and hides it.
    button.click();
    await settle();
    check(atTail(), 'jump click landed at the tail');
    check(!visible(), 'at tail after jump click: button hidden');
    // (3) restore-from-minimize: minimize unmounts the transcript; restore
    // re-materializes the tail and lands at the bottom by design, so the
    // button must be hidden after the rebuild — and a fresh scroll-up on the
    // rebuilt DOM must reveal it again.
    log.scrollTop = 0;
    await settle();
    check(visible(), 'scrolled up before minimize: button visible');
    qa.setMinimized(sid, true);
    await settle();
    check(!visible(), 'minimized: button hidden');
    qa.setMinimized(sid, false);
    await settle();
    check(atTail(), 'restore-from-minimize landed at the tail');
    check(!visible(), 'restored at tail: button hidden');
    log.scrollTop = 0;
    await settle();
    check(visible(), 'scrolled up after restore rebuild: button visible');
  } catch (error) {
    out.failures.push('probe threw: ' + (error && error.message ? error.message : String(error)));
  } finally {
    try { qa.remove(sid); } catch (_) {}
  }
  return out;
})()`;

// ---------------------------------------------------------------------------
// Chapter-jump landing probe (assertion (e)).
//
// Pins where chapter-nav jumps LAND in a real windowed pane transcript:
// centered in the scroller for rows that fit, first line pinned just below
// the top edge for rows taller than the viewport — measured in viewport
// coordinates (rect deltas against the scroller), the coordinates a reader
// actually sees. Regression bait: the landing math once used offsetTop,
// which for pane rows is relative to the positioned .session-window (the
// scroller itself is position: static), so every jump overshot by the
// header chrome height — absorbed invisibly by centered landings in tall
// maximized panes, but a card-sized grid scroller's taller-than-viewport
// branch clipped the target's first line under the header (live user
// report, 2026-07-19; the card-sized cap below is what makes that branch
// fire — a tall probe pane would silently pass it). The transcript is
// deeper than the 600-row render window so both jump targets start
// UNMOUNTED, pinning materialize→measure→scroll ordering too, and the
// tail asserts jump-then-append stability across both append lanes.
const CHAPTER_JUMP_PROBE_EXPRESSION = `(async () => {
  const out = { failures: [], steps: [] };
  const check = (cond, label) => {
    out.steps.push((cond ? 'pass' : 'FAIL') + ' ' + label);
    if (!cond) out.failures.push(label);
  };
  const settle = () => new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(resolve, 0)));
  });
  const sid = 'boot-smoke-chapter-probe';
  const sweeps = window.qa && window.qa.sessionWindowSweeps;
  const nav = window.qa && window.qa.chapterNav;
  for (const [facade, name] of [
    [sweeps, 'sessionWindowSweeps.build'],
    [sweeps, 'sessionWindowSweeps.append'],
    [sweeps, 'sessionWindowSweeps.remove'],
    [nav, 'chapterNav.qaInsertRecords'],
    [nav, 'chapterNav.qaScrollTail'],
    [nav, 'chapterNav.jumpPane'],
    [nav, 'chapterNav.paneState'],
  ]) {
    if (!facade || typeof facade[name.split('.')[1]] !== 'function') {
      out.failures.push('window.qa.' + name + ' missing');
      return out;
    }
  }
  try {
    if (!sweeps.build(sid, { task: 'chapter jump landing probe' })) {
      out.failures.push('synthetic session window did not materialize');
      return out;
    }
    const root = document.querySelector('.session-window[data-session-id="' + sid + '"]');
    const log = root && root.querySelector('.session-window-log');
    if (!root || !log) {
      out.failures.push('probe window DOM missing');
      return out;
    }
    // Grid-card scroll geometry: pin the stylesheet's card cap so the
    // taller-than-viewport branch really fires (mirrors the jump-button
    // probe's deterministic-geometry pin).
    log.style.maxHeight = '220px';
    log.style.overflowY = 'auto';
    const geom = (mark) => {
      const node = log.querySelector('[data-history-index="' + mark + '"]');
      if (!node) return null;
      const viewTop = log.getBoundingClientRect().top + log.clientTop;
      const rect = node.getBoundingClientRect();
      const nodeTop = rect.top - viewTop;
      return {
        nodeTop: Math.round(nodeTop),
        nodeHeight: Math.round(rect.height),
        viewHeight: log.clientHeight,
        centerOffset: Math.round(nodeTop + rect.height / 2 - log.clientHeight / 2),
      };
    };
    const SHORT_MARK = 60;
    const TALL_MARK = 250;
    const TOTAL = 900;
    const base = Date.now() - 7200000;
    const records = [];
    for (let i = 0; i < TOTAL; i += 1) {
      if (i === SHORT_MARK) {
        records.push({ source: 'user', level: 'info', content: 'probe user target (short) ' + i, ts_ms: base + i * 1000 });
      } else if (i === TALL_MARK) {
        const lines = [];
        for (let k = 0; k < 80; k += 1) lines.push('probe tall user line ' + k);
        records.push({ source: 'user', level: 'info', content: lines.join('\\n'), ts_ms: base + i * 1000 });
      } else {
        records.push({ source: 'system', level: 'info', content: 'probe filler ' + i, ts_ms: base + i * 1000 });
      }
    }
    nav.qaInsertRecords(sid, records);
    nav.qaScrollTail(sid);
    await settle();
    const st0 = nav.paneState(sid);
    check(!!st0 && st0.len === TOTAL, 'history holds all ' + TOTAL + ' records (got ' + (st0 && st0.len) + ')');
    check(!!st0 && st0.user.length === 2 && st0.user[0] === SHORT_MARK && st0.user[1] === TALL_MARK,
      'user marks indexed at ' + SHORT_MARK + '/' + TALL_MARK + ' (got ' + JSON.stringify(st0 && st0.user) + ')');
    check(!!st0 && st0.renderStart > TALL_MARK,
      'both targets start unmounted (renderStart ' + (st0 && st0.renderStart) + ')');
    check(log.scrollHeight > log.clientHeight * 3, 'probe window is scrollable');

    // (1) prev-jump onto the TALL unmounted target: materializes it, then
    // top-aligns with the first line fully visible below the top edge.
    check(nav.jumpPane(sid, 'user', -1) === true, 'tall jump dispatched');
    await settle();
    const tall = geom(TALL_MARK);
    check(!!tall, 'tall target mounted after jump');
    if (tall) {
      out.steps.push('tall landing ' + JSON.stringify(tall));
      check(tall.nodeHeight > tall.viewHeight,
        'tall target overflows the viewport (' + tall.nodeHeight + ' > ' + tall.viewHeight + ')');
      check(tall.nodeTop >= 0, 'tall target first line not clipped (nodeTop ' + tall.nodeTop + ')');
      check(tall.nodeTop <= 20, 'tall target pinned near the top edge (nodeTop ' + tall.nodeTop + ')');
    }

    // (2) prev-jump onto the SHORT unmounted target: lands centered.
    check(nav.jumpPane(sid, 'user', -1) === true, 'short jump dispatched');
    await settle();
    const centered = geom(SHORT_MARK);
    check(!!centered, 'short target mounted after jump');
    if (centered) {
      out.steps.push('short landing ' + JSON.stringify(centered));
      check(centered.nodeHeight <= centered.viewHeight, 'short target fits the viewport');
      check(Math.abs(centered.centerOffset) <= 20,
        'short target centered (centerOffset ' + centered.centerOffset + 'px)');
    }

    // (3) jump-then-append stability: appends on both lanes (live node
    // lane, record insert lane) while parked on the jump cursor must not
    // move the landing.
    for (let i = 0; i < 3; i += 1) sweeps.append(sid, 'probe live append ' + i);
    nav.qaInsertRecords(sid, [
      { source: 'system', level: 'info', content: 'probe late insert', ts_ms: base + (TOTAL + 60) * 1000 },
    ]);
    await settle();
    const after = geom(SHORT_MARK);
    check(!!after, 'target still mounted after appends');
    if (after && centered) {
      check(Math.abs(after.nodeTop - centered.nodeTop) <= 2,
        'landing stable across appends (drift ' + (after.nodeTop - centered.nodeTop) + 'px)');
    }
  } catch (error) {
    out.failures.push('probe threw: ' + (error && error.message ? error.message : String(error)));
  } finally {
    try { sweeps.remove(sid); } catch (_) {}
  }
  return out;
})()`;

function remoteObjectText(arg) {
  if (!arg || typeof arg !== 'object') return '';
  if (arg.value !== undefined) {
    return typeof arg.value === 'string' ? arg.value : JSON.stringify(arg.value);
  }
  return arg.description || arg.unserializableValue || `<${arg.type || 'value'}>`;
}

function exceptionText(details) {
  const exception = details.exception || {};
  const head = exception.description || exception.value || details.text || 'uncaught exception';
  const where = details.url ? ` at ${details.url}:${details.lineNumber}:${details.columnNumber}` : '';
  return `${head}${where}`;
}

// ---------------------------------------------------------------------------
// The smoke.

async function main() {
  let opts;
  try {
    opts = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(`argument error: ${error.message}`);
    usage();
    process.exit(2);
  }

  // Pre-flight: a clear "daemon unreachable" beats a browser navigation
  // error. The SPA is served at every GET path, so the URL itself is not
  // shape-sensitive.
  try {
    const { status } = await httpGet(opts.url);
    if (status !== 200) throw new Error(`expected 200, got ${status}`);
  } catch (error) {
    console.error(`daemon pre-flight failed: GET ${opts.url}: ${error.message}`);
    process.exit(1);
  }

  const executable = resolveBrowserExecutable(opts.browser);
  console.log(`browser: ${executable}`);
  console.log(`daemon:  ${opts.url}`);

  const userDataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'dashboard-boot-smoke-'));
  const browserStderr = [];
  const child = spawn(executable, browserArgs(userDataDir, opts.url), {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  child.stderr.on('data', (chunk) => {
    for (const line of String(chunk).split(/\r?\n/)) {
      if (line.trim()) {
        browserStderr.push(line.slice(0, LINE_LIMIT));
        if (browserStderr.length > 200) browserStderr.shift();
      }
    }
  });

  let cdp = null;
  let sessionId = null;
  const ledger = new BootLedger();
  let verdict = 1;
  try {
    const { port } = await waitForDevToolsPort(userDataDir, child, CDP_READY_TIMEOUT_MS);
    const version = await httpJson(`http://127.0.0.1:${port}/json/version`);
    const socket = await openWebSocket(version.webSocketDebuggerUrl, CDP_READY_TIMEOUT_MS);
    cdp = new CdpConnection(socket);

    // Attach to a fresh tab and enable event domains BEFORE navigating —
    // module-eval errors fire in the first milliseconds of the load and
    // are lost to a late attach.
    const target = await cdp.send('Target.createTarget', { url: 'about:blank' });
    const attached = await cdp.send('Target.attachToTarget', { targetId: target.targetId, flatten: true });
    sessionId = attached.sessionId;
    cdp.on('event', (message) => ledger.onCdpEvent(message, sessionId));
    await cdp.send('Page.enable', {}, sessionId);
    await cdp.send('Runtime.enable', {}, sessionId);
    // Log.enable can be unsupported on exotic builds; boot errors still
    // arrive via Runtime.exceptionThrown, so tolerate its absence.
    await cdp.send('Log.enable', {}, sessionId).catch(() => {});
    // Network domain is deliberately NOT enabled: the smoke does not need
    // request-level data, and with Network enabled headless Chrome stalls
    // large streaming response bodies (validate-dashboard.cjs, KNOWN
    // LIMIT, diagnosed 2026-07-07).
    cdp.on('event', (message) => {
      if (message.sessionId === sessionId && message.method === 'Page.javascriptDialogOpening') {
        cdp.send('Page.handleJavaScriptDialog', { accept: false }, sessionId).catch(() => {});
      }
    });

    const nav = await cdp.send('Page.navigate', { url: opts.url }, sessionId);
    if (nav.errorText) throw new Error(`navigation failed: ${nav.errorText}`);

    const evaluate = async (expression, { awaitPromise = false } = {}) => {
      const result = await cdp.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise }, sessionId);
      if (result.exceptionDetails) throw new Error(`evaluate failed: ${exceptionText(result.exceptionDetails)}`);
      return result.result ? result.result.value : undefined;
    };

    // Boot readiness: document parsed AND the late-module exposures exist
    // AND the static shell rendered. Polled as one probe so the timeout
    // message can say exactly which leg never came up.
    const probeExpression = `(() => ({
      readyState: document.readyState,
      retryFilesTransfer: typeof window.retryFilesTransfer,
      createVirtualDisplay: typeof window.createVirtualDisplay,
      shell: Boolean(document.getElementById('files-transfer-list')),
      moduleAlive: typeof window.__intendantModuleAlive === 'undefined'
        ? null
        : Boolean(window.__intendantModuleAlive),
    }))()`;
    let probe = null;
    const bootedAt = Date.now();
    try {
      probe = await waitUntil(async () => {
        // Fail fast once a fatal entry lands: a module-eval error means the
        // late exposures will never appear, and the verdict is already red —
        // no point running out the full deadline.
        if (ledger.fatal.length > 0) {
          throw new Error(`page reported ${ledger.fatal.length} boot error(s) during load`);
        }
        // A poll can land exactly on the about:blank → dashboard context
        // swap; context-teardown errors are transient, retry them. Anything
        // else propagates (and the deadline path reports the last state).
        let state;
        try {
          state = await evaluate(probeExpression);
        } catch (evalError) {
          if (/execution context|cannot find context|target navigated|target closed/i.test(evalError.message)) {
            return null;
          }
          throw evalError;
        }
        if (!state) return null;
        const booted = state.readyState !== 'loading'
          && state.retryFilesTransfer === 'function'
          && state.createVirtualDisplay === 'function'
          && state.shell;
        return booted ? state : null;
      }, opts.timeoutMs, 'boot assertions did not pass in time');
    } catch (error) {
      const last = await evaluate(probeExpression).catch(() => null);
      const detail = last
        ? ` last probe: readyState=${last.readyState}`
          + ` retryFilesTransfer=${last.retryFilesTransfer} (want function)`
          + ` createVirtualDisplay=${last.createVirtualDisplay} (want function)`
          + ` shell=${last.shell} (want true)`
        : ' (probe itself failed)';
      throw new Error(`${error.message}.${detail}`);
    }
    console.log(`boot assertions passed in ${Date.now() - bootedAt}ms`);

    // Bonus assertion — never required to exist (see header).
    if (probe.moduleAlive === false) {
      ledger.record('module-alive', 'window.__intendantModuleAlive is defined but falsy', { fatal: true });
    } else if (probe.moduleAlive === true) {
      console.log('window.__intendantModuleAlive: true');
    }

    await delay(POST_BOOT_SETTLE_MS);

    // Assertion (d): drive the jump-button state machine on a synthetic
    // window. Runs before the ledger verdict so any page error the probe
    // provokes also fails the smoke. Plain boots only: the probe needs real
    // scroll geometry from the Activity pane, and a deep-link boot
    // (#stats / #files legs) routes to a tab that leaves that pane
    // display:none — the hash-less leg hard-asserts the geometry, so an
    // activity-pane regression still fails loudly there.
    if (new URL(opts.url).hash) {
      console.log('jump-button probe: skipped (deep-link boot, activity pane not routed)');
    } else {
      const jump = await evaluate(JUMP_BUTTON_PROBE_EXPRESSION, { awaitPromise: true });
      const jumpSteps = Array.isArray(jump && jump.steps) ? jump.steps : [];
      const jumpFailures = Array.isArray(jump && jump.failures) ? jump.failures : ['probe returned no result'];
      console.log(`jump-button probe: ${jumpSteps.length} steps, ${jumpFailures.length} failures`);
      for (const line of jumpSteps.slice(0, MAX_REPORT_LINES)) console.log(`  ${line}`);
      if (jumpFailures.length > 0) {
        for (const line of jumpFailures) ledger.record('jump-button', line, { fatal: true });
        throw new Error(`jump-button probe failed ${jumpFailures.length} assertion(s)`);
      }

      // Assertion (e): chapter-jump landings on a deep windowed transcript
      // (same synthetic-pane technique and gating as (d) above).
      const chapter = await evaluate(CHAPTER_JUMP_PROBE_EXPRESSION, { awaitPromise: true });
      const chapterSteps = Array.isArray(chapter && chapter.steps) ? chapter.steps : [];
      const chapterFailures = Array.isArray(chapter && chapter.failures) ? chapter.failures : ['probe returned no result'];
      console.log(`chapter-jump probe: ${chapterSteps.length} steps, ${chapterFailures.length} failures`);
      for (const line of chapterSteps.slice(0, MAX_REPORT_LINES)) console.log(`  ${line}`);
      if (chapterFailures.length > 0) {
        for (const line of chapterFailures) ledger.record('chapter-jump', line, { fatal: true });
        throw new Error(`chapter-jump probe failed ${chapterFailures.length} assertion(s)`);
      }
    }

    if (ledger.allowed.length > 0) {
      console.log(`allowed network-level noise (${ledger.allowed.length} entries):`);
      for (const line of ledger.allowed.slice(0, MAX_REPORT_LINES)) console.log(`  ${line}`);
    }
    if (ledger.fatal.length > 0) {
      throw new Error(`page reported ${ledger.fatal.length} boot error(s)`);
    }
    console.log('DASHBOARD BOOT SMOKE PASS');
    verdict = 0;
  } catch (error) {
    console.error(`DASHBOARD BOOT SMOKE FAIL: ${error.message}`);
    for (const line of ledger.fatal.slice(0, MAX_REPORT_LINES)) console.error(`  ${line}`);
    if (ledger.fatal.length > MAX_REPORT_LINES) {
      console.error(`  … ${ledger.fatal.length - MAX_REPORT_LINES} more`);
    }
    if (browserStderr.length > 0) {
      console.error('browser stderr (tail):');
      for (const line of browserStderr.slice(-8)) console.error(`  ${line}`);
    }
    if (opts.artifactDir) {
      try {
        fs.mkdirSync(opts.artifactDir, { recursive: true });
        fs.writeFileSync(path.join(opts.artifactDir, 'browser-stderr.log'), browserStderr.join('\n') + '\n');
        fs.writeFileSync(
          path.join(opts.artifactDir, 'boot-errors.log'),
          [...ledger.fatal.map((l) => `FATAL ${l}`), ...ledger.allowed.map((l) => `allowed ${l}`)].join('\n') + '\n',
        );
        if (cdp && sessionId) {
          const shot = await cdp.send('Page.captureScreenshot', { format: 'png' }, sessionId).catch(() => null);
          if (shot && shot.data) {
            const shotPath = path.join(opts.artifactDir, 'boot-failure.png');
            fs.writeFileSync(shotPath, Buffer.from(shot.data, 'base64'));
            console.error(`failure screenshot: ${shotPath}`);
          }
        }
      } catch (artifactError) {
        console.error(`artifact capture failed: ${artifactError.message}`);
      }
    }
  } finally {
    if (cdp) {
      await Promise.race([cdp.send('Browser.close'), delay(1000)]).catch(() => {});
      cdp.close();
    }
    if (child.exitCode === null) {
      child.kill('SIGTERM');
      await Promise.race([new Promise((resolve) => child.once('exit', resolve)), delay(2000)]);
      if (child.exitCode === null) child.kill('SIGKILL');
    }
    try {
      fs.rmSync(userDataDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    } catch (cleanupError) {
      // Chromium helper processes can still be flushing the profile dir
      // after the kill above; teardown must never outvote the verdict.
      console.error(`profile-dir cleanup failed (ignored): ${cleanupError.message}`);
    }
  }
  process.exit(verdict);
}

main().catch((error) => {
  console.error(`DASHBOARD BOOT SMOKE FAIL: ${error.message}`);
  process.exit(1);
});

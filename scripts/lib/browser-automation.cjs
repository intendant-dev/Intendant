#!/usr/bin/env node
'use strict';

const fs = require('fs');
const http = require('http');
const https = require('https');
const os = require('os');
const path = require('path');
const { EventEmitter } = require('events');
const { spawn } = require('child_process');

const DEFAULT_CDP_TIMEOUT_MS = 15000;
const DEFAULT_CDP_COMMAND_TIMEOUT_MS = 8000;
const DEFAULT_EVALUATE_TIMEOUT_MS = 90000;
const DEFAULT_POLL_EVALUATE_TIMEOUT_MS = 2000;
const BROWSER_EXECUTABLE_ENVS = [
  'INTENDANT_BROWSER_WORKSPACE_EXECUTABLE',
  'INTENDANT_BROWSER_EXECUTABLE',
  'CHROME_PATH',
  'CHROME_BIN',
];

function loadPlaywright() {
  const candidates = ['playwright'];
  if (process.env.PLAYWRIGHT_NODE_PATH) {
    candidates.push(path.join(process.env.PLAYWRIGHT_NODE_PATH, 'playwright'));
    candidates.push(path.join(process.env.PLAYWRIGHT_NODE_PATH, 'node_modules', 'playwright'));
  }
  if (process.env.NODE_PATH) {
    for (const entry of process.env.NODE_PATH.split(path.delimiter).filter(Boolean)) {
      candidates.push(path.join(entry, 'playwright'));
    }
  }
  for (const candidate of candidates) {
    try {
      return require(candidate);
    } catch (err) {
      if (err && err.code !== 'MODULE_NOT_FOUND') throw err;
    }
  }
  return null;
}

async function launchBrowser(opts = {}) {
  const playwright = loadPlaywright();
  if (playwright) {
    return launchPlaywrightBrowser(playwright, opts);
  }
  return launchCdpBrowser(opts);
}

async function launchPlaywrightBrowser(playwright, opts) {
  const browser = await playwright.chromium.launch({ headless: opts.headless !== false });
  const context = await browser.newContext({ ignoreHTTPSErrors: Boolean(opts.ignoreHTTPSErrors) });
  return {
    kind: 'playwright',
    async newPage() {
      return context.newPage();
    },
    async close() {
      await browser.close();
    },
  };
}

async function launchCdpBrowser(opts = {}) {
  const executable = resolveBrowserExecutable(opts.executable);
  const userDataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-connect-cdp-'));
  const args = browserArgs(userDataDir, opts);
  const child = spawn(executable, args, {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', chunk => {
    stderr = trimLog(stderr + String(chunk), 12000);
  });
  const childExit = new Promise(resolve => child.once('exit', resolve));
  let connection = null;
  try {
    const { port, wsPath } = await waitForDevToolsPort(userDataDir, child, () => stderr, DEFAULT_CDP_TIMEOUT_MS);
    connection = new CdpConnection(await openWebSocket(`ws://127.0.0.1:${port}${wsPath}`, DEFAULT_CDP_TIMEOUT_MS));
    return new CdpBrowser(connection, child, childExit, userDataDir, () => stderr, Boolean(opts.ignoreHTTPSErrors));
  } catch (err) {
    if (connection) connection.close();
    if (!child.killed) child.kill('SIGTERM');
    await Promise.race([childExit, delay(1000)]).catch(() => {});
    removeDir(userDataDir);
    throw err;
  }
}

function browserArgs(userDataDir, opts) {
  const args = [
    '--remote-debugging-port=0',
    `--user-data-dir=${userDataDir}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-background-networking',
    '--disable-dev-shm-usage',
    '--disable-extensions',
    '--disable-gpu',
    '--disable-popup-blocking',
    '--window-size=1440,1000',
  ];
  if (opts.headless !== false) {
    args.push('--headless=new');
  }
  if (opts.noSandbox) {
    args.push('--no-sandbox');
  }
  if (opts.ignoreHTTPSErrors) {
    args.push('--ignore-certificate-errors');
    args.push('--allow-insecure-localhost');
  }
  if (Array.isArray(opts.browserArgs)) {
    args.push(...opts.browserArgs);
  }
  return args;
}

function resolveBrowserExecutable(explicit) {
  const candidates = [];
  if (explicit) candidates.push(explicit);
  for (const envName of BROWSER_EXECUTABLE_ENVS) {
    if (process.env[envName]) candidates.push(process.env[envName]);
  }
  if (process.platform === 'darwin') {
    candidates.push(...systemBrowserCandidates());
    candidates.push(...managedBrowserCandidates());
  } else {
    candidates.push(...managedBrowserCandidates());
    candidates.push(...systemBrowserCandidates());
  }
  for (const candidate of candidates) {
    if (candidate && isExecutableFile(candidate)) {
      return candidate;
    }
  }
  throw new Error(
    'Playwright is not installed and no Chromium executable was found. Set INTENDANT_BROWSER_WORKSPACE_EXECUTABLE, INTENDANT_BROWSER_EXECUTABLE, CHROME_PATH, or CHROME_BIN.'
  );
}

function managedBrowserCandidates() {
  const home = os.homedir();
  const roots = [];
  const cacheRoot = process.env.XDG_CACHE_HOME || (home ? path.join(home, '.cache') : '');
  const dataRoot = process.env.XDG_DATA_HOME || (home ? path.join(home, '.local', 'share') : '');
  if (process.platform === 'darwin' && home) {
    roots.push(path.join(home, 'Library', 'Caches', 'ms-playwright'));
    roots.push(path.join(home, 'Library', 'Caches', 'puppeteer'));
    roots.push(path.join(home, 'Library', 'Caches', 'chrome-for-testing'));
    roots.push(path.join(home, 'Library', 'Caches', 'intendant', 'browser-workspaces'));
    roots.push(path.join(home, 'Library', 'Application Support', 'intendant', 'browser-workspaces'));
  }
  if (cacheRoot) {
    roots.push(path.join(cacheRoot, 'ms-playwright'));
    roots.push(path.join(cacheRoot, 'puppeteer'));
    roots.push(path.join(cacheRoot, 'chrome-for-testing'));
    roots.push(path.join(cacheRoot, 'intendant', 'browser-workspaces'));
  }
  if (dataRoot) {
    roots.push(path.join(dataRoot, 'intendant', 'browser-workspaces'));
    roots.push(path.join(dataRoot, 'intendant', 'browsers'));
  }
  const names =
    process.platform === 'win32'
      ? ['chrome.exe', 'msedge.exe', 'chromium.exe']
      : process.platform === 'darwin'
        ? ['Google Chrome for Testing', 'Chromium', 'chrome']
        : ['chrome', 'chromium', 'chromium-browser', 'google-chrome'];
  return roots.flatMap(root => findExecutablesUnder(root, names, 8));
}

function systemBrowserCandidates() {
  if (process.platform === 'darwin') {
    return [
      '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
      '/Applications/Chromium.app/Contents/MacOS/Chromium',
      '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
      '/Applications/Brave Browser.app/Contents/MacOS/Brave Browser',
    ];
  }
  if (process.platform === 'win32') {
    const roots = [
      process.env.PROGRAMFILES,
      process.env['PROGRAMFILES(X86)'],
      process.env.LOCALAPPDATA,
    ].filter(Boolean);
    const rels = [
      ['Google', 'Chrome', 'Application', 'chrome.exe'],
      ['Microsoft', 'Edge', 'Application', 'msedge.exe'],
      ['Chromium', 'Application', 'chrome.exe'],
    ];
    return roots.flatMap(root => rels.map(rel => path.join(root, ...rel)));
  }
  return whichCandidates(['google-chrome', 'chrome', 'chromium', 'chromium-browser', 'msedge', 'brave-browser']);
}

function whichCandidates(names) {
  const dirs = String(process.env.PATH || '').split(path.delimiter).filter(Boolean);
  return dirs.flatMap(dir => names.map(name => path.join(dir, name)));
}

function findExecutablesUnder(root, names, maxDepth) {
  if (!root || maxDepth < 0 || !fs.existsSync(root)) return [];
  let entries;
  try {
    entries = fs.readdirSync(root, { withFileTypes: true });
  } catch (_) {
    return [];
  }
  entries.sort((a, b) => a.name.localeCompare(b.name));
  const found = [];
  for (const entry of entries) {
    const full = path.join(root, entry.name);
    if (entry.isFile() && names.includes(entry.name) && isExecutableFile(full)) {
      found.push(full);
    } else if (entry.isDirectory()) {
      found.push(...findExecutablesUnder(full, names, maxDepth - 1));
    }
  }
  return found;
}

function isExecutableFile(candidate) {
  try {
    const stat = fs.statSync(candidate);
    if (!stat.isFile()) return false;
    return process.platform === 'win32' || Boolean(stat.mode & 0o111);
  } catch (_) {
    return false;
  }
}

async function waitForDevToolsPort(userDataDir, child, stderr, timeoutMs) {
  const activePortPath = path.join(userDataDir, 'DevToolsActivePort');
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`Chromium exited before CDP was ready${formatStderr(stderr())}`);
    }
    if (fs.existsSync(activePortPath)) {
      const lines = fs.readFileSync(activePortPath, 'utf8').trim().split(/\r?\n/);
      const port = Number(lines[0]);
      if (Number.isFinite(port) && port > 0) {
        return { port, wsPath: lines[1] || '/devtools/browser' };
      }
    }
    await delay(80);
  }
  throw new Error(`CDP was not ready within ${timeoutMs}ms${formatStderr(stderr())}`);
}

async function openWebSocket(wsUrl, timeoutMs) {
  const Ws = globalThis.WebSocket || loadOptionalWs();
  if (typeof Ws !== 'function') {
    throw new Error('CDP fallback requires Node with global WebSocket support or the optional `ws` package.');
  }
  return new Promise((resolve, reject) => {
    const ws = new Ws(wsUrl);
    const timer = setTimeout(() => {
      try {
        ws.close();
      } catch (_) {}
      reject(new Error(`CDP WebSocket did not open within ${timeoutMs}ms`));
    }, timeoutMs);
    addWsListener(ws, 'open', () => {
      clearTimeout(timer);
      resolve(ws);
    });
    addWsListener(ws, 'error', event => {
      clearTimeout(timer);
      reject(event && (event.error || event.message) ? (event.error || event.message) : event);
    });
  });
}

function loadOptionalWs() {
  try {
    return require('ws');
  } catch (_) {
    return null;
  }
}

function addWsListener(ws, eventName, callback) {
  if (typeof ws.addEventListener === 'function') {
    ws.addEventListener(eventName, callback);
  } else if (typeof ws.on === 'function') {
    ws.on(eventName, callback);
  }
}

class CdpConnection extends EventEmitter {
  constructor(socket) {
    super();
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    addWsListener(socket, 'message', event => this.handleMessage(event && event.data !== undefined ? event.data : event));
    addWsListener(socket, 'close', () => this.rejectAll(new Error('CDP WebSocket closed')));
    addWsListener(socket, 'error', event => this.rejectAll(event && (event.error || event.message) ? (event.error || event.message) : event));
  }

  send(method, params = {}, sessionId, timeoutMs = DEFAULT_CDP_COMMAND_TIMEOUT_MS) {
    const id = this.nextId;
    this.nextId += 1;
    const payload = { id, method, params };
    if (sessionId) payload.sessionId = sessionId;
    this.socket.send(JSON.stringify(payload));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.pending.delete(id)) {
          reject(new Error(`CDP ${method} timed out`));
        }
      }, timeoutMs);
      if (typeof timer.unref === 'function') timer.unref();
      this.pending.set(id, { resolve, reject, timer });
    });
  }

  handleMessage(raw) {
    let text;
    if (typeof raw === 'string') {
      text = raw;
    } else if (Buffer.isBuffer(raw)) {
      text = raw.toString('utf8');
    } else if (raw instanceof ArrayBuffer) {
      text = Buffer.from(raw).toString('utf8');
    } else if (ArrayBuffer.isView(raw)) {
      text = Buffer.from(raw.buffer, raw.byteOffset, raw.byteLength).toString('utf8');
    } else {
      text = String(raw || '');
    }
    let message;
    try {
      message = JSON.parse(text);
    } catch (_) {
      return;
    }
    if (message.id) {
      if (this.pending.has(message.id)) {
        const pending = this.pending.get(message.id);
        this.pending.delete(message.id);
        clearTimeout(pending.timer);
        if (message.error) {
          pending.reject(new Error(message.error.message || JSON.stringify(message.error)));
        } else {
          pending.resolve(message.result || {});
        }
      }
      return;
    }
    this.emit('event', message);
  }

  rejectAll(error) {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error instanceof Error ? error : new Error(String(error)));
    }
    this.pending.clear();
  }

  close() {
    try {
      this.socket.close();
    } catch (_) {}
  }
}

class CdpBrowser {
  constructor(connection, child, childExit, userDataDir, stderr, ignoreHTTPSErrors) {
    this.connection = connection;
    this.child = child;
    this.childExit = childExit;
    this.userDataDir = userDataDir;
    this.stderr = stderr;
    this.ignoreHTTPSErrors = ignoreHTTPSErrors;
    this.closed = false;
    this.kind = 'cdp';
  }

  async newPage() {
    const { targetId } = await this.connection.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await this.connection.send('Target.attachToTarget', {
      targetId,
      flatten: true,
    });
    const page = new CdpPage(this.connection, sessionId, targetId, this.ignoreHTTPSErrors);
    await page.init();
    return page;
  }

  async close() {
    if (this.closed) return;
    this.closed = true;
    this.connection.close();
    if (!this.child.killed && this.child.exitCode === null) {
      this.child.kill('SIGTERM');
    }
    await Promise.race([this.childExit, delay(1500)]).catch(() => {});
    if (this.child.exitCode === null) {
      this.child.kill('SIGKILL');
      await Promise.race([this.childExit, delay(1500)]).catch(() => {});
    }
    removeDir(this.userDataDir);
  }
}

class CdpPage {
  constructor(connection, sessionId, targetId, ignoreHTTPSErrors) {
    this.connection = connection;
    this.sessionId = sessionId;
    this.targetId = targetId;
    this.ignoreHTTPSErrors = ignoreHTTPSErrors;
    this.consoleHandlers = [];
    this.eventWaiters = [];
    this.lastDocumentStatus = null;
    this.onCdpEvent = message => this.handleEvent(message);
    this.connection.on('event', this.onCdpEvent);
  }

  async init() {
    await this.connection.send('Page.enable', {}, this.sessionId);
    await this.connection.send('Runtime.enable', {}, this.sessionId);
    await this.connection.send('Network.enable', {}, this.sessionId);
    if (this.ignoreHTTPSErrors) {
      await this.connection.send('Security.enable', {}, this.sessionId).catch(() => {});
      await this.connection.send('Security.setIgnoreCertificateErrors', { ignore: true }, this.sessionId).catch(() => {});
    }
  }

  on(eventName, handler) {
    if (eventName === 'console') {
      this.consoleHandlers.push(handler);
    }
  }

  async goto(url, opts = {}) {
    this.lastDocumentStatus = null;
    const timeoutMs = opts.timeout || DEFAULT_CDP_TIMEOUT_MS;
    const observedDocument = this.waitForPageEvent(
      'Network.responseReceived',
      timeoutMs,
      params => params?.type === 'Document'
    )
      .then(value => ({ kind: 'document', value }))
      .catch(error => ({ kind: 'document_error', error }));
    const navigation = this.connection.send('Page.navigate', { url }, this.sessionId, timeoutMs)
      .then(value => ({ kind: 'navigate', value }))
      .catch(error => ({ kind: 'navigate_error', error }));
    const first = await Promise.race([observedDocument, navigation]);
    if (first.kind === 'navigate_error') {
      if (!String(first.error && first.error.message || first.error).includes('timed out')) {
        throw first.error;
      }
      const documentResult = await observedDocument;
      if (documentResult.kind === 'document_error') throw documentResult.error;
    } else if (first.kind === 'navigate') {
      if (first.value && first.value.errorText) {
        throw new Error(`navigation failed: ${first.value.errorText}`);
      }
      const documentResult = await observedDocument;
      if (documentResult.kind === 'document_error' && !this.lastDocumentStatus) {
        throw documentResult.error;
      }
    } else if (first.kind === 'document_error') {
      throw first.error;
    }
    return {
      status: () => this.lastDocumentStatus || 0,
    };
  }

  async evaluate(fnOrExpression, opts = {}) {
    const expression =
      typeof fnOrExpression === 'function'
        ? `(${fnOrExpression.toString()})()`
        : String(fnOrExpression);
    const result = await this.connection.send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
    }, this.sessionId, opts.timeoutMs || DEFAULT_EVALUATE_TIMEOUT_MS);
    if (result.exceptionDetails) {
      const text = result.exceptionDetails.text || result.exceptionDetails.exception?.description || 'evaluation failed';
      throw new Error(text);
    }
    return result.result ? result.result.value : undefined;
  }

  async waitForFunction(fnOrExpression, opts = {}) {
    const timeoutMs = Number(opts.timeout || DEFAULT_CDP_TIMEOUT_MS);
    const deadline = Date.now() + timeoutMs;
    let lastError = null;
    while (Date.now() < deadline) {
      try {
        if (await this.evaluate(fnOrExpression, { timeoutMs: DEFAULT_POLL_EVALUATE_TIMEOUT_MS })) return true;
      } catch (err) {
        lastError = err;
      }
      await delay(100);
    }
    throw new Error(`timed out waiting for browser function${lastError ? `: ${lastError.message}` : ''}`);
  }

  async waitForTimeout(ms) {
    await delay(ms);
  }

  async waitForLoadState() {
    await delay(0);
  }

  async close() {
    this.connection.off('event', this.onCdpEvent);
    await this.connection.send('Target.closeTarget', { targetId: this.targetId }).catch(() => {});
  }

  waitForPageEvent(method, timeoutMs, predicate = () => true) {
    return new Promise((resolve, reject) => {
      const waiter = { method, predicate, resolve, reject };
      waiter.timer = setTimeout(() => {
        this.eventWaiters = this.eventWaiters.filter(item => item !== waiter);
        reject(new Error(`timed out waiting for ${method}`));
      }, timeoutMs);
      this.eventWaiters.push(waiter);
    });
  }

  handleEvent(message) {
    if (message.sessionId !== this.sessionId) return;
    if (message.method === 'Runtime.consoleAPICalled') {
      const params = message.params || {};
      const text = (params.args || []).map(formatRemoteValue).join(' ');
      const consoleMessage = {
        type: () => params.type || 'log',
        text: () => text,
      };
      for (const handler of this.consoleHandlers) handler(consoleMessage);
    }
    if (message.method === 'Network.responseReceived' && message.params?.type === 'Document') {
      this.lastDocumentStatus = Number(message.params.response?.status || 0) || this.lastDocumentStatus;
    }
    const matching = this.eventWaiters.filter(waiter => {
      if (waiter.method !== message.method) return false;
      try {
        return waiter.predicate(message.params || {});
      } catch (_) {
        return false;
      }
    });
    if (matching.length > 0) {
      this.eventWaiters = this.eventWaiters.filter(waiter => !matching.includes(waiter));
      for (const waiter of matching) {
        clearTimeout(waiter.timer);
        waiter.resolve(message.params || {});
      }
    }
  }
}

function formatRemoteValue(arg) {
  if (!arg) return '';
  if (arg.unserializableValue !== undefined) return String(arg.unserializableValue);
  if (arg.value !== undefined) {
    if (typeof arg.value === 'object') return JSON.stringify(arg.value);
    return String(arg.value);
  }
  return arg.description || arg.type || '';
}

function httpStatus(url, opts = {}) {
  return httpGet(url, opts).then(resp => resp.status);
}

async function httpJson(url, opts = {}) {
  const resp = await httpGet(url, opts);
  if (resp.status < 200 || resp.status >= 300) {
    throw new Error(`GET ${url} returned ${resp.status}`);
  }
  try {
    return JSON.parse(resp.body);
  } catch (err) {
    throw new Error(`GET ${url} returned invalid JSON: ${err.message}`);
  }
}

function httpGet(url, opts = {}) {
  return new Promise((resolve, reject) => {
    const parsed = new URL(url);
    const client = parsed.protocol === 'https:' ? https : http;
    const req = client.request(parsed, {
      method: 'GET',
      rejectUnauthorized: opts.ignoreHTTPSErrors === true ? false : undefined,
    }, res => {
      const chunks = [];
      res.on('data', chunk => chunks.push(chunk));
      res.on('end', () => {
        resolve({
          status: res.statusCode || 0,
          headers: res.headers,
          body: Buffer.concat(chunks).toString('utf8'),
        });
      });
    });
    req.on('error', reject);
    req.setTimeout(opts.timeoutMs || 10000, () => {
      req.destroy(new Error(`GET ${url} timed out`));
    });
    req.end();
  });
}

function trimLog(text, max) {
  if (text.length <= max) return text;
  return text.slice(text.length - max);
}

function formatStderr(stderr) {
  const lines = String(stderr || '').trim().split(/\r?\n/).filter(Boolean).slice(-2);
  return lines.length ? `; ${lines.join('; ')}` : '';
}

function removeDir(dir) {
  try {
    fs.rmSync(dir, { recursive: true, force: true });
  } catch (_) {}
}

function delay(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

module.exports = {
  httpGet,
  httpJson,
  httpStatus,
  launchBrowser,
};

#!/usr/bin/env node
'use strict';

const assert = require('assert');
const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');
const { spawn, spawnSync } = require('child_process');
const { httpStatus, launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_DAEMON_PORT = 8876;
const DEFAULT_RENDEZVOUS_PORT = 9876;
const DEFAULT_DAEMON_ID = 'connect-e2e-daemon';
const DEFAULT_CONNECT_TOKEN = 'connect-e2e-token';
const START_TIMEOUT_MS = 30000;
const CONNECT_TIMEOUT_MS = 30000;
const RENDEZVOUS_TEST_USER_ID = 'rendezvous-user-123';
const RENDEZVOUS_TEST_ACCOUNT_NAME = 'rendezvous-e2e';
const TAMPERED_DAEMON_PUBLIC_KEY = 'tampered-daemon-public-key';
const TAMPERED_SESSION_GRANT = 'tampered-connect-session-grant';
const TAMPERED_CLIENT_NONCE = 'tampered-connect-client-nonce';
const FRAME_FIXTURE_PNG_BASE64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=';

function parseArgs(argv) {
  const repoRoot = path.resolve(__dirname, '..');
  const out = {
    repoRoot,
    dashboardBinary: path.join(repoRoot, 'target', 'release', 'intendant'),
    daemonPort: DEFAULT_DAEMON_PORT,
    rendezvousPort: DEFAULT_RENDEZVOUS_PORT,
    daemonId: DEFAULT_DAEMON_ID,
    connectToken: DEFAULT_CONNECT_TOKEN,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--dashboard-binary') {
      out.dashboardBinary = path.resolve(argv[++i]);
    } else if (arg === '--daemon-port') {
      out.daemonPort = Number(argv[++i]);
    } else if (arg === '--rendezvous-port') {
      out.rendezvousPort = Number(argv[++i]);
    } else if (arg === '--daemon-id') {
      out.daemonId = String(argv[++i] || '').trim();
    } else if (arg === '--connect-token') {
      out.connectToken = String(argv[++i] || '').trim();
    } else if (arg === '--help' || arg === '-h') {
      console.log(`Usage:
  node scripts/validate-connect-rendezvous.cjs [options]

Options:
  --dashboard-binary <path>   Intendant binary to launch.
  --daemon-port <port>        Fresh daemon web port. Default ${DEFAULT_DAEMON_PORT}.
  --rendezvous-port <port>    Local public-origin emulator port. Default ${DEFAULT_RENDEZVOUS_PORT}.
  --daemon-id <id>            Rendezvous daemon id. Default ${DEFAULT_DAEMON_ID}.
  --connect-token <token>     Bearer token required for daemon rendezvous endpoints. Default ${DEFAULT_CONNECT_TOKEN}.
`);
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  assert(Number.isInteger(out.daemonPort) && out.daemonPort > 0, 'invalid daemon port');
  assert(Number.isInteger(out.rendezvousPort) && out.rendezvousPort > 0, 'invalid rendezvous port');
  assert(out.daemonId, 'daemon id is required');
  assert(out.connectToken, 'connect token is required');
  return out;
}

function sendJson(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, {
    'content-type': 'application/json',
    'content-length': Buffer.byteLength(text),
    'cache-control': 'no-store',
  });
  res.end(text);
}

function sendText(res, status, text, contentType = 'text/plain; charset=utf-8') {
  res.writeHead(status, {
    'content-type': contentType,
    'content-length': Buffer.byteLength(text),
    'cache-control': 'no-store',
  });
  res.end(text);
}

function sendFile(res, filePath, contentType) {
  const body = fs.readFileSync(filePath);
  res.writeHead(200, {
    'content-type': contentType || contentTypeForPath(filePath),
    'content-length': body.length,
    'cache-control': 'no-store',
  });
  res.end(body);
}

function contentTypeForPath(filePath) {
  const ext = path.extname(filePath).toLowerCase();
  switch (ext) {
    case '.html': return 'text/html; charset=utf-8';
    case '.js': return 'text/javascript; charset=utf-8';
    case '.mjs': return 'text/javascript; charset=utf-8';
    case '.css': return 'text/css; charset=utf-8';
    case '.wasm': return 'application/wasm';
    case '.json': return 'application/json; charset=utf-8';
    case '.png': return 'image/png';
    case '.jpg':
    case '.jpeg': return 'image/jpeg';
    case '.svg': return 'image/svg+xml';
    case '.ico': return 'image/x-icon';
    default: return 'application/octet-stream';
  }
}

function safeStaticPath(staticRoot, pathname) {
  let decoded;
  try {
    decoded = decodeURIComponent(pathname);
  } catch {
    return null;
  }
  const rel = decoded.replace(/^\/+/, '');
  if (!rel || rel.includes('\0')) return null;
  const candidate = path.resolve(staticRoot, rel);
  const root = path.resolve(staticRoot);
  if (candidate !== root && !candidate.startsWith(root + path.sep)) return null;
  return candidate;
}

async function readJson(req) {
  const chunks = [];
  let total = 0;
  for await (const chunk of req) {
    total += chunk.length;
    if (total > 2 * 1024 * 1024) throw new Error('request body too large');
    chunks.push(chunk);
  }
  if (chunks.length === 0) return {};
  return JSON.parse(Buffer.concat(chunks).toString('utf8'));
}

function requestRefererSearchParams(req) {
  const raw = String(req.headers.referer || '');
  if (!raw) return new URLSearchParams();
  try {
    return new URL(raw, 'http://127.0.0.1').searchParams;
  } catch {
    return new URLSearchParams();
  }
}

function createRendezvousServer(staticRoot, options = {}) {
  const daemons = new Map();
  const events = new Map();
  const pollers = new Map();
  const pendingOffers = new Map();
  const authToken = String(options.authToken || '').trim();
  // The emulator's minimal `/connect` page does not have the production
  // browser identity store, so give it one real P-256 key here. Every
  // synthetic offer is signed end-to-end and the daemon grants this exact
  // fingerprint through a separate local IAM fixture.
  const { publicKey: testClientPublicKey, privateKey: testClientPrivateKey } =
    crypto.generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
  const testClientJwk = testClientPublicKey.export({ format: 'jwk' });
  const testClientRaw = Buffer.concat([
    Buffer.from([0x04]),
    Buffer.from(testClientJwk.x, 'base64url'),
    Buffer.from(testClientJwk.y, 'base64url'),
  ]);
  const testClientKey = testClientRaw.toString('base64url');
  const testClientKeyFingerprint = crypto.createHash('sha256').update(testClientRaw).digest('base64url');
  const server = http.createServer(async (req, res) => {
    try {
      const url = new URL(req.url, 'http://127.0.0.1');
      if (url.pathname.startsWith('/api/daemon/') && authToken) {
        const expected = `Bearer ${authToken}`;
        if (String(req.headers.authorization || '') !== expected) {
          return sendJson(res, 401, { error: 'missing or invalid daemon bearer token' });
        }
      }
      if (req.method === 'GET' && (url.pathname === '/' || url.pathname === '/connect')) {
        return sendText(res, 200, publicBootstrapHtml(), 'text/html; charset=utf-8');
      }
      if (req.method === 'GET' && url.pathname === '/app') {
        return sendFile(res, path.join(staticRoot, 'app.html'), 'text/html; charset=utf-8');
      }
      if (req.method === 'GET' && !url.pathname.startsWith('/api/')) {
        const assetPath = safeStaticPath(staticRoot, url.pathname);
        if (assetPath && fs.existsSync(assetPath) && fs.statSync(assetPath).isFile()) {
          return sendFile(res, assetPath);
        }
      }
      if (req.method === 'GET' && url.pathname === '/api/status') {
        const daemonId = url.searchParams.get('daemon_id') || '';
        const daemon = daemons.get(daemonId) || null;
        return sendJson(res, 200, {
          ok: true,
          daemon_id: daemonId,
          registered: Boolean(daemon),
          daemon_public_key: daemon?.daemonPublicKey || '',
          queued: (events.get(daemonId) || []).length,
          pending_offers: Array.from(pendingOffers.values()).filter(p => p.daemonId === daemonId).length,
          daemon_auth_required: Boolean(authToken),
        });
      }
      if (req.method === 'POST' && url.pathname === '/api/daemon/register') {
        const body = await readJson(req);
        const daemonId = String(body.daemon_id || '').trim();
        if (!daemonId) return sendJson(res, 400, { error: 'missing daemon_id' });
        daemons.set(daemonId, {
          daemonId,
          daemonPublicKey: String(body.daemon_public_key || ''),
          registeredAt: Date.now(),
        });
        return sendJson(res, 200, { ok: true });
      }
      if (req.method === 'GET' && url.pathname === '/api/daemon/next') {
        const daemonId = String(url.searchParams.get('daemon_id') || '').trim();
        if (!daemonId) return sendJson(res, 400, { error: 'missing daemon_id' });
        const queue = events.get(daemonId) || [];
        if (queue.length > 0) return sendJson(res, 200, queue.shift());
        const timeoutMs = Math.min(Number(url.searchParams.get('timeout_ms') || 15000), 30000);
        let settled = false;
        const timer = setTimeout(() => {
          if (settled) return;
          settled = true;
          clearPoller(daemonId, res);
          res.writeHead(204);
          res.end();
        }, timeoutMs);
        const waiter = { res, timer };
        if (!pollers.has(daemonId)) pollers.set(daemonId, []);
        pollers.get(daemonId).push(waiter);
        req.on('close', () => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          clearPoller(daemonId, res);
        });
        return;
      }
      if (req.method === 'POST' && url.pathname === '/api/daemon/answer') {
        const body = await readJson(req);
        const offer = pendingOffers.get(String(body.request_id || ''));
        if (!offer) return sendJson(res, 404, { error: 'offer not found' });
        const daemon = daemons.get(offer.daemonId);
        if (!daemon) {
          pendingOffers.delete(offer.id);
          clearTimeout(offer.timer);
          sendJson(offer.res, 410, { error: 'daemon registration expired' });
          return sendJson(res, 410, { error: 'daemon registration expired' });
        }
        pendingOffers.delete(offer.id);
        clearTimeout(offer.timer);
        const advertisedDaemonPublicKey = offer.tamperRegisteredKey
          ? TAMPERED_DAEMON_PUBLIC_KEY
          : daemon.daemonPublicKey;
        const advertisedSessionGrant = offer.tamperSessionGrant
          ? TAMPERED_SESSION_GRANT
          : offer.sessionGrant;
        sendJson(offer.res, 200, {
          ok: true,
          session_id: body.session_id,
          sdp: body.sdp,
          binding: body.binding,
          daemon_public_key: advertisedDaemonPublicKey,
          session_grant: advertisedSessionGrant,
        });
        return sendJson(res, 200, { ok: true });
      }
      if (req.method === 'POST' && url.pathname === '/api/daemon/error') {
        const body = await readJson(req);
        const offer = pendingOffers.get(String(body.request_id || ''));
        if (offer) {
          pendingOffers.delete(offer.id);
          clearTimeout(offer.timer);
          sendJson(offer.res, 502, { error: String(body.error || 'daemon error') });
        }
        return sendJson(res, 200, { ok: true });
      }
      if (req.method === 'POST' && url.pathname === '/api/daemon/ack') {
        await readJson(req);
        return sendJson(res, 200, { ok: true });
      }
      if (req.method === 'POST' && url.pathname === '/api/browser/offer') {
        const body = await readJson(req);
        const daemonId = String(body.daemon_id || '').trim();
        const sdp = String(body.sdp || '');
        if (!daemonId || !sdp.trim()) return sendJson(res, 400, { error: 'missing daemon_id or sdp' });
        if (!daemons.has(daemonId)) return sendJson(res, 404, { error: 'daemon not registered' });
        const refererParams = requestRefererSearchParams(req);
        const tamperRegisteredKey = refererParams.get('tamper_registered_key') === '1';
        const tamperSessionGrant = refererParams.get('tamper_session_grant') === '1';
        const tamperClientNonce = refererParams.get('tamper_client_nonce') === '1';
        const id = crypto.randomUUID();
        const sessionGrant = `connect-session-grant-${crypto.randomUUID()}`;
        const clientNonce = String(body.client_nonce || '').trim();
        const clientKeyTs = Date.now();
        const sdpDigest = crypto.createHash('sha256').update(sdp).digest('base64url');
        const clientKeyPayload = [
          'intendant-client-key-offer-v1',
          daemonId,
          clientNonce,
          sdpDigest,
          String(clientKeyTs),
        ].join('\n');
        const clientKeySig = crypto.sign('sha256', Buffer.from(clientKeyPayload), {
          key: testClientPrivateKey,
          dsaEncoding: 'ieee-p1363',
        }).toString('base64url');
        const browserSuppliedKey = String(body.client_key || '').trim();
        const timer = setTimeout(() => {
          if (!pendingOffers.has(id)) return;
          pendingOffers.delete(id);
          sendJson(res, 504, { error: 'timed out waiting for daemon answer' });
        }, CONNECT_TIMEOUT_MS);
        pendingOffers.set(id, {
          id,
          daemonId,
          res,
          timer,
          tamperRegisteredKey,
          tamperSessionGrant,
          tamperClientNonce,
          sessionGrant,
          clientNonce,
        });
        res.on('close', () => {
          clearTimeout(timer);
          pendingOffers.delete(id);
        });
        enqueueEvent(daemonId, {
          id,
          kind: 'offer',
          sdp,
          session_grant: sessionGrant,
          client_nonce: tamperClientNonce ? TAMPERED_CLIENT_NONCE : clientNonce,
          client_key: browserSuppliedKey || testClientKey,
          client_key_sig: browserSuppliedKey ? body.client_key_sig : clientKeySig,
          client_key_ts: browserSuppliedKey ? body.client_key_ts : clientKeyTs,
          ...(browserSuppliedKey && body.client_key_proto ? { client_key_proto: body.client_key_proto } : {}),
          ...(browserSuppliedKey && body.client_key_account_user_id ? { client_key_account_user_id: body.client_key_account_user_id } : {}),
          ...(browserSuppliedKey && body.client_key_account_name ? { client_key_account_name: body.client_key_account_name } : {}),
          user_id: RENDEZVOUS_TEST_USER_ID,
          account_name: RENDEZVOUS_TEST_ACCOUNT_NAME,
        });
        return;
      }
      if (req.method === 'POST' && url.pathname === '/api/browser/ice') {
        const body = await readJson(req);
        const daemonId = String(body.daemon_id || '').trim();
        const sessionId = String(body.session_id || '').trim();
        if (!daemonId || !sessionId) return sendJson(res, 400, { error: 'missing daemon_id or session_id' });
        enqueueEvent(daemonId, {
          id: crypto.randomUUID(),
          kind: 'ice',
          session_id: sessionId,
          candidate: body.candidate || {},
        });
        return sendJson(res, 200, { ok: true });
      }
      if (req.method === 'POST' && url.pathname === '/api/browser/close') {
        const body = await readJson(req);
        const daemonId = String(body.daemon_id || '').trim();
        const sessionId = String(body.session_id || '').trim();
        if (daemonId && sessionId) {
          enqueueEvent(daemonId, {
            id: crypto.randomUUID(),
            kind: 'close',
            session_id: sessionId,
          });
        }
        return sendJson(res, 200, { ok: true });
      }
      return sendJson(res, 404, { error: 'not found' });
    } catch (err) {
      return sendJson(res, 500, { error: err && err.message || String(err) });
    }
  });

  function clearPoller(daemonId, res) {
    const list = pollers.get(daemonId) || [];
    const next = list.filter(p => p.res !== res);
    if (next.length > 0) pollers.set(daemonId, next);
    else pollers.delete(daemonId);
  }

  function enqueueEvent(daemonId, event) {
    const list = pollers.get(daemonId) || [];
    if (list.length > 0) {
      const waiter = list.shift();
      if (list.length > 0) pollers.set(daemonId, list);
      else pollers.delete(daemonId);
      clearTimeout(waiter.timer);
      return sendJson(waiter.res, 200, event);
    }
    if (!events.has(daemonId)) events.set(daemonId, []);
    events.get(daemonId).push(event);
  }

  server.testClientKeyFingerprint = testClientKeyFingerprint;
  return server;
}

function publicBootstrapHtml() {
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Intendant Connect Rendezvous E2E</title>
</head>
<body>
<pre id="status">starting</pre>
<script>
(() => {
  const statusEl = document.getElementById('status');
  const daemonId = new URLSearchParams(location.search).get('daemon_id') || '${DEFAULT_DAEMON_ID}';
  const MAX_CHUNKED_RESPONSE_BYTES = 128 * 1024 * 1024;
  const MAX_BYTE_STREAM_BYTES = 128 * 1024 * 1024;
  const UPLOAD_CHUNK_BYTES = 16 * 1024;
  const UPLOAD_BUFFER_HIGH_BYTES = 1024 * 1024;
  function paint(value) {
    statusEl.textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
  }
  function bytesToBase64Url(bytes) {
    let binary = '';
    for (const b of bytes) binary += String.fromCharCode(b);
    return btoa(binary).replace(/\\+/g, '-').replace(/\\//g, '_').replace(/=+$/g, '');
  }
  function base64UrlToBytes(value) {
    const normalized = String(value || '').replace(/-/g, '+').replace(/_/g, '/');
    const binary = atob(normalized.padEnd(Math.ceil(normalized.length / 4) * 4, '='));
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }
  function base64ToBytes(value) {
    const binary = atob(String(value || ''));
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }
  function bytesToBase64(bytes) {
    let binary = '';
    for (let i = 0; i < bytes.byteLength; i++) binary += String.fromCharCode(bytes[i]);
    return btoa(binary);
  }
  async function sha256B64u(text) {
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(String(text)));
    return bytesToBase64Url(new Uint8Array(digest));
  }
  function bindingPayload(binding) {
    const parts = [
      binding.protocol || '',
      binding.session_id || '',
      binding.daemon_public_key || '',
      String(binding.created_unix_ms || ''),
      String(binding.expires_unix_ms || ''),
      binding.offer_sha256 || '',
      binding.answer_sha256 || '',
    ];
    if (binding.client_nonce) parts.push(binding.client_nonce);
    if (binding.session_grant_sha256) parts.push(binding.session_grant_sha256);
    return parts.join('\\n');
  }
  async function verifyEd25519(publicKeyBytes, signatureBytes, payloadBytes) {
    let key;
    try {
      key = await crypto.subtle.importKey('raw', publicKeyBytes, { name: 'Ed25519' }, false, ['verify']);
    } catch (firstErr) {
      try {
        key = await crypto.subtle.importKey('raw', publicKeyBytes, 'Ed25519', false, ['verify']);
      } catch {
        throw firstErr;
      }
    }
    return crypto.subtle.verify({ name: 'Ed25519' }, key, signatureBytes, payloadBytes);
  }
  async function verifyBinding(binding, sessionId, offerSdp, answerSdp, sessionGrant = '', clientNonce = '') {
    if (!binding || typeof binding !== 'object') return { ok: false, error: 'missing binding' };
    if (binding.protocol !== 'intendant-dashboard-control-v1') return { ok: false, error: 'unexpected protocol' };
    if (String(binding.session_id || '') !== String(sessionId || '')) return { ok: false, error: 'session mismatch' };
    const createdUnixMs = Number(binding.created_unix_ms || 0);
    const expiresUnixMs = Number(binding.expires_unix_ms || 0);
    if (!Number.isFinite(createdUnixMs) || createdUnixMs <= 0) return { ok: false, error: 'missing binding creation time' };
    if (!Number.isFinite(expiresUnixMs) || expiresUnixMs <= 0) return { ok: false, error: 'missing binding expiry' };
    const nowUnixMs = Date.now();
    if (expiresUnixMs + 30000 < nowUnixMs) return { ok: false, error: 'binding expired' };
    if (createdUnixMs - 30000 > nowUnixMs) return { ok: false, error: 'binding timestamp from future' };
    if (binding.offer_sha256 !== await sha256B64u(offerSdp || '')) return { ok: false, error: 'offer hash mismatch' };
    if (binding.answer_sha256 !== await sha256B64u(answerSdp || '')) return { ok: false, error: 'answer hash mismatch' };
    const nonce = String(clientNonce || '');
    if (nonce) {
      if (String(binding.client_nonce || '') !== nonce) return { ok: false, error: 'client nonce mismatch' };
    } else if (binding.client_nonce) {
      return { ok: false, error: 'unexpected client nonce binding' };
    }
    const grant = String(sessionGrant || '');
    if (grant) {
      const grantHash = await sha256B64u(grant);
      if (binding.session_grant_sha256 !== grantHash) return { ok: false, error: 'session grant hash mismatch' };
    } else if (binding.session_grant_sha256) {
      return { ok: false, error: 'unexpected session grant binding' };
    }
    const verified = await verifyEd25519(
      base64UrlToBytes(binding.daemon_public_key || ''),
      base64UrlToBytes(binding.signature || ''),
      new TextEncoder().encode(bindingPayload(binding))
    );
    if (!verified) return { ok: false, error: 'signature invalid' };
    return {
      ok: true,
      daemonPublicKey: binding.daemon_public_key,
      createdUnixMs,
      expiresUnixMs,
      clientNonce: binding.client_nonce || '',
      sessionGrantSha256: binding.session_grant_sha256 || '',
    };
  }
  function abortError(message = 'dashboard control request aborted') {
    try { return new DOMException(message, 'AbortError'); } catch {
      const err = new Error(message);
      err.name = 'AbortError';
      return err;
    }
  }
  const connect = {
    pc: null,
    channel: null,
    sessionId: '',
    verifiedBinding: null,
    claimedDaemonPublicKey: '',
    sessionGrantSha256: '',
    clientNonce: '',
    expiresUnixMs: 0,
    pendingIce: [],
    pending: new Map(),
    chunkedResponses: new Map(),
    byteStreams: new Map(),
    completedChunkedResponses: 0,
    completedByteStreams: 0,
    lastStatus: null,
    lastError: '',
    seq: 0,
    async start() {
      this.pc = new RTCPeerConnection({});
      this.channel = this.pc.createDataChannel('intendant-dashboard-control', { ordered: true });
      this.channel.onopen = () => {
        this.sendFrame({ t: 'hello', id: this.nextId(), features: ['response_credit', 'byte_streams', 'upload_frames', 'terminal_frames'] });
        paint(this.status());
      };
      this.channel.onmessage = ev => this.handleMessage(ev.data);
      this.pc.onconnectionstatechange = () => paint(this.status());
      this.pc.onicecandidate = ev => {
        if (!ev.candidate) return;
        const candidate = ev.candidate.toJSON ? ev.candidate.toJSON() : ev.candidate;
        if (!this.sessionId) this.pendingIce.push(candidate);
        else this.sendIce(candidate).catch(err => console.warn('ice failed', err));
      };
      const offer = await this.pc.createOffer();
      await this.pc.setLocalDescription(offer);
      const offerSdp = offer.sdp || '';
      this.clientNonce = bytesToBase64Url(crypto.getRandomValues(new Uint8Array(32)));
      const answer = await fetch('/api/browser/offer', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ daemon_id: daemonId, sdp: offerSdp, client_nonce: this.clientNonce }),
      }).then(async resp => {
        const body = await resp.json().catch(() => ({}));
        if (!resp.ok) throw new Error(body.error || 'offer failed');
        return body;
      });
      this.sessionId = String(answer.session_id || '');
      const claimedDaemonPublicKey = String(answer.daemon_public_key || '');
      if (!claimedDaemonPublicKey) {
        throw new Error('rendezvous answer missing daemon_public_key');
      }
      const sessionGrant = String(answer.session_grant || '');
      if (!sessionGrant) {
        throw new Error('rendezvous answer missing session_grant');
      }
      const verified = await verifyBinding(answer.binding, this.sessionId, offerSdp, answer.sdp || '', sessionGrant, this.clientNonce);
      if (!verified.ok) throw new Error('binding rejected: ' + (verified.error || 'unknown'));
      if (String(verified.daemonPublicKey || '') !== claimedDaemonPublicKey) {
        throw new Error('binding rejected: daemon public key mismatch');
      }
      this.verifiedBinding = verified;
      this.claimedDaemonPublicKey = claimedDaemonPublicKey;
      this.sessionGrantSha256 = verified.sessionGrantSha256 || '';
      this.expiresUnixMs = verified.expiresUnixMs || 0;
      await this.pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
      for (const candidate of this.pendingIce.splice(0)) await this.sendIce(candidate);
      paint(this.status());
      return this.status();
    },
    async sendIce(candidate) {
      await fetch('/api/browser/ice', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ daemon_id: daemonId, session_id: this.sessionId, candidate }),
      });
    },
    handleMessage(data) {
      let msg;
      try { msg = JSON.parse(String(data)); } catch { return; }
      this.handleFrame(msg);
    },
    handleFrame(msg) {
      if (msg.t === 'hello_ack') {
        paint(this.status());
        return;
      }
      if (msg.t === 'terminal_output' || msg.t === 'terminal_exited' || msg.t === 'terminal_opened' || msg.t === 'terminal_error') {
        try {
          window.dispatchEvent(new CustomEvent('intendant-dashboard-terminal-frame', { detail: msg }));
        } catch (_) {}
        return;
      }
      if (msg.t === 'response_start') {
        this.handleResponseStart(msg);
        return;
      }
      if (msg.t === 'response_chunk') {
        this.handleResponseChunk(msg);
        return;
      }
      if (msg.t === 'response_end') {
        this.handleResponseEnd(msg);
        return;
      }
      if (msg.t === 'byte_stream_start') {
        this.handleByteStreamStart(msg);
        return;
      }
      if (msg.t === 'byte_stream_chunk') {
        this.handleByteStreamChunk(msg);
        return;
      }
      if (msg.t === 'byte_stream_end') {
        this.handleByteStreamEnd(msg);
        return;
      }
      if (msg.t === 'stream_start') {
        this.handleStreamStart(msg);
        return;
      }
      if (msg.t === 'stream_event') {
        this.handleStreamEvent(msg);
        return;
      }
      if (msg.t === 'stream_end') {
        this.handleStreamEnd(msg);
        return;
      }
      if (msg.t !== 'pong' && msg.t !== 'response') return;
      const pending = this.pending.get(msg.id);
      if (!pending) return;
      this.pending.delete(msg.id);
      if (msg.cancelled) pending.reject(abortError(msg.error || 'request cancelled'));
      else if (msg.t === 'response' && msg.ok === false) pending.reject(new Error(msg.error || 'request failed'));
      else pending.resolve(msg.t === 'pong' ? msg : msg.result);
    },
    handleResponseStart(msg) {
      const id = String(msg.id || '');
      const chunkKey = String(msg.chunk_id || id);
      if (!id || !chunkKey || !this.pending.has(id)) return;
      const totalBytes = Number(msg.total_bytes);
      const expectedChunks = Number(msg.chunks);
      if (
        msg.encoding !== 'base64-json-frame' ||
        !Number.isSafeInteger(totalBytes) ||
        totalBytes < 0 ||
        totalBytes > MAX_CHUNKED_RESPONSE_BYTES ||
        !Number.isSafeInteger(expectedChunks) ||
        expectedChunks < 0
      ) {
        this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunked response header');
        return;
      }
      this.chunkedResponses.set(chunkKey, {
        id,
        totalBytes,
        expectedChunks,
        receivedBytes: 0,
        chunks: new Map(),
        ended: false,
      });
      paint(this.status());
    },
    handleResponseChunk(msg) {
      const id = String(msg.id || '');
      const chunkKey = String(msg.chunk_id || id);
      const state = this.chunkedResponses.get(chunkKey);
      if (!state) return;
      const seq = Number(msg.seq);
      if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
        this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunk sequence');
        return;
      }
      if (state.chunks.has(seq)) return;
      let bytes;
      try {
        bytes = base64ToBytes(msg.data);
      } catch {
        this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunk encoding');
        return;
      }
      state.chunks.set(seq, bytes);
      state.receivedBytes += bytes.byteLength;
      if (state.receivedBytes > state.totalBytes) {
        this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response exceeded declared size');
        return;
      }
      const completed = this.maybeCompleteChunkedResponse(chunkKey);
      if (!completed && this.chunkedResponses.has(chunkKey)) {
        this.sendChunkCredit(id, 1, chunkKey === id ? null : chunkKey);
      }
      paint(this.status());
    },
    handleResponseEnd(msg) {
      const id = String(msg.id || '');
      const chunkKey = String(msg.chunk_id || id);
      const state = this.chunkedResponses.get(chunkKey);
      if (!state) return;
      const finalChunks = Number(msg.chunks);
      if (!Number.isSafeInteger(finalChunks) || finalChunks !== state.expectedChunks) {
        this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunked response footer');
        return;
      }
      state.ended = true;
      this.maybeCompleteChunkedResponse(chunkKey);
      paint(this.status());
    },
    maybeCompleteChunkedResponse(chunkKey) {
      const state = this.chunkedResponses.get(chunkKey);
      if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
      const merged = new Uint8Array(state.totalBytes);
      let offset = 0;
      for (let seq = 0; seq < state.expectedChunks; seq++) {
        const chunk = state.chunks.get(seq);
        if (!chunk) {
          this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response missed a chunk');
          return false;
        }
        merged.set(chunk, offset);
        offset += chunk.byteLength;
      }
      if (offset !== state.totalBytes) {
        this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response size mismatch');
        return false;
      }
      this.chunkedResponses.delete(chunkKey);
      let frame;
      try {
        frame = JSON.parse(new TextDecoder().decode(merged));
      } catch {
        this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response was not valid JSON');
        return false;
      }
      if (!['response', 'stream_event'].includes(frame.t) || String(frame.id || '') !== state.id) {
        this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response id mismatch');
        return false;
      }
      this.completedChunkedResponses += 1;
      this.handleFrame(frame);
      return true;
    },
    rejectChunkedResponse(chunkKey, message) {
      const state = this.chunkedResponses.get(chunkKey);
      const id = state?.id || chunkKey;
      this.chunkedResponses.delete(chunkKey);
      const pending = this.pending.get(id);
      if (pending) {
        this.pending.delete(id);
        pending.reject(new Error(message));
      }
      paint(this.status());
    },
    handleByteStreamStart(msg) {
      const id = String(msg.id || '');
      const streamId = String(msg.stream_id || id);
      if (!id || !streamId || !this.pending.has(id)) return;
      const totalBytes = Number(msg.total_bytes);
      const expectedChunks = Number(msg.chunks);
      if (
        msg.encoding !== 'base64' ||
        !Number.isSafeInteger(totalBytes) ||
        totalBytes < 0 ||
        totalBytes > MAX_BYTE_STREAM_BYTES ||
        !Number.isSafeInteger(expectedChunks) ||
        expectedChunks < 0
      ) {
        this.rejectByteStream(streamId, 'invalid dashboard control byte stream header', id);
        return;
      }
      this.byteStreams.set(streamId, {
        id,
        streamId,
        totalBytes,
        expectedChunks,
        receivedBytes: 0,
        chunks: new Map(),
        ended: false,
        result: null,
        contentType: String(msg.content_type || 'application/octet-stream'),
        filename: msg.filename ? String(msg.filename) : '',
      });
      paint(this.status());
    },
    handleByteStreamChunk(msg) {
      const id = String(msg.id || '');
      const streamId = String(msg.stream_id || id);
      const state = this.byteStreams.get(streamId);
      if (!state) return;
      const seq = Number(msg.seq);
      if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
        this.rejectByteStream(streamId, 'invalid dashboard control byte stream chunk sequence');
        return;
      }
      if (state.chunks.has(seq)) return;
      let bytes;
      try {
        bytes = base64ToBytes(msg.data);
      } catch {
        this.rejectByteStream(streamId, 'invalid dashboard control byte stream encoding');
        return;
      }
      state.chunks.set(seq, bytes);
      state.receivedBytes += bytes.byteLength;
      if (state.receivedBytes > state.totalBytes) {
        this.rejectByteStream(streamId, 'dashboard control byte stream exceeded declared size');
        return;
      }
      const completed = this.maybeCompleteByteStream(streamId);
      if (!completed && this.byteStreams.has(streamId)) {
        this.sendChunkCredit(id, 1, streamId === id ? null : streamId);
      }
      paint(this.status());
    },
    handleByteStreamEnd(msg) {
      const id = String(msg.id || '');
      const streamId = String(msg.stream_id || id);
      const state = this.byteStreams.get(streamId);
      if (!state) return;
      if (msg.ok === false) {
        this.rejectByteStream(streamId, msg.error || 'dashboard control byte stream failed');
        return;
      }
      const finalChunks = Number(msg.chunks);
      if (!Number.isSafeInteger(finalChunks) || finalChunks !== state.expectedChunks) {
        this.rejectByteStream(streamId, 'invalid dashboard control byte stream footer');
        return;
      }
      state.ended = true;
      state.result = msg.result || null;
      this.maybeCompleteByteStream(streamId);
      paint(this.status());
    },
    maybeCompleteByteStream(streamId) {
      const state = this.byteStreams.get(streamId);
      if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
      const merged = new Uint8Array(state.totalBytes);
      let offset = 0;
      for (let seq = 0; seq < state.expectedChunks; seq++) {
        const chunk = state.chunks.get(seq);
        if (!chunk) {
          this.rejectByteStream(streamId, 'dashboard control byte stream missed a chunk');
          return false;
        }
        merged.set(chunk, offset);
        offset += chunk.byteLength;
      }
      if (offset !== state.totalBytes) {
        this.rejectByteStream(streamId, 'dashboard control byte stream size mismatch');
        return false;
      }
      this.byteStreams.delete(streamId);
      const pending = this.pending.get(state.id);
      if (!pending) return true;
      const result = state.result && typeof state.result === 'object' && !Array.isArray(state.result)
        ? { ...state.result }
        : {};
      result.ok = result.ok !== false;
      result.bytes = merged;
      result.size = state.totalBytes;
      result.content_type = result.content_type || state.contentType;
      result.filename = result.filename || state.filename;
      result.stream_id = state.streamId;
      this.completedByteStreams += 1;
      this.pending.delete(state.id);
      this.deleteChunkedResponsesForRequest(state.id);
      pending.resolve(result);
      paint(this.status());
      return true;
    },
    rejectByteStream(streamId, message, requestId = '') {
      const state = this.byteStreams.get(streamId);
      const id = state?.id || requestId || streamId;
      this.byteStreams.delete(streamId);
      const pending = this.pending.get(id);
      if (pending) {
        this.pending.delete(id);
        pending.reject(new Error(message));
      }
      paint(this.status());
    },
    handleStreamStart(msg) {
      const pending = this.pending.get(String(msg.id || ''));
      const stream = pending?.stream;
      if (!stream) return;
      stream.started = true;
      this.callStreamCallback(stream, 'start', msg);
    },
    handleStreamEvent(msg) {
      const pending = this.pending.get(String(msg.id || ''));
      const stream = pending?.stream;
      if (!stream) return;
      stream.eventCount += 1;
      this.callStreamCallback(stream, 'event', msg.event, msg);
    },
    handleStreamEnd(msg) {
      const id = String(msg.id || '');
      const pending = this.pending.get(id);
      const stream = pending?.stream;
      if (!pending || !stream) return;
      this.pending.delete(id);
      if (msg.ok === false) {
        pending.reject(new Error(msg.error || 'dashboard control stream failed'));
        return;
      }
      this.callStreamCallback(stream, 'end', msg.result || null, msg);
      pending.resolve(msg.result || null);
    },
    callStreamCallback(stream, name, ...args) {
      const callbacks = stream.callbacks;
      if (typeof callbacks === 'function' && name === 'event') {
        callbacks(...args);
      } else if (callbacks && typeof callbacks[name] === 'function') {
        callbacks[name](...args);
      }
    },
    request(method, params = {}, options = {}) {
      if (options.signal?.aborted) return Promise.reject(abortError());
      if (!this.canUseRpc()) return Promise.reject(new Error('dashboard control RPC is not connected'));
      const id = this.nextId();
      const promise = this.waitFor(id, { ...options, method });
      this.sendFrame({ t: 'request', id, method, params });
      if (method === 'status') {
        return promise.then(status => {
          if (status && typeof status === 'object') this.lastStatus = status;
          return status;
        });
      }
      return promise;
    },
    requestBytes(method, params = {}, options = {}) {
      if (options.signal?.aborted) return Promise.reject(abortError());
      if (!this.canUseRpc()) return Promise.reject(new Error('dashboard control byte stream is not connected'));
      const id = this.nextId();
      const promise = this.waitFor(id, { ...options, method });
      const pending = this.pending.get(id);
      if (pending) pending.expectBytes = true;
      this.sendFrame({ t: 'request', id, method, params, bytes: true });
      return promise;
    },
    async uploadBytes(method, params = {}, bytes, options = {}) {
      if (options.signal?.aborted) return Promise.reject(abortError());
      if (!this.canUseRpc()) return Promise.reject(new Error('dashboard control upload is not connected'));
      const data = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
      const id = this.nextId();
      const totalBytes = data.byteLength;
      const chunkSize = options.chunkBytes || UPLOAD_CHUNK_BYTES;
      const chunks = Math.ceil(totalBytes / chunkSize);
      const promise = this.waitFor(id, { ...options, method });
      this.sendFrame({
        t: 'upload_start',
        id,
        method,
        params,
        encoding: 'base64',
        total_bytes: totalBytes,
        chunks,
      });
      try {
        for (let seq = 0, offset = 0; offset < totalBytes; seq++, offset += chunkSize) {
          if (options.signal?.aborted) throw abortError();
          if (!this.pending.has(id)) break;
          const chunk = data.subarray(offset, Math.min(offset + chunkSize, totalBytes));
          this.sendFrame({
            t: 'upload_chunk',
            id,
            seq,
            data: bytesToBase64(chunk),
          });
          await this.waitForBufferedAmountLow(options.signal);
        }
        if (this.pending.has(id)) this.sendFrame({ t: 'upload_end', id, chunks });
      } catch (err) {
        if (this.pending.has(id)) this.sendFrame({ t: 'cancel', id });
        throw err;
      }
      return promise;
    },
    async waitForBufferedAmountLow(signal = null) {
      while (
        this.channel &&
        this.channel.readyState === 'open' &&
        this.channel.bufferedAmount > UPLOAD_BUFFER_HIGH_BYTES
      ) {
        if (signal?.aborted) throw abortError();
        await new Promise(resolve => setTimeout(resolve, 10));
      }
    },
    terminalFrame(frame) {
      if (!this.canUseRpc()) return false;
      this.sendFrame(frame);
      return true;
    },
    stream(method, params = {}, options = {}, onEvent = {}) {
      if (options.signal?.aborted) return Promise.reject(abortError());
      if (!this.canUseRpc()) return Promise.reject(new Error('dashboard control stream is not connected'));
      const id = this.nextId();
      const promise = this.waitFor(id, { ...options, method });
      const pending = this.pending.get(id);
      if (pending) {
        pending.stream = {
          callbacks: onEvent,
          eventCount: 0,
          started: false,
        };
      }
      this.sendFrame({ t: 'request', id, method, params, stream: true });
      return promise;
    },
    waitFor(id, options = {}) {
      return new Promise((resolve, reject) => {
        let settled = false;
        const signal = options.signal || null;
        const fail = (err, cancel = false) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
          this.pending.delete(id);
          this.deleteChunkedResponsesForRequest(id);
          this.deleteByteStreamsForRequest(id);
          if (cancel) this.sendFrame({ t: 'cancel', id });
          reject(err);
        };
        const timeoutMs = Number.isFinite(Number(options.timeoutMs)) ? Number(options.timeoutMs) : 10000;
        const label = String(options.label || options.method || id || 'request');
        const timer = setTimeout(() => fail(new Error(label + ' request timed out'), true), timeoutMs);
        const abortHandler = signal ? () => fail(abortError(), true) : null;
        if (signal && abortHandler) signal.addEventListener('abort', abortHandler, { once: true });
        this.pending.set(id, {
          resolve: value => {
            if (settled) return;
            settled = true;
            clearTimeout(timer);
            if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
            this.deleteChunkedResponsesForRequest(id);
            this.deleteByteStreamsForRequest(id);
            resolve(value);
          },
          reject: err => fail(err),
        });
      });
    },
    deleteChunkedResponsesForRequest(id) {
      for (const [chunkKey, state] of this.chunkedResponses) {
        if (chunkKey === id || state?.id === id) {
          this.chunkedResponses.delete(chunkKey);
        }
      }
    },
    deleteByteStreamsForRequest(id) {
      for (const [streamId, state] of this.byteStreams) {
        if (streamId === id || state?.id === id) {
          this.byteStreams.delete(streamId);
        }
      }
    },
    canUseRpc() {
      return Boolean(this.verifiedBinding && this.pc?.connectionState === 'connected' && this.channel?.readyState === 'open');
    },
    sendFrame(frame) {
      if (this.channel?.readyState === 'open') this.channel.send(JSON.stringify(frame));
    },
    sendChunkCredit(id, chunks, chunkId = null) {
      const frame = { t: 'credit', id, chunks };
      if (chunkId) frame.chunk_id = chunkId;
      this.sendFrame(frame);
    },
    status() {
      return {
        daemonId,
        lastError: this.lastError,
        connected: this.pc?.connectionState === 'connected',
        pcState: this.pc?.connectionState || '',
        channelState: this.channel?.readyState || '',
        sessionId: this.sessionId,
        verifiedBinding: this.verifiedBinding,
        claimedDaemonPublicKey: this.claimedDaemonPublicKey,
        sessionGrantSha256: this.sessionGrantSha256,
        clientNonce: this.clientNonce,
        expiresUnixMs: this.expiresUnixMs,
        pendingRequests: this.pending.size,
        pendingChunkedResponses: this.chunkedResponses.size,
        pendingByteStreams: this.byteStreams.size,
        completedChunkedResponses: this.completedChunkedResponses,
        completedByteStreams: this.completedByteStreams,
        apiAgentCardAvailable: this.lastStatus?.api_agent_card_available ?? null,
        apiCachedBootstrapEventsAvailable: this.lastStatus?.api_cached_bootstrap_events_available ?? null,
        apiBrowserWorkspaceSnapshotAvailable: this.lastStatus?.api_browser_workspace_snapshot_available ?? null,
        apiStateSnapshotAvailable: this.lastStatus?.api_state_snapshot_available ?? null,
        apiDisplayBootstrapAvailable: this.lastStatus?.api_display_bootstrap_available ?? null,
        apiDisplayInputAuthorityAvailable: this.lastStatus?.api_display_input_authority_available ?? null,
        apiSessionLogReplayAvailable: this.lastStatus?.api_session_log_replay_available ?? null,
        apiExternalSessionActivityReplayAvailable: this.lastStatus?.api_external_session_activity_replay_available ?? null,
        apiDashboardBootstrapAvailable: this.lastStatus?.api_dashboard_bootstrap_available ?? null,
        apiPeersAvailable: this.lastStatus?.api_peers_available ?? null,
        apiSessionsAvailable: this.lastStatus?.api_sessions_available ?? null,
        apiSessionsStreamAvailable: this.lastStatus?.api_sessions_stream_available ?? null,
        byteStreamsAvailable: this.lastStatus?.byte_streams_available ?? null,
        uploadFramesAvailable: this.lastStatus?.upload_frames_available ?? null,
        terminalFramesAvailable: this.lastStatus?.terminal_frames_available ?? null,
        presenceFramesAvailable: this.lastStatus?.presence_frames_available ?? null,
        presenceActiveHandoffAvailable: this.lastStatus?.presence_active_handoff_available ?? null,
        presenceToolRequestAvailable: this.lastStatus?.presence_tool_request_available ?? null,
        apiPresenceVideoFrameAvailable: this.lastStatus?.api_presence_video_frame_available ?? null,
        apiSessionDetailAvailable: this.lastStatus?.api_session_detail_available ?? null,
        apiSessionReportAvailable: this.lastStatus?.api_session_report_available ?? null,
        apiSessionDeleteAvailable: this.lastStatus?.api_session_delete_available ?? null,
        apiSessionCurrentAgentOutputAvailable: this.lastStatus?.api_session_current_agent_output_available ?? null,
        apiSessionCurrentHistoryAvailable: this.lastStatus?.api_session_current_history_available ?? null,
        apiSessionCurrentRollbackAvailable: this.lastStatus?.api_session_current_rollback_available ?? null,
        apiSessionCurrentRedoAvailable: this.lastStatus?.api_session_current_redo_available ?? null,
        apiSessionCurrentPruneAvailable: this.lastStatus?.api_session_current_prune_available ?? null,
        apiSessionCurrentChangesAvailable: this.lastStatus?.api_session_current_changes_available ?? null,
        apiSessionContextSnapshotAvailable: this.lastStatus?.api_session_context_snapshot_available ?? null,
        apiSessionCurrentUploadAvailable: this.lastStatus?.api_session_current_upload_available ?? null,
        apiSessionCurrentUploadsAvailable: this.lastStatus?.api_session_current_uploads_available ?? null,
        apiSessionCurrentUploadRawAvailable: this.lastStatus?.api_session_current_upload_raw_available ?? null,
        apiSessionCurrentUploadDeleteAvailable: this.lastStatus?.api_session_current_upload_delete_available ?? null,
        apiMediaEditorAvailable: this.lastStatus?.api_media_editor_available ?? null,
        apiMediaAnnotationAttachAvailable: this.lastStatus?.api_media_annotation_attach_available ?? null,
        apiMediaAnnotationSubmitAvailable: this.lastStatus?.api_media_annotation_submit_available ?? null,
        apiMediaClipStartAvailable: this.lastStatus?.api_media_clip_start_available ?? null,
        apiMediaClipFrameAvailable: this.lastStatus?.api_media_clip_frame_available ?? null,
        apiMediaClipEndAvailable: this.lastStatus?.api_media_clip_end_available ?? null,
        apiMediaClipCancelAvailable: this.lastStatus?.api_media_clip_cancel_available ?? null,
        apiFsStatAvailable: this.lastStatus?.api_fs_stat_available ?? null,
        apiFsListAvailable: this.lastStatus?.api_fs_list_available ?? null,
        apiFsMkdirAvailable: this.lastStatus?.api_fs_mkdir_available ?? null,
        apiFsReadAvailable: this.lastStatus?.api_fs_read_available ?? null,
        apiSessionsSearchAvailable: this.lastStatus?.api_sessions_search_available ?? null,
        apiSettingsAvailable: this.lastStatus?.api_settings_available ?? null,
        apiSettingsSaveAvailable: this.lastStatus?.api_settings_save_available ?? null,
        apiControlMsgAvailable: this.lastStatus?.api_control_msg_available ?? null,
        apiSessionControlMsgAvailable: this.lastStatus?.api_session_control_msg_available ?? null,
        apiDashboardActionMsgAvailable: this.lastStatus?.api_dashboard_action_msg_available ?? null,
        apiDiagnosticsVisualFreshnessAvailable: this.lastStatus?.api_diagnostics_visual_freshness_available ?? null,
        apiKeyStatusAvailable: this.lastStatus?.api_key_status_available ?? null,
        apiApiKeysSaveAvailable: this.lastStatus?.api_api_keys_save_available ?? null,
        apiVoiceSessionAvailable: this.lastStatus?.api_voice_session_available ?? null,
        apiProjectRootAvailable: this.lastStatus?.api_project_root_available ?? null,
        apiDisplaysAvailable: this.lastStatus?.api_displays_available ?? null,
        apiRecordingsAvailable: this.lastStatus?.api_recordings_available ?? null,
        apiRecordingAssetAvailable: this.lastStatus?.api_recording_asset_available ?? null,
        apiSessionRecordingsAvailable: this.lastStatus?.api_session_recordings_available ?? null,
        apiSessionRecordingAssetAvailable: this.lastStatus?.api_session_recording_asset_available ?? null,
        apiSessionFrameAssetAvailable: this.lastStatus?.api_session_frame_asset_available ?? null,
        apiWorktreesAvailable: this.lastStatus?.api_worktrees_available ?? null,
        apiWorktreesScanAvailable: this.lastStatus?.api_worktrees_scan_available ?? null,
        apiWorktreesRemoveAvailable: this.lastStatus?.api_worktrees_remove_available ?? null,
        apiManagedContextAvailable: this.lastStatus?.api_managed_context_available ?? null,
        apiMcpToolCallAvailable: this.lastStatus?.api_mcp_tool_call_available ?? null,
        apiPeerMutationsAvailable: this.lastStatus?.api_peer_mutations_available ?? null,
        apiPeerPairingAvailable: this.lastStatus?.api_peer_pairing_available ?? null,
        apiPeerWebRtcSignalAvailable: this.lastStatus?.api_peer_webrtc_signal_available ?? null,
        apiCoordinatorAvailable: this.lastStatus?.api_coordinator_available ?? null,
      };
    },
    close() {
      if (this.sessionId) {
        fetch('/api/browser/close', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ daemon_id: daemonId, session_id: this.sessionId }),
        }).catch(() => {});
      }
      this.chunkedResponses.clear();
      this.byteStreams.clear();
      try { this.channel?.close(); } catch {}
      try { this.pc?.close(); } catch {}
    },
    nextId() {
      this.seq += 1;
      return 'public-connect-' + Date.now() + '-' + this.seq;
    },
  };
  window.intendantPublicConnectDashboard = connect;
  connect.start().catch(err => {
    console.error(err);
    connect.lastError = err?.message || String(err);
    paint(connect.lastError);
  });
})();
</script>
</body>
</html>`;
}

function wait(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

function slugComponent(value) {
  const slug = String(value || '')
    .trim()
    .replace(/[^a-zA-Z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .toLowerCase();
  return slug || 'unknown';
}

// Adversarial fixture: a real operator grant and a forged persisted ceiling
// must still be refused when the daemon stamps the offer as hosted.
function writeAdversarialClientKeyGrant(homeDir, fingerprint, accountName = RENDEZVOUS_TEST_ACCOUNT_NAME) {
  assert(fingerprint, 'hosted browser key fingerprint is required');
  const certDir = path.join(homeDir, '.intendant', 'access-certs');
  fs.mkdirSync(certDir, { recursive: true });
  const iamPath = path.join(certDir, 'iam.json');
  const state = fs.existsSync(iamPath)
    ? JSON.parse(fs.readFileSync(iamPath, 'utf8'))
    : { principals: [], roles: [], grants: [], audit_events: [] };
  state.schema_version = 2;
  state.principals = Array.isArray(state.principals) ? state.principals : [];
  state.roles = Array.isArray(state.roles) ? state.roles : [];
  state.grants = Array.isArray(state.grants) ? state.grants : [];
  state.audit_events = Array.isArray(state.audit_events) ? state.audit_events : [];
  const principalId = `principal:client-key:${slugComponent(fingerprint)}`;
  const now = Date.now();
  if (!state.principals.some(principal => principal.id === principalId)) {
    state.principals.push({
      id: principalId,
      kind: 'client_key',
      label: accountName ? `@${accountName} browser` : 'Hosted browser',
      status: 'active',
      source: 'local_iam_state',
      account: accountName ? { provider: 'intendant.dev', account_name: accountName, handle: accountName } : null,
      organization: null,
      authn: [{
        kind: 'client_key',
        label: 'Browser identity key',
        fingerprint,
        origin: 'hosted-connect-e2e',
      }],
      notes: 'Rendezvous immutable-refusal adversarial browser-key grant',
      created_at_unix_ms: now,
    });
  }
  if (!state.grants.some(grant => grant.principal_id === principalId && grant.status === 'active')) {
    state.grants.push({
      id: `grant:user-client:${slugComponent(principalId)}:local:role-operator`,
      principal_id: principalId,
      target_id: 'local',
      role_id: 'role:operator',
      policy_id: 'policy:operator',
      status: 'active',
      source: 'local_iam_state',
      reason: 'Rendezvous immutable-refusal adversarial browser-key grant',
      created_at_unix_ms: now,
      revoked_at_unix_ms: null,
    });
  }
  state.role_ceilings = {
    ...(state.role_ceilings || {}),
    connect_account: 'role:none',
    client_key: 'role:operator',
  };
  fs.writeFileSync(iamPath, `${JSON.stringify(state, null, 2)}\n`, { mode: 0o600 });
  return { iamPath, bytes: fs.readFileSync(iamPath, 'utf8') };
}

function prepareDaemonHomeAccessCerts(binary, homeDir, label) {
  const result = spawnSync(binary, [
    'access',
    'setup',
    '--no-serve-certs',
    '--force',
    '--name',
    label,
    '--ip',
    '127.0.0.1',
    '--host',
    'localhost',
  ], {
    cwd: path.resolve(__dirname, '..'),
    env: { ...process.env, HOME: homeDir },
    encoding: 'utf8',
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(`failed to prepare daemon access certs: ${result.stderr || result.stdout || `exit ${result.status}`}`);
  }
}

function createRecordingFixture(label, homeDir = os.homedir()) {
  const streamName = `dashboard_control_${label}_${process.pid}_${Date.now()}`;
  const dir = path.join(homeDir, '.intendant', 'recordings', streamName);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'segments.csv'), 'seg_00000.mp4,0,1.25\n');
  fs.writeFileSync(path.join(dir, 'seg_00000.mp4'), 'recording segment e2e rendezvous');
  return { streamName, dir };
}

function createHlsRecordingFixture(label, homeDir = os.homedir()) {
  const streamName = `dashboard_control_hls_${label}_${process.pid}_${Date.now()}`;
  const dir = path.join(homeDir, '.intendant', 'recordings', streamName);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'segments.csv'), 'seg_00000.ts,0,1.25\n');
  fs.writeFileSync(path.join(dir, 'seg_00000.ts'), 'recording hls transport stream e2e rendezvous');
  return { streamName, dir };
}

function removeRecordingFixture(fixture) {
  if (!fixture?.dir) return;
  fs.rmSync(fixture.dir, { recursive: true, force: true });
}

function createSessionFrameFixture(label, homeDir = os.homedir()) {
  const sessionId = `dashboard-control-frame-${label}-${process.pid}-${Date.now()}`;
  const filename = 'ann-dashboard-frame.png';
  const dir = path.join(homeDir, '.intendant', 'logs', sessionId);
  const framesDir = path.join(dir, 'frames');
  fs.mkdirSync(framesDir, { recursive: true });
  fs.writeFileSync(path.join(framesDir, filename), Buffer.from(FRAME_FIXTURE_PNG_BASE64, 'base64'));
  return { sessionId, filename, dir };
}

function removeSessionFrameFixture(fixture) {
  if (!fixture?.dir) return;
  fs.rmSync(fixture.dir, { recursive: true, force: true });
}

// The daemon under test runs against an isolated temp HOME. Interactive
// login shells started by the terminal probe would hit zsh's first-run
// wizard (zsh-newuser-install) in an rc-less home and block forever, so
// give the home minimal shell rc files.
function seedShellRcFixtures(homeDir) {
  for (const rc of ['.zshrc', '.bashrc', '.bash_profile', '.profile']) {
    fs.writeFileSync(path.join(homeDir, rc), '# validator fixture\n');
  }
}

// The daemon under test runs against an isolated temp HOME, so the
// large-payload assertions (chunked >64KiB api_sessions/stream replace
// events) need enough session history to exist. Seed synthetic session
// dirs the way the daemon writes them; ~150 entries comfortably clears
// the 64KiB chunk threshold (~930 bytes per listed session).
function createSessionSeedFixtures(label, homeDir, count = 150) {
  const logsDir = path.join(homeDir, '.intendant', 'logs');
  fs.mkdirSync(logsDir, { recursive: true });
  const dirs = [];
  for (let i = 0; i < count; i += 1) {
    const sessionId = `validator-seed-${label}-${String(i).padStart(4, '0')}`;
    const dir = path.join(logsDir, sessionId);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, 'session_meta.json'), JSON.stringify({
      session_id: sessionId,
      created_at: '2026-07-01T10:00:00',
      project_root: `/tmp/validator-seed-${label}`,
      status: 'completed',
      last_turn: 3,
    }));
    fs.writeFileSync(path.join(dir, 'session.jsonl'), [
      '{"ts":"10:00:00.000","event":"session_start","level":"info","message":"Session started"}',
      `{"ts":"10:00:01.000","event":"user_prompt","level":"info","message":"synthetic validator seed session ${i} with a reasonably descriptive first prompt used to size the dashboard sessions payload"}`,
      '',
    ].join('\n'));
    dirs.push(dir);
  }
  return { dirs };
}

function createFilesystemFixture(label) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `intendant-dashboard-control-fs-${label}-`));
  const filePath = path.join(dir, 'filesystem-read.txt');
  const text = `dashboard filesystem read e2e ${label}`;
  fs.writeFileSync(filePath, text);
  return { dir, filePath, text };
}

function removeFilesystemFixture(fixture) {
  if (!fixture?.dir) return;
  fs.rmSync(fixture.dir, { recursive: true, force: true });
}

async function waitFor(predicate, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    last = await predicate();
    if (last) return last;
    await wait(200);
  }
  throw new Error(`timed out waiting for ${label}`);
}

async function waitForBrowserConnect(page, globalName = 'intendantPublicConnectDashboard') {
  let last = null;
  const deadline = Date.now() + CONNECT_TIMEOUT_MS;
  const globalNameJson = JSON.stringify(globalName);
  while (Date.now() < deadline) {
    last = await page.evaluate(`(() => {
      const dashboard = window[${globalNameJson}];
      if (!dashboard) return null;
      return dashboard.status();
    })()`).catch(() => null);
    if (
      last &&
      last.connected &&
      last.channelState === 'open' &&
      last.verifiedBinding &&
      last.verifiedBinding.ok
    ) {
      return last;
    }
    await page.waitForTimeout(250);
  }
  throw new Error(`${globalName} did not connect: ${JSON.stringify(last)}`);
}

async function main() {
  const options = parseArgs(process.argv);
  const rendezvous = createRendezvousServer(path.join(options.repoRoot, 'static'), {
    authToken: options.connectToken,
  });
  await new Promise((resolve, reject) => {
    rendezvous.once('error', reject);
    rendezvous.listen(options.rendezvousPort, '127.0.0.1', resolve);
  });

  const daemonLogs = [];
  const daemonHome = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-connect-rendezvous-home-'));
  prepareDaemonHomeAccessCerts(options.dashboardBinary, daemonHome, 'connect-rendezvous-e2e');
  const recordingFixture = createRecordingFixture('rendezvous', daemonHome);
  const hlsRecordingFixture = createHlsRecordingFixture('rendezvous', daemonHome);
  const sessionFrameFixture = createSessionFrameFixture('rendezvous', daemonHome);
  createSessionSeedFixtures('rendezvous', daemonHome);
  seedShellRcFixtures(daemonHome);
  const filesystemFixture = createFilesystemFixture('rendezvous');
  const daemon = spawn(options.dashboardBinary, ['--no-tui', '--web', String(options.daemonPort)], {
    cwd: options.repoRoot,
    env: {
      ...process.env,
      HOME: daemonHome,
      INTENDANT_CONNECT_RENDEZVOUS_URL: `http://127.0.0.1:${options.rendezvousPort}`,
      INTENDANT_CONNECT_DAEMON_ID: options.daemonId,
      INTENDANT_CONNECT_TOKEN: options.connectToken,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const daemonExit = new Promise(resolve => daemon.once('exit', resolve));
  daemon.stdout.on('data', chunk => daemonLogs.push(chunk.toString()));
  daemon.stderr.on('data', chunk => daemonLogs.push(chunk.toString()));
  daemon.once('error', err => daemonLogs.push(String(err && err.message || err)));

  let browser;
  try {
    await waitFor(() => daemonLogs.join('').includes(`Dashboard: https://0.0.0.0:${options.daemonPort}`), START_TIMEOUT_MS, 'daemon web startup');
    const daemonNoAuthStatus = await httpStatus(`http://127.0.0.1:${options.rendezvousPort}/api/daemon/next?daemon_id=${encodeURIComponent(options.daemonId)}&timeout_ms=1`);
    assert.strictEqual(daemonNoAuthStatus, 401, `/api/daemon/next without bearer returned ${daemonNoAuthStatus}`);
    const registeredStatus = await waitFor(async () => {
      const status = await fetchJson(`http://127.0.0.1:${options.rendezvousPort}/api/status?daemon_id=${encodeURIComponent(options.daemonId)}`);
      assert.strictEqual(status.daemon_auth_required, true, 'rendezvous status did not advertise daemon auth requirement');
      return status.registered ? status : null;
    }, START_TIMEOUT_MS, 'daemon rendezvous registration');
    assert(
      registeredStatus.daemon_public_key,
      `rendezvous registration did not expose daemon public key: ${JSON.stringify(registeredStatus)}`
    );

    const certlessConfigStatus = await httpStatus(`https://127.0.0.1:${options.daemonPort}/config`, {
      ignoreHTTPSErrors: true,
    });
    assert.strictEqual(certlessConfigStatus, 401, `/config without client cert returned ${certlessConfigStatus}`);

    browser = await launchBrowser({ headless: true, ignoreHTTPSErrors: true });
    const page = await browser.newPage();
    page.on('console', msg => console.log(`[browser:${msg.type()}] ${msg.text()}`));
    const publicOrigin = `http://127.0.0.1:${options.rendezvousPort}`;
    const response = await page.goto(`${publicOrigin}/connect?daemon_id=${encodeURIComponent(options.daemonId)}`, {
      waitUntil: 'domcontentloaded',
      timeout: CONNECT_TIMEOUT_MS,
    });
    assert(response, 'public bootstrap produced no response');
    assert.strictEqual(response.status(), 200, `public bootstrap returned ${response.status()}`);
    await page.waitForFunction(() => Boolean(window.intendantPublicConnectDashboard));
    const ungranted = await waitFor(async () => {
      const status = await page.evaluate(() => window.intendantPublicConnectDashboard?.status?.() || null);
      return status?.lastError ? status : null;
    }, CONNECT_TIMEOUT_MS, 'synthetic hosted refusal before local grant');
    assert(
      /role:none|hosted control is unavailable|not authorized|no effective hosted permission/i.test(String(ungranted.lastError)),
      `expected fail-closed hosted refusal: ${JSON.stringify(ungranted)}`
    );
    const adversarialIam = writeAdversarialClientKeyGrant(
      daemonHome,
      rendezvous.testClientKeyFingerprint,
      RENDEZVOUS_TEST_ACCOUNT_NAME
    );
    await page.reload({ waitUntil: 'domcontentloaded', timeout: CONNECT_TIMEOUT_MS });
    await page.waitForFunction(() => Boolean(window.intendantPublicConnectDashboard));
    const stillRefused = await waitFor(async () => {
      const status = await page.evaluate(() => window.intendantPublicConnectDashboard?.status?.() || null);
      return status?.lastError ? status : null;
    }, CONNECT_TIMEOUT_MS, 'synthetic hosted refusal after active grant and forged ceiling');
    assert(
      /role:none|hosted control is unavailable|not authorized|no effective hosted permission/i.test(String(stillRefused.lastError)),
      `expected immutable hosted refusal: ${JSON.stringify(stillRefused)}`
    );
    assert.strictEqual(
      daemonLogs.join('').split('[dashboard/control] data channel open:').length - 1,
      0,
      'hosted rendezvous unexpectedly opened a dashboard-control DataChannel'
    );
    const iamAfter = fs.readFileSync(adversarialIam.iamPath, 'utf8');
    assert.strictEqual(
      iamAfter,
      adversarialIam.bytes,
      'hosted signaling refusal must happen before touching local IAM state'
    );
    console.log(JSON.stringify({
      ok: true,
      publicOrigin,
      daemonId: options.daemonId,
      hosted_refusal: stillRefused.lastError,
      data_channel_open: false,
    }, null, 2));

  } finally {
    if (browser) await browser.close().catch(() => {});
    if (!daemon.killed) daemon.kill('SIGINT');
    await Promise.race([daemonExit, wait(5000)]);
    await new Promise(resolve => rendezvous.close(resolve));
    removeRecordingFixture(recordingFixture);
    removeRecordingFixture(hlsRecordingFixture);
    removeSessionFrameFixture(sessionFrameFixture);
    removeFilesystemFixture(filesystemFixture);
    fs.rmSync(daemonHome, { recursive: true, force: true });
  }
}

async function fetchJson(url) {
  const resp = await fetch(url);
  if (!resp.ok) throw new Error(`${url} returned ${resp.status}`);
  return resp.json();
}

main()
  .then(() => process.exit(0))
  .catch(err => {
    console.error(err);
    process.exit(1);
  });

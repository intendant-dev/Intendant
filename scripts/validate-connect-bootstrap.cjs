#!/usr/bin/env node
'use strict';

const assert = require('assert');
const { httpJson, httpStatus, launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_ORIGIN = 'https://127.0.0.1:8766';
const CONNECT_TIMEOUT_MS = 30000;

function usage() {
  console.log(`Usage:
  node scripts/validate-connect-bootstrap.cjs [--origin <https-origin>]

Environment:
  INTENDANT_CONNECT_ORIGIN   Origin to test. Defaults to ${DEFAULT_ORIGIN}.
  PLAYWRIGHT_NODE_PATH       Optional node_modules directory containing playwright.
  CHROME_PATH/CHROME_BIN     Optional Chromium executable for the CDP fallback.
`);
}

function parseArgs(argv) {
  let origin = process.env.INTENDANT_CONNECT_ORIGIN || DEFAULT_ORIGIN;
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') {
      usage();
      process.exit(0);
    }
    if (arg === '--origin') {
      origin = argv[i + 1];
      i += 1;
      continue;
    }
    throw new Error(`unknown argument: ${arg}`);
  }
  if (!origin || !/^https:\/\//.test(origin)) {
    throw new Error(`origin must be an https origin, got: ${origin || '<empty>'}`);
  }
  return { origin: origin.replace(/\/+$/, '') };
}

async function waitForConnect(page) {
  const deadline = Date.now() + CONNECT_TIMEOUT_MS;
  let last = null;
  while (Date.now() < deadline) {
    try {
      last = await page.evaluate(() => {
        if (!window.intendantConnectDashboard) return null;
        return window.intendantConnectDashboard.status();
      });
    } catch (err) {
      if (!String(err && err.message || err).includes('Execution context was destroyed')) {
        throw err;
      }
      await page.waitForLoadState('domcontentloaded').catch(() => {});
      last = null;
    }
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
  throw new Error(`connect bootstrap did not connect: ${JSON.stringify(last)}`);
}

async function main() {
  const { origin } = parseArgs(process.argv);
  const browser = await launchBrowser({ headless: true, ignoreHTTPSErrors: true });

  try {
    const certlessConfigStatus = await httpStatus(`${origin}/config`, { ignoreHTTPSErrors: true });
    assert.strictEqual(
      certlessConfigStatus,
      401,
      `/config without client cert returned ${certlessConfigStatus}`
    );

    const statusBody = await httpJson(`${origin}/connect/status`, { ignoreHTTPSErrors: true });
    assert.strictEqual(
      statusBody.transport,
      'webrtc-dashboard-control',
      'connect status did not advertise dashboard control'
    );
    assert.strictEqual(
      statusBody.mtls_required_for_dashboard,
      true,
      'connect status did not report dashboard mTLS requirement'
    );

    const page = await browser.newPage();
    page.on('console', msg => console.log(`[browser:${msg.type()}] ${msg.text()}`));
    let response;
    try {
      response = await page.goto(`${origin}/connect/bootstrap`, {
        waitUntil: 'domcontentloaded',
        timeout: CONNECT_TIMEOUT_MS,
      });
    } catch (err) {
      if (browser.kind === 'cdp') {
        throw new Error(
          `CDP browser fallback could not load ${origin}/connect/bootstrap: ${err.message}. ` +
            'Install Playwright, set PLAYWRIGHT_NODE_PATH, or use a Chrome profile that trusts this daemon HTTPS origin.'
        );
      }
      throw err;
    }
    assert(response, '/connect/bootstrap produced no response');
    assert.strictEqual(response.status(), 200, `/connect/bootstrap returned ${response.status()}`);
    await page.waitForFunction(() => Boolean(window.intendantConnectDashboard));
    const connected = await waitForConnect(page);

    const result = await page.evaluate(async () => {
      const ctl = window.intendantConnectDashboard;
      const beforeChunks = ctl.status().completedChunkedResponses || 0;
      const largeSessions = await ctl.request('api_sessions', { limit: 'all' }, { timeoutMs: 60000 });
      const largeSessionsJson = JSON.stringify(largeSessions);
      return {
        status: await ctl.request('status'),
        config: await ctl.request('config'),
        sessions: await ctl.request('api_sessions', { limit: 2 }),
        largeSessions: {
          ok: Array.isArray(largeSessions),
          length: Array.isArray(largeSessions) ? largeSessions.length : null,
          jsonBytes: new TextEncoder().encode(largeSessionsJson).length,
          completedChunkedResponsesBefore: beforeChunks,
        },
        appError: await ctl.request('api_peer_eligible', { capabilities: [] }),
        finalStatus: ctl.status(),
      };
    });

    assert(result.status && result.status.session_id, 'status RPC did not return a session id');
    assert(result.config && typeof result.config === 'object', 'config RPC did not return an object');
    assert(Array.isArray(result.sessions), 'api_sessions did not return an array');
    assert(
      result.appError && result.appError._httpStatus === 400,
      'application error metadata was not preserved'
    );
    assert(result.largeSessions.ok, 'large api_sessions did not return an array');
    assert(
      result.largeSessions.jsonBytes > 65536,
      `large api_sessions did not cross chunk threshold: ${result.largeSessions.jsonBytes}`
    );
    assert(
      result.finalStatus.completedChunkedResponses > result.largeSessions.completedChunkedResponsesBefore,
      'chunked response counter did not advance'
    );
    assert.strictEqual(
      result.finalStatus.pendingChunkedResponses,
      0,
      'chunked response map was not drained'
    );
    assert.strictEqual(result.finalStatus.pendingRequests, 0, 'request map was not drained');

    console.log(JSON.stringify({
      origin,
      certlessConfigStatus,
      connectStatus: statusBody,
      connected,
      rpc: {
        controlSessionId: result.status.session_id,
        sessionCount: result.sessions.length,
        largeSessionCount: result.largeSessions.length,
        largeSessionBytes: result.largeSessions.jsonBytes,
        completedChunkedResponses: result.finalStatus.completedChunkedResponses,
        appErrorStatus: result.appError._httpStatus,
        pendingRequests: result.finalStatus.pendingRequests,
        pendingChunkedResponses: result.finalStatus.pendingChunkedResponses,
      },
    }, null, 2));

    await page.evaluate(() => window.intendantConnectDashboard.close());
  } finally {
    await browser.close();
  }
}

main().catch(err => {
  console.error(err);
  process.exit(1);
});

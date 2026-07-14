'use strict';

const assert = require('assert');

/**
 * Prove the hosted Connect service is metadata-only: every retired browser
 * signaling mutation is rejected, and the historical dashboard URL redirects
 * to `/connect` without constructing a control client.
 */
async function assertHostedControlUnavailable(page, origin, daemonId, timeoutMs = 45000) {
  await page.goto(`${origin}/connect`, {
    waitUntil: 'domcontentloaded',
    timeout: timeoutMs,
  });
  const attempts = await page.evaluate(async id => {
    const meResponse = await fetch('/api/me');
    const me = await meResponse.json();
    const headers = {
      'content-type': 'application/json',
      'x-intendant-csrf': me.csrf_token || '',
    };
    const requests = [
      ['/api/browser/offer', { daemon_id: id, sdp: 'retired-hosted-offer' }],
      ['/api/browser/ice', {
        daemon_id: id,
        session_id: 'retired-hosted-session',
        candidate: { candidate: 'candidate:retired 1 udp 1 127.0.0.1 9 typ host' },
      }],
      ['/api/browser/close', { daemon_id: id, session_id: 'retired-hosted-session' }],
    ];
    return Promise.all(requests.map(async ([path, body]) => {
      const response = await fetch(path, {
        method: 'POST',
        headers,
        body: JSON.stringify(body),
      });
      return {
        path,
        status: response.status,
        body: await response.text(),
      };
    }));
  }, daemonId);
  for (const attempt of attempts) {
    assert.strictEqual(
      attempt.status,
      403,
      `${attempt.path} must be retired on the hosted service: ${JSON.stringify(attempt)}`
    );
  }

  await page.goto(
    `${origin}/app?connect=1&daemon_id=${encodeURIComponent(daemonId)}`,
    { waitUntil: 'domcontentloaded', timeout: timeoutMs }
  );
  const finalUrl = new URL(typeof page.url === 'function' ? page.url() : page.url);
  assert.strictEqual(finalUrl.pathname, '/connect', `retired /app did not redirect: ${finalUrl}`);
  const browserState = await page.evaluate(() => ({
    dashboardControl: Boolean(window.intendantDashboardControl),
    connectDashboard: Boolean(window.intendantConnectDashboard),
  }));
  assert.strictEqual(browserState.dashboardControl, false, 'redirect constructed a dashboard control client');
  assert.strictEqual(browserState.connectDashboard, false, 'redirect constructed a Connect dashboard client');
  return attempts;
}

module.exports = { assertHostedControlUnavailable };

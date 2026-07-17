// ── Hosted-control lease client ────────────────────────────────────────
// The private key and lease live only in this module's tab memory. Reloading
// the page deliberately requires a new daemon-local approval.
const hostedControlNativeFetch = window.fetch.bind(window);
const HOSTED_DOORBELL_REQUEST_PROTOCOL = 'intendant-hosted-control-doorbell-request-v1';
const HOSTED_POLL_PROTOCOL = 'intendant-hosted-control-poll-v1';
const HOSTED_REQUEST_PROTOCOL = 'intendant-hosted-control-request-v1';
let hostedControlBootstrap = null;
let hostedControlKeyPair = null;
let hostedControlLease = null;
let hostedControlFetchInstalled = false;
let hostedControlManagement = null;
let hostedControlManagementFetchedAt = 0;
let hostedControlManagementInFlight = null;

function hostedControlActive() {
  return !!(hostedControlLease && hostedControlKeyPair?.privateKey);
}

function hostedControlB64u(bytes) {
  const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  let binary = '';
  for (let i = 0; i < view.length; i++) binary += String.fromCharCode(view[i]);
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '');
}

function hostedControlRandomNonce() {
  const bytes = new Uint8Array(24);
  crypto.getRandomValues(bytes);
  return hostedControlB64u(bytes);
}

async function hostedControlSign(payload) {
  if (!hostedControlKeyPair?.privateKey) throw new Error('Hosted lease key is unavailable');
  const signature = await crypto.subtle.sign(
    { name: 'ECDSA', hash: 'SHA-256' },
    hostedControlKeyPair.privateKey,
    new TextEncoder().encode(payload),
  );
  return hostedControlB64u(signature);
}

async function hostedControlSha256(payload) {
  return hostedControlB64u(await crypto.subtle.digest(
    'SHA-256',
    new TextEncoder().encode(payload),
  ));
}

function hostedControlDoorbellPayload(input) {
  return [
    HOSTED_DOORBELL_REQUEST_PROTOCOL,
    input.browser_public_key,
    input.requested_preset,
    String(input.requested_ttl_secs),
    input.requester_label,
    hostedControlBootstrap.fleet_origin,
    hostedControlBootstrap.daemon_id,
    input.nonce,
    String(input.timestamp_unix_ms),
  ].join('\n');
}

function hostedControlDoorbellDocumentPayload(request) {
  return [
    request.protocol,
    request.request_id,
    request.request_nonce,
    request.browser_public_key,
    request.browser_key_fingerprint,
    request.requested_preset,
    String(request.requested_ttl_secs),
    request.requester_label,
    request.fleet_origin,
    request.daemon_id,
    request.daemon_label,
    request.daemon_public_key,
    String(request.created_unix_ms),
    String(request.expires_unix_ms),
  ].join('\n');
}

function hostedControlEnsureGate() {
  let gate = document.getElementById('hosted-control-gate');
  if (gate) return gate;
  gate = document.createElement('div');
  gate.id = 'hosted-control-gate';
  gate.className = 'hosted-control-gate';
  gate.hidden = true;
  gate.innerHTML = `
    <section class="hosted-control-card" role="dialog" aria-modal="true"
             aria-labelledby="hosted-control-title">
      <h1 id="hosted-control-title">Borrow control of this daemon</h1>
      <p id="hosted-control-summary"></p>
      <div class="hosted-control-fingerprint" id="hosted-control-daemon"></div>
      <div class="hosted-control-grid">
        <label>Ceiling preset<select id="hosted-control-preset"></select></label>
        <label>Lease duration<select id="hosted-control-ttl">
          <option value="900">15 minutes</option>
          <option value="3600">1 hour</option>
          <option value="14400">4 hours</option>
          <option value="28800">8 hours</option>
          <option value="86400">24 hours</option>
        </select></label>
        <label style="grid-column:1/-1">This browser
          <input id="hosted-control-label" maxlength="96" autocomplete="off">
        </label>
      </div>
      <p><strong>Confirm on a signed app, the direct-mTLS dashboard, or the
      daemon's local console.</strong> A different device is recommended, not
      required. On borrowed hardware, use cross-device WebAuthn (QR/hybrid) so
      your passkey stays on your phone.</p>
      <div class="hosted-control-actions">
        <button class="primary" id="hosted-control-request" type="button">Request lease</button>
      </div>
      <div class="hosted-control-status" id="hosted-control-status" role="status"
           aria-live="polite"></div>
      <div class="hosted-control-fingerprint" id="hosted-control-key" hidden></div>
    </section>`;
  document.body.appendChild(gate);
  return gate;
}

function hostedControlSetGateStatus(message, error = false) {
  const status = document.getElementById('hosted-control-status');
  if (!status) return;
  status.textContent = message;
  status.classList.toggle('error', error);
}

function hostedControlTtlLabel(seconds) {
  if (seconds % 3600 === 0) {
    const hours = seconds / 3600;
    return `${hours} ${hours === 1 ? 'hour' : 'hours'}`;
  }
  if (seconds % 60 === 0) {
    const minutes = seconds / 60;
    return `${minutes} ${minutes === 1 ? 'minute' : 'minutes'}`;
  }
  return `${seconds} seconds`;
}

function hostedControlEnsureTtlOption(select, seconds) {
  const existing = Array.from(select.options)
    .find(option => Number(option.value) === seconds);
  if (existing) return existing;
  const option = document.createElement('option');
  option.value = String(seconds);
  option.textContent = hostedControlTtlLabel(seconds);
  const next = Array.from(select.options)
    .find(candidate => Number(candidate.value) > seconds);
  select.insertBefore(option, next || null);
  return option;
}

function hostedControlValidateLease(lease, publicKey) {
  if (!lease || lease.protocol !== 'intendant-hosted-control-lease-v1'
      || lease.daemon_id !== hostedControlBootstrap.daemon_id
      || lease.fleet_origin !== hostedControlBootstrap.fleet_origin
      || lease.browser_public_key !== publicKey
      || !['view', 'tasks', 'operate'].includes(lease.preset)
      || Number(lease.expires_unix_ms) <= Date.now()) {
    throw new Error('The daemon returned a lease for a different key or audience');
  }
}

async function hostedControlPoll(request) {
  const requestHash = await hostedControlSha256(hostedControlDoorbellDocumentPayload(request));
  while (Date.now() < Number(request.expires_unix_ms)) {
    const proof = {
      request_id: request.request_id,
      nonce: hostedControlRandomNonce(),
      timestamp_unix_ms: Date.now(),
      signature: '',
    };
    proof.signature = await hostedControlSign([
      HOSTED_POLL_PROTOCOL,
      request.request_id,
      requestHash,
      proof.nonce,
      String(proof.timestamp_unix_ms),
    ].join('\n'));
    const response = await hostedControlNativeFetch('/api/hosted-control/requests/poll', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(proof),
      cache: 'no-store',
    });
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error || `Lease status failed (${response.status})`);
    const status = body.request?.status;
    if (status === 'approved' && body.lease) return body.lease;
    if (status === 'denied') throw new Error('The lease request was denied');
    if (status === 'expired') throw new Error('The lease request expired');
    hostedControlSetGateStatus('Awaiting confirmation on a trusted surface…');
    await new Promise(resolve => setTimeout(resolve, 1500));
  }
  throw new Error('The lease request expired');
}

function hostedControlApplySurface() {
  const preset = hostedControlLease.preset;
  document.documentElement.dataset.hostedPreset = preset;
  document.body.dataset.hostedPreset = preset;
  document.getElementById('direct-mode-toggle')?.removeAttribute('checked');
  const direct = document.getElementById('direct-mode-toggle');
  if (direct) direct.checked = false;
  const project = document.getElementById('new-session-project-root');
  if (project) project.value = '';
  const badge = document.createElement('div');
  badge.className = 'hosted-control-badge';
  badge.textContent = `${preset.toUpperCase()} · lease`;
  badge.title = `Hosted ${preset} lease; expires ${new Date(Number(hostedControlLease.expires_unix_ms)).toLocaleString()}`;
  document.body.appendChild(badge);
}

function hostedControlPathNeedsProof(url) {
  if (url.origin !== location.origin) return false;
  const path = url.pathname;
  if (path === '/' || path === '/favicon.ico' || path === '/audio-processor.js'
      || path.startsWith('/static/') || path.startsWith('/wasm-web/')
      || path.startsWith('/wasm-station/')
      || path === '/.well-known/agent-card.json'
      || path === '/api/hosted-control/bootstrap'
      || path === '/api/hosted-control/requests'
      || path === '/api/hosted-control/requests/poll'
      || path === '/api/hosted-control/anchor-decisions') {
    return false;
  }
  return true;
}

async function hostedControlProofHeaders(method, url) {
  const nonce = hostedControlRandomNonce();
  const timestamp = Date.now();
  const target = url.pathname + url.search;
  const payload = [
    HOSTED_REQUEST_PROTOCOL,
    method.toUpperCase(),
    target,
    hostedControlBootstrap.daemon_id,
    hostedControlLease.document_sha256,
    nonce,
    String(timestamp),
  ].join('\n');
  return {
    'x-intendant-hosted-lease': hostedControlLease.lease_id,
    'x-intendant-hosted-nonce': nonce,
    'x-intendant-hosted-timestamp': String(timestamp),
    'x-intendant-hosted-proof': await hostedControlSign(payload),
  };
}

function hostedControlInstallFetch() {
  if (hostedControlFetchInstalled) return;
  hostedControlFetchInstalled = true;
  window.fetch = async (input, init = {}) => {
    const request = input instanceof Request ? input : null;
    const url = new URL(request ? request.url : String(input), location.href);
    const method = String(init.method || request?.method || 'GET').toUpperCase();
    if (!hostedControlActive() || !hostedControlPathNeedsProof(url)) {
      return hostedControlNativeFetch(input, init);
    }
    const headers = new Headers(request?.headers || undefined);
    new Headers(init.headers || undefined).forEach((value, key) => headers.set(key, value));
    const proof = await hostedControlProofHeaders(method, url);
    Object.entries(proof).forEach(([key, value]) => headers.set(key, value));
    if (request) {
      return hostedControlNativeFetch(new Request(request, { ...init, headers }));
    }
    return hostedControlNativeFetch(input, { ...init, headers });
  };
}

async function hostedControlPrepare() {
  let response;
  try {
    response = await hostedControlNativeFetch('/api/hosted-control/bootstrap', {
      cache: 'no-store',
    });
  } catch {
    return false;
  }
  if (response.status === 404) return false;
  const body = await response.json().catch(() => ({}));
  if (!response.ok) {
    const gate = hostedControlEnsureGate();
    gate.hidden = false;
    hostedControlSetGateStatus(body.error || 'Hosted control is unavailable', true);
    await new Promise(() => {});
  }
  hostedControlBootstrap = body;
  hostedControlKeyPair = await crypto.subtle.generateKey(
    { name: 'ECDSA', namedCurve: 'P-256' },
    false,
    ['sign', 'verify'],
  );
  const publicKey = hostedControlB64u(
    await crypto.subtle.exportKey('raw', hostedControlKeyPair.publicKey),
  );
  const gate = hostedControlEnsureGate();
  gate.hidden = false;
  document.getElementById('hosted-control-summary').textContent =
    `${body.daemon_label || body.daemon_id} will mint only the preset you request, bounded by its ${body.ceiling} ceiling.`;
  document.getElementById('hosted-control-daemon').textContent =
    `daemon ${body.daemon_id}\nidentity ${body.daemon_public_key}`;
  const label = document.getElementById('hosted-control-label');
  label.value = `${navigator.platform || 'Browser'} browser`;
  const presetSelect = document.getElementById('hosted-control-preset');
  const presets = ['view', 'tasks', 'operate'];
  const ceilingIndex = presets.indexOf(body.ceiling);
  presetSelect.replaceChildren(...presets.slice(0, ceilingIndex + 1).map(preset => {
    const option = document.createElement('option');
    option.value = preset;
    option.textContent = preset[0].toUpperCase() + preset.slice(1);
    option.selected = preset === body.default_preset;
    return option;
  }));
  const ttlSelect = document.getElementById('hosted-control-ttl');
  const maxTtl = Number(body.max_ttl_secs);
  const preferredTtl = Math.min(Number(body.default_ttl_secs), maxTtl);
  hostedControlEnsureTtlOption(ttlSelect, preferredTtl);
  for (const option of Array.from(ttlSelect.options)) {
    option.hidden = Number(option.value) > maxTtl;
    option.disabled = option.hidden;
  }
  ttlSelect.value = String(preferredTtl);

  await new Promise(resolve => {
    document.getElementById('hosted-control-request').onclick = async event => {
      const button = event.currentTarget;
      button.disabled = true;
      try {
        const input = {
          browser_public_key: publicKey,
          requested_preset: presetSelect.value,
          requested_ttl_secs: Number(ttlSelect.value),
          requester_label: label.value.trim(),
          nonce: hostedControlRandomNonce(),
          timestamp_unix_ms: Date.now(),
          signature: '',
        };
        if (!input.requester_label) throw new Error('Name this browser first');
        input.signature = await hostedControlSign(hostedControlDoorbellPayload(input));
        const create = await hostedControlNativeFetch('/api/hosted-control/requests', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(input),
          cache: 'no-store',
        });
        const request = await create.json().catch(() => ({}));
        if (!create.ok) throw new Error(request.error || `Lease request failed (${create.status})`);
        const key = document.getElementById('hosted-control-key');
        key.hidden = false;
        key.textContent = `request ${request.request_id}\nkey ${request.browser_key_fingerprint}\npreset ${request.requested_preset} · ${request.requested_ttl_secs}s`;
        hostedControlSetGateStatus('Awaiting confirmation on a trusted surface…');
        const lease = await hostedControlPoll(request);
        hostedControlValidateLease(lease, publicKey);
        hostedControlLease = lease;
        hostedControlInstallFetch();
        hostedControlApplySurface();
        gate.hidden = true;
        resolve();
      } catch (error) {
        hostedControlSetGateStatus(error?.message || String(error), true);
        button.disabled = false;
      }
    };
  });
  return true;
}

async function hostedControlWebSocketUrl(fallbackUrl = '') {
  if (!hostedControlActive()) return fallbackUrl || buildWsUrl();
  const response = await fetch('/api/hosted-control/ws-ticket', {
    method: 'POST',
    cache: 'no-store',
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok || !body.ticket) {
    throw new Error(body.error || `WebSocket ticket failed (${response.status})`);
  }
  const url = new URL(buildWsUrl());
  url.searchParams.set('hosted_ticket', body.ticket);
  return url.toString();
}

window.__intendantReconnectServer = async fallbackUrl => {
  const app = window.__presenceWeb;
  if (!app) return;
  try {
    app.reconnect_server(await hostedControlWebSocketUrl(fallbackUrl));
  } catch (error) {
    console.warn('[hosted-control] reconnect ticket failed', error);
    setTimeout(() => window.__intendantReconnectServer?.(fallbackUrl), 3000);
  }
};

function hostedControlNormalizeControlMessage(payload) {
  if (!hostedControlActive()) return payload;
  const action = String(payload?.action || '').trim();
  const preset = hostedControlLease.preset;
  if (!action) return null;
  const taskShape = () => {
    const task = String(payload.task || payload.text || '').trim();
    if (!task || task.startsWith('/')) return null;
    if (action === 'create_session') {
      const normalized = { action, task };
      const name = String(payload.name || '').trim();
      if (name) normalized.name = name;
      return normalized;
    }
    const sessionId = String(payload.session_id || '').trim();
    if (!sessionId) return null;
    if (action === 'start_task') {
      const normalized = { action, task, session_id: sessionId };
      if (payload.follow_up_id != null) normalized.follow_up_id = payload.follow_up_id;
      return normalized;
    }
    if (action === 'follow_up') {
      const normalized = { action, text: task, session_id: sessionId };
      if (payload.follow_up_id != null) normalized.follow_up_id = payload.follow_up_id;
      return normalized;
    }
    if (action === 'steer') {
      const normalized = { action, text: task, session_id: sessionId };
      if (payload.id != null) normalized.id = payload.id;
      return normalized;
    }
    return null;
  };
  if (['create_session', 'start_task', 'follow_up', 'steer'].includes(action)) {
    return preset === 'view' ? null : taskShape();
  }
  if (['status', 'usage', 'list_displays', 'query_detail'].includes(action)) return payload;
  return preset === 'operate' ? payload : null;
}

function hostedControlEscape(value) {
  return String(value ?? '').replace(/[&<>"']/g, char => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  })[char]);
}

async function hostedControlManagementPost(path, body) {
  const response = await fetch(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  const result = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(result.error || `Hosted-control update failed (${response.status})`);
  hostedControlManagementFetchedAt = 0;
  await hostedControlRefreshManagement(true);
}

async function hostedControlRefreshManagement(force = false) {
  if (hostedControlActive()) return null;
  if (!force && hostedControlManagement && Date.now() - hostedControlManagementFetchedAt < 15000) {
    return hostedControlManagement;
  }
  if (hostedControlManagementInFlight) return hostedControlManagementInFlight;
  hostedControlManagementInFlight = fetch('/api/access/hosted-control', { cache: 'no-store' })
    .then(async response => {
      if (response.status === 404) return null;
      const body = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error(body.error || `Hosted-control state failed (${response.status})`);
      hostedControlManagement = body;
      hostedControlManagementFetchedAt = Date.now();
      return body;
    })
    .finally(() => { hostedControlManagementInFlight = null; });
  return hostedControlManagementInFlight;
}

function hostedControlRenderManagementCard() {
  const mount = document.getElementById('access-hosted-control-card');
  if (!mount || hostedControlActive()) return;
  hostedControlRefreshManagement().then(state => {
    if (!state || !mount.isConnected) return;
    mount.hidden = false;
    const renderKey = JSON.stringify(state);
    if (mount.dataset.hostedControlRenderKey === renderKey) return;
    if (mount.contains(document.activeElement)
        && document.activeElement?.matches('input, select, textarea')) {
      // Access refreshes periodically. Keep a user who is editing a TTL,
      // session id, or preset in control of that field; the next refresh
      // after focus leaves will apply any changed daemon state.
      return;
    }
    mount.dataset.hostedControlRenderKey = renderKey;
    const presetRank = { view: 0, tasks: 1, operate: 2 };
    const pending = (state.pending_requests || []).map(request => {
      const options = ['view', 'tasks', 'operate']
        .filter(preset => presetRank[preset] <= presetRank[request.requested_preset]
          && presetRank[preset] <= presetRank[state.policy.ceiling])
        .map(preset => `<option value="${preset}" ${preset === request.requested_preset ? 'selected' : ''}>${preset}</option>`).join('');
      return `<div class="hosted-control-mgmt-section" data-hosted-request="${hostedControlEscape(request.request_id)}">
        <strong>${hostedControlEscape(request.requester_label)}</strong>
        <div class="hosted-control-fingerprint">${hostedControlEscape(request.browser_key_fingerprint)}</div>
        <div class="hosted-control-mgmt-row">
          <select data-hosted-approved-preset>${options}</select>
          <input data-hosted-approved-ttl type="number" min="60"
                 max="${Number(request.requested_ttl_secs)}"
                 value="${Number(request.requested_ttl_secs)}" aria-label="Approved TTL seconds">
          <button class="primary" data-hosted-decide="approve">Approve</button>
          <button data-hosted-decide="deny">Deny</button>
        </div>
      </div>`;
    }).join('') || '<p>No pending requests.</p>';
    const leases = (state.active_leases || []).map(record => {
      const lease = record.document;
      return `<div class="hosted-control-mgmt-row">
        <span><strong>${hostedControlEscape(lease.preset)}</strong> · expires
        ${hostedControlEscape(new Date(Number(lease.expires_unix_ms)).toLocaleString())}</span>
        <button data-hosted-revoke="${hostedControlEscape(lease.lease_id)}">Revoke</button>
      </div>`;
    }).join('') || '<p>No active leases.</p>';
    const eligible = (state.policy.eligible_session_ids || [])
      .map(id => `<span class="acc-chip">${hostedControlEscape(id)}</span>`).join(' ') || 'None';
    mount.innerHTML = `<div class="acc-section-head">
      <div class="acc-section-title">Hosted control</div>
      <div class="acc-section-sub">Daemon-minted, browser-key-bound leases. Feature:
      ${state.enabled ? 'enabled' : state.configured ? 'initialization error' : 'dark'}.
      Signed-app confirmation is unavailable until a qualifying signed distribution ships.</div>
    </div>
    <div class="hosted-control-mgmt">
      <div class="hosted-control-mgmt-section">
        <strong>Daemon ceiling</strong>
        <div class="hosted-control-mgmt-row">
          <select id="hosted-policy-ceiling">
            ${['view', 'tasks', 'operate'].map(preset =>
              `<option value="${preset}" ${preset === state.policy.ceiling ? 'selected' : ''}>${preset}</option>`).join('')}
          </select>
          <input id="hosted-policy-ttl" type="number" min="60" max="86400"
                 value="${Number(state.policy.max_ttl_secs)}" aria-label="Maximum TTL seconds">
          <label><input id="hosted-policy-operate-ack" type="checkbox">
            I reviewed integrated-daemon hardening before enabling Operate</label>
          <button class="primary" id="hosted-policy-save">Save ceiling</button>
        </div>
      </div>
      <div><strong>Pending lease requests</strong>${pending}</div>
      <div class="hosted-control-mgmt-section"><strong>Active leases</strong>${leases}</div>
      <div class="hosted-control-mgmt-section"><strong>Hosted-eligible sessions</strong>
        <div>${eligible}</div>
        <div class="hosted-control-mgmt-row">
          <input id="hosted-eligible-session" placeholder="session id">
          <button id="hosted-eligible-add">Mark eligible</button>
          <button id="hosted-eligible-remove">Remove</button>
        </div>
        <p>Tasks follows each session's existing autonomy setting. The lease cannot resolve approvals.</p>
      </div>
    </div>`;
    mount.querySelector('#hosted-policy-save').onclick = () => hostedControlManagementPost(
      '/api/access/hosted-control/policy',
      {
        ceiling: mount.querySelector('#hosted-policy-ceiling').value,
        max_ttl_secs: Number(mount.querySelector('#hosted-policy-ttl').value),
        operate_acknowledged: mount.querySelector('#hosted-policy-operate-ack').checked,
      },
    ).then(hostedControlRenderManagementCard).catch(error => showControlToast?.('error', error.message));
    for (const button of mount.querySelectorAll('[data-hosted-decide]')) {
      button.onclick = () => {
        const card = button.closest('[data-hosted-request]');
        const approve = button.dataset.hostedDecide === 'approve';
        hostedControlManagementPost('/api/access/hosted-control/requests/decide', {
          request_id: card.dataset.hostedRequest,
          approve,
          approved_preset: approve ? card.querySelector('[data-hosted-approved-preset]').value : null,
          approved_ttl_secs: approve ? Number(card.querySelector('[data-hosted-approved-ttl]').value) : null,
        }).then(hostedControlRenderManagementCard)
          .catch(error => showControlToast?.('error', error.message));
      };
    }
    for (const button of mount.querySelectorAll('[data-hosted-revoke]')) {
      button.onclick = () => hostedControlManagementPost(
        '/api/access/hosted-control/leases/revoke',
        { lease_id: button.dataset.hostedRevoke },
      ).then(hostedControlRenderManagementCard)
        .catch(error => showControlToast?.('error', error.message));
    }
    const eligibility = eligibleValue => {
      const sessionId = mount.querySelector('#hosted-eligible-session').value.trim();
      if (!sessionId) return;
      hostedControlManagementPost('/api/access/hosted-control/sessions/eligibility', {
        session_id: sessionId,
        eligible: eligibleValue,
      }).then(hostedControlRenderManagementCard)
        .catch(error => showControlToast?.('error', error.message));
    };
    mount.querySelector('#hosted-eligible-add').onclick = () => eligibility(true);
    mount.querySelector('#hosted-eligible-remove').onclick = () => eligibility(false);
  }).catch(error => {
    mount.hidden = false;
    delete mount.dataset.hostedControlRenderKey;
    mount.innerHTML = `<div class="acc-section-head"><div class="acc-section-title">Hosted control</div>
      <div class="acc-section-sub">${hostedControlEscape(error.message)}</div></div>`;
  });
}

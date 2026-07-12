/* ── Access redesign: role metadata, chips, and pane renderers ──
   One vocabulary everywhere: a PRINCIPAL acts, a ROUTE carries the
   request (never authority), and the target DAEMON decides via its
   local IAM. Roles get warmer colors the more they can do. */

const ACCESS_ROLE_META = {
  'role:root': { cls: 'role-root', warn: true, short: 'Full control of this daemon, including access administration.' },
  'role:operator': { cls: 'role-operator', short: 'Operate sessions, display, shell, and files. No access or settings administration.' },
  'role:terminal': { cls: 'role-terminal', short: 'Open and use the interactive shell.' },
  'role:files-write': { cls: 'role-files', short: 'Browse, download, and upload files.' },
  'role:files-read': { cls: 'role-files', short: 'Browse and download files, read-only.' },
  'role:session-reader': { cls: 'role-sessions', short: 'Read sessions, logs, and status.' },
  'role:peer-user': { cls: 'role-peer', short: 'Act through connected peers; each peer\'s own grants bound the reach.' },
  'role:observer': { cls: 'role-observer', short: 'Watch read-only, including live displays.' },
  'role:scoped-human': { cls: 'role-scoped', short: 'Inspect the access model only. A safe first grant.' },
  'role:peer-profile': { cls: 'role-peer', short: 'Daemon-to-daemon authority bounded by the peer profile.' },
  'role:none': { cls: 'role-scoped', short: 'No permissions at all. Ceiling-only: used to refuse hosted-origin control entirely, never granted to anyone.' },
};

/* Trust tiers (docs/src/trust-tiers.md). The id vocabulary mirrors the
   daemon's DAEMON_TIERS constant and is pinned by the
   dashboard_tier_vocabulary_mirrors_daemon_tiers parity test — change
   both together. */
const ACCESS_TIER_META = {
  'integrated': {
    label: 'Integrated',
    glyph: '⌂',
    short: 'Holds your personal world — accounts, files, mail. Protect it: control it directly or through the app, and grant conservatively.',
  },
  'disposable': {
    label: 'Disposable',
    glyph: '♻',
    short: 'Scratch box holding nothing durable. Worst case: rotate a key and destroy it. Hosted tabs are fine here.',
  },
};

/* The curated hosted-ceiling positions, strongest last. Every id must
   name a builtin role (pinned by the role-catalog parity test). */
const ACCESS_HOSTED_CEILING_CHOICES = [
  { id: 'role:operator', label: 'Operate (default)', short: 'Hosted tabs can drive sessions, terminal, files, and fuel credentials — the right setting for disposable boxes.' },
  { id: 'role:observer', label: 'View only', short: 'Hosted tabs can watch displays and sessions but change nothing. Vault fueling from hosted tabs stops working on this daemon.' },
  { id: 'role:none', label: 'Nothing', short: 'This daemon refuses hosted-origin control entirely — reach it directly, through the app, or from an enrolled peer.' },
];

function accessRoleMeta(roleId) {
  const id = accessModelLabel(roleId);
  if (ACCESS_ROLE_META[id]) return ACCESS_ROLE_META[id];
  if (/peer/.test(id)) return { cls: 'role-peer', short: 'Daemon-to-daemon authority.' };
  if (/root/.test(id)) return ACCESS_ROLE_META['role:root'];
  return { cls: 'role-scoped', short: '' };
}

function accessRoleBadge(roleId, labelText, options = {}) {
  const meta = accessRoleMeta(roleId);
  const badge = document.createElement('span');
  badge.className = `acc-badge ${options.cls || meta.cls}`;
  badge.textContent = labelText || accessModelLabel(roleId).replace(/^role:/, '') || 'role';
  const role = accessIamRoleById(roleId);
  badge.title = accessModelLabel(role?.summary, meta.short);
  return badge;
}

function accessIamRoleById(roleId) {
  const id = accessModelLabel(roleId);
  if (!id) return null;
  const iam = accessIamModel(accessOverviewModel());
  const roles = Array.isArray(iam.roles) ? iam.roles : [];
  return roles.find(role => accessModelLabel(role.id) === id) || null;
}

/* Role ceiling that applies to a session authenticated by `authnKind`
   (optionally with the binding's recorded enrollment origin). Mirrors the
   daemon's role_ceiling_for_session so the UI can explain what a session
   will actually be able to do. */
function accessRoleCeilingFor(authnKind, authnOrigin = '') {
  const iam = accessIamModel(accessOverviewModel());
  const ceilings = iam.role_ceilings && typeof iam.role_ceilings === 'object' ? iam.role_ceilings : {};
  const kind = accessModelLabel(authnKind);
  const ceiling = accessModelLabel(ceilings[kind]);
  if (!kind || !ceiling) return null;
  if (kind === 'client_key') {
    const origin = accessModelLabel(authnOrigin).replace(/\/+$/, '');
    if (!origin) return null;
    const hosted = (Array.isArray(iam.hosted_origins) ? iam.hosted_origins : [])
      .some(candidate => accessModelLabel(candidate).replace(/\/+$/, '') === origin);
    if (!hosted) return null;
  }
  return ceiling;
}

/* Ceiling chip for a principal's grants: which of its bindings are capped. */
function accessPrincipalCeiling(principal) {
  const authn = accessOverviewArray(principal, 'authn');
  for (const item of authn) {
    const ceiling = accessRoleCeilingFor(item?.kind, item?.origin);
    if (ceiling) return { ceiling, binding: accessModelLabel(item?.kind) };
  }
  return null;
}

function accessRouteChip(kind, labelText, title = '') {
  const chip = document.createElement('span');
  chip.className = `acc-chip route-${kind || 'local'}`;
  chip.textContent = labelText || '';
  chip.title = title || {
    mtls: 'Direct browser mTLS: your browser holds a client certificate for this daemon.',
    connect: 'Intendant Connect: hosted rendezvous tunnel. A route, not authority — the daemon still checks its local IAM.',
    peer: 'Daemon-to-daemon route over mutual TLS, bounded by a peer profile.',
    local: 'Local or trusted dashboard session on this machine.',
    webrtc: 'Local WebRTC control channel.',
    remembered: 'Remembered in this browser only; not configured on this daemon right now.',
  }[kind] || '';
  return chip;
}

/* How is THIS page reaching the daemon right now? */
function accessCurrentRouteInfo() {
  if (dashboardConnectModeEnabled()) {
    return { kind: 'connect', label: 'Intendant Connect' };
  }
  if (dashboardControlTransportEnabled()) {
    return { kind: 'webrtc', label: 'Local WebRTC' };
  }
  if (location.protocol === 'https:') {
    return { kind: 'mtls', label: 'Browser mTLS' };
  }
  return { kind: 'local', label: 'Local' };
}

function accessRouteInfoForTarget(target, descriptor, rememberedOnly) {
  if (rememberedOnly) return { kind: 'remembered', label: 'Remembered' };
  if (descriptor?.local || target?.local) return accessCurrentRouteInfo();
  return { kind: 'peer', label: 'Peer mTLS' };
}

/* Sync-provenance chip for fleet records that round-trip the hosted store.
   null for live/local targets, which need no such badge. */
function accessTargetProvenanceChip(target) {
  const source = String(target?.source || '');
  if (!/browser_fleet|hosted_access|connect_daemon/.test(source)) return null;
  const id = String(target?.host_id || target?.id || '').trim();
  const provenance = accessFleetProvenance.get(id)
    || (source === 'connect_daemon' ? 'hosted-claim' : null);
  if (!provenance) return null;
  const spec = {
    'verified': { cls: 'webrtc', label: 'synced ✓ this browser', title: 'This fleet record was signed by this browser\u2019s identity key and verified after syncing through the hosted store.' },
    'signed': { cls: 'remembered', label: 'synced · other device', title: 'Signed by a different device\u2019s key; the signature verifies but this browser cannot vouch for the signer.' },
    'unverified': { cls: 'remembered', label: 'synced · unverified', title: 'This record carries no valid owner signature — treat labels and URLs as hints from the metadata store, not facts.' },
    'hosted-claim': { cls: 'connect', label: 'hosted claim', title: 'Attested by the hosted Connect service\u2019s claim records. Navigation metadata only; the daemon still decides all access.' },
  }[provenance];
  if (!spec) return null;
  return accessRouteChip(spec.cls, spec.label, spec.title);
}

/* Best human name for a target: label unless it is a bare address,
   then the URL hostname, then a shortened id. Raw IPs and key-shaped
   ids only appear as the subtitle, never as the headline. */
function accessTargetDisplayName(target, descriptor) {
  const candidates = [
    accessTargetPetname(target),
    descriptor?.displayName,
    target?.label,
  ];
  const urlHost = (() => {
    try {
      const url = target?.url || target?.browser_tcp_via_url || target?.ws_url || descriptor?.peer?.url;
      return url ? new URL(url).hostname : '';
    } catch { return ''; }
  })();
  const idish = value => {
    const text = String(value || '').trim();
    if (!text) return true;
    if (/^\d{1,3}(\.\d{1,3}){3}(:\d+)?$/.test(text)) return true;
    if (/^[A-Za-z0-9_-]{24,}$/.test(text)) return true;
    return false;
  };
  for (const candidate of candidates) {
    const text = String(candidate || '').trim();
    if (text && !idish(text)) return text;
  }
  if (urlHost && !idish(urlHost)) return urlHost;
  const fallback = String(candidates.find(Boolean) || target?.host_id || target?.id || '').trim();
  if (fallback.length > 20 && /^[A-Za-z0-9_-]+$/.test(fallback)) {
    return `${fallback.slice(0, 8)}…${fallback.slice(-4)}`;
  }
  return fallback || 'Daemon';
}

function accessCurrentPrincipalInfo() {
  const principal = dashboardControlTransport?.lastStatus?.access_principal;
  const connectMode = dashboardConnectModeEnabled();
  const route = accessCurrentRouteInfo();
  if (principal && typeof principal === 'object') {
    const authn = Array.isArray(principal.authn) ? principal.authn : [];
    const cert = authn.find(item => accessModelLabel(item?.kind) === 'browser_mtls_cert');
    const connect = authn.find(item => accessModelLabel(item?.kind) === 'connect_account');
    const key = authn.find(item => accessModelLabel(item?.kind) === 'client_key');
    return {
      known: true,
      kind: accessModelLabel(principal.kind, 'browser_session'),
      label: accessModelLabel(principal.label, connectMode ? 'Connect account' : 'This browser'),
      roleId: accessModelLabel(principal.role_id, 'role:root'),
      fingerprint: accessModelLabel(cert?.fingerprint),
      clientKeyFingerprint: accessModelLabel(key?.fingerprint, clientIdentityCache?.fingerprint || ''),
      accountName: accessModelLabel(connect?.account_name || principal.account?.account_name || principal.account?.handle),
      accountUserId: accessModelLabel(connect?.user_id || principal.account?.user_id),
      ceiling: accessRoleCeilingFor(principal.authn_kind, principal.authn_origin),
      route,
    };
  }
  return {
    known: false,
    kind: connectMode ? 'connect_account' : 'browser_session',
    label: connectMode ? 'Connect account' : 'This browser',
    roleId: connectMode ? '' : 'role:root',
    fingerprint: '',
    clientKeyFingerprint: accessModelLabel(clientIdentityCache?.fingerprint),
    accountName: '',
    accountUserId: '',
    route,
  };
}

function accessPrincipalGlyph(kind) {
  const value = accessModelLabel(kind);
  if (/connect|passkey/.test(value)) return { text: '@', cls: 'kind-connect' };
  if (/client_key/.test(value)) return { text: 'KEY', cls: 'kind-cert' };
  if (/certificate|mtls/.test(value)) return { text: 'CRT', cls: 'kind-cert' };
  if (/human/.test(value)) return { text: 'HU', cls: 'kind-human' };
  if (/peer/.test(value)) return { text: 'PD', cls: 'kind-peer' };
  return { text: 'BR', cls: 'kind-session' };
}

function accessHeroLine(label, value, options = {}) {
  const line = document.createElement('div');
  line.className = 'acc-hero-line';
  const key = document.createElement('strong');
  key.textContent = label;
  const val = document.createElement('span');
  if (options.mono) val.classList.add('mono');
  val.textContent = value;
  val.title = options.title || value;
  line.append(key, val);
  return line;
}

function accessHeroCard({ glyph, glyphCls, kicker, title, chips = [], lines = [], stats = [], actions = [] }) {
  const card = document.createElement('div');
  card.className = 'acc-hero-card';
  const glyphEl = document.createElement('div');
  glyphEl.className = `acc-hero-glyph ${glyphCls || ''}`;
  glyphEl.textContent = glyph || '';
  glyphEl.setAttribute('aria-hidden', 'true');
  const kickerEl = document.createElement('div');
  kickerEl.className = 'acc-hero-kicker';
  kickerEl.textContent = kicker || '';
  const titleEl = document.createElement('div');
  titleEl.className = 'acc-hero-title';
  titleEl.textContent = title || '';
  card.append(glyphEl, kickerEl, titleEl);
  if (chips.length) {
    const chipRow = document.createElement('div');
    chipRow.className = 'acc-hero-chips';
    for (const chip of chips) if (chip) chipRow.appendChild(chip);
    card.appendChild(chipRow);
  }
  if (lines.length) {
    const lineWrap = document.createElement('div');
    lineWrap.className = 'acc-hero-lines';
    for (const line of lines) if (line) lineWrap.appendChild(line);
    card.appendChild(lineWrap);
  }
  if (stats.length) {
    const statRow = document.createElement('div');
    statRow.className = 'acc-hero-stats';
    for (const stat of stats) {
      const cell = document.createElement('div');
      cell.className = 'acc-hero-stat';
      cell.title = stat.title || '';
      const n = document.createElement('div');
      n.className = 'n';
      n.textContent = String(stat.value);
      const l = document.createElement('div');
      l.className = 'l';
      l.textContent = stat.label;
      cell.append(n, l);
      if (stat.onClick) cell.addEventListener('click', stat.onClick);
      statRow.appendChild(cell);
    }
    card.appendChild(statRow);
  }
  if (actions.length) {
    const actionRow = document.createElement('div');
    actionRow.className = 'acc-hero-actions';
    for (const action of actions) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'acc-btn' + (action.primary ? ' primary' : '');
      btn.textContent = action.label;
      btn.addEventListener('click', action.onClick);
      actionRow.appendChild(btn);
    }
    card.appendChild(actionRow);
  }
  return card;
}

function accessUserClientPrincipals(overview = accessOverviewModel()) {
  return accessOverviewArray(overview, 'principals')
    .filter(principal => accessModelLabel(principal.kind) !== 'peer_daemon');
}

function accessPeerGrantCounts(overview = accessOverviewModel()) {
  const grants = accessOverviewArray(overview, 'grants');
  const inbound = grants.filter(grant => accessModelLabel(grant.kind) === 'inbound_daemon_peer_profile');
  const outbound = grants.filter(grant => accessModelLabel(grant.kind) === 'daemon_peer_profile');
  return {
    inboundActive: inbound.filter(grant => accessModelLabel(grant.status, 'active') !== 'revoked').length,
    inboundRevoked: inbound.filter(grant => accessModelLabel(grant.status) === 'revoked').length,
    outboundActive: outbound.filter(grant => accessModelLabel(grant.status, 'active') !== 'offline').length,
    outboundTotal: outbound.length,
  };
}

/* This device's own identity-key fingerprint, cached for synchronous
   renderers. The SAS property of the knock ceremony: the approving human
   compares the fingerprint the REQUESTING device displays locally against
   the pending entry on the trusted session — approval by comparison, not
   by timing coincidence. */
let accessOwnDeviceFingerprint = '';
let accessOwnDeviceFingerprintRequested = false;
function accessEnsureOwnFingerprint() {
  if (accessOwnDeviceFingerprintRequested) return;
  accessOwnDeviceFingerprintRequested = true;
  (async () => {
    try {
      const identity = await clientIdentityGet();
      if (identity?.fingerprint) {
        accessOwnDeviceFingerprint = String(identity.fingerprint);
        renderAccessAdminSummaries();
      }
    } catch { /* no identity available — the hint below stays generic */ }
  })();
}

/* Overview: actionable warnings first — an empty div when all is well. */
function renderAccessAttention() {
  const mount = document.getElementById('access-attention');
  if (!mount) return;
  mount.innerHTML = '';
  accessEnsureOwnFingerprint();
  const items = [];
  const status = dashboardTransport?.status
    ? dashboardTransport.status()
    : { enabled: dashboardControlTransportEnabled(), connected: false };
  const summary = dashboardTransportStatusSummary(status);
  const lastError = String(status.lastError || dashboardControlLastError || '').trim();
  if (summary.kind === 'err') {
    if (dashboardConnectModeEnabled()) {
      // Three distinct failures share this err state; the transport records
      // which one happened ('refused' | 'transport' | 'signaling') so the
      // guidance matches reality. IAM advice is reserved for a genuine
      // daemon refusal — sending someone to grant roles over a firewall
      // problem wastes their debugging on the wrong layer.
      const failureKind = String(status.lastErrorKind || dashboardControlLastErrorKind || '').trim();
      if (failureKind === 'refused') {
        items.push({
          kind: 'err',
          icon: '!',
          message: 'Hosted Connect reached this daemon, but the daemon refused dashboard control.',
          detail: lastError,
          steps: [
            'Open the daemon directly (its https://host:8765 address) with root access.',
            'Go to Access → People & Devices and grant your Connect account a role.',
            accessOwnDeviceFingerprint
              ? `Verify it is really this device: this browser's key fingerprint is ${accessOwnDeviceFingerprint} — approve the pending request only if it matches.`
              : 'Compare the pending request’s key fingerprint against this device before approving.',
            'Reload this page — Connect is only the route; the daemon decides.',
          ],
          action: { label: 'Diagnostics', onClick: () => routeTo('access', 'diagnostics') },
        });
      } else if (failureKind === 'transport') {
        items.push({
          kind: 'err',
          icon: '!',
          message: 'This daemon answered through Hosted Connect, but the connection could not be established.',
          detail: lastError,
          steps: [
            'Check that the daemon’s gateway port (its https://host:8765 listener) accepts inbound connections from the internet — a cloud box needs an inbound firewall rule.',
            'The daemon advertises the public address the rendezvous observed for it; an extra NAT or proxy layer can make that address unreachable.',
            'Hard-reload this page to retry with a fresh connection.',
          ],
          action: { label: 'Diagnostics', onClick: () => routeTo('access', 'diagnostics') },
        });
      } else if (failureKind === 'signaling') {
        items.push({
          kind: 'err',
          icon: '!',
          message: 'This daemon did not answer the Hosted Connect offer.',
          detail: lastError,
          steps: [
            'Check that the daemon is running and has internet access — offers only reach it while it polls the rendezvous.',
            'Confirm Connect is still enabled on the daemon and that it is claimed by this account.',
            'Wait a moment and reload this page — a daemon that just started can take a few seconds to begin polling.',
          ],
          action: { label: 'Diagnostics', onClick: () => routeTo('access', 'diagnostics') },
        });
      } else {
        items.push({
          kind: 'err',
          icon: '!',
          message: 'The Hosted Connect control channel failed.',
          detail: lastError,
          action: { label: 'Diagnostics', onClick: () => routeTo('access', 'diagnostics') },
        });
      }
    } else {
      items.push({
        kind: 'err',
        icon: '!',
        message: 'The dashboard control channel failed.',
        detail: lastError,
        action: { label: 'Diagnostics', onClick: () => routeTo('access', 'diagnostics') },
      });
    }
  } else if (summary.kind === 'warn' && status.reconnecting) {
    items.push({
      kind: 'warn',
      icon: '~',
      message: 'Dashboard control is reconnecting.',
      detail: String(status.reconnectReason || '').trim(),
      action: { label: 'Diagnostics', onClick: () => routeTo('access', 'diagnostics') },
    });
  }
  const overview = accessOverviewModel();
  const iam = accessIamModel(overview);
  if (accessModelLabel(iam.load_status) === 'error') {
    items.push({
      kind: 'err',
      icon: '!',
      message: 'This daemon could not read its local IAM state.',
      detail: accessModelLabel(iam.state_path),
      action: { label: 'Inspect', onClick: () => routeTo('access', 'advanced') },
    });
  }
  if (accessPendingEnrollments.length) {
    items.push({
      kind: 'info',
      icon: 'i',
      message: `${accessPlural(accessPendingEnrollments.length, 'device is', 'devices are')} waiting for an enrollment decision.`,
      detail: 'A verified browser key reached this daemon without a grant.',
      action: { label: 'Review', onClick: () => routeTo('access', 'people') },
    });
  }
  const draftGrants = accessOverviewArray(overview, 'grants')
    .filter(grant => accessModelLabel(grant.kind) === 'user_client_local_iam'
      && accessModelLabel(grant.status) === 'draft').length;
  if (draftGrants) {
    items.push({
      kind: 'info',
      icon: 'i',
      message: `${accessPlural(draftGrants, 'draft grant')} ${draftGrants === 1 ? 'is' : 'are'} saved but not enforced.`,
      detail: 'Draft grants deny access until activated.',
      action: { label: 'Review', onClick: () => routeTo('access', 'people') },
    });
  }
  for (const item of items) {
    const node = document.createElement('div');
    node.className = `acc-attention-item ${item.kind}`;
    const icon = document.createElement('div');
    icon.className = 'acc-attention-icon';
    icon.textContent = item.icon;
    const msg = document.createElement('div');
    msg.className = 'acc-attention-msg';
    msg.textContent = item.message;
    if (item.detail) {
      const detail = document.createElement('span');
      detail.className = 'acc-attention-detail';
      detail.textContent = item.detail;
      msg.appendChild(detail);
    }
    if (Array.isArray(item.steps) && item.steps.length) {
      const list = document.createElement('ol');
      list.className = 'acc-attention-steps';
      for (const step of item.steps) {
        const li = document.createElement('li');
        li.textContent = step;
        list.appendChild(li);
      }
      msg.appendChild(list);
    }
    node.append(icon, msg);
    if (item.action) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'acc-btn';
      btn.textContent = item.action.label;
      btn.addEventListener('click', item.action.onClick);
      node.appendChild(btn);
    }
    mount.appendChild(node);
  }
}

function renderAccessIdentityHero() {
  const mount = document.getElementById('access-identity-hero');
  if (!mount) return;
  mount.innerHTML = '';
  const overview = accessOverviewModel();
  const iam = accessIamModel(overview);
  const me = accessCurrentPrincipalInfo();
  const glyph = accessPrincipalGlyph(me.kind);
  const role = accessIamRoleById(me.roleId);
  const roleLabel = accessModelLabel(role?.label, me.roleId ? me.roleId.replace(/^role:/, '') : 'Unknown');
  const roleMeta = accessRoleMeta(me.roleId);
  const permissionCount = Array.isArray(role?.permissions) ? role.permissions.length : 0;

  const youChips = [accessRouteChip(me.route.kind, me.route.label)];
  if (me.roleId) youChips.push(accessRoleBadge(me.roleId, roleLabel));
  const youLines = [];
  if (me.roleId) {
    youLines.push(accessHeroLine('Can do', roleMeta.short || accessModelLabel(role?.summary, ''), {
      title: Array.isArray(role?.permissions) ? role.permissions.join(', ') : '',
    }));
    if (permissionCount) {
      youLines.push(accessHeroLine('Permissions', `${permissionCount} of ${accessOverviewArray(overview, 'permissions').length || 18}`, {
        title: Array.isArray(role?.permissions) ? role.permissions.join(', ') : '',
      }));
    }
  } else {
    youLines.push(accessHeroLine('Can do', 'Not authorized yet — this daemon has no grant for this account.'));
  }
  if (me.ceiling) {
    youLines.push(accessHeroLine('Ceiling', `Sessions on this route are capped at ${me.ceiling.replace(/^role:/, '')} by daemon policy.`, {
      title: 'role_ceilings in iam.json bounds what low-provenance routes can do, regardless of the granted role.',
    }));
  }
  if (me.clientKeyFingerprint) {
    youLines.push(accessHeroLine('Browser key', me.clientKeyFingerprint, {
      mono: true,
      title: 'This origin’s identity key. Grant it on other daemons to sign in there with no certificate install.',
    }));
  }
  if (me.fingerprint) youLines.push(accessHeroLine('Certificate', me.fingerprint, { mono: true }));
  if (me.accountName || me.accountUserId) {
    youLines.push(accessHeroLine('Account', me.accountName ? `@${me.accountName}` : me.accountUserId, {
      mono: !me.accountName,
      title: me.accountUserId,
    }));
  }
  mount.appendChild(accessHeroCard({
    glyph: glyph.text,
    glyphCls: glyph.cls,
    kicker: 'You',
    title: me.label,
    chips: youChips,
    lines: youLines,
    actions: [{ label: 'People & Devices', onClick: () => routeTo('access', 'people') }],
  }));

  const scope = overview.scope || {};
  const targets = accessTargetRecords();
  const peerCounts = accessPeerGrantCounts(overview);
  const people = accessUserClientPrincipals(overview).length;
  const daemonChips = [];
  const iamChip = document.createElement('span');
  iamChip.className = 'acc-chip route-local';
  iamChip.textContent = `IAM ${accessIamLoadLabel(iam)}`;
  iamChip.title = accessIamEnforcementReason(iam);
  daemonChips.push(iamChip);
  const daemonLines = [];
  const hostId = accessModelLabel(scope.target_id, selfPeerId || 'local');
  daemonLines.push(accessHeroLine('Identity', hostId, { mono: true }));
  if (accessModelLabel(iam.state_path)) {
    daemonLines.push(accessHeroLine('IAM store', accessModelLabel(iam.state_path), { mono: true }));
  }
  mount.appendChild(accessHeroCard({
    glyph: 'D',
    glyphCls: 'daemon',
    kicker: 'This daemon',
    title: accessModelLabel(scope.label, selfHostLabel || 'This daemon'),
    chips: daemonChips,
    lines: daemonLines,
    stats: [
      { value: targets.length, label: targets.length === 1 ? 'daemon' : 'daemons', title: 'Daemons this dashboard can reach', onClick: () => routeTo('access', 'daemons') },
      { value: people, label: people === 1 ? 'person or device' : 'people & devices', title: 'User/client principals known to this daemon', onClick: () => routeTo('access', 'people') },
      { value: peerCounts.inboundActive + peerCounts.outboundTotal, label: (peerCounts.inboundActive + peerCounts.outboundTotal) === 1 ? 'peer link' : 'peer links', title: 'Inbound and outbound daemon-to-daemon grants', onClick: () => routeTo('access', 'peers') },
    ],
  }));
}

function renderAccessFleetStrip() {
  const mount = document.getElementById('access-fleet-strip');
  if (!mount) return;
  mount.innerHTML = '';
  const targets = accessTargetRecords();
  for (const target of targets) {
    const hostId = target.local ? '' : String(target.host_id || target.id || '').trim();
    const descriptor = dashboardTargetDescriptor(hostId);
    const rememberedOnly = target.source === 'browser_fleet' && !descriptor.local && !descriptor.peer;
    const summary = dashboardTargetSummary(hostId, 'files');
    const connection = accessTargetConnection(target, descriptor, summary);
    const card = document.createElement('div');
    card.className = `acc-fleet-card ${rememberedOnly ? 'ghost' : (connection.state || 'checking')}`;
    card.title = connection.title || '';
    const top = document.createElement('div');
    top.className = 'acc-fleet-card-top';
    const dot = document.createElement('span');
    dot.className = 'acc-fleet-dot';
    const name = document.createElement('span');
    name.className = 'acc-fleet-name';
    name.textContent = accessTargetDisplayName(target, descriptor);
    top.append(dot, name);
    // Petnamed machines keep their self-reported label visible as a
    // muted secondary — the owner's name never hides what the box calls
    // itself.
    const petname = accessTargetPetname(target);
    if (petname && target.label && petname !== String(target.label).trim()) {
      const selfLabel = document.createElement('span');
      selfLabel.className = 'acc-fleet-selflabel';
      selfLabel.textContent = `· ${target.label}`;
      top.appendChild(selfLabel);
    }
    // Inline rename (✎): the owner's name for this identity, signed
    // into the v5 fleet record (trust-tiers § lookalike names).
    if (!target.local && !descriptor.local) {
      const rename = document.createElement('button');
      rename.type = 'button';
      rename.className = 'acc-fleet-rename';
      rename.textContent = '\u270e';
      rename.title = petname
        ? `Rename "${petname}" — your name for this machine, bound to its identity`
        : 'Name this machine — your name, bound to its identity (a lookalike never inherits it)';
      rename.addEventListener('click', event => {
        event.stopPropagation();
        const input = document.createElement('input');
        input.type = 'text';
        input.className = 'acc-fleet-rename-input';
        input.value = petname;
        input.placeholder = String(target.label || 'name this machine');
        input.maxLength = 120;
        const commit = () => {
          accessFleetSetPetname(target.host_id || target.id, input.value);
          renderAccessFleetStrip();
          renderDashboardTargetSummaries();
        };
        input.addEventListener('keydown', ev => {
          if (ev.key === 'Enter') commit();
          if (ev.key === 'Escape') renderAccessFleetStrip();
          ev.stopPropagation();
        });
        input.addEventListener('blur', commit);
        input.addEventListener('click', ev => ev.stopPropagation());
        name.replaceWith(input);
        input.focus();
        input.select();
      });
      top.appendChild(rename);
    }
    if (target.local || descriptor.local) {
      const self = document.createElement('span');
      self.className = 'acc-fleet-self';
      self.textContent = 'this daemon';
      top.appendChild(self);
    }
    const meta = document.createElement('div');
    meta.className = 'acc-fleet-meta';
    const route = accessRouteInfoForTarget(target, descriptor, rememberedOnly);
    meta.appendChild(accessRouteChip(route.kind, route.label));
    // The two-lane badge is the one authority statement every fleet
    // card wears (trust-tiers § Two lanes): 'you · <role>' or
    // 'via <daemon> · <profile>', warning only when an integrated
    // machine is reached through the delegation lane.
    {
      const lane = dashboardLaneBadge(target.local ? '' : String(target.host_id || target.id || '').trim());
      const badge = document.createElement('span');
      badge.className = `acc-badge lane-${lane.lane}${lane.warn ? ' lane-warn' : ''}`;
      badge.textContent = lane.text;
      if (lane.title) badge.title = lane.title;
      meta.appendChild(badge);
    }
    const provenanceChip = accessTargetProvenanceChip(target);
    if (provenanceChip) meta.appendChild(provenanceChip);
    // Owner-set trust tier from the signed v4 fleet record (or the live
    // targets payload for the current daemon) — the fleet's zones at a
    // glance, offline daemons included. Absent = unset: show nothing.
    const tierMeta = ACCESS_TIER_META[String(target.tier || '').trim()];
    if (tierMeta) {
      const tierChip = document.createElement('span');
      tierChip.className = `acc-chip acc-tier-${String(target.tier).trim()}`;
      tierChip.textContent = `${tierMeta.glyph} ${tierMeta.label}`;
      tierChip.title = tierMeta.short;
      meta.appendChild(tierChip);
    }
    // The door to the direct path (docs/src/trust-tiers.md): when the
    // record carries the daemon's own URL, offer it — a direct tab talks
    // straight to that daemon (local vault, no Connect service in the
    // loop). Self-signed daemons show the browser's certificate warning
    // on first visit until a real cert or the enrollment ceremony.
    const directUrl = String(target.url || descriptor.peer?.url || '').trim();
    if (directUrl && !target.local && !descriptor.local && /^https?:\/\//.test(directUrl)) {
      const direct = document.createElement('button');
      direct.type = 'button';
      direct.className = 'acc-btn acc-fleet-direct';
      direct.textContent = '↗ direct';
      direct.title = `Open ${directUrl} — this daemon's own dashboard, no rendezvous in the loop. Works when this browser can reach it (same LAN/VPN); a self-signed daemon shows a one-time certificate warning.`;
      direct.addEventListener('click', event => {
        event.stopPropagation();
        window.open(directUrl, '_blank', 'noopener');
      });
      meta.appendChild(direct);
    }
    card.append(top, meta);
    card.addEventListener('click', () => routeTo('access', 'daemons'));
    mount.appendChild(card);
  }
  // Hosted fleet-sync health (42-usage-terminal.js sets the flag after a
  // write kept failing through the CSRF-refresh retry): silence here cost
  // real debugging time — records looked synced and simply were not.
  if (accessFleetHostedSyncEnabled() && accessFleetHostedSyncFailing) {
    const warn = document.createElement('span');
    warn.className = 'acc-chip route-danger acc-fleet-sync-warn';
    warn.textContent = 'hosted sync failing';
    warn.title = 'Pushing fleet records to the hosted store keeps failing (even after refreshing the session token). Local records are intact and sync retries on the next change — check the hosted sign-in and network.';
    mount.appendChild(warn);
  }
  const add = document.createElement('button');
  add.type = 'button';
  add.className = 'acc-fleet-add';
  add.textContent = '+ Link a daemon';
  add.addEventListener('click', () => routeTo('access', 'peers'));
  mount.appendChild(add);
}

function renderAccessExplainer() {
  const mount = document.getElementById('access-model-explainer');
  if (!mount || mount.dataset.built === 'true') return;
  mount.dataset.built = 'true';
  {
    // The design's "How access works" section eyebrow.
    const head = document.createElement('div');
    head.className = 'ui2-acc-explainer-head';
    head.textContent = 'How access works';
    mount.appendChild(head);
  }
  const steps = [{
    kicker: 'Who',
    title: 'People, devices & daemons',
    text: 'A principal is whoever is acting: you in a browser, a certificate, a Connect account, or another daemon.',
  }, {
    kicker: 'How',
    title: 'A route, never authority',
    text: 'Browser mTLS, Intendant Connect, or a peer link only carry the request. Reaching a daemon proves nothing by itself.',
  }, {
    kicker: 'Decides',
    title: 'Each daemon, locally',
    text: 'The target daemon checks its own IAM grants and enforces the role. No hosted service can mint authority here.',
  }];
  steps.forEach((step, index) => {
    if (index) {
      const arrow = document.createElement('div');
      arrow.className = 'acc-explainer-arrow';
      arrow.textContent = '→';
      arrow.setAttribute('aria-hidden', 'true');
      mount.appendChild(arrow);
    }
    const node = document.createElement('div');
    node.className = 'acc-explainer-step';
    const kicker = document.createElement('div');
    kicker.className = 'acc-explainer-kicker';
    kicker.textContent = step.kicker;
    const title = document.createElement('div');
    title.className = 'acc-explainer-title';
    title.textContent = step.title;
    const text = document.createElement('div');
    text.className = 'acc-explainer-text';
    text.textContent = step.text;
    node.append(kicker, title, text);
    mount.appendChild(node);
  });
}

/* People & Devices: your identity on this daemon. */
function renderAccessPeopleCurrent() {
  const mount = document.getElementById('access-people-current');
  if (!mount) return;
  mount.innerHTML = '';
  const me = accessCurrentPrincipalInfo();
  const glyph = accessPrincipalGlyph(me.kind);
  const role = accessIamRoleById(me.roleId);
  const roleLabel = accessModelLabel(role?.label, me.roleId ? me.roleId.replace(/^role:/, '') : 'Unknown');
  const chips = [accessRouteChip(me.route.kind, me.route.label)];
  if (me.roleId) chips.push(accessRoleBadge(me.roleId, roleLabel));
  const lines = [];
  if (me.clientKeyFingerprint) {
    lines.push(accessHeroLine('Browser key', me.clientKeyFingerprint, {
      mono: true,
      title: 'Held in this browser’s origin storage; signed into every session offer. Paste this fingerprint into another daemon’s grant form to get access there.',
    }));
  }
  if (me.fingerprint) {
    lines.push(accessHeroLine('Certificate', me.fingerprint, { mono: true }));
  }
  if (me.accountName || me.accountUserId) {
    lines.push(accessHeroLine('Account', me.accountName ? `@${me.accountName}` : me.accountUserId, { title: me.accountUserId }));
  }
  if (!me.clientKeyFingerprint && !me.fingerprint && !me.accountName && !me.accountUserId) {
    lines.push(accessHeroLine('Binding', 'Trusted session only — no persistent identity. Save a grant below to pin this browser or an account to a role.'));
  }
  const actions = [];
  if (me.clientKeyFingerprint && clientIdentityCache) {
    actions.push({
      label: 'Copy key fingerprint',
      onClick: () => {
        navigator.clipboard?.writeText(me.clientKeyFingerprint)
          .then(() => showControlToast?.('success', 'Fingerprint copied'))
          .catch(() => showControlToast?.('error', 'Copy failed'));
      },
    });
    actions.push({
      label: 'Reset key',
      onClick: async () => {
        const ok = await showDashboardConfirm({
          title: 'Reset browser identity key',
          message: 'Grants bound to the current key stop matching this browser. A fresh key is created on the next connection.',
          confirmLabel: 'Reset key',
          cancelLabel: 'Cancel',
          danger: true,
        });
        if (!ok) return;
        try {
          await clientIdentityReset();
          showControlToast?.('success', 'Browser key reset — reload to mint a new one');
        } catch (err) {
          showControlToast?.('error', `Reset failed: ${err?.message || err}`);
        }
        renderAccessAdminSummaries();
      },
    });
  }
  mount.appendChild(accessHeroCard({
    glyph: glyph.text,
    glyphCls: glyph.cls,
    kicker: 'Your identity on this daemon',
    title: me.label,
    chips,
    lines,
    actions,
  }));
}

/* Overview: Trust tier — what this machine holds, and how tightly hosted
   tabs are capped on it (docs/src/trust-tiers.md). The tier is a doctrine
   label (it grants and denies nothing); the ceiling row below it is the
   enforcement, kept visually coupled so choosing "Integrated" naturally
   leads to hardening hosted control. */
function accessHostedCeilingCurrent(iam) {
  const ceilings = iam.role_ceilings && typeof iam.role_ceilings === 'object' ? iam.role_ceilings : {};
  const account = accessModelLabel(ceilings['connect_account']);
  const key = accessModelLabel(ceilings['client_key']);
  if (!account && !key) return { value: '', state: 'uncapped' };
  if (account !== key) return { value: '', state: 'mixed' };
  return { value: account, state: ACCESS_HOSTED_CEILING_CHOICES.some(c => c.id === account) ? 'choice' : 'custom' };
}

function renderAccessTierCard() {
  const mount = document.getElementById('access-tier-card');
  if (!mount) return;
  const iam = accessIamModel(accessOverviewModel());
  const tier = accessModelLabel(iam.tier);
  // daemonApi availability (transport F4): tunnel status boolean when
  // connected, HTTP-twin reachability otherwise — honest in Connect
  // mode, where a down tunnel means no lane at all.
  const canSetTier = daemonApi.availability('api_access_set_tier').ok;
  const canSetCeiling = daemonApi.availability('api_access_set_hosted_ceiling').ok;

  mount.textContent = '';
  const head = document.createElement('div');
  head.className = 'acc-section-head';
  const title = document.createElement('div');
  title.className = 'acc-section-title';
  title.textContent = 'Trust tier';
  const sub = document.createElement('div');
  sub.className = 'acc-section-sub';
  sub.textContent = 'What would a compromise of this machine cost you? The tier is a label that guides grants and clients — the enforcement lives in the hosted-control cap below it.';
  head.append(title, sub);
  mount.appendChild(head);

  const card = document.createElement('div');
  card.className = 'acc-principal-card';

  const headRow = document.createElement('div');
  headRow.className = 'acc-principal-head';
  const glyphEl = document.createElement('div');
  glyphEl.className = 'acc-principal-glyph kind-local';
  glyphEl.textContent = tier ? (ACCESS_TIER_META[tier]?.glyph || 'T') : '?';
  const nameWrap = document.createElement('div');
  const name = document.createElement('div');
  name.className = 'acc-principal-name';
  name.textContent = tier
    ? `${ACCESS_TIER_META[tier]?.label || tier} machine`
    : 'What does this machine hold?';
  const kindLine = document.createElement('div');
  kindLine.className = 'acc-principal-kind';
  kindLine.textContent = tier
    ? (ACCESS_TIER_META[tier]?.short || '')
    : 'Pick a tier so grant flows and clients can hold you to it.';
  nameWrap.append(name, kindLine);
  headRow.append(glyphEl, nameWrap);
  if (!tier) {
    headRow.appendChild(accessRouteChip('remembered', 'not set',
      'No tier chosen yet. The tier changes no permissions by itself — it drives warnings, recommendations, and (via the cap below) hosted-control policy.'));
  }
  card.appendChild(headRow);

  // The two tier options, side by side, current one highlighted.
  const options = document.createElement('div');
  options.className = 'acc-grant-flow-actions acc-tier-options';
  for (const [id, meta] of Object.entries(ACCESS_TIER_META)) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'acc-btn' + (tier === id ? ' primary' : '');
    btn.textContent = `${meta.glyph} ${meta.label}`;
    btn.title = meta.short + (tier === id ? ' (current — click again to clear)' : '');
    btn.disabled = !canSetTier;
    btn.addEventListener('click', () => accessSetTier(tier === id ? null : id));
    options.appendChild(btn);
  }
  card.appendChild(options);

  // Hosted-control cap: the enforcement row.
  const ceiling = accessHostedCeilingCurrent(iam);
  const capRow = document.createElement('div');
  capRow.className = 'acc-grant-flow-actions acc-tier-cap-row';
  const capLabel = document.createElement('span');
  capLabel.className = 'acc-principal-kind';
  capLabel.textContent = 'Hosted tabs may:';
  capLabel.title = 'Applies to sessions arriving with hosted provenance — Connect accounts and browser keys enrolled from a hosted origin. Direct and app sessions are never capped by this. Fleet-name sessions (d-….fleet…) are uncapped by default because this daemon serves their code; to cap them too, add the fleet zone\'s origins to hosted_origins in iam.json (docs: trust-tiers, first contact).';
  capRow.appendChild(capLabel);
  const select = document.createElement('select');
  select.className = 'acc-btn';
  select.disabled = !canSetCeiling;
  for (const choice of ACCESS_HOSTED_CEILING_CHOICES) {
    const opt = document.createElement('option');
    opt.value = choice.id;
    opt.textContent = choice.label;
    opt.title = choice.short;
    select.appendChild(opt);
  }
  if (ceiling.state === 'choice') {
    select.value = ceiling.value;
  } else {
    const opt = document.createElement('option');
    opt.value = '';
    opt.disabled = true;
    opt.selected = true;
    opt.textContent = ceiling.state === 'mixed'
      ? 'custom (per-binding, via iam.json)'
      : ceiling.state === 'custom'
        ? `custom (${ceiling.value.replace(/^role:/, '')})`
        : 'uncapped (via iam.json)';
    select.appendChild(opt);
  }
  select.addEventListener('change', () => {
    // The change is committed immediately — re-baseline the shared render
    // guard so the picked value no longer reads as unsaved work (which
    // would freeze this section against future rebuilds).
    select.dataset.accessGuardBase = select.value;
    if (select.value) accessSetHostedCeiling(select.value);
  });
  capRow.appendChild(select);
  card.appendChild(capRow);

  const hint = document.createElement('div');
  hint.className = 'acc-principal-kind';
  const currentChoice = ACCESS_HOSTED_CEILING_CHOICES.find(c => c.id === ceiling.value);
  hint.textContent = ceiling.state === 'choice'
    ? (currentChoice?.short || '')
    : ceiling.state === 'uncapped'
      ? 'Ceilings are disabled in iam.json — hosted sessions can hold whatever their grant says, including root. Choose a cap here to restore them.'
      : 'This daemon carries a hand-tuned ceiling in iam.json; picking an option here overwrites it for both hosted bindings.';
  card.appendChild(hint);

  // The doctrine nudge: an integrated machine still operable from any
  // hosted tab is the mismatch the tiers chapter warns about.
  if (tier === 'integrated' && ceiling.value === 'role:operator') {
    const nudge = document.createElement('div');
    nudge.className = 'acc-connect-warn';
    const text = document.createElement('span');
    text.textContent = 'Integrated machine, yet hosted tabs can still operate it. Recommended: cap them to View only (or Nothing) and drive this daemon directly or through the app. ';
    const harden = document.createElement('button');
    harden.type = 'button';
    harden.className = 'acc-btn';
    harden.textContent = 'Cap to View only';
    harden.disabled = !canSetCeiling;
    harden.addEventListener('click', () => accessSetHostedCeiling('role:observer'));
    nudge.append(text, harden);
    card.appendChild(nudge);
  }

  mount.appendChild(card);
}

/* Overview: Intendant Connect — this daemon's hosted reachability and
   claim binding. States: off → one-click enable; registering; unclaimed
   → reveal the twelve words (manage-gated, fetched only on click);
   claimed → who owns it, with binding provenance (daemon co-signed vs
   service-asserted vs MISMATCH), plus the daemon-signed release. */
let accessConnectRevealed = null; // claim-code payload cache; dropped on expiry/state change

function accessConnectStateLabel(status) {
  if (!status.configured) return 'Off';
  if (!status.running) return 'Enabled, not running';
  if (!status.registered) return 'Registering…';
  if (status.claimed === true) {
    return status.claimed_by_handle
      ? `Claimed by @${status.claimed_by_handle}`
      : 'Claimed';
  }
  if (status.claimed === false) return 'Awaiting claim';
  return 'Connecting…';
}

function renderAccessConnectCard() {
  const mount = document.getElementById('access-connect-card');
  if (!mount) return;
  mount.innerHTML = '';
  const status = accessConnectStatus;
  // Older daemon or a principal without AccessInspect: stay invisible.
  if (!status || typeof status !== 'object') return;
  if (accessConnectRevealed) {
    const expires = Number(accessConnectRevealed.claim_code_expires_unix_ms || 0);
    const stale = (expires && Date.now() > expires)
      || status.claimed !== false
      || !status.claim_code_available;
    if (stale) accessConnectRevealed = null;
  }
  // daemonApi availability (transport F4) — see renderAccessTierCard.
  const canConfig = daemonApi.availability('api_access_connect_config').ok;
  const canReveal = daemonApi.availability('api_access_connect_claim_code').ok;
  const canUnclaim = daemonApi.availability('api_access_connect_unclaim').ok;

  const head = document.createElement('div');
  head.className = 'acc-section-head';
  const title = document.createElement('div');
  title.className = 'acc-section-title';
  title.textContent = 'Intendant Connect';
  const sub = document.createElement('div');
  sub.className = 'acc-section-sub';
  sub.textContent = 'Hosted reachability for this daemon: it registers with a rendezvous, you claim it into your fleet with a twelve-word phrase, and any browser signed into your account can find it. Claiming grants no authority — every session still resolves against this daemon’s IAM.';
  head.append(title, sub);
  mount.appendChild(head);

  const card = document.createElement('div');
  card.className = 'acc-principal-card';

  const headRow = document.createElement('div');
  headRow.className = 'acc-principal-head';
  const glyphEl = document.createElement('div');
  glyphEl.className = 'acc-principal-glyph kind-connect';
  glyphEl.textContent = 'NET';
  const nameWrap = document.createElement('div');
  const name = document.createElement('div');
  name.className = 'acc-principal-name';
  name.textContent = accessConnectStateLabel(status);
  const kind = document.createElement('div');
  kind.className = 'acc-principal-kind';
  const bits = [];
  if (status.rendezvous_url) bits.push(status.rendezvous_url.replace(/^https?:\/\//, ''));
  if (status.registered && status.last_register_unix_ms) {
    bits.push(`registered ${new Date(Number(status.last_register_unix_ms)).toLocaleTimeString()}`);
  }
  kind.textContent = bits.join(' · ');
  nameWrap.append(name, kind);
  headRow.append(glyphEl, nameWrap);
  if (status.env_forced) {
    headRow.appendChild(accessRouteChip('remembered', 'forced by env',
      'INTENDANT_CONNECT_RENDEZVOUS_URL is set in this daemon’s environment; it overrides intendant.toml, so the toggle here cannot turn Connect off.'));
  }
  if (status.claimed === true) {
    const binding = status.claim_binding;
    if (binding === 'daemon-signed') {
      headRow.appendChild(accessRouteChip('webrtc', 'co-signed ✓',
        'This daemon’s own key co-signed exactly this claim (v2 proof) — the binding is provable, not just asserted by the rendezvous.'));
    } else if (binding === 'mismatch') {
      headRow.appendChild(accessRouteChip('danger', 'BINDING MISMATCH',
        'The rendezvous asserts an owner this daemon never co-signed. Treat the hosted binding as suspect: release the claim and re-claim it yourself.'));
    } else {
      headRow.appendChild(accessRouteChip('remembered', 'service-asserted',
        'No local co-signed record for this claim (made before this daemon kept records, or via an older service) — the owner shown is the rendezvous’s assertion.'));
    }
  }
  // Hosted-bundle code transparency (the CT tripwire's sibling): this
  // daemon periodically fetches what the rendezvous serves and compares
  // it against the rendezvous's own public transparency log.
  if (status.hosted_bundle_state === 'alert') {
    headRow.appendChild(accessRouteChip('danger', 'HOSTED CODE ALERT',
      `This daemon fetched the dashboard code the rendezvous serves and it does not match the rendezvous's public transparency log: ${(status.hosted_bundle_mismatches || []).join(' | ')}. Until the operator explains, treat hosted tabs against this rendezvous as compromised — reach this daemon directly or by its fleet name instead. Re-check out of band with: intendant hosted-verify.`));
  } else if (status.hosted_bundle_state === 'ok') {
    headRow.appendChild(accessRouteChip('webrtc', 'hosted code ✓',
      'What this rendezvous serves matches its public transparency log'
      + (status.hosted_bundle_checked_unix_ms ? ` (checked ${new Date(Number(status.hosted_bundle_checked_unix_ms)).toLocaleString()})` : '')
      + (status.hosted_bundle_last_error ? `. Last attempt failed: ${status.hosted_bundle_last_error} — the verdict shown is from the last completed check.` : '')
      + '. Verified out of band by this daemon (page JS can never honestly verify the origin serving it).'));
  }
  card.appendChild(headRow);

  if (status.claimed === true && status.claim_binding === 'mismatch') {
    const warn = document.createElement('div');
    warn.className = 'acc-connect-warn';
    const signed = status.signed_claim || {};
    warn.textContent = `The rendezvous says this daemon belongs to ${status.claimed_by_handle ? '@' + status.claimed_by_handle : (status.claimed_by_user_id || 'an unknown account')}, but this daemon co-signed a claim by ${signed.account_name ? '@' + signed.account_name : (signed.account_user_id || 'a different account')}. Release the claim below and re-claim from your own account.`;
    card.appendChild(warn);
  }

  if (status.last_error) {
    const err = document.createElement('div');
    err.className = 'acc-connect-warn';
    err.textContent = `Last error: ${status.last_error}`;
    card.appendChild(err);
  }

  // Fleet certificate (docs/src/trust-tiers.md, the convenient direct
  // path): when the rendezvous serves a fleet DNS zone, this daemon owns
  // a real name — one click publishes its addresses and mints a
  // Let's Encrypt certificate, so the direct dashboard gets a warning-free
  // padlock (LAN address + public name included).
  const fleetCert = status.fleet_cert || {};
  if (fleetCert.name) {
    const row = document.createElement('div');
    row.className = 'acc-grant-flow-actions';
    const label = document.createElement('span');
    label.className = 'acc-principal-kind';
    const validUntil = fleetCert.not_after_unix_ms
      ? new Date(Number(fleetCert.not_after_unix_ms)).toLocaleDateString()
      : '';
    label.textContent = `Fleet name: ${fleetCert.name}`;
    label.title = fleetCert.addresses?.length
      ? `Resolves to ${fleetCert.addresses.join(', ')} (published by this daemon; LAN addresses are fine — the certificate makes the padlock, not the address).`
      : 'No addresses published yet — requesting a certificate publishes this daemon’s routable addresses first.';
    row.appendChild(label);
    const chip = document.createElement('span');
    if (fleetCert.ct_state === 'alert') {
      const ct = document.createElement('span');
      ct.className = 'acc-chip route-danger';
      ct.textContent = 'CT ALERT';
      ct.title = `The public Certificate Transparency logs hold ${fleetCert.ct_unknown?.length || 'unknown'} certificate(s) for this daemon's fleet name that this daemon never requested: ${(fleetCert.ct_unknown || []).join(' | ')}. If you did not mint these yourself through another channel, someone who controls the fleet zone (or a CA) issued a certificate for this name — treat the fleet route as compromised and reach this daemon directly or through the app.`;
      row.appendChild(ct);
    }
    if (fleetCert.state === 'valid') {
      chip.className = 'acc-chip route-webrtc';
      chip.textContent = validUntil ? `certificate ✓ until ${validUntil}` : 'certificate ✓';
      chip.title = 'A browser-trusted Let’s Encrypt certificate is serving for this name — open the direct dashboard at https://' + fleetCert.name + (window.location.port ? ':' + window.location.port : '') + ' without warnings. Renewals are automatic.'
        + (fleetCert.ct_state === 'ok' && fleetCert.ct_checked_unix_ms
          ? ` CT tripwire: all publicly logged certificates for this name are this daemon's own (checked ${new Date(Number(fleetCert.ct_checked_unix_ms)).toLocaleString()}).`
          : '');
    } else if (fleetCert.state === 'requesting') {
      chip.className = 'acc-chip route-remembered';
      chip.textContent = 'requesting…';
      chip.title = 'Publishing DNS records and answering the Let’s Encrypt challenge — usually under a minute.';
    } else if (fleetCert.state === 'error') {
      chip.className = 'acc-chip route-danger';
      chip.textContent = 'certificate error';
      chip.title = fleetCert.last_error || 'The last certificate request failed.';
    }
    if (chip.textContent) row.appendChild(chip);
    const canRequest = daemonApi.availability('api_fleet_cert_request').ok;
    if (fleetCert.state !== 'requesting' && fleetCert.state !== 'valid') {
      const request = document.createElement('button');
      request.type = 'button';
      request.className = 'acc-btn primary';
      request.textContent = 'Get a real certificate';
      request.title = 'Publishes this daemon’s routable addresses under its fleet name and mints a Let’s Encrypt certificate (DNS-01 through the rendezvous — the private key never leaves this machine). The name lands in public CT logs; it is an opaque hash for that reason.';
      request.disabled = !canRequest;
      request.addEventListener('click', () => accessFleetCertRequest());
      row.appendChild(request);
    }
    if (fleetCert.state === 'error' && fleetCert.last_error) {
      const err = document.createElement('div');
      err.className = 'acc-connect-warn';
      err.textContent = `Certificate: ${fleetCert.last_error}`;
      card.appendChild(err);
    }
    card.appendChild(row);
  }

  // The reveal: the twelve words, full-width, with where to type them.
  if (accessConnectRevealed?.claim_code) {
    const phraseWrap = document.createElement('div');
    phraseWrap.className = 'acc-connect-phrase';
    for (const word of String(accessConnectRevealed.claim_code).split('-')) {
      const el = document.createElement('span');
      el.className = 'acc-connect-word';
      el.textContent = word;
      phraseWrap.appendChild(el);
    }
    card.appendChild(phraseWrap);
    const hint = document.createElement('div');
    hint.className = 'acc-principal-kind';
    const expires = Number(accessConnectRevealed.claim_code_expires_unix_ms || 0);
    const expiresText = expires
      ? ` · expires ${new Date(expires).toLocaleTimeString()} (a fresh phrase mints automatically)`
      : '';
    hint.textContent = status.bootstrap
      ? `First-owner bootstrap: this daemon minted the phrase itself (the rendezvous holds only its hash). Entering it claims the daemon AND enrolls the claiming browser as owner${expiresText || ' · valid while this daemon runs'}`
      : `Sign in and enter this phrase to claim this daemon into your fleet${expiresText}`;
    card.appendChild(hint);
    if (accessConnectRevealed.claim_url) {
      const linkRow = document.createElement('div');
      linkRow.className = 'acc-principal-authn';
      const link = document.createElement('a');
      link.href = accessConnectRevealed.claim_url;
      link.target = '_blank';
      link.rel = 'noopener';
      link.textContent = accessConnectRevealed.claim_url;
      linkRow.appendChild(link);
      card.appendChild(linkRow);
    }
  }

  const actions = document.createElement('div');
  actions.className = 'acc-grant-flow-actions';
  if (!status.configured) {
    const enable = document.createElement('button');
    enable.type = 'button';
    enable.className = 'acc-btn primary';
    enable.textContent = 'Turn on Connect';
    enable.title = `Registers this daemon with ${status.default_rendezvous_url || 'the hosted rendezvous'} and writes [connect] to intendant.toml. You can claim it with the twelve-word phrase afterwards.`;
    enable.disabled = !canConfig;
    enable.addEventListener('click', () => accessConnectSetEnabled(true));
    actions.appendChild(enable);
  } else {
    if (status.claimed === false && status.claim_code_available && !accessConnectRevealed) {
      const reveal = document.createElement('button');
      reveal.type = 'button';
      reveal.className = 'acc-btn primary';
      reveal.textContent = 'Reveal claim phrase';
      reveal.title = 'Shows the twelve words that bind this daemon to an account. Anyone who sees them (for their ten-minute lifetime) can claim this daemon into their fleet — claiming grants no session authority, but release requires this card or the account side.';
      reveal.disabled = !canReveal;
      reveal.addEventListener('click', async () => {
        try {
          accessConnectRevealed = await accessConnectFetchClaimCode();
        } catch (err) {
          showControlToast?.('error', err?.message || 'Claim phrase fetch failed');
          accessConnectRevealed = null;
        }
        renderAccessAdminSummaries();
      });
      actions.appendChild(reveal);
    }
    if (accessConnectRevealed) {
      const hide = document.createElement('button');
      hide.type = 'button';
      hide.className = 'acc-btn';
      hide.textContent = 'Hide phrase';
      hide.addEventListener('click', () => {
        accessConnectRevealed = null;
        renderAccessAdminSummaries();
      });
      actions.appendChild(hide);
    }
    if (status.claimed === true) {
      const release = document.createElement('button');
      release.type = 'button';
      release.className = 'acc-btn danger';
      release.textContent = 'Release claim';
      release.title = 'Daemon-signed release: detaches this daemon from the account that claimed it (fleet entry, presence, notifications). Sessions and IAM grants are untouched. A fresh claim phrase mints right after.';
      release.disabled = !canUnclaim;
      // Two-click confirm — no blocking dialogs in the dashboard.
      release.addEventListener('click', () => {
        if (release.dataset.armed === '1') {
          release.disabled = true;
          accessConnectUnclaim();
          return;
        }
        release.dataset.armed = '1';
        release.textContent = 'Confirm release';
        setTimeout(() => {
          release.dataset.armed = '';
          release.textContent = 'Release claim';
        }, 5000);
      });
      actions.appendChild(release);
    }
    const disable = document.createElement('button');
    disable.type = 'button';
    disable.className = 'acc-btn';
    disable.textContent = 'Turn off';
    disable.title = status.env_forced
      ? 'Connect is forced on by INTENDANT_CONNECT_RENDEZVOUS_URL in the daemon’s environment; unset it to allow turning off here.'
      : 'Stops registering with the rendezvous and persists [connect] enabled = false. An existing claim binding stays at the service until released.';
    disable.disabled = !canConfig || status.env_forced;
    disable.addEventListener('click', () => accessConnectSetEnabled(false));
    actions.appendChild(disable);
  }
  card.appendChild(actions);
  mount.appendChild(card);
}

/* People & Devices: devices knocking on this daemon, awaiting a decision.
   Empty (and invisible) when nothing is pending. */
function renderAccessEnrollmentRequests() {
  const mount = document.getElementById('access-enrollment-requests');
  if (!mount) return;
  mount.innerHTML = '';
  if (!accessPendingEnrollments.length) return;
  const canManage = daemonApi.availability('api_access_enrollment_decide').ok;

  const head = document.createElement('div');
  head.className = 'acc-section-head';
  const title = document.createElement('div');
  title.className = 'acc-section-title';
  title.textContent = 'Pending devices';
  const sub = document.createElement('div');
  sub.className = 'acc-section-sub';
  sub.textContent = 'Browsers whose verified identity key reached this daemon without a grant. Approving writes a normal IAM grant; hosted-route keys stay under the role ceiling.';
  head.append(title, sub);
  mount.appendChild(head);

  const list = document.createElement('div');
  list.className = 'acc-principal-list';
  for (const request of accessPendingEnrollments) {
    const fingerprint = accessModelLabel(request.fingerprint);
    const card = document.createElement('div');
    card.className = 'acc-principal-card';
    const headRow = document.createElement('div');
    headRow.className = 'acc-principal-head';
    const glyphEl = document.createElement('div');
    glyphEl.className = 'acc-principal-glyph kind-cert';
    glyphEl.textContent = 'KEY';
    const nameWrap = document.createElement('div');
    const name = document.createElement('div');
    name.className = 'acc-principal-name';
    name.textContent = request.account_hint
      ? `${request.account_hint} — new device`
      : 'New device';
    const kind = document.createElement('div');
    kind.className = 'acc-principal-kind';
    const attempts = Number(request.attempts || 1);
    kind.textContent = `${attempts === 1 ? '1 attempt' : `${attempts} attempts`} · last ${new Date(Number(request.last_seen_unix_ms || 0)).toLocaleString()}`;
    nameWrap.append(name, kind);
    headRow.append(glyphEl, nameWrap);
    // Route provenance chip: which first-contact rung the key arrived on
    // (classified daemon-side — origin_class; docs/src/trust-tiers.md).
    const originClass = request.origin_class || (request.origin ? 'hosted' : 'unknown');
    if (originClass === 'hosted') {
      headRow.appendChild(accessRouteChip('connect', 'via hosted route', `The offer arrived through ${request.origin}; the rendezvous serves that page's code, so the hosted role ceiling applies until the key is re-enrolled from a daemon-served origin.`));
    } else if (originClass === 'fleet') {
      headRow.appendChild(accessRouteChip('remembered', 'via fleet name', `The offer arrived through ${request.origin} — this daemon serves that page's code, but the rendezvous names the route (it could hijack DNS and mint a certificate; such an attack is active-only and lands in public CT logs, which this daemon monitors). Rung two of first contact: stronger than hosted, weaker than a typed address. No ceiling applies by default; add this exact origin to hosted_origins in iam.json to cap it.`));
    } else if (request.origin) {
      headRow.appendChild(accessRouteChip('local', 'via direct origin', `The offer arrived through ${request.origin} — a daemon-served origin the rendezvous neither serves nor names.`));
    }
    if (request.origin && accessModelLabel(accessIamModel(accessOverviewModel()).tier) === 'integrated') {
      headRow.appendChild(accessRouteChip('danger', 'integrated tier',
        'This is an integrated-tier machine and the key arrived over the network. Approving grants remote authority here — verify the fingerprint below against the requesting device before approving anything above observer (docs: trust-tiers).'));
    }
    if (request.account_hint) {
      headRow.appendChild(request.account_attested
        ? accessRouteChip('webrtc', 'account attested ✓', 'The device key itself signed this account claim (v2 offer) — the name is as trustworthy as the key fingerprint below.')
        : accessRouteChip('remembered', 'account via route', 'The account name is asserted by the signaling relay, not signed by the device key. Verify the key fingerprint against the requesting device before trusting the name.'));
    }
    card.appendChild(headRow);

    const authn = document.createElement('div');
    authn.className = 'acc-principal-authn';
    authn.textContent = `Key fingerprint ${fingerprint}`;
    authn.title = fingerprint;
    card.appendChild(authn);

    const actions = document.createElement('div');
    actions.className = 'acc-grant-flow-actions';
    const roleSelect = document.createElement('select');
    roleSelect.className = 'acc-btn';
    roleSelect.title = 'Role the approved device receives on this daemon';
    for (const role of accessAssignableIamRoles()) {
      const option = document.createElement('option');
      option.value = accessModelLabel(role.id);
      option.textContent = accessModelLabel(role.label, role.id);
      if (accessModelLabel(role.id) === 'role:observer') option.selected = true;
      roleSelect.appendChild(option);
    }
    const approve = document.createElement('button');
    approve.type = 'button';
    approve.className = 'acc-btn primary';
    approve.textContent = 'Approve';
    approve.disabled = !canManage;
    approve.addEventListener('click', () => accessDecideEnrollment(fingerprint, true, roleSelect.value));
    const deny = document.createElement('button');
    deny.type = 'button';
    deny.className = 'acc-btn danger';
    deny.textContent = 'Deny';
    deny.disabled = !canManage;
    deny.addEventListener('click', () => accessDecideEnrollment(fingerprint, false));
    actions.append(roleSelect, approve, deny);
    card.appendChild(actions);
    list.appendChild(card);
  }
  mount.appendChild(list);
}

/* People & Devices: principal cards with their grants and lifecycle. */
function renderAccessPeopleGrants() {
  const mount = document.getElementById('access-people-grants');
  if (!mount) return;
  mount.innerHTML = '';
  const overview = accessOverviewModel();
  const targetLabels = accessOverviewTargetLabelMap(overview);
  const grantsByPrincipal = accessGrantsBy(overview, 'principal_id');
  const principals = accessUserClientPrincipals(overview);
  if (!principals.length) {
    const empty = document.createElement('div');
    empty.className = 'access-empty';
    empty.textContent = 'No people or devices yet — save a grant above to add one.';
    mount.appendChild(empty);
    return;
  }
  const canManage = daemonApi.availability('api_access_iam_update_grant').ok;
  for (const principal of principals) {
    const card = document.createElement('div');
    card.className = 'acc-principal-card';
    const head = document.createElement('div');
    head.className = 'acc-principal-head';
    const glyph = accessPrincipalGlyph(principal.kind);
    const glyphEl = document.createElement('div');
    glyphEl.className = `acc-principal-glyph ${glyph.cls}`;
    glyphEl.textContent = glyph.text;
    const nameWrap = document.createElement('div');
    const name = document.createElement('div');
    name.className = 'acc-principal-name';
    name.textContent = accessModelLabel(principal.label, principal.id || 'Principal');
    const kind = document.createElement('div');
    kind.className = 'acc-principal-kind';
    kind.textContent = accessModelLabel(principal.kind_label || principal.kind, 'Principal');
    nameWrap.append(name, kind);
    head.append(glyphEl, nameWrap);
    head.appendChild(accessModelCreateStatus(principal.status || (principal.local ? 'current' : 'active')));
    card.appendChild(head);
    const authn = accessOverviewArray(principal, 'authn')
      .map(item => {
        const label = accessModelLabel(item.label || item.kind);
        const fp = accessModelLabel(item.fingerprint);
        return fp ? `${label} ${fp.slice(0, 18)}…` : label;
      })
      .filter(Boolean)
      .join(' · ');
    if (authn) {
      const authnEl = document.createElement('div');
      authnEl.className = 'acc-principal-authn';
      authnEl.textContent = authn;
      authnEl.title = accessModelLabel(principal.id);
      card.appendChild(authnEl);
    }
    const grants = grantsByPrincipal.get(accessModelLabel(principal.id)) || [];
    if (grants.length) {
      const rows = document.createElement('div');
      rows.className = 'acc-grant-rows';
      for (const grant of grants) {
        rows.appendChild(accessCreateGrantRow(grant, targetLabels, canManage, principal));
      }
      card.appendChild(rows);
    }
    mount.appendChild(card);
  }
}

function accessCreateGrantRow(grant, targetLabels, canManage, principal = null) {
  const row = document.createElement('div');
  row.className = 'acc-grant-row';
  const grantId = accessModelLabel(grant.id);
  const status = accessModelLabel(grant.status, 'active');
  const roleId = accessModelLabel(grant.role_id || grant.role);
  row.appendChild(accessRoleBadge(roleId.startsWith('role:') ? roleId : `role:${roleId}`, accessModelLabel(grant.role_label || grant.profile || grant.role, 'Role')));
  const target = document.createElement('span');
  target.className = 't';
  target.textContent = `on ${targetLabels.get(accessModelLabel(grant.target_id)) || accessModelLabel(grant.target_id, 'this daemon')}`;
  row.appendChild(target);
  row.appendChild(accessModelCreateStatus(status));
  const capped = principal ? accessPrincipalCeiling(principal) : null;
  if (capped) {
    const chip = document.createElement('span');
    chip.className = 'acc-chip route-connect';
    chip.textContent = `ceiling: ${capped.ceiling.replace(/^role:/, '')}`;
    chip.title = `Sessions authenticated by this principal's ${capped.binding.replace(/_/g, ' ')} binding are capped at ${capped.ceiling} by this daemon's role_ceilings policy, regardless of the granted role.`;
    row.appendChild(chip);
  }
  const fsScope = grant.fs_scope;
  if (fsScope && ((fsScope.read_roots || []).length || (fsScope.write_roots || []).length)) {
    const chip = document.createElement('span');
    chip.className = 'acc-chip route-local';
    const reads = (fsScope.read_roots || []).length;
    const writes = (fsScope.write_roots || []).length;
    chip.textContent = `fs: ${reads}r/${writes}w roots`;
    chip.title = 'Filesystem scope\nread:\n  ' + ((fsScope.read_roots || []).join('\n  ') || '(none)')
      + '\nwrite:\n  ' + ((fsScope.write_roots || []).join('\n  ') || '(none)');
    row.appendChild(chip);
  }
  const expiresAt = Number(grant.expires_at_unix_ms || 0);
  if (expiresAt) {
    const expires = document.createElement('span');
    expires.className = 't';
    const remaining = expiresAt - Date.now();
    expires.textContent = remaining > 0
      ? `expires in ${remaining > 86_400_000
        ? `${Math.round(remaining / 86_400_000)}d`
        : (remaining > 3_600_000 ? `${Math.round(remaining / 3_600_000)}h` : `${Math.max(1, Math.round(remaining / 60_000))}m`)}`
      : 'expired';
    expires.title = new Date(expiresAt).toLocaleString();
    row.appendChild(expires);
  }
  const localIamGrant = accessModelLabel(grant.kind) === 'user_client_local_iam';
  if (localIamGrant && grantId) {
    const busy = accessGrantLifecycleSubmitting.has(grantId);
    const actions = document.createElement('div');
    actions.className = 'acc-grant-row-actions';
    const mkBtn = (label, opts, onClick) => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'acc-btn' + (opts.primary ? ' primary' : '') + (opts.danger ? ' danger' : '');
      btn.textContent = busy ? 'Saving' : label;
      btn.disabled = busy || !canManage;
      btn.addEventListener('click', onClick);
      return btn;
    };
    if (status !== 'active') actions.appendChild(mkBtn('Activate', { primary: true }, () => accessUpdateGrantLifecycle(grantId, { status: 'active' })));
    if (status !== 'draft') actions.appendChild(mkBtn('Draft', {}, () => accessUpdateGrantLifecycle(grantId, { status: 'draft' })));
    if (status !== 'revoked') actions.appendChild(mkBtn('Revoke', { danger: true }, () => accessConfirmRevokeGrant(grantId)));
    row.appendChild(actions);
  }
  const why = document.createElement('div');
  why.className = 'why';
  why.textContent = accessGrantWhy(grant);
  row.appendChild(why);
  return row;
}

/* Peers: inbound/outbound summary. */
function renderAccessPeerSummary() {
  const mount = document.getElementById('access-peer-summary');
  if (!mount) return;
  mount.innerHTML = '';
  const counts = accessPeerGrantCounts();
  mount.appendChild(accessHeroCard({
    glyph: 'IN',
    glyphCls: 'daemon',
    kicker: 'Inbound',
    title: counts.inboundActive
      ? accessPlural(counts.inboundActive, 'daemon may call in', 'daemons may call in')
      : 'No inbound peer access',
    lines: [accessHeroLine('Meaning', 'Approved peer identities that may call capabilities on this daemon, bounded by their profile.'),
      ...(counts.inboundRevoked ? [accessHeroLine('Revoked', `${counts.inboundRevoked} kept visible for audit`)] : [])],
  }));
  mount.appendChild(accessHeroCard({
    glyph: 'OUT',
    glyphCls: 'daemon',
    kicker: 'Outbound',
    title: counts.outboundTotal
      ? `${counts.outboundActive}/${counts.outboundTotal} routes active`
      : 'No outbound peer routes',
    lines: [accessHeroLine('Meaning', 'Peer daemons this one can reach. Operate them from the Daemons pane.')],
    actions: [{ label: 'Open Daemons', onClick: () => routeTo('access', 'daemons') }],
  }));
}

/* Advanced: role catalog cards. */
function renderAccessRoleCatalog() {
  const mount = document.getElementById('access-role-catalog');
  if (!mount) return;
  mount.innerHTML = '';
  const iam = accessIamModel(accessOverviewModel());
  const roles = Array.isArray(iam.roles) && iam.roles.length ? iam.roles : accessFallbackIamRoles();
  for (const role of roles) {
    const node = document.createElement('div');
    node.className = 'acc-role-info';
    const top = document.createElement('div');
    top.className = 'acc-role-info-top';
    top.appendChild(accessRoleBadge(role.id, accessModelLabel(role.label, role.id)));
    top.appendChild(accessModelCreateStatus(role.status || 'enforced'));
    const desc = document.createElement('div');
    desc.className = 'd';
    desc.textContent = accessModelLabel(role.summary, accessRoleMeta(role.id).short);
    const perms = document.createElement('div');
    perms.className = 'perms';
    const list = Array.isArray(role.permissions) ? role.permissions : [];
    perms.textContent = list.length ? list.join(' · ') : 'No permissions';
    perms.title = list.join(', ');
    node.append(top, desc, perms);
    mount.appendChild(node);
  }
}

/* Advanced: trusted organizations + issuance. */
// daemonApi (transport F4): every org call is a POST twin — the fallback
// policy derives no-replay from the verb, exactly the legacy
// fallbackAfterRpcFailure:false semantics (a delivered tunnel attempt is
// never replayed over HTTP; with no tunnel the write goes direct; Connect
// mode never uses HTTP). The descriptor owns each method's path — the old
// per-call-site path argument died with the tri-form. IAM-mutating
// writes: payload shapes unchanged, zero retries.
async function accessOrgCall(method, payload) {
  const resp = await daemonApi.request(method, payload ?? {});
  if (!resp.ok) throw new Error(resp.body?.error || `request failed (${resp.status})`);
  return resp.body;
}

function renderAccessOrganizations() {
  const mount = document.getElementById('access-organizations');
  if (!mount) return;
  mount.innerHTML = '';
  const iam = accessIamModel(accessOverviewModel());
  const trusted = Array.isArray(iam.trusted_orgs) ? iam.trusted_orgs : [];
  const issuers = Array.isArray(iam.org_issuers) ? iam.org_issuers : [];
  const canManage = daemonApi.availability('api_access_org_trust').ok;

  const list = document.createElement('div');
  list.className = 'acc-principal-list';
  for (const org of trusted) {
    const card = document.createElement('div');
    card.className = 'acc-principal-card';
    const head = document.createElement('div');
    head.className = 'acc-principal-head';
    const glyph = document.createElement('div');
    glyph.className = 'acc-principal-glyph kind-human';
    glyph.textContent = 'ORG';
    const nameWrap = document.createElement('div');
    const name = document.createElement('div');
    name.className = 'acc-principal-name';
    name.textContent = `@${accessModelLabel(org.handle)}`;
    const kind = document.createElement('div');
    kind.className = 'acc-principal-kind';
    const key = accessModelLabel(org.root_key);
    const orlSeq = Number(org.last_orl_seq || 0);
    const peerCap = String(org.max_peer_profile || '').trim();
    kind.textContent = `root key ${key.slice(0, 12)}…${key.slice(-4)} · peer cap: ${peerCap || 'none'}${orlSeq > 0 ? ` · revocations seq ${orlSeq}` : ''}`;
    kind.title = key;
    nameWrap.append(name, kind);
    head.append(glyph, nameWrap);
    head.appendChild(accessRoleBadge(accessModelLabel(org.max_role, 'role:operator'), `cap: ${accessModelLabel(org.max_role, 'role:operator').replace(/^role:/, '')}`));
    head.appendChild(accessModelCreateStatus(org.status || 'active'));
    if (accessModelLabel(org.status, 'active') !== 'revoked') {
      const revoke = document.createElement('button');
      revoke.type = 'button';
      revoke.className = 'acc-btn danger';
      revoke.textContent = 'Revoke trust';
      revoke.disabled = !canManage;
      revoke.addEventListener('click', async () => {
        const ok = await showDashboardConfirm({
          title: `Revoke org @${org.handle}`,
          message: 'Every grant this org materialized here is revoked immediately; new documents are refused.',
          confirmLabel: 'Revoke org',
          cancelLabel: 'Cancel',
          danger: true,
        });
        if (!ok) return;
        try {
          await accessOrgCall('api_access_org_revoke', { handle: org.handle });
          showControlToast?.('success', `Org @${org.handle} revoked`);
        } catch (err) {
          showControlToast?.('error', err?.message || 'Org revoke failed');
        }
        await refreshAccessOverviewFromApi({ silent: true }).catch(() => null);
        renderAccessAdminSummaries();
      });
      head.appendChild(revoke);
    }
    card.appendChild(head);
    list.appendChild(card);
  }
  if (!trusted.length) {
    const empty = document.createElement('div');
    empty.className = 'access-empty';
    empty.textContent = 'No organizations trusted on this daemon.';
    list.appendChild(empty);
  }
  mount.appendChild(list);

  // Trust form.
  const form = document.createElement('div');
  form.className = 'acc-grant-flow';
  form.style.marginTop = '12px';
  const head = document.createElement('div');
  head.className = 'acc-grant-flow-head';
  const title = document.createElement('div');
  title.className = 'acc-grant-flow-title';
  title.textContent = 'Trust an organization';
  const sub = document.createElement('div');
  sub.className = 'acc-grant-flow-sub';
  sub.textContent = 'Its signed documents can then grant roles here, up to the cap you set.';
  head.append(title, sub);
  form.appendChild(head);
  const grid = document.createElement('div');
  grid.className = 'acc-field-grid';
  const fields = [
    ['access-org-trust-handle', 'Org handle', 'e.g. acme'],
    ['access-org-trust-key', 'Org root key', 'ed25519 public key (base64url)'],
  ];
  for (const [id, labelText, placeholder] of fields) {
    const label = document.createElement('label');
    label.textContent = labelText;
    const input = document.createElement('input');
    input.type = 'text';
    input.id = id;
    input.autocomplete = 'off';
    input.spellcheck = false;
    input.placeholder = placeholder;
    label.appendChild(input);
    grid.appendChild(label);
  }
  const capLabel = document.createElement('label');
  capLabel.textContent = 'Role cap';
  const capSelect = document.createElement('select');
  capSelect.id = 'access-org-trust-cap';
  for (const role of accessAssignableIamRoles()) {
    const option = document.createElement('option');
    option.value = accessModelLabel(role.id);
    option.textContent = accessModelLabel(role.label, role.id);
    if (accessModelLabel(role.id) === 'role:operator') option.selected = true;
    capSelect.appendChild(option);
  }
  capLabel.appendChild(capSelect);
  grid.appendChild(capLabel);
  const peerCapLabel = document.createElement('label');
  peerCapLabel.textContent = 'Peer cap';
  peerCapLabel.title = 'Cap for org grants to member DAEMONS (peer profiles). Off by default: the human and peer lanes are separate trust decisions.';
  const peerCapSelect = document.createElement('select');
  peerCapSelect.id = 'access-org-trust-peer-cap';
  for (const [value, text] of [
    ['', 'No peer authority (default)'],
    ['presence-only', 'presence-only'],
    ['stats', 'stats'],
    ['session-reader', 'session-reader'],
    ['read-only-display', 'read-only-display'],
    ['file-reader', 'file-reader'],
    ['file-operator', 'file-operator'],
    ['terminal-operator', 'terminal-operator'],
    ['task-runner', 'task-runner'],
    ['operator', 'operator'],
  ]) {
    const option = document.createElement('option');
    option.value = value;
    option.textContent = text;
    peerCapSelect.appendChild(option);
  }
  peerCapLabel.appendChild(peerCapSelect);
  grid.appendChild(peerCapLabel);
  form.appendChild(grid);
  const actions = document.createElement('div');
  actions.className = 'acc-grant-flow-actions';
  const trustBtn = document.createElement('button');
  trustBtn.type = 'button';
  trustBtn.className = 'acc-btn primary';
  trustBtn.textContent = 'Trust org';
  trustBtn.disabled = !canManage;
  trustBtn.addEventListener('click', async () => {
    const handle = document.getElementById('access-org-trust-handle')?.value?.trim();
    const rootKey = document.getElementById('access-org-trust-key')?.value?.trim();
    if (!handle || !rootKey) {
      showControlToast?.('error', 'Handle and root key are required');
      return;
    }
    try {
      await accessOrgCall('api_access_org_trust', {
        handle,
        root_key: rootKey,
        max_role: document.getElementById('access-org-trust-cap')?.value || 'role:operator',
        max_peer_profile: document.getElementById('access-org-trust-peer-cap')?.value || '',
      });
      showControlToast?.('success', `Org @${handle} trusted`);
      // Consumed on success: clear the inputs and re-baseline this form
      // (cap selects included) so the shared render guard lets the
      // trusted-orgs list rebuild with the new entry.
      const handleInput = document.getElementById('access-org-trust-handle');
      const keyInput = document.getElementById('access-org-trust-key');
      if (handleInput) handleInput.value = '';
      if (keyInput) keyInput.value = '';
      accessGuardStamp(form);
    } catch (err) {
      showControlToast?.('error', err?.message || 'Org trust failed');
    }
    await refreshAccessOverviewFromApi({ silent: true }).catch(() => null);
    renderAccessAdminSummaries();
  });
  actions.appendChild(trustBtn);
  form.appendChild(actions);
  mount.appendChild(form);

  // Issuance card, only where an org root key lives.
  if (issuers.length) {
    const issue = document.createElement('div');
    issue.className = 'acc-grant-flow';
    issue.style.marginTop = '12px';
    const ihead = document.createElement('div');
    ihead.className = 'acc-grant-flow-head';
    const ititle = document.createElement('div');
    ititle.className = 'acc-grant-flow-title';
    ititle.textContent = 'Issue an org grant';
    const isub = document.createElement('div');
    isub.className = 'acc-grant-flow-sub';
    isub.textContent = 'Signs a document with this daemon\u2019s org root key. Send it to the member; they present it on any daemon that trusts the org.';
    ihead.append(ititle, isub);
    issue.appendChild(ihead);
    const igrid = document.createElement('div');
    igrid.className = 'acc-field-grid';
    const handleLabel = document.createElement('label');
    handleLabel.textContent = 'Org';
    const handleSelect = document.createElement('select');
    handleSelect.id = 'access-org-issue-handle';
    for (const handle of issuers) {
      const option = document.createElement('option');
      option.value = handle;
      option.textContent = `@${handle}`;
      handleSelect.appendChild(option);
    }
    handleLabel.appendChild(handleSelect);
    igrid.appendChild(handleLabel);
    const kindLabel = document.createElement('label');
    kindLabel.textContent = 'Subject';
    const kindSelect = document.createElement('select');
    kindSelect.id = 'access-org-issue-kind';
    for (const [value, text] of [
      ['client_key', 'Member browser key'],
      ['peer', 'Member daemon (peer certificate)'],
    ]) {
      const option = document.createElement('option');
      option.value = value;
      option.textContent = text;
      kindSelect.appendChild(option);
    }
    kindLabel.appendChild(kindSelect);
    igrid.appendChild(kindLabel);
    for (const [id, labelText, placeholder] of [
      ['access-org-issue-fingerprint', 'Member browser key fingerprint', 'from their Access page'],
      ['access-org-issue-label', 'Member label', 'e.g. Alice'],
    ]) {
      const label = document.createElement('label');
      label.textContent = labelText;
      const input = document.createElement('input');
      input.type = 'text';
      input.id = id;
      input.autocomplete = 'off';
      input.spellcheck = false;
      input.placeholder = placeholder;
      label.appendChild(input);
      igrid.appendChild(label);
    }
    const roleLabel = document.createElement('label');
    roleLabel.textContent = 'Role';
    const roleSelect = document.createElement('select');
    roleSelect.id = 'access-org-issue-role';
    const rebuildRoleOptions = () => {
      roleSelect.innerHTML = '';
      if (kindSelect.value === 'peer') {
        for (const profile of ['presence-only', 'stats', 'session-reader', 'read-only-display', 'file-reader', 'file-operator', 'terminal-operator', 'task-runner', 'operator']) {
          const option = document.createElement('option');
          option.value = `peer:${profile}`;
          option.textContent = `peer:${profile}`;
          if (profile === 'session-reader') option.selected = true;
          roleSelect.appendChild(option);
        }
      } else {
        for (const role of accessAssignableIamRoles()) {
          const option = document.createElement('option');
          option.value = accessModelLabel(role.id);
          option.textContent = accessModelLabel(role.label, role.id);
          if (accessModelLabel(role.id) === 'role:observer') option.selected = true;
          roleSelect.appendChild(option);
        }
      }
    };
    rebuildRoleOptions();
    kindSelect.addEventListener('change', () => {
      rebuildRoleOptions();
      const fp = document.getElementById('access-org-issue-fingerprint');
      if (fp) fp.placeholder = kindSelect.value === 'peer' ? 'peer certificate SHA-256 (64 hex chars)' : 'from their Access page';
      const fpLabel = fp?.closest('label');
      if (fpLabel) fpLabel.firstChild.textContent = kindSelect.value === 'peer' ? 'Member daemon cert fingerprint' : 'Member browser key fingerprint';
    });
    roleLabel.appendChild(roleSelect);
    igrid.appendChild(roleLabel);
    const ttlLabel = document.createElement('label');
    ttlLabel.textContent = 'Valid for';
    const ttlSelect = document.createElement('select');
    ttlSelect.id = 'access-org-issue-ttl';
    for (const [value, text] of [
      [String(7 * 24 * 60 * 60 * 1000), '7 days'],
      [String(30 * 24 * 60 * 60 * 1000), '30 days (default)'],
      [String(90 * 24 * 60 * 60 * 1000), '90 days (max)'],
    ]) {
      const option = document.createElement('option');
      option.value = value;
      option.textContent = text;
      if (text.includes('default')) option.selected = true;
      ttlSelect.appendChild(option);
    }
    ttlLabel.appendChild(ttlSelect);
    igrid.appendChild(ttlLabel);
    issue.appendChild(igrid);
    const iactions = document.createElement('div');
    iactions.className = 'acc-grant-flow-actions';
    const issueBtn = document.createElement('button');
    issueBtn.type = 'button';
    issueBtn.className = 'acc-btn primary';
    issueBtn.textContent = 'Issue & sign';
    issueBtn.disabled = !canManage;
    const output = document.createElement('textarea');
    output.className = 'daemon-pairing-textarea';
    output.rows = 5;
    output.readOnly = true;
    output.placeholder = 'Signed document appears here';
    output.style.marginTop = '10px';
    issueBtn.addEventListener('click', async () => {
      try {
        const issueKind = document.getElementById('access-org-issue-kind')?.value || 'client_key';
        const issueFp = document.getElementById('access-org-issue-fingerprint')?.value?.trim();
        const data = await accessOrgCall('api_access_org_issue', {
          handle: document.getElementById('access-org-issue-handle')?.value,
          client_key_fingerprint: issueKind === 'peer' ? '' : issueFp,
          peer_fingerprint: issueKind === 'peer' ? issueFp : '',
          label: document.getElementById('access-org-issue-label')?.value?.trim(),
          role_id: document.getElementById('access-org-issue-role')?.value,
          ttl_ms: Number(document.getElementById('access-org-issue-ttl')?.value || 0) || undefined,
        });
        output.value = JSON.stringify(data.document, null, 2);
        showControlToast?.('success', 'Org grant signed — send it to the member');
      } catch (err) {
        showControlToast?.('error', err?.message || 'Issue failed');
      }
    });
    const copyBtn = document.createElement('button');
    copyBtn.type = 'button';
    copyBtn.className = 'acc-btn';
    copyBtn.textContent = 'Copy';
    copyBtn.addEventListener('click', () => {
      if (!output.value) return;
      navigator.clipboard?.writeText(output.value)
        .then(() => showControlToast?.('success', 'Document copied'))
        .catch(() => showControlToast?.('error', 'Copy failed'));
    });
    iactions.append(issueBtn, copyBtn);
    issue.appendChild(iactions);
    issue.appendChild(output);
    mount.appendChild(issue);
  }

  // Membership revocation, only where an org root key lives: extend the
  // root-signed revocation list, then carry it to consuming daemons.
  if (issuers.length) {
    const revokeFlow = document.createElement('div');
    revokeFlow.className = 'acc-grant-flow';
    revokeFlow.style.marginTop = '12px';
    const rhead = document.createElement('div');
    rhead.className = 'acc-grant-flow-head';
    const rtitle = document.createElement('div');
    rtitle.className = 'acc-grant-flow-title';
    rtitle.textContent = 'Revoke org membership';
    const rsub = document.createElement('div');
    rsub.className = 'acc-grant-flow-sub';
    rsub.textContent = 'Adds a document grant_id and/or a member key fingerprint to the root-signed revocation list. Copy the list and apply it on every daemon that trusts the org — renewal and re-presentation are refused from then on.';
    rhead.append(rtitle, rsub);
    revokeFlow.appendChild(rhead);
    const rgrid = document.createElement('div');
    rgrid.className = 'acc-field-grid';
    const rHandleLabel = document.createElement('label');
    rHandleLabel.textContent = 'Org';
    const rHandleSelect = document.createElement('select');
    rHandleSelect.id = 'access-org-orl-handle';
    for (const handle of issuers) {
      const option = document.createElement('option');
      option.value = handle;
      option.textContent = `@${handle}`;
      rHandleSelect.appendChild(option);
    }
    rHandleLabel.appendChild(rHandleSelect);
    rgrid.appendChild(rHandleLabel);
    for (const [id, labelText, placeholder] of [
      ['access-org-orl-grant-id', 'Document grant_id', 'from the issued document (optional)'],
      ['access-org-orl-subject', 'Member key fingerprint', 'revokes every document for the key (optional)'],
      ['access-org-orl-issuer', 'Issuer key', 'revokes everything a delegated issuer signed (optional)'],
    ]) {
      const label = document.createElement('label');
      label.textContent = labelText;
      const input = document.createElement('input');
      input.type = 'text';
      input.id = id;
      input.autocomplete = 'off';
      input.spellcheck = false;
      input.placeholder = placeholder;
      label.appendChild(input);
      rgrid.appendChild(label);
    }
    revokeFlow.appendChild(rgrid);
    const ractions = document.createElement('div');
    ractions.className = 'acc-grant-flow-actions';
    const orlOutput = document.createElement('textarea');
    orlOutput.className = 'daemon-pairing-textarea';
    orlOutput.rows = 5;
    orlOutput.readOnly = true;
    orlOutput.placeholder = 'Signed revocation list appears here';
    orlOutput.style.marginTop = '10px';
    const revokeMemberBtn = document.createElement('button');
    revokeMemberBtn.type = 'button';
    revokeMemberBtn.className = 'acc-btn danger';
    revokeMemberBtn.textContent = 'Revoke & re-sign list';
    revokeMemberBtn.disabled = !canManage;
    revokeMemberBtn.addEventListener('click', async () => {
      const grantId = document.getElementById('access-org-orl-grant-id')?.value?.trim();
      const subject = document.getElementById('access-org-orl-subject')?.value?.trim();
      const issuerKey = document.getElementById('access-org-orl-issuer')?.value?.trim();
      if (!grantId && !subject && !issuerKey) {
        showControlToast?.('error', 'Provide a grant_id, a member key fingerprint, or an issuer key');
        return;
      }
      try {
        const data = await accessOrgCall('api_access_org_revoke_member', {
          handle: document.getElementById('access-org-orl-handle')?.value,
          ...(grantId ? { grant_id: grantId } : {}),
          ...(subject ? { subject } : {}),
          ...(issuerKey ? { issuer_key: issuerKey } : {}),
        });
        orlOutput.value = JSON.stringify(data.orl, null, 2);
        const local = data.applied ? ` (${data.applied.revoked_grants} local grant${data.applied.revoked_grants === 1 ? '' : 's'} revoked here)` : '';
        const published = await orgPublishRevocations(data.orl);
        showControlToast?.('success', `Revocation list now at seq ${data.orl?.seq}${local}${published ? ' — published; member browsers will carry it to their daemons' : " — apply it on the org's daemons (no rendezvous reachable to publish through)"}`);
      } catch (err) {
        showControlToast?.('error', err?.message || 'Revocation failed');
      }
      await refreshAccessOverviewFromApi({ silent: true }).catch(() => null);
      renderAccessAdminSummaries();
    });
    const copyOrlBtn = document.createElement('button');
    copyOrlBtn.type = 'button';
    copyOrlBtn.className = 'acc-btn';
    copyOrlBtn.textContent = 'Copy current list';
    copyOrlBtn.addEventListener('click', async () => {
      try {
        const handle = document.getElementById('access-org-orl-handle')?.value;
        const data = await accessOrgFetchOrl(handle);
        orlOutput.value = JSON.stringify(data.orl, null, 2);
        await navigator.clipboard?.writeText(orlOutput.value);
        showControlToast?.('success', `Revocation list seq ${data.orl?.seq} copied`);
      } catch (err) {
        showControlToast?.('error', err?.message || 'Could not fetch the revocation list');
      }
    });
    ractions.append(revokeMemberBtn, copyOrlBtn);
    revokeFlow.appendChild(ractions);
    revokeFlow.appendChild(orlOutput);
    mount.appendChild(revokeFlow);

    // Renewal, org daemon side: re-sign a still-valid document with a
    // fresh window (same grant_id, so the revocation list keeps working).
    const renewFlow = document.createElement('div');
    renewFlow.className = 'acc-grant-flow';
    renewFlow.style.marginTop = '12px';
    const nhead = document.createElement('div');
    nhead.className = 'acc-grant-flow-head';
    const ntitle = document.createElement('div');
    ntitle.className = 'acc-grant-flow-title';
    ntitle.textContent = 'Renew a member document';
    const nsub = document.createElement('div');
    nsub.className = 'acc-grant-flow-sub';
    nsub.textContent = 'Paste a still-valid document; it is re-signed with a fresh window and the same grant_id. Revoked grants and subjects are refused.';
    nhead.append(ntitle, nsub);
    renewFlow.appendChild(nhead);
    const renewInput = document.createElement('textarea');
    renewInput.className = 'daemon-pairing-textarea';
    renewInput.id = 'access-org-renew-doc';
    renewInput.rows = 5;
    renewInput.placeholder = '{"v":1,"kind":"org-grant", …}';
    renewFlow.appendChild(renewInput);
    const nactions = document.createElement('div');
    nactions.className = 'acc-grant-flow-actions';
    const renewOutput = document.createElement('textarea');
    renewOutput.className = 'daemon-pairing-textarea';
    renewOutput.rows = 5;
    renewOutput.readOnly = true;
    renewOutput.placeholder = 'Renewed document appears here';
    renewOutput.style.marginTop = '10px';
    const renewBtn = document.createElement('button');
    renewBtn.type = 'button';
    renewBtn.className = 'acc-btn primary';
    renewBtn.textContent = 'Renew & sign';
    renewBtn.addEventListener('click', async () => {
      let doc;
      try { doc = JSON.parse(renewInput.value.trim()); } catch {
        showControlToast?.('error', 'That is not valid JSON');
        return;
      }
      try {
        const data = await accessOrgCall('api_access_org_renew', doc);
        renewOutput.value = JSON.stringify(data.document, null, 2);
        showControlToast?.('success', 'Document renewed — send it back to the member');
      } catch (err) {
        showControlToast?.('error', err?.message || 'Renewal failed');
      }
    });
    const copyRenewBtn = document.createElement('button');
    copyRenewBtn.type = 'button';
    copyRenewBtn.className = 'acc-btn';
    copyRenewBtn.textContent = 'Copy';
    copyRenewBtn.addEventListener('click', () => {
      if (!renewOutput.value) return;
      navigator.clipboard?.writeText(renewOutput.value)
        .then(() => showControlToast?.('success', 'Renewed document copied'))
        .catch(() => showControlToast?.('error', 'Copy failed'));
    });
    nactions.append(renewBtn, copyRenewBtn);
    renewFlow.appendChild(nactions);
    renewFlow.appendChild(renewOutput);
    mount.appendChild(renewFlow);

    // Issuer delegation (root side): sign a certificate for a deputy
    // daemon's issuer key so day-to-day signing moves off the root.
    const delegateFlow = document.createElement('div');
    delegateFlow.className = 'acc-grant-flow';
    delegateFlow.style.marginTop = '12px';
    const dhead = document.createElement('div');
    dhead.className = 'acc-grant-flow-head';
    const dtitle = document.createElement('div');
    dtitle.className = 'acc-grant-flow-title';
    dtitle.textContent = 'Delegate an issuer key';
    const dsub = document.createElement('div');
    dsub.className = 'acc-grant-flow-sub';
    dsub.textContent = 'Paste a deputy daemon\u2019s issuer key (from its \u201cBecome an issuer\u201d flow). The certificate travels inside every document the deputy signs \u2014 nothing is published.';
    dhead.append(dtitle, dsub);
    delegateFlow.appendChild(dhead);
    const dgrid = document.createElement('div');
    dgrid.className = 'acc-field-grid';
    for (const [id, labelText, placeholder] of [
      ['access-org-delegate-key', 'Issuer key', 'ed25519 public key (base64url)'],
      ['access-org-delegate-label', 'Label', 'e.g. CI signer'],
      ['access-org-delegate-scope', 'Scope (optional)', 'role:… or peer:… — empty allows both lanes'],
    ]) {
      const label = document.createElement('label');
      label.textContent = labelText;
      const input = document.createElement('input');
      input.type = 'text';
      input.id = id;
      input.autocomplete = 'off';
      input.spellcheck = false;
      input.placeholder = placeholder;
      label.appendChild(input);
      dgrid.appendChild(label);
    }
    delegateFlow.appendChild(dgrid);
    const dactions = document.createElement('div');
    dactions.className = 'acc-grant-flow-actions';
    const delegateOutput = document.createElement('textarea');
    delegateOutput.className = 'daemon-pairing-textarea';
    delegateOutput.rows = 5;
    delegateOutput.readOnly = true;
    delegateOutput.placeholder = 'Signed issuer certificate appears here';
    delegateOutput.style.marginTop = '10px';
    const delegateBtn = document.createElement('button');
    delegateBtn.type = 'button';
    delegateBtn.className = 'acc-btn primary';
    delegateBtn.textContent = 'Delegate & sign';
    delegateBtn.disabled = !canManage;
    delegateBtn.addEventListener('click', async () => {
      try {
        const data = await accessOrgCall('api_access_org_issuer_delegate', {
          handle: document.getElementById('access-org-orl-handle')?.value || document.getElementById('access-org-issue-handle')?.value,
          issuer_key: document.getElementById('access-org-delegate-key')?.value?.trim(),
          label: document.getElementById('access-org-delegate-label')?.value?.trim(),
          max_role: document.getElementById('access-org-delegate-scope')?.value?.trim() || '',
        });
        delegateOutput.value = JSON.stringify(data.certificate, null, 2);
        showControlToast?.('success', 'Issuer certificate signed \u2014 send it to the deputy daemon');
      } catch (err) {
        showControlToast?.('error', err?.message || 'Delegation failed');
      }
    });
    const copyCertBtn = document.createElement('button');
    copyCertBtn.type = 'button';
    copyCertBtn.className = 'acc-btn';
    copyCertBtn.textContent = 'Copy';
    copyCertBtn.addEventListener('click', () => {
      if (!delegateOutput.value) return;
      navigator.clipboard?.writeText(delegateOutput.value)
        .then(() => showControlToast?.('success', 'Certificate copied'))
        .catch(() => showControlToast?.('error', 'Copy failed'));
    });
    dactions.append(delegateBtn, copyCertBtn);
    delegateFlow.appendChild(dactions);
    delegateFlow.appendChild(delegateOutput);
    mount.appendChild(delegateFlow);
  }

  // Become an issuer (deputy side): mint a local issuer key, send it to
  // the org root for delegation, then install the returned certificate.
  {
    const issuerFold = document.createElement('details');
    issuerFold.className = 'acc-fold';
    const issuerSummary = document.createElement('summary');
    issuerSummary.textContent = 'Become an issuer for an org';
    issuerFold.appendChild(issuerSummary);
    const foldBody = document.createElement('div');
    foldBody.className = 'acc-fold-body';
    const hint = document.createElement('p');
    hint.className = 'acc-fold-hint';
    hint.textContent = 'Create this daemon\u2019s issuer key for an org, send the key to the org root for a delegation certificate, and paste the certificate back here. Documents you issue then carry the certificate; the root key stays offline.';
    foldBody.appendChild(hint);
    const grid = document.createElement('div');
    grid.className = 'acc-field-grid';
    const handleLabel = document.createElement('label');
    handleLabel.textContent = 'Org handle';
    const handleInput = document.createElement('input');
    handleInput.type = 'text';
    handleInput.id = 'access-org-issuer-handle';
    handleInput.autocomplete = 'off';
    handleInput.spellcheck = false;
    handleInput.placeholder = 'e.g. acme';
    handleLabel.appendChild(handleInput);
    grid.appendChild(handleLabel);
    foldBody.appendChild(grid);
    const keyOut = document.createElement('textarea');
    keyOut.className = 'daemon-pairing-textarea';
    keyOut.rows = 2;
    keyOut.readOnly = true;
    keyOut.placeholder = 'Issuer key appears here \u2014 send it to the org root';
    foldBody.appendChild(keyOut);
    const certIn = document.createElement('textarea');
    certIn.className = 'daemon-pairing-textarea';
    certIn.rows = 4;
    certIn.placeholder = 'Paste the root-signed issuer certificate here';
    certIn.style.marginTop = '8px';
    foldBody.appendChild(certIn);
    const actions = document.createElement('div');
    actions.className = 'acc-grant-flow-actions';
    const initBtn = document.createElement('button');
    initBtn.type = 'button';
    initBtn.className = 'acc-btn primary';
    initBtn.textContent = 'Create / show issuer key';
    initBtn.addEventListener('click', async () => {
      try {
        const data = await accessOrgCall('api_access_org_issuer_init', {
          handle: handleInput.value?.trim(),
        });
        keyOut.value = data.issuer_key || '';
        showControlToast?.('success', data.certificate_installed ? 'Issuer key ready (certificate already installed)' : 'Issuer key ready \u2014 send it to the org root');
      } catch (err) {
        showControlToast?.('error', err?.message || 'Issuer init failed');
      }
    });
    const installBtn = document.createElement('button');
    installBtn.type = 'button';
    installBtn.className = 'acc-btn';
    installBtn.textContent = 'Install certificate';
    installBtn.addEventListener('click', async () => {
      let cert;
      try { cert = JSON.parse(certIn.value.trim()); } catch {
        showControlToast?.('error', 'That is not valid JSON');
        return;
      }
      try {
        await accessOrgCall('api_access_org_issuer_install', {
          handle: handleInput.value?.trim(),
          certificate: cert,
        });
        showControlToast?.('success', 'Issuer certificate installed \u2014 this daemon can now issue for the org');
        // Ceremony complete: the pasted certificate and displayed key are
        // consumed. Clearing them also releases the shared render guard so
        // the section can rebuild with the issuance flows unlocked.
        certIn.value = '';
        keyOut.value = '';
        handleInput.value = '';
        accessGuardStamp(foldBody);
        await refreshAccessOverviewFromApi({ silent: true }).catch(() => null);
        renderAccessAdminSummaries();
      } catch (err) {
        showControlToast?.('error', err?.message || 'Install failed');
      }
    });
    actions.append(initBtn, installBtn);
    foldBody.appendChild(actions);
    issuerFold.appendChild(foldBody);
    mount.appendChild(issuerFold);
  }

  // Applying a carried revocation list works on any daemon that trusts
  // the org — the signature is the authority, so the endpoint is public
  // and this flow is available to whoever carries the list here.
  if (trusted.length) {
    const applyFlow = document.createElement('div');
    applyFlow.className = 'acc-grant-flow';
    applyFlow.style.marginTop = '12px';
    const ahead = document.createElement('div');
    ahead.className = 'acc-grant-flow-head';
    const atitle = document.createElement('div');
    atitle.className = 'acc-grant-flow-title';
    atitle.textContent = 'Apply a revocation list';
    const asub = document.createElement('div');
    asub.className = 'acc-grant-flow-sub';
    asub.textContent = 'Paste the org’s current signed list. Matching materialized grants are revoked and listed entries are refused from now on; stale lists are rejected.';
    ahead.append(atitle, asub);
    applyFlow.appendChild(ahead);
    const applyInput = document.createElement('textarea');
    applyInput.className = 'daemon-pairing-textarea';
    applyInput.id = 'access-org-orl-apply-doc';
    applyInput.rows = 5;
    applyInput.placeholder = '{"v":1,"kind":"org-revocations", …}';
    applyFlow.appendChild(applyInput);
    const aactions = document.createElement('div');
    aactions.className = 'acc-grant-flow-actions';
    const applyBtn = document.createElement('button');
    applyBtn.type = 'button';
    applyBtn.className = 'acc-btn primary';
    applyBtn.textContent = 'Apply list';
    applyBtn.addEventListener('click', async () => {
      let orl;
      try { orl = JSON.parse(applyInput.value.trim()); } catch {
        showControlToast?.('error', 'That is not valid JSON');
        return;
      }
      try {
        const data = await accessOrgCall('api_access_org_orl_apply', orl);
        const applied = data.applied || {};
        showControlToast?.(
          'success',
          applied.changed
            ? `Applied @${applied.org_handle} revocations seq ${applied.seq} — ${applied.revoked_grants} grant${applied.revoked_grants === 1 ? '' : 's'} revoked`
            : `Already at seq ${applied.seq} — nothing to do`
        );
        // The pasted list is consumed; clearing it releases the shared
        // render guard so the section can rebuild.
        applyInput.value = '';
      } catch (err) {
        showControlToast?.('error', err?.message || 'Apply failed');
      }
      await refreshAccessOverviewFromApi({ silent: true }).catch(() => null);
      renderAccessAdminSummaries();
    });
    aactions.appendChild(applyBtn);
    applyFlow.appendChild(aactions);
    mount.appendChild(applyFlow);
  }
}

/* Fetch an org's current signed revocation list from the daemon holding
   its root key. daemonApi (transport F4): GET twin — tunnel first, then
   the direct-HTTP retry the verb-derived policy allows (the legacy site
   opted out, but a public idempotent read replays safely; the policy is
   data, not per-site judgment). The descriptor lifts `handle` into the
   row's {org_handle} capture. */
async function accessOrgFetchOrl(handle) {
  const resp = await daemonApi.request('api_access_org_orl', { handle });
  if (!resp.ok) throw new Error(resp.body?.error || `request failed (${resp.status})`);
  return resp.body;
}

/* Advanced: local IAM state card with raw links. */
function renderAccessIamStateCard() {
  const mount = document.getElementById('access-iam-state-card');
  if (!mount) return;
  mount.innerHTML = '';
  const overview = accessOverviewModel();
  const iam = accessIamModel(overview);
  const capabilities = iam.capabilities && typeof iam.capabilities === 'object' ? iam.capabilities : {};
  const unresolved = accessOverviewArray(overview.architecture || {}, 'unresolved');
  const ceilings = iam.role_ceilings && typeof iam.role_ceilings === 'object' ? iam.role_ceilings : {};
  const ceilingText = Object.keys(ceilings).length
    ? Object.entries(ceilings).map(([kind, role]) => `${kind} → ${String(role).replace(/^role:/, '')}`).join(' · ')
    : 'none configured';
  const lines = [
    accessHeroLine('State', accessIamLoadLabel(iam)),
    accessHeroLine('Store', accessModelLabel(iam.state_path, 'Default access store'), { mono: true }),
    accessHeroLine('Managed', `${accessModelLabel(iam.managed_principals, '0')} principals · ${accessModelLabel(iam.managed_grants, '0')} grants`),
    accessHeroLine('Write API', capabilities.write_api_available ? 'Available' : 'Not exposed'),
    accessHeroLine('Ceilings', ceilingText, {
      title: 'role_ceilings: low-provenance bindings (Connect accounts, hosted-origin browser keys) never exceed these roles per session. Edit in iam.json.',
    }),
    accessHeroLine('Enforcement', accessIamEnforcementReason(iam)),
  ];
  if (unresolved.length) {
    lines.push(accessHeroLine('Open design', unresolved.join('; ')));
  }
  mount.appendChild(accessHeroCard({
    glyph: '{}',
    glyphCls: 'daemon',
    kicker: 'Local IAM state',
    title: 'iam.json',
    lines,
    actions: dashboardConnectModeEnabled() ? [] : [
      { label: 'Raw overview JSON', onClick: () => window.open('/api/access/overview', '_blank', 'noopener') },
      { label: 'Raw IAM state JSON', onClick: () => window.open('/api/access/iam/state', '_blank', 'noopener') },
    ],
  }));
}

/* ui-v2 only (design-overhaul P2): the design's in-page header above the
   Access subtabs. Injected at render time under the flag so the v1 DOM
   stays byte-identical; idempotent, so ticks cost one getElementById. */
function ui2AccessEnsurePageChrome() {
  if (document.getElementById('ui2-access-pagehead')) return;
  const subtabs = document.getElementById('access-subtabs');
  if (!subtabs || !subtabs.parentElement) return;
  const head = document.createElement('div');
  head.id = 'ui2-access-pagehead';
  head.className = 'ui2-acc-pagehead';
  const title = document.createElement('h1');
  title.className = 'ui2-acc-title';
  title.textContent = 'Access';
  const sub = document.createElement('div');
  sub.className = 'ui2-acc-sub';
  sub.textContent = 'Who can reach this daemon and its fleet — and on whose authority. Every daemon decides for itself.';
  head.append(title, sub);
  subtabs.parentElement.insertBefore(head, subtabs);
}

function renderAccessAdminSummaries() {
  if (!document.getElementById('tab-access')) return;
  ui2AccessEnsurePageChrome();
  // The vault + custody sections live on their own #vault pane
  // (re-parented at boot): give them the same tick-driven cadence there
  // that this fanout provides for the Access pane (lease countdowns,
  // custody freshness).
  renderOrDefer('vault', 'vault-section', renderAccessVaultSection);
  // Transport ticks call this 17-renderer fanout constantly; skip the DOM
  // work while the Access pane is hidden and run once on the next entry.
  if (!paneIsVisible('access')) {
    renderOrDefer('access', 'admin-summaries', renderAccessAdminSummaries);
    return;
  }
  // Every section rebuilds through the shared guard (42-usage-terminal.js
  // accessGuardedRender): a rebuild is skipped while its mount holds the
  // user's work (focus, edited fields, an output textarea carrying a
  // freshly signed document), and skipped outright when the section's
  // state fingerprint matches the last rebuild — the enrollments
  // refresher's change-gate, applied per section. Shared fp inputs are
  // computed once per fanout run; each section folds in what it reads.
  const overview = accessOverviewModel();
  const overviewFp = accessSectionFp(overview);
  const status = dashboardTransport?.status
    ? dashboardTransport.status()
    : { enabled: dashboardControlTransportEnabled(), connected: false };
  const summary = dashboardTransportStatusSummary(status);
  const transportFp = accessSectionFp([
    summary.kind, summary.label, String(status.lastError || ''),
    Boolean(status.reconnecting), String(status.reconnectReason || ''),
    Boolean(status.eventsActive), dashboardControlLastError, dashboardControlLastErrorKind,
  ]);
  const daemonsFp = accessSectionFp(daemons.map(d =>
    [d.host_id, d.label, d.connected !== false, d.profile || '', d.url || '']));
  const peersFp = accessSectionFp(Array.from(
    peerDashboardControlConnectionsByHost,
    ([host, conn]) => [host, Boolean(conn?.canUseRpc?.())]
  ));
  const targetsFp = accessSectionFp([
    dashboardAccessTargets,
    Array.from(accessFleetProvenance),
    accessFleetHostedSyncFailing,
  ]);
  const principalFp = accessSectionFp([
    dashboardControlTransport?.lastStatus?.access_principal || null,
    clientIdentityCache?.fingerprint || '',
    accessCurrentRouteInfo().kind,
    selfPeerId || '', selfHostLabel || '',
  ]);
  const avail = method => daemonApi.availability(method).ok;
  // Relative-time texts ("expires in 2h") go stale without a model change;
  // a minute bucket re-renders them at a sane cadence.
  const minuteFp = String(Math.floor(Date.now() / 60_000));

  accessGuardedRender('access-attention',
    accessSectionFp([transportFp, overviewFp, accessPendingEnrollments.length, accessOwnDeviceFingerprint]),
    renderAccessAttention);
  accessGuardedRender('access-identity-hero',
    accessSectionFp([overviewFp, principalFp, targetsFp, daemonsFp]),
    renderAccessIdentityHero);
  accessGuardedRender('access-tier-card',
    accessSectionFp([overviewFp, avail('api_access_set_tier'), avail('api_access_set_hosted_ceiling')]),
    renderAccessTierCard);
  accessGuardedRender('access-connect-card',
    accessSectionFp([
      accessConnectStatus, accessConnectRevealed,
      accessConnectRevealed ? minuteFp : '',
      avail('api_access_connect_config'), avail('api_access_connect_claim_code'),
      avail('api_access_connect_unclaim'), avail('api_fleet_cert_request'),
    ]),
    renderAccessConnectCard);
  accessGuardedRender('access-fleet-strip',
    accessSectionFp([targetsFp, daemonsFp, transportFp, peersFp]),
    renderAccessFleetStrip);
  renderAccessExplainer();
  accessGuardedRender('access-target-overview',
    accessSectionFp([overviewFp, targetsFp, daemonsFp, transportFp, peersFp]),
    renderAccessTargetsSurface);
  accessGuardedRender('access-people-current',
    accessSectionFp([overviewFp, principalFp]),
    renderAccessPeopleCurrent);
  accessGuardedRender('access-enrollment-requests',
    accessSectionFp([accessPendingEnrollments, overviewFp, avail('api_access_enrollment_decide')]),
    renderAccessEnrollmentRequests);
  // The grant form keeps its bespoke guard (submit-cycle bypass) and no
  // fingerprint — its render also refreshes the ceiling note in place.
  renderAccessUserClientGrantForm();
  accessGuardedRender('access-people-grants',
    accessSectionFp([overviewFp, avail('api_access_iam_update_grant'),
      Array.from(accessGrantLifecycleSubmitting), minuteFp]),
    renderAccessPeopleGrants);
  accessGuardedRender('access-peer-summary',
    accessSectionFp([overviewFp]),
    renderAccessPeerSummary);
  accessGuardedRender('access-peer-details',
    accessSectionFp([overviewFp, daemonsFp, transportFp]),
    () => accessRenderDetailCards('access-peer-details', accessPeerTrustDetailCards()));
  accessGuardedRender('access-diagnostics-overview',
    accessSectionFp([transportFp, daemonsFp]),
    () => accessRenderCards('access-diagnostics-overview', accessDiagnosticsCards()));
  // Time-driven (lease countdowns) and self-guarded: renders every tick.
  renderAccessVaultSection();
  accessGuardedRender('access-role-catalog',
    accessSectionFp([overviewFp]),
    renderAccessRoleCatalog);
  accessGuardedRender('access-organizations',
    accessSectionFp([overviewFp, avail('api_access_org_trust')]),
    renderAccessOrganizations);
  accessGuardedRender('access-permission-matrix',
    accessSectionFp([overviewFp]),
    renderAccessPermissionMatrix);
  accessGuardedRender('access-grant-details',
    accessSectionFp([overviewFp]),
    () => accessRenderDetailCards('access-grant-details', accessAuditGrantCards()));
  accessGuardedRender('access-iam-state-card',
    accessSectionFp([overviewFp]),
    renderAccessIamStateCard);
  accessGuardedRender('access-model-details',
    accessSectionFp([overviewFp]),
    renderAccessModelDetails);
}

function setShellHostStatus(text = '', kind = '') {
  const el = document.getElementById('shell-host-status');
  if (!el) return;
  el.textContent = text || '';
  el.className = 'shell-host-status' + (kind ? ` ${kind}` : '');
  renderDashboardTargetSummary('shell-target-summary', currentShellHostId(), 'shell');
}

function resetShellConnectionStateForHostChange() {
  shellOpenSent = false;
  shellOpenAcked = false;
  shellQueuedInput = '';
  shellWaitingNoticeShown = false;
  shellPendingResize = null;
  shellOutputQueue = [];
  shellOutputQueuedBytes = 0;
}

function refreshShellHostOptions() {
  const select = document.getElementById('shell-host-select');
  if (!select) return;
  const previous = currentShellHostId();
  const options = [{ id: SHELL_HOST_ID, label: 'This daemon', connected: true }];
  for (const peer of daemons) {
    options.push({
      id: peer.host_id,
      label: peer.label || peer.host_id,
      connected: peer.connected !== false,
    });
  }
  select.innerHTML = '';
  for (const option of options) {
    const el = document.createElement('option');
    el.value = option.id;
    el.textContent = option.connected ? option.label : `${option.label} (offline)`;
    select.appendChild(el);
  }
  const stillPresent = options.some(option => option.id === previous);
  selectedShellHostId = stillPresent ? previous : SHELL_HOST_ID;
  select.value = selectedShellHostId;
  setShellHostStatus(selectedShellHostId === SHELL_HOST_ID ? '' : `Target: ${shellHostLabel(selectedShellHostId)}`, '');
  renderDashboardTargetSummary('shell-target-summary', currentShellHostId(), 'shell');
}

function setShellHost(hostId) {
  const next = String(hostId || SHELL_HOST_ID).trim() || SHELL_HOST_ID;
  if (next === currentShellHostId()) return;
  if (currentShellHostId() !== SHELL_HOST_ID) {
    const existing = peerDashboardControlConnectionsByHost.get(currentShellHostId());
    existing?.terminalFrame?.({
      t: 'terminal_close',
      host_id: currentShellHostId(),
      terminal_id: SHELL_TERMINAL_ID,
    });
  } else if (shellOpenSent) {
    sendShellMessage({ t: 'terminal_close', host_id: SHELL_HOST_ID, terminal_id: SHELL_TERMINAL_ID });
  }
  selectedShellHostId = next;
  resetShellConnectionStateForHostChange();
  if (shellTerm) {
    shellTerm.reset();
    shellTerm.write(`\x1b[90m[Shell target: ${shellHostLabel(next)}]\x1b[0m\r\n`);
  }
  setShellHostStatus(next === SHELL_HOST_ID ? '' : `Target: ${shellHostLabel(next)}`, '');
  renderDashboardTargetSummary('shell-target-summary', next, 'shell');
  if (activeTab === 'terminal' && activeTermSubtab === 'shell') {
    openShellSessionIfPossible(true);
  }
}


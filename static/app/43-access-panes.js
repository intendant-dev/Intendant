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
};

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

/* Overview: actionable warnings first — an empty div when all is well. */
function renderAccessAttention() {
  const mount = document.getElementById('access-attention');
  if (!mount) return;
  mount.innerHTML = '';
  const items = [];
  const status = dashboardTransport?.status
    ? dashboardTransport.status()
    : { enabled: dashboardControlTransportEnabled(), connected: false };
  const summary = dashboardTransportStatusSummary(status);
  const lastError = String(status.lastError || dashboardControlLastError || '').trim();
  if (summary.kind === 'err') {
    if (dashboardConnectModeEnabled()) {
      items.push({
        kind: 'err',
        icon: '!',
        message: 'Hosted Connect reached this daemon, but the daemon refused dashboard control.',
        detail: lastError,
        steps: [
          'Open the daemon directly (its https://host:8765 address) with root access.',
          'Go to Access → People & Devices and grant your Connect account a role.',
          'Reload this page — Connect is only the route; the daemon decides.',
        ],
        action: { label: 'Diagnostics', onClick: () => routeTo('access', 'diagnostics') },
      });
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
    const roleLabel = target.effective_role_label || descriptor.accessRole || '';
    if (roleLabel) {
      const roleId = /peer/i.test(roleLabel) ? 'role:peer-profile' : (/root/i.test(roleLabel) ? 'role:root' : '');
      meta.appendChild(accessRoleBadge(roleId, roleLabel));
    }
    const provenanceChip = accessTargetProvenanceChip(target);
    if (provenanceChip) meta.appendChild(provenanceChip);
    card.append(top, meta);
    card.addEventListener('click', () => routeTo('access', 'daemons'));
    mount.appendChild(card);
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

/* People & Devices: devices knocking on this daemon, awaiting a decision.
   Empty (and invisible) when nothing is pending. */
function renderAccessEnrollmentRequests() {
  const mount = document.getElementById('access-enrollment-requests');
  if (!mount) return;
  mount.innerHTML = '';
  if (!accessPendingEnrollments.length) return;
  const canManage = dashboardControlTransport?.lastStatus?.api_access_enrollment_decide_available !== false;

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
    if (request.origin) {
      headRow.appendChild(accessRouteChip('connect', 'via hosted route', `The offer arrived through ${request.origin}; the key will be recorded with that origin, so the role ceiling applies until it is re-enrolled from an anchor origin.`));
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
  const canManage = dashboardControlTransport?.lastStatus?.api_access_iam_update_grant_available !== false;
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
async function accessOrgCall(method, path, payload) {
  const resp = await dashboardJsonFetch(method, payload, () => authedFetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  }), method, { fallbackAfterRpcFailure: false });
  const data = await resp.json().catch(() => ({}));
  if (!resp.ok) throw new Error(data?.error || `request failed (${resp.status})`);
  return data;
}

function renderAccessOrganizations() {
  const mount = document.getElementById('access-organizations');
  if (!mount) return;
  mount.innerHTML = '';
  const iam = accessIamModel(accessOverviewModel());
  const trusted = Array.isArray(iam.trusted_orgs) ? iam.trusted_orgs : [];
  const issuers = Array.isArray(iam.org_issuers) ? iam.org_issuers : [];
  const canManage = dashboardControlTransport?.lastStatus?.api_access_org_trust_available !== false;

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
          await accessOrgCall('api_access_org_revoke', '/api/access/orgs/revoke', { handle: org.handle });
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
      await accessOrgCall('api_access_org_trust', '/api/access/orgs/trust', {
        handle,
        root_key: rootKey,
        max_role: document.getElementById('access-org-trust-cap')?.value || 'role:operator',
        max_peer_profile: document.getElementById('access-org-trust-peer-cap')?.value || '',
      });
      showControlToast?.('success', `Org @${handle} trusted`);
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
        const data = await accessOrgCall('api_access_org_issue', '/api/access/org-grants/issue', {
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
        const data = await accessOrgCall('api_access_org_revoke_member', '/api/access/org-grants/revoke-member', {
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
        const data = await accessOrgCall('api_access_org_renew', '/api/access/org-grants/renew', doc);
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
        const data = await accessOrgCall('api_access_org_issuer_delegate', '/api/access/org-grants/issuers/delegate', {
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
        const data = await accessOrgCall('api_access_org_issuer_init', '/api/access/org-grants/issuers/init', {
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
        await accessOrgCall('api_access_org_issuer_install', '/api/access/org-grants/issuers/install', {
          handle: handleInput.value?.trim(),
          certificate: cert,
        });
        showControlToast?.('success', 'Issuer certificate installed \u2014 this daemon can now issue for the org');
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
        const data = await accessOrgCall('api_access_org_orl_apply', '/api/access/orgs/revocations/apply', orl);
        const applied = data.applied || {};
        showControlToast?.(
          'success',
          applied.changed
            ? `Applied @${applied.org_handle} revocations seq ${applied.seq} — ${applied.revoked_grants} grant${applied.revoked_grants === 1 ? '' : 's'} revoked`
            : `Already at seq ${applied.seq} — nothing to do`
        );
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
   its root key (RPC in tunnel mode, plain GET otherwise). */
async function accessOrgFetchOrl(handle) {
  const resp = await dashboardJsonFetch(
    'api_access_org_orl',
    { handle },
    () => authedFetch(`/api/access/orgs/${encodeURIComponent(handle || '')}/revocations`),
    'api_access_org_orl',
    { fallbackAfterRpcFailure: false }
  );
  const data = await resp.json().catch(() => ({}));
  if (!resp.ok) throw new Error(data?.error || `request failed (${resp.status})`);
  return data;
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

function renderAccessAdminSummaries() {
  if (!document.getElementById('tab-access')) return;
  // Transport ticks call this 17-renderer fanout constantly; skip the DOM
  // work while the Access pane is hidden and run once on the next entry.
  if (!paneIsVisible('access')) {
    renderOrDefer('access', 'admin-summaries', renderAccessAdminSummaries);
    return;
  }
  renderAccessAttention();
  renderAccessIdentityHero();
  renderAccessFleetStrip();
  renderAccessExplainer();
  renderAccessTargetsSurface();
  renderAccessPeopleCurrent();
  renderAccessEnrollmentRequests();
  renderAccessUserClientGrantForm();
  renderAccessPeopleGrants();
  renderAccessPeerSummary();
  accessRenderDetailCards('access-peer-details', accessPeerTrustDetailCards());
  accessRenderCards('access-diagnostics-overview', accessDiagnosticsCards());
  renderAccessVaultSection();
  renderAccessRoleCatalog();
  renderAccessOrganizations();
  renderAccessPermissionMatrix();
  accessRenderDetailCards('access-grant-details', accessAuditGrantCards());
  renderAccessIamStateCard();
  renderAccessModelDetails();
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


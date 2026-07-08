// ── ui-v2 Settings: 4-section layout + autonomy & approvals ────────────
// Design-overhaul P2. Under the ui-v2 flag the Settings tab becomes the
// design's four sections (Autonomy & approvals / Providers & models /
// Presence & voice / Account & advanced) with a left section nav. The
// EXISTING v1 cards and inputs are RE-PARENTED into the new panes — ids,
// listeners, and the /api/settings save path survive untouched; deep
// catalog rows (binaries, CU backend, transcription granularity, …) fold
// into per-card "Advanced" disclosures. v1 without the flag is
// byte-identical (nothing here runs).
//
// Corrections applied vs the design prototype (binding, from the
// program ledger + crates/intendant-core/src/autonomy.rs):
//   · the autonomy dial has FOUR segments — Low/Medium/High/Full. There
//     is no "Off" level in the backend (autonomy.rs AutonomyLevel).
//   · per-level copy states the real needs_approval() semantics — real
//     High auto-approves Ask-ruled network AND destructive actions.
//   · approval rules are the REAL 8 backend categories (6 curated +
//     "More gates" fold), wired to the Control pane's existing
//     `set_approval_rule` machinery (shared controlApprovalRules state,
//     same dispatchControlMsg sender — no new wire formats).
//   · no fake controls: no offline-window slider (that knob is the
//     Vault fueling flow's, per-grant), no "Spend & credentials"
//     category (no backend ActionCategory), and the main provider/model
//     rows are read-only mirrors with an env-var note (selection is
//     PROVIDER/MODEL_NAME at launch; there is no persisted setting).

const UI2_SET_SECTIONS = [
  { id: 'autonomy', label: 'Autonomy & approvals' },
  { id: 'providers', label: 'Providers & models' },
  { id: 'presence', label: 'Presence & voice' },
  { id: 'advanced', label: 'Account & advanced' },
];

// Truthful per-level descriptions, from autonomy.rs::needs_approval():
// hard gates (HumanInput, LiveAudioSpawn) always ask; Full returns false
// for everything else (bypasses rules, even Deny, and external requests
// are auto-approved past Deny); DisplayControl follows the session grant;
// Low asks for everything except FileRead; Medium gates Ask-ruled
// writes/deletes/destructive/network; High gates nothing Ask-ruled.
const UI2_AUTONOMY_LEVELS = [
  {
    level: 'Low',
    desc: 'Everything waits for your approval except file reads. Deny rules still refuse.',
  },
  {
    level: 'Medium',
    desc: 'The default. Reads and shell commands run on their own; writes, deletes, network egress, and destructive actions wait when their rule below says Ask.',
  },
  {
    level: 'High',
    desc: 'Auto-approves whatever the rules leave on Ask — including network egress and destructive actions. Only Deny rules and the always-on gates still stop it.',
  },
  {
    level: 'Full',
    desc: 'Runs everything without asking and bypasses the rules below — external-agent requests are auto-approved even past Deny. Only human-input and live-audio requests still reach you.',
  },
];

// The 8 real backend categories (CONTROL_APPROVAL_CATEGORIES order):
// 6 curated rows + 2 in the "More gates" fold.
const UI2_APPROVAL_ROWS = [
  { cat: 'file_read', label: 'Read files', sub: 'open and read files and project state' },
  { cat: 'file_write', label: 'Edit & write files', sub: 'create, modify, and rename files' },
  { cat: 'file_delete', label: 'Delete files', sub: 'remove files and directories' },
  { cat: 'command_exec', label: 'Run shell commands', sub: 'anything that executes on the machine' },
  { cat: 'network', label: 'Network egress', sub: 'outbound HTTP, package installs, fetches' },
  { cat: 'destructive', label: 'Destructive actions', sub: 'rm -rf, force-push, resets, drops' },
];
const UI2_APPROVAL_FOLD_ROWS = [
  {
    cat: 'display_control', label: 'Control your display',
    sub: 'screenshots and input on your own session display — asks once per session until granted',
  },
  {
    cat: 'tool_call', label: 'External-agent tool calls',
    sub: 'a managed backend calling Intendant’s MCP / computer-use tools — Auto allows these without Full autonomy',
  },
];
const UI2_APPROVAL_RULES = ['auto', 'ask', 'deny'];

// `var`, deliberately: the ui2Settings* hooks are function declarations
// (hoisted module-wide) and 48-router-settings.js calls them during its
// own evaluation on #settings deep links — before this fragment has
// evaluated. A `let` here would still be in its temporal dead zone at
// that moment and the read would throw, aborting the module. `var`
// hoists as `undefined`, so the built-guard reads falsy instead.
var ui2SettingsBuilt = false;

function ui2SettingsEl(tag, className, text) {
  const el = document.createElement(tag);
  if (className) el.className = className;
  if (text !== undefined) el.textContent = text;
  return el;
}

// Wrap a list of existing .settings-row nodes (found by their input ids)
// into a <details> "Advanced" fold appended to `card`. Rows keep their
// ids/listeners — this is a pure re-parent.
function ui2SettingsFoldRows(card, summaryText, inputIds) {
  const rows = inputIds
    .map((id) => document.getElementById(id)?.closest('.settings-row'))
    .filter(Boolean);
  if (!rows.length) return null;
  const details = ui2SettingsEl('details', 'ui2-fold');
  const summary = ui2SettingsEl('summary', 'ui2-fold-summary');
  summary.innerHTML = `<span class="ui2-fold-chev">${ui2Icon('right', 14)}</span><span>${summaryText}</span>`;
  const body = ui2SettingsEl('div', 'ui2-fold-body');
  for (const row of rows) body.appendChild(row);
  details.append(summary, body);
  card.appendChild(details);
  return details;
}

// Build an accent segmented control that PROXIES an existing <select>
// (the select stays the state carrier for the save path; its row is
// hidden by CSS via the ui2-seg-carrier class).
function ui2SettingsSegProxy(selectId, choices, ariaLabel) {
  const sel = document.getElementById(selectId);
  if (!sel) return null;
  sel.closest('.settings-row')?.classList.add('ui2-seg-carrier');
  const seg = ui2SettingsEl('div', 'ui2-seg ui2-seg-wide');
  seg.setAttribute('role', 'group');
  seg.setAttribute('aria-label', ariaLabel);
  const sync = () => {
    seg.querySelectorAll('button').forEach((b) => {
      const on = b.dataset.value === sel.value;
      b.classList.toggle('is-accent', on);
      b.setAttribute('aria-pressed', on ? 'true' : 'false');
    });
  };
  for (const choice of choices) {
    const btn = ui2SettingsEl('button', 'ui2-seg-btn', choice.label);
    btn.type = 'button';
    btn.dataset.value = choice.value;
    if (choice.note) btn.title = choice.note;
    btn.addEventListener('click', () => {
      if (sel.value === choice.value) return;
      sel.value = choice.value;
      sel.dispatchEvent(new Event('change', { bubbles: true }));
      sync();
    });
    seg.appendChild(btn);
  }
  sel.addEventListener('change', sync);
  seg.dataset.proxyFor = selectId;
  sync();
  return seg;
}

function ui2SettingsApprovalRow(spec) {
  const row = ui2SettingsEl('div', 'ui2-rule-row');
  row.dataset.category = spec.cat;
  const meta = ui2SettingsEl('div', 'ui2-rule-meta');
  meta.append(
    ui2SettingsEl('div', 'ui2-rule-label', spec.label),
    ui2SettingsEl('div', 'ui2-rule-sub', spec.sub),
  );
  const seg = ui2SettingsEl('div', 'ui2-seg ui2-rule-seg');
  seg.setAttribute('role', 'group');
  seg.setAttribute('aria-label', `${spec.label} approval rule`);
  for (const rule of UI2_APPROVAL_RULES) {
    const btn = ui2SettingsEl('button', 'ui2-seg-btn', rule.charAt(0).toUpperCase() + rule.slice(1));
    btn.type = 'button';
    btn.dataset.rule = rule;
    btn.addEventListener('click', () => ui2SettingsSetRule(spec.cat, rule));
    seg.appendChild(btn);
  }
  row.append(meta, seg);
  return row;
}

// Change an approval rule from the v2 card. Mirrors the Control pane's
// onControlApprovalRuleChange exactly: dedup against the shared
// controlApprovalRules state, dispatch the shipped set_approval_rule
// ControlMsg, and keep the v1 Control-pane <select> in step so the two
// surfaces can never drift.
function ui2SettingsSetRule(category, rule) {
  if (controlApprovalRules[category] === rule) return;
  controlApprovalRules[category] = rule;
  const sel = document.getElementById('control-approval-' + category);
  if (sel) sel.value = rule;
  dispatchControlMsg({ action: 'set_approval_rule', category, rule });
  ui2SettingsRenderRules();
}

// Change the autonomy level from the v2 dial. Same path as the v1
// statusline chip's cycleAutonomy: updateStatusBar mutates #sb-autonomy
// (which every mirror, including this dial, observes) and the level is
// dispatched via the shipped set_autonomy ControlMsg.
function ui2SettingsSetAutonomy(level) {
  const current = (document.getElementById('sb-autonomy')?.textContent || '').trim();
  if (current === level) return;
  updateStatusBar({ autonomy: level });
  dispatchControlMsg({ action: 'set_autonomy', level: level.toLowerCase() });
}

function ui2SettingsRenderAutonomy() {
  if (!ui2SettingsBuilt) return;
  const level = (document.getElementById('sb-autonomy')?.textContent || '').trim();
  const spec = UI2_AUTONOMY_LEVELS.find((l) => l.level === level) || null;
  const pill = document.getElementById('ui2-set-auto-pill');
  if (pill) {
    pill.textContent = spec ? level : (level || '—');
    pill.dataset.level = (spec ? level : '').toLowerCase();
  }
  document.querySelectorAll('#ui2-set-auto-dial .ui2-seg-btn').forEach((b) => {
    const on = b.dataset.level === level;
    b.classList.toggle('is-accent', on);
    b.setAttribute('aria-pressed', on ? 'true' : 'false');
  });
  const desc = document.getElementById('ui2-set-auto-desc');
  if (desc) {
    desc.textContent = spec
      ? spec.desc
      : 'Autonomy level unknown — waiting for the daemon.';
  }
}

function ui2SettingsRenderRules() {
  if (!ui2SettingsBuilt) return;
  document.querySelectorAll('#ui2-set-rules .ui2-rule-row').forEach((row) => {
    const current = controlApprovalRules[row.dataset.category];
    row.querySelectorAll('.ui2-seg-btn').forEach((b) => {
      const on = !!current && b.dataset.rule === current;
      b.classList.toggle('is-auto', on && current === 'auto');
      b.classList.toggle('is-ask', on && current === 'ask');
      b.classList.toggle('is-deny', on && current === 'deny');
      b.setAttribute('aria-pressed', on ? 'true' : 'false');
    });
  });
}

// Hook target: called at the end of renderControlPane() (fragment 37) so
// a rules refresh from ANY surface re-paints this card too.
function ui2SettingsSyncFromControl() {
  if (typeof ui2Enabled !== 'function' || !ui2Enabled()) return;
  ui2SettingsRenderRules();
}

// Hook target: called at the end of loadSettings() (fragment 53) — the
// programmatic .value assignments there fire no change events, so the
// segmented proxies re-sync here.
function ui2SettingsSyncMirrors() {
  if (!ui2SettingsBuilt) return;
  document.querySelectorAll('#ui2-set-shell .ui2-seg[data-proxy-for]').forEach((seg) => {
    const sel = document.getElementById(seg.dataset.proxyFor);
    if (!sel) return;
    seg.querySelectorAll('button').forEach((b) => {
      const on = b.dataset.value === sel.value;
      b.classList.toggle('is-accent', on);
      b.setAttribute('aria-pressed', on ? 'true' : 'false');
    });
  });
}

// Hook target: called from switchSettingsSubtab (fragment 48) after the
// pane toggle so section-shown work runs on real switches.
function ui2SettingsOnSubtabShown(name) {
  if (typeof ui2Enabled !== 'function' || !ui2Enabled() || !ui2SettingsBuilt) return;
  if (name === 'autonomy') {
    // Re-pull the live rules (shared refreshControlPane path) each visit.
    refreshControlPane();
  }
}

function ui2SettingsApplyActive() {
  const name = activeSettingsSubtab;
  document.querySelectorAll('#tab-settings .subtab-btn').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.settingsTab === name);
  });
  document.querySelectorAll('#tab-settings .subtab-pane').forEach((pane) => {
    pane.classList.toggle('active', pane.id === `settings-pane-${name}`);
  });
  updateSettingsSaveRow();
}

function ui2SettingsBuild() {
  if (ui2SettingsBuilt) return;
  if (typeof ui2Enabled !== 'function' || !ui2Enabled()) return;
  const tab = document.getElementById('tab-settings');
  if (!tab) return;

  // ── shell: side nav + content column ──
  const shell = ui2SettingsEl('div', 'ui2-set-shell');
  shell.id = 'ui2-set-shell';
  const side = ui2SettingsEl('aside', 'ui2-set-side');
  side.appendChild(ui2SettingsEl('h1', 'ui2-set-title', 'Settings'));
  const nav = ui2SettingsEl('nav', 'ui2-set-nav');
  nav.setAttribute('aria-label', 'Settings sections');
  for (const s of UI2_SET_SECTIONS) {
    // subtab-btn + data-settings-tab so the existing switch/apply loops
    // in 48-router-settings.js drive the active state for free.
    const btn = ui2SettingsEl('button', 'subtab-btn ui2-set-nav-btn', s.label);
    btn.type = 'button';
    btn.dataset.settingsTab = s.id;
    btn.addEventListener('click', () => routeTo('settings', s.id));
    nav.appendChild(btn);
  }
  side.appendChild(nav);
  const content = ui2SettingsEl('div', 'ui2-set-content');
  const panes = {};
  for (const s of UI2_SET_SECTIONS) {
    const pane = ui2SettingsEl('div', 'subtab-pane ui2-set-pane');
    pane.id = `settings-pane-${s.id}`;
    panes[s.id] = pane;
    content.appendChild(pane);
  }
  shell.append(side, content);
  tab.insertBefore(shell, tab.firstChild);

  const cardOf = (headingId) =>
    document.getElementById(headingId)?.closest('section.ui-card') || null;

  // ── Autonomy & approvals (new, v2-only controls over shipped state) ──
  {
    const auto = ui2SettingsEl('section', 'ui-card ui2-auto-card');
    const head = ui2SettingsEl('div', 'ui-section-head ui2-auto-head');
    const title = ui2SettingsEl('h3', 'ui-section-title', 'Autonomy');
    const pill = ui2SettingsEl('span', 'ui2-auto-pill');
    pill.id = 'ui2-set-auto-pill';
    pill.textContent = '—';
    const hint = ui2SettingsEl('span', 'ui2-auto-hint', 'the master dial');
    head.append(title, pill, hint);
    auto.appendChild(head);
    const dial = ui2SettingsEl('div', 'ui2-seg ui2-auto-dial');
    dial.id = 'ui2-set-auto-dial';
    dial.setAttribute('role', 'group');
    dial.setAttribute('aria-label', 'Autonomy level');
    for (const l of UI2_AUTONOMY_LEVELS) {
      const btn = ui2SettingsEl('button', 'ui2-seg-btn', l.level);
      btn.type = 'button';
      btn.dataset.level = l.level;
      btn.addEventListener('click', () => ui2SettingsSetAutonomy(l.level));
      dial.appendChild(btn);
    }
    auto.appendChild(dial);
    const desc = ui2SettingsEl('p', 'ui2-auto-desc');
    desc.id = 'ui2-set-auto-desc';
    auto.appendChild(desc);
    auto.appendChild(ui2SettingsEl(
      'p', 'ui2-auto-gates',
      'Always, at every level: requests for your input and live-audio sessions ask; '
      + 'display control asks once per session until granted. The dial applies live '
      + 'but is not saved — a daemon restart returns to the --autonomy launch level.',
    ));
    panes.autonomy.appendChild(auto);

    const rules = ui2SettingsEl('section', 'ui-card ui2-rules-card');
    rules.id = 'ui2-set-rules';
    const rhead = ui2SettingsEl('div', 'ui-section-head');
    rhead.appendChild(ui2SettingsEl('h3', 'ui-section-title', 'Approval rules'));
    rhead.appendChild(ui2SettingsEl(
      'div', 'ui-section-sub',
      'Per-category gates for the internal agent — they refine the dial. Applied live and saved to intendant.toml [approval]. The same rules appear under Activity → Control.',
    ));
    rules.appendChild(rhead);
    for (const spec of UI2_APPROVAL_ROWS) rules.appendChild(ui2SettingsApprovalRow(spec));
    const fold = ui2SettingsEl('details', 'ui2-fold');
    const summary = ui2SettingsEl('summary', 'ui2-fold-summary');
    summary.innerHTML = `<span class="ui2-fold-chev">${ui2Icon('right', 14)}</span><span>More gates</span>`;
    const foldBody = ui2SettingsEl('div', 'ui2-fold-body');
    for (const spec of UI2_APPROVAL_FOLD_ROWS) foldBody.appendChild(ui2SettingsApprovalRow(spec));
    fold.append(summary, foldBody);
    rules.appendChild(fold);
    rules.appendChild(ui2SettingsEl(
      'p', 'ui2-rules-note',
      'How the dial and rules combine: Auto always runs. Deny surfaces the request and refuses it. '
      + 'Ask gates writes, deletes, network, and destructive actions at Medium; at Low everything '
      + 'except reads waits; at High, Ask behaves like Auto; at Full the rules are bypassed entirely.',
    ));
    panes.autonomy.appendChild(rules);
  }

  // ── Providers & models ──
  {
    // Read-only main provider/model (no persisted setting exists — the
    // daemon selects at launch from PROVIDER/MODEL_NAME; honest mirror).
    const ro = ui2SettingsEl('section', 'ui-card ui2-ro-card');
    const head = ui2SettingsEl('div', 'ui-section-head');
    head.appendChild(ui2SettingsEl('h3', 'ui-section-title', 'Main model'));
    head.appendChild(ui2SettingsEl(
      'div', 'ui-section-sub',
      'The provider and model driving the main agent loop on this daemon.',
    ));
    ro.appendChild(head);
    const mkRoRow = (label, valueId) => {
      const row = ui2SettingsEl('div', 'settings-row ui2-ro-row');
      row.appendChild(ui2SettingsEl('label', null, label));
      const val = ui2SettingsEl('span', 'ui2-ro-value', '—');
      val.id = valueId;
      row.appendChild(val);
      return row;
    };
    ro.appendChild(mkRoRow('Provider', 'ui2-ro-provider'));
    ro.appendChild(mkRoRow('Model', 'ui2-ro-model'));
    ro.appendChild(ui2SettingsEl(
      'p', 'settings-note ui2-ro-note',
      'Selected at daemon launch via the PROVIDER and MODEL_NAME environment variables '
      + '(or auto-detected from available keys) — read-only here.',
    ));
    panes.providers.appendChild(ro);

    // API keys card, re-parented whole (Save Keys + status ticks intact;
    // the focusSettingsApiKeys deep link scrolls to this same node).
    const keys = cardOf('settings-keys-heading');
    if (keys) panes.providers.appendChild(keys);

    // "take effect on next task" note from the old Agent pane.
    const agentNote = document.querySelector('#settings-pane-agent .settings-note');
    if (agentNote) panes.providers.appendChild(agentNote);

    // External agent → "Managed backend" with a segmented proxy over the
    // existing select + an Advanced fold for binaries and tier.
    const ext = cardOf('settings-external-agent-heading');
    if (ext) {
      panes.providers.appendChild(ext);
      const seg = ui2SettingsSegProxy('set-external-agent', [
        { value: '', label: 'Native', note: 'Intendant’s internal agent loop' },
        { value: 'codex', label: 'Codex', note: 'Supervise OpenAI Codex as the backend' },
        { value: 'claude-code', label: 'Claude Code', note: 'Supervise Claude Code as the backend' },
      ], 'Managed backend');
      if (seg) {
        const carrier = document.getElementById('set-external-agent').closest('.settings-row');
        carrier.parentNode.insertBefore(seg, carrier);
      }
      ui2SettingsFoldRows(ext, 'Binaries & service tier', [
        'set-codex-command', 'set-codex-managed-command', 'set-claude-command', 'set-codex-service-tier',
      ]);
      const link = ui2SettingsEl('p', 'settings-note ui2-crosslink');
      link.innerHTML = 'Live backend controls — sandbox, approval policy, model override, thread actions — stay in <a href="#activity/control">Activity → Control</a>.';
      ext.appendChild(link);
    }

    // Computer Use card: backend select (and the vaulted cu-first rows,
    // whose visibility loadSettings still drives) fold as advanced.
    const cu = cardOf('settings-cu-heading');
    if (cu) {
      panes.providers.appendChild(cu);
      const fold = ui2SettingsFoldRows(cu, 'Backend & routing', ['set-cu-backend']);
      const routingRows = document.getElementById('cu-routing-rows');
      if (fold && routingRows) fold.querySelector('.ui2-fold-body').prepend(routingRows);
    }
  }

  // ── Presence & voice ──
  {
    const presence = cardOf('settings-presence-heading');
    if (presence) {
      panes.presence.appendChild(presence);
      const seg = ui2SettingsSegProxy('set-presence-live-provider', [
        { value: '', label: 'Auto', note: 'Pick a live-voice provider from available keys' },
        { value: 'gemini', label: 'Gemini Live' },
        { value: 'openai', label: 'OpenAI Realtime' },
      ], 'Live voice provider');
      if (seg) {
        const segRow = ui2SettingsEl('div', 'ui2-seg-row');
        segRow.appendChild(ui2SettingsEl('div', 'ui2-seg-row-label', 'Live voice'));
        segRow.appendChild(seg);
        const carrier = document.getElementById('set-presence-live-provider').closest('.settings-row');
        carrier.parentNode.insertBefore(segRow, carrier);
      }
      ui2SettingsFoldRows(presence, 'Models & providers', [
        'set-presence-provider', 'set-presence-model', 'set-presence-live-model',
      ]);
    }
    const transcription = cardOf('settings-transcription-heading');
    if (transcription) {
      panes.presence.appendChild(transcription);
      ui2SettingsFoldRows(transcription, 'Engine & language', [
        'set-transcription-provider', 'set-transcription-model',
        'set-transcription-endpoint', 'set-transcription-language',
      ]);
    }
    const recording = cardOf('settings-recording-heading');
    if (recording) {
      panes.presence.appendChild(recording);
      ui2SettingsFoldRows(recording, 'Framerate & quality', [
        'set-recording-framerate', 'set-recording-quality',
      ]);
    }
    const liveAudio = cardOf('settings-live-audio-heading');
    if (liveAudio) {
      panes.presence.appendChild(liveAudio);
      ui2SettingsFoldRows(liveAudio, 'Timeout', ['set-live-audio-timeout']);
    }
  }

  // ── Account & advanced (the old Debug pane, re-homed) ──
  {
    const debugBody = document.querySelector('#settings-pane-debug .settings-pane-body');
    if (debugBody) {
      while (debugBody.firstChild) panes.advanced.appendChild(debugBody.firstChild);
    }
    const idNote = ui2SettingsEl('p', 'settings-note ui2-crosslink');
    idNote.innerHTML = 'Identity, passkeys, and who can reach this daemon live in <a href="#access/overview">Access</a>; credentials and fueling live in the Vault.';
    panes.advanced.appendChild(idNote);
  }

  // Save/Reset row moves under the content column (still matched by the
  // existing '#tab-settings .settings-save-row' selector).
  const saveRow = tab.querySelector('.settings-save-row');
  if (saveRow) content.appendChild(saveRow);

  ui2SettingsBuilt = true;

  // Mirrors: statusline is the client-side truth for level + identity.
  ui2Mirror('sb-autonomy', () => ui2SettingsRenderAutonomy());
  ui2Mirror('sb-provider', (src) => {
    const el = document.getElementById('ui2-ro-provider');
    if (el) el.textContent = (src.textContent || '').trim() || '—';
  });
  ui2Mirror('sb-model', (src) => {
    const el = document.getElementById('ui2-ro-model');
    if (el) el.textContent = (src.textContent || '').trim() || '—';
  });

  ui2SettingsRenderRules();
  ui2SettingsSyncMirrors();
  ui2SettingsApplyActive();
  // Populate approval rules early so the Autonomy section is live on
  // first open (shared Control-pane fetch; polls until transport is up).
  refreshControlPane();
}

if (typeof ui2Enabled === 'function' && ui2Enabled()) {
  ui2SettingsBuild();
}

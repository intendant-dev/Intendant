// ── New Session launch ──
// The Sessions tab New Session form: agent picker + launch-config
// machinery, unfueled/projectless preflights and banners, spawn notices,
// and startNewSession itself. The fs pickers the form opens (project
// directory, agent binary) stay in 55-files-ide.js — they serve the
// transfers flows too.

function selectedNewSessionCodexCatalogEntry() {
  const selection = document.getElementById('new-session-codex-model-select')?.value || '';
  if (selection === '__custom__') return null;
  const model = selection || newSessionCodexGlobalModel;
  return newSessionCodexModelCatalog
    .filter(entry => entry.id === model || model.startsWith(`${entry.id}-`))
    .sort((a, b) => b.id.length - a.id.length)[0] || null;
}

function populateNewSessionCodexReasoningEfforts() {
  const select = document.getElementById('new-session-codex-reasoning-effort');
  if (!select) return;
  const previous = select.value || '';
  const model = selectedNewSessionCodexCatalogEntry();
  const efforts = model?.reasoning_efforts?.length
    ? model.reasoning_efforts
    : newSessionCodexReasoningEfforts;
  select.replaceChildren();
  const inherit = document.createElement('option');
  inherit.value = '';
  const globalSupported = !model || !newSessionCodexGlobalReasoningEffort ||
    efforts.includes(newSessionCodexGlobalReasoningEffort);
  inherit.textContent = newSessionCodexGlobalReasoningEffort && globalSupported
    ? `Global setting (${newSessionCodexGlobalReasoningEffort})`
    : (model?.default_reasoning_effort
      ? `Model default (${model.default_reasoning_effort})`
      : 'Model / global default');
  select.appendChild(inherit);
  for (const effort of efforts) {
    const option = document.createElement('option');
    option.value = effort;
    option.textContent = effort === 'ultra'
      ? 'ultra — automatic task delegation'
      : effort;
    select.appendChild(option);
  }
  select.value = efforts.includes(previous) ? previous : '';
  if (model && !globalSupported && !select.value && efforts.includes(model.default_reasoning_effort)) {
    // A selected model must not inherit an incompatible global effort (for
    // example Luna under an Ultra global pin). Make its advertised default
    // explicit on the create-session wire instead.
    select.value = model.default_reasoning_effort;
  }
}

function populateControlCodexReasoningEfforts() {
  const select = document.getElementById('control-codex-reasoning');
  if (!select) return;
  const previous = select.value || controlCodexConfig.reasoning_effort || '';
  select.replaceChildren();
  const inherit = document.createElement('option');
  inherit.value = '';
  inherit.textContent = '(default)';
  select.appendChild(inherit);
  for (const effort of newSessionCodexReasoningEfforts) {
    const option = document.createElement('option');
    option.value = effort;
    option.textContent = effort === 'ultra'
      ? 'ultra — automatic task delegation'
      : effort;
    select.appendChild(option);
  }
  select.value = newSessionCodexReasoningEfforts.includes(previous) ? previous : '';
}

function populateNewSessionCodexModelSelect() {
  const select = document.getElementById('new-session-codex-model-select');
  if (!select) return;
  const previous = select.value || '';
  select.replaceChildren();
  const inherit = document.createElement('option');
  inherit.value = '';
  inherit.textContent = newSessionCodexGlobalModel
    ? `Global setting (${newSessionCodexGlobalModel})`
    : 'Global / Codex default';
  select.appendChild(inherit);
  for (const entry of newSessionCodexModelCatalog) {
    const option = document.createElement('option');
    option.value = entry.id;
    option.textContent = `${entry.display_name} — ${entry.id}`;
    select.appendChild(option);
  }
  const custom = document.createElement('option');
  custom.value = '__custom__';
  custom.textContent = 'Custom model id…';
  select.appendChild(custom);
  select.value = previous === '__custom__' || newSessionCodexModelCatalog.some(entry => entry.id === previous)
    ? previous
    : '';
  updateNewSessionCodexCustomModelRow();
  populateNewSessionCodexReasoningEfforts();
}

function updateNewSessionCodexCustomModelRow() {
  const select = document.getElementById('new-session-codex-model-select');
  const row = document.getElementById('new-session-codex-model-custom-row');
  const input = document.getElementById('new-session-codex-model');
  if (!select || !row) return;
  const custom = select.value === '__custom__' && !select.disabled;
  row.classList.toggle('hidden', !custom);
  if (input) input.disabled = !custom;
}

function onNewSessionCodexModelSelectChange() {
  updateNewSessionCodexCustomModelRow();
  populateNewSessionCodexReasoningEfforts();
}
window.onNewSessionCodexModelSelectChange = onNewSessionCodexModelSelectChange;
populateControlCodexReasoningEfforts();
populateNewSessionCodexModelSelect();

// The Claude model picker offers version-safe aliases (the CLI resolves
// them to the latest model); the free-text id input only appears behind
// the explicit "Custom model id…" choice.
function updateNewSessionClaudeCustomModelRow() {
  const select = document.getElementById('new-session-claude-model-select');
  const row = document.getElementById('new-session-claude-model-custom-row');
  if (!select || !row) return;
  const custom = select.value === '__custom__' && !select.disabled;
  row.classList.toggle('hidden', !custom);
}
function onNewSessionClaudeModelSelectChange() {
  updateNewSessionClaudeCustomModelRow();
}
window.onNewSessionClaudeModelSelectChange = onNewSessionClaudeModelSelectChange;

function normalizeContextArchiveMode(mode) {
  return ['summary', 'exact', 'off'].includes(mode) ? mode : 'summary';
}

function normalizeContextArchiveModeOptional(mode) {
  const v = String(mode || '').trim();
  return ['summary', 'exact', 'off'].includes(v) ? v : '';
}

function normalizeCodexSandbox(mode) {
  const v = String(mode || '').trim();
  return ['workspace-write', 'danger-full-access', 'read-only'].includes(v) ? v : 'workspace-write';
}

function normalizeCodexSandboxOptional(mode) {
  const v = String(mode || '').trim();
  return ['workspace-write', 'danger-full-access', 'read-only'].includes(v) ? v : '';
}

function normalizeCodexApprovalPolicy(policy) {
  const v = String(policy || '').trim();
  return ['on-request', 'never', 'untrusted'].includes(v) ? v : 'on-request';
}

function normalizeCodexApprovalPolicyOptional(policy) {
  const v = String(policy || '').trim();
  return ['on-request', 'never', 'untrusted'].includes(v) ? v : '';
}

function normalizeCodexServiceTier(tier) {
  const v = String(tier || '').trim().toLowerCase();
  if (!v || v === 'inherit' || v === 'default' || v === 'auto' || v === 'codex') return '';
  if (v === 'fast' || v === 'priority') return 'priority';
  if (['standard', 'normal', 'none', 'off', 'clear', 'disabled', 'false', '0'].includes(v)) return 'standard';
  if (v === 'flex') return 'flex';
  return v;
}

function codexServiceTierIsFast(tier) {
  return normalizeCodexServiceTier(tier) === 'priority';
}

function resetNewSessionCodexFastModeToDefault() {
  newSessionCodexFastModeTouched = false;
  newSessionCodexFastMode = codexServiceTierIsFast(newSessionCodexDefaultServiceTier);
}

function setNewSessionAgentDefaults(settings) {
  newSessionAgentCommands = {
    codex: settings.codex_command || 'codex',
    'claude-code': settings.claude_command || 'claude',
  };
  newSessionCodexManagedContext =
    settings.codex_managed_context === 'managed' ? 'managed' : 'vanilla';
  newSessionCodexContextArchive = normalizeContextArchiveMode(settings.codex_context_archive || 'summary');
  newSessionCodexSandbox = normalizeCodexSandbox(settings.codex_sandbox || 'workspace-write');
  newSessionCodexApprovalPolicy = normalizeCodexApprovalPolicy(settings.codex_approval_policy || 'on-request');
  newSessionCodexDefaultServiceTier = normalizeCodexServiceTier(settings.codex_service_tier || '');
  newSessionCodexGlobalModel = String(settings.codex_model || '').trim();
  newSessionCodexGlobalReasoningEffort = String(settings.codex_reasoning_effort || '').trim();
  const configuredCatalog = Array.isArray(settings.codex_models)
    ? settings.codex_models
      .map(entry => ({
        id: String(entry?.id || '').trim(),
        display_name: String(entry?.display_name || entry?.id || '').trim(),
        default_reasoning_effort: String(entry?.default_reasoning_effort || '').trim(),
        reasoning_efforts: Array.isArray(entry?.reasoning_efforts)
          ? entry.reasoning_efforts.map(value => String(value || '').trim()).filter(Boolean)
          : [],
      }))
      .filter(entry => entry.id)
    : [];
  if (configuredCatalog.length) newSessionCodexModelCatalog = configuredCatalog;
  const configuredEfforts = Array.isArray(settings.codex_reasoning_efforts)
    ? settings.codex_reasoning_efforts.map(value => String(value || '').trim()).filter(Boolean)
    : [];
  if (configuredEfforts.length) newSessionCodexReasoningEfforts = configuredEfforts;
  populateControlCodexReasoningEfforts();
  populateNewSessionCodexModelSelect();
  newSessionCodexLaunchDefaultsLoaded = true;
  if (!newSessionCodexFastModeTouched) {
    newSessionCodexFastMode = codexServiceTierIsFast(newSessionCodexDefaultServiceTier);
  }
  renderNewSessionAgentControls();
}

function commandDefaultForNewSessionAgent(agentId) {
  return newSessionAgentCommands[agentId] || ({
    codex: 'codex',
    'claude-code': 'claude',
  }[agentId] || '');
}

function effectiveNewSessionAgentId() {
  const select = document.getElementById('new-session-agent');
  const raw = select?.value || '';
  if (raw === 'internal') return 'internal';
  return normalizeAgentId(raw) || newSessionConfiguredAgent || '';
}

function renderNewSessionAgentControls(options = {}) {
  const select = document.getElementById('new-session-agent');
  const commandInput = document.getElementById('new-session-agent-command');
  const browseBtn = document.getElementById('new-session-agent-command-browse');
  const codexModelSel = document.getElementById('new-session-codex-model-select');
  const codexModelInp = document.getElementById('new-session-codex-model');
  const codexReasoningSel = document.getElementById('new-session-codex-reasoning-effort');
  const sandboxSel = document.getElementById('new-session-codex-sandbox');
  const approvalSel = document.getElementById('new-session-codex-approval-policy');
  const managedContextSel = document.getElementById('new-session-codex-managed-context');
  const contextArchiveSel = document.getElementById('new-session-codex-context-archive');
  const fastToggle = document.getElementById('new-session-codex-fast');
  const fastWrap = document.getElementById('new-session-codex-fast-wrap');
  const managedContextNote = document.getElementById('new-session-managed-context-note');
  if (!select || !commandInput) return;

  // Grey out backends whose CLI is missing on the daemon host; kick the
  // probe on first render and re-apply when it lands.
  if (Array.isArray(externalAgentAvailability)) {
    applyExternalAgentAvailabilityToNewSessionPicker();
  } else {
    refreshExternalAgentAvailability();
  }

  const currentOption = select.querySelector('option[value=""]');
  if (currentOption) {
    currentOption.textContent = newSessionConfiguredAgent
      ? `Current setting (${prettyAgentName(newSessionConfiguredAgent)})`
      : 'Current setting (internal agent)';
  }

  const selectedAgent = normalizeAgentId(select.value);
  const effectiveAgent = effectiveNewSessionAgentId();
  const hasExternalAgent = !!selectedAgent;
  // The external-options fold follows the backend choice: open while an
  // external agent is selected (or is the configured default), closed for
  // the internal agent.
  const externalFold = document.getElementById('new-session-external-fold');
  if (externalFold) {
    externalFold.open = hasExternalAgent ||
      (!!effectiveAgent && effectiveAgent !== 'internal' && effectiveAgent !== 'intendant');
  }
  // Execution shape (auto / orchestrate / direct) only applies to the
  // internal agent — external CLIs run their own loops.
  const executionSel = document.getElementById('new-session-execution');
  if (executionSel) {
    const appliesToInternal =
      !effectiveAgent || effectiveAgent === 'internal' || effectiveAgent === 'intendant';
    executionSel.disabled = !appliesToInternal;
    if (!appliesToInternal) executionSel.value = '';
    document
      .getElementById('new-session-execution-wrap')
      ?.classList.toggle('disabled', !appliesToInternal);
  }
  commandInput.disabled = !hasExternalAgent;
  if (browseBtn) browseBtn.disabled = !hasExternalAgent;
  commandInput.placeholder = hasExternalAgent
    ? commandDefaultForNewSessionAgent(selectedAgent)
    : 'Select an external agent';
  if (!hasExternalAgent || options.replaceCommand) {
    commandInput.value = '';
  }
  const claudeModelSel = document.getElementById('new-session-claude-model-select');
  const claudeModelInp = document.getElementById('new-session-claude-model');
  const claudeModeSel = document.getElementById('new-session-claude-permission-mode');
  const claudeEffortSel = document.getElementById('new-session-claude-effort');
  const appliesToClaude = effectiveAgent === 'claude-code';
  const appliesToCodex = effectiveAgent === 'codex';
  if (codexModelSel) {
    codexModelSel.disabled = !appliesToCodex;
    if (!appliesToCodex) codexModelSel.value = '';
  }
  if (codexModelInp) {
    if (!appliesToCodex) codexModelInp.value = '';
  }
  updateNewSessionCodexCustomModelRow();
  if (codexReasoningSel) {
    codexReasoningSel.disabled = !appliesToCodex;
    if (!appliesToCodex) codexReasoningSel.value = '';
  }
  if (claudeModelSel) {
    claudeModelSel.disabled = !appliesToClaude;
    if (!appliesToClaude) claudeModelSel.value = '';
  }
  if (claudeModelInp) {
    claudeModelInp.disabled = !appliesToClaude;
    if (!appliesToClaude) claudeModelInp.value = '';
  }
  updateNewSessionClaudeCustomModelRow();
  if (claudeModeSel) {
    claudeModeSel.disabled = !appliesToClaude;
    if (!appliesToClaude) claudeModeSel.value = '';
  }
  if (claudeEffortSel) {
    claudeEffortSel.disabled = !appliesToClaude;
    if (!appliesToClaude) claudeEffortSel.value = '';
  }
  if (managedContextSel) {
    managedContextSel.disabled = !appliesToCodex;
    managedContextSel.value = newSessionCodexManagedContext;
  }
  if (sandboxSel) {
    sandboxSel.disabled = !appliesToCodex;
    sandboxSel.value = normalizeCodexSandbox(newSessionCodexSandbox);
  }
  if (approvalSel) {
    approvalSel.disabled = !appliesToCodex;
    approvalSel.value = normalizeCodexApprovalPolicy(newSessionCodexApprovalPolicy);
  }
  if (contextArchiveSel) {
    contextArchiveSel.disabled = !appliesToCodex;
    contextArchiveSel.value = normalizeContextArchiveMode(newSessionCodexContextArchive);
  }
  if (fastToggle) {
    fastToggle.disabled = !appliesToCodex;
    fastToggle.checked = appliesToCodex && !!newSessionCodexFastMode;
    if (fastWrap) {
      fastWrap.classList.toggle('disabled', !appliesToCodex);
      fastWrap.classList.toggle('active', appliesToCodex && !!newSessionCodexFastMode);
      const defaultFast = codexServiceTierIsFast(newSessionCodexDefaultServiceTier);
      fastWrap.title = appliesToCodex
        ? (defaultFast
          ? 'Global default is Fast; uncheck to force this new session to normal'
          : 'Start the new Codex session with Fast service tier')
        : 'Fast service tier applies to Codex sessions';
    }
  }
  if (managedContextNote) {
    const mode = managedContextSel?.value || newSessionCodexManagedContext;
    managedContextNote.classList.toggle('warn', appliesToCodex && mode === 'managed');
    managedContextNote.textContent = appliesToCodex && mode === 'managed'
      ? 'Managed requires a patched Codex binary with the managed app-server protocol.'
      : '';
  }
  updateNewSessionFuelBanner();
}

function setNewSessionStartButtonPending(pending) {
  const btn = document.getElementById('new-session-start-btn');
  if (!btn) return;
  btn.disabled = !!pending;
  btn.classList.toggle('pending', !!pending);
  btn.textContent = pending ? 'Spawning...' : 'Start session';
  if (!pending) updateNewSessionFuelBanner();
}

// ── Unfueled preflight ──
// The status frame's aggregate `fueled` flag (presence-level, no settings
// permission needed) gates internal launches before they spawn a session
// that can only die with "No API key found". Strict === false: an unknown
// state (no status frame yet, older daemon) never blocks.

// Layered like refreshUnfueledEmptyState: the status frame's aggregate
// wins when present; otherwise a one-shot HTTP key-status probe fills in
// (the control transport may not be connected yet — or ever, for some
// bindings). Unknown never blocks.
let daemonUnfueledCached = null;
let daemonFuelProbeInFlight = false;
// ui-v2 fueled-banner detail: which providers the key-status probe saw.
// null = never probed; [] = probed and none (or probe failed — generic
// copy, no re-hammering).
let daemonFuelProviders = null;

function daemonInternalUnfueled() {
  const status = dashboardControlTransport?.lastStatus;
  if (status && typeof status.fueled === 'boolean') return status.fueled === false;
  return daemonUnfueledCached === true;
}

function refreshFuelStateForBanner() {
  const status = dashboardControlTransport?.lastStatus;
  // The green Fueled banner names the fueled providers, so the one-shot
  // probe also runs when the status frame already answered the boolean.
  const wantProviders = daemonFuelProviders === null;
  if (status && typeof status.fueled === 'boolean' && !wantProviders) return;
  if ((daemonUnfueledCached !== null && !wantProviders) || daemonFuelProbeInFlight) return;
  if (typeof fetchApiKeyStatus !== 'function') return;
  daemonFuelProbeInFlight = true;
  fetchApiKeyStatus()
    .then(d => {
      if (d && !d.error) {
        if (daemonUnfueledCached === null) daemonUnfueledCached = !(d.openai || d.anthropic || d.gemini);
        daemonFuelProviders = [
          d.anthropic ? 'Anthropic' : '',
          d.openai ? 'OpenAI' : '',
          d.gemini ? 'Gemini' : '',
        ].filter(Boolean);
      } else if (daemonFuelProviders === null) {
        daemonFuelProviders = [];
      }
    })
    .catch(() => { if (daemonFuelProviders === null) daemonFuelProviders = []; })
    .finally(() => {
      daemonFuelProbeInFlight = false;
      updateNewSessionFuelBanner();
    });
}

function newSessionAddKeysAction() {
  return { label: 'Add API keys', onClick: () => focusSettingsApiKeys() };
}

// Leads with the immediate fix (the paired newSessionAddKeysAction deep
// link lands on that card); .env and vault leases stay as secondary paths.
const NEW_SESSION_UNFUELED_MESSAGE =
  'No model credentials for the internal agent yet — add a key in Settings → API Keys (applies immediately, no restart). ' +
  'A .env key on the daemon or a vault credential lease works too; external agents (Codex, Claude Code) sign in with their own accounts.';

// ── Projectless preflight ──
// A daemon launched outside any project reports project_root: null and has
// no default project — a session cannot start without one. Mirrors the
// unfueled preflight: known-projectless blocks submit with a pointer at the
// Project field; unknown (fetch failed, older daemon) never blocks — the
// daemon's structured no_project failure is the backstop.
let daemonProjectless = null; // null = unknown

const NEW_SESSION_NO_PROJECT_MESSAGE =
  'This daemon has no project open. Pick a project directory in the Project field to start a session.';

function newSessionPickProjectAction() {
  return {
    label: 'Pick project',
    onClick: () => {
      const input = document.getElementById('new-session-project-root');
      input?.focus();
      input?.scrollIntoView?.({ block: 'center' });
    },
  };
}

// Shared submit guard (Sessions pane + Station launch): true = blocked.
function newSessionProjectlessBlocked(requestedProjectRoot) {
  if (requestedProjectRoot || daemonProjectless !== true) return false;
  setNewSessionSpawnNotice('error', NEW_SESSION_NO_PROJECT_MESSAGE, newSessionPickProjectAction());
  return true;
}

// A no_project SessionEnded can only come from a failed create (no session
// ever starts under it), so one arriving while a spawn is pending is ours:
// fail the pending notice with the structured class instead of leaving it
// to the timeout or prose-matched log entries.
function maybeFailPendingNewSessionSpawnNoProject(errorKind) {
  if (errorKind !== 'no_project' || !newSessionSpawnPending) return false;
  clearNewSessionSpawnTimers();
  clearNewSessionSpawnRecent();
  newSessionSpawnPending = false;
  newSessionSpawnTask = '';
  newSessionSpawnName = '';
  setNewSessionStartButtonPending(false);
  setNewSessionSpawnNotice('error', NEW_SESSION_NO_PROJECT_MESSAGE, newSessionPickProjectAction());
  showControlToast('error', NEW_SESSION_NO_PROJECT_MESSAGE);
  return true;
}

// QA readback (window.qa convention): the preflight inputs the
// validate-dashboard harness asserts on — module scope hides them.
// Probe functions stay cheap and side-effect-free.
window.qa = Object.assign(window.qa || {}, {
  sessionsFuel: () => ({
    fueled: dashboardControlTransport?.lastStatus?.fueled ?? null,
    haveStatus: !!dashboardControlTransport?.lastStatus,
    unfueledCached: daemonUnfueledCached,
    projectless: daemonProjectless,
    effectiveAgent: effectiveNewSessionAgentId(),
    configuredAgent: newSessionConfiguredAgent || '',
    bannerHidden: !!document.getElementById('new-session-unfueled-banner')?.classList.contains('hidden'),
    startDisabled: !!document.getElementById('new-session-start-btn')?.disabled,
  }),

});

// ── ui-v2 execution segmented control (design overhaul) ──
// The reference's Auto / Orchestrate / Direct segmented choice with a
// per-choice note (execInfo copy, verbatim — it matches the current
// semantics). The v1 <select id="new-session-execution"> stays the source
// of truth (startNewSession reads it; updateNewSessionAgentFields drives
// its disabled state) — the segments only proxy value + disabled, and the
// select is hidden by ui2-sessions.css under the flag. v1 DOM untouched.
const UI2_EXEC_CHOICES = [
  { value: '', label: 'Auto', note: 'The task-size heuristic decides between a single agent and supervised sub-agents.' },
  { value: 'orchestrate', label: 'Orchestrate', note: 'Delegates the task to supervised sub-agents working in isolated git worktrees.' },
  { value: 'direct', label: 'Direct', note: 'A single agent handles the whole task — no delegation.' },
];
let ui2ExecSegEl = null;
let ui2ExecNoteEl = null;

function ui2SyncExecSeg() {
  if (!ui2ExecSegEl) return;
  const sel = document.getElementById('new-session-execution');
  if (!sel) return;
  const value = sel.value || '';
  const disabled = !!sel.disabled;
  ui2ExecSegEl.classList.toggle('disabled', disabled);
  for (const btn of ui2ExecSegEl.querySelectorAll('button[data-exec]')) {
    const active = (btn.dataset.exec || '') === value;
    btn.classList.toggle('is-accent', active && !disabled);
    btn.setAttribute('aria-pressed', active ? 'true' : 'false');
    btn.disabled = disabled;
  }
  if (ui2ExecNoteEl) {
    const choice = UI2_EXEC_CHOICES.find(c => c.value === value) || UI2_EXEC_CHOICES[0];
    ui2ExecNoteEl.textContent = disabled
      ? 'Execution shape applies to the internal agent — external CLIs run their own loops.'
      : choice.note;
  }
}

{
  const wrap = document.getElementById('new-session-execution-wrap');
  if (wrap && wrap.parentElement) {
    const field = document.createElement('div');
    field.className = 'sessions-new-session-field ui2-exec-field';
    const label = document.createElement('span');
    label.className = 'ui2-exec-label';
    label.textContent = 'Execution';
    const seg = document.createElement('div');
    seg.className = 'ui2-seg ui2-exec-seg';
    seg.setAttribute('role', 'group');
    seg.setAttribute('aria-label', 'Execution shape');
    for (const choice of UI2_EXEC_CHOICES) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.dataset.exec = choice.value;
      btn.textContent = choice.label;
      btn.title = choice.note;
      btn.addEventListener('click', () => {
        const sel = document.getElementById('new-session-execution');
        if (!sel || sel.disabled) return;
        sel.value = choice.value;
        sel.dispatchEvent(new Event('change', { bubbles: true }));
        ui2SyncExecSeg();
      });
      seg.appendChild(btn);
    }
    const note = document.createElement('div');
    note.className = 'sessions-agent-note ui2-exec-note';
    field.append(label, seg, note);
    ui2ExecSegEl = seg;
    ui2ExecNoteEl = note;
    wrap.after(field);
    document.getElementById('new-session-execution')?.addEventListener('change', ui2SyncExecSeg);
    ui2SyncExecSeg();
  }
}

function updateNewSessionFuelBanner() {
  const banner = document.getElementById('new-session-unfueled-banner');
  if (!banner) return;
  refreshFuelStateForBanner();
  const effective = effectiveNewSessionAgentId();
  const internalSelected = effective === 'internal' || !effective;
  const show = internalSelected && daemonInternalUnfueled();
  banner.classList.toggle('hidden', !show);
  const btn = document.getElementById('new-session-start-btn');
  if (btn && !newSessionSpawnPending) {
    btn.disabled = show;
    btn.title = show ? 'Internal sessions need an API key or a vault credential lease' : '';
  }

  // The design's green happy-path banner. Shown exclusively when fuel is
  // positively known (status frame `fueled === true` or the key probe
  // found a provider) — an unknown state shows neither banner, never a
  // claimed one.
  const fueledBanner = document.getElementById('new-session-fueled-banner');
  if (fueledBanner) {
    const status = dashboardControlTransport?.lastStatus;
    const knownFueled = (status && status.fueled === true) || daemonUnfueledCached === false;
    const showFueled = internalSelected && !show && knownFueled;
    fueledBanner.classList.toggle('hidden', !showFueled);
    if (showFueled) {
      const textEl = document.getElementById('new-session-fueled-text');
      const names = Array.isArray(daemonFuelProviders) && daemonFuelProviders.length > 0
        ? daemonFuelProviders.join(' + ')
        : '';
      if (textEl) {
        textEl.textContent = names
          ? `Fueled — ${names} credentials active, ready to launch.`
          : 'Fueled — model credentials active, ready to launch.';
      }
    }
  }
  ui2SyncExecSeg();
}

function setNewSessionSpawnNotice(kind, text, action) {
  const notice = document.getElementById('new-session-spawn-notice');
  const textEl = document.getElementById('new-session-spawn-text');
  if (!notice || !textEl) return;
  const hasText = !!String(text || '').trim();
  const noticeKind = ['ok', 'warn', 'error'].includes(kind) ? kind : 'pending';
  notice.className = `sessions-spawn-notice ${noticeKind}` + (hasText ? '' : ' hidden');
  textEl.textContent = text || '';
  notice.title = text || '';
  let actionBtn = document.getElementById('new-session-spawn-action');
  if (action && hasText) {
    if (!actionBtn) {
      actionBtn = document.createElement('button');
      actionBtn.id = 'new-session-spawn-action';
      actionBtn.type = 'button';
      actionBtn.className = 'sessions-spawn-action';
      notice.appendChild(actionBtn);
    }
    actionBtn.textContent = action.label;
    actionBtn.onclick = action.onClick;
  } else if (actionBtn) {
    actionBtn.remove();
  }
  stationScheduleUpdate();
}

function clearNewSessionSpawnTimers() {
  if (newSessionSpawnTimeout) clearTimeout(newSessionSpawnTimeout);
  if (newSessionSpawnClearTimeout) clearTimeout(newSessionSpawnClearTimeout);
  newSessionSpawnTimeout = null;
  newSessionSpawnClearTimeout = null;
}

function clearNewSessionSpawnRecent() {
  if (newSessionSpawnRecentTimeout) clearTimeout(newSessionSpawnRecentTimeout);
  newSessionSpawnRecent = null;
  newSessionSpawnRecentTimeout = null;
}

function rememberNewSessionSpawnRecent(sessionId, task) {
  clearNewSessionSpawnRecent();
  const sid = String(sessionId || '').trim();
  if (!sid) return;
  newSessionSpawnRecent = {
    sessionId: sid,
    task: String(task || '').trim(),
    expiresAt: Date.now() + NEW_SESSION_LAUNCH_FAILURE_GRACE_MS,
  };
  newSessionSpawnRecentTimeout = setTimeout(() => {
    newSessionSpawnRecent = null;
    newSessionSpawnRecentTimeout = null;
  }, NEW_SESSION_LAUNCH_FAILURE_GRACE_MS);
}

function isNewSessionLaunchFailureReason(reason) {
  const text = String(reason || '').toLowerCase();
  return text.includes('error') || text.includes('failed') || text.includes('failure');
}

function formatNewSessionLaunchFailureReason(reason) {
  const text = String(reason || '').trim();
  if (!text) return 'Session failed shortly after it started.';
  return `Session failed: ${text.replace(/^error:\s*/i, '')}`;
}

function maybeFailRecentNewSessionSpawn(sessionId, reason, errorKind) {
  const sid = String(sessionId || '').trim();
  if (!sid || !newSessionSpawnRecent) return false;
  if (newSessionSpawnRecent.sessionId !== sid) return false;
  if (Date.now() > Number(newSessionSpawnRecent.expiresAt || 0)) {
    clearNewSessionSpawnRecent();
    return false;
  }
  if (!isNewSessionLaunchFailureReason(reason)) return false;

  const message = formatNewSessionLaunchFailureReason(reason);
  clearNewSessionSpawnTimers();
  clearNewSessionSpawnRecent();
  newSessionSpawnPending = false;
  newSessionSpawnTask = '';
  newSessionSpawnName = '';
  setNewSessionStartButtonPending(false);
  // Structured failure classes carry an action instead of prose-parsing.
  const action = errorKind === 'unfueled'
    ? newSessionAddKeysAction()
    : errorKind === 'no_project'
      ? newSessionPickProjectAction()
      : null;
  setNewSessionSpawnNotice('error', message, action);
  showControlToast('error', message);
  return true;
}

function beginNewSessionSpawnNotice(task, text, name = '') {
  clearNewSessionSpawnTimers();
  clearNewSessionSpawnRecent();
  newSessionSpawnPending = true;
  newSessionSpawnTask = String(task || '').trim();
  newSessionSpawnName = String(name || '').trim();
  setNewSessionStartButtonPending(true);
  setNewSessionSpawnNotice('pending', text || 'Spawning new session...');
  newSessionSpawnTimeout = setTimeout(() => {
    if (!newSessionSpawnPending) return;
    newSessionSpawnPending = false;
    newSessionSpawnTask = '';
    newSessionSpawnName = '';
    setNewSessionStartButtonPending(false);
    // Blame the transport when it is actually down — "check the Activity
    // log" is a dead end while no event lane exists to have delivered
    // anything to the daemon in the first place.
    const laneDown = typeof dashboardEventLaneUp === 'function' && !dashboardEventLaneUp();
    setNewSessionSpawnNotice('warn', laneDown
      ? 'No start confirmation — the dashboard has no live event connection (reconnecting), so the request may not have reached the daemon. Retry once the connection is back.'
      : 'No start confirmation yet. Check the Activity log before retrying.');
    showControlToast('info', laneDown
      ? 'No start confirmation — dashboard event connection is down.'
      : 'No new-session start confirmation yet.');
  }, NEW_SESSION_SPAWN_TIMEOUT_MS);
}

function updateNewSessionSpawnNotice(kind, text) {
  if (!newSessionSpawnPending) return;
  setNewSessionSpawnNotice(kind, text);
}

function failNewSessionSpawnNotice(text) {
  clearNewSessionSpawnTimers();
  clearNewSessionSpawnRecent();
  newSessionSpawnPending = false;
  newSessionSpawnTask = '';
  newSessionSpawnName = '';
  setNewSessionStartButtonPending(false);
  setNewSessionSpawnNotice('error', text || 'New session did not start.');
  showControlToast('error', text || 'New session did not start.');
}

function clearNewSessionDraftIfUnchanged(task, name) {
  const expectedTask = String(task || '').trim();
  const input = document.getElementById('new-session-input');
  if (input && expectedTask && input.value.trim() === expectedTask) {
    clearTaskTextarea(input);
  }
  const expectedName = String(name || '').trim();
  const nameInput = document.getElementById('new-session-name');
  if (nameInput && expectedName && nameInput.value.trim() === expectedName) {
    nameInput.value = '';
  }
}

function finishNewSessionSpawnNotice(sessionId, task) {
  if (!newSessionSpawnPending) return;
  const expectedTask = newSessionSpawnTask;
  const expectedName = newSessionSpawnName;
  const actualTask = String(task || '').trim();
  if (expectedTask && actualTask && expectedTask !== actualTask) return;
  clearNewSessionSpawnTimers();
  newSessionSpawnPending = false;
  newSessionSpawnTask = '';
  newSessionSpawnName = '';
  clearNewSessionDraftIfUnchanged(actualTask || expectedTask, expectedName);
  setNewSessionStartButtonPending(false);
  const shortId = sessionId ? ` (${shortSessionId(sessionId)})` : '';
  setNewSessionSpawnNotice('ok', `Session started${shortId}. Activity is ready.`);
  showControlToast('success', `Session started${shortId}`);
  rememberNewSessionSpawnRecent(sessionId, actualTask || expectedTask);
  newSessionSpawnClearTimeout = setTimeout(() => {
    setNewSessionSpawnNotice('', '');
    newSessionSpawnClearTimeout = null;
  }, 2500);
}

function maybeFailNewSessionSpawnFromLog(c) {
  if (!newSessionSpawnPending || !c) return;
  const level = String(c.level || '').toLowerCase();
  if (level !== 'error') return;
  const content = String(c.content || '').trim();
  if (!/^(Session create failed|Project load failed):/.test(content)) return;
  failNewSessionSpawnNotice(content);
}

async function loadNewSessionProjectRoot() {
  try {
    const d = await fetchProjectRoot();
    // project_root: null = projectless daemon (a rooted daemon always
    // reports a non-empty string). On fetch failure the flag stays
    // unknown and never blocks.
    daemonProjectless = !d.project_root;
    setNewSessionProjectRoot(d.project_root || '');
  } catch (e) {
    console.warn('Failed to load project root:', e);
  }
}

async function startNewSession() {
  const input = document.getElementById('new-session-input');
  if (!input) return;
  const task = input.value.trim();
  if (!task) return;
  if (newSessionSpawnPending) {
    showControlToast('info', 'A new session is already spawning.');
    return;
  }
  if (!app) {
    failNewSessionSpawnNotice('Dashboard is not connected to the server.');
    return;
  }
  const effectiveAgent = effectiveNewSessionAgentId();
  if ((effectiveAgent === 'internal' || !effectiveAgent) && daemonInternalUnfueled()) {
    // Belt-and-braces behind the banner: the fueled flag may have flipped
    // since the last render.
    updateNewSessionFuelBanner();
    setNewSessionSpawnNotice('error', NEW_SESSION_UNFUELED_MESSAGE, newSessionAddKeysAction());
    return;
  }

  const nameInput = document.getElementById('new-session-name');
  const sessionName = nameInput?.value.trim() || '';
  const direct = document.getElementById('direct-mode-toggle')?.checked || false;
  const attachments = pendingAttachments.map(a => a.frameId);
  const attachmentReceipt = pendingAttachments.slice();
  const requestedProjectRoot = document.getElementById('new-session-project-root')?.value.trim() || '';
  if (newSessionProjectlessBlocked(requestedProjectRoot)) return;
  beginNewSessionSpawnNotice(
    task,
    requestedProjectRoot ? 'Checking project directory...' : 'Spawning new session...',
    sessionName
  );

  let projectRoot = '';
  try {
    projectRoot = await ensureNewSessionProjectDirectory(requestedProjectRoot);
  } catch (e) {
    failNewSessionSpawnNotice(e?.message || 'Project directory check failed.');
    return;
  }
  if (requestedProjectRoot && !projectRoot) {
    failNewSessionSpawnNotice('Project directory needs attention before the session can start.');
    return;
  }

  const msg = { action: 'create_session', task: task };
  if (sessionName) msg.name = sessionName;
  if (projectRoot) msg.project_root = projectRoot;
  const agentValue = document.getElementById('new-session-agent')?.value || '';
  const selectedAgent = normalizeAgentId(agentValue);
  if (agentValue === 'internal') {
    msg.agent = 'internal';
  } else if (selectedAgent) {
    msg.agent = selectedAgent;
    const agentCommand = document.getElementById('new-session-agent-command')?.value.trim() || '';
    if (agentCommand) msg.agent_command = agentCommand;
  }
  if (effectiveNewSessionAgentId() === 'claude-code') {
    const modelChoice = document.getElementById('new-session-claude-model-select')?.value || '';
    const model = modelChoice === '__custom__'
      ? (document.getElementById('new-session-claude-model')?.value.trim() || '')
      : modelChoice;
    if (model) msg.claude_model = model;
    const mode = document.getElementById('new-session-claude-permission-mode')?.value || '';
    if (mode) msg.claude_permission_mode = mode;
    const effort = document.getElementById('new-session-claude-effort')?.value || '';
    if (effort) msg.claude_effort = effort;
  }
  if (effectiveNewSessionAgentId() === 'codex') {
    const modelChoice = document.getElementById('new-session-codex-model-select')?.value || '';
    const model = modelChoice === '__custom__'
      ? (document.getElementById('new-session-codex-model')?.value.trim() || '')
      : modelChoice;
    if (model) msg.codex_model = model;
    const reasoningEffort = document.getElementById('new-session-codex-reasoning-effort')?.value || '';
    if (reasoningEffort) msg.codex_reasoning_effort = reasoningEffort;
    if (newSessionCodexLaunchDefaultsLoaded) {
      msg.codex_sandbox = normalizeCodexSandbox(
        document.getElementById('new-session-codex-sandbox')?.value || newSessionCodexSandbox
      );
      msg.codex_approval_policy = normalizeCodexApprovalPolicy(
        document.getElementById('new-session-codex-approval-policy')?.value || newSessionCodexApprovalPolicy
      );
      const mode = document.getElementById('new-session-codex-managed-context')?.value === 'managed'
        ? 'managed'
        : 'vanilla';
      msg.codex_managed_context = mode;
      const archiveMode = normalizeContextArchiveMode(
        document.getElementById('new-session-codex-context-archive')?.value || newSessionCodexContextArchive
      );
      msg.codex_context_archive = archiveMode;
      const fastChecked = !!document.getElementById('new-session-codex-fast')?.checked;
      if (fastChecked) {
        msg.codex_service_tier = 'priority';
      } else if (codexServiceTierIsFast(newSessionCodexDefaultServiceTier)) {
        msg.codex_service_tier = 'standard';
      }
    }
  }
  // Execution shape: an explicit per-launch choice beats the global Direct
  // toggle; Auto (or an external agent — the select is disabled and cleared
  // then) preserves the old behavior of the toggle forcing direct.
  const executionSel = document.getElementById('new-session-execution');
  const execution = executionSel && !executionSel.disabled ? executionSel.value : '';
  if (execution === 'orchestrate') {
    msg.orchestrate = true;
  } else if (execution === 'direct' || direct) {
    msg.direct = true;
  }
  // Worktree launch: the daemon validates/derives the branch, creates the
  // worktree off the project's HEAD, and roots the session inside it.
  if (document.getElementById('new-session-worktree')?.checked) {
    msg.worktree = true;
    const worktreeBranch = document.getElementById('new-session-worktree-branch')?.value.trim() || '';
    if (worktreeBranch) msg.worktree_branch = worktreeBranch;
  }
  if (attachments.length > 0) msg.attachments = attachments;

  try {
    const sent = dispatchSessionControlMsg(msg, {
      onError: err => failNewSessionSpawnNotice(err?.message || 'Failed to send new-session request.'),
    });
    if (!sent) throw new Error('Dashboard is not connected to the server.');
  } catch (e) {
    failNewSessionSpawnNotice(e?.message || 'Failed to send new-session request.');
    return;
  }

  updateNewSessionSpawnNotice('pending', 'Spawning new session...');
  showControlToast('info', 'Spawning new session...');
  resetNewSessionCodexFastModeToDefault();
  renderNewSessionAgentControls();
  if (attachments.length > 0) {
    renderAttachmentReceipt(task, attachmentReceipt, 'Sent');
    clearPendingAttachments({ retainPreviewUrls: true });
  }
}
window.startNewSession = startNewSession;

// Reveal the optional branch-name input only while the worktree launch is
// requested; an untouched form stays exactly as before.
function onNewSessionWorktreeToggle() {
  const checked = !!document.getElementById('new-session-worktree')?.checked;
  document.getElementById('new-session-worktree-branch-row')?.classList.toggle('hidden', !checked);
}
window.onNewSessionWorktreeToggle = onNewSessionWorktreeToggle;

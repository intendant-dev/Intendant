// ── Plugins & Skills (System → Plugins; tab id #plugins is permanent) ──
//
// The unified surface: three daemon-derived sections, presentation-only
// grouping with no vocabulary of its own.
//   1. Plugins — host-owned cards over GET /api/plugins (tunnel twin
//      api_plugins_list) and POST /api/plugins/{plugin_id}
//      (api_plugin_set_enabled). Everything on a card derives from the
//      daemon body — lifecycle state, readiness layers, per-skill install
//      facts — and the toggle's response carries the refreshed entry plus
//      the installer's per-root report, so there is no second fetch and
//      no pretending: the card says what the installer actually did.
//      Plugins here are declarative bundles (skills + readiness); they
//      ship no code, hooks, or frames, so this surface renders daemon
//      data only.
//   2. Skills — the unified skill catalog over GET /api/skills (tunnel
//      twin api_skills_list): every skill the daemon manages, one row
//      each, rendered verbatim (provenance, trust posture, per-root
//      install facts). Each row's served `lifecycle` body decides its
//      gesture — one daemon classification, no client kind table:
//      control 'toggle' rows deactivate/re-enable via POST
//      /api/skills/{name} (api_skill_set_enabled; the daemon flips the
//      persisted disabled-set and sweeps BOTH install roots in-request,
//      the response carrying the refreshed row + installer report, and a
//      deactivated row renders the served gate-resolved attribution);
//      control 'plugin' rows deep-link their plugin card — the plugin
//      toggle is the one lifecycle authority, no second switch here.
//   3. Automation templates — the served definition catalog
//      (api_agenda_definitions) read-first: provenance/shadowed/invalid
//      state verbatim, with Automate… opening the EXISTING agenda stamp
//      sheet preselected — no second stamp lane.
//
// Deep-link TDZ rule: evaluates BEFORE the router (48) because a
// #plugins deep link makes the router's eval-time boot call
// pluginsOnTabShown(), which reads this fragment's module-level lets.
// Top level declares only lets/consts/functions.

let pluginsRows = [];
let pluginsError = '';
let pluginsLoaded = false;
let pluginsFetchInFlight = null;
let pluginsBusy = {};        // plugin id -> true while a toggle runs
let pluginsLastInstall = {}; // plugin id -> install report from the last toggle
let skillsRows = null;       // null until the catalog loads
let skillsError = '';
let skillsFetchInFlight = null;
let skillsBusy = {};         // skill name -> true while a toggle runs
let skillsLastInstall = {};  // skill name -> install report from the last toggle
let templatesRows = null;    // null until the catalog loads
let templatesError = '';
let templatesFetchInFlight = null;

function pluginsOnTabShown() {
  if (pluginsLoaded) renderPlugins();
  else loadPlugins();
  if (skillsRows) renderSkillsSection();
  else loadSkillsSection();
  if (templatesRows) renderTemplatesSection();
  else loadTemplatesSection();
}

async function loadPlugins() {
  if (pluginsFetchInFlight) return pluginsFetchInFlight;
  pluginsFetchInFlight = (async () => {
    const avail = daemonApi.availability('api_plugins_list');
    if (!avail.ok) {
      pluginsError = avail.reason === 'denied'
        ? "This session's role can't read the plugin catalog."
        : avail.reason === 'unsupported'
          ? 'This daemon predates the plugin catalog — upgrade it to manage plugins from here.'
          : 'Daemon connection not ready yet.';
      pluginsFetchInFlight = null;
      renderPlugins();
      return;
    }
    try {
      const resp = await daemonApi.request('api_plugins_list', {});
      if (resp.ok && resp.body && Array.isArray(resp.body.plugins)) {
        pluginsRows = resp.body.plugins;
        pluginsError = '';
        pluginsLoaded = true;
      } else {
        pluginsError = (resp.body && resp.body.error) || `plugin catalog unavailable (${resp.status})`;
      }
    } catch (e) {
      pluginsError = String((e && e.message) || e);
    } finally {
      pluginsFetchInFlight = null;
    }
    renderPlugins();
  })();
  return pluginsFetchInFlight;
}

// The daemon's snake_case lifecycle vocabulary as chip label + tone.
// Unknown values pass through verbatim — the daemon is the source of
// truth ("derive, don't mirror").
const PLUGIN_STATE_CHIPS = {
  available: { label: 'Available', cls: '' },
  needs_setup: { label: 'Needs setup', cls: 'warn' },
  enabled: { label: 'Enabled', cls: 'ok' },
  setup_failed: { label: 'Setup failed', cls: 'warn' },
};

async function pluginSetEnabled(pluginId, enabled) {
  if (pluginsBusy[pluginId]) return;
  const avail = daemonApi.availability('api_plugin_set_enabled');
  if (!avail.ok) {
    pluginsError = avail.reason === 'denied'
      ? "This session's role can't manage plugins."
      : 'Plugin management is unavailable on this daemon.';
    renderPlugins();
    return;
  }
  pluginsBusy[pluginId] = true;
  renderPlugins();
  try {
    const resp = await daemonApi.request('api_plugin_set_enabled', {
      plugin_id: pluginId,
      enabled,
    });
    if (resp.ok && resp.body && resp.body.plugin) {
      pluginsRows = pluginsRows.map(p => (p && p.id === pluginId) ? resp.body.plugin : p);
      pluginsLastInstall[pluginId] = resp.body.install || null;
      pluginsError = '';
    } else {
      pluginsError = (resp.body && resp.body.error) || `plugin toggle failed (${resp.status})`;
    }
  } catch (e) {
    pluginsError = String((e && e.message) || e);
  } finally {
    delete pluginsBusy[pluginId];
  }
  renderPlugins();
}

function pluginReadinessHtml(readiness) {
  if (!readiness || !Array.isArray(readiness.layers) || !readiness.layers.length) return '';
  const rows = readiness.layers.map(layer => {
    const ok = layer && layer.status === 'ready';
    const chip = ok
      ? '<span class="ui-chip ok">ready</span>'
      : `<span class="ui-chip warn">${escapeHtml((layer && layer.status) || 'unknown')}</span>`;
    const fix = !ok && layer && layer.fix
      ? `<div class="plugin-layer-fix">${escapeHtml(layer.fix)}</div>`
      : '';
    return `<div class="plugin-layer">
      <div class="plugin-layer-head">${chip}<span class="plugin-layer-name">${escapeHtml((layer && layer.layer) || '')}</span></div>
      <div class="plugin-layer-detail">${escapeHtml((layer && layer.detail) || '')}</div>${fix}
    </div>`;
  }).join('');
  return `<div class="plugin-readiness">${rows}</div>`;
}

function pluginSkillsHtml(skills) {
  if (!Array.isArray(skills) || !skills.length) return '';
  const rows = skills.map(s => {
    const roots = s && s.roots && typeof s.roots === 'object'
      ? Object.entries(s.roots).map(([root, st]) => {
          const cls = st === 'installed' ? 'ok' : (st === 'absent' ? '' : 'warn');
          return `<span class="ui-chip ${cls}">${escapeHtml(root)}: ${escapeHtml(String(st))}</span>`;
        }).join(' ')
      : '';
    return `<div class="plugin-skill-row"><code>${escapeHtml((s && s.name) || '')}</code>${roots}</div>`;
  }).join('');
  return `<div class="plugin-skills">${rows}</div>`;
}

// One line summarizing what the installer did on the last toggle —
// straight from the response's per-root report, never invented.
function pluginInstallNoteHtml(install) {
  if (!Array.isArray(install)) return '';
  const parts = install.map(r => {
    if (!r || !r.root) return '';
    if (r.outcome === 'applied' && r.detail) {
      const d = r.detail;
      const bits = [];
      if (Array.isArray(d.installed) && d.installed.length) bits.push(`installed ${d.installed.length}`);
      if (Array.isArray(d.removed_stale) && d.removed_stale.length) bits.push(`removed ${d.removed_stale.length}`);
      if (Array.isArray(d.skipped_user_owned) && d.skipped_user_owned.length) bits.push(`kept ${d.skipped_user_owned.length} user-owned`);
      if (!bits.length) bits.push('no changes');
      return `${r.root}: ${bits.join(', ')}`;
    }
    if (r.outcome === 'failed') return `${r.root}: failed — ${r.detail || 'unknown error'}`;
    if (r.outcome === 'root_user_owned') return `${r.root}: user-owned, untouched`;
    return '';
  }).filter(Boolean);
  if (!parts.length) return '';
  return `<div class="plugin-install-note">Last change — ${escapeHtml(parts.join(' · '))}</div>`;
}

function pluginCardHtml(p) {
  if (!p || !p.id) return '';
  const chip = PLUGIN_STATE_CHIPS[p.state] || { label: p.state || 'unknown', cls: '' };
  const busy = Boolean(pluginsBusy[p.id]);
  const toggleLabel = busy ? 'Working…' : (p.enabled ? 'Disable' : 'Enable');
  const btnCls = p.enabled ? 'ui-btn' : 'ui-btn primary';
  const summaryLine = p.readiness && p.readiness.summary
    ? `<div class="plugin-summary-line">${escapeHtml(p.readiness.summary)}</div>`
    : '';
  const installNote = pluginInstallNoteHtml(pluginsLastInstall[p.id]);
  return `<div class="ui-card plugin-card" data-plugin-id="${escapeHtml(p.id)}">
    <div class="plugin-card-head">
      <div class="plugin-card-title">${escapeHtml(p.display_name || p.id)}</div>
      <span class="ui-chip ${chip.cls}">${escapeHtml(chip.label)}</span>
      <button type="button" class="${btnCls}" data-plugin-toggle="${escapeHtml(p.id)}" data-enable="${p.enabled ? '0' : '1'}"${busy ? ' disabled' : ''}>${toggleLabel}</button>
    </div>
    <div class="plugin-card-summary">${escapeHtml(p.summary || '')}</div>
    ${summaryLine}
    ${pluginReadinessHtml(p.readiness)}
    ${pluginSkillsHtml(p.skills)}
    ${installNote}
  </div>`;
}

function renderPlugins() {
  const status = document.getElementById('plugins-status');
  if (status) {
    status.textContent = pluginsError ? `Error: ${pluginsError}` : '';
    status.classList.toggle('plugins-status-error', Boolean(pluginsError));
  }
  const list = document.getElementById('plugins-list');
  if (!list) return;
  if (!pluginsLoaded && !pluginsError) {
    list.innerHTML = '<div class="ui-explainer">Loading plugin catalog…</div>';
    return;
  }
  if (!pluginsRows.length) {
    list.innerHTML = '<div class="ui-empty"><div class="ui-empty-title">No bundled plugins</div><div class="ui-empty-hint">This build ships no optional plugins.</div></div>';
    return;
  }
  list.innerHTML = pluginsRows.map(pluginCardHtml).join('');
  list.querySelectorAll('button[data-plugin-toggle]').forEach(btn => {
    btn.onclick = () => pluginSetEnabled(btn.dataset.pluginToggle, btn.dataset.enable === '1');
  });
}

// ── Skills section (GET /api/skills) ───────────────────────────────────

async function loadSkillsSection() {
  if (skillsFetchInFlight) return skillsFetchInFlight;
  skillsFetchInFlight = (async () => {
    const avail = daemonApi.availability('api_skills_list');
    if (!avail.ok) {
      skillsError = avail.reason === 'denied'
        ? "This session's role can't read the skill catalog."
        : avail.reason === 'unsupported'
          ? 'This daemon predates the skill catalog — upgrade it to see skills here.'
          : 'Daemon connection not ready yet.';
      skillsFetchInFlight = null;
      renderSkillsSection();
      return;
    }
    try {
      const resp = await daemonApi.request('api_skills_list', {});
      if (resp.ok && resp.body && Array.isArray(resp.body.skills)) {
        skillsRows = resp.body.skills;
        skillsError = '';
      } else {
        skillsError = (resp.body && resp.body.error) || `skill catalog unavailable (${resp.status})`;
      }
    } catch (e) {
      skillsError = String((e && e.message) || e);
    } finally {
      skillsFetchInFlight = null;
    }
    renderSkillsSection();
  })();
  return skillsFetchInFlight;
}

// Scroll-and-flash the plugin card that owns a plugin-provenance skill
// row (R5: the one-authority rule made visible — the row itself carries
// no second lifecycle switch).
function skillsRevealPluginCard(pluginId) {
  const card = document.querySelector(`.plugin-card[data-plugin-id="${CSS.escape(pluginId)}"]`);
  if (!card) return;
  card.scrollIntoView({ behavior: 'smooth', block: 'center' });
  card.classList.remove('plugin-card-flash');
  void card.offsetWidth; // restart the animation on repeat clicks
  card.classList.add('plugin-card-flash');
  setTimeout(() => card.classList.remove('plugin-card-flash'), 1800);
}

// Deactivate / re-enable one toggle-controlled skill. The daemon is the
// wall: it flips the persisted disabled-set, sweeps both install roots
// in the same request, and replies with the refreshed row plus the
// installer's per-root report — or a named per-kind refusal (plugin
// payloads refuse toward their plugin's toggle) rendered verbatim.
async function skillSetEnabled(name, enabled) {
  if (skillsBusy[name]) return;
  const avail = daemonApi.availability('api_skill_set_enabled');
  if (!avail.ok) {
    skillsError = avail.reason === 'denied'
      ? "This session's role can't manage skills."
      : 'Skill management is unavailable on this daemon.';
    renderSkillsSection();
    return;
  }
  skillsBusy[name] = true;
  renderSkillsSection();
  try {
    const resp = await daemonApi.request('api_skill_set_enabled', { name, enabled });
    if (resp.ok && resp.body && resp.body.skill) {
      skillsRows = (skillsRows || []).map(s => (s && s.name === name) ? resp.body.skill : s);
      skillsLastInstall[name] = resp.body.install || null;
      skillsError = '';
    } else {
      skillsError = (resp.body && resp.body.error) || `skill toggle failed (${resp.status})`;
    }
  } catch (e) {
    skillsError = String((e && e.message) || e);
  } finally {
    delete skillsBusy[name];
  }
  renderSkillsSection();
}

// "Disabled by you · <when>" from the served gate-resolved record —
// never a client-side claim. Plain TEXT only — callers escape.
function skillAttributionText(rec) {
  if (!rec || typeof rec !== 'object') return 'Disabled';
  const who = rec.kind === 'dashboard' ? 'you'
    : rec.kind === 'local_process' ? 'local ctl'
      : (rec.principal || 'unknown');
  const when = rec.at_ms ? new Date(rec.at_ms).toLocaleString() : '';
  return `Disabled by ${who}${when ? ` · ${when}` : ''}`;
}

// Per-root install chips, the same status vocabulary the plugin cards
// render: the daemon's strings pass through verbatim, tone only.
function skillRootChipsHtml(roots) {
  if (!roots || typeof roots !== 'object') return '';
  return Object.entries(roots).map(([root, st]) => {
    const cls = st === 'installed' ? 'ok' : (st === 'absent' ? '' : 'warn');
    return `<span class="ui-chip ${cls}">${escapeHtml(root)}: ${escapeHtml(String(st))}</span>`;
  }).join(' ');
}

function skillRowHtml(s) {
  if (!s || !s.name) return '';
  // The served lifecycle body is the one gesture authority: 'toggle'
  // rows get the deactivate/re-enable button (state + attribution from
  // the same body), 'plugin' rows keep the plugin deep-link as their
  // only door, and anything this frontend does not know renders no
  // gesture at all (an older daemon serves none — same outcome).
  const lc = (s.lifecycle && typeof s.lifecycle === 'object') ? s.lifecycle : null;
  const disabled = Boolean(lc && lc.control === 'toggle' && lc.enabled === false);
  const busy = Boolean(skillsBusy[s.name]);
  let gesture = '';
  if (lc && lc.control === 'toggle') {
    const label = busy ? 'Working…' : (disabled ? 'Re-enable' : 'Deactivate');
    gesture = `<button type="button" class="${disabled ? 'ui-btn primary' : 'ui-btn'} skill-toggle" data-skill-toggle="${escapeHtml(s.name)}" data-enable="${disabled ? '1' : '0'}"${busy ? ' disabled' : ''}>${label}</button>`;
  }
  const pluginLink = s.plugin_id
    ? `<button type="button" class="ui-btn skill-plugin-link" data-skill-plugin="${escapeHtml(s.plugin_id)}" title="This skill's lifecycle is its plugin's toggle — there is no per-skill switch">View plugin</button>`
    : '';
  const stateChip = disabled ? '<span class="ui-chip warn">deactivated</span>' : '';
  const attribution = disabled
    ? `<div class="skill-row-attribution">${escapeHtml(skillAttributionText(lc.disabled_by))} — the bytes stay in the binary; re-enable restores the install.</div>`
    : '';
  const desc = s.description
    ? `<div class="skill-row-desc">${escapeHtml(s.description)}</div>`
    : '';
  const trust = s.trust_posture
    ? `<div class="skill-row-trust">${escapeHtml(s.trust_posture)}</div>`
    : '';
  const installNote = pluginInstallNoteHtml(skillsLastInstall[s.name]);
  return `<div class="ui-card skill-row${disabled ? ' skill-row-deactivated' : ''}">
    <div class="skill-row-head">
      <code class="skill-row-name">${escapeHtml(s.name)}</code>
      <span class="ui-chip">${escapeHtml(String(s.provenance || ''))}</span>
      ${stateChip}
      ${pluginLink}
      ${gesture}
    </div>
    ${attribution}
    ${desc}
    ${trust}
    <div class="skill-row-roots">${skillRootChipsHtml(s.roots)}</div>
    ${installNote}
  </div>`;
}

function renderSkillsSection() {
  const status = document.getElementById('skills-status');
  if (status) {
    status.textContent = skillsError ? `Error: ${skillsError}` : '';
    status.classList.toggle('plugins-status-error', Boolean(skillsError));
  }
  const list = document.getElementById('skills-list');
  if (!list) return;
  if (!skillsRows && !skillsError) {
    list.innerHTML = '<div class="ui-explainer">Loading skill catalog…</div>';
    return;
  }
  if (!skillsRows || !skillsRows.length) {
    list.innerHTML = skillsError ? '' : '<div class="ui-empty"><div class="ui-empty-title">No managed skills</div><div class="ui-empty-hint">This daemon manages no skills.</div></div>';
    return;
  }
  list.innerHTML = skillsRows.map(skillRowHtml).join('');
  list.querySelectorAll('button[data-skill-plugin]').forEach(btn => {
    btn.onclick = () => skillsRevealPluginCard(btn.dataset.skillPlugin);
  });
  list.querySelectorAll('button[data-skill-toggle]').forEach(btn => {
    btn.onclick = () => skillSetEnabled(btn.dataset.skillToggle, btn.dataset.enable === '1');
  });
}

// ── Automation templates section (api_agenda_definitions) ──────────────

async function loadTemplatesSection() {
  if (templatesFetchInFlight) return templatesFetchInFlight;
  templatesFetchInFlight = (async () => {
    const avail = daemonApi.availability('api_agenda_definitions');
    if (!avail.ok) {
      templatesError = avail.reason === 'denied'
        ? "This session's role can't read the automation-template catalog."
        : avail.reason === 'unsupported'
          ? 'This daemon predates the automation-template catalog.'
          : 'Daemon connection not ready yet.';
      templatesFetchInFlight = null;
      renderTemplatesSection();
      return;
    }
    try {
      const resp = await daemonApi.request('api_agenda_definitions', {});
      if (resp.ok && resp.body && Array.isArray(resp.body.definitions)) {
        templatesRows = resp.body.definitions;
        templatesError = '';
      } else {
        templatesError = (resp.body && resp.body.error) || `template catalog unavailable (${resp.status})`;
      }
    } catch (e) {
      templatesError = String((e && e.message) || e);
    } finally {
      templatesFetchInFlight = null;
    }
    renderTemplatesSection();
  })();
  return templatesFetchInFlight;
}

function templateRowHtml(d) {
  if (!d || !d.name) return '';
  // Provenance / shadowed / invalid chips render the served values
  // verbatim; the stampability rule mirrors the Automate sheet's picker
  // (valid && !shadowed) so this section never promises a stamp the
  // sheet would refuse.
  const chips = [`<span class="ui-chip">${escapeHtml(String(d.provenance || ''))}</span>`];
  if (d.shadowed) chips.push('<span class="ui-chip warn">shadowed</span>');
  if (!d.valid) chips.push('<span class="ui-chip warn">invalid</span>');
  const usable = Boolean(d.valid && !d.shadowed);
  const automateTitle = usable
    ? 'Open the Automate sheet with this definition selected'
    : (d.shadowed
      ? 'shadowed by a personal definition of the same name'
      : (d.reason || 'invalid definition'));
  const kindLine = typeof agendaDefinitionKindLine === 'function' ? agendaDefinitionKindLine(d) : '';
  const desc = d.description
    ? `<div class="template-row-desc">${escapeHtml(d.description)}</div>`
    : '';
  const reason = !d.valid && d.reason
    ? `<div class="template-row-reason">${escapeHtml(d.reason)}</div>`
    : '';
  return `<div class="ui-card template-row">
    <div class="template-row-head">
      <span class="template-row-title">${escapeHtml(d.title || d.name)}</span>
      ${chips.join(' ')}
      <button type="button" class="ui-btn template-automate" data-template="${escapeHtml(d.name)}" title="${escapeHtml(automateTitle)}"${usable ? '' : ' disabled'}>Automate…</button>
    </div>
    ${kindLine ? `<div class="template-row-kind">${escapeHtml(kindLine)}</div>` : ''}
    ${desc}
    ${reason}
  </div>`;
}

function renderTemplatesSection() {
  const status = document.getElementById('templates-status');
  if (status) {
    status.textContent = templatesError ? `Error: ${templatesError}` : '';
    status.classList.toggle('plugins-status-error', Boolean(templatesError));
  }
  const list = document.getElementById('templates-list');
  if (!list) return;
  if (!templatesRows && !templatesError) {
    list.innerHTML = '<div class="ui-explainer">Loading template catalog…</div>';
    return;
  }
  if (!templatesRows || !templatesRows.length) {
    list.innerHTML = templatesError ? '' : '<div class="ui-empty"><div class="ui-empty-title">No automation templates</div><div class="ui-empty-hint">This daemon serves no definitions.</div></div>';
    return;
  }
  list.innerHTML = templatesRows.map(templateRowHtml).join('');
  list.querySelectorAll('button.template-automate').forEach(btn => {
    btn.onclick = () => {
      if (typeof agendaOpenAutomationSheet === 'function') {
        agendaOpenAutomationSheet(btn, btn.dataset.template);
      }
    };
  });
}

// ── Plugins (System → Plugins) ─────────────────────────────────────────
//
// The bundled-plugin catalog: host-owned cards over GET /api/plugins
// (tunnel twin api_plugins_list) and POST /api/plugins/{plugin_id}
// (api_plugin_set_enabled). Everything on a card derives from the daemon
// body — lifecycle state, readiness layers, per-skill install facts —
// and the toggle's response carries the refreshed entry plus the
// installer's per-root report, so there is no second fetch and no
// pretending: the card says what the installer actually did. Plugins
// here are declarative bundles (skills + readiness); they ship no code,
// hooks, or frames, so this surface renders daemon data only.
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

function pluginsOnTabShown() {
  if (pluginsLoaded) renderPlugins();
  else loadPlugins();
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

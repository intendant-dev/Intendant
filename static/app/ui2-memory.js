// Memory Explorer: browse/search the daemon's P1 Memory plane and
// propose claims (#tab-memory). Data flows through daemonApi (tunnel
// `api_memory_search` / `api_memory_propose`, HTTP twin fallback) and
// refreshes live on the `memory_changed` event lane. The plane is
// EPHEMERAL (the ratified P1 write bar) and the pane says so.
//
// Claim statements are quoted DATA, never instructions: everything
// renders through escapeHtml as plain text — no markdown execution,
// no HTML. Provenance (`proposed_by`) is gate-derived attribution
// from the daemon; the Explorer renders it verbatim.
//
// Search is server-side and bounded (candidates hidden by the SERVICE
// default); this owner surface opts into candidates by default —
// every claim starts as a candidate, so a curation surface that hid
// them would show an empty plane. The toggle stays visible.

let memoryClaims = null; // null = never fetched (fetch on first need)
let memoryFetchInFlight = null;
let memoryRefetchQueued = false;
let memoryLoadError = '';
let memoryQuery = '';
let memoryExpandedId = '';
let memoryDurability = 'ephemeral';

async function memoryRefresh() {
  if (memoryFetchInFlight) {
    // Coalesce: one trailing refetch picks up whatever arrived while
    // the in-flight request was running.
    memoryRefetchQueued = true;
    return memoryFetchInFlight;
  }
  memoryFetchInFlight = (async () => {
    try {
      const toggle = document.getElementById('memory-show-candidates');
      const resp = await daemonApi.request('api_memory_search', {
        q: memoryQuery,
        limit: 50,
        candidates: !toggle || toggle.checked,
      });
      if (resp.ok && resp.body && Array.isArray(resp.body.results)) {
        memoryClaims = resp.body.results;
        memoryDurability = resp.body.durability || 'ephemeral';
        memoryLoadError = '';
      } else {
        memoryLoadError = (resp.body && resp.body.error) || `memory unavailable (${resp.status})`;
      }
    } catch (e) {
      memoryLoadError = String(e && e.message || e);
    } finally {
      memoryFetchInFlight = null;
    }
    memoryRenderTab();
    if (memoryRefetchQueued) {
      memoryRefetchQueued = false;
      memoryRefresh();
    }
  })();
  return memoryFetchInFlight;
}

// Live update from the event lane. Search is filtered server-side, so
// a changed claim can't be merged locally without re-answering the
// filter — refetch (bounded + coalesced) when any surface holds data.
function memoryObserveServerMessage(d) {
  if (!d || !d.claim || !d.claim.id) return;
  if (memoryClaims === null) {
    if (memoryTabVisible()) memoryRefresh();
    return;
  }
  memoryRefresh();
}

function memoryTabVisible() {
  const pane = document.getElementById('tab-memory');
  return !!(pane && pane.classList.contains('active'));
}

function memoryOnTabShown() {
  if (memoryClaims === null) memoryRefresh();
  else memoryRenderTab();
}

function memoryFlashNote(message) {
  const note = document.getElementById('memory-tab-note');
  if (!note) return;
  note.style.display = '';
  note.textContent = message;
  setTimeout(() => {
    note.textContent = '';
    note.style.display = 'none';
  }, 6000);
}

function memoryActorLabel(p) {
  // Gate-attributed authorship (P1.2 provenance): rendered for humans,
  // data only — never markup.
  if (p.session) return `session ${p.session.slice(0, 12)}`;
  if (p.actor === 'dashboard') return 'you';
  if (p.actor === 'local_process') return 'local ctl';
  if (p.actor === 'peer') return 'a peer daemon';
  if (p.actor === 'agent_session') return 'an agent session';
  if (p.actor === 'unattributed') return 'unattributed';
  return p.principal || '';
}

function memoryProvenanceLine(claim) {
  const p = claim.proposed_by || {};
  const created = claim.created_ms
    ? new Date(claim.created_ms).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
    : '';
  const parts = [];
  if (created) parts.push(`proposed ${created}`);
  const who = memoryActorLabel(p);
  if (who) parts.push(`by ${who}`);
  return escapeHtml(parts.join(' · '));
}

function memoryDetailBlock(claim) {
  const p = claim.proposed_by || {};
  const row = (label, value) => value
    ? `<div class="memory-detail-row"><span class="memory-detail-label">${label}</span><span class="memory-detail-value">${escapeHtml(String(value))}</span></div>`
    : '';
  const created = claim.created_ms ? new Date(claim.created_ms).toLocaleString() : '';
  return `<div class="memory-item-detail">
    ${row('claim id', claim.id)}
    ${row('created', created)}
    ${row('actor', p.actor)}
    ${row('principal', p.principal)}
    ${row('actor session', p.session)}
    ${row('session context', claim.session)}
    ${row('project', claim.project)}
    ${row('model', claim.model)}
    ${row('durability', claim.durability)}
  </div>`;
}

function memoryRenderTab() {
  const list = document.getElementById('memory-tab-list');
  if (!list) return;
  const note = document.getElementById('memory-durability-note');
  if (note) {
    note.textContent = memoryDurability === 'durable'
      ? 'Durable plane — claims survive daemon restarts on this machine. Sync across machines arrives in a later phase.'
      : 'Ephemeral plane — claims live in daemon memory and vanish on restart. Durable storage runs on the primary-OS daemon.';
  }
  const counts = document.getElementById('memory-tab-counts');
  if (counts) {
    const n = memoryClaims ? memoryClaims.length : 0;
    counts.textContent = memoryClaims === null ? '' : `${n} claim${n === 1 ? '' : 's'} shown · ${memoryDurability}`;
  }
  if (memoryLoadError) {
    list.innerHTML = `<div class="ui-empty">${escapeHtml(memoryLoadError)}</div>`;
    return;
  }
  if (memoryClaims === null) {
    list.innerHTML = '<div class="ui-empty">Loading…</div>';
    return;
  }
  if (!memoryClaims.length) {
    list.innerHTML = memoryQuery
      ? '<div class="ui-empty">No claims match.</div>'
      : '<div class="ui-empty">No claims yet — propose one above, or run <code>intendant ctl memory propose</code>.</div>';
    return;
  }
  // Newest first reads best in a review list.
  const rows = memoryClaims.slice().sort((a, b) => (b.created_ms || 0) - (a.created_ms || 0)).map((claim) => {
    const labels = (claim.labels || [])
      .map((label) => `<span class="memory-chip">#${escapeHtml(label)}</span>`)
      .join('');
    const expanded = claim.id === memoryExpandedId;
    return `<div class="memory-item${expanded ? ' expanded' : ''}" data-id="${escapeHtml(claim.id)}" data-status="${escapeHtml(claim.status)}">
      <div class="memory-item-head">
        <span class="memory-status" data-status="${escapeHtml(claim.status)}">${escapeHtml(claim.status)}</span>
        <span class="memory-item-kind">${escapeHtml(claim.kind)}</span>
        <span class="memory-item-statement">${escapeHtml(claim.statement)}</span>
        <span class="memory-chip sensitivity">${escapeHtml(claim.sensitivity)}</span>${labels}
      </div>
      <div class="memory-item-foot">
        <span class="memory-item-meta">${memoryProvenanceLine(claim)}</span>
        <span class="memory-item-meta memory-item-id">${escapeHtml(claim.id.slice(0, 12))}</span>
      </div>
      ${expanded ? memoryDetailBlock(claim) : ''}
    </div>`;
  });
  list.innerHTML = rows.join('');
  // Whole-row toggle (no hover-only affordance): click/keyboard opens
  // the full provenance detail.
  list.querySelectorAll('.memory-item').forEach((el) => {
    el.addEventListener('click', () => {
      memoryExpandedId = memoryExpandedId === el.dataset.id ? '' : el.dataset.id;
      memoryRenderTab();
    });
  });
}

async function memoryProposeFromForm(button) {
  const statement = (document.getElementById('memory-add-statement') || {}).value || '';
  const kind = (document.getElementById('memory-add-kind') || {}).value || 'observation';
  const sensitivity = (document.getElementById('memory-add-sensitivity') || {}).value || 'private';
  if (!statement.trim()) return false;
  if (button) button.disabled = true;
  try {
    const resp = await daemonApi.request('api_memory_propose', {
      kind,
      statement: statement.trim(),
      sensitivity,
    });
    if (resp.ok && resp.body && resp.body.claim) {
      // The event lane refetches too; refreshing here keeps the UI
      // honest even if this tab's event socket is briefly down.
      memoryRefresh();
      return true;
    }
    memoryFlashNote((resp.body && resp.body.error) || `propose failed (${resp.status})`);
    return false;
  } catch (e) {
    memoryFlashNote(String(e && e.message || e));
    return false;
  } finally {
    if (button) button.disabled = false;
  }
}

{
  const wire = () => {
    const addBtn = document.getElementById('memory-add-btn');
    const statement = document.getElementById('memory-add-statement');
    const submit = async () => {
      const ok = await memoryProposeFromForm(addBtn);
      if (ok && statement) {
        statement.value = '';
        statement.focus();
      }
    };
    if (addBtn) addBtn.addEventListener('click', submit);
    if (statement) {
      statement.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          submit();
        }
      });
    }
    const search = document.getElementById('memory-search-input');
    if (search) {
      let debounce = 0;
      search.addEventListener('input', () => {
        clearTimeout(debounce);
        debounce = setTimeout(() => {
          memoryQuery = search.value.trim();
          memoryRefresh();
        }, 250);
      });
    }
    const toggle = document.getElementById('memory-show-candidates');
    if (toggle) toggle.addEventListener('change', () => memoryRefresh());
  };
  if (document.readyState === 'complete') wire();
  else document.addEventListener('DOMContentLoaded', wire, { once: true });
}

// Agenda: the daemon's durable ledger of parked intent (tasks, notes,
// deferred follow-ups). Two surfaces share one cache: the Agenda tab
// (#tab-agenda — list, filters, composer) and a compact card on the
// activity pane stacked under the vitals rail. Data flows through
// daemonApi (tunnel `api_agenda_list` / `api_agenda_op`, HTTP twin
// fallback) and refreshes live on the `agenda_changed` event lane.
//
// Item bodies are DATA, never instructions: everything renders through
// escapeHtml as plain quoted text — no markdown execution, no HTML.

let agendaItems = null; // null = never fetched (fetch on first need)
let agendaCounts = { open: 0, done: 0, retired: 0 };
let agendaSkippedLines = 0;
let agendaFilter = 'open';
let agendaFetchInFlight = null;
let agendaLoadError = '';

async function agendaRefresh() {
  if (agendaFetchInFlight) return agendaFetchInFlight;
  agendaFetchInFlight = (async () => {
    try {
      const resp = await daemonApi.request('api_agenda_list', {});
      if (resp.ok && resp.body && Array.isArray(resp.body.items)) {
        agendaItems = resp.body.items;
        agendaCounts = resp.body.counts || agendaCounts;
        agendaSkippedLines = resp.body.skipped_lines || 0;
        agendaLoadError = '';
      } else {
        agendaLoadError = (resp.body && resp.body.error) || `agenda unavailable (${resp.status})`;
      }
    } catch (e) {
      agendaLoadError = String(e && e.message || e);
    } finally {
      agendaFetchInFlight = null;
    }
    agendaRenderAll();
  })();
  return agendaFetchInFlight;
}

// Live update from the event lane: merge the changed item, adopt counts.
function agendaObserveServerMessage(d) {
  if (!d || !d.item || !d.item.id) return;
  if (agendaItems === null) {
    // Card/tab never fetched; only bother if either surface is live.
    if (document.getElementById('ui2-agenda-card') || agendaTabVisible()) agendaRefresh();
    return;
  }
  const at = agendaItems.findIndex((item) => item.id === d.item.id);
  if (at >= 0) agendaItems[at] = d.item;
  else agendaItems.push(d.item);
  if (d.counts) agendaCounts = d.counts;
  agendaRenderAll();
}

function agendaTabVisible() {
  const pane = document.getElementById('tab-agenda');
  return !!(pane && pane.classList.contains('active'));
}

function agendaOnTabShown() {
  if (agendaItems === null) agendaRefresh();
  else agendaRenderAll();
}

async function agendaSendOp(params, button) {
  if (button) button.disabled = true;
  try {
    const resp = await daemonApi.request('api_agenda_op', params);
    if (resp.ok && resp.body && resp.body.item) {
      // The event lane repaints too; merging here keeps the UI honest
      // even if this tab's event socket is briefly down.
      agendaObserveServerMessage({ item: resp.body.item });
      return true;
    }
    const message = (resp.body && resp.body.error) || `agenda op failed (${resp.status})`;
    agendaFlashError(message);
    return false;
  } catch (e) {
    agendaFlashError(String(e && e.message || e));
    return false;
  } finally {
    if (button) button.disabled = false;
  }
}

function agendaFlashError(message) {
  const note = document.getElementById('agenda-tab-skipped');
  if (!note) return;
  note.style.display = '';
  note.textContent = message;
  setTimeout(() => {
    note.textContent = '';
    agendaRenderAll(); // restores the skipped-lines note if one applies
  }, 6000);
}

function agendaGlyph(status) {
  if (status === 'done') return '<span class="agenda-glyph done" aria-label="done">✓</span>';
  if (status === 'retired') return '<span class="agenda-glyph retired" aria-label="retired">⊘</span>';
  return '<span class="agenda-glyph open" aria-label="open">○</span>';
}

function agendaDueChip(item) {
  if (!item.due_ms) return '';
  const due = new Date(item.due_ms);
  const overdue = item.status === 'open' && item.due_ms < Date.now();
  const label = due.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
    + (due.getHours() || due.getMinutes()
      ? ' ' + due.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })
      : '');
  return `<span class="agenda-chip due${overdue ? ' overdue' : ''}">due ${escapeHtml(label)}</span>`;
}

function agendaActorLabel(p) {
  // Gate-attributed actor (A2): kind + session/principal, rendered for
  // humans. Data only — never markup.
  if (p.session_id) return `session ${p.session_id.slice(0, 12)}`;
  if (p.kind === 'dashboard') return 'you';
  if (p.kind === 'local_process') return 'local ctl';
  if (p.kind === 'peer') return 'a peer daemon';
  if (p.kind === 'agent_session') return 'an agent session';
  return p.principal || '';
}

function agendaProvenanceLine(item) {
  const p = item.provenance || {};
  const created = p.created_ms
    ? new Date(p.created_ms).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
    : '';
  const who = agendaActorLabel(p);
  const parts = [];
  if (created) parts.push(`parked ${created}`);
  if (who) parts.push(`by ${who}`);
  return escapeHtml(parts.join(' · '));
}

function agendaActionButtons(item) {
  const actions = [];
  if (item.status === 'open') actions.push(['complete', 'Complete'], ['retire', 'Retire']);
  else if (item.status === 'done') actions.push(['reopen', 'Reopen'], ['retire', 'Retire']);
  else actions.push(['reopen', 'Reopen']);
  return actions
    .map(([op, label]) =>
      `<button type="button" class="agenda-btn" data-op="${op}" data-id="${escapeHtml(item.id)}">${label}</button>`)
    .join('');
}

function agendaRenderAll() {
  agendaRenderTab();
  agendaRenderCard();
}

function agendaRenderTab() {
  const list = document.getElementById('agenda-tab-list');
  if (!list) return;
  const counts = document.getElementById('agenda-tab-counts');
  if (counts) {
    counts.textContent =
      `${agendaCounts.open || 0} open · ${agendaCounts.done || 0} done · ${agendaCounts.retired || 0} retired`;
  }
  const note = document.getElementById('agenda-tab-skipped');
  if (note && !note.textContent) {
    if (agendaSkippedLines > 0) {
      note.style.display = '';
      note.textContent =
        `${agendaSkippedLines} history line(s) from another build are preserved but not shown.`;
    } else {
      note.style.display = 'none';
    }
  }
  if (agendaLoadError) {
    list.innerHTML = `<div class="ui-empty">${escapeHtml(agendaLoadError)}</div>`;
    return;
  }
  if (agendaItems === null) {
    list.innerHTML = '<div class="ui-empty">Loading…</div>';
    return;
  }
  const filtered = agendaItems.filter((item) =>
    agendaFilter === 'all' ? true : item.status === agendaFilter);
  if (!filtered.length) {
    const what = agendaFilter === 'all' ? '' : `${agendaFilter} `;
    list.innerHTML =
      `<div class="ui-empty">No ${what}items — park one above, or run <code>intendant ctl agenda add</code>.</div>`;
    return;
  }
  // Newest first reads best in a review list; ULIDs sort by creation.
  const rows = filtered.slice().sort((a, b) => (a.id < b.id ? 1 : -1)).map((item) => {
    const tags = (item.tags || [])
      .map((tag) => `<span class="agenda-chip">#${escapeHtml(tag)}</span>`)
      .join('');
    const body = item.body
      ? `<div class="agenda-item-body">${escapeHtml(item.body)}</div>`
      : '';
    return `<div class="agenda-item" data-status="${escapeHtml(item.status)}">
      <div class="agenda-item-head">
        ${agendaGlyph(item.status)}
        <span class="agenda-item-kind">${escapeHtml(item.kind)}</span>
        <span class="agenda-item-title">${escapeHtml(item.title)}</span>
        ${agendaDueChip(item)}${tags}
      </div>
      ${body}
      <div class="agenda-item-foot">
        <span class="agenda-item-meta">${agendaProvenanceLine(item)}</span>
        <span class="agenda-item-actions">${agendaActionButtons(item)}</span>
      </div>
    </div>`;
  });
  list.innerHTML = rows.join('');
  list.querySelectorAll('button[data-op]').forEach((btn) => {
    btn.addEventListener('click', () =>
      agendaSendOp({ op: btn.dataset.op, id: btn.dataset.id }, btn));
  });
}

// ---- Compact card on the activity pane (stacked under the vitals rail).

function agendaBuildCard() {
  const pane = document.getElementById('activity-log-pane');
  if (!pane || document.getElementById('ui2-agenda-card')) return;
  const card = document.createElement('aside');
  card.id = 'ui2-agenda-card';
  card.setAttribute('aria-label', 'Agenda');
  card.innerHTML = `
    <div class="agenda-card-head">
      <span class="agenda-card-title">Agenda</span>
      <button type="button" class="agenda-card-open" id="agenda-card-open">open</button>
    </div>
    <div class="agenda-card-list" id="agenda-card-list"><div class="agenda-card-empty">…</div></div>
    <form class="agenda-card-add" id="agenda-card-add">
      <input type="text" id="agenda-card-input" maxlength="500" placeholder="Park a task…" aria-label="Park a task" />
    </form>`;
  pane.appendChild(card);
  const open = card.querySelector('#agenda-card-open');
  if (open) open.addEventListener('click', () => routeTo('agenda'));
  const form = card.querySelector('#agenda-card-add');
  const input = card.querySelector('#agenda-card-input');
  if (form && input) {
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      const title = input.value.trim();
      if (!title) return;
      input.disabled = true;
      const ok = await agendaSendOp({ op: 'add', kind: 'task', title });
      input.disabled = false;
      if (ok) input.value = '';
      input.focus();
    });
  }
}

function agendaRenderCard() {
  const list = document.getElementById('agenda-card-list');
  if (!list) return;
  const title = document.querySelector('#ui2-agenda-card .agenda-card-title');
  if (title) {
    const open = agendaCounts.open || 0;
    title.textContent = open > 0 ? `Agenda · ${open} open` : 'Agenda';
  }
  if (agendaItems === null) {
    list.innerHTML = `<div class="agenda-card-empty">${agendaLoadError ? escapeHtml(agendaLoadError) : '…'}</div>`;
    return;
  }
  const open = agendaItems.filter((item) => item.status === 'open');
  if (!open.length) {
    list.innerHTML = '<div class="agenda-card-empty">Nothing parked.</div>';
    return;
  }
  // Oldest first: long-parked intent stays visible instead of scrolling away.
  const rows = open.slice(0, 5).map((item) => {
    const p = item.provenance || {};
    // Agent-parked items carry their session provenance right on the card.
    const who = p.session_id
      ? `<span class="agenda-card-row-who">· sess ${escapeHtml(p.session_id.slice(0, 8))}</span>`
      : '';
    return `<div class="agenda-card-row" data-id="${escapeHtml(item.id)}">
      <button type="button" class="agenda-card-done" data-id="${escapeHtml(item.id)}" aria-label="Complete">○</button>
      <span class="agenda-card-row-title" title="${escapeHtml(item.title)}">${escapeHtml(item.title)}</span>${who}
    </div>`;
  });
  const more = open.length > 5
    ? `<div class="agenda-card-more">+${open.length - 5} more…</div>`
    : '';
  list.innerHTML = rows.join('') + more;
  list.querySelectorAll('.agenda-card-done').forEach((btn) => {
    btn.addEventListener('click', () =>
      agendaSendOp({ op: 'complete', id: btn.dataset.id }, btn));
  });
  list.querySelectorAll('.agenda-card-row-title').forEach((el) => {
    el.addEventListener('click', () => routeTo('agenda'));
  });
  const moreEl = list.querySelector('.agenda-card-more');
  if (moreEl) moreEl.addEventListener('click', () => routeTo('agenda'));
}

// The vitals rail owns the top-right column; stack the card just under
// its live height (both hide together below 1180px / in grid layout).
function agendaPositionCard() {
  const card = document.getElementById('ui2-agenda-card');
  const rail = document.getElementById('ui2-vitals-rail');
  if (!card) return;
  if (!rail || !rail.offsetParent) {
    card.dataset.railHidden = '1';
    return;
  }
  delete card.dataset.railHidden;
  card.style.top = `${rail.offsetTop + rail.offsetHeight + 12}px`;
}

{
  const wire = () => {
    const filters = document.getElementById('agenda-filters');
    if (filters) {
      filters.querySelectorAll('.agenda-filter').forEach((btn) => {
        btn.addEventListener('click', () => {
          agendaFilter = btn.dataset.filter || 'open';
          filters.querySelectorAll('.agenda-filter').forEach((b) =>
            b.classList.toggle('active', b === btn));
          agendaRenderTab();
        });
      });
    }
    const addBtn = document.getElementById('agenda-add-btn');
    const addTitle = document.getElementById('agenda-add-title');
    const addKind = document.getElementById('agenda-add-kind');
    const submitAdd = async () => {
      const title = (addTitle && addTitle.value || '').trim();
      if (!title) return;
      const kind = (addKind && addKind.value) === 'note' ? 'note' : 'task';
      const ok = await agendaSendOp({ op: 'add', kind, title }, addBtn);
      if (ok && addTitle) {
        addTitle.value = '';
        addTitle.focus();
      }
    };
    if (addBtn) addBtn.addEventListener('click', submitAdd);
    if (addTitle) {
      addTitle.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          submitAdd();
        }
      });
    }
    agendaBuildCard();
    agendaRefresh();
    setInterval(agendaPositionCard, 1000);
    agendaPositionCard();
  };
  if (document.readyState === 'complete') wire();
  else document.addEventListener('DOMContentLoaded', wire, { once: true });
}

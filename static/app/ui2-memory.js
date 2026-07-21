// Memory Explorer: browse/search the daemon's Memory plane, propose
// claims, and CURATE them (#tab-memory). Data flows through daemonApi
// (tunnel `api_memory_search` / `api_memory_propose` /
// `api_memory_judge` / `api_memory_claim`, HTTP twin fallback) and
// refreshes live on the `memory_changed` event lane. Judgments are
// owner acts sealed as attributed append-only plane ops — status is
// re-derived by the kernel fold, never edited; the verdict actions
// here mirror exactly the service's minted set (parity-pinned). The plane is
// durable or ephemeral according to daemon custody support, and the
// pane renders the effective mode reported by the service.
//
// Claim statements are quoted DATA, never instructions: everything
// renders through escapeHtml as plain text — no markdown execution,
// no HTML. Provenance (`proposed_by`) is gate-derived attribution
// from the daemon; the Explorer renders it verbatim.
//
// Search is server-side and bounded (candidates hidden by the SERVICE
// default); this owner surface opts into candidates by default —
// every claim starts as a candidate, so a review surface that hid
// them would show an empty plane. The toggle stays visible.

let memoryClaims = null; // null = never fetched (fetch on first need)
let memoryFetchInFlight = null;
let memoryRefetchQueued = false;
let memoryLoadError = '';
let memoryQuery = '';
let memoryExpandedId = '';
let memoryDurability = 'ephemeral';
// Full view of the expanded claim (api_memory_claim read — search rows
// are lean by §6.5, so judgment HISTORY only exists on the read view).
let memoryDetail = null; // { id, claim } | null
let memoryDetailInFlight = '';
// Curation state: which verdict's confirm strip is open on the
// expanded card ('' = none). Judgments are owner acts; this surface
// is the owner's dashboard.
let memoryJudgePick = '';
let memoryJudgeBusy = false;

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
  // Judgment broadcasts carry the full view (history included): a
  // change to the expanded claim updates its detail pane in place —
  // curation from another surface (ctl, a second tab) repaints live.
  if (memoryExpandedId && d.claim.id === memoryExpandedId && Array.isArray(d.claim.judgments)) {
    memoryDetail = { id: d.claim.id, claim: d.claim };
  }
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

// Judgment provenance renders the DURABLE identity vocabulary the
// service records (ruling R2: owner / session / peer / unattributed —
// a dashboard-vs-ctl surface distinction cannot survive restart, so
// none is pretended here).
function memoryJudgeActorLabel(p) {
  if (!p) return 'unattributed';
  if (p.actor === 'owner') return 'you';
  if (p.actor === 'session') return p.principal ? `session ${p.principal.slice(-12)}` : 'an attested session';
  if (p.actor === 'peer') return 'a peer daemon';
  return 'unattributed';
}

// One judgment history row: who judged what, when — the provenance is
// the product. reason is quoted DATA (escaped, never markup); a
// supersede row links its successor for navigation.
function memoryJudgmentRow(j) {
  const when = j.at_ms
    ? new Date(j.at_ms).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })
    : '';
  const parts = [
    `<span class="memory-judgment-verdict" data-verdict="${escapeHtml(j.verdict)}">${escapeHtml(j.verdict)}</span>`,
    `<span class="memory-judgment-meta">by ${escapeHtml(memoryJudgeActorLabel(j.judged_by))}${when ? ` · ${escapeHtml(when)}` : ''}</span>`,
  ];
  if (j.replacement) {
    parts.push(`<a class="memory-judgment-successor" data-goto="${escapeHtml(j.replacement)}" href="#memory" title="Open the superseding claim">→ ${escapeHtml(j.replacement.slice(0, 12))}</a>`);
  }
  const reason = j.reason
    ? `<div class="memory-judgment-reason">“${escapeHtml(j.reason)}”</div>`
    : '';
  return `<div class="memory-judgment-row">${parts.join(' ')}${reason}</div>`;
}

// The curation row (owner surface): verdict actions with a CONFIRM
// affordance — picking a verdict opens a strip with an optional
// reason (DTO cap 2000) and, for supersede, the replacement picker;
// nothing seals until Confirm. Pins are deliberately absent (they are
// fail-closed at the stamped kernel — Track J finding #1).
function memoryCurationBlock(claim) {
  if (memoryJudgeBusy) {
    return '<div class="memory-curation"><span class="memory-judgment-meta">judging…</span></div>';
  }
  if (!memoryJudgePick) {
    const verb = (v) => `<button type="button" class="memory-judge-btn" data-judge="${v}">${v}</button>`;
    return `<div class="memory-curation">
      ${verb('accept')}${verb('dispute')}${verb('retire')}${verb('supersede')}
    </div>`;
  }
  const others = (memoryClaims || []).filter((c) => c.id !== claim.id);
  const replacementPicker = memoryJudgePick === 'supersede'
    ? (others.length
      ? `<select id="memory-judge-replacement" aria-label="Superseding claim">
          ${others.map((c) => `<option value="${escapeHtml(c.id)}">${escapeHtml(c.id.slice(0, 12))} · ${escapeHtml(c.statement.slice(0, 60))} (${escapeHtml(c.status)})</option>`).join('')}
        </select>
        <span class="memory-judgment-meta">supersession takes effect once the replacement is accepted</span>`
      : '<span class="memory-judgment-meta">no other claim to supersede with — propose the replacement first</span>')
    : '';
  const canConfirm = memoryJudgePick !== 'supersede' || others.length;
  return `<div class="memory-curation memory-judge-strip">
    <span class="memory-judgment-verdict" data-verdict="${escapeHtml(memoryJudgePick)}">${escapeHtml(memoryJudgePick)}</span>
    ${replacementPicker}
    <input id="memory-judge-reason" type="text" maxlength="2000"
           placeholder="Reason (optional, recorded verbatim)" aria-label="Judgment reason" />
    ${canConfirm ? '<button type="button" id="memory-judge-confirm">Confirm</button>' : ''}
    <button type="button" id="memory-judge-cancel">Cancel</button>
  </div>`;
}

function memoryDetailBlock(claim) {
  // The expanded card upgrades to the READ view (with judgment
  // history) once its fetch lands; until then the lean search row
  // renders with a loading note.
  const full = memoryDetail && memoryDetail.id === claim.id ? memoryDetail.claim : null;
  const c = full || claim;
  const p = c.proposed_by || {};
  const row = (label, value) => value
    ? `<div class="memory-detail-row"><span class="memory-detail-label">${label}</span><span class="memory-detail-value">${escapeHtml(String(value))}</span></div>`
    : '';
  const created = c.created_ms ? new Date(c.created_ms).toLocaleString() : '';
  const judgments = full
    ? `<div class="memory-judgments">
        <div class="memory-detail-label">judgments</div>
        ${(full.judgments || []).length
          ? full.judgments.map(memoryJudgmentRow).join('')
          : '<div class="memory-judgment-meta">none — every claim starts as a candidate; judging it is your act</div>'}
      </div>`
    : '<div class="memory-judgment-meta">loading history…</div>';
  return `<div class="memory-item-detail">
    ${row('claim id', c.id)}
    ${row('created', created)}
    ${row('actor', p.actor)}
    ${row('principal', p.principal)}
    ${row('actor session', p.session)}
    ${row('session context', c.session)}
    ${row('project', c.project)}
    ${row('model', c.model)}
    ${row('durability', c.durability)}
    ${judgments}
    ${memoryCurationBlock(c)}
  </div>`;
}

// Fetch the expanded claim's full read view (judgment history rides
// only single-claim views). Coalesced per id; stale ids are dropped.
async function memoryFetchDetail(id) {
  if (!id || memoryDetailInFlight === id) return;
  if (memoryDetail && memoryDetail.id === id) return;
  memoryDetailInFlight = id;
  try {
    const resp = await daemonApi.request('api_memory_claim', { id });
    if (memoryExpandedId !== id) return; // user moved on
    if (resp.ok && resp.body && resp.body.claim) {
      memoryDetail = { id, claim: resp.body.claim };
    } else {
      memoryDetail = null;
      memoryFlashNote((resp.body && resp.body.error) || `claim read failed (${resp.status})`);
    }
  } catch (e) {
    memoryFlashNote(String(e && e.message || e));
  } finally {
    if (memoryDetailInFlight === id) memoryDetailInFlight = '';
    memoryRenderTab();
  }
}

// Seal one judgment via the owner lane and report the HONEST outcome:
// a supersede whose replacement is not yet accepted is recorded but
// moves nothing (§11.2 rule 2) — the note says so instead of faking
// atomicity (ruling R4).
async function memoryJudgeConfirm(claimId) {
  const verdict = memoryJudgePick;
  if (!verdict) return;
  const reasonEl = document.getElementById('memory-judge-reason');
  const reason = reasonEl && reasonEl.value.trim() ? reasonEl.value.trim() : null;
  const replacementEl = document.getElementById('memory-judge-replacement');
  const replacement = verdict === 'supersede' && replacementEl ? replacementEl.value : null;
  memoryJudgeBusy = true;
  memoryRenderTab();
  try {
    const args = { verdict, id: claimId };
    if (reason) args.reason = reason;
    if (replacement) args.replacement = replacement;
    const resp = await daemonApi.request('api_memory_judge', args);
    if (resp.ok && resp.body && resp.body.claim) {
      const after = resp.body.claim;
      memoryDetail = { id: claimId, claim: after };
      memoryJudgePick = '';
      if (verdict === 'supersede' && after.status !== 'superseded') {
        memoryFlashNote(`supersede recorded — takes effect once ${String(replacement || '').slice(0, 12)} is accepted (status: ${after.status})`);
      } else {
        memoryFlashNote(`${verdict} recorded — status: ${after.status}`);
      }
      memoryRefresh();
    } else {
      // Named outcomes (actor-not-permitted, policy-missing, the
      // reason cap…) surface verbatim.
      memoryFlashNote((resp.body && resp.body.error) || `judge failed (${resp.status})`);
    }
  } catch (e) {
    memoryFlashNote(String(e && e.message || e));
  } finally {
    memoryJudgeBusy = false;
    memoryRenderTab();
  }
}

// Superseded → successor navigation: expand the linked claim. When
// the current filter hides it, fall back to a direct read (the detail
// pane renders from the read view, so navigation still works — the
// list highlight just has nothing to scroll to).
async function memoryGotoClaim(id) {
  memoryExpandedId = id;
  memoryDetail = null;
  memoryJudgePick = '';
  memoryFetchDetail(id);
  const present = () => document.querySelector(`.memory-item[data-id="${CSS.escape(id)}"]`);
  if (!present()) {
    // The successor sits outside the current filter — widen to the
    // unfiltered view (candidates on) so the card can render.
    memoryQuery = '';
    const search = document.getElementById('memory-search-input');
    if (search) search.value = '';
    const toggle = document.getElementById('memory-show-candidates');
    if (toggle) toggle.checked = true;
    await memoryRefresh();
  } else {
    memoryRenderTab();
  }
  const el = present();
  if (el) el.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  else memoryFlashNote(`claim ${id.slice(0, 12)} is not in the first 50 results — find it via search`);
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
  // the full provenance detail. Clicks on the card's interactive
  // curation controls must not collapse it.
  list.querySelectorAll('.memory-item').forEach((el) => {
    el.addEventListener('click', (e) => {
      if (e.target.closest('button, input, select, a, .memory-curation')) return;
      const next = memoryExpandedId === el.dataset.id ? '' : el.dataset.id;
      memoryExpandedId = next;
      memoryDetail = null;
      memoryJudgePick = '';
      memoryRenderTab();
      if (next) memoryFetchDetail(next);
    });
  });
  const expanded = memoryExpandedId && list.querySelector(`.memory-item[data-id="${CSS.escape(memoryExpandedId)}"]`);
  if (expanded) {
    expanded.querySelectorAll('[data-judge]').forEach((btn) => {
      btn.addEventListener('click', () => {
        memoryJudgePick = btn.dataset.judge;
        memoryRenderTab();
        const reason = document.getElementById('memory-judge-reason');
        if (reason) reason.focus();
      });
    });
    const confirm = expanded.querySelector('#memory-judge-confirm');
    if (confirm) confirm.addEventListener('click', () => memoryJudgeConfirm(memoryExpandedId));
    const cancel = expanded.querySelector('#memory-judge-cancel');
    if (cancel) cancel.addEventListener('click', () => {
      memoryJudgePick = '';
      memoryRenderTab();
    });
    const reason = expanded.querySelector('#memory-judge-reason');
    if (reason) reason.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        memoryJudgeConfirm(memoryExpandedId);
      }
    });
  }
  list.querySelectorAll('[data-goto]').forEach((link) => {
    link.addEventListener('click', (e) => {
      e.preventDefault();
      memoryGotoClaim(link.dataset.goto);
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

// Agenda item inspector (redesign slice A): the right-side panel with the
// full item anatomy — question answering, details, the scheduled-session
// manifest, reminder, gates, organization, references, and the thread —
// plus the schedule sheet, the preview sheet, and the reminder-policy
// popover. Renders into #ag2-inspector (built by ui2-agenda-cards.js's
// scaffold); ≥1180px it is an animated-width side panel, below that a
// fixed overlay with a backdrop (pure CSS media query — same DOM).
//
// Everything item-authored renders escaped; ask preview HTML only inside
// sandboxed srcdoc iframes (agendaHydratePreviewFrames). Ops are the
// AgendaCommand vocabulary verbatim — nothing invented.

let agendaInspWired = false;
let agendaInspEditingTitle = false;
let agendaInspTitleDraft = '';
let agendaInspEditingBody = false;
let agendaInspBodyDraft = '';
let agendaInspAdds = { blocker: false, dep: false, ref: false };
let agendaInspBlockerDraft = '';
let agendaInspAnnDraft = '';
let agendaInspTagDraft = '';
let agendaInspRefDraft = '';
let agendaInspRefMust = false;
let agendaInspRelKind = '';

// The typed-adjacency vocabulary (G2 `link_kind`) — a static mirror of
// the daemon's RELATES_TO_LINK_KINDS, pinned by a daemon-side parity
// test so a vocabulary change that forgets this file fails the suite
// instead of shipping as drift.
const AGENDA_REL_KINDS = ['duplicates', 'supersedes', 'follow_up_of', 'evidences'];
// Reading direction: a typed link reads storer → target ("A supersedes
// B" lives on A). The passive forms keep the incoming side readable.
const AGENDA_REL_KIND_LABELS = {
  duplicates: ['duplicates ▸', '◂ duplicated by'],
  supersedes: ['supersedes ▸', '◂ superseded by'],
  follow_up_of: ['follow-up of ▸', '◂ has follow-up'],
  evidences: ['evidences ▸', '◂ evidenced by'],
};

function agendaRelKindLabel(kind, outgoing) {
  const pair = AGENDA_REL_KIND_LABELS[kind];
  // Unknown kinds (a newer daemon's vocabulary) stay readable as text.
  if (!pair) return outgoing ? `${kind} ▸` : `◂ ${kind}`;
  return outgoing ? pair[0] : pair[1];
}

function agendaOpenInspector(id) {
  const opened = agendaFindItem(id);
  if (!opened) return;
  agendaSelId = id;
  // Reviewing IS looking: the revision on screen when the inspector
  // opens is the one the changed-since chip measures against.
  const st = agendaEffectState(opened);
  if (st && typeof agendaAckEffectDigest === 'function') {
    agendaAckEffectDigest(st.effect.effect_id, st.effect.digest);
  }
  agendaHoodReset(); // slice D: a fresh selection starts collapsed
  agendaInspEditingTitle = false;
  agendaInspEditingBody = false;
  agendaInspAdds = { blocker: false, dep: false, ref: false };
  agendaInspBlockerDraft = '';
  agendaInspAnnDraft = '';
  agendaInspTagDraft = '';
  agendaInspRefDraft = '';
  agendaInspRefMust = false;
  agendaRenderTab();
  agendaInspectorRender();
}

// Returns true when it actually closed something (the Escape chain).
function agendaCloseInspector() {
  if (!agendaSelId) return false;
  agendaSelId = null;
  agendaRenderTab();
  agendaInspectorRender();
  return true;
}

function agendaInspectorRender() {
  const host = document.getElementById('ag2-inspector');
  const backdrop = document.getElementById('ag2-inspector-backdrop');
  if (!host) return;
  if (!agendaInspWired) {
    agendaInspWired = true;
    host.addEventListener('click', agendaInspClick);
    host.addEventListener('input', agendaInspInput);
    host.addEventListener('change', agendaInspChange);
    host.addEventListener('keydown', agendaInspKeydown);
    if (backdrop) backdrop.addEventListener('click', () => agendaCloseInspector());
  }
  // Track AS S5: the inspector renders the FULL item (body, thread,
  // ask questions, manifests). The list feed is summaries, so the full
  // copy comes from the item-route cache — a miss renders the summary
  // degraded (loading hints where full-only sections go) and the
  // arrival repaints.
  const summary = agendaSelId ? agendaFindItem(agendaSelId) : null;
  const item = agendaSelId ? agendaFullItemFor(agendaSelId) || summary : null;
  if (!item) {
    // Selection gone (retired elsewhere, fold moved): close honestly.
    agendaSelId = null;
    host.classList.remove('open');
    if (backdrop) backdrop.hidden = true;
    host.innerHTML = '';
    return;
  }
  host.classList.add('open');
  if (backdrop) backdrop.hidden = false;
  agendaRenderPreservingFocus(host, () => {
    host.innerHTML = `<div class="ag2-insp-col">
      ${agendaInspHeaderHtml(item)}
      <div class="ag2-insp-scroll">
        ${agendaInspQuestionHtml(item)}
        ${agendaInspDetailsHtml(item)}
        ${agendaInspEffectHtml(item)}
        ${agendaInspReminderHtml(item)}
        ${agendaInspGatesHtml(item)}
        ${agendaInspOrganizationHtml(item)}
        ${agendaInspPrStateHtml(item)}
        ${agendaInspRefsHtml(item)}
        ${agendaInspThreadHtml(item)}
        ${agendaDepthCalm() ? '' : agendaHoodSectionHtml(item)}
      </div>
    </div>`;
  });
  agendaHydratePreviewFrames(host);
}

// ---- PR render join (tier 2, Track PR) ----
// Expand-time fetch-through of one anchor's live PR state: checks,
// review, mergeability — auto-fetched when the inspector opens on an
// item bearing a GitHub PR url ref (ambient context, unlike the
// deliberate-gesture drift button: this is state ABOUT the pointed-at
// thing, not a claim about a claim). Cached daemon-side behind a
// single-flight + freshness floor; every rendered state carries its
// age; unavailable renders as exactly that — the card never errors,
// and nothing here ever writes an op.

let agendaPrTier2 = {}; // item id → { status, state?, detail?, fetchedAt }
let agendaPrTier2Inflight = new Set();

function agendaPrStateEnsure(itemId) {
  const cached = agendaPrTier2[itemId];
  if (cached && Date.now() - cached.fetchedAt < 30_000) return;
  if (agendaPrTier2Inflight.has(itemId)) return;
  agendaPrTier2Inflight.add(itemId);
  daemonApi.request('api_agenda_pr_state', { item_id: itemId }).then((resp) => {
    agendaPrTier2[itemId] = {
      status: (resp && resp.status) || 'unavailable',
      state: (resp && resp.state) || null,
      detail: (resp && resp.detail) || '',
      fetchedAt: Date.now(),
    };
  }).catch((e) => {
    agendaPrTier2[itemId] = {
      status: 'unavailable',
      state: null,
      detail: String(e?.message || e),
      fetchedAt: Date.now(),
    };
  }).finally(() => {
    agendaPrTier2Inflight.delete(itemId);
    if (agendaSelId === itemId) agendaInspectorRender();
  });
}

function agendaInspPrStateHtml(item) {
  if (!agendaPrLocator(item)) return '';
  agendaPrStateEnsure(item.id);
  const row = agendaPrTier2[item.id];
  let body;
  if (!row) {
    body = '<div class="ag2-insp-sub">checking…</div>';
  } else if (row.status !== 'live' || !row.state) {
    body = `<div class="ag2-insp-sub">state unavailable${row.detail ? ` — ${escapeHtml(row.detail)}` : ''}</div>`;
  } else {
    const s = row.state;
    const bits = [];
    if (s.merged) bits.push('merged');
    else if (s.pr_state) bits.push(escapeHtml(s.pr_state));
    if (s.draft) bits.push('draft');
    if (s.mergeable === false) bits.push('conflicts');
    else if (s.mergeable === true) bits.push('mergeable');
    const checks = s.checks || {};
    if (checks.total) {
      bits.push(`checks ${checks.succeeded || 0}/${checks.total}${checks.failed ? ` (${checks.failed} failed)` : ''}`);
    }
    const review = s.review || {};
    if (review.approved) bits.push(`${review.approved} approval${review.approved === 1 ? '' : 's'}`);
    if (review.changes_requested) bits.push(`${review.changes_requested} change request${review.changes_requested === 1 ? '' : 's'}`);
    const renamed = s.title && item.title && !item.title.endsWith(s.title)
      ? `<div class="ag2-insp-sub">now titled: ${escapeHtml(s.title)}</div>` : '';
    body = `<div class="ag2-insp-sub">${bits.map(escapeHtml).join(' · ') || 'no state reported'}
      <span class="ag2-dim"> · as of ${escapeHtml(agendaRelTime(s.fetched_at_ms))}</span></div>${renamed}`;
  }
  return `<div class="ag2-insp-section" data-pr-state>
    <div class="ag2-insp-sechead">Pull request</div>
    ${body}
  </div>`;
}

window.qa = Object.assign(window.qa || {}, {
  agendaPrJoin: () => ({
    tier1Locators: Object.keys(agendaPullRequests || {}).length,
    tier2Rows: Object.keys(agendaPrTier2 || {}).map((id) => ({
      id,
      status: agendaPrTier2[id].status,
      ageMs: Date.now() - agendaPrTier2[id].fetchedAt,
    })),
    inflight: agendaPrTier2Inflight.size,
  }),
  // Decision-card readback + driver for the QA harness (`--probe-json` /
  // `--wait-for-function`). Fragments are scope-wrapped, so harness code
  // cannot reach the agenda functions directly — the optional `drive`
  // argument is the closure-side driver (the station.activate pattern):
  // `{route:true}` opens the Agenda tab, `{open:id}` the inspector,
  // `{readRef:locator}` the ref reader (idempotent while that locator's
  // sheet is up), `{closeReader:true}` closes a stale sheet first.
  // Returns the readback: the open item's structured options, surfaced
  // recommendations, and the ref-reader sheet's state.
  agendaDecisionCard: (drive) => {
    drive = drive || {};
    if (drive.route && typeof routeTo === 'function') routeTo('agenda');
    if (drive.open && agendaSelId !== drive.open && agendaFindItem(drive.open)) {
      agendaOpenInspector(drive.open);
    }
    if (drive.closeReader && agendaSheetState && agendaSheetState.kind === 'refread') {
      agendaSheetClose();
    }
    if (drive.readRef && agendaSelId
      && !(agendaSheetState && agendaSheetState.kind === 'refread'
        && agendaSheetState.locator === drive.readRef)) {
      agendaOpenRefReader(agendaSelId, drive.readRef);
    }
    const item = agendaInspItem();
    if (!item) return null;
    const sheet = agendaSheetState && agendaSheetState.kind === 'refread'
      ? {
        locator: agendaSheetState.locator,
        loading: !!agendaSheetState.loading,
        error: agendaSheetState.error || null,
        source: (agendaSheetState.data && agendaSheetState.data.source) || null,
        drift: (agendaSheetState.data && agendaSheetState.data.drift) || null,
        size: (agendaSheetState.data && agendaSheetState.data.size) || 0,
      }
      : null;
    return {
      id: item.id,
      optionLabels: ((item.ask && item.ask.questions) || [])
        .map((q) => (q.options || []).map((o) => o.label)),
      recommendedPills: document.querySelectorAll('#ag2-inspector .ag2-pill.rec').length,
      recommendations: agendaBodyRecommendations(item.body || '').map((r) => r.text),
      answerDraft: agendaQaDrafts[item.id] || '',
      openableFileRefs: document.querySelectorAll('#ag2-inspector [data-open-ref]').length,
      refReader: sheet,
    };
  },
  // Fireability readback + driver (card 01KYSZAGQVHAAYS7BK9H3QFM3C QA
  // leg): per-item effect state as the cards judge it (kind, the served
  // refusal, which affordances the DOM actually offers), plus the
  // schedule sheet's focus state. Drivers: `{route:true}` opens the tab,
  // `{sched:id}` the schedule sheet, `{reschedule:id}` fires the
  // missed-card one-tap (async — poll the readback for the outcome).
  agendaFireability: (drive) => {
    drive = drive || {};
    if (drive.route && typeof routeTo === 'function') routeTo('agenda');
    if (drive.sched && agendaFindItem(drive.sched)) {
      agendaOpenSchedSheet(drive.sched);
    }
    if (drive.reschedule) {
      const btn = document.querySelector(
        `[data-resched-effect="${(window.CSS && CSS.escape) ? CSS.escape(drive.reschedule) : drive.reschedule}"]`
      );
      agendaRescheduleMissed(drive.reschedule, btn || undefined);
    }
    const effects = (agendaItems || []).flatMap((row) => {
      const item = agendaFullItemFor(row.id) || row;
      const st = agendaEffectState(item);
      if (!st) return [];
      const sel = (window.CSS && CSS.escape) ? CSS.escape(item.id) : item.id;
      return [{
        id: item.id,
        kind: st.kind,
        refusal: st.effect.fireability_refusal || null,
        hasApprove: !!document.querySelector(`[data-op-btn="approve_effect"][data-id="${sel}"]`),
        hasDecline: !!document.querySelector(`[data-op-btn="withdraw_effect"][data-id="${sel}"]`),
        hasReschedule: !!document.querySelector(`[data-resched-effect="${sel}"]`),
        hasFixPlan: !!document.querySelector(`[data-edit-sched="${sel}"][data-focus]:not([data-focus=""])`),
      }];
    });
    const s = agendaSheetState;
    return {
      effects,
      sheet: s && (s.kind === 'sched' || s.kind === 'sched-loading')
        ? {
          kind: s.kind,
          itemId: s.itemId,
          focusField: s.focusField || '',
          error: s.error || '',
          projectRequired: !!document.querySelector('#ag2-sheet .ags-hint-required'),
        }
        : null,
    };
  },
});

// ---- Header ----

function agendaInspHeaderHtml(item) {
  const statusTone = item.status === 'open' ? 'iris' : item.status === 'done' ? 'green' : 'neutral';
  const p = item.provenance || {};
  const s = agendaSessionInfo(p.session_id);
  const byHtml = agendaActorHtml(p);
  const title = agendaInspEditingTitle
    ? `<div class="ag2-insp-titleedit">
        <input type="text" id="ag2-insp-title-input" data-fkey="insp-title" maxlength="500"
               value="${escapeHtml(agendaInspTitleDraft)}" aria-label="Title" />
        <button type="button" class="ag2-btn" data-act="title-save">Save</button>
      </div>`
    : `<div class="ag2-insp-title" data-act="title-edit" title="Click to edit the title">${escapeHtml(item.title)}</div>`;
  const acts = [];
  const A = (label, act, cls, tip) =>
    acts.push(`<button type="button" class="ag2-btn ${cls || ''}" data-act="${act}"${tip ? ` title="${escapeHtml(tip)}"` : ''}>${escapeHtml(label)}</button>`);
  if (item.status === 'open') {
    A('Start a session', 'start', 'prim',
      'Opens the confirm sheet — review exactly what the spawn runs with');
    const sid = agendaFollowUpSid(item);
    if (sid) {
      A('Follow up', 'follow-live', '',
        'The origin conversation is live — open the composer targeted at it with this item quoted');
    } else if (agendaFollowUpResumable(item)) {
      A('Follow up (resumes session)', 'follow-resume', '',
        'The origin conversation has ended — resume it (same conversation, its recorded project) with this item quoted');
    }
    if (item.kind === 'question') {
      A('Close unanswered', 'complete', 'ghost',
        'Completes without an answer — the asker is told the outcome');
    } else {
      A('Mark done', 'complete');
    }
    A('Retire', 'retire', 'danger',
      'Hides from open lenses — never deletes; history is append-only');
  } else {
    A('Reopen', 'reopen', '',
      item.kind === 'question'
        ? 'Re-asks — clears the current reply view; the log keeps every reply' : '');
    if (item.status === 'done') A('Retire', 'retire', 'danger');
  }
  return `<div class="ag2-insp-head">
    <div class="ag2-insp-chips">
      <span class="ag2-kind">${escapeHtml(item.kind)}</span>
      ${agendaChipHtml(item.status, statusTone)}
      <button type="button" class="ag2-idbtn" data-act="copy-id" title="Copy the full item id">${escapeHtml(item.id.slice(0, 6).toLowerCase())}</button>
      <span class="ag2-spacer"></span>
      <button type="button" class="ag2-x" data-act="close" title="Close — esc">×</button>
    </div>
    ${title}
    <div class="ag2-insp-ctx">
      <span>parked ${escapeHtml(agendaRelTime(p.created_ms))}${byHtml ? ' by' : ''}</span>
      ${byHtml}
      <span>· updated ${escapeHtml(agendaRelTime(item.updated_ms))}</span>
    </div>
    <div class="ag2-insp-actions">${acts.join('')}</div>
  </div>`;
}

// ---- Question section ----

function agendaInspQuestionHtml(item) {
  if (item.kind !== 'question') return '';
  const id = escapeHtml(item.id);
  const questions = (item.ask && item.ask.questions) || [];
  const blocks = questions.map((q, qi) => {
    const picks = agendaQaPicks(item.id, qi);
    const noteKey = `${item.id}:${qi}`;
    const noteRow = item.status === 'open' && picks.length && (q.previews || []).length
      ? `<input type="text" class="ag2-insp-notein" data-qa-note="${escapeHtml(noteKey)}" data-fkey="note-${qi}"
           placeholder="Note anchored to “${escapeHtml(picks[0])}” — travels with the structured answer (optional)…"
           value="${escapeHtml(agendaQaNotes[noteKey] || '')}" />`
      : '';
    return `<div class="ag2-insp-q">
      <div class="ag2-insp-qtext">${escapeHtml(q.question)}</div>
      ${agendaQaPillsHtml(item, qi)}
      ${agendaPreviewStripHtml(item, qi, 'insp')}
      ${(q.previews || []).length ? '<div class="ag2-hint">rendered previews from the ask’s blob store — sandboxed, quoted; the blobs are the agent’s renders, never live pages</div>' : ''}
      ${noteRow}
    </div>`;
  }).join('');
  let composer = '';
  if (item.status === 'open') {
    const hasAsk = questions.length > 0;
    const draft = agendaQaDrafts[item.id] || '';
    // Explicit recommendations surfaced from the prose body (decision-card
    // UX): a highlighted line with a one-click answer prefill, so the
    // disposition the body describes stops being buried mid-paragraph.
    // Structured asks carry theirs as a "(Recommended)" option instead.
    const recs = agendaBodyRecommendations(item.body || '');
    const recStrip = recs.map((rec) => `<div class="ag2-insp-rec">
        <span class="ag2-rec-chip">${escapeHtml(rec.kind)}</span>
        <span class="ag2-rec-text">${escapeHtml(rec.text)}</span>
        <button type="button" class="ag2-btn ag2-rec-use" data-rec-use="${escapeHtml(rec.text)}"
                title="Prefill the answer box with this — you still send it">Use as answer</button>
      </div>`).join('');
    const railDoor = hasAsk
      ? `<button type="button" class="ag2-linkbtn" data-act="rail-open">Open on the question rail ›</button>`
      : '';
    const note = hasAsk
      ? `Rich ask #${item.ask.ask_id} — parked on every dashboard’s question rail; answering here resolves it everywhere. Nothing blocks, nothing expires.`
      : `Parked ${agendaRelTime((item.provenance || {}).created_ms)} — the answer lands on the item; a live asking session hears it, and an ended one’s successor reads it at session start.`;
    composer = `${hasAsk ? '' : `<div class="ag2-insp-qtext">${escapeHtml(item.title)}</div>`}
      ${recStrip}
      <div class="ag2-qa-row">
        <input type="text" class="ag2-qa-input" maxlength="4000" data-qa-draft="${id}" data-fkey="insp-qa"
               placeholder="${hasAsk ? 'Add a note with your pick (optional)…' : 'Type your answer…'}"
               aria-label="Answer" value="${escapeHtml(draft)}" />
        <button type="button" class="ag2-btn prim" data-answer="${id}">Answer</button>
      </div>
      <div class="ag2-hint">${escapeHtml(note)} ${railDoor}</div>`;
  }
  let resolved = '';
  if (item.answer) {
    const a = item.answer;
    const rows = [];
    if (a.structured) {
      const sByQ = a.structured;
      questions.forEach((q) => {
        const picks = (sByQ.selections || {})[q.question] || [];
        const followup = (sByQ.followups || {})[q.question];
        const notes = ((sByQ.annotations || {})[q.question] || []);
        if (!picks.length && followup === undefined && !notes.length
          && !((sByQ.answers || {})[q.question])) return;
        rows.push(`<div class="ag2-insp-resq">
          <div class="ag2-insp-resq-q">${escapeHtml(q.question)}</div>
          <div class="ag2-insp-resq-picks">
            ${picks.map((p) => `<span class="ag2-respick">${escapeHtml(p)}</span>`).join('')}
            ${followup !== undefined ? `<span class="ag2-restext">${escapeHtml(followup)}</span>` : ''}
          </div>
          ${notes.map((n) => `<div class="ag2-resnote">anchored to “${escapeHtml(n.preview)}”: ${escapeHtml(n.note)}</div>`).join('')}
        </div>`);
      });
    }
    if (!rows.length && a.text) {
      rows.push(`<div class="ag2-insp-resq"><span class="ag2-restext">${escapeHtml(a.text)}</span></div>`);
    }
    const who = agendaActorLabel(a) || 'unattributed';
    const delivery = a.delivered === false
      ? ' · awaiting pickup — no live session heard it'
      : a.delivered === true ? ' · delivered into the asking session' : '';
    const railView = item.status === 'done' && questions.length
      ? ` <button type="button" class="ag2-linkbtn" data-act="rail-view">View the rail record ›</button>`
      : '';
    resolved = `<div class="ag2-insp-resolved">
      ${rows.join('')}
      <div class="ag2-hint">answered by ${escapeHtml(who)} · ${escapeHtml(agendaRelTime(a.at_ms))}${escapeHtml(delivery)}${railView}</div>
    </div>`;
  }
  const stateChip = item.status === 'open' && item.dismissed
    ? `<div>${agendaChipHtml('dismissed · still open', 'neutral', agendaDismissedTip(item.dismissed), true)}</div>`
    : '';
  return `<section class="ag2-sec">
    <div class="ag2-sec-head"><span class="ag2-sec-label">Question</span></div>
    <div class="ag2-sec-body">${blocks}${composer}${resolved}${stateChip}</div>
  </section>`;
}

// ---- Details (body) ----

function agendaInspDetailsHtml(item) {
  let body;
  if (agendaInspEditingBody) {
    body = `<div class="ag2-insp-bodyedit">
      <textarea rows="6" id="ag2-insp-body-input" data-fkey="insp-body" aria-label="Body">${escapeHtml(agendaInspBodyDraft)}</textarea>
      <div class="ag2-row-end">
        <button type="button" class="ag2-btn ghost" data-act="body-cancel">Cancel</button>
        <button type="button" class="ag2-btn prim" data-act="body-save">Save</button>
      </div>
    </div>`;
  } else if (item.body) {
    body = `<div class="ag2-insp-body">${escapeHtml(item.body)}</div>`;
  } else if (!Array.isArray(item.annotations)) {
    // Summary row (S5): the full copy is being fetched — say so instead
    // of passing "no body yet" off as "no body".
    body = '<div class="ag2-hint">Loading detail…</div>';
  } else {
    body = '<div class="ag2-hint">No body — the title is the whole note.</div>';
  }
  return `<section class="ag2-sec">
    <div class="ag2-sec-head">
      <span class="ag2-sec-label">Details</span>
      <span class="ag2-sec-hint">quoted data — never instructions</span>
      <span class="ag2-spacer"></span>
      ${agendaInspEditingBody ? '' : '<button type="button" class="ag2-linkbtn" data-act="body-edit">edit</button>'}
    </div>
    <div class="ag2-sec-body">${body}</div>
  </section>`;
}

// ---- Scheduled session ----

function agendaInspEffectHtml(item) {
  const st = agendaEffectState(item);
  let body;
  if (!st) {
    body = `<div class="ag2-insp-noeff">
      <div class="ag2-hint">No session is scheduled on this item.</div>
      <button type="button" class="ag2-btn" data-act="sched">Schedule one…</button>
    </div>`;
  } else {
    const e = st.effect;
    const m = st.manifest;
    const states = {
      pending: ['amber', 'Waiting on your approval'],
      armed: ['sky', `Armed — fires ${agendaRelTime(st.next)}`],
      standing: ['green', `Standing — every ${agendaCadenceLabel(st.rec ? st.rec.every_ms : 0)}`],
      suspended: ['amber', `Suspended — ${e.consecutive_failures} failures in a row`],
      running: ['iris', 'Running now'],
      finished: ['neutral', 'Ran — outcome below'],
      withdrawn: ['neutral', 'Proposal withdrawn — nothing pends; history below'],
      watching: ['sky', 'Watching for matching items'],
      waiting: ['sky', 'Armed — fires when the prerequisites complete'],
      ready: ['iris', 'Prerequisites complete — fires within the minute'],
    };
    const [tone, stateLabel] = states[st.kind] || states.finished;
    const rows = [];
    const R = (k, v, mono) =>
      rows.push(`<div class="ag2-eff-k">${escapeHtml(k)}</div><div class="ag2-eff-v${mono ? ' mono' : ''}">${escapeHtml(v)}</div>`);
    if (st.trig) {
      R('fires', st.trig.kind === 'on_item_match'
        ? `when a NEW open ${st.trig.item_kind || 'item'} carries ${(st.trig.tags || []).length ? `tags ${st.trig.tags.join(', ')}` : 'the matched shape'} — arrivals batch for a minute`
        : 'the moment every prerequisite completes — no clock involved');
      if (!agendaDepthCalm()) {
        R('guardrails', `arrivals coalesce · one occurrence in flight · suspends after ${st.threshold} failures in a row`);
      }
    } else {
      R('when', st.rec
        ? `every ${agendaCadenceLabel(st.rec.every_ms)} · next ${agendaAbsTime(st.next)}`
        : `${agendaAbsTime(m.fire_at_ms)} (${agendaRelTime(m.fire_at_ms)})`);
    }
    if (st.rec && (st.rec.until_ms || st.rec.max_occurrences)) {
      R('ends', `${st.rec.until_ms ? agendaAbsTime(st.rec.until_ms) : ''}${st.rec.max_occurrences ? `${st.rec.until_ms ? ' or ' : ''}after ${st.rec.max_occurrences} runs` : ''}`);
    }
    R('shape', `${m.interactive ? 'interactive — opens and waits for you' : 'goal run — autonomous, writes back'} · ${m.orchestrate ? 'orchestrated' : 'direct'}`);
    // Manifest plumbing rows fold away at calm depth; the sealed facts
    // stay one depth notch (or the raw sheet) away.
    if (!agendaDepthCalm()) {
      R('project', m.project_root || 'inherited at fire time: parking session’s root, else daemon default', !!m.project_root);
      const cfg = m.agent_config || null;
      R('config', cfg
        ? `${[cfg.agent, ...Object.entries(cfg).filter(([k]) => k !== 'agent').map(([, v]) => v)].filter(Boolean).join(' · ')} — explicit, recorded on the manifest`
        : 'inherits daemon defaults (Settings → reasoning)', !!cfg);
    }
    if (st.rec) {
      R('on failure', `suspend after ${st.threshold} failed runs in a row — surfaced, never silently re-fired`);
    }
    let lastRun = '';
    if (e.last_run) {
      const run = e.last_run;
      const runTone = run.state === 'completed' ? 'green'
        : run.state === 'failed' ? 'rose'
          : run.state === 'started' ? 'iris' : 'amber';
      const s = run.session_id && agendaSessionInfo(run.session_id);
      const sessionLink = s && s.key
        ? ` <a class="ag2-linkbtn" data-jump-session="${escapeHtml(s.key)}">view session ›</a>`
        : '';
      const attempt = (e.last_run_attempt || 0) > 0
        ? ` · attempt ${e.last_run_attempt} (auto-retry after failure)` : '';
      // The self-report axis (Track AO): rendered as its own labeled
      // block BESIDE the transport verdict — never sharing its glyph
      // or palette. The transport note above it stays the transport's
      // last-words line (R6), tooltip included.
      const att = run.attestation || null;
      let attestHtml = '';
      if (att) {
        const refs = (att.refs || []).map((r) => `<div class="ag2-attest-ref">
            <span class="ag2-attest-ref-loc">${escapeHtml(r.locator)}</span>
            <span class="ag2-hint" title="The pin the session stated at attest time — hash-verified at intake, re-checked on demand; a pointer, never inlined content">pin ${escapeHtml((r.sha256 || '').slice(0, 8))}</span>
            <span class="agenda-ref-drift" data-item="${escapeHtml(item.id)}" data-locator="${escapeHtml(r.locator)}"></span>
          </div>`).join('');
        attestHtml = `<div class="ag2-eff-attest">
          <div class="ag2-eff-lastrun-head">
            ${agendaAttestChipHtml(att)}
            <span class="ag2-hint">${escapeHtml(agendaRelTime(att.at_ms))}</span>
          </div>
          ${att.note ? `<div class="ag2-eff-attest-note">${escapeHtml(att.note)}</div>` : ''}
          ${refs}
        </div>`;
      } else if (run.state !== 'started') {
        attestHtml = `<div class="ag2-eff-attest">${agendaUnattestedChipHtml()}</div>`;
      }
      lastRun = `<div class="ag2-eff-lastrun">
        <div class="ag2-eff-lastrun-head">
          ${agendaChipHtml(`last run · ${run.state}`, runTone)}
          <span class="ag2-hint">${escapeHtml(agendaDepthCalm() ? agendaRelTime(run.at_ms) : `${agendaRelTime(run.at_ms)} · occurrence ${run.occurrence_id}${attempt}`)}</span>
          ${sessionLink}
        </div>
        ${run.note ? `<div class="ag2-eff-note" title="The session’s final message as the run ended — the transport record, not a self-report">${escapeHtml(run.note)}</div>` : ''}
        ${attestHtml}
      </div>`;
    }
    const acts = [];
    const A = (label, act, cls, tip) =>
      acts.push(`<button type="button" class="ag2-btn ${cls || ''}" data-act="${act}"${tip ? ` title="${escapeHtml(tip)}"` : ''}>${escapeHtml(label)}</button>`);
    if (st.kind === 'pending') {
      A('Approve this exact plan', 'eff-approve', 'prim',
        `Binds digest ${agendaShortDigest(e.digest)} — any edit voids it`);
      A('Edit schedule…', 'sched');
      A('Decline', 'eff-withdraw', 'danger',
        'Withdraws this proposal — it stops asking for approval; the item and its history stay. Propose again anytime');
    } else if (['armed', 'watching', 'waiting', 'ready'].includes(st.kind)) {
      A('Edit (voids approval)', 'sched');
      A('Revoke approval', 'eff-revoke', 'danger', 'Instant, owner-surface only');
    } else if (st.kind === 'standing') {
      A('Run now', 'eff-run-now', '',
        'One extra occurrence of the approved digest — within the reviewed decision, no new ceremony');
      A('Edit (voids approval)', 'sched');
      A('Revoke', 'eff-revoke', 'danger');
    } else if (st.kind === 'suspended') {
      A('Re-approve to re-arm', 'eff-approve', 'prim',
        'Same digest, one click — resets the failure streak');
      A('Revoke', 'eff-revoke', 'danger');
    } else if (st.kind === 'running') {
      // In-flight: nothing to arm or edit mid-run; the last-run row links
      // the session.
    } else {
      A('Schedule again…', 'sched');
    }
    // The digest chip stays at every depth — it is what the Approve
    // gesture signs (bound: what the recorded approval covers); a
    // re-proposed revision visibly carries its NEW digest here.
    const bound = !!(e.approval && e.approval.digest === e.digest);
    body = `<div class="ag2-effcard t-${tone}">
      <div class="ag2-effcard-head">
        <span class="ag2-eff-dot"></span>
        <span class="ag2-eff-state">${escapeHtml(stateLabel)}</span>
        <span class="ag2-spacer"></span>
        ${agendaEffectRevisionChipHtml(e)}
        ${agendaDigestChipHtml(bound ? e.approval.digest : e.digest,
    bound ? 'Your recorded approval covers exactly this manifest revision'
      : e.approval ? 'Re-proposed since your last approval — the NEW revision Approve would bind'
        : 'The manifest revision Approve would bind',
    agendaDigestPulseClass(e.effect_id))}
        <span class="ag2-hint mono">${bound ? 'approved' : 'unapproved'}</span>
      </div>
      <div class="ag2-eff-grid">${rows.join('')}</div>
      ${typeof agendaSealsStripHtml === 'function' ? agendaSealsStripHtml(item) : ''}
      ${agendaDepthCalm() ? '' : `<div class="ag2-eff-goal">${escapeHtml(m.goal || '')}</div>`}
      ${lastRun}
      ${acts.length ? `<div class="ag2-insp-actions">${acts.join('')}</div>` : ''}
    </div>`;
  }
  return `<section class="ag2-sec">
    <div class="ag2-sec-head">
      <span class="ag2-sec-label">Scheduled session</span>
      <span class="ag2-sec-hint">nothing fires without your approval of the exact plan</span>
    </div>
    <div class="ag2-sec-body">${body}</div>
  </section>`;
}

// ---- Reminder ----

function agendaInspReminderHtml(item) {
  const overdue = item.due_ms && item.due_ms < Date.now();
  const dueChip = item.due_ms
    ? agendaChipHtml(`${overdue ? 'overdue — was ' : ''}${agendaAbsTime(item.due_ms)} · ${agendaRelTime(item.due_ms)}`,
      overdue ? 'amber' : 'sky')
    : agendaChipHtml('no reminder', 'neutral');
  const policy = agendaReminderPolicy;
  const urgency = agendaItemUrgency(item.id);
  const defaultUrgency = (policy && policy.default_urgency) || 'attention';
  const options = ['default', 'mute', 'info', 'attention', 'urgent'].map((v) => {
    const label = v === 'default' ? `default (${defaultUrgency})` : v;
    return `<option value="${v}"${v === urgency ? ' selected' : ''}>${label}</option>`;
  }).join('');
  const minToHhmm = (min) =>
    `${String(Math.floor(min / 60)).padStart(2, '0')}:${String(min % 60).padStart(2, '0')}`;
  const note = !policy ? ''
    : !policy.enabled
      ? 'Reminders are globally off — nothing fires until you re-enable them.'
      : `Delivery follows your policy: ${policy.quiet_hours ? `quiet hours ${minToHhmm(policy.quiet_hours.start_min)}–${minToHhmm(policy.quiet_hours.end_min)} defer everything; ` : ''}anything staler than ${policy.staleness_hours}h folds into a digest. Completing or retiring cancels the pending reminder.`;
  return `<section class="ag2-sec">
    <div class="ag2-sec-head">
      <span class="ag2-sec-label">Reminder</span>
      <span class="ag2-sec-hint">a reminder notifies you — it never authorizes work</span>
    </div>
    <div class="ag2-sec-body">
      <div class="ag2-insp-remrow">
        ${dueChip}
        <select data-act-change="due-preset" aria-label="Change the reminder">
          <option value="">Change…</option>
          <option value="3h">In 3 hours</option>
          <option value="eve">This evening 18:00</option>
          <option value="tom">Tomorrow 09:00</option>
          <option value="mon">Next Monday 09:00</option>
          <option value="clear">No reminder</option>
        </select>
        <span class="ag2-spacer"></span>
        <span class="ag2-hint">loudness</span>
        <select data-act-change="urgency" aria-label="Reminder loudness"
                title="Per-item override on your reminder policy (settings.manage — an agenda.write grant can’t raise its own loudness)">${options}</select>
      </div>
      ${note ? `<div class="ag2-hint">${escapeHtml(note)}</div>` : ''}
    </div>
  </section>`;
}

// ---- Blocked on (blockers + prerequisites) ----

function agendaInspGatesHtml(item) {
  const id = escapeHtml(item.id);
  const blockers = (item.blockers || []).map((b) => {
    const meta = [
      b.cleared ? `cleared ${agendaRelTime(b.cleared.at_ms)} by ${agendaActorLabel(b.cleared) || 'unattributed'}` : '',
      `set ${agendaRelTime(b.set_ms)} by ${agendaActorLabel(b) || 'unattributed'}`,
      'nothing evaluates this; people do',
    ].filter(Boolean).join(' · ');
    const clear = !b.cleared && item.status === 'open'
      ? `<button type="button" class="ag2-btn" data-clear-blocker="${escapeHtml(b.blocker_id)}"
           title="Clearing is an op, never a deletion — the entry stays as history">Clear</button>`
      : '';
    return `<div class="ag2-insp-blocker${b.cleared ? ' cleared' : ''}">
      <span class="ag2-insp-bdot"></span>
      <div class="ag2-insp-bmain">
        <div class="ag2-insp-btext">${escapeHtml(b.criterion)}</div>
        <div class="ag2-hint">${escapeHtml(meta)}</div>
      </div>
      ${clear}
    </div>`;
  });
  // The shared per-prerequisite judgment (ui2-agenda.js): live status
  // on every link — the delivered-awaiting-Complete distinction, the
  // in-flight word, and the honest out-of-window degrade (an absent
  // target on an unblocked item is provably done, never "missing").
  const deps = agendaPrereqStates(item).map((p) => {
    const go = p.kind === 'delivered'
      ? `<button type="button" class="ag2-btn ghost ag2-blk-go" data-open-item="${escapeHtml(p.id)}"
          title="Opens the delivered prerequisite — its Mark done is the tap that releases this wait">Review &amp; complete ›</button>`
      : '';
    return `<div class="ag2-insp-dep${p.kind === 'delivered' ? ' delivered' : ''}">
      ${agendaChipHtml(p.status, p.tone, p.detail)}
      ${p.target
    ? `<a class="ag2-insp-deplink" data-open-item="${escapeHtml(p.id)}">waits on “${escapeHtml(p.title)}”</a>`
    : `<span class="ag2-insp-deplink">waits on ${escapeHtml(p.title)}</span>`}
      ${go}
      <button type="button" class="ag2-x" data-remove-dep="${escapeHtml(p.id)}" title="Drop the link (the log keeps history)">×</button>
    </div>`;
  });
  const empty = !blockers.length && !deps.length
    ? '<div class="ag2-hint">Nothing gates this item.</div>' : '';
  // Blockers and dependency links describe OPEN work — the daemon refuses
  // them elsewhere, so the affordances only exist there.
  if (item.status !== 'open') {
    return `<section class="ag2-sec">
      <div class="ag2-sec-head">
        <span class="ag2-sec-label">Blocked on</span>
        <span class="ag2-sec-hint">stated criteria &amp; prerequisites — nothing evaluates them; people do</span>
      </div>
      <div class="ag2-sec-body">${blockers.join('')}${deps.join('')}${empty}</div>
    </section>`;
  }
  const desc = agendaDescendantIds(item.id);
  const depOptions = (agendaItems || [])
    .filter((x) => x.id !== item.id && x.status === 'open' && !desc.has(x.id)
      && !(item.relies_on || []).some((d) => d.target_id === x.id))
    .map((x) => `<option value="${escapeHtml(x.id)}">${escapeHtml(x.title.slice(0, 46))}</option>`)
    .join('');
  const blockerAdd = agendaInspAdds.blocker
    ? `<div class="ag2-insp-addrow">
        <input type="text" data-fkey="blocker-add" data-draft="blocker" maxlength="4000"
               placeholder="A human criterion — e.g. “api access granted”; nothing will evaluate it"
               value="${escapeHtml(agendaInspBlockerDraft)}" aria-label="Blocker criterion" />
        <button type="button" class="ag2-btn" data-act="blocker-add">Set blocker</button>
      </div>` : '';
  const depAdd = agendaInspAdds.dep
    ? `<div class="ag2-insp-addrow">
        <select data-act-change="dep-add" aria-label="This item waits on">
          <option value="">This item waits on…</option>${depOptions}
        </select>
      </div>` : '';
  return `<section class="ag2-sec">
    <div class="ag2-sec-head">
      <span class="ag2-sec-label">Blocked on</span>
      <span class="ag2-sec-hint">stated criteria &amp; prerequisites — nothing evaluates them; people do</span>
    </div>
    <div class="ag2-sec-body">
      ${blockers.join('')}${deps.join('')}${empty}
      <div class="ag2-insp-addbtns">
        <button type="button" class="ag2-dashbtn" data-act="toggle-blocker-add">+ state a blocker</button>
        <button type="button" class="ag2-dashbtn" data-act="toggle-dep-add">+ add a prerequisite</button>
      </div>
      ${blockerAdd}${depAdd}
    </div>
  </section>`;
}

// ---- Organization (placement, see-also, tags) ----

function agendaInspOrganizationHtml(item) {
  const desc = agendaDescendantIds(item.id);
  const others = (agendaItems || []).filter((x) => x.id !== item.id && x.status !== 'retired');
  const placeOptions = others
    .filter((x) => !desc.has(x.id))
    .sort((a, b) => agendaChildrenOf(b.id).length - agendaChildrenOf(a.id).length)
    .map((x) => {
      const hub = agendaChildrenOf(x.id).length ? '▣ ' : '';
      const selected = item.part_of && item.part_of.parent_id === x.id ? ' selected' : '';
      return `<option value="${escapeHtml(x.id)}"${selected}>${hub}${escapeHtml(x.title.slice(0, 46))}</option>`;
    }).join('');
  const relEdges = agendaRelationEdges(item);
  const partners = new Set(relEdges.map((edge) => edge.pid));
  const rels = relEdges.map(({ pid, kind, outgoing }) => {
    const target = agendaFindItem(pid);
    if (!target) return '';
    const kindChip = kind
      ? `<span class="ag2-relkind">${escapeHtml(agendaRelKindLabel(kind, outgoing))}</span>`
      : '';
    return `<span class="ag2-relchip">
      ${kindChip}<a data-open-item="${escapeHtml(pid)}">${escapeHtml(target.title.slice(0, 34))}</a>
      <button type="button" class="ag2-x" data-remove-rel="${escapeHtml(pid)}" title="Remove the link (the log keeps history; re-add to change its kind)">×</button>
    </span>`;
  }).join('');
  const relKindOptions = ['', ...AGENDA_REL_KINDS].map((kind) => {
    const selected = kind === agendaInspRelKind ? ' selected' : '';
    const label = kind ? kind.replace(/_/g, ' ') : 'see-also';
    return `<option value="${kind}"${selected}>${label}</option>`;
  }).join('');
  const relOptions = others
    .filter((x) => !partners.has(x.id))
    .map((x) => `<option value="${escapeHtml(x.id)}">${escapeHtml(x.title.slice(0, 46))}</option>`)
    .join('');
  const tags = (item.tags || []).map((t) =>
    `<span class="ag2-tagchip">${escapeHtml(t)}<button type="button" class="ag2-x" data-remove-tag="${escapeHtml(t)}" title="Remove tag">×</button></span>`).join('');
  return `<section class="ag2-sec">
    <div class="ag2-sec-head">
      <span class="ag2-sec-label">Organization</span>
      <span class="ag2-sec-hint">pure navigation — grouping never hides, blocks, or completes anything</span>
    </div>
    <div class="ag2-sec-body">
      <div class="ag2-insp-orgrow">
        <span class="ag2-orgk">Filed under</span>
        <select data-act-change="place" aria-label="Filed under">
          <option value="">— not filed (stays in every lens either way)</option>
          ${placeOptions}
        </select>
      </div>
      <div class="ag2-insp-orgrow">
        <span class="ag2-orgk">See also</span>
        <div class="ag2-orgv">
          ${rels}
          <select class="ag2-relkind-add" data-act-change="rel-kind" aria-label="Link kind (reads this item → target)">
            ${relKindOptions}
          </select>
          <select class="ag2-reladd" data-act-change="rel-add" aria-label="Relate to">
            <option value="">+ relate…</option>${relOptions}
          </select>
        </div>
      </div>
      <div class="ag2-insp-orgrow">
        <span class="ag2-orgk">Tags</span>
        <div class="ag2-orgv">
          ${tags}
          <input type="text" class="ag2-tagin" data-fkey="tag-add" data-draft="tag" maxlength="60"
                 placeholder="+ tag" aria-label="Add a tag" value="${escapeHtml(agendaInspTagDraft)}" />
        </div>
      </div>
    </div>
  </section>`;
}

// ---- References ----

function agendaInspRefsHtml(item) {
  const refs = item.refs || [];
  let hasFileDigest = false;
  const rows = refs.map((r) => {
    const label = r.label ? `${r.label} — ` : '';
    let target;
    if (r.ref_type === 'url') {
      target = `<a class="ag2-ref-loc" href="${escapeHtml(r.locator)}" target="_blank" rel="noopener noreferrer nofollow">${escapeHtml(label + r.locator)}</a>`;
    } else if (r.ref_type === 'session') {
      const s = agendaSessionInfo(r.locator);
      const text = label + ((s && s.name) || `session ${String(r.locator).slice(0, 12)}`);
      target = s && s.key
        ? `<a class="ag2-ref-loc" data-jump-session="${escapeHtml(s.key)}" title="${escapeHtml(r.locator)}">${escapeHtml(text)}</a>`
        : `<span class="ag2-ref-loc" title="${escapeHtml(r.locator)}">${escapeHtml(text)}</span>`;
    } else if (r.ref_type === 'memory') {
      const text = label ? label + r.locator : `claim ${String(r.locator).slice(0, 12)}`;
      target = `<a class="ag2-ref-loc" data-open-claim="${escapeHtml(r.locator)}" title="${escapeHtml(r.locator)}">${escapeHtml(text)}</a>`;
    } else {
      // File refs open in the in-dashboard reader (decision-card UX): a
      // must-read you cannot read is a contradiction. Sealed snapshot
      // when the pin has one, live bytes with the drift verdict otherwise.
      if (r.digest) hasFileDigest = true;
      const tip = r.digest
        ? `sha256 ${r.digest} recorded at attach — click to read (sealed snapshot when pinned)`
        : `${r.locator} — click to read the live file`;
      target = `<a class="ag2-ref-loc" data-open-ref="${escapeHtml(r.locator)}" title="${escapeHtml(tip)}">${escapeHtml(label + r.locator)}</a>`;
    }
    const must = r.must_read
      ? '<span class="ag2-ref-must" title="A pointer the reading agent weighs — not a standing order">must-read</span>'
      : '';
    const drift = r.ref_type === 'file' && r.digest
      ? `<span class="agenda-ref-drift" data-item="${escapeHtml(item.id)}" data-locator="${escapeHtml(r.locator)}"></span>`
      : '';
    return `<div class="ag2-ref${r.must_read ? ' must' : ''}">
      <span class="ag2-ref-type">${escapeHtml(r.ref_type)}</span>
      ${target}${must}${drift}
      <button type="button" class="ag2-x" data-remove-ref-type="${escapeHtml(r.ref_type)}" data-remove-ref-loc="${escapeHtml(r.locator)}"
              title="Remove the pointer (an op — the log keeps history)">×</button>
    </div>`;
  }).join('');
  const empty = refs.length ? '' : '<div class="ag2-hint">No pointers yet — park the brief’s path, not its text.</div>';
  const verify = hasFileDigest
    ? '<button type="button" class="ag2-linkbtn" data-act="verify-refs" title="Re-hash file refs against their attach-time sha256 — on demand, never on list render">Verify files</button>'
    : '';
  const addRow = agendaInspAdds.ref
    ? `<div class="ag2-insp-addrow">
        <input type="text" class="mono" data-fkey="ref-add" data-draft="ref" maxlength="2000"
               placeholder="A path, URL, claim id, or session id — the type is inferred"
               value="${escapeHtml(agendaInspRefDraft)}" aria-label="Pointer locator" />
        <label class="ag2-check"><input type="checkbox" data-act-change="ref-must"${agendaInspRefMust ? ' checked' : ''}>must-read</label>
        <button type="button" class="ag2-btn" data-act="ref-add">Attach</button>
      </div>` : '';
  return `<section class="ag2-sec">
    <div class="ag2-sec-head">
      <span class="ag2-sec-label">References</span>
      <span class="ag2-sec-hint">typed pointers, never content — bodies go stale, pointers don’t</span>
      <span class="ag2-spacer"></span>
      ${verify}
    </div>
    <div class="ag2-sec-body">
      ${rows}${empty}
      <div class="ag2-insp-addbtns">
        <button type="button" class="ag2-dashbtn" data-act="toggle-ref-add">+ attach a pointer</button>
      </div>
      ${addRow}
    </div>
  </section>`;
}

// ---- Thread ----

function agendaInspThreadHtml(item) {
  if (!Array.isArray(item.annotations)) {
    // Summary row (S5): the thread rides the full item, being fetched.
    const n = item.annotations_count || 0;
    return `<section class="ag2-sec"><div class="ag2-hint">${n
      ? `Loading the ${n}-note thread…` : 'Loading detail…'}</div></section>`;
  }
  const notes = item.annotations || [];
  const all = agendaExpandedThreads.has(item.id);
  const shown = all ? notes : notes.slice(-3);
  const rail = shown.map((n) => {
    const who = n.kind === 'dashboard' ? 'you'
      : (agendaActorLabel(n) || n.source || 'unattributed');
    const meta = `${who}${n.source ? ` · --source ${n.source}` : ''} · ${agendaRelTime(n.at_ms)}`;
    const dot = n.kind === 'dashboard' ? 'iris' : n.source === 'triage' ? 'sky' : 'neutral';
    return `<div class="ag2-insp-note">
      <span class="ag2-insp-notedot t-${dot}"></span>
      <div class="ag2-insp-notemeta">${escapeHtml(meta)}</div>
      <div class="ag2-insp-notetext">${escapeHtml(n.text)}</div>
    </div>`;
  }).join('');
  const more = notes.length > 3
    ? `<button type="button" class="ag2-linkbtn" data-act="thread-all">${all ? 'collapse' : `show all ${notes.length}`}</button>`
    : '';
  const empty = notes.length ? '' : '<div class="ag2-hint">No notes yet — the thread is the handoff trail.</div>';
  return `<section class="ag2-sec">
    <div class="ag2-sec-head">
      <span class="ag2-sec-label">Thread</span>
      <span class="ag2-sec-hint">${escapeHtml(`${notes.length} note${notes.length === 1 ? '' : 's'} — attributed, append-only`)}</span>
    </div>
    <div class="ag2-sec-body">
      ${empty}${rail ? `<div class="ag2-insp-thread">${rail}</div>` : ''}${more}
      <div class="ag2-insp-addrow">
        <input type="text" data-fkey="ann-add" data-draft="ann" maxlength="4000"
               placeholder="Add a note to the thread — attributed to you" aria-label="Annotation"
               value="${escapeHtml(agendaInspAnnDraft)}" />
        <button type="button" class="ag2-btn" data-act="ann-add">Annotate</button>
      </div>
    </div>
  </section>`;
}

// ---- Inspector event delegation ----

function agendaInspItem() {
  // Full copy when the cache has one (S5); summary otherwise — action
  // handlers only need ids/status, which both shapes carry.
  if (!agendaSelId) return null;
  return agendaFullItemFor(agendaSelId) || agendaFindItem(agendaSelId);
}

function agendaInspClick(e) {
  const item = agendaInspItem();
  if (!item) return;
  const sessionLink = e.target.closest('a.agenda-session-link');
  if (sessionLink) {
    e.preventDefault();
    agendaJumpToSession(sessionLink.dataset.sessionKey);
    return;
  }
  const jump = e.target.closest('[data-jump-session]');
  if (jump) {
    agendaJumpToSession(jump.dataset.jumpSession);
    return;
  }
  const claim = e.target.closest('[data-open-claim]');
  if (claim) {
    routeTo('memory');
    if (typeof memoryGotoClaim === 'function') memoryGotoClaim(claim.dataset.openClaim);
    return;
  }
  const openRef = e.target.closest('[data-open-ref]');
  if (openRef) {
    agendaOpenRefReader(item.id, openRef.dataset.openRef);
    return;
  }
  const recUse = e.target.closest('[data-rec-use]');
  if (recUse) {
    // One-click answer prefill: the surfaced recommendation lands in the
    // composer as a DRAFT — the owner still sends it (or edits first).
    agendaQaDrafts[item.id] = recUse.dataset.recUse;
    agendaInspectorRender();
    const input = document.querySelector(`#ag2-inspector .ag2-qa-input[data-qa-draft="${window.CSS && CSS.escape ? CSS.escape(item.id) : item.id}"]`);
    if (input) {
      input.focus();
      input.setSelectionRange(input.value.length, input.value.length);
    }
    return;
  }
  const openItem = e.target.closest('[data-open-item]');
  if (openItem) {
    agendaOpenInspector(openItem.dataset.openItem);
    return;
  }
  const pill = e.target.closest('.ag2-pill');
  if (pill) {
    if (!(item.answer && item.answer.structured)) {
      agendaQaTogglePick(item, Number(pill.dataset.pillQ), pill.dataset.pillLabel);
    }
    return;
  }
  const expand = e.target.closest('[data-prev-expand]');
  if (expand) {
    const card = expand.closest('[data-prev-item]');
    if (card) {
      agendaOpenPreviewSheet(card.dataset.prevItem,
        Number(card.dataset.prevQ), Number(expand.dataset.prevExpand));
    }
    return;
  }
  const prev = e.target.closest('[data-prev-item]');
  if (prev) {
    agendaPreviewCardClick(prev);
    return;
  }
  const answerBtn = e.target.closest('[data-answer]');
  if (answerBtn) {
    agendaSubmitAnswer(item, answerBtn);
    return;
  }
  const clearBlocker = e.target.closest('[data-clear-blocker]');
  if (clearBlocker) {
    agendaSendOp({ op: 'clear_blocker', id: item.id, blocker_id: clearBlocker.dataset.clearBlocker }, clearBlocker);
    return;
  }
  const removeDep = e.target.closest('[data-remove-dep]');
  if (removeDep) {
    agendaSendOp({ op: 'remove_relies_on', id: item.id, target_id: removeDep.dataset.removeDep }, removeDep);
    return;
  }
  const removeRel = e.target.closest('[data-remove-rel]');
  if (removeRel) {
    // Either order works — the daemon resolves which side stores the link.
    agendaSendOp({ op: 'remove_relates_to', id: item.id, target_id: removeRel.dataset.removeRel }, removeRel);
    return;
  }
  const removeTag = e.target.closest('[data-remove-tag]');
  if (removeTag) {
    const tags = (item.tags || []).filter((t) => t !== removeTag.dataset.removeTag);
    agendaSendOp({ op: 'patch', id: item.id, patch: { tags } }, removeTag);
    return;
  }
  const removeRef = e.target.closest('[data-remove-ref-type]');
  if (removeRef) {
    agendaSendOp({
      op: 'remove_ref', id: item.id,
      ref_type: removeRef.dataset.removeRefType, locator: removeRef.dataset.removeRefLoc,
    }, removeRef);
    return;
  }
  const hoodAct = e.target.closest('[data-hood-act]');
  if (hoodAct) {
    // Under-the-hood section (slice D, ui2-agenda-hood.js).
    agendaHoodClick(item, hoodAct);
    return;
  }
  const act = e.target.closest('[data-act]');
  if (!act) return;
  switch (act.dataset.act) {
    case 'close': agendaCloseInspector(); break;
    case 'copy-id': agendaCopyText(item.id, 'the full item id'); break;
    case 'title-edit':
      // Patch is presentation state and works on any status.
      agendaInspEditingTitle = true;
      agendaInspTitleDraft = item.title;
      agendaInspectorRender();
      document.getElementById('ag2-insp-title-input')?.focus();
      break;
    case 'title-save': agendaInspSaveTitle(item); break;
    case 'start': agendaOpenStartSheet(item.id, act); break;
    case 'follow-live': {
      const sid = agendaFollowUpSid(item);
      if (sid) agendaFollowUpWithRecorder(item, sid);
      break;
    }
    case 'follow-resume': agendaFollowUpResume(item); break;
    case 'complete': agendaSendOp({ op: 'complete', id: item.id }, act); break;
    case 'reopen': agendaSendOp({ op: 'reopen', id: item.id }, act); break;
    case 'retire': agendaSendOp({ op: 'retire', id: item.id }, act); break;
    case 'rail-open': agendaOpenParkedAsk(item.id); break;
    case 'rail-view': agendaViewAnsweredAsk(item.id); break;
    case 'body-edit':
      agendaInspEditingBody = true;
      agendaInspBodyDraft = item.body || '';
      agendaInspectorRender();
      document.getElementById('ag2-insp-body-input')?.focus();
      break;
    case 'body-cancel':
      agendaInspEditingBody = false;
      agendaInspectorRender();
      break;
    case 'body-save':
      agendaInspEditingBody = false;
      agendaSendOp({ op: 'patch', id: item.id, patch: { body: agendaInspBodyDraft } }, act);
      break;
    case 'sched': agendaOpenSchedSheet(item.id); break;
    case 'eff-approve': {
      const st = agendaEffectState(item);
      if (st) {
        // Approve binds the digest of exactly the revision rendered.
        agendaSendOp({ op: 'approve_effect', id: item.id, digest: st.effect.digest }, act)
          .then((updated) => { if (updated) agendaApprovalMoment(updated); });
      }
      break;
    }
    case 'eff-revoke': agendaSendOp({ op: 'revoke_effect', id: item.id }, act); break;
    // Decline a still-unapproved proposal (withdraw_effect): stops the
    // approval solicitation now; the item, thread, and fired history
    // stay. The daemon refuses on an approved manifest (that side is
    // Revoke) — no digest rides this op, there is nothing to bind.
    case 'eff-withdraw': agendaSendOp({ op: 'withdraw_effect', id: item.id }, act); break;
    case 'eff-run-now':
      // One extra occurrence of the approved standing manifest — the
      // daemon refuses (named) outside its rules: recurring + approved,
      // not suspended, no run in flight, no earlier request pending.
      agendaSendOp({ op: 'request_occurrence', id: item.id }, act);
      break;
    case 'toggle-blocker-add':
      agendaInspAdds.blocker = !agendaInspAdds.blocker;
      agendaInspectorRender();
      break;
    case 'toggle-dep-add':
      agendaInspAdds.dep = !agendaInspAdds.dep;
      agendaInspectorRender();
      break;
    case 'toggle-ref-add':
      agendaInspAdds.ref = !agendaInspAdds.ref;
      agendaInspectorRender();
      break;
    case 'blocker-add': agendaInspAddBlocker(item, act); break;
    case 'ref-add': agendaInspAddRef(item, act); break;
    case 'ann-add': agendaInspAddAnnotation(item, act); break;
    case 'verify-refs': agendaVerifyRefs(item.id, act); break;
    case 'thread-all':
      if (agendaExpandedThreads.has(item.id)) agendaExpandedThreads.delete(item.id);
      else agendaExpandedThreads.add(item.id);
      agendaInspectorRender();
      break;
    default: break;
  }
}

function agendaInspInput(e) {
  const t = e.target;
  if (t.id === 'ag2-insp-title-input') agendaInspTitleDraft = t.value;
  else if (t.id === 'ag2-insp-body-input') agendaInspBodyDraft = t.value;
  else if (t.dataset.qaDraft) agendaQaDrafts[t.dataset.qaDraft] = t.value;
  else if (t.dataset.qaNote) agendaQaNotes[t.dataset.qaNote] = t.value;
  else if (t.dataset.draft === 'blocker') agendaInspBlockerDraft = t.value;
  else if (t.dataset.draft === 'ann') agendaInspAnnDraft = t.value;
  else if (t.dataset.draft === 'tag') agendaInspTagDraft = t.value;
  else if (t.dataset.draft === 'ref') agendaInspRefDraft = t.value;
}

function agendaInspKeydown(e) {
  if (e.key !== 'Enter') return;
  const t = e.target;
  const item = agendaInspItem();
  if (!item) return;
  if (t.id === 'ag2-insp-title-input') {
    e.preventDefault();
    agendaInspSaveTitle(item);
  } else if (t.dataset && t.dataset.qaDraft) {
    e.preventDefault();
    agendaSubmitAnswer(item);
  } else if (t.dataset && t.dataset.draft === 'blocker') {
    e.preventDefault();
    agendaInspAddBlocker(item);
  } else if (t.dataset && t.dataset.draft === 'ann') {
    e.preventDefault();
    agendaInspAddAnnotation(item);
  } else if (t.dataset && t.dataset.draft === 'tag') {
    e.preventDefault();
    agendaInspAddTag(item);
  } else if (t.dataset && t.dataset.draft === 'ref') {
    e.preventDefault();
    agendaInspAddRef(item);
  }
}

function agendaInspChange(e) {
  const t = e.target;
  const item = agendaInspItem();
  if (!item) return;
  if (t.dataset.actChange === 'due-preset') {
    const v = t.value;
    t.value = '';
    if (!v) return;
    // Merge-patch semantics: `null` clears, a value sets (AgendaPatch
    // double_option).
    const ms = v === 'clear' ? null : agendaDuePresetMs(v);
    agendaSendOp({ op: 'patch', id: item.id, patch: { due_ms: ms } }, t);
  } else if (t.dataset.actChange === 'urgency') {
    agendaSetItemUrgency(item.id, t.value, t);
  } else if (t.dataset.actChange === 'place') {
    agendaInspPlace(item, t.value, t);
  } else if (t.dataset.actChange === 'rel-kind') {
    // Draft only — the link sends when a target is picked.
    agendaInspRelKind = t.value;
  } else if (t.dataset.actChange === 'rel-add') {
    const v = t.value;
    t.value = '';
    if (v) {
      const params = { op: 'add_relates_to', id: item.id, target_id: v };
      if (agendaInspRelKind) params.link_kind = agendaInspRelKind;
      agendaInspRelKind = '';
      agendaSendOp(params, t);
    }
  } else if (t.dataset.actChange === 'dep-add') {
    const v = t.value;
    if (v) {
      agendaInspAdds.dep = false;
      agendaSendOp({ op: 'add_relies_on', id: item.id, target_id: v }, t);
    }
  } else if (t.dataset.actChange === 'ref-must') {
    agendaInspRefMust = !!t.checked;
  }
}

function agendaInspSaveTitle(item) {
  const text = agendaInspTitleDraft.trim();
  agendaInspEditingTitle = false;
  if (text && text !== item.title) {
    agendaSendOp({ op: 'patch', id: item.id, patch: { title: text } });
  } else {
    agendaInspectorRender();
  }
}

// Placement changes ride the real vocabulary: none→parent = add_part_of;
// parent→none = remove_part_of; parent→parent = the atomic `place`
// (validate-new-first re-parent, steward override 2026-07-22).
function agendaInspPlace(item, value, control) {
  const current = item.part_of ? item.part_of.parent_id : '';
  if (value === current) return;
  if (!value && current) {
    agendaSendOp({ op: 'remove_part_of', id: item.id, parent_id: current }, control);
  } else if (value && !current) {
    agendaSendOp({ op: 'add_part_of', id: item.id, parent_id: value }, control);
  } else if (value && current) {
    agendaSendOp({ op: 'place', id: item.id, under: value }, control);
  }
}

function agendaInspAddBlocker(item, button) {
  const text = agendaInspBlockerDraft.trim();
  if (!text) return;
  agendaInspBlockerDraft = '';
  agendaInspAdds.blocker = false;
  agendaSendOp({ op: 'set_blocker', id: item.id, criterion: text }, button);
}

function agendaInspAddAnnotation(item, button) {
  const text = agendaInspAnnDraft.trim();
  if (!text) return;
  agendaInspAnnDraft = '';
  agendaSendOp({ op: 'annotate', id: item.id, text }, button);
}

function agendaInspAddTag(item) {
  const tag = agendaInspTagDraft.trim();
  if (!tag) return;
  agendaInspTagDraft = '';
  if ((item.tags || []).includes(tag)) {
    agendaInspectorRender();
    return;
  }
  agendaSendOp({ op: 'patch', id: item.id, patch: { tags: [...(item.tags || []), tag] } });
}

// Pointer type inference for the add row (the daemon validates the typed
// command either way — this only picks the claimed type): http(s) → url,
// a 12+-hex run → memory claim id, an id the sessions join knows → session,
// a trailing slash → dir (the browser cannot stat; the daemon re-checks
// and normalizes the slash away), anything else → file path.
function agendaInspInferRefType(locator) {
  if (/^https?:\/\//i.test(locator)) return 'url';
  if (/^[0-9a-f]{12,}$/i.test(locator)) return 'memory';
  if (agendaSessions && agendaSessions[locator]) return 'session';
  if (/^sess-/.test(locator)) return 'session';
  if (locator.length > 1 && /\/$/.test(locator)) return 'dir';
  return 'file';
}

function agendaInspAddRef(item, button) {
  const locator = agendaInspRefDraft.trim();
  if (!locator) return;
  const refType = agendaInspInferRefType(locator);
  const params = { op: 'add_ref', id: item.id, ref_type: refType, locator };
  if (agendaInspRefMust) params.must_read = true;
  agendaInspRefDraft = '';
  agendaInspRefMust = false;
  agendaInspAdds.ref = false;
  agendaSendOp(params, button).then((ok) => {
    if (ok && typeof showControlToast === 'function') {
      showControlToast('success', refType === 'file'
        ? 'Attached — the file was hashed at intake; the digest travels, blobs never do.'
        : `Attached a ${refType} pointer.`);
    }
  });
}

function agendaCopyText(text, what) {
  const done = () => {
    if (typeof showControlToast === 'function') {
      showControlToast('info', `${`Copied ${what || ''}`.trim()}.`);
    }
  };
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text).then(done,
      () => agendaFlashError('Clipboard unavailable in this context.'));
  } else {
    agendaFlashError('Clipboard unavailable in this context.');
  }
}

// Re-render helper that keeps the focused input (by data-fkey) focused
// across an innerHTML replace — event-lane repaints otherwise steal the
// caret mid-typing.
function agendaRenderPreservingFocus(host, render) {
  const active = document.activeElement;
  const key = active && host.contains(active) ? active.getAttribute('data-fkey') : null;
  const selStart = key && active.selectionStart != null ? active.selectionStart : null;
  render();
  if (!key) return;
  const next = host.querySelector(`[data-fkey="${CSS.escape(key)}"]`);
  if (!next) return;
  next.focus();
  if (selStart != null && next.setSelectionRange) {
    try { next.setSelectionRange(selStart, selStart); } catch (err) { /* non-text inputs */ }
  }
}

// ---- Sheets (schedule + preview) ----

let agendaSheetState = null;

function agendaEnsureSheetHost() {
  let host = document.getElementById('ag2-sheet');
  if (host) return host;
  host = document.createElement('div');
  host.id = 'ag2-sheet';
  host.hidden = true;
  host.innerHTML = '<div class="ag2-sheet-backdrop"></div><div class="ag2-sheet-panel" role="dialog" aria-modal="true"></div>';
  document.body.appendChild(host);
  host.querySelector('.ag2-sheet-backdrop').addEventListener('click', () => agendaSheetClose());
  const panel = host.querySelector('.ag2-sheet-panel');
  panel.addEventListener('click', agendaSheetClick);
  panel.addEventListener('input', agendaSheetInput);
  panel.addEventListener('change', agendaSheetInput);
  return host;
}

// Returns true when a sheet was open (the Escape chain).
function agendaSheetClose() {
  const host = document.getElementById('ag2-sheet');
  const wasOpen = !!(host && !host.hidden);
  if (host) host.hidden = true;
  agendaSheetState = null;
  return wasOpen;
}

function agendaSheetRender() {
  const host = agendaEnsureSheetHost();
  const panel = host.querySelector('.ag2-sheet-panel');
  if (!agendaSheetState) {
    host.hidden = true;
    return;
  }
  const item = agendaFindItem(agendaSheetState.itemId);
  if (!item) {
    agendaSheetClose();
    return;
  }
  host.hidden = false;
  agendaRenderPreservingFocus(panel, () => {
    panel.innerHTML = agendaSheetState.kind === 'prev'
      ? agendaPrevSheetHtml(item)
      : agendaSheetState.kind === 'raw'
        ? agendaRawSheetHtml(item) // slice D (ui2-agenda-hood.js)
        : agendaSheetState.kind === 'refread'
          ? agendaRefReadSheetHtml(item)
          : agendaSchedSheetHtml(item);
  });
  agendaHydratePreviewFrames(panel);
}

// -- Ref reader sheet (decision-card UX) --
//
// Opens one attached FILE ref in-dashboard through the ref-scoped
// `api_agenda_ref_content` lane: sealed snapshot bytes when the ref's
// attach pin has one (the sealed-refs store), live bytes with the honest
// drift verdict otherwise. Content is quoted data on every path — text
// renders escaped in a <pre>, images via a data: URL <img>, and
// agent-authored HTML is never given the dashboard origin.

function agendaOpenRefReader(itemId, locator) {
  if (!locator) return;
  agendaSheetState = {
    kind: 'refread', itemId, locator, loading: true, data: null, error: '',
  };
  agendaSheetRender();
  const mine = () => agendaSheetState && agendaSheetState.kind === 'refread'
    && agendaSheetState.itemId === itemId && agendaSheetState.locator === locator;
  daemonApi.request('api_agenda_ref_content', { item_id: itemId, locator })
    .then((resp) => {
      if (!mine()) return;
      if (resp && resp.ok && resp.body) {
        agendaSheetState.data = resp.body;
      } else {
        const body = (resp && resp.body) || {};
        agendaSheetState.error = body.error || `read failed (${(resp && resp.status) || 'no response'})`;
      }
    })
    .catch((err) => {
      if (mine()) agendaSheetState.error = String((err && err.message) || err);
    })
    .finally(() => {
      if (!mine()) return;
      agendaSheetState.loading = false;
      agendaSheetRender();
    });
}

// The provenance strip's honest wording per (source, drift) — the sealed
// lane names what you are reading; the live lane names what it may have
// become since attach.
function agendaRefReadProvenance(data) {
  const sealed = data.source === 'sealed';
  const badge = sealed
    ? ['iris', 'sealed snapshot']
    : ['sky', 'live file'];
  let drift;
  if (sealed) {
    drift = data.drift === 'unchanged' ? ['green', 'live file still matches the pin']
      : data.drift === 'missing' ? ['amber', 'live file gone — the sealed revision is preserved here']
        : ['amber', 'live file drifted from sealed revision — you are reading the sealed bytes'];
  } else {
    drift = data.drift === 'unchanged' ? ['green', 'matches the attach-time digest']
      : data.drift === 'changed' ? ['amber', 'drifted since attach — this is the file as it stands NOW']
        : ['neutral', 'no attach digest — live bytes, unverified'];
  }
  return { badge, drift };
}

function agendaRefReadSheetHtml(item) {
  const s = agendaSheetState;
  const name = s.data ? s.data.name : String(s.locator).split('/').pop() || s.locator;
  let body;
  if (s.loading) {
    body = '<div class="ag2-hint">Reading…</div>';
  } else if (s.error) {
    body = `<div class="ag2-sheet-error">${escapeHtml(s.error)}</div>`;
  } else {
    const d = s.data;
    const { badge, drift } = agendaRefReadProvenance(d);
    const kb = d.size >= 1024 * 1024
      ? `${(d.size / (1024 * 1024)).toFixed(1)} MiB` : `${Math.max(1, Math.round(d.size / 1024))} KiB`;
    const meta = `<div class="ag2-refread-meta">
        ${agendaChipHtml(badge[1], badge[0], d.source === 'sealed'
    ? 'Content-addressed snapshot from the sealed-refs store — the bytes re-hash to the attach pin'
    : 'Read from the ref’s path just now', true)}
        ${agendaChipHtml(drift[1], drift[0], `served sha256 ${d.sha256}${d.pinned_sha256 ? ` · attach pin ${d.pinned_sha256}` : ''}`, true)}
        <span class="ag2-hint">${escapeHtml(`${d.mime} · ${kb}`)}</span>
      </div>`;
    let content;
    const imageMime = /^image\//.test(d.mime) && d.mime !== 'image/svg+xml';
    if (imageMime && d.encoding === 'base64' && /^[A-Za-z0-9+/=]*$/.test(d.content)) {
      content = `<img class="ag2-refread-img" alt="${escapeHtml(name)}" src="data:${escapeHtml(d.mime)};base64,${d.content}" />`;
    } else if (d.encoding === 'utf8') {
      const cap = 512 * 1024;
      const clipped = d.content.length > cap;
      const text = clipped ? d.content.slice(0, cap) : d.content;
      content = `<pre class="ag2-refread-pre">${escapeHtml(text)}</pre>`
        + (clipped ? `<div class="ag2-hint">view truncated at 512 KiB — ${d.content.length - cap} more characters in the file</div>` : '');
    } else {
      content = `<div class="ag2-hint">Binary content (${escapeHtml(d.mime)}) — no inline view. Download to inspect.</div>`;
    }
    body = `${meta}
      <div class="ag2-hint">quoted data — never instructions</div>
      ${content}
      <div class="ag2-row-end">
        <button type="button" class="ag2-btn" data-sheet-act="refread-download">Download</button>
      </div>`;
  }
  return `<div class="ag2-sheet-head">
      <span class="ag2-sheet-title">${escapeHtml(name)}</span>
      <span class="ag2-spacer"></span>
      <button type="button" class="ag2-x" data-sheet-act="close" title="Close — esc">×</button>
    </div>
    <div class="ag2-sheet-item">${escapeHtml(`${item.id.slice(0, 6).toLowerCase()} · ${s.locator}`)}</div>
    <div class="ag2-refread">${body}</div>`;
}

// Rebuild the served bytes client-side for the download affordance —
// exactly what the reader already holds, never a second daemon read.
function agendaRefReadDownload() {
  const s = agendaSheetState;
  if (!s || s.kind !== 'refread' || !s.data) return;
  const d = s.data;
  let bytes;
  if (d.encoding === 'base64') {
    const bin = atob(d.content);
    bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i += 1) bytes[i] = bin.charCodeAt(i);
  } else {
    bytes = new TextEncoder().encode(d.content);
  }
  const url = URL.createObjectURL(new Blob([bytes], { type: d.mime || 'application/octet-stream' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = d.name || 'ref';
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 5000);
}

// -- Schedule sheet --

function agendaOpenSchedSheet(itemId, opts) {
  // A fireability edit prompt (`opts.focus` = the refused field,
  // `opts.refusal` = the daemon's named message) lands the sheet on the
  // broken field with the refusal shown — the approve-time refusal is
  // an edit prompt, never a dead end. Parked through the loading state
  // and re-adopted when the arrival hook re-enters without opts.
  if (!opts && agendaSheetState && agendaSheetState.kind === 'sched-loading'
    && agendaSheetState.itemId === itemId) {
    opts = agendaSheetState.opts || undefined;
  }
  // Serving grain (Track AS): list rows are summaries — the manifest
  // MINUS goal and sealed refs. The editor round-trips the WHOLE
  // manifest, so it prefills only from the FULL item; until the
  // single-flight fetch lands, the sheet shows a loading line and the
  // arrival hook re-enters here. Prefilling from a summary would blank
  // the goal and silently unseal the refs on save — the exact bug the
  // round-trip law exists to prevent.
  const item = agendaFullItemFor(itemId);
  if (!item) {
    if (!agendaFindItem(itemId)) return;
    agendaSheetState = { kind: 'sched-loading', itemId, opts: opts || null };
    agendaSheetRender();
    return;
  }
  const st = agendaEffectState(item);
  const m = st && st.manifest;
  const toLocal = (ms) => {
    const d = new Date(ms);
    const p = (n) => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
  };
  const rec = m && m.recurrence;
  // Executor prefill: the manifest's recorded pins, so the swap gesture
  // (edit executor → approval voided → re-approve) starts from what the
  // owner approved. Empty = inherit the daemon default at fire time.
  const cfg = (m && m.agent_config) || {};
  // Cadence prefill round-trips faithfully: a whole-day cadence maps to
  // its day count; anything else stays selectable as the literal
  // manifest cadence (the model-pin pattern) so an edit around it never
  // silently rewrites — or drops — what the owner isn't touching.
  const days = rec && rec.every_ms % 864e5 === 0 ? String(rec.every_ms / 864e5) : '';
  const untilLocal = (ms) => {
    const d = new Date(ms);
    const p = (n) => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
  };
  agendaSheetState = {
    kind: 'sched',
    itemId,
    goal: m ? m.goal : agendaStartGoalStatement(item),
    when: toLocal(st && st.next > Date.now() ? st.next : Date.now() + 864e5),
    repeat: rec ? (['1', '7', '14'].includes(days) ? days : 'keep') : '',
    keepEveryMs: rec ? rec.every_ms : 0,
    until: rec && rec.until_ms ? untilLocal(rec.until_ms) : '',
    maxRuns: rec && rec.max_occurrences ? String(rec.max_occurrences) : '',
    suspend: rec && rec.suspend_after_failures ? String(rec.suspend_after_failures) : '3',
    orchestrate: !!(m && m.orchestrate),
    // The shape toggle (goal run vs interactive) — digest-bound like
    // every other manifest field.
    shape: m && m.interactive ? 'interactive' : 'goal',
    projectRoot: (m && m.project_root) || '',
    // Carried VERBATIM through an edit (rendered read-only): the event
    // trigger and the sealed binding refs. The edit sheet is a client
    // of re-propose, and re-propose replaces the whole manifest — what
    // the owner isn't editing must ride along untouched.
    trigger: (m && m.trigger) || null,
    // The on-unblock offer for dependents (suggested mode): preselected
    // only on a FRESH propose for an item with relies_on edges; an
    // existing proposal opens exactly as it stands — existing time-floor
    // manifests are never silently converted by opening this sheet.
    onUnblock: !m && Array.isArray(item.relies_on) && item.relies_on.length > 0,
    bindingRefs: m && Array.isArray(m.binding_refs) ? m.binding_refs.slice() : [],
    execBackend: cfg.agent || '',
    execModel: cfg.claude_model || cfg.codex_model || cfg.kimi_model || cfg.pi_model || '',
    execEffort: cfg.claude_effort || cfg.codex_reasoning_effort || cfg.kimi_thinking || cfg.pi_thinking || '',
    approveNow: true,
    voids: !!(st && st.effect.approval),
    // A served fireability refusal (or an approve-time one carried in
    // opts) renders as the sheet's inline error from the first paint,
    // and the named field gets focus below.
    error: (opts && opts.refusal)
      || (st && st.effect.fireability_refusal
        ? `unfireable(${st.effect.fireability_refusal.field}): ${st.effect.fireability_refusal.reason}`
        : ''),
    focusField: (opts && opts.focus)
      || (st && st.effect.fireability_refusal && st.effect.fireability_refusal.field) || '',
  };
  // Opening the editor IS looking at the current revision.
  if (st && typeof agendaAckEffectDigest === 'function') {
    agendaAckEffectDigest(st.effect.effect_id, st.effect.digest);
  }
  agendaSheetRender();
  agendaSchedApplyFocus();
  // Model/effort option lists come from the served settings; fetch like
  // the start sheet does and re-render when they land.
  if (agendaStartSheetSettings === null && typeof fetchDashboardSettings === 'function') {
    fetchDashboardSettings()
      .then((d) => { if (d && !d.error) agendaStartSheetSettings = d; })
      .catch(() => {})
      .finally(() => {
        if (agendaSheetState && agendaSheetState.kind === 'sched') agendaSheetRender();
      });
  }
  // The projectless-required hint derives from the SAME source the
  // start sheet uses (api_project_root); fetch once and re-render so
  // the Project row can say "required" instead of guessing.
  if (agendaDaemonDefaultProject === null && typeof fetchProjectRoot === 'function') {
    fetchProjectRoot()
      .then((root) => { agendaDaemonDefaultProject = root || ''; })
      .catch(() => { agendaDaemonDefaultProject = ''; })
      .finally(() => {
        if (agendaSheetState && agendaSheetState.kind === 'sched') agendaSheetRender();
      });
  }
}

// Migration honesty (fireability): an op refused by the daemon's
// `unfireable(<field>): …` grammar — a parked pin-less manifest meeting
// the validator at its next approve, or the missed-card one-tap hitting
// a now-unresolvable plan — surfaces as an EDIT prompt, never a dead
// end: the one plan editor opens with the named field focused and the
// refusal shown. Called by agendaSendOp on every refusal; the grammar is
// the daemon's pinned wire contract (FIREABILITY_REFUSAL_PREFIX in
// agenda/fireability.rs).
function agendaFireabilityEditPrompt(params, message) {
  if (!params) return;
  // approve refusals always prompt; a propose refusal prompts only when
  // no sheet owns the gesture (the missed-card one-tap) — sheet flows
  // keep their own inline errors.
  const prompts = params.op === 'approve_effect'
    || (params.op === 'propose_effect' && !agendaSheetState);
  if (!prompts) return;
  const match = /^unfireable\((project|executor|floor)\): /.exec(String(message || ''));
  if (!match) return;
  agendaOpenSchedSheet(params.id, { focus: match[1], refusal: String(message) });
}

// Land the sheet on the field a fireability refusal named: focus it and
// pulse its row. project → the Project input, executor → the backend
// select, floor → the First-run input.
function agendaSchedApplyFocus() {
  const s = agendaSheetState;
  if (!s || s.kind !== 'sched' || !s.focusField) return;
  const selector = s.focusField === 'project' ? '[data-sheet="projectRoot"]'
    : s.focusField === 'executor' ? '[data-sheet="execBackend"]'
      : s.focusField === 'floor' ? '[data-sheet="when"]' : '';
  if (!selector) return;
  const host = document.getElementById('ag2-sheet');
  const el = host && host.querySelector(selector);
  if (!el) return;
  el.focus();
  const row = el.closest('div') || el;
  row.classList.add('ag2-field-attn');
  setTimeout(() => row.classList.remove('ag2-field-attn'), 4000);
}

function agendaSchedSheetHtml(item) {
  const s = agendaSheetState;
  if (s.kind === 'sched-loading') {
    return `<div class="ag2-sheet-head">
        <span class="ag2-sheet-title">Loading the full manifest…</span>
        <span class="ag2-spacer"></span>
        <button type="button" class="ag2-x" data-sheet-act="close" title="Close — esc">×</button>
      </div>
      <div class="ag2-hint">The list serves summaries; the editor prefills from the full item so nothing is dropped on save.</div>`;
  }
  // The on-unblock offer (dependents' suggested mode): rendered whenever
  // the manifest is time-driven and the item carries relies_on edges;
  // ticked, it swaps the cadence lane for the dependency-gated trigger
  // (a manifest is cadenced OR triggered — the daemon's intake rule,
  // honored here instead of met as a refusal).
  const offerUnblock = !s.trigger && Array.isArray(item.relies_on) && item.relies_on.length > 0;
  const unblockOn = offerUnblock && !!s.onUnblock;
  const standing = !!s.repeat && !unblockOn;
  const standingBlock = standing
    ? `<div class="ag2-sheet-callout t-green">Standing series — one approval covers every run until revoked. A failure streak suspends it for you to re-arm.</div>
      <div class="ag2-sheet-grid">
        <span class="ag2-sheet-k">Ends</span>
        <div class="ag2-sheet-inline">
          <input type="date" data-sheet="until" value="${escapeHtml(s.until)}" aria-label="Series end date" />
          <span class="ag2-hint">or after</span>
          <input type="number" min="1" placeholder="∞" class="ag2-sheet-num" data-sheet="maxRuns" value="${escapeHtml(s.maxRuns)}" aria-label="Maximum runs" />
          <span class="ag2-hint">runs</span>
        </div>
        <span class="ag2-sheet-k" title="Consecutive non-working outcomes that suspend the series: failed or unknown runs, and completed runs the session itself reported blocked or abandoned (self-reported). Achieved or unreported completions reset the streak; partial is neutral; missed runs from daemon downtime don’t count">Suspend after</span>
        <div class="ag2-sheet-inline">
          <input type="number" min="1" max="10" class="ag2-sheet-num" data-sheet="suspend" value="${escapeHtml(s.suspend)}" aria-label="Suspend after failures" />
          <span class="ag2-hint">failed runs in a row</span>
        </div>
      </div>`
    : '';
  // The shape toggle — honest about consequences ON the card, in the
  // scheduler's own semantics (the fired session's launch shape).
  const shapeHint = s.shape === 'interactive'
    ? 'Opens with the goal as your message and waits for you — it does not auto-run.'
    : 'Autonomous one-shot — runs the goal unattended and writes back.';
  const shapeBlock = `<div class="ag2-sheet-grid" data-mf-field="interactive">
      <span class="ag2-sheet-k">Shape</span>
      <div>
        <div class="ag2-sheet-inline">
          <button type="button" class="ag2-seg-btn${s.shape !== 'interactive' ? ' active' : ''}" data-sheet-act="sched-shape" data-shape="goal">goal run</button>
          <button type="button" class="ag2-seg-btn${s.shape === 'interactive' ? ' active' : ''}" data-sheet-act="sched-shape" data-shape="interactive">interactive</button>
        </div>
        <div class="ag2-hint">${escapeHtml(shapeHint)}</div>
      </div>
    </div>`;
  // Cadence controls when the manifest is time-driven; the event
  // trigger renders read-only (carried verbatim — cadence and trigger
  // are mutually exclusive, and trigger editing has no form yet). A
  // cadence the day-select can't express stays selectable as the
  // literal manifest cadence (the model-pin pattern) so re-rendering
  // never drops it.
  const keepOption = s.keepEveryMs
    && (s.keepEveryMs % 864e5 !== 0 || !['1', '7', '14'].includes(String(s.keepEveryMs / 864e5)))
    ? `<option value="keep"${s.repeat === 'keep' ? ' selected' : ''}>keep — every ${escapeHtml(agendaCadenceLabel(s.keepEveryMs))}</option>`
    : '';
  const cadenceBlock = s.trigger
    ? `<span class="ag2-sheet-k" data-mf-field="trigger">Fires</span>
      <div>
        <div>${escapeHtml(s.trigger.kind === 'on_item_match'
    ? `on matching items (${[s.trigger.item_kind, ...(s.trigger.tags || [])].filter(Boolean).join(', ')})`
    : 'when this item unblocks')}</div>
        <div class="ag2-hint">Event-triggered — carried unchanged through this edit; the time above is the arm floor, not a fire instant.</div>
      </div>`
    : unblockOn
      ? `<span class="ag2-sheet-k" data-mf-field="trigger">Fires</span>
      <div>
        <div>when this item unblocks (its prerequisites complete)</div>
        <div class="ag2-hint">Event-triggered (on_unblock): approve anytime — the fire waits for the real unblock. The time above is the arm floor, not a fire instant.</div>
      </div>`
      : `<span class="ag2-sheet-k">Repeats</span>
      <select data-sheet="repeat" data-mf-field="recurrence" aria-label="Repeats">
        <option value=""${s.repeat === '' ? ' selected' : ''}>never — one run</option>
        <option value="1"${s.repeat === '1' ? ' selected' : ''}>every day</option>
        <option value="7"${s.repeat === '7' ? ' selected' : ''}>every week</option>
        <option value="14"${s.repeat === '14' ? ' selected' : ''}>every two weeks</option>
        ${keepOption}
      </select>`;
  // The offer row itself: a visible, reversible tick — never applied
  // silently (a fresh propose on a dependent item arrives pre-ticked;
  // editing an existing plan arrives as-it-stands).
  const unblockOffer = offerUnblock
    ? `<label class="ag2-check" data-sheet-offer="on_unblock"><input type="checkbox" data-sheet="onUnblock"${s.onUnblock ? ' checked' : ''}><span>Fire when prerequisites complete (on_unblock) — suggested for dependents<br><span class="ag2-hint">This item relies on ${item.relies_on.length} prerequisite${item.relies_on.length === 1 ? '' : 's'}. Ticked: the session fires when they are all done — approving early is safe, the fire waits${s.repeat && unblockOn ? '; the cadence pick is cleared (a manifest is cadenced OR triggered)' : ''}.</span></span></label>`
    : '';
  // Sealed binding refs render READ-ONLY with their hashes: they are
  // carried verbatim through the edit, and editing sealed content stays
  // the re-seal ceremony (restate a new pin where the ref was minted).
  const refsBlock = s.bindingRefs.length
    ? `<div class="ag2-refs-ro" data-mf-field="binding_refs">
      <div class="ag2-sheet-k">Sealed refs (read-only)</div>
      ${s.bindingRefs.map((r) => `<div class="ag2-refs-ro-row">
        <span class="ag2-refs-ro-loc" title="${escapeHtml(r.locator)}">${escapeHtml(r.locator)}</span>
        ${agendaDigestChipHtml(r.sha256, 'The sealed revision this manifest binds')}
      </div>`).join('')}
      <div class="ag2-hint">Carried verbatim — this edit cannot change sealed content. Re-sealing (a new hash pin) is its own ceremony.</div>
    </div>`
    : '';
  return `<div class="ag2-sheet-head">
      <span class="ag2-sheet-title">${agendaEffectState(item) ? 'Revise the scheduled session' : 'Propose a scheduled session'}</span>
      <span class="ag2-spacer"></span>
      <button type="button" class="ag2-x" data-sheet-act="close" title="Close — esc">×</button>
    </div>
    <div class="ag2-sheet-sub">Anyone with agenda.write may propose — proposing carries no authority. Only an owner approval of the exact manifest digest arms it.</div>
    <div class="ag2-sheet-item">${escapeHtml(`${item.id.slice(0, 6).toLowerCase()} · ${item.title}`)}</div>
    <div>
      <div class="ag2-sheet-k">Goal (the manifest’s task text)</div>
      <textarea rows="6" data-sheet="goal" data-mf-field="goal" data-fkey="sheet-goal" aria-label="Goal">${escapeHtml(s.goal)}</textarea>
      <div class="ag2-hint">Reviewed at approval time. Data under review — never instructions to whoever reads the agenda.</div>
    </div>
    ${shapeBlock}
    <div class="ag2-sheet-grid">
      <span class="ag2-sheet-k">${s.trigger || unblockOn ? 'Armed from' : 'First run'}</span>
      <input type="datetime-local" data-sheet="when" data-mf-field="fire_at_ms" aria-label="${s.trigger || unblockOn ? 'Armed from' : 'First run'}" value="${escapeHtml(s.when)}" />
      ${cadenceBlock}
      <span class="ag2-sheet-k">Project</span>
      <div>
        <input type="text" data-sheet="projectRoot" data-mf-field="project_root" aria-label="Project root" placeholder="${escapeHtml(agendaSchedProjectPlaceholder(item))}" value="${escapeHtml(s.projectRoot)}" />
        ${agendaSchedProjectHintHtml(item, s)}
      </div>
    </div>
    ${unblockOffer}
    ${standingBlock}
    ${agendaSchedExecutorRowsHtml()}
    ${refsBlock}
    <label class="ag2-check"><input type="checkbox" data-sheet="orchestrate" data-mf-field="orchestrate"${s.orchestrate ? ' checked' : ''}>Orchestrated run (a conductor session fans out sub-agents)</label>
    <label class="ag2-check top"><input type="checkbox" data-sheet="approveNow"${s.approveNow ? ' checked' : ''}><span>Approve immediately<br><span class="ag2-hint">You’re on an owner surface. Any later edit mints a new digest and voids this approval.</span></span></label>
    ${s.voids ? '<div class="ag2-sheet-callout t-amber">This revises the manifest — the current approval becomes void until re-approved.</div>' : ''}
    ${s.error ? `<div class="ag2-sheet-error">${escapeHtml(s.error)}</div>` : ''}
    <div class="ag2-row-end">
      <button type="button" class="ag2-btn ghost" data-sheet-act="close">Cancel</button>
      <button type="button" class="ag2-btn prim" data-sheet-act="sched-confirm">${s.approveNow ? 'Propose & approve' : 'Propose schedule'}</button>
    </div>`;
}

// The Project row's hint, derived from the SAME resolution the daemon's
// fireability validator runs (explicit → the parking session's root →
// the daemon default → refused named): an empty pick on an item the
// chain cannot cover says "required" up front, exactly like the
// start-now sheet — one knowledge source (agendaStartProjectResolution),
// never a sheet-local copy of the rule. The daemon remains the
// authority: its propose-time refusal renders inline if a stale hint
// let an unresolvable propose through.
function agendaSchedProjectHintHtml(item, s) {
  if (s.projectRoot) {
    return '<div class="ag2-hint">Absolute directory the fired session runs under; digest-bound — recorded on the manifest so the approval covers where.</div>';
  }
  const res = agendaStartProjectResolution(item);
  const hint = `empty = ${agendaStartProjectHint(res.source)}`
    + (res.value ? ` (${res.value})` : '');
  const required = res.source === 'none';
  return `<div class="ag2-hint${required ? ' ags-hint-required' : ''}">${escapeHtml(
    required
      ? 'required — this daemon runs without a default project and the propose will be refused without one'
      : hint
  )}</div>`;
}

function agendaSchedProjectPlaceholder(item) {
  const res = agendaStartProjectResolution(item);
  return res.value ? res.value : res.source === 'none' ? 'required on this daemon' : 'resolves when proposed';
}

// The executor rows on the schedule sheet (Track AU): backend/model/
// effort selects in the same settings-derived vocabulary the start-now
// sheet uses. Digest-bound like the rest of the manifest — the amber
// revision callout already names what an edit does to the approval, so
// the swap gesture (edit executor → approval voided → re-approve) works
// from the card. Untouched = inherit; a manifest pin the option list
// doesn't know stays selectable (never silently dropped).
function agendaSchedExecutorRowsHtml() {
  const s = agendaSheetState;
  const settings = agendaStartSheetSettings;
  const withCurrent = (options, current) => {
    const values = options.map(([v]) => v);
    if (current && !values.includes(current)) options.push([current, `${current} (manifest pin)`]);
    return options;
  };
  const select = (key, id, options, selected, label) => {
    const rows = options.map(([value, text]) =>
      `<option value="${escapeHtml(value)}"${value === selected ? ' selected' : ''}>${escapeHtml(text)}</option>`);
    return `<span class="ag2-sheet-k">${escapeHtml(label)}</span>
      <select data-sheet="${key}" id="${id}" aria-label="${escapeHtml(label)}">${rows.join('')}</select>`;
  };
  const backendRow = select('execBackend', 'ag2-sched-backend', withCurrent([
    ['', 'Daemon default'], ['internal', 'Internal agent'],
    ['claude-code', 'Claude Code'], ['codex', 'Codex'], ['kimi', 'Kimi Code'],
  ], s.execBackend), s.execBackend, 'Executor');
  let modelRows = '';
  if (s.execBackend && s.execBackend !== 'internal') {
    const spec = agendaStartBackendConfig(settings || {}, s.execBackend);
    const models = withCurrent(
      [['', 'Daemon default']].concat((spec.models || []).map((m) => [m, m])), s.execModel);
    const efforts = withCurrent(
      [['', 'Daemon default']].concat((spec.efforts || []).map((e) => [e, e])), s.execEffort);
    modelRows = select('execModel', 'ag2-sched-model', models, s.execModel, 'Model')
      + select('execEffort', 'ag2-sched-effort', efforts, s.execEffort, spec.effortLabel || 'Effort');
  }
  return `<div class="ag2-sheet-grid" data-mf-field="agent_config">
      ${backendRow}${modelRows}
    </div>
    <div class="ag2-hint">Digest-bound: the approval covers who runs this goal — changing the executor revises the manifest.</div>`;
}

async function agendaSchedConfirm(button) {
  const s = agendaSheetState;
  if (!s) return;
  const item = agendaFindItem(s.itemId);
  if (!item) {
    agendaSheetClose();
    return;
  }
  const fail = (message) => {
    s.error = message;
    agendaSheetRender();
  };
  const goal = s.goal.trim();
  if (!goal) return fail('The manifest needs a goal.');
  const fire = new Date(s.when).getTime();
  if (!fire || Number.isNaN(fire)) return fail('Pick a first-run time.');
  const params = {
    op: 'propose_effect', id: item.id, goal, fire_at_ms: fire,
    orchestrate: !!s.orchestrate,
  };
  // Re-propose replaces the WHOLE manifest, so the sheet round-trips
  // every field — what the owner isn't editing rides along verbatim
  // (shape, project pin, event trigger, sealed refs), never dropped
  // because this form happens not to edit it.
  if (s.shape === 'interactive') params.interactive = true;
  if (s.projectRoot && s.projectRoot.trim()) params.project_root = s.projectRoot.trim();
  if (s.trigger) params.trigger = s.trigger;
  // The ticked on-unblock offer mints the dependency-gated trigger —
  // the EXISTING trigger vocabulary, exactly what `ctl agenda schedule
  // --on-unblock` proposes; `when` stays the arm floor.
  else if (s.onUnblock) params.trigger = { kind: 'on_unblock' };
  if (s.bindingRefs && s.bindingRefs.length) params.binding_refs = s.bindingRefs;
  if (s.repeat && !params.trigger) {
    const rec = {
      every_ms: s.repeat === 'keep' ? s.keepEveryMs : Number(s.repeat) * 864e5,
    };
    if (s.until) {
      const until = new Date(`${s.until}T23:59`).getTime();
      if (until && !Number.isNaN(until)) rec.until_ms = until;
    }
    if (s.maxRuns && Number(s.maxRuns) > 0) rec.max_occurrences = Number(s.maxRuns);
    rec.suspend_after_failures = Math.max(1, Number(s.suspend) || 3);
    params.recurrence = rec;
  }
  // Executor pins (Track AU): explicit picks only, assembled exactly as
  // the start-now sheet does — untouched selects send nothing and the
  // daemon's resolution chain fills them; a model/effort pick without an
  // explicit backend pins the backend it belongs to, so the approved
  // config can never silently re-target.
  const execConfig = {};
  if (s.execBackend) execConfig.agent = s.execBackend;
  if (s.execBackend && s.execBackend !== 'internal') {
    const spec = agendaStartBackendConfig(agendaStartSheetSettings || {}, s.execBackend);
    if (s.execModel) execConfig[spec.modelKey] = s.execModel;
    if (s.execEffort) execConfig[spec.effortKey] = s.execEffort;
  }
  if (Object.keys(execConfig).length) params.agent_config = execConfig;
  if (button) button.disabled = true;
  try {
    const resp = await daemonApi.request('api_agenda_op', params);
    if (!(resp.ok && resp.body && resp.body.item)) {
      return fail((resp.body && resp.body.error) || `propose failed (${resp.status})`);
    }
    agendaObserveServerMessage({ item: resp.body.item });
    // The revision the daemon just minted from this sheet — its digest
    // comes back on the proposed item and is what any approval binds.
    const effect = (resp.body.item.effects || [])[0];
    // The owner's own edit: acknowledge the new digest (no "revised"
    // warning for a change they just made) and pulse the chip so the
    // card visibly carries the in-place update.
    if (effect && typeof agendaAckEffectDigest === 'function') {
      agendaAckEffectDigest(effect.effect_id, effect.digest);
      agendaDigestPulse = { effectId: effect.effect_id, at: Date.now() };
    }
    // Close BEFORE the approve leg: a refused approve surfaces through
    // agendaSendOp (tab flash, card paint, and the fireability edit
    // prompt re-opening this sheet focused) — closing afterwards would
    // clobber that re-open.
    agendaSheetClose();
    agendaSheetRender();
    let approved = false;
    if (s.approveNow && effect && effect.digest) {
      approved = await agendaSendOp({ op: 'approve_effect', id: item.id, digest: effect.digest });
    }
    if (typeof showControlToast === 'function') {
      const short = effect && effect.digest ? agendaShortDigest(effect.digest) : '';
      showControlToast(approved ? 'success' : 'info', approved
        ? (params.recurrence ? `Proposed and approved — one approval covers the series (digest ${short}).`
          : params.trigger ? (params.trigger.kind === 'on_unblock'
            ? `Proposed and approved — armed; fires when the prerequisites complete (digest ${short}).`
            : `Proposed and approved — armed; fires on matching items (digest ${short}).`)
            : `Proposed and approved — fires ${agendaAbsTime(fire)} (digest ${short}).`)
        : `Proposed — waiting on an owner approval of ${short ? `digest ${short}` : 'this exact digest'}.`);
    }
  } catch (e) {
    fail(String(e && e.message || e));
  } finally {
    if (button) button.disabled = false;
  }
}

// The missed-window card's one-tap remedy — the RESCHEDULE lane's single
// propose emitter (the third lane beside the edit sheet's confirm and
// the seals adopt; each lane one emitter). Re-proposes the manifest
// VERBATIM with the floor moved to now (the one thing the miss broke)
// and re-approves the fresh digest — the tap happens on an owner
// surface, and the copy on the button names exactly that. Grain law:
// the manifest comes from the FULL item (a summary would blank the goal
// and unseal the refs); a cache miss awaits the item route directly.
async function agendaRescheduleMissed(itemId, button) {
  if (button) button.disabled = true;
  try {
    let item = agendaFullItemFor(itemId);
    if (!item) {
      try {
        const resp = await daemonApi.request('api_agenda_item', { item_id: itemId });
        if (resp.ok && resp.body && resp.body.item) {
          agendaAdoptFullItem(resp.body.item);
          item = resp.body.item;
        }
      } catch (e) { /* named refusal below */ }
    }
    if (!item) {
      agendaFlashError('could not load the full plan to reschedule — try again');
      return false;
    }
    const st = agendaEffectState(item);
    const m = st && st.manifest;
    if (!st || !m || st.kind !== 'missed') {
      agendaFlashError('nothing to reschedule — the plan changed under this card');
      agendaRenderTab();
      return false;
    }
    const params = {
      op: 'propose_effect',
      id: item.id,
      goal: m.goal,
      fire_at_ms: Date.now(),
      orchestrate: !!m.orchestrate,
    };
    if (m.interactive) params.interactive = true;
    if (m.project_root) params.project_root = m.project_root;
    if (m.agent_config) params.agent_config = m.agent_config;
    if (m.recurrence) params.recurrence = m.recurrence;
    if (m.trigger) params.trigger = m.trigger;
    if (Array.isArray(m.binding_refs) && m.binding_refs.length) params.binding_refs = m.binding_refs;
    const proposed = await agendaSendOp(params, button);
    if (!proposed) return false;
    const effect = (proposed.effects || [])[0];
    if (effect && typeof agendaAckEffectDigest === 'function') {
      agendaAckEffectDigest(effect.effect_id, effect.digest);
      agendaDigestPulse = { effectId: effect.effect_id, at: Date.now() };
    }
    const approved = effect && effect.digest
      ? await agendaSendOp({ op: 'approve_effect', id: item.id, digest: effect.digest }, button)
      : false;
    if (approved) {
      if (typeof agendaApprovalMoment === 'function') agendaApprovalMoment(approved);
      if (typeof showControlToast === 'function') {
        showControlToast('success', 'Rescheduled — the same plan runs now under a fresh approval.');
      }
    }
    return !!approved;
  } finally {
    if (button) button.disabled = false;
  }
}

// -- Preview sheet (expand) --

function agendaOpenPreviewSheet(itemId, qi, pi) {
  const item = agendaFindItem(itemId);
  const q = item && item.ask && item.ask.questions[qi];
  if (!q || !(q.previews || [])[pi]) return;
  agendaSheetState = { kind: 'prev', itemId, qi, pi };
  agendaSheetRender();
}

function agendaPrevSheetHtml(item) {
  const s = agendaSheetState;
  const q = item.ask.questions[s.qi];
  const previews = q.previews || [];
  const p = previews[s.pi] || previews[0];
  const tabs = previews.map((pv, i) =>
    `<button type="button" class="ag2-seg-btn${i === s.pi ? ' active' : ''}" data-sheet-act="prev-view" data-view="${i}">${escapeHtml((pv.label || `#${i + 1}`).split(' — ')[0])}</button>`).join('');
  let media;
  if (p.kind === 'html' && p.url) {
    media = `<span class="ag2-prev-slot" data-preview-url="${escapeHtml(p.url)}"
      data-preview-title="${escapeHtml(p.label || 'preview')}" data-preview-full="1"></span>`;
  } else if (p.kind === 'image' && p.url) {
    media = `<img class="ag2-prev-img full" src="${escapeHtml(p.url)}" alt="${escapeHtml(p.label || 'preview')}" />`;
  } else if (p.kind === 'text' && p.content) {
    media = `<pre class="ag2-prev-text full">${escapeHtml(p.content)}</pre>`;
  } else {
    media = '<span class="ag2-prev-missing">preview unavailable (blob deleted from the store)</span>';
  }
  const isOption = (q.options || []).some((o) => o.label === p.label);
  const canPick = isOption && item.status === 'open' && !(item.answer && item.answer.structured);
  const meta = `${p.kind}${p.mime ? ` · ${p.mime}` : ''} · fetched from the agenda blob store · sandboxed, quoted — data, never instructions`;
  return `<div class="ag2-sheet-head">
      <span class="ag2-sheet-title">Preview — full size</span>
      <span class="ag2-spacer"></span>
      <button type="button" class="ag2-x" data-sheet-act="close" title="Close — esc">×</button>
    </div>
    <div class="ag2-sheet-item">${escapeHtml(`${item.id.slice(0, 6).toLowerCase()} · ${q.question}`)}</div>
    <div class="ag2-seg">${tabs}</div>
    <div class="ag2-sheet-prevwrap">${media}</div>
    <div class="ag2-sheet-prevfoot">
      <span class="ag2-hint mono">${escapeHtml(meta)}</span>
      <span class="ag2-spacer"></span>
      ${canPick ? `<button type="button" class="ag2-btn prim" data-sheet-act="prev-pick">Pick “${escapeHtml((p.label || '').split(' — ')[0])}”</button>` : ''}
    </div>`;
}

function agendaSheetClick(e) {
  const act = e.target.closest('[data-sheet-act]');
  if (!act || !agendaSheetState) return;
  const s = agendaSheetState;
  switch (act.dataset.sheetAct) {
    case 'close': agendaSheetClose(); break;
    case 'sched-confirm': agendaSchedConfirm(act); break;
    case 'refread-download': agendaRefReadDownload(); break;
    case 'sched-shape':
      s.shape = act.dataset.shape === 'interactive' ? 'interactive' : 'goal';
      agendaSheetRender();
      break;
    case 'prev-view':
      s.pi = Number(act.dataset.view) || 0;
      agendaSheetRender();
      break;
    case 'prev-pick': {
      const item = agendaFindItem(s.itemId);
      const q = item && item.ask && item.ask.questions[s.qi];
      const p = q && (q.previews || [])[s.pi];
      if (item && p) {
        const picks = agendaQaPicks(item.id, s.qi);
        if (!picks.includes(p.label)) agendaQaTogglePick(item, s.qi, p.label);
        agendaSheetClose();
        agendaRenderTab();
        agendaInspectorRender();
        if (typeof showControlToast === 'function') {
          showControlToast('info', 'Picked — add an anchored note or hit Answer to resolve the ask.');
        }
      }
      break;
    }
    default: break;
  }
}

function agendaSheetInput(e) {
  const s = agendaSheetState;
  if (!s || s.kind !== 'sched') return;
  const t = e.target.closest('[data-sheet]');
  if (!t) return;
  const key = t.dataset.sheet;
  const structural = key === 'repeat' || key === 'approveNow' || key === 'execBackend'
    || key === 'onUnblock';
  if (t.type === 'checkbox') s[key] = !!t.checked;
  else s[key] = t.value;
  if (key === 'execBackend') {
    // A backend change swaps the model/effort vocabulary — stale picks
    // from the previous backend must not ride along silently.
    s.execModel = '';
    s.execEffort = '';
  }
  if (structural && e.type === 'change') agendaSheetRender();
}

// ---- Reminder-policy bell popover ----

let agendaBellOpen = false;
let agendaBellWired = false;

function agendaBellToggle() {
  agendaBellOpen = !agendaBellOpen;
  agendaBellRender();
}

// Returns true when it actually closed (the Escape chain).
function agendaBellClose() {
  if (!agendaBellOpen) return false;
  agendaBellOpen = false;
  agendaBellRender();
  return true;
}

function agendaEnsureBellHost() {
  let host = document.getElementById('ag2-bell-pop');
  if (host) return host;
  host = document.createElement('div');
  host.id = 'ag2-bell-pop';
  host.hidden = true;
  host.innerHTML = '<div class="ag2-bell-overlay"></div><div class="ag2-bell-panel" role="dialog" aria-label="Reminder delivery policy"></div>';
  document.body.appendChild(host);
  host.querySelector('.ag2-bell-overlay').addEventListener('click', () => agendaBellClose());
  return host;
}

function agendaBellRender() {
  const host = agendaEnsureBellHost();
  const panel = host.querySelector('.ag2-bell-panel');
  if (!agendaBellOpen) {
    host.hidden = true;
    return;
  }
  host.hidden = false;
  // Anchor under the bell button (fixed positioning).
  const bell = document.getElementById('ag2-bell');
  if (bell) {
    const rect = bell.getBoundingClientRect();
    panel.style.top = `${Math.round(rect.bottom + 8)}px`;
    panel.style.right = `${Math.round(Math.max(12, window.innerWidth - rect.right))}px`;
  }
  const policy = agendaReminderPolicy;
  if (!policy) {
    panel.innerHTML = '<div class="ag2-hint">Reminder policy unavailable — the daemon has not served one yet.</div>';
    return;
  }
  const minToHhmm = (min) =>
    `${String(Math.floor(min / 60)).padStart(2, '0')}:${String(min % 60).padStart(2, '0')}`;
  const quiet = policy.quiet_hours || null;
  const seg = ['info', 'attention', 'urgent'].map((v) =>
    `<button type="button" class="ag2-seg-btn u-${v}${policy.default_urgency === v ? ' active' : ''}" data-bell-urg="${v}">${v}</button>`).join('');
  panel.innerHTML = `
    <div class="ag2-bell-head">
      <span class="ag2-bell-title">Reminder delivery</span>
      <span class="ag2-spacer"></span>
      <span class="ag2-hint mono">owner policy</span>
    </div>
    <div class="ag2-bell-row">
      <div class="ag2-bell-rowmain">
        <div class="ag2-bell-k">Deliver reminders</div>
        <div class="ag2-hint">Due times notify you at their instant.</div>
      </div>
      <button type="button" class="ag2-tog${policy.enabled ? ' on' : ''}" data-bell-act="enabled" role="switch" aria-checked="${policy.enabled}" aria-label="Deliver reminders"><span class="ag2-tog-knob"></span></button>
    </div>
    <div>
      <div class="ag2-bell-k">Default loudness</div>
      <div class="ag2-seg">${seg}</div>
    </div>
    <div class="ag2-bell-row">
      <div class="ag2-bell-rowmain">
        <div class="ag2-bell-k">Quiet hours</div>
        <div class="ag2-hint">Defers every reminder — urgent included. Approved scheduled sessions still fire.</div>
      </div>
      <button type="button" class="ag2-tog${quiet ? ' on' : ''}" data-bell-act="quiet" role="switch" aria-checked="${!!quiet}" aria-label="Quiet hours"><span class="ag2-tog-knob"></span></button>
    </div>
    ${quiet ? `<div class="ag2-bell-times">
      <input type="time" data-bell-time="start" value="${minToHhmm(quiet.start_min)}" aria-label="Quiet hours start" />
      <span class="ag2-hint">to</span>
      <input type="time" data-bell-time="end" value="${minToHhmm(quiet.end_min)}" aria-label="Quiet hours end" />
      <span class="ag2-hint">${agendaQuietNow() ? 'quiet now' : ''}</span>
    </div>` : ''}
    <div class="ag2-bell-row">
      <div class="ag2-bell-rowmain">
        <div class="ag2-bell-k">Fold stale reminders into a digest</div>
        <div class="ag2-hint">Anything older than this arrives summarized, not one by one.</div>
      </div>
      <input type="number" min="1" max="336" class="ag2-sheet-num" data-bell-act="stale" value="${policy.staleness_hours}" aria-label="Staleness hours" />
      <span class="ag2-hint">hours</span>
    </div>
    <div class="ag2-bell-foot">settings.manage · POST /api/agenda/reminders/policy — an agenda.write grant can’t make its own item louder.</div>`;
  if (!agendaBellWired) {
    agendaBellWired = true;
    panel.addEventListener('click', (e) => {
      const urg = e.target.closest('[data-bell-urg]');
      if (urg) {
        agendaSendPolicyPatch({ default_urgency: urg.dataset.bellUrg }, urg).then(() => agendaBellRender());
        return;
      }
      const act = e.target.closest('[data-bell-act]');
      if (!act) return;
      const current = agendaReminderPolicy || {};
      if (act.dataset.bellAct === 'enabled') {
        agendaSendPolicyPatch({ enabled: !current.enabled }, act).then(() => agendaBellRender());
      } else if (act.dataset.bellAct === 'quiet') {
        agendaSendPolicyPatch({
          quiet_hours: current.quiet_hours ? null : { start_min: 22 * 60, end_min: 7 * 60 + 30 },
        }, act).then(() => agendaBellRender());
      }
    });
    panel.addEventListener('change', (e) => {
      const time = e.target.closest('[data-bell-time]');
      const current = agendaReminderPolicy || {};
      if (time && current.quiet_hours) {
        const [h, m] = String(time.value || '').split(':').map(Number);
        if (Number.isNaN(h) || Number.isNaN(m)) return;
        const next = { ...current.quiet_hours };
        next[time.dataset.bellTime === 'start' ? 'start_min' : 'end_min'] = h * 60 + m;
        agendaSendPolicyPatch({ quiet_hours: next }, time).then(() => agendaBellRender());
        return;
      }
      const stale = e.target.closest('[data-bell-act="stale"]');
      if (stale) {
        const hours = Math.max(1, Math.min(336, Number(stale.value) || 0));
        if (hours) {
          agendaSendPolicyPatch({ staleness_hours: hours }, stale).then(() => agendaBellRender());
        }
      }
    });
  }
}

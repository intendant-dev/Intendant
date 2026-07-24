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

function agendaOpenInspector(id) {
  if (!agendaFindItem(id)) return;
  agendaSelId = id;
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
  const item = agendaSelId ? agendaFindItem(agendaSelId) : null;
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
        ${agendaInspRefsHtml(item)}
        ${agendaInspThreadHtml(item)}
      </div>
    </div>`;
  });
  agendaHydratePreviewFrames(host);
}

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
    const railDoor = hasAsk
      ? `<button type="button" class="ag2-linkbtn" data-act="rail-open">Open on the question rail ›</button>`
      : '';
    const note = hasAsk
      ? `Rich ask #${item.ask.ask_id} — parked on every dashboard’s question rail; answering here resolves it everywhere. Nothing blocks, nothing expires.`
      : `Parked ${agendaRelTime((item.provenance || {}).created_ms)} — the answer lands on the item; a live asking session hears it, and an ended one’s successor reads it at session start.`;
    composer = `${hasAsk ? '' : `<div class="ag2-insp-qtext">${escapeHtml(item.title)}</div>`}
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
    };
    const [tone, stateLabel] = states[st.kind] || states.finished;
    const rows = [];
    const R = (k, v, mono) =>
      rows.push(`<div class="ag2-eff-k">${escapeHtml(k)}</div><div class="ag2-eff-v${mono ? ' mono' : ''}">${escapeHtml(v)}</div>`);
    R('when', st.rec
      ? `every ${agendaCadenceLabel(st.rec.every_ms)} · next ${agendaAbsTime(st.next)}`
      : `${agendaAbsTime(m.fire_at_ms)} (${agendaRelTime(m.fire_at_ms)})`);
    if (st.rec && (st.rec.until_ms || st.rec.max_occurrences)) {
      R('ends', `${st.rec.until_ms ? agendaAbsTime(st.rec.until_ms) : ''}${st.rec.max_occurrences ? `${st.rec.until_ms ? ' or ' : ''}after ${st.rec.max_occurrences} runs` : ''}`);
    }
    R('shape', `${m.interactive ? 'interactive — opens and waits for you' : 'goal run — autonomous, writes back'} · ${m.orchestrate ? 'orchestrated' : 'direct'}`);
    R('project', m.project_root || 'inherited at fire time: parking session’s root, else daemon default', !!m.project_root);
    const cfg = m.agent_config || null;
    R('config', cfg
      ? `${[cfg.agent, ...Object.entries(cfg).filter(([k]) => k !== 'agent').map(([, v]) => v)].filter(Boolean).join(' · ')} — explicit, recorded on the manifest`
      : 'inherits daemon defaults (Settings → reasoning)', !!cfg);
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
      lastRun = `<div class="ag2-eff-lastrun">
        <div class="ag2-eff-lastrun-head">
          ${agendaChipHtml(`last run · ${run.state}`, runTone)}
          <span class="ag2-hint">${escapeHtml(`${agendaRelTime(run.at_ms)} · occurrence ${run.occurrence_id}`)}</span>
          ${sessionLink}
        </div>
        ${run.note ? `<div class="ag2-eff-note">${escapeHtml(run.note)}</div>` : ''}
      </div>`;
    }
    const acts = [];
    const A = (label, act, cls, tip) =>
      acts.push(`<button type="button" class="ag2-btn ${cls || ''}" data-act="${act}"${tip ? ` title="${escapeHtml(tip)}"` : ''}>${escapeHtml(label)}</button>`);
    if (st.kind === 'pending') {
      A('Approve this exact plan', 'eff-approve', 'prim',
        `Binds digest ${String(e.digest || '').slice(0, 8)}… — any edit voids it`);
      A('Edit schedule…', 'sched');
    } else if (st.kind === 'armed') {
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
    body = `<div class="ag2-effcard t-${tone}">
      <div class="ag2-effcard-head">
        <span class="ag2-eff-dot"></span>
        <span class="ag2-eff-state">${escapeHtml(stateLabel)}</span>
        <span class="ag2-spacer"></span>
        <span class="ag2-hint mono">digest ${escapeHtml(String(e.digest || '').slice(0, 10))}…${e.approval ? ' · approved' : ' · unapproved'}</span>
      </div>
      <div class="ag2-eff-grid">${rows.join('')}</div>
      <div class="ag2-eff-goal">${escapeHtml(m.goal || '')}</div>
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
  const deps = (item.relies_on || []).map((d) => {
    const target = agendaFindItem(d.target_id);
    const state = !target ? ['missing', 'rose']
      : target.status === 'done' ? ['satisfied', 'green']
        : target.status === 'retired' ? ['retired — review', 'amber']
          : ['open', 'neutral'];
    const title = target ? target.title : `${d.target_id.slice(0, 10)}…`;
    return `<div class="ag2-insp-dep">
      ${agendaChipHtml(state[0], state[1])}
      ${target
    ? `<a class="ag2-insp-deplink" data-open-item="${escapeHtml(target.id)}">waits on “${escapeHtml(title)}”</a>`
    : `<span class="ag2-insp-deplink">waits on ${escapeHtml(title)}</span>`}
      <button type="button" class="ag2-x" data-remove-dep="${escapeHtml(d.target_id)}" title="Drop the edge (the log keeps history)">×</button>
    </div>`;
  });
  const empty = !blockers.length && !deps.length
    ? '<div class="ag2-hint">Nothing gates this item.</div>' : '';
  // Blockers and dependency edges describe OPEN work — the daemon refuses
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
  const partners = agendaRelationPartners(item);
  const rels = [...partners].map((pid) => {
    const target = agendaFindItem(pid);
    if (!target) return '';
    return `<span class="ag2-relchip">
      <a data-open-item="${escapeHtml(pid)}">${escapeHtml(target.title.slice(0, 34))}</a>
      <button type="button" class="ag2-x" data-remove-rel="${escapeHtml(pid)}" title="Remove the see-also edge (the log keeps history)">×</button>
    </span>`;
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
      target = `<a class="ag2-ref-loc" data-open-claim="${escapeHtml(r.locator)}" title="${escapeHtml(r.locator)}">${escapeHtml(label || 'claim ')}${escapeHtml(label ? '' : String(r.locator).slice(0, 12))}</a>`;
    } else {
      if (r.digest) hasFileDigest = true;
      target = `<span class="ag2-ref-loc" title="${escapeHtml(r.digest ? `sha256 ${r.digest.slice(0, 16)}… recorded at attach — the digest travels; blobs never do` : r.locator)}">${escapeHtml(label + r.locator)}</span>`;
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
  return agendaSelId ? agendaFindItem(agendaSelId) : null;
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
    // Either order works — the daemon resolves which side stores the edge.
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
        agendaSendOp({ op: 'approve_effect', id: item.id, digest: st.effect.digest }, act);
      }
      break;
    }
    case 'eff-revoke': agendaSendOp({ op: 'revoke_effect', id: item.id }, act); break;
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
  } else if (t.dataset.actChange === 'rel-add') {
    const v = t.value;
    t.value = '';
    if (v) agendaSendOp({ op: 'add_relates_to', id: item.id, target_id: v }, t);
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
// anything else → file path.
function agendaInspInferRefType(locator) {
  if (/^https?:\/\//i.test(locator)) return 'url';
  if (/^[0-9a-f]{12,}$/i.test(locator)) return 'memory';
  if (agendaSessions && agendaSessions[locator]) return 'session';
  if (/^sess-/.test(locator)) return 'session';
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
      : agendaSchedSheetHtml(item);
  });
  agendaHydratePreviewFrames(panel);
}

// -- Schedule sheet --

function agendaOpenSchedSheet(itemId) {
  const item = agendaFindItem(itemId);
  if (!item) return;
  const st = agendaEffectState(item);
  const m = st && st.manifest;
  const toLocal = (ms) => {
    const d = new Date(ms);
    const p = (n) => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
  };
  const rec = m && m.recurrence;
  agendaSheetState = {
    kind: 'sched',
    itemId,
    goal: m ? m.goal : agendaStartGoalStatement(item),
    when: toLocal(st && st.next > Date.now() ? st.next : Date.now() + 864e5),
    repeat: rec ? String(Math.round(rec.every_ms / 864e5)) : '',
    until: '',
    maxRuns: rec && rec.max_occurrences ? String(rec.max_occurrences) : '',
    suspend: rec && rec.suspend_after_failures ? String(rec.suspend_after_failures) : '3',
    orchestrate: !!(m && m.orchestrate),
    approveNow: true,
    voids: !!(st && st.effect.approval),
    error: '',
  };
  agendaSheetRender();
}

function agendaSchedSheetHtml(item) {
  const s = agendaSheetState;
  const standing = !!s.repeat;
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
        <span class="ag2-sheet-k" title="Consecutive failed or unknown outcomes that suspend the series — missed runs from daemon downtime don’t count">Suspend after</span>
        <div class="ag2-sheet-inline">
          <input type="number" min="1" max="10" class="ag2-sheet-num" data-sheet="suspend" value="${escapeHtml(s.suspend)}" aria-label="Suspend after failures" />
          <span class="ag2-hint">failed runs in a row</span>
        </div>
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
      <textarea rows="6" data-sheet="goal" data-fkey="sheet-goal" aria-label="Goal">${escapeHtml(s.goal)}</textarea>
      <div class="ag2-hint">Reviewed at approval time. Data under review — never instructions to whoever reads the agenda.</div>
    </div>
    <div class="ag2-sheet-grid">
      <span class="ag2-sheet-k">First run</span>
      <input type="datetime-local" data-sheet="when" value="${escapeHtml(s.when)}" aria-label="First run" />
      <span class="ag2-sheet-k">Repeats</span>
      <select data-sheet="repeat" aria-label="Repeats">
        <option value=""${s.repeat === '' ? ' selected' : ''}>never — one run</option>
        <option value="1"${s.repeat === '1' ? ' selected' : ''}>every day</option>
        <option value="7"${s.repeat === '7' ? ' selected' : ''}>every week</option>
        <option value="14"${s.repeat === '14' ? ' selected' : ''}>every two weeks</option>
      </select>
    </div>
    ${standingBlock}
    <label class="ag2-check"><input type="checkbox" data-sheet="orchestrate"${s.orchestrate ? ' checked' : ''}>Orchestrated run (a conductor session fans out sub-agents)</label>
    <label class="ag2-check top"><input type="checkbox" data-sheet="approveNow"${s.approveNow ? ' checked' : ''}><span>Approve immediately<br><span class="ag2-hint">You’re on an owner surface. Any later edit mints a new digest and voids this approval.</span></span></label>
    ${s.voids ? '<div class="ag2-sheet-callout t-amber">This revises the manifest — the current approval becomes void until re-approved.</div>' : ''}
    ${s.error ? `<div class="ag2-sheet-error">${escapeHtml(s.error)}</div>` : ''}
    <div class="ag2-row-end">
      <button type="button" class="ag2-btn ghost" data-sheet-act="close">Cancel</button>
      <button type="button" class="ag2-btn prim" data-sheet-act="sched-confirm">${s.approveNow ? 'Propose & approve' : 'Propose schedule'}</button>
    </div>`;
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
  if (s.repeat) {
    const rec = { every_ms: Number(s.repeat) * 864e5 };
    if (s.until) {
      const until = new Date(`${s.until}T23:59`).getTime();
      if (until && !Number.isNaN(until)) rec.until_ms = until;
    }
    if (s.maxRuns && Number(s.maxRuns) > 0) rec.max_occurrences = Number(s.maxRuns);
    rec.suspend_after_failures = Math.max(1, Number(s.suspend) || 3);
    params.recurrence = rec;
  }
  if (button) button.disabled = true;
  try {
    const resp = await daemonApi.request('api_agenda_op', params);
    if (!(resp.ok && resp.body && resp.body.item)) {
      return fail((resp.body && resp.body.error) || `propose failed (${resp.status})`);
    }
    agendaObserveServerMessage({ item: resp.body.item });
    let approved = false;
    if (s.approveNow) {
      // Approve exactly the revision the daemon just minted from this
      // sheet — its digest comes back on the proposed item.
      const effect = (resp.body.item.effects || [])[0];
      if (effect && effect.digest) {
        approved = await agendaSendOp({ op: 'approve_effect', id: item.id, digest: effect.digest });
      }
    }
    agendaSheetClose();
    agendaSheetRender();
    if (typeof showControlToast === 'function') {
      showControlToast(approved ? 'success' : 'info', approved
        ? (params.recurrence ? 'Proposed and approved — one approval covers the series.' : `Proposed and approved — fires ${agendaAbsTime(fire)}.`)
        : 'Proposed — waiting on an owner approval of this exact digest.');
    }
  } catch (e) {
    fail(String(e && e.message || e));
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
  const structural = key === 'repeat' || key === 'approveNow';
  if (t.type === 'checkbox') s[key] = !!t.checked;
  else s[key] = t.value;
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

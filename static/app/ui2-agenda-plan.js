// Agenda tab Upcoming lens (redesign slice C): a 14-day day-grouped
// timeline of what will actually happen — reminder deliveries and
// scheduled-session fires. Registered in AGENDA_LENSES
// (ui2-agenda-cards.js) between "Graph" and "Questions" as a
// custom-surface lens: render() owns #ag2-groups, deactivate() stops the
// refresh timer. Data and derivations come from ui2-agenda.js
// (agendaItems, agendaEffectState, agendaReminderPolicy); rows carry
// data-open-item, so the cards fragment's existing #ag2-groups click
// delegation opens the slice-A inspector with no listeners of our own.
//
// Instants are SERVED, never recomputed (the whole point of PR #576's
// display fields): `effects[].next_fire_ms` is the planner's next real
// fire instant (absent when nothing will fire — unapproved, suspended,
// spent, exhausted) and item-level `deferred_until` is the planner's
// quiet-hours deferral instant (absent when nothing defers). The only
// client arithmetic is presentation: projecting up to two further
// standing instants by pure period addition anchored on the served
// instant, and a wall-clock quiet-window chip on fire rows — both
// annotated at their sites.
//
// Item-authored text (titles) renders through escapeHtml; every other
// string here is static vocabulary, a number, or a daemon-minted digest
// prefix (escaped anyway). No pulsing dots under prefers-reduced-motion:
// the dots pulse via the shared ui2-pulse keyframes, which the global
// reduced-motion rule in 16-styles-v2-tokens.css already collapses.
//
// Lifecycle: static DOM (no rAF), but relative correctness drifts — a
// 60s re-render keeps Today/now rows honest. The timer is armed only
// while this lens is the active surface and the document is visible:
// deactivate() (the render pass's sweep) and visibilitychange → hidden
// tear it down, and a tick that finds the agenda tab hidden disarms
// itself (at most one no-op tick after a tab switch; the render path
// re-arms on re-entry). No background timers persist.

const AGENDA_PLAN_DAY_MS = 24 * 60 * 60 * 1000;
const AGENDA_PLAN_HORIZON_DAYS = 14;
// The served instant plus at most this many projected future instants
// per standing series.
const AGENDA_PLAN_STANDING_INSTANTS = 3;

let agendaPlanTimer = null;

// ---- Lens surface (the AGENDA_LENSES render/deactivate pair) ----

function agendaPlanRenderLens(host) {
  const days = agendaPlanDays();
  const foot = '<div class="ag2-plan-foot">Fire instants come from the daemon’s planner (served on the item DTO); day grouping and cadence projection rendered at view time — quiet hours defer reminders, never approved sessions.</div>';
  if (!days.length) {
    host.innerHTML = `<div class="ag2-plan">
      <div class="ag2-empty">
        <div class="ag2-empty-glyph">◍</div>
        <div class="ag2-empty-title">Nothing on the horizon</div>
        <div class="ag2-empty-hint">No armed fires and no future reminders in the next ${AGENDA_PLAN_HORIZON_DAYS} days.</div>
      </div>
      ${foot}
    </div>`;
  } else {
    host.innerHTML = `<div class="ag2-plan">
      ${days.map(agendaPlanDayHtml).join('')}
      ${foot}
    </div>`;
  }
  agendaPlanEnsureTimer();
}

function agendaPlanTeardown() {
  if (agendaPlanTimer !== null) {
    clearInterval(agendaPlanTimer);
    agendaPlanTimer = null;
  }
}

function agendaPlanShouldRun() {
  return agendaLens === 'plan' && !document.hidden && agendaTabVisible();
}

function agendaPlanEnsureTimer() {
  if (agendaPlanTimer !== null || !agendaPlanShouldRun()) return;
  agendaPlanTimer = setInterval(() => {
    if (!agendaPlanShouldRun()) {
      // Lens switched under us, tab hidden, or document hidden: disarm
      // entirely — the render path re-arms on the next paint.
      agendaPlanTeardown();
      return;
    }
    agendaRenderTab();
  }, 60000);
}

// ---- Time presentation ----

// Fixed-width 24h clock text for the mono time column (locale AM/PM
// would break the column alignment the design's 44px slot assumes).
function agendaPlanHm(ms) {
  const d = new Date(ms);
  const pad = (n) => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function agendaPlanDayLabel(ms) {
  const d = new Date(ms);
  const today = new Date();
  const tomorrow = new Date();
  tomorrow.setDate(tomorrow.getDate() + 1);
  if (d.toDateString() === today.toDateString()) return 'Today';
  if (d.toDateString() === tomorrow.toDateString()) return 'Tomorrow';
  return d.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' });
}

// Whether a fire instant falls inside the owner's quiet window — a
// DISPLAY ANNOTATION ONLY on session rows (the "in quiet hours" chip),
// never a deferral computation: approved sessions are never deferred
// (the A3 ruling), so there is no served deferral instant to read and a
// local wall-clock membership check is the honest annotation. Client
// twin of QuietHours::contains (minutes since local midnight, window may
// cross it).
function agendaPlanInQuietAt(ms) {
  const quiet = agendaReminderPolicy && agendaReminderPolicy.quiet_hours;
  if (!quiet || quiet.start_min === quiet.end_min) return false;
  const d = new Date(ms);
  const minute = d.getHours() * 60 + d.getMinutes();
  if (quiet.start_min < quiet.end_min) {
    return minute >= quiet.start_min && minute < quiet.end_min;
  }
  return minute >= quiet.start_min || minute < quiet.end_min;
}

// ---- Row composition ----

// The effective delivery loudness for an item's reminder: the owner's
// per-item override, else the policy default (mirrors
// ReminderPolicy::urgency_for).
function agendaPlanUrgency(item) {
  const policy = agendaReminderPolicy || {};
  const overrides = policy.item_urgency || {};
  return overrides[item.id] || policy.default_urgency || 'attention';
}

const AGENDA_PLAN_URGENCY_TONES = {
  urgent: 'rose',
  attention: 'amber',
  info: 'sky',
  mute: 'neutral',
};

// Every event on the horizon: {at, item, what, tone, pulse, chips, note}.
// Deliberately unfiltered (like the graph lens): the timeline shows the
// whole open ledger's future; search and the lens-bar filter chips keep
// applying to the card lenses only.
function agendaPlanEvents() {
  const now = Date.now();
  const horizon = now + AGENDA_PLAN_HORIZON_DAYS * AGENDA_PLAN_DAY_MS;
  const policy = agendaReminderPolicy || {};
  const remindersOff = policy.enabled === false;
  const events = [];
  (agendaItems || []).forEach((item) => {
    if (item.status !== 'open') return;
    // Reminder delivery. Overdue reminders (due before now) are the
    // Needs-you lens's business; this lens shows the future only.
    if (item.due_ms && item.due_ms > now && item.due_ms <= horizon) {
      const urgency = agendaPlanUrgency(item);
      const chips = [agendaChipHtml(urgency,
        AGENDA_PLAN_URGENCY_TONES[urgency] || 'amber',
        'Delivery loudness — owner policy')];
      // Served planner derivation: when quiet hours would defer this
      // delivery, the instant it would actually happen. Absent = nothing
      // defers (including reminders disabled) — absence claims nothing.
      if (item.deferred_until) {
        chips.push(agendaChipHtml(`deferred → ${agendaPlanHm(item.deferred_until)}`,
          'amber', 'Quiet hours defer every reminder — urgent included', true));
      }
      events.push({
        at: item.due_ms, item, what: 'reminder', tone: 'sky', pulse: '',
        chips,
        note: remindersOff ? 'reminders are globally off — this will not fire' : null,
      });
    }
    const st = agendaEffectState(item);
    if (!st) return;
    const digest8 = String(st.effect.digest || '').slice(0, 8);
    if (st.kind === 'running') {
      events.push({
        at: now, item, what: 'session running now', tone: 'iris',
        pulse: 'fast', chips: [], note: null,
      });
    } else if (st.kind === 'pending') {
      events.push({
        at: Math.max(st.manifest.fire_at_ms, now), item,
        what: 'proposed session — will not fire', tone: 'amber', pulse: 'slow',
        chips: [agendaChipHtml('needs approval', 'amber',
          `Approval binds digest ${digest8}… exactly`)],
        note: `waiting on your approval of digest ${digest8}… — proposing carries no authority`,
      });
    } else if (st.kind === 'suspended') {
      events.push({
        at: now, item, what: 'standing series suspended', tone: 'amber',
        pulse: 'slow',
        chips: [agendaChipHtml(`${st.effect.consecutive_failures} failures`, 'rose',
          'Consecutive non-success outcomes since the last approval')],
        note: 'nothing fires while suspended — re-approving the unchanged digest re-arms the series',
      });
    }
    // Fire rows ride the SERVED planner instant exclusively: absent
    // means nothing will fire (unapproved, suspended, spent, exhausted),
    // so these compose correctly with the states above by construction —
    // a running standing series still shows its next instants.
    const nextFire = st.effect.next_fire_ms;
    if (nextFire == null || nextFire > horizon) return;
    const quietChip = (at) => (agendaPlanInQuietAt(at)
      ? [agendaChipHtml('in quiet hours', 'sky',
        'Approved sessions are never deferred — approving an off-hours run was the explicit decision', true)]
      : []);
    if (!st.rec) {
      events.push({
        at: nextFire, item, what: 'session fires — one-shot', tone: 'green',
        pulse: '', chips: quietChip(nextFire), note: null,
      });
      return;
    }
    // Standing series: the served instant, then up to two further
    // instants by pure period addition — arithmetic projections of the
    // approved cadence anchored on the daemon's served instant, NOT
    // planner logic (the planner's own judgments — suspension, spend,
    // exhaustion — are already baked into next_fire_ms's presence).
    // until_ms/max_occurrences are manifest bounds applied to the
    // projection; the occurrence index of an instant is its offset from
    // fire_at_ms on the cadence grid.
    const every = st.rec.every_ms > 0 ? st.rec.every_ms : 0;
    const requested = (st.effect.requested || []).length;
    const what = `standing run · every ${agendaCadenceLabel(st.rec.every_ms)}`;
    for (let i = 0, at = nextFire; i < AGENDA_PLAN_STANDING_INSTANTS; i++, at += every) {
      if (at > horizon) break;
      if (st.rec.until_ms && at > st.rec.until_ms) break;
      if (st.rec.max_occurrences) {
        const index = Math.round((at - st.manifest.fire_at_ms) / (every || 1));
        if (index >= st.rec.max_occurrences) break;
      }
      events.push({
        at, item, what, tone: 'green', pulse: '', chips: quietChip(at),
        note: i === 0 && requested
          ? `plus ${requested} owner-requested extra occurrence${requested === 1 ? '' : 's'} pending`
          : null,
      });
      if (!every) break;
    }
    if (st.rec.until_ms && st.rec.until_ms <= horizon) {
      events.push({
        at: st.rec.until_ms, item, what: 'standing series ends',
        tone: 'neutral', pulse: '', chips: [], note: null,
      });
    }
  });
  events.sort((a, b) => a.at - b.at);
  return events;
}

// Events grouped into day buckets: {key, label, hint, rows}.
function agendaPlanDays() {
  const days = [];
  agendaPlanEvents().forEach((event) => {
    const key = new Date(event.at).toDateString();
    let day = days.find((d) => d.key === key);
    if (!day) {
      day = { key, label: agendaPlanDayLabel(event.at), rows: [] };
      days.push(day);
    }
    day.rows.push(event);
  });
  days.forEach((day) => {
    day.hint = `${day.rows.length} event${day.rows.length === 1 ? '' : 's'}`;
  });
  return days;
}

// ---- Render ----

function agendaPlanRowHtml(row) {
  const title = String(row.item.title || '');
  const shown = title.length > 44 ? `${title.slice(0, 43)}…` : title;
  const dotClasses = ['ag2-plan-dot', `t-${row.tone}`];
  if (row.pulse) dotClasses.push('pulse', row.pulse);
  return `<div class="ag2-plan-row" data-open-item="${escapeHtml(row.item.id)}" role="button" tabindex="0">
    <span class="${dotClasses.join(' ')}" aria-hidden="true"></span>
    <div class="ag2-plan-line">
      <span class="ag2-plan-time">${agendaPlanHm(row.at)}</span>
      <span class="ag2-plan-title">${escapeHtml(shown)}</span>
      <span class="ag2-plan-what">${escapeHtml(row.what)}</span>
      ${row.chips.join('')}
    </div>
    ${row.note ? `<div class="ag2-plan-note">${escapeHtml(row.note)}</div>` : ''}
  </div>`;
}

function agendaPlanDayHtml(day) {
  return `<div class="ag2-plan-day">
    <div class="ag2-plan-day-head">
      <span class="ag2-plan-day-label">${escapeHtml(day.label)}</span>
      <span class="ag2-plan-day-hint">${escapeHtml(day.hint)}</span>
    </div>
    <div class="ag2-plan-rail">${day.rows.map(agendaPlanRowHtml).join('')}</div>
  </div>`;
}

// ---- Wire (the stop/resume conduit; see the fragment header) ----

{
  const wire = () => {
    document.addEventListener('visibilitychange', () => {
      if (document.hidden) {
        agendaPlanTeardown();
        return;
      }
      if (agendaLens === 'plan' && agendaTabVisible()) {
        // Resume through the render pass: repaint the drifted relative
        // rows immediately and re-arm the timer.
        agendaRenderTab();
      }
    });
  };
  if (document.readyState === 'complete') wire();
  else document.addEventListener('DOMContentLoaded', wire, { once: true });
}

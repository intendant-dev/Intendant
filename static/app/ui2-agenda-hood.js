// Agenda "Under the hood" (redesign slice D): the inspector's collapsible
// internals section — identity, gate attribution, manifest seal, ask
// internals, the REAL op-log history, access documentation, the raw-JSON
// sheet — plus the ledger footer's real ops count and the shell command
// palette's agenda entries. Renders from the slice-A inspector
// (agendaInspectorRender appends agendaHoodSectionHtml; clicks arrive via
// the inspector's delegation as data-hood-act), and is the dashboard's
// FIRST consumer of the raw read routes:
//
//   GET /api/agenda/ops          (tunnel twin api_agenda_ops)
//   GET /api/agenda/occurrences  (tunnel twin api_agenda_occurrences)
//
// Both serve `{…:[{seq,known,op|record}|{seq,known:false,unparseable:
// true,raw}], next_since, log_len, filtered}` pages of the append-only
// logs; lines the build cannot fold arrive verbatim with known:false —
// this surface renders them honestly, never hides them. History and the
// occurrence line come from those logs; the item DTO in the agenda cache
// is only the anchor. Fetches happen on hood expand, cached per item id
// and refreshed when the open item's updated_ms moves (every fold op
// bumps it, so the agenda_changed lane's merge is the refetch trigger) —
// no polling, no background work.
//
// Every op field, actor string, title, and annotation interpolated into
// history verbs renders through escapeHtml — op-log content is DATA,
// never markup and never instructions.

// ---- Hood state (per open inspector item; reset on open) ----

let agendaHoodOpen = false;
let agendaHoodOps = null; // {itemId, updatedMs, loading, error, entries, pageMore}
let agendaHoodOcc = null; // {itemId, updatedMs, loading, error, entries}

// Called by agendaOpenInspector: a fresh selection starts collapsed with
// no carried-over caches.
function agendaHoodReset() {
  agendaHoodOpen = false;
  agendaHoodOps = null;
  agendaHoodOcc = null;
}

// ---- Raw-log fetches (bounded paging; page 500 like the route default;
// an item with more ops than one page follows the cursor a few pages so
// the NEWEST ops are always present — the log serves oldest-first) ----

async function agendaHoodFetchPages(method, itemId, key) {
  const entries = [];
  let since = 0;
  for (let page = 0; page < 8; page++) {
    let resp;
    try {
      resp = await daemonApi.request(method, { item: itemId, limit: 500, since });
    } catch (e) {
      // Transport rejection must land as a rendered error, never a
      // cache stuck in loading.
      return { error: String((e && e.message) || e), entries, pageMore: false };
    }
    if (!resp.ok || !resp.body || !Array.isArray(resp.body[key])) {
      const message = (resp.body && resp.body.error) || `unavailable (${resp.status})`;
      return { error: message, entries, pageMore: false };
    }
    entries.push(...resp.body[key]);
    if (resp.body.next_since >= resp.body.log_len) return { error: '', entries, pageMore: false };
    since = resp.body.next_since;
  }
  // Page budget exhausted with the cursor still short of the log's end.
  return { error: '', entries, pageMore: true };
}

function agendaHoodEnsureData(item) {
  // An errored cache counts as fresh: retry only when the item's
  // updated_ms moves or the inspector reopens — a persistent failure
  // must never become a render→fetch→render loop.
  const fresh = (cache) => cache && cache.itemId === item.id
    && cache.updatedMs === (item.updated_ms || 0);
  if (!fresh(agendaHoodOps)) {
    const stamp = { itemId: item.id, updatedMs: item.updated_ms || 0 };
    agendaHoodOps = { ...stamp, loading: true, error: '', entries: [], pageMore: false };
    agendaHoodFetchPages('api_agenda_ops', item.id, 'ops').then((got) => {
      if (!agendaHoodOps || agendaHoodOps.itemId !== stamp.itemId
        || agendaHoodOps.updatedMs !== stamp.updatedMs) return; // superseded
      agendaHoodOps = { ...stamp, loading: false, ...got };
      agendaInspectorRender();
    });
  }
  const effect = (item.effects || [])[0];
  if (effect && effect.last_run && !fresh(agendaHoodOcc)) {
    const stamp = { itemId: item.id, updatedMs: item.updated_ms || 0 };
    agendaHoodOcc = { ...stamp, loading: true, error: '', entries: [] };
    agendaHoodFetchPages('api_agenda_occurrences', item.id, 'occurrences').then((got) => {
      if (!agendaHoodOcc || agendaHoodOcc.itemId !== stamp.itemId
        || agendaHoodOcc.updatedMs !== stamp.updatedMs) return;
      agendaHoodOcc = { itemId: stamp.itemId, updatedMs: stamp.updatedMs, loading: false, error: got.error, entries: got.entries };
      agendaInspectorRender();
    });
  }
}

// ---- Section ----

function agendaHoodSectionHtml(item) {
  const head = `<button type="button" class="ag2-hood-toggle" data-hood-act="toggle"
      aria-expanded="${agendaHoodOpen ? 'true' : 'false'}">
    <span class="ag2-hood-title">Under the hood</span>
    <span class="ag2-hood-sub">ids · digests · attribution · raw ops</span>
    <span class="ag2-spacer"></span>
    <span class="ag2-hood-chev">${agendaHoodOpen ? '▾' : '▸'}</span>
  </button>`;
  if (!agendaHoodOpen) return `<section class="ag2-hood">${head}</section>`;
  agendaHoodEnsureData(item);
  return `<section class="ag2-hood">${head}
    <div class="ag2-hood-body">
      ${agendaHoodIdentityHtml(item)}
      ${agendaHoodBornHtml(item)}
      ${agendaHoodManifestHtml(item)}
      ${agendaHoodAskHtml(item)}
      ${agendaHoodHistoryHtml(item)}
      ${agendaHoodAccessHtml()}
      ${agendaHoodFooterHtml(item)}
    </div>
  </section>`;
}

// Delegated from agendaInspClick on [data-hood-act].
function agendaHoodClick(item, el) {
  switch (el.dataset.hoodAct) {
    case 'toggle':
      agendaHoodOpen = !agendaHoodOpen;
      agendaInspectorRender();
      break;
    case 'copy-id':
      agendaCopyText(item.id, 'the full item id');
      break;
    case 'copy-digest': {
      const effect = (item.effects || [])[0];
      if (effect) agendaCopyText(effect.digest, 'the manifest digest');
      break;
    }
    case 'raw':
      agendaSheetState = { kind: 'raw', itemId: item.id };
      agendaSheetRender();
      break;
    case 'copy-ctl':
      agendaCopyText(`intendant ctl agenda annotate ${item.id.slice(0, 6).toLowerCase()} "…"`,
        'a ctl handle — any unique id prefix works');
      break;
    default:
      break;
  }
}

// ---- Identity ----

function agendaHoodIdentityHtml(item) {
  const p = item.provenance || {};
  const times = [['parked', p.created_ms], ['updated', item.updated_ms]];
  if (item.completed_ms) times.push(['completed', item.completed_ms]);
  const grid = times.map(([k, v]) => `<div class="ag2-hood-time">
      <div class="ag2-hood-time-k">${escapeHtml(k)}</div>
      <div class="ag2-hood-time-v">${escapeHtml(agendaAbsTime(v))}</div>
    </div>`).join('');
  return `<div class="ag2-hood-card">
    <div class="ag2-hood-eyebrow">Identity</div>
    <button type="button" class="ag2-hood-idbtn" data-hood-act="copy-id" title="Click to copy the full item id">
      <span class="t-time">${escapeHtml(item.id.slice(0, 10))}</span><span class="t-rand">${escapeHtml(item.id.slice(10))}</span>
    </button>
    <div class="ag2-hood-note">ulid — the highlighted 10 chars encode creation time, so sort order is creation order</div>
    <div class="ag2-hood-times">${grid}</div>
  </div>`;
}

// ---- Born as (gate attribution passport) ----

function agendaHoodBornHtml(item) {
  const p = item.provenance || {};
  const chips = [];
  chips.push(agendaChipHtml(p.kind || 'unattributed', 'iris',
    'actor class as the daemon’s gates resolved it — mapped at the authenticated edge, never parsed from the request'));
  if (p.principal) {
    chips.push(`<span class="ag2-hood-vchip" title="IAM principal exactly as the gate named it">${escapeHtml(p.principal)}</span>`);
  }
  if (p.source) {
    chips.push(`<span class="ag2-hood-source" title="self-described --source label — unverified data beside the attribution; never a principal, never a trust input">via “${escapeHtml(p.source)}”</span>`);
  }
  let session = '';
  if (p.session_id) {
    const s = agendaSessionInfo(p.session_id);
    if (s && s.key) {
      session = `<div class="ag2-hood-session"><a class="agenda-session-link ag2-hood-sess" href="#sessions"
          data-session-key="${escapeHtml(s.key)}">${escapeHtml(`${p.session_id} → “${s.name || String(s.conversation_id || '').slice(0, 8)}”`)}</a></div>`;
    } else {
      session = `<div class="ag2-hood-session"><span class="ag2-hood-sess plain">${escapeHtml(`${p.session_id} — unresolved; degrades to the raw id, never an error`)}</span></div>`;
    }
  }
  const bornNote = p.source
    ? 'the label is data beside the gate attribution — unverified by doctrine'
    : p.session_id
      ? 'session-bound by token possession (the injected loopback capability) — never echoed from request fields'
      : 'attributed as the owner surface at the authenticated edge';
  return `<div class="ag2-hood-card">
    <div class="ag2-hood-eyebrow">Born as</div>
    <div class="ag2-hood-chips">${chips.join('')}</div>
    ${session}
    <div class="ag2-hood-note">${escapeHtml(bornNote)}</div>
  </div>`;
}

// ---- Manifest seal ----

// The latest journal record for this effect's last occurrence — journal
// truth first; the fold's last_run fields are the fallback when the
// journal page has nothing (absence claims nothing).
function agendaHoodOccurrenceLine(item, effect) {
  const run = effect.last_run;
  if (!run) return '';
  let state = run.state;
  let source = 'journal synced before dispatch';
  if (agendaHoodOcc && agendaHoodOcc.itemId !== item.id) {
    // Cache from a previous selection (only reachable when this item's
    // fetch is gated): claim nothing from it.
    return `<div class="ag2-hood-occ">${escapeHtml(`occurrence ${run.occurrence_id} · ${state} · fold view`)}</div>`;
  }
  if (agendaHoodOcc && !agendaHoodOcc.loading && !agendaHoodOcc.error) {
    const records = agendaHoodOcc.entries
      .filter((entry) => entry && entry.record
        && entry.record.occurrence_id === run.occurrence_id);
    if (records.length) {
      state = records[records.length - 1].record.state || state;
    } else {
      source = 'fold view — the journal page holds no record for it';
    }
  } else if (agendaHoodOcc && agendaHoodOcc.loading) {
    source = 'reading occurrences.jsonl…';
  } else if (agendaHoodOcc && agendaHoodOcc.error) {
    source = 'fold view — journal read unavailable';
  }
  const requested = (effect.requested || []).length;
  const tail = requested ? ` · ${requested} owner run-request${requested > 1 ? 's' : ''}` : '';
  return `<div class="ag2-hood-occ">${escapeHtml(`occurrence ${run.occurrence_id} · ${state} · ${source}${tail}`)}</div>`;
}

function agendaHoodManifestHtml(item) {
  const effect = (item.effects || [])[0];
  if (!effect || !effect.manifest) return '';
  const bound = !!effect.approval && effect.approval.digest === effect.digest;
  const digestGroups = (effect.digest.match(/.{1,4}/g) || [effect.digest]).join(' ');
  const sealTone = bound ? 'green' : 'amber';
  const sealLine = bound
    ? 'approval bound to this exact revision — any edit voids it'
    : 'no approval binds this revision — nothing fires';
  let attribution;
  if (effect.approval) {
    attribution = `approved ${agendaAbsTime(effect.approval.at_ms)} by ${effect.approval.principal || '—'} (${effect.approval.kind || '—'})`;
  } else {
    const proposer = effect.proposed_session_id
      ? agendaActorLabel({ session_id: effect.proposed_session_id, kind: effect.proposed_kind, principal: effect.proposed_principal })
      : (effect.proposed_kind || '—');
    attribution = `proposed ${agendaAbsTime(effect.proposed_ms)} by ${proposer}`;
  }
  let streak = '';
  const rec = effect.manifest.recurrence;
  if (rec) {
    const threshold = Math.max(1, rec.suspend_after_failures || 3);
    const n = Math.min(effect.consecutive_failures || 0, threshold);
    const dots = Array.from({ length: threshold }, (_, i) =>
      `<span class="ag2-hood-dot${i < n ? ' filled' : ''}"></span>`).join('');
    const suspended = (effect.consecutive_failures || 0) >= threshold;
    const label = suspended
      ? `suspended — ${n} of ${threshold} non-success outcomes; re-approval resets the streak`
      : `${n} of ${threshold} non-success outcomes before suspension (missed runs don’t count)`;
    streak = `<div class="ag2-hood-streak">${dots}<span class="ag2-hood-note">${escapeHtml(label)}</span></div>`;
  }
  return `<div class="ag2-hood-card">
    <div class="ag2-hood-cardhead">
      <span class="ag2-hood-eyebrow">Manifest seal</span>
      <span class="ag2-spacer"></span>
      <span class="ag2-hood-effid">${escapeHtml(effect.effect_id)}</span>
    </div>
    <button type="button" class="ag2-hood-idbtn digest" data-hood-act="copy-digest"
      title="Click to copy the full digest — sha256 over item + effect identity and the manifest’s canonical bytes">${escapeHtml(digestGroups)}</button>
    <div class="ag2-hood-seal t-${sealTone}"><span class="ag2-hood-sealdot"></span>${escapeHtml(sealLine)}</div>
    <div class="ag2-hood-note mono">${escapeHtml(attribution)}</div>
    ${streak}
    ${agendaHoodOccurrenceLine(item, effect)}
  </div>`;
}

// ---- Ask internals ----

function agendaHoodAskHtml(item) {
  if (item.kind !== 'question') return '';
  const chips = [];
  if (item.ask) {
    chips.push(`<span class="ag2-hood-vchip" title="question-rail identity from the approval allocator — floored above every persisted ask at fold, so a restarted daemon never re-mints one">ask #${escapeHtml(String(item.ask.ask_id))}</span>`);
    (item.ask.questions || []).forEach((q) => {
      (q.previews || []).forEach((pv) => {
        if (!pv.upload_id) return; // inline text previews carry no blob
        chips.push(`<span class="ag2-hood-vchip" title="${escapeHtml(`GET ${pv.url || ''} — attachment + nosniff, so a blob never renders by direct navigation; the dashboard fetches it into a sandboxed srcdoc`)}">${escapeHtml(`blob ${pv.upload_id} · ${pv.mime || 'text/html'}`)}</span>`);
      });
    });
  }
  if (item.dismissed) {
    chips.push(`<span class="ag2-hood-source" title="${escapeHtml(`rails cleared ${agendaRelTime(item.dismissed.at_ms)} — deliberately excluded from page-load and boot re-announcement`)}">${escapeHtml(`dismissed · ${item.dismissed.action || '—'}`)}</span>`);
  }
  if (item.answer) {
    const delivered = item.answer.delivered;
    const label = delivered === true ? 'delivered: true'
      : delivered === false ? 'delivered: false' : 'delivery: unrecorded';
    const tone = delivered === true ? 'green' : delivered === false ? 'sky' : 'iris';
    const tip = delivered === true
      ? 'reached the asking session as a user message at a turn boundary, resolved across its resume lineage'
      : delivered === false
        ? 'recorded by a daemon-authored record_ask_delivery op; one info notification raised (title only, never answer text)'
        : 'absent data claims nothing';
    chips.push(agendaChipHtml(label, tone, tip));
  }
  if (!chips.length) return '';
  const note = item.status === 'open'
    ? 'only an answer resolves a question — dismissal is not resolution'
    : 'the item keeps the durable record: the joined text plus the structured resolution';
  return `<div class="ag2-hood-card">
    <div class="ag2-hood-eyebrow">Ask internals</div>
    <div class="ag2-hood-chips">${chips.join('')}</div>
    <div class="ag2-hood-note">${escapeHtml(note)}</div>
  </div>`;
}

// ---- History (the real op log) ----

// Presentation verb for one served envelope. Every interpolated field is
// op-log DATA — the caller escapes the whole verb string.
function agendaHoodOpVerb(op, envelope) {
  const type = String(op.type || '');
  const titleOf = (id, cap) => {
    const target = agendaFindItem(id);
    const title = target ? target.title : '';
    return title ? `“${title.length > cap ? `${title.slice(0, cap - 1)}…` : title}”` : `“${String(id || '?').slice(0, 8)}…”`;
  };
  switch (type) {
    case 'add': return 'parked';
    case 'patch': return 'patched';
    case 'complete': return 'completed';
    case 'reopen': return 'reopened';
    case 'retire': return 'retired — hidden, never deleted';
    case 'answer': return 'answered';
    case 'dismiss': return `dismissed${op.action ? ` (${op.action})` : ''} — still open`;
    case 'annotate': {
      // The self-described label rides the verb (the triage mandate's
      // annotations read as "annotated · --source triage").
      const source = envelope.source || op.source || '';
      return source ? `annotated · --source ${source}` : 'annotated';
    }
    case 'set_blocker': return 'blocker set';
    case 'clear_blocker': return 'blocker cleared';
    case 'add_ref': return `ref attached · ${op.ref_type || 'file'}`;
    case 'remove_ref': return `ref removed · ${op.ref_type || 'file'}`;
    case 'add_part_of': return `filed under ${titleOf(op.parent_id, 27)}`;
    case 'remove_part_of': return 'unfiled';
    case 'add_relates_to': return `related to ${titleOf(op.target_id, 23)}`;
    case 'remove_relates_to': return `relation removed · ${titleOf(op.target_id, 23)}`;
    case 'add_relies_on': return `relies on ${titleOf(op.target_id, 23)}`;
    case 'remove_relies_on': return `dependency removed · ${titleOf(op.target_id, 23)}`;
    case 'propose_effect': return 'manifest proposed';
    case 'approve_effect': return 'approved — digest bound';
    case 'revoke_effect': return 'approval revoked';
    case 'request_occurrence': return 'run requested';
    case 'record_occurrence': return `run ${op.state || '—'}`;
    case 'record_ask_delivery':
      return op.delivered === false ? 'delivery pending — no live asker' : 'answer delivered';
    default: return `op · ${type || '—'}`;
  }
}

function agendaHoodOpDot(op, known) {
  if (!known) return 'neutral';
  switch (String(op.type || '')) {
    case 'add': case 'reopen': case 'request_occurrence': return 'iris';
    case 'set_blocker': return 'rose';
    case 'clear_blocker': case 'approve_effect': case 'answer': case 'complete': return 'green';
    case 'add_ref': case 'remove_ref': case 'record_ask_delivery': return 'sky';
    case 'propose_effect': case 'revoke_effect': return 'amber';
    case 'record_occurrence':
      return op.state === 'completed' || op.state === 'delivered' ? 'green'
        : op.state === 'failed' ? 'rose'
          : op.state === 'started' ? 'iris' : 'amber';
    default: return 'neutral';
  }
}

// Who performed a served op, from the ENVELOPE's gate attribution (the
// self-described source label supplements, never replaces it).
function agendaHoodOpWho(envelope) {
  const actor = envelope.actor || null;
  const label = actor ? agendaActorLabel(actor) : '';
  if (label) return label;
  const source = envelope.source
    || (envelope.op && envelope.op.source) || '';
  if (source) return `“${source}”`;
  const daemonOps = ['record_occurrence', 'record_ask_delivery'];
  if (envelope.op && daemonOps.includes(String(envelope.op.type || ''))) return 'the daemon';
  return 'unattributed';
}

function agendaHoodHistoryHtml(item) {
  const cache = agendaHoodOps;
  let body;
  let label = '';
  if (!cache || cache.loading) {
    body = '<div class="ag2-hood-note">reading agenda.jsonl…</div>';
  } else if (cache.error) {
    body = `<div class="ag2-hood-note">${escapeHtml(`op log ${cache.error}`)}</div>`;
  } else {
    const rows = cache.entries.map((entry) => {
      if (entry.unparseable) {
        return { at: 0, verb: 'unreadable line · preserved', who: '', dot: 'neutral' };
      }
      const envelope = entry.op || {};
      const op = envelope.op || {};
      const verb = entry.known ? agendaHoodOpVerb(op, envelope) : `op · ${String(op.type || '—')}`;
      return {
        at: envelope.at_ms || 0,
        verb,
        who: agendaHoodOpWho(envelope),
        dot: agendaHoodOpDot(op, !!entry.known),
      };
    });
    rows.reverse(); // newest first (the log serves oldest-first)
    const shown = rows.slice(0, 9).map((row) => `<div class="ag2-hood-tl-row">
        <span class="ag2-hood-tl-dot t-${row.dot}"></span>
        <span class="ag2-hood-tl-verb">${escapeHtml(row.verb)}</span>
        ${row.who ? `<span class="ag2-hood-tl-who">${escapeHtml(`by ${row.who}`)}</span>` : ''}
        <span class="ag2-spacer"></span>
        <span class="ag2-hood-tl-t">${escapeHtml(row.at ? agendaRelTime(row.at) : '—')}</span>
      </div>`).join('');
    const more = rows.length > 9
      ? `<div class="ag2-hood-more">+ ${rows.length - 9}${cache.pageMore ? '+' : ''} earlier ops in the log</div>`
      : '';
    body = `<div class="ag2-hood-rail">${shown}</div>${more}`;
    label = `${rows.length}${cache.pageMore ? '+' : ''} ops · append-only`;
  }
  return `<div class="ag2-hood-card">
    <div class="ag2-hood-cardhead">
      <span class="ag2-hood-eyebrow">History</span>
      <span class="ag2-spacer"></span>
      <span class="ag2-hood-effid">${escapeHtml(label)}</span>
    </div>
    ${body}
  </div>`;
}

// ---- Access (documentation copy — static, like the prototype) ----

function agendaHoodAccessHtml() {
  const chips = [
    ['agenda.read · GET /api/agenda',
      'the ledger snapshot: items, counts, skipped lines, reminder policy, session join map'],
    ['agenda.write · POST /api/agenda/op',
      'one validated command per call — attribution mapped at the authenticated edge, never from the body'],
    ['owner surface · approve / revoke / start',
      'the tenant edge refuses agent-session, peer, and unattributed callers with a named denial'],
    ['settings.manage · reminder policy',
      'separate authority — an agenda.write grant can’t make its own item louder'],
  ].map(([label, tip]) =>
    `<span class="ag2-hood-vchip" title="${escapeHtml(tip)}">${escapeHtml(label)}</span>`).join('');
  return `<div class="ag2-hood-card">
    <div class="ag2-hood-eyebrow">Access</div>
    <div class="ag2-hood-chips">${chips}</div>
    <div class="ag2-hood-note">approvals, revokes, and start-now are owner-surface acts — an agent session is refused by policy, even on its own manifest</div>
  </div>`;
}

function agendaHoodFooterHtml(item) {
  const handle = `intendant ctl agenda annotate ${item.id.slice(0, 6).toLowerCase()} "…"`;
  return `<div class="ag2-hood-foot">
    <button type="button" class="ag2-btn ghost" data-hood-act="raw">View raw item JSON</button>
    <button type="button" class="ag2-btn ghost" data-hood-act="copy-ctl"
      title="${escapeHtml(`${handle} — any unique id prefix works in ctl`)}">Copy ctl handle</button>
    <span class="ag2-hood-footnote">retire hides, nothing deletes — agenda.jsonl keeps every line</span>
  </div>`;
}

// ---- Raw-JSON sheet ----

// Presentation strip of nulls and empty arrays (the prototype's toRaw):
// the served DTO minus visual noise, pretty-printed. The label below the
// pre names the truth precisely — the served item is the fold product
// PLUS the planner's display-only decorations (next_fire_ms,
// deferred_until), so "pure fold product" would be false.
function agendaHoodStripRaw(value) {
  return JSON.parse(JSON.stringify(value, (key, v) =>
    (v === null || (Array.isArray(v) && !v.length) ? undefined : v)));
}

function agendaRawSheetHtml(item) {
  const pretty = JSON.stringify(agendaHoodStripRaw(item), null, 2);
  return `<div class="ag2-sheet-head">
      <span class="ag2-sheet-title">Raw item JSON</span>
      <span class="ag2-spacer"></span>
      <button type="button" class="ag2-x" data-sheet-act="close" title="Close — esc">×</button>
    </div>
    <div class="ag2-sheet-item">${escapeHtml(`${item.id.slice(0, 6).toLowerCase()} · ${item.title}`)}</div>
    <pre class="ag2-sheet-raw">${escapeHtml(pretty)}</pre>
    <div class="ag2-hood-footnote">Exactly what GET /api/agenda serves — the fold product of agenda.jsonl plus the planner’s display-only decorations (next_fire_ms, deferred_until). Nothing here is stored state beyond the ops.</div>`;
}

// ---- Ledger footer ops count ----
// The real log length (one {limit:1} page for its log_len), rendered by
// the cards fragment between the counts and the skipped-lines segment.
// Fetch-on-render lifecycle: agendaRenderTab calls the sync on every
// paint; it fetches only while the tab is visible and only when the data
// signature moved (the agenda_changed merge bumps updated_ms /counts, so
// a changed ledger refetches; a parked tab never fetches — no timers to
// tear down).

let agendaLedgerOpsLen = null;
let agendaLedgerOpsSig = '';
let agendaLedgerOpsInFlight = false;

function agendaLedgerOpsSegment() {
  return agendaLedgerOpsLen === null ? '' : ` · ${agendaLedgerOpsLen} ops in the log`;
}

function agendaLedgerOpsSync() {
  if (!agendaTabVisible() || document.hidden || agendaLedgerOpsInFlight) return;
  if (!Array.isArray(agendaItems)) return;
  const sig = `${agendaCounts.open}|${agendaCounts.done}|${agendaCounts.retired}|`
    + agendaItems.reduce((max, it) => Math.max(max, it.updated_ms || 0), 0);
  if (sig === agendaLedgerOpsSig && agendaLedgerOpsLen !== null) return;
  agendaLedgerOpsInFlight = true;
  daemonApi.request('api_agenda_ops', { limit: 1 }).then((resp) => {
    agendaLedgerOpsInFlight = false;
    if (!resp.ok || !resp.body || typeof resp.body.log_len !== 'number') {
      // Unsupported/unavailable: remember the signature so a failing
      // daemon is not re-asked on every repaint — the next DATA change
      // retries once. The segment simply stays absent.
      agendaLedgerOpsSig = sig;
      return;
    }
    const changed = agendaLedgerOpsLen !== resp.body.log_len;
    agendaLedgerOpsLen = resp.body.log_len;
    agendaLedgerOpsSig = sig;
    if (changed) agendaRenderTab();
  }).catch(() => {
    agendaLedgerOpsInFlight = false;
    agendaLedgerOpsSig = sig;
  });
}

// ---- Command palette entries (the shell ⌘K seam) ----
// ui2PaletteEntries (ui2-chrome.js) concatenates this provider through a
// typeof guard — the palette's documented convention for cross-fragment
// state (read by name at event time). Two entry classes: lens
// destinations derived from AGENDA_LENSES (never a mirrored list), and
// item-title hits over the loaded agenda cache.
function agendaPaletteEntries(query) {
  const q = (query || '').trim().toLowerCase();
  const entries = [];
  if (typeof AGENDA_LENSES !== 'undefined') {
    for (const lens of AGENDA_LENSES) {
      const label = `Agenda: ${lens.label}`;
      if (q && !label.toLowerCase().includes(q)) continue;
      entries.push({
        section: 'Agenda',
        icon: 'agenda',
        label,
        hint: 'go',
        run: () => {
          agendaLens = lens.id;
          routeTo('agenda');
          agendaRenderTab();
        },
      });
    }
  }
  if (q.length >= 2 && Array.isArray(agendaItems)) {
    const scored = [];
    for (const item of agendaItems) {
      const score = ui2FuzzyScore(q, item.title);
      if (score < 0) continue;
      scored.push({ score, item });
    }
    scored.sort((a, b) => b.score - a.score);
    for (const { item } of scored.slice(0, 9)) {
      const title = String(item.title || '');
      entries.push({
        section: 'Agenda',
        icon: 'agenda',
        label: title.length > 44 ? `${title.slice(0, 43)}…` : title,
        hint: `${item.kind} · ${item.status}`,
        matchless: true,
        run: () => {
          routeTo('agenda');
          agendaOpenInspector(item.id);
        },
      });
    }
  }
  return entries;
}

// ── Quick-search message lane (default on; ?message_search=off escape) ──
// Unions full-text message hits from the rolling message index
// (`/api/sessions/message-search`, tunnel twin `api_sessions_message_search`)
// into the Recent list's quick search, UNDER the existing metadata lane:
// the metadata lane stays immediate and untouched; once the query reaches
// two characters this lane debounces ~225 ms, aborts any in-flight request,
// and rejects stale responses by token. Hits attach to their session cards
// (role, timestamp, best snippet with server-anchored highlights,
// superseded/truncated/subagent badges); hits whose sessions are missing
// from the loaded metadata corpus render as compact stub cards.
//
// The lane is enabled by default after its 2026-07-12 soak.
// `?message_search=off` (or `=0`) is the inert escape hatch:
// scheduleSessionMessageSearch() returns before touching anything, the
// toolbar toggle and status line stay hidden, and the render pipeline sees
// `active:false` everywhere. The daemon-side
// `api_sessions_message_search_available` status boolean (derived from the
// tunnel method table) gates availability — no hand-mirrored capability flag.
//
// Transport: `daemonApi.request('api_sessions_message_search', …)` rides
// tunnel-first with the direct-HTTP GET fallback — the descriptor row
// (32-daemon-api.js) and the C1 route row are both landed, and the
// daemon-side parity test (`daemon_api_http_map_mirrors_gateway_routes`,
// dashboard_control/mod.rs) pins them together. The ⌘K palette's Messages
// section (ui2-chrome.js) shares this method and the flag below.
//
// State lives in 31-init-identity-fleet.js's early client-state block
// (`_sessionMsgSearch*` — deep-link TDZ rule); render-side touchpoints live
// in 57-sessions-replay.js and are gated on `ctx.msgSearch`.

const SESSION_MSG_SEARCH_DEBOUNCE_MS = 225;
const SESSION_MSG_SEARCH_MIN_QUERY = 2;
const SESSION_MSG_SEARCH_TIMEOUT_MS = 15000;

function messageSearchFlagEnabled() {
  if (_msgSearchFlagMemo === null) {
    // Default ON since the 2026-07-12 soak (real-corpus acceptance +
    // browser QA green); `?message_search=off` is the escape hatch. The
    // runtime availability precheck still gates the lane per daemon, so
    // old daemons degrade to the honest "unavailable" note.
    const raw = new URLSearchParams(window.location.search).get('message_search');
    _msgSearchFlagMemo = raw !== 'off' && raw !== '0';
  }
  return _msgSearchFlagMemo;
}

// The preview lane retires from search matching whenever the message
// lane can serve this host (plan §8): flag on + the daemon-derived
// availability of api_sessions_message_search. Per-query state is
// deliberately ignored — an available lane is the authority on message
// text even when the current query has no hits yet.
function messageSearchSupersedesPreviews() {
  if (!messageSearchFlagEnabled()) return false;
  // Follow the lane's own serving state, not raw availability: a stale
  // pre-hello "unavailable" answer (typed before the tunnel came up)
  // must keep the preview fallback, or search would show neither
  // preview matches nor message hits.
  if (_sessionMsgSearch.unavailable || _sessionMsgSearch.error) return false;
  const host = currentSessionsHostId();
  const target = host && host !== selfPeerId ? host : null;
  return !!daemonApi.availability('api_sessions_message_search', target).ok;
}

// Superseded matches are included by default and badged (plan D2); the
// toolbar toggle (revealed only under the flag) flips the server-side
// `include_superseded` param. Absent element ⇒ the default (include).
function sessionsIncludeSuperseded() {
  return document.getElementById('sessions-include-superseded')?.checked !== false;
}

// The response entry for one session, keyed exactly like the render loop
// keys its rows (normalizeAgentId'd source + session id).
function sessionMessageSearchHitFor(source, sessionId) {
  if (!_sessionMsgSearch.active) return null;
  return _sessionMsgSearch.hits.get(sessionLogSearchKey(source, sessionId)) || null;
}

// "Results are live for the query the list is currently rendering." The
// metadata lane compares lowercased text, so this does too — during the
// debounce + flight window after an edit the applied results answer the
// OLD query and must not paint against the new one (display-layer stale
// rejection; the token check below is the response-layer one).
function sessionMessageSearchActiveFor(queryLower) {
  return messageSearchFlagEnabled()
    && _sessionMsgSearch.active
    && _sessionMsgSearch.queryLower === String(queryLower || '');
}

// The applied result set. Render passes derive stub rows from this map by
// corpus membership (collectSessionsListMatches), never from an apply-time
// snapshot — the corpus can hydrate under live results.
function sessionMessageSearchHits() {
  return _sessionMsgSearch.hits;
}

function sessionMessageSearchParams() {
  const query = (document.getElementById('sessions-search')?.value || '').trim();
  // Source filter: concrete ids ride the API's `source` csv; the synthetic
  // 'external' value has no server vocabulary, so it widens to 'all' and
  // the client-side sessionMatchesSourceFilter pass keeps the honest set.
  const sourceFilter = sessionSourceFilterValue();
  const source = (!sourceFilter.length || sourceFilter.includes('external'))
    ? 'all'
    : sourceFilter.join(',');
  return {
    q: query,
    source,
    include_superseded: sessionsIncludeSuperseded(),
    subagents: sessionsShowSubagents(),
    host: currentSessionsHostId(),
  };
}

function sessionMessageSearchSig(params) {
  return JSON.stringify([
    params.q,
    params.source,
    params.include_superseded,
    params.subagents,
    params.host,
  ]);
}

// Single entry point, called from _refreshSessionsFilters (input, Escape
// clear, source/subagents/superseded toggles all funnel through it) and
// from setSessionsHost. Inert with the flag off.
function scheduleSessionMessageSearch() {
  if (!messageSearchFlagEnabled()) return;
  const params = sessionMessageSearchParams();
  if (params.q.length < SESSION_MSG_SEARCH_MIN_QUERY) {
    clearSessionMessageSearch();
    return;
  }
  const sig = sessionMessageSearchSig(params);
  if (sig === _sessionMsgSearch.sig && (_sessionMsgSearch.active || _sessionMsgSearch.loading)) {
    return; // already answered or in flight for exactly this request
  }
  if (_sessionMsgSearchTimer) clearTimeout(_sessionMsgSearchTimer);
  _sessionMsgSearchTimer = setTimeout(() => {
    _sessionMsgSearchTimer = null;
    runSessionMessageSearch(params, sig, 0);
  }, SESSION_MSG_SEARCH_DEBOUNCE_MS);
}

// Query dropped under the minimum (or cleared): abort the lane and repaint
// only if it was visible. Callers re-render via _refilterSessions.
function clearSessionMessageSearch() {
  if (_sessionMsgSearchTimer) {
    clearTimeout(_sessionMsgSearchTimer);
    _sessionMsgSearchTimer = null;
  }
  if (_sessionMsgSearchAbort) {
    _sessionMsgSearchAbort.abort();
    _sessionMsgSearchAbort = null;
  }
  _sessionMsgSearchToken += 1; // anything still in flight is stale
  const hadState = _sessionMsgSearch.active || _sessionMsgSearch.loading
    || !!_sessionMsgSearch.error || !!_sessionMsgSearch.unavailable;
  if (!hadState) return;
  _sessionMsgSearch = {
    sig: '',
    query: '',
    queryLower: '',
    active: false,
    loading: false,
    state: '',
    partialReason: null,
    error: '',
    unavailable: '',
    hits: new Map(),
    extrasHint: 0,
    moreAvailable: false,
    windowDays: 0,
    seq: _sessionMsgSearch.seq + 1,
  };
  updateSessionsMessageSearchStatus();
}

function failSessionMessageSearch(patch) {
  _sessionMsgSearch = {
    ..._sessionMsgSearch,
    active: false,
    loading: false,
    state: '',
    partialReason: null,
    hits: new Map(),
    extrasHint: 0,
    moreAvailable: false,
    error: '',
    unavailable: '',
    ...patch,
    seq: _sessionMsgSearch.seq + 1,
  };
  _sessionMsgSearchAbort = null;
  updateSessionsMessageSearchStatus();
  _refilterSessions();
}

function runSessionMessageSearch(params, sig, attempt) {
  if (_sessionMsgSearchAbort) _sessionMsgSearchAbort.abort();
  const controller = new AbortController();
  _sessionMsgSearchAbort = controller;
  const token = ++_sessionMsgSearchToken;

  // Keep the previous results painted while a same-query re-filter is in
  // flight; drop them the moment the TEXT changes (they answer the old
  // query and would highlight the wrong thing).
  const sameQuery = _sessionMsgSearch.query === params.q;
  _sessionMsgSearch = {
    ..._sessionMsgSearch,
    sig,
    query: params.q,
    queryLower: params.q.toLowerCase(),
    active: sameQuery ? _sessionMsgSearch.active : false,
    hits: sameQuery ? _sessionMsgSearch.hits : new Map(),
    extrasHint: sameQuery ? _sessionMsgSearch.extrasHint : 0,
    loading: true,
    error: '',
    unavailable: '',
    seq: _sessionMsgSearch.seq + 1,
  };
  updateSessionsMessageSearchStatus();

  const target = params.host && params.host !== selfPeerId ? params.host : null;

  // Honest precheck via the daemon-derived method availability (the tunnel
  // method table's `<method>_available` boolean once C1 lands; 'unsupported'
  // on daemons without the route). Optimistic pre-hello states fall through
  // to the request, which reports the truth itself.
  const availability = daemonApi.availability('api_sessions_message_search', target);
  if (!availability.ok) {
    const note = availability.reason === 'denied'
      ? 'Message search is not permitted for this dashboard connection.'
      : availability.reason === 'transport-down'
        ? 'Message search needs the dashboard tunnel — waiting for it to reconnect.'
        : 'Message search is not available on this daemon yet.';
    failSessionMessageSearch({ unavailable: note });
    return;
  }

  const requestParams = {
    q: params.q,
    source: params.source,
    include_superseded: params.include_superseded,
    subagents: params.subagents,
  };
  const options = { signal: controller.signal, timeoutMs: SESSION_MSG_SEARCH_TIMEOUT_MS };
  if (target) options.target = target;

  daemonApi.request('api_sessions_message_search', requestParams, options)
    .then(resp => {
      if (token !== _sessionMsgSearchToken) return; // stale response
      if (resp.status === 429) {
        // Busy: back off one debounce period, retry once.
        if (attempt >= 1) {
          failSessionMessageSearch({ error: 'Message search is busy — edit the query to retry.' });
          return;
        }
        setTimeout(() => {
          if (token !== _sessionMsgSearchToken) return;
          runSessionMessageSearch(params, sig, attempt + 1);
        }, SESSION_MSG_SEARCH_DEBOUNCE_MS);
        return;
      }
      if (resp.status === 410) {
        // cursor_expired — restart the search transparently. This lane
        // only requests first pages today, but the restart contract is
        // cheap to honor and covers the C2/pagination follow-up.
        if (attempt >= 2) {
          failSessionMessageSearch({ error: 'Message search snapshot expired repeatedly — try again.' });
          return;
        }
        runSessionMessageSearch(params, sig, attempt + 1);
        return;
      }
      if (resp.status === 404 || resp.status === 405) {
        failSessionMessageSearch({ unavailable: 'Message search is not available on this daemon yet.' });
        return;
      }
      const body = resp.body && typeof resp.body === 'object' ? resp.body : {};
      if (!resp.ok || body.ok === false) {
        failSessionMessageSearch({
          error: `Message search failed: ${body.error || `HTTP ${resp.status}`}`,
        });
        return;
      }
      applySessionMessageSearchResults(params, sig, body);
    })
    .catch(err => {
      if (token !== _sessionMsgSearchToken) return;
      if (err?.kind === 'abort' || err?.name === 'AbortError') return;
      if (err?.kind === 'unavailable') {
        failSessionMessageSearch({ unavailable: 'Message search is not available on this daemon yet.' });
        return;
      }
      if (err?.kind === 'denied') {
        failSessionMessageSearch({ unavailable: 'Message search is not permitted for this dashboard connection.' });
        return;
      }
      failSessionMessageSearch({ error: `Message search failed: ${err?.message || err}` });
    });
}

function applySessionMessageSearchResults(params, sig, body) {
  const known = new Set();
  for (const s of Array.isArray(_cachedSessions) ? _cachedSessions : []) {
    const source = normalizeAgentId(s.source || '') || 'intendant';
    if (s.session_id) known.add(sessionLogSearchKey(source, s.session_id));
  }
  const hits = new Map();
  let extrasHint = 0;
  for (const entry of Array.isArray(body.sessions) ? body.sessions : []) {
    if (!entry || typeof entry !== 'object') continue;
    const source = normalizeAgentId(entry.source || '') || 'intendant';
    const sessionId = String(entry.session_id || '').trim();
    if (!sessionId) continue;
    const key = sessionLogSearchKey(source, sessionId);
    const normalized = { ...entry, source, session_id: sessionId, key };
    hits.set(key, normalized);
    if (!known.has(key)) extrasHint += 1;
  }
  _sessionMsgSearch = {
    sig,
    query: params.q,
    queryLower: params.q.toLowerCase(),
    active: true,
    loading: false,
    state: String(body.state || ''),
    partialReason: body.partial_reason || null,
    error: '',
    unavailable: '',
    hits,
    extrasHint,
    moreAvailable: !!body.cursor,
    windowDays: Number(body.window_days) || 0,
    seq: _sessionMsgSearch.seq + 1,
  };
  _sessionMsgSearchAbort = null;
  updateSessionsMessageSearchStatus();
  _refilterSessions();
}

function updateSessionsMessageSearchStatus() {
  const el = document.getElementById('sessions-msg-search-status');
  if (!el) return;
  if (!messageSearchFlagEnabled()) {
    el.classList.add('hidden');
    return;
  }
  el.classList.remove('error');
  el.title = _sessionMsgSearch.windowDays > 0
    ? `The message index covers the last ${_sessionMsgSearch.windowDays} days.`
    : '';
  let text = '';
  if (_sessionMsgSearch.unavailable) {
    text = _sessionMsgSearch.unavailable;
  } else if (_sessionMsgSearch.error) {
    el.classList.add('error');
    text = _sessionMsgSearch.error;
  } else if (_sessionMsgSearch.loading) {
    text = 'Searching messages...';
  } else if (_sessionMsgSearch.active) {
    const count = _sessionMsgSearch.hits.size;
    const parts = [];
    parts.push(count === 0
      ? 'No message matches.'
      : `Message matches in ${count.toLocaleString()} ${count === 1 ? 'session' : 'sessions'}.`);
    if (_sessionMsgSearch.extrasHint > 0) {
      parts.push(`${_sessionMsgSearch.extrasHint.toLocaleString()} of them are not in the loaded session list.`);
    }
    if (_sessionMsgSearch.state === 'building') {
      parts.push('The message index is still building - results may be incomplete.');
    } else if (_sessionMsgSearch.partialReason) {
      parts.push(`Results may be incomplete (${_sessionMsgSearch.partialReason}).`);
    }
    if (_sessionMsgSearch.moreAvailable) {
      parts.push('More matching sessions exist - refine the query to narrow them.');
    }
    text = parts.join(' ');
  }
  el.textContent = text;
  el.classList.toggle('hidden', !text);
}

// ── Snippet highlighting ──────────────────────────────────────────────────
// Snippets are attacker-influenced text: they render through
// textContent/createTextNode ONLY, and highlight placement comes from the
// server's byte ranges — the client never re-derives matches (server-side
// normalization changes lengths). `ranges` index into the ORIGINAL message
// text as UTF-8 BYTE offsets; the snippet starts `snippet_offset_bytes`
// into that text, so the in-snippet byte range is
// [start - snippet_offset_bytes, end - snippet_offset_bytes). JS strings
// are UTF-16, so the math runs on TextEncoder bytes and each piece decodes
// back separately — never on code-unit indices.
function messageSnippetSegments(snippet, ranges, offsetBytes) {
  const text = String(snippet || '');
  const bytes = new TextEncoder().encode(text);
  const total = bytes.length;
  const offset = Number.isFinite(Number(offsetBytes)) ? Number(offsetBytes) : 0;
  const clamped = [];
  for (const range of Array.isArray(ranges) ? ranges : []) {
    if (!Array.isArray(range) || range.length < 2) continue;
    let start = Number(range[0]) - offset;
    let end = Number(range[1]) - offset;
    if (!Number.isFinite(start) || !Number.isFinite(end)) continue;
    start = Math.max(0, Math.min(total, Math.floor(start)));
    end = Math.max(0, Math.min(total, Math.floor(end)));
    if (end > start) clamped.push([start, end]);
  }
  if (!clamped.length) return [{ text, hit: false }];
  clamped.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  const merged = [];
  for (const [start, end] of clamped) {
    const last = merged[merged.length - 1];
    if (last && start <= last[1]) last[1] = Math.max(last[1], end);
    else merged.push([start, end]);
  }
  const decoder = new TextDecoder('utf-8');
  const segments = [];
  let pos = 0;
  for (const [start, end] of merged.slice(0, 16)) {
    if (start > pos) segments.push({ text: decoder.decode(bytes.subarray(pos, start)), hit: false });
    segments.push({ text: decoder.decode(bytes.subarray(start, end)), hit: true });
    pos = end;
  }
  if (pos < total) segments.push({ text: decoder.decode(bytes.subarray(pos)), hit: false });
  return segments;
}

function messageSearchTimestampLabels(tsMs) {
  const ms = Number(tsMs);
  if (!Number.isFinite(ms) || ms <= 0) return { rel: '', abs: '' };
  const iso = new Date(ms).toISOString();
  return { rel: sessionRelativeLabel(iso), abs: iso };
}

// One quiet row on the session card: count, role, time, badges, best
// snippet. The server orders a session's hits most-recent-first, so
// hits[0] is the display pick; total_hits covers the rest.
function buildSessionMessageHitBlock(entry) {
  const wrap = document.createElement('div');
  wrap.className = 'sc-msg-hit';
  const hits = Array.isArray(entry.hits) ? entry.hits : [];
  const best = hits[0] && typeof hits[0] === 'object' ? hits[0] : null;
  const total = Number(entry.total_hits) || hits.length;

  const count = document.createElement('span');
  count.className = 'sc-msg-hit-count';
  count.textContent = `${total} message ${total === 1 ? 'match' : 'matches'}`;
  const titleParts = ['Most recent matching message is shown.'];
  if (entry.source_gone) titleParts.push('The source log is no longer on disk; matches come from the index.');
  count.title = titleParts.join('\n');
  wrap.appendChild(count);

  if (!best) return wrap;

  const role = document.createElement('span');
  role.className = 'sc-msg-hit-role';
  role.textContent = String(best.role || '');
  wrap.appendChild(role);

  const { rel, abs } = messageSearchTimestampLabels(best.ts_ms);
  if (rel || abs) {
    const time = document.createElement('span');
    time.className = 'sc-msg-hit-time';
    time.textContent = rel || abs;
    if (abs) time.title = abs;
    wrap.appendChild(time);
  }

  const addChip = (tone, label, title) => {
    const chip = document.createElement('span');
    chip.className = `ui-chip ${tone} sc-msg-hit-chip`;
    chip.textContent = label;
    if (title) chip.title = title;
    wrap.appendChild(chip);
  };
  if (best.superseded) {
    addChip('warn', 'superseded', 'This message was superseded by a rewind or restore. Use the Superseded toggle to hide such matches.');
  }
  if (best.truncated) {
    addChip('muted', 'truncated', 'The indexed text was truncated at the per-message cap; the match may continue beyond it.');
  }
  if (best.subagent) {
    addChip('muted', 'subagent', 'This message lives in a subagent transcript of the session.');
  }

  const snippet = document.createElement('span');
  snippet.className = 'sc-msg-hit-text';
  const segments = messageSnippetSegments(best.snippet, best.ranges, best.snippet_offset_bytes);
  for (const segment of segments) {
    if (!segment.text) continue;
    if (segment.hit) {
      const mark = document.createElement('mark');
      mark.textContent = segment.text;
      snippet.appendChild(mark);
    } else {
      snippet.appendChild(document.createTextNode(segment.text));
    }
  }
  snippet.title = String(best.snippet || '');
  wrap.appendChild(snippet);

  // C2 seam: `best.locator` (opaque, versioned) is the anchor for a
  // locate-jump (`/api/session/{id}?locate=<locator>`) once C2 lands; the
  // card's whole-row click stays the plain session open until then.
  return wrap;
}

// ── Stub rows: hits outside the loaded metadata corpus ────────────────────
function sessionMessageSearchStubMatch(entry) {
  const { abs } = messageSearchTimestampLabels(entry.best_ts_ms);
  return {
    s: {
      session_id: entry.session_id,
      source: entry.source,
      updated_at: abs,
    },
    source: entry.source,
    shortId: String(entry.session_id).substring(0, 8),
    isCurrent: false,
    displayStatus: '',
    logHit: null,
    msgHit: entry,
    msgStub: true,
  };
}

// Card-cache twin of sessionCardFor for stub rows (same node-reuse
// contract; the sig covers everything the card renders).
function sessionMessageSearchStubCardFor(m, ctx, st, nextCards) {
  const key = `msg\u001f${m.msgHit.key}`;
  const sig = 'msg-stub\u001f' + JSON.stringify(m.msgHit) + '\u001f' + String(!!ctx.viewingPeer);
  const cached = st.cards.get(key);
  let card;
  if (cached && cached.sig === sig) {
    card = cached.card;
  } else {
    card = buildSessionMessageSearchStubCard(m, ctx);
    card.dataset.sessionKey = key;
    card.dataset.qaBuildSeq = String(++_sessionCardBuildSeq);
  }
  (nextCards || st.cards).set(key, { sig, card });
  return card;
}

function buildSessionMessageSearchStubCard(m, ctx) {
  const entry = m.msgHit;
  const card = document.createElement('div');
  card.className = 'session-card sc-split sc-msg-stub';
  const main = document.createElement('div');
  main.className = 'sc-main';
  card.appendChild(main);

  const top = document.createElement('div');
  top.className = 'sc-top';
  const titleBlock = document.createElement('div');
  titleBlock.className = 'sc-title-block';
  const titleRow = document.createElement('div');
  titleRow.className = 'sc-title-row';
  const nameEl = document.createElement('div');
  nameEl.className = 'sc-name';
  nameEl.textContent = `Session ${m.shortId}`;
  nameEl.title = entry.session_id;
  titleRow.appendChild(nameEl);
  titleBlock.appendChild(titleRow);
  top.appendChild(titleBlock);

  const sourceEl = document.createElement('span');
  sourceEl.className = 'ui-badge sc-source' + (m.source === 'intendant' ? ' local' : '');
  sourceEl.textContent = m.source === 'intendant' ? 'Intendant' : (prettyAgentName(m.source) || m.source);
  sourceEl.dataset.src = m.source;
  top.appendChild(sourceEl);

  const stubChip = document.createElement('span');
  stubChip.className = 'ui-chip muted sc-role';
  stubChip.textContent = 'not in loaded list';
  stubChip.title = 'The message index knows this session, but it is not in the loaded session list (still hydrating, or beyond this host’s list). Click to open its detail view.';
  top.appendChild(stubChip);
  main.appendChild(top);

  card.appendChild(buildSessionMessageHitBlock(entry));

  const meta = document.createElement('div');
  meta.className = 'sc-meta';
  const idSpan = document.createElement('span');
  const idLabel = document.createElement('span');
  idLabel.className = 'label';
  idLabel.textContent = 'id';
  const idValue = document.createElement('span');
  idValue.className = 'value sc-id';
  idValue.textContent = entry.session_id;
  idSpan.appendChild(idLabel);
  idSpan.appendChild(idValue);
  meta.appendChild(idSpan);
  const { rel, abs } = messageSearchTimestampLabels(entry.best_ts_ms);
  if (rel || abs) {
    const tsSpan = document.createElement('span');
    const tsLabel = document.createElement('span');
    tsLabel.className = 'label';
    tsLabel.textContent = 'matched';
    const tsValue = document.createElement('span');
    tsValue.className = 'value';
    tsValue.textContent = rel || abs;
    if (abs) tsValue.title = abs;
    tsSpan.appendChild(tsLabel);
    tsSpan.appendChild(tsValue);
    meta.appendChild(tsSpan);
  }
  main.appendChild(meta);

  // Jump-to-session: the plain detail open (C2's locate-anchored jump
  // replaces this seam with an anchored one).
  const stubSession = { session_id: entry.session_id, source: entry.source };
  if (ctx.viewingPeer) {
    card.classList.add('sc-peer');
    card.title = 'Open this session on the peer’s dashboard';
    card.addEventListener('click', () => openPeerSessionExternally(stubSession));
  } else {
    card.addEventListener('click', () => openSessionDetail(stubSession));
  }
  return card;
}

// ── Boot wiring (inert unless the flag is on) ─────────────────────────────
(function initSessionsMessageSearchControls() {
  if (!messageSearchFlagEnabled()) return;
  const supersededField = document.getElementById('sessions-msg-superseded-field');
  if (supersededField) supersededField.classList.remove('hidden');
  const supersededInput = document.getElementById('sessions-include-superseded');
  if (supersededInput) {
    supersededInput.checked = localStorage.getItem(SESSIONS_MSG_SUPERSEDED_KEY) !== 'false';
    supersededInput.addEventListener('change', (ev) => {
      localStorage.setItem(SESSIONS_MSG_SUPERSEDED_KEY, String(!!ev.target.checked));
      scheduleSessionMessageSearch();
    });
  }
})();

// QA readback (window.qa convention): the message-lane facts the
// validate-dashboard harness and the flagged soak assert on.
window.qa = Object.assign(window.qa || {}, {
  messageSearch: () => ({
    flag: messageSearchFlagEnabled(),
    supersedesPreviews: messageSearchSupersedesPreviews(),
    query: _sessionMsgSearch.query,
    active: _sessionMsgSearch.active,
    loading: _sessionMsgSearch.loading,
    state: _sessionMsgSearch.state,
    partialReason: _sessionMsgSearch.partialReason,
    error: _sessionMsgSearch.error,
    unavailable: _sessionMsgSearch.unavailable,
    sessions: _sessionMsgSearch.hits.size,
    extrasHint: _sessionMsgSearch.extrasHint,
    moreAvailable: _sessionMsgSearch.moreAvailable,
    windowDays: _sessionMsgSearch.windowDays,
    includeSuperseded: sessionsIncludeSuperseded(),
    seq: _sessionMsgSearch.seq,
    token: _sessionMsgSearchToken,
  }),
});

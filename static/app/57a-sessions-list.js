// ── Sessions list rendering: windowed, signature-guarded, node-stable ──
// The corpus (_cachedSessions) can be thousands of rows; the DOM holds only
// `sessionsRenderWindow` cards (one SESSION_CARD_RENDER_PAGE initially,
// auto-grown page-at-a-time by a scroll sentinel, with the Show-more button
// as the explicit fallback). Each render pass is decided by a cheap
// signature: an identical pass is skipped outright, a grown window appends
// only the new cards, and only a changed rowset/filter/sort runs a full
// pass — and even a full pass reuses the DOM node of every card whose
// content signature is unchanged, so a stream refresh keeps node identity
// (and event listeners) for untouched rows instead of wiping the subtree.
// (Pass state lives in the early client-state block — deep-link TDZ.)

function sessionsListRenderStateFor(elId) {
  let st = _sessionsListRenderState.get(elId);
  if (!st) {
    st = {
      dataRef: null,
      optionsKey: '',
      window: 0,
      matched: null, // match entries from the last full pass (sorted, filtered)
      matchedCount: 0,
      hiddenSubagentCount: 0,
      renderedCount: 0,
      cards: new Map(), // row key → { sig, card } — bounded by the render window
      trailer: [],
      io: null,
      growPending: false,
      lastMode: 'none',
      lastMs: 0,
      passSeq: 0, // counts non-skip passes; QA sequencing hook
    };
    _sessionsListRenderState.set(elId, st);
  }
  return st;
}

// Wipe sites (loading skeletons, empty states, error placeholders) replace
// the list DOM out from under the pass state — they must drop it so the
// next render runs a full pass instead of appending into a foreign subtree.
function resetSessionsListRenderState(elId) {
  const st = elId ? _sessionsListRenderState.get(elId) : null;
  if (!st) return;
  if (st.io) st.io.disconnect();
  _sessionsListRenderState.delete(elId);
}

// Everything besides the rowset that changes what the list shows. A pass
// with the same data array identity and the same key as the previous one
// is a no-op (mergeSessionRows returns a new array whenever data changed).
function sessionsListOptionsKey(options, currentSessionId) {
  return JSON.stringify([
    options.mode || '',
    options.query || '',
    options.sortValue || 'updated-desc',
    options.projectFilter || '',
    options.sourceFilter || [],
    options.statusFilter || [],
    !!options.deepSearchOnly,
    options.deepSearchOnly ? _sessionDeepSearchToken : 0,
    options.deepSearchOnly ? !!_sessionDeepSearch.active : false,
    !!options.hideSubagents,
    currentSessionId,
    sessionsViewingPeer() ? currentSessionsHostId() : '',
    // Message lane (flagged): seq bumps on every visible lane change, so
    // arriving/clearing results re-key the pass. Constant 0 when inactive.
    options.mode === 'recent' ? _sessionMsgSearch.seq : 0,
  ]);
}

function collectSessionsListMatches(sessions, options, ctx) {
  const sortVal = options.sortValue || 'updated-desc';
  const [field, dir] = sortVal.split('-');
  const asc = dir === 'asc';
  const sortKey = (s) => {
    switch (field) {
      case 'created': return sessionDateSortValue(s, 'created_at');
      case 'updated': return sessionDateSortValue(s, 'updated_at');
      case 'cost': return s.estimated_cost || 0;
      case 'size': return s.total_bytes || 0;
      case 'turns': return s.turns || 0;
      default: return 0;
    }
  };
  // Decorate-sort-undecorate: sessionDateSortValue parses timestamps, so
  // key once per row instead of once per comparison.
  const decorated = sessions.map(s => [sortKey(s), s]);
  decorated.sort((a, b) => asc ? a[0] - b[0] : b[0] - a[0]);

  const query = options.query || '';
  const projectFilter = options.projectFilter || '';
  const sourceFilter = options.sourceFilter || [];
  const statusFilter = options.statusFilter || [];
  const matched = [];
  // Message lane: corpus membership decides which hits become stub rows —
  // evaluated per pass (a hit session can enter the corpus mid-hydration,
  // and its stub must yield to the real row, filters included).
  const msgKnown = ctx.msgSearch ? new Set() : null;
  let hiddenSubagentCount = 0;
  for (const [, s] of decorated) {
    const source = normalizeAgentId(s.source || '') || 'intendant';
    if (msgKnown && s.session_id) msgKnown.add(sessionLogSearchKey(source, s.session_id));
    const shortId = (s.session_id || '').substring(0, 8);
    const isCurrent = !!(ctx.currentSessionId && s.session_id && s.session_id.startsWith(ctx.currentSessionId));

    // Override status for current session — never show as abandoned/idle
    let displayStatus = s.status || 'in_progress';
    if (isCurrent && (displayStatus === 'abandoned' || displayStatus === 'idle')) {
      displayStatus = 'in_progress';
    }

    // Cheap filters first; sessionConfigMetadata (object merges per row)
    // only runs for rows that survive them.
    if (!sessionMatchesProjectFilter(s, projectFilter)) continue;
    if (!sessionMatchesSourceFilter(source, sourceFilter)) continue;
    if (!sessionMatchesStatusFilter(displayStatus, statusFilter)) continue;
    // Union with the message lane (flagged): a row whose messages match
    // stays visible even when the metadata haystack misses the query.
    const msgHit = ctx.msgSearch ? sessionMessageSearchHitFor(source, s.session_id) : null;
    if (!sessionMatchesSearch(s, query, displayStatus, source, shortId, isCurrent) && !msgHit) continue;
    const logHit = ctx.deepSearchOnly ? sessionDeepSearchHit(s, source) : null;
    if (ctx.deepSearchActive && !logHit) continue;
    if (ctx.hideSubagents && sessionRelationshipKindForRow(s) === 'subagent') {
      hiddenSubagentCount += 1;
      continue;
    }
    matched.push({ s, source, shortId, isCurrent, displayStatus, logHit, msgHit });
  }
  // Message-lane stubs: hits whose sessions are absent from the loaded
  // metadata corpus (mid-hydration, capped peer lists). Only the filters we
  // can honestly evaluate apply — source rides the response; project and
  // status are unknown for these rows, so any active project/status filter
  // hides them rather than guessing.
  if (ctx.msgSearch) {
    const projectActive = Array.isArray(projectFilter) ? projectFilter.length > 0 : !!projectFilter;
    if (!projectActive && statusFilter.length === 0) {
      for (const entry of sessionMessageSearchHits().values()) {
        if (msgKnown.has(entry.key)) continue;
        if (!sessionMatchesSourceFilter(entry.source, sourceFilter)) continue;
        matched.push(sessionMessageSearchStubMatch(entry));
      }
    }
  }
  return { matched, hiddenSubagentCount };
}

// Card inputs computed from OUTSIDE the row object itself (lineage across
// other rows, live config metadata, deep-search hits, peer browsing).
// The row's own fields are covered by JSON.stringify(row) in the card
// signature; this covers the rest, so signature equality ⇒ identical card.
function sessionCardDerived(m, ctx) {
  const { s } = m;
  const configMeta = sessionConfigMetadata(s);
  const configSource = sessionConfigSource(configMeta);
  const relationshipKind = sessionRelationshipKindForRow(s, configMeta);
  const backendSource = normalizeAgentId(
    s.backend_source ||
    s.backendSource ||
    s.configured_source ||
    s.configuredSource ||
    configMeta.backend_source ||
    configMeta.backendSource ||
    configMeta.configured_source ||
    configMeta.configuredSource ||
    ''
  );
  const hasExternalBackend = !!(
    backendSource && backendSource !== 'intendant' ||
    configSource && configSource !== 'intendant' ||
    s.backend_session_id ||
    s.backendSessionId ||
    configMeta.backendSessionId ||
    configMeta.backend_session_id
  );
  let nickname = '';
  let parentId = '';
  let parent = null;
  let parentLabel = '';
  if (relationshipKind) {
    nickname = compactSessionText(configMeta.agentNickname || s.agent_nickname || s.agentNickname);
    const lineage = sessionLineageParentForSession(ctx.lineageIndex, s, configMeta);
    parentId = lineage.parentId;
    parent = lineage.parent;
    parentLabel = sessionLineageDisplayLabel(parent, parentId);
  }
  const lineageChildren = sessionLineageChildrenForSession(ctx.lineageIndex, s, configMeta)
    .filter(child => !ctx.hideSubagents || sessionRelationshipKindForRow(child) !== 'subagent');
  // Quick-search hit inside the conversation preview: surface the message
  // matching the most query terms as a snippet line on the card (any-term
  // picking made a stray "to" beat the entry holding the actual phrase).
  let previewHit = null;
  if (ctx.query && Array.isArray(s.preview)) {
    const terms = ctx.query.split(/\s+/).filter(Boolean);
    let bestScore = 0;
    for (const p of s.preview) {
      const text = String((p && p.text) || '').toLowerCase();
      if (!text) continue;
      const score = terms.reduce((n, term) => n + (text.includes(term) ? 1 : 0), 0);
      if (score > bestScore) {
        bestScore = score;
        previewHit = p;
      }
    }
  }
  return {
    configMeta, configSource, relationshipKind, backendSource, hasExternalBackend,
    nickname, parentId, parent, parentLabel, lineageChildren, previewHit,
  };
}

function sessionCardSig(m, derived, ctx) {
  // Message-lane hit (flagged): the whole entry is card content (count,
  // best snippet, ranges, badges), so sign all of it. Empty when inactive.
  const msgHitSig = m.msgHit ? JSON.stringify(m.msgHit) : '';
  const childSig = derived.lineageChildren.map(child => {
    const childId = sessionLineageIds(child)[0] || child.session_id || '';
    return `${sessionRelationshipKindForRow(child)}\u001e${childId}\u001e${sessionLineageDisplayLabel(child, childId)}`;
  }).join('\u001d');
  const hitSig = m.logHit
    ? `${m.logHit.matches || 0}\u001e${(((m.logHit.snippets || [])[0]) || {}).content || ''}`
    : '';
  const previewHitSig = derived.previewHit
    ? `${derived.previewHit.role || ''}\u001e${derived.previewHit.text || ''}`
    : '';
  return JSON.stringify(m.s) + '\u001f' + JSON.stringify([
    m.displayStatus, m.isCurrent, m.source, ctx.viewingPeer,
    derived.relationshipKind, derived.configSource, derived.backendSource,
    derived.hasExternalBackend, derived.nickname, derived.parentId,
    derived.parentLabel, childSig, hitSig, previewHitSig, msgHitSig,
  ]);
}

// Reused cards keep their DOM nodes, so their relative "Nm ago" label is
// refreshed in place instead of by rebuild (the absolute timestamp lives
// in the tooltip and never drifts).
function refreshSessionCardRelativeLabel(card) {
  const changedAt = card.dataset.changedAt || '';
  if (!changedAt) return;
  const span = card.querySelector('.sc-meta .value[data-rel="changed"]');
  if (!span) return;
  const label = sessionRelativeLabel(changedAt) || changedAt;
  if (span.textContent !== label) span.textContent = label;
}

function sessionCardFor(m, ctx, st, nextCards) {
  // Message-lane stub rows (sessions the index knows but the loaded
  // metadata corpus does not) render their own compact card.
  if (m.msgStub) return sessionMessageSearchStubCardFor(m, ctx, st, nextCards);
  const key = sessionListRowKey(m.s);
  const derived = sessionCardDerived(m, ctx);
  const sig = sessionCardSig(m, derived, ctx);
  const cached = key ? st.cards.get(key) : null;
  let card;
  if (cached && cached.sig === sig) {
    card = cached.card;
    refreshSessionCardRelativeLabel(card);
  } else {
    card = buildSessionCard(m, derived, ctx);
    if (key) card.dataset.sessionKey = key;
    card.dataset.qaBuildSeq = String(++_sessionCardBuildSeq);
  }
  if (key) (nextCards || st.cards).set(key, { sig, card });
  return card;
}

function growSessionsRenderWindow() {
  sessionsRenderWindow += SESSION_CARD_RENDER_PAGE;
  renderSessionsViews();
}

function removeSessionsListTrailer(st) {
  for (const node of st.trailer) node.remove();
  st.trailer = [];
}

// Scroll sentinel + Show-more fallback below the rendered cards. The
// IntersectionObserver grows the window a page at a time as the user nears
// the bottom; the button stays for environments without IO and as the
// explicit bulk control.
function appendSessionsListTrailer(el, st) {
  removeSessionsListTrailer(st);
  const remaining = st.matchedCount - st.renderedCount;
  if (remaining <= 0) {
    if (st.io) st.io.disconnect();
    return;
  }
  const sentinel = document.createElement('div');
  sentinel.className = 'sessions-scroll-sentinel';
  sentinel.setAttribute('aria-hidden', 'true');
  const moreRow = document.createElement('div');
  moreRow.className = 'sessions-show-more';
  const moreBtn = document.createElement('button');
  moreBtn.type = 'button';
  moreBtn.className = 'ui-btn sessions-show-more-btn';
  moreBtn.textContent = `Show ${Math.min(SESSION_CARD_RENDER_PAGE, remaining).toLocaleString()} more (${remaining.toLocaleString()} remaining)`;
  moreBtn.onclick = () => growSessionsRenderWindow();
  moreRow.appendChild(moreBtn);
  el.appendChild(sentinel);
  el.appendChild(moreRow);
  st.trailer = [sentinel, moreRow];
  if (!st.io && typeof IntersectionObserver === 'function') {
    st.io = new IntersectionObserver((entries) => {
      if (!entries.some(e => e.isIntersecting)) return;
      if (st.growPending) return;
      st.growPending = true;
      requestAnimationFrame(() => {
        st.growPending = false;
        if (st.matchedCount > st.renderedCount) growSessionsRenderWindow();
      });
    }, { rootMargin: '600px 0px' });
  }
  if (st.io) {
    st.io.disconnect();
    st.io.observe(sentinel);
  }
}

function renderSessionsList(sessions, el, options = {}) {
  if (!el) return;
  updateSessionsSearchStatus();
  const elId = el.id || '';
  const t0 = performance.now();

  const deepSearchOnly = !!options.deepSearchOnly;
  if (deepSearchOnly && !_sessionDeepSearch.active) {
    const message = _sessionDeepSearch.loading
      ? 'Deep search is running...'
      : 'Run a deep search to show log matches';
    resetSessionsListRenderState(elId);
    el.innerHTML = `<div class="empty-state">${message}</div>`;
    return;
  }

  if (sessions.length === 0) {
    resetSessionsListRenderState(elId);
    el.innerHTML = '<div class="empty-state">No sessions found</div>';
    return;
  }

  const currentSessionId = daemonSessionFullId || '';
  const st = sessionsListRenderStateFor(elId);
  const optionsKey = sessionsListOptionsKey(options, currentSessionId);
  const windowSize = sessionsRenderWindow;
  const samePass = st.dataRef === sessions && st.optionsKey === optionsKey && st.matched !== null;
  if (samePass && st.window === windowSize) {
    st.lastMode = 'skip'; // identical pass — the DOM is already right
    return;
  }

  const ctx = {
    deepSearchOnly,
    deepSearchActive: deepSearchOnly && _sessionDeepSearch.active,
    // Message lane (flagged): live only when the applied results answer
    // exactly the query this pass renders (display-layer stale rejection).
    msgSearch: !deepSearchOnly && sessionMessageSearchActiveFor(options.query || ''),
    hideSubagents: !!options.hideSubagents,
    viewingPeer: sessionsViewingPeer(),
    currentSessionId,
    query: options.query || '',
    lineageIndex: buildSessionLineageIndex(sessions),
  };

  const appendOnly = samePass && windowSize > st.window && st.renderedCount > 0;
  let matched = st.matched;
  let hiddenSubagentCount = st.hiddenSubagentCount;
  if (!appendOnly) {
    const collected = collectSessionsListMatches(sessions, options, ctx);
    matched = collected.matched;
    hiddenSubagentCount = collected.hiddenSubagentCount;
  }
  const matchedCount = matched.length;

  if (matchedCount === 0) {
    const query = options.query || '';
    const projectFilter = options.projectFilter || '';
    const hasProjectFilter = Array.isArray(projectFilter) ? projectFilter.length > 0 : !!projectFilter;
    const empty = document.createElement('div');
    empty.className = 'empty-state';
    empty.textContent = ctx.deepSearchActive && sessionDeepSearchHasMissingMetadata()
      ? 'Loading session metadata for matching log results...'
      : ctx.deepSearchActive
      ? 'No sessions match the deep search results'
      : query
      ? 'No sessions match your search'
      : hasProjectFilter
      ? 'No sessions match the selected projects'
      : hiddenSubagentCount > 0
      ? 'No main sessions found. Subagent sessions are hidden from Recent.'
      : ctx.viewingPeer
      ? 'No sessions visible on this peer — it may not have granted session access.'
      : 'No sessions match the active filters';
    resetSessionsListRenderState(elId);
    el.innerHTML = '';
    el.appendChild(empty);
    return;
  }

  const renderedTarget = Math.min(windowSize, matchedCount);
  const nextCards = appendOnly ? null : new Map();
  const built = [];
  for (let i = appendOnly ? st.renderedCount : 0; i < renderedTarget; i++) {
    built.push(sessionCardFor(matched[i], ctx, st, nextCards));
  }

  if (appendOnly) {
    removeSessionsListTrailer(st);
    for (const card of built) el.appendChild(card);
  } else {
    st.cards = nextCards;
    st.trailer = [];
    el.replaceChildren(...built);
  }
  st.dataRef = sessions;
  st.optionsKey = optionsKey;
  st.window = windowSize;
  st.matched = matched;
  st.matchedCount = matchedCount;
  st.hiddenSubagentCount = hiddenSubagentCount;
  st.renderedCount = renderedTarget;
  appendSessionsListTrailer(el, st);
  st.lastMode = appendOnly ? 'append' : 'full';
  st.lastMs = performance.now() - t0;
  st.passSeq += 1;
}

// QA readback (window.qa convention): DOM-budget and node-identity facts
// the validate-dashboard harness asserts on — module scope hides them.
// Probe functions stay cheap and side-effect-free.
window.qa = Object.assign(window.qa || {}, {
  sessionsDom: (listId = 'sessions-list') => {
    const el = document.getElementById(listId);
    const st = _sessionsListRenderState.get(listId) || null;
    const cards = el ? Array.from(el.querySelectorAll('.session-card')) : [];
    return {
      nodes: el ? el.getElementsByTagName('*').length : 0,
      cards: cards.length,
      matched: st ? st.matchedCount : null,
      hiddenSubagents: st ? st.hiddenSubagentCount : null,
      corpus: Array.isArray(_cachedSessions) ? _cachedSessions.length : null,
      window: sessionsRenderWindow,
      page: SESSION_CARD_RENDER_PAGE,
      lastMode: st ? st.lastMode : 'none',
      lastMs: st ? st.lastMs : null,
      passSeq: st ? st.passSeq : 0,
      loadToken: _sessionsLoadToken,
      trailer: st ? st.trailer.length : 0,
      hydration: {
        active: _sessionsHydrationState.active,
        done: _sessionsHydrationState.done,
        phase: _sessionsHydrationState.phase,
        received: _sessionsHydrationState.received,
      },
      cardSeqs: cards.slice(0, 24).map(c => ({
        key: c.dataset.sessionKey || '',
        seq: Number(c.dataset.qaBuildSeq || 0),
      })),
    };
  },
  // QA ACTIONS (not probes — they mutate): the module scope hides the
  // sessions functions from harness-evaluated JS, so scripted scenarios
  // need explicit bridges (same reason as the window.* onclick bridges;
  // precedent: station.activate).
  sessionsActions: {
    reload: () => loadSessions({ force: true }),
    grow: () => growSessionsRenderWindow(),
    // Pull the complete corpus over the plain-JSON path — deterministic
    // fallback while the RPC stream leg can wedge before its replace.
    pullAll: () => fetchSessionsForHost(undefined, { force: true, limit: 'all' })
      .then(rows => applyLoadedSessions(rows, document.getElementById('sessions-aggregate'))),
  },
});

// Compaction boilerplate ("This session is being continued from a previous
// conversation…") makes a useless card title for continued sessions — skip
// to the next meaningful conversation content when the initial-message
// fallback starts with it. The full original stays in the tooltip via the
// callers' title attributes.
const SESSION_CONTINUED_BOILERPLATE_RE = /^this session is being continued from/i;

function sessionCardTaskText(s) {
  const raw = compactSessionText(s?.task);
  if (!raw || !SESSION_CONTINUED_BOILERPLATE_RE.test(raw)) return raw;
  // Next meaningful preview message (server-side conversation preview).
  if (Array.isArray(s?.preview)) {
    for (const p of s.preview) {
      const t = compactSessionText(p && p.text);
      if (t && !SESSION_CONTINUED_BOILERPLATE_RE.test(t)) return t;
    }
  }
  // Skip past the preamble to the summary body the boilerplate introduces.
  const idx = raw.search(/summar(?:ized|y)[^:]{0,40}:/i);
  if (idx >= 0) {
    const rest = raw.slice(raw.indexOf(':', idx) + 1).trim();
    if (rest) return `Continued · ${rest}`;
  }
  return `Continued session · ${raw}`;
}

function buildSessionCard(m, derived, ctx) {
  const { s, source, shortId, isCurrent, displayStatus, logHit } = m;
  const { configMeta, configSource, relationshipKind, backendSource, hasExternalBackend } = derived;
  const isExternal = source !== 'intendant';

  const rowIsPartial = s.partial === true;
  const card = document.createElement('div');
  // sc-split: content column + actions column as real flex layout. The
  // actions used to float absolutely over the card and collided with the
  // badge chips (and clipped "Delete…") whenever the cluster grew or the
  // card ran short.
  card.className = 'session-card sc-split';
  if (isCurrent) card.classList.add('current');
  if (displayStatus === 'abandoned') card.classList.add('dimmed');
  if (rowIsPartial) card.classList.add('partial');
  const main = document.createElement('div');
  main.className = 'sc-main';
  card.appendChild(main);

  const sessionName = compactSessionText(s.name);
  const rawTaskText = compactSessionText(s.task);
  const taskText = sessionCardTaskText(s);
  const sessionId = s.session_id || '';
  const isResident = s.role === 'resident' || displayStatus === 'resident';
  const primaryText = sessionName || taskText || (isResident ? 'Daemon session' : 'Untitled session');
  const titleKindText = sessionName ? 'name' : taskText ? 'initial' : isResident ? 'resident' : 'untitled';

  // Top row: name/task + status badges
  const top = document.createElement('div');
  top.className = 'sc-top';

  const titleBlock = document.createElement('div');
  titleBlock.className = 'sc-title-block';
  const titleRow = document.createElement('div');
  titleRow.className = 'sc-title-row';
  const titleKind = document.createElement('span');
  titleKind.className = 'sc-title-kind';
  titleKind.classList.add(titleKindText);
  titleKind.textContent = titleKindText;
  titleKind.title = sessionName
    ? 'User-assigned session name'
    : taskText
      ? 'Initial message fallback'
      : isResident
        ? "The daemon's own resident session — becomes a task session when you message it"
        : 'No initial message found';
  titleRow.appendChild(titleKind);
  const nameEl = document.createElement('div');
  nameEl.className = 'sc-name';
  nameEl.textContent = primaryText;
  // Tooltip keeps the untrimmed original when the visible title skipped
  // continuation boilerplate.
  nameEl.title = (!sessionName && rawTaskText && rawTaskText !== taskText) ? rawTaskText : primaryText;
  titleRow.appendChild(nameEl);
  titleBlock.appendChild(titleRow);
  top.appendChild(titleBlock);

  const statusEl = document.createElement('span');
  statusEl.className = `ui-chip ${sessionStatusChipTone(displayStatus)} sc-status ${displayStatus}`;
  statusEl.textContent = displayStatus.replace('_', ' ');
  top.appendChild(statusEl);

  const sourceEl = document.createElement('span');
  sourceEl.className = 'ui-badge sc-source' + (isExternal ? '' : ' local');
  sourceEl.textContent = isExternal
    ? (
      s.source_label ||
      s.sourceLabel ||
      s.backend_source_label ||
      s.backendSourceLabel ||
      prettyAgentName(source) ||
      source
    )
    : (s.source_label || s.sourceLabel || 'Intendant');
  if (!isExternal && hasExternalBackend && backendSource && backendSource !== 'intendant') {
    sourceEl.title = `${prettyAgentName(backendSource) || backendSource} backend`;
  }
  // Badge tint is CSS-keyed off the normalized source id (native=iris,
  // codex=neutral, claude-code=amber).
  sourceEl.dataset.src = source;
  top.appendChild(sourceEl);

  if (rowIsPartial) {
    const loadingEl = document.createElement('span');
    loadingEl.className = 'ui-chip info sc-role sc-loading-chip';
    loadingEl.textContent = 'loading details';
    loadingEl.title = 'Tokens, costs, lineage, and disk sizes are still hydrating';
    top.appendChild(loadingEl);
  }

  if (s.role) {
    const roleEl = document.createElement('span');
    roleEl.className = 'ui-chip muted sc-role';
    roleEl.textContent = s.role;
    top.appendChild(roleEl);
  }

  if (relationshipKind) {
    const { nickname, parentId, parent, parentLabel } = derived;
    const kindLabel = sessionLineageRelationshipLabel(relationshipKind);
    const chipText = relationshipKind === 'subagent' && nickname
      ? `${kindLabel} ${nickname}`
      : parentId
        ? `${kindLabel} of ${parentLabel}`
        : kindLabel;
    const titleParts = [
      relationshipKind === 'subagent' && nickname ? `Subagent: ${nickname}` : `Relationship: ${kindLabel}`,
      parentId ? `Parent: ${parentLabel} (${parentId})` : '',
    ].filter(Boolean);
    const relationEl = createSessionLineageListChip(s, parent, parentId, chipText, titleParts.join('\n') || `${kindLabel} session`);
    top.appendChild(relationEl);
  }

  const lineageChildren = derived.lineageChildren;
  if (lineageChildren.length > 0) {
    const childrenEl = document.createElement('span');
    childrenEl.className = 'ui-chip muted sc-role sc-lineage-chip sc-lineage-children';
    childrenEl.textContent = lineageChildren.length === 1 ? '1 child' : `${lineageChildren.length} children`;
    childrenEl.title = lineageChildren
      .slice(0, 8)
      .map(child => {
        const childKind = sessionLineageRelationshipLabel(sessionRelationshipKindForRow(child));
        const childId = sessionLineageIds(child)[0] || child.session_id || '';
        return `${childKind}: ${sessionLineageDisplayLabel(child, childId)}${childId ? ` (${childId})` : ''}`;
      })
      .join('\n') + (lineageChildren.length > 8 ? `\n+${lineageChildren.length - 8} more` : '');
    top.appendChild(childrenEl);
  }

  if (isCurrent) {
    const badge = document.createElement('span');
    badge.className = 'ui-chip info sc-current-badge';
    badge.textContent = 'current';
    top.appendChild(badge);
  }

  main.appendChild(top);

  // Task
  if (taskText && sessionName) {
    const taskEl = document.createElement('div');
    taskEl.className = 'sc-task';
    taskEl.textContent = taskText;
    taskEl.title = rawTaskText || taskText;
    card.appendChild(taskEl);
  }

  if (logHit) {
    const hitEl = document.createElement('div');
    hitEl.className = 'sc-search-hit';
    const hitCount = document.createElement('span');
    hitCount.className = 'sc-search-hit-count';
    const matches = Number(logHit.matches || 0);
    hitCount.textContent = `${matches} log ${matches === 1 ? 'match' : 'matches'}`;
    hitEl.appendChild(hitCount);
    const firstSnippet = (logHit.snippets || [])[0] || {};
    const hitText = document.createElement('span');
    hitText.className = 'sc-search-hit-text';
    hitText.textContent = firstSnippet.content || '';
    hitText.title = firstSnippet.content || '';
    hitEl.appendChild(hitText);
    card.appendChild(hitEl);
  }

  // Message-search hit (flagged quick-search message lane): the most
  // recent matching message with server-anchored highlights and
  // superseded/truncated/subagent badges.
  if (m.msgHit) {
    card.appendChild(buildSessionMessageHitBlock(m.msgHit));
  }

  // Quick-search hit inside the conversation preview: one quiet snippet
  // line showing where the match lives. Deep Search keeps the log-hit row
  // above for full-history matches.
  if (derived.previewHit) {
    const prevEl = document.createElement('div');
    prevEl.className = 'sc-preview-hit';
    const roleEl = document.createElement('span');
    roleEl.className = 'sc-preview-hit-role';
    roleEl.textContent = derived.previewHit.role === 'assistant' ? 'assistant' : 'user';
    prevEl.appendChild(roleEl);
    const textEl = document.createElement('span');
    textEl.className = 'sc-preview-hit-text';
    textEl.textContent = derived.previewHit.text || '';
    textEl.title = derived.previewHit.text || '';
    prevEl.appendChild(textEl);
    card.appendChild(prevEl);
  }

  // Meta row — the full session id (click to copy) plus compact
  // essentials; the long tail (absolute dates, provider, token breakdown,
  // disk) moves to the row tooltip and stays visible in the
  // session-detail overlay.
  const meta = document.createElement('div');
  meta.className = 'sc-meta';
  const addMetaField = (label, value, valueClass, valueTitle) => {
    if (value === null || value === undefined || value === '') return null;
    const span = document.createElement('span');
    const l = document.createElement('span');
    l.className = 'label';
    l.textContent = label;
    const v = document.createElement('span');
    v.className = 'value' + (valueClass ? ` ${valueClass}` : '');
    v.textContent = value;
    if (valueTitle) v.title = valueTitle;
    span.appendChild(l);
    span.appendChild(v);
    meta.appendChild(span);
    return v;
  };

  const idVal = addMetaField('id', sessionId || shortId, 'sc-id', 'Click to copy the session ID');
  if (idVal) {
    idVal.addEventListener('click', (e) => {
      e.stopPropagation();
      const fullId = sessionId || shortId;
      copyTextToClipboard(fullId)
        .then(() => showControlToast('success', `Copied session ID ${shortSessionId(fullId)}`))
        .catch(err => showControlToast('error', `Copy session ID failed: ${err?.message || err}`));
    });
  }
  const changedAt = s.updated_at || s.changed_at;
  if (changedAt) {
    const changedVal = addMetaField('changed', sessionRelativeLabel(changedAt) || changedAt, null, changedAt);
    if (changedVal) {
      // refreshSessionCardRelativeLabel re-derives this label in place when
      // the card is reused across render passes.
      changedVal.dataset.rel = 'changed';
      card.dataset.changedAt = changedAt;
    }
  }
  const projectPath = sessionProjectDirectory(s);
  if (projectPath) addMetaField('project', compactPathLabel(projectPath), null, projectPath);
  if (s.model) addMetaField('model', s.model, 'model-val');
  if (s.turns > 0) addMetaField('turns', s.turns);
  if (rowIsPartial) {
    const loading = document.createElement('span');
    loading.className = 'sc-loading-meta';
    const loadingLabel = document.createElement('span');
    loadingLabel.className = 'label';
    loadingLabel.textContent = 'details';
    const loadingVal = document.createElement('span');
    loadingVal.className = 'value';
    loadingVal.textContent = 'tokens, cost, lineage, size';
    loading.appendChild(loadingLabel);
    loading.appendChild(loadingVal);
    meta.appendChild(loading);
  } else {
    if (s.total_tokens > 0) addMetaField('tokens', s.total_tokens.toLocaleString());
    if (s.estimated_cost > 0) addMetaField('cost', formatUsd(s.estimated_cost));
  }

  const tipParts = [];
  if (s.created_at) tipParts.push(`created: ${s.created_at}`);
  if (changedAt) tipParts.push(`changed: ${changedAt}`);
  if (s.provider) tipParts.push(`provider: ${s.provider}`);
  if (!rowIsPartial && s.total_tokens > 0) {
    tipParts.push(`tokens: ${(s.prompt_tokens || 0).toLocaleString()} in${s.cached_tokens > 0 ? ` (${s.cached_tokens.toLocaleString()} cached)` : ''} / ${(s.completion_tokens || 0).toLocaleString()} out`);
  }
  if (!rowIsPartial && s.total_bytes > 0) tipParts.push(`disk: ${_fmtBytes(s.total_bytes)}`);
  if (projectPath) tipParts.push(`project: ${projectPath}`);
  if (tipParts.length) meta.title = tipParts.join('\n');
  main.appendChild(meta);

  // Media stats row (recordings, annotations, clips) + total session size
  const hasMedia = (s.recordings > 0 || s.annotations > 0 || s.clips > 0);
  if (!rowIsPartial && hasMedia) {
    const mediaMeta = document.createElement('div');
    mediaMeta.className = 'sc-meta sc-media';
    const addMediaField = (label, value, valueClass) => {
      const span = document.createElement('span');
      const l = document.createElement('span');
      l.className = 'label';
      l.textContent = label;
      const v = document.createElement('span');
      v.className = 'value' + (valueClass ? ` ${valueClass}` : '');
      v.textContent = value;
      span.appendChild(l);
      span.appendChild(v);
      mediaMeta.appendChild(span);
    };
    if (s.recordings > 0) addMediaField('recordings', s.recordings, 'v-blue');
    if (s.annotations > 0) addMediaField('annotations', s.annotations, 'v-peach');
    if (s.clips > 0) addMediaField('clips', s.clips, 'v-peach');
    if (s.total_bytes > 0) addMediaField('size', _fmtBytes(s.total_bytes), 'v-dim');
    main.appendChild(mediaMeta);
  }

  // Actions row. While browsing a peer host the cards are read-only —
  // resume/delete/config act on THIS daemon's stores and would target
  // the wrong machine; interaction happens on the peer's own dashboard
  // (whole-card click below).
  if (!ctx.viewingPeer
      && (sessionId || s.can_resume || s.can_delete || ((s.total_bytes > 0 || !isCurrent) && !isExternal))) {
    const actions = document.createElement('div');
    actions.className = 'sc-actions';

    if (s.can_resume !== false) {
      const resumeBtn = document.createElement('button');
      resumeBtn.className = 'ui-btn sc-resume-btn';
      resumeBtn.textContent = 'Resume session';
      resumeBtn.title = 'Resume this session in Activity';
      resumeBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        resumeSession(s);
      });
      actions.appendChild(resumeBtn);
    }

    if (sessionId) {
      const renameBtn = document.createElement('button');
      renameBtn.className = 'ui-btn sc-rename-btn';
      renameBtn.textContent = 'Rename';
      renameBtn.title = 'Rename this session';
      renameBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        requestSessionRename(s);
      });
      actions.appendChild(renameBtn);
    }

    if (sessionId && configSource && configSource !== 'intendant') {
      const configBtn = document.createElement('button');
      configBtn.className = 'ui-btn sc-rename-btn';
      configBtn.textContent = 'Configure';
      configBtn.title = 'Configure binary and managed context for this session';
      configBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        openSessionConfigModal(s);
      });
      actions.appendChild(configBtn);
    }

    const canDeleteLocalSessionData = source === 'intendant'
      ? s.can_delete !== false
      : !!(s.can_delete_intendant_log && s.intendant_session_id);
    if (canDeleteLocalSessionData) {
      const sessionDeleteLabel = source === 'intendant'
        ? 'Delete session'
        : 'Delete Intendant log';
      const sessionDeleteBytes = source === 'intendant'
        ? s.total_bytes
        : (s.intendant_total_bytes || 0);
      const btn = document.createElement('button');
      btn.className = 'ui-btn danger sc-delete-btn';
      btn.textContent = 'Delete...';
      btn.addEventListener('click', (e) => {
        e.stopPropagation();
        // Close any existing menu
        document.querySelectorAll('.sc-delete-menu').forEach(m => m.remove());
        const menu = document.createElement('div');
        menu.className = 'sc-delete-menu';
        menu.setAttribute('role', 'menu');

        const addItem = (label, target, bytes, danger) => {
          const item = document.createElement('button');
          item.type = 'button';
          item.className = 'sc-delete-menu-item' + (danger ? ' danger' : '');
          item.setAttribute('role', 'menuitem');
          const txt = document.createElement('span');
          txt.textContent = label;
          item.appendChild(txt);
          if (bytes > 0) {
            const hint = document.createElement('span');
            hint.className = 'size-hint';
            hint.textContent = _fmtBytes(bytes);
            item.appendChild(hint);
          }
          item.addEventListener('click', (ev) => {
            ev.stopPropagation();
            menu.remove();
            deleteSessionData(s.session_id, target, label);
          });
          menu.appendChild(item);
        };

        if ((s.recordings || 0) > 0) addItem('Delete recordings', 'recordings', s.recording_bytes || 0);
        if ((s.frames_bytes || 0) > 0 || (s.annotations || 0) > 0 || (s.clips || 0) > 0) addItem('Delete frames', 'frames', s.frames_bytes || 0);
        const hasMedia = (s.recordings || 0) > 0 || (s.frames_bytes || 0) > 0;
        if (hasMedia) addItem('Delete all media', 'media', (s.recording_bytes || 0) + (s.frames_bytes || 0));
        if ((s.turns_bytes || 0) > 0) addItem('Delete turn data', 'turns', s.turns_bytes);
        if (!isCurrent) addItem(sessionDeleteLabel, 'session', sessionDeleteBytes, true);

        if (menu.children.length > 0) {
          actions.appendChild(menu);
          const closeMenu = () => {
            menu.remove();
            document.removeEventListener('click', close);
            document.removeEventListener('keydown', onKey);
          };
          const close = (ev) => { if (!menu.contains(ev.target)) closeMenu(); };
          // Escape closes and hands focus back to the Delete… trigger so
          // keyboard users don't drop to the document body.
          const onKey = (ev) => {
            if (ev.key !== 'Escape') return;
            ev.preventDefault();
            ev.stopPropagation();
            closeMenu();
            btn.focus();
          };
          setTimeout(() => {
            document.addEventListener('click', close);
            document.addEventListener('keydown', onKey);
          }, 0);
          menu.querySelector('.sc-delete-menu-item')?.focus();
        }
      });
      actions.appendChild(btn);
    }
    card.appendChild(actions);
  }

  if (ctx.viewingPeer) {
    card.classList.add('sc-peer');
    card.title = 'Open this session on the peer’s dashboard';
    card.addEventListener('click', () => openPeerSessionExternally(s));
  } else {
    card.addEventListener('click', () => openSessionDetail(s));
  }
  // Keyboard access: the whole card is the open-detail affordance, so it
  // must be reachable and activatable without a pointer. Enter/Space on
  // the card itself (not on inner buttons — those handle their own keys).
  card.setAttribute('role', 'button');
  card.tabIndex = 0;
  card.setAttribute('aria-label', `Open session: ${primaryText}`);
  card.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter' && e.key !== ' ') return;
    if (e.target !== card) return;
    e.preventDefault();
    card.click();
  });
  return card;
}


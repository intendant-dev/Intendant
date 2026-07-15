// ── Context visualizer (Activity → Context pane) ──
// Snapshot store/replay, payload analysis, and the Three.js scene for
// the managed-context visualization, plus the pane DOM render path. The
// pane top-level state, category/theme tables, the contextViz handle
// object, and the analysis memo WeakMap stay in 48-router-settings.js:
// deep links run applyCurrentRoute during that fragment evaluation and
// can reach these functions before this fragment evaluates — function
// declarations hoist module-wide, but const/let would still be in TDZ.
function contextValueText(value) {
  if (value === null || value === undefined) return '--';
  if (typeof value === 'number') return value.toLocaleString();
  return String(value);
}

function escapeContextHtml(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function highlightJson(jsonText) {
  const tokenRe = /("(?:\\u[\da-fA-F]{4}|\\[^u]|[^\\"])*"\s*:)|("(?:\\u[\da-fA-F]{4}|\\[^u]|[^\\"])*")|\b(true|false)\b|\bnull\b|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?|[{}\[\],]/g;
  let html = '';
  let lastIndex = 0;
  let match;
  while ((match = tokenRe.exec(jsonText)) !== null) {
    const token = match[0];
    html += escapeContextHtml(jsonText.slice(lastIndex, match.index));
    let cls = 'json-number';
    if (match[1]) {
      cls = 'json-key';
    } else if (match[2]) {
      cls = 'json-string';
    } else if (token === 'true' || token === 'false') {
      cls = 'json-boolean';
    } else if (token === 'null') {
      cls = 'json-null';
    } else if (/^[{}\[\],]$/.test(token)) {
      cls = 'json-punctuation';
    }
    html += `<span class="${cls}">${escapeContextHtml(token)}</span>`;
    lastIndex = tokenRe.lastIndex;
  }
  html += escapeContextHtml(jsonText.slice(lastIndex));
  return html;
}

function contextTargetSessionId(sessionId) {
  const sid = String(sessionId || '').trim();
  return sid ? (sessionWindowTargetForLogSession(sid) || sid) : '';
}

function contextSessionKey(snapshot) {
  const sid = snapshot && String(snapshot.session_id || '').trim();
  return contextTargetSessionId(sid) || CONTEXT_GLOBAL_SESSION;
}

function contextSnapshotFingerprint(snapshot) {
  const raw = Object.prototype.hasOwnProperty.call(snapshot || {}, 'raw') ? snapshot.raw : snapshot;
  const rawMeta = raw && typeof raw === 'object' && !Array.isArray(raw)
    ? (raw._intendant_context || raw.context || {})
    : {};
  const rawMarker = rawMeta.raw_hash || rawMeta.request_id || rawMeta.raw_payload_id || (
    typeof raw === 'string' ? `${raw.length}:${raw.slice(0, 64)}` : ''
  );
  return [
    contextSessionKey(snapshot),
    snapshot?.ts || '',
    snapshot?.request_index ?? '',
    snapshot?.request_id || '',
    snapshot?.turn ?? '',
    snapshot?.label || '',
    snapshot?.format || '',
    snapshot?.token_count ?? '',
    snapshot?.token_count_kind || '',
    snapshot?.item_count ?? '',
    rawMarker,
  ].join('|');
}

function contextSnapshotRequestLabel(snapshot) {
  if (!snapshot) return 'request --';
  const idx = snapshot.request_index ?? snapshot.raw?._intendant_context?.request_index;
  if (idx !== undefined && idx !== null) return `request ${idx}`;
  if (snapshot.turn !== undefined && snapshot.turn !== null) return `turn ${snapshot.turn}`;
  return 'request --';
}

function contextSnapshotSortValue(snapshot) {
  const idx = snapshot?.request_index ?? snapshot?.raw?._intendant_context?.request_index;
  if (Number.isFinite(Number(idx))) return Number(idx);
  const ts = Date.parse(snapshot?.ts || '');
  if (Number.isFinite(ts)) return ts / 1000;
  return snapshot?.__context_seq || 0;
}

function normalizeContextSnapshot(snapshot) {
  const copy = { ...(snapshot || {}) };
  if (!copy.ts) copy.ts = new Date().toISOString();
  copy.__context_seq = contextSnapshotSeq++;
  copy.__context_key = contextSnapshotFingerprint(copy);
  return copy;
}

function resetContextSnapshotState() {
  latestContextSnapshot = null;
  contextSnapshotsBySession.clear();
  contextSnapshotTimelinesBySession.clear();
  contextReplayIndexBySession.clear();
  contextSnapshotExactFetches.clear();
  contextSnapshotExactFailures.clear();
  contextSelectedPartId = null;
  contextLastAnalysis = null;
  contextRawRenderedKey = null;
}

function storeContextSnapshot(snapshot) {
  if (!snapshot) return null;
  const normalized = normalizeContextSnapshot(snapshot);
  const sid = contextSessionKey(normalized);
  let timeline = contextSnapshotTimelinesBySession.get(sid);
  if (!timeline) {
    timeline = [];
    contextSnapshotTimelinesBySession.set(sid, timeline);
  }
  const duplicate = timeline.some(existing => existing.__context_key === normalized.__context_key);
  if (!duplicate) timeline.push(normalized);
  if (sid === CONTEXT_GLOBAL_SESSION) {
    latestContextSnapshot = normalized;
  } else {
    contextSnapshotsBySession.set(sid, normalized);
  }
  if (!latestContextSnapshot || normalized.__context_seq >= (latestContextSnapshot.__context_seq || 0)) {
    latestContextSnapshot = normalized;
  }
  return normalized;
}

function handleContextReplaySnapshots(snapshots) {
  resetContextSnapshotState();
  const sorted = [...(snapshots || [])].sort((a, b) => {
    const sa = contextSnapshotSortValue(a);
    const sb = contextSnapshotSortValue(b);
    if (sa !== sb) return sa - sb;
    return String(a?.request_id || '').localeCompare(String(b?.request_id || ''));
  });
  for (const snapshot of sorted) storeContextSnapshot(snapshot);
  if (contextPaneVisible()) scheduleContextPaneRender();
}

function contextTimelineForForegroundSession() {
  const sid = contextTargetSessionId(resolvePromptTargetSessionId());
  if (sid && contextSnapshotTimelinesBySession.has(sid)) {
    return { key: sid, timeline: contextSnapshotTimelinesBySession.get(sid) || [] };
  }
  if (sid && contextSnapshotTimelinesBySession.size === 1) {
    const first = contextSnapshotTimelinesBySession.entries().next().value;
    return { key: first[0], timeline: first[1] || [] };
  }
  if (sid && contextSnapshotTimelinesBySession.size > 0) {
    return { key: sid, timeline: [] };
  }
  if (latestContextSnapshot) {
    const key = contextSessionKey(latestContextSnapshot);
    return { key, timeline: contextSnapshotTimelinesBySession.get(key) || [] };
  }
  if (contextSnapshotTimelinesBySession.size === 1) {
    const first = contextSnapshotTimelinesBySession.entries().next().value;
    return { key: first[0], timeline: first[1] || [] };
  }
  return { key: CONTEXT_GLOBAL_SESSION, timeline: [] };
}

function formatContextTimestamp(ts) {
  if (!ts) return '--';
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return String(ts);
  return date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

function contextRawValue(snapshot) {
  return snapshot && Object.prototype.hasOwnProperty.call(snapshot, 'raw') ? snapshot.raw : snapshot;
}

function contextSnapshotRawMeta(snapshot) {
  const raw = contextRawValue(snapshot);
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return {};
  return raw._intendant_context || raw.context || {};
}

function contextSnapshotRawSummary(snapshot) {
  const raw = contextRawValue(snapshot);
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return {};
  return raw.summary || {};
}

function contextSnapshotFile(snapshot) {
  const meta = contextSnapshotRawMeta(snapshot);
  return String(snapshot?.snapshot_file || snapshot?.snapshotFile || meta.snapshot_file || '').trim();
}

function contextSnapshotExactReplayAvailable(snapshot) {
  const meta = contextSnapshotRawMeta(snapshot);
  const summary = contextSnapshotRawSummary(snapshot);
  return Boolean(
    snapshot?.exact_replay_available ||
    snapshot?.exactReplayAvailable ||
    meta.exact_replay_available ||
    meta.exactReplayAvailable ||
    summary.exact_replay_available ||
    summary.exactReplayAvailable
  );
}

function contextSnapshotHasExactRaw(snapshot) {
  if (!snapshot) return false;
  const raw = contextRawValue(snapshot);
  const meta = contextSnapshotRawMeta(snapshot);
  const summary = contextSnapshotRawSummary(snapshot);
  if (meta.raw_omitted || summary.raw_omitted) return false;
  if (meta.archive_mode === 'summary' || summary.kind === 'compact_context_snapshot') return false;
  if (Array.isArray(raw?.summary_parts)) return false;
  return raw !== undefined;
}

function contextSnapshotNeedsExact(snapshot) {
  if (!snapshot || snapshot.__exact_loaded || contextSnapshotHasExactRaw(snapshot)) return false;
  const hasSelector = Boolean(
    contextSnapshotFile(snapshot) ||
    snapshot.request_id ||
    (snapshot.request_index !== undefined && snapshot.request_index !== null) ||
    snapshot.ts
  );
  return Boolean(hasSelector && contextSnapshotExactReplayAvailable(snapshot));
}

function contextSnapshotLazySessionId(snapshot) {
  const sid = contextSessionKey(snapshot);
  if (sid && sid !== CONTEXT_GLOBAL_SESSION) return sid;
  return resolvePromptTargetSessionId() || daemonSessionFullId || '';
}

function contextSnapshotLazySource(snapshot) {
  const source = String(snapshot?.source || '').trim();
  return source || 'intendant';
}

function contextSnapshotExactFetchKey(snapshot) {
  return `${contextSnapshotLazySessionId(snapshot)}|${contextSnapshotFile(snapshot)}|${snapshot?.request_id || ''}|${snapshot?.request_index ?? ''}|${snapshot?.ts || ''}`;
}

function contextSnapshotExactFetchFailed(snapshot) {
  const key = contextSnapshotExactFetchKey(snapshot);
  return Boolean(key && contextSnapshotExactFailures.has(key));
}

function replaceStoredContextSnapshot(previous, exact) {
  if (!previous || !exact) return previous;
  const key = previous.__context_key || contextSnapshotFingerprint(previous);
  const sid = contextSessionKey(previous);
  const next = {
    ...previous,
    ...exact,
    raw: Object.prototype.hasOwnProperty.call(exact, 'raw') ? exact.raw : previous.raw,
    snapshot_file: contextSnapshotFile(exact) || contextSnapshotFile(previous),
    exact_replay_available: true,
    __exact_loaded: true,
    __context_key: key,
    __context_seq: previous.__context_seq,
  };
  const timeline = contextSnapshotTimelinesBySession.get(sid);
  if (timeline) {
    const idx = timeline.findIndex(item => item.__context_key === key);
    if (idx >= 0) timeline[idx] = next;
  }
  if (contextSnapshotsBySession.get(sid)?.__context_key === key) {
    contextSnapshotsBySession.set(sid, next);
  }
  if (latestContextSnapshot?.__context_key === key) {
    latestContextSnapshot = next;
  }
  return next;
}

function ensureExactContextSnapshot(snapshot) {
  if (!contextSnapshotNeedsExact(snapshot)) return null;
  const key = contextSnapshotExactFetchKey(snapshot);
  if (!key || contextSnapshotExactFailures.has(key)) return null;
  if (contextSnapshotExactFetches.has(key)) return contextSnapshotExactFetches.get(key);
  const sessionId = contextSnapshotLazySessionId(snapshot);
  const file = contextSnapshotFile(snapshot);
  if (!sessionId) return null;
  const source = contextSnapshotLazySource(snapshot);
  const rpcParams = { session_id: sessionId, source };
  if (file) {
    rpcParams.file = file;
  }
  if (snapshot.request_id) {
    rpcParams.request_id = snapshot.request_id;
  }
  if (snapshot.request_index !== undefined && snapshot.request_index !== null) {
    rpcParams.request_index = snapshot.request_index;
  }
  if (snapshot.ts) {
    rpcParams.ts = snapshot.ts;
  }
  // daemonApi (transport F2): tunnel first, direct HTTP per the GET-twin
  // fallback policy; the descriptor lifts the selector params into the
  // /api/session/{id}/context-snapshot query the legacy URL carried.
  const promise = daemonApi.request('api_session_context_snapshot', rpcParams)
    .then(resp => {
      const data = (resp.body && typeof resp.body === 'object') ? resp.body : {};
      if (!resp.ok || data?.error) {
        throw new Error(data?.error || `context snapshot fetch returned ${resp.status}`);
      }
      const exact = data.snapshot || data;
      if (!Object.prototype.hasOwnProperty.call(exact || {}, 'raw')) {
        throw new Error('context snapshot response did not include raw payload');
      }
      contextSnapshotExactFailures.delete(key);
      const replaced = replaceStoredContextSnapshot(snapshot, exact);
      contextRawRenderedKey = null;
      if (contextPaneVisible()) renderContextPane();
      return replaced;
    })
    .catch(err => {
      contextSnapshotExactFailures.add(key);
      console.warn('Failed to load exact context snapshot', err);
      return null;
    })
    .finally(() => {
      contextSnapshotExactFetches.delete(key);
    });
  contextSnapshotExactFetches.set(key, promise);
  return promise;
}

function contextFullText(value) {
  if (typeof value === 'string') return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch (_) {
    return String(value ?? '');
  }
}

function contextPreviewText(text, limit) {
  const cleaned = String(text ?? '').replace(/\s+\n/g, '\n').trim();
  if (cleaned.length > limit) return cleaned.slice(0, limit - 1) + '...';
  return cleaned;
}

function contextPreview(value, limit = 1600) {
  if (typeof value === 'string') {
    return contextPreviewText(value, limit);
  }
  if (value === null || value === undefined || typeof value !== 'object') {
    return contextPreviewText(String(value ?? ''), limit);
  }
  const firstText = contextFindFirstText(value);
  if (firstText) return contextPreviewText(firstText, limit);
  try {
    const seen = new WeakSet();
    const json = JSON.stringify(value, (key, val) => {
      if (typeof val === 'string' && val.length > 260) return val.slice(0, 257) + '...';
      if (val && typeof val === 'object') {
        if (seen.has(val)) return '[Circular]';
        seen.add(val);
      }
      return val;
    }, 2);
    return contextPreviewText(json, limit);
  } catch (_) {
    return contextPreviewText(String(value ?? ''), limit);
  }
}

function contextStringSize(value) {
  if (value === null || value === undefined) return 0;
  if (typeof value === 'string') return value.length;
  try {
    return JSON.stringify(value).length;
  } catch (_) {
    return String(value).length;
  }
}

function contextEstimateTokens(value) {
  const chars = contextStringSize(value);
  if (!chars) return 1;
  return Math.max(1, Math.ceil(chars / 4));
}

function contextFindFirstText(value) {
  if (!value) return '';
  if (typeof value === 'string') return value;
  if (Array.isArray(value)) {
    for (const item of value) {
      const found = contextFindFirstText(item);
      if (found) return found;
    }
    return '';
  }
  if (typeof value === 'object') {
    for (const key of ['text', 'input_text', 'output_text', 'summary', 'content', 'output', 'arguments']) {
      const candidate = value[key];
      if (typeof candidate === 'string' && candidate.trim()) return candidate;
    }
    for (const key of ['parts', 'content']) {
      const found = contextFindFirstText(value[key]);
      if (found) return found;
    }
  }
  return '';
}

function contextHasMedia(value) {
  if (!value) return false;
  if (Array.isArray(value)) return value.some(contextHasMedia);
  if (typeof value !== 'object') return false;
  const type = String(value.type || value.mime_type || value.mimeType || '').toLowerCase();
  if (/(image|audio|video|file)/.test(type)) return true;
  if (value.image_url || value.input_image || value.inline_data || value.inlineData || value.media) return true;
  return Object.values(value).some(v => typeof v === 'object' && contextHasMedia(v));
}

function contextNormalizeCategory(category) {
  return CONTEXT_CATEGORY_DEFS[category] ? category : 'other';
}

function contextAddPart(parts, category, title, subtitle, value, path, meta = {}) {
  const normalizedCategory = contextNormalizeCategory(category);
  const preview = contextPreview(value);
  const cleanTitle = String(title || CONTEXT_CATEGORY_DEFS[normalizedCategory].label || 'Context item').trim();
  parts.push({
    id: `part-${parts.length}`,
    index: parts.length,
    category: normalizedCategory,
    title: cleanTitle || 'Context item',
    subtitle: String(subtitle || '').trim(),
    path,
    value,
    preview,
    estimatedTokens: contextEstimateTokens(value),
    chars: contextStringSize(value),
    meta,
  });
}

function contextToolName(tool, fallbackIndex) {
  return tool?.function?.name || tool?.name || tool?.tool?.name || `tool ${fallbackIndex + 1}`;
}

function contextMessageCategory(item) {
  const role = String(item?.role || item?.speaker || '').toLowerCase();
  const type = String(item?.type || item?.kind || '').toLowerCase();
  if (type.includes('reasoning') || type.includes('thinking')) return 'reasoning';
  if (type.includes('function_call_output') || type === 'tool_result' || type === 'functionresponse') return 'tool_output';
  if (type.includes('function_call') || type === 'tool_use' || type === 'functioncall') return 'tool_call';
  if (role === 'system' || role === 'developer') return 'instructions';
  if (role === 'user' || role === 'human') return contextHasMedia(item) ? 'media' : 'user';
  if (role === 'assistant' || role === 'model') return contextHasMedia(item) ? 'media' : 'assistant';
  if (role === 'tool') return 'tool_output';
  if (contextHasMedia(item)) return 'media';
  return 'other';
}

function contextMessageTitle(item, index) {
  const role = String(item?.role || '').trim();
  const type = String(item?.type || item?.kind || '').trim();
  if (item?.name) return `${item.name} ${type || 'item'}`;
  if (item?.function?.name) return `${item.function.name} ${type || 'tool'}`;
  if (role && type) return `${role} ${type}`;
  if (role) return `${role} message`;
  if (type) return type.replace(/_/g, ' ');
  return `item ${index + 1}`;
}

function contextAddMessagePart(parts, item, index, path) {
  const category = contextMessageCategory(item);
  const title = contextMessageTitle(item, index);
  const subtitle = contextFindFirstText(item).slice(0, 180);
  contextAddPart(parts, category, title, subtitle, item, path, {
    role: item?.role,
    type: item?.type || item?.kind,
  });
}

function contextAddRequestConfig(parts, raw, consumedKeys) {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return;
  const config = {};
  for (const [key, value] of Object.entries(raw)) {
    if (consumedKeys.has(key)) continue;
    if (value === null || value === undefined) continue;
    if (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') {
      config[key] = value;
    } else if (key === 'reasoning' || key === 'metadata' || key === 'include' || key === 'tool_choice') {
      config[key] = value;
    }
  }
  if (Object.keys(config).length) {
    contextAddPart(parts, 'config', 'request configuration', 'Model, reasoning, cache, and request knobs', config, '$.config');
  }
}

function contextAddSummaryParts(parts, summaryParts) {
  for (const item of summaryParts || []) {
    const category = contextNormalizeCategory(item?.category || 'other');
    const estimatedTokens = Number(item?.estimated_tokens ?? item?.estimatedTokens ?? item?.tokens ?? 1);
    const chars = Number(item?.chars ?? 0);
    parts.push({
      id: `part-${parts.length}`,
      index: parts.length,
      category,
      title: String(item?.title || CONTEXT_CATEGORY_DEFS[category].label || 'Context item').trim(),
      subtitle: String(item?.subtitle || '').trim(),
      path: item?.path || '$',
      value: item,
      preview: String(item?.preview || item?.subtitle || '').trim(),
      estimatedTokens: Number.isFinite(estimatedTokens) ? Math.max(1, estimatedTokens) : 1,
      chars: Number.isFinite(chars) ? chars : 0,
      meta: item?.meta || {},
    });
  }
}

function analyzeContextSnapshotCached(snapshot) {
  if (!snapshot || typeof snapshot !== 'object') return analyzeContextSnapshot(snapshot);
  let analysis = contextAnalysisBySnapshot.get(snapshot);
  if (!analysis) {
    analysis = analyzeContextSnapshot(snapshot);
    contextAnalysisBySnapshot.set(snapshot, analysis);
  }
  return analysis;
}

function analyzeContextSnapshot(snapshot) {
  const raw = contextRawValue(snapshot);
  const parts = [];
  const consumedKeys = new Set();
  const summaryParts = Array.isArray(snapshot?.summary_parts)
    ? snapshot.summary_parts
    : (Array.isArray(raw?.summary_parts) ? raw.summary_parts : null);
  if (summaryParts) {
    contextAddSummaryParts(parts, summaryParts);
  } else if (raw && typeof raw === 'object' && !Array.isArray(raw)) {
    for (const key of ['instructions', 'system', 'system_instruction', 'developer', 'developer_message']) {
      if (raw[key]) {
        consumedKeys.add(key);
        contextAddPart(parts, 'instructions', key.replace(/_/g, ' '), contextFindFirstText(raw[key]).slice(0, 180), raw[key], `$.${key}`);
      }
    }
    if (Array.isArray(raw.tools)) {
      consumedKeys.add('tools');
      raw.tools.forEach((tool, index) => {
        contextAddPart(parts, 'schema', `tool schema: ${contextToolName(tool, index)}`, contextFindFirstText(tool).slice(0, 160), tool, `$.tools[${index}]`);
      });
    }
    if (Array.isArray(raw.input)) {
      consumedKeys.add('input');
      raw.input.forEach((item, index) => contextAddMessagePart(parts, item, index, `$.input[${index}]`));
    }
    if (Array.isArray(raw.messages)) {
      consumedKeys.add('messages');
      raw.messages.forEach((item, index) => contextAddMessagePart(parts, item, index, `$.messages[${index}]`));
    }
    if (Array.isArray(raw.contents)) {
      consumedKeys.add('contents');
      raw.contents.forEach((item, index) => contextAddMessagePart(parts, item, index, `$.contents[${index}]`));
    }
    if (Array.isArray(raw.history)) {
      consumedKeys.add('history');
      raw.history.forEach((item, index) => contextAddMessagePart(parts, item, index, `$.history[${index}]`));
    }
    contextAddRequestConfig(parts, raw, consumedKeys);
  } else if (Array.isArray(raw)) {
    raw.forEach((item, index) => contextAddMessagePart(parts, item, index, `$[${index}]`));
  }
  if (!parts.length && raw !== undefined) {
    contextAddPart(parts, 'other', 'raw context payload', 'Unrecognized context shape', raw, '$');
  }

  const estimateTotal = parts.reduce((sum, part) => sum + part.estimatedTokens, 0);
  const exactTotal = snapshot?.token_count_kind === 'backend_reported'
    ? Number(snapshot?.token_count || 0)
    : 0;
  const totalTokens = exactTotal > 0 ? exactTotal : estimateTotal;
  const scale = exactTotal > 0 && estimateTotal > 0 ? exactTotal / estimateTotal : 1;
  for (const part of parts) {
    part.tokens = Math.max(1, Math.round(part.estimatedTokens * scale));
    part.share = totalTokens > 0 ? part.tokens / totalTokens : 0;
  }
  const byCategory = new Map();
  for (const part of parts) {
    const current = byCategory.get(part.category) || { tokens: 0, count: 0 };
    current.tokens += part.tokens;
    current.count += 1;
    byCategory.set(part.category, current);
  }
  return {
    snapshot,
    raw,
    parts,
    byCategory,
    totalTokens,
    estimateTotal,
    exactTotal,
    effectiveWindow: Number(snapshot?.effective_context_window || snapshot?.context_window || 0),
    hardWindow: Number(snapshot?.hard_context_window || snapshot?.hardContextWindow || 0),
  };
}

function renderContextLegend(analysis) {
  const host = document.getElementById('context-legend');
  if (!host) return;
  host.innerHTML = '';
  if (!analysis || !analysis.parts.length) return;
  const categories = [...analysis.byCategory.keys()].sort((a, b) => CONTEXT_CATEGORY_ORDER.indexOf(a) - CONTEXT_CATEGORY_ORDER.indexOf(b));
  for (const category of categories) {
    const def = CONTEXT_CATEGORY_DEFS[category] || CONTEXT_CATEGORY_DEFS.other;
    const item = document.createElement('span');
    item.className = 'context-legend-item';
    const swatch = document.createElement('span');
    swatch.className = 'context-legend-swatch';
    swatch.style.background = def.color;
    const label = document.createElement('span');
    const stats = analysis.byCategory.get(category);
    label.textContent = `${def.label} ${Math.round((stats.tokens / Math.max(1, analysis.totalTokens)) * 100)}%`;
    item.append(swatch, label);
    host.appendChild(item);
  }
}

function renderContextDetail(analysis, part) {
  const set = (id, value) => {
    const el = document.getElementById(id);
    if (el) el.textContent = contextValueText(value);
  };
  if (!analysis || !analysis.parts.length) {
    set('context-detail-title', 'No segment selected');
    set('context-detail-subtitle', 'Context snapshots will appear here after the worker sends a model request.');
    set('context-detail-share', '--');
    set('context-detail-estimate', '--');
    set('context-detail-category', '--');
    set('context-detail-path', '--');
    set('context-detail-preview', 'Large context items appear here after selection.');
    return;
  }
  if (!part) {
    const categories = [...analysis.byCategory.entries()]
      .sort((a, b) => b[1].tokens - a[1].tokens)
      .map(([category, stats]) => `${CONTEXT_CATEGORY_DEFS[category]?.label || category}: ${Math.round((stats.tokens / Math.max(1, analysis.totalTokens)) * 100)}%`)
      .join(' | ');
    set('context-detail-title', 'Context overview');
    set('context-detail-subtitle', `${analysis.parts.length.toLocaleString()} segments across ${analysis.byCategory.size} categories. ${categories}`);
    set('context-detail-share', '100%');
    set('context-detail-estimate', `${analysis.totalTokens.toLocaleString()} tokens`);
    set('context-detail-category', 'all');
    set('context-detail-path', '$');
    set('context-detail-preview', 'Select a context item to render its full payload here.');
    return;
  }
  const def = CONTEXT_CATEGORY_DEFS[part.category] || CONTEXT_CATEGORY_DEFS.other;
  set('context-detail-title', part.title);
  set('context-detail-subtitle', part.subtitle || 'No text preview available for this segment.');
  set('context-detail-share', `${Math.round(part.share * 1000) / 10}%`);
  set('context-detail-estimate', `~${part.tokens.toLocaleString()} tokens`);
  set('context-detail-category', def.label);
  set('context-detail-path', part.path || '--');
  set('context-detail-preview', contextFullText(part.value) || '--');
}

function renderContextHotspots(analysis) {
  const host = document.getElementById('context-hotspots-list');
  if (!host) return;
  host.innerHTML = '';
  if (!analysis || !analysis.parts.length) {
    const empty = document.createElement('span');
    empty.className = 'context-replay-label';
    empty.textContent = 'No snapshots';
    host.appendChild(empty);
    return;
  }
  const hotspots = [...analysis.parts].sort((a, b) => b.tokens - a.tokens).slice(0, 6);
  for (const part of hotspots) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'context-hotspot-btn';
    btn.textContent = `${part.title} - ~${part.tokens.toLocaleString()}`;
    btn.title = `${part.path || ''} ${part.subtitle || ''}`.trim();
    btn.addEventListener('click', () => focusContextPart(part.id));
    host.appendChild(btn);
  }
}

function selectedContextPart(analysis) {
  if (!analysis || !contextSelectedPartId) return null;
  return analysis.parts.find(part => part.id === contextSelectedPartId) || null;
}

function updateContextReplayControls(snapshot) {
  const { key, timeline } = contextTimelineForForegroundSession();
  const liveBtn = document.getElementById('context-live-btn');
  const replayBtn = document.getElementById('context-replay-btn');
  const range = document.getElementById('context-replay-range');
  const label = document.getElementById('context-replay-label');
  if (liveBtn) liveBtn.classList.toggle('active', contextReplayMode === 'live');
  if (replayBtn) {
    replayBtn.classList.toggle('active', contextReplayMode === 'replay');
    replayBtn.disabled = timeline.length === 0;
  }
  const max = Math.max(0, timeline.length - 1);
  let idx = timeline.indexOf(snapshot);
  if (idx < 0 && timeline.length) idx = contextReplayIndexBySession.get(key) ?? max;
  idx = Math.max(0, Math.min(max, idx));
  if (range) {
    range.min = '0';
    range.max = String(max);
    range.value = String(idx);
    range.disabled = timeline.length <= 1;
  }
  if (label) {
    if (!timeline.length) {
      label.textContent = 'No snapshots';
    } else {
      const modeLabel = contextReplayMode === 'replay' ? 'Replay' : 'Live';
      label.textContent = `${modeLabel} snapshot ${idx + 1} / ${timeline.length} - ${contextSnapshotRequestLabel(snapshot)} - ${formatContextTimestamp(snapshot?.ts)}`;
    }
  }
}

function contextShouldHandleKeyboardEvent(ev) {
  if (ev.defaultPrevented || ev.altKey || ev.ctrlKey || ev.metaKey) return false;
  const target = ev.target;
  const tag = String(target?.tagName || '').toLowerCase();
  if (target?.isContentEditable || ['input', 'textarea', 'select'].includes(tag)) return false;
  return contextFocusMode || (activeTab === 'activity' && activeActivitySubtab === 'context');
}

function stepContextReplaySnapshot(delta) {
  const { key, timeline } = contextTimelineForForegroundSession();
  if (!timeline.length) return false;
  const currentSnapshot = contextSnapshotForForegroundSession();
  let current = timeline.indexOf(currentSnapshot);
  if (current < 0) current = contextReplayIndexBySession.get(key) ?? timeline.length - 1;
  current = Math.max(0, Math.min(timeline.length - 1, Number(current) || 0));
  const next = Math.max(0, Math.min(timeline.length - 1, current + delta));
  if (next !== current || contextReplayMode !== 'replay') {
    contextReplayMode = 'replay';
    contextReplayIndexBySession.set(key, next);
    contextSelectedPartId = null;
    // Coalesced like the slider: held arrow keys repeat faster than frames.
    scheduleContextPaneRender();
  }
  return true;
}

function updateContextFocusControls() {
  const pane = document.querySelector('.context-pane');
  if (pane) {
    pane.classList.toggle('context-focus', contextFocusMode);
    pane.classList.toggle('context-raw-open', contextRawOpen);
  }
  document.body.classList.toggle('context-focus-active', contextFocusMode);
  const rawBtn = document.getElementById('context-raw-toggle-btn');
  if (rawBtn) {
    rawBtn.setAttribute('aria-pressed', contextRawOpen ? 'true' : 'false');
    rawBtn.title = contextRawOpen ? 'Hide raw context snapshot' : 'Render full raw context snapshot';
    rawBtn.textContent = contextRawOpen ? 'Hide raw' : 'Raw';
  }
}

function setContextFocusMode(open, opts = {}) {
  const next = Boolean(open);
  if (contextFocusMode === next && !opts.force) return;
  contextFocusMode = next;
  if (!contextFocusMode) {
    contextRawOpen = false;
    contextRawRenderedKey = null;
  }
  updateContextFocusControls();
  if (contextFocusMode) {
    const pane = document.querySelector('.context-pane');
    if (opts.requestFullscreen && pane?.requestFullscreen) {
      pane.requestFullscreen().catch(() => {});
    }
    contextStartThreeAnimation();
    requestAnimationFrame(() => contextRenderThree());
  } else {
    if (document.fullscreenElement?.classList?.contains('context-pane')) {
      document.exitFullscreen().catch(() => {});
    }
    renderContextRaw(contextSnapshotForForegroundSession());
    contextRenderThree();
  }
}

function setContextRawOpen(open) {
  const next = Boolean(open);
  if (contextRawOpen === next) return;
  contextRawOpen = next;
  contextRawRenderedKey = null;
  updateContextFocusControls();
  if (contextPaneVisible()) renderContextPane();
}

function wireContextPaneListeners() {
  if (contextListenersWired) return;
  contextListenersWired = true;
  document.addEventListener('click', ev => {
    const target = ev.target instanceof Element
      ? ev.target.closest('#context-live-btn,#context-replay-btn,#context-reset-view-btn,#context-open-focus-btn,#context-close-focus-btn,#context-raw-toggle-btn')
      : null;
    if (!target) return;
    if (target.id === 'context-live-btn') {
      contextReplayMode = 'live';
      contextSelectedPartId = null;
      renderContextPane();
      return;
    }
    if (target.id === 'context-replay-btn') {
      if (target.disabled) return;
      const { key, timeline } = contextTimelineForForegroundSession();
      if (!timeline.length) return;
      contextReplayMode = 'replay';
      if (!contextReplayIndexBySession.has(key)) contextReplayIndexBySession.set(key, timeline.length - 1);
      renderContextPane();
      return;
    }
    if (target.id === 'context-reset-view-btn') {
      contextResetView();
      return;
    }
    if (target.id === 'context-open-focus-btn') {
      setContextFocusMode(true, { requestFullscreen: true });
      return;
    }
    if (target.id === 'context-close-focus-btn') {
      setContextFocusMode(false);
      return;
    }
    if (target.id === 'context-raw-toggle-btn') {
      setContextRawOpen(!contextRawOpen);
    }
  });
  document.addEventListener('input', ev => {
    const target = ev.target;
    if (!(target instanceof HTMLInputElement) || target.id !== 'context-replay-range') return;
    const { key, timeline } = contextTimelineForForegroundSession();
    if (!timeline.length) return;
    contextReplayMode = 'replay';
    contextReplayIndexBySession.set(key, Math.max(0, Math.min(timeline.length - 1, Number(target.value) || 0)));
    contextSelectedPartId = null;
    // rAF-coalesced: range inputs fire per pixel while dragging, and each
    // distinct index is a different snapshot (full analyze + scene build).
    // Intermediate positions within one frame render nothing the eye
    // would have seen; the trailing frame shows the slider's final index.
    scheduleContextPaneRender();
  });
  document.addEventListener('keydown', ev => {
    if (contextShouldHandleKeyboardEvent(ev)) {
      const delta = {
        ArrowLeft: -1,
        ArrowUp: -1,
        ArrowRight: 1,
        ArrowDown: 1,
      }[ev.key];
      if (delta && stepContextReplaySnapshot(delta)) {
        ev.preventDefault();
        return;
      }
    }
    if (!contextFocusMode || ev.key !== 'Escape') return;
    if (contextRawOpen) {
      setContextRawOpen(false);
    } else {
      setContextFocusMode(false);
    }
  });
}

function focusContextPart(partId) {
  contextSelectedPartId = partId;
  ensureExactContextSnapshot(contextSnapshotForForegroundSession());
  const part = selectedContextPart(contextLastAnalysis);
  renderContextDetail(contextLastAnalysis, part);
  contextUpdateMeshSelection();
  contextViz.zoom = Math.min(contextViz.zoom, 28);
  contextRenderThree();
  // The bob loop runs only while a part is selected — (re)arm it here
  // (it exits on deselect/tab-away by itself).
  if (contextPaneVisible()) contextStartThreeAnimation();
}

function contextDisposeObject(object) {
  object.traverse(child => {
    if (child.geometry) child.geometry.dispose();
    if (child.material) {
      const materials = Array.isArray(child.material) ? child.material : [child.material];
      for (const material of materials) {
        if (material.map) material.map.dispose();
        material.dispose();
      }
    }
  });
}

function contextClampZoom(value) {
  return Math.max(12, Math.min(82, value));
}

function contextPointerDistance(points) {
  if (points.length < 2) return 0;
  const dx = points[0].x - points[1].x;
  const dy = points[0].y - points[1].y;
  return Math.sqrt(dx * dx + dy * dy);
}

function contextPointerCenter(points) {
  if (!points.length) return { x: 0, y: 0 };
  let x = 0;
  let y = 0;
  for (const point of points) {
    x += point.x;
    y += point.y;
  }
  return { x: x / points.length, y: y / points.length };
}

function contextResetView() {
  contextViz.rotationX = 0.34;
  contextViz.rotationY = -0.48;
  contextViz.zoom = 38;
  contextViz.targetX = 0;
  contextViz.targetY = 2.2;
  contextViz.targetZ = 0;
  contextRenderThree();
}

function contextPanCamera(dx, dy) {
  if (!contextViz.camera || !contextViz.canvas || typeof THREE === 'undefined') return;
  const rect = contextViz.canvas.getBoundingClientRect();
  const scale = contextViz.zoom / Math.max(240, rect.height) * 1.75;
  contextViz.camera.updateMatrixWorld();
  const right = new THREE.Vector3().setFromMatrixColumn(contextViz.camera.matrixWorld, 0);
  const up = new THREE.Vector3().setFromMatrixColumn(contextViz.camera.matrixWorld, 1);
  const x = -dx * scale;
  const y = dy * scale;
  contextViz.targetX += right.x * x + up.x * y;
  contextViz.targetY += right.y * x + up.y * y;
  contextViz.targetZ += right.z * x + up.z * y;
}

function contextResetPointerState() {
  contextViz.dragging = false;
  contextViz.dragMoved = false;
  contextViz.panMode = false;
  contextViz.primaryPointerId = null;
  contextViz.pinchDistance = 0;
  contextViz.activePointers.clear();
  contextViz.canvas?.classList.remove('dragging');
}

function contextInitThree() {
  const canvas = document.getElementById('context-scene');
  if (!canvas) return false;
  if (contextViz.renderer) return true;
  contextViz.canvas = canvas;
  contextViz.scene = new THREE.Scene();
  contextViz.camera = new THREE.PerspectiveCamera(42, 1, 0.1, 1000);
  contextViz.camera.position.set(0, 18, contextViz.zoom);
  contextViz.renderer = new THREE.WebGLRenderer({ canvas, antialias: true, alpha: true });
  contextViz.renderer.setPixelRatio(Math.min(2, window.devicePixelRatio || 1));
  contextViz.root = new THREE.Group();
  contextViz.scene.add(contextViz.root);
  contextViz.raycaster = new THREE.Raycaster();
  contextViz.pointer = new THREE.Vector2();
  const ambient = new THREE.AmbientLight(0xffffff, 0.52);
  const key = new THREE.DirectionalLight(0xffffff, 1.15);
  key.position.set(-14, 24, 18);
  const fill = new THREE.PointLight(0x94e2d5, 0.8, 90);
  fill.position.set(18, 13, -16);
  contextViz.scene.add(ambient, key, fill);

  contextViz.resizeObserver = new ResizeObserver(() => contextRenderThree());
  contextViz.resizeObserver.observe(canvas.parentElement || canvas);
  canvas.addEventListener('pointerdown', ev => {
    ev.preventDefault();
    canvas.setPointerCapture?.(ev.pointerId);
    contextViz.activePointers.set(ev.pointerId, { x: ev.clientX, y: ev.clientY, type: ev.pointerType });
    if (contextViz.activePointers.size === 2) {
      const points = [...contextViz.activePointers.values()];
      contextViz.pinchDistance = contextPointerDistance(points);
      const center = contextPointerCenter(points);
      contextViz.pinchCenterX = center.x;
      contextViz.pinchCenterY = center.y;
      contextViz.pinchZoom = contextViz.zoom;
      contextViz.dragging = false;
      contextViz.dragMoved = true;
      canvas.classList.remove('dragging');
      return;
    }
    if (contextViz.activePointers.size > 1) return;
    contextViz.dragging = true;
    contextViz.dragMoved = false;
    contextViz.panMode = ev.button === 1 || ev.button === 2 || ev.shiftKey;
    contextViz.primaryPointerId = ev.pointerId;
    contextViz.dragX = ev.clientX;
    contextViz.dragY = ev.clientY;
    canvas.classList.add('dragging');
  });
  canvas.addEventListener('pointermove', ev => {
    if (contextViz.activePointers.has(ev.pointerId)) {
      contextViz.activePointers.set(ev.pointerId, { x: ev.clientX, y: ev.clientY, type: ev.pointerType });
    }
    if (contextViz.activePointers.size >= 2) {
      ev.preventDefault();
      const points = [...contextViz.activePointers.values()];
      const distance = contextPointerDistance(points);
      if (distance > 4 && contextViz.pinchDistance > 4) {
        contextViz.zoom = contextClampZoom(contextViz.pinchZoom * (contextViz.pinchDistance / distance));
        contextViz.dragMoved = true;
      }
      const center = contextPointerCenter(points);
      contextPanCamera(center.x - contextViz.pinchCenterX, center.y - contextViz.pinchCenterY);
      contextViz.pinchCenterX = center.x;
      contextViz.pinchCenterY = center.y;
      contextRenderThree();
      return;
    }
    if (!contextViz.dragging) return;
    ev.preventDefault();
    if (contextViz.primaryPointerId !== null && ev.pointerId !== contextViz.primaryPointerId) return;
    const dx = ev.clientX - contextViz.dragX;
    const dy = ev.clientY - contextViz.dragY;
    if (Math.abs(dx) + Math.abs(dy) > 2) contextViz.dragMoved = true;
    contextViz.dragX = ev.clientX;
    contextViz.dragY = ev.clientY;
    if (contextViz.panMode || ev.shiftKey) {
      contextViz.panMode = true;
      contextPanCamera(dx, dy);
    } else {
      contextViz.rotationY += dx * 0.008;
      contextViz.rotationX = Math.max(-1.22, Math.min(1.22, contextViz.rotationX + dy * 0.006));
    }
    contextRenderThree();
  });
  canvas.addEventListener('pointerup', ev => {
    ev.preventDefault();
    canvas.classList.remove('dragging');
    canvas.releasePointerCapture?.(ev.pointerId);
    contextViz.activePointers.delete(ev.pointerId);
    const shouldPick = !contextViz.dragMoved && !contextViz.panMode && contextViz.primaryPointerId === ev.pointerId;
    if (contextViz.activePointers.size === 1) {
      const [nextId, point] = contextViz.activePointers.entries().next().value;
      contextViz.primaryPointerId = nextId;
      contextViz.dragX = point.x;
      contextViz.dragY = point.y;
      contextViz.dragging = true;
      contextViz.panMode = false;
      return;
    }
    if (contextViz.activePointers.size === 0) {
      contextViz.dragging = false;
      contextViz.panMode = false;
      contextViz.primaryPointerId = null;
      contextViz.pinchDistance = 0;
    }
    if (shouldPick) contextPickThreePart(ev);
  });
  canvas.addEventListener('pointercancel', ev => {
    canvas.releasePointerCapture?.(ev.pointerId);
    contextResetPointerState();
  });
  canvas.addEventListener('wheel', ev => {
    ev.preventDefault();
    contextViz.zoom = contextClampZoom(contextViz.zoom + ev.deltaY * 0.035);
    contextRenderThree();
  }, { passive: false });
  canvas.addEventListener('contextmenu', ev => ev.preventDefault());
  return true;
}

function contextSetThreeSize() {
  if (!contextViz.renderer || !contextViz.canvas) return;
  const rect = contextViz.canvas.getBoundingClientRect();
  const width = Math.max(1, Math.floor(rect.width));
  const height = Math.max(1, Math.floor(rect.height));
  const size = contextViz.renderer.getSize(new THREE.Vector2());
  if (size.x !== width || size.y !== height) {
    contextViz.renderer.setSize(width, height, false);
    contextViz.camera.aspect = width / height;
    contextViz.camera.updateProjectionMatrix();
  }
}

function contextLabelSprite(text, color = CONTEXT_VIZ_THEME.labelText, scale = 1) {
  const canvas = document.createElement('canvas');
  const ctx = canvas.getContext('2d');
  const fontSize = 44;
  ctx.font = `600 ${fontSize}px system-ui, sans-serif`;
  const metrics = ctx.measureText(text);
  canvas.width = Math.ceil(metrics.width + 28);
  canvas.height = 72;
  ctx.font = `600 ${fontSize}px system-ui, sans-serif`;
  ctx.fillStyle = CONTEXT_VIZ_THEME.labelBg;
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.fillStyle = color;
  ctx.textBaseline = 'middle';
  ctx.fillText(text, 14, canvas.height / 2);
  const texture = new THREE.CanvasTexture(canvas);
  const material = new THREE.SpriteMaterial({ map: texture, transparent: true });
  const sprite = new THREE.Sprite(material);
  sprite.scale.set((canvas.width / canvas.height) * scale, scale, 1);
  return sprite;
}

// Label sprites are the GPU-upload churn of a scene rebuild (2D-canvas text
// rasterize + CanvasTexture upload per label). Text/color/scale triples
// recur across rebuilds — category lane names, the timeline caption — so
// cache the sprites and let contextClearThreeRoot detach them undisposed.
// Eviction disposes only unparented sprites; one still in the scene loses
// its cached flag instead, so the next root clear disposes it normally.
const contextLabelSpriteCache = new Map();
const CONTEXT_LABEL_SPRITE_CACHE_CAP = 96;
function contextLabelSpriteCached(text, color = CONTEXT_VIZ_THEME.labelText, scale = 1) {
  // labelBg is read inside contextLabelSprite - key on it too so a theme
  // flip re-rasterizes instead of serving stale-background sprites.
  const key = [text, color, scale, CONTEXT_VIZ_THEME.labelBg].join('\u001f');
  let sprite = contextLabelSpriteCache.get(key);
  if (sprite) {
    contextLabelSpriteCache.delete(key);
    contextLabelSpriteCache.set(key, sprite); // LRU bump
    return sprite;
  }
  sprite = contextLabelSprite(text, color, scale);
  sprite.userData.cachedLabel = true;
  contextLabelSpriteCache.set(key, sprite);
  while (contextLabelSpriteCache.size > CONTEXT_LABEL_SPRITE_CACHE_CAP) {
    const [oldKey, old] = contextLabelSpriteCache.entries().next().value;
    contextLabelSpriteCache.delete(oldKey);
    if (old.parent) old.userData.cachedLabel = false;
    else contextDisposeObject(old);
  }
  return sprite;
}

function contextClearThreeRoot() {
  if (!contextViz.root) return;
  for (const child of [...contextViz.root.children]) {
    contextViz.root.remove(child);
    if (child.userData && child.userData.cachedLabel) continue;
    contextDisposeObject(child);
  }
  contextViz.meshes = [];
  contextViz.sceneKey = '';
}

function contextPressureColor(pct) {
  if (pct >= 0.92) return CONTEXT_VIZ_THEME.pressure.high;
  if (pct >= 0.55) return CONTEXT_VIZ_THEME.pressure.mid;
  return CONTEXT_VIZ_THEME.pressure.ok;
}

function contextBuildThree(analysis, snapshot) {
  const empty = document.getElementById('context-scene-empty');
  if (empty) empty.style.display = analysis && analysis.parts.length ? 'none' : 'flex';
  if (!contextInitThree()) return;
  // Scene identity: analyzed snapshot + the timeline shape the rail bars
  // render from (+ theme, whose colors bake into materials). Re-renders
  // for the SAME scene — another session's context_snapshot arriving while
  // this pane is visible, detail-pane churn, selection changes — used to
  // tear down and rebuild every geometry, material, and label texture.
  // Selection highlighting mutates in place (contextUpdateMeshSelection),
  // so it deliberately stays out of the key.
  const { timeline } = contextTimelineForForegroundSession();
  const sceneKey = analysis && analysis.parts.length
    ? [
        contextRawRenderKey(snapshot),
        timeline.length,
        timeline.indexOf(snapshot),
        CONTEXT_VIZ_THEME.labelBg,
      ].join('|')
    : 'empty';
  if (contextViz.sceneKey === sceneKey && (sceneKey === 'empty' || contextViz.meshes.length)) {
    contextUpdateMeshSelection();
    contextRenderThree();
    return;
  }
  contextClearThreeRoot();
  if (!analysis || !analysis.parts.length) {
    contextViz.sceneKey = 'empty';
    contextRenderThree();
    return;
  }
  const root = contextViz.root;
  const categories = [...analysis.byCategory.keys()].sort((a, b) => CONTEXT_CATEGORY_ORDER.indexOf(a) - CONTEXT_CATEGORY_ORDER.indexOf(b));
  const laneSpacing = 5.6;
  const laneMap = new Map(categories.map((category, idx) => [category, (idx - (categories.length - 1) / 2) * laneSpacing]));
  const zSpread = 24;
  const maxTokens = Math.max(1, ...analysis.parts.map(part => part.tokens));
  const baseMaterial = new THREE.MeshStandardMaterial({ color: new THREE.Color(CONTEXT_VIZ_THEME.laneBase), transparent: true, opacity: 0.28, roughness: 0.84, metalness: 0.05 });
  for (const category of categories) {
    const x = laneMap.get(category) || 0;
    const lane = new THREE.Mesh(new THREE.BoxGeometry(4.3, 0.08, zSpread + 4), baseMaterial.clone());
    lane.position.set(x, -0.06, 0);
    root.add(lane);
    const def = CONTEXT_CATEGORY_DEFS[category] || CONTEXT_CATEGORY_DEFS.other;
    const label = contextLabelSpriteCached(def.label, def.color, 1.25);
    label.position.set(x, 0.65, -zSpread / 2 - 2.2);
    root.add(label);
  }

  analysis.parts.forEach((part, idx) => {
    const def = CONTEXT_CATEGORY_DEFS[part.category] || CONTEXT_CATEGORY_DEFS.other;
    const x = laneMap.get(part.category) || 0;
    const z = analysis.parts.length <= 1 ? 0 : (idx / (analysis.parts.length - 1) - 0.5) * zSpread;
    const height = Math.max(0.45, Math.min(8.8, 0.35 + Math.log10(part.tokens + 1) * 1.35));
    const depth = Math.max(0.65, Math.min(3.8, 0.65 + Math.sqrt(part.tokens / maxTokens) * 3.1));
    const width = Math.max(1.1, Math.min(3.9, 1.15 + Math.sqrt(part.share) * 5.0));
    const geometry = new THREE.BoxGeometry(width, height, depth);
    const material = new THREE.MeshStandardMaterial({
      color: new THREE.Color(def.color),
      roughness: 0.56,
      metalness: 0.08,
      emissive: new THREE.Color(def.color),
      emissiveIntensity: part.id === contextSelectedPartId ? 0.32 : 0.06,
      transparent: true,
      opacity: 0.92,
    });
    const mesh = new THREE.Mesh(geometry, material);
    mesh.position.set(x, height / 2, z);
    mesh.userData.partId = part.id;
    mesh.userData.baseY = mesh.position.y;
    mesh.userData.tokens = part.tokens;
    root.add(mesh);
    contextViz.meshes.push(mesh);
    if (part.share > 0.16) {
      const ring = new THREE.Mesh(
        new THREE.TorusGeometry(Math.max(width, depth) * 0.72, 0.035, 8, 48),
        new THREE.MeshBasicMaterial({ color: new THREE.Color(contextPressureColor(part.share)), transparent: true, opacity: 0.72 })
      );
      ring.rotation.x = Math.PI / 2;
      ring.position.set(x, 0.08, z);
      root.add(ring);
    }
  });

  const effectiveWindow = analysis.effectiveWindow || analysis.hardWindow || analysis.totalTokens;
  const hardWindow = analysis.hardWindow || effectiveWindow;
  const pct = effectiveWindow > 0 ? Math.min(1.25, analysis.totalTokens / effectiveWindow) : 0;
  const reservoirX = ((categories.length - 1) / 2) * laneSpacing + 7.2;
  const reservoirHeight = 10;
  const frame = new THREE.LineSegments(
    new THREE.EdgesGeometry(new THREE.BoxGeometry(2.4, reservoirHeight, 2.4)),
    new THREE.LineBasicMaterial({ color: new THREE.Color(CONTEXT_VIZ_THEME.frame), transparent: true, opacity: 0.72 })
  );
  frame.position.set(reservoirX, reservoirHeight / 2, 0);
  root.add(frame);
  const fillHeight = Math.max(0.08, Math.min(reservoirHeight, pct * reservoirHeight));
  const fill = new THREE.Mesh(
    new THREE.BoxGeometry(1.75, fillHeight, 1.75),
    new THREE.MeshStandardMaterial({ color: new THREE.Color(contextPressureColor(pct)), transparent: true, opacity: 0.78, roughness: 0.45 })
  );
  fill.position.set(reservoirX, fillHeight / 2, 0);
  root.add(fill);
  const usageLabel = contextLabelSpriteCached(`${Math.round(pct * 100)}% window`, contextPressureColor(pct), 1.1);
  usageLabel.position.set(reservoirX, reservoirHeight + 1.1, 0);
  root.add(usageLabel);
  if (hardWindow && hardWindow !== effectiveWindow) {
    const hardLabel = contextLabelSpriteCached('hard limit tracked', CONTEXT_VIZ_THEME.pressure.high, 0.9);
    hardLabel.position.set(reservoirX, -0.2, 2.7);
    root.add(hardLabel);
  }

  if (timeline.length > 1) {
    const selectedIdx = timeline.indexOf(snapshot);
    const railWidth = Math.min(28, Math.max(10, timeline.length * 0.72));
    timeline.forEach((entry, idx) => {
      const entryWindow = Number(entry.effective_context_window || entry.context_window || effectiveWindow || 0);
      const entryTokens = entry.token_count_kind === 'backend_reported'
        ? Number(entry.token_count || 0)
        : 0;
      const entryPct = entryWindow > 0 ? Math.min(1.1, entryTokens / entryWindow) : (idx + 1) / timeline.length;
      const barHeight = 0.25 + entryPct * 4.6;
      const x = timeline.length <= 1 ? 0 : (idx / (timeline.length - 1) - 0.5) * railWidth;
      const color = idx === selectedIdx ? CONTEXT_VIZ_THEME.selected : contextPressureColor(entryPct);
      const bar = new THREE.Mesh(
        new THREE.BoxGeometry(0.22, barHeight, 0.55),
        new THREE.MeshStandardMaterial({ color: new THREE.Color(color), transparent: true, opacity: idx === selectedIdx ? 0.95 : 0.5 })
      );
      bar.position.set(x, barHeight / 2, zSpread / 2 + 3.6);
      root.add(bar);
    });
    const railLabel = contextLabelSpriteCached('session replay timeline', CONTEXT_VIZ_THEME.railLabel, 1.0);
    railLabel.position.set(0, 5.6, zSpread / 2 + 3.6);
    root.add(railLabel);
  }

  contextViz.sceneKey = sceneKey;
  contextUpdateMeshSelection();
  contextRenderThree();
}

function contextUpdateMeshSelection() {
  for (const mesh of contextViz.meshes) {
    const selected = mesh.userData.partId === contextSelectedPartId;
    mesh.scale.setScalar(selected ? 1.14 : 1);
    if (mesh.material) {
      mesh.material.emissiveIntensity = selected ? 0.45 : 0.06;
      mesh.material.opacity = selected ? 1 : 0.92;
    }
  }
}

function contextPickThreePart(ev) {
  if (!contextViz.raycaster || !contextViz.camera || !contextViz.canvas) return;
  const rect = contextViz.canvas.getBoundingClientRect();
  contextViz.pointer.x = ((ev.clientX - rect.left) / Math.max(1, rect.width)) * 2 - 1;
  contextViz.pointer.y = -(((ev.clientY - rect.top) / Math.max(1, rect.height)) * 2 - 1);
  contextViz.raycaster.setFromCamera(contextViz.pointer, contextViz.camera);
  const hits = contextViz.raycaster.intersectObjects(contextViz.meshes, false);
  if (hits.length && hits[0].object.userData.partId) {
    focusContextPart(hits[0].object.userData.partId);
  }
}

function contextRenderThree(time = 0) {
  if (!contextViz.renderer || !contextViz.scene || !contextViz.camera) return;
  contextSetThreeSize();
  const radius = contextClampZoom(contextViz.zoom);
  const pitch = Math.max(-1.22, Math.min(1.22, contextViz.rotationX));
  const yaw = contextViz.rotationY;
  const cosPitch = Math.cos(pitch);
  const targetX = contextViz.targetX;
  const targetY = contextViz.targetY;
  const targetZ = contextViz.targetZ;
  contextViz.camera.position.set(
    targetX + Math.sin(yaw) * cosPitch * radius,
    targetY + Math.sin(pitch) * radius,
    targetZ + Math.cos(yaw) * cosPitch * radius
  );
  contextViz.camera.lookAt(targetX, targetY, targetZ);
  contextViz.camera.updateMatrixWorld();
  if (contextViz.root) {
    contextViz.root.rotation.set(0, 0, 0);
  }
  if (contextViz.meshes.length) {
    const t = time * 0.001;
    for (const mesh of contextViz.meshes) {
      // On-demand renders pass time=0: settle every mesh at its base so a
      // retired bob loop can't strand the previously-selected mesh mid-arc.
      const selected = time && mesh.userData.partId === contextSelectedPartId;
      if (selected) {
        mesh.position.y = mesh.userData.baseY + Math.sin(t * 3.2) * 0.08;
      } else {
        mesh.position.y = mesh.userData.baseY;
      }
    }
  }
  contextViz.renderer.render(contextViz.scene, contextViz.camera);
}

// The only per-frame animation is the selected-part bob — with nothing
// selected the scene is static and every interaction (drag, zoom, snapshot,
// slider) already renders on demand, so the 60 fps loop only runs while a
// part is selected and stops itself on deselect/tab-away (with one settling
// render so the bob doesn't freeze mid-arc).
function contextStartThreeAnimation() {
  if (!contextSelectedPartId) {
    contextStopThreeAnimation();
    contextRenderThree();
    return;
  }
  if (contextViz.raf) return;
  const frame = time => {
    contextViz.raf = null;
    if (activeTab === 'activity' && activeActivitySubtab === 'context' && contextSelectedPartId) {
      contextRenderThree(time);
      contextViz.raf = requestAnimationFrame(frame);
    } else {
      contextRenderThree();
    }
  };
  contextViz.raf = requestAnimationFrame(frame);
}

function contextStopThreeAnimation() {
  if (contextViz.raf) cancelAnimationFrame(contextViz.raf);
  contextViz.raf = null;
}

function handleContextSnapshot(snapshot) {
  storeContextSnapshot(snapshot);
  if (!contextPaneVisible()) return;
  scheduleContextPaneRender();
}

function contextPaneVisible() {
  return activeTab === 'activity' && activeActivitySubtab === 'context';
}

function scheduleContextPaneRender() {
  if (contextRenderScheduled) return;
  contextRenderScheduled = true;
  requestAnimationFrame(() => {
    contextRenderScheduled = false;
    if (!contextPaneVisible()) return;
    renderContextPane();
  });
}

function contextSnapshotForForegroundSession() {
  const { key, timeline } = contextTimelineForForegroundSession();
  if (contextReplayMode === 'replay' && timeline.length) {
    const requested = contextReplayIndexBySession.get(key);
    const idx = Math.max(0, Math.min(timeline.length - 1, Number.isFinite(requested) ? requested : timeline.length - 1));
    contextReplayIndexBySession.set(key, idx);
    return timeline[idx];
  }
  const sid = contextTargetSessionId(resolvePromptTargetSessionId());
  if (sid && contextSnapshotsBySession.has(sid)) return contextSnapshotsBySession.get(sid);
  if (sid && contextSnapshotsBySession.size === 1) return contextSnapshotsBySession.values().next().value;
  if (sid && contextSnapshotsBySession.size > 0) return null;
  return latestContextSnapshot;
}

function contextRawRenderKey(snapshot) {
  if (!snapshot) return 'empty';
  return snapshot.__context_key || contextSnapshotFingerprint(snapshot);
}

function renderContextRaw(snapshot) {
  const rawEl = document.getElementById('context-raw');
  if (!rawEl) return;
  if (!snapshot) {
    if (contextRawRenderedKey !== 'empty') rawEl.textContent = 'No context snapshot yet';
    contextRawRenderedKey = 'empty';
    return;
  }
  if (!contextRawOpen) {
    if (contextRawRenderedKey !== 'collapsed') {
      rawEl.textContent = 'Raw context collapsed. Press Raw to render the full snapshot.';
    }
    contextRawRenderedKey = 'collapsed';
    return;
  }
  if (contextSnapshotNeedsExact(snapshot)) {
    ensureExactContextSnapshot(snapshot);
    const loadingKey = `loading:${contextRawRenderKey(snapshot)}`;
    if (contextSnapshotExactFetchFailed(snapshot)) {
      const failedKey = `failed:${contextRawRenderKey(snapshot)}`;
      if (contextRawRenderedKey !== failedKey) {
        rawEl.textContent = 'Exact context snapshot failed to load. Showing compact replay payload.\n\n' + contextFullText(contextRawValue(snapshot));
      }
      contextRawRenderedKey = failedKey;
      return;
    }
    if (contextRawRenderedKey !== loadingKey) {
      rawEl.textContent = 'Loading exact context snapshot...';
    }
    contextRawRenderedKey = loadingKey;
    return;
  }
  const renderKey = `raw:${contextRawRenderKey(snapshot)}`;
  if (contextRawRenderedKey === renderKey) return;
  rawEl.textContent = contextFullText(contextRawValue(snapshot));
  contextRawRenderedKey = renderKey;
}

function renderContextPane() {
  const rawEl = document.getElementById('context-raw');
  if (!rawEl) return;
  wireContextPaneListeners();
  updateContextFocusControls();
  const set = (id, value) => {
    const el = document.getElementById(id);
    if (el) el.textContent = contextValueText(value);
  };
  const snapshot = contextSnapshotForForegroundSession();
  updateContextReplayControls(snapshot);
  if (!snapshot) {
    set('context-source', '--');
    set('context-turn', '--');
    set('context-items', '--');
    set('context-tokens', '--');
    set('context-format', '--');
    renderContextRaw(null);
    contextLastAnalysis = null;
    renderContextLegend(null);
    renderContextDetail(null, null);
    renderContextHotspots(null);
    contextBuildThree(null, null);
    return;
  }
  const d = snapshot;
  ensureExactContextSnapshot(snapshot);
  set('context-source', d.label || d.source || 'model');
  set('context-turn', contextSnapshotRequestLabel(d).replace(/^request /, '#'));
  set('context-items', d.item_count);
  const effectiveWindow = d.effective_context_window || d.context_window;
  const hardWindow = d.hard_context_window || d.hardContextWindow || null;
  const tokenKind = d.token_count_kind || '';
  const tokenPrefix = tokenKind === 'local_estimate' ? 'local estimate ' : '';
  const tokens = d.token_count && effectiveWindow
    ? `${tokenPrefix}${Number(d.token_count).toLocaleString()} / ${Number(effectiveWindow).toLocaleString()} effective${hardWindow && hardWindow !== effectiveWindow ? ` (${Number(hardWindow).toLocaleString()} hard)` : ''}`
    : d.token_count;
  set('context-tokens', tokens);
  set('context-format', d.format || '--');
  renderContextRaw(snapshot);
  const analysis = analyzeContextSnapshotCached(snapshot);
  contextLastAnalysis = analysis;
  const selected = selectedContextPart(analysis);
  renderContextLegend(analysis);
  renderContextDetail(analysis, selected);
  renderContextHotspots(analysis);
  contextBuildThree(analysis, snapshot);
  if (activeTab === 'activity' && activeActivitySubtab === 'context') contextStartThreeAnimation();
}


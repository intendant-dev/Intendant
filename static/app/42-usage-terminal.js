// ── Usage Tab Rendering ──
// ── Token Ticker ──
// Thin alias over the canonical compact-number formatter (declared in
// 53-stats-settings.js; function declarations hoist module-wide, so the
// later fragment is safely callable from these event-time paths).
function fmtK(n) {
  return formatCompactNumber(n);
}

function flashEl(el) {
  el.classList.remove('tk-flash');
  void el.offsetWidth; // force reflow
  el.classList.add('tk-flash');
}

function updateTickerFromUsage(c) {
  if (!c.live_json) return;
  let d;
  try { d = typeof c.live_json === 'string' ? JSON.parse(c.live_json) : c.live_json; } catch { return; }
  if (!d || !d.total_tokens) return;

  const total = d.total_tokens || 0;
  const input = d.input_tokens || d.prompt_tokens || 0;
  const output = d.output_tokens || d.completion_tokens || 0;
  const cached = d.cached_tokens || 0;
  const thinking = d.thinking_tokens || 0;
  const cacheHit = input > 0 ? Math.round(cached / input * 100) : 0;

  let liveCost = null;
  if (c.cost_json) {
    try {
      const costSummary = typeof c.cost_json === 'string' ? JSON.parse(c.cost_json) : c.cost_json;
      const liveLine = Array.isArray(costSummary?.lines)
        ? costSummary.lines.find(line => line.label === 'Live Model')
        : null;
      if (liveLine && Number.isFinite(Number(liveLine.cost))) liveCost = Number(liveLine.cost);
    } catch {}
  }

  // Collapsed view
  const tkTokens = document.getElementById('tk-tokens');
  const tkCached = document.getElementById('tk-cached');
  const tkCost = document.getElementById('tk-cost');
  tkTokens.textContent = fmtK(total) + ' tok';
  tkCached.textContent = fmtK(cached) + ' cached (' + cacheHit + '%)';
  tkCost.textContent = liveCost == null ? '$--' : formatUsd(liveCost, 4);
  flashEl(tkTokens);

  // Expanded view
  document.getElementById('tk-detail-1').textContent =
    'Live: ' + total.toLocaleString() + ' tokens (in: ' + input.toLocaleString() + ' | out: ' + output.toLocaleString() + ' | think: ' + thinking.toLocaleString() + ')';
  document.getElementById('tk-detail-2').textContent =
    'Cache: ' + cached.toLocaleString() + ' / ' + input.toLocaleString() + ' input = ' + cacheHit + '% hit | Frames: ' + tickerFramesSent + ' sent, ' + tickerFramesDropped + ' dropped | Cost: ' + (liveCost == null ? '$--' : formatUsd(liveCost, 4));
}

function updateTickerFrames() {
  const el = document.getElementById('tk-frames');
  el.textContent = tickerFramesSent + ' frames' + (tickerFramesDropped > 0 ? ' (' + tickerFramesDropped + ' dropped)' : '');
  flashEl(el);
}

window.toggleTicker = function() {
  tickerExpanded = !tickerExpanded;
  document.getElementById('ticker-chevron').innerHTML = tickerExpanded ? '&#x25BE;' : '&#x25B8;';
  document.getElementById('ticker-collapsed').classList.toggle('hidden', tickerExpanded);
  document.getElementById('ticker-expanded').classList.toggle('hidden', !tickerExpanded);
};

function renderUsageTab(c) {
  // update_usage fires on every token tick; rebuilding the Stats region
  // while the pane is hidden is wasted work. Defer to the next pane entry.
  if (!paneIsVisible('stats')) {
    renderOrDefer('stats', 'usage', () => renderUsageTab(c));
    return;
  }
  const cardsEl = document.getElementById('usage-cards');
  const emptyEl = document.getElementById('usage-empty');
  const costEl = document.getElementById('cost-section');
  const historyEl = document.getElementById('token-history');

  if (!c.main_json) {
    emptyEl.style.display = 'grid';
    cardsEl.style.display = 'none';
    costEl.style.display = 'none';
    historyEl.style.display = 'none';
    return;
  }

  emptyEl.style.display = 'none';
  cardsEl.style.display = 'flex';
  cardsEl.innerHTML = '';

  const mainData = JSON.parse(c.main_json);
  cardsEl.appendChild(renderUsageCard('Main Model', mainData));

  if (c.presence_json) {
    const presenceData = JSON.parse(c.presence_json);
    cardsEl.appendChild(renderUsageCard('Presence Model', presenceData));
  }

  // Live model usage card (from Gemini Live / OpenAI Realtime via WASM AppState)
  if (c.live_json) {
    const ld = JSON.parse(c.live_json);
    if (ld.total_tokens > 0) {
      const liveCard = document.createElement('div');
      liveCard.className = 'usage-card';
      liveCard.id = 'live-usage-card';
      const providerLabel = ld.provider ? ld.provider.charAt(0).toUpperCase() + ld.provider.slice(1) : 'Live';
      const modelLabel = ld.model || 'unknown';
      liveCard.innerHTML = `
        <div class="card-title">Live Model</div>
        <div class="card-model">
          <span class="provider">${providerLabel}</span>
          <span class="model">${modelLabel}</span>
        </div>
        <div class="token-breakdown">
          <span class="label">Input tokens</span>
          <span class="value">${ld.input_tokens.toLocaleString()}</span>
          <span class="label sub">Cached</span>
          <span class="value">${ld.cached_tokens.toLocaleString()}</span>
          <span class="label">Output tokens</span>
          <span class="value">${ld.output_tokens.toLocaleString()}</span>
          ${ld.thinking_tokens > 0 ? `<span class="label">Thinking tokens</span><span class="value">${ld.thinking_tokens.toLocaleString()}</span>` : ''}
          <span class="label total-row">Total</span>
          <span class="value total-row">${ld.total_tokens.toLocaleString()}</span>
        </div>
      `;
      cardsEl.appendChild(liveCard);
    }
  }

  // Cost — build the whole grid, then assign once (update_usage fires on
  // every token tick; incremental innerHTML += reparses the grid N times).
  if (c.cost_json) {
    const cost = JSON.parse(c.cost_json);
    costEl.style.display = 'block';
    const grid = document.getElementById('cost-grid');
    const cells = [];
    for (const cl of cost.lines) {
      cells.push(`<span class="label">${cl.label}</span><span class="value">${formatUsd(cl.cost)}</span>`);
      cells.push(`<span class="label sub">Input</span><span class="value sub">${formatUsd(cl.input_cost)}</span>`);
      cells.push(`<span class="label sub">Output</span><span class="value sub">${formatUsd(cl.output_cost)}</span>`);
    }
    if (cost.lines.length > 1) {
      cells.push(`<span class="label strong">Total</span><span class="value">${formatUsd(cost.total)}</span>`);
    }
    grid.innerHTML = cells.join('');
  } else {
    costEl.style.display = 'none';
  }

  // History
  if (c.history_json) {
    const history = JSON.parse(c.history_json);
    historyEl.style.display = 'block';
    const chart = document.getElementById('history-chart');
    const maxTokens = Math.max(...history.map(h => h.tokens), 1);
    const frag = document.createDocumentFragment();
    for (const h of history) {
      const bar = document.createElement('div');
      bar.className = 'history-bar';
      bar.style.height = Math.max((h.tokens / maxTokens) * 100, 2) + '%';
      bar.title = `Turn ${h.turn}: ${h.tokens.toLocaleString()} tokens`;
      frag.appendChild(bar);
    }
    chart.replaceChildren(frag);
  } else {
    historyEl.style.display = 'none';
  }
}

function renderUsageCard(label, data) {
  const card = document.createElement('div');
  card.className = 'usage-card';
  const pctForWindow = (tokens, windowSize) => windowSize > 0 ? (tokens / windowSize) * 100 : null;
  const prompt = data.prompt_tokens || 0;
  const completion = data.completion_tokens || 0;
  const cached = data.cached_tokens || 0;
  const cacheCreated = data.cache_creation_tokens || 0;
  const uncachedInput = Math.max(0, prompt - cached - cacheCreated);
  const effectiveWindow = data.context_window || 0;
  const hardWindow = data.hard_context_window || data.hardContextWindow || null;
  const effectivePct = pctForWindow(data.tokens_used, effectiveWindow);
  const hardPct = pctForWindow(data.tokens_used, hardWindow);
  const displayPct = Number.isFinite(effectivePct) ? effectivePct : data.usage_pct;
  const pressurePct = Number.isFinite(displayPct) ? displayPct : 0;
  // Same thresholds as before: <50 calm, 50-85 warn, >=85 danger.
  const meterTone = pressurePct >= 85 ? ' danger' : pressurePct >= 50 ? ' warn' : '';
  const remainingEffective = Math.max(0, effectiveWindow - data.tokens_used);
  const remainingHard = hardWindow ? Math.max(0, hardWindow - data.tokens_used) : null;
  const effectivePctText = Number.isFinite(displayPct) ? `${displayPct.toFixed(1)}% effective` : '--';
  const hardLine = hardWindow && hardWindow !== effectiveWindow
    ? `<span class="label">Hard limit</span><span class="value">${hardWindow.toLocaleString()}</span>
      <span class="label sub">Hard usage</span><span class="value">${hardPct === null ? '--' : hardPct.toFixed(1) + '%'}</span>
      <span class="label">Hard remaining</span><span class="value">${remainingHard.toLocaleString()}</span>`
    : '';
  card.innerHTML = `
    <div class="card-title">${label}</div>
    <div class="card-model"><span class="provider">${data.provider}</span> / <span class="model-name">${data.model}</span></div>
    <div class="token-bar">
      <div class="bar-label">
        <span class="tokens">${data.tokens_used.toLocaleString()} / ${effectiveWindow.toLocaleString()} effective</span>
        <span class="pct">${effectivePctText}</span>
      </div>
      <div class="ui-meter${meterTone}"><i></i></div>
    </div>
    <div class="token-breakdown">
      <span class="label">Input tokens</span><span class="value">${prompt.toLocaleString()}</span>
      <span class="label sub">Cached</span>
      <span class="value">${cached.toLocaleString()}${prompt > 0 ? ' (' + (cached / prompt * 100).toFixed(0) + '%)' : ''}</span>
      ${cacheCreated > 0 ? `<span class="label sub">Cache writes</span><span class="value">${cacheCreated.toLocaleString()}</span>` : ''}
      <span class="label sub">Uncached</span><span class="value">${uncachedInput.toLocaleString()}</span>
      <span class="label">Output tokens</span><span class="value">${completion.toLocaleString()}</span>
      ${hardLine}
      <span class="label total-row">Effective remaining</span>
      <span class="value total-row">${remainingEffective.toLocaleString()}</span>
    </div>`;
  card.querySelector('.ui-meter > i').style.width = `${Math.min(pressurePct, 100)}%`;
  return card;
}

// ── Terminal Tab (lazy xterm.js) ──
function base64ToBytes(base64data) {
  const binary = atob(base64data);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function currentShellHostId() {
  return String(selectedShellHostId || SHELL_HOST_ID).trim() || SHELL_HOST_ID;
}

function shellFrameMatchesCurrent(hostId, terminalId) {
  return String(terminalId || '') === SHELL_TERMINAL_ID &&
    String(hostId || SHELL_HOST_ID) === currentShellHostId();
}

function shellHostLabel(hostId = currentShellHostId()) {
  const id = String(hostId || SHELL_HOST_ID).trim() || SHELL_HOST_ID;
  if (id === SHELL_HOST_ID || id === selfPeerId) return 'This daemon';
  const peer = daemons.find(d => d.host_id === id);
  return peer?.label || id;
}

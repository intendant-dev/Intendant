// ── Sessions: worktrees panel + session detail/replay ──
// The worktrees management panel (scan/filter/inspect/remove), session
// resume/delete actions, and the session-detail overlay: title + lineage,
// recordings and frame assets, and the windowed detail log/replay view.
// The Recent-list rendering lives in 57a-sessions-list.js.

function _refilterWorktrees() {
  // Filter/search/sort change — snap the Show-more window back.
  worktreesRenderWindow = WORKTREE_CARD_RENDER_LIMIT;
  if (worktreesLoaded && _cachedWorktreeScan) {
    renderWorktrees(_cachedWorktreeScan);
  }
}

function worktreeHasScannedData(scan) {
  return !!(scan && scan.scanned_at);
}

function setWorktreesStatus(message, kind = '') {
  const el = document.getElementById('worktrees-status');
  if (!el) return;
  el.classList.toggle('error', kind === 'error');
  el.textContent = message || '';
}

function setWorktreesActivityNotice(kind, text, autoHideMs = 0) {
  const notice = document.getElementById('worktrees-activity-notice');
  const textEl = document.getElementById('worktrees-activity-text');
  if (!notice || !textEl) return;
  if (worktreesActivityClearTimeout) {
    clearTimeout(worktreesActivityClearTimeout);
    worktreesActivityClearTimeout = null;
  }
  const hasText = !!String(text || '').trim();
  const noticeKind = ['ok', 'warn', 'error'].includes(kind) ? kind : 'pending';
  notice.className = `sessions-spawn-notice ${noticeKind}` + (hasText ? '' : ' hidden');
  textEl.textContent = text || '';
  notice.title = text || '';
  if (hasText && autoHideMs > 0) {
    worktreesActivityClearTimeout = setTimeout(() => {
      setWorktreesActivityNotice('', '');
      worktreesActivityClearTimeout = null;
    }, autoHideMs);
  }
}

function setWorktreesLoadPending(pending, mode = '') {
  const listEl = document.getElementById('worktrees-list');
  const scanBtn = document.getElementById('worktrees-scan-btn');
  const cachedBtn = document.getElementById('worktrees-refresh-btn');
  if (listEl) listEl.classList.toggle('is-busy', !!pending);
  if (scanBtn) {
    scanBtn.disabled = !!pending;
    scanBtn.classList.toggle('pending', !!pending && mode === 'scan');
    scanBtn.textContent = pending && mode === 'scan' ? 'Scanning...' : 'Scan';
  }
  if (cachedBtn) {
    cachedBtn.disabled = !!pending;
    cachedBtn.classList.toggle('pending', !!pending && mode === 'cache');
    cachedBtn.textContent = pending && mode === 'cache' ? 'Loading...' : 'Cached';
  }
}

function loadWorktrees(options = {}) {
  const forceScan = !!options.forceScan;
  const listEl = document.getElementById('worktrees-list');
  if (!listEl) return Promise.resolve(null);
  if (worktreesLoadInFlight === 'scan') {
    setWorktreesLoadPending(true, 'scan');
    // The activity banner is the single scan-progress surface; the
    // #worktrees-status text line only reports scan results/errors.
    setWorktreesActivityNotice('pending', 'Scanning worktrees...');
    return worktreesLoadPromise || Promise.resolve(_cachedWorktreeScan);
  }
  if (worktreesLoadInFlight === 'cache' && !forceScan) {
    setWorktreesLoadPending(true, 'cache');
    return worktreesLoadPromise || Promise.resolve(_cachedWorktreeScan);
  }
  const browserCachedScan = worktreeHasScannedData(_cachedWorktreeScan) ? _cachedWorktreeScan : null;
  const requestSerial = ++worktreesRequestSerial;
  worktreesLoadInFlight = forceScan ? 'scan' : 'cache';
  setWorktreesLoadPending(true, worktreesLoadInFlight);
  if (forceScan) {
    if (!browserCachedScan) {
      listEl.innerHTML = '';
    }
    setWorktreesStatus('');
    setWorktreesActivityNotice('pending', 'Scanning worktrees...');
  } else {
    if (browserCachedScan) {
      renderWorktrees(browserCachedScan);
      setWorktreesStatus('Showing the last scan while checking the cached inventory...');
    } else {
      setWorktreesStatus('Loading cached worktree scan...');
    }
  }
  const url = forceScan ? '/api/worktrees/scan' : '/api/worktrees';
  const rpcMethod = forceScan ? 'api_worktrees_scan' : 'api_worktrees';
  // daemonApi (transport F2): the cached read is a GET twin (tunnel first,
  // HTTP fallback allowed); the forced scan is a POST twin, so the facade
  // never replays it over HTTP after an attempted tunnel send.
  const promise = daemonApi.request(rpcMethod, {})
    .then(resp => resp.ok ? resp.body : Promise.reject(new Error(`${url} returned ${resp.status}`)))
    .then(scan => {
      if (requestSerial !== worktreesRequestSerial) return;
      worktreesLoaded = true;
      if (!forceScan && !worktreeHasScannedData(scan) && browserCachedScan) {
        renderWorktrees(browserCachedScan);
        setWorktreesStatus('Showing the last browser scan; no server-cached scan is available. Click Scan to refresh from disk.');
        return;
      }
      _cachedWorktreeScan = scan;
      renderWorktrees(scan);
      if (forceScan && worktreeHasScannedData(scan)) {
        const count = Number(scan?.summary?.worktrees || scan?.worktrees?.length || 0).toLocaleString();
        setWorktreesActivityNotice('ok', `Scan complete. Found ${count} worktree${count === '1' ? '' : 's'}.`, 2500);
      }
      return scan;
    })
    .catch(err => {
      if (requestSerial !== worktreesRequestSerial) return;
      const message = err.message || 'Failed to load worktrees';
      if (browserCachedScan) {
        renderWorktrees(browserCachedScan);
        setWorktreesStatus(`${message}. Showing the last browser scan.`, 'error');
      } else {
        setWorktreesStatus(message, 'error');
        listEl.innerHTML = '<div class="empty-state">Failed to load worktrees</div>';
      }
      setWorktreesActivityNotice('error', message);
    })
    .finally(() => {
      if (requestSerial !== worktreesRequestSerial) return;
      worktreesLoadInFlight = '';
      worktreesLoadPromise = null;
      setWorktreesLoadPending(false);
    });
  worktreesLoadPromise = promise;
  return promise;
}

function renderWorktrees(scan) {
  renderWorktreesAggregate(scan, document.getElementById('worktrees-aggregate'));
  renderWorktreesList(scan, document.getElementById('worktrees-list'));
  stationScheduleUpdate();
  const scannedAt = scan && scan.scanned_at;
  if (!scannedAt) {
    setWorktreesStatus('No worktree scan yet.');
    return;
  }
  const scanned = worktreeDate(scannedAt);
  const when = scanned ? scanned.toLocaleString() : scannedAt;
  const errors = Array.isArray(scan.errors) && scan.errors.length > 0
    ? ` ${scan.errors.length} scan warning${scan.errors.length === 1 ? '' : 's'}.`
    : '';
  setWorktreesStatus(`Last scanned ${when}.${errors}`);
}

function renderWorktreesAggregate(scan, el) {
  if (!el) return;
  el.innerHTML = '';
  const summary = scan?.summary || {};
  const cards = [
    { label: 'Worktrees', value: Number(summary.worktrees || 0).toLocaleString() },
    { label: 'Disk', value: _fmtBytes(summary.total_bytes || 0) },
    { label: 'Dirty', value: Number(summary.dirty || 0).toLocaleString() },
    { label: 'Unmerged', value: Number(summary.unmerged || 0).toLocaleString() },
    { label: 'Active', value: Number(summary.active || 0).toLocaleString() },
    { label: 'Candidates', value: Number(summary.cleanup_candidates || 0).toLocaleString(), sub: summary.cleanup_candidates > 0 ? 'safe to remove' : '' },
  ];
  renderAggregateStatTiles(el, cards);
}

function worktreeDate(value) {
  if (!value) return null;
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? null : new Date(parsed);
}

function worktreeDateSortValue(wt, field) {
  const d = worktreeDate(wt?.[field]);
  return d ? d.getTime() : 0;
}

function fmtWorktreeMinute(value) {
  const d = worktreeDate(value);
  if (!d) return '--';
  return new Intl.DateTimeFormat(undefined, {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
  }).format(d);
}

function worktreeSearchText(wt) {
  return [
    wt.path,
    wt.repo_root,
    wt.repo_name,
    wt.branch,
    wt.branch_ref,
    wt.default_branch,
    wt.upstream,
    wt.merge_status,
    wt.recommended_action,
    wt.safety,
    ...(wt.labels || []),
    ...(wt.related_sessions || []).map(s => `${s.session_id} ${s.source} ${s.status}`),
  ].filter(Boolean).join(' ').toLowerCase();
}

function worktreeMatchesSearch(wt, query) {
  if (!query) return true;
  const haystack = worktreeSearchText(wt);
  return query.split(/\s+/).every(term => haystack.includes(term));
}

function worktreeRiskRank(wt) {
  if (wt.conflicted > 0) return 90;
  if (wt.active_sessions > 0) return 80;
  if (wt.dirty) return 70;
  if (wt.merge_status === 'unmerged') return 60;
  if (wt.locked) return 50;
  if (wt.size_truncated) return 40;
  if (wt.safe_to_remove) return 25;
  if (wt.merge_status === 'unknown') return 20;
  return 10;
}

function shellQuote(value) {
  return "'" + String(value || '').replace(/'/g, "'\\''") + "'";
}

function worktreeRemoveCommand(wt) {
  return `git -C ${shellQuote(wt.repo_root)} worktree remove ${shellQuote(wt.path)}`;
}

let worktreeInspectContext = null;

function setWorktreeInspectStatus(message, kind = '') {
  const el = document.getElementById('worktree-inspect-status');
  if (!el) return;
  el.className = 'session-config-status worktree-inspect-status' + (kind ? ` ${kind}` : '');
  el.textContent = message || '';
}

function closeWorktreeInspectModal() {
  const modal = document.getElementById('worktree-inspect-modal');
  if (modal) modal.style.display = 'none';
}

function worktreeInspectPayload(wt) {
  return {
    repo_root: wt?.repo_root || '',
    path: wt?.path || '',
    expected_head: wt?.head || null,
  };
}

async function openWorktreeInspect(wt) {
  const modal = document.getElementById('worktree-inspect-modal');
  if (!modal) return;
  worktreeInspectContext = { wt: { ...wt }, inspect: null };
  modal.style.display = 'flex';
  renderWorktreeInspectLoading(wt);
  try {
    const payload = worktreeInspectPayload(wt);
    // daemonApi (transport F2): a POST twin — the facade's no-replay
    // policy covers the fallbackAfterRpcFailure:false this call passed by
    // hand.
    const resp = await daemonApi.request('api_worktrees_inspect', payload);
    const inspect = (resp.body && typeof resp.body === 'object') ? resp.body : {};
    if (!resp.ok || inspect.ok === false) {
      throw new Error(inspect.error || `worktree inspect returned ${resp.status}`);
    }
    worktreeInspectContext = { wt: inspect.entry || wt, inspect };
    renderWorktreeInspect(inspect);
  } catch (err) {
    renderWorktreeInspectError(wt, err?.message || 'Worktree inspect failed');
  }
}

function renderWorktreeInspectLoading(wt) {
  document.getElementById('worktree-inspect-title').textContent = 'Inspect worktree';
  document.getElementById('worktree-inspect-path').textContent = wt?.path || '';
  document.getElementById('worktree-inspect-subtitle').textContent = 'Loading current Git state...';
  document.getElementById('worktree-inspect-metrics').innerHTML = '';
  document.getElementById('worktree-inspect-reasons').innerHTML = '<div class="empty-state">Loading reasons...</div>';
  document.getElementById('worktree-inspect-files').innerHTML = '<div class="empty-state">Loading dirty files...</div>';
  document.getElementById('worktree-inspect-commands').innerHTML = '';
  setWorktreeInspectStatus('', '');
}

function renderWorktreeInspectError(wt, message) {
  document.getElementById('worktree-inspect-title').textContent = 'Inspect worktree';
  document.getElementById('worktree-inspect-path').textContent = wt?.path || '';
  document.getElementById('worktree-inspect-subtitle').textContent = 'Inspect failed.';
  document.getElementById('worktree-inspect-reasons').innerHTML = '<div class="empty-state">No current details available</div>';
  document.getElementById('worktree-inspect-files').innerHTML = '<div class="empty-state">No file details available</div>';
  document.getElementById('worktree-inspect-commands').innerHTML = '';
  setWorktreeInspectStatus(message, 'error');
}

function appendWorktreeInspectMetric(parent, label, value) {
  const card = document.createElement('div');
  card.className = 'worktree-inspect-metric';
  const l = document.createElement('span');
  l.className = 'label';
  l.textContent = label;
  const v = document.createElement('span');
  v.className = 'value';
  v.textContent = value || '--';
  card.appendChild(l);
  card.appendChild(v);
  parent.appendChild(card);
}

function renderWorktreeInspect(inspect) {
  const wt = inspect?.entry || {};
  const title = document.getElementById('worktree-inspect-title');
  const path = document.getElementById('worktree-inspect-path');
  const subtitle = document.getElementById('worktree-inspect-subtitle');
  if (title) title.textContent = wt.branch || wt.head_short || 'Inspect worktree';
  if (path) path.textContent = wt.path || '';
  if (subtitle) {
    subtitle.textContent = [
      wt.repo_name || wt.repo_root || '',
      wt.safe_to_remove ? 'removal candidate' : (wt.recommended_action || 'review'),
      wt.safety || '',
    ].filter(Boolean).join(' · ');
  }

  const metrics = document.getElementById('worktree-inspect-metrics');
  if (metrics) {
    metrics.innerHTML = '';
    appendWorktreeInspectMetric(metrics, 'Status', wt.safe_to_remove ? 'candidate' : (wt.recommended_action || 'review'));
    appendWorktreeInspectMetric(metrics, 'Dirty', `${wt.staged || 0} staged / ${wt.unstaged || 0} unstaged / ${wt.untracked || 0} untracked`);
    appendWorktreeInspectMetric(metrics, 'Merge', wt.merge_status || '--');
    appendWorktreeInspectMetric(metrics, 'Sessions', `${wt.active_sessions || 0} active / ${wt.related_session_count || 0} related`);
    appendWorktreeInspectMetric(metrics, 'Size', _fmtBytes(wt.size_bytes || 0) + (wt.size_truncated ? ' +' : ''));
    appendWorktreeInspectMetric(metrics, 'Branch', wt.branch || (wt.detached ? 'detached' : '--'));
    appendWorktreeInspectMetric(metrics, 'HEAD', wt.head_short || '--');
    appendWorktreeInspectMetric(metrics, 'Changed', fmtWorktreeMinute(wt.last_changed_at));
  }

  renderWorktreeInspectReasons(inspect?.reasons || []);
  renderWorktreeInspectFiles(inspect?.status_files || [], inspect?.status_total || 0, !!inspect?.status_truncated);
  renderWorktreeInspectCommands(wt);
  setWorktreeInspectStatus('Current Git state loaded.', 'ok');
}

function renderWorktreeInspectReasons(reasons) {
  const el = document.getElementById('worktree-inspect-reasons');
  if (!el) return;
  el.innerHTML = '';
  if (!reasons.length) {
    el.innerHTML = '<div class="empty-state">No review blockers found</div>';
    return;
  }
  for (const reason of reasons) {
    const row = document.createElement('div');
    const severity = String(reason.severity || 'warning').replace(/[^a-z0-9_-]/gi, '-');
    row.className = `worktree-inspect-reason ${severity}`;
    const title = document.createElement('div');
    title.className = 'worktree-inspect-reason-title';
    title.textContent = reason.label || reason.code || 'Review';
    const detail = document.createElement('div');
    detail.className = 'worktree-inspect-reason-detail';
    detail.textContent = reason.detail || '';
    row.appendChild(title);
    row.appendChild(detail);
    el.appendChild(row);
  }
}

function worktreeInspectFileLabel(file) {
  const category = String(file?.category || '').replace(/\+/g, ' + ');
  const status = `${file?.index_status || ' '}${file?.worktree_status || ' '}`.trim();
  return [category || 'changed', status ? `(${status})` : ''].filter(Boolean).join(' ');
}

function renderWorktreeInspectFiles(files, total, truncated) {
  const el = document.getElementById('worktree-inspect-files');
  if (!el) return;
  el.innerHTML = '';
  if (!files.length) {
    el.innerHTML = '<div class="empty-state">No dirty files reported</div>';
    return;
  }
  const order = ['conflicted', 'staged+unstaged', 'staged', 'unstaged', 'untracked', 'clean'];
  const rows = files.slice().sort((a, b) => {
    const ai = order.indexOf(a.category);
    const bi = order.indexOf(b.category);
    return (ai < 0 ? order.length : ai) - (bi < 0 ? order.length : bi) ||
      String(a.path || '').localeCompare(String(b.path || ''));
  });
  for (const file of rows) {
    const row = document.createElement('div');
    row.className = 'worktree-inspect-file';
    const title = document.createElement('div');
    title.className = 'worktree-inspect-file-title';
    title.textContent = file.path || '';
    const detail = document.createElement('div');
    detail.className = 'worktree-inspect-file-detail';
    detail.textContent = file.original_path
      ? `${worktreeInspectFileLabel(file)} · renamed from ${file.original_path}`
      : worktreeInspectFileLabel(file);
    row.appendChild(title);
    row.appendChild(detail);
    el.appendChild(row);
  }
  if (truncated) {
    const more = document.createElement('div');
    more.className = 'empty-state';
    more.textContent = `Showing ${files.length.toLocaleString()} of ${Number(total || files.length).toLocaleString()} changed files. Open Shell for the full status.`;
    el.appendChild(more);
  }
}

function worktreeInspectCommands(wt) {
  const path = wt?.path || '';
  const repo = wt?.repo_root || path;
  const commands = [
    { title: 'Show status', command: `git -C ${shellQuote(path)} status --short --branch` },
    { title: 'Show unstaged diff stat', command: `git -C ${shellQuote(path)} diff --stat` },
    { title: 'Show staged diff stat', command: `git -C ${shellQuote(path)} diff --cached --stat` },
    { title: 'Preview untracked cleanup', command: `git -C ${shellQuote(path)} clean -nd` },
    { title: 'Stash tracked and untracked changes', command: `git -C ${shellQuote(path)} stash push -u -m ${shellQuote('intendant worktree cleanup')}` },
    { title: 'Remove after cleanup', command: `git -C ${shellQuote(repo)} worktree remove ${shellQuote(path)}` },
  ];
  return commands.filter(cmd => !cmd.command.includes("''"));
}

function renderWorktreeInspectCommands(wt) {
  const el = document.getElementById('worktree-inspect-commands');
  if (!el) return;
  el.innerHTML = '';
  for (const item of worktreeInspectCommands(wt)) {
    const row = document.createElement('div');
    row.className = 'worktree-inspect-command';
    const main = document.createElement('div');
    main.className = 'worktree-inspect-command-main';
    const title = document.createElement('div');
    title.className = 'worktree-inspect-command-title';
    title.textContent = item.title;
    const code = document.createElement('code');
    code.textContent = item.command;
    main.appendChild(title);
    main.appendChild(code);
    const copy = document.createElement('button');
    copy.type = 'button';
    copy.className = 'ui-btn sc-resume-btn';
    copy.textContent = 'Copy';
    copy.addEventListener('click', () => {
      copyTextToClipboard(item.command).then(
        () => setWorktreeInspectStatus('Command copied.', 'ok'),
        err => setWorktreeInspectStatus(err?.message || 'Copy failed', 'error')
      );
    });
    row.appendChild(main);
    row.appendChild(copy);
    el.appendChild(row);
  }
}

function worktreeInspectCurrentPath() {
  return worktreeInspectContext?.inspect?.entry?.path || worktreeInspectContext?.wt?.path || '';
}

function worktreeInspectOpenShell() {
  const path = worktreeInspectCurrentPath();
  if (!path) return;
  closeWorktreeInspectModal();
  routeTo('terminal', 'shell');
  setTimeout(() => {
    setShellHost(SHELL_HOST_ID);
    if (!shellInitialized) initShell();
    openShellSessionIfPossible();
    sendShellBytes(`cd ${shellQuote(path)}`);
    showControlToast('success', 'Shell opened with worktree path queued.');
  }, 0);
}

function worktreeInspectOpenFiles() {
  const path = worktreeInspectCurrentPath();
  if (!path) return;
  closeWorktreeInspectModal();
  routeTo('files');
  setTimeout(() => {
    setFilesDownloadPath(path);
    setFilesDownloadStatus('', 'Worktree path loaded. Use Browse to choose a file inside it.');
    document.getElementById('files-download-path')?.focus();
  }, 0);
}

window.closeWorktreeInspectModal = closeWorktreeInspectModal;
window.worktreeInspectOpenShell = worktreeInspectOpenShell;
window.worktreeInspectOpenFiles = worktreeInspectOpenFiles;

function formatWorktreeDivergence(target, ahead, behind) {
  const label = target ? `${target} ` : '';
  return `${label}+${Number(ahead || 0).toLocaleString()} / -${Number(behind || 0).toLocaleString()}`;
}

function recomputeWorktreeSummary(scan) {
  const rows = Array.isArray(scan?.worktrees) ? scan.worktrees : [];
  const summary = {
    worktrees: rows.length,
    repos: new Set(rows.map(wt => wt.repo_root).filter(Boolean)).size,
    total_bytes: 0,
    dirty: 0,
    unmerged: 0,
    active: 0,
    stale: 0,
    cleanup_candidates: 0,
    truncated_sizes: 0,
  };
  for (const wt of rows) {
    summary.total_bytes += Number(wt.size_bytes || 0);
    if (wt.dirty) summary.dirty += 1;
    if (wt.merge_status === 'unmerged') summary.unmerged += 1;
    if ((wt.active_sessions || 0) > 0) summary.active += 1;
    if ((wt.labels || []).includes('stale')) summary.stale += 1;
    if (wt.safe_to_remove) summary.cleanup_candidates += 1;
    if (wt.size_truncated) summary.truncated_sizes += 1;
  }
  scan.summary = summary;
}

function worktreeRemovalState(pathKey) {
  const key = String(pathKey || '');
  if (!key) return '';
  if (activeWorktreeRemovalPath === key) return 'removing';
  return pendingWorktreeRemovals.has(key) ? 'queued' : '';
}

function setWorktreeRemoveButtonPending(button, state) {
  if (!button) return;
  const mode = state === true ? 'removing' : (state || '');
  const pending = mode === 'queued' || mode === 'removing';
  button.disabled = pending;
  button.classList.toggle('pending', pending);
  button.textContent = mode === 'queued'
    ? 'Queued...'
    : mode === 'removing'
    ? 'Removing...'
    : 'Remove worktree...';
}

async function removeWorktree(wt, button) {
  const branch = wt.branch || wt.head_short || '(unknown ref)';
  const ok = await showDashboardConfirm({
    title: 'Remove worktree',
    message:
      `Remove worktree ${branch}?\n\n` +
      `Path: ${wt.path}\n` +
      `Repo: ${wt.repo_root}`,
    warning:
      `${wt.safety || 'This worktree was marked as safe to remove.'}\n\n` +
      'Before removing, Intendant will re-check that this is not the main worktree, has no active sessions, is not locked, is clean, is merged or prunable, and still points at the same HEAD.',
    confirmLabel: 'Remove',
  });
  if (!ok) return;

  const pathKey = String(wt.path || '');
  if (!pathKey) {
    setWorktreesStatus('Worktree removal failed: missing path.', 'error');
    setWorktreesActivityNotice('error', 'Worktree removal failed: missing path.');
    return;
  }
  if (pendingWorktreeRemovals.has(pathKey)) {
    const state = worktreeRemovalState(pathKey);
    setWorktreesStatus(`${state === 'removing' ? 'Already removing' : 'Already queued'} ${wt.path}...`);
    return;
  }

  pendingWorktreeRemovals.add(pathKey);
  worktreeRemovalQueue.push({
    pathKey,
    wt: { ...wt },
  });
  setWorktreeRemoveButtonPending(button, activeWorktreeRemovalPath ? 'queued' : 'removing');
  button?.closest('.worktree-card')?.classList.add(activeWorktreeRemovalPath ? 'queued' : 'removing');
  const waiting = worktreeRemovalQueue.length;
  if (_cachedWorktreeScan) renderWorktrees(_cachedWorktreeScan);
  if (activeWorktreeRemovalPath) {
    setWorktreesStatus(`Queued ${wt.path} for removal. ${waiting} waiting.`);
    setWorktreesActivityNotice('pending', `Queued ${wt.path} for removal. ${waiting} waiting.`);
  }
  drainWorktreeRemovalQueue();
}

async function drainWorktreeRemovalQueue() {
  if (activeWorktreeRemovalPath) return;
  while (worktreeRemovalQueue.length > 0) {
    const job = worktreeRemovalQueue.shift();
    const wt = job?.wt || {};
    const pathKey = job?.pathKey || String(wt.path || '');
    if (!pathKey || !pendingWorktreeRemovals.has(pathKey)) continue;

    activeWorktreeRemovalPath = pathKey;
    if (_cachedWorktreeScan) renderWorktrees(_cachedWorktreeScan);
    setWorktreesStatus(`Removing ${wt.path}...`);
    setWorktreesActivityNotice('pending', `Removing ${wt.path}...${worktreeRemovalQueue.length ? ` ${worktreeRemovalQueue.length} queued.` : ''}`);

    let finalStatus = '';
    let finalStatusKind = '';
    let finalNoticeKind = 'ok';
    let finalNoticeAutoHideMs = 0;
    try {
      const payload = {
        repo_root: wt.repo_root,
        path: wt.path,
        expected_head: wt.head || null,
      };
      // daemonApi (transport F2): a POST twin — the facade's no-replay
      // policy covers the fallbackAfterRpcFailure:false this call passed
      // by hand.
      const r = await daemonApi.request('api_worktrees_remove', payload);
      const result = (r.body && typeof r.body === 'object') ? r.body : {};
      if (!r.ok || result.ok === false) {
        throw new Error(result.error || `worktree removal returned ${r.status}`);
      }

      const freed = Number(result.size_bytes || wt.size_bytes || 0);
      if (_cachedWorktreeScan && Array.isArray(_cachedWorktreeScan.worktrees)) {
        _cachedWorktreeScan.worktrees = _cachedWorktreeScan.worktrees
          .filter(entry => entry.path !== wt.path);
        recomputeWorktreeSummary(_cachedWorktreeScan);
      }
      const remaining = worktreeRemovalQueue.length;
      finalStatus = `Removed ${wt.path}. Freed about ${_fmtBytes(freed)}.${remaining ? ` ${remaining} removal${remaining === 1 ? '' : 's'} still queued.` : ' Click Scan to refresh from disk.'}`;
      finalNoticeKind = remaining ? 'pending' : 'ok';
      finalNoticeAutoHideMs = remaining ? 0 : 3500;
      showControlToast('success', 'Worktree removed.');
    } catch (err) {
      const remaining = worktreeRemovalQueue.length;
      const message = err.message || 'Worktree removal failed';
      finalStatus = remaining ? `${message}. Continuing with ${remaining} queued.` : message;
      finalStatusKind = 'error';
      finalNoticeKind = 'error';
      showControlToast('error', message);
    } finally {
      pendingWorktreeRemovals.delete(pathKey);
      activeWorktreeRemovalPath = '';
      if (_cachedWorktreeScan) renderWorktrees(_cachedWorktreeScan);
      if (finalStatus) {
        setWorktreesStatus(finalStatus, finalStatusKind);
        setWorktreesActivityNotice(finalNoticeKind, finalStatus, finalNoticeAutoHideMs);
      }
    }
  }
}

function renderWorktreesList(scan, el) {
  if (!el) return;
  el.innerHTML = '';
  const all = Array.isArray(scan?.worktrees) ? scan.worktrees : [];
  if (!scan?.scanned_at) {
    el.innerHTML = '<div class="empty-state">No worktree scan yet</div>';
    return;
  }
  if (all.length === 0) {
    el.innerHTML = '<div class="empty-state">No worktrees found in scanned roots</div>';
    return;
  }

  const query = (document.getElementById('worktrees-search')?.value || '').trim().toLowerCase();
  const showActive = document.getElementById('filter-worktrees-active')?.checked ?? true;
  const showDirty = document.getElementById('filter-worktrees-dirty')?.checked ?? true;
  const showUnmerged = document.getElementById('filter-worktrees-unmerged')?.checked ?? true;
  const showMain = document.getElementById('filter-worktrees-main')?.checked ?? false;

  let rows = all.filter(wt => {
    if (!showActive && (wt.active_sessions || 0) > 0) return false;
    if (!showDirty && wt.dirty) return false;
    if (!showUnmerged && wt.merge_status === 'unmerged') return false;
    if (!showMain && wt.is_main) return false;
    return worktreeMatchesSearch(wt, query);
  });

  const sortVal = document.getElementById('sort-worktrees')?.value || 'size-desc';
  rows.sort((a, b) => {
    switch (sortVal) {
      case 'changed-desc':
        return worktreeDateSortValue(b, 'last_changed_at') - worktreeDateSortValue(a, 'last_changed_at');
      case 'changed-asc':
        return worktreeDateSortValue(a, 'last_changed_at') - worktreeDateSortValue(b, 'last_changed_at');
      case 'head-age-desc':
        return worktreeDateSortValue(a, 'head_author_time') - worktreeDateSortValue(b, 'head_author_time');
      case 'risk-desc':
        return worktreeRiskRank(b) - worktreeRiskRank(a) || (b.size_bytes || 0) - (a.size_bytes || 0);
      case 'path-asc':
        return String(a.path || '').localeCompare(String(b.path || ''));
      case 'size-desc':
      default:
        return (b.size_bytes || 0) - (a.size_bytes || 0);
    }
  });

  if (rows.length === 0) {
    el.innerHTML = '<div class="empty-state">No worktrees match the active filters</div>';
    return;
  }

  const totalRows = rows.length;
  if (rows.length > worktreesRenderWindow) {
    rows = rows.slice(0, worktreesRenderWindow);
  }

  for (const wt of rows) {
    const pathKey = String(wt.path || '');
    const removalState = worktreeRemovalState(pathKey);
    const removing = removalState === 'removing';
    const queued = removalState === 'queued';
    const card = document.createElement('div');
    card.className = 'session-card worktree-card';
    card.dataset.worktreePath = pathKey;
    if (removing) card.classList.add('removing');
    else if (queued) card.classList.add('queued');
    if (wt.safe_to_remove) card.classList.add('safe');
    else if (wt.dirty || wt.merge_status === 'unmerged' || (wt.active_sessions || 0) > 0 || wt.conflicted > 0) card.classList.add('danger');
    else card.classList.add('warning');

    const top = document.createElement('div');
    top.className = 'sc-top';

    const titleBlock = document.createElement('div');
    titleBlock.className = 'sc-title-block';
    const titleRow = document.createElement('div');
    titleRow.className = 'sc-title-row';
    const titleKind = document.createElement('span');
    titleKind.className = 'sc-title-kind';
    titleKind.textContent = wt.is_main ? 'main' : (wt.detached ? 'detached' : 'branch');
    titleRow.appendChild(titleKind);
    const branchEl = document.createElement('div');
    branchEl.className = 'wt-branch';
    branchEl.textContent = wt.branch || wt.head_short || '(unknown ref)';
    branchEl.title = wt.branch_ref || wt.head || branchEl.textContent;
    titleRow.appendChild(branchEl);
    titleBlock.appendChild(titleRow);
    const pathEl = document.createElement('div');
    pathEl.className = 'wt-path';
    pathEl.textContent = wt.path || '';
    pathEl.title = wt.path || '';
    titleBlock.appendChild(pathEl);
    top.appendChild(titleBlock);

    const wtStatus = removalState ? 'in_progress' : (wt.safe_to_remove ? 'completed' : (wt.dirty || wt.merge_status === 'unmerged' ? 'interrupted' : 'idle'));
    const statusEl = document.createElement('span');
    statusEl.className = `ui-chip ${sessionStatusChipTone(wtStatus)} sc-status ${wtStatus}`;
    statusEl.textContent = removalState || (wt.safe_to_remove ? 'candidate' : wt.recommended_action || 'review');
    top.appendChild(statusEl);
    card.appendChild(top);

    const labels = document.createElement('div');
    labels.className = 'wt-labels';
    for (const label of wt.labels || []) {
      const pill = document.createElement('span');
      pill.className = 'wt-label ' + String(label).replace(/[^a-z0-9_-]/gi, '-');
      pill.textContent = label;
      labels.appendChild(pill);
    }
    card.appendChild(labels);

    const safety = document.createElement('div');
    safety.className = 'wt-safety';
    safety.textContent = wt.safety || 'Review manually.';
    card.appendChild(safety);

    const meta = document.createElement('div');
    meta.className = 'sc-meta';
    const addMeta = (label, value, valueClass, title) => {
      if (value === null || value === undefined || value === '') return;
      const span = document.createElement('span');
      const l = document.createElement('span');
      l.className = 'label';
      l.textContent = label;
      const v = document.createElement('span');
      v.className = 'value' + (valueClass ? ` ${valueClass}` : '');
      v.textContent = value;
      if (title) v.title = title;
      span.appendChild(l);
      span.appendChild(v);
      meta.appendChild(span);
    };
    addMeta('repo', wt.repo_name);
    addMeta('size', _fmtBytes(wt.size_bytes || 0) + (wt.size_truncated ? ' +' : ''), wt.safe_to_remove ? 'v-green' : null);
    addMeta('changed', fmtWorktreeMinute(wt.last_changed_at), null, wt.last_changed_at || '');
    addMeta('commit', fmtWorktreeMinute(wt.head_author_time), null, wt.head_author_time || '');
    addMeta('merge', wt.merge_status, wt.merge_status === 'merged' ? 'v-green' : (wt.merge_status === 'unmerged' ? 'v-red' : null));
    if (wt.default_branch && (wt.default_ahead || wt.default_behind)) {
      addMeta('base', formatWorktreeDivergence(wt.default_branch, wt.default_ahead, wt.default_behind), (wt.default_behind || 0) > 0 ? 'v-yellow' : null, wt.default_branch);
    }
    if (wt.upstream && (!wt.default_branch || wt.upstream !== wt.default_branch) && (wt.ahead || wt.behind)) {
      addMeta('tracking', formatWorktreeDivergence(wt.upstream, wt.ahead, wt.behind), null, wt.upstream);
    }
    if (wt.dirty) addMeta('changes', `${wt.staged || 0} staged, ${wt.unstaged || 0} unstaged, ${wt.untracked || 0} untracked`, 'v-red');
    if ((wt.active_sessions || 0) > 0 || (wt.related_session_count || 0) > 0) {
      addMeta('sessions', `${wt.active_sessions || 0} active / ${wt.related_session_count || 0} related`, (wt.active_sessions || 0) > 0 ? 'v-yellow' : null);
    }
    addMeta('files', `${Number(wt.file_count || 0).toLocaleString()} files`);
    card.appendChild(meta);

    const actions = document.createElement('div');
    actions.className = 'sc-actions';
    const copyPath = document.createElement('button');
    copyPath.className = 'ui-btn sc-copy-btn';
    copyPath.textContent = 'Copy path';
    copyPath.addEventListener('click', (ev) => {
      ev.stopPropagation();
      copyTextToClipboard(wt.path || '').then(
        () => setWorktreesStatus('Copied worktree path.'),
        err => setWorktreesStatus(err.message || 'Copy failed', 'error')
      );
    });
    actions.appendChild(copyPath);

    const inspectBtn = document.createElement('button');
    inspectBtn.className = 'ui-btn sc-resume-btn';
    inspectBtn.textContent = 'Inspect';
    inspectBtn.title = 'Inspect review reasons, dirty files, and cleanup commands';
    inspectBtn.addEventListener('click', (ev) => {
      ev.stopPropagation();
      openWorktreeInspect(wt);
    });
    actions.appendChild(inspectBtn);

    if (wt.safe_to_remove) {
      const command = worktreeRemoveCommand(wt);
      const removeBtn = document.createElement('button');
      removeBtn.className = 'ui-btn danger sc-delete-btn';
      removeBtn.textContent = queued ? 'Queued...' : (removing ? 'Removing...' : 'Remove worktree...');
      removeBtn.title = 'Re-check safety and remove this worktree';
      removeBtn.disabled = !!removalState;
      removeBtn.classList.toggle('pending', !!removalState);
      removeBtn.addEventListener('click', (ev) => {
        ev.stopPropagation();
        removeWorktree(wt, removeBtn);
      });
      actions.appendChild(removeBtn);

      const copyCmd = document.createElement('button');
      copyCmd.className = 'ui-btn sc-copy-btn';
      copyCmd.textContent = 'Copy remove command';
      copyCmd.disabled = !!removalState;
      copyCmd.addEventListener('click', (ev) => {
        ev.stopPropagation();
        copyTextToClipboard(command).then(
          () => setWorktreesStatus('Copied git worktree remove command.'),
          err => setWorktreesStatus(err.message || 'Copy failed', 'error')
        );
      });
      actions.appendChild(copyCmd);
      const cmd = document.createElement('span');
      cmd.className = 'wt-command';
      cmd.textContent = command;
      cmd.title = command;
      actions.appendChild(cmd);
    }
    card.appendChild(actions);
    el.appendChild(card);
  }

  if (totalRows > rows.length) {
    const remaining = totalRows - rows.length;
    const moreRow = document.createElement('div');
    moreRow.className = 'sessions-show-more';
    const moreBtn = document.createElement('button');
    moreBtn.type = 'button';
    moreBtn.className = 'ui-btn sessions-show-more-btn';
    moreBtn.textContent = `Show ${Math.min(WORKTREE_CARD_RENDER_LIMIT, remaining).toLocaleString()} more (${remaining.toLocaleString()} remaining)`;
    moreBtn.onclick = () => {
      worktreesRenderWindow += WORKTREE_CARD_RENDER_LIMIT;
      if (_cachedWorktreeScan) renderWorktrees(_cachedWorktreeScan);
    };
    moreRow.appendChild(moreBtn);
    el.appendChild(moreRow);
  }
}

function resumeSession(s) {
  if (!app || !s || !s.session_id) return;
  const sid = String(s.session_id || '').trim();
  if (!sid) return;
  const source = normalizeAgentId(s.backend_source || s.source || '') || 'intendant';
  const resumeId = String(s.backend_session_id || s.resume_id || sid).trim() || sid;
  const projectRoot = s.project_root || s.projectRoot || null;
  const meta = {
    ...sessionWindowMetaFromSession(s),
    source,
    source_label: prettyAgentName(source) || source,
    backend_source: source,
    backend_session_id: resumeId,
    project_root: projectRoot,
    phase: 'idle',
    ended: false,
  };
  ensureSessionWindow(sid, meta);
  if (source !== 'intendant') setSessionWindowDetached(sid, true, 'resume requested');
  focusSessionWindow(sid);
  const overrides = sessionLaunchOverridesForSession(s);
  dispatchSessionControlMsg({
    action: 'resume_session',
    source,
    session_id: sid,
    resume_id: resumeId,
    project_root: projectRoot,
    direct: true,
    ...overrides,
  });
  // Resuming from the rendered Station must not yank the user out of the
  // canvas (routeTo deactivates the Station renderer — the surface looked
  // frozen with stale hit zones). Stay put and open the resumed session's
  // transcript instead; the legacy tabs keep their original routing.
  if (stationRenderedPrimaryActive()) {
    stationStatus(`Resumed ${shortSessionId(sid)} — transcript follows live`);
    stationOpenTranscript(sid, { source });
  } else {
    routeTo('activity');
  }
}

function showSessionDeleteStatus(message, kind = '') {
  const status = document.getElementById('session-delete-status');
  if (!status) return;
  status.className = 'session-config-status' + (kind ? ` ${kind}` : '');
  status.textContent = message || '';
}

function closeSessionDeleteModal() {
  const modal = document.getElementById('session-delete-modal');
  if (modal) modal.style.display = 'none';
  sessionDeletePending = null;
  showSessionDeleteStatus('');
  const confirmBtn = document.getElementById('session-delete-confirm');
  if (confirmBtn) {
    confirmBtn.disabled = false;
    confirmBtn.textContent = 'Delete';
  }
}

function deleteSessionData(sessionId, target, label) {
  const sid = String(sessionId || '').trim();
  if (!sid) {
    showControlToast('error', 'Delete failed: session ID is missing');
    return;
  }
  sessionDeletePending = { sessionId: sid, target, label };
  const shortId = sid.substring(0, 8);
  const message = document.getElementById('session-delete-message');
  if (message) message.textContent = `${label} for session ${shortId}?`;
  showSessionDeleteStatus('');
  const confirmBtn = document.getElementById('session-delete-confirm');
  if (confirmBtn) {
    confirmBtn.disabled = false;
    confirmBtn.textContent = 'Delete';
  }
  const modal = document.getElementById('session-delete-modal');
  if (modal) modal.style.display = 'flex';
  setTimeout(() => document.getElementById('session-delete-confirm')?.focus(), 0);
}

async function confirmSessionDeleteModal() {
  if (!sessionDeletePending) return;
  const { sessionId, target, label } = sessionDeletePending;

  const confirmBtn = document.getElementById('session-delete-confirm');
  if (confirmBtn) {
    confirmBtn.disabled = true;
    confirmBtn.textContent = 'Deleting...';
  }
  showSessionDeleteStatus('Deleting...');

  try {
    // Transport F8a: facade DELETE twin (/api/session/{id}/{target};
    // target 'session' deletes the whole session, a data kind deletes
    // that kind) — the verb-derived no-replay policy is the legacy
    // fallbackAfterRpcFailure:false semantics. The endpoint answers 200
    // with an {ok, error} body, so the body is the real verdict.
    const resp = await daemonApi.request('api_session_delete', { session_id: sessionId, target });
    const result = (resp.body && typeof resp.body === 'object') ? resp.body : {};
    if (!resp.ok || !result.ok) throw new Error(result.error || `HTTP ${resp.status}`);
    if (target === 'session') {
      _cachedSessions = _cachedSessions.filter(s => s.session_id !== sessionId);
    } else {
      const s = _cachedSessions.find(s => s.session_id === sessionId);
      if (s) {
        const freed = result.bytes_freed || 0;
        if (target === 'media' || target === 'recordings') {
          s.recordings = 0; s.recording_bytes = 0;
        }
        if (target === 'media' || target === 'frames') {
          s.annotations = 0; s.clips = 0; s.frames_bytes = 0;
        }
        if (target === 'turns') { s.turns_bytes = 0; }
        s.total_bytes = Math.max(0, (s.total_bytes || 0) - freed);
      }
    }
    sessionsListCache.set(sessionListCacheKey(selfPeerId), _cachedSessions);
    updateSessionProjectFilterOptions(_cachedSessions);
    renderSessionsAggregate(_cachedSessions, document.getElementById('sessions-aggregate'));
    _refilterSessions();
    closeSessionDeleteModal();
    showControlToast('success', `${label} complete`);
  } catch (e) {
    const message = e?.message || 'unknown';
    showSessionDeleteStatus('Delete failed: ' + message, 'error');
    showControlToast('error', 'Delete failed: ' + message);
    if (confirmBtn) {
      confirmBtn.disabled = false;
      confirmBtn.textContent = 'Delete';
    }
  }
}

function renderSessionDetailTitle(session) {
  const titleEl = document.getElementById('session-detail-title');
  if (!titleEl || !session) return;
  const sessionId = session.session_id || session.resume_id || '';
  const source = session.source || 'intendant';
  const sessionName = compactSessionText(session.name);
  const task = compactSessionText(session.task);
  const isResident = session.role === 'resident' || session.status === 'resident';
  const primaryText = sessionName || task || (isResident ? 'Daemon session' : 'Untitled session');
  const sourceLabel = session.source_label || (source === 'intendant' ? 'Intendant' : prettyAgentName(source) || source);
  const titleKindLabel = sessionName ? 'name' : task ? 'initial' : isResident ? 'resident' : 'untitled';
  const titleKindHelp = sessionName
    ? 'User-assigned session name'
    : task
      ? 'Initial message fallback'
      : isResident
        ? "The daemon's own resident session — becomes a task session when you message it"
        : 'No initial message found';

  titleEl.innerHTML = '';
  const titleLine = document.createElement('div');
  titleLine.className = 'sd-title-line';
  const titleKind = document.createElement('span');
  titleKind.className = 'sd-title-kind';
  titleKind.classList.add(titleKindLabel);
  titleKind.textContent = titleKindLabel;
  titleKind.title = titleKindHelp;
  const titleText = document.createElement('span');
  titleText.className = 'sd-title-text';
  titleText.textContent = primaryText;
  titleText.title = primaryText;
  const titleId = document.createElement('span');
  titleId.className = 'sd-title-id';
  titleId.textContent = `${sourceLabel} ${sessionId}`;
  titleId.title = sessionId;
  titleLine.appendChild(titleKind);
  titleLine.appendChild(titleText);
  titleLine.appendChild(titleId);
  titleEl.appendChild(titleLine);
  if (sessionName && task) {
    const subline = document.createElement('div');
    subline.className = 'sd-title-subline';
    const subKind = document.createElement('span');
    subKind.className = 'sd-title-kind';
    subKind.classList.add('initial');
    subKind.textContent = 'initial';
    subKind.title = 'Initial message fallback';
    const subText = document.createElement('span');
    subText.className = 'sd-title-text';
    subText.textContent = task;
    subText.title = task;
    subline.appendChild(subKind);
    subline.appendChild(subText);
    titleEl.appendChild(subline);
  }

  // Long-tail stats trimmed from the list cards stay visible here.
  const stats = document.createElement('div');
  stats.className = 'sd-meta-strip';
  const addStat = (label, value, valueTitle) => {
    if (value === null || value === undefined || value === '') return;
    const span = document.createElement('span');
    const l = document.createElement('span');
    l.className = 'label';
    l.textContent = label;
    const v = document.createElement('span');
    v.className = 'value';
    v.textContent = value;
    if (valueTitle) v.title = valueTitle;
    span.appendChild(l);
    span.appendChild(v);
    stats.appendChild(span);
  };
  addStat('created', session.created_at);
  addStat('changed', session.updated_at || session.changed_at);
  addStat('provider', session.provider);
  addStat('model', session.model);
  if (session.turns > 0) addStat('turns', session.turns);
  if (session.total_tokens > 0) {
    addStat('in', (session.prompt_tokens || 0).toLocaleString());
    if (session.cached_tokens > 0) addStat('cached', session.cached_tokens.toLocaleString());
    addStat('out', (session.completion_tokens || 0).toLocaleString());
    addStat('total', session.total_tokens.toLocaleString());
  }
  if (session.estimated_cost > 0) addStat('cost', formatUsd(session.estimated_cost));
  if (session.total_bytes > 0) addStat('disk', _fmtBytes(session.total_bytes));
  if (stats.children.length > 0) titleEl.appendChild(stats);

  renderSessionDetailLineage(titleEl, session);

  const renameBtn = document.getElementById('session-detail-rename');
  if (renameBtn) {
    renameBtn.disabled = !sessionId;
    renameBtn.title = sessionId ? 'Rename this session' : 'Session ID is missing';
  }
  const resumeBtn = document.getElementById('session-detail-resume');
  if (resumeBtn) {
    const canResume = !!sessionId && session.can_resume !== false;
    resumeBtn.disabled = !canResume;
    resumeBtn.style.display = canResume ? '' : 'none';
    resumeBtn.title = canResume ? 'Resume this session in Activity' : 'Session cannot be resumed';
  }
  const configBtn = document.getElementById('session-detail-config');
  if (configBtn) {
    const source = sessionConfigSource(sessionConfigMetadata(session));
    const canConfigure = !!sessionId && !!source && source !== 'intendant';
    configBtn.disabled = !canConfigure;
    configBtn.style.display = canConfigure ? '' : 'none';
    configBtn.title = canConfigure
      ? 'Configure binary and managed context for this session'
      : 'Only external-agent sessions have launch config';
  }
}

function sessionDetailContextForSubtab(subtab) {
  const targetSubtab = subtab === 'deep' ? 'deep' : 'recent';
  const pane = document.getElementById(`sessions-pane-${targetSubtab}`);
  if (!pane) return null;
  return {
    subtab: targetSubtab,
    pane,
    headerEl: pane.querySelector('.sessions-header'),
    listEl: document.getElementById(targetSubtab === 'deep' ? 'sessions-deep-list' : 'sessions-list'),
    aggEl: targetSubtab === 'recent' ? document.getElementById('sessions-aggregate') : null,
  };
}

function restoreSessionDetailContext(ctx) {
  if (!ctx) return;
  if (ctx.listEl) ctx.listEl.style.display = '';
  if (ctx.aggEl) ctx.aggEl.style.display = '';
  if (ctx.headerEl) ctx.headerEl.style.display = '';
}

function prepareSessionDetailContext(ctx, detailEl) {
  if (!ctx || !detailEl) return false;
  restoreSessionDetailContext(currentSessionDetailContext);
  if (detailEl.parentElement !== ctx.pane) {
    ctx.pane.appendChild(detailEl);
  }
  if (ctx.listEl) ctx.listEl.style.display = 'none';
  if (ctx.aggEl) ctx.aggEl.style.display = 'none';
  if (ctx.headerEl) ctx.headerEl.style.display = 'none';
  currentSessionDetailContext = ctx;
  return true;
}

function openSessionDetail(sessionOrId, taskArg) {
  const session = typeof sessionOrId === 'object'
    ? sessionOrId
    : { session_id: sessionOrId, task: taskArg, source: 'intendant' };
  const sessionId = session.session_id;
  const source = session.source || 'intendant';
  const detailEl = document.getElementById('session-detail');
  const logsEl = document.getElementById('session-detail-logs');
  const detailSubtab = activeSessionsSubtab === 'deep' ? 'deep' : 'recent';
  const detailContext = sessionDetailContextForSubtab(detailSubtab);
  if (!detailEl || !logsEl || !prepareSessionDetailContext(detailContext, detailEl)) return;

  detailEl.classList.remove('hidden');
  currentSessionDetail = { ...session, source };
  renderSessionDetailTitle(currentSessionDetail);
  logsEl.innerHTML = '<div class="empty-state">Loading...</div>';
  sessionDetailLogView = null;

  // Reset sections
  revokeSessionFrameObjectUrls();
  document.getElementById('sd-recordings').classList.add('hidden');
  document.getElementById('sd-frames').classList.add('hidden');
  document.getElementById('session-detail-frames').innerHTML = '';

  const detailSessionId = sessionId;
  const detailSource = source;
  const renderDetailFailure = (message) => {
    // Server/exception text renders as text, never markup.
    logsEl.textContent = '';
    const failure = document.createElement('div');
    failure.className = 'empty-state';
    failure.textContent = message;
    logsEl.appendChild(failure);
  };
  fetchSessionDetailPayload(detailSessionId, { source: detailSource, limit: SESSION_DETAIL_PAGE_LIMIT })
    .then(data => {
      if (data.error) {
        renderDetailFailure(String(data.error));
        return;
      }
      const entries = data.entries || [];
      applySessionIdentitiesFromReplayEntries(entries);
      applySessionGoalsFromReplayEntries(entries);
      renderSessionDetailLogs(entries, logsEl, {
        sessionId: detailSessionId,
        source: detailSource,
        pageStart: data.page_start ?? data.pageStart,
        pageEnd: data.page_end ?? data.pageEnd,
        totalEntries: data.total_entries ?? data.totalEntries,
        hasOlder: data.has_older ?? data.hasOlder,
      });
      updateSessionDetailLogsBadge(sessionDetailLogView, entries.length);

      // Render frames gallery
      const frames = data.frames || [];
      if (frames.length > 0) {
        renderSessionFrames(sessionId, frames);
      }
    })
    .catch(e => {
      renderDetailFailure(`Failed to load${e?.message ? ` — ${e.message}` : ''}`);
    });

  // Lazy-load recordings for Intendant sessions. External CLI histories do not
  // have Intendant media directories.
  if (source === 'intendant') {
    loadSessionRecordings(sessionId);
  }
}

function closeSessionDetail() {
  // Clean up session recording player
  if (sessionRecPlayer) { sessionRecPlayer.destroy(); sessionRecPlayer = null; }
  revokeSessionFrameObjectUrls();
  document.getElementById('session-detail-recordings').innerHTML = '';
  document.getElementById('session-detail-frames').innerHTML = '';
  document.getElementById('sd-recordings').classList.add('hidden');
  document.getElementById('sd-frames').classList.add('hidden');

  restoreSessionDetailContext(currentSessionDetailContext || sessionDetailContextForSubtab(activeSessionsSubtab));
  document.getElementById('session-detail').classList.add('hidden');
  currentSessionDetail = null;
  currentSessionDetailContext = null;
  sessionDetailLogView = null;
}
window.closeSessionDetail = closeSessionDetail;
window.loadSessions = loadSessions;

// ── Session Recording Replay ──
let sessionRecPlayer = null;
const sessionFrameObjectUrls = new Set();

function revokeSessionFrameObjectUrls() {
  for (const url of sessionFrameObjectUrls) {
    try { URL.revokeObjectURL(url); } catch (_) {}
  }
  sessionFrameObjectUrls.clear();
}

function dashboardSessionFrameAssetRpcAvailable() {
  return Boolean(
    dashboardTransport?.canUseRpc?.() &&
    dashboardControlTransport?.lastStatus?.byte_streams_available === true &&
    dashboardControlTransport?.lastStatus?.api_session_frame_asset_available === true
  );
}

async function sessionFrameAssetObjectUrl(sessionId, filename) {
  try {
    const transferResult = await dashboardFetchTransferArtifactBytes({
      type: 'session_frame_asset',
      session_id: sessionId,
      filename,
    }, {
      timeoutMs: 60000,
      filename,
    });
    if (transferResult?.blob) {
      const url = URL.createObjectURL(transferResult.blob);
      sessionFrameObjectUrls.add(url);
      return url;
    }
  } catch (err) {
    console.warn('session frame asset transfer failed; falling back to direct RPC', err);
  }
  if (!dashboardSessionFrameAssetRpcAvailable()) return null;
  try {
    const raw = await dashboardTransport.requestBytes('api_session_frame_asset', {
      session_id: sessionId,
      filename,
    }, { timeoutMs: 60000 });
    if (!raw || raw._httpOk === false || raw.ok === false) return null;
    if (!(raw.bytes instanceof Uint8Array)) return null;
    const contentType = raw.content_type || raw.contentType || 'application/octet-stream';
    const url = URL.createObjectURL(new Blob([raw.bytes], { type: contentType }));
    sessionFrameObjectUrls.add(url);
    return url;
  } catch (err) {
    const suffix = dashboardConnectModeEnabled()
      ? '; no HTTP fallback in Connect mode'
      : '; using HTTP image fallback';
    console.warn(`session frame asset RPC failed${suffix}`, err);
    return null;
  }
}

async function loadSessionRecordings(sessionId) {
  const container = document.getElementById('session-detail-recordings');
  const section = document.getElementById('sd-recordings');
  section.classList.add('hidden');
  container.innerHTML = '';

  let streams;
  try {
    // Transport F8a: facade GET twin (tunnel first, HTTP fallback); the
    // listing is a bare array, so the Array check below is the verdict.
    const resp = await daemonApi.request('api_session_recordings', { session_id: sessionId });
    streams = resp.body;
  } catch { return; }
  if (!Array.isArray(streams) || streams.length === 0) return;

  // Show the section
  section.classList.remove('hidden');
  const totalDur = streams.reduce((s, st) => s + (st.total_duration_secs || 0), 0);
  document.getElementById('sd-recordings-badge').textContent =
    streams.length + ' stream' + (streams.length > 1 ? 's' : '') +
    (totalDur > 0 ? ' \u00b7 ' + Math.round(totalDur) + 's' : '');

  // Stream selector
  const header = document.createElement('div');
  header.className = 'session-rec-header';
  const select = document.createElement('select');
  for (const s of streams) {
    const opt = document.createElement('option');
    opt.value = s.stream_name;
    opt.textContent = s.stream_name.startsWith('display_') ? ':' + s.stream_name.slice(8) : s.stream_name;
    if (s.total_duration_secs) opt.textContent += ` (${Math.round(s.total_duration_secs)}s)`;
    select.appendChild(opt);
  }
  header.appendChild(select);
  container.appendChild(header);

  // Player wrap
  const playerWrap = document.createElement('div');
  playerWrap.className = 'recording-player-wrap';
  const video = document.createElement('video');
  video.preload = 'auto';
  playerWrap.appendChild(video);
  container.appendChild(playerWrap);

  // Timeline
  const timeline = document.createElement('div');
  timeline.className = 'recording-timeline';
  const timelineBar = document.createElement('div');
  timelineBar.className = 'timeline-bar';
  const timelineProgress = document.createElement('div');
  timelineProgress.className = 'timeline-progress';
  const timelineCursor = document.createElement('div');
  timelineCursor.className = 'timeline-cursor';
  timeline.appendChild(timelineBar);
  timeline.appendChild(timelineProgress);
  timeline.appendChild(timelineCursor);
  container.appendChild(timeline);

  // Controls
  const controls = document.createElement('div');
  controls.className = 'recording-controls';
  const playBtn = document.createElement('button');
  playBtn.innerHTML = '&#x25B6;';
  playBtn.title = 'Play/Pause';
  const timeLabel = document.createElement('span');
  timeLabel.className = 'rec-time';
  timeLabel.textContent = '0:00 / 0:00';
  const speedSelect = document.createElement('select');
  speedSelect.title = 'Playback speed';
  for (const sp of ['0.5', '1', '2', '4']) {
    const o = document.createElement('option');
    o.value = sp;
    o.textContent = sp + 'x';
    if (sp === '1') o.selected = true;
    speedSelect.appendChild(o);
  }
  controls.appendChild(playBtn);
  controls.appendChild(timeLabel);
  controls.appendChild(speedSelect);
  container.appendChild(controls);

  const baseUrl = `/api/session/${sessionId}/recordings`;

  function initPlayer(streamName) {
    if (sessionRecPlayer) sessionRecPlayer.destroy();
    sessionRecPlayer = new RecordingPlayer(video, timeline, timelineCursor, timelineProgress, timeLabel, playBtn, baseUrl);
    speedSelect.value = '1';
    speedSelect.onchange = () => sessionRecPlayer.setSpeed(parseFloat(speedSelect.value));
    sessionRecPlayer.load(streamName);
  }

  select.addEventListener('change', () => { if (select.value) initPlayer(select.value); });
  initPlayer(streams[0].stream_name);
}

function renderSessionFrames(sessionId, frames) {
  const section = document.getElementById('sd-frames');
  const container = document.getElementById('session-detail-frames');
  revokeSessionFrameObjectUrls();
  container.innerHTML = '';
  section.classList.remove('hidden');

  // Group frames by category
  const annotations = frames.filter(f => f.startsWith('ann-'));
  const clipMap = new Map(); // clipId → [filenames]
  const other = [];
  for (const f of frames) {
    if (f.startsWith('ann-')) continue;
    if (f.startsWith('clip-')) {
      const m = f.match(/^(clip-.+-\d+)-f\d+/);
      const key = m ? m[1] : f;
      if (!clipMap.has(key)) clipMap.set(key, []);
      clipMap.get(key).push(f);
    } else {
      other.push(f);
    }
  }

  // Badge summary
  const parts = [];
  if (annotations.length) parts.push(annotations.length + ' annotation' + (annotations.length > 1 ? 's' : ''));
  if (clipMap.size) parts.push(clipMap.size + ' clip' + (clipMap.size > 1 ? 's' : ''));
  if (other.length) parts.push(other.length + ' other');
  document.getElementById('sd-frames-badge').textContent = parts.join(' \u00b7 ');

  function makeThumb(name) {
    const thumb = document.createElement('div');
    thumb.className = 'sd-frame-thumb';
    const img = document.createElement('img');
    const fallbackSrc = `/api/session/${encodeURIComponent(sessionId)}/frames/${encodeURIComponent(name)}`;
    img.loading = 'lazy';
    const placeholderSrc = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=';
    img.src = dashboardConnectModeEnabled() ? placeholderSrc : fallbackSrc;
    img.dataset.fallbackSrc = fallbackSrc;
    img.alt = name;
    sessionFrameAssetObjectUrl(sessionId, name).then(url => {
      if (!url) {
        if (!dashboardConnectModeEnabled() && !img.src) img.src = fallbackSrc;
        return;
      }
      if (!img.isConnected) {
        URL.revokeObjectURL(url);
        sessionFrameObjectUrls.delete(url);
        return;
      }
      img.dataset.objectUrl = url;
      img.src = url;
    });
    thumb.appendChild(img);
    const label = document.createElement('div');
    label.className = 'frame-label';
    label.textContent = name;
    thumb.appendChild(label);
    thumb.addEventListener('click', () => {
      const lb = document.createElement('div');
      lb.className = 'sd-frame-lightbox';
      const full = document.createElement('img');
      full.src = img.src || (dashboardConnectModeEnabled() ? placeholderSrc : fallbackSrc);
      lb.appendChild(full);
      lb.addEventListener('click', () => lb.remove());
      document.body.appendChild(lb);
    });
    return thumb;
  }

  function makeGroup(title, badge, fileList, collapsed) {
    const group = document.createElement('div');
    group.className = 'sd-frame-group' + (collapsed ? ' collapsed' : '');
    const header = document.createElement('div');
    header.className = 'sd-frame-group-header';
    header.innerHTML = `<span class="sd-frame-group-chevron">&#x25BE;</span>${title} <span class="sd-frame-group-badge">${badge}</span>`;
    header.addEventListener('click', () => group.classList.toggle('collapsed'));
    group.appendChild(header);
    const gallery = document.createElement('div');
    gallery.className = 'sd-frames-gallery';
    for (const name of fileList) gallery.appendChild(makeThumb(name));
    group.appendChild(gallery);
    return group;
  }

  // Annotations — expanded
  if (annotations.length) {
    container.appendChild(makeGroup(
      'Annotations', annotations.length + ' frame' + (annotations.length > 1 ? 's' : ''),
      annotations, false
    ));
  }

  // Clips — each collapsed
  for (const [clipId, clipFrames] of clipMap) {
    container.appendChild(makeGroup(
      clipId, clipFrames.length + ' frame' + (clipFrames.length > 1 ? 's' : ''),
      clipFrames, true
    ));
  }

  // Other/raw frames — collapsed
  if (other.length) {
    container.appendChild(makeGroup(
      'Raw frames', other.length + ' frame' + (other.length > 1 ? 's' : ''),
      other, true
    ));
  }
}
window.intendantSessionFrameRenderer = {
  render: renderSessionFrames,
  revokeObjectUrls: revokeSessionFrameObjectUrls,
};

const SESSION_DETAIL_RENDER_LIMIT = 600;
const SESSION_DETAIL_SCROLL_CHUNK = 300;
const SESSION_DETAIL_SCROLL_THRESHOLD_PX = 64;
const SESSION_DETAIL_PAGE_LIMIT = 1000;
let detailVerbosity = 'normal';
let detailLogFilter = 'all';
let detailExpandAll = false;
let sessionDetailLogView = null;

function makeSessionDetailLogButton(label, title, onClick) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.textContent = label;
  btn.title = title;
  btn.addEventListener('click', onClick);
  return btn;
}

function sessionDetailNumberOrNull(value) {
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? number : null;
}

function sessionDetailPageInfoFromView(view) {
  if (!view) return {};
  return {
    sessionId: view.sessionId,
    source: view.source,
    pageStart: view.pageStart,
    pageEnd: view.pageEnd,
    totalEntries: view.totalEntries,
    hasOlder: view.hasOlder,
  };
}

function updateSessionDetailViewPageState(view, info = {}) {
  if (!view) return;
  view.sessionId = String(
    info.sessionId ||
    view.sessionId ||
    currentSessionDetail?.session_id ||
    currentSessionDetail?.resume_id ||
    ''
  ).trim();
  view.source = normalizeAgentId(info.source || view.source || currentSessionDetail?.source || 'intendant') || 'intendant';

  const pageStart = sessionDetailNumberOrNull(info.pageStart ?? info.page_start);
  if (pageStart !== null) {
    view.pageStart = Number.isFinite(view.pageStart) ? Math.min(view.pageStart, pageStart) : pageStart;
  }
  const pageEnd = sessionDetailNumberOrNull(info.pageEnd ?? info.page_end);
  if (pageEnd !== null) {
    view.pageEnd = Number.isFinite(view.pageEnd) ? Math.max(view.pageEnd, pageEnd) : pageEnd;
  }
  const totalEntries = sessionDetailNumberOrNull(info.totalEntries ?? info.total_entries);
  if (totalEntries !== null) view.totalEntries = totalEntries;
  view.hasOlder = Boolean(info.hasOlder ?? info.has_older) ||
    (Number.isFinite(view.pageStart) && view.pageStart > 0);
}

function sessionDetailEntryMergeKey(entry = {}) {
  if (!entry || typeof entry !== 'object') return '';
  const content = compactSessionTextBounded(
    entry.content ?? entry.summary ?? entry.message ?? entry.text ?? entry.stdout ?? entry.stderr ?? entry.data?.message ?? '',
    SESSION_TEXT_SIGNATURE_CHAR_LIMIT,
    { signature: true }
  );
  return [
    entry.event || entry.type || '',
    entry.ts || entry.timestamp || entry.time || '',
    entry.session_id || entry.sessionId || '',
    entry.source || '',
    entry.level || '',
    entry.item_id || entry.itemId || '',
    entry.output_id || entry.outputId || '',
    entry.user_turn_index ?? entry.userTurnIndex ?? '',
    entry.user_turn_revision ?? entry.userTurnRevision ?? '',
    entry.turn ?? '',
    content,
  ].join('\u001f');
}

function mergeSessionDetailEntries(view, incoming, prepend = true) {
  if (!view || !Array.isArray(incoming) || incoming.length === 0) return 0;
  if (!Array.isArray(view.entries)) view.entries = [];
  const existingKeys = new Set(
    view.entries.map(entry => sessionDetailEntryMergeKey(entry)).filter(Boolean)
  );
  const unique = [];
  for (const entry of incoming) {
    const key = sessionDetailEntryMergeKey(entry);
    if (key && existingKeys.has(key)) continue;
    unique.push(entry);
    if (key) existingKeys.add(key);
  }
  if (!unique.length) return 0;
  view.entries = prepend ? unique.concat(view.entries) : view.entries.concat(unique);
  return unique.length;
}

function rebuildSessionDetailViewRows(view) {
  if (!view) return;
  view.rows = buildSessionDetailRows(view.entries || []);
  view.entryCount = countSessionDetailEntries(view.rows, 0, view.rows.length);
  view.expandableRowCount = countSessionDetailExpandableRows(view.rows);
  view.expandedRows = new Set();
  if (detailExpandAll) setSessionDetailRowsExpanded(view, true);
}

function updateSessionDetailLogsBadge(view, fallbackCount = 0) {
  const badge = document.getElementById('sd-logs-badge');
  if (!badge) return;
  const loaded = Array.isArray(view?.entries)
    ? view.entries.length
    : Math.max(0, Number(fallbackCount) || 0);
  const totalEntries = sessionDetailNumberOrNull(view?.totalEntries);
  badge.textContent = totalEntries !== null && totalEntries > loaded
    ? `${loaded}/${totalEntries} entries`
    : `${loaded} entries`;
}

function renderSessionDetailLogs(entries, el, pageInfo = {}) {
  el.innerHTML = '';
  sessionDetailLogView = null;
  const initialEntries = Array.isArray(entries) ? entries : [];

  const toolbar = document.createElement('div');
  toolbar.className = 'session-detail-log-toolbar';
  const status = document.createElement('div');
  status.className = 'session-detail-log-status';

  const controls = document.createElement('div');
  controls.className = 'session-detail-log-actions';
  const firstBtn = makeSessionDetailLogButton('First', 'Show earliest loaded entries', () => {
    const view = sessionDetailLogView;
    if (view?.renderStart > 0) {
      setSessionDetailLogRange(view, 0);
    } else if (view?.hasOlder) {
      loadOlderSessionDetailRows(view, { forceRemote: true });
    }
  });
  const prevBtn = makeSessionDetailLogButton('Prev', 'Show previous entries', () => {
    const view = sessionDetailLogView;
    if (view?.renderStart <= 0 && view?.hasOlder) {
      loadOlderSessionDetailRows(view, { forceRemote: true });
    } else {
      setSessionDetailLogRange(view, (view?.renderStart || 0) - SESSION_DETAIL_RENDER_LIMIT);
    }
  });
  const jumpInput = document.createElement('input');
  jumpInput.className = 'session-detail-log-jump';
  jumpInput.type = 'number';
  jumpInput.min = '1';
  jumpInput.step = '1';
  jumpInput.placeholder = 'Entry';
  jumpInput.title = 'Jump to entry number';
  jumpInput.addEventListener('change', () => jumpSessionDetailLogToEntry(sessionDetailLogView, jumpInput.value));
  jumpInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') {
      event.preventDefault();
      jumpSessionDetailLogToEntry(sessionDetailLogView, jumpInput.value);
    }
  });
  const nextBtn = makeSessionDetailLogButton('Next', 'Show next entries', () => {
    setSessionDetailLogRange(sessionDetailLogView, (sessionDetailLogView?.renderStart || 0) + SESSION_DETAIL_RENDER_LIMIT);
  });
  const latestBtn = makeSessionDetailLogButton('Latest', 'Show latest loaded entries', () => {
    setSessionDetailLogRange(sessionDetailLogView, sessionDetailMaxRenderStart(sessionDetailLogView), 'bottom');
  });
  const expandAllBtn = makeSessionDetailLogButton('Expand all', 'Expand every collapsible entry in this log view', () => {
    toggleSessionDetailExpandAll(sessionDetailLogView);
  });
  const filterSelect = document.createElement('select');
  filterSelect.className = 'session-detail-log-filter';
  filterSelect.title = 'Log source filter';
  filterSelect.innerHTML = '<option value="all">All messages</option><option value="user">User only</option><option value="non-user">Hide user</option>';
  filterSelect.value = detailLogFilter;
  const verbositySelect = document.createElement('select');
  verbositySelect.className = 'verbosity-select';
  verbositySelect.title = 'Log verbosity';
  verbositySelect.innerHTML = '<option value="normal">Normal</option><option value="verbose">Verbose</option><option value="debug">Debug</option>';
  verbositySelect.value = detailVerbosity;
  controls.appendChild(firstBtn);
  controls.appendChild(prevBtn);
  controls.appendChild(jumpInput);
  controls.appendChild(nextBtn);
  controls.appendChild(latestBtn);
  controls.appendChild(expandAllBtn);
  controls.appendChild(filterSelect);
  controls.appendChild(verbositySelect);
  toolbar.appendChild(status);
  toolbar.appendChild(controls);
  el.appendChild(toolbar);

  const logsContainer = document.createElement('div');
  logsContainer.className = 'session-detail-log-stream';
  el.appendChild(logsContainer);

  const pagerControls = { firstBtn, prevBtn, nextBtn, latestBtn, expandAllBtn, jumpInput };
  const renderCurrent = () => {
    const currentEntries = sessionDetailLogView?.entries || initialEntries;
    const currentPageInfo = sessionDetailLogView
      ? sessionDetailPageInfoFromView(sessionDetailLogView)
      : pageInfo;
    renderSessionDetailEntries(currentEntries, logsContainer, status, pagerControls, currentPageInfo);
    updateSessionDetailLogsBadge(sessionDetailLogView, currentEntries.length);
  };
  filterSelect.addEventListener('change', () => {
    detailLogFilter = filterSelect.value;
    renderCurrent();
  });
  verbositySelect.addEventListener('change', () => {
    detailVerbosity = verbositySelect.value;
    renderCurrent();
  });

  renderCurrent();
}

function sessionDetailVisibleLevels() {
  const normalLevels = ['info', 'model', 'agent', 'error', 'warn', 'subagent', 'presence'];
  const verboseLevels = [...normalLevels, 'detail'];
  const debugLevels = [...verboseLevels, 'debug'];
  return detailVerbosity === 'debug' ? debugLevels :
         detailVerbosity === 'verbose' ? verboseLevels : normalLevels;
}

const sessionDetailLevelColors = {
  model: 'var(--blue)', agent: 'var(--teal)', error: 'var(--red)',
  warn: 'var(--yellow)', subagent: 'var(--mauve)', presence: 'var(--green)',
  detail: 'var(--log-detail)', debug: 'var(--overlay1)',
};
const sessionDetailSourceLabels = {
  system: '\u2139', worker: 'Model', agent: 'Run',
  server: 'Servr', live: 'Live', sub: 'Sub', orch: 'Orch',
  presence: 'Prsnc', user: 'User',
};

// External Claude Code tool results ride user-role transcript lines; the
// server flattening keeps no marker, so only unambiguous shapes relabel
// USER \u2192 TOOL: explicit tool_result fields, a serialized tool_result block
// that escaped flattening, a <tool_use_error> envelope, or two consecutive
// Read-tool "N\u2192" numbered lines. Anything a human could plausibly have
// typed stays USER.
function sessionDetailToolResultShape(e) {
  if (String(e?.kind || '').toLowerCase() === 'tool_result') return true;
  if (e?.tool_use_id) return true;
  const head = String(e?.content || '').slice(0, 600).trimStart();
  if (/^\[?\s*\{\s*"type"\s*:\s*"tool_result"/.test(head)) return true;
  if (head.startsWith('<tool_use_error>')) return true;
  const lines = head.split('\n', 3);
  if (/^\s{0,8}\d+\u2192/.test(lines[0] || '') && /^\s{0,8}\d+\u2192/.test(lines[1] || '')) return true;
  return false;
}

function isSessionDetailUserEntry(entry) {
  const source = String(entry?.source || '').trim().toLowerCase();
  const role = String(entry?.role || entry?.type || '').trim().toLowerCase();
  return source === 'user' || role === 'user' || role === 'human' ||
    (entry?.user_turn_index !== undefined && entry?.user_turn_index !== null);
}

function sessionDetailEntryMatchesLogFilter(entry) {
  if (detailLogFilter === 'user') return isSessionDetailUserEntry(entry);
  if (detailLogFilter === 'non-user') return !isSessionDetailUserEntry(entry);
  return true;
}

function sessionDetailRowIsExpandable(row) {
  if (row?.kind !== 'entry') return false;
  const e = row.record || {};
  const content = String(e.content || '');
  return isDiffLog({ level: e.level, source: e.source, content }) ||
    content.split('\n').length > 3 || content.length > 300;
}

function setSessionDetailRowsExpanded(view, expanded) {
  if (!view) return;
  view.expandedRows.clear();
  if (!expanded) return;
  for (let i = 0; i < view.rows.length; i++) {
    if (sessionDetailRowIsExpandable(view.rows[i])) view.expandedRows.add(i);
  }
}

function toggleSessionDetailExpandAll(view) {
  if (!view) return;
  detailExpandAll = !detailExpandAll;
  setSessionDetailRowsExpanded(view, detailExpandAll);
  const scrollTop = view.scroller?.scrollTop || 0;
  renderSessionDetailRange(view, view.renderStart);
  if (view.scroller) view.scroller.scrollTop = scrollTop;
}

function buildSessionDetailRows(entries) {
  const rows = [];
  let lastTurn = null;
  const visibleLevels = sessionDetailVisibleLevels();

  for (let e of entries || []) {
    // Display-only session notes carry their body in `text` and their
    // image references in `attachments`; normalize them into the generic
    // record shape (content + attachment_previews) the renderer expects.
    if (e && e.event === 'session_note') {
      const note = sessionNoteLogCommand(e);
      if (!note) continue;
      e = { ...e, ...note };
    }
    // Agent→user notifications normalize the same way (title folded into
    // the content, urgency mapped to the row level).
    if (e && e.event === 'user_notification') {
      const notification = userNotificationLogCommand(e);
      if (!notification) continue;
      e = { ...e, ...notification };
    }
    const level = e.level || 'info';
    if (!visibleLevels.includes(level)) continue;
    if (!sessionDetailEntryMatchesLogFilter(e)) continue;
    const source = e.source || '';
    let displaySource = sessionDetailSourceLabels[source] || source || '\u2139';
    // 'YOU/USER' mislabel on external tool results: user-role lines whose
    // shape is unambiguously a tool result render as TOOL. External
    // transcripts only \u2014 native logs label their sources correctly.
    if (source === 'user'
        && (currentSessionDetail?.source || 'intendant') !== 'intendant'
        && sessionDetailToolResultShape(e)) {
      displaySource = 'Tool';
    }
    const content = e.content || e.summary || e.message || e.stdout || e.stderr || '';
    if (!String(content).trim()) continue;
    const detailRecord = {
      ...e,
      level,
      source,
      session_id: e.session_id || currentSessionDetail?.backend_session_id || currentSessionDetail?.resume_id || currentSessionDetail?.session_id || '',
      content,
    };

    if (e.turn && e.turn !== lastTurn) {
      lastTurn = e.turn;
      rows.push({ kind: 'turn', turn: e.turn });
    }

    rows.push({
      kind: 'entry',
      record: detailRecord,
      displaySource,
    });
  }

  return rows;
}

function renderSessionDetailEntries(entries, el, statusEl = null, pagerControls = null, pageInfo = {}) {
  const view = {
    entries: Array.isArray(entries) ? entries.slice() : [],
    rows: [],
    scroller: el,
    statusEl,
    pagerControls,
    renderStart: 0,
    renderEnd: 0,
    expandedRows: new Set(),
    pendingScrollFrame: 0,
  };
  updateSessionDetailViewPageState(view, pageInfo);
  rebuildSessionDetailViewRows(view);
  sessionDetailLogView = view;
  el._sessionDetailView = view;
  if (el._sessionDetailScrollHandler) {
    el.removeEventListener('scroll', el._sessionDetailScrollHandler);
  }
  el._sessionDetailScrollHandler = () => handleSessionDetailLogScroll(view);
  el.addEventListener('scroll', el._sessionDetailScrollHandler);
  renderSessionDetailRange(view, 0);
  el.scrollTop = 0;
}

function sessionDetailMaxRenderStart(view) {
  return Math.max(0, (view?.rows?.length || 0) - SESSION_DETAIL_RENDER_LIMIT);
}

function setSessionDetailLogRange(view, start, scrollMode = 'top') {
  if (!view) return;
  renderSessionDetailRange(view, start);
  if (!view.scroller) return;
  if (scrollMode === 'bottom') {
    view.scroller.scrollTop = Math.max(0, view.scroller.scrollHeight - view.scroller.clientHeight);
  } else {
    view.scroller.scrollTop = 0;
  }
}

function sessionDetailRowIndexForEntryOrdinal(rows, ordinal) {
  const target = Math.max(1, Math.floor(Number(ordinal) || 1));
  let count = 0;
  for (let i = 0; i < rows.length; i++) {
    if (rows[i]?.kind !== 'entry') continue;
    count += 1;
    if (count >= target) return i;
  }
  return sessionDetailMaxRenderStart({ rows });
}

function jumpSessionDetailLogToEntry(view, value) {
  if (!view || !view.entryCount) return;
  const ordinal = Math.max(1, Math.min(view.entryCount, Math.floor(Number(value) || 1)));
  const rowIndex = sessionDetailRowIndexForEntryOrdinal(view.rows, ordinal);
  setSessionDetailLogRange(view, Math.min(rowIndex, sessionDetailMaxRenderStart(view)));
}

function renderSessionDetailRange(view, start) {
  if (!view || !view.scroller) return;
  const { rows, scroller } = view;
  if (rows.length === 0) {
    view.renderStart = 0;
    view.renderEnd = 0;
    scroller.innerHTML = '<div class="empty-state">No matching log entries</div>';
    updateSessionDetailLogStatus(view);
    return;
  }

  const maxStart = sessionDetailMaxRenderStart(view);
  const safeStart = Math.max(0, Math.min(Number(start) || 0, maxStart));
  const safeEnd = Math.min(rows.length, safeStart + SESSION_DETAIL_RENDER_LIMIT);
  const fragment = document.createDocumentFragment();
  for (let i = safeStart; i < safeEnd; i++) {
    const node = materializeSessionDetailRow(view, rows[i], i);
    if (node) fragment.appendChild(node);
  }
  view.renderStart = safeStart;
  view.renderEnd = safeEnd;
  scroller.replaceChildren(fragment);
  updateSessionDetailLogStatus(view);
}

function updateSessionDetailLogStatus(view) {
  if (!view) return;
  const total = view.entryCount || 0;
  const loadSuffix = sessionDetailLoadedRawSuffix(view);
  if (total === 0) {
    if (view.statusEl) {
      const loading = view.loadingOlder ? 'Loading older entries...' : `0 ${sessionDetailLogFilterLabel()}`;
      view.statusEl.textContent = `${loading}${loadSuffix}`;
    }
    updateSessionDetailLogPager(view, 0, 0, total);
    return;
  }
  const before = countSessionDetailEntries(view.rows, 0, view.renderStart);
  const current = countSessionDetailEntries(view.rows, view.renderStart, view.renderEnd);
  if (view.statusEl) {
    const prefix = view.loadingOlder ? 'Loading older entries; ' : '';
    view.statusEl.textContent = `${prefix}Showing ${before + 1}-${before + current} of ${total} ${sessionDetailLogFilterLabel()}${loadSuffix}`;
  }
  updateSessionDetailLogPager(view, before, current, total);
}

function sessionDetailLogFilterLabel() {
  if (detailLogFilter === 'user') return 'user entries';
  if (detailLogFilter === 'non-user') return 'non-user entries';
  return 'entries';
}

function sessionDetailLoadedRawSuffix(view) {
  const totalEntries = sessionDetailNumberOrNull(view?.totalEntries);
  const loaded = Array.isArray(view?.entries) ? view.entries.length : 0;
  if (totalEntries !== null && totalEntries > loaded) {
    return ` (${loaded}/${totalEntries} raw loaded)`;
  }
  return '';
}

function countSessionDetailExpandableRows(rows) {
  let count = 0;
  for (const row of rows || []) {
    if (sessionDetailRowIsExpandable(row)) count += 1;
  }
  return count;
}

function updateSessionDetailLogPager(view, before, current, total) {
  const controls = view?.pagerControls;
  if (!controls) return;
  const hasRows = total > 0 && view.rows.length > 0;
  const canMoveOlder = hasRows && view.renderStart > 0;
  const canLoadOlder = Boolean(view.hasOlder);
  controls.firstBtn.disabled = (!canMoveOlder && !canLoadOlder) || view.loadingOlder;
  controls.prevBtn.disabled = controls.firstBtn.disabled;
  controls.nextBtn.disabled = !hasRows || view.renderEnd >= view.rows.length;
  controls.latestBtn.disabled = controls.nextBtn.disabled;
  if (controls.expandAllBtn) {
    const canToggleExpand = hasRows && (view.expandableRowCount > 0 || detailExpandAll);
    controls.expandAllBtn.disabled = !canToggleExpand;
    controls.expandAllBtn.textContent = detailExpandAll ? 'Collapse all' : 'Expand all';
    controls.expandAllBtn.title = detailExpandAll
      ? 'Collapse every expanded entry in this log view'
      : 'Expand every collapsible entry in this log view';
  }
  controls.jumpInput.disabled = !hasRows;
  controls.jumpInput.max = String(Math.max(1, total));
  if (document.activeElement !== controls.jumpInput) {
    controls.jumpInput.value = hasRows ? String(before + (current > 0 ? 1 : 0)) : '';
  }
}

function countSessionDetailEntries(rows, start, end) {
  let count = 0;
  const safeStart = Math.max(0, Number(start) || 0);
  const safeEnd = Math.min(rows.length, Math.max(safeStart, Number(end) || 0));
  for (let i = safeStart; i < safeEnd; i++) {
    if (rows[i]?.kind === 'entry') count += 1;
  }
  return count;
}

function materializeSessionDetailRow(view, row, index) {
  if (!row) return null;
  if (row.kind === 'turn') {
    const sep = document.createElement('div');
    sep.className = 'log-turn-sep';
    sep.dataset.detailRowIndex = String(index);
    sep.textContent = '\u2500\u2500 Turn ' + row.turn + ' \u2500\u2500';
    return sep;
  }
  if (row.kind !== 'entry') return null;

  const e = row.record;
  if (isSessionWindowCommandOutputRecord(e)) {
    const outputEntry = buildSessionWindowCommandOutputEntry(e);
    if (!outputEntry) return null;
    outputEntry.dataset.detailRowIndex = String(index);
    return outputEntry;
  }

  const entry = document.createElement('div');
  entry.className = 'log-entry level-' + e.level;
  entry.dataset.detailRowIndex = String(index);
  if (e.kind) entry.dataset.kind = e.kind;
  const sourceClass = String(e.source).toLowerCase().replace(/[^a-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '');
  if (sourceClass) entry.classList.add('source-' + sourceClass);
  if (e.session_id) entry.dataset.sessionId = e.session_id;
  if (e.user_turn_index !== undefined && e.user_turn_index !== null) {
    entry.dataset.userTurnIndex = String(e.user_turn_index);
  }
  if (e.user_turn_revision !== undefined && e.user_turn_revision !== null) {
    entry.dataset.userTurnRevision = String(e.user_turn_revision);
  }
  if (e.superseded) {
    entry.classList.add('superseded');
    entry.dataset.superseded = 'true';
  }

  const ts = document.createElement('span');
  ts.className = 'log-ts';
  ts.textContent = formatSessionDetailTimestamp(e.ts);
  const tsTitle = formatLogTimestampTitle(e.ts);
  if (tsTitle && ts.textContent !== tsTitle) ts.title = tsTitle;

  const lvl = document.createElement('span');
  lvl.className = 'log-level';
  lvl.textContent = row.displaySource;
  lvl.style.color = sessionDetailLevelColors[e.level] || 'var(--text)';

  if (isDiffLog({ level: e.level, source: e.source, content: e.content })) {
    entry.classList.add('diff-log-entry');
    if (view.expandedRows.has(index)) entry.classList.add('expanded');
    const parsed = parseUnifiedDiff(diffLogContent({ source: e.source, content: e.content }));
    const wrap = document.createElement('span');
    wrap.className = 'log-content diff-log-wrap';
    const summary = document.createElement('span');
    summary.className = 'diff-log-summary';
    summary.innerHTML = diffLogSummaryHtml(parsed);
    const body = document.createElement('span');
    body.className = 'diff-log-body';
    renderDiffLogBody(body, parsed, { sessionId: e.session_id || '' });
    wrap.appendChild(summary);
    wrap.appendChild(body);
    const toggle = document.createElement('span');
    toggle.className = 'collapse-toggle';
    toggle.innerHTML = '<span class="arrow">\u25B8 diff</span><span class="arrow-up">\u25BE hide</span>';
    entry.appendChild(ts);
    entry.appendChild(lvl);
    entry.appendChild(wrap);
    entry.appendChild(toggle);
    entry.addEventListener('click', (event) => {
      if (event.target?.closest?.('a, button')) return;
      requestAnimationFrame(() => {
        if (entry.classList.contains('expanded')) view.expandedRows.add(index);
        else view.expandedRows.delete(index);
      });
    });
    wireDiffLogEntry(entry);
    return entry;
  }

  const cnt = document.createElement('span');
  cnt.className = 'log-content';
  renderLogContentElement(cnt, {
    level: e.level,
    source: row.displaySource,
    content: e.content
  });
  appendLogStateBadges(cnt, e);
  appendLogAttachmentStrip(cnt, e);
  if (sessionDetailLevelColors[e.level]) cnt.style.color = sessionDetailLevelColors[e.level];

  entry.appendChild(ts);
  entry.appendChild(lvl);
  entry.appendChild(cnt);
  appendEditUserMessageButton(entry, e);

  if (e.content.split('\n').length > 3 || e.content.length > 300) {
    const expanded = view.expandedRows.has(index);
    cnt.style.maxHeight = expanded ? 'none' : '1.5em';
    cnt.style.overflow = expanded ? 'visible' : 'hidden';
    cnt.style.cursor = 'pointer';
    const toggle = document.createElement('span');
    toggle.className = 'collapse-toggle';
    toggle.innerHTML = '<span class="arrow">\u25B8 more</span><span class="arrow-up">\u25BE less</span>';
    toggle.style.cssText = 'color:var(--blue);font-size:11px;flex-shrink:0;padding:2px 6px;cursor:pointer;';
    toggle.querySelector('.arrow').style.display = expanded ? 'none' : '';
    toggle.querySelector('.arrow-up').style.display = expanded ? '' : 'none';
    entry.appendChild(toggle);
    entry.addEventListener('click', (event) => {
      if (event.target?.closest?.('.log-edit-message')) return;
      const nextExpanded = cnt.style.maxHeight !== 'none';
      cnt.style.maxHeight = nextExpanded ? 'none' : '1.5em';
      cnt.style.overflow = nextExpanded ? 'visible' : 'hidden';
      toggle.querySelector('.arrow').style.display = nextExpanded ? 'none' : '';
      toggle.querySelector('.arrow-up').style.display = nextExpanded ? '' : 'none';
      if (nextExpanded) view.expandedRows.add(index);
      else view.expandedRows.delete(index);
    });
  }

  return entry;
}

function handleSessionDetailLogScroll(view) {
  if (!view || view.pendingScrollFrame) return;
  view.pendingScrollFrame = requestAnimationFrame(() => {
    view.pendingScrollFrame = 0;
    if (sessionDetailLogView !== view || view.scroller?._sessionDetailView !== view) return;
    if (loadOlderSessionDetailRows(view)) return;
    loadNewerSessionDetailRows(view);
  });
}

function sessionDetailRowSignatures(row, fallbackSessionId = '') {
  if (!row || row.kind !== 'entry') return [];
  return sessionWindowTranscriptSignaturesForRecord(row.record || {}, fallbackSessionId);
}

function findSessionDetailRowIndexBySignatures(rows, signatures, fallbackSessionId = '') {
  if (!Array.isArray(rows) || !Array.isArray(signatures) || signatures.length === 0) return -1;
  const wanted = new Set(signatures);
  for (let i = 0; i < rows.length; i++) {
    const rowSignatures = sessionDetailRowSignatures(rows[i], fallbackSessionId);
    if (rowSignatures.some(signature => wanted.has(signature))) return i;
  }
  return -1;
}

function sessionDetailAnchorRow(view) {
  if (!view || !Array.isArray(view.rows)) return null;
  const start = Math.max(0, view.renderStart || 0);
  if (view.rows[start]?.kind === 'entry') return view.rows[start];
  for (let i = start; i < view.rows.length; i++) {
    if (view.rows[i]?.kind === 'entry') return view.rows[i];
  }
  for (let i = start - 1; i >= 0; i--) {
    if (view.rows[i]?.kind === 'entry') return view.rows[i];
  }
  return null;
}

function loadOlderSessionDetailRows(view, options = {}) {
  const scroller = view?.scroller;
  if (!scroller) return false;
  if (!options.forceRemote && scroller.scrollTop > SESSION_DETAIL_SCROLL_THRESHOLD_PX) return false;
  if (view.renderStart <= 0) {
    return loadOlderRemoteSessionDetailRows(view);
  }
  if (view.rows.length === 0) return false;
  const anchorIndex = view.renderStart;
  const anchor = scroller.querySelector(`[data-detail-row-index="${anchorIndex}"]`);
  const anchorTop = anchor ? anchor.offsetTop - scroller.scrollTop : 0;
  renderSessionDetailRange(view, Math.max(0, view.renderStart - SESSION_DETAIL_SCROLL_CHUNK));
  const nextAnchor = scroller.querySelector(`[data-detail-row-index="${anchorIndex}"]`);
  if (nextAnchor) scroller.scrollTop = Math.max(0, nextAnchor.offsetTop - anchorTop);
  return true;
}

function loadOlderRemoteSessionDetailRows(view) {
  if (!view || view.loadingOlder) return false;
  const before = Number(view.pageStart);
  if (!Number.isFinite(before) || before <= 0) {
    view.hasOlder = false;
    updateSessionDetailLogStatus(view);
    return false;
  }
  const sessionId = String(
    view.sessionId ||
    currentSessionDetail?.session_id ||
    currentSessionDetail?.resume_id ||
    ''
  ).trim();
  const source = normalizeAgentId(view.source || currentSessionDetail?.source || 'intendant') || 'intendant';
  if (!sessionId || !source) return false;

  const scroller = view.scroller;
  const anchorIndex = Math.max(0, view.renderStart || 0);
  const anchorSignatures = sessionDetailRowSignatures(sessionDetailAnchorRow(view), sessionId);
  const anchor = scroller?.querySelector(`[data-detail-row-index="${anchorIndex}"]`);
  const anchorTop = anchor ? anchor.offsetTop - scroller.scrollTop : 0;

  view.loadingOlder = true;
  updateSessionDetailLogStatus(view);
  fetchSessionDetailPayload(sessionId, {
    source,
    limit: SESSION_DETAIL_PAGE_LIMIT,
    before,
    cache: 'no-store',
  })
    .then(data => {
      if (sessionDetailLogView !== view || view.scroller?._sessionDetailView !== view || data?.error) return;
      updateSessionDetailViewPageState(view, {
        sessionId,
        source,
        pageStart: data.page_start ?? data.pageStart,
        pageEnd: data.page_end ?? data.pageEnd,
        totalEntries: data.total_entries ?? data.totalEntries,
        hasOlder: data.has_older ?? data.hasOlder,
      });
      const entries = Array.isArray(data.entries) ? data.entries : [];
      if (entries.length) {
        applySessionIdentitiesFromReplayEntries(entries);
        applySessionGoalsFromReplayEntries(entries);
        mergeSessionDetailEntries(view, entries, true);
      }
      rebuildSessionDetailViewRows(view);
      const nextAnchorIndex = findSessionDetailRowIndexBySignatures(
        view.rows,
        anchorSignatures,
        sessionId
      );
      if (nextAnchorIndex >= 0) {
        renderSessionDetailRange(view, Math.max(0, nextAnchorIndex - SESSION_DETAIL_SCROLL_CHUNK));
        const nextAnchor = scroller?.querySelector(`[data-detail-row-index="${nextAnchorIndex}"]`);
        if (nextAnchor) scroller.scrollTop = Math.max(0, nextAnchor.offsetTop - anchorTop);
      } else {
        renderSessionDetailRange(view, 0);
      }
      updateSessionDetailLogsBadge(view);
    })
    .catch(err => {
      console.warn('Failed to load older session detail page', sessionId, err);
    })
    .finally(() => {
      if (sessionDetailLogView === view) {
        view.loadingOlder = false;
        updateSessionDetailLogStatus(view);
      }
    });
  return true;
}

function loadNewerSessionDetailRows(view) {
  const scroller = view?.scroller;
  if (!scroller || view.renderEnd >= view.rows.length || view.rows.length === 0) return false;
  const bottomDistance = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
  if (bottomDistance > SESSION_DETAIL_SCROLL_THRESHOLD_PX) return false;
  const anchorIndex = Math.max(view.renderStart, view.renderEnd - 1);
  const anchor = scroller.querySelector(`[data-detail-row-index="${anchorIndex}"]`);
  const anchorBottom = anchor
    ? anchor.offsetTop + anchor.offsetHeight - scroller.scrollTop
    : scroller.clientHeight;
  const maxStart = Math.max(0, view.rows.length - SESSION_DETAIL_RENDER_LIMIT);
  const nextStart = Math.min(maxStart, view.renderStart + SESSION_DETAIL_SCROLL_CHUNK);
  renderSessionDetailRange(view, nextStart);
  const nextAnchor = scroller.querySelector(`[data-detail-row-index="${anchorIndex}"]`);
  if (nextAnchor) {
    scroller.scrollTop = Math.max(0, nextAnchor.offsetTop + nextAnchor.offsetHeight - anchorBottom);
  }
  return true;
}


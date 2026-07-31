// ── ui2-update-lane — the self-update panel (Access → Daemons) ──────
// Sits beside the daemons list's provenance rows and renders the
// daemon's `update_lane` block from GET /api/daemon/handover: install
// flavor, the bounded behind-origin-main / behind-latest-release check,
// and the produce job's live phase + log tail. The buttons are the
// owner's consent surface: POST /api/daemon/update-lane/{check,produce}
// (HTTP-only by design — a tunnel-only remote surface watches progress
// here but cannot click a build onto the box; the catch renders that
// honestly). Producing lands a newer binary at the watched path; the
// EXISTING update chip + one-click swap lane takes over from there.
// textContent construction throughout: repo paths, versions, and child
// process log lines are observed strings, never markup.
(() => {
  const UPDATE_LANE_POLL_MS = 30000;
  const UPDATE_LANE_BUSY_POLL_MS = 3000;
  const action = { inFlight: false, note: '' };
  let lastBlock = null;
  let pollTimer = null;

  function updateLaneMount() {
    return document.getElementById('update-lane-card');
  }

  async function updateLanePost(path) {
    if (action.inFlight) return;
    action.inFlight = true;
    action.note = '';
    render(lastBlock);
    try {
      const resp = await authedFetch(path, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: '{}',
      });
      let body = null;
      try { body = await resp.json(); } catch (_) { /* non-JSON error body */ }
      if (body && body.update_lane) lastBlock = body.update_lane;
      if (!resp.ok) {
        action.note = (body && body.detail)
          ? body.detail
          : `This surface cannot reach the update lane (HTTP ${resp.status}).`;
      }
    } catch (err) {
      action.note = `This surface cannot reach the update lane: ${(err && err.message) || err}`;
    } finally {
      action.inFlight = false;
      render(lastBlock);
      schedulePoll(true);
    }
  }

  function actionButton(label, path, disabled) {
    const btn = document.createElement('button');
    btn.className = 'update-lane-btn';
    btn.textContent = label;
    btn.disabled = Boolean(disabled) || action.inFlight;
    btn.addEventListener('click', () => updateLanePost(path));
    return btn;
  }

  function line(parent, cls, text) {
    const el = document.createElement('div');
    el.className = cls;
    el.textContent = text;
    parent.appendChild(el);
    return el;
  }

  function describeCheck(block, body) {
    const check = block.check || {};
    const running = block.running || {};
    if (check.error) {
      line(body, 'update-lane-error', `Check failed: ${check.error}`);
      return { behind: false };
    }
    if (block.flavor === 'source') {
      if (typeof check.behind !== 'number') {
        line(body, 'update-lane-note', check.in_flight
          ? 'Checking origin/main…'
          : 'Not checked yet.');
        return { behind: false };
      }
      const capped = check.behind_capped ? '+' : '';
      if (check.behind > 0) {
        line(body, 'update-lane-status',
          `Behind origin/main by ${check.behind}${capped} commit${check.behind === 1 ? '' : 's'} `
          + `(tip ${String(check.tip_sha || '').slice(0, 10)} — running ${running.git_sha || '?'}).`);
      } else {
        line(body, 'update-lane-note',
          `Up to date with origin/main (running ${running.git_sha || '?'}).`);
      }
      if (check.dirty) {
        line(body, 'update-lane-note',
          'The checkout has local changes — the update will refuse to pull over them.');
      }
      return { behind: check.behind > 0 };
    }
    if (block.flavor === 'consumer-app') {
      if (!check.latest_tag) {
        line(body, 'update-lane-note', check.in_flight
          ? 'Checking the latest logged release…'
          : 'Not checked yet.');
        return { behind: false };
      }
      const behind = Number(check.behind) > 0;
      line(body, behind ? 'update-lane-status' : 'update-lane-note',
        `Latest logged release: ${check.latest_tag} (${check.latest_version}) — running ${running.version || '?'}.`
        + (check.compare_error ? ` ${check.compare_error}.` : ''));
      return { behind };
    }
    return { behind: false };
  }

  function describeJob(block, body) {
    const job = block.job;
    if (!job) return false;
    const runningJob = !('ok' in job);
    if (runningJob) {
      line(body, 'update-lane-status',
        `${job.lane === 'source' ? 'Building from main' : 'Downloading release'} — ${job.phase}…`);
    } else if (job.ok) {
      line(body, 'update-lane-done', job.detail || 'Update produced.');
    } else {
      line(body, 'update-lane-error', `Update failed: ${job.error || 'see the daemon log'}. The running daemon is untouched.`);
    }
    const tail = Array.isArray(job.log_tail) ? job.log_tail.slice(-8) : [];
    if (tail.length && (runningJob || !job.ok)) {
      const log = document.createElement('pre');
      log.className = 'update-lane-log';
      log.textContent = tail.join('\n');
      body.appendChild(log);
    }
    return runningJob;
  }

  function render(block) {
    const mount = updateLaneMount();
    if (!mount) return;
    lastBlock = block;
    mount.textContent = '';
    if (!block) return;
    const card = document.createElement('div');
    card.className = 'update-lane-card';
    const head = document.createElement('div');
    head.className = 'update-lane-head';
    const title = document.createElement('strong');
    title.textContent = 'Daemon update';
    head.appendChild(title);
    const flavorChip = document.createElement('span');
    flavorChip.className = 'update-lane-flavor';
    flavorChip.textContent = block.flavor === 'source'
      ? 'source install'
      : block.flavor === 'consumer-app' ? 'release install' : 'unmanaged';
    head.appendChild(flavorChip);
    card.appendChild(head);

    const body = document.createElement('div');
    body.className = 'update-lane-body';
    if (block.flavor === 'source' && block.repo_root) {
      line(body, 'update-lane-note', `Checkout: ${block.repo_root}${block.app_bundle ? ' (app bundle)' : ''}`);
    } else if (block.flavor === 'consumer-app' && block.app_root) {
      line(body, 'update-lane-note', `Installed app: ${block.app_root}`);
    }
    if (block.unavailable) {
      line(body, 'update-lane-note', block.unavailable);
    }

    const verdict = describeCheck(block, body);
    const jobRunning = describeJob(block, body);

    if (action.note) line(body, 'update-lane-note', action.note);

    const actions = document.createElement('div');
    actions.className = 'update-lane-actions';
    if (!block.unavailable) {
      if (block.flavor === 'source') {
        actions.appendChild(actionButton(
          jobRunning ? 'Updating…' : 'Update from main',
          '/api/daemon/update-lane/produce',
          jobRunning || !verdict.behind,
        ));
      } else if (block.flavor === 'consumer-app') {
        actions.appendChild(actionButton(
          jobRunning ? 'Updating…' : 'Download & verify release',
          '/api/daemon/update-lane/produce',
          jobRunning || !verdict.behind,
        ));
      }
      actions.appendChild(actionButton('Check now', '/api/daemon/update-lane/check',
        jobRunning || Boolean(block.check && block.check.in_flight)));
    }
    card.appendChild(body);
    if (actions.childElementCount) card.appendChild(actions);
    mount.appendChild(card);
  }

  async function poll() {
    try {
      if (typeof daemonApi === 'undefined') return;
      if (!daemonApi.availability('api_daemon_handover').ok) return;
      const resp = await daemonApi.request('api_daemon_handover', {});
      if (resp && resp.ok && resp.body) render(resp.body.update_lane || null);
    } catch (_) { /* transient — keep the last render */ }
  }

  function schedulePoll(immediate) {
    if (pollTimer) clearTimeout(pollTimer);
    const busy = Boolean(lastBlock && (
      (lastBlock.job && !('ok' in lastBlock.job)) ||
      (lastBlock.check && lastBlock.check.in_flight)));
    pollTimer = setTimeout(async () => {
      await poll();
      schedulePoll(false);
    }, immediate ? 400 : (busy ? UPDATE_LANE_BUSY_POLL_MS : UPDATE_LANE_POLL_MS));
  }

  function start() {
    poll().then(() => schedulePoll(false));
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
})();

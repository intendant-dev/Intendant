// ── ui2-update-lane — the update panel (Access → Daemons) ───────────
// The check-for-updates front door over the daemon's `update_lane`
// block (GET /api/daemon/handover): running provenance, the two-channel
// vocabulary — Releases (default; logged, PGP-verified builds) and
// Dev — build from main (power-user lane behind the Advanced fold) —
// each channel's bounded check as data (release compare; behind-count +
// shortlog), and the produce job's live phase + log tail with real
// errors verbatim. Which channel an install can check/produce comes
// from the payload's `channels` catalog, never hardcoded here. The
// buttons are the owner's consent surface: POST
// /api/daemon/update-lane/{check,produce} with {"channel": …}
// (HTTP-only by design — a tunnel-only remote surface watches progress
// here but cannot click a build onto the box; the catch renders that
// honestly). Producing lands a newer binary at the watched path; the
// swap step below is the EXISTING chip consumer (ui2-handover renders
// it into this panel's mount — one swap implementation, two surfaces).
// Nothing here updates anything without an explicit click.
// textContent construction throughout: repo paths, versions, commit
// subjects, and child process log lines are observed strings, never
// markup.
(() => {
  const UPDATE_LANE_POLL_MS = 30000;
  const UPDATE_LANE_BUSY_POLL_MS = 3000;
  const ADVANCED_OPEN_KEY = 'update-lane-advanced-open';
  const action = { inFlight: false, note: '' };
  let lastBlock = null;
  let pollTimer = null;

  function updateLaneMount() {
    return document.getElementById('update-lane-card');
  }

  function advancedOpenStored() {
    try { return localStorage.getItem(ADVANCED_OPEN_KEY) === '1'; } catch (_) { return false; }
  }
  function advancedOpenStore(open) {
    try {
      if (open) localStorage.setItem(ADVANCED_OPEN_KEY, '1');
      else localStorage.removeItem(ADVANCED_OPEN_KEY);
    } catch (_) { /* storage unavailable — the fold lives for this page only */ }
  }

  async function updateLanePost(path, channel) {
    if (action.inFlight) return;
    action.inFlight = true;
    action.note = '';
    render(lastBlock);
    try {
      const resp = await authedFetch(path, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ channel }),
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

  function actionButton(label, path, channel, disabled) {
    const btn = document.createElement('button');
    btn.className = 'update-lane-btn';
    btn.textContent = label;
    btn.disabled = Boolean(disabled) || action.inFlight;
    btn.addEventListener('click', () => updateLanePost(path, channel));
    return btn;
  }

  function line(parent, cls, text) {
    const el = document.createElement('div');
    el.className = cls;
    el.textContent = text;
    parent.appendChild(el);
    return el;
  }

  // ── Channel sections (availability + data come from the payload) ──

  function channelInfo(block, name) {
    return (block.channels && block.channels[name]) || { check: false, produce: false };
  }
  function channelCheck(block, name) {
    return (block.checks && block.checks[name]) || {};
  }

  // The releases check as data: latest logged release vs the running
  // package version.
  function describeReleasesCheck(check, running, body) {
    if (check.error) {
      line(body, 'update-lane-error', `Release check failed: ${check.error}`);
      return { behind: false };
    }
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

  // The dev check as data: behind-count + the bounded shortlog.
  function describeDevCheck(check, running, body) {
    if (check.error) {
      line(body, 'update-lane-error', `Compare failed: ${check.error}`);
      return { behind: false };
    }
    if (typeof check.behind !== 'number') {
      line(body, 'update-lane-note', check.in_flight
        ? 'Fetching origin and comparing…'
        : 'Not checked yet.');
      return { behind: false };
    }
    const capped = check.behind_capped ? '+' : '';
    if (check.behind > 0) {
      line(body, 'update-lane-status',
        `Behind origin/main by ${check.behind}${capped} commit${check.behind === 1 ? '' : 's'} `
        + `(tip ${String(check.tip_sha || '').slice(0, 10)} — running ${running.git_sha || '?'}).`);
      const shortlog = Array.isArray(check.shortlog) ? check.shortlog : [];
      if (shortlog.length) {
        const log = document.createElement('pre');
        log.className = 'update-lane-shortlog';
        log.textContent = shortlog.join('\n')
          + (check.behind > shortlog.length ? `\n… and ${check.behind}${capped === '+' ? '+' : ''} total` : '');
        body.appendChild(log);
      }
    } else {
      line(body, 'update-lane-note',
        `Up to date with origin/main (running ${running.git_sha || '?'}).`);
    }
    if (check.dirty) {
      line(body, 'update-lane-note',
        'The checkout has local changes to tracked files — a build will refuse to pull over them.');
    }
    return { behind: check.behind > 0 };
  }

  // One channel's section: data, then the consent buttons this install
  // actually supports — an unavailable produce renders its reason
  // instead of a button that cannot mean what it says.
  function channelSection(block, name, title, jobRunning, describe, checkLabel, produceLabel) {
    const info = channelInfo(block, name);
    const check = channelCheck(block, name);
    const section = document.createElement('div');
    section.className = 'update-lane-channel';
    line(section, 'update-lane-channel-title', title);
    const body = document.createElement('div');
    body.className = 'update-lane-body';
    section.appendChild(body);
    const verdict = info.check
      ? describe(check, block.running || {}, body)
      : { behind: false };
    if (!info.produce && info.reason) {
      line(body, 'update-lane-note', info.reason);
    }
    const actions = document.createElement('div');
    actions.className = 'update-lane-actions';
    if (info.check) {
      actions.appendChild(actionButton(checkLabel, '/api/daemon/update-lane/check', name,
        jobRunning || Boolean(check.in_flight)));
    }
    if (info.produce) {
      actions.appendChild(actionButton(
        jobRunning ? 'Working…' : produceLabel,
        '/api/daemon/update-lane/produce', name,
        jobRunning || !verdict.behind,
      ));
    }
    if (actions.childElementCount) section.appendChild(actions);
    return section;
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
      : (block.flavor === 'consumer-app' || block.flavor === 'consumer-binary')
        ? 'release install' : 'unmanaged';
    head.appendChild(flavorChip);
    card.appendChild(head);

    const body = document.createElement('div');
    body.className = 'update-lane-body';
    const running = block.running || {};
    line(body, 'update-lane-note',
      `Running: commit ${running.git_sha || '?'} · v${running.version || '?'} · built ${running.built_at || '?'}`);
    if (block.flavor === 'source' && block.repo_root) {
      line(body, 'update-lane-note', `Checkout: ${block.repo_root}${block.app_bundle ? ' (app bundle)' : ''}`);
    } else if (block.flavor === 'consumer-app' && block.app_root) {
      line(body, 'update-lane-note', `Installed app: ${block.app_root}`);
    } else if (block.flavor === 'consumer-binary' && block.install_dir) {
      line(body, 'update-lane-note', `Installed at: ${block.install_dir}`);
    }
    if (block.unavailable) {
      line(body, 'update-lane-note', block.unavailable);
    }

    const jobRunning = describeJob(block, body);
    if (action.note) line(body, 'update-lane-note', action.note);
    card.appendChild(body);

    // Releases — the default channel, for everyone.
    card.appendChild(channelSection(
      block, 'releases', 'Releases — verified, signed builds (default)', jobRunning,
      describeReleasesCheck, 'Check latest release', 'Download & install release',
    ));

    // Dev — build from main, one obvious click deeper.
    const advanced = document.createElement('details');
    advanced.className = 'update-lane-advanced';
    if (advancedOpenStored()) advanced.open = true;
    advanced.addEventListener('toggle', () => advancedOpenStore(advanced.open));
    const summary = document.createElement('summary');
    summary.textContent = 'Advanced';
    advanced.appendChild(summary);
    advanced.appendChild(channelSection(
      block, 'dev', 'Dev — build from main', jobRunning,
      describeDevCheck, 'Fetch & compare', 'Pull & build from main',
    ));
    card.appendChild(advanced);

    // The swap step: rendered by the EXISTING chip consumer
    // (ui2-handover) into this mount — supervisor-claimed one-click
    // when the daemon is app-supervised, its honest reach otherwise.
    // Empty while no newer build sits on disk.
    const swap = document.createElement('div');
    swap.className = 'update-lane-swap';
    card.appendChild(swap);
    if (window.intendantHandoverUpdate
        && typeof window.intendantHandoverUpdate.renderSwapSection === 'function') {
      window.intendantHandoverUpdate.renderSwapSection(swap);
    }

    // The pinned honesty sentence — byte-identical on every update surface
    // (static_assets.rs pins the served copy).
    line(card, 'update-lane-footer',
      'The update installs alongside the current version — running sessions finish uninterrupted, and the old version may keep running until they are done.');
    line(card, 'update-lane-footer',
      'Updates happen only on your click here — nothing installs or restarts automatically.');
    mount.appendChild(card);
  }

  function anyCheckInFlight(block) {
    if (!block || !block.checks) return false;
    return ['releases', 'dev'].some((name) =>
      block.checks[name] && block.checks[name].in_flight);
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
      anyCheckInFlight(lastBlock)));
    pollTimer = setTimeout(async () => {
      await poll();
      schedulePoll(false);
    }, immediate ? 400 : (busy ? UPDATE_LANE_BUSY_POLL_MS : UPDATE_LANE_POLL_MS));
  }

  // The palette's 'Check for updates' seam (production, not QA): the
  // SAME consent POST the panel's check button sends, for the channel
  // this install natively follows per the served catalog (source → dev,
  // else releases; a channel the payload marks uncheckable falls back
  // to the other) — never a new check path. Routing to the panel is the
  // caller's job; the click lands here so the panel's in-flight latch,
  // notes, and poll cadence stay the one story. A missing block (first
  // poll not landed, or a daemon without the lane) no-ops honestly.
  function nativeCheckChannel() {
    const block = lastBlock;
    if (!block) return null;
    const native = block.flavor === 'source' ? 'dev' : 'releases';
    if (channelInfo(block, native).check) return native;
    const other = native === 'dev' ? 'releases' : 'dev';
    return channelInfo(block, other).check ? other : null;
  }
  window.intendantUpdateLane = {
    check: () => {
      const channel = nativeCheckChannel();
      if (channel) updateLanePost('/api/daemon/update-lane/check', channel);
    },
    // The release chip's one-click rides this SAME consent POST — the
    // emission-shape law: every lane POST goes through the one
    // updateLanePost above, so the chip adds no second produce path
    // and inherits the in-flight latch, notes, and poll cadence.
    produce: (channel) => updateLanePost('/api/daemon/update-lane/produce', channel),
  };

  function start() {
    poll().then(() => schedulePoll(false));
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
})();

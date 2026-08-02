// ── ui2-handover — drain banner + successor chip (HS5) + update chip
// (HS6) ──────────────────────────────────────────────────────────────
// Polls GET /api/daemon/handover (tunnel twin api_daemon_handover): the
// scheduler-lease status block. Renders:
//   - THIS daemon draining: a persistent banner that NAMES the wait set
//     (`holdouts`: each holding session with its state — a limit-parked
//     holdout leads with its parked-until instant, the decisive fact)
//     and carries a prominent doorway to the successor once the lease
//     sidecar names it.
//   - other co-homed daemons draining (all of them, not just the
//     first): a predecessor section each, with its remaining count, its
//     named holdout rows off the presence record, and a doorway to it.
//     Both banner kinds render in a strip DOCKED under the oversight
//     bar — its measured height rides --ui2-handover-h (the
//     --ui2-composer-h reservation pattern), so neither form ever
//     covers the tab chrome — and each collapses to a one-line pill
//     under the update chip's standing-fact pattern, persisted per
//     (kind, boot id): an unseen fact announces itself expanded once,
//     a collapse then holds across renders and reloads, and there is
//     deliberately no dismiss while the fact is true.
//   - a newer/changed binary on disk (the daemon's update watch): the
//     bottom-corner update chip with both builds' provenance and the
//     keychain/TCC honesty line where it applies. Collapsible to a
//     per-sha-persistent pill (never dismissable — a standing fact),
//     and one-click on every surface of an app-supervised daemon: the
//     webview bridge in the app, the daemon swap relay elsewhere.
//     Suppressed while draining — a drain in motion outranks the
//     waiting update.
// The payload's boot_id also feeds the "daemon updated — reload" nudge
// (maybeNudgeDaemonBoot), beside the config-lane chokepoint.
// Elements are removed when nothing applies; transient poll failures
// keep the last honest render rather than flapping.
(() => {
  const HANDOVER_POLL_MS = 30000;
  let bannerEl = null;
  let updateEl = null;
  let releaseEl = null;

  // Both standing-fact chips share one bottom-corner dock — a fixed
  // flex column — so "a newer release is logged" and "a newer build
  // sits on disk" stack instead of covering each other: distinct facts,
  // both may stand, never conflated. The dock is click-transparent;
  // the chips are the interactive surfaces.
  let chipDockEl = null;
  function chipDock() {
    if (!chipDockEl) {
      chipDockEl = document.createElement('div');
      chipDockEl.id = 'handover-chip-dock';
      document.body.appendChild(chipDockEl);
    }
    return chipDockEl;
  }
  function chipDockPrune() {
    if (chipDockEl && !updateEl && !releaseEl) { chipDockEl.remove(); chipDockEl = null; }
  }

  function updateChip() {
    if (!updateEl) {
      updateEl = document.createElement('div');
      updateEl.id = 'handover-update-chip';
      updateEl.className = 'handover-update-chip';
      chipDock().appendChild(updateEl);
    }
    return updateEl;
  }

  function updateChipClear() {
    if (updateEl) { updateEl.remove(); updateEl = null; }
    chipDockPrune();
  }

  // The release-availability chip sits ABOVE the on-disk chip: the
  // logged fact leads, the produced artifact follows.
  function releaseChip() {
    if (!releaseEl) {
      releaseEl = document.createElement('div');
      releaseEl.id = 'handover-release-chip';
      releaseEl.className = 'handover-update-chip handover-release-chip';
      chipDock().insertBefore(releaseEl, chipDockEl.firstChild);
    }
    return releaseEl;
  }

  function releaseChipClear() {
    if (releaseEl) { releaseEl.remove(); releaseEl = null; }
    chipDockPrune();
  }

  // Collapsed-state persistence, PER SHA: a never-seen on-disk build
  // renders the full card once (the standing fact announces itself,
  // mirroring its one-per-sha notification); the owner's collapse then
  // persists for that sha — across polls and reloads — as a small pill
  // that keeps the fact discoverable without covering content. There is
  // deliberately NO dismiss: some rendering of the fact always stands
  // (a stale binary must not be forgettable), only its size is the
  // owner's choice. A new sha starts fresh.
  function updateChipStateKey(sha) {
    return 'handover-update-chip:' + (sha || 'unknown');
  }
  function updateChipStoredState(sha) {
    try { return localStorage.getItem(updateChipStateKey(sha)); } catch (_) { return null; }
  }
  function updateChipStoreState(sha, state) {
    try {
      // One live sha at a time — stale per-sha keys go as we write.
      for (let i = localStorage.length - 1; i >= 0; i--) {
        const key = localStorage.key(i);
        if (key && key.indexOf('handover-update-chip:') === 0 && key !== updateChipStateKey(sha)) {
          localStorage.removeItem(key);
        }
      }
      localStorage.setItem(updateChipStateKey(sha), state);
    } catch (_) { /* storage unavailable — the choice lives for this page only */ }
  }

  // Collapsed-state persistence for the release chip, PER TAG — the
  // update chip's per-sha pattern verbatim: a never-seen release
  // announces itself expanded once; the owner's collapse then persists
  // for that tag — across polls and reloads — as a pill that keeps the
  // fact discoverable without covering content. There is deliberately
  // NO dismiss: a standing fact only changes size. A new tag starts
  // fresh.
  function releaseChipStateKey(tag) {
    return 'handover-release-chip:' + (tag || 'unknown');
  }
  function releaseChipStoredState(tag) {
    try { return localStorage.getItem(releaseChipStateKey(tag)); } catch (_) { return null; }
  }
  function releaseChipStoreState(tag, state) {
    try {
      // One live tag at a time — stale per-tag keys go as we write.
      for (let i = localStorage.length - 1; i >= 0; i--) {
        const key = localStorage.key(i);
        if (key && key.indexOf('handover-release-chip:') === 0 && key !== releaseChipStateKey(tag)) {
          localStorage.removeItem(key);
        }
      }
      localStorage.setItem(releaseChipStateKey(tag), state);
    } catch (_) { /* storage unavailable — the choice lives for this page only */ }
  }

  // The one action's client state (the button must never double-fire and
  // its feedback must survive the 30s poll cadence). `relayRequestedMs`
  // bridges the gap between a relay click and the next poll's payload
  // (which carries `swap_pending_ms` server-side, surviving reloads).
  const updateAction = { inFlight: false, note: '', relayRequestedMs: 0 };
  let lastHandoverBody = null;

  // The release chip's click feedback — the produce job's live truth
  // arrives on the payload; the note only bridges the gap after a
  // click, and the payload's job block supersedes it.
  const releaseAction = { note: '' };

  // While a consumer produce job runs, chase the payload faster than
  // the 30s cadence — bounded: re-armed only while the job is live.
  let quickPollTimer = null;
  function armQuickHandoverPoll() {
    if (quickPollTimer) return;
    quickPollTimer = setTimeout(async () => {
      quickPollTimer = null;
      await handoverPoll();
    }, 3000);
  }

  // A one-click swap is in motion: parked on the daemon awaiting the
  // app supervisor's claim (payload fact), or just clicked here.
  function relayPendingNow(body) {
    const pendingMs = body && Number(body.swap_pending_ms);
    if (Number.isFinite(pendingMs) && pendingMs > 0) return true;
    return updateAction.relayRequestedMs > 0
      && (Date.now() - updateAction.relayRequestedMs) < 90000;
  }

  // The relay lane (any surface beyond the app's own webview): park the
  // request on the daemon; the app supervisor's health tick claims it
  // and performs the swap. Progress reaches this surface through the
  // payload (`swap_pending_ms`, then the drain banner); failures come
  // back as `swap_result` and the notification lane.
  async function relayUpdateSwap(body) {
    updateAction.note = 'Asking the app to start the new daemon — the drain banner appears here when it takes over; in-flight sessions finish on this daemon.';
    handoverUpdateRender(body);
    try {
      const resp = await authedFetch('/api/daemon/update-swap', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ requested_by: 'dashboard update chip' }),
      });
      if (!resp.ok) {
        let detail = `HTTP ${resp.status}`;
        try {
          const err = await resp.json();
          if (err && err.detail) detail = err.detail;
        } catch (_) { /* non-JSON error body */ }
        updateAction.relayRequestedMs = 0;
        updateAction.note = `The swap request was refused: ${detail}`;
      }
    } catch (err) {
      updateAction.relayRequestedMs = 0;
      updateAction.note = `This surface could not reach the daemon: ${(err && err.message) || err}`;
    } finally {
      if (lastHandoverBody) handoverUpdateRender(lastHandoverBody);
    }
  }
  // The app supervisor reports a failed swap here (the success path
  // reloads the tab against the promoted successor instead).
  window.__intendantAppSwapFailed = (detail) => {
    updateAction.inFlight = false;
    updateAction.note = `Update failed: ${detail || 'see the app log'}. The running daemon is untouched.`;
    if (lastHandoverBody) handoverUpdateRender(lastHandoverBody);
  };

  // The update panel (ui2-update-lane) composes this SAME swap consumer
  // instead of duplicating it: it hands over a mount element, and every
  // chip-state render refills it — one implementation of the
  // supervised / relay / hand-off logic, two surfaces. The mount is
  // replaced wholesale per panel render; a disconnected mount is
  // silently dropped.
  let panelSwapMount = null;
  function fillPanelSwapSection() {
    const mount = panelSwapMount;
    if (!mount) return;
    if (!mount.isConnected) { panelSwapMount = null; return; }
    mount.textContent = '';
    const body = lastHandoverBody;
    const update = body && body.update;
    if (!update || body.draining) return;
    const disk = update.on_disk;
    const running = update.running || {};
    const head = document.createElement('div');
    head.className = 'update-lane-swap-head';
    head.textContent = disk
      ? `New build on disk: commit ${disk.git_sha || '?'} (${disk.version || '?'}, built ${disk.built_at || '?'}) — running ${running.git_sha || '?'}.`
      : `The binary on disk changed but its provenance is unreadable${update.probe_error ? ` (${update.probe_error})` : ''}.`;
    mount.appendChild(head);
    if (update.honesty) {
      const honesty = document.createElement('div');
      honesty.className = 'handover-update-honesty';
      honesty.textContent = update.honesty;
      mount.appendChild(honesty);
    }
    // The pinned honesty sentence — byte-identical on every update surface
    // (static_assets.rs pins the served copy).
    const bounds = document.createElement('div');
    bounds.className = 'update-lane-note';
    bounds.textContent = 'The update installs alongside the current version — running sessions finish uninterrupted, and the old version may keep running until they are done.';
    mount.appendChild(bounds);
    mount.appendChild(handoverUpdateActions(body, disk));
  }

  // The one-click's behavior, NAMED: the chip button and the palette's
  // dynamic entry both invoke this — the webview bridge or the daemon
  // relay, one implementation, never a second swap path.
  function performOneClickSwap(body) {
    if (updateAction.inFlight || relayPendingNow(lastHandoverBody)) return;
    if (window.__intendantAppSupervisor === true) {
      updateAction.inFlight = true;
      updateAction.note = 'Starting the new daemon — this dashboard reloads when it takes over; in-flight sessions finish on the old one.';
      try {
        window.webkit.messageHandlers.updateSwap.postMessage(null);
      } catch (_) {
        updateAction.inFlight = false;
        updateAction.note = 'Could not reach the app supervisor.';
      }
      handoverUpdateRender(body);
    } else {
      updateAction.relayRequestedMs = Date.now();
      relayUpdateSwap(body);
    }
  }

  // The successor-exec lane's live in-flight fact (payload-side, so it
  // survives reloads and other tabs' clicks).
  function successorExecBusy(body) {
    const exec = body && body.successor_exec;
    return Boolean(exec && exec.in_flight === true);
  }

  // The ruled unsupervised one-click (successor exec, 2026-07-31): ask
  // THIS daemon to spawn the verified on-disk build as its successor,
  // confirm readiness, then drain toward it. The click names the build
  // it offers (expected_git_sha) — the daemon refuses if the artifact
  // changed under the button or the swap would be build-neutral. One
  // named emitter for every surface; the daemon's phase/verdict comes
  // back through the payload's successor_exec block.
  async function performSuccessorExec(body, disk) {
    if (updateAction.inFlight || successorExecBusy(body)) return;
    updateAction.inFlight = true;
    updateAction.note = 'Starting the new daemon from the built binary — this daemon drains toward it once it is ready; in-flight sessions finish here.';
    handoverUpdateRender(body);
    try {
      const resp = await authedFetch('/api/daemon/successor-exec', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          expected_git_sha: disk.git_sha,
          requested_by: 'dashboard update chip',
        }),
      });
      if (!resp.ok) {
        let detail = `HTTP ${resp.status}`;
        try {
          const err = await resp.json();
          if (err && err.detail) detail = err.detail;
        } catch (_) { /* non-JSON error body */ }
        updateAction.note = `The spawn was refused: ${detail}`;
      }
    } catch (err) {
      updateAction.note = `This surface could not reach the daemon: ${(err && err.message) || err}`;
    } finally {
      updateAction.inFlight = false;
      if (lastHandoverBody) handoverUpdateRender(lastHandoverBody);
    }
  }

  // Why no spawn button renders, said honestly for the arm we are in —
  // the pre-ruling copy survives only where it is still true (no
  // successor-exec lane on this daemon).
  function successorExecReachCopy(body, disk) {
    const exec = body && body.successor_exec;
    if (!exec || exec.available !== true) {
      return 'No other daemon is running to hand off to — this daemon cannot launch one itself. The macOS app (and a service-managed install) can do this in one click.';
    }
    if (!disk || !disk.git_sha) {
      return 'The changed binary on disk has no readable provenance — it cannot be started as a successor until a verifiable build lands.';
    }
    if (body && body.held === false) {
      return 'This daemon is not the scheduler-lease holder — hand-offs happen from the holder’s own dashboard (the handover status names it).';
    }
    return 'No successor lane is available right now.';
  }

  // The unsupervised arm's hand-off, equally named for both surfaces:
  // ask THIS daemon to drain toward an already-running newer daemon.
  async function performTakeoverHandoff(successor, body) {
    if (updateAction.inFlight) return;
    updateAction.inFlight = true;
    updateAction.note = `Asking this daemon to drain — :${successor.port} takes over.`;
    handoverUpdateRender(body);
    try {
      const resp = await authedFetch('/api/daemon/takeover', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ requested_by: 'dashboard update chip' }),
      });
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      updateAction.note = 'Draining — in-flight sessions finish here, then this daemon exits.';
    } catch (err) {
      updateAction.note = `Hand-off failed from this surface: ${(err && err.message) || err}`;
    } finally {
      updateAction.inFlight = false;
      if (lastHandoverBody) handoverUpdateRender(lastHandoverBody);
    }
  }

  // The co-homed daemon a hand-off would drain toward: live, not this
  // boot, not already on its way out. Prefer one whose build matches the
  // on-disk update.
  function handoverSuccessorCandidate(body, disk) {
    const daemons = Array.isArray(body && body.daemons) ? body.daemons : [];
    const live = daemons.filter((d) =>
      d && d.live && d.boot_id !== body.boot_id && d.state !== 'draining' && d.state !== 'exited');
    if (disk && disk.git_sha) {
      const matching = live.find((d) => d.version && d.version.git_sha === disk.git_sha);
      if (matching) return matching;
    }
    return live[0] || null;
  }

  // The chip's action row. With a live app supervisor the chip offers
  // the real one-click on EVERY surface: inside the app's own webview
  // (__intendantAppSupervisor) the bridge message drives it directly;
  // anywhere else — a browser against an app-supervised daemon
  // (body.app_supervised, the live parent-pid-verified fact) — the
  // daemon relay carries the click to the supervisor. On a genuinely
  // CLI-launched daemon the chip is honest about its reach: it can ask
  // THIS daemon to drain toward an already-running newer daemon; it
  // cannot launch one, and it says so instead of pointing at a terminal.
  function handoverUpdateActions(body, disk) {
    const actions = document.createElement('div');
    actions.className = 'handover-update-actions';
    const supervised = window.__intendantAppSupervisor === true
      || (body && body.app_supervised === true);
    if (supervised) {
      const busy = updateAction.inFlight || relayPendingNow(body);
      const btn = document.createElement('button');
      btn.textContent = busy ? 'Updating…' : 'Update now';
      btn.disabled = busy;
      btn.addEventListener('click', () => performOneClickSwap(body));
      actions.appendChild(btn);
    } else {
      const successor = handoverSuccessorCandidate(body, disk);
      const exec = body && body.successor_exec;
      if (successor) {
        const matches = disk && successor.version && successor.version.git_sha === disk.git_sha;
        const btn = document.createElement('button');
        btn.textContent = `Hand off to :${successor.port}${matches ? ' (runs the on-disk build)' : ''}`;
        btn.disabled = updateAction.inFlight;
        btn.addEventListener('click', () => performTakeoverHandoff(successor, body));
        actions.appendChild(btn);
      } else if (exec && exec.available === true && disk && disk.git_sha && body.held !== false) {
        // The ruled spawn (successor exec): no successor is running yet
        // — start one from the verified build, then drain toward it.
        const busy = updateAction.inFlight || successorExecBusy(body);
        const btn = document.createElement('button');
        btn.textContent = busy
          ? 'Starting the new daemon…'
          : `Start the new daemon & hand off (${String(disk.git_sha).slice(0, 10)})`;
        btn.disabled = busy;
        btn.addEventListener('click', () => performSuccessorExec(body, disk));
        actions.appendChild(btn);
      } else {
        const reach = document.createElement('div');
        reach.className = 'handover-update-reach';
        reach.textContent = successorExecReachCopy(body, disk);
        actions.appendChild(reach);
      }
    }
    if (updateAction.note) {
      const note = document.createElement('div');
      note.className = 'handover-update-note';
      note.textContent = updateAction.note;
      actions.appendChild(note);
    } else {
      // The successor-exec flow's own story (another tab's click, a
      // finished attempt): the payload block renders when no local
      // click feedback outranks it. A drain in motion suppresses the
      // whole chip, so the success arm shows only briefly.
      const exec = body && body.successor_exec;
      const execText = !exec || !exec.phase ? ''
        : exec.in_flight ? `Successor exec: ${exec.phase}…`
        : exec.ok === true ? (exec.detail || 'Successor exec completed.')
        : exec.ok === false ? `Successor exec failed: ${exec.error || 'see the daemon log'}`
        : '';
      if (execText) {
        const note = document.createElement('div');
        note.className = 'handover-update-note';
        note.textContent = execText;
        actions.appendChild(note);
      }
    }
    return actions;
  }

  // textContent construction throughout: the on-disk provenance strings
  // come from probing a binary the daemon merely observed — never markup.
  function handoverUpdateRender(body) {
    lastHandoverBody = body;
    fillPanelSwapSection();
    const update = body && body.update;
    if (!update || body.draining) { updateChipClear(); return; }
    const disk = update.on_disk;
    const running = update.running || {};
    const sha = (disk && disk.git_sha) || 'unknown';
    const el = updateChip();
    el.textContent = '';
    // Local click feedback holds the card open; otherwise the owner's
    // stored per-sha choice rules (an unseen sha announces expanded
    // once). Payload-side progress (another surface's pending swap)
    // respects a stored collapse — the pill shows it instead.
    const feedbackOpen = updateAction.inFlight || Boolean(updateAction.note);
    const collapsed = updateChipStoredState(sha) === 'collapsed' && !feedbackOpen;
    el.classList.toggle('collapsed', collapsed);
    if (collapsed) {
      el.textContent = relayPendingNow(body)
        ? 'Updating…'
        : (disk ? `Update · ${String(sha).slice(0, 10)}` : 'Update on disk');
      el.title = disk
        ? `Newer build on disk: commit ${sha} — running ${running.git_sha || '?'}. Click to expand.`
        : 'The intendant binary on disk changed. Click to expand.';
      el.setAttribute('role', 'button');
      el.onclick = () => {
        updateChipStoreState(sha, 'expanded');
        handoverUpdateRender(lastHandoverBody);
      };
      return;
    }
    el.onclick = null;
    el.removeAttribute('role');
    el.removeAttribute('title');
    const head = document.createElement('div');
    head.className = 'handover-update-head';
    const strong = document.createElement('strong');
    head.appendChild(strong);
    const collapse = document.createElement('button');
    collapse.type = 'button';
    collapse.className = 'handover-update-collapse';
    collapse.textContent = '–';
    collapse.title = 'Collapse to a pill — the fact stays visible';
    collapse.setAttribute('aria-label', 'Collapse the update chip to a pill');
    collapse.addEventListener('click', (ev) => {
      // Same bubble hazard as the banner's collapse: the collapsed
      // re-render puts the expand handler on this click's path.
      ev.stopPropagation();
      updateAction.note = '';
      updateChipStoreState(sha, 'collapsed');
      handoverUpdateRender(lastHandoverBody);
    });
    head.appendChild(collapse);
    el.appendChild(head);
    if (disk) {
      strong.textContent = 'Update on disk';
      el.appendChild(document.createTextNode(
        `commit ${disk.git_sha || '?'} · built ${disk.built_at || '?'} — running ${running.git_sha || '?'}`
      ));
      if (update.honesty) {
        const honesty = document.createElement('div');
        honesty.className = 'handover-update-honesty';
        honesty.textContent = update.honesty;
        el.appendChild(honesty);
      }
    } else {
      strong.textContent = 'Binary changed on disk';
      el.appendChild(document.createTextNode(
        `provenance unreadable${update.probe_error ? ` (${update.probe_error})` : ''}`
      ));
    }
    // The pinned honesty sentence — byte-identical on every update surface
    // (static_assets.rs pins the served copy).
    const bounds = document.createElement('div');
    bounds.className = 'handover-update-note';
    bounds.textContent = 'The update installs alongside the current version — running sessions finish uninterrupted, and the old version may keep running until they are done.';
    el.appendChild(bounds);
    if (body.swap_result && body.swap_result.ok === false && !updateAction.note) {
      const failed = document.createElement('div');
      failed.className = 'handover-update-note';
      failed.textContent = `Update failed: ${body.swap_result.detail || 'see the app log'}. The running daemon is untouched.`;
      el.appendChild(failed);
    } else if (relayPendingNow(body) && !updateAction.note && !updateAction.inFlight) {
      const pending = document.createElement('div');
      pending.className = 'handover-update-note';
      pending.textContent = 'The app supervisor is starting the new daemon — the drain banner appears here when it takes over.';
      el.appendChild(pending);
    }
    el.appendChild(handoverUpdateActions(body, disk));
    // The pill expands into the full update panel: the chip stays the
    // standing fact; channels, checks, and builds live there.
    if (typeof routeTo === 'function') {
      const panelLink = document.createElement('button');
      panelLink.type = 'button';
      panelLink.className = 'handover-update-panel-link';
      panelLink.textContent = 'Open the update panel';
      panelLink.addEventListener('click', () => routeTo('access', 'daemons'));
      el.appendChild(panelLink);
    }
  }

  // The consumer produce job as the release chip tells it (the job
  // slot is shared with dev builds; only the consumer lane is this
  // chip's story).
  function releaseJobState(body) {
    const job = body && body.update_lane && body.update_lane.job;
    if (!job || job.lane !== 'consumer') return null;
    return job;
  }

  // The release chip's action row: the one-click rides the update
  // panel's ONE produce emitter (the emission-shape law — this module
  // adds no lane wire of its own), and an install that cannot take the
  // release gets the honest reason and the panel doorway instead of a
  // button that cannot mean what it says.
  function releaseChipActions(release, tag, job) {
    const actions = document.createElement('div');
    actions.className = 'handover-update-actions';
    if (job && !('ok' in job)) {
      const note = document.createElement('div');
      note.className = 'handover-update-note';
      note.textContent = `Downloading release — ${job.phase}…`;
      actions.appendChild(note);
    } else if (job && job.ok) {
      const note = document.createElement('div');
      note.className = 'handover-update-note';
      note.textContent = job.detail || 'Release installed — the update chip offers the swap.';
      actions.appendChild(note);
    } else {
      if (job && job.ok === false) {
        const failed = document.createElement('div');
        failed.className = 'handover-update-note';
        failed.textContent = `Update failed: ${job.error || 'see the daemon log'}. The running daemon is untouched.`;
        actions.appendChild(failed);
      }
      if (release.one_click === true) {
        const btn = document.createElement('button');
        btn.textContent = `Update to ${tag}`;
        btn.addEventListener('click', () => {
          if (window.intendantUpdateLane
              && typeof window.intendantUpdateLane.produce === 'function') {
            releaseAction.note = 'Starting the verified download — progress lands here.';
            window.intendantUpdateLane.produce('releases');
            handoverReleaseRender(lastHandoverBody);
            armQuickHandoverPoll();
          }
        });
        actions.appendChild(btn);
      } else if (release.reason) {
        const reach = document.createElement('div');
        reach.className = 'handover-update-reach';
        reach.textContent = release.reason;
        actions.appendChild(reach);
      }
      if (releaseAction.note) {
        const note = document.createElement('div');
        note.className = 'handover-update-note';
        note.textContent = releaseAction.note;
        actions.appendChild(note);
      }
    }
    // The chip stays small: version detail, channels, and the job log
    // live on the update panel.
    if (typeof routeTo === 'function') {
      const panelLink = document.createElement('button');
      panelLink.type = 'button';
      panelLink.className = 'handover-update-panel-link';
      panelLink.textContent = 'Open the update panel';
      panelLink.addEventListener('click', () => routeTo('access', 'daemons'));
      actions.appendChild(panelLink);
    }
    return actions;
  }

  // The release-availability chip: a releases-channel check found a
  // VERIFIED release newer than the running version (the payload's
  // `release_update` block — server-derived, absent on quiet failure),
  // promoted into the same docked chip/pill lane as the on-disk chip.
  // A DISTINCT fact: "new release available" here, "new build on disk"
  // there — both may render. textContent construction throughout —
  // release tags and versions are observed strings, never markup.
  function handoverReleaseRender(body) {
    const release = body && body.release_update;
    if (!release || body.draining) { releaseChipClear(); return; }
    const tag = String(release.latest_tag || 'unknown');
    const running = String(release.running_version || '?');
    const job = releaseJobState(body);
    if (job) {
      // The payload's job truth supersedes the local click bridge.
      releaseAction.note = '';
      if (!('ok' in job)) armQuickHandoverPoll();
    }
    const el = releaseChip();
    el.textContent = '';
    const feedbackOpen = Boolean(releaseAction.note) || Boolean(job && !('ok' in job));
    const collapsed = releaseChipStoredState(tag) === 'collapsed' && !feedbackOpen;
    el.classList.toggle('collapsed', collapsed);
    if (collapsed) {
      el.textContent = `Release · ${tag}`;
      el.title = `New release ${tag} — running v${running}. Click to expand.`;
      el.setAttribute('role', 'button');
      el.onclick = () => {
        releaseChipStoreState(tag, 'expanded');
        handoverReleaseRender(lastHandoverBody);
      };
      return;
    }
    el.onclick = null;
    el.removeAttribute('role');
    el.removeAttribute('title');
    const head = document.createElement('div');
    head.className = 'handover-update-head';
    const strong = document.createElement('strong');
    strong.textContent = 'New release available';
    head.appendChild(strong);
    const collapse = document.createElement('button');
    collapse.type = 'button';
    collapse.className = 'handover-update-collapse';
    collapse.textContent = '–';
    collapse.title = 'Collapse to a pill — the fact stays visible';
    collapse.setAttribute('aria-label', 'Collapse the release chip to a pill');
    collapse.addEventListener('click', (ev) => {
      // Same bubble hazard as its siblings: the collapsed re-render
      // installs the expand handler on this click's path.
      ev.stopPropagation();
      releaseAction.note = '';
      releaseChipStoreState(tag, 'collapsed');
      handoverReleaseRender(lastHandoverBody);
    });
    head.appendChild(collapse);
    el.appendChild(head);
    el.appendChild(document.createTextNode(
      `${tag} (v${String(release.latest_version || '?')}) — running v${running}`
    ));
    // The pinned honesty sentence — byte-identical on every update surface
    // (static_assets.rs pins the served copy).
    const bounds = document.createElement('div');
    bounds.className = 'handover-update-note';
    bounds.textContent = 'The update installs alongside the current version — running sessions finish uninterrupted, and the old version may keep running until they are done.';
    el.appendChild(bounds);
    el.appendChild(releaseChipActions(release, tag, job));
  }

  let bannerResize = null;

  function handoverBanner() {
    if (!bannerEl) {
      bannerEl = document.createElement('div');
      bannerEl.id = 'handover-banner';
      bannerEl.className = 'handover-banner';
      document.body.appendChild(bannerEl);
      if (typeof ResizeObserver === 'function') {
        bannerResize = new ResizeObserver(handoverBannerReserve);
        bannerResize.observe(bannerEl);
      } else {
        window.addEventListener('resize', handoverBannerReserve);
      }
    }
    return bannerEl;
  }

  function handoverClear() {
    if (bannerEl) {
      if (bannerResize) { bannerResize.disconnect(); bannerResize = null; }
      window.removeEventListener('resize', handoverBannerReserve);
      bannerEl.remove();
      bannerEl = null;
    }
    handoverBannerReserve();
  }

  // The strip reserves its own real height: #app's top padding and the
  // fixed panels hanging off the oversight bar all offset by
  // --ui2-handover-h (the --ui2-composer-h pattern), so the banner —
  // expanded or collapsed — displaces content instead of covering it.
  function handoverBannerReserve() {
    const h = bannerEl ? Math.ceil(bannerEl.getBoundingClientRect().height) : 0;
    document.documentElement.style.setProperty('--ui2-handover-h', (h > 0 ? h : 0) + 'px');
  }

  // Collapsed-state persistence for the banners, PER (kind, boot id) —
  // the update chip's per-sha pattern verbatim: a never-seen fact (this
  // daemon's drain, a given set of draining predecessors) renders the
  // full banner once; the owner's collapse then persists for that fact
  // — across polls and reloads — as a one-line pill that keeps it
  // discoverable without walling anything. There is deliberately NO
  // dismiss: some rendering of the fact always stands while it is true
  // (a drain in motion must not be forgettable), only its size is the
  // owner's choice. A new boot, or a changed predecessor set, starts
  // fresh.
  function bannerStateKey(kind, bootId) {
    return 'handover-banner:' + kind + ':' + (bootId || 'unknown');
  }
  function bannerStoredState(key) {
    try { return localStorage.getItem(key); } catch (_) { return null; }
  }
  function bannerStoreState(key, state) {
    try {
      // One live banner fact at a time — stale keys go as we write.
      for (let i = localStorage.length - 1; i >= 0; i--) {
        const k = localStorage.key(i);
        if (k && k.indexOf('handover-banner:') === 0 && k !== key) {
          localStorage.removeItem(k);
        }
      }
      localStorage.setItem(key, state);
    } catch (_) { /* storage unavailable — the choice lives for this page only */ }
  }

  // Applies the stored choice to the element and strips pill-only
  // attributes when expanded; both kinds route through here.
  function bannerCollapsedNow(el, key) {
    const collapsed = bannerStoredState(key) === 'collapsed';
    el.classList.toggle('collapsed', collapsed);
    if (!collapsed) {
      el.onclick = null;
      el.onkeydown = null;
      el.removeAttribute('role');
      el.removeAttribute('tabindex');
      el.removeAttribute('title');
    }
    return collapsed;
  }

  // The one-line pill: the whole strip is the expand affordance
  // (click or Enter/Space), and the trailing "(open)" names it.
  function bannerFillPill(el, key, label) {
    el.title = 'Click to expand.';
    el.setAttribute('role', 'button');
    el.setAttribute('tabindex', '0');
    const expand = () => {
      bannerStoreState(key, 'expanded');
      handoverRender(lastHandoverBody);
    };
    el.onclick = expand;
    el.onkeydown = (ev) => {
      if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); expand(); }
    };
    el.appendChild(document.createTextNode(label + ' '));
    const open = document.createElement('span');
    open.className = 'handover-banner-open';
    open.textContent = '(open)';
    el.appendChild(open);
  }

  function bannerCollapseButton(key, ariaLabel) {
    const collapse = document.createElement('button');
    collapse.type = 'button';
    collapse.className = 'handover-banner-collapse';
    collapse.textContent = '–';
    collapse.title = 'Collapse to a pill — the fact stays visible';
    collapse.setAttribute('aria-label', ariaLabel);
    collapse.addEventListener('click', (ev) => {
      // The container is the pill's expand affordance: the re-render
      // installs its handler on the still-bubbling click's path, so an
      // unstopped collapse click would expand right back (probe-caught
      // live on both this banner and the update chip).
      ev.stopPropagation();
      bannerStoreState(key, 'collapsed');
      handoverRender(lastHandoverBody);
    });
    return collapse;
  }

  // The predecessor pill's one line, honest about what is known: the
  // port when there is one predecessor, the summed still-finishing
  // count only when every predecessor reports one.
  function bannerPredecessorPill(others) {
    const counts = others.map((d) => Number(d && d.session_count));
    const total = counts.every((n) => Number.isFinite(n) && n >= 0)
      ? counts.reduce((a, b) => a + b, 0)
      : null;
    const finishing = total === null ? '' : ` — ${total} session${total === 1 ? '' : 's'} finishing`;
    if (others.length === 1) {
      const port = Number(others[0] && others[0].port);
      const where = Number.isFinite(port) && port > 0 ? ` on :${port}` : '';
      return `A predecessor daemon is draining${where}${finishing}`;
    }
    return `${others.length} predecessor daemons are draining${finishing}`;
  }

  // The §5.1 live-holder resolution rule (update-abstraction intake) —
  // the ONE place a successor doorway target may come from. The lease
  // sidecar names the most recent acquirer OR the most recent drainer
  // during the entry→acquisition window (6c), so the sidecar target
  // counts only while its daemons entry is live and neither draining
  // nor exited; otherwise the hand-off candidate rule is the fallback
  // (live, non-draining, on-disk build preferred). Null means "nobody
  // yet": render the pending line, never a doorway into a drain.
  function resolveLiveHolder(body) {
    const daemons = Array.isArray(body && body.daemons) ? body.daemons : [];
    const sidecar = body && body.sidecar;
    if (sidecar && body.boot_id && sidecar.boot_id !== body.boot_id) {
      const entry = daemons.find((d) => d && d.boot_id === sidecar.boot_id);
      if (entry && entry.live && entry.state !== 'draining' && entry.state !== 'exited') {
        const port = Number(entry.port);
        if (Number.isFinite(port) && port > 0) return entry;
      }
    }
    const update = body && body.update;
    return handoverSuccessorCandidate(body, update ? update.on_disk : null);
  }

  // Render at most this many holdout rows; the rest fold into an honest
  // "…and N more" (parked rows arrive sorted first from the daemon, so
  // the decisive parked-until facts survive the fold).
  const HOLDOUT_RENDER_CAP = 8;

  function handoverShortId(id) {
    const raw = String(id || '');
    return raw.length > 8 ? raw.slice(0, 8) : raw;
  }

  // "9:10 PM (in ~42m)" from an epoch-seconds reset instant, in the
  // viewer's locale; honest about a reset that is due or just passed.
  function handoverParkedUntil(resetsAtEpoch) {
    const epoch = Number(resetsAtEpoch);
    if (!Number.isFinite(epoch) || epoch <= 0) return null;
    const at = new Date(epoch * 1000);
    const clock = at.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
    const deltaMin = Math.round((at.getTime() - Date.now()) / 60000);
    if (deltaMin > 1) {
      const spell = deltaMin >= 90 ? `~${Math.round(deltaMin / 60)}h` : `~${deltaMin}m`;
      return `${clock} (in ${spell})`;
    }
    return `${clock} (due now)`;
  }

  // Owner-facing state words. A limit-parked holdout leads with the
  // parked-until instant — the decisive fact (the park can hold the
  // drain for hours). A live background-task park says what the idle
  // session actually waits on (a DIED park never reaches this list —
  // those sessions are releasable and leave the wait set). Unknown
  // phases pass through rather than lie.
  function handoverHoldoutState(holdout) {
    const park = holdout && holdout.limit_park;
    if (park) {
      const until = handoverParkedUntil(park.resets_at_epoch);
      return until
        ? `rate-limit parked until ${until}`
        : 'rate-limit parked — reset time unknown';
    }
    const bgPark = holdout && holdout.bg_park;
    if (bgPark && !bgPark.died_cause) {
      const n = Array.isArray(bgPark.tasks) ? bgPark.tasks.length : 0;
      return n === 1
        ? 'parked on a background task'
        : `parked on ${n || 'its'} background tasks`;
    }
    const phase = String((holdout && holdout.phase) || '');
    switch (phase) {
      case 'running':
      case 'thinking': return 'working';
      case 'waiting_approval': return 'waiting on an approval';
      case 'waiting_human': return 'waiting on you';
      case 'waiting_rate_limit': return 'rate-limit parked';
      case 'waiting_service_recovery': return 'paused for provider recovery';
      case 'idle': return 'idle — awaiting input';
      case 'interrupting':
      case 'interrupted': return 'interrupted';
      default: return phase || 'unknown';
    }
  }

  // The named wait set. textContent throughout — session names are
  // agent/user-controlled and must never reach innerHTML (the update
  // chip states the same rule).
  function handoverHoldoutList(rows, totalCount) {
    const wrap = document.createElement('div');
    wrap.className = 'handover-holdouts';
    const list = document.createElement('ul');
    list.className = 'handover-holdout-list';
    rows.slice(0, HOLDOUT_RENDER_CAP).forEach((holdout) => {
      if (!holdout || typeof holdout !== 'object') return;
      const item = document.createElement('li');
      const who = document.createElement('span');
      who.className = 'handover-holdout-name';
      who.textContent = holdout.name || handoverShortId(holdout.session_id) || 'unnamed session';
      item.appendChild(who);
      const state = document.createElement('span');
      state.className = 'handover-holdout-state';
      if (holdout.limit_park) state.classList.add('parked');
      state.textContent = ` — ${handoverHoldoutState(holdout)}`;
      item.appendChild(state);
      list.appendChild(item);
    });
    wrap.appendChild(list);
    const total = Number(totalCount);
    const shown = Math.min(rows.length, HOLDOUT_RENDER_CAP);
    const folded = Number.isFinite(total) && total > shown ? total - shown : 0;
    if (folded > 0) {
      const more = document.createElement('div');
      more.className = 'handover-holdout-more';
      more.textContent = `…and ${folded} more`;
      wrap.appendChild(more);
    }
    return wrap;
  }

  // A real doorway to a co-homed daemon — same-host port substitution,
  // like the old inline link, but rendered as a prominent element. On
  // the loopback posture a sibling refuses tokenless pages, so the
  // click decorates the URL through the same-home token map
  // (withSiblingLoopbackToken, the fleet-links lane) and lands authed
  // instead of on the named 401; non-loopback surfaces pass through
  // untouched. Modifier/middle clicks keep their native semantics on
  // the bare href.
  function handoverDaemonLink(port, label) {
    const link = document.createElement('a');
    link.className = 'handover-successor-link';
    const bare = `${location.protocol}//${location.hostname}:${port}/`;
    link.href = bare;
    link.target = '_blank';
    link.rel = 'noopener';
    link.textContent = label;
    link.addEventListener('click', (ev) => {
      if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey || ev.button !== 0) return;
      ev.preventDefault();
      Promise.resolve(withSiblingLoopbackToken(bare)).then((url) => {
        window.open(url, '_blank', 'noopener');
      });
    });
    return link;
  }

  function handoverRender(body) {
    if (!body || body.available === false) {
      handoverClear();
      updateChipClear();
      releaseChipClear();
      return;
    }
    if (body.boot_id && typeof maybeNudgeDaemonBoot === 'function') {
      maybeNudgeDaemonBoot(body.boot_id);
    }
    handoverUpdateRender(body);
    handoverReleaseRender(body);
    if (body.draining) {
      const holder = resolveLiveHolder(body);
      const el = handoverBanner();
      el.dataset.kind = 'draining';
      el.textContent = '';
      const key = bannerStateKey('draining', body.boot_id);
      const rows = Array.isArray(body.holdouts) ? body.holdouts : [];
      if (bannerCollapsedNow(el, key)) {
        bannerFillPill(el, key, rows.length
          ? `This daemon is draining — waiting on ${rows.length} in-flight session${rows.length === 1 ? '' : 's'}`
          : 'This daemon is draining — in-flight sessions are finishing');
        handoverBannerReserve();
        return;
      }
      el.appendChild(bannerCollapseButton(key, 'Collapse the drain banner to a pill'));
      const head = document.createElement('div');
      head.className = 'handover-banner-head';
      const lead = document.createElement('strong');
      lead.textContent = 'This daemon is draining.';
      head.appendChild(lead);
      const tail = document.createElement('span');
      tail.textContent = rows.length
        ? ` Waiting on ${rows.length} in-flight session${rows.length === 1 ? '' : 's'}, then it exits.`
        : ' In-flight sessions finish here, then it exits.';
      head.appendChild(tail);
      el.appendChild(head);
      if (holder) {
        el.appendChild(
          handoverDaemonLink(holder.port, `Open the successor daemon (:${holder.port}) →`)
        );
      } else {
        const none = document.createElement('div');
        none.className = 'handover-banner-note';
        none.textContent = 'No successor has acquired the lease yet.';
        el.appendChild(none);
      }
      if (rows.length) el.appendChild(handoverHoldoutList(rows, rows.length));
      handoverBannerReserve();
      return;
    }
    const others = Array.isArray(body.daemons)
      ? body.daemons.filter((d) => d && d.live && d.state === 'draining' && d.boot_id !== body.boot_id)
      : [];
    if (others.length) {
      const el = handoverBanner();
      el.dataset.kind = 'predecessor';
      el.textContent = '';
      // The fact identity is the SET of draining predecessor boots: a
      // predecessor appearing (or the set changing) announces expanded
      // once; the same set stays as the owner left it.
      const key = bannerStateKey('predecessor',
        others.map((d) => String((d && d.boot_id) || 'unknown')).sort().join('+'));
      if (bannerCollapsedNow(el, key)) {
        bannerFillPill(el, key, bannerPredecessorPill(others));
        handoverBannerReserve();
        return;
      }
      el.appendChild(bannerCollapseButton(key, 'Collapse the predecessor banner to a pill'));
      others.forEach((d) => {
        const port = Number(d.port);
        const count = Number(d.session_count);
        const section = document.createElement('div');
        section.className = 'handover-predecessor';
        const head = document.createElement('div');
        head.className = 'handover-banner-head';
        const lead = document.createElement('span');
        lead.textContent = `A predecessor daemon is draining${Number.isFinite(port) ? ` on :${port}` : ''}`
          + `${Number.isFinite(count) ? ` — ${count} session${count === 1 ? '' : 's'} still finishing there` : ''}.`;
        head.appendChild(lead);
        if (Number.isFinite(port) && port > 0) {
          head.appendChild(document.createTextNode(' '));
          head.appendChild(handoverDaemonLink(port, `Open :${port} →`));
        }
        section.appendChild(head);
        const rows = Array.isArray(d.holdouts) ? d.holdouts : [];
        if (rows.length) section.appendChild(handoverHoldoutList(rows, count));
        const note = document.createElement('div');
        note.className = 'handover-banner-note';
        note.textContent = 'Mid-work sessions it releases are picked up here when it exits.';
        section.appendChild(note);
        el.appendChild(section);
      });
      handoverBannerReserve();
      return;
    }
    handoverClear();
  }

  async function handoverPoll() {
    try {
      if (typeof daemonApi === 'undefined') return;
      if (!daemonApi.availability('api_daemon_handover').ok) return;
      const resp = await daemonApi.request('api_daemon_handover', {});
      if (resp && resp.ok) handoverRender(resp.body);
    } catch (_) { /* transient — keep the last render */ }
  }

  // QA driver: the tokenless validate-dashboard posture cannot fetch
  // the authed handover payload, so probes render the chip from a
  // synthetic body and exercise the collapse persistence directly.
  window.qa = window.qa || {};
  window.qa.handoverUpdateChip = {
    render: (body) => handoverUpdateRender(body),
    stateKey: updateChipStateKey,
  };
  // Same tokenless-QA posture for the drain banner: probes feed a
  // synthetic handover body and assert the named wait set + the
  // successor doorway render; `stateKey` lets them seed and read the
  // per-(kind, boot) collapse choice to walk the pill round-trip.
  window.qa.handoverBanner = {
    render: (body) => handoverRender(body),
    stateKey: bannerStateKey,
  };
  // And for the release chip: probes feed a synthetic body carrying
  // `release_update` and walk the pill round-trip plus the dock
  // stacking beside the on-disk chip. Setting lastHandoverBody keeps
  // the chip's own re-render clicks on the synthetic fact.
  window.qa.handoverReleaseChip = {
    render: (body) => { lastHandoverBody = body; handoverReleaseRender(body); },
    stateKey: releaseChipStateKey,
  };

  // The palette's dynamic entry, as data (ui2-chrome reads this by name
  // at event time): the SAME one-click affordance the chip is currently
  // serving, or null — no update on disk, a drain in motion (the chip's
  // own suppression), and the honest no-reach arm all serve no action,
  // so the palette shows none. Labels say what happens NOW and always
  // carry 'update' (the palette matches labels only — users type what
  // they see); `busy` mirrors the chip button's disabled state.
  function updatePaletteEntry() {
    const body = lastHandoverBody;
    const update = body && body.update;
    if (!update || body.draining) return null;
    const disk = update.on_disk;
    const tag = disk ? String(disk.git_sha || disk.version || '').slice(0, 10) : '';
    const what = tag ? `update ${tag}` : 'update (binary changed on disk)';
    const supervised = window.__intendantAppSupervisor === true
      || (body && body.app_supervised === true);
    if (supervised) {
      const busy = updateAction.inFlight || relayPendingNow(body);
      return {
        label: busy ? `Installing ${what}…` : `Install ${what}`,
        busy,
        run: () => performOneClickSwap(body),
      };
    }
    const successor = handoverSuccessorCandidate(body, disk);
    if (!successor) return null;
    const matches = disk && successor.version && successor.version.git_sha === disk.git_sha;
    return {
      label: `Update: hand off to :${successor.port}${matches ? ' (runs the on-disk build)' : ''}`,
      busy: updateAction.inFlight,
      run: () => performTakeoverHandoff(successor, body),
    };
  }

  // The update panel's and palette's composition hooks (production, not
  // QA): the panel registers its swap mount here, the palette asks for
  // the served one-click as data — this module keeps both honest.
  window.intendantHandoverUpdate = {
    renderSwapSection: (mount) => {
      panelSwapMount = mount;
      fillPanelSwapSection();
    },
    paletteEntry: updatePaletteEntry,
  };

  function handoverStart() {
    handoverPoll();
    setInterval(handoverPoll, HANDOVER_POLL_MS);
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', handoverStart);
  } else {
    handoverStart();
  }
})();

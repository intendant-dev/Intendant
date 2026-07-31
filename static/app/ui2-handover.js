// ── ui2-handover — drain banner + successor chip (HS5) + update chip
// (HS6) ──────────────────────────────────────────────────────────────
// Polls GET /api/daemon/handover (tunnel twin api_daemon_handover): the
// scheduler-lease status block. Renders:
//   - THIS daemon draining: a persistent banner — in-flight sessions
//     finish here; new work continues on the successor (linked once the
//     lease sidecar names it).
//   - another co-homed daemon draining: a compact predecessor chip with
//     its remaining session count.
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

  function updateChip() {
    if (!updateEl) {
      updateEl = document.createElement('div');
      updateEl.id = 'handover-update-chip';
      updateEl.className = 'handover-update-chip';
      document.body.appendChild(updateEl);
    }
    return updateEl;
  }

  function updateChipClear() {
    if (updateEl) { updateEl.remove(); updateEl = null; }
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

  // The one action's client state (the button must never double-fire and
  // its feedback must survive the 30s poll cadence). `relayRequestedMs`
  // bridges the gap between a relay click and the next poll's payload
  // (which carries `swap_pending_ms` server-side, surviving reloads).
  const updateAction = { inFlight: false, note: '', relayRequestedMs: 0 };
  let lastHandoverBody = null;

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
      btn.addEventListener('click', () => {
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
      });
      actions.appendChild(btn);
    } else {
      const successor = handoverSuccessorCandidate(body, disk);
      if (successor) {
        const matches = disk && successor.version && successor.version.git_sha === disk.git_sha;
        const btn = document.createElement('button');
        btn.textContent = `Hand off to :${successor.port}${matches ? ' (runs the on-disk build)' : ''}`;
        btn.disabled = updateAction.inFlight;
        btn.addEventListener('click', async () => {
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
        });
        actions.appendChild(btn);
      } else {
        const reach = document.createElement('div');
        reach.className = 'handover-update-reach';
        reach.textContent = 'No other daemon is running to hand off to — this daemon cannot launch one itself. The macOS app (and a service-managed install) can do this in one click.';
        actions.appendChild(reach);
      }
    }
    if (updateAction.note) {
      const note = document.createElement('div');
      note.className = 'handover-update-note';
      note.textContent = updateAction.note;
      actions.appendChild(note);
    }
    return actions;
  }

  // textContent construction throughout: the on-disk provenance strings
  // come from probing a binary the daemon merely observed — never markup.
  function handoverUpdateRender(body) {
    lastHandoverBody = body;
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
    collapse.addEventListener('click', () => {
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
  }

  function handoverBanner() {
    if (!bannerEl) {
      bannerEl = document.createElement('div');
      bannerEl.id = 'handover-banner';
      bannerEl.className = 'handover-banner';
      document.body.appendChild(bannerEl);
    }
    return bannerEl;
  }

  function handoverClear() {
    if (bannerEl) { bannerEl.remove(); bannerEl = null; }
  }

  function handoverSuccessorPort(body) {
    const sidecar = body && body.sidecar;
    if (!sidecar || !body.boot_id || sidecar.boot_id === body.boot_id) return null;
    const port = Number(sidecar.port);
    return Number.isFinite(port) && port > 0 ? port : null;
  }

  function handoverRender(body) {
    if (!body || body.available === false) { handoverClear(); updateChipClear(); return; }
    if (body.boot_id && typeof maybeNudgeDaemonBoot === 'function') {
      maybeNudgeDaemonBoot(body.boot_id);
    }
    handoverUpdateRender(body);
    if (body.draining) {
      const port = handoverSuccessorPort(body);
      const link = port
        ? `<a href="${location.protocol}//${location.hostname}:${port}/" target="_blank" rel="noopener">:${port}</a>`
        : null;
      const el = handoverBanner();
      el.dataset.kind = 'draining';
      el.innerHTML = link
        ? `<strong>This daemon is draining.</strong> In-flight sessions finish here — continue new work on the successor at ${link}.`
        : '<strong>This daemon is draining.</strong> In-flight sessions finish here — no successor has acquired the lease yet.';
      return;
    }
    const others = Array.isArray(body.daemons)
      ? body.daemons.filter((d) => d && d.live && d.state === 'draining' && d.boot_id !== body.boot_id)
      : [];
    if (others.length) {
      const d = others[0];
      const port = Number(d.port);
      const count = Number(d.session_count);
      const sessions = Number.isFinite(count)
        ? ` · ${count} session${count === 1 ? '' : 's'} left`
        : '';
      const el = handoverBanner();
      el.dataset.kind = 'predecessor';
      el.innerHTML = `Predecessor daemon draining${Number.isFinite(port) ? ` on :${port}` : ''}${sessions} — it exits when its last in-flight session ends.`;
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

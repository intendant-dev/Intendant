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
//     keychain/TCC honesty line where it applies. Suppressed while
//     draining — a drain in motion outranks the waiting update.
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

  // The one action's client state (the button must never double-fire and
  // its feedback must survive the 30s poll cadence).
  const updateAction = { inFlight: false, note: '' };
  let lastHandoverBody = null;
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

  // The chip's action row. Inside the macOS app (__intendantAppSupervisor)
  // the supervisor owns the spawn, so the chip offers the real one-click.
  // On a CLI-launched daemon the chip is honest about its reach: it can
  // ask THIS daemon to drain toward an already-running newer daemon; it
  // cannot launch one, and it says so instead of pointing at a terminal.
  function handoverUpdateActions(body, disk) {
    const actions = document.createElement('div');
    actions.className = 'handover-update-actions';
    if (window.__intendantAppSupervisor === true) {
      const btn = document.createElement('button');
      btn.textContent = updateAction.inFlight ? 'Updating…' : 'Update now';
      btn.disabled = updateAction.inFlight;
      btn.addEventListener('click', () => {
        if (updateAction.inFlight) return;
        updateAction.inFlight = true;
        updateAction.note = 'Starting the new daemon — this dashboard reloads when it takes over; in-flight sessions finish on the old one.';
        try {
          window.webkit.messageHandlers.updateSwap.postMessage(null);
        } catch (_) {
          updateAction.inFlight = false;
          updateAction.note = 'Could not reach the app supervisor.';
        }
        handoverUpdateRender(body);
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
    const el = updateChip();
    el.textContent = '';
    const strong = document.createElement('strong');
    el.appendChild(strong);
    if (disk) {
      strong.textContent = 'Update on disk: ';
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
        ` — provenance unreadable${update.probe_error ? ` (${update.probe_error})` : ''}`
      ));
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

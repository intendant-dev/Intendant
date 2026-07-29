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

  // textContent construction throughout: the on-disk provenance strings
  // come from probing a binary the daemon merely observed — never markup.
  function handoverUpdateRender(body) {
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

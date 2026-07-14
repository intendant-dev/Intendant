// ── composer conversation peek ─────────────────────────────────────────
// A transient sheet floating above the dock: the prompt-target session's
// transcript tail, live, with tap-through to the full Activity view. The
// peek is a read-only mirror — entries are clones of the target window's
// log (the appendLogEntryToSessionWindow precedent: cloneNode + strip
// ids), so it never mutates session state; interactive affordances that
// need the live wiring (collapse, copy, retry) are hidden by the peek
// stylesheet in favor of "Open in Activity".
// Boot follows the ui2-chrome single-boot idiom; every entry point is a
// no-op when the mount is missing so a stub build stays inert.
{
  const PEEK_TAIL_CAP = 12;
  const PEEK_RENDER_THROTTLE_MS = 120;
  // Chrome's phase categories that mean "the agent is answering" — the
  // auto-open signal. `waiting` is excluded: approvals/questions already
  // have their own attention surfaces.
  const PEEK_WORKING_CATS = new Set(['thinking', 'running']);

  let peekBarEl = null;
  let peekRoot = null;
  let peekList = null;
  let peekBadge = null;
  let peekPhaseDot = null;
  let peekPhaseText = null;

  let peekBoundSid = '';
  let peekBoundLog = null;
  let peekRendered = []; // [{ source, clone }] in list order
  let peekFollow = true;
  // Explicit close suppresses auto-open until the working streak ends or
  // the target changes — otherwise the next phase mutation would fight
  // the user's dismissal.
  let peekDismissed = false;
  let peekLastTarget = '';
  let peekLastPhaseCat = null;
  let peekFlushTimer = 0;
  let peekPendingStructural = false;
  const peekPendingDirty = new Set();

  function peekIsOpen() {
    return !!peekRoot && !peekRoot.hidden;
  }

  function peekComposerPill() {
    return document.documentElement.dataset.composerState === 'pill';
  }

  function peekTargetSid() {
    return typeof resolvePromptTargetSessionId === 'function'
      ? (resolvePromptTargetSessionId() || '') : '';
  }

  function peekTargetWindow(sid) {
    return (sid && typeof sessionWindows !== 'undefined')
      ? (sessionWindows.get(sid) || null) : null;
  }

  function peekCloneEntry(source) {
    const clone = source.cloneNode(true);
    clone.removeAttribute('id');
    clone.querySelectorAll('[id]').forEach(el => el.removeAttribute('id'));
    return clone;
  }

  function peekTailSources() {
    if (!peekBoundLog) return [];
    const out = [];
    for (let node = peekBoundLog.lastElementChild; node && out.length < PEEK_TAIL_CAP; node = node.previousElementSibling) {
      if (node.classList.contains('session-window-empty')) continue;
      out.push(node);
    }
    return out.reverse();
  }

  function peekEmptyText() {
    if (!peekBoundLog) return 'No transcript yet — send a message below.';
    const ph = peekBoundLog.querySelector('.session-window-empty');
    if (ph && ph.classList.contains('session-window-empty-loading')) return 'Loading transcript…';
    if (ph && ph.classList.contains('session-window-empty-error')) return 'Transcript failed to load — open Activity to retry.';
    return 'No output yet';
  }

  // CSS cannot see overflow: entries whose body actually clips get the
  // fade tag; short ones keep their last line un-washed.
  function peekTagClamped(clone) {
    const content = clone.querySelector('.log-content');
    if (content && content.scrollHeight > content.clientHeight + 1) {
      clone.classList.add('ui2-peek-clamped');
    }
  }

  function peekRenderTail() {
    if (!peekList || !peekIsOpen()) return;
    const sources = peekTailSources();
    if (sources.length === 0) {
      peekRendered = [];
      peekPendingDirty.clear();
      const empty = document.createElement('div');
      empty.className = 'ui2-peek-empty';
      empty.textContent = peekEmptyText();
      peekList.replaceChildren(empty);
      return;
    }
    const prev = new Map(peekRendered.map(r => [r.source, r]));
    const next = [];
    const fresh = [];
    for (const source of sources) {
      const kept = prev.get(source);
      if (kept && !peekPendingDirty.has(source)) {
        next.push(kept);
        continue;
      }
      const clone = peekCloneEntry(source);
      next.push({ source, clone });
      fresh.push(clone);
    }
    peekRendered = next;
    // Surgical reconcile instead of replaceChildren: kept nodes never
    // leave the DOM, so the aria-live list announces genuinely new
    // entries instead of re-reading the whole tail on every append.
    const keep = new Set(next.map(r => r.clone));
    for (let child = peekList.firstElementChild; child;) {
      const drop = child;
      child = child.nextElementSibling;
      if (!keep.has(drop)) drop.remove();
    }
    let cursor = peekList.firstElementChild;
    for (const r of next) {
      if (r.clone === cursor) cursor = cursor.nextElementSibling;
      else peekList.insertBefore(r.clone, cursor);
    }
    for (const clone of fresh) peekTagClamped(clone);
    peekPendingDirty.clear();
    if (peekFollow) peekList.scrollTop = peekList.scrollHeight;
  }

  function peekFlush() {
    peekFlushTimer = 0;
    if (!peekIsOpen()) {
      peekPendingStructural = false;
      peekPendingDirty.clear();
      return;
    }
    if (peekPendingStructural) {
      peekPendingStructural = false;
      peekRenderTail();
      return;
    }
    if (peekPendingDirty.size === 0) return;
    let touched = false;
    for (const r of peekRendered) {
      if (!peekPendingDirty.has(r.source)) continue;
      const clone = peekCloneEntry(r.source);
      r.clone.replaceWith(clone);
      r.clone = clone;
      peekTagClamped(clone);
      touched = true;
    }
    peekPendingDirty.clear();
    if (touched && peekFollow) peekList.scrollTop = peekList.scrollHeight;
  }

  function peekSchedule() {
    if (peekFlushTimer || !peekIsOpen()) return;
    peekFlushTimer = setTimeout(peekFlush, PEEK_RENDER_THROTTLE_MS);
  }

  // Command-output groups and edit/superseded markers mutate entries in
  // place (no childList change on the log), so the observer watches the
  // subtree and patches just the touched entries; only additions/removals
  // on the log itself re-render the tail.
  const peekLogObserver = new MutationObserver(records => {
    for (const record of records) {
      if (record.target === peekBoundLog) {
        if (record.type === 'childList') peekPendingStructural = true;
        continue;
      }
      let node = record.target;
      while (node && node.parentNode !== peekBoundLog) node = node.parentNode;
      if (node) peekPendingDirty.add(node);
    }
    if (peekPendingStructural || peekPendingDirty.size) peekSchedule();
  });

  function peekBind(sid) {
    if (peekBadge) {
      peekBadge.hidden = !sid;
      if (typeof renderSessionIdentity === 'function') {
        renderSessionIdentity(peekBadge, sid, { order: 'name-id', titlePrefix: 'Prompt target' });
      }
      if (typeof applySessionBadgeStyle === 'function') applySessionBadgeStyle(peekBadge, sid);
    }
    const win = peekTargetWindow(sid);
    const log = win ? win.log : null;
    if (sid === peekBoundSid && log === peekBoundLog) return;
    peekLogObserver.disconnect();
    peekBoundSid = sid;
    peekBoundLog = log;
    peekRendered = [];
    peekPendingDirty.clear();
    peekPendingStructural = false;
    if (peekBoundLog) {
      peekLogObserver.observe(peekBoundLog, {
        childList: true, subtree: true, characterData: true, attributes: true,
      });
      if (typeof hydrateSessionWindowIfEmpty === 'function') {
        Promise.resolve(hydrateSessionWindowIfEmpty(sid)).catch(() => {}).finally(() => {
          if (peekBoundSid === sid) peekRenderTail();
        });
      }
    }
    peekRenderTail();
  }

  function peekOpen() {
    if (!peekRoot || peekIsOpen() || peekComposerPill()) return;
    const sid = peekTargetSid();
    if (!sid) return;
    peekFollow = true;
    peekRoot.hidden = false;
    peekBind(sid);
    peekRenderTail();
  }

  function peekClose(dismissed) {
    if (!peekRoot || peekRoot.hidden) return;
    peekRoot.hidden = true;
    if (dismissed) peekDismissed = true;
    peekLogObserver.disconnect();
    peekBoundSid = '';
    peekBoundLog = null;
    peekRendered = [];
    peekPendingDirty.clear();
    peekPendingStructural = false;
    if (peekFlushTimer) {
      clearTimeout(peekFlushTimer);
      peekFlushTimer = 0;
    }
    if (peekList) peekList.replaceChildren();
  }

  function peekOpenInActivity() {
    peekClose(false);
    if (typeof routeTo === 'function') routeTo('activity', 'log');
    if (typeof window.focusForegroundSessionWindow === 'function') {
      window.focusForegroundSessionWindow();
    }
  }

  function peekOnTargetChip() {
    const sid = peekTargetSid();
    if (sid !== peekLastTarget) {
      peekLastTarget = sid;
      peekDismissed = false;
    }
    if (!sid) {
      peekClose(false);
      return;
    }
    if (peekIsOpen()) {
      peekBind(sid);
      return;
    }
    // A target materializing mid-turn is the first-message case: the send
    // happened before any session existed, so the phase edge fired with
    // nothing to show — open now that there is a transcript to watch.
    // Same replay guard as the phase edge: rebinds driven by bootstrap
    // replay are history, not a turn in flight.
    if (typeof processingLogReplay !== 'undefined' && processingLogReplay) return;
    if (PEEK_WORKING_CATS.has(peekLastPhaseCat) && !peekDismissed && !peekComposerPill()) {
      peekOpen();
    }
  }

  function peekOnPhase(banner) {
    const cat = typeof ui2PhaseCategory === 'function'
      ? ui2PhaseCategory(banner.className) : 'idle';
    const label = document.getElementById('phase-text');
    if (peekPhaseDot) peekPhaseDot.dataset.cat = cat;
    if (peekPhaseText) {
      peekPhaseText.textContent = ((label && label.textContent) || 'Idle').trim() || 'Idle';
    }
    const wasWorking = PEEK_WORKING_CATS.has(peekLastPhaseCat);
    const isWorking = PEEK_WORKING_CATS.has(cat);
    peekLastPhaseCat = cat;
    if (!isWorking) {
      if (wasWorking) peekDismissed = false;
      return;
    }
    if (wasWorking || peekDismissed || peekIsOpen() || peekComposerPill()) return;
    // Bootstrap replay re-drives historical phase edges through the
    // mirror — auto-opening on those would pop the peek on every page
    // load of a dashboard with any past task.
    if (typeof processingLogReplay !== 'undefined' && processingLogReplay) return;
    peekOpen();
  }

  function peekBuild(root) {
    const icon = (name, size) => (typeof ui2Icon === 'function' ? ui2Icon(name, size) : '');
    root.setAttribute('role', 'region');
    root.setAttribute('aria-label', 'Conversation peek');

    const header = document.createElement('div');
    header.className = 'ui2-peek-header';
    header.title = 'Open the full transcript in Activity';

    peekBadge = document.createElement('span');
    peekBadge.className = 'ui2-peek-target task-target-session-badge';
    peekBadge.hidden = true;

    const phase = document.createElement('span');
    phase.className = 'ui2-peek-phase';
    peekPhaseDot = document.createElement('span');
    peekPhaseDot.className = 'ui2-peek-phase-dot';
    peekPhaseDot.dataset.cat = 'idle';
    peekPhaseText = document.createElement('span');
    peekPhaseText.className = 'ui2-peek-phase-text';
    peekPhaseText.textContent = 'Idle';
    phase.appendChild(peekPhaseDot);
    phase.appendChild(peekPhaseText);

    const openBtn = document.createElement('button');
    openBtn.type = 'button';
    openBtn.className = 'ui2-peek-open';
    openBtn.title = 'Open the full transcript in Activity';
    openBtn.setAttribute('aria-label', 'Open in Activity');
    openBtn.innerHTML = icon('external', 12) + '<span class="ui2-peek-open-label">Open in Activity</span>';

    const closeBtn = document.createElement('button');
    closeBtn.type = 'button';
    closeBtn.className = 'ui2-peek-close';
    closeBtn.title = 'Close conversation peek';
    closeBtn.setAttribute('aria-label', 'Close conversation peek');
    closeBtn.innerHTML = icon('close', 14);

    header.appendChild(peekBadge);
    header.appendChild(phase);
    header.appendChild(openBtn);
    header.appendChild(closeBtn);

    peekList = document.createElement('div');
    peekList.className = 'ui2-peek-list';
    peekList.setAttribute('role', 'log');
    peekList.setAttribute('aria-live', 'polite');
    peekList.setAttribute('aria-label', 'Recent transcript');

    root.appendChild(header);
    root.appendChild(peekList);

    header.addEventListener('click', (e) => {
      if (e.target.closest && e.target.closest('button')) return;
      peekOpenInActivity();
    });
    openBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      peekOpenInActivity();
    });
    closeBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      peekClose(true);
      const input = document.getElementById('activity-task-input');
      if (input) input.focus({ preventScroll: true });
    });
    peekList.addEventListener('scroll', () => {
      peekFollow = peekList.scrollTop + peekList.clientHeight >= peekList.scrollHeight - 24;
    });
  }

  // Boot autofocus (and any other programmatic focus before the user has
  // touched the page) must not open the peek — a sheet popping over the
  // content on every load of a dashboard with history is a surprise, not
  // an affordance. Focus counts as intent only after a real gesture.
  let peekSawUserGesture = false;

  const peekWire = () => {
    const root = document.getElementById('ui2-composer-peek');
    const bar = root ? root.closest('.global-task-bar') : null;
    if (!root || !bar) return;
    peekRoot = root;
    peekBarEl = bar;
    peekBuild(root);

    document.addEventListener('pointerdown', () => { peekSawUserGesture = true; }, { capture: true, passive: true });
    document.addEventListener('keydown', () => { peekSawUserGesture = true; }, { capture: true });

    bar.addEventListener('focusin', (e) => {
      if (!peekSawUserGesture) return;
      if (peekIsOpen() || peekComposerPill()) return;
      // Focus moving WITHIN the dock (input → Send, the close button's
      // refocus) is not an entry — reopening there would undo an Esc/×
      // dismissal the user just made.
      if (e.relatedTarget instanceof Node && bar.contains(e.relatedTarget)) return;
      // Focus landing on the target-switch chrome is retargeting intent,
      // not composition — opening the peek under that popover just stacks
      // two overlays (and made Esc close the hidden one first).
      const tsw = document.getElementById('ui2-target-switch');
      if (e.target instanceof Node && (
        (tsw && tsw.contains(e.target)) ||
        (e.target.id === 'task-target-switch-btn')
      )) return;
      peekDismissed = false;
      peekOpen();
    });

    // Window-level capture instead of bar-level: capture descends
    // window → bar, so this preempts the composer state machine's
    // Esc-to-pill regardless of listener registration order — one Esc
    // closes the peek, the next one reaches the dock.
    window.addEventListener('keydown', (e) => {
      if (e.key !== 'Escape' || !peekIsOpen()) return;
      // Onion ordering: the target-switch popover is the innermost open
      // surface — its own Esc handling closes it; consuming here would
      // close the peek hidden BEHIND the popover instead.
      const tsw = document.getElementById('ui2-target-switch');
      if (tsw && !tsw.hidden) return;
      if (e.target instanceof Node && peekBarEl.contains(e.target)) {
        e.preventDefault();
        e.stopImmediatePropagation();
        peekClose(true);
        return;
      }
      peekClose(true);
    }, true);

    document.addEventListener('pointerdown', (e) => {
      if (!peekIsOpen()) return;
      if (e.target instanceof Node && peekBarEl.contains(e.target)) return;
      peekClose(true);
    }, true);

    window.addEventListener('ui2:composer-state', (e) => {
      if (((e && e.detail && e.detail.state) || '') === 'pill') peekClose(false);
    });

    if (typeof ui2Mirror === 'function') {
      ui2Mirror('task-target-chip', peekOnTargetChip);
      ui2Mirror('phase-banner', peekOnPhase);
    }

    // A cold target may grow its window only later (ensureSessionWindow
    // on the first live event) — rebind when the grid gains it.
    const grid = document.getElementById('session-window-grid');
    if (grid) {
      new MutationObserver(() => {
        if (!peekIsOpen() || !peekBoundSid) return;
        const win = peekTargetWindow(peekBoundSid);
        if (win && win.log !== peekBoundLog) peekBind(peekBoundSid);
      }).observe(grid, { childList: true });
    }

    window.__ui2Peek = {
      isOpen: peekIsOpen,
      open: peekOpen,
      close: () => peekClose(true),
      sessionId: () => peekBoundSid,
    };
  };
  if (document.readyState === 'complete') peekWire();
  else document.addEventListener('DOMContentLoaded', peekWire, { once: true });
}

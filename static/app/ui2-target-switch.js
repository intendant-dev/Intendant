// ── composer target hot-swap ───────────────────────────────────────────
// In-place prompt-target switcher for the dock: #task-target-switch-btn
// toggles a listbox of exactly the sessions the resolver may target
// (isPromptTargetSessionUsable), plus "Auto" (hand the pick back to the
// resolver) and "New session…". Selecting retargets via focusSessionWindow
// — the whole point is that it never leaves the current tab (no route
// change, no scroll; the chip's own click stays the jump-to-Activity
// affordance). IIFE: fragments share one script scope and nothing here is
// consumed by other fragments.
(() => {
  const FILTER_AT = 6;   // more rows than this → show the type-to-filter input
  const POLL_MS = 800;   // open-state refresh: sessions come and go

  let btnEl = null;
  let popEl = null;
  let filterEl = null;
  let rowsEl = null;
  let isOpen = false;
  let pollTimer = 0;
  let filterQuery = '';
  let lastSig = '';

  const composerPill = () =>
    document.documentElement.dataset.composerState === 'pill';

  function usableSessionIds() {
    const ids = [];
    for (const sid of sessionWindows.keys()) {
      if (isPromptTargetSessionUsable(sid)) ids.push(sid);
    }
    return ids;
  }

  function rowPhase(sid) {
    if (hasPendingActiveSessionWindow(sid)) return 'active';
    const win = sessionWindows.get(sid);
    return sessionPhaseClass(win?.phase || 'idle') || 'done';
  }

  // Everything a rendered listing depends on; the open-state poll re-renders
  // only when this changes (a vanished session, a phase flip, a rename, a
  // target rebind elsewhere).
  function currentSignature() {
    const parts = [
      resolvePromptTargetSessionId(),
      explicitForegroundSessionId() ? 'E' : 'A',
      filterQuery,
    ];
    for (const sid of usableSessionIds()) {
      parts.push(sid, sessionIdentityParts(sid).name, rowPhase(sid));
    }
    return parts.join('\u0000');
  }

  const navRows = () => Array.from(popEl.querySelectorAll('.ui2-tsw-row'));

  function makeRow(key, cls) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = cls ? `ui2-tsw-row ${cls}` : 'ui2-tsw-row';
    row.setAttribute('role', 'option');
    row.setAttribute('aria-selected', 'false');
    row.tabIndex = -1;
    row.dataset.key = key;
    return row;
  }

  function buildAutoRow(active) {
    const row = makeRow('auto', 'ui2-tsw-auto');
    const main = document.createElement('span');
    main.className = 'ui2-tsw-row-main';
    const label = document.createElement('span');
    label.className = 'ui2-tsw-row-label';
    label.textContent = 'Auto';
    const sub = document.createElement('span');
    sub.className = 'ui2-tsw-row-sub';
    sub.textContent = 'picks the active session';
    main.append(label, sub);
    row.appendChild(main);
    row.title = 'Let Intendant pick the prompt target';
    row.setAttribute('aria-label', 'Auto: pick the prompt target for me');
    if (active) row.setAttribute('aria-selected', 'true');
    return row;
  }

  function buildSessionRow(sid, target) {
    const row = makeRow(`s:${sid}`);
    const badge = document.createElement('span');
    badge.className = 'ui2-tsw-badge';
    renderSessionIdentity(badge, sid, { showName: false });
    applySessionBadgeStyle(badge, sid);
    const name = sessionIdentityParts(sid).name;
    const nameEl = document.createElement('span');
    nameEl.className = 'ui2-tsw-row-name';
    nameEl.textContent = name;
    const phase = rowPhase(sid);
    const dot = document.createElement('span');
    dot.className = `ui2-tsw-dot ${phase}`;
    dot.setAttribute('aria-hidden', 'true');
    row.append(badge, nameEl, dot);
    row.title = name ? `${name} · ${sid}` : sid;
    const phaseWord = phase === 'active' ? 'running' : phase === 'waiting' ? 'waiting' : 'idle';
    row.setAttribute('aria-label', `${name || shortSessionId(sid)} · ${phaseWord}`);
    if (sid === target) row.setAttribute('aria-selected', 'true');
    return row;
  }

  function buildEmpty(text) {
    const el = document.createElement('div');
    el.className = 'ui2-tsw-empty';
    el.textContent = text;
    return el;
  }

  function renderRows() {
    const usable = usableSessionIds();
    const target = resolvePromptTargetSessionId();
    const autoActive = !explicitForegroundSessionId();
    filterEl.hidden = !(usable.length > FILTER_AT || filterQuery.trim() !== '');

    const active = document.activeElement;
    const prevKey = popEl.contains(active) && active !== filterEl
      ? active.dataset?.key || ''
      : '';

    let list = usable;
    const q = filterQuery.trim();
    if (q) {
      list = usable
        .map(sid => ({
          sid,
          score: Math.max(
            ui2FuzzyScore(q, sessionIdentityParts(sid).name),
            ui2FuzzyScore(q, sid),
          ),
        }))
        .filter(x => x.score >= 0)
        .sort((a, b) => b.score - a.score)
        .map(x => x.sid);
    }

    const children = [];
    if (usable.length) children.push(buildAutoRow(autoActive));
    for (const sid of list) children.push(buildSessionRow(sid, target));
    if (!usable.length) children.push(buildEmpty('No targetable sessions'));
    else if (!list.length) children.push(buildEmpty('No matches'));
    rowsEl.replaceChildren(...children);
    lastSig = currentSignature();

    // A refresh can remove the focused row — land on the survivor with the
    // same key, else the first row, so the keyboard flow never dies.
    if (prevKey) {
      const again = navRows().find(r => r.dataset.key === prevKey);
      (again || navRows()[0])?.focus();
    }
  }

  function openPopover() {
    if (isOpen || composerPill()) return;
    filterQuery = '';
    filterEl.value = '';
    isOpen = true;
    renderRows();
    popEl.hidden = false;
    btnEl.setAttribute('aria-expanded', 'true');
    const first = popEl.querySelector('.ui2-tsw-row[aria-selected="true"]') || navRows()[0];
    if (first) {
      first.focus();
      first.scrollIntoView({ block: 'nearest' });
    }
    document.addEventListener('pointerdown', onDocPointerDown, true);
    pollTimer = setInterval(pollRefresh, POLL_MS);
  }

  function closePopover(opts = {}) {
    if (!isOpen) return;
    isOpen = false;
    popEl.hidden = true;
    btnEl.setAttribute('aria-expanded', 'false');
    document.removeEventListener('pointerdown', onDocPointerDown, true);
    if (pollTimer) {
      clearInterval(pollTimer);
      pollTimer = 0;
    }
    if (opts.refocus) btnEl.focus();
  }

  function pollRefresh() {
    if (composerPill()) {
      closePopover();
      return;
    }
    if (currentSignature() !== lastSig) renderRows();
  }

  function onDocPointerDown(e) {
    if (popEl.contains(e.target) || btnEl.contains(e.target)) return;
    closePopover();
  }

  function activateRow(key) {
    if (key === 'auto') {
      // The explicit pick lives in two refs (foreground + current), possibly
      // holding different ids — discard until the explicit lane is empty so
      // the resolver takes over.
      for (let i = 0; i < 4 && explicitForegroundSessionId(); i++) {
        discardPromptTargetReference(explicitForegroundSessionId());
      }
      updateTaskTargetChip();
      closePopover({ refocus: true });
    } else if (key === 'new') {
      // Close without refocus: openNewSessionFromPrompt focuses the
      // new-session input on the next frame and must win.
      openNewSessionFromPrompt();
      closePopover();
    } else if (key.startsWith('s:')) {
      const sid = key.slice(2);
      if (!isPromptTargetSessionUsable(sid)) {
        renderRows();
        return;
      }
      focusSessionWindow(sid);
      closePopover({ refocus: true });
    }
  }

  function onPopKeydown(e) {
    const inFilter = e.target === filterEl;
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      closePopover({ refocus: true });
      return;
    }
    if (e.key === 'Tab') {
      // Close and hand focus back to the button so the default Tab
      // continues from the composer instead of a removed row.
      closePopover({ refocus: true });
      return;
    }
    const rows = navRows();
    if (!rows.length) return;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      e.stopPropagation();
      const idx = rows.indexOf(document.activeElement);
      if (inFilter) {
        (e.key === 'ArrowDown' ? rows[0] : rows[rows.length - 1]).focus();
      } else if (e.key === 'ArrowUp' && idx === 0 && !filterEl.hidden) {
        filterEl.focus();
      } else {
        const next = e.key === 'ArrowDown'
          ? Math.min(rows.length - 1, idx + 1)
          : Math.max(0, idx - 1);
        rows[next].focus();
      }
      document.activeElement?.scrollIntoView?.({ block: 'nearest' });
      return;
    }
    if ((e.key === 'Home' || e.key === 'End') && !inFilter) {
      e.preventDefault();
      e.stopPropagation();
      (e.key === 'Home' ? rows[0] : rows[rows.length - 1]).focus();
      document.activeElement?.scrollIntoView?.({ block: 'nearest' });
      return;
    }
    if (e.key === 'Enter' && inFilter) {
      e.preventDefault();
      e.stopPropagation();
      const top = rows.find(r => (r.dataset.key || '').startsWith('s:'));
      if (top) activateRow(top.dataset.key);
      return;
    }
    if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
      // Contain printable keys: the document-level shortcut layer treats
      // bare letters as approval verbs (y/s/a/n) when focus is not in an
      // input, and popover rows are buttons. Redirect into the filter when
      // it is shown — focusing during keydown lands this keystroke there.
      e.stopPropagation();
      if (!inFilter && !filterEl.hidden) filterEl.focus();
    }
  }

  const wire = () => {
    btnEl = document.getElementById('task-target-switch-btn');
    popEl = document.getElementById('ui2-target-switch');
    if (!btnEl || !popEl) return;

    const eyebrow = document.createElement('div');
    eyebrow.className = 'ui2-tsw-eyebrow';
    eyebrow.textContent = 'Prompt target';
    eyebrow.setAttribute('aria-hidden', 'true');

    filterEl = document.createElement('input');
    filterEl.type = 'text';
    filterEl.className = 'ui2-tsw-filter';
    filterEl.placeholder = 'Filter sessions…';
    filterEl.setAttribute('aria-label', 'Filter sessions');
    filterEl.autocomplete = 'off';
    filterEl.spellcheck = false;
    filterEl.hidden = true;

    rowsEl = document.createElement('div');
    rowsEl.className = 'ui2-tsw-rows';

    const foot = document.createElement('div');
    foot.className = 'ui2-tsw-foot';
    const newRow = makeRow('new', 'ui2-tsw-new');
    const newLabel = document.createElement('span');
    newLabel.className = 'ui2-tsw-row-label';
    newLabel.textContent = '+ New session…';
    newRow.appendChild(newLabel);
    newRow.title = 'Draft a new session from the composer';
    foot.appendChild(newRow);

    popEl.replaceChildren(eyebrow, filterEl, rowsEl, foot);

    btnEl.style.display = '';
    btnEl.setAttribute('aria-controls', 'ui2-target-switch');

    btnEl.addEventListener('click', () => {
      if (isOpen) closePopover({ refocus: true });
      else openPopover();
    });
    btnEl.addEventListener('keydown', (e) => {
      if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
        e.preventDefault();
        openPopover();
      } else if (e.key === 'Escape' && isOpen) {
        e.stopPropagation();
        closePopover({ refocus: true });
      }
    });
    popEl.addEventListener('keydown', onPopKeydown);
    popEl.addEventListener('click', (e) => {
      const row = e.target.closest?.('.ui2-tsw-row');
      if (row && popEl.contains(row)) activateRow(row.dataset.key || '');
    });
    filterEl.addEventListener('input', () => {
      filterQuery = filterEl.value;
      renderRows();
    });
    window.addEventListener('ui2:composer-state', (e) => {
      if ((e?.detail?.state || '') === 'pill') closePopover();
    });
  };
  if (document.readyState === 'complete') wire();
  else document.addEventListener('DOMContentLoaded', wire, { once: true });
})();

/* V3 — the universal index (⌘K).
   Lanes: Actions · Go to · Settings · Search deeper. Register-blind: the
   index always speaks instrument vocabulary regardless of density.
   Settings derive from V3.settingsCatalog — the same table the Settings
   room renders, so a row added there is searchable here, automatically.
   Jump-and-flash: settings land on their exact folded row. */
window.V3 = window.V3 || {};

V3.palette = (function () {
  let sel = 0, items = [];

  const overlay = () => document.getElementById('palette');
  const input = () => document.getElementById('palette-input');
  const results = () => document.getElementById('palette-results');

  /* ---------------- index sources ---------------- */
  function roomItems() {
    return V3.ROOMS.concat(V3.VANTAGES).map(r => ({
      lane: 'Go to', name: r.title, hint: r.gloss, icon: r.icon,
      run: () => V3.go('#/' + r.id)
    }));
  }
  function objectItems() {
    const out = [];
    V3.data.sessions.forEach(s => out.push({
      lane: 'Go to', name: s.name, hint: 'session · ' + (s.backend || '') + (s.active ? ' · working' : ' · ' + s.phase),
      icon: 'stage', run: () => V3.go('#/work/session/' + s.id)
    }));
    V3.data.machines.forEach(m => out.push({
      lane: 'Go to', name: m.petname, hint: 'machine · ' + m.label + ' · via ' + m.route,
      icon: 'machines', run: () => V3.go('#/machines/' + m.id)
    }));
    V3.data.people.forEach(p => out.push({
      lane: 'Go to', name: p.who, hint: 'key · ' + p.role + ' · ' + p.lifecycle,
      icon: 'key', run: () => V3.go('#/people')
    }));
    V3.data.displays.forEach(d => out.push({
      lane: 'Go to', name: 'Display ' + d.id, hint: 'display · ' + (d.live ? 'live' : 'off'),
      icon: 'screens', run: () => V3.go('#/screens')
    }));
    V3.data.agenda.forEach(a => out.push({
      lane: 'Go to', name: a.title, hint: 'the list · ' + a.kind + ' · ' + a.status,
      icon: 'books', run: () => V3.go('#/books')
    }));
    return out;
  }
  function settingsItems() {
    return V3.settingsCatalog.map(row => ({
      lane: 'Settings', name: row.name, hint: row.section, icon: 'settings',
      aliases: row.aliases || '',
      run: () => { V3.go('#/settings'); setTimeout(() => V3.flashTarget(row.fold || ('set-' + row.key)), 80); }
    }));
  }
  function actionItems() {
    const top = V3.queueStore.pending()[0];
    const live = V3.data.sessions.filter(s => s.active);
    const out = [
      { lane: 'Actions', name: 'New session', hint: 'give the house work', icon: 'plus', run: () => V3.go('#/work/new') },
      { lane: 'Actions', name: 'Pair a machine', hint: 'grow the fleet', icon: 'machines',
        run: () => { V3.go('#/machines'); setTimeout(() => V3.flashTarget('fold-pairing'), 80); } },
      { lane: 'Actions', name: 'Add API keys', hint: V3.data.fuel && !V3.data.fuel.openai && !V3.data.fuel.anthropic && !V3.data.fuel.gemini ? 'the daemon is dry' : 'fuel',
        icon: 'fuel', run: () => { V3.go('#/settings'); setTimeout(() => V3.flashTarget('fold-keys'), 80); } },
      { lane: 'Actions', name: 'Speak to the house', hint: 'live voice', icon: 'mic', run: () => V3.actions.toggleVoice() },
      { lane: 'Actions', name: 'Flip theme', hint: 'lamplight ⇄ daylight', icon: 'theme', run: () => V3.cycleTheme() },
      { lane: 'Actions', name: 'Cycle density', hint: 'cozy · standard · studio', icon: 'density', run: () => V3.cycleDensity() },
      { lane: 'Actions', name: 'Show keyboard map', hint: '?', icon: 'question', run: () => V3.showShortcuts() },
      { lane: 'Actions', name: 'Open the classic dashboard', hint: 'V2, right here', icon: 'external', run: () => { location.href = '/'; } }
    ];
    live.forEach(s => out.push({
      lane: 'Actions', name: 'Stop “' + s.name + '”', hint: 'interrupt the turn', icon: 'stop',
      run: () => { V3.actions.interrupt(s.id); V3.toast('Interrupt sent to ' + s.name, 'brick'); }
    }));
    if (top) {
      out.unshift(
        { lane: 'Actions', name: 'Approve: ' + top.title, hint: 'top of the queue · y', icon: 'check',
          run: () => V3.actions.resolveQueueItem(top, 'approve') },
        { lane: 'Actions', name: 'Deny: ' + top.title, hint: 'top of the queue · n', icon: 'x',
          run: () => V3.actions.resolveQueueItem(top, 'deny') }
      );
    }
    return out;
  }
  function deepItems(q) {
    if (!q || q.length < 3) return [];
    const needle = q.toLowerCase();
    const hits = [];
    Object.keys(V3.data.logs).forEach(sid => {
      V3.data.logs[sid].forEach(l => {
        if (hits.length < 4 && l.text.toLowerCase().includes(needle))
          hits.push({ lane: 'Search deeper', name: l.text.slice(0, 72), hint: sid + ' · ' + l.t, icon: 'search', run: () => V3.go('#/work/session/' + sid) });
      });
    });
    V3.data.sessions.forEach(s => {
      if (hits.length < 6 && (s.task || '').toLowerCase().includes(needle))
        hits.push({ lane: 'Search deeper', name: s.task.slice(0, 72) + (s.task.length > 72 ? '…' : ''), hint: s.name, icon: 'search', run: () => V3.go('#/work/session/' + s.id) });
    });
    return hits;
  }

  /* ---------------- fuzzy ---------------- */
  function score(q, item) {
    const hay = (item.name + ' ' + (item.hint || '') + ' ' + (item.aliases || '')).toLowerCase();
    const needle = q.toLowerCase().trim();
    if (!needle) return 1;
    if (hay.startsWith(needle)) return 100;
    const nameLow = item.name.toLowerCase();
    if (nameLow.startsWith(needle)) return 90;
    if (nameLow.includes(needle)) return 70;
    if (hay.includes(needle)) return 50;
    let i = 0;
    for (const c of hay) if (c === needle[i]) i++;
    return i === needle.length ? 25 : 0;
  }

  /* ---------------- render ---------------- */
  function render() {
    const q = input().value;
    const all = [...actionItems(), ...roomItems(), ...objectItems(), ...settingsItems()];
    let matched = all.map(it => ({ it, s: score(q, it) })).filter(x => x.s > 0);
    matched.sort((a, b) => b.s - a.s);
    matched = matched.slice(0, 14);
    const deep = deepItems(q);
    items = matched.map(x => x.it).concat(deep);

    let lastLane = null;
    results().innerHTML = items.map((it, idx) => {
      let h = '';
      if (it.lane !== lastLane) { h += '<div class="pal-lane">' + V3.esc(it.lane) + '</div>'; lastLane = it.lane; }
      h += '<div class="pal-item' + (idx === sel ? ' sel' : '') + '" data-idx="' + idx + '">' +
        V3.ICON(it.icon || 'chev', 15) +
        '<span class="pal-name">' + V3.esc(it.name) + '</span>' +
        (it.hint ? '<span class="pal-hint">' + V3.esc(it.hint) + '</span>' : '') + '</div>';
      return h;
    }).join('') || '<div class="pal-lane">No matches — the index covers every room, object, and setting.</div>';

    results().querySelectorAll('.pal-item').forEach(el => {
      el.addEventListener('click', () => choose(+el.dataset.idx));
      el.addEventListener('mousemove', () => { sel = +el.dataset.idx; paint(); });
    });
  }
  function paint() {
    results().querySelectorAll('.pal-item').forEach(el => el.classList.toggle('sel', +el.dataset.idx === sel));
  }
  function choose(idx) {
    const it = items[idx]; if (!it) return;
    close();
    setTimeout(() => it.run(), 10);
  }
  function onKey(e) {
    if (e.key === 'ArrowDown') { e.preventDefault(); sel = Math.min(sel + 1, items.length - 1); paint(); scrollSel(); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); sel = Math.max(sel - 1, 0); paint(); scrollSel(); }
    else if (e.key === 'Enter') { e.preventDefault(); choose(sel); }
    else if (e.key === 'Escape') { close(); }
  }
  function scrollSel() {
    const el = results().querySelector('.pal-item.sel');
    if (el) el.scrollIntoView({ block: 'nearest' });
  }

  function open() {
    sel = 0;
    overlay().hidden = false;
    input().value = '';
    render();
    setTimeout(() => input().focus(), 20);
    input().oninput = () => { sel = 0; render(); };
    input().onkeydown = onKey;
    overlay().onclick = e => { if (e.target === overlay()) close(); };
  }
  function close() { overlay().hidden = true; }

  return { open, close, get isOpen() { return !overlay().hidden; } };
})();

/* Proscenium — the universal index (⌘K).
   Lanes: Actions · Go to · Settings · Search deeper. Register-blind: the
   index always speaks instrument vocabulary regardless of density.
   Jump-and-flash: settings land on their exact folded row. */
window.P = window.P || {};

P.palette = (function () {
  let sel = 0, items = [], deepTimer = null;

  const overlay = () => document.getElementById('palette');
  const input = () => document.getElementById('palette-input');
  const results = () => document.getElementById('palette-results');

  /* ---------------- index sources ---------------- */
  function roomItems() {
    return P.ROOMS.map(r => ({
      lane: 'Go to', name: r.title, hint: r.gloss, icon: r.icon,
      run: () => P.go('#/' + r.id)
    }));
  }
  function objectItems() {
    const out = [];
    P.data.sessions.forEach(s => out.push({
      lane: 'Go to', name: s.name, hint: 'session · ' + s.backend + ' · ' + P.machineName(s.machine),
      icon: 'stage', run: () => P.go('#/work/session/' + s.id)
    }));
    P.data.machines.forEach(m => out.push({
      lane: 'Go to', name: m.petname, hint: 'machine · ' + m.label + ' · via ' + m.route,
      icon: 'machines', run: () => P.go('#/machines/' + m.id)
    }));
    P.data.people.forEach(p => out.push({
      lane: 'Go to', name: p.who, hint: 'key · ' + p.role + ' · ' + p.lifecycle,
      icon: 'key', run: () => P.go('#/people')
    }));
    P.data.displays.forEach(d => out.push({
      lane: 'Go to', name: d.name, hint: 'display · ' + (d.live ? 'live' : 'private'),
      icon: 'screens', run: () => P.go('#/screens')
    }));
    P.data.agenda.forEach(a => out.push({
      lane: 'Go to', name: a.title, hint: 'the list · ' + a.kind + ' · ' + a.status,
      icon: 'books', run: () => P.go('#/books')
    }));
    return out;
  }
  function settingsItems() {
    const out = [];
    P.data.settings.forEach(row => {
      const mk = (name, hint) => ({
        lane: 'Settings', name: name, hint: hint, icon: 'settings',
        run: () => { P.go('#/settings'); setTimeout(() => P.flashTarget(row.fold || ''), 60); }
      });
      out.push(mk(row.name, row.section));
      (row.rows || []).forEach(r => out.push(mk(r[0], row.name + ' · ' + row.section)));
    });
    return out;
  }
  function actionItems() {
    const top = P.queueStore.pending()[0];
    const out = [
      {
        lane: 'Actions', name: 'New session', hint: 'give the house work', icon: 'plus',
        run: () => P.go('#/work/new')
      },
      {
        lane: 'Actions', name: 'Pair a machine', hint: 'grow the fleet', icon: 'machines',
        run: () => { P.go('#/machines'); setTimeout(() => P.flashTarget('fold-pairing'), 60); }
      },
      {
        lane: 'Actions', name: 'Fuel from your vault', hint: 'keys & leases', icon: 'fuel',
        run: () => { P.go('#/people'); setTimeout(() => P.flashTarget('fold-vault'), 60); }
      },
      {
        lane: 'Actions', name: 'Renew Workshop’s lease', hint: '2 days left', icon: 'fuel',
        run: () => P.toast('Lease renewed — Workshop stays fueled', 'sage')
      },
      {
        lane: 'Actions', name: 'Take control of Display 1', hint: 'input authority', icon: 'hand',
        run: () => P.go('#/screens')
      },
      {
        lane: 'Actions', name: 'Stop “fix-login”', hint: 'interrupt the turn', icon: 'stop',
        run: () => P.toast('Interrupt sent to fix-login', 'brick')
      },
      {
        lane: 'Actions', name: 'Flip theme', hint: 'lamplight ⇄ daylight', icon: 'theme',
        run: () => P.cycleTheme()
      },
      {
        lane: 'Actions', name: 'Cycle density', hint: 'cozy · standard · studio', icon: 'density',
        run: () => P.cycleDensity()
      },
      {
        lane: 'Actions', name: 'Show keyboard map', hint: '?', icon: 'question',
        run: () => P.showShortcuts()
      }
    ];
    if (top) {
      out.unshift(
        {
          lane: 'Actions', name: 'Approve: ' + top.title, hint: 'top of the queue · y', icon: 'check',
          run: () => P.queueStore.resolve(top.id, top.actions[0].id)
        },
        {
          lane: 'Actions', name: 'Deny: ' + top.title, hint: 'top of the queue · n', icon: 'x',
          run: () => P.queueStore.resolve(top.id, 'deny')
        }
      );
    }
    return out;
  }
  function deepItems(q) {
    if (!q || q.length < 3) return [];
    const hits = [];
    const needle = q.toLowerCase();
    P.data.fixLoginLog.forEach(l => {
      if (l[2].toLowerCase().includes(needle))
        hits.push({ lane: 'Search deeper', name: l[2], hint: 'fix-login · ' + l[0], icon: 'search', run: () => P.go('#/work/session/fix-login') });
    });
    P.data.sessions.forEach(s => {
      if (s.task.toLowerCase().includes(needle))
        hits.push({ lane: 'Search deeper', name: s.task.slice(0, 80) + '…', hint: s.name, icon: 'search', run: () => P.go('#/work/session/' + s.id) });
    });
    return hits.slice(0, 4);
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
    // subsequence
    let i = 0;
    for (const c of hay) if (c === needle[i]) i++;
    return i === needle.length ? 25 : 0;
  }

  /* alias lookup for settings vocabulary */
  function withAliases(it) {
    const row = P.data.settings.find(r => r.name === it.name || (r.rows || []).some(x => x[0] === it.name));
    if (row) it.aliases = row.aliases || '';
    return it;
  }

  /* ---------------- render ---------------- */
  function render() {
    const q = input().value;
    const all = [...actionItems(), ...roomItems(), ...objectItems(), ...settingsItems().map(withAliases)];
    let matched = all.map(it => ({ it, s: score(q, it) })).filter(x => x.s > 0);
    matched.sort((a, b) => b.s - a.s);
    matched = matched.slice(0, 14);
    const deep = deepItems(q);
    items = matched.map(x => x.it).concat(deep);

    const lanes = [];
    let lastLane = null;
    const html = items.map((it, idx) => {
      let h = '';
      if (it.lane !== lastLane) { h += '<div class="pal-lane">' + P.esc(it.lane) + '</div>'; lastLane = it.lane; }
      h += '<div class="pal-item' + (idx === sel ? ' sel' : '') + '" data-idx="' + idx + '">' +
        P.ICON(it.icon || 'chev', 15) +
        '<span class="pal-name">' + P.esc(it.name) + '</span>' +
        (it.hint ? '<span class="pal-hint">' + P.esc(it.hint) + '</span>' : '') + '</div>';
      return h;
    }).join('');
    results().innerHTML = html || '<div class="pal-lane">No matches — the index covers every room, object, and setting.</div>';

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

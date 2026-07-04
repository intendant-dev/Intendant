(() => {
  // In-page expression for scripts/validate-dashboard.cjs --wait-for-function.
  // SMOKE is the fixture directory on the daemon host; seed it with hello.rs
  // (containing "hello from the files tab") before running, or inject another
  // path first via --wait-for-function "window.__IDE_SMOKE_DIR='...'||true".
  const SMOKE = window.__IDE_SMOKE_DIR || '/tmp/files-ide-smoke';
  const ide = window.intendantDashboardFilesIde;
  if (!ide || !document.querySelector('.tab-btn[data-tab="files"]')) return false;
  if (!window.__ideSmoke) {
    window.__ideSmoke = { state: 'running', steps: [] };
    (async () => {
      const S = window.__ideSmoke;
      const step = name => S.steps.push(name);
      const assert = (cond, why) => { if (!cond) throw new Error('assert failed: ' + why); };
      const sleep = ms => new Promise(r => setTimeout(r, ms));
      try {
        document.querySelector('.tab-btn[data-tab="files"]').click();
        await sleep(300);
        assert(document.querySelector('#files-pane-editor .files-ide-body'), 'editor workbench rendered');
        const bodyRect = document.querySelector('.files-ide-body').getBoundingClientRect();
        assert(bodyRect.width >= window.innerWidth * 0.9, 'editor uses full pane width, got ' + bodyRect.width + '/' + window.innerWidth);
        step('files tab + editor card');

        let snap = await ide._debugSetRoot(SMOKE);
        // The daemon canonicalizes the root (macOS: /tmp -> /private/tmp);
        // adopt its form for every path we send afterwards.
        assert(snap.root === SMOKE || snap.root.endsWith(SMOKE), 'tree rooted at smoke dir, got ' + snap.root);
        const ROOT = snap.root;
        assert(document.querySelectorAll('.files-ide-tree-row').length >= 3, 'tree rows rendered');
        step('tree lists smoke dir');

        snap = await ide._debugOpen(ROOT + '/hello.rs');
        assert(window.CodeMirror, 'editor bundle lazy-loaded');
        assert(document.querySelector('.files-ide-editor-host .CodeMirror'), 'CodeMirror mounted');
        assert(snap.active && snap.active.path.endsWith('hello.rs'), 'hello.rs active');
        assert(snap.active.text.includes('hello from the files tab'), 'content loaded');
        assert(/^[0-9a-f]{64}$/.test(snap.active.baselineSha), 'baseline sha captured');
        assert(snap.active.dirty === false, 'buffer clean after open');
        step('open + content + sha baseline');

        ide._debugSetText('fn main() {\n    println!("edited in the dashboard IDE");\n}\n');
        snap = ide._debugSnapshot();
        assert(snap.active.dirty === true, 'dirty after edit');
        snap = await ide._debugSave();
        assert(snap.saveStatus.startsWith('Saved'), 'save status shows Saved, got ' + snap.saveStatus);
        assert(snap.active.dirty === false, 'clean after save');
        step('edit + save');

        const external = 'fn main() {\n    println!("changed behind the editor\'s back");\n}\n';
        const forceResp = await fetch('/api/fs/write', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ path: ROOT + '/hello.rs', content: external, force: true }),
        });
        assert(forceResp.ok, 'external force write ok');
        ide._debugSetText('fn main() {\n    println!("this save must conflict");\n}\n');
        snap = await ide._debugSave();
        assert(snap.openTabs.some(t => t.conflict === 'conflict'), 'conflict recorded');
        assert(snap.banner.includes('changed on disk'), 'conflict banner shown, got ' + snap.banner);
        step('external change detected as 409 conflict');

        await window.filesIdeReloadActive();
        snap = ide._debugSnapshot();
        assert(snap.active.text === external, 'reload pulled disk content');
        assert(snap.banner === '', 'banner cleared after reload');
        step('reload from disk');

        ide._debugSetText('fn main() {\n    println!("final from the dashboard IDE");\n}\n');
        snap = await ide._debugSave();
        assert(snap.saveStatus.startsWith('Saved'), 'post-reload save ok');
        step('post-reload save');

        snap = await ide._debugOpen(ROOT + '/created-in-ui.toml', { createNew: true });
        assert(snap.active.path.endsWith('created-in-ui.toml'), 'new buffer active');
        ide._debugSetText('[ui]\nmade = true\n');
        snap = await ide._debugSave();
        assert(snap.saveStatus.startsWith('Saved'), 'new file saved');
        assert(snap.openTabs.length === 2, 'two tabs open');
        assert(document.querySelectorAll('.files-ide-tab').length === 2, 'tab strip shows two tabs');
        step('create-new file via editor');

        const modeLine = document.getElementById('files-ide-status-meta').textContent;
        assert(/TOML/i.test(modeLine), 'language detected for toml, got ' + modeLine);
        assert(document.querySelectorAll('.files-ide-editor-host .cm-keyword, .files-ide-editor-host .cm-atom, .files-ide-editor-host .cm-property').length > 0, 'syntax highlighting active');
        step('language detection + highlighting');

        // Rename the open buffer's file: the tab retargets, no data moves
        // through the browser, and the old name is gone from the listing.
        snap = await ide._debugRename(ROOT + '/created-in-ui.toml', 'renamed-in-ui.toml');
        assert(snap.active.path.endsWith('renamed-in-ui.toml'), 'active buffer retargeted, got ' + snap.active.path);
        assert(snap.openTabs.length === 2, 'still two tabs after rename');
        const treeStatus = document.getElementById('files-ide-tree-status').textContent;
        assert(!treeStatus, 'rename left no error, got ' + treeStatus);
        const rowPaths = Array.from(document.querySelectorAll('.files-ide-tree-row')).map(r => r.dataset.path);
        assert(rowPaths.some(p => p.endsWith('renamed-in-ui.toml')), 'renamed entry listed');
        assert(!rowPaths.some(p => p.endsWith('created-in-ui.toml')), 'old name gone from listing');
        step('rename retargets open tab');

        // Row actions exist on tree rows (hover-revealed).
        assert(document.querySelector('.files-ide-tree-row [data-act="rename"]'), 'rename row action present');
        assert(document.querySelector('.files-ide-tree-row [data-act="delete"]'), 'delete row action present');
        step('row actions rendered');

        // Find-in-file: matches counted, stepping wraps.
        await ide._debugOpen(ROOT + '/hello.rs');
        const find = await ide._debugFind('println');
        assert(find.open === true, 'find bar open');
        assert(find.total >= 1, 'find counted matches, got ' + find.total);
        assert(!document.getElementById('files-ide-find').classList.contains('hidden'), 'find bar visible');
        assert(document.querySelectorAll('.files-ide-editor-host .files-ide-find-match').length >= 1, 'matches highlighted');
        const stepped = ide._debugFindStep(1);
        assert(stepped.index === (find.index + 1) % find.total || find.total === 1, 'find stepped');
        window.filesIdeCloseFind();
        assert(document.getElementById('files-ide-find').classList.contains('hidden'), 'find bar closed');
        step('find in file');

        // Delete a clean file: its tab closes and the row disappears.
        const del = await ide._debugDelete(ROOT + '/renamed-in-ui.toml');
        assert(del.openTabs.length === 1, 'deleted file tab closed, got ' + del.openTabs.length);
        assert(!del.treeStatus, 'delete left no error, got ' + del.treeStatus);
        const afterDelete = Array.from(document.querySelectorAll('.files-ide-tree-row')).map(r => r.dataset.path);
        assert(!afterDelete.some(p => p.endsWith('renamed-in-ui.toml')), 'deleted entry gone from listing');
        step('delete closes clean tab');

        // Delete the still-open dirty buffer's file: the tab survives with
        // the missing-file banner so unsaved work is not thrown away.
        ide._debugSetText('fn main() {\n    println!("unsaved survivor");\n}\n');
        const orphan = await ide._debugDelete(ROOT + '/hello.rs');
        assert(orphan.openTabs.length === 1, 'dirty tab survives delete');
        assert(orphan.openTabs[0].conflict === 'missing', 'dirty tab flagged missing, got ' + orphan.openTabs[0].conflict);
        assert(orphan.banner.includes('no longer exists'), 'missing banner shown, got ' + orphan.banner);
        await window.filesIdeOverwriteActive();
        snap = ide._debugSnapshot();
        assert(snap.active.dirty === false, 'overwrite recreated the file');
        step('delete orphans dirty buffer; overwrite recreates');

        S.state = 'done';
      } catch (e) {
        S.state = 'fail';
        S.error = String((e && e.message) || e);
      }
    })();
    return false;
  }
  if (window.__ideSmoke.state === 'running') return false;
  if (window.__ideSmoke.state === 'fail') {
    throw new Error('IDE smoke failed after [' + window.__ideSmoke.steps.join(', ') + ']: ' + window.__ideSmoke.error);
  }
  return 'IDE-SMOKE-PASS: ' + window.__ideSmoke.steps.join(' | ');
})()

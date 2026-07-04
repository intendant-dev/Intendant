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
        assert(snap.root === SMOKE, 'tree rooted at smoke dir, got ' + snap.root);
        assert(document.querySelectorAll('.files-ide-tree-row').length >= 3, 'tree rows rendered');
        step('tree lists smoke dir');

        snap = await ide._debugOpen(SMOKE + '/hello.rs');
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
          body: JSON.stringify({ path: SMOKE + '/hello.rs', content: external, force: true }),
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

        snap = await ide._debugOpen(SMOKE + '/created-in-ui.toml', { createNew: true });
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

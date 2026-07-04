'use strict';
// Two-daemon Files-IDE smoke: browser -> daemon A (18800, plain HTTP) ->
// dashboard-control tunnel to peer daemon B (18801, mTLS) -> B's disk,
// with B enforcing profile file-operator + write_roots=[peer-files].
const fs = require('fs');
const path = require('path');
const { chromium } = require('playwright');

const RIG = process.env.RIG || '/tmp/files-ide-peer-rig';
const PEER_FILES = RIG + '/peer-files';
const OUTSIDE = RIG + '/outside';
const PEER_ID = process.env.PEER_ID || 'intendant:peer-b';

const steps = [];
function step(name) { steps.push(name); console.log('STEP ok:', name); }
function assert(cond, why) { if (!cond) throw new Error('assert failed: ' + why); }

(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1500, height: 950 } });
  page.on('pageerror', e => console.log('PAGEERROR:', String(e).slice(0, 300)));
  try {
    await page.goto((process.env.DASH_URL || 'http://127.0.0.1:18800') + '/app', { waitUntil: 'domcontentloaded' });
    await page.waitForFunction(() => window.intendantDashboardFilesIde && document.querySelector('.tab-btn[data-tab="files"]'), null, { timeout: 30000 });
    await page.click('.tab-btn[data-tab="files"]');
    await page.waitForSelector('#files-pane-editor .files-ide-body', { timeout: 10000 });
    const layout = await page.evaluate(() => {
      const body = document.querySelector('.files-ide-body').getBoundingClientRect();
      return { w: body.width / window.innerWidth, h: body.height / window.innerHeight };
    });
    assert(layout.w >= 0.9, 'editor uses the full pane width, got ' + layout.w.toFixed(2));
    assert(layout.h >= 0.5, 'editor fills most of the pane height, got ' + layout.h.toFixed(2));
    step('dashboard loaded, Files tab open');

    // Peer appears in the editor host select once A's registry syncs.
    await page.waitForFunction(id => {
      const sel = document.getElementById('files-ide-host');
      return sel && Array.from(sel.options).some(o => o.value === id);
    }, PEER_ID, { timeout: 30000 });
    await page.evaluate(id => {
      const sel = document.getElementById('files-ide-host');
      sel.value = id;
      sel.dispatchEvent(new Event('change'));
    }, PEER_ID);
    step('peer target selected in editor');

    // Rooting the tree at B's granted dir opens the WebRTC tunnel on demand.
    const rootSnap = await page.evaluate(async dir =>
      window.intendantDashboardFilesIde._debugSetRoot(dir), PEER_FILES);
    assert(rootSnap.host === PEER_ID, 'snapshot host is the peer, got ' + rootSnap.host);
    // B canonicalizes the root (macOS: /tmp -> /private/tmp); adopt its form
    // for every path we send to it afterwards.
    assert(rootSnap.root === PEER_FILES || rootSnap.root.endsWith(PEER_FILES), 'tree rooted at peer dir, got ' + rootSnap.root);
    const ROOT = rootSnap.root;
    const treeNames = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.files-ide-tree-row .files-ide-tree-name')).map(e => e.textContent));
    assert(treeNames.some(n => n.includes('peer-note.md')), 'peer-note.md listed, got ' + treeNames.join(','));
    assert(treeNames.some(n => n.includes('sub')), 'sub/ listed');
    step('peer directory listed over the tunnel');

    // Open, edit, save on the peer.
    let snap = await page.evaluate(async p =>
      window.intendantDashboardFilesIde._debugOpen(p), ROOT + '/peer-note.md');
    assert(snap.active && snap.active.host === PEER_ID, 'buffer bound to peer host');
    assert(snap.active.text.includes('Edited from another daemon soon'), 'peer file content loaded');
    assert(/^[0-9a-f]{64}$/.test(snap.active.baselineSha), 'sha baseline from peer read');
    step('peer file opened with sha baseline');

    await page.evaluate(() => window.intendantDashboardFilesIde._debugSetText(
      '# Peer note\n\nEdited from daemon A through the dashboard tunnel.\n'));
    snap = await page.evaluate(async () => window.intendantDashboardFilesIde._debugSave());
    assert(snap.saveStatus.startsWith('Saved'), 'peer save reported, got ' + snap.saveStatus);
    const onDisk1 = fs.readFileSync(PEER_FILES + '/peer-note.md', 'utf8');
    assert(onDisk1.includes('through the dashboard tunnel'), 'peer disk updated: ' + JSON.stringify(onDisk1));
    step('peer save landed on B\'s disk');
    await page.screenshot({ path: path.join(RIG, 'shot-1-peer-edit.png') });

    // External change on B's disk -> stale-sha save must 409.
    fs.writeFileSync(PEER_FILES + '/peer-note.md', '# Peer note\n\nB changed this behind the tunnel.\n');
    await page.evaluate(() => window.intendantDashboardFilesIde._debugSetText('# Peer note\n\nthis must conflict\n'));
    snap = await page.evaluate(async () => window.intendantDashboardFilesIde._debugSave());
    assert(snap.openTabs.some(t => t.conflict === 'conflict'), 'conflict recorded on peer save');
    assert(snap.banner.includes('changed on disk'), 'conflict banner, got ' + snap.banner);
    await page.screenshot({ path: path.join(RIG, 'shot-2-conflict.png') });
    await page.evaluate(async () => window.filesIdeReloadActive());
    snap = await page.evaluate(() => window.intendantDashboardFilesIde._debugSnapshot());
    assert(snap.active.text.includes('B changed this'), 'reload pulled B\'s disk content');
    step('conflict detected across daemons, reload recovered');

    // Write DENIAL: outside B's write_roots. B must refuse; nothing created.
    await page.evaluate(async p =>
      window.intendantDashboardFilesIde._debugOpen(p, { createNew: true }), OUTSIDE + '/denied.txt');
    await page.evaluate(() => window.intendantDashboardFilesIde._debugSetText('should never land'));
    snap = await page.evaluate(async () => window.intendantDashboardFilesIde._debugSave());
    const denialText = (snap.banner + ' ' + snap.saveStatus).toLowerCase();
    assert(/outside|not allowed|denied|forbidden|roots/.test(denialText), 'denial surfaced, got ' + denialText);
    assert(!fs.existsSync(OUTSIDE + '/denied.txt'), 'denied file must not exist on B');
    step('write outside write_roots denied by B');
    await page.screenshot({ path: path.join(RIG, 'shot-3-denied.png') });

    // Read DENIAL: outside is not in read_roots either.
    await page.evaluate(async dir =>
      window.intendantDashboardFilesIde._debugSetRoot(dir).catch(() => null), OUTSIDE);
    const treeStatus = await page.evaluate(() => document.getElementById('files-ide-tree-status').textContent);
    assert(/outside|denied|not allowed|failed|roots/i.test(treeStatus), 'read denial in tree status, got ' + treeStatus);
    step('read outside roots denied by B');

    // Create a new file on the peer inside the grant.
    await page.evaluate(async dir => window.intendantDashboardFilesIde._debugSetRoot(dir), PEER_FILES);
    await page.evaluate(async p =>
      window.intendantDashboardFilesIde._debugOpen(p, { createNew: true }), ROOT + '/made-on-a.md');
    await page.evaluate(() => window.intendantDashboardFilesIde._debugSetText('created on daemon B from daemon A\'s dashboard\n'));
    snap = await page.evaluate(async () => window.intendantDashboardFilesIde._debugSave());
    assert(snap.saveStatus.startsWith('Saved'), 'create-new on peer saved, got ' + snap.saveStatus);
    assert(fs.readFileSync(PEER_FILES + '/made-on-a.md', 'utf8').includes('created on daemon B'), 'new file on B disk');
    step('new file created on peer');
    await page.screenshot({ path: path.join(RIG, 'shot-4-final.png') });

    // Rename on the peer (api_fs_rename over the tunnel): B's disk moves the
    // file, the open tab retargets, and the browser never sees the bytes.
    snap = await page.evaluate(async p =>
      window.intendantDashboardFilesIde._debugRename(p, 'renamed-on-a.md'), ROOT + '/made-on-a.md');
    assert(snap.active.path.endsWith('renamed-on-a.md'), 'peer rename retargeted tab, got ' + snap.active.path);
    assert(!fs.existsSync(PEER_FILES + '/made-on-a.md'), 'old name gone on B');
    assert(fs.readFileSync(PEER_FILES + '/renamed-on-a.md', 'utf8').includes('created on daemon B'), 'renamed file kept content on B');
    step('rename executed on peer disk');

    // Rename DENIAL: a destination outside write_roots must be refused by
    // B. The inline rename UI can only target the same directory, so this
    // exercises the raw tunnel RPC — the lane a hostile client would use.
    // The denial surfaces as a non-ok envelope on the HTTP lane and as a
    // thrown tunnel error on the peer lane — accept either shape.
    const renameDenied = await page.evaluate(async ({ from, to }) =>
      window.intendantDashboardFilesIde._debugRawRename(from, to)
        .then(resp => ({ ok: resp.ok, detail: (resp.body && resp.body.error) || '' }))
        .catch(e => ({ ok: false, detail: String((e && e.message) || e) })),
      { from: ROOT + '/renamed-on-a.md', to: OUTSIDE + '/escape.md' });
    assert(renameDenied.ok === false, 'cross-root rename refused, got ' + JSON.stringify(renameDenied));
    assert(/outside|roots|denied|forbidden/i.test(renameDenied.detail), 'denial names the scope, got ' + renameDenied.detail);
    assert(!fs.existsSync(OUTSIDE + '/escape.md'), 'no file escaped the write roots');
    assert(fs.existsSync(PEER_FILES + '/renamed-on-a.md'), 'source untouched after denial');
    step('rename outside write_roots denied by B');

    // Delete on the peer: clean tab closes, file gone from B's disk.
    snap = await page.evaluate(async p =>
      window.intendantDashboardFilesIde._debugDelete(p), ROOT + '/renamed-on-a.md');
    assert(!fs.existsSync(PEER_FILES + '/renamed-on-a.md'), 'deleted on B disk');
    assert(!snap.openTabs.some(t => t.path.endsWith('renamed-on-a.md')), 'deleted file tab closed');
    assert(!snap.treeStatus, 'delete left no error, got ' + snap.treeStatus);
    step('delete executed on peer disk');
    await page.screenshot({ path: path.join(RIG, 'shot-5-after-rename-delete.png') });

    console.log('PEER-IDE-SMOKE PASS:', steps.length, 'steps');
  } catch (e) {
    console.log('PEER-IDE-SMOKE FAIL after [' + steps.join(' | ') + ']:', e.message);
    try { await page.screenshot({ path: path.join(RIG, 'shot-fail.png') }); } catch (_) {}
    process.exitCode = 1;
  } finally {
    await browser.close();
  }
})();

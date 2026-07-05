#!/usr/bin/env node
// Session-vitals smoke: mock daemon in a dirty git repo; expect a
// session_vitals event carrying the git segment AND the cache segment
// (the mock provider emits cache-bearing usage, the native derivation
// forwards it, the hub merges both sections) on the control socket.
const { spawn, execSync } = require('child_process');
const fs = require('fs');
const net = require('net');
const os = require('os');
const path = require('path');

const BINARY = process.argv[2];
const HOME = fs.mkdtempSync(path.join(os.tmpdir(), 'vitals-home-'));
const PROJ = fs.mkdtempSync(path.join(os.tmpdir(), 'vitals-proj-'));
const PORT = 18997;

// The project is a real git repo: one commit, a feature branch one ahead
// of main, plus a dirty file.
const g = (args) => execSync(`git -c user.email=t@t -c user.name=t -c commit.gpgsign=false ${args}`, { cwd: PROJ });
g('init -q -b main');
fs.writeFileSync(path.join(PROJ, 'a.txt'), 'one\n');
g('add .'); g('commit -qm base');
g('checkout -qb feature');
fs.writeFileSync(path.join(PROJ, 'b.txt'), 'two\n');
g('add .'); g('commit -qm work');
fs.writeFileSync(path.join(PROJ, 'a.txt'), 'one modified\n');

const script = { profiles: [{ steps: [
  { content: 'ok', tool_calls: [{ name: 'signal_done', arguments: { message: 'vitals smoke task done' } }] },
]}]};
const scriptPath = path.join(HOME, 'mock_script.json');
fs.writeFileSync(scriptPath, JSON.stringify(script));

const env = { ...process.env, HOME, USERPROFILE: HOME, PROVIDER: 'mock', INTENDANT_MOCK_SCRIPT: scriptPath };
for (const k of ['OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'MODEL_NAME']) delete env[k];

const child = spawn(BINARY, ['--no-tui', '--web', String(PORT), '--bind', '127.0.0.1', '--no-tls',
  '--control-socket', '--autonomy', 'full', 'trivial task'], { cwd: PROJ, env, stdio: ['ignore', 'pipe', 'pipe'] });
let exited = false;
child.on('exit', () => { exited = true; });
child.stderr.on('data', () => {});
child.stdout.on('data', () => {});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
(async () => {
  const socketPath = `/tmp/intendant-${child.pid}.sock`;
  const deadline = Date.now() + 30000;
  let sock = null;
  while (Date.now() < deadline && !sock) {
    if (exited) throw new Error('daemon exited early');
    if (fs.existsSync(socketPath)) {
      try {
        sock = await new Promise((resolve, reject) => {
          const s = net.createConnection(socketPath);
          s.on('connect', () => resolve(s));
          s.on('error', reject);
        });
      } catch { /* retry */ }
    }
    if (!sock) await sleep(250);
  }
  if (!sock) throw new Error('no control socket');

  // The startup emission raced our connect; change the tree so the next
  // probe tick (5s cadence) emits again while we listen.
  fs.writeFileSync(path.join(PROJ, 'c.txt'), 'three\n');

  let buf = '';
  const done = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('timeout waiting for session_vitals')), 30000);
    sock.on('data', (d) => {
      buf += d.toString();
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i); buf = buf.slice(i + 1);
        if (!line.includes('session_vitals')) continue;
        let e; try { e = JSON.parse(line); } catch { continue; }
        if (e.event !== 'session_vitals') continue;
        clearTimeout(timer);
        resolve(e);
      }
    });
  });
  const e = await done;
  const git = e.vitals && e.vitals.git;
  const gitOk = git && git.branch === 'feature' && git.dirtyFiles === 2
    && git.primaryRef === 'main' && git.ahead === 1 && git.behind === 0
    && git.mergeParity === 'clean';
  // The startup task already ran (mock is instant), so its cache sample
  // (first request: writes only, TTL hint 300) merged into the hub before
  // this git-change emission — the snapshot carries both sections.
  const cache = e.vitals && e.vitals.cache;
  const cacheOk = cache && cache.hitPct === 0 && cache.ttlSeconds === 300
    && Number(cache.lastActivityEpoch) > 0;
  const ok = gitOk && cacheOk;
  console.log(`vitals event: ${JSON.stringify(e).slice(0, 400)}`);
  if (!gitOk) console.log('git segment mismatch');
  if (!cacheOk) console.log('cache segment mismatch');
  console.log(ok ? 'VITALS SMOKE PASS' : 'VITALS SMOKE FAIL');
  child.kill('SIGTERM');
  process.exit(ok ? 0 : 1);
})().catch((err) => { console.error('SMOKE ERROR: ' + err.message); child.kill('SIGTERM'); process.exit(1); });

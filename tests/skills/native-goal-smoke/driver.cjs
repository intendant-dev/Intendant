#!/usr/bin/env node
// Native-session goal smoke: drives a mock-provider intendant daemon over
// the Unix control socket. Keyless. Proves: goal-set answers + broadcasts
// session_goal, goal-get reports, the goal notice preludes the next task's
// prompt (mock asserts transcript contains it), goal-clear broadcasts null,
// and status ops without a goal fail honestly.
const { spawn } = require('child_process');
const fs = require('fs');
const net = require('net');
const os = require('os');
const path = require('path');

const BINARY = process.argv[2] || path.resolve(__dirname, '../../../..', 'target/release/intendant');
const HOME = fs.mkdtempSync(path.join(os.tmpdir(), 'native-goal-home-'));
const PROJ = fs.mkdtempSync(path.join(os.tmpdir(), 'native-goal-proj-'));
const PORT = 18994;

const script = {
  profiles: [{
    steps: [
      {
        content: 'Initial task done.',
        tool_calls: [{ name: 'signal_done', arguments: { message: 'initial task complete' } }],
      },
      {
        expect_transcript_contains: '[Operator goal] smoke goal',
        content: 'Goal noted; finishing.',
        tool_calls: [{ name: 'signal_done', arguments: { message: 'native goal smoke complete' } }],
      },
    ],
  }],
};
const scriptPath = path.join(HOME, 'mock_script.json');
fs.writeFileSync(scriptPath, JSON.stringify(script, null, 2));

const t0 = Date.now();
const ts = () => ((Date.now() - t0) / 1000).toFixed(1).padStart(6) + 's';
const log = (tag, line) => console.log(`[${ts()} ${tag}] ${line}`);
const checks = [];
const check = (name, ok, detail = '') => {
  checks.push({ name, ok, detail });
  log('check', `${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const env = { ...process.env, HOME, USERPROFILE: HOME, PROVIDER: 'mock', INTENDANT_MOCK_SCRIPT: scriptPath };
for (const k of ['OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'MODEL_NAME',
  'PRESENCE_PROVIDER', 'PRESENCE_MODEL', 'CU_PROVIDER', 'CU_MODEL']) delete env[k];

// An initial task is required: idle web startups take the daemon path,
// which does not spawn the control socket — with a task the run lands in
// the web-TUI branch whose worker loop is run_with_presence (the native
// goal engine's home).
const child = spawn(BINARY, ['--no-tui', '--web', String(PORT), '--bind', '127.0.0.1', '--no-tls',
  '--control-socket', '--autonomy', 'full', 'do the initial smoke task'], {
  cwd: PROJ, env, stdio: ['ignore', 'pipe', 'pipe'],
});
let sessionId = null;
const scanForSession = (l) => {
  const m = l.match(/Session ID: ([0-9a-f-]{36})/);
  if (m) { sessionId = m[1]; log('driver', `session id ${sessionId}`); }
};
child.stdout.on('data', (d) => d.toString().split('\n').forEach((l) => { if (l.trim()) { log('out', l.trim().slice(0, 220)); scanForSession(l); } }));
child.stderr.on('data', (d) => d.toString().split('\n').forEach((l) => { if (l.trim()) { log('err', l.trim().slice(0, 220)); scanForSession(l); } }));
let exited = false;
child.on('exit', (code, sig) => { exited = true; log('daemon', `exited code=${code} sig=${sig}`); });

const events = [];
const waiters = [];
let sock = null;
function onLine(line) {
  let event;
  try { event = JSON.parse(line); } catch { return; }
  events.push(event);
  const name = event.event || '?';
  if (!['model_response_delta', 'presence_log', 'log', 'usage_update', 'status'].includes(name)) {
    log('ev', line.slice(0, 240));
  }
  for (const w of [...waiters]) {
    if (w.predicate(event)) {
      waiters.splice(waiters.indexOf(w), 1);
      w.resolve(event);
    }
  }
}
function waitFor(description, predicate, timeoutMs = 30000) {
  const seen = events.find(predicate);
  if (seen) return Promise.resolve(seen);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`timeout waiting for ${description}`)), timeoutMs);
    waiters.push({ predicate, resolve: (e) => { clearTimeout(timer); resolve(e); } });
  });
}
const send = (msg) => { log('ctl', JSON.stringify(msg).slice(0, 200)); sock.write(JSON.stringify(msg) + '\n'); };

async function connect() {
  const socketPath = `/tmp/intendant-${child.pid}.sock`;
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    if (exited) throw new Error('daemon exited before socket came up');
    if (fs.existsSync(socketPath)) {
      try {
        await new Promise((resolve, reject) => {
          const s = net.createConnection(socketPath);
          let buf = '';
          s.on('connect', () => { sock = s; resolve(); });
          s.on('error', reject);
          s.on('data', (d) => {
            buf += d.toString();
            let i;
            while ((i = buf.indexOf('\n')) >= 0) { const l = buf.slice(0, i); buf = buf.slice(i + 1); if (l.trim()) onLine(l); }
          });
        });
        log('driver', `connected to ${socketPath}`);
        return;
      } catch { /* not accepting yet */ }
    }
    await sleep(250);
  }
  throw new Error('control socket never came up');
}

const goalEvent = (e) => e.event === 'session_goal';
const resultEvent = (e) => e.event === 'codex_thread_action_result';

(async () => {
  await connect();

  // The mock's initial task completes in milliseconds — before the socket
  // client subscribes — so its events are unobservable here; the goal ops
  // below exercise the idle path and are request/response (can't race).

  // 1. Set a goal while idle.
  send({ action: 'codex_thread_action', op: 'goal-set', session_id: sessionId, params: { objective: 'smoke goal', tokenBudget: 5000 } });
  const setRes = await waitFor('goal-set result', (e) => resultEvent(e) && /goal-set/.test(e.action || ''));
  check('goal-set-succeeds', setRes.success === true && /smoke goal/.test(setRes.message || ''),
    `success=${setRes.success} msg=${setRes.message}`);
  const setGoal = await waitFor('session_goal broadcast', (e) => goalEvent(e) && JSON.stringify(e).includes('smoke goal'));
  check('goal-set-broadcasts', Boolean(setGoal), JSON.stringify(setGoal.goal || {}).slice(0, 120));

  // 2. Read it back.
  send({ action: 'codex_thread_action', op: 'goal-get', session_id: sessionId });
  const getRes = await waitFor('goal-get result', (e) => resultEvent(e) && /goal-get/.test(e.action || ''));
  check('goal-get-reports', getRes.success === true && /active.*smoke goal/.test(getRes.message || ''), getRes.message);

  // 3. The notice preludes the next task's prompt — the mock provider
  // asserts the transcript contains it before signalling done.
  send({ action: 'follow_up', text: 'do the smoke task' });
  const done = await waitFor('mock task completes with the goal in-prompt',
    (e) => JSON.stringify(e).includes('native goal smoke complete'), 60000);
  check('goal-notice-reaches-prompt', Boolean(done), `${done.event}`);

  // 4. Clear; broadcast goes null.
  send({ action: 'codex_thread_action', op: 'goal-clear', session_id: sessionId });
  const clearRes = await waitFor('goal-clear result', (e) => resultEvent(e) && /goal-clear/.test(e.action || ''));
  check('goal-clear-succeeds', clearRes.success === true && /cleared/.test(clearRes.message || ''), clearRes.message);
  const clearGoal = await waitFor('session_goal cleared broadcast',
    (e) => goalEvent(e) && (e.goal === null || e.goal === undefined));
  check('goal-clear-broadcasts-null', Boolean(clearGoal));

  // 5. Status ops without a goal refuse honestly.
  send({ action: 'codex_thread_action', op: 'goal-pause', session_id: sessionId });
  const pauseRes = await waitFor('goal-pause result', (e) => resultEvent(e) && /goal-pause/.test(e.action || ''));
  check('goal-pause-without-goal-fails-honestly',
    pauseRes.success === false && /objective first/.test(pauseRes.message || ''), pauseRes.message);

  const failed = checks.filter((c) => !c.ok);
  console.log(`\n${checks.length - failed.length}/${checks.length} checks passed`);
  child.kill('SIGTERM');
  process.exit(failed.length ? 1 : 0);
})().catch((e) => {
  console.error(`SMOKE ERROR: ${e.message}`);
  child.kill('SIGTERM');
  process.exit(1);
});

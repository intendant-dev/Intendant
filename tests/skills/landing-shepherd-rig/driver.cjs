#!/usr/bin/env node
// Landing-shepherd rig: scratch repo + stub `gh` under a fast poll;
// prove a DIRTY armed PR produces a WAKE of the owning session within
// one poll interval, and that a DIRTY armed PR on an ownerless branch
// PARKS one needs-you agenda item (the classifier table and the
// wake/park fallback order are pinned by unit tests — this leg proves
// the live latency and the real delivery lanes).
const { spawn, execFileSync } = require('child_process');
const fs = require('fs');
const net = require('net');
const os = require('os');
const path = require('path');

const BINARY = process.argv[2] && path.resolve(process.argv[2]);
if (!BINARY) {
  console.error('usage: driver.cjs <path to intendant binary>');
  process.exit(2);
}
const POLL_MS = 1000;
const HOME = fs.mkdtempSync(path.join(os.tmpdir(), 'shepherd-home-'));
const PROJ = fs.mkdtempSync(path.join(os.tmpdir(), 'shepherd-proj-'));

// The scratch repository: checked out on the armed PR's head branch
// (the supervised seat roots here, so `git worktree list` maps it to
// this branch), plus a ref-only ghost branch nobody owns.
const git = (...args) => execFileSync('git', ['-C', PROJ, ...args], { stdio: 'pipe' });
git('init', '--initial-branch=main');
git('config', 'user.email', 'rig@example.com');
git('config', 'user.name', 'Shepherd Rig');
fs.writeFileSync(path.join(PROJ, 'README.md'), 'landing shepherd rig\n');
git('add', 'README.md');
git('commit', '-m', 'seed');
git('checkout', '-b', 'rig-armed-seat');
git('branch', 'rig-ghost-seat');

// The stub `gh`: serves the repo slug, the PR list from a state file
// the driver flips mid-run, and a queue-less mergeQueue. First on PATH.
const BIN = path.join(HOME, 'bin');
fs.mkdirSync(BIN, { recursive: true });
const PRS_FILE = path.join(HOME, 'prs.json');
fs.writeFileSync(PRS_FILE, '[]');
const GH_LOG = path.join(HOME, 'gh-calls.log');
fs.writeFileSync(
  path.join(BIN, 'gh'),
  `#!/bin/sh
echo "$@" >> "${GH_LOG}"
case "$1" in
  repo) echo '{"nameWithOwner":"rig/landing"}' ;;
  pr) /bin/cat "${PRS_FILE}" ;;
  api) echo '{"data":{"repository":{"mergeQueue":null}}}' ;;
  *) exit 1 ;;
esac
exit 0
`
);
fs.chmodSync(path.join(BIN, 'gh'), 0o755);

const dirtyArmedPr = (number, headRef) => ({
  number,
  title: `rig change ${number}`,
  headRefName: headRef,
  isDraft: false,
  state: 'OPEN',
  mergeStateStatus: 'DIRTY',
  mergeable: 'CONFLICTING',
  autoMergeRequest: { enabledAt: '2026-08-02T00:00:00Z' },
  statusCheckRollup: [{ __typename: 'CheckRun', status: 'COMPLETED', conclusion: 'SUCCESS' }],
});

// Mock provider: the primary task and the supervised seat each get an
// instance; the seat's second step is the WAKE follow-up turn and
// asserts the shepherd's text actually reached the transcript.
const script = {
  profiles: [
    {
      match: 'hold the landing seat',
      steps: [
        { content: 'Holding the seat.', tool_calls: [{ name: 'signal_done', arguments: { message: 'seat parked idle' } }] },
        {
          expect_transcript_contains: '[landing-shepherd] Your PR #101',
          content: 'Reconciling per the ritual.',
          tool_calls: [{ name: 'signal_done', arguments: { message: 'wake acknowledged' } }],
        },
      ],
    },
    {
      steps: [
        { content: 'Primary idle.', tool_calls: [{ name: 'signal_done', arguments: { message: 'primary done' } }] },
      ],
    },
  ],
};
const scriptPath = path.join(HOME, 'mock_script.json');
fs.writeFileSync(scriptPath, JSON.stringify(script));

const env = {
  ...process.env,
  HOME,
  USERPROFILE: HOME,
  PATH: `${BIN}:${process.env.PATH}`,
  PROVIDER: 'mock',
  INTENDANT_MOCK_SCRIPT: scriptPath,
  INTENDANT_LANDING_SHEPHERD_POLL_MS: String(POLL_MS),
};
for (const k of ['OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'MODEL_NAME',
  'INTENDANT_HOME', 'INTENDANT_COORDINATION_DIR', 'INTENDANT_SESSION_ID']) delete env[k];

const child = spawn(
  BINARY,
  ['--no-tui', '--web', '0', '--bind', '127.0.0.1', '--no-tls', '--control-socket',
    '--autonomy', 'full', 'primary hold'],
  { cwd: PROJ, env, stdio: ['ignore', 'pipe', 'pipe'] }
);
let exited = false;
child.on('exit', () => { exited = true; });

// Watch stderr for the shepherd's delivery lines.
let stderrBuf = '';
const stderrWaiters = [];
child.stderr.on('data', (d) => {
  stderrBuf += d.toString();
  for (const w of stderrWaiters.splice(0)) w();
});
child.stdout.on('data', () => {});
const waitForStderr = (needle, timeoutMs) =>
  new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    const check = () => {
      const at = stderrBuf.indexOf(needle);
      if (at >= 0) return resolve(Date.now());
      if (Date.now() > deadline) return reject(new Error(`timeout waiting for stderr: ${needle}`));
      stderrWaiters.push(check);
      setTimeout(check, 100);
    };
    check();
  });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const grepTree = (root, needle) => {
  if (!fs.existsSync(root)) return false;
  for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
    const p = path.join(root, entry.name);
    try {
      if (entry.isDirectory()) { if (grepTree(p, needle)) return true; }
      else if (entry.isFile() && fs.readFileSync(p, 'utf8').includes(needle)) return true;
    } catch { /* unreadable: skip */ }
  }
  return false;
};

(async () => {
  // Control socket up = daemon up.
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
  sock.on('data', () => {});

  // The supervised seat: rooted at the daemon's project root (the
  // scratch repo on rig-armed-seat), alive after its task completes.
  // Two readiness gates, both evidence instead of sleeps: the control
  // socket binds BEFORE the supervisor subscribes to the intent lane
  // (a create_session sent in that gap is dropped — no subscriber, no
  // delivery), so wait for the gateway banner first; and a cold
  // scratch HOME's first boot (skill install, store init) can push the
  // seat's first turn well past any fixed delay, so wait for the
  // turn's transcript evidence before flipping the fixture.
  await waitForStderr('Dashboard: http', 30000);
  sock.write(JSON.stringify({ action: 'create_session', task: 'hold the landing seat', name: 'rig seat' }) + '\n');
  const logsRootEarly = path.join(HOME, '.intendant', 'logs');
  const seatDeadline = Date.now() + 60000;
  while (!grepTree(logsRootEarly, 'Holding the seat.')) {
    if (Date.now() > seatDeadline) throw new Error('seat session never completed its first turn');
    if (exited) throw new Error('daemon exited before the seat was ready');
    await sleep(250);
  }
  await sleep(POLL_MS); // one quiet shepherd tick with the seat live

  // THE FLIP: PR #101 (owned branch) and PR #102 (ghost branch) go
  // DIRTY + armed in one poll's fixture.
  fs.writeFileSync(PRS_FILE, JSON.stringify([
    dirtyArmedPr(101, 'rig-armed-seat'),
    dirtyArmedPr(102, 'rig-ghost-seat'),
  ]));
  const flippedAt = Date.now();

  const wokeAt = await waitForStderr('[landing-shepherd] woke session', 20000);
  const parkedAt = await waitForStderr('[landing-shepherd] parked needs-you item', 20000);
  const wakeElapsed = wokeAt - flippedAt;
  const parkElapsed = parkedAt - flippedAt;
  // One poll interval, plus subprocess/delivery slack (5 git/gh spawns).
  const SLACK_MS = 3000;
  const wakeWithinInterval = wakeElapsed <= POLL_MS + SLACK_MS;
  const parkWithinInterval = parkElapsed <= POLL_MS + SLACK_MS;

  // Delivery proof beyond the log line: the wake text reached the
  // seat's transcript (the mock's step-2 expect also pins this), and
  // the needs-you item reached the agenda store.
  await sleep(2500);
  const logsRoot = path.join(HOME, '.intendant', 'logs');
  const wakeInTranscript = grepTree(logsRoot, '[landing-shepherd] Your PR #101');
  const ackInTranscript = grepTree(logsRoot, 'wake acknowledged');
  const agendaRoot = path.join(HOME, '.intendant', 'agenda');
  const parkInAgenda = grepTree(agendaRoot, 'Landing needs you: PR #102');
  const noWrongPark = !grepTree(agendaRoot, 'Landing needs you: PR #101');

  console.log(`wake after ${wakeElapsed}ms, park after ${parkElapsed}ms (poll ${POLL_MS}ms, slack ${SLACK_MS}ms)`);
  if (!wakeWithinInterval) console.log('wake exceeded one poll interval (+slack)');
  if (!parkWithinInterval) console.log('park exceeded one poll interval (+slack)');
  if (!wakeInTranscript) console.log('wake text missing from the seat transcript');
  if (!ackInTranscript) console.log('seat never ran the wake follow-up turn');
  if (!parkInAgenda) console.log('needs-you item missing from the agenda store');
  if (!noWrongPark) console.log('OWNED PR #101 was parked instead of woken');
  const ok = wakeWithinInterval && parkWithinInterval && wakeInTranscript
    && ackInTranscript && parkInAgenda && noWrongPark;
  console.log(ok ? 'LANDING SHEPHERD RIG PASS' : 'LANDING SHEPHERD RIG FAIL');
  child.kill('SIGTERM');
  process.exit(ok ? 0 : 1);
})().catch((err) => {
  console.error('RIG ERROR: ' + err.message);
  console.error('--- daemon stderr tail ---');
  console.error(stderrBuf.split('\n').slice(-40).join('\n'));
  child.kill('SIGTERM');
  process.exit(1);
});

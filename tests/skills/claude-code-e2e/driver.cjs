#!/usr/bin/env node
// Claude Code external-agent E2E driver (haiku-only).
//
// Drives a release intendant binary supervising Claude Code through the Unix
// control socket (--control-socket): approvals (allow + deny), native
// mid-turn steering, native interrupt, native session-id capture, and
// resume. Every model call is Claude haiku (set via [agent.claude_code]
// model + MODEL_NAME guard for any intendant-side calls).
//
// Usage: node driver.cjs [--binary <path>] [--workdir <path>] [--port <n>] [--keep]
// Exit code 0 = all checks passed.

const { spawn, execSync } = require('child_process');
const fs = require('fs');
const net = require('net');
const os = require('os');
const path = require('path');

const args = process.argv.slice(2);
function argValue(name, fallback) {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
}
const BINARY = path.resolve(argValue('--binary', path.join(__dirname, '../../../target/release/intendant')));
const WORKDIR = argValue('--workdir', fs.mkdtempSync(path.join(os.tmpdir(), 'cc-e2e-')));
const PORT = Number(argValue('--port', 18899));
const KEEP = args.includes('--keep');

const t0 = Date.now();
const ts = () => ((Date.now() - t0) / 1000).toFixed(1).padStart(6) + 's';
const logLines = [];
function log(tag, line) {
  const entry = `[${ts()} ${tag}] ${line}`;
  logLines.push(entry);
  console.log(entry);
}

const checks = [];
function check(name, ok, detail = '') {
  checks.push({ name, ok, detail });
  log(ok ? 'PASS' : 'FAIL', `${name}${detail ? ` — ${detail}` : ''}`);
}

// ---------------------------------------------------------------------------
// Control-socket session wrapper
// ---------------------------------------------------------------------------

class IntendantRun {
  constructor(label, cliArgs) {
    this.label = label;
    this.events = [];
    this.waiters = [];
    this.autoApprove = false;
    this.exited = false;

    log(this.label, `spawn: intendant ${cliArgs.join(' ')}`);
    this.child = spawn(BINARY, cliArgs, {
      cwd: WORKDIR,
      env: {
        ...process.env,
        // Guard: any intendant-side model call must be haiku too. External
        // CLI mode makes none, but belt-and-suspenders per the e2e policy.
        PROVIDER: 'anthropic',
        MODEL_NAME: 'claude-haiku-4-5-20251001',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    this.child.stdout.on('data', (d) => {
      for (const line of d.toString().split('\n')) {
        if (line.trim()) log(`${this.label}-out`, line.trim().slice(0, 300));
      }
    });
    this.child.stderr.on('data', (d) => {
      for (const line of d.toString().split('\n')) {
        if (line.trim()) log(`${this.label}-err`, line.trim().slice(0, 300));
      }
    });
    this.exitPromise = new Promise((resolve) => {
      this.child.on('exit', (code, sig) => {
        this.exited = true;
        log(this.label, `exited code=${code} sig=${sig}`);
        resolve(code);
      });
    });
    this.socketPath = `/tmp/intendant-${this.child.pid}.sock`;
  }

  async connect(timeoutMs = 30000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (this.exited) throw new Error(`${this.label} exited before socket came up`);
      if (fs.existsSync(this.socketPath)) {
        try {
          await this.#openSocket();
          log(this.label, `connected to ${this.socketPath}`);
          return;
        } catch {
          // Socket file exists but not accepting yet.
        }
      }
      await sleep(300);
    }
    throw new Error(`${this.label}: control socket never came up at ${this.socketPath}`);
  }

  #openSocket() {
    return new Promise((resolve, reject) => {
      const sock = net.createConnection(this.socketPath);
      let buf = '';
      sock.on('connect', () => {
        this.sock = sock;
        resolve();
      });
      sock.on('error', reject);
      sock.on('data', (d) => {
        buf += d.toString();
        let idx;
        while ((idx = buf.indexOf('\n')) >= 0) {
          const line = buf.slice(0, idx);
          buf = buf.slice(idx + 1);
          if (!line.trim()) continue;
          let event;
          try {
            event = JSON.parse(line);
          } catch {
            continue;
          }
          this.#onEvent(event, line);
        }
      });
    });
  }

  #onEvent(event, rawLine) {
    this.events.push(event);
    const name = event.event || '?';
    // Keep noisy streams out of the log, everything else in.
    if (!['model_response_delta', 'presence_log', 'log'].includes(name)) {
      log(`${this.label}-ev`, rawLine.slice(0, 260));
    }
    if (name === 'approval_required' && this.autoApprove) {
      this.approve(event.id);
    }
    for (const waiter of [...this.waiters]) {
      if (waiter.predicate(event)) {
        this.waiters.splice(this.waiters.indexOf(waiter), 1);
        waiter.resolve(event);
      }
    }
  }

  send(msg) {
    const line = JSON.stringify(msg);
    log(`${this.label}-ctl`, line.slice(0, 200));
    this.sock.write(line + '\n');
  }

  // Approval ids are per-slot, not global — the registry reuses id 1 for
  // each new pending approval — so responses key off the event occurrence,
  // never a seen-id set.
  approve(id) {
    this.send({ action: 'approve', id });
  }

  deny(id) {
    this.send({ action: 'deny', id });
  }

  /// Wait for a matching event — including any already received (so
  /// callers can't race the stream).
  waitFor(description, predicate, timeoutMs = 120000, { skip = 0 } = {}) {
    const seen = this.events.filter(predicate);
    if (seen.length > skip) return Promise.resolve(seen[skip]);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.waiters.splice(this.waiters.findIndex((w) => w.resolve === wrapped), 1);
        reject(new Error(`timeout waiting for ${description}`));
      }, timeoutMs);
      const wrapped = (event) => {
        clearTimeout(timer);
        resolve(event);
      };
      let remaining = skip - seen.length;
      this.waiters.push({
        predicate: (e) => {
          if (!predicate(e)) return false;
          if (remaining > 0) {
            remaining -= 1;
            return false;
          }
          return true;
        },
        resolve: wrapped,
      });
    });
  }

  async stop() {
    if (this.exited) return;
    this.child.kill('SIGTERM');
    const result = await Promise.race([this.exitPromise, sleep(15000).then(() => 'timeout')]);
    if (result === 'timeout') {
      log(this.label, 'SIGTERM timeout — killing');
      this.child.kill('SIGKILL');
      await this.exitPromise;
    }
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const isTurnEnd = (e) => e.event === 'task_complete' || e.event === 'round_complete';
const isApproval = (e) => e.event === 'approval_required';

// Session list over the plain-HTTP dashboard (run1 passes --bind/--no-tls).
async function listSessions(port) {
  try {
    const res = await fetch(`http://127.0.0.1:${port}/api/sessions`);
    if (!res.ok) return [];
    const body = await res.json();
    const rows = Array.isArray(body?.sessions) ? body.sessions : (Array.isArray(body) ? body : []);
    return rows.map((row) => ({
      id: String(row.id || row.session_id || ''),
      source: String(row.backend_source || row.source || ''),
    })).filter((row) => row.id);
  } catch {
    return [];
  }
}

// ---------------------------------------------------------------------------
// Scenario
// ---------------------------------------------------------------------------

async function main() {
  log('setup', `binary: ${BINARY}`);
  log('setup', `workdir: ${WORKDIR}`);
  if (!fs.existsSync(BINARY)) throw new Error(`intendant binary not found at ${BINARY}`);

  fs.mkdirSync(WORKDIR, { recursive: true });
  execSync('git init -q', { cwd: WORKDIR });
  fs.writeFileSync(path.join(WORKDIR, 'README.md'), 'claude-code e2e playground\n');
  fs.writeFileSync(
    path.join(WORKDIR, 'intendant.toml'),
    [
      '[agent]',
      'default_backend = "claude-code"',
      '',
      '[agent.claude_code]',
      '# E2E policy: haiku only — never a more expensive model.',
      'model = "claude-haiku-4-5-20251001"',
      'permission_mode = "default"',
      '',
    ].join('\n'),
  );
  execSync('git add -A && git -c user.email=e2e@local -c user.name=e2e commit -qm seed', {
    cwd: WORKDIR,
    shell: '/bin/zsh',
  });

  const probePath = path.join(WORKDIR, 'probe.txt');
  const steeredPath = path.join(WORKDIR, 'steered.txt');

  // ---- Run 1: approvals, steer, interrupt --------------------------------
  const run = new IntendantRun('run1', [
    '--agent', 'claude-code',
    '--no-tui',
    // No --log-file override: --continue resolves the prior session through
    // the default per-project log index, which a custom log dir would hide.
    '--web', String(PORT),
    // Plain HTTP on loopback so the fork phase can poll /api/sessions.
    '--bind', '127.0.0.1',
    '--no-tls',
    '--control-socket',
    'Use a shell command to create a file named probe.txt containing exactly hello-from-cc. Then reply DONE.',
  ]);

  let backendSessionId = null;
  let forkChildNativeId = null;
  try {
    await run.connect();

    // Phase 1: create probe.txt behind an approval.
    const approval1 = await run.waitFor('approval #1 (create probe.txt)', isApproval);
    check('approval-surfaced', /bash/i.test(approval1.command) || /probe\.txt/.test(approval1.command),
      `command=${JSON.stringify(approval1.command).slice(0, 120)}`);
    run.approve(approval1.id);

    await run.waitFor('turn 1 end', isTurnEnd);
    // The runtime may still be flushing the file when the turn ends.
    await sleep(1500);
    const probeOk = fs.existsSync(probePath) && fs.readFileSync(probePath, 'utf8').includes('hello-from-cc');
    check('approved-tool-ran', probeOk, `probe.txt ${fs.existsSync(probePath) ? 'content: ' + JSON.stringify(fs.readFileSync(probePath, 'utf8').trim()) : 'missing'}`);

    // Native session id: a real UUID announced via session_identity.
    const identity = await run.waitFor(
      'canonical claude-code session identity',
      (e) => e.event === 'session_identity' && e.source === 'claude-code'
        && /^[0-9a-f-]{36}$/.test(e.backend_session_id || ''),
      15000,
    );
    backendSessionId = identity.backend_session_id;
    check('native-session-id', true, backendSessionId);

    const usage = run.events.find(
      (e) => e.event === 'usage_update' && e.main && e.main.tokens_used > 0,
    );
    check('usage-reported', Boolean(usage), usage ? `model=${usage.main.model} tokens=${usage.main.tokens_used}/${usage.main.context_window}` : 'no non-zero usage_update event');
    check('usage-is-haiku', Boolean(usage && /haiku/.test(usage.main.model || '')), usage ? usage.main.model : '');
    // Zeros BEFORE any real data are honest (nothing consumed yet); a zero
    // AFTER real usage means stale bookkeeping is resetting the meter.
    const firstRealUsage = run.events.findIndex(
      (e) => e.event === 'usage_update' && e.main && e.main.tokens_used > 0,
    );
    const zeroAfterReal = firstRealUsage >= 0 && run.events
      .slice(firstRealUsage + 1)
      .some((e) => e.event === 'usage_update' && e.main && !(e.main.tokens_used > 0));
    check('no-zero-usage-after-real', !zeroAfterReal,
      zeroAfterReal ? 'a zero usage snapshot reset the meter after real data' : 'meter never regressed to zero');

    const caps = run.events.find((e) => e.event === 'session_capabilities');
    check('capabilities-advertised', Boolean(caps && caps.capabilities && caps.capabilities.steer && caps.capabilities.interrupt),
      caps ? JSON.stringify(caps.capabilities).slice(0, 100) : 'none seen');

    // Phase 2: deny path — probe.txt must survive.
    run.send({ action: 'follow_up', text: 'Use a shell command to delete probe.txt. If the command is denied, reply DENIED and stop trying.' });
    const approval2 = await run.waitFor('approval #2 (delete probe.txt)', isApproval, 120000, { skip: 1 });
    run.deny(approval2.id);
    await run.waitFor('turn 2 end', isTurnEnd, 120000, { skip: 1 });
    check('denied-tool-blocked', fs.existsSync(probePath), 'probe.txt still exists after deny');

    // Phase 3: native mid-turn steer. Auto-approve everything from here on
    // (the steer's own shell command needs an approval too).
    run.autoApprove = true;
    run.send({
      action: 'follow_up',
      text: 'Run exactly this bash command: for i in $(seq 1 20); do sleep 1; done; echo waited. Then reply WAITED.',
    });
    // Give the model time to start the ~20s loop (safe commands may run
    // without an approval prompt depending on host settings — don't gate
    // the steer on one), then steer into the RUNNING turn.
    await sleep(8000);
    run.send({
      action: 'steer',
      text: 'Additional mid-turn instruction: before replying WAITED, also create a file named steered.txt containing exactly steered (shell or Write tool, your choice).',
    });
    await run.waitFor('turn 3 end', isTurnEnd, 180000, { skip: 2 });
    await sleep(1500);
    check('steer-absorbed-in-turn', fs.existsSync(steeredPath),
      fs.existsSync(steeredPath) ? 'steered.txt created within the steered turn' : 'steered.txt missing at turn end');

    // Phase 4: interrupt a long-running turn; the process must survive.
    run.send({
      action: 'follow_up',
      text: 'Run exactly this bash command: for i in $(seq 1 90); do sleep 1; done; echo done90. Then reply LONGDONE.',
    });
    // Let the 90s loop start (approval, if any, is auto-approved).
    await sleep(8000);
    const interruptSentAt = Date.now();
    run.send({ action: 'interrupt' });
    await Promise.race([
      run.waitFor('interrupted event', (e) => e.event === 'interrupted', 90000),
      run.waitFor('turn 4 end', isTurnEnd, 90000, { skip: 3 }),
    ]);
    const interruptLatency = (Date.now() - interruptSentAt) / 1000;
    check('interrupt-aborts-turn', interruptLatency < 45, `turn ended ${interruptLatency.toFixed(1)}s after interrupt (loop had ~87s left)`);

    // Phase 5: the backend survives the interrupt.
    run.send({ action: 'follow_up', text: 'Reply with exactly: ALIVE' });
    const alive = await run.waitFor(
      'ALIVE response',
      (e) => e.event === 'model_response' && /ALIVE/.test(e.summary || ''),
      120000,
    );
    check('process-survives-interrupt', Boolean(alive));

    // Phase 6: the universal thread-action vocabulary is advertised.
    const capsUniversal = run.events.find((e) => e.event === 'session_capabilities'
      && Array.isArray((e.capabilities || {}).thread_actions));
    check('thread-actions-advertised',
      Boolean(capsUniversal && ['compact', 'fork'].every((op) => capsUniversal.capabilities.thread_actions.includes(op))),
      capsUniversal ? JSON.stringify(capsUniversal.capabilities.thread_actions) : 'no thread_actions in any capabilities event');

    // Phase 7: /compact via the universal thread_action channel. The CLI
    // compacts in place (status: compacting → compact_boundary → a free
    // result); the session must still recall pre-compact facts afterwards.
    run.send({ action: 'thread_action', op: 'compact', session_id: backendSessionId });
    const compactResult = await run.waitFor(
      'compact result',
      (e) => e.event === 'codex_thread_action_result' && e.action === 'compact',
      120000,
    );
    check('compact-dispatched', compactResult.success === true, compactResult.message || '');
    run.send({ action: 'follow_up', text: 'What was the name of the very first file you created this session? Reply with only the file name.' });
    const postCompact = await run.waitFor(
      'post-compact recall',
      (e) => e.event === 'model_response' && /probe\.txt/.test(e.summary || ''),
      180000,
    );
    check('compact-retains-context', Boolean(postCompact), (postCompact.summary || '').slice(0, 80));

    // Phase 8: fork — a NEW wrapper session resumes this thread with
    // --fork-session. The child announces its own native id on its first
    // turn, which also emits the fork relationship.
    const sessionsBefore = await listSessions(PORT);
    run.send({ action: 'thread_action', op: 'fork', session_id: backendSessionId });
    const forkResult = await run.waitFor(
      'fork result',
      (e) => e.event === 'codex_thread_action_result' && e.action === 'fork',
      60000,
    );
    check('fork-dispatched', forkResult.success === true, forkResult.message || '');
    let childWrapperId = null;
    let lastRows = [];
    for (let i = 0; i < 45 && !childWrapperId; i++) {
      await sleep(1000);
      lastRows = await listSessions(PORT);
      const fresh = lastRows.filter((row) => /claude/.test(row.source)
        && !sessionsBefore.some((old) => old.id === row.id)
        && row.id !== backendSessionId);
      if (fresh.length) childWrapperId = fresh[0].id;
    }
    if (!childWrapperId) {
      log('fork-debug', `before=${JSON.stringify(sessionsBefore)} after=${JSON.stringify(lastRows)}`);
    }
    check('fork-creates-wrapper-session', Boolean(childWrapperId),
      childWrapperId || 'no new claude-code session appeared in /api/sessions');

    // The fork's first prompt binds its native id + relationship, and must
    // recall the parent's pre-fork context.
    run.send({
      action: 'follow_up',
      session_id: childWrapperId,
      text: 'What was the name of the very first file you created this session? Reply with only the file name.',
    });
    const forkRel = await run.waitFor(
      'fork relationship',
      (e) => e.event === 'session_relationship' && e.relationship === 'fork'
        && e.parent_session_id === backendSessionId,
      180000,
    );
    const childNativeId = forkRel.child_session_id;
    forkChildNativeId = childNativeId;
    check('fork-child-has-own-native-id',
      /^[0-9a-f-]{36}$/.test(childNativeId || '') && childNativeId !== backendSessionId,
      `parent=${backendSessionId} child=${childNativeId}`);
    const forkRecall = await run.waitFor(
      'fork context recall',
      (e) => e.event === 'model_response' && /probe\.txt/.test(e.summary || '')
        && (e.session_id === childNativeId || e.session_id === childWrapperId),
      180000,
    );
    check('fork-retains-context', Boolean(forkRecall), (forkRecall.summary || '').slice(0, 80));

    // The parent thread must be untouched by the fork.
    run.send({ action: 'follow_up', text: 'Reply with exactly: PARENTALIVE' });
    const parentAlive = await run.waitFor(
      'parent alive after fork',
      (e) => e.event === 'model_response' && /PARENTALIVE/.test(e.summary || ''),
      120000,
    );
    check('parent-survives-fork', Boolean(parentAlive));
  } finally {
    await run.stop();
  }

  // ---- Run 2: resume the most recent session by native id ----------------
  // The most recent session is now the FORK wrapper, so `--continue` must
  // bind to the fork child's native id (resume resolution reads the
  // wrapper log's identity record) and recall the forked context.
  const run2 = new IntendantRun('run2', [
    '--agent', 'claude-code',
    '--no-tui',
    '--web', String(PORT + 1),
    '--control-socket',
    '--continue',
    'Earlier in this conversation you created a file with a shell command before any other file. Reply with only that file name.',
  ]);
  try {
    await run2.connect();
    const identity2 = await run2.waitFor(
      'resumed claude-code identity',
      (e) => e.event === 'session_identity' && e.source === 'claude-code'
        && /^[0-9a-f-]{36}$/.test(e.backend_session_id || ''),
      120000,
    );
    check('resume-binds-fork-native-session', identity2.backend_session_id === forkChildNativeId,
      `fork-child=${forkChildNativeId} run2=${identity2.backend_session_id}`);
    const recall = await run2.waitFor(
      'context recall answer',
      (e) => e.event === 'model_response' && (e.summary || '').length > 0,
      120000,
    );
    check('resume-retains-context', /probe\.txt/.test(recall.summary || ''), `answer=${JSON.stringify((recall.summary || '').slice(0, 120))}`);
  } finally {
    await run2.stop();
  }
}

main()
  .catch((e) => {
    log('ERROR', e.stack || String(e));
    check('scenario-completed', false, String(e.message || e));
  })
  .finally(() => {
    const failed = checks.filter((c) => !c.ok);
    console.log('\n===== Claude Code E2E summary =====');
    for (const c of checks) console.log(` ${c.ok ? '✅' : '❌'} ${c.name}${c.detail ? ` — ${c.detail}` : ''}`);
    console.log(`${checks.length - failed.length}/${checks.length} checks passed`);
    fs.writeFileSync(path.join(WORKDIR, 'e2e.log'), logLines.join('\n') + '\n');
    console.log(`workdir: ${WORKDIR}${KEEP ? ' (kept)' : ''}`);
    process.exit(failed.length ? 1 : 0);
  });

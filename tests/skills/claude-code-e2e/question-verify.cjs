#!/usr/bin/env node
// AskUserQuestion E2E (haiku-only): a supervised Claude Code session asks the
// user a structured question. Verifies the question surfaces as the
// `user_question` outbound event (NOT a generic `approval_required`), that
// `{"action":"answer_question"}` delivers the chosen option back to the
// model, and that skip dismisses the prompt without aborting the turn.
//
// Usage: node question-verify.cjs [--binary <path>] [--workdir <path>] [--port <n>] [--keep]
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
const WORKDIR = argValue('--workdir', fs.mkdtempSync(path.join(os.tmpdir(), 'cc-q-e2e-')));
const PORT = Number(argValue('--port', 18907));
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

class IntendantRun {
  constructor(label, cliArgs) {
    this.label = label;
    this.events = [];
    this.waiters = [];
    this.exited = false;

    log(this.label, `spawn: intendant ${cliArgs.join(' ')}`);
    this.child = spawn(BINARY, cliArgs, {
      cwd: WORKDIR,
      env: {
        ...process.env,
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
    if (!['model_response_delta', 'presence_log', 'log'].includes(name)) {
      log(`${this.label}-ev`, rawLine.slice(0, 300));
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
    log(`${this.label}-ctl`, line.slice(0, 240));
    this.sock.write(line + '\n');
  }

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
const isQuestion = (e) => e.event === 'user_question';

async function main() {
  log('setup', `binary: ${BINARY}`);
  log('setup', `workdir: ${WORKDIR}`);
  if (!fs.existsSync(BINARY)) throw new Error(`intendant binary not found at ${BINARY}`);

  fs.mkdirSync(WORKDIR, { recursive: true });
  execSync('git init -q', { cwd: WORKDIR });
  fs.writeFileSync(path.join(WORKDIR, 'README.md'), 'ask-user-question e2e playground\n');
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

  const run = new IntendantRun('run1', [
    '--agent', 'claude-code',
    '--no-tui',
    '--web', String(PORT),
    '--bind', '127.0.0.1',
    '--no-tls',
    '--control-socket',
    'Use the AskUserQuestion tool to ask me one question: "Which database should we use?" with header "Database" and exactly two options: PostgreSQL (description "Relational") and SQLite (description "Embedded"). After I answer, reply with exactly: CHOSEN: <my answer>',
  ]);

  try {
    await run.connect();

    // ---- Phase 1: structured question surfaces and the answer round-trips.
    const q1 = await run.waitFor('user_question #1', isQuestion);
    const questions = Array.isArray(q1.questions) ? q1.questions : [];
    check('question-surfaced-structured', questions.length === 1,
      `questions=${JSON.stringify(q1.questions).slice(0, 200)}`);
    const first = questions[0] || {};
    check('question-carries-options',
      first.question === 'Which database should we use?'
        && Array.isArray(first.options)
        && first.options.length === 2
        && first.options[0].label === 'PostgreSQL'
        && first.options[0].description === 'Relational',
      JSON.stringify(first).slice(0, 220));
    check('question-carries-header', first.header === 'Database', `header=${JSON.stringify(first.header)}`);

    // The question must NOT have also surfaced as a generic approval.
    const strayApproval = run.events.find(
      (e) => e.event === 'approval_required' && /AskUserQuestion/.test(e.command || ''),
    );
    check('question-not-an-approval', !strayApproval,
      strayApproval ? `approval_required leaked: ${strayApproval.command}` : 'no AskUserQuestion approval_required');

    run.send({
      action: 'answer_question',
      id: q1.id,
      session_id: q1.session_id,
      answers: { 'Which database should we use?': 'PostgreSQL' },
    });

    const resolved1 = await run.waitFor(
      'question #1 resolved',
      (e) => e.event === 'approval_resolved' && e.action === 'answer',
    );
    check('answer-resolves-prompt', Boolean(resolved1), `id=${resolved1.id}`);

    await run.waitFor('turn 1 end', isTurnEnd);
    const echoed = run.events.some(
      (e) => e.event === 'model_response' && /CHOSEN:\s*PostgreSQL/.test(e.content || ''),
    ) || run.events.some(
      (e) => (e.event === 'task_complete' || e.event === 'done_signal')
        && /CHOSEN:\s*PostgreSQL/.test(e.summary || e.message || ''),
    );
    check('answer-reached-model', echoed, 'model echoed CHOSEN: PostgreSQL');

    // ---- Phase 2: skip dismisses without aborting the turn.
    run.send({
      action: 'follow_up',
      text: 'Use the AskUserQuestion tool to ask me one more question: "Which cache should we use?" with options Redis and Memcached. If the question is declined or you get no answer, reply with exactly: NO-ANSWER and stop.',
    });
    const q2 = await run.waitFor('user_question #2', isQuestion, 120000, { skip: 1 });
    check('second-question-surfaced', Array.isArray(q2.questions) && q2.questions.length === 1,
      JSON.stringify(q2.questions).slice(0, 160));

    run.send({ action: 'skip', id: q2.id, session_id: q2.session_id });
    const resolved2 = await run.waitFor(
      'question #2 resolved via skip',
      (e) => e.event === 'approval_resolved' && e.action === 'skip',
    );
    check('skip-resolves-prompt', Boolean(resolved2), `id=${resolved2.id}`);

    await run.waitFor('turn 2 end', isTurnEnd, 120000, { skip: 1 });
    const noAnswer = run.events.some(
      (e) => e.event === 'model_response' && /NO-ANSWER/.test(e.content || ''),
    ) || run.events.some(
      (e) => (e.event === 'task_complete' || e.event === 'done_signal')
        && /NO-ANSWER/.test(e.summary || e.message || ''),
    );
    check('skip-turn-completes-gracefully', noAnswer, 'model replied NO-ANSWER after dismissal');
  } finally {
    await run.stop();
    fs.writeFileSync(path.join(WORKDIR, 'e2e.log'), logLines.join('\n') + '\n');
    log('done', `event log: ${path.join(WORKDIR, 'e2e.log')}`);
    if (!KEEP && checks.every((c) => c.ok)) {
      fs.rmSync(WORKDIR, { recursive: true, force: true });
    } else {
      log('done', `workdir kept: ${WORKDIR}`);
    }
  }

  console.log('\n=== Results ===');
  for (const c of checks) console.log(`${c.ok ? '✅' : '❌'} ${c.name}${c.detail ? ` — ${c.detail}` : ''}`);
  const failed = checks.filter((c) => !c.ok).length;
  console.log(failed === 0 ? '\nAll checks passed.' : `\n${failed} check(s) FAILED.`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((err) => {
  log('fatal', err.stack || String(err));
  console.log('\n=== Results (fatal) ===');
  for (const c of checks) console.log(`${c.ok ? '✅' : '❌'} ${c.name}`);
  process.exit(1);
});

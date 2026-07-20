#!/usr/bin/env node
// Real Kimi Code external-agent E2E.
//
// This is intentionally not a CI test: it uses the installed/authenticated
// Kimi CLI and the K2.7 Coding model. It drives Intendant through its Unix
// control socket and loopback dashboard API, while isolating Intendant state,
// Kimi session history, and copied auth material in one disposable root. Any
// OAuth refresh rotation is compare-and-swap published back to the source
// credential after the supervised process stops, before that root is deleted.
//
// Usage:
//   node driver.cjs [--binary <path>] [--workdir <path>] [--port <n>]
//                   [--keep] [--quick] [--background-only]
//                   [--auth-sync-self-test]
//
// --quick skips the slow steering/interrupt/background-agent phases. The
// default is the exhaustive acceptance scenario.
// --background-only runs the native background-agent phase after startup,
// which is useful for focused compatibility diagnosis.

"use strict";

const { execFileSync, execSync, spawn } = require("child_process");
const crypto = require("crypto");
const fs = require("fs");
const net = require("net");
const os = require("os");
const path = require("path");

const MODEL = "kimi-code/kimi-for-coding";
const HIGHSPEED_MODEL = "kimi-code/kimi-for-coding-highspeed";
const MODEL_DISPLAY = "K2.7 Coding";
const SUPPORTED_KIMI_VERSION = /^0\.(?:27|28)\./;
const args = process.argv.slice(2);
const USAGE = `Usage:
  node driver.cjs [--binary <path>] [--workdir <path>] [--port <n>]
                  [--kimi <path>] [--keep] [--quick] [--background-only]
                  [--auth-sync-self-test]

The default is the exhaustive authenticated K2.7 Coding acceptance scenario.
--quick skips slow steer/interrupt/background phases; --background-only runs
only the native background-agent phase after startup. --auth-sync-self-test
validates credential rotation copy-back without contacting Kimi.`;

if (args.includes("--help") || args.includes("-h")) {
  console.log(USAGE);
  process.exit(0);
}

function argValue(name, fallback) {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
}

const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), "kimi-intendant-e2e-"));
const WORKDIR_ARG = argValue("--workdir", "");
const WORKDIR = path.resolve(WORKDIR_ARG || path.join(ROOT, "project"));
const STATE_HOME = path.join(ROOT, "intendant-state");
const VAULT_STATE_HOME = path.join(ROOT, "vault-intendant-state");
const KIMI_HOME = path.join(ROOT, "kimi-home");
const KEEP = args.includes("--keep");
const QUICK = args.includes("--quick");
const BACKGROUND_ONLY = args.includes("--background-only");
const AUTH_SYNC_SELF_TEST = args.includes("--auth-sync-self-test");
const BINARY = path.resolve(
  argValue(
    "--binary",
    path.join(__dirname, "../../../target/release/intendant"),
  ),
);
const KIMI_COMMAND = resolveCommand(argValue("--kimi", "kimi"));
const REQUESTED_PORT = Number(argValue("--port", "0"));

const ATTACHMENT_TOKEN = `ATTACHMENT_${randomToken()}`;
const KEEP_CODEWORD = `KEEP_${randomToken()}`;
const DROP_CODEWORD = `DROP_${randomToken()}`;
const QUESTION = "Which acceptance lane should this Kimi E2E use?";
const QUESTION_ANSWER = "Blue";
const MULTI_QUESTION = "Which acceptance flags should this Kimi E2E record?";
const MULTI_ANSWER = "Protocol";
const t0 = Date.now();
const logLines = [];
const checks = [];
const skips = [];
let kimiAuthSnapshot = null;

function ts() {
  return `${((Date.now() - t0) / 1000).toFixed(1).padStart(7)}s`;
}

function log(tag, line) {
  const entry = `[${ts()} ${tag}] ${line}`;
  logLines.push(entry);
  console.log(entry);
}

function check(name, ok, detail = "") {
  checks.push({ name, ok, detail });
  log(ok ? "PASS" : "FAIL", `${name}${detail ? ` — ${detail}` : ""}`);
}

function skip(name, detail) {
  skips.push({ name, detail });
  log("SKIP", `${name} — ${detail}`);
}

function randomToken() {
  return Math.random().toString(16).slice(2, 10).toUpperCase();
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function resolveCommand(command) {
  if (path.isAbsolute(command)) return command;
  try {
    return execFileSync("which", [command], { encoding: "utf8" }).trim();
  } catch {
    return command;
  }
}

function tomlString(value) {
  return JSON.stringify(String(value));
}

function responseText(event) {
  return String(event?.summary || event?.content || event?.message || "");
}

function eventTargets(event, ...ids) {
  const target = String(event?.session_id || "");
  return !target || ids.filter(Boolean).includes(target);
}

function isTurnEnd(event) {
  return (
    event.event === "done_signal" ||
    event.event === "task_complete" ||
    event.event === "round_complete"
  );
}

function resultEvent(event, op) {
  return event.event === "codex_thread_action_result" && event.action === op;
}

function activeToolNames(message) {
  const value = String(message || "");
  if (/no active tools/i.test(value)) return [];
  return value
    .split("\n")
    .map((line) => line.trim().replace(/^[-*]\s+/, ""))
    .filter((line) => line.startsWith("active\t"))
    .map((line) => line.split("\t")[1])
    .filter(Boolean)
    .sort();
}

function registeredToolNames(message) {
  return String(message || "")
    .split("\n")
    .map((line) => line.trim().replace(/^[-*]\s+/, ""))
    .filter(
      (line) => line.startsWith("active\t") || line.startsWith("inactive\t"),
    )
    .map((line) => line.split("\t")[1])
    .filter(Boolean)
    .sort();
}

function sameStrings(left, right) {
  return (
    left.length === right.length &&
    left.every((value, index) => value === right[index])
  );
}

function sessionRows(body) {
  if (Array.isArray(body)) return body;
  if (Array.isArray(body?.sessions)) return body.sessions;
  return [];
}

function sessionRowForId(body, id) {
  if (!id) return undefined;
  return sessionRows(body).find((row) =>
    [
      row?.session_id,
      row?.resume_id,
      row?.backend_session_id,
      row?.intendant_session_id,
    ].includes(id),
  );
}

function kimiProfileRowsForId(body, id) {
  if (!id) return [];
  return sessionRows(body)
    .filter((row) =>
      [
        row?.session_id,
        row?.resume_id,
        row?.backend_session_id,
        row?.intendant_session_id,
      ].includes(id),
    )
    .map((row) => ({
      session_id: row?.session_id,
      resume_id: row?.resume_id,
      backend_session_id: row?.backend_session_id,
      intendant_session_id: row?.intendant_session_id,
      source: row?.source,
      backend_source: row?.backend_source,
      configured_source: row?.configured_source,
      kimi_model: row?.kimi_model,
      kimi_thinking: row?.kimi_thinking,
      kimi_permission_mode: row?.kimi_permission_mode,
      kimi_allowed_tools: row?.kimi_allowed_tools,
      kimi_plan_mode: row?.kimi_plan_mode,
      kimi_swarm_mode: row?.kimi_swarm_mode,
    }));
}

function worktreeSnapshot(root) {
  const rows = [];
  const visit = (dir, relativeDir = "") => {
    for (const name of fs.readdirSync(dir).sort()) {
      if (
        (relativeDir === "" && name === ".git") ||
        (relativeDir === "" && name === ".intendant")
      ) {
        continue;
      }
      const relative = path.posix.join(relativeDir, name);
      const absolute = path.join(dir, name);
      const stat = fs.lstatSync(absolute);
      if (stat.isDirectory()) {
        rows.push(`d ${relative} ${stat.mode & 0o777}`);
        visit(absolute, relative);
      } else if (stat.isSymbolicLink()) {
        rows.push(`l ${relative} ${fs.readlinkSync(absolute)}`);
      } else if (stat.isFile()) {
        const digest = crypto
          .createHash("sha256")
          .update(fs.readFileSync(absolute))
          .digest("hex");
        rows.push(`f ${relative} ${stat.mode & 0o777} ${stat.size} ${digest}`);
      }
    }
  };
  visit(root);
  return rows.join("\n");
}

function chmodTreePrivate(root) {
  if (!fs.existsSync(root)) return;
  const st = fs.lstatSync(root);
  if (st.isSymbolicLink()) return;
  fs.chmodSync(root, st.isDirectory() ? 0o700 : 0o600);
  if (st.isDirectory()) {
    for (const name of fs.readdirSync(root)) {
      chmodTreePrivate(path.join(root, name));
    }
  }
}

function credentialDigest(bytes) {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

function requirePrivateRegularCredential(credential, label) {
  const stat = fs.lstatSync(credential);
  if (!stat.isFile() || stat.isSymbolicLink()) {
    throw new Error(`${label} must be a regular file`);
  }
  if ((stat.mode & 0o077) !== 0) {
    throw new Error(`${label} must not be readable by group or other users`);
  }
}

function copyKimiAuthState() {
  const requestedSource = path.resolve(
    process.env.KIMI_CODE_HOME || path.join(os.homedir(), ".kimi-code"),
  );
  const source = fs.realpathSync(requestedSource);
  const credential = path.join(source, "credentials", "kimi-code.json");
  if (!fs.existsSync(credential)) {
    throw new Error(
      `Kimi is not authenticated: expected ${credential}. Run "kimi login" first.`,
    );
  }
  requirePrivateRegularCredential(credential, "Kimi source credential");
  const sourceBytes = fs.readFileSync(credential);
  kimiAuthSnapshot = {
    credential,
    initialDigest: credentialDigest(sourceBytes),
  };
  sourceBytes.fill(0);

  fs.mkdirSync(KIMI_HOME, { recursive: true, mode: 0o700 });
  for (const name of [
    "credentials",
    "oauth",
    "config.toml",
    "tui.toml",
    "device_id",
  ]) {
    const from = path.join(source, name);
    if (!fs.existsSync(from)) continue;
    fs.cpSync(from, path.join(KIMI_HOME, name), {
      recursive: true,
      dereference: true,
      errorOnExist: false,
    });
  }
  chmodTreePrivate(KIMI_HOME);
}

// OAuth providers may rotate the refresh grant on a successful model call.
// The E2E intentionally runs from an isolated KIMI_CODE_HOME, so deleting that
// home without publishing the rotated credential makes the machine's original
// login unusable. Adopt the refreshed file only after every supervised Kimi
// descendant is stopped, and only if the source still matches the snapshot
// copied at startup. The compare-and-swap refuses to clobber a concurrent
// `kimi login` or direct CLI refresh.
function syncKimiAuthState() {
  if (!kimiAuthSnapshot) return;
  const isolated = path.join(KIMI_HOME, "credentials", "kimi-code.json");
  if (!fs.existsSync(isolated)) {
    throw new Error("isolated Kimi credential disappeared before refresh sync");
  }
  requirePrivateRegularCredential(isolated, "Kimi isolated credential");
  requirePrivateRegularCredential(
    kimiAuthSnapshot.credential,
    "Kimi source credential",
  );

  const isolatedBytes = fs.readFileSync(isolated);
  const sourceBytes = fs.readFileSync(kimiAuthSnapshot.credential);
  const isolatedDigest = credentialDigest(isolatedBytes);
  const sourceDigest = credentialDigest(sourceBytes);
  try {
    if (isolatedDigest === kimiAuthSnapshot.initialDigest) {
      log("auth", "Kimi OAuth credential did not rotate");
      return;
    }
    if (sourceDigest !== kimiAuthSnapshot.initialDigest) {
      throw new Error(
        "Kimi source credential changed during E2E; refusing to overwrite a concurrent login or refresh",
      );
    }

    const parent = path.dirname(kimiAuthSnapshot.credential);
    const temporary = path.join(
      parent,
      `.intendant-e2e-refresh-${process.pid}-${crypto.randomUUID()}`,
    );
    let fd;
    try {
      fd = fs.openSync(temporary, "wx", 0o600);
      fs.writeFileSync(fd, isolatedBytes);
      fs.fsyncSync(fd);
      fs.closeSync(fd);
      fd = undefined;
      fs.renameSync(temporary, kimiAuthSnapshot.credential);
      fs.chmodSync(kimiAuthSnapshot.credential, 0o600);
      const parentFd = fs.openSync(parent, "r");
      try {
        fs.fsyncSync(parentFd);
      } finally {
        fs.closeSync(parentFd);
      }
    } finally {
      if (fd !== undefined) fs.closeSync(fd);
      fs.rmSync(temporary, { force: true });
    }
    requirePrivateRegularCredential(
      kimiAuthSnapshot.credential,
      "refreshed Kimi source credential",
    );
    const installed = fs.readFileSync(kimiAuthSnapshot.credential);
    try {
      if (credentialDigest(installed) !== isolatedDigest) {
        throw new Error("Kimi refreshed credential failed post-write verification");
      }
    } finally {
      installed.fill(0);
    }
    log("auth", "persisted Kimi OAuth refresh from isolated E2E home");
  } finally {
    isolatedBytes.fill(0);
    sourceBytes.fill(0);
  }
}

function runAuthSyncSelfTest() {
  const sourceHome = path.join(ROOT, "auth-sync-source");
  const sourceCredential = path.join(
    sourceHome,
    "credentials",
    "kimi-code.json",
  );
  const isolatedCredential = path.join(
    KIMI_HOME,
    "credentials",
    "kimi-code.json",
  );
  fs.mkdirSync(path.dirname(sourceCredential), {
    recursive: true,
    mode: 0o700,
  });
  fs.mkdirSync(path.dirname(isolatedCredential), {
    recursive: true,
    mode: 0o700,
  });

  const install = (source, isolated) => {
    fs.writeFileSync(sourceCredential, source, { mode: 0o600 });
    fs.chmodSync(sourceCredential, 0o600);
    fs.writeFileSync(isolatedCredential, isolated, { mode: 0o600 });
    fs.chmodSync(isolatedCredential, 0o600);
    kimiAuthSnapshot = {
      credential: sourceCredential,
      initialDigest: credentialDigest(Buffer.from(source)),
    };
  };

  install("synthetic-initial", "synthetic-rotated");
  syncKimiAuthState();
  check(
    "auth-refresh-copyback",
    fs.readFileSync(sourceCredential, "utf8") === "synthetic-rotated",
  );

  install("synthetic-second", "synthetic-second-rotated");
  fs.writeFileSync(sourceCredential, "synthetic-concurrent", { mode: 0o600 });
  fs.chmodSync(sourceCredential, 0o600);
  let refusedConcurrent = false;
  try {
    syncKimiAuthState();
  } catch (error) {
    refusedConcurrent = /changed during E2E/.test(String(error));
  }
  check(
    "auth-refresh-concurrent-write-refused",
    refusedConcurrent &&
      fs.readFileSync(sourceCredential, "utf8") === "synthetic-concurrent",
  );
}

function descendantsOf(rootPid) {
  let rows;
  try {
    rows = execFileSync("ps", ["-axo", "pid=,ppid=,command="], {
      encoding: "utf8",
    })
      .split("\n")
      .map((line) => {
        const match = line.trim().match(/^(\d+)\s+(\d+)\s+(.*)$/);
        return match
          ? { pid: Number(match[1]), ppid: Number(match[2]), command: match[3] }
          : null;
      })
      .filter(Boolean);
  } catch {
    return [];
  }
  const found = new Set([rootPid]);
  let changed = true;
  while (changed) {
    changed = false;
    for (const row of rows) {
      if (found.has(row.ppid) && !found.has(row.pid)) {
        found.add(row.pid);
        changed = true;
      }
    }
  }
  found.delete(rootPid);
  return rows.filter((row) => found.has(row.pid));
}

async function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      const port = typeof address === "object" && address ? address.port : 0;
      server.close((error) => (error ? reject(error) : resolve(port)));
    });
  });
}

function intendantEnv(stateHome) {
  const env = {
    ...process.env,
    INTENDANT_HOME: stateHome,
    KIMI_CODE_HOME: KIMI_HOME,
    NO_COLOR: "1",
  };
  for (const key of [
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "MODEL_NAME",
    "PRESENCE_PROVIDER",
    "PRESENCE_MODEL",
    "CU_PROVIDER",
    "CU_MODEL",
  ]) {
    delete env[key];
  }
  return env;
}

class IntendantRun {
  constructor(port) {
    this.events = [];
    this.waiters = [];
    this.exited = false;
    this.sock = null;
    this.ownedDescendants = new Set();
    this.approvalResponder = null;
    this.port = port;

    const cliArgs = [
      "--agent",
      "kimi",
      "--no-tui",
      "--web",
      String(port),
      "--bind",
      "127.0.0.1",
      "--no-tls",
      "--control-socket",
      "Before doing anything else, call AskUserQuestion exactly once. Ask " +
        `${JSON.stringify(QUESTION)} with header "Lane", single-select options ` +
        '"Blue" (description "Primary acceptance lane") and "Green" ' +
        '(description "Alternate acceptance lane"). After the answer, reply ' +
        "with exactly QUESTION_ANSWER=<chosen label> and do nothing else.",
    ];
    log("spawn", `${BINARY} ${cliArgs.map(shellDisplay).join(" ")}`);
    this.child = spawn(BINARY, cliArgs, {
      cwd: WORKDIR,
      env: intendantEnv(STATE_HOME),
      stdio: ["ignore", "pipe", "pipe"],
    });
    this.socketPath = `/tmp/intendant-${this.child.pid}.sock`;
    this.child.stdout.on("data", (data) => this.#logStream("out", data));
    this.child.stderr.on("data", (data) => this.#logStream("err", data));
    this.exitPromise = new Promise((resolve) => {
      this.child.on("exit", (code, signal) => {
        this.exited = true;
        log("daemon", `exited code=${code} signal=${signal}`);
        resolve({ code, signal });
      });
    });
  }

  #logStream(tag, data) {
    for (const line of data.toString().split("\n")) {
      if (line.trim()) log(tag, line.trim().slice(0, 500));
    }
  }

  async connect(timeoutMs = 45_000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (this.exited)
        throw new Error("Intendant exited before its control socket came up");
      this.observeDescendants();
      if (fs.existsSync(this.socketPath)) {
        try {
          await this.#openSocket();
          log("socket", `connected to ${this.socketPath}`);
          return;
        } catch {
          // The socket file can appear just before accept starts.
        }
      }
      await sleep(100);
    }
    throw new Error(`control socket did not appear at ${this.socketPath}`);
  }

  #openSocket() {
    return new Promise((resolve, reject) => {
      const socket = net.createConnection(this.socketPath);
      let buffer = "";
      socket.once("connect", () => {
        this.sock = socket;
        resolve();
      });
      socket.once("error", reject);
      socket.on("data", (data) => {
        buffer += data.toString();
        let newline;
        while ((newline = buffer.indexOf("\n")) >= 0) {
          const line = buffer.slice(0, newline);
          buffer = buffer.slice(newline + 1);
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
    const noisy = new Set([
      "model_response_delta",
      "presence_log",
      "log",
      "status",
      "usage",
      "usage_update",
      "session_vitals",
    ]);
    if (!noisy.has(event.event)) {
      log("event", rawLine.slice(0, 500));
    }
    if (event.event === "approval_required" && this.approvalResponder) {
      const responder = this.approvalResponder;
      queueMicrotask(() => responder(event));
    }
    for (const waiter of [...this.waiters]) {
      if (waiter.predicate(event)) {
        this.waiters.splice(this.waiters.indexOf(waiter), 1);
        waiter.resolve(event);
      }
    }
  }

  mark() {
    return this.events.length;
  }

  waitFor(description, predicate, timeoutMs = 120_000, since = 0) {
    const seen = this.events.slice(since).find(predicate);
    if (seen) return Promise.resolve(seen);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        const index = this.waiters.findIndex(
          (waiter) => waiter.resolve === wrapped,
        );
        if (index >= 0) this.waiters.splice(index, 1);
        reject(new Error(`timeout waiting for ${description}`));
      }, timeoutMs);
      const wrapped = (event) => {
        clearTimeout(timer);
        resolve(event);
      };
      this.waiters.push({ predicate, resolve: wrapped });
    });
  }

  send(message) {
    if (!this.sock) throw new Error("control socket is not connected");
    log("control", JSON.stringify(message).slice(0, 500));
    this.sock.write(`${JSON.stringify(message)}\n`);
  }

  approve(event) {
    this.send({
      action: "approve",
      session_id: event.session_id,
      id: event.id,
    });
  }

  deny(event) {
    this.send({ action: "deny", session_id: event.session_id, id: event.id });
  }

  observeDescendants() {
    if (!this.child?.pid) return;
    for (const row of descendantsOf(this.child.pid)) {
      this.ownedDescendants.add(row.pid);
    }
  }

  async stop() {
    this.observeDescendants();
    if (this.sock) {
      this.sock.destroy();
      this.sock = null;
    }
    if (!this.exited) {
      this.child.kill("SIGTERM");
      const result = await Promise.race([
        this.exitPromise,
        sleep(15_000).then(() => null),
      ]);
      if (!result) {
        log("cleanup", "Intendant ignored SIGTERM; sending SIGKILL");
        this.child.kill("SIGKILL");
        await this.exitPromise;
      }
    }
    // Product cleanup should have reaped these. This fallback is deliberately
    // restricted to PIDs proven to have descended from this Intendant.
    for (const pid of this.ownedDescendants) {
      try {
        process.kill(pid, 0);
        process.kill(pid, "SIGTERM");
      } catch {
        // Already gone.
      }
    }
    await sleep(500);
    for (const pid of this.ownedDescendants) {
      try {
        process.kill(pid, 0);
        process.kill(pid, "SIGKILL");
      } catch {
        // Already gone.
      }
    }
    try {
      fs.rmSync(this.socketPath, { force: true });
    } catch {
      // Best effort; the server normally unlinks it.
    }
  }
}

function shellDisplay(value) {
  return /^[A-Za-z0-9_./:=+-]+$/.test(value) ? value : JSON.stringify(value);
}

async function httpJson(port, pathname, options = {}, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(
        `http://127.0.0.1:${port}${pathname}`,
        options,
      );
      const text = await response.text();
      let body;
      try {
        body = text ? JSON.parse(text) : null;
      } catch {
        body = { text };
      }
      if (!response.ok) {
        throw new Error(
          `${response.status} ${JSON.stringify(body).slice(0, 500)}`,
        );
      }
      return body;
    } catch (error) {
      lastError = error;
      await sleep(250);
    }
  }
  throw new Error(`HTTP ${pathname} failed: ${lastError}`);
}

async function pollJson(
  port,
  pathname,
  predicate,
  timeoutMs = 45_000,
  describeLast,
) {
  const deadline = Date.now() + timeoutMs;
  let lastBody;
  let lastError;
  while (Date.now() < deadline) {
    try {
      lastBody = await httpJson(port, pathname, {}, 5_000);
      if (predicate(lastBody)) return lastBody;
    } catch (error) {
      lastError = error;
    }
    await sleep(750);
  }
  const lastDescription = describeLast
    ? describeLast(lastBody)
    : JSON.stringify(lastBody).slice(0, 500);
  throw new Error(
    `poll ${pathname} timed out; last=${lastDescription} ` +
      `error=${lastError || "none"}`,
  );
}

async function upload(port, name, mime, bytes) {
  return httpJson(
    port,
    `/api/session/current/uploads?name=${encodeURIComponent(name)}&destination=task`,
    {
      method: "POST",
      headers: { "content-type": mime },
      body: bytes,
    },
  );
}

async function threadAction(
  run,
  sessionId,
  op,
  params = {},
  timeoutMs = 90_000,
) {
  const mark = run.mark();
  run.send({
    action: "thread_action",
    session_id: sessionId,
    op,
    params,
  });
  return run.waitFor(
    `/${op} result`,
    (event) =>
      resultEvent(event, op) &&
      (!event.session_id || event.session_id === sessionId),
    timeoutMs,
    mark,
  );
}

async function expectAction(
  run,
  sessionId,
  op,
  params = {},
  timeoutMs = 90_000,
) {
  const result = await threadAction(run, sessionId, op, params, timeoutMs);
  check(`${op}-dispatches`, result.success === true, result.message || "");
  return result;
}

async function pollActiveTools(
  run,
  sessionId,
  expected,
  timeoutMs = 30_000,
) {
  const deadline = Date.now() + timeoutMs;
  let lastResult;
  while (Date.now() < deadline) {
    lastResult = await threadAction(run, sessionId, "tools", {}, 15_000);
    if (
      lastResult.success === true &&
      sameStrings(activeToolNames(lastResult.message), expected)
    ) {
      return lastResult;
    }
    await sleep(250);
  }
  throw new Error(
    `active tools did not converge to ${JSON.stringify(expected)}; last=` +
      `${lastResult?.message || "none"}`,
  );
}

async function exerciseBackgroundTask(run, port, wrapperId, sessionId) {
  // Use one short child to prove output persistence and a second long child
  // to prove live inspection and cancellation. Kimi 0.28 can stall a coder
  // subagent's model stream before a manual-mode tool call is emitted, so
  // child approval routing stays covered by hermetic event tests while this
  // authenticated phase exercises the native background surface in yolo
  // mode. Main-agent native approvals are exercised above.
  await expectAction(run, sessionId, "permission-mode", { mode: "yolo" });
  const completedMark = run.mark();
  run.send({
    action: "follow_up",
    session_id: sessionId,
    direct: true,
    text:
      'Call the Agent tool exactly once with description "kimi e2e output", ' +
      'subagent_type "coder", run_in_background true, and prompt: ' +
      "`Reply with exactly BACKGROUND_CHILD_OUTPUT_OK and do not use tools.` " +
      "After launching it, do not poll or wait; reply exactly " +
      "BACKGROUND_OUTPUT_LAUNCHED.",
  });
  const completedRelationship = await run.waitFor(
    "background output subagent relationship",
    (event) =>
      event.event === "session_relationship" &&
      event.relationship === "subagent" &&
      event.parent_session_id === sessionId,
    180_000,
    completedMark,
  );
  await run.waitFor(
    "background output launch response",
    (event) =>
      event.event === "model_response" &&
      responseText(event).includes("BACKGROUND_OUTPUT_LAUNCHED"),
    180_000,
    completedMark,
  );
  await run.waitFor(
    "parent idle after background output launch",
    (event) => isTurnEnd(event) && eventTargets(event, wrapperId, sessionId),
    180_000,
    completedMark,
  );
  check(
    "background-subagent-scoped",
    /^session_.+:.+/.test(completedRelationship.child_session_id || ""),
    completedRelationship.child_session_id || "",
  );
  let taskList;
  let taskLine;
  const completedTaskDeadline = Date.now() + 120_000;
  while (Date.now() < completedTaskDeadline) {
    taskList = await threadAction(run, sessionId, "tasks");
    taskLine = (taskList.message || "")
      .split("\n")
      .find(
        (line) => /completed/i.test(line) && /kimi e2e output/i.test(line),
      );
    if (taskLine) break;
    await sleep(500);
  }
  const outputTaskId = taskLine?.split("\t")[0]?.trim();
  check(
    "background-task-listed",
    Boolean(outputTaskId),
    taskList?.message || "",
  );
  if (!outputTaskId) {
    throw new Error("could not recover completed Kimi background task id");
  }
  let taskOutput;
  const taskOutputDeadline = Date.now() + 45_000;
  while (Date.now() < taskOutputDeadline) {
    taskOutput = await threadAction(run, sessionId, "task-output", {
      task_id: outputTaskId,
      output_bytes: 65_536,
    });
    if (
      taskOutput.success !== true ||
      /BACKGROUND_CHILD_OUTPUT_OK/.test(taskOutput.message || "")
    ) {
      break;
    }
    await sleep(750);
  }
  check(
    "task-output-dispatches",
    taskOutput?.success === true,
    taskOutput?.message || "",
  );
  check(
    "background-task-native-output",
    /BACKGROUND_CHILD_OUTPUT_OK/.test(taskOutput?.message || ""),
    (taskOutput?.message || "").slice(0, 500),
  );
  const inspector = await pollJson(
    port,
    `/api/session/${encodeURIComponent(sessionId)}/background-tasks`,
    (body) =>
      body.supported === true &&
      body.source === "kimi" &&
      body.tasks?.some(
        (task) => task.taskId === outputTaskId && task.hasOutput === true,
      ),
    45_000,
  );
  const inspectorTask = inspector.tasks?.find(
    (task) => task.taskId === outputTaskId,
  );
  check(
    "background-task-http-inspector",
    inspector.supported === true &&
      inspector.source === "kimi" &&
      inspectorTask?.status === "completed" &&
      inspectorTask?.running !== true,
    JSON.stringify(inspector).slice(0, 600),
  );
  const output = await httpJson(
    port,
    `/api/session/${encodeURIComponent(sessionId)}/background-tasks/` +
      `${encodeURIComponent(outputTaskId)}/output?tail_kb=64`,
  );
  check(
    "background-task-http-output",
    output.taskId === outputTaskId &&
      typeof output.content === "string" &&
      /BACKGROUND_CHILD_OUTPUT_OK/.test(output.content),
    JSON.stringify(output).slice(0, 500),
  );

  const cancelMark = run.mark();
  run.send({
    action: "follow_up",
    session_id: sessionId,
    direct: true,
    text:
      'Call the Agent tool exactly once with description "kimi e2e cancel", ' +
      'subagent_type "coder", run_in_background true, and prompt: ' +
      "`Call Bash exactly once with command for i in $(seq 1 120); do echo " +
      "BG_CANCEL_TICK_$i; sleep 1; done, timeout 180, and " +
      "run_in_background false (or omitted). Do not speak before the tool " +
      "call. After Bash finishes, reply BG_CANCEL_FINISHED.` After launching " +
      "it, do not poll or wait; reply exactly BACKGROUND_CANCEL_LAUNCHED.",
  });
  const cancelRelationship = await run.waitFor(
    "background cancellable subagent relationship",
    (event) =>
      event.event === "session_relationship" &&
      event.relationship === "subagent" &&
      event.parent_session_id === sessionId &&
      event.child_session_id !== completedRelationship.child_session_id,
    180_000,
    cancelMark,
  );
  await run.waitFor(
    "background cancel launch response",
    (event) =>
      event.event === "model_response" &&
      responseText(event).includes("BACKGROUND_CANCEL_LAUNCHED"),
    180_000,
    cancelMark,
  );
  await run.waitFor(
    "cancellable child Bash starts",
    (event) =>
      event.event === "agent_started" &&
      event.session_id === cancelRelationship.child_session_id &&
      /BG_CANCEL_TICK_|seq 1 120/i.test(event.commands_preview || ""),
    180_000,
    cancelMark,
  );
  let cancelTaskLine;
  let cancelTaskList;
  const runningTaskDeadline = Date.now() + 45_000;
  while (Date.now() < runningTaskDeadline) {
    cancelTaskList = await threadAction(run, sessionId, "tasks");
    cancelTaskLine = (cancelTaskList.message || "")
      .split("\n")
      .find(
        (line) => /running/i.test(line) && /kimi e2e cancel/i.test(line),
      );
    if (cancelTaskLine) break;
    await sleep(500);
  }
  const cancelTaskId = cancelTaskLine?.split("\t")[0]?.trim();
  check(
    "background-running-task-listed",
    Boolean(cancelTaskId),
    cancelTaskList?.message || "",
  );
  if (!cancelTaskId) {
    throw new Error("could not recover running Kimi background task id");
  }
  const runningInspector = await pollJson(
    port,
    `/api/session/${encodeURIComponent(sessionId)}/background-tasks`,
    (body) =>
      body.supported === true &&
      body.source === "kimi" &&
      body.tasks?.some(
        (task) => task.taskId === cancelTaskId && task.running === true,
      ),
    45_000,
  );
  check(
    "background-running-task-http-inspector",
    runningInspector.tasks?.some(
      (task) => task.taskId === cancelTaskId && task.running === true,
    ),
    JSON.stringify(runningInspector).slice(0, 600),
  );
  const cancel = await expectAction(run, sessionId, "task-cancel", {
    task_id: cancelTaskId,
  });
  check(
    "background-task-cancelled",
    /cancelled|already/i.test(cancel.message || ""),
    cancel.message || "",
  );
}

function setupProject() {
  if (WORKDIR_ARG && fs.existsSync(WORKDIR)) {
    const entries = fs.readdirSync(WORKDIR);
    if (entries.length > 0) {
      throw new Error(
        `--workdir must name a new or empty directory; refusing to mutate nonempty ${WORKDIR}`,
      );
    }
  }
  fs.mkdirSync(WORKDIR, { recursive: true });
  fs.mkdirSync(STATE_HOME, { recursive: true, mode: 0o700 });
  fs.writeFileSync(
    path.join(WORKDIR, "README.md"),
    "Kimi external-agent E2E playground\n",
  );
  fs.writeFileSync(path.join(WORKDIR, ".gitignore"), ".intendant/\n");
  fs.writeFileSync(
    path.join(WORKDIR, "intendant.toml"),
    [
      "[agent]",
      'default_backend = "kimi"',
      "",
      "[agent.kimi]",
      `command = ${tomlString(KIMI_COMMAND)}`,
      `model = ${tomlString(MODEL)}`,
      'thinking = "high"',
      'permission_mode = "manual"',
      "plan_mode = false",
      "swarm_mode = false",
      "",
    ].join("\n"),
  );
  fs.writeFileSync(
    path.join(WORKDIR, ".mcp.json"),
    JSON.stringify(
      {
        mcpServers: {
          // Force the bridge-name collision path. Intendant must publish its
          // scoped bearer server under a stable alternate name, and the live
          // MCP phase must still discover and call that alternate tool.
          intendant: { url: "http://127.0.0.1:9/intentionally-unreachable" },
        },
      },
      null,
      2,
    ),
  );
  if (!fs.existsSync(path.join(WORKDIR, ".git"))) {
    execSync("git init -q -b main", { cwd: WORKDIR });
  }
  execSync(
    "git add -A && git -c user.email=e2e@local -c user.name=e2e " +
      "-c commit.gpgsign=false commit -qm seed --allow-empty",
    { cwd: WORKDIR, shell: "/bin/zsh" },
  );
}

async function exerciseVaultSigninCeremony(port, stateHome) {
  const credential = path.join(KIMI_HOME, "credentials", "kimi-code.json");
  const before = fs.readFileSync(credential);
  let ceremonyActive = false;
  let cancelled = false;
  try {
    const started = await httpJson(port, "/api/kimi-auth/start", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ mode: "kimi-code" }),
    });
    ceremonyActive = started?.ok === true;
    check(
      "vault-kimi-auth-starts-isolated-device-flow",
      ceremonyActive && started?.status?.provider === "kimi",
    );

    const status = await pollJson(
      port,
      "/api/kimi-auth/status",
      (body) =>
        body?.phase === "awaiting_user" ||
        body?.phase === "failed" ||
        body?.phase === "timed_out",
      45_000,
      (body) => JSON.stringify({ phase: body?.phase, error: body?.error }),
    );
    let parsedUrl;
    try {
      parsedUrl = new URL(String(status.url || ""));
    } catch {
      parsedUrl = null;
    }
    const currentUrl =
      parsedUrl?.protocol === "https:" &&
      ["kimi.com", "www.kimi.com"].includes(parsedUrl.hostname) &&
      parsedUrl.pathname.replace(/\/+$/, "") === "/code/authorize_device";
    const legacyUrl =
      parsedUrl?.protocol === "https:" &&
      (parsedUrl.hostname === "auth.kimi.com" ||
        parsedUrl.hostname.endsWith(".auth.kimi.com"));
    check(
      "vault-kimi-auth-publishes-provider-url",
      status.phase === "awaiting_user" && (currentUrl || legacyUrl),
      parsedUrl ? `${parsedUrl.hostname}${parsedUrl.pathname}` : status.phase,
    );
    check(
      "vault-kimi-auth-publishes-code-and-deadline",
      /^[A-Za-z0-9-]{4,64}$/.test(String(status.user_code || "")) &&
        Number(status.deadline_unix_ms) > Date.now(),
    );

    const cancel = await httpJson(port, "/api/kimi-auth/cancel", {
      method: "POST",
    });
    cancelled = cancel?.ok === true && cancel?.phase === "cancelled";
    ceremonyActive = false;
    check("vault-kimi-auth-cancels-cleanly", cancelled);

    const cleanupDeadline = Date.now() + 5_000;
    const ceremonyRoot = path.join(stateHome, "kimi-auth");
    while (fs.existsSync(ceremonyRoot) && Date.now() < cleanupDeadline) {
      await sleep(50);
    }
    const after = fs.readFileSync(credential);
    const credentialUnchanged = before.equals(after);
    after.fill(0);
    const privateMode =
      process.platform === "win32" ||
      (fs.statSync(credential).mode & 0o077) === 0;
    check(
      "vault-kimi-auth-cancel-preserves-primary",
      credentialUnchanged && privateMode,
    );
    check(
      "vault-kimi-auth-cleans-isolated-home",
      !fs.existsSync(ceremonyRoot),
    );
  } finally {
    before.fill(0);
    if (ceremonyActive && !cancelled) {
      await httpJson(
        port,
        "/api/kimi-auth/cancel",
        { method: "POST" },
        5_000,
      ).catch(() => {});
    }
  }
}

async function exerciseVaultSigninCeremonyOnIdleDaemon() {
  fs.mkdirSync(VAULT_STATE_HOME, { recursive: true, mode: 0o700 });
  const port = await freePort();
  const cliArgs = [
    "--no-tui",
    "--web",
    String(port),
    "--bind",
    "127.0.0.1",
    "--no-tls",
  ];
  log("vault-spawn", `${BINARY} ${cliArgs.map(shellDisplay).join(" ")}`);
  const child = spawn(BINARY, cliArgs, {
    cwd: WORKDIR,
    env: intendantEnv(VAULT_STATE_HOME),
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.on("data", (data) => {
    for (const line of data.toString().split("\n")) {
      if (line.trim()) log("vault-out", line.trim().slice(0, 500));
    }
  });
  child.stderr.on("data", (data) => {
    for (const line of data.toString().split("\n")) {
      if (line.trim()) log("vault-err", line.trim().slice(0, 500));
    }
  });
  let exited = false;
  const exitPromise = new Promise((resolve) => {
    child.on("exit", (code, signal) => {
      exited = true;
      log("vault-daemon", `exited code=${code} signal=${signal}`);
      resolve({ code, signal });
    });
  });
  try {
    await exerciseVaultSigninCeremony(port, VAULT_STATE_HOME);
  } finally {
    if (!exited) child.kill("SIGTERM");
    let result = await Promise.race([
      exitPromise,
      sleep(15_000).then(() => null),
    ]);
    if (!result) {
      log("vault-cleanup", "idle daemon ignored SIGTERM; sending SIGKILL");
      child.kill("SIGKILL");
      result = await exitPromise;
    }
  }
}

async function scenario(run, port, kimiVersion) {
  await run.connect();

  const identity = await run.waitFor(
    "native Kimi session identity",
    (event) =>
      event.event === "session_identity" &&
      event.source === "kimi" &&
      /^session_[A-Za-z0-9_-]+$/.test(event.backend_session_id || ""),
    90_000,
  );
  const wrapperId = identity.session_id;
  const sessionId = identity.backend_session_id;
  check("native-session-id", true, `wrapper=${wrapperId} native=${sessionId}`);

  const capabilities = await run.waitFor(
    "Kimi capabilities",
    (event) =>
      event.event === "session_capabilities" &&
      event.capabilities &&
      eventTargets(event, wrapperId, sessionId),
    60_000,
  );
  const advertised = capabilities.capabilities.thread_actions || [];
  const requiredActions = [
    "compact",
    "fork",
    "side",
    "side-close",
    "undo",
    "rename",
    "archive",
    "restore",
    "goal-set",
    "goal-get",
    "goal-edit",
    "goal-pause",
    "goal-resume",
    "goal-complete",
    "goal-clear",
    "review",
    "fast",
    "model",
    "models",
    "thinking",
    "permission-mode",
    "plan-mode",
    "swarm-mode",
    "tasks",
    "task-output",
    "task-cancel",
    "tools",
    "tools-set",
    "tools-all",
    "context-clear",
  ];
  check(
    "capabilities-advertise-full-kimi-surface",
    capabilities.capabilities.follow_up &&
      capabilities.capabilities.steer &&
      capabilities.capabilities.interrupt &&
      requiredActions.every((op) => advertised.includes(op)),
    JSON.stringify(advertised),
  );
  check(
    "unrelated-memory-reset-not-advertised",
    !advertised.includes("memory-reset"),
    JSON.stringify(advertised),
  );
  // Initial prompt deliberately blocks on Kimi's native structured question,
  // which gives the socket time to observe identity and capability startup.
  const question = await run.waitFor(
    "native structured question",
    (event) =>
      event.event === "user_question" &&
      event.questions?.some((item) => item.question === QUESTION),
    120_000,
  );
  const questionItem = question.questions.find(
    (item) => item.question === QUESTION,
  );
  check(
    "structured-question-schema",
    questionItem.header === "Lane" &&
      questionItem.multi_select === false &&
      questionItem.options?.some((option) => option.label === QUESTION_ANSWER),
    JSON.stringify(questionItem).slice(0, 350),
  );
  const questionMark = run.mark();
  run.send({
    action: "answer_question",
    session_id: question.session_id,
    id: question.id,
    answers: { [QUESTION]: QUESTION_ANSWER },
  });
  const questionResponse = await run.waitFor(
    "question answer reflected by Kimi",
    (event) =>
      event.event === "model_response" &&
      responseText(event).includes(`QUESTION_ANSWER=${QUESTION_ANSWER}`),
    120_000,
    questionMark,
  );
  check("structured-question-round-trip", Boolean(questionResponse));
  await run.waitFor(
    "question turn end",
    (event) => isTurnEnd(event) && eventTargets(event, wrapperId, sessionId),
    120_000,
    questionMark,
  );
  if (BACKGROUND_ONLY) {
    await exerciseBackgroundTask(run, port, wrapperId, sessionId);
    return;
  }

  // A one-item answer to a multi-select question must stay wire-typed as
  // `multi`; Kimi distinguishes it from a single-select response even though
  // both contain one option id.
  const multiQuestionMark = run.mark();
  run.send({
    action: "follow_up",
    session_id: sessionId,
    direct: true,
    text:
      "Before doing anything else, call AskUserQuestion exactly once. Ask " +
      `${JSON.stringify(MULTI_QUESTION)} with header "Flags", multi-select ` +
      'enabled, and options "Protocol" (description "Wire protocol") and ' +
      '"History" (description "History persistence"). After the answer, reply ' +
      "with exactly MULTI_ANSWER=<comma-separated chosen labels> and do " +
      "nothing else.",
  });
  const multiQuestion = await run.waitFor(
    "native one-choice multi-select question",
    (event) =>
      event.event === "user_question" &&
      event.questions?.some((item) => item.question === MULTI_QUESTION),
    120_000,
    multiQuestionMark,
  );
  const multiQuestionItem = multiQuestion.questions.find(
    (item) => item.question === MULTI_QUESTION,
  );
  check(
    "structured-multi-question-schema",
    multiQuestionItem.multi_select === true &&
      multiQuestionItem.options?.some(
        (option) => option.label === MULTI_ANSWER,
      ),
    JSON.stringify(multiQuestionItem).slice(0, 350),
  );
  const multiAnswerMark = run.mark();
  run.send({
    action: "answer_question",
    session_id: multiQuestion.session_id,
    id: multiQuestion.id,
    answers: { [MULTI_QUESTION]: MULTI_ANSWER },
  });
  const multiQuestionResponse = await run.waitFor(
    "one-choice multi-select answer reflected by Kimi",
    (event) =>
      event.event === "model_response" &&
      responseText(event).includes(`MULTI_ANSWER=${MULTI_ANSWER}`),
    120_000,
    multiAnswerMark,
  );
  check(
    "structured-one-choice-multi-round-trip",
    Boolean(multiQuestionResponse),
  );
  await run.waitFor(
    "multi question turn end",
    (event) => isTurnEnd(event) && eventTargets(event, wrapperId, sessionId),
    120_000,
    multiAnswerMark,
  );

  // Exercise the generated, bearer-authenticated MCP bridge itself. This is
  // intentionally a read-only deterministic tool, but it proves Kimi loaded
  // the scoped server declaration and can reach Intendant through it.
  const mcpMark = run.mark();
  let mcpApproval = null;
  run.approvalResponder = (event) => {
    if (
      /intendant.*list_displays|list_displays.*intendant/i.test(
        event.command || "",
      )
    ) {
      mcpApproval = event;
      run.approve(event);
    }
  };
  run.send({
    action: "follow_up",
    session_id: sessionId,
    direct: true,
    text:
      "Use the injected Intendant MCP server's list_displays tool exactly " +
      "once. Do not use Bash or any other tool. After it succeeds, reply " +
      "with exactly INTENDANT_MCP_OK and do nothing else.",
  });
  const mcpToolStart = await run.waitFor(
    "injected Intendant MCP tool start",
    (event) =>
      event.event === "agent_started" &&
      eventTargets(event, wrapperId, sessionId) &&
      /intendant.*list_displays|list_displays.*intendant/i.test(
        event.commands_preview || "",
      ),
    120_000,
    mcpMark,
  );
  const mcpToolOutput = await run.waitFor(
    "injected Intendant MCP tool output",
    (event) =>
      event.event === "agent_output" &&
      eventTargets(event, wrapperId, sessionId) &&
      event.item_id === mcpToolStart.item_id &&
      (event.stdout || "").trim().length > 0,
    120_000,
    mcpMark,
  );
  const mcpResponse = await run.waitFor(
    "injected Intendant MCP completion response",
    (event) =>
      event.event === "model_response" &&
      eventTargets(event, wrapperId, sessionId) &&
      responseText(event).trim() === "INTENDANT_MCP_OK",
    120_000,
    mcpMark,
  );
  await run.waitFor(
    "injected Intendant MCP turn end",
    (event) => isTurnEnd(event) && eventTargets(event, wrapperId, sessionId),
    120_000,
    mcpMark,
  );
  run.approvalResponder = null;
  check(
    "injected-intendant-mcp-approval-routing",
    !/^0\.28\./.test(kimiVersion) || Boolean(mcpApproval),
    mcpApproval?.command || `Kimi ${kimiVersion} required no approval`,
  );
  check(
    "injected-intendant-mcp-round-trip",
    Boolean(mcpToolStart.item_id) &&
      Boolean(mcpToolOutput) &&
      Boolean(mcpResponse) &&
      !/denied|forbidden|unauthorized/i.test(mcpToolOutput.stdout || ""),
    `${mcpToolStart.commands_preview || ""} ${(mcpToolOutput.stdout || "").slice(0, 300)}`,
  );

  // Upload one ordinary file and one image through the real dashboard route,
  // then target the live external session with StartTask attachments.
  const textUpload = await upload(
    port,
    "e2e-token.txt",
    "text/plain",
    Buffer.from(`${ATTACHMENT_TOKEN}\n`, "utf8"),
  );
  const redPng = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR4nGP4" +
      "/x8AAusB9Y9Z4wAAAABJRU5ErkJggg==",
    "base64",
  );
  const imageUpload = await upload(port, "red-pixel.png", "image/png", redPng);
  check(
    "dashboard-upload-stages-file-and-image",
    Boolean(textUpload.id && imageUpload.id),
    `file=${textUpload.id} image=${imageUpload.id}`,
  );

  const attachmentMark = run.mark();
  let writeApproval = null;
  run.approvalResponder = (event) => {
    if (/probe\.txt|write/i.test(event.command || "")) {
      writeApproval = event;
      run.approve(event);
    }
  };
  run.send({
    action: "start_task",
    session_id: sessionId,
    task:
      `The attached text file contains a token. Remember both that token and ` +
      `the conversation codeword ${KEEP_CODEWORD}. The attached image is red. ` +
      "Use the Write tool (not Bash) to create probe.txt containing exactly " +
      `${ATTACHMENT_TOKEN}. Then reply with exactly ` +
      `ATTACHMENT_OK=${ATTACHMENT_TOKEN}; COLOR=red; CODEWORD=${KEEP_CODEWORD}.`,
    direct: true,
    attachments: [`upload:${textUpload.id}`, `upload:${imageUpload.id}`],
  });
  const attachmentResponse = await run.waitFor(
    "attachment response",
    (event) =>
      event.event === "model_response" &&
      responseText(event).includes(ATTACHMENT_TOKEN) &&
      responseText(event).includes(KEEP_CODEWORD),
    180_000,
    attachmentMark,
  );
  await run.waitFor("attachment turn end", isTurnEnd, 180_000, attachmentMark);
  run.approvalResponder = null;
  check(
    "native-file-and-image-attachments",
    /red/i.test(responseText(attachmentResponse)),
    responseText(attachmentResponse).slice(0, 300),
  );
  const probePath = path.join(WORKDIR, "probe.txt");
  check(
    "workspace-write-ran",
    fs.existsSync(probePath) &&
      fs.readFileSync(probePath, "utf8").trim() === ATTACHMENT_TOKEN,
    fs.existsSync(probePath)
      ? JSON.stringify(fs.readFileSync(probePath, "utf8"))
      : "missing",
  );
  check(
    "tool-start-streamed",
    run.events
      .slice(attachmentMark)
      .some(
        (event) =>
          event.event === "agent_started" &&
          /probe\.txt|write/i.test(event.commands_preview || ""),
      ),
  );
  check(
    "file-diff-surfaced",
    run.events
      .slice(attachmentMark)
      .some(
        (event) =>
          event.event === "log_entry" &&
          event.source === "Diff" &&
          /probe\.txt|ATTACHMENT_/.test(event.content || ""),
      ) ||
      run.events
        .slice(attachmentMark)
        .some(
          (event) =>
            event.event === "file_changed" &&
            event.path === "probe.txt" &&
            event.lines_added > 0,
        ) ||
      /--- before|\+\+\+ after|ATTACHMENT_/.test(
        writeApproval?.command || "",
      ),
    writeApproval?.command || "safe workspace write",
  );

  const usage = run.events.find(
    (event) => event.event === "usage_update" && event.main?.tokens_used > 0,
  );
  const nonzeroMainUsage = run.events.filter(
    (event) => event.event === "usage_update" && event.main?.tokens_used > 0,
  );
  const allowedUsageModels = new Set(
    [MODEL, MODEL.split("/").at(-1), MODEL_DISPLAY].map((model) =>
      model.toLowerCase(),
    ),
  );
  const canonicalUsage = nonzeroMainUsage.find((event) =>
    allowedUsageModels.has(String(event.main?.model || "").toLowerCase()),
  );
  const forbiddenUsage = nonzeroMainUsage.filter((event) =>
    /highspeed|k3/i.test(event.main?.model || ""),
  );
  check(
    "usage-reported",
    Boolean(usage) && Boolean(canonicalUsage) && forbiddenUsage.length === 0,
    usage
      ? `${usage.main.model} ${usage.main.tokens_used}/${usage.main.context_window}`
      : "none",
  );
  check(
    "reasoning-streamed",
    run.events.some(
      (event) =>
        event.event === "model_response" &&
        (event.reasoning_summary || "").trim(),
    ),
  );
  check(
    "text-streamed-incrementally",
    run.events.some(
      (event) =>
        event.event === "model_response_delta" && (event.text || "").length > 0,
    ),
  );

  // A denied native approval must leave the file intact. This prompt is also
  // the turn that historical-fork and undo tests remove later.
  const denyMark = run.mark();
  run.approvalResponder = (event) => run.deny(event);
  run.send({
    action: "follow_up",
    session_id: sessionId,
    direct: true,
    text:
      `Remember the new conversation codeword ${DROP_CODEWORD}. Then use Bash ` +
      "exactly once to delete probe.txt. If denied, reply exactly " +
      `DELETE_DENIED; CODEWORD=${DROP_CODEWORD}, and do not try another tool.`,
  });
  const denied = await run.waitFor(
    "delete approval",
    (event) =>
      event.event === "approval_required" &&
      /probe\.txt|rm /i.test(event.command || ""),
    120_000,
    denyMark,
  );
  await run.waitFor("denied delete turn end", isTurnEnd, 180_000, denyMark);
  run.approvalResponder = null;
  check("denied-tool-blocked", fs.existsSync(probePath), denied.command || "");

  // Historical Kimi forks are exact active-real-user turn boundaries. Pick
  // the point immediately before the DROP turn, fork there, and require the
  // child to remember the earlier attachment/codeword but not the dropped one.
  const forkCatalogPath = `/api/session/${encodeURIComponent(sessionId)}/fork-points?limit=200`;
  const catalog = await pollJson(
    port,
    forkCatalogPath,
    (body) =>
      body.supported === true &&
      body.fork_points?.some(
        (point) =>
          point.kind === "turn-boundary" &&
          Boolean(point.turn) &&
          (point.preview || "").includes(DROP_CODEWORD),
      ),
    45_000,
  );
  const historicalPoint = catalog.fork_points.find(
    (point) =>
      point.kind === "turn-boundary" &&
      (point.preview || "").includes(DROP_CODEWORD),
  );
  check(
    "historical-fork-point-catalog",
    Boolean(historicalPoint?.turn),
    JSON.stringify(historicalPoint),
  );
  if (!historicalPoint?.turn) {
    throw new Error("Kimi historical turn boundary was not present");
  }
  const historicalMark = run.mark();
  const forkRequestId = `kimi-e2e-fork-${randomToken()}`;
  run.send({
    action: "fork_session_at_anchor",
    source: "kimi",
    session_id: sessionId,
    resume_id: sessionId,
    anchor: { kind: historicalPoint.kind, turn: historicalPoint.turn },
    name: "Kimi historical E2E child",
    task:
      "Reply with the attachment token and conversation codewords you remember. " +
      "Do not inspect files and do not guess anything missing.",
    project_root: WORKDIR,
    request_id: forkRequestId,
  });
  const forkResult = await run.waitFor(
    "historical fork accepted",
    (event) =>
      event.event === "session_fork_result" &&
      event.request_id === forkRequestId,
    90_000,
    historicalMark,
  );
  check(
    "historical-fork-dispatches",
    !forkResult.error && forkResult.relationship === "anchor-fork",
    JSON.stringify(forkResult),
  );
  const childStartup = await run.waitFor(
    "historical child identity or startup failure",
    (event) =>
      (event.event === "session_identity" &&
        event.source === "kimi" &&
        event.backend_session_id !== sessionId) ||
      (event.event === "session_ended" &&
        event.source === "kimi" &&
        event.session_id !== wrapperId &&
        Boolean(event.error)),
    120_000,
    historicalMark,
  );
  if (childStartup.event === "session_ended") {
    throw new Error(
      `historical child failed before identity: ${childStartup.error}`,
    );
  }
  const childIdentity = childStartup;
  const historicalChild = childIdentity.backend_session_id;
  const relationship = await run.waitFor(
    "historical fork relationship",
    (event) =>
      event.event === "session_relationship" &&
      event.relationship === "anchor-fork" &&
      event.child_session_id === historicalChild,
    120_000,
    historicalMark,
  );
  check(
    "historical-fork-lineage",
    relationship.parent_session_id === sessionId,
    JSON.stringify(relationship),
  );
  const childRecall = await run.waitFor(
    "historical child recall",
    (event) =>
      event.event === "model_response" &&
      eventTargets(event, childIdentity.session_id, historicalChild) &&
      responseText(event).includes(KEEP_CODEWORD),
    180_000,
    historicalMark,
  );
  check(
    "historical-fork-is-exact",
    responseText(childRecall).includes(ATTACHMENT_TOKEN) &&
      !responseText(childRecall).includes(DROP_CODEWORD),
    responseText(childRecall).slice(0, 400),
  );
  run.send({ action: "stop_session", session_id: childIdentity.session_id });

  // Parent-native undo removes precisely the DROP turn.
  await expectAction(run, sessionId, "undo", { count: 1 });
  const undoMark = run.mark();
  run.send({
    action: "follow_up",
    session_id: sessionId,
    direct: true,
    text:
      "Reply with the attachment token and conversation codewords still present " +
      "before this prompt. Do not inspect files and do not guess.",
  });
  const undoRecall = await run.waitFor(
    "post-undo recall",
    (event) =>
      event.event === "model_response" &&
      eventTargets(event, wrapperId, sessionId) &&
      responseText(event).includes(KEEP_CODEWORD),
    180_000,
    undoMark,
  );
  check(
    "native-undo-removes-latest-turn",
    responseText(undoRecall).includes(ATTACHMENT_TOKEN) &&
      !responseText(undoRecall).includes(DROP_CODEWORD),
    responseText(undoRecall).slice(0, 400),
  );

  // Live profile controls are REST-backed and do not require a restart.
  const modelResult = await expectAction(run, sessionId, "model", {
    model: MODEL,
  });
  check(
    "live-model-is-k2.7-coding",
    modelResult.message?.includes(MODEL),
    modelResult.message || MODEL_DISPLAY,
  );
  await expectAction(run, sessionId, "thinking", { thinking: "medium" });
  await expectAction(run, sessionId, "thinking", { thinking: "high" });
  await expectAction(run, sessionId, "permission-mode", { mode: "yolo" });
  await expectAction(run, sessionId, "plan-mode", { enabled: true });
  await expectAction(run, sessionId, "plan-mode", { enabled: false });
  await expectAction(run, sessionId, "swarm-mode", { enabled: true });
  await expectAction(run, sessionId, "swarm-mode", { enabled: false });

  const models = await expectAction(run, sessionId, "models");
  const modelLines = String(models.message || "").split("\n");
  const canonicalModelLine = modelLines.find((line) =>
    line.startsWith(`${MODEL}\t`),
  );
  const highspeedModelLine = modelLines.find((line) =>
    line.startsWith(`${HIGHSPEED_MODEL}\t`),
  );
  check(
    "live-model-catalog-includes-k2.7-pair",
    Boolean(
      canonicalModelLine &&
        highspeedModelLine &&
        canonicalModelLine.includes(`\t${MODEL_DISPLAY}\t`) &&
        highspeedModelLine.includes(`\t${MODEL_DISPLAY} Highspeed\t`) &&
        /\t[1-9][0-9]* tokens$/.test(canonicalModelLine) &&
        /\t[1-9][0-9]* tokens$/.test(highspeedModelLine),
    ),
    (models.message || "").slice(0, 800),
  );
  const displayModelResult = await expectAction(run, sessionId, "model", {
    model: MODEL_DISPLAY,
  });
  check(
    "display-label-resolves-to-canonical-model",
    (displayModelResult.message || "").includes(MODEL) &&
      !(displayModelResult.message || "").includes(HIGHSPEED_MODEL),
    displayModelResult.message || "",
  );

  // Kimi's /fast is a real profile-model toggle. Toggle to the catalogued
  // Highspeed alias and immediately back without dispatching any model work,
  // so every actual turn in this scenario remains on canonical K2.7 Coding.
  const fastMark = run.mark();
  const fastOn = await expectAction(run, sessionId, "fast");
  check(
    "fast-selects-k2.7-highspeed",
    (fastOn.message || "").includes(HIGHSPEED_MODEL) ||
      /K2\.7 Coding Highspeed/i.test(fastOn.message || ""),
    fastOn.message || "",
  );
  const fastOff = await expectAction(run, sessionId, "fast");
  check(
    "second-fast-restores-canonical-k2.7",
    (fastOff.message || "").includes(MODEL) &&
      !(fastOff.message || "").includes(HIGHSPEED_MODEL),
    fastOff.message || "",
  );
  check(
    "no-model-turn-while-highspeed",
    !run.events
      .slice(fastMark)
      .some(
        (event) =>
          event.event === "model_response" ||
          event.event === "model_response_delta" ||
          event.event === "agent_started",
      ),
  );

  const tools = await expectAction(run, sessionId, "tools");
  const baselineTools = activeToolNames(tools.message);
  check(
    "active-tool-inventory",
    baselineTools.length > 2 &&
      baselineTools.some((name) =>
        /Bash|Write|Agent|AskUserQuestion/i.test(name),
      ),
    (tools.message || "").slice(0, 500),
  );

  const exactTools = ["Read", "Write"].sort();
  await expectAction(run, sessionId, "tools-set", { names: exactTools });
  const exactToolsReport = await expectAction(run, sessionId, "tools");
  const exactToolsReadback = activeToolNames(exactToolsReport.message);
  check(
    "active-tools-exact-replacement",
    sameStrings(exactToolsReadback, exactTools),
    JSON.stringify(exactToolsReadback),
  );

  await expectAction(run, sessionId, "tools-set", { names: [] });
  const emptyToolsReport = await expectAction(run, sessionId, "tools");
  check(
    "active-tools-empty-list-disables-all",
    activeToolNames(emptyToolsReport.message).length === 0 &&
      /no active tools/i.test(emptyToolsReport.message || ""),
    emptyToolsReport.message || "",
  );

  await expectAction(run, sessionId, "tools-all");
  const restoredToolsReport = await expectAction(run, sessionId, "tools");
  const restoredTools = activeToolNames(restoredToolsReport.message);
  const registeredTools = registeredToolNames(restoredToolsReport.message);
  check(
    "active-tools-all-restores-inventory",
    registeredTools.length > 2 &&
      sameStrings(restoredTools, registeredTools) &&
      baselineTools.every((name) => restoredTools.includes(name)) &&
      exactTools.every((name) => restoredTools.includes(name)),
    (restoredToolsReport.message || "").slice(0, 500),
  );

  // Review is a real Kimi turn rather than a fake local summary. Intendant
  // temporarily gives Kimi exactly zero tools and supplies a bounded
  // controller-built workspace evidence packet for this one prompt, then
  // restores a deliberately nontrivial exact active-tool set.
  // It runs only after /fast restored canonical K2.7 Coding.
  await expectAction(run, sessionId, "tools-set", { names: exactTools });
  const beforeReviewSnapshot = worktreeSnapshot(WORKDIR);
  const reviewMark = run.mark();
  await expectAction(
    run,
    sessionId,
    "review",
    {
      prompt:
        "Review probe.txt only. Confirm whether it contains exactly the " +
        `attachment token already present in this conversation. Do not edit ` +
        "any file. End with exactly KIMI_REVIEW_OK.",
    },
    180_000,
  );
  const reviewResponse = await run.waitFor(
    "enforced read-only review response",
    (event) =>
      event.event === "model_response" &&
      eventTargets(event, wrapperId, sessionId) &&
      responseText(event).includes("KIMI_REVIEW_OK"),
    180_000,
    reviewMark,
  );
  await run.waitFor(
    "enforced read-only review turn end",
    (event) => isTurnEnd(event) && eventTargets(event, wrapperId, sessionId),
    180_000,
    reviewMark,
  );
  check(
    "enforced-read-only-review-runs-without-editing",
    Boolean(reviewResponse) &&
      fs.existsSync(probePath) &&
      fs.readFileSync(probePath, "utf8").trim() === ATTACHMENT_TOKEN &&
      worktreeSnapshot(WORKDIR) === beforeReviewSnapshot,
    responseText(reviewResponse).slice(0, 400),
  );
  const postReviewTools = await pollActiveTools(
    run,
    sessionId,
    exactTools,
    30_000,
  );
  const postReviewToolNames = activeToolNames(postReviewTools.message);
  check(
    "review-restores-exact-tool-set",
    sameStrings(postReviewToolNames, exactTools),
    JSON.stringify(postReviewToolNames),
  );
  await expectAction(run, sessionId, "tools-all");
  const postReviewAllTools = await expectAction(run, sessionId, "tools");
  check(
    "review-all-tools-restored-for-later-phases",
    sameStrings(activeToolNames(postReviewAllTools.message), restoredTools),
    (postReviewAllTools.message || "").slice(0, 500),
  );

  // Native goal lifecycle includes Kimi's complete v2 budget surface and
  // explicit completion controls. The generous budgets are never exhausted.
  const goalObjective = `Preserve ${KEEP_CODEWORD} while running acceptance checks.`;
  const goalTokenBudget = 100_000;
  const goalTurnBudget = 1_000;
  const goalWallClockBudgetMs = 600_000;
  const goalMark = run.mark();
  await expectAction(run, sessionId, "goal-set", {
    objective: goalObjective,
  });
  const initialGoalEvent = await run.waitFor(
    "native initial goal event",
    (event) =>
      event.event === "session_goal" &&
      event.goal?.objective === goalObjective &&
      event.goal?.status === "active",
    60_000,
    goalMark,
  );
  check(
    "native-goal-set",
    initialGoalEvent.goal?.status === "active",
    JSON.stringify(initialGoalEvent.goal),
  );

  const goalBudgetMark = run.mark();
  const budgetResult = await expectAction(run, sessionId, "goal-edit", {
    token_budget: goalTokenBudget,
    turn_budget: goalTurnBudget,
    wall_clock_budget_ms: goalWallClockBudgetMs,
  });
  const goalEvent = await run.waitFor(
    "native goal token-budget event",
    (event) =>
      event.event === "session_goal" &&
      event.goal?.objective === goalObjective &&
      event.goal?.token_budget === goalTokenBudget,
    60_000,
    goalBudgetMark,
  );
  check(
    "native-goal-edit-all-budgets",
    goalEvent.goal?.status === "active" &&
      goalEvent.goal?.token_budget === goalTokenBudget &&
      (budgetResult.message || "").includes(
        `token_budget=${goalTokenBudget}`,
      ) &&
      (budgetResult.message || "").includes(`turn_budget=${goalTurnBudget}`) &&
      (budgetResult.message || "").includes(
        `wall_clock_budget_ms=${goalWallClockBudgetMs}`,
      ),
    `${JSON.stringify(goalEvent.goal)} ${budgetResult.message || ""}`,
  );
  await expectAction(run, sessionId, "goal-pause");
  await expectAction(run, sessionId, "goal-resume");
  const goalGet = await expectAction(run, sessionId, "goal-get");
  check(
    "native-goal-get",
    goalGet.message?.includes(goalObjective) &&
      goalGet.message?.includes(`token_budget=${goalTokenBudget}`) &&
      goalGet.message?.includes(`turn_budget=${goalTurnBudget}`) &&
      goalGet.message?.includes(
        `wall_clock_budget_ms=${goalWallClockBudgetMs}`,
      ),
    goalGet.message || "",
  );
  const goalCompleteMark = run.mark();
  const completed = await expectAction(run, sessionId, "goal-complete", {
    reason: "Kimi external-agent goal lifecycle acceptance passed",
  });
  const completedGoal = await run.waitFor(
    "native goal completion event",
    (event) =>
      event.event === "session_goal" &&
      event.goal?.objective === goalObjective &&
      /complete/i.test(event.goal?.status || ""),
    60_000,
    goalCompleteMark,
  );
  check(
    "native-goal-complete",
    /complete/i.test(completed.message || "") &&
      /complete/i.test(completedGoal.goal?.status || ""),
    completed.message || JSON.stringify(completedGoal.goal),
  );
  const clearedGoal = await expectAction(run, sessionId, "goal-get");
  check(
    "completed-goal-is-no-longer-active",
    /no active goal/i.test(clearedGoal.message || ""),
    clearedGoal.message || "",
  );
  await expectAction(run, sessionId, "goal-set", {
    objective: "Throwaway goal-clear acceptance",
  });
  await expectAction(run, sessionId, "goal-clear");
  const explicitlyClearedGoal = await expectAction(run, sessionId, "goal-get");
  check(
    "native-goal-clear",
    /no active goal/i.test(explicitlyClearedGoal.message || ""),
    explicitlyClearedGoal.message || "",
  );

  // A native head fork is attached as its own managed Kimi wrapper, preserving
  // the exact live profile and tool set before accepting independent work.
  const parentProfileSessions = await pollJson(
    port,
    "/api/sessions",
    (body) => {
      const row = sessionRowForId(body, sessionId);
      return Boolean(
        row?.kimi_model &&
          row?.kimi_thinking &&
          row?.kimi_permission_mode &&
          Array.isArray(row?.kimi_allowed_tools),
      );
    },
    60_000,
  );
  const parentProfileRow = sessionRowForId(parentProfileSessions, sessionId);
  const parentConfiguredTools = [
    ...(parentProfileRow?.kimi_allowed_tools || []),
  ]
    .filter((value) => typeof value === "string")
    .sort();
  check(
    "effective-thinking-is-backend-echoed",
    ["high", "on"].includes(parentProfileRow?.kimi_thinking),
    JSON.stringify(parentProfileRow).slice(0, 1_200),
  );
  const headForkMark = run.mark();
  const headFork = await expectAction(run, sessionId, "fork", {
    name: "Kimi head E2E fork",
  });
  const headForkId = (headFork.message || "").match(
    /thread\s+(session_[A-Za-z0-9_-]+)/,
  )?.[1];
  check(
    "native-head-fork-returns-child",
    Boolean(headForkId && headForkId !== sessionId),
    headFork.message || "",
  );
  if (!headForkId || headForkId === sessionId) {
    throw new Error(`invalid Kimi head fork id: ${headForkId || "missing"}`);
  }
  const headForkRelationship = await run.waitFor(
    "head fork relationship",
    (event) =>
      event.event === "session_relationship" &&
      event.relationship === "fork" &&
      event.child_session_id === headForkId,
    120_000,
    headForkMark,
  );
  check(
    "head-fork-parent-lineage",
    [wrapperId, sessionId].includes(headForkRelationship.parent_session_id) &&
      headForkRelationship.ephemeral === false,
    JSON.stringify(headForkRelationship),
  );
  const headForkIdentity = await run.waitFor(
    "head fork Kimi identity",
    (event) =>
      event.event === "session_identity" &&
      event.source === "kimi" &&
      event.backend_session_id === headForkId,
    120_000,
    headForkMark,
  );
  check(
    "head-fork-never-misidentified-as-codex",
    !run.events
      .slice(headForkMark)
      .some(
        (event) =>
          event.event === "session_identity" &&
          event.source === "codex" &&
          event.backend_session_id === headForkId,
      ),
  );
  const headForkSessions = await pollJson(
    port,
    "/api/sessions",
    (body) => {
      const row = sessionRowForId(body, headForkId);
      const tools = [...(row?.kimi_allowed_tools || [])]
        .filter((value) => typeof value === "string")
        .sort();
      return (
        row?.kimi_model === parentProfileRow?.kimi_model &&
        row?.kimi_thinking === parentProfileRow?.kimi_thinking &&
        row?.kimi_permission_mode === parentProfileRow?.kimi_permission_mode &&
        row?.kimi_plan_mode === parentProfileRow?.kimi_plan_mode &&
        row?.kimi_swarm_mode === parentProfileRow?.kimi_swarm_mode &&
        sameStrings(tools, parentConfiguredTools)
      );
    },
    60_000,
    (body) => JSON.stringify(kimiProfileRowsForId(body, headForkId)),
  );
  const headForkRow = sessionRowForId(headForkSessions, headForkId);
  const headForkConfiguredTools = [...(headForkRow?.kimi_allowed_tools || [])]
    .filter((value) => typeof value === "string")
    .sort();
  check(
    "head-fork-persists-exact-live-profile",
    headForkRow?.kimi_model === parentProfileRow?.kimi_model &&
      headForkRow?.kimi_thinking === parentProfileRow?.kimi_thinking &&
      headForkRow?.kimi_permission_mode ===
        parentProfileRow?.kimi_permission_mode &&
      headForkRow?.kimi_plan_mode === parentProfileRow?.kimi_plan_mode &&
      headForkRow?.kimi_swarm_mode === parentProfileRow?.kimi_swarm_mode &&
      sameStrings(headForkConfiguredTools, parentConfiguredTools) &&
      sameStrings(headForkConfiguredTools, restoredTools),
    JSON.stringify(headForkRow).slice(0, 1_200),
  );
  const headForkTools = await expectAction(run, headForkId, "tools");
  check(
    "head-fork-restores-exact-live-tools",
    sameStrings(activeToolNames(headForkTools.message), restoredTools),
    (headForkTools.message || "").slice(0, 800),
  );
  const headForkFollowUpMark = run.mark();
  run.send({
    action: "follow_up",
    session_id: headForkId,
    direct: true,
    text:
      "Without inspecting files, reply with exactly " +
      `HEAD_FORK_RECALL=${ATTACHMENT_TOKEN}.`,
  });
  const headForkReply = await run.waitFor(
    "head fork follow-up",
    (event) =>
      event.event === "model_response" &&
      eventTargets(
        event,
        headForkIdentity.session_id,
        headForkIdentity.backend_session_id,
      ) &&
      responseText(event).includes(`HEAD_FORK_RECALL=${ATTACHMENT_TOKEN}`),
    180_000,
    headForkFollowUpMark,
  );
  check(
    "head-fork-accepts-independent-follow-up",
    Boolean(headForkReply),
    responseText(headForkReply).slice(0, 400),
  );
  const headForkStopMark = run.mark();
  run.send({
    action: "stop_session",
    session_id: headForkIdentity.session_id,
  });
  await run.waitFor(
    "head fork wrapper stopped",
    (event) =>
      event.event === "session_ended" &&
      eventTargets(
        event,
        headForkIdentity.session_id,
        headForkIdentity.backend_session_id,
      ),
    60_000,
    headForkStopMark,
  );

  // Compact in place, then force one native todo plan event and recall evidence
  // from before compaction.
  await expectAction(run, sessionId, "compact", {}, 180_000);
  const compactMark = run.mark();
  const todoTool = restoredTools.includes("TodoList") ? "TodoList" : "TodoWrite";
  run.send({
    action: "follow_up",
    session_id: sessionId,
    direct: true,
    text:
      `Call ${todoTool} exactly once with two items: "verify compact recall" ` +
      '(in_progress) and "finish E2E" (pending). Then reply with exactly ' +
      `COMPACT_RECALL=${ATTACHMENT_TOKEN}.`,
  });
  const compactRecall = await run.waitFor(
    "post-compact recall",
    (event) =>
      event.event === "model_response" &&
      responseText(event).includes(ATTACHMENT_TOKEN),
    180_000,
    compactMark,
  );
  check(
    "compact-retains-context",
    Boolean(compactRecall),
    responseText(compactRecall),
  );
  check(
    "plan-update-surfaced",
    run.events
      .slice(compactMark)
      .some(
        (event) =>
          event.event === "model_response" &&
          /\*\*Plan\*\*|verify compact recall|finish E2E/i.test(
            responseText(event),
          ),
      ),
  );

  if (QUICK) {
    skip("mid-turn-steer-and-interrupt", "--quick selected");
  } else {
    // True Kimi steering: wait until the long tool is observed, then inject a
    // Write request into that same running turn.
    const steerPath = path.join(WORKDIR, "steered.txt");
    const steerMark = run.mark();
    run.send({
      action: "follow_up",
      session_id: sessionId,
      direct: true,
      text:
        "Use Bash exactly once to run `for i in $(seq 1 20); do sleep 1; done; " +
        "echo waited`. Then reply exactly WAITED.",
    });
    await run.waitFor(
      "long steering tool start",
      (event) =>
        event.event === "agent_started" &&
        /sleep 1|seq 1 20|waited/.test(event.commands_preview || ""),
      120_000,
      steerMark,
    );
    const activeSteerMark = run.mark();
    const steerSentAt = Date.now();
    run.send({
      action: "steer",
      session_id: sessionId,
      id: `steer-${randomToken()}`,
      text:
        "Before ending this same turn, use Write to create steered.txt " +
        "containing exactly STEERED_IN_TURN.",
      attachments: [],
    });
    await run.waitFor(
      "steered parent turn end",
      (event) =>
        isTurnEnd(event) && eventTargets(event, wrapperId, sessionId),
      180_000,
      activeSteerMark,
    );
    check(
      "native-mid-turn-steer",
      fs.existsSync(steerPath) &&
        fs.readFileSync(steerPath, "utf8").includes("STEERED_IN_TURN"),
      `elapsed=${((Date.now() - steerSentAt) / 1000).toFixed(1)}s`,
    );
    check(
      "steer-delivery-ack",
      run.events
        .slice(activeSteerMark)
        .some(
          (event) =>
            event.event === "steer_delivered" && event.mid_turn === true,
        ),
    );

    // Interrupt a much longer tool and prove the server process survives.
    const interruptMark = run.mark();
    run.send({
      action: "follow_up",
      session_id: sessionId,
      direct: true,
      text:
        "Use Bash exactly once to run `for i in $(seq 1 90); do sleep 1; done; " +
        "echo ninety`. Then reply LONG_DONE.",
    });
    await run.waitFor(
      "interruptible tool start",
      (event) =>
        event.event === "agent_started" &&
        /sleep 1|seq 1 90|ninety/.test(event.commands_preview || ""),
      120_000,
      interruptMark,
    );
    const interruptAt = Date.now();
    run.send({ action: "interrupt", session_id: sessionId });
    await run.waitFor(
      "interrupt completion",
      (event) =>
        event.event === "interrupted" ||
        (isTurnEnd(event) && eventTargets(event, wrapperId, sessionId)),
      60_000,
      interruptMark,
    );
    const interruptSeconds = (Date.now() - interruptAt) / 1000;
    check(
      "native-interrupt-aborts-turn",
      interruptSeconds < 35,
      `${interruptSeconds.toFixed(1)}s`,
    );
    const aliveMark = run.mark();
    run.send({
      action: "follow_up",
      session_id: sessionId,
      direct: true,
      text: "Reply with exactly KIMI_STILL_ALIVE.",
    });
    await run.waitFor(
      "post-interrupt response",
      (event) =>
        event.event === "model_response" &&
        responseText(event).includes("KIMI_STILL_ALIVE"),
      120_000,
      aliveMark,
    );
    check("server-survives-interrupt", true);
  }

  // Native :btw side conversation, scoped to its own attachable child.
  const sideMark = run.mark();
  const sideResult = await expectAction(run, sessionId, "side", {
    prompt: "Reply with exactly SIDE_KIMI_OK. Do not use tools.",
  });
  const sideThread = (sideResult.message || "").match(
    /thread\s+(session_[^\s]+:[^\s]+)\s+from parent/,
  )?.[1];
  check(
    "btw-returns-composite-child",
    Boolean(sideThread),
    sideResult.message || "",
  );
  const sideRelationship = await run.waitFor(
    "side relationship",
    (event) =>
      event.event === "session_relationship" &&
      event.relationship === "side" &&
      (!sideThread || event.child_session_id === sideThread),
    120_000,
    sideMark,
  );
  const effectiveSideThread = sideThread || sideRelationship.child_session_id;
  const sideIdentity = await run.waitFor(
    "side Kimi identity",
    (event) =>
      event.event === "session_identity" &&
      event.source === "kimi" &&
      event.session_id === effectiveSideThread &&
      event.backend_session_id === effectiveSideThread,
    120_000,
    sideMark,
  );
  const sideCapabilities = await run.waitFor(
    "side Kimi capabilities",
    (event) =>
      event.event === "session_capabilities" &&
      event.session_id === effectiveSideThread &&
      event.capabilities,
    120_000,
    sideMark,
  );
  const sideThreadActions =
    sideCapabilities.capabilities.thread_actions || [];
  check(
    "btw-advertises-only-child-safe-actions",
    sameStrings(sideThreadActions, [
      "context-clear",
      "tools",
      "tools-set",
      "tools-all",
      "side-close",
    ]) &&
      !sideThreadActions.some((op) =>
        ["fork", "goal-set", "review", "tasks", "undo"].includes(op),
      ),
    JSON.stringify(sideThreadActions),
  );
  const sideReply = await run.waitFor(
    "side reply",
    (event) =>
      event.event === "model_response" &&
      event.session_id === effectiveSideThread &&
      responseText(event).includes("SIDE_KIMI_OK"),
    180_000,
    sideMark,
  );
  await run.waitFor(
    "side turn end",
    (event) =>
      isTurnEnd(event) && eventTargets(event, sideIdentity.session_id),
    180_000,
    sideMark,
  );
  check(
    "btw-scoped-activity",
    [wrapperId, sessionId].includes(sideRelationship.parent_session_id) &&
      sideRelationship.ephemeral === true &&
    Boolean(sideReply),
    `child=${effectiveSideThread}`,
  );
  const sideTools = await expectAction(run, effectiveSideThread, "tools");
  const sideRegisteredTools = registeredToolNames(sideTools.message);
  check(
    "btw-agent-tool-inventory",
    sideRegisteredTools.length > 2,
    (sideTools.message || "").slice(0, 600),
  );
  await expectAction(run, effectiveSideThread, "tools-set", { names: [] });
  const emptySideTools = await expectAction(
    run,
    effectiveSideThread,
    "tools",
  );
  check(
    "btw-agent-exact-empty-tools",
    activeToolNames(emptySideTools.message).length === 0,
    emptySideTools.message || "",
  );
  await expectAction(run, effectiveSideThread, "tools-all");
  const restoredSideTools = await expectAction(
    run,
    effectiveSideThread,
    "tools",
  );
  check(
    "btw-agent-restores-all-tools",
    sameStrings(activeToolNames(restoredSideTools.message), sideRegisteredTools),
    (restoredSideTools.message || "").slice(0, 600),
  );
  const sideContextClear = await expectAction(
    run,
    effectiveSideThread,
    "context-clear",
  );
  check(
    "btw-agent-context-clear-is-agent-scoped",
    sideContextClear.message?.includes(effectiveSideThread.split(":").at(-1)),
    sideContextClear.message || "",
  );
  const sideCloseMark = run.mark();
  await expectAction(run, effectiveSideThread, "side-close");
  await run.waitFor(
    "side session ended",
    (event) =>
      event.event === "session_ended" &&
      event.session_id === effectiveSideThread,
    60_000,
    sideCloseMark,
  );
  const postSideMark = run.mark();
  run.send({
    action: "follow_up",
    session_id: sessionId,
    direct: true,
    text: "Reply with exactly KIMI_PARENT_AFTER_SIDE_CLOSE.",
  });
  await run.waitFor(
    "parent follow-up after side close",
    (event) =>
      event.event === "model_response" &&
      eventTargets(event, wrapperId, sessionId) &&
      responseText(event).includes("KIMI_PARENT_AFTER_SIDE_CLOSE"),
    120_000,
    postSideMark,
  );
  check("side-close-restores-parent-target", true);

  if (QUICK) {
    skip("background-task-output-cancel", "--quick selected");
  } else {
    await exerciseBackgroundTask(run, port, wrapperId, sessionId);
  }

  // Native session lifecycle controls.
  await expectAction(run, sessionId, "rename", {
    title: "Kimi exhaustive E2E",
  });
  await expectAction(run, sessionId, "archive");
  await expectAction(run, sessionId, "restore");

  // Catalog, detail replay, deep search, and message-index search all read
  // Kimi's isolated native session store rather than the wrapper mirror.
  const sessions = await pollJson(
    port,
    "/api/sessions",
    (body) =>
      JSON.stringify(body).includes(sessionId) &&
      /kimi/i.test(JSON.stringify(body)),
    45_000,
  );
  check(
    "session-catalog-lists-kimi",
    JSON.stringify(sessions).includes(sessionId),
  );
  const detail = await httpJson(
    port,
    `/api/session/${encodeURIComponent(sessionId)}?source=kimi&limit=200`,
  );
  const detailText = JSON.stringify(detail);
  check(
    "session-detail-replays-kimi",
    detailText.includes(KEEP_CODEWORD) && detailText.includes(ATTACHMENT_TOKEN),
    `${detailText.length} bytes`,
  );
  const deep = await httpJson(
    port,
    `/api/sessions/search?q=${encodeURIComponent(KEEP_CODEWORD)}&source=kimi`,
  );
  check(
    "deep-search-finds-kimi",
    JSON.stringify(deep).includes(sessionId),
    JSON.stringify(deep).slice(0, 500),
  );
  const messageSearch = await pollJson(
    port,
    `/api/sessions/message-search?q=${encodeURIComponent(KEEP_CODEWORD)}&source=kimi&limit=20`,
    (body) =>
      body.state !== "building" && JSON.stringify(body).includes(sessionId),
    75_000,
  );
  check(
    "message-search-finds-kimi",
    JSON.stringify(messageSearch).includes(KEEP_CODEWORD),
    `state=${messageSearch.state}`,
  );

  // Stop then explicitly resume the same native session through the daemon
  // funnel. This is stronger than merely checking that the history is listed:
  // a fresh Kimi server must re-adopt the id and answer from its context.
  const resumeMark = run.mark();
  run.send({ action: "stop_session", session_id: wrapperId });
  await run.waitFor(
    "parent wrapper stopped",
    (event) =>
      event.event === "session_ended" &&
      eventTargets(event, wrapperId, sessionId),
    60_000,
    resumeMark,
  );
  run.send({
    action: "resume_session",
    source: "kimi",
    session_id: sessionId,
    resume_id: sessionId,
    project_root: WORKDIR,
    task:
      "Without inspecting files, reply with exactly " +
      `RESUME_RECALL=${ATTACHMENT_TOKEN}.`,
    direct: true,
    attachments: [],
    fork: false,
  });
  const resumedIdentity = await run.waitFor(
    "resumed Kimi identity",
    (event) =>
      event.event === "session_identity" &&
      event.source === "kimi" &&
      event.backend_session_id === sessionId,
    120_000,
    resumeMark,
  );
  const resumed = await run.waitFor(
    "resumed Kimi context response",
    (event) =>
      event.event === "model_response" &&
      eventTargets(event, resumedIdentity.session_id, sessionId) &&
      responseText(event).includes(ATTACHMENT_TOKEN),
    180_000,
    resumeMark,
  );
  check(
    "resume-rebinds-native-session",
    resumedIdentity.backend_session_id === sessionId,
    `wrapper=${resumedIdentity.session_id} native=${resumedIdentity.backend_session_id}`,
  );
  check("resume-retains-context", Boolean(resumed), responseText(resumed));

  // clearContext is deliberately destructive and has no Codex memory-reset
  // semantics. Exercise it only after every recall/search/resume assertion on
  // this disposable session. Disable tools for the verification turn so Kimi
  // cannot recover the token from probe.txt.
  await expectAction(run, sessionId, "context-clear");
  await expectAction(run, sessionId, "tools-set", { names: [] });
  const contextClearMark = run.mark();
  run.send({
    action: "follow_up",
    session_id: sessionId,
    direct: true,
    text:
      "Without using tools: if you cannot recall any attachment token from " +
      "before this prompt, reply with exactly CONTEXT_CLEARED. Otherwise reply " +
      "CONTEXT_RETAINED=<the token you remember>.",
  });
  const clearedContextResponse = await run.waitFor(
    "post-context-clear response",
    (event) =>
      event.event === "model_response" &&
      eventTargets(event, resumedIdentity.session_id, sessionId) &&
      /CONTEXT_(?:CLEARED|RETAINED)/.test(responseText(event)),
    180_000,
    contextClearMark,
  );
  await run.waitFor(
    "post-context-clear turn end",
    (event) =>
      isTurnEnd(event) &&
      eventTargets(event, resumedIdentity.session_id, sessionId),
    180_000,
    contextClearMark,
  );
  check(
    "native-context-clear-removes-conversation",
    responseText(clearedContextResponse).trim() === "CONTEXT_CLEARED" &&
      !responseText(clearedContextResponse).includes(ATTACHMENT_TOKEN) &&
      !responseText(clearedContextResponse).includes(KEEP_CODEWORD),
    responseText(clearedContextResponse).slice(0, 400),
  );

  const availability = await httpJson(port, "/api/external-agents");
  const kimiAvailability = availability.external_agents?.find(
    (entry) => entry.id === "kimi",
  );
  const compatibility = kimiAvailability?.compatibility;
  check(
    "passive-protocol-watch-is-clean",
    compatibility?.state === "no_drift_observed" &&
      compatibility?.reported_version === kimiVersion &&
      compatibility?.finding_counts?.warning === 0 &&
      compatibility?.finding_counts?.error === 0,
    JSON.stringify(compatibility || {}).slice(0, 600),
  );

  return { sessionId, wrapperId, resumedWrapperId: resumedIdentity.session_id };
}

async function main() {
  if (AUTH_SYNC_SELF_TEST) {
    runAuthSyncSelfTest();
    return;
  }
  log("setup", `Intendant: ${BINARY}`);
  log("setup", `Kimi: ${KIMI_COMMAND}`);
  log("setup", `model: ${MODEL} (${MODEL_DISPLAY})`);
  log("setup", `root: ${ROOT}`);
  log("setup", `project: ${WORKDIR}`);
  if (!fs.existsSync(BINARY)) {
    throw new Error(`Intendant binary not found: ${BINARY}`);
  }
  if (!fs.existsSync(KIMI_COMMAND)) {
    throw new Error(`Kimi binary not found: ${KIMI_COMMAND}`);
  }
  const version = execFileSync(KIMI_COMMAND, ["--version"], {
    encoding: "utf8",
  }).trim();
  check("kimi-installed", SUPPORTED_KIMI_VERSION.test(version), version);
  copyKimiAuthState();
  setupProject();
  await exerciseVaultSigninCeremonyOnIdleDaemon();

  const port = REQUESTED_PORT || (await freePort());
  const run = new IntendantRun(port);
  try {
    await scenario(run, port, version);
  } finally {
    try {
      await run.stop();
    } finally {
      syncKimiAuthState();
    }
  }
}

async function finish() {
  let scenarioError = null;
  try {
    await main();
  } catch (error) {
    scenarioError = error;
    log("ERROR", error.stack || String(error));
    check("scenario-completed", false, error.message || String(error));
  }

  const failed = checks.filter((item) => !item.ok);
  const report = [
    "",
    "===== Kimi Code E2E summary =====",
    ...checks.map(
      (item) =>
        ` ${item.ok ? "✅" : "❌"} ${item.name}` +
        `${item.detail ? ` — ${item.detail}` : ""}`,
    ),
    ...skips.map((item) => ` ⏭️  ${item.name} — ${item.detail}`),
    `${checks.length - failed.length}/${checks.length} checks passed; ` +
      `${skips.length} explicitly skipped`,
    `model: ${MODEL} (${MODEL_DISPLAY})`,
    `root: ${ROOT}${KEEP ? " (kept)" : ""}`,
  ];
  console.log(report.join("\n"));

  try {
    fs.writeFileSync(
      path.join(ROOT, "e2e.log"),
      `${logLines.join("\n")}\n${report.join("\n")}\n`,
    );
  } catch {
    // The setup may have failed before ROOT was usable.
  }
  if (!KEEP) {
    try {
      fs.rmSync(ROOT, { recursive: true, force: true });
    } catch (error) {
      console.error(`cleanup warning: ${error.message}`);
    }
  }
  process.exitCode = scenarioError || failed.length ? 1 : 0;
}

finish();

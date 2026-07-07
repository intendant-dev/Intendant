//! Headless end-to-end tests: spawn the real `intendant` binary and drive
//! the production stack — CLI parsing, provider selection, the agent loop,
//! tool dispatch, the sandboxed `intendant-runtime` subprocess, and session
//! logging — with no API keys, no network beyond loopback, and no display.
//!
//! The model is the scripted mock provider (`PROVIDER=mock` +
//! `INTENDANT_MOCK_SCRIPT`, see `src/bin/caller/provider_mock.rs`): each
//! test writes a JSON script of responses/tool calls, runs the binary in an
//! isolated HOME + project dir, and asserts on the exit status, the session
//! log, and on-disk effects. Scripts fail loudly (unmet expectation or
//! exhausted steps error out), so a hung or drifted loop fails the test
//! instead of green-looping.
//!
//! Besides one-shot runs, the suite can host persistent `--web` daemons on
//! ephemeral loopback ports (see [`DaemonRig`]) and federate them over
//! `POST /api/peers` — still keyless and with no network beyond loopback.
//!
//! Run: cargo test --test e2e -- --nocapture

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Generous ceiling for one binary run: a debug build on a cold CI runner
/// spawns the runtime several times but each chat turn is local.
const RUN_TIMEOUT: Duration = Duration::from_secs(180);

/// Ceiling for a `--web` daemon to bind its port and serve the agent card
/// on a cold CI runner.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(60);

/// Deadline for the load-sensitive cross-daemon waits in the federated peer
/// test (the A->B connection forming, the delegated task completing on B).
/// A 90s ceiling was blown twice by a healthy tree on a loaded CI box —
/// debug binaries, several daemons, and concurrent jobs stack up — and the
/// suite's wall-clock cost is bounded by how fast the waits *succeed*, not
/// by this deadline, so be generous.
const PEER_WAIT_TIMEOUT: Duration = Duration::from_secs(240);

fn intendant_bin() -> &'static str {
    // Referencing the runtime binary's env var makes Cargo build it too —
    // the caller resolves `intendant-runtime` as its sibling on disk.
    let _runtime = env!("CARGO_BIN_EXE_intendant-runtime");
    env!("CARGO_BIN_EXE_intendant")
}

struct TestRig {
    home: tempfile::TempDir,
    project: tempfile::TempDir,
}

impl TestRig {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("temp home"),
            project: tempfile::tempdir().expect("temp project"),
        }
    }

    fn write_script(&self, script: &serde_json::Value) -> PathBuf {
        let path = self.home.path().join("mock_script.json");
        std::fs::write(&path, serde_json::to_vec_pretty(script).unwrap()).expect("write script");
        path
    }

    fn command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(intendant_bin());
        cmd.current_dir(self.project.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // Isolate from the host: fresh HOME (session logs, config,
            // credential leases) and no real provider keys.
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GEMINI_API_KEY")
            .env_remove("MODEL_NAME")
            .env_remove("PRESENCE_PROVIDER")
            .env_remove("PRESENCE_MODEL")
            .env_remove("CU_PROVIDER")
            .env_remove("CU_MODEL")
            // A host shell exporting the Connect rendezvous URL would make
            // the daemon-lane tests dial out; the suite is no-network.
            .env_remove("INTENDANT_CONNECT_RENDEZVOUS_URL")
            // The suite is display-free by contract, but a host with a live
            // X session at :0 (a self-hosted runner doubling as a desktop)
            // breaks that hermeticity: the caller's vision probe finds the
            // socket, exports DISPLAY=:0, and the runtime's fail-closed
            // user-display gate then refuses every exec_command without a
            // grant. Pin the virtual-display convention instead — nothing
            // here opens an X connection, and display ids > 0 need no grant.
            .env("DISPLAY", ":99")
            .env("PROVIDER", "mock");
        cmd
    }

    /// Run to completion with a hard timeout; on expiry the child is
    /// killed (kill_on_drop) and the test fails with captured output.
    async fn run(&self, mut cmd: tokio::process::Command) -> std::process::Output {
        let child = cmd.spawn().expect("spawn intendant");
        match tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output()).await {
            Ok(output) => output.expect("collect output"),
            Err(_) => panic!("intendant did not exit within {RUN_TIMEOUT:?}"),
        }
    }

    /// Concatenated session.jsonl contents from every session dir the run
    /// produced under the isolated home.
    fn session_logs(&self) -> String {
        let logs_dir = self.home.path().join(".intendant").join("logs");
        let mut combined = String::new();
        if let Ok(entries) = std::fs::read_dir(&logs_dir) {
            for entry in entries.flatten() {
                if let Ok(contents) = std::fs::read_to_string(entry.path().join("session.jsonl")) {
                    combined.push_str(&contents);
                    combined.push('\n');
                }
            }
        }
        combined
    }

    /// Concatenated per-turn artifacts (`turns/*.txt`, `turns/*.json`) —
    /// large payloads like runtime stdout are offloaded there rather than
    /// inlined in session.jsonl.
    fn turn_artifacts(&self) -> String {
        let logs_dir = self.home.path().join(".intendant").join("logs");
        let mut combined = String::new();
        let Ok(sessions) = std::fs::read_dir(&logs_dir) else {
            return combined;
        };
        for session in sessions.flatten() {
            let Ok(turns) = std::fs::read_dir(session.path().join("turns")) else {
                continue;
            };
            for turn in turns.flatten() {
                if let Ok(contents) = std::fs::read_to_string(turn.path()) {
                    combined.push_str(&contents);
                    combined.push('\n');
                }
            }
        }
        combined
    }
}

fn text_of(output: &std::process::Output) -> String {
    format!(
        "status: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn marker_file(project: &Path, name: &str) -> PathBuf {
    project.join(name)
}

/// Last `max` bytes of `contents`, starting on a char boundary —
/// panic-message-sized views of daemon logs and session state.
fn tail(contents: &str, max: usize) -> String {
    let mut start = contents.len().saturating_sub(max);
    while !contents.is_char_boundary(start) {
        start += 1;
    }
    contents[start..].to_string()
}

/// Two distinct free loopback ports, grabbed while both listeners are
/// alive so the kernel cannot hand out the same port twice. The listeners
/// are dropped on return and the daemons re-bind moments later; if a port
/// is stolen in that window the daemon walks to the next free port and the
/// test fails loudly on its readiness poll — a flake, never a false pass.
fn two_free_loopback_ports() -> (u16, u16) {
    let first = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    let second = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    (
        first.local_addr().expect("ephemeral port addr").port(),
        second.local_addr().expect("ephemeral port addr").port(),
    )
}

/// A persistent-daemon variant of [`TestRig`]: the same isolated binary
/// invocation, but running as an idle `--web` daemon on a loopback port
/// instead of a one-shot task.
struct DaemonRig {
    /// Declared first so drop kills the daemon (kill_on_drop) before the
    /// rig's temp dirs are removed — fields drop in declaration order, and
    /// a still-running process holds open log handles on Windows.
    child: tokio::process::Child,
    rig: TestRig,
    port: u16,
}

impl DaemonRig {
    /// Tail of the daemon's combined stdout/stderr log, for failure context.
    fn log_tail(&self) -> String {
        let path = self.rig.home.path().join("daemon.log");
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        tail(&contents, 4000)
    }
}

/// Spawn an idle daemon (no task) with the given mock script and wait until
/// its gateway serves the agent card — the same flags and readiness probe as
/// the peer-sessions smoke rig (`tests/skills/peer-sessions-smoke`).
async fn spawn_daemon(
    client: &reqwest::Client,
    script: &serde_json::Value,
    port: u16,
) -> DaemonRig {
    let rig = TestRig::new();
    // These rigs model a *rooted* daemon: an idle --web daemon launched
    // from a markerless cwd runs projectless and then requires an explicit
    // per-session project root — but a peer-delegated task
    // (PeerOp::DelegateTask → ControlMsg::StartTask) carries none, so the
    // daemon's default project must exist for it to run. An empty
    // intendant.toml is the minimal project marker (parses to config
    // defaults; pinned by project.rs's empty-config unit test).
    std::fs::write(rig.project.path().join("intendant.toml"), "")
        .expect("mark the daemon rig's project root");
    let script_path = rig.write_script(script);
    let log = std::fs::File::create(rig.home.path().join("daemon.log")).expect("daemon log");
    let mut cmd = rig.command();
    cmd.env("INTENDANT_MOCK_SCRIPT", &script_path)
        // A daemon outlives the test body's reads; piped-but-unread stdio
        // would deadlock once the pipe buffer fills, so tee both streams
        // to a file instead.
        .stdout(log.try_clone().expect("clone daemon log"))
        .stderr(log);
    cmd.arg("--web")
        .arg(port.to_string())
        .args([
            "--bind",
            "127.0.0.1",
            "--no-tls",
            "--no-tui",
            "--autonomy",
            "full",
        ])
        .arg("--advertise-url")
        .arg(format!("ws://127.0.0.1:{port}/ws"));
    let child = cmd.spawn().expect("spawn intendant daemon");
    let mut daemon = DaemonRig { child, rig, port };

    let card_url = format!("http://127.0.0.1:{port}/.well-known/agent-card.json");
    let deadline = tokio::time::Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        if http_get_json(client, &card_url).await.is_some() {
            return daemon;
        }
        if let Ok(Some(status)) = daemon.child.try_wait() {
            panic!(
                "daemon on port {port} exited during startup ({status}):\n{}",
                daemon.log_tail()
            );
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon on port {port} did not serve its agent card within \
             {DAEMON_START_TIMEOUT:?}:\n{}",
            daemon.log_tail()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Run `intendant ctl --port <daemon port> <args…>` against a daemon,
/// isolated the same way as the daemon itself. `intendant ctl` honors
/// INTENDANT_MCP_URL / INTENDANT_PORT / INTENDANT_SESSION_ID /
/// INTENDANT_MANAGED_CONTEXT from the environment — a test run from inside a
/// supervised session inherits them and would target the wrong daemon — so
/// scrub them and let `--port` select exactly this daemon (tokenless
/// loopback /mcp binds the root-capable `local_process` default on a fresh,
/// grant-less HOME).
async fn ctl(daemon: &DaemonRig, args: &[&str]) -> std::process::Output {
    let mut cmd = daemon.rig.command();
    cmd.env_remove("INTENDANT_MCP_URL")
        .env_remove("INTENDANT_PORT")
        .env_remove("INTENDANT_SESSION_ID")
        .env_remove("INTENDANT_MANAGED_CONTEXT");
    cmd.arg("ctl")
        .arg("--port")
        .arg(daemon.port.to_string())
        .args(args);
    daemon.rig.run(cmd).await
}

/// Parse a ctl run's stdout as the single JSON document the command prints.
fn stdout_json(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "ctl did not print one JSON document ({e}):\n{}",
            text_of(output)
        )
    })
}

async fn http_get_json(client: &reqwest::Client, url: &str) -> Option<serde_json::Value> {
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// The first peer in `GET /api/peers` whose connection is up, if any.
async fn connected_peer(client: &reqwest::Client, port: u16) -> Option<serde_json::Value> {
    let peers = http_get_json(client, &format!("http://127.0.0.1:{port}/api/peers")).await?;
    peers
        .get("peers")?
        .as_array()?
        .iter()
        .find(|peer| {
            peer.pointer("/connection_state/state")
                .and_then(|v| v.as_str())
                == Some("connected")
        })
        .cloned()
}

/// Poll `probe` every 250 ms until it yields `Some`, panicking after
/// `timeout` — the suite's polling convention for daemon-shaped tests
/// (one-shot runs use [`TestRig::run`]'s hard timeout instead). On timeout
/// the panic carries `context()`: pass the relevant daemon log/session
/// tails so the failure says *why* it stalled, not just that it did
/// (`String::new` when there is nothing useful to dump).
async fn poll_until<T, Fut>(
    what: &str,
    timeout: Duration,
    mut probe: impl FnMut() -> Fut,
    context: impl Fn() -> String,
) -> T
where
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(found) = probe().await {
            return found;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out after {timeout:?} waiting for {what}:\n{}",
                context()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn direct_mode_completes_a_scripted_task_through_the_real_stack() {
    let rig = TestRig::new();
    // The marker appears nowhere in the task or scripted content — only the
    // runtime's tool result can introduce it, so step two's expectation
    // proves the exec round-tripped through the sandboxed runtime.
    let script = rig.write_script(&serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "Running the scripted command.",
                  "tool_calls": [{ "name": "exec_command",
                                   "arguments": { "nonce": 1, "command": "echo MOCK_E2E_ROUNDTRIP" } }] },
                { "expect_transcript_contains": "MOCK_E2E_ROUNDTRIP",
                  "content": "All work finished.",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "mock run complete" } }] }
            ]
        }]
    }));

    let mut cmd = rig.command();
    cmd.env("INTENDANT_MOCK_SCRIPT", &script).args([
        "--direct",
        "--no-web",
        "--no-tui",
        "--autonomy",
        "full",
        "run the scripted command",
    ]);
    let output = rig.run(cmd).await;
    assert!(output.status.success(), "{}", text_of(&output));

    let logs = rig.session_logs();
    // The exec dispatched to the runtime (runtime function name) and was
    // auto-approved under --autonomy full.
    assert!(
        logs.contains("execAsAgent"),
        "session log missing the runtime dispatch:\n{logs}"
    );
    assert!(
        logs.contains("auto_approved"),
        "session log missing the autonomy decision:\n{logs}"
    );
    // The done signal fires only after the mock's expectation saw the
    // runtime's output in the transcript — the round-trip proof.
    assert!(
        logs.contains("mock run complete"),
        "session log missing the done signal:\n{logs}"
    );
    // Direct evidence too: the offloaded runtime stdout artifact.
    let artifacts = rig.turn_artifacts();
    assert!(
        artifacts.contains("MOCK_E2E_ROUNDTRIP"),
        "turn artifacts missing the runtime stdout:\n{artifacts}"
    );
}

#[tokio::test]
async fn direct_mode_writes_files_through_the_runtime() {
    let rig = TestRig::new();
    let target = marker_file(rig.project.path(), "e2e_artifact.txt");
    let script = rig.write_script(&serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "Writing the artifact.",
                  "tool_calls": [{ "name": "edit_file",
                                   "arguments": { "nonce": 1,
                                                  "file_path": target.to_string_lossy(),
                                                  "operation": "write",
                                                  "content": "written by the mock e2e\n" } }] },
                { "content": "File written.",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "write complete" } }] }
            ]
        }]
    }));

    let mut cmd = rig.command();
    cmd.env("INTENDANT_MOCK_SCRIPT", &script).args([
        "--direct",
        "--no-web",
        "--no-tui",
        "--autonomy",
        "full",
        "write the artifact file",
    ]);
    let output = rig.run(cmd).await;
    assert!(output.status.success(), "{}", text_of(&output));

    let written = std::fs::read_to_string(&target)
        .unwrap_or_else(|e| panic!("artifact not written ({e}):\n{}", text_of(&output)));
    assert_eq!(written, "written by the mock e2e\n");
}

/// Append a child pipe's lines into a shared buffer as they arrive.
fn drain_pipe_into(
    pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    buf: std::sync::Arc<std::sync::Mutex<String>>,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::AsyncBufReadExt;
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(mut guard) = buf.lock() {
                guard.push_str(&line);
                guard.push('\n');
            }
        }
    })
}

/// Poll a shared output buffer until `needle` appears (returning the full
/// buffer) or the timeout elapses (panicking with the buffer for context).
async fn wait_for_output(
    buf: &std::sync::Arc<std::sync::Mutex<String>>,
    needle: &str,
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = buf.lock().map(|guard| guard.clone()).unwrap_or_default();
        if snapshot.contains(needle) {
            return snapshot;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for {needle:?} in daemon output:\n{snapshot}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Read WebSocket text frames until one satisfies `pred`; `None` on
/// timeout (callers decide whether that is fatal and what context to dump).
async fn next_matching_ws_event<S>(
    ws: &mut S,
    timeout: Duration,
    mut pred: impl FnMut(&serde_json::Value) -> bool,
) -> Option<serde_json::Value>
where
    S: futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::StreamExt;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .ok()?
            .expect("/ws stream ended unexpectedly")
            .expect("/ws read failed");
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                if pred(&json) {
                    return Some(json);
                }
            }
        }
    }
}

/// Drive a supervised session over `/ws` through its happy-path completion
/// contract: wait for the session's `round_complete`, then send an explicit
/// `stop_session`, then require a `session_ended` without an `error_kind`.
///
/// This encodes what "done" means for a supervised daemon session — after
/// its done signal the loop finishes the round and the session *parks*
/// awaiting follow-ups; `session_ended` fires only on an explicit stop or an
/// error. A scenario that asserts `session_ended` straight after completion
/// therefore hangs for the full harness timeout (a real past failure on
/// this suite) — use this helper instead. Scenario-specific completion
/// evidence (done-signal messages, runtime round-trip markers in the
/// session log) stays in the test; `context` supplies the daemon
/// stderr/log tail for panic messages when an event never arrives.
async fn complete_and_stop_session<S>(ws: &mut S, session_id: &str, context: impl Fn() -> String)
where
    S: futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + futures_util::Sink<
            tokio_tungstenite::tungstenite::Message,
            Error = tokio_tungstenite::tungstenite::Error,
        > + Unpin,
{
    use futures_util::SinkExt;

    // Task completion: the loop finishes its round (done signal) and the
    // session parks for follow-ups — by design there is no SessionEnded
    // here, so round_complete is the completion signal.
    next_matching_ws_event(ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("round_complete")
            && json.get("session_id").and_then(|v| v.as_str()) == Some(session_id)
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "session {session_id} never completed its round; daemon context:\n{}",
            context()
        )
    });

    // The parked session ends when explicitly stopped — the one place a
    // supervised session emits session_ended on the happy path.
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "stop_session",
            "session_id": session_id,
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send stop_session");
    let ended = next_matching_ws_event(ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("session_ended")
            && json.get("session_id").and_then(|v| v.as_str()) == Some(session_id)
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "stopped session {session_id} never ended; daemon context:\n{}",
            context()
        )
    });
    assert!(
        ended.get("error_kind").is_none_or(|v| v.is_null()),
        "user-stopped session must end without an error class, got {ended}"
    );
}

/// The daemon lane, projectless: booted from an empty (markerless) temp
/// cwd it must come up serving — no cwd baseline scan, `project_root:
/// null` on the gateway — run a CreateSession that carries an explicit
/// `project_root` to completion, and fail one without it with the
/// structured `no_project` error kind instead of adopting cwd.
///
/// "Completion" for a supervised session is `round_complete` plus the done
/// signal in its log: by design the session then parks awaiting follow-ups
/// (no SessionEnded on task completion); [`complete_and_stop_session`]
/// encodes that contract, ending with an explicit stop.
#[tokio::test]
async fn projectless_daemon_serves_and_requires_an_explicit_session_project() {
    use futures_util::SinkExt;

    let rig = TestRig::new();
    let script = rig.write_script(&serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "Working in the explicit project.",
                  "tool_calls": [{ "name": "exec_command",
                                   "arguments": { "nonce": 1, "command": "echo PROJECTLESS_E2E_ROUNDTRIP" } }] },
                { "expect_transcript_contains": "PROJECTLESS_E2E_ROUNDTRIP",
                  "content": "All work finished.",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "projectless mock run complete" } }] }
            ]
        }]
    }));
    // The session's project: a real directory, distinct from the daemon's
    // (empty, markerless) launch cwd, passed explicitly per session.
    let session_project = tempfile::tempdir().expect("session project dir");

    let mut cmd = rig.command();
    cmd.env("INTENDANT_MOCK_SCRIPT", &script).args([
        "--no-tls",
        "--autonomy",
        "full",
        "--web",
        "18921", // base only: the daemon scans forward if taken; the real
                 // port is parsed from the "Dashboard:" startup line.
    ]);
    let mut child = cmd.spawn().expect("spawn intendant daemon");
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stdout_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let _stderr_drain = drain_pipe_into(child.stderr.take().expect("stderr"), stderr_buf.clone());
    let _stdout_drain = drain_pipe_into(child.stdout.take().expect("stdout"), stdout_buf.clone());

    // (a) It comes up projectless and serves.
    let stderr_so_far = wait_for_output(&stderr_buf, "Dashboard: http://", RUN_TIMEOUT).await;
    assert!(
        stderr_so_far.contains("Projectless daemon:"),
        "missing the projectless startup line:\n{stderr_so_far}"
    );
    assert!(
        stderr_so_far.contains("rewind snapshots off: the daemon has no project"),
        "the projectless daemon still tried to watch a project root:\n{stderr_so_far}"
    );
    let port: u16 = stderr_so_far
        .lines()
        .find_map(|line| {
            let url = line.strip_prefix("Dashboard: http://")?;
            url.rsplit(':').next()?.trim().parse().ok()
        })
        .expect("parse the dashboard port from the startup line");

    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + RUN_TIMEOUT;
    let project_root_body: serde_json::Value = loop {
        match http.get(format!("{base}/api/project-root")).send().await {
            Ok(resp) if resp.status().is_success() => {
                break resp.json().await.expect("project-root JSON")
            }
            _ if tokio::time::Instant::now() >= deadline => {
                panic!("gateway never served /api/project-root");
            }
            _ => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    };
    assert!(
        project_root_body
            .get("project_root")
            .is_some_and(|v| v.is_null()),
        "projectless daemon must report project_root: null, got {project_root_body}"
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    // (c) CreateSession WITHOUT a project root: the structured failure,
    // not a dead session and not a cwd-rooted one. The very first control
    // message can race daemon startup on a saturated box — the gateway
    // task accepts /ws a beat before the supervisor's bus subscription —
    // so retry the send until the structured failure arrives.
    let mut ended = None;
    for _ in 0..6 {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "action": "create_session",
                "task": "must not start without a project",
                "direct": true,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send projectless create_session");
        ended = next_matching_ws_event(&mut ws, Duration::from_secs(30), |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("session_ended")
        })
        .await;
        if ended.is_some() {
            break;
        }
    }
    let ended = ended.unwrap_or_else(|| {
        panic!(
            "no session_ended for the projectless create; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });
    assert_eq!(
        ended.get("error_kind").and_then(|v| v.as_str()),
        Some("no_project"),
        "expected the structured no_project failure, got {ended}"
    );
    assert!(
        ended
            .get("reason")
            .and_then(|v| v.as_str())
            .is_some_and(|reason| reason.contains("no project selected")),
        "no_project reason should tell the user to pick a project, got {ended}"
    );

    // (b) CreateSession WITH an explicit project root runs the scripted
    // task through the real stack to completion.
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "create_session",
            "task": "run the scripted command in the explicit project",
            "project_root": session_project.path().to_string_lossy(),
            "direct": true,
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send rooted create_session");
    let started = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("session_started")
            && json
                .get("task")
                .and_then(|v| v.as_str())
                .is_some_and(|task| task.contains("explicit project"))
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "rooted create never started; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });
    let started_id = started
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("session_started carries a session id")
        .to_string();
    // Completion + shutdown ride the suite's supervised-session contract:
    // round_complete, an explicit stop, then a clean session_ended.
    complete_and_stop_session(&mut ws, &started_id, || {
        stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
    })
    .await;
    let logs = rig.session_logs();
    assert!(
        logs.contains("projectless mock run complete"),
        "session log missing the done signal:\n{logs}"
    );
    assert!(
        logs.contains("PROJECTLESS_E2E_ROUNDTRIP")
            || rig.turn_artifacts().contains("PROJECTLESS_E2E_ROUNDTRIP"),
        "missing the runtime round-trip evidence"
    );

    child.kill().await.expect("stop the daemon");
}

#[tokio::test]
async fn mock_provider_without_a_script_fails_closed() {
    let rig = TestRig::new();
    let mut cmd = rig.command();
    // PROVIDER=mock but no INTENDANT_MOCK_SCRIPT: the run must fail with a
    // configuration error, not fall through to a real provider.
    cmd.args([
        "--direct",
        "--no-web",
        "--no-tui",
        "--autonomy",
        "full",
        "should never run",
    ]);
    let output = rig.run(cmd).await;
    assert!(!output.status.success(), "{}", text_of(&output));
    // The configuration error surfaces on stderr on some platforms and in
    // the session log on others (the loop's error sink) — accept either,
    // but it must name the missing variable.
    let evidence = format!("{}\n{}", text_of(&output), rig.session_logs());
    assert!(
        evidence.contains("INTENDANT_MOCK_SCRIPT"),
        "expected the mock-script configuration error, got:\n{evidence}"
    );
}

/// `intendant ctl peer …` against a live federated pair: daemon A federates
/// to daemon B over `POST /api/peers` (the peer-sessions smoke's pattern),
/// then the ctl surface — which reaches A's `/mcp` over tokenless loopback —
/// must (1) print `{"peers":[]}` before any peer exists, (2) list B with its
/// snapshot id/label and a connected connection_state, and (3) delegate a
/// task to B via `peer task`, printing a task_id while B's supervisor starts
/// a child session that really executes the instructions. B-side execution
/// is proven the same way the smoke rig proves it: the instructions carry a
/// marker that selects B's scripted mock profile, and that profile's done
/// signal — gated on the runtime echo reaching the transcript — lands in
/// B's session log (any other session would consume the fallback profile
/// and log "unexpected session" instead).
#[tokio::test]
async fn ctl_peer_list_and_task_drive_a_federated_peer_daemon() {
    const TASK_MARK: &str = "PEER_E2E_DELEGATED_TASK";
    const DONE_MESSAGE: &str = "peer delegated task complete";

    // Daemon A idles; ctl drives it. The fallback profile only exists so an
    // unexpected session fails the test legibly instead of hanging the mock.
    let idle_script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]
        }]
    });
    let peer_script = serde_json::json!({
        "profiles": [
            { "match": TASK_MARK, "steps": [
                { "content": "Running the delegated command.",
                  "tool_calls": [{ "name": "exec_command",
                                   "arguments": { "nonce": 1, "command": "echo PEER_E2E_ROUNDTRIP" } }] },
                { "expect_transcript_contains": "PEER_E2E_ROUNDTRIP",
                  "content": "Delegated work finished.",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": DONE_MESSAGE } }] }
            ]},
            { "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]}
        ]
    });

    // Everything here is loopback; ignore any ambient HTTP(S)_PROXY.
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    let (port_a, port_b) = two_free_loopback_ports();
    let mut a = spawn_daemon(&client, &idle_script, port_a).await;
    let mut b = spawn_daemon(&client, &peer_script, port_b).await;

    // Zero peers: a fresh daemon lists an empty registry.
    let empty = ctl(&a, &["peer", "list"]).await;
    assert!(empty.status.success(), "{}", text_of(&empty));
    let parsed = stdout_json(&empty);
    assert_eq!(
        parsed.get("peers").and_then(|v| v.as_array()).map(Vec::len),
        Some(0),
        "expected an empty peers list:\n{}",
        text_of(&empty)
    );

    // Federate A -> B by B's card URL and wait for the connection to form.
    let response = client
        .post(format!("http://127.0.0.1:{port_a}/api/peers"))
        .json(&serde_json::json!({
            "card_url": format!("http://127.0.0.1:{port_b}/.well-known/agent-card.json"),
            "label": "e2e-peer-b",
        }))
        .send()
        .await
        .expect("POST /api/peers");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "federating A->B failed (HTTP {status}): {body}\n--- daemon A ---\n{}",
        a.log_tail()
    );
    let peer = poll_until(
        "daemon A reporting peer B connected",
        PEER_WAIT_TIMEOUT,
        || {
            let client = client.clone();
            async move { connected_peer(&client, port_a).await }
        },
        || {
            format!(
                "--- daemon A log tail ---\n{}\n--- daemon B log tail ---\n{}",
                a.log_tail(),
                b.log_tail()
            )
        },
    )
    .await;
    let peer_id = peer
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("peer snapshot missing id: {peer}"))
        .to_string();
    let peer_label = peer
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    // `ctl peer list` prints the same snapshot GET /api/peers serves.
    let list = ctl(&a, &["peer", "list"]).await;
    assert!(list.status.success(), "{}", text_of(&list));
    let listed = stdout_json(&list);
    let entry = listed
        .get("peers")
        .and_then(|v| v.as_array())
        .and_then(|peers| {
            peers
                .iter()
                .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(peer_id.as_str()))
        })
        .unwrap_or_else(|| {
            panic!(
                "ctl peer list is missing peer {peer_id}:\n{}",
                text_of(&list)
            )
        })
        .clone();
    assert_eq!(
        entry
            .pointer("/connection_state/state")
            .and_then(|v| v.as_str()),
        Some("connected"),
        "ctl peer list did not report B connected:\n{}",
        text_of(&list)
    );
    assert_eq!(
        entry.get("label").and_then(|v| v.as_str()),
        Some(peer_label.as_str()),
        "ctl peer list label diverges from the /api/peers snapshot:\n{}",
        text_of(&list)
    );

    // Delegate a task to B through A; the command must print a task id.
    let instructions = format!("{TASK_MARK} - run the scripted delegated steps");
    let task = ctl(&a, &["peer", "task", &peer_id, &instructions]).await;
    assert!(task.status.success(), "{}", text_of(&task));
    let task_json = stdout_json(&task);
    let task_id = task_json
        .get("task_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !task_id.is_empty(),
        "ctl peer task did not print a task_id:\n{}",
        text_of(&task)
    );

    // B really ran it: only the TASK_MARK-matched profile emits the done
    // message, and the mock releases it only after the runtime's echo
    // reached the transcript — instructions crossed, session ran, exec
    // round-tripped.
    poll_until(
        "the delegated task completing on peer daemon B",
        PEER_WAIT_TIMEOUT,
        || {
            let logs = b.rig.session_logs();
            async move { logs.contains(DONE_MESSAGE).then_some(()) }
        },
        || {
            format!(
                "--- daemon B log tail ---\n{}\n--- daemon B session logs (tail) ---\n{}",
                b.log_tail(),
                tail(&b.rig.session_logs(), 4000)
            )
        },
    )
    .await;
    let artifacts = b.rig.turn_artifacts();
    assert!(
        artifacts.contains("PEER_E2E_ROUNDTRIP"),
        "peer B turn artifacts missing the delegated runtime stdout:\n{artifacts}"
    );

    // Explicit shutdown (kill_on_drop remains the panic-path backstop) so
    // both daemons are dead before their temp homes are removed.
    let _ = a.child.kill().await;
    let _ = b.child.kill().await;
}

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
/// on a cold CI runner. The deadline guards a daemon that comes up wedged —
/// alive but never serving its gateway — a permanent failure class that any
/// bound catches; a *crashed* daemon is caught immediately by the boot
/// poll's child-exit check regardless of this value, and a healthy boot
/// returns on its first successful poll, so headroom costs green runs
/// nothing. 60s was blown once by a healthy debug-build boot on the fleet
/// Mac leg at load >20 on 12 cores (2026-07-12), so 180s — the same scale
/// as RUN_TIMEOUT's one-shot ceiling — shrugs off runner load without
/// weakening the guard.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(180);

/// Deadline for the load-sensitive cross-daemon waits in the federated peer
/// test (the A->B connection forming, the delegated task completing on B).
/// A 90s ceiling was blown twice by a healthy tree on a loaded CI box —
/// debug binaries, several daemons, and concurrent jobs stack up — and the
/// suite's wall-clock cost is bounded by how fast the waits *succeed*, not
/// by this deadline, so be generous. (The Windows leg blew even 240s on the
/// task-completion wait on 2026-07-12; that incident's likely mechanism — a
/// StartTask lost to a fire-and-forget wire — is now fixed at the product
/// level: delegation resolves through an application delivery receipt with
/// at-least-once re-send + receiver dedup, so by the time `ctl peer task`
/// reports `delivery: "acknowledged"` the task is dispatched on B and this
/// deadline only covers the scripted run itself.)
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
            // The macOS/Windows half of the same contract: display
            // enumeration and capture serve a deterministic 1280×720
            // synthetic source instead of touching ScreenCaptureKit or
            // GDI/DXGI. Without this, a Windows daemon's startup
            // auto-activation BitBlts the runner's real desktop (or spins
            // in an Access-denied retry storm when the runner's screen is
            // locked), and a macOS grant starts a real SCK stream of the
            // runner account's screen. Honored only alongside
            // PROVIDER=mock — fail closed, like the provider itself.
            .env("INTENDANT_MOCK_DISPLAY", "synthetic")
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

/// One free loopback port, same caveats as [`two_free_loopback_ports`].
fn free_loopback_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("ephemeral port addr")
        .port()
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

    /// Tail of the daemon's durable federated peer-event record
    /// (`peers.jsonl`; see `peer::spawn_peer_log_writer`), for failure
    /// context AND for the delivery-receipt assertion: connection
    /// transitions and delegation-time activity live here, on the
    /// *sending* daemon, where a lost cross-daemon message leaves its
    /// only trace. The file lives in the daemon's *session* log dir —
    /// `build_and_hydrate_peer_registry(log_dir, …)` is called with the
    /// daemon session's directory (startup/wiring.rs), not the logs
    /// root — so scan every session dir like [`TestRig::session_logs`]
    /// does. (This helper originally read a flat
    /// `.intendant/logs/peers.jsonl` that nothing writes, so the
    /// forensics rail dumped empty; the receipt assertion made that
    /// visible.)
    fn peer_log_tail(&self) -> String {
        let logs_dir = self.rig.home.path().join(".intendant").join("logs");
        let mut combined = String::new();
        if let Ok(entries) = std::fs::read_dir(&logs_dir) {
            for entry in entries.flatten() {
                if let Ok(contents) = std::fs::read_to_string(entry.path().join("peers.jsonl")) {
                    combined.push_str(&contents);
                    combined.push('\n');
                }
            }
        }
        tail(&combined, 4000)
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
    spawn_daemon_on_rig(client, TestRig::new(), script, port, false).await
}

/// [`spawn_daemon`] against a caller-prepared rig — used when the rig's
/// state root needs provisioning (e.g. `access setup`) before boot. With
/// `tls` the daemon is left on its default TLS+mTLS path (it auto-loads
/// the rig's provisioned access certs), so `client` must tolerate the
/// self-signed server cert.
async fn spawn_daemon_on_rig(
    client: &reqwest::Client,
    rig: TestRig,
    script: &serde_json::Value,
    port: u16,
    tls: bool,
) -> DaemonRig {
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
    cmd.arg("--web").arg(port.to_string()).args([
        "--bind",
        "127.0.0.1",
        "--no-tui",
        "--autonomy",
        "full",
    ]);
    let (ws_scheme, http_scheme) = if tls {
        ("wss", "https")
    } else {
        cmd.arg("--no-tls");
        ("ws", "http")
    };
    cmd.arg("--advertise-url")
        .arg(format!("{ws_scheme}://127.0.0.1:{port}/ws"));
    let child = cmd.spawn().expect("spawn intendant daemon");
    let mut daemon = DaemonRig { child, rig, port };

    // Readiness. Plain rigs poll the agent card as before. TLS rigs
    // cannot: the gateway's default-mTLS policy requires a client cert
    // for everything except peer access and Connect bootstrap, and the
    // card is not exempt — so a certless probe would spin forever.
    // The pairing doorbell IS deliberately certless (it exists for
    // pre-identity bootstrap), so any HTTP response from it proves the
    // TLS listener is up (the vm pairing script probes the same path).
    let probe_url = if tls {
        format!("{http_scheme}://127.0.0.1:{port}/api/peer-pairing/requests/not-found")
    } else {
        format!("{http_scheme}://127.0.0.1:{port}/.well-known/agent-card.json")
    };
    let deadline = tokio::time::Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        let ready = if tls {
            client.get(&probe_url).send().await.is_ok()
        } else {
            http_get_json(client, &probe_url).await.is_some()
        };
        if ready {
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
            "daemon on port {port} did not answer {probe_url} within \
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

/// [`poll_until`]'s non-panicking core: poll `probe` every 250 ms until it
/// yields `Some`, or return `None` once `timeout` elapses — for
/// opportunistic waits where the caller has a recovery move on a miss
/// (e.g. re-delegating a possibly-lost peer task) rather than a verdict.
async fn try_poll_until<T, Fut>(timeout: Duration, mut probe: impl FnMut() -> Fut) -> Option<T>
where
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(found) = probe().await {
            return Some(found);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
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
    probe: impl FnMut() -> Fut,
    context: impl Fn() -> String,
) -> T
where
    Fut: std::future::Future<Output = Option<T>>,
{
    match try_poll_until(timeout, probe).await {
        Some(found) => found,
        None => panic!(
            "timed out after {timeout:?} waiting for {what}:\n{}",
            context()
        ),
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

/// Worktree sessions, the daemon lane: `CreateSession { worktree: true }`
/// must branch a fresh worktree off the project's HEAD and run the session
/// INSIDE it. The scripted loop proves the cwd through the real runtime by
/// writing a relative-path probe file — it must land in the worktree
/// checkout, not the base project — and the session's recorded meta pins
/// the linkage (branch, checkout path, base branch/sha) plus the effective
/// project root.
#[tokio::test]
async fn create_session_with_worktree_runs_inside_the_worktree() {
    use futures_util::SinkExt;

    let rig = TestRig::new();
    // The daemon's default project is a real git repo with one commit
    // (worktree launches branch from HEAD, so an empty repo is an error by
    // design). intendant.toml doubles as the rooted-daemon marker
    // spawn_daemon_on_rig would write; committing it keeps the base clean.
    let project = rig.project.path().to_path_buf();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&project)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "e2e@test.com"]);
    git(&["config", "user.name", "E2E"]);
    std::fs::write(project.join("intendant.toml"), "").expect("project marker");
    std::fs::write(project.join("README.md"), "# worktree e2e\n").expect("seed file");
    git(&["add", "."]);
    git(&["commit", "-m", "initial"]);
    let base_head = {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project)
            .output()
            .expect("read base HEAD");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // `echo MARKER && echo done > wt_probe.txt` works under both sh and
    // cmd; the relative redirect target is the cwd proof.
    let script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "Writing the cwd probe.",
                  "tool_calls": [{ "name": "exec_command",
                                   "arguments": { "nonce": 1,
                                                  "command": "echo WORKTREE_E2E_ROUNDTRIP && echo done > wt_probe.txt" } }] },
                { "expect_transcript_contains": "WORKTREE_E2E_ROUNDTRIP",
                  "content": "Probe written.",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "worktree session complete" } }] }
            ]
        }]
    });

    let client = reqwest::Client::new();
    let mut daemon = spawn_daemon_on_rig(&client, rig, &script, free_loopback_port(), false).await;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws", daemon.port))
            .await
            .expect("connect /ws");

    // The very first control message can race daemon startup on a
    // saturated box (the gateway accepts /ws a beat before the
    // supervisor's bus subscription), so retry until session_started.
    let mut started = None;
    for _ in 0..6 {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "action": "create_session",
                "task": "write the probe file in the isolated checkout",
                "direct": true,
                "worktree": true,
                "worktree_branch": "wt-e2e",
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send worktree create_session");
        started = next_matching_ws_event(&mut ws, Duration::from_secs(30), |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("session_started")
                && json
                    .get("task")
                    .and_then(|v| v.as_str())
                    .is_some_and(|task| task.contains("isolated checkout"))
        })
        .await;
        if started.is_some() {
            break;
        }
    }
    let started = started.unwrap_or_else(|| {
        panic!(
            "worktree create_session never started; daemon log:\n{}",
            daemon.log_tail()
        )
    });
    let session_id = started
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("session_started carries a session id")
        .to_string();

    complete_and_stop_session(&mut ws, &session_id, || daemon.log_tail()).await;

    // The worktree exists where the launch contract says (a `.git` file,
    // not a directory, marks a linked worktree checkout).
    let worktree_path = project.join(".intendant").join("worktrees").join("wt-e2e");
    assert!(
        worktree_path.join(".git").is_file(),
        "expected a linked worktree checkout at {}",
        worktree_path.display()
    );

    // cwd proof: the relative-path probe landed inside the worktree, and
    // did NOT land in the base project.
    assert!(
        worktree_path.join("wt_probe.txt").is_file(),
        "probe file missing from the worktree — the session did not run inside it; daemon log:\n{}",
        daemon.log_tail()
    );
    assert!(
        !project.join("wt_probe.txt").exists(),
        "probe file leaked into the base project — the session ran in the wrong cwd"
    );

    // Recorded linkage: effective project root is the worktree, and the
    // worktree meta names the branch and where it branched from.
    let meta_raw = std::fs::read_to_string(
        daemon
            .rig
            .home
            .path()
            .join(".intendant")
            .join("logs")
            .join(&session_id)
            .join("session_meta.json"),
    )
    .expect("session_meta.json for the worktree session");
    let meta: serde_json::Value = serde_json::from_str(&meta_raw).expect("meta parses");
    let recorded_root = meta
        .get("project_root")
        .and_then(|v| v.as_str())
        .expect("meta records a project root");
    assert!(
        std::fs::canonicalize(recorded_root).expect("recorded root exists")
            == std::fs::canonicalize(&worktree_path).expect("worktree exists"),
        "meta project_root {recorded_root} is not the worktree {}",
        worktree_path.display()
    );
    let linkage = meta.get("worktree").expect("meta records worktree linkage");
    assert_eq!(linkage["branch"], "wt-e2e", "{linkage}");
    assert_eq!(linkage["base_branch"], "main", "{linkage}");
    assert_eq!(
        linkage["base_sha"],
        serde_json::json!(base_head),
        "{linkage}"
    );
    assert!(
        linkage
            .get("base_root")
            .and_then(|v| v.as_str())
            .is_some_and(
                |root| std::fs::canonicalize(root).ok() == std::fs::canonicalize(&project).ok()
            ),
        "{linkage}"
    );

    // The done signal only fired after the runtime round-trip, and the
    // base checkout never left its branch.
    assert!(
        daemon
            .rig
            .session_logs()
            .contains("worktree session complete"),
        "session log missing the done signal"
    );
    let base_branch_now = std::process::Command::new("git")
        .args(["symbolic-ref", "--short", "-q", "HEAD"])
        .current_dir(&project)
        .output()
        .expect("read base branch");
    assert_eq!(
        String::from_utf8_lossy(&base_branch_now.stdout).trim(),
        "main",
        "worktree launch must not move the base checkout"
    );

    daemon.child.kill().await.expect("stop the daemon");
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

/// `intendant ctl session note` against a live daemon, end to end:
/// (1) the note broadcasts as an `OutboundEvent::SessionNote` on `/ws`
/// with the text, source, and attachment *references* (never bytes);
/// (2) the referenced blob really serves from the upload store's `/raw`
/// route with the stored MIME and exact bytes; (3) the note persists as a
/// `session_note` row in the session log — the replay source of truth.
#[tokio::test]
async fn ctl_session_note_posts_a_display_only_note_with_image() {
    const NOTE_TEXT: &str = "E2E_SESSION_NOTE milestone reached";
    // Not a decodable PNG — the note rail stores and serves bytes verbatim,
    // so any payload proves the round trip.
    const IMAGE_BYTES: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];

    let idle_script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]
        }]
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    let port = free_loopback_port();
    let daemon = spawn_daemon(&client, &idle_script, port).await;

    let image_path = daemon.rig.project.path().join("note-image.png");
    std::fs::write(&image_path, IMAGE_BYTES).expect("write note image");

    // Subscribe before posting so the broadcast cannot race the assert.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    let output = ctl(
        &daemon,
        &[
            "--json",
            "session",
            "note",
            NOTE_TEXT,
            "--image",
            image_path.to_str().expect("utf8 image path"),
            "--session",
            "note-e2e-session",
            "--source",
            "e2e",
        ],
    )
    .await;
    assert!(output.status.success(), "{}", text_of(&output));
    let posted = stdout_json(&output);
    assert_eq!(posted["status"], "posted", "{posted}");
    assert_eq!(posted["session_id"], "note-e2e-session", "{posted}");
    let note_id = posted["note_id"].as_str().expect("note_id").to_string();
    let attachment_url = posted["attachments"][0]["url"]
        .as_str()
        .expect("attachment url")
        .to_string();

    // (1) The /ws broadcast carries the note as references, not bytes.
    let event = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("session_note")
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "session_note never broadcast on /ws:\n{}",
            daemon.log_tail()
        )
    });
    assert_eq!(event["note_id"], note_id.as_str(), "{event}");
    assert_eq!(event["text"], NOTE_TEXT, "{event}");
    assert_eq!(event["source"], "e2e", "{event}");
    assert_eq!(event["session_id"], "note-e2e-session", "{event}");
    assert_eq!(
        event["attachments"][0]["url"],
        attachment_url.as_str(),
        "{event}"
    );
    assert_eq!(event["attachments"][0]["mime"], "image/png", "{event}");
    assert!(
        event["attachments"][0].get("data").is_none(),
        "attachments must be references, never inline bytes: {event}"
    );

    // (2) The referenced blob serves with the stored MIME and exact bytes.
    let response = client
        .get(format!("http://127.0.0.1:{port}{attachment_url}"))
        .send()
        .await
        .expect("GET note attachment");
    assert!(
        response.status().is_success(),
        "attachment fetch failed: HTTP {}\n{}",
        response.status(),
        daemon.log_tail()
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let bytes = response.bytes().await.expect("attachment bytes");
    assert_eq!(&bytes[..], IMAGE_BYTES, "blob bytes must round-trip");

    // (3) The note persisted to the session log for replay.
    poll_until(
        "the session_note row in the session log",
        RUN_TIMEOUT,
        || {
            let logs = daemon.rig.session_logs();
            let note_id = note_id.clone();
            async move {
                (logs.contains("\"event\":\"session_note\"") && logs.contains(&note_id))
                    .then_some(())
            }
        },
        || {
            format!(
                "--- session logs ---\n{}\n--- daemon log tail ---\n{}",
                tail(&daemon.rig.session_logs(), 2000),
                daemon.log_tail()
            )
        },
    )
    .await;
}

/// Task #6 end to end, against the real binaries: a resumable upload
/// rides direct HTTP as job create → capped raw chunks → commit,
/// survives a "client restart" (re-list by handle, resume at the
/// received extent, wrong-offset refusal), verifies the declared
/// sha256 at commit, and the finished file rides back out over the
/// download row — full read with `X-Content-Sha256`, then a `Range`
/// request answering 206 with the `X-Transfer-*` resume echoes. Both
/// delete shapes (native DELETE + the WKWebView POST fallback) tear the
/// jobs down.
#[tokio::test]
async fn transfer_jobs_round_trip_over_direct_http() {
    use sha2::Digest as _;

    let idle_script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]
        }]
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    let port = free_loopback_port();
    let daemon = spawn_daemon(&client, &idle_script, port).await;
    let base = format!("http://127.0.0.1:{port}");

    // Two uneven chunks force a mid-file resume boundary.
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let (head, tail_bytes) = payload.split_at(180_000);
    let sha256 = {
        let digest = sha2::Sha256::digest(&payload);
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let dest = daemon.rig.project.path().join("received").join("big.bin");
    std::fs::create_dir_all(dest.parent().expect("dest parent")).expect("mk dest dir");

    // Create the upload job (declared size + checksum).
    let created: serde_json::Value = client
        .post(format!("{base}/api/transfers"))
        .json(&serde_json::json!({
            "kind": "upload",
            "destination": dest.to_string_lossy(),
            "name": "big.bin",
            "total_size": payload.len(),
            "sha256": sha256,
        }))
        .send()
        .await
        .expect("create upload job")
        .json()
        .await
        .expect("create body");
    assert_eq!(created["ok"], true, "{created}");
    let job_id = created["job"]["id"].as_str().expect("job id").to_string();
    let resume_token = created["job"]["resume_token"]
        .as_str()
        .expect("resume token")
        .to_string();
    assert_eq!(created["job"]["completed_bytes"], 0, "{created}");

    // First chunk.
    let first = client
        .post(format!("{base}/api/transfers/{job_id}/chunk?offset=0"))
        .body(head.to_vec())
        .send()
        .await
        .expect("first chunk");
    assert_eq!(first.status().as_u16(), 200);
    let first: serde_json::Value = first.json().await.expect("first chunk body");
    assert_eq!(first["job"]["status"], "running", "{first}");
    assert_eq!(first["job"]["completed_bytes"], head.len(), "{first}");

    // "Client restart": re-list by handle and read the received extent.
    let listed: serde_json::Value = client
        .get(format!("{base}/api/transfers?id={job_id}"))
        .send()
        .await
        .expect("list jobs")
        .json()
        .await
        .expect("list body");
    let jobs = listed["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs.len(), 1, "{listed}");
    let boundary = jobs[0]["completed_bytes"].as_u64().expect("extent") as usize;
    assert_eq!(boundary, head.len(), "{listed}");

    // A wrong-offset chunk that overlaps the persisted extent without
    // being covered by it is refused — the resume contract (a fully
    // covered duplicate would be answered idempotently instead).
    let conflict = client
        .post(format!(
            "{base}/api/transfers/{job_id}/chunk?offset={}",
            boundary - 1
        ))
        .body(tail_bytes.to_vec())
        .send()
        .await
        .expect("conflicting chunk");
    assert_eq!(conflict.status().as_u16(), 409);

    // Resume at the boundary, addressing the job by resume token.
    let second = client
        .post(format!(
            "{base}/api/transfers/{resume_token}/chunk?offset={boundary}"
        ))
        .body(tail_bytes.to_vec())
        .send()
        .await
        .expect("second chunk");
    assert_eq!(second.status().as_u16(), 200);
    let second: serde_json::Value = second.json().await.expect("second chunk body");
    assert_eq!(second["job"]["status"], "ready", "{second}");

    // Commit verifies size + sha256 and renames into place.
    let committed = client
        .post(format!("{base}/api/transfers/{job_id}/commit"))
        .send()
        .await
        .expect("commit");
    assert_eq!(committed.status().as_u16(), 200);
    let committed: serde_json::Value = committed.json().await.expect("commit body");
    assert_eq!(committed["job"]["status"], "completed", "{committed}");
    assert_eq!(std::fs::read(&dest).expect("committed file"), payload);

    // Round-trip back out: a download job over the same lane.
    let download: serde_json::Value = client
        .post(format!("{base}/api/transfers"))
        .json(&serde_json::json!({
            "kind": "download",
            "path": dest.to_string_lossy(),
        }))
        .send()
        .await
        .expect("create download job")
        .json()
        .await
        .expect("download job body");
    assert_eq!(download["ok"], true, "{download}");
    let download_id = download["job"]["id"].as_str().expect("dl id").to_string();

    // Full read: 200 with the content hash + resume echoes.
    let full = client
        .get(format!("{base}/api/transfers/{download_id}/download"))
        .send()
        .await
        .expect("full download");
    assert_eq!(full.status().as_u16(), 200);
    assert_eq!(
        full.headers()
            .get("X-Content-Sha256")
            .and_then(|value| value.to_str().ok()),
        Some(sha256.as_str())
    );
    assert_eq!(
        full.headers()
            .get("X-Transfer-Total-Size")
            .and_then(|value| value.to_str().ok()),
        Some(payload.len().to_string().as_str())
    );
    assert_eq!(full.bytes().await.expect("full body").as_ref(), payload);

    // Ranged read: standard 206/Content-Range plus the end-exclusive
    // X-Transfer resume echoes.
    let ranged = client
        .get(format!("{base}/api/transfers/{download_id}/download"))
        .header("Range", "bytes=100-199")
        .send()
        .await
        .expect("ranged download");
    assert_eq!(ranged.status().as_u16(), 206);
    assert_eq!(
        ranged
            .headers()
            .get("Content-Range")
            .and_then(|value| value.to_str().ok()),
        Some(format!("bytes 100-199/{}", payload.len()).as_str())
    );
    assert_eq!(
        ranged
            .headers()
            .get("X-Transfer-Range-End")
            .and_then(|value| value.to_str().ok()),
        Some("200")
    );
    assert_eq!(
        ranged.bytes().await.expect("ranged body").as_ref(),
        &payload[100..200]
    );

    // Teardown over both delete shapes.
    let deleted: serde_json::Value = client
        .delete(format!("{base}/api/transfers/{job_id}"))
        .send()
        .await
        .expect("native delete")
        .json()
        .await
        .expect("delete body");
    assert_eq!(deleted["deleted"], true, "{deleted}");
    let fallback: serde_json::Value = client
        .post(format!("{base}/api/transfers/{download_id}/delete"))
        .send()
        .await
        .expect("fallback delete")
        .json()
        .await
        .expect("fallback delete body");
    assert_eq!(fallback["deleted"], true, "{fallback}");
}

/// The Stream lane over direct HTTP (transport-unification S10):
/// `GET /api/sessions/stream` answers the NDJSON head (EOF-delimited,
/// `application/x-ndjson`) and the shared line source's lifecycle —
/// start, the hydrating phase marker, the replace payload, done — as
/// parseable one-object lines, against the real daemon binary.
#[tokio::test]
async fn sessions_stream_serves_ndjson_over_direct_http() {
    let idle_script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]
        }]
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    let port = free_loopback_port();
    let daemon = spawn_daemon(&client, &idle_script, port).await;

    let response = client
        .get(format!(
            "http://127.0.0.1:{port}/api/sessions/stream?limit=50"
        ))
        .send()
        .await
        .expect("stream response");
    assert_eq!(response.status().as_u16(), 200, "{}", daemon.log_tail());
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/x-ndjson")
    );
    // EOF-delimited: no Content-Length on the streamed head.
    assert!(response.headers().get("content-length").is_none());

    let body = response.text().await.expect("stream body");
    let events: Vec<serde_json::Value> = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("one JSON object per NDJSON line"))
        .collect();
    assert!(events.len() >= 3, "{body}");
    assert_eq!(events.first().unwrap()["type"], "start", "{body}");
    assert_eq!(events.first().unwrap()["limit"], 50, "{body}");
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "phase" && event["phase"] == "hydrating"),
        "{body}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "replace" && event["sessions"].is_array()),
        "{body}"
    );
    assert_eq!(events.last().unwrap()["type"], "done", "{body}");
}

/// The display-request rail end to end: a caller rings the user-display
/// doorbell (`ctl display request` → the `request_user_display` tool), a
/// dashboard surface (here: a raw `/ws` client) receives the dedicated
/// `display_request_raised` event and resolves it with the dedicated
/// `resolve_display_request` action — approve mints the real grant
/// (`user_display_granted` broadcast) and the blocked ctl call returns the
/// structured approved result. The daemon runs at `--autonomy full`, which
/// proves the rail's core invariant live: even full autonomy never
/// auto-approves a display request — it waits for the click. The deny leg
/// exercises deny + the per-session cooldown (the following ask is refused
/// without raising anything).
///
/// Leg 0 pins the held-grant short-circuit, which is also what makes the
/// test platform-uniform: a Windows daemon auto-registers the user desktop
/// and holds the grant from startup
/// (`display_glue::auto_activate_windows_user_display` — capture consent
/// is implicit there by design), so a request on a fresh Windows daemon
/// correctly answers `already_granted` without ringing. Granting
/// explicitly first makes every platform take that same path, and the
/// revoke that follows gives the popup legs one clean, grantless starting
/// state everywhere.
#[tokio::test]
async fn display_request_rail_round_trips_over_ws() {
    use futures_util::SinkExt;

    const REASON: &str = "E2E_DISPLAY_REQUEST verify the deploy output";

    // Displayless contract: with the suite-wide synthetic display backend
    // (`INTENDANT_MOCK_DISPLAY=synthetic`), the whole rail round-trip is
    // fast on every platform. A platform capture stack sneaking back in
    // shows up here as seconds-to-minutes (Windows GDI Access-denied retry
    // storms on a locked runner, SCK/TCC stalls) — so pin the wall clock.
    // Generous versus the single-digit-second measured times: the fleet
    // Mac leg breached a 30s bound at 35s while 3x oversubscribed (load
    // avg 39 on 12 cores, two merge-queue ejections on 2026-07-12), and
    // the failure classes this guards are minutes-scale, so 90s keeps
    // the guard while shrugging off runner load.
    let wall_clock = std::time::Instant::now();
    const RAIL_WALL_CLOCK_BOUND: Duration = Duration::from_secs(90);

    let idle_script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]
        }]
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    let port = free_loopback_port();
    let daemon = spawn_daemon(&client, &idle_script, port).await;

    // Subscribe before requesting so the broadcast cannot race the assert.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    // ── Leg 0: a held grant short-circuits without ringing ──
    let output = ctl(&daemon, &["display", "grant-user"]).await;
    assert!(output.status.success(), "{}", text_of(&output));
    let output = ctl(
        &daemon,
        &[
            "--json",
            "display",
            "request",
            "--reason",
            "is the door already open?",
            "--session",
            "display-e2e-pregrant",
        ],
    )
    .await;
    assert!(output.status.success(), "{}", text_of(&output));
    let result = stdout_json(&output);
    assert_eq!(result["status"], "already_granted", "{result}");

    // Clean slate for the popup legs (this also revokes the Windows
    // startup auto-grant like any other grant).
    let output = ctl(&daemon, &["display", "revoke-user"]).await;
    assert!(output.status.success(), "{}", text_of(&output));

    // ── Leg 1: request(view_and_control) → user approves ──
    let request = ctl(
        &daemon,
        &[
            "--json",
            "display",
            "request",
            "--reason",
            REASON,
            "--access",
            "control",
            "--wait",
            "60",
            "--session",
            "display-e2e-approve",
        ],
    );
    let resolver = async {
        let raised = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("display_request_raised")
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "display_request_raised never broadcast on /ws:\n{}",
                daemon.log_tail()
            )
        });
        assert_eq!(raised["session_id"], "display-e2e-approve", "{raised}");
        assert_eq!(raised["access"], "view_and_control", "{raised}");
        assert_eq!(raised["reason"], REASON, "{raised}");
        assert!(
            raised["expires_unix_ms"].as_u64().unwrap_or(0) > 0,
            "{raised}"
        );
        let id = raised["id"].as_u64().expect("request id");

        // The user's click: the dedicated resolution action (a display
        // request is NEVER resolvable through approve/approve_all).
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "action": "resolve_display_request",
                "session_id": "display-e2e-approve",
                "id": id,
                "decision": "approve",
                "duration": "until_revoked",
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send resolve_display_request");

        // The approval minted the real grant through the existing path.
        // Emission order inside the control plane's approve arm: the grant
        // event first, then the resolution — and a /ws reader consumes
        // frames in order, so wait for them in that order.
        let granted = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("user_display_granted")
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "user_display_granted never broadcast after approval:\n{}",
                daemon.log_tail()
            )
        });
        assert_eq!(granted["display_id"], 0, "{granted}");
        assert_eq!(granted["agent_visible"], true, "{granted}");

        let resolved = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("display_request_resolved")
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "display_request_resolved never broadcast:\n{}",
                daemon.log_tail()
            )
        });
        assert_eq!(resolved["id"], id, "{resolved}");
        assert_eq!(resolved["outcome"], "approved", "{resolved}");
        assert_eq!(resolved["duration"], "until_revoked", "{resolved}");

        // The grant the approval minted also activated a capture session,
        // and under the suite-wide synthetic display mode that session is
        // the deterministic synthetic source on every platform: 1280×720,
        // no ScreenCaptureKit / GDI / X11 involved. Its `display_ready`
        // geometry is the end-to-end proof — a platform backend would
        // report the host's real resolution (or never come up at all on a
        // headless runner). Any leg-0 display events were consumed by the
        // raised-matcher above, so the next display_ready is this leg's.
        let ready = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("display_ready")
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "display_ready never broadcast after the approved grant:\n{}",
                daemon.log_tail()
            )
        });
        assert_eq!(ready["display_id"], 0, "{ready}");
        assert_eq!(ready["width"], 1280, "{ready}");
        assert_eq!(ready["height"], 720, "{ready}");
        assert_eq!(ready["agent_visible"], true, "{ready}");
    };
    let (output, ()) = tokio::join!(request, resolver);
    assert!(output.status.success(), "{}", text_of(&output));
    let result = stdout_json(&output);
    assert_eq!(result["status"], "approved", "{result}");
    assert_eq!(result["access"], "view_and_control", "{result}");
    assert_eq!(result["duration"], "until_revoked", "{result}");

    // ── Leg 2: revoke, then a request the user denies ──
    let output = ctl(&daemon, &["display", "revoke-user"]).await;
    assert!(output.status.success(), "{}", text_of(&output));

    let request = ctl(
        &daemon,
        &[
            "--json",
            "display",
            "request",
            "--reason",
            "second look please",
            "--access",
            "view",
            "--wait",
            "60",
            "--session",
            "display-e2e-deny",
        ],
    );
    let resolver = async {
        let raised = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("display_request_raised")
                && json.get("session_id").and_then(|v| v.as_str()) == Some("display-e2e-deny")
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "deny-leg display_request_raised never broadcast:\n{}",
                daemon.log_tail()
            )
        });
        assert_eq!(raised["access"], "view", "{raised}");
        let id = raised["id"].as_u64().expect("request id");
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "action": "resolve_display_request",
                "session_id": "display-e2e-deny",
                "id": id,
                "decision": "deny",
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send deny");
        let resolved = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("display_request_resolved")
                && json.get("id").and_then(|v| v.as_u64()) == Some(id)
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "deny-leg display_request_resolved never broadcast:\n{}",
                daemon.log_tail()
            )
        });
        assert_eq!(resolved["outcome"], "denied", "{resolved}");
    };
    let (output, ()) = tokio::join!(request, resolver);
    assert!(output.status.success(), "{}", text_of(&output));
    let result = stdout_json(&output);
    assert_eq!(result["status"], "denied", "{result}");
    assert!(
        result["retry_after_secs"].as_u64().unwrap_or(0) > 0,
        "{result}"
    );

    // ── Leg 3: the cooldown refuses the next ask without a popup ──
    let output = ctl(
        &daemon,
        &[
            "--json",
            "display",
            "request",
            "--reason",
            "asking again immediately",
            "--session",
            "display-e2e-deny",
        ],
    )
    .await;
    assert!(output.status.success(), "{}", text_of(&output));
    let result = stdout_json(&output);
    assert_eq!(result["status"], "cooldown", "{result}");
    assert!(
        result["retry_after_secs"].as_u64().unwrap_or(0) > 0,
        "{result}"
    );

    let elapsed = wall_clock.elapsed();
    assert!(
        elapsed < RAIL_WALL_CLOCK_BOUND,
        "display-request rail e2e took {elapsed:?} (bound {RAIL_WALL_CLOCK_BOUND:?}) — \
         is a real capture backend in play?\n{}",
        daemon.log_tail()
    );
}

/// The CU action-visualization lane end to end: grant the (synthetic) user
/// display, execute one `screenshot` action through `ctl cu actions` (the
/// MCP `execute_cu_actions` tool → `computer_use::execute_actions` → the
/// `CuActionObserver`), and require the display-scoped `cu_action` event
/// the Live tab's overlays/feed render from to broadcast on `/ws` with the
/// pinned wire shape. A screenshot is the one action that is safe on every
/// backend here: it is input-free and the suite-wide synthetic display
/// serves the frame (no SCK/GDI/X11). Also pins the lane's ephemerality —
/// the event must never land in session.jsonl (no replay).
#[tokio::test]
async fn cu_actions_broadcast_display_scoped_events_over_ws() {
    let idle_script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]
        }]
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    let port = free_loopback_port();
    let daemon = spawn_daemon(&client, &idle_script, port).await;

    // Subscribe before acting so the broadcast cannot race the assert.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    // Synthetic user-display capture session (1280×720 on every platform).
    let output = ctl(&daemon, &["display", "grant-user"]).await;
    assert!(output.status.success(), "{}", text_of(&output));

    // The grant returns once RECORDED; the capture session registers
    // asynchronously — "capture is ready after DisplayReady" is the
    // command's own contract. A CU call racing past registration misses
    // the session lookup and falls to the subprocess capture path, which
    // for a user-session target on Linux is a real X server — correctly
    // absent on a headless runner, so the action fails and the observer
    // (by design) emits nothing. Proven live on the Linux fleet box:
    // firing immediately after grant-user loses that race 5/5 with
    // "cannot connect to X display". Screenshots are idempotent, so
    // retry the action itself until its per-action result reports ok —
    // this also avoids racing the /ws broadcast for the grant's own
    // display_ready, and `ctl cu actions` exits 0 even for failed
    // actions (failures are informational summaries), so only the
    // per-action text is trustworthy. A persistent real failure
    // surfaces here with its actual error text instead of as a /ws
    // timeout downstream.
    let deadline = tokio::time::Instant::now() + RUN_TIMEOUT;
    loop {
        let output = ctl(
            &daemon,
            &[
                "--json",
                "cu",
                "actions",
                "--actions",
                r#"[{"type":"screenshot"}]"#,
                "--target",
                "user_session",
            ],
        )
        .await;
        assert!(output.status.success(), "{}", text_of(&output));
        if String::from_utf8_lossy(&output.stdout).contains("(screenshot): ok") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "CU screenshot never succeeded (capture session still absent?):\n{}\n{}",
            text_of(&output),
            daemon.log_tail()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let event = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("cu_action")
    })
    .await
    .unwrap_or_else(|| panic!("cu_action never broadcast on /ws:\n{}", daemon.log_tail()));
    assert_eq!(event["display_id"], 0, "{event}");
    assert_eq!(event["kind"], "screenshot", "{event}");
    assert_eq!(event["raw"], "screenshot()", "{event}");
    // Coordinate reference = the synthetic session resolution, the space
    // viewers normalize overlay geometry against.
    assert_eq!(event["ref_w"], 1280, "{event}");
    assert_eq!(event["ref_h"], 720, "{event}");
    // A screenshot has no landing point, and the MCP surface is
    // sessionless — absent fields are omitted from the wire, not nulled.
    assert!(event.get("x").is_none(), "{event}");
    assert!(event.get("y").is_none(), "{event}");
    assert!(event.get("session_id").is_none(), "{event}");
    assert!(event["ts"].as_u64().unwrap_or(0) > 0, "{event}");
    assert!(
        event["event_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("cu-"),
        "{event}"
    );

    // Ephemeral lane: the broadcast above is the event's ONLY life — it
    // must never be written to session.jsonl (and hence never replays).
    // An absence check can pass early by timing alone; the authoritative
    // pin is event.rs's cu_action_events_never_reach_the_session_log —
    // this asserts the same contract at the wire edge.
    assert!(
        !daemon.rig.session_logs().contains("\"cu_action\""),
        "cu_action must not be written to session.jsonl"
    );
}

/// The CU observation policy end to end on the synthetic rig: `--observe`
/// drives what the batch result carries, and the result names the
/// observation and why. A `wait` action is the safe probe on every backend
/// (no OS input path, no capture race): `ax`/`auto`/`none` never capture
/// pixels, and under the armed synthetic backend the element walk serves the
/// deterministic synthetic tree instead of touching a native accessibility
/// API (macOS AX / AT-SPI / UIA) — this must stay true or CI walks a fleet
/// runner's real desktop.
#[tokio::test]
async fn cu_observe_modes_choose_ax_pixels_or_nothing() {
    let idle_script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]
        }]
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    let port = free_loopback_port();
    let daemon = spawn_daemon(&client, &idle_script, port).await;

    // user_session CU requires the display grant (the synthetic backend
    // serves the capture session it registers).
    let output = ctl(&daemon, &["display", "grant-user"]).await;
    assert!(output.status.success(), "{}", text_of(&output));

    let run = |observe: &'static str| {
        let daemon = &daemon;
        async move {
            let output = ctl(
                daemon,
                &[
                    "cu",
                    "actions",
                    "--actions",
                    r#"[{"type":"wait","ms":1}]"#,
                    "--target",
                    "user_session",
                    "--observe",
                    observe,
                ],
            )
            .await;
            assert!(output.status.success(), "{}", text_of(&output));
            let text = String::from_utf8_lossy(&output.stdout).to_string();
            assert!(
                text.contains("(wait 1ms): ok"),
                "wait action must verify ({observe}):\n{text}"
            );
            text
        }
    };

    // observe=ax: the element tree IS the observation — synthetic tree text
    // inline, no screenshot capture at all.
    let text = run("ax").await;
    assert!(
        text.contains("observation: ax (observe=ax"),
        "result must name the ax observation:\n{text}"
    );
    assert!(
        text.contains("Synthetic Desktop") && text.contains("button \"OK\""),
        "synthetic element tree must ride the result:\n{text}"
    );
    assert!(
        !text.contains("post-action screenshot captured"),
        "ax observation must not capture pixels:\n{text}"
    );

    // observe=auto: the synthetic tree is above the usability floor, so auto
    // deterministically picks ax and says so.
    let text = run("auto").await;
    assert!(
        text.contains("observation: ax (auto: ax usable ("),
        "auto must pick the usable tree and give the reason:\n{text}"
    );
    assert!(
        text.contains("--- screen elements ---"),
        "auto-chosen ax observation must carry the tree:\n{text}"
    );

    // observe=none: results only.
    let text = run("none").await;
    assert!(
        text.contains("observation: none (observe=none)"),
        "none must be named:\n{text}"
    );
    assert!(
        !text.contains("post-action screenshot captured")
            && !text.contains("--- screen elements ---"),
        "observe=none must attach nothing:\n{text}"
    );
}

/// `intendant ctl ask` end to end: the ctl process BLOCKS while the daemon
/// renders the question on the rail (`user_question` on /ws), a frontend
/// answers via `answer_question`, and the blocked ctl returns the exact
/// answer — plus `approval_resolved` so every other dashboard clears.
/// This is the codex/MCP/ctl path to the question rail that previously
/// only the native loop and supervised Claude Code could reach.
#[tokio::test]
async fn ctl_ask_blocks_until_the_dashboard_answers() {
    const QUESTION: &str = "Which color should the widget be?";
    const ANSWER: &str = "cerulean";

    let idle_script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]
        }]
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    let port = free_loopback_port();
    let daemon = spawn_daemon(&client, &idle_script, port).await;

    // Subscribe before asking so the broadcast cannot race the assert.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    // Spawn `ctl ask` WITHOUT awaiting: it must stay blocked until the
    // answer lands (same construction as the ctl() helper).
    let mut cmd = daemon.rig.command();
    cmd.env_remove("INTENDANT_MCP_URL")
        .env_remove("INTENDANT_PORT")
        .env_remove("INTENDANT_SESSION_ID")
        .env_remove("INTENDANT_MANAGED_CONTEXT");
    cmd.arg("ctl")
        .arg("--port")
        .arg(daemon.port.to_string())
        .args([
            "--json",
            "ask",
            QUESTION,
            "--option",
            "red:Warm and bold",
            "--option",
            "blue",
            "--header",
            "Paint",
            "--session",
            "ask-e2e-session",
        ]);
    let ask_child = cmd.spawn().expect("spawn ctl ask");

    // (1) The existing question rail event announces the structured ask.
    let question = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("user_question")
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "user_question never broadcast on /ws:\n{}",
            daemon.log_tail()
        )
    });
    assert_eq!(question["session_id"], "ask-e2e-session", "{question}");
    assert_eq!(question["questions"][0]["question"], QUESTION, "{question}");
    assert_eq!(question["questions"][0]["header"], "Paint", "{question}");
    assert_eq!(
        question["questions"][0]["options"][0]["label"], "red",
        "{question}"
    );
    assert_eq!(
        question["questions"][0]["options"][0]["description"], "Warm and bold",
        "{question}"
    );
    assert_eq!(
        question["questions"][0]["options"][1]["label"], "blue",
        "{question}"
    );
    let question_id = question
        .get("id")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("user_question must carry an id, got {question}"));

    // (2) Answer from the dashboard wire (free text — always allowed).
    {
        use futures_util::SinkExt;
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "action": "answer_question",
                "session_id": "ask-e2e-session",
                "id": question_id,
                "answers": { QUESTION: ANSWER },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send answer_question");
    }

    // (3) The blocked ctl returns the structured outcome with the answer.
    let output = tokio::time::timeout(RUN_TIMEOUT, ask_child.wait_with_output())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "ctl ask did not return after the answer:\n{}",
                daemon.log_tail()
            )
        })
        .expect("collect ctl ask output");
    assert!(output.status.success(), "{}", text_of(&output));
    let outcome = stdout_json(&output);
    assert_eq!(outcome["status"], "answered", "{outcome}");
    assert_eq!(outcome["answer"], ANSWER, "{outcome}");
    assert_eq!(outcome["answers"][QUESTION], ANSWER, "{outcome}");
    assert_eq!(outcome["id"], question_id, "{outcome}");

    // (4) The resolution broadcast clears the rail on other dashboards.
    let resolved = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("approval_resolved")
            && json.get("id").and_then(|v| v.as_u64()) == Some(question_id)
    })
    .await
    .unwrap_or_else(|| panic!("approval_resolved never broadcast:\n{}", daemon.log_tail()));
    assert_eq!(resolved["action"], "answer", "{resolved}");
}

/// `intendant ctl notify` end to end: fire-and-forget returns immediately,
/// the `user_notification` event reaches /ws with its urgency, and the
/// notification persists into the session log for replay.
#[tokio::test]
async fn ctl_notify_broadcasts_and_persists() {
    const TEXT: &str = "E2E_NOTIFY deploy finished";

    let idle_script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]
        }]
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    let port = free_loopback_port();
    let daemon = spawn_daemon(&client, &idle_script, port).await;

    // Subscribe before notifying so the broadcast cannot race the assert.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    let output = ctl(
        &daemon,
        &[
            "--json",
            "notify",
            TEXT,
            "--title",
            "CI",
            "--urgency",
            "attention",
            "--session",
            "notify-e2e-session",
        ],
    )
    .await;
    assert!(output.status.success(), "{}", text_of(&output));
    let sent = stdout_json(&output);
    assert_eq!(sent["status"], "sent", "{sent}");
    assert_eq!(sent["session_id"], "notify-e2e-session", "{sent}");
    assert_eq!(sent["urgency"], "attention", "{sent}");
    let notification_id = sent["id"].as_str().expect("notification id").to_string();

    // (1) The /ws broadcast carries the notification for toast/attention.
    let event = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("user_notification")
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "user_notification never broadcast on /ws:\n{}",
            daemon.log_tail()
        )
    });
    assert_eq!(event["id"], notification_id.as_str(), "{event}");
    assert_eq!(event["text"], TEXT, "{event}");
    assert_eq!(event["title"], "CI", "{event}");
    assert_eq!(event["urgency"], "attention", "{event}");
    assert_eq!(event["session_id"], "notify-e2e-session", "{event}");

    // (2) The notification persisted to the session log for replay.
    poll_until(
        "the user_notification row in the session log",
        RUN_TIMEOUT,
        || {
            let logs = daemon.rig.session_logs();
            let notification_id = notification_id.clone();
            async move {
                (logs.contains("\"event\":\"user_notification\"")
                    && logs.contains(&notification_id))
                .then_some(())
            }
        },
        || {
            format!(
                "--- session logs ---\n{}\n--- daemon log tail ---\n{}",
                tail(&daemon.rig.session_logs(), 2000),
                daemon.log_tail()
            )
        },
    )
    .await;
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

    // Failure forensics shared by every cross-daemon wait below. Always
    // dump BOTH sides: the narrative of a delegation lost in flight lives
    // on A (connection transitions in its daemon log, the durable
    // peer-event record in its peers.jsonl), while only session activity
    // shows on B — a B-only dump cannot distinguish "B is slow" from "B
    // never received the task" (the blind spot of the 2026-07-12 Windows
    // timeout, which dumped B alone).
    let dump_daemons = || {
        format!(
            "--- daemon A log tail ---\n{}\n\
             --- daemon A peers.jsonl tail ---\n{}\n\
             --- daemon B log tail ---\n{}\n\
             --- daemon B session logs (tail) ---\n{}",
            a.log_tail(),
            a.peer_log_tail(),
            b.log_tail(),
            tail(&b.rig.session_logs(), 4000),
        )
    };

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
        &dump_daemons,
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

    // Delegate a task to B through A. Delegation now resolves through the
    // application-level delivery receipt (`delegation_id` on the StartTask
    // frame; B acks with `task_received` when it actually dispatches):
    // `delivery: "acknowledged"` means the task is running on B and
    // `task_id` is B's real session id — not the sender-minted
    // `task-out-{n}` marker of the fire-and-forget era. A StartTask lost
    // before B reads it is re-sent under the same delegation id and B
    // dedups, so this test's former arrival gate + one-shot re-delegation
    // is retired: the product owns the retry now.
    let instructions = format!("{TASK_MARK} - run the scripted delegated steps");
    let task = ctl(&a, &["peer", "task", &peer_id, &instructions]).await;
    assert!(task.status.success(), "{}", text_of(&task));
    let task_json = stdout_json(&task);
    let task_id = task_json
        .get("task_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        !task_id.is_empty(),
        "ctl peer task did not print a task_id:\n{}",
        text_of(&task)
    );
    assert_eq!(
        task_json.get("delivery").and_then(|v| v.as_str()),
        Some("acknowledged"),
        "peer delegation was not acknowledged by B:\n{}\n{}",
        text_of(&task),
        dump_daemons()
    );
    let delegation_id = task_json
        .get("delegation_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        !delegation_id.is_empty(),
        "acknowledged delegation must report its delegation_id:\n{}",
        text_of(&task)
    );
    assert!(
        !task_id.starts_with("task-out-"),
        "acknowledged delivery must carry B's session id, not the \
         sender-side marker {task_id}:\n{}",
        dump_daemons()
    );
    // The acknowledged id is real on B: its session log directory exists
    // (created before dispatch, so by receipt time it must be on disk).
    assert!(
        b.rig
            .home
            .path()
            .join(".intendant")
            .join("logs")
            .join(&task_id)
            .join("session.jsonl")
            .exists(),
        "B has no session log for acknowledged session {task_id}:\n{}",
        dump_daemons()
    );
    // And the receipt leaves a durable trace on A's federated peer-event
    // record (`peers.jsonl`, the forensics rail dump_daemons reads). The
    // actor writes it via the async log sink, so it may trail the ctl
    // response by a beat — poll briefly.
    poll_until(
        "the task_receipt landing in daemon A's peers.jsonl",
        Duration::from_secs(30),
        || {
            let tail = a.peer_log_tail();
            let delegation_id = delegation_id.clone();
            async move {
                (tail.contains("task_receipt") && tail.contains(&delegation_id)).then_some(())
            }
        },
        &dump_daemons,
    )
    .await;

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
        &dump_daemons,
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

/// The remainder of a stdout line beginning with `prefix` — the pairing
/// CLI's contract for machine consumption (`:: request id: …`).
fn line_suffix(text: &str, prefix: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no line starts with {prefix:?} in:\n{text}"))
        .trim()
        .to_string()
}

/// Run `intendant ctl --peer <peer> <args…>` from side A's project —
/// deliberately no `--port` and no daemon on side A: peer routing reads
/// `[[peer]]` from the project config and dials the peer directly.
async fn ctl_peer(rig: &TestRig, peer: &str, args: &[&str]) -> std::process::Output {
    let mut cmd = rig.command();
    cmd.env_remove("INTENDANT_MCP_URL")
        .env_remove("INTENDANT_PORT")
        .env_remove("INTENDANT_SESSION_ID")
        .env_remove("INTENDANT_MANAGED_CONTEXT");
    cmd.arg("ctl").args(["--peer", peer]).args(args);
    rig.run(cmd).await
}

/// The full mTLS peer-principal path over a real pairing ceremony — what
/// the `--no-tls` federation test above deliberately cannot exercise:
/// there every /mcp request binds the tokenless-loopback principal, never
/// a peer identity. Here daemon B serves TLS+mTLS from a provisioned
/// access store; side A (no daemon and no pre-provisioned store — `peer
/// request` mints its own keypair) pairs via request → headless approve →
/// complete under the scoped `read-only-display` profile, then drives B's
/// /mcp daemon-lessly with `ctl --peer`.
///
/// The allowed call proves the issued client cert resolved to the granted
/// profile: ctl's `x-intendant-peer` marker makes an unresolvable cert a
/// hard 403, so success cannot be an anonymous fallback. The denied call
/// proves the same principal is refused display input, and the diagnostic
/// must name the peer-daemon principal — a transport failure or a
/// non-peer denial would not. A second ceremony under `peer-operator`
/// then completes the matrix: the swapped-in cert clears the
/// display-input gate for the very tool the read-only principal was
/// refused (pinned via the handler's pre-display "No actions provided"
/// reply, so the leg holds on headless rigs).
///
/// Not on Windows: the `intendant access` provisioning CLI is
/// `#[cfg(not(target_os = "windows"))]` and `WindowsBackend::cert_dir()`
/// ignores the sandboxed state root, so the rig cannot provision there.
#[cfg(not(windows))]
#[tokio::test]
async fn ctl_peer_mtls_pairing_binds_scoped_profile_and_gates_display_input() {
    let insecure_probe = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(4))
        .build()
        .expect("build insecure probe client");

    // Side B: provision an isolated access store under the rig's state
    // root, then boot the daemon on its default TLS+mTLS path (no
    // `--no-tls` ⇒ it auto-loads the store's server cert and verifies
    // client certs against the store's CA).
    let port_b = free_loopback_port();
    let rig_b = TestRig::new();
    std::fs::write(rig_b.project.path().join("intendant.toml"), "")
        .expect("mark side B's project root");
    let setup = {
        let mut cmd = rig_b.command();
        cmd.args([
            "access",
            "setup",
            "--ip",
            "127.0.0.1",
            "--host",
            "localhost",
            "--name",
            "peer-e2e-b",
            "--port",
            &port_b.to_string(),
            "--no-serve-certs",
            "--force",
        ]);
        rig_b.run(cmd).await
    };
    assert!(
        setup.status.success(),
        "access setup failed:\n{}",
        text_of(&setup)
    );

    let idle_script = serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "fallback profile (unexpected session)",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "unexpected session" } }] }
            ]
        }]
    });
    let mut daemon_b =
        spawn_daemon_on_rig(&insecure_probe, rig_b, &idle_script, port_b, true).await;

    // Side A: a bare project, nothing else.
    let rig_a = TestRig::new();
    std::fs::write(rig_a.project.path().join("intendant.toml"), "")
        .expect("mark side A's project root");
    let request = {
        let mut cmd = rig_a.command();
        cmd.args([
            "peer",
            "request",
            &format!("https://127.0.0.1:{port_b}"),
            "--label",
            "peer-e2e-a",
            "--profile",
            "read-only-display",
        ]);
        rig_a.run(cmd).await
    };
    assert!(
        request.status.success(),
        "peer request failed:\n{}\ndaemon B log:\n{}",
        text_of(&request),
        daemon_b.log_tail()
    );
    let request_stdout = String::from_utf8_lossy(&request.stdout).into_owned();
    let request_id = line_suffix(&request_stdout, ":: request id: ");
    let approval_code = line_suffix(&request_stdout, ":: approval code: ");

    // Approve headlessly on B: a direct state-file write against B's
    // cert store (the running daemon rereads the file per status poll).
    // The profile is stated explicitly again — approval, not the
    // request, is what grants — and canonically: the CLI validates
    // profile names loudly, while unknown strings arriving on the wire
    // are stored as-is and stay fail-closed (presence-only) at
    // authorization time.
    let approve = {
        let mut cmd = daemon_b.rig.command();
        cmd.args([
            "peer",
            "approve",
            &approval_code,
            "--profile",
            "read-only-display",
        ]);
        daemon_b.rig.run(cmd).await
    };
    assert!(
        approve.status.success(),
        "peer approve failed:\n{}",
        text_of(&approve)
    );

    // Complete on A. `peer complete` exits 0 even while the request is
    // still pending, so the install lines — not the exit status — are
    // the assertion.
    let complete = {
        let mut cmd = rig_a.command();
        cmd.args(["peer", "complete", &request_id, "--label", "peer-e2e-b"]);
        rig_a.run(cmd).await
    };
    let complete_text = text_of(&complete);
    assert!(
        complete.status.success(),
        "peer complete failed:\n{complete_text}"
    );
    assert!(
        complete_text.contains(":: client cert:"),
        "peer complete did not install an identity (still pending?):\n{complete_text}"
    );
    let peer_config = std::fs::read_to_string(rig_a.project.path().join("intendant.toml"))
        .expect("read side A's intendant.toml");
    assert!(
        peer_config.contains("[[peer]]") && peer_config.contains("pinned_fingerprints"),
        "completion did not persist the [[peer]] entry:\n{peer_config}"
    );

    // B recorded the inbound identity under the scoped profile — this
    // guards the assertions below against the alias/degradation traps
    // in profile handling (an accidental peer-root grant would make the
    // deny leg vacuous… by passing, not failing, so pin the profile).
    let identities = {
        let mut cmd = daemon_b.rig.command();
        cmd.args(["peer", "identities"]);
        daemon_b.rig.run(cmd).await
    };
    let identities_text = text_of(&identities);
    assert!(
        identities_text.contains("Approved")
            && identities_text.contains("profile=read-only-display"),
        "approved identity with the scoped profile not recorded on B:\n{identities_text}"
    );

    // Allowed: display list is display-view, within read-only-display.
    let allowed = ctl_peer(&rig_a, "peer-e2e-b", &["display", "list"]).await;
    let allowed_text = text_of(&allowed);
    assert!(
        allowed.status.success(),
        "peer-routed display list failed:\n{allowed_text}\ndaemon B log:\n{}",
        daemon_b.log_tail()
    );
    assert!(
        !allowed_text.contains("Permission denied"),
        "display view should be within read-only-display:\n{allowed_text}"
    );

    // Denied: cu actions is display-input, above the profile ceiling.
    let denied = ctl_peer(
        &rig_a,
        "peer-e2e-b",
        &["cu", "actions", "--actions", r#"[{"type":"screenshot"}]"#],
    )
    .await;
    let denied_text = text_of(&denied);
    assert!(
        denied_text.contains("Permission denied for tool 'execute_cu_actions'"),
        "display input should be denied under read-only-display:\n{denied_text}\ndaemon B log:\n{}",
        daemon_b.log_tail()
    );
    assert!(
        denied_text.contains("principal:peer-daemon:"),
        "denial should carry the peer-daemon principal:\n{denied_text}"
    );

    // Upgrade: a second ceremony under `peer-operator` completes the
    // gate matrix. `peer complete` upserts keyed by card_url, so side
    // A's single [[peer]] entry swaps in place to the operator label
    // and cert pair — B then resolves the newly presented cert to the
    // higher profile (both identities stay approved on B; the
    // presented cert decides).
    let request_op = {
        let mut cmd = rig_a.command();
        cmd.args([
            "peer",
            "request",
            &format!("https://127.0.0.1:{port_b}"),
            "--label",
            "peer-e2e-a-op",
            "--profile",
            "peer-operator",
        ]);
        rig_a.run(cmd).await
    };
    assert!(
        request_op.status.success(),
        "operator peer request failed:\n{}\ndaemon B log:\n{}",
        text_of(&request_op),
        daemon_b.log_tail()
    );
    let request_op_stdout = String::from_utf8_lossy(&request_op.stdout).into_owned();
    let request_op_id = line_suffix(&request_op_stdout, ":: request id: ");
    let approval_op_code = line_suffix(&request_op_stdout, ":: approval code: ");

    let approve_op = {
        let mut cmd = daemon_b.rig.command();
        cmd.args([
            "peer",
            "approve",
            &approval_op_code,
            "--profile",
            "peer-operator",
        ]);
        daemon_b.rig.run(cmd).await
    };
    assert!(
        approve_op.status.success(),
        "operator peer approve failed:\n{}",
        text_of(&approve_op)
    );

    let complete_op = {
        let mut cmd = rig_a.command();
        cmd.args([
            "peer",
            "complete",
            &request_op_id,
            "--label",
            "peer-e2e-b-op",
        ]);
        rig_a.run(cmd).await
    };
    let complete_op_text = text_of(&complete_op);
    assert!(
        complete_op.status.success(),
        "operator peer complete failed:\n{complete_op_text}"
    );
    assert!(
        complete_op_text.contains(":: client cert:"),
        "operator peer complete did not install an identity (still pending?):\n{complete_op_text}"
    );
    let peer_config = std::fs::read_to_string(rig_a.project.path().join("intendant.toml"))
        .expect("read side A's intendant.toml after upgrade");
    assert!(
        peer_config.contains("peer-e2e-b-op"),
        "upgrade did not relabel the [[peer]] entry:\n{peer_config}"
    );
    assert_eq!(
        peer_config.matches("[[peer]]").count(),
        1,
        "same-card_url completion must update in place, not duplicate:\n{peer_config}"
    );

    let identities = {
        let mut cmd = daemon_b.rig.command();
        cmd.args(["peer", "identities"]);
        daemon_b.rig.run(cmd).await
    };
    let identities_text = text_of(&identities);
    assert!(
        identities_text.contains("profile=peer-operator"),
        "operator identity not recorded on B:\n{identities_text}"
    );

    // Allowed input: the same tool the read-only principal was refused
    // now reaches its handler. `--args {"actions":[]}` deliberately
    // bypasses ctl's client-side validation (`cu actions` rejects empty
    // arrays locally), so the affirmative signal is the handler's own
    // "No actions provided" — emitted before any display-target
    // resolution, which is what makes the leg meaningful on a headless
    // rig: the gate opened, and only the (absent) display stops it.
    let allowed_input = ctl_peer(
        &rig_a,
        "peer-e2e-b-op",
        &[
            "tools",
            "call",
            "execute_cu_actions",
            "--args",
            r#"{"actions":[]}"#,
        ],
    )
    .await;
    let allowed_input_text = text_of(&allowed_input);
    assert!(
        !allowed_input_text.contains("Permission denied"),
        "display input should be allowed under peer-operator:\n{allowed_input_text}\ndaemon B log:\n{}",
        daemon_b.log_tail()
    );
    assert!(
        allowed_input_text.contains("No actions provided"),
        "the empty-actions probe should reach the tool handler:\n{allowed_input_text}\ndaemon B log:\n{}",
        daemon_b.log_tail()
    );

    let _ = daemon_b.child.kill().await;
}

/// A supervised session under the web gateway surfaces Ask-category
/// approvals on the dashboard instead of failing closed. The launch used
/// to hard-code `headless: true`, so a CreateSession'd session auto-denied
/// gated commands with "Approval required in headless mode" even though
/// the dispatch table routes Approve/Deny/Skip into the session's own
/// approval registry — and the spawn_sub_agent contract documents children
/// as having "their own approvals". Pin the whole loop: the approval event
/// arrives tagged with the session id, an `approve` over /ws releases it,
/// and the gated command really runs (the seeded marker file disappears).
#[tokio::test]
async fn supervised_session_surfaces_approvals_on_the_dashboard() {
    use futures_util::SinkExt;

    let rig = TestRig::new();
    let script = rig.write_script(&serde_json::json!({
        "profiles": [{
            "steps": [
                // `rmdir`, not `rm`: it is in the classifier's destructive
                // list AND exists on every platform this e2e runs on — a
                // POSIX utility and a cmd.exe built-in (`rm` is neither on
                // Windows, where the approved command then fails and the
                // marker survives; that exact miss ejected this test's PR
                // from the merge queue on the windows leg).
                { "content": "Deleting the marker.",
                  "tool_calls": [{ "name": "exec_command",
                                   "arguments": { "nonce": 1, "command": "rmdir approval-pin-marker" } }] },
                // No transcript expectation: a successful rmdir prints
                // nothing — the test's proof is the marker directory itself
                // disappearing from the session project.
                { "content": "Marker removed.",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "approval pin complete" } }] }
            ]
        }]
    }));
    // The session's project: seeded with the empty marker directory the
    // gated `rmdir` deletes.
    let session_project = tempfile::tempdir().expect("session project dir");
    let marker = session_project.path().join("approval-pin-marker");
    std::fs::create_dir(&marker).expect("seed marker dir");

    // Default autonomy (Medium): `rmdir` classifies destructive → Ask.
    let mut cmd = rig.command();
    cmd.env("INTENDANT_MOCK_SCRIPT", &script)
        .args(["--no-tls", "--web", "18941"]);
    let mut child = cmd.spawn().expect("spawn intendant daemon");
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stdout_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let _stderr_drain = drain_pipe_into(child.stderr.take().expect("stderr"), stderr_buf.clone());
    let _stdout_drain = drain_pipe_into(child.stdout.take().expect("stdout"), stdout_buf.clone());

    let stderr_so_far = wait_for_output(&stderr_buf, "Dashboard: http://", RUN_TIMEOUT).await;
    let port: u16 = stderr_so_far
        .lines()
        .find_map(|line| {
            let url = line.strip_prefix("Dashboard: http://")?;
            url.rsplit(':').next()?.trim().parse().ok()
        })
        .expect("parse the dashboard port from the startup line");

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    // The very first control message can race daemon startup (the gateway
    // accepts /ws a beat before the supervisor subscribes) — retry the
    // send until the approval surfaces. A duplicate create is harmless
    // here: we approve the first approval's session and stop it; the rig
    // kills the daemon on drop.
    let mut approval = None;
    for _ in 0..6 {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "action": "create_session",
                "task": "delete the pinned marker directory",
                "project_root": session_project.path().to_string_lossy(),
                "direct": true,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send create_session");
        approval = next_matching_ws_event(&mut ws, Duration::from_secs(30), |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("approval_required")
        })
        .await;
        if approval.is_some() {
            break;
        }
    }
    let approval = approval.unwrap_or_else(|| {
        panic!(
            "no approval_required from the supervised session (fail-closed regression?); daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });
    let session_id = approval
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("approval event must carry its session id, got {approval}"))
        .to_string();
    let approval_id = approval
        .get("id")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("approval event must carry an id, got {approval}"));

    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "approve",
            "session_id": session_id,
            "id": approval_id,
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send approve");

    // The released command really runs: the seeded marker disappears.
    let deadline = tokio::time::Instant::now() + RUN_TIMEOUT;
    while marker.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "approved rmdir never ran; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let stderr_ctx = stderr_buf.clone();
    complete_and_stop_session(&mut ws, &session_id, move || {
        stderr_ctx.lock().map(|b| b.clone()).unwrap_or_default()
    })
    .await;

    let _ = child.kill().await;
}

/// The native askHuman command reaches the dashboard question rail: a
/// supervised session's question arrives as a `user_question` event tagged
/// with the session id, the dashboard's `answer_question` resolves it, and
/// the answer text reaches the model as the tool result (pinned by the mock
/// script's transcript expectation on the next step).
#[tokio::test]
async fn supervised_session_ask_human_reaches_the_question_rail() {
    use futures_util::SinkExt;

    let rig = TestRig::new();
    let script = rig.write_script(&serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "I need to know the color before painting.",
                  "tool_calls": [{ "name": "ask_human",
                                   "arguments": { "nonce": 1, "question": "Which color should the widget be?" } }] },
                // The transcript gate proves the rail answer became the
                // askHuman tool result the model actually read.
                { "content": "Painting it cerulean.",
                  "expect_transcript_contains": "cerulean",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "question answered" } }] }
            ]
        }]
    }));
    let session_project = tempfile::tempdir().expect("session project dir");

    let mut cmd = rig.command();
    cmd.env("INTENDANT_MOCK_SCRIPT", &script)
        .args(["--no-tls", "--web", "18942"]);
    let mut child = cmd.spawn().expect("spawn intendant daemon");
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stdout_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let _stderr_drain = drain_pipe_into(child.stderr.take().expect("stderr"), stderr_buf.clone());
    let _stdout_drain = drain_pipe_into(child.stdout.take().expect("stdout"), stdout_buf.clone());

    let stderr_so_far = wait_for_output(&stderr_buf, "Dashboard: http://", RUN_TIMEOUT).await;
    let port: u16 = stderr_so_far
        .lines()
        .find_map(|line| {
            let url = line.strip_prefix("Dashboard: http://")?;
            url.rsplit(':').next()?.trim().parse().ok()
        })
        .expect("parse the dashboard port from the startup line");

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    // Same startup race + retry shape as the approvals e2e above.
    let mut question = None;
    for _ in 0..6 {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "action": "create_session",
                "task": "paint the widget the right color",
                "project_root": session_project.path().to_string_lossy(),
                "direct": true,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send create_session");
        question = next_matching_ws_event(&mut ws, Duration::from_secs(30), |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("user_question")
        })
        .await;
        if question.is_some() {
            break;
        }
    }
    let question = question.unwrap_or_else(|| {
        panic!(
            "no user_question from the supervised session's askHuman (file-park regression?); daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });
    let session_id = question
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("user_question must carry its session id, got {question}"))
        .to_string();
    let question_id = question
        .get("id")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("user_question must carry an id, got {question}"));
    let question_text = question
        .pointer("/questions/0/question")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("user_question must carry the question text, got {question}"));
    assert_eq!(question_text, "Which color should the widget be?");

    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "answer_question",
            "session_id": session_id,
            "id": question_id,
            "answers": { question_text: "cerulean" },
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send answer_question");

    // signal_done only fires after the transcript gate saw "cerulean" —
    // reaching a completed round IS the proof the answer landed.
    let stderr_ctx = stderr_buf.clone();
    complete_and_stop_session(&mut ws, &session_id, move || {
        stderr_ctx.lock().map(|b| b.clone()).unwrap_or_default()
    })
    .await;

    let _ = child.kill().await;
}

/// Shared assertions for the message-search wire contract (plan §4/§11):
/// the intendant extractor consumes exactly these `session.jsonl` shapes,
/// and its unit tests use fixtures of them — these helpers pin the shapes
/// to the real binary so schema drift fails here instead of silently
/// unindexing. Returns the parsed `conversation_message` rows.
fn canonical_message_rows(rows: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    let messages: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|row| row.get("event").and_then(|v| v.as_str()) == Some("conversation_message"))
        .collect();
    for row in &messages {
        assert!(
            msg_field(row, "message_id")
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "conversation_message without a message_id: {row}"
        );
        assert!(
            msg_field(row, "message_seq").as_u64().is_some(),
            "conversation_message without a message_seq: {row}"
        );
        assert!(
            row.get("ts_ms").and_then(|v| v.as_i64()).is_some(),
            "conversation_message without ts_ms: {row}"
        );
    }
    let seqs: Vec<u64> = messages
        .iter()
        .filter_map(|row| msg_field(row, "message_seq").as_u64())
        .collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        seqs.len(),
        sorted.len(),
        "message seqs must be unique: {seqs:?}"
    );
    messages
}

fn msg_field(row: &serde_json::Value, name: &str) -> serde_json::Value {
    row.pointer(&format!("/data/{name}"))
        .cloned()
        .unwrap_or_default()
}

/// The user-side row with exactly this RAW text (no attachment preludes,
/// no tool-result envelopes, no delivery wrappers).
fn user_message_row<'rows>(
    messages: &[&'rows serde_json::Value],
    text: &str,
) -> &'rows serde_json::Value {
    messages
        .iter()
        .find(|row| {
            msg_field(row, "role").as_str() == Some("user")
                && msg_field(row, "text").as_str() == Some(text)
        })
        .unwrap_or_else(|| {
            panic!(
                "no user conversation_message with raw text {text:?}; rows:\n{}",
                messages
                    .iter()
                    .map(|row| row.to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
}

/// Resolve an assistant row's sidecar span byte-for-byte, exactly like
/// the extractor does.
fn resolve_assistant_span(log_dir: &std::path::Path, row: &serde_json::Value) -> String {
    let file = row
        .get("file")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("assistant row without a sidecar file: {row}"));
    let offset = msg_field(row, "model_offset")
        .as_u64()
        .unwrap_or_else(|| panic!("assistant row without model_offset: {row}"));
    let len = msg_field(row, "model_bytes")
        .as_u64()
        .unwrap_or_else(|| panic!("assistant row without model_bytes: {row}"));
    let bytes = std::fs::read(log_dir.join(file)).expect("read sidecar");
    String::from_utf8_lossy(&bytes[offset as usize..(offset + len) as usize]).into_owned()
}

fn assert_assistant_spans_resolve(
    log_dir: &std::path::Path,
    messages: &[&serde_json::Value],
    expected: &[&str],
) {
    let assistant_texts: Vec<String> = messages
        .iter()
        .filter(|row| msg_field(row, "role").as_str() == Some("assistant"))
        .map(|row| resolve_assistant_span(log_dir, row))
        .collect();
    for text in expected {
        assert!(
            assistant_texts.iter().any(|resolved| resolved == text),
            "no assistant span resolved to {text:?}; got {assistant_texts:?}"
        );
    }
}

fn parsed_session_rows(log_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let log = std::fs::read_to_string(log_dir.join("session.jsonl")).expect("read session.jsonl");
    log.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Message-lane wire contract, supervised-session half: a create_session
/// child run through task → askHuman answer → between-rounds steer writes
/// canonical `conversation_message` rows with RAW user text (provenances
/// `task` / `ask_human_answer` / `steer`), the askHuman projection's
/// `ref_seq`, and assistant sidecar spans that resolve byte-for-byte. The
/// steer rides the supervisor's parked-delivery fallback (`route_steer`
/// queues an id-carrying steer as a follow-up when no active turn acks
/// it) — the shape the dashboard uses. Conversation rollback deliberately
/// lives in the OTHER half: a child parks in `run_round_loop`'s follow-up
/// drain, which never sees `ConversationRollbackRequested`.
#[tokio::test]
async fn supervised_session_writes_task_ask_human_and_steer_message_rows() {
    use futures_util::SinkExt;

    let rig = TestRig::new();
    let script = rig.write_script(&serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "Need input before charting.",
                  "tool_calls": [{ "name": "ask_human",
                                   "arguments": { "nonce": 1, "question": "Which payload?" } }] },
                { "content": "Round one done, payload chosen.",
                  "expect_transcript_contains": "the emerald payload",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "round one" } }] },
                { "content": "Steered round two.",
                  "expect_transcript_contains": "steer toward the vault",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "round two" } }] }
            ]
        }]
    }));
    let session_project = tempfile::tempdir().expect("session project dir");

    let mut cmd = rig.command();
    cmd.env("INTENDANT_MOCK_SCRIPT", &script)
        .args(["--no-tls", "--web", "0"]);
    let mut child = cmd.spawn().expect("spawn intendant daemon");
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stdout_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let _stderr_drain = drain_pipe_into(child.stderr.take().expect("stderr"), stderr_buf.clone());
    let _stdout_drain = drain_pipe_into(child.stdout.take().expect("stdout"), stdout_buf.clone());

    let stderr_so_far = wait_for_output(&stderr_buf, "Dashboard: http://", RUN_TIMEOUT).await;
    let port: u16 = stderr_so_far
        .lines()
        .find_map(|line| {
            let url = line.strip_prefix("Dashboard: http://")?;
            url.rsplit(':').next()?.trim().parse().ok()
        })
        .expect("parse the dashboard port from the startup line");

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    // Same startup race + retry shape as the askHuman e2e above.
    let mut question = None;
    for _ in 0..6 {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "action": "create_session",
                "task": "chart the payload course",
                "project_root": session_project.path().to_string_lossy(),
                "direct": true,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send create_session");
        question = next_matching_ws_event(&mut ws, Duration::from_secs(30), |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("user_question")
        })
        .await;
        if question.is_some() {
            break;
        }
    }
    let question = question.unwrap_or_else(|| {
        panic!(
            "no user_question from the session's askHuman; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });
    let session_id = question
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("user_question carries its session id")
        .to_string();
    let question_id = question
        .get("id")
        .and_then(|v| v.as_u64())
        .expect("user_question carries an id");
    let question_text = question
        .pointer("/questions/0/question")
        .and_then(|v| v.as_str())
        .expect("user_question carries the question text")
        .to_string();

    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "answer_question",
            "session_id": session_id,
            "id": question_id,
            "answers": { question_text: "the emerald payload" },
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send answer_question");

    // Round 1 completes (the transcript gate proved the answer landed).
    next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("round_complete")
            && json.get("session_id").and_then(|v| v.as_str()) == Some(session_id.as_str())
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "round 1 never completed; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });

    // A between-rounds steer. The id is load-bearing: only an id-carrying
    // steer arms route_steer's no-active-turn fallback that delivers to a
    // parked session, and the dashboard always sends one.
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "steer",
            "session_id": session_id,
            "id": "steer-e2e-1",
            "text": "steer toward the vault",
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send steer");
    next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("round_complete")
            && json.get("session_id").and_then(|v| v.as_str()) == Some(session_id.as_str())
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "steered round 2 never completed; daemon stderr:\n{}\nsession logs:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default(),
            tail(&rig.session_logs(), 6000)
        )
    });

    // Both rounds are already consumed above — only the stop half of the
    // usual completion ritual remains (the session parks by design).
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({ "action": "stop_session", "session_id": session_id })
            .to_string()
            .into(),
    ))
    .await
    .expect("send stop_session");
    next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("session_ended")
            && json.get("session_id").and_then(|v| v.as_str()) == Some(session_id.as_str())
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "stopped session never ended; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });
    let _ = child.kill().await;

    // ---- The wire contract the extractor consumes ----
    let log_dir = rig
        .home
        .path()
        .join(".intendant")
        .join("logs")
        .join(&session_id);
    let rows = parsed_session_rows(&log_dir);
    let messages = canonical_message_rows(&rows);

    let task_row = user_message_row(&messages, "chart the payload course");
    assert_eq!(msg_field(task_row, "provenance").as_str(), Some("task"));
    let answer_row = user_message_row(&messages, "the emerald payload");
    assert_eq!(
        msg_field(answer_row, "provenance").as_str(),
        Some("ask_human_answer")
    );
    assert!(
        msg_field(answer_row, "ref_seq").as_u64().is_some(),
        "the native-tool askHuman projection must reference its tool result: {answer_row}"
    );
    let steer_row = user_message_row(&messages, "steer toward the vault");
    assert_eq!(msg_field(steer_row, "provenance").as_str(), Some("steer"));

    assert_assistant_spans_resolve(
        &log_dir,
        &messages,
        &[
            "Need input before charting.",
            "Round one done, payload chosen.",
            "Steered round two.",
        ],
    );
}

/// A steer WITHOUT an id delivered to a parked session must still start
/// the next round (regression: nothing consumed SteerRequested for a
/// parked session — the supervisor's queue-as-follow-up fallback only
/// arms for id-carrying steers, and the round watcher dies with its
/// round — so id-less steers, the API/MCP default, vanished silently;
/// the parked drain now picks them up directly). Also covers the
/// round-boundary acceptance race: a steer arriving as the round dies
/// may be "accepted" by the doomed watcher, and the drain must deliver
/// it anyway.
#[tokio::test]
async fn parked_session_delivers_an_id_less_steer() {
    use futures_util::SinkExt;

    let rig = TestRig::new();
    let script = rig.write_script(&serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "Round one answer.",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "round one" } }] },
                { "content": "Steered onward.",
                  "expect_transcript_contains": "quiet steer text",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "round two" } }] }
            ]
        }]
    }));
    let session_project = tempfile::tempdir().expect("session project dir");

    let mut cmd = rig.command();
    cmd.env("INTENDANT_MOCK_SCRIPT", &script)
        .args(["--no-tls", "--web", "0"]);
    let mut child = cmd.spawn().expect("spawn intendant daemon");
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stdout_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let _stderr_drain = drain_pipe_into(child.stderr.take().expect("stderr"), stderr_buf.clone());
    let _stdout_drain = drain_pipe_into(child.stdout.take().expect("stdout"), stdout_buf.clone());

    let stderr_so_far = wait_for_output(&stderr_buf, "Dashboard: http://", RUN_TIMEOUT).await;
    let port: u16 = stderr_so_far
        .lines()
        .find_map(|line| {
            let url = line.strip_prefix("Dashboard: http://")?;
            url.rsplit(':').next()?.trim().parse().ok()
        })
        .expect("parse the dashboard port from the startup line");

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    // Same startup race + retry shape as the sibling tests.
    let mut completed = None;
    for _ in 0..6 {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "action": "create_session",
                "task": "run the first round",
                "project_root": session_project.path().to_string_lossy(),
                "direct": true,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send create_session");
        completed = next_matching_ws_event(&mut ws, Duration::from_secs(30), |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("round_complete")
        })
        .await;
        if completed.is_some() {
            break;
        }
    }
    let completed = completed.unwrap_or_else(|| {
        panic!(
            "round 1 never completed; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });
    let session_id = completed
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("round_complete carries its session id")
        .to_string();

    // The id-less steer (API/MCP default shape) to the parked session.
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "steer",
            "session_id": session_id,
            "text": "quiet steer text",
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send steer");
    next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("round_complete")
            && json.get("session_id").and_then(|v| v.as_str()) == Some(session_id.as_str())
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "id-less steered round never completed; daemon stderr:\n{}\nsession logs:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default(),
            tail(&rig.session_logs(), 6000)
        )
    });
    let _ = child.kill().await;

    // The steer text entered the message lane with steer provenance.
    let log_dir = rig
        .home
        .path()
        .join(".intendant")
        .join("logs")
        .join(&session_id);
    let rows = parsed_session_rows(&log_dir);
    let messages = canonical_message_rows(&rows);
    let steer_row = user_message_row(&messages, "quiet steer text");
    assert_eq!(msg_field(steer_row, "provenance").as_str(), Some("steer"));
}

/// Targeted conversation rollback: a SUPERVISED session's parked drain
/// executes `POST /api/session/current/rollback { session_id, round_id,
/// revert_conversation: true, revert_files: false }` — previously the
/// signal was only handled by the headless boot-session loop, so the
/// pure-daemon rollback rail was dead (the gap the B2 message-lane e2e
/// documented). Two rounds run, the conversation rolls back to round 1,
/// and a third round proves the session keeps working on the truncated
/// conversation; the session log carries the same `conversation_rewound`
/// cut the message-search extractor derives supersession from, and the
/// completion event carries the session id.
#[tokio::test]
async fn supervised_session_rolls_back_conversation_to_a_round() {
    use futures_util::SinkExt;

    let rig = TestRig::new();
    let script = rig.write_script(&serde_json::json!({
        "profiles": [{
            "steps": [
                { "content": "Round one done.",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "round one" } }] },
                { "content": "Round two done.",
                  "expect_transcript_contains": "start round two",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "round two" } }] },
                { "content": "Post-rollback round done.",
                  "expect_transcript_contains": "after the rewind",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "round three" } }] }
            ]
        }]
    }));
    let session_project = tempfile::tempdir().expect("session project dir");

    let mut cmd = rig.command();
    cmd.env("INTENDANT_MOCK_SCRIPT", &script)
        .args(["--no-tls", "--web", "0"]);
    let mut child = cmd.spawn().expect("spawn intendant daemon");
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stdout_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let _stderr_drain = drain_pipe_into(child.stderr.take().expect("stderr"), stderr_buf.clone());
    let _stdout_drain = drain_pipe_into(child.stdout.take().expect("stdout"), stdout_buf.clone());

    let stderr_so_far = wait_for_output(&stderr_buf, "Dashboard: http://", RUN_TIMEOUT).await;
    let port: u16 = stderr_so_far
        .lines()
        .find_map(|line| {
            let url = line.strip_prefix("Dashboard: http://")?;
            url.rsplit(':').next()?.trim().parse().ok()
        })
        .expect("parse the dashboard port from the startup line");

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    // Same startup race + retry shape as the sibling tests.
    let mut completed = None;
    for _ in 0..6 {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "action": "create_session",
                "task": "run round one",
                "project_root": session_project.path().to_string_lossy(),
                "direct": true,
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send create_session");
        completed = next_matching_ws_event(&mut ws, Duration::from_secs(30), |json| {
            json.get("event").and_then(|v| v.as_str()) == Some("round_complete")
        })
        .await;
        if completed.is_some() {
            break;
        }
    }
    let completed = completed.unwrap_or_else(|| {
        panic!(
            "round 1 never completed; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });
    let session_id = completed
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("round_complete carries its session id")
        .to_string();

    // Round 2 via a follow-up steer (id-carrying, the delivered path).
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "steer",
            "session_id": session_id,
            "id": "rollback-e2e-steer-1",
            "text": "start round two",
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send round-2 steer");
    next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("round_complete")
            && json.get("session_id").and_then(|v| v.as_str()) == Some(session_id.as_str())
            && json.get("round").and_then(|v| v.as_u64()) == Some(2)
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "round 2 never completed; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });

    // Targeted conversation rollback to round 1 through the route.
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/api/session/current/rollback"
        ))
        .json(&serde_json::json!({
            "session_id": session_id,
            "round_id": 1,
            "revert_conversation": true,
            "revert_files": false,
        }))
        .send()
        .await
        .expect("send targeted rollback");
    assert!(
        response.status().is_success(),
        "targeted rollback rejected: {:?}",
        response.text().await
    );
    let rolled = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("conversation_rolled_back")
            && json.get("session_id").and_then(|v| v.as_str()) == Some(session_id.as_str())
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "targeted rollback never completed; daemon stderr:\n{}\nsession logs:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default(),
            tail(&rig.session_logs(), 6000)
        )
    });
    assert!(
        rolled
            .get("turns_removed")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            > 0,
        "rollback must remove round 2's messages: {rolled}"
    );

    // The session still works: round 3 on the truncated conversation.
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "steer",
            "session_id": session_id,
            "id": "rollback-e2e-steer-2",
            "text": "after the rewind",
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send post-rollback steer");
    next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("round_complete")
            && json.get("session_id").and_then(|v| v.as_str()) == Some(session_id.as_str())
            && json.get("round").and_then(|v| v.as_u64()) == Some(2)
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "post-rollback round never completed; daemon stderr:\n{}\nsession logs:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default(),
            tail(&rig.session_logs(), 6000)
        )
    });
    let _ = child.kill().await;

    // The wire contract: the cut partitions round 2, round 3's steer rides
    // after it with a fresh seq.
    let log_dir = rig
        .home
        .path()
        .join(".intendant")
        .join("logs")
        .join(&session_id);
    let rows = parsed_session_rows(&log_dir);
    let messages = canonical_message_rows(&rows);
    let round2_seq = msg_field(
        user_message_row(&messages, "start round two"),
        "message_seq",
    )
    .as_u64()
    .unwrap();
    let round3_seq = msg_field(
        user_message_row(&messages, "after the rewind"),
        "message_seq",
    )
    .as_u64()
    .unwrap();
    let rewound = rows
        .iter()
        .find(|row| row.get("event").and_then(|v| v.as_str()) == Some("conversation_rewound"))
        .expect("conversation_rewound row exists");
    let cut_after_seq = msg_field(rewound, "cut_after_seq").as_u64().unwrap();
    assert!(
        cut_after_seq < round2_seq && round2_seq < round3_seq,
        "cut {cut_after_seq} must supersede round 2 (seq {round2_seq}) and precede          round 3 (seq {round3_seq})"
    );
}

/// Message-lane wire contract, rollback half: the HEADLESS shape (task on
/// argv) is the one execution shape whose outer loop (run_with_presence)
/// handles `ConversationRollbackRequested`, so the round-rollback rail —
/// `GET /api/session/current/history` + `POST
/// /api/session/current/rollback` — runs here. Two rounds (boot task with
/// an askHuman answer, then a second task into the same primary session),
/// then a conversation-only revert to round 1 must write a
/// `conversation_rewound` row whose cut keeps round 1 and supersedes
/// round 2 — exactly the SeqCut semantics the extractor derives
/// supersession from.
#[tokio::test]
async fn headless_rollback_writes_a_conversation_rewound_cut() {
    use futures_util::SinkExt;

    let rig = TestRig::new();
    let script = rig.write_script(&serde_json::json!({
        "profiles": [{
            "steps": [
                // The boot task starts before any websocket can connect,
                // and rail events do not replay to late joiners — the
                // scripted think-time holds the question back until this
                // test's connection is up.
                { "content": "Need input before charting.",
                  "delay_ms": 8_000,
                  "tool_calls": [{ "name": "ask_human",
                                   "arguments": { "nonce": 1, "question": "Which payload?" } }] },
                { "content": "Round one done, payload chosen.",
                  "expect_transcript_contains": "the emerald payload",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "round one" } }] },
                { "content": "Second round response.",
                  "expect_transcript_contains": "chase the decoy",
                  "tool_calls": [{ "name": "signal_done",
                                   "arguments": { "message": "round two" } }] }
            ]
        }]
    }));
    // The rollback rail needs the file watcher's round history, which a
    // projectless run turns off — mark the rig project.
    std::fs::write(rig.project.path().join("intendant.toml"), "").expect("project marker");

    let mut cmd = rig.command();
    cmd.env("INTENDANT_MOCK_SCRIPT", &script).args([
        "--no-tls",
        "--web",
        "0",
        "--direct",
        "chart the payload course",
    ]);
    let mut child = cmd.spawn().expect("spawn intendant with a boot task");
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stdout_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let _stderr_drain = drain_pipe_into(child.stderr.take().expect("stderr"), stderr_buf.clone());
    let _stdout_drain = drain_pipe_into(child.stdout.take().expect("stdout"), stdout_buf.clone());

    let stderr_so_far = wait_for_output(&stderr_buf, "Dashboard: http://", RUN_TIMEOUT).await;
    let port: u16 = stderr_so_far
        .lines()
        .find_map(|line| {
            let url = line.strip_prefix("Dashboard: http://")?;
            url.rsplit(':').next()?.trim().parse().ok()
        })
        .expect("parse the dashboard port from the startup line");
    // The primary (boot) session announces its id and log dir at startup.
    let primary_id = stderr_so_far
        .lines()
        .find_map(|line| line.strip_prefix("Session ID: "))
        .expect("parse the primary session id from the startup lines")
        .trim()
        .to_string();

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("connect /ws");

    let question = next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("user_question")
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "no user_question from the boot task's askHuman; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });
    let session_id = question
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or(&primary_id)
        .to_string();
    let question_id = question
        .get("id")
        .and_then(|v| v.as_u64())
        .expect("user_question carries an id");
    let question_text = question
        .pointer("/questions/0/question")
        .and_then(|v| v.as_str())
        .expect("user_question carries the question text")
        .to_string();

    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "answer_question",
            "session_id": session_id,
            "id": question_id,
            "answers": { question_text: "the emerald payload" },
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send answer_question");

    let round_complete_for_primary = |json: &serde_json::Value, session_id: &str| {
        json.get("event").and_then(|v| v.as_str()) == Some("round_complete")
            && json
                .get("session_id")
                .and_then(|v| v.as_str())
                .is_none_or(|id| id == session_id)
    };
    next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        round_complete_for_primary(json, &session_id)
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "round 1 never completed; daemon stderr:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
        )
    });

    // Round 2: a second task into the SAME primary session (the dispatcher
    // claims start_task for the primary id; the supervisor would claim an
    // id-less one as a fresh child session).
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({
            "action": "start_task",
            "session_id": primary_id,
            "task": "chase the decoy",
            "direct": true,
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send start_task");
    next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        round_complete_for_primary(json, &session_id)
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "round 2 never completed; daemon stderr:\n{}\nsession logs:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default(),
            tail(&rig.session_logs(), 6000)
        )
    });

    // Conversation-only revert to the first recorded round, through the
    // dashboard's route (the production `conversation_rewound` path).
    // Idle-state and history-visibility race the round_complete event, so
    // poll both.
    let client = reqwest::Client::new();
    let stderr_ctx = stderr_buf.clone();
    let first_round_id = poll_until(
        "the session history to list its first round",
        Duration::from_secs(30),
        || {
            let client = client.clone();
            async move {
                let history = http_get_json(
                    &client,
                    &format!("http://127.0.0.1:{port}/api/session/current/history"),
                )
                .await?;
                history
                    .get("rounds")?
                    .as_array()?
                    .first()?
                    .get("id")?
                    .as_u64()
            }
        },
        move || stderr_ctx.lock().map(|b| b.clone()).unwrap_or_default(),
    )
    .await;
    let stderr_ctx = stderr_buf.clone();
    poll_until(
        "the rollback route to accept the revert",
        Duration::from_secs(30),
        || {
            let client = client.clone();
            async move {
                let response = client
                    .post(format!(
                        "http://127.0.0.1:{port}/api/session/current/rollback"
                    ))
                    .json(&serde_json::json!({
                        "round_id": first_round_id,
                        "revert_conversation": true,
                        "revert_files": false,
                    }))
                    .send()
                    .await
                    .ok()?;
                response.status().is_success().then_some(())
            }
        },
        move || stderr_ctx.lock().map(|b| b.clone()).unwrap_or_default(),
    )
    .await;
    next_matching_ws_event(&mut ws, RUN_TIMEOUT, |json| {
        json.get("event").and_then(|v| v.as_str()) == Some("conversation_rolled_back")
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "conversation rollback never completed; daemon stderr:\n{}\nsession logs:\n{}",
            stderr_buf.lock().map(|b| b.clone()).unwrap_or_default(),
            tail(&rig.session_logs(), 6000)
        )
    });

    // Every event row is flushed per write; killing the daemon loses
    // nothing, and the primary session is not user-stoppable anyway.
    let _ = child.kill().await;

    // ---- The wire contract the extractor consumes ----
    let log_dir = rig
        .home
        .path()
        .join(".intendant")
        .join("logs")
        .join(&session_id);
    let rows = parsed_session_rows(&log_dir);
    let messages = canonical_message_rows(&rows);

    let task_row = user_message_row(&messages, "chart the payload course");
    assert_eq!(msg_field(task_row, "provenance").as_str(), Some("task"));
    let answer_row = user_message_row(&messages, "the emerald payload");
    assert_eq!(
        msg_field(answer_row, "provenance").as_str(),
        Some("ask_human_answer")
    );
    assert!(
        msg_field(answer_row, "ref_seq").as_u64().is_some(),
        "the native-tool askHuman projection must reference its tool result: {answer_row}"
    );
    let round2_row = user_message_row(&messages, "chase the decoy");
    assert_assistant_spans_resolve(
        &log_dir,
        &messages,
        &[
            "Need input before charting.",
            "Round one done, payload chosen.",
            "Second round response.",
        ],
    );

    // The rollback cut partitions round 2 strictly after the cut, with
    // all of round 1 surviving — the SeqCut semantics the extractor
    // derives supersession from.
    let rewound = rows
        .iter()
        .find(|row| row.get("event").and_then(|v| v.as_str()) == Some("conversation_rewound"))
        .unwrap_or_else(|| {
            panic!(
                "no conversation_rewound row; session log:\n{}",
                tail(&rig.session_logs(), 6000)
            )
        });
    assert_eq!(msg_field(rewound, "kind").as_str(), Some("tail_rollback"));
    assert!(
        msg_field(rewound, "superseded_at_ms").as_i64().is_some(),
        "conversation_rewound must stamp superseded_at_ms: {rewound}"
    );
    let cut_after_seq = msg_field(rewound, "cut_after_seq")
        .as_u64()
        .expect("conversation_rewound carries cut_after_seq");
    let round1_max = ["chart the payload course", "the emerald payload"]
        .iter()
        .map(|text| {
            msg_field(user_message_row(&messages, text), "message_seq")
                .as_u64()
                .unwrap()
        })
        .max()
        .unwrap();
    let round2_seq = msg_field(round2_row, "message_seq").as_u64().unwrap();
    assert!(
        round1_max <= cut_after_seq && cut_after_seq < round2_seq,
        "cut_after_seq {cut_after_seq} must keep round 1 (max seq {round1_max}) and \
         supersede round 2 (seq {round2_seq})"
    );
}

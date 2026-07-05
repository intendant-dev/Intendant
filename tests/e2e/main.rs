//! Headless end-to-end tests: spawn the real `intendant` binary and drive
//! the production stack — CLI parsing, provider selection, the agent loop,
//! tool dispatch, the sandboxed `intendant-runtime` subprocess, and session
//! logging — with no API keys, no network, and no display.
//!
//! The model is the scripted mock provider (`PROVIDER=mock` +
//! `INTENDANT_MOCK_SCRIPT`, see `src/bin/caller/provider_mock.rs`): each
//! test writes a JSON script of responses/tool calls, runs the binary in an
//! isolated HOME + project dir, and asserts on the exit status, the session
//! log, and on-disk effects. Scripts fail loudly (unmet expectation or
//! exhausted steps error out), so a hung or drifted loop fails the test
//! instead of green-looping.
//!
//! Run: cargo test --test e2e -- --nocapture

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Generous ceiling for one binary run: a debug build on a cold CI runner
/// spawns the runtime several times but each chat turn is local.
const RUN_TIMEOUT: Duration = Duration::from_secs(180);

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

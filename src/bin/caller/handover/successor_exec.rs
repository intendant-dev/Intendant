//! The successor-exec lane (the update-channels gate's deferred design
//! question, RULED YES 2026-07-31): a CLI-launched, UNSUPERVISED daemon
//! may spawn its own successor — on the owner's explicit update-panel
//! click, and only then. The ruling's five bindings ARE this module's
//! contract:
//!
//! 1. **Explicit-click-only** — the spawn happens only on the owner's
//!    panel click (`POST /api/daemon/successor-exec`); never
//!    automatically, never on a schedule, never as a side effect of
//!    produce (the produce lane still only lands an artifact on disk).
//! 2. **Supervisor-absent-only** — only while no live app supervisor is
//!    attached (`app_supervised() == false`); supervised daemons keep
//!    the swap-request relay (`/api/daemon/update-swap`) unchanged.
//! 3. **Ordering** — spawn-secondary → readiness → drain: the successor
//!    boots as a plain secondary (never `--takeover`; it must not race
//!    the incumbent for the lease at boot), the incumbent confirms it is
//!    ready, and only then drains toward it. A successor that never
//!    becomes ready is terminated (it acquired nothing and drained
//!    nothing) and the running daemon is left exactly as it was.
//! 4. **Trust class unchanged** — the same loopback/own-origin
//!    owner-grade posture as the other update rows, and NO tunnel twin:
//!    remote surfaces observe through the handover status block; they
//!    cannot click a successor-exec onto the box.
//! 5. **No copyable-command fallback** — the ruled lane is the spawn; a
//!    copy-the-takeover-command lane is deliberately not built.
//!
//! Plus the launch-before-replace reinforcement (the 2026-07-31
//! owner-hit specimen: a supervisor spawned "whatever the bundle held"
//! while the owner's fresh build was still compiling, and the takeover
//! was silently build-neutral): the exec target is pinned by PATH AND
//! HASH. The click carries the commit the surface offered
//! (`expected_git_sha`); the lane re-probes the on-disk target at click
//! time and refuses on offered≠target (the artifact changed under the
//! button) and on offered==running (the swap would not change builds);
//! the spawned successor's registered build is re-checked before the
//! drain; and after the takeover the new lease holder's reported build
//! is compared against the offered build, with the verdict surfaced to
//! the owner either way.
//!
//! Composition, not duplication: detection and the artifact stay the
//! update watch's (`watched_binary_path` + `run_version_probe`);
//! producing stays the update lane's; the drain is the ruled HS3
//! machinery (`request_drain`); the successor acquires through the
//! ordinary secondary poll (fast while the boot is young — see
//! `HandoverRuntime::secondary_poll_interval`). This module only
//! sequences them and holds the honest state the panel renders.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// Bounded log tail for the status payload (each line also reaches the
/// daemon log as it happens) — the update lane's honesty shape.
const LOG_TAIL_LINES: usize = 40;
const LOG_LINE_CAP: usize = 400;

/// Readiness budget: presence registration + gateway answer. The app
/// supervisor's swap gives its successor ~15 s; a CLI box may be cold —
/// give it 60 s before the spawn is declared failed and reaped.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const READY_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Post-drain acquisition watch: the successor's young-boot fast poll
/// converges in ~1 s; past this bound the flow reports honestly and
/// leaves the story to the Q4 successor watch (drain is one-way — there
/// is nothing to roll back).
const ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Grace for a SIGTERM'd failed successor before the hard kill.
const TERMINATE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The pure offered-vs-target verdict (the specimen's pin): the exec
/// target must be exactly the build the click offered, and an offered
/// build equal to the running one is refused as build-neutral instead of
/// performing a swap that changes nothing.
pub(crate) fn exec_target_verdict(
    probed_sha: &str,
    expected_sha: &str,
    running_sha: &str,
) -> Result<(), String> {
    if probed_sha != expected_sha {
        return Err(format!(
            "the binary on disk is commit {probed_sha}, but this click offered commit \
             {expected_sha} — the artifact changed since the panel rendered. Nothing was \
             spawned; re-check and click again."
        ));
    }
    if probed_sha == running_sha {
        return Err(format!(
            "the binary on disk is the RUNNING build (commit {running_sha}) — a hand-off \
             would not change builds. Nothing was spawned."
        ));
    }
    Ok(())
}

/// The successor once its presence registered: everything later phases
/// key on.
#[derive(Debug, Clone)]
struct SuccessorFacts {
    boot_id: String,
    port: u16,
    pid: u32,
    git_sha: String,
}

#[derive(Debug)]
struct ExecState {
    phase: String,
    started_ms: u64,
    offered_sha: String,
    requested_by: Option<String>,
    child_pid: Option<u32>,
    successor: Option<SuccessorFacts>,
    log: VecDeque<String>,
    /// `Some` = finished; `Ok` carries the success detail.
    outcome: Option<Result<String, String>>,
    /// The post-takeover verdict: `Some(true)` = the new lease holder
    /// reports exactly the offered build; `Some(false)` = it does not
    /// (surfaced loudly); `None` = not (yet) verifiable.
    build_verified: Option<bool>,
}

/// The lane singleton, installed on the [`super::HandoverRuntime`] at
/// wiring (gateway shapes only — without a web port there is no panel
/// to click). Holds the exec target and the successor's replayed
/// daemon-shaping flags; the flow state rides `status_json()` as the
/// top-level `successor_exec` block.
pub(crate) struct SuccessorExecLane {
    runtime: Weak<super::HandoverRuntime>,
    exe_path: PathBuf,
    /// Standing daemon-shaping flags replayed onto the successor
    /// (`CliFlags::successor_replay`): the owner's bind/TLS/autonomy
    /// posture must not silently reset to defaults. One-shot argv (the
    /// task, `--takeover`, `--continue`, the old `--web` port) is never
    /// here by construction.
    replay_args: Vec<String>,
    state: Mutex<Option<ExecState>>,
    in_flight: AtomicBool,
}

impl SuccessorExecLane {
    fn new(runtime: &Arc<super::HandoverRuntime>, exe_path: PathBuf, replay_args: Vec<String>) -> Self {
        SuccessorExecLane {
            runtime: Arc::downgrade(runtime),
            exe_path,
            replay_args,
            state: Mutex::new(None),
            in_flight: AtomicBool::new(false),
        }
    }

    /// The `successor_exec` block on the handover status payload. Always
    /// present once the lane is wired (`available: true` is the panel's
    /// render gate); the last flow's state persists until the next click
    /// supersedes it.
    pub(crate) fn status_block(&self) -> serde_json::Value {
        let mut block = serde_json::json!({
            "available": true,
            "exec_path": self.exe_path.display().to_string(),
        });
        let obj = block.as_object_mut().expect("literal object");
        if let Ok(state) = self.state.lock() {
            if let Some(state) = state.as_ref() {
                obj.insert("phase".into(), state.phase.clone().into());
                obj.insert("started_ms".into(), state.started_ms.into());
                obj.insert("offered_sha".into(), state.offered_sha.clone().into());
                obj.insert(
                    "in_flight".into(),
                    state.outcome.is_none().into(),
                );
                if let Some(requested_by) = &state.requested_by {
                    obj.insert("requested_by".into(), requested_by.clone().into());
                }
                if let Some(pid) = state.child_pid {
                    obj.insert("child_pid".into(), pid.into());
                }
                if let Some(successor) = &state.successor {
                    obj.insert(
                        "successor".into(),
                        serde_json::json!({
                            "boot_id": successor.boot_id,
                            "port": successor.port,
                            "pid": successor.pid,
                            "git_sha": successor.git_sha,
                        }),
                    );
                }
                if let Some(verified) = state.build_verified {
                    obj.insert("build_verified".into(), verified.into());
                }
                match &state.outcome {
                    Some(Ok(detail)) => {
                        obj.insert("ok".into(), true.into());
                        obj.insert("detail".into(), detail.clone().into());
                    }
                    Some(Err(error)) => {
                        obj.insert("ok".into(), false.into());
                        obj.insert("error".into(), error.clone().into());
                    }
                    None => {}
                }
                obj.insert(
                    "log_tail".into(),
                    state.log.iter().cloned().collect::<Vec<_>>().into(),
                );
            }
        }
        block
    }

    fn log(&self, line: impl Into<String>) {
        let mut line = line.into();
        if line.len() > LOG_LINE_CAP {
            let mut cut = LOG_LINE_CAP;
            while cut > 0 && !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
        }
        eprintln!("[successor-exec] {line}");
        if let Ok(mut state) = self.state.lock() {
            if let Some(state) = state.as_mut() {
                if state.log.len() >= LOG_TAIL_LINES {
                    state.log.pop_front();
                }
                state.log.push_back(line);
            }
        }
    }

    fn set_phase(&self, phase: &str) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(state) = state.as_mut() {
                state.phase = phase.to_string();
            }
        }
        self.log(format!("phase: {phase}"));
    }

    fn finish(&self, outcome: Result<String, String>) {
        match &outcome {
            Ok(detail) => self.log(format!("done: {detail}")),
            Err(error) => self.log(format!("failed: {error}")),
        }
        let (title, text, urgency) = match &outcome {
            Ok(detail) => (
                "Update handed off",
                detail.clone(),
                crate::types::NotificationUrgency::Info,
            ),
            Err(error) => (
                "Successor exec failed",
                error.clone(),
                crate::types::NotificationUrgency::Attention,
            ),
        };
        if let Ok(mut state) = self.state.lock() {
            if let Some(state) = state.as_mut() {
                state.outcome = Some(outcome);
            }
        }
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.notify_user(
                &format!("successor-exec-{}", super::now_ms()),
                Some(title),
                &text,
                urgency,
            );
        }
        self.in_flight.store(false, Ordering::Release);
    }

    /// The owner's click. Every refusal is a complete honest sentence
    /// for the requesting surface; nothing is spawned unless every gate
    /// passes. `expected_git_sha` is REQUIRED — the request names the
    /// build the owner saw, or it does not run (the specimen's pin).
    pub(crate) fn request_spawn(
        self: &Arc<Self>,
        expected_git_sha: &str,
        requested_by: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let expected = expected_git_sha.trim().to_string();
        if expected.is_empty() {
            return Err(
                "the request must name the offered build (`expected_git_sha`) — the spawn \
                 targets an exact verified artifact, never \"whatever is on disk\""
                    .to_string(),
            );
        }
        let Some(runtime) = self.runtime.upgrade() else {
            return Err("handover runtime gone — daemon shutting down".to_string());
        };
        // Binding 2 (supervisor-absent-only): a live app supervisor owns
        // the one-click swap; the relay lane stays the supervised path.
        if runtime.app_supervised() {
            return Err(
                "a live app supervisor is attached to this daemon — its one-click swap \
                 (`Update now`) performs updates here; the successor-exec lane is for \
                 CLI-launched daemons only"
                    .to_string(),
            );
        }
        if runtime.is_draining() {
            return Err(
                "this daemon is already draining — the hand-off is already in motion".to_string(),
            );
        }
        if !runtime.is_holder() {
            return Err(
                "this daemon does not hold the scheduler lease — a hand-off from here would \
                 change nothing; use the lease holder's own dashboard (the handover status \
                 block names it)"
                    .to_string(),
            );
        }
        // Never exec a path the produce job may be mid-writing.
        if let Some(lane) = runtime.update_lane() {
            if lane.job_in_flight() {
                return Err(
                    "an update job is still producing the artifact — wait for it to finish, \
                     then click again"
                        .to_string(),
                );
            }
        }
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("a successor exec is already in flight".to_string());
        }
        if let Ok(mut state) = self.state.lock() {
            *state = Some(ExecState {
                phase: "starting".to_string(),
                started_ms: super::now_ms(),
                offered_sha: expected.clone(),
                requested_by: requested_by.clone(),
                child_pid: None,
                successor: None,
                log: VecDeque::new(),
                outcome: None,
                build_verified: None,
            });
        }
        let lane = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = lane.run_flow(&runtime, &expected).await;
            lane.finish(outcome);
        });
        Ok(self.status_block())
    }

    /// The ruled sequence. Every early return before the drain leaves
    /// the running daemon exactly as it was (a spawned-but-failed
    /// successor is terminated — it acquired nothing and drained
    /// nothing); after the drain there is no rollback (drain is one-way)
    /// and the flow only reports.
    async fn run_flow(
        self: &Arc<Self>,
        runtime: &Arc<super::HandoverRuntime>,
        expected_sha: &str,
    ) -> Result<String, String> {
        // ── Verify the exec target by path AND hash (specimen pin) ──
        self.set_phase("verify-target");
        let probed = super::update_watch::run_version_probe(&self.exe_path)
            .await
            .map_err(|err| {
                format!(
                    "the exec target at {} failed its --version probe ({err}) — refusing to \
                     spawn an unverifiable binary",
                    self.exe_path.display()
                )
            })?;
        let running_sha = super::update_lane::running_sha_for_compare();
        exec_target_verdict(&probed.git_sha, expected_sha, &running_sha)?;
        self.log(format!(
            "exec target verified: {} is commit {} ({}, built {})",
            self.exe_path.display(),
            probed.git_sha,
            probed.version,
            probed.built_at,
        ));

        // ── Spawn the successor as a plain secondary (binding 3) ──
        self.set_phase("spawn");
        let state_root = runtime.state_root().to_path_buf();
        let mut child = self.spawn_successor(&state_root)?;
        let child_pid = child.id();
        if let Ok(mut state) = self.state.lock() {
            if let Some(state) = state.as_mut() {
                state.child_pid = Some(child_pid);
            }
        }
        self.log(format!(
            "successor spawned (pid {child_pid}) — a plain secondary on its own port; \
             output appends to {}",
            successor_log_path(&state_root).display()
        ));

        // ── Readiness: presence (pid → boot/port/build), then HTTP ──
        self.set_phase("waiting-ready");
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        let successor = loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!(
                    "the new daemon exited during startup ({status}) — the running daemon is \
                     untouched; see {}",
                    successor_log_path(&state_root).display()
                ));
            }
            if let Some(record) = super::read_presence_records(&state_root)
                .into_iter()
                .find(|record| record.pid == child_pid)
            {
                break SuccessorFacts {
                    boot_id: record.boot_id,
                    port: record.port,
                    pid: child_pid,
                    git_sha: record.version.git_sha,
                };
            }
            if tokio::time::Instant::now() >= deadline {
                self.reap_failed_successor(&mut child);
                return Err(format!(
                    "the new daemon never registered its presence within {}s — terminated it; \
                     the running daemon is untouched (see {})",
                    READY_TIMEOUT.as_secs(),
                    successor_log_path(&state_root).display()
                ));
            }
            tokio::time::sleep(READY_POLL).await;
        };
        if let Ok(mut state) = self.state.lock() {
            if let Some(state) = state.as_mut() {
                state.successor = Some(successor.clone());
            }
        }
        self.log(format!(
            "successor registered: boot {} on :{} running commit {}",
            successor.boot_id, successor.port, successor.git_sha
        ));

        // ── Verify the successor IS the offered build before handing
        // over (the specimen's launch-before-replace residue: the file
        // would have to be swapped in the probe→spawn window, but the
        // registered build is the successor's own compiled truth). ──
        self.set_phase("verify-successor");
        if successor.git_sha != probed.git_sha {
            self.reap_failed_successor(&mut child);
            return Err(format!(
                "the spawned daemon reports commit {}, not the offered {} — refusing to hand \
                 over to a build the click did not approve; terminated it, the running daemon \
                 is untouched",
                successor.git_sha, probed.git_sha
            ));
        }

        // ── Readiness: the successor must actually SERVE before the
        // incumbent points anyone at it (the app supervisor's own bar). ──
        self.set_phase("probe-gateway");
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!(
                    "the new daemon exited before serving ({status}) — the running daemon is \
                     untouched; see {}",
                    successor_log_path(&state_root).display()
                ));
            }
            if gateway_answers(&state_root, successor.port).await {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                self.reap_failed_successor(&mut child);
                return Err(format!(
                    "the new daemon on :{} never answered its gateway within {}s — terminated \
                     it; the running daemon is untouched (see {})",
                    successor.port,
                    READY_TIMEOUT.as_secs(),
                    successor_log_path(&state_root).display()
                ));
            }
            tokio::time::sleep(READY_POLL).await;
        }
        self.log(format!("successor gateway answers on :{}", successor.port));

        // ── Drain toward it (the ruled HS3 machinery; one-way) ──
        self.set_phase("drain");
        match runtime.request_drain(Some(format!(
            "successor-exec click → :{} ({})",
            successor.port, successor.boot_id
        ))) {
            super::DrainRequest::Entered | super::DrainRequest::AlreadyDraining => {}
            super::DrainRequest::NotHolder => {
                // Holdership was lost between the click gate and here
                // (another drain, a lease-infrastructure failure). The
                // successor stays: it is a healthy secondary that will
                // converge on whatever the lease does next.
                return Err(format!(
                    "this daemon no longer holds the scheduler lease — nothing was handed \
                     over; the spawned daemon on :{} remains as an ordinary secondary",
                    successor.port
                ));
            }
        }

        // ── Post-takeover verification (specimen pin): the new holder's
        // reported build must be the offered build, said out loud. ──
        self.set_phase("verify-takeover");
        let acquire_deadline = tokio::time::Instant::now() + ACQUIRE_TIMEOUT;
        loop {
            if let Some(sidecar) = super::read_lease_sidecar(&state_root) {
                if sidecar.boot_id == successor.boot_id {
                    let verified = sidecar.version.git_sha == expected_sha;
                    if let Ok(mut state) = self.state.lock() {
                        if let Some(state) = state.as_mut() {
                            state.build_verified = Some(verified);
                        }
                    }
                    if verified {
                        return Ok(format!(
                            "the new daemon on :{} now holds the scheduler lease and runs the \
                             offered build (commit {}) — in-flight sessions finish on this \
                             daemon, then it exits",
                            successor.port, expected_sha
                        ));
                    }
                    return Err(format!(
                        "the swap did NOT land the offered build: the new lease holder on :{} \
                         reports commit {}, offered was {} — in-flight sessions still finish \
                         here, but treat the update as not applied",
                        successor.port, sidecar.version.git_sha, expected_sha
                    ));
                }
            }
            if tokio::time::Instant::now() >= acquire_deadline {
                return Err(format!(
                    "drain entered, but the successor on :{} has not acquired the scheduler \
                     lease within {}s — the successor watch will alert loudly if it is gone; \
                     this daemon keeps serving its in-flight sessions",
                    successor.port,
                    ACQUIRE_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    /// Spawn the successor: the VERIFIED exec target, `--web 0` (the
    /// kernel picks a free port — the race-free parallel-daemon shape)
    /// plus the replayed standing flags, the incumbent's environment
    /// minus the app-supervisor claim, stdin null, output appended to
    /// the successor log, detached into its own process group so the
    /// incumbent's terminal signals (Ctrl-C on a CLI daemon) never
    /// reach it.
    fn spawn_successor(&self, state_root: &Path) -> Result<std::process::Child, String> {
        let mut cmd = std::process::Command::new(&self.exe_path);
        cmd.arg("--web").arg("0");
        cmd.args(&self.replay_args);
        // The successor is NOT app-supervised (binding 2 is about the
        // incumbent; the claim would be stale in the child and the live
        // parent-pid check would reject it anyway — strip it at the
        // source instead of relying on that).
        cmd.env_remove("INTENDANT_APP_SUPERVISOR_PID");
        cmd.stdin(std::process::Stdio::null());
        let log_path = successor_log_path(state_root);
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(mut log) => {
                use std::io::Write as _;
                let _ = writeln!(
                    log,
                    "\n--- successor-exec spawn {} ---",
                    chrono::Utc::now().to_rfc3339()
                );
                match log.try_clone() {
                    Ok(err_log) => {
                        cmd.stdout(log);
                        cmd.stderr(err_log);
                    }
                    Err(_) => {
                        // Clone failure costs stderr capture, not the spawn.
                        cmd.stdout(log);
                        cmd.stderr(std::process::Stdio::null());
                    }
                }
            }
            Err(err) => {
                // A daemon must not be un-spawnable because its log file
                // is not — but say so, and drop the output honestly.
                eprintln!(
                    "[successor-exec] successor log {} unavailable ({err}) — successor \
                     output will be discarded",
                    log_path.display()
                );
                cmd.stdout(std::process::Stdio::null());
                cmd.stderr(std::process::Stdio::null());
            }
        }
        crate::platform::configure_detached_spawn(&mut cmd);
        cmd.spawn().map_err(|err| {
            format!(
                "could not spawn the new daemon from {}: {err}",
                self.exe_path.display()
            )
        })
    }

    /// Terminate a successor that never became ready: SIGTERM first
    /// where the platform has it, a bounded grace, then the hard kill.
    /// It acquired nothing and drained nothing — reaping it is clean
    /// recovery (the app supervisor's own failed-swap rule).
    fn reap_failed_successor(&self, child: &mut std::process::Child) {
        let pid = child.id();
        if crate::platform::request_graceful_terminate(pid) {
            let deadline = std::time::Instant::now() + TERMINATE_GRACE;
            while std::time::Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    self.log(format!("failed successor (pid {pid}) exited on SIGTERM"));
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        self.log(format!("failed successor (pid {pid}) killed"));
    }
}

/// Where the spawned successor's stdout/stderr append — beside the
/// state root's other daemon logs, one file across spawns (each spawn
/// writes a separator line), the CLI counterpart of the app wrapper's
/// `app-backend.log`.
fn successor_log_path(state_root: &Path) -> PathBuf {
    state_root.join("successor-exec.log")
}

/// Does the successor's gateway answer? HEAD `/` on its port, scheme
/// from ITS per-port loopback sidecar when it has landed (the daemon's
/// self-signed TLS is fine — the probe asserts liveness, not identity;
/// the takeover requester rides the same posture).
async fn gateway_answers(state_root: &Path, port: u16) -> bool {
    let scheme = std::fs::read_to_string(crate::loopback_token::loopback_sidecar_path(
        state_root, port,
    ))
    .ok()
    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    .and_then(|meta| meta.get("scheme")?.as_str().map(str::to_string))
    .unwrap_or_else(|| "http".to_string());
    let Ok(client) = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    match client
        .head(format!("{scheme}://127.0.0.1:{port}/"))
        .send()
        .await
    {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

/// Wire the lane onto the runtime (gateway shapes, beside the update
/// watch + lane). `replay_args` is the parser-captured standing flag
/// set for the successor.
pub(crate) fn spawn_successor_exec_lane(
    runtime: &Arc<super::HandoverRuntime>,
    replay_args: Vec<String>,
) {
    let Some(exe_path) = super::update_watch::watched_binary_path() else {
        eprintln!("[successor-exec] current_exe unresolvable — successor-exec lane off for this boot");
        return;
    };
    let lane = Arc::new(SuccessorExecLane::new(runtime, exe_path, replay_args));
    runtime.set_successor_exec(lane);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The specimen's two refusals, wording-pinned: offered≠target (the
    /// artifact changed under the button) and offered==running (a
    /// build-neutral swap is refused out loud, never performed
    /// silently).
    #[test]
    fn exec_target_verdict_refuses_mismatch_and_build_neutral() {
        assert!(exec_target_verdict("abc123", "abc123", "olddef").is_ok());

        let mismatch = exec_target_verdict("abc123", "def456", "olddef").unwrap_err();
        assert!(
            mismatch.contains("changed since the panel rendered"),
            "{mismatch}"
        );
        assert!(mismatch.contains("Nothing was spawned"), "{mismatch}");
        assert!(mismatch.contains("abc123") && mismatch.contains("def456"));

        let neutral = exec_target_verdict("abc123", "abc123", "abc123").unwrap_err();
        assert!(neutral.contains("would not change builds"), "{neutral}");
        assert!(neutral.contains("Nothing was spawned"), "{neutral}");
    }

    fn test_lane(runtime: &Arc<super::super::HandoverRuntime>) -> Arc<SuccessorExecLane> {
        Arc::new(SuccessorExecLane::new(
            runtime,
            PathBuf::from("/nonexistent/intendant-for-test"),
            Vec::new(),
        ))
    }

    /// The click gates, each refusal an honest sentence: a missing
    /// offered sha, a live app supervisor (binding 2 — the relay lane
    /// owns supervised daemons), a draining daemon, and a non-holder
    /// all refuse without spawning anything.
    #[tokio::test]
    async fn request_spawn_refuses_each_gate_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = super::super::HandoverRuntime::initialize(dir.path(), 7001, 0);
        runtime.set_app_supervisor_pid_for_test(None);
        let runtime = Arc::new(runtime);
        let lane = test_lane(&runtime);

        let missing = lane.request_spawn("  ", None).unwrap_err();
        assert!(missing.contains("expected_git_sha"), "{missing}");

        // Binding 2: supervised daemons refuse toward the relay lane.
        // (Only exercisable where a live parent pid exists to claim.)
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let mut supervised =
                super::super::HandoverRuntime::initialize(dir.path(), 7002, 0);
            supervised.set_app_supervisor_pid_for_test(Some(
                std::os::unix::process::parent_id(),
            ));
            let supervised = Arc::new(supervised);
            let lane = test_lane(&supervised);
            let refusal = lane.request_spawn("abc123", None).unwrap_err();
            assert!(refusal.contains("app supervisor"), "{refusal}");
            assert!(refusal.contains("CLI-launched"), "{refusal}");
        }

        // Draining refuses.
        assert_eq!(
            runtime.request_drain(None),
            super::super::DrainRequest::Entered
        );
        let draining = lane.request_spawn("abc123", None).unwrap_err();
        assert!(draining.contains("draining"), "{draining}");

        // A non-holder refuses (fresh runtime, lease held elsewhere).
        let holder = super::super::HandoverRuntime::initialize(dir.path(), 7003, 0);
        assert!(holder.is_holder(), "post-drain lease is free to take");
        let secondary = Arc::new(super::super::HandoverRuntime::initialize(
            dir.path(),
            7004,
            0,
        ));
        assert!(!secondary.is_holder());
        let lane = test_lane(&secondary);
        let not_holder = lane.request_spawn("abc123", None).unwrap_err();
        assert!(not_holder.contains("scheduler lease"), "{not_holder}");
    }

    /// One flow at a time, and the in-flight flag is visible on the
    /// status block until the flow finishes (here: the flow fails fast
    /// on the unprobeable test path, releasing the guard).
    #[tokio::test]
    async fn request_spawn_is_single_flight_and_status_carries_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = super::super::HandoverRuntime::initialize(dir.path(), 7001, 0);
        runtime.set_app_supervisor_pid_for_test(None);
        let runtime = Arc::new(runtime);
        let lane = test_lane(&runtime);

        let block = lane.status_block();
        assert_eq!(block["available"], true);
        assert!(block.get("phase").is_none(), "idle lane has no phase");

        let started = lane
            .request_spawn("abc123", Some("test click".to_string()))
            .expect("gates pass");
        assert_eq!(started["offered_sha"], "abc123");
        assert_eq!(started["in_flight"], true);

        // The flow fails on the nonexistent exec target and releases
        // the guard; a second click then passes the in-flight gate.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let block = lane.status_block();
            if block.get("ok").is_some() {
                assert_eq!(block["ok"], false);
                assert!(
                    block["error"]
                        .as_str()
                        .is_some_and(|err| err.contains("--version probe")),
                    "{block}"
                );
                break;
            }
            // While in flight, a second click refuses.
            if block["in_flight"] == true {
                if let Err(refusal) = lane.request_spawn("abc123", None) {
                    assert!(
                        refusal.contains("already in flight") || block.get("ok").is_some(),
                        "{refusal}"
                    );
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "flow never finished: {}",
                lane.status_block()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

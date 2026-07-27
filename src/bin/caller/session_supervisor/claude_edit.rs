//! The pencil's in-place edit ladder for Claude Code sessions.
//!
//! Since CC ≥2.1.218 the supervision wire exposes `rewind_conversation`,
//! so a pencil edit rewinds the SAME backend session id instead of
//! forking a new one. Three rungs, tried in order, each refusal mapping
//! honestly to the next:
//!
//! 1. **Wire rewind** (live wrapper, capability-probed CLI): the
//!    supervisor resolves the edited row and its walk-back list from
//!    the transcript, and the wrapper drives CC's native
//!    `rewind_conversation` control requests, then sends the edited
//!    prompt as the next turn. Same id, same wrapper, sidecars and
//!    config untouched.
//! 2. **Transcript surgery** (no live process): stop the wrapper if one
//!    is alive, wait for the file to quiesce, truncate the session's
//!    own transcript at the edited row (`claude_inplace.rs`), and
//!    resume the same id with the edited prompt as the first task.
//! 3. **Anchor fork** (`fork_claude_edit_branch_from_anchor`) — the
//!    pre-ladder behavior, kept only for the cases neither in-place
//!    rung can serve (off-active-chain target, first-message edit) and
//!    labeled honestly when it runs.
//!
//! Single-ownership rule: once the wire rung's message is queued on a
//! wrapper, the ladder NEVER races a second rung against it on a
//! timeout — only the wrapper's own `rewind-unavailable` refusal hands
//! the edit to the surgery rung. A parked (rate-limited) wrapper
//! services the queued edit when it wakes; that is one lineage doing
//! the work once, which is the whole point.

use super::*;

/// Stop reason the surgery rung uses; the wrapper's terminal-outcome
/// handling treats it as a user-requested stop (`external_mode.rs`).
pub(crate) const CLAUDE_EDIT_INPLACE_STOP_REASON: &str = "rewinding in place for an edited message";

/// Stop reason the FORK rung uses on the parent it replaces: edit
/// semantics are replace even on the last-resort rung, so the parent
/// wrapper stops and the lineage retires to the child instead of
/// staying alive beside it (RC1 of the 2026-07-27 incident).
pub(crate) const CLAUDE_EDIT_SUPERSEDED_STOP_REASON: &str = "superseded by an edit branch";

/// How long the ladder listens for the wire rung's outcome before
/// reaping the listener task. NOT a fallback trigger: a queued edit on
/// a parked wrapper may legitimately take hours, and racing surgery
/// against a still-queued wire edit would run the edit twice.
const CLAUDE_EDIT_WAITER_CAP: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Transcript-quiescence probe before surgery: the file must hold still
/// this long (two identical stats) within the budget, or surgery is
/// refused — something is still writing it.
const CLAUDE_EDIT_QUIESCENCE_SAMPLE_GAP: std::time::Duration =
    std::time::Duration::from_millis(300);
const CLAUDE_EDIT_QUIESCENCE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Backend ids with an edit ladder currently in flight. A second pencil
/// on the same session while one is unresolved refuses honestly instead
/// of racing two rungs — the 2026-07-27 incident's lesson is precisely
/// that concurrent revival lanes for one logical session must not exist.
static ACTIVE_CLAUDE_EDITS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();

fn active_claude_edits() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    ACTIVE_CLAUDE_EDITS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// RAII claim on a backend id in [`ACTIVE_CLAUDE_EDITS`].
struct ClaudeEditClaim {
    backend_id: String,
}

impl ClaudeEditClaim {
    fn take(backend_id: &str) -> Option<Self> {
        let mut active = match active_claude_edits().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !active.insert(backend_id.to_string()) {
            return None;
        }
        Some(Self {
            backend_id: backend_id.to_string(),
        })
    }
}

impl Drop for ClaudeEditClaim {
    fn drop(&mut self) {
        let mut active = match active_claude_edits().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        active.remove(&self.backend_id);
    }
}

impl SessionSupervisor {
    /// Entry point for every claude-code pencil edit (routed from
    /// `deliver_edit_user_message` when a live wrapper exists, and from
    /// `route_edit_user_message` for detached sessions).
    pub(crate) async fn claude_edit_in_place_ladder(
        &self,
        request: EditUserMessageRequest,
        target: Option<EditRouteTarget>,
    ) {
        let status_sid = target
            .as_ref()
            .map(|t| t.managed_id.clone())
            .unwrap_or_else(|| request.requested_id.clone());
        let turn = request.user_turn_index;
        let Some(original_text) = request
            .original_text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
        else {
            self.emit_edit_user_message_status(
                Some(status_sid),
                turn,
                "failed",
                "the edited row carried no original text to locate in the transcript",
            );
            return;
        };

        // Resolve the backend identity and transcript exactly as the
        // fork lane always has.
        let token = status_sid.clone();
        let home = self.logs_home();
        let backend_id = match persisted_external_identity_for_session_in_home(&home, &token) {
            Some((source, id)) if source == "claude-code" => id,
            Some((source, _)) => {
                self.emit_edit_user_message_status(
                    Some(status_sid),
                    turn,
                    "failed",
                    format!("session is a {source} session, not claude-code"),
                );
                return;
            }
            None => token.clone(),
        };
        let Some(transcript) = crate::web_gateway::find_claude_session_file(&home, &backend_id)
        else {
            self.emit_edit_user_message_status(
                Some(status_sid),
                turn,
                "failed",
                format!("transcript for claude-code session {backend_id} not found"),
            );
            return;
        };

        let Some(claim) = ClaudeEditClaim::take(&backend_id) else {
            self.emit_edit_user_message_status(
                Some(status_sid),
                turn,
                "failed",
                "another edit of this session is still being applied — wait for it to finish",
            );
            return;
        };

        let anchor_transcript = transcript.clone();
        let anchor = match tokio::task::spawn_blocking(move || {
            crate::session_fork::claude_edit_branch_anchor(&anchor_transcript, &original_text)
        })
        .await
        .unwrap_or_else(|err| Err(format!("anchor resolution task failed: {err}")))
        {
            Ok(anchor) => anchor,
            Err(reason) => {
                self.emit_edit_user_message_status(Some(status_sid), turn, "failed", reason);
                return;
            }
        };

        // Rung 1: wire rewind, when a live wrapper can carry it and the
        // installed CLI speaks the subtype.
        if let Some(target) = target {
            let capability = match self.config.claude_rewind_capability_for_tests {
                Some(capability) => capability,
                None => {
                    self.claude_rewind_wire_capability_for(&target.project_root)
                        .await
                }
            };
            match capability {
                external_agent::claude_code::ClaudeRewindWireCapability::Unsupported => {
                    // The canary firing live: the configured CLI does not
                    // expose the rewind subtype this build was probed
                    // against. Loud, then fall through to surgery.
                    self.warn(&format!(
                        "installed claude does not expose the `{}` control subtype (CC wire drift, or a pre-2.1.218 CLI) — pencil edits fall back to transcript surgery",
                        external_agent::claude_code::CLAUDE_REWIND_WIRE_SUBTYPE
                    ));
                }
                _ => {
                    let resolved_attachments = self
                        .resolve_session_attachments(
                            &request.attachments,
                            &target.session_dir,
                            &target.project_root,
                        )
                        .await;
                    let msg = FollowUpMessage::edit_user_message(
                        request.text.clone(),
                        resolved_attachments,
                        request.user_turn_index,
                        request.user_turn_revision.unwrap_or(1),
                        request.original_text.clone(),
                        request.attachments.clone(),
                    )
                    .with_claude_inplace_rewind_targets(anchor.rewind_targets_newest_first());
                    // Subscribe BEFORE queueing so a fast wrapper outcome
                    // cannot slip past the waiter.
                    let outcome_rx = self.config.bus.subscribe();
                    match target.follow_up_tx.try_send(msg) {
                        Ok(()) => {
                            self.emit_edit_user_message_status(
                                Some(target.managed_id.clone()),
                                turn,
                                "queued",
                                format!(
                                    "in-place edit queued for claude-code session {}",
                                    short_session(&target.managed_id)
                                ),
                            );
                            self.spawn_claude_edit_outcome_waiter(
                                request,
                                anchor,
                                backend_id,
                                transcript,
                                target.managed_id,
                                claim,
                                outcome_rx,
                            );
                            return;
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            self.emit_edit_user_message_status(
                                Some(target.managed_id.clone()),
                                turn,
                                "failed",
                                "claude-code session input queue is full",
                            );
                            return;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            // The wrapper died under us: the process is
                            // gone, which is exactly the surgery rung's
                            // precondition.
                        }
                    }
                }
            }
            self.claude_edit_surgery_rung(request, anchor, backend_id, transcript, claim)
                .await;
            return;
        }

        // No live wrapper: surgery directly (no process to stop).
        self.claude_edit_surgery_rung(request, anchor, backend_id, transcript, claim)
            .await;
    }

    /// Wire-rung outcome listener. Terminal `ok`/`failed` end the
    /// ladder; the wrapper's `rewind-unavailable` refusal hands the edit
    /// to the surgery rung. Deliberately NO timeout-to-fallback: the
    /// queued message stays the single owner of the edit until the
    /// wrapper reports (see the module doc).
    #[allow(clippy::too_many_arguments)] // internal plumbing: each param is a distinct dependency of the waiter
    fn spawn_claude_edit_outcome_waiter(
        &self,
        request: EditUserMessageRequest,
        anchor: crate::session_fork::ClaudeEditAnchor,
        backend_id: String,
        transcript: PathBuf,
        managed_id: String,
        claim: ClaudeEditClaim,
        mut outcome_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    ) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            let claim = claim;
            let turn = request.user_turn_index;
            let matches_session = |sid: &Option<String>| {
                sid.as_deref().is_some_and(|sid| {
                    sid == request.requested_id || sid == managed_id || sid == backend_id
                })
            };
            let deadline = tokio::time::Instant::now() + CLAUDE_EDIT_WAITER_CAP;
            loop {
                let event = tokio::select! {
                    event = outcome_rx.recv() => event,
                    _ = tokio::time::sleep_until(deadline) => {
                        supervisor.warn(&format!(
                            "edit of claude-code session {} is still queued after {} minutes — it will apply when the session next drains its queue",
                            short_session(&backend_id),
                            CLAUDE_EDIT_WAITER_CAP.as_secs() / 60
                        ));
                        return;
                    }
                };
                match event {
                    Ok(AppEvent::UserMessageEditStatus {
                        session_id,
                        user_turn_index,
                        status,
                        ..
                    }) if user_turn_index == turn && matches_session(&session_id) => {
                        match status.as_str() {
                            "ok" | "failed" => return,
                            "rewind-unavailable" => {
                                supervisor
                                    .claude_edit_surgery_rung(
                                        request, anchor, backend_id, transcript, claim,
                                    )
                                    .await;
                                return;
                            }
                            _ => {}
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Rung 2: stop the wrapper if one is still alive, wait for the
    /// transcript to quiesce, truncate it at the edited row, and resume
    /// the SAME session id with the edited prompt. Refusals fall to the
    /// fork rung with the reason in the status.
    async fn claude_edit_surgery_rung(
        &self,
        request: EditUserMessageRequest,
        anchor: crate::session_fork::ClaudeEditAnchor,
        backend_id: String,
        transcript: PathBuf,
        claim: ClaudeEditClaim,
    ) {
        let _claim = claim;
        let turn = request.user_turn_index;
        let status_sid = request.requested_id.clone();
        self.emit_edit_user_message_status(
            Some(status_sid.clone()),
            turn,
            "running",
            "rewinding this session in place — truncating the transcript at the edited message",
        );

        // A wrapper may still hold the session (wire refusal path, or a
        // lookup race): stop it and wait for the backend process to go.
        let (_, live_target, _) = self.lookup_edit_route_target(&backend_id).await;
        if let Some(live) = live_target {
            if let Some(stopped) = self
                .stop_managed_session(Some(live.managed_id), CLAUDE_EDIT_INPLACE_STOP_REASON)
                .await
            {
                self.wait_for_stopped_session(stopped).await;
            }
        }
        if let Err(reason) = wait_for_transcript_quiescence(&transcript).await {
            self.claude_edit_fork_rung(request, anchor, reason).await;
            return;
        }

        let surgery_transcript = transcript.clone();
        let target_uuid = anchor.target_user_uuid.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            crate::session_fork::truncate_claude_transcript_in_place(
                &surgery_transcript,
                &target_uuid,
            )
        })
        .await
        .unwrap_or_else(|err| Err(format!("surgery task failed: {err}")));
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(reason) => {
                self.claude_edit_fork_rung(request, anchor, reason).await;
                return;
            }
        };

        // Same-id resume with the edited prompt as the first task; the
        // parent's project root keeps the resume in the right project
        // dir, exactly like the fork lane.
        let home = self.logs_home();
        let project_root = crate::session_config::load_for_resume(
            &home,
            "claude-code",
            &backend_id,
            Some(&backend_id),
        )
        .and_then(|cfg| cfg.project_root);
        self.resume_session(
            "claude-code".to_string(),
            backend_id.clone(),
            Some(backend_id.clone()),
            project_root,
            Some(request.text.clone()),
            Some(true),
            request.attachments.clone(),
            false,
            None,
            LaunchOverrides::default(),
            false,
            false,
        )
        .await;
        self.emit_edit_user_message_status(
            Some(status_sid),
            turn,
            "ok",
            format!(
                "rewound in place — the edited prompt continues in this session ({} lines kept; the pre-edit transcript is retained as {})",
                outcome.kept_lines,
                outcome
                    .backup_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "a .bak-edit backup".to_string())
            ),
        );
    }

    /// Rung 3: the anchor fork, labeled with WHY the in-place rungs
    /// refused. First-message edits have no fork anchor and fail
    /// honestly here.
    async fn claude_edit_fork_rung(
        &self,
        request: EditUserMessageRequest,
        anchor: crate::session_fork::ClaudeEditAnchor,
        reason: String,
    ) {
        let token = request.requested_id.clone();
        let Some(fork_anchor) = anchor.fork_anchor.clone() else {
            self.emit_edit_user_message_status(
                Some(token),
                request.user_turn_index,
                "failed",
                format!(
                    "in-place rewind unavailable ({reason}) and the first message cannot be branched from before — the edit was not applied"
                ),
            );
            return;
        };
        self.fork_claude_edit_branch_from_anchor(
            request,
            token,
            fork_anchor,
            format!(
                "in-place rewind unavailable ({reason}) — branching into a new session from before this message"
            ),
        )
        .await;
    }

    /// Capability-probe the CLI the target project would spawn
    /// (project-configured command, default `claude`), off the async
    /// runtime — the cold probe scans a quarter-gigabyte binary.
    async fn claude_rewind_wire_capability_for(
        &self,
        project_root: &Path,
    ) -> external_agent::claude_code::ClaudeRewindWireCapability {
        let command = crate::project::Project::from_root(project_root.to_path_buf())
            .map(|project| project.config.agent.claude_code.command.clone())
            .unwrap_or_else(|_| "claude".to_string());
        tokio::task::spawn_blocking(move || {
            external_agent::claude_code::claude_rewind_wire_capability(&command)
        })
        .await
        .unwrap_or(external_agent::claude_code::ClaudeRewindWireCapability::Unknown)
    }
}

/// The transcript must hold still (two identical len+mtime stats,
/// [`CLAUDE_EDIT_QUIESCENCE_SAMPLE_GAP`] apart) before surgery touches
/// it. A file that never settles within the budget means some process —
/// ours mid-shutdown or a foreign CLI — is still writing it.
async fn wait_for_transcript_quiescence(transcript: &Path) -> Result<(), String> {
    let stat = |path: &Path| {
        std::fs::metadata(path)
            .map(|meta| (meta.len(), meta.modified().ok()))
            .map_err(|err| format!("failed to stat the transcript: {err}"))
    };
    let deadline = tokio::time::Instant::now() + CLAUDE_EDIT_QUIESCENCE_BUDGET;
    let mut previous = stat(transcript)?;
    loop {
        tokio::time::sleep(CLAUDE_EDIT_QUIESCENCE_SAMPLE_GAP).await;
        let current = stat(transcript)?;
        if current == previous {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(
                "the transcript is still being written (a claude process is attached)".to_string(),
            );
        }
        previous = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_supervisor::tests::{managed_session, test_supervisor};

    fn fixture_line(uuid: &str, parent: Option<&str>, kind: &str, text: &str) -> String {
        serde_json::json!({
            "uuid": uuid,
            "parentUuid": parent,
            "type": kind,
            "timestamp": "2026-07-27T00:00:00.000Z",
            "message": {"role": kind, "content": [{"type": "text", "text": text}]},
        })
        .to_string()
    }

    /// Hermetic claude store + supervisor wired for ladder tests. The
    /// capability seam avoids the real-binary probe; the launch gate
    /// holds any resume body until the test opens it.
    struct LadderRig {
        home: tempfile::TempDir,
        project: tempfile::TempDir,
        supervisor: SessionSupervisor,
        bus: EventBus,
        gate_tx: tokio::sync::watch::Sender<bool>,
        transcript: PathBuf,
    }

    fn ladder_rig(
        parent_id: &str,
        capability: external_agent::claude_code::ClaudeRewindWireCapability,
    ) -> LadderRig {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let project_dir = home.path().join(".claude").join("projects").join("proj");
        std::fs::create_dir_all(&project_dir).unwrap();
        let lines = [
            fixture_line("u1", None, "user", "first prompt"),
            fixture_line("a1", Some("u1"), "assistant", "reply one"),
            fixture_line("u2", Some("a1"), "user", "do the thing"),
            fixture_line("a2", Some("u2"), "assistant", "stale answer"),
        ];
        let transcript = project_dir.join(format!("{parent_id}.jsonl"));
        std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();

        let bus = EventBus::new();
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let mut config =
            (*test_supervisor(project.path().to_path_buf(), bus.clone()).config).clone();
        config.logs_home_override = Some(home.path().to_path_buf());
        config.launch_gate_for_tests = Some(gate_rx);
        config.claude_rewind_capability_for_tests = Some(capability);
        let supervisor = SessionSupervisor::new(config);
        LadderRig {
            home,
            project,
            supervisor,
            bus,
            gate_tx,
            transcript,
        }
    }

    fn edit_request(parent_id: &str) -> EditUserMessageRequest {
        EditUserMessageRequest {
            requested_id: parent_id.to_string(),
            user_turn_index: 2,
            user_turn_revision: Some(1),
            original_text: Some("do the thing".to_string()),
            text: "do the improved thing".to_string(),
            attachments: Vec::new(),
        }
    }

    async fn register_live_session(
        rig: &LadderRig,
        parent_id: &str,
    ) -> mpsc::Receiver<FollowUpMessage> {
        let (tx, rx) = mpsc::channel(4);
        let mut session = managed_session(parent_id, "claude-code");
        session.project_root = rig.project.path().to_path_buf();
        session.session_dir = rig.project.path().join("session-dir");
        session.follow_up_tx = tx;
        rig.supervisor
            .state
            .lock()
            .await
            .sessions
            .insert(parent_id.to_string(), session);
        rx
    }

    /// The incident's shape: a live (e.g. limit-parked) wrapper gets a
    /// pencil edit. The ladder must queue the in-place rewind on THAT
    /// wrapper — one live session before, one after, and no fork.
    #[tokio::test]
    async fn edit_after_limit_yields_exactly_one_live_session() {
        let parent_id = "5c1e2a51-0000-4000-8000-0000000000aa";
        let rig = ladder_rig(
            parent_id,
            external_agent::claude_code::ClaudeRewindWireCapability::Supported,
        );
        let mut follow_rx = register_live_session(&rig, parent_id).await;
        let mut bus_rx = rig.bus.subscribe();
        let _ = &rig.home;

        rig.supervisor
            .route_edit_user_message(
                Some(parent_id.to_string()),
                Some("claude-code".to_string()),
                Some(parent_id.to_string()),
                None,
                Some(true),
                2,
                Some(1),
                Some("do the thing".to_string()),
                "do the improved thing".to_string(),
                Vec::new(),
            )
            .await;

        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), follow_rx.recv())
            .await
            .expect("edit delivered to the live wrapper")
            .expect("channel open");
        assert_eq!(msg.text, "do the improved thing");
        assert_eq!(msg.edit_user_turn_index, Some(2));
        assert_eq!(
            msg.claude_inplace_rewind_targets,
            vec!["u2".to_string()],
            "the wire rung must carry the resolved rewind target"
        );

        // Exactly one live session; nothing forked, nothing spawned.
        assert_eq!(rig.supervisor.state.lock().await.sessions.len(), 1);
        while let Ok(event) = bus_rx.try_recv() {
            assert!(
                !matches!(event, AppEvent::SessionForkResult { .. }),
                "an in-place edit must not fork"
            );
        }
    }

    /// The ladder's order pin: the wire rung is attempted first; a
    /// wrapper `rewind-unavailable` refusal (and only that — never a
    /// supervisor-side timeout) hands the edit to the surgery rung,
    /// which truncates the SAME transcript and resumes the SAME id.
    #[tokio::test]
    async fn pencil_prefers_wire_rewind_and_falls_back_in_order() {
        let parent_id = "5c1e2a51-0000-4000-8000-0000000000ab";
        let rig = ladder_rig(
            parent_id,
            external_agent::claude_code::ClaudeRewindWireCapability::Supported,
        );
        let mut follow_rx = register_live_session(&rig, parent_id).await;
        let mut bus_rx = rig.bus.subscribe();

        rig.supervisor
            .claude_edit_in_place_ladder(
                edit_request(parent_id),
                Some(EditRouteTarget {
                    managed_id: parent_id.to_string(),
                    source: "claude-code".to_string(),
                    project_root: rig.project.path().to_path_buf(),
                    session_dir: rig.project.path().join("session-dir"),
                    follow_up_tx: rig
                        .supervisor
                        .state
                        .lock()
                        .await
                        .sessions
                        .get(parent_id)
                        .expect("registered")
                        .follow_up_tx
                        .clone(),
                }),
            )
            .await;

        // Rung 1 first: the wire message reaches the live wrapper.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), follow_rx.recv())
            .await
            .expect("wire rung attempted first")
            .expect("channel open");
        assert_eq!(msg.claude_inplace_rewind_targets, vec!["u2".to_string()]);

        // The wrapper refuses → the waiter must run the surgery rung.
        rig.bus.send(AppEvent::UserMessageEditStatus {
            session_id: Some(parent_id.to_string()),
            user_turn_index: 2,
            status: "rewind-unavailable".to_string(),
            message: "in-place rewind unavailable: stale target".to_string(),
        });

        // Surgery stops the wrapper (our channel closes)…
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match follow_rx.recv().await {
                    Some(_) => continue,
                    None => break,
                }
            }
        })
        .await
        .expect("surgery rung stops the live wrapper first");
        // …truncates the transcript in place…
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let body = std::fs::read_to_string(&rig.transcript).unwrap_or_default();
                if !body.contains("do the thing") && !body.contains("stale answer") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("transcript truncated at the edited row");
        assert!(std::fs::read_to_string(&rig.transcript)
            .expect("transcript readable")
            .contains("reply one"));

        // …and resumes the SAME id: register the same-id session while
        // the launch gate holds the resume, then open it — the funnel
        // must route the edited prompt there as a follow-up.
        let (tx, mut resumed_rx) = mpsc::channel(4);
        {
            let mut state = rig.supervisor.state.lock().await;
            let mut session = managed_session(parent_id, "claude-code");
            session.project_root = rig.project.path().to_path_buf();
            session.session_dir = rig.project.path().join("session-dir");
            session.follow_up_tx = tx;
            state.sessions.insert(parent_id.to_string(), session);
        }
        rig.gate_tx.send(true).unwrap();
        let resumed = tokio::time::timeout(std::time::Duration::from_secs(10), resumed_rx.recv())
            .await
            .expect("edited prompt delivered to the same session id")
            .expect("channel open");
        assert_eq!(resumed.text, "do the improved thing");
        assert!(resumed.claude_inplace_rewind_targets.is_empty());

        // A backup of the pre-edit transcript sits beside it, and the
        // whole ladder never forked.
        let parent_dir = rig.transcript.parent().unwrap();
        let backups: Vec<_> = std::fs::read_dir(parent_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".bak-edit-"))
            .collect();
        assert_eq!(backups.len(), 1, "surgery keeps exactly one backup");
        while let Ok(event) = bus_rx.try_recv() {
            assert!(
                !matches!(event, AppEvent::SessionForkResult { .. }),
                "the in-place ladder must not fork"
            );
        }
    }

    /// Detached session (no wrapper at all): the ladder goes straight to
    /// surgery and resumes the same id — the case today's fork lane
    /// couldn't serve at all.
    #[tokio::test]
    async fn detached_edit_rewinds_in_place_and_resumes_same_id() {
        let parent_id = "5c1e2a51-0000-4000-8000-0000000000ac";
        let rig = ladder_rig(
            parent_id,
            external_agent::claude_code::ClaudeRewindWireCapability::Supported,
        );
        let mut bus_rx = rig.bus.subscribe();

        let ladder = {
            let supervisor = rig.supervisor.clone();
            let request = edit_request(parent_id);
            tokio::spawn(async move {
                supervisor.claude_edit_in_place_ladder(request, None).await;
            })
        };

        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let body = std::fs::read_to_string(&rig.transcript).unwrap_or_default();
                if !body.contains("do the thing") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("transcript truncated");

        let (tx, mut resumed_rx) = mpsc::channel(4);
        {
            let mut state = rig.supervisor.state.lock().await;
            let mut session = managed_session(parent_id, "claude-code");
            session.project_root = rig.project.path().to_path_buf();
            session.session_dir = rig.project.path().join("session-dir");
            session.follow_up_tx = tx;
            state.sessions.insert(parent_id.to_string(), session);
        }
        rig.gate_tx.send(true).unwrap();
        let resumed = tokio::time::timeout(std::time::Duration::from_secs(10), resumed_rx.recv())
            .await
            .expect("edited prompt delivered")
            .expect("channel open");
        assert_eq!(resumed.text, "do the improved thing");
        ladder.await.expect("ladder completes");
        while let Ok(event) = bus_rx.try_recv() {
            assert!(!matches!(event, AppEvent::SessionForkResult { .. }));
        }
    }

    /// A first-message edit has no fork anchor and (with the wire rung
    /// unavailable) must fail honestly — never fork, never truncate the
    /// whole conversation away.
    #[tokio::test]
    async fn first_message_edit_fails_honestly_without_forking() {
        let parent_id = "5c1e2a51-0000-4000-8000-0000000000ad";
        let rig = ladder_rig(
            parent_id,
            external_agent::claude_code::ClaudeRewindWireCapability::Unsupported,
        );
        let before = std::fs::read_to_string(&rig.transcript).unwrap();
        let mut bus_rx = rig.bus.subscribe();

        let mut request = edit_request(parent_id);
        request.user_turn_index = 1;
        request.original_text = Some("first prompt".to_string());
        rig.supervisor
            .claude_edit_in_place_ladder(request, None)
            .await;

        let mut saw_honest_failure = false;
        while let Ok(event) = bus_rx.try_recv() {
            match event {
                AppEvent::SessionForkResult { .. } => panic!("first-message edit must not fork"),
                AppEvent::UserMessageEditStatus {
                    status, message, ..
                } if status == "failed" => {
                    assert!(message.contains("first message"), "{message}");
                    saw_honest_failure = true;
                }
                _ => {}
            }
        }
        assert!(saw_honest_failure, "the failure must be reported");
        assert_eq!(
            std::fs::read_to_string(&rig.transcript).unwrap(),
            before,
            "the transcript must be untouched"
        );
    }
}

//! `AppEvent` → `PeerEvent` upcaster.
//!
//! Translates Intendant's internal [`crate::event::AppEvent`] stream
//! into the transport-neutral [`PeerEvent`] vocabulary so the
//! federation layer can render a peer Intendant's activity uniformly
//! alongside non-Intendant peers (OpenClaw, Hermes, A2A). Used by the
//! `IntendantWsTransport` to map the full-fidelity wire stream a peer
//! Intendant emits into the lean `PeerEvent` shape consumers can
//! handle without knowing the source type.
//!
//! ## Why a struct, not a free function
//!
//! Streaming model output is the key driver. `AppEvent::ModelResponseDelta`
//! chunks don't carry a turn number or any other natural correlation
//! field, so a stateless mapping can't tell the receiver "these
//! deltas all belong to the same message." The upcaster tracks the
//! current turn's streaming message ID in its own state, then reuses
//! it across deltas and stamps the final `ModelResponse` with the
//! same ID — the receiving dashboard can aggregate cleanly. A
//! stateless `fn(AppEvent) -> Vec<PeerEvent>` would either drop the
//! ID problem on the consumer or force every transport to carry its
//! own sequencing state.
//!
//! ## Return shape
//!
//! `upcast` returns `Vec<PeerEvent>` because the 1:N fan-out is
//! genuine: `AppEvent::ModelResponse` naturally produces a Message
//! (completed) *and* a Usage snapshot (token accounting) *and* (when
//! reasoning is present) a second Message with the reasoning content.
//! Events that have no federation-relevant content (`Tick`, `Key`,
//! `Resize`, `ControlCommand`, high-frequency `DisplayMetrics`)
//! return an empty vec and are dropped.
//!
//! ## Policy notes
//!
//! - LogLevel mapping: Intendant's internal `LogLevel` has more
//!   variants than the peer vocabulary (Model, Agent, SubAgent,
//!   Detail). These are all source-specific and collapse to `Info`
//!   or `Debug` on the wire — the `source` field on the peer Log
//!   event carries the differentiation.
//! - ActionCategory → string: peer's `ApprovalRequest.category` is
//!   deliberately free-form because non-Intendant peers have
//!   different category vocabularies. Intendant's own categories
//!   serialize as lowercase snake_case names.
//! - DisplayMetrics: dropped entirely. It's a high-frequency metric
//!   stream, not an event, and the federation layer doesn't need
//!   per-peer display metrics visible in the aggregate feed.

use crate::app_state_pricing::{estimate_live_usage_cost, estimate_session_cost, LiveUsageTokens};
use crate::event::AppEvent;
use crate::peer::{
    ActivityId, ActivityKind, ActivityOutcome, ApprovalDecision, ApprovalRequest, Capability,
    LogLevel, MessageContent, MessageId, MessageRole, ModelUsage, PeerDisplayInfo, PeerEvent,
    PeerStatus, SessionInfo, TaskId, UsageSnapshot,
};
use crate::types::OutboundEvent;

// ---------------------------------------------------------------------------
// Shared stateless helpers
// ---------------------------------------------------------------------------
//
// Both upcasters consume the same peer vocabulary, so the small
// translation primitives (log level mapping, approval decision
// parsing, status phase mapping, etc.) live at module scope as
// `pub(crate)` functions. Factoring them out is the main defense
// against drift: if one upcaster starts interpreting "warn" or
// "waiting_approval" differently from the other, a parity test
// fires and points at the exact helper that needs fixing.

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(crate) fn log_event(level: LogLevel, source: &str, message: String) -> PeerEvent {
    PeerEvent::Log {
        level,
        source: source.to_string(),
        message,
        ts: now_rfc3339(),
    }
}

/// Peer-facing text for a display-only session note: the note body plus a
/// count marker for attachments, whose `/api/session/current/uploads/*`
/// URLs only resolve against the origin daemon.
pub(crate) fn session_note_peer_log_text(text: &str, attachment_count: usize) -> String {
    match attachment_count {
        0 => text.to_string(),
        1 => format!("{text} [1 image attachment]"),
        n => format!("{text} [{n} image attachments]"),
    }
}

/// Peer-facing text for an agent→user notification: `title: text`, with an
/// urgency marker for the escalated levels.
pub(crate) fn user_notification_peer_log_text(
    title: Option<&str>,
    text: &str,
    urgency: crate::types::NotificationUrgency,
) -> String {
    let body = match title {
        Some(title) => format!("{title}: {text}"),
        None => text.to_string(),
    };
    match urgency {
        crate::types::NotificationUrgency::Info => body,
        other => format!("[{}] {body}", other.as_str()),
    }
}

/// Urgent notifications surface as warnings in the peer activity log;
/// everything else is plain info.
pub(crate) fn user_notification_peer_log_level(
    urgency: crate::types::NotificationUrgency,
) -> LogLevel {
    match urgency {
        crate::types::NotificationUrgency::Urgent => LogLevel::Warn,
        _ => LogLevel::Info,
    }
}

/// Map Intendant's internal multi-source `LogLevel` to the peer
/// module's 5-level vocabulary. Source-specific variants
/// (Model/Agent/SubAgent) collapse to `Info` because the peer Log
/// event has a separate `source` field that carries the
/// differentiation.
#[allow(dead_code)]
pub(crate) fn upcast_log_level(level: &crate::types::LogLevel) -> LogLevel {
    use crate::types::LogLevel as L;
    match level {
        L::Debug => LogLevel::Debug,
        L::Detail => LogLevel::Debug,
        L::Info | L::Model | L::Agent | L::SubAgent => LogLevel::Info,
        L::Warn => LogLevel::Warn,
        L::Error => LogLevel::Error,
    }
}

/// Map a wire-format log level string (as produced by
/// `OutboundEvent::LogEntry` and `OutboundEvent::PresenceLog`) to
/// the peer vocabulary. Same mapping table as `upcast_log_level`
/// but keyed on strings instead of the typed enum. Kept aligned
/// with `upcast_log_level` by the parity tests.
pub(crate) fn wire_log_level(s: &str) -> LogLevel {
    match s {
        "trace" => LogLevel::Trace,
        "debug" | "detail" => LogLevel::Debug,
        "info" | "model" | "agent" | "subagent" => LogLevel::Info,
        "warn" | "warning" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    }
}

/// Map Intendant's internal `ActionCategory` to a free-form string
/// for `ApprovalRequest.category`. Lowercase snake_case to match
/// the convention other autonomous daemons (OpenClaw) use for
/// category tags.
#[allow(dead_code)]
pub(crate) fn action_category_wire(cat: &crate::autonomy::ActionCategory) -> String {
    use crate::autonomy::ActionCategory as C;
    match cat {
        C::FileRead => "file_read",
        C::FileWrite => "file_write",
        C::FileDelete => "file_delete",
        C::CommandExec => "command_exec",
        C::NetworkRequest => "network_request",
        C::Destructive => "destructive",
        C::HumanInput => "human_input",
        C::LiveAudioSpawn => "live_audio_spawn",
        C::DisplayControl => "display_control",
        C::ToolCall => "tool_call",
    }
    .to_string()
}

/// Map the action string on `ApprovalResolved` (which is free-form
/// from the TUI's action labels or the `ApprovalResponse` variant
/// names) to a typed `ApprovalDecision`.
pub(crate) fn approval_decision_from_action(action: &str) -> ApprovalDecision {
    match action {
        "approve" | "accept" => ApprovalDecision::Accept,
        "approve_all" | "accept_for_session" | "approveall" => ApprovalDecision::AcceptForSession,
        "deny" | "decline" => ApprovalDecision::Decline,
        "skip" | "cancel" => ApprovalDecision::Cancel,
        _ => ApprovalDecision::Decline,
    }
}

/// Map the free-form `StatusUpdate.phase` / `OutboundEvent::Status.phase`
/// string to a typed `PeerStatus`. Unknown phases default to `Idle`
/// rather than `Unknown` because `Idle` is the more graceful render
/// when we're connected but don't recognize the phase label — the
/// peer is *there*, we just don't know what it's doing.
pub(crate) fn status_from_phase(phase: &str) -> PeerStatus {
    match phase {
        "idle" | "waiting_followup" | "done" => PeerStatus::Idle,
        "working" | "thinking" | "acting" | "executing" | "running" => PeerStatus::Working,
        "approval" | "waiting_approval" | "needs_approval" => PeerStatus::NeedsApproval,
        "error" | "failed" => PeerStatus::Error,
        _ => PeerStatus::Idle,
    }
}

// ---------------------------------------------------------------------------
// Per-session fold — the consuming-side enrichment of SessionInfo
// ---------------------------------------------------------------------------

/// Cap on distinct sessions tracked per upcaster. `SessionEnded` prunes
/// the normal flow; the cap only bounds a peer that keeps announcing
/// sessions without ever ending them. Evicts the oldest `started_at`
/// when exceeded.
pub(crate) const MAX_TRACKED_PEER_SESSIONS: usize = 128;

/// Folds a peer's per-session event stream (started / identity /
/// status / relationship / goal / vitals / usage / approvals) into
/// [`SessionInfo`] snapshots, so consumers see one idempotent
/// `SessionUpdated` stream instead of six event shapes. Shared by both
/// upcasters — the same drift defense as the stateless helpers above.
///
/// Update methods **upsert**: an event naming an unknown session
/// creates its entry (`started_at` = now). That is what makes a
/// primary that connected mid-flight self-healing — it learns the
/// peer's pre-existing sessions from their first live event instead
/// of never showing them.
#[derive(Default)]
pub(crate) struct PeerSessionFold {
    sessions: std::collections::BTreeMap<String, SessionInfo>,
    /// Pending approval id → session id, so `needs_approval` means
    /// "at least one pending approval names this session".
    pending_approvals: std::collections::BTreeMap<u64, String>,
    /// The peer daemon's own primary session id, when known (the
    /// native transport learns it from the `/ws` bootstrap
    /// `state_snapshot` frame). Sessions matching it are stamped
    /// `is_primary` so renderers can merge them into the peer node.
    primary_session_id: Option<String>,
}

impl PeerSessionFold {
    pub(crate) fn set_primary_session_id(&mut self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        self.primary_session_id = Some(session_id.to_string());
        // Retro-stamp in case the session was learned before the
        // bootstrap frame was parsed (ordering is connection-dependent).
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.is_primary = true;
        }
    }

    /// Insert-or-refresh on a session-started announcement. Returns the
    /// current snapshot to embed in the `SessionStarted` event.
    fn started(&mut self, session_id: &str, label: Option<&str>) -> SessionInfo {
        let entry = self.entry(session_id);
        if let Some(label) = label {
            if !label.is_empty() {
                entry.label = Some(label.to_string());
            }
        }
        let snapshot = entry.clone();
        self.evict_over_cap(session_id);
        snapshot
    }

    /// Upsert + change-detect. Returns `Some(snapshot)` only when
    /// `apply` changed something, so chatty sources (per-tick status,
    /// per-turn usage) emit `SessionUpdated` only on real transitions.
    fn update(
        &mut self,
        session_id: &str,
        apply: impl FnOnce(&mut SessionInfo),
    ) -> Option<SessionInfo> {
        if session_id.trim().is_empty() {
            return None;
        }
        let created = !self.sessions.contains_key(session_id);
        let entry = self.entry(session_id);
        let before = entry.clone();
        apply(entry);
        let changed = created || *entry != before;
        let snapshot = changed.then(|| entry.clone());
        self.evict_over_cap(session_id);
        snapshot
    }

    /// A pending approval was raised. When it names a session, that
    /// session's `needs_approval` flips on; returns the snapshot if
    /// that changed anything.
    fn approval_requested(&mut self, id: u64, session_id: Option<&str>) -> Option<SessionInfo> {
        let session_id = session_id.unwrap_or("").trim().to_string();
        if session_id.is_empty() {
            return None;
        }
        self.pending_approvals.insert(id, session_id.clone());
        self.update(&session_id, |s| s.needs_approval = true)
    }

    /// A pending approval was resolved; recompute `needs_approval` for
    /// the session it named (other approvals may still be pending).
    fn approval_resolved(&mut self, id: u64) -> Option<SessionInfo> {
        let session_id = self.pending_approvals.remove(&id)?;
        let still_pending = self
            .pending_approvals
            .values()
            .any(|sid| *sid == session_id);
        self.update(&session_id, |s| s.needs_approval = still_pending)
    }

    fn ended(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
        self.pending_approvals.retain(|_, sid| sid != session_id);
    }

    /// Phase transition from a session-scoped lifecycle event
    /// (TurnStarted/AgentStarted → working, DoneSignal/TaskComplete →
    /// done). These are the signals native daemon-lane sessions
    /// actually emit — dedicated per-session `status` events exist
    /// only on the primary rail — so folding them is what gives
    /// remote native sessions live phases at all.
    fn phase(&mut self, session_id: Option<&str>, phase: &str) -> Option<SessionInfo> {
        let session_id = session_id.unwrap_or("");
        if session_id.trim().is_empty() {
            return None;
        }
        self.update(session_id, |s| s.phase = phase.to_string())
    }

    fn entry(&mut self, session_id: &str) -> &mut SessionInfo {
        let is_primary = self.primary_session_id.as_deref() == Some(session_id);
        self.sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionInfo {
                session_id: session_id.to_string(),
                started_at: now_rfc3339(),
                is_primary,
                ..SessionInfo::default()
            })
    }

    /// Evict the oldest-started session (never the one just touched)
    /// when over cap. `started_at` is RFC3339, so lexicographic order
    /// is chronological.
    fn evict_over_cap(&mut self, just_touched: &str) {
        while self.sessions.len() > MAX_TRACKED_PEER_SESSIONS {
            let oldest = self
                .sessions
                .iter()
                .filter(|(id, _)| id.as_str() != just_touched)
                .min_by(|a, b| a.1.started_at.cmp(&b.1.started_at))
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => self.ended(&id),
                None => break,
            }
        }
    }
}

/// Wrap a fold change (if any) as a single-element `SessionUpdated`
/// event vec — the common tail of every enrichment arm.
pub(crate) fn session_updated_events(changed: Option<SessionInfo>) -> Vec<PeerEvent> {
    changed
        .map(|session| PeerEvent::SessionUpdated { session })
        .into_iter()
        .collect()
}

/// Cap on distinct displays tracked per upcaster. `display_capture_lost`
/// / `user_display_revoked` prune the normal flow; the cap only bounds a
/// peer that keeps announcing new display ids without ever losing them.
/// Unlike sessions there is no timestamp to age by, so at the cap new
/// ids are refused (existing ids keep updating) — a paired peer with 64
/// concurrent live displays is not a real topology.
pub(crate) const MAX_TRACKED_PEER_DISPLAYS: usize = 64;

/// Folds a peer's display-availability wire events (`display_ready`,
/// `display_resize`, `display_capture_lost`, `user_display_revoked`)
/// into change-only [`PeerEvent::DisplayReady`] / [`PeerEvent::DisplayLost`]
/// emissions. Change-only matters because the peer's gateway replays
/// `display_ready` for every active display on each transport
/// (re)connect — repeats are the common case, and consumers should see
/// one idempotent stream. Shared by both upcasters, same drift defense
/// as [`PeerSessionFold`].
#[derive(Default)]
pub(crate) struct PeerDisplayFold {
    displays: std::collections::BTreeMap<u32, PeerDisplayInfo>,
}

impl PeerDisplayFold {
    /// Upsert from a `display_ready`; emits only when the display is new
    /// or its geometry actually changed. `agent_visible == false` marks a
    /// private user view — those are never announced to peers (a peer's
    /// agents and dashboards are not the granting user), and one that was
    /// previously tracked as shared retires with a `DisplayLost`.
    fn ready(
        &mut self,
        display_id: u32,
        width: u32,
        height: u32,
        agent_visible: bool,
    ) -> Vec<PeerEvent> {
        if !agent_visible {
            return self.lost(display_id, Some("private user view".to_string()));
        }
        let info = PeerDisplayInfo {
            display_id,
            width,
            height,
        };
        if !self.displays.contains_key(&display_id)
            && self.displays.len() >= MAX_TRACKED_PEER_DISPLAYS
        {
            return vec![];
        }
        let changed = self.displays.get(&display_id) != Some(&info);
        self.displays.insert(display_id, info.clone());
        if changed {
            vec![PeerEvent::DisplayReady { display: info }]
        } else {
            vec![]
        }
    }

    /// Geometry update from a `display_resize`. Only updates displays
    /// already tracked: resize events carry no visibility, so letting
    /// them upsert would resurrect a private view (or a lost display)
    /// from its resize traffic.
    fn resize(&mut self, display_id: u32, width: u32, height: u32) -> Vec<PeerEvent> {
        if !self.displays.contains_key(&display_id) {
            return vec![];
        }
        self.ready(display_id, width, height, true)
    }

    /// Retire a display; emits only if it was actually tracked (a
    /// `capture_lost` for a display this connection never saw
    /// announces nothing).
    fn lost(&mut self, display_id: u32, reason: Option<String>) -> Vec<PeerEvent> {
        if self.displays.remove(&display_id).is_some() {
            vec![PeerEvent::DisplayLost { display_id, reason }]
        } else {
            vec![]
        }
    }
}

// ---------------------------------------------------------------------------
// AppEventUpcaster — in-process AppEvent → PeerEvent
// ---------------------------------------------------------------------------

/// Stateful `AppEvent` → `PeerEvent` upcaster.
#[allow(dead_code)]
pub struct AppEventUpcaster {
    /// Monotonic counter for synthesizing stable IDs when the source
    /// event doesn't carry a natural one.
    seq: u64,
    /// The current streaming-message ID. Seeded by `TurnStarted` so
    /// subsequent `ModelResponseDelta` chunks and the final
    /// `ModelResponse` within one turn all share it. Cleared by
    /// `ModelResponse` (end of the model's stream for this turn),
    /// `DoneSignal` (end of the whole turn), or a new `TurnStarted`.
    /// Deltas that arrive without a prior `TurnStarted` synthesize a
    /// fresh seq-based ID and store it here so follow-up deltas reuse it.
    current_message_id: Option<MessageId>,
    /// Tracks the turn number of the currently-in-flight model turn
    /// activity. Set by `TurnStarted`; consumed by `DoneSignal` /
    /// `TaskComplete` / the next `TurnStarted` to emit a matching
    /// `ActivityCompleted` with the same id the Started event used.
    /// Without this, activities start as `turn-{turn}` but complete
    /// as `done-{seq}` — observers can't correlate start and end.
    current_turn: Option<usize>,
    /// Same tracking for in-flight agent (tool call) activities.
    /// `AgentStarted` carries the turn number; `AgentOutput` and
    /// the implicit completion (next `AgentStarted`, `DoneSignal`,
    /// or `TaskComplete`) reuse it so the progress/complete events
    /// match the started one. Without this, agents start as
    /// `agent-{turn}` but progress as `agent-latest`.
    current_agent_turn: Option<usize>,
    /// Per-session enrichment fold (see [`PeerSessionFold`]).
    sessions: PeerSessionFold,
    /// Display-availability fold (see [`PeerDisplayFold`]).
    displays: PeerDisplayFold,
}

impl Default for AppEventUpcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl AppEventUpcaster {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            seq: 0,
            current_message_id: None,
            current_turn: None,
            current_agent_turn: None,
            sessions: PeerSessionFold::default(),
            displays: PeerDisplayFold::default(),
        }
    }

    /// Drain any in-flight agent activity before starting a new one
    /// or closing out a turn. Shared helper because the same cleanup
    /// logic runs from `AgentStarted` (close previous agent before
    /// this one begins), `DoneSignal`, `TaskComplete`, and `TurnStarted`
    /// (defensive, in case the agent wasn't explicitly closed).
    ///
    /// `outcome` is the outcome to stamp on the emitted
    /// `ActivityCompleted`. Success for the normal signals
    /// (DoneSignal, defensive closes from TurnStarted/AgentStarted)
    /// since in those cases we have no reason to believe the agent
    /// failed. TaskComplete propagates its own outcome — a failed
    /// task means the in-flight agent is failing too, so we don't
    /// want to stamp it Success alongside a failed turn.
    #[allow(dead_code)]
    fn close_pending_agent(&mut self, outcome: ActivityOutcome) -> Option<PeerEvent> {
        self.current_agent_turn
            .take()
            .map(|turn| PeerEvent::ActivityCompleted {
                id: ActivityId(format!("agent-{turn}")),
                outcome,
            })
    }

    /// Drain any in-flight turn activity. Called from `DoneSignal`,
    /// `TaskComplete`, and the next `TurnStarted` (defensive).
    #[allow(dead_code)]
    fn close_pending_turn(&mut self, outcome: ActivityOutcome) -> Option<PeerEvent> {
        self.current_turn
            .take()
            .map(|turn| PeerEvent::ActivityCompleted {
                id: ActivityId(format!("turn-{turn}")),
                outcome,
            })
    }

    #[allow(dead_code)]
    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    /// Return the current message ID, creating a fresh seq-based one
    /// (and storing it) if no stream is in flight. Used by both
    /// `ModelResponseDelta` and `ModelResponse` so they share state
    /// seamlessly — whichever arrives first seeds the ID, subsequent
    /// events reuse it.
    #[allow(dead_code)]
    fn current_or_new_message_id(&mut self) -> MessageId {
        if let Some(id) = &self.current_message_id {
            return id.clone();
        }
        let seq = self.next_seq();
        let id = MessageId(format!("msg-seq-{seq}"));
        self.current_message_id = Some(id.clone());
        id
    }

    /// Map an `AppEvent` to zero or more `PeerEvent`s.
    #[allow(dead_code)]
    pub fn upcast(&mut self, event: &AppEvent) -> Vec<PeerEvent> {
        match event {
            // ---- Dropped internal events ----
            AppEvent::Tick
            | AppEvent::ControlCommand(_)
            | AppEvent::DisplayMetrics { .. }
            // Hub-internal: peers see the folded SessionVitals instead.
            | AppEvent::SessionActivity { .. }
            | AppEvent::SessionConfigFacts { .. }
            | AppEvent::ContextSnapshot { .. }
            | AppEvent::CodexThreadActionRequested { .. }
            | AppEvent::ExternalFollowUpRequested { .. }
            | AppEvent::FollowUpCancelRequested { .. }
            | AppEvent::SessionStopRequested { .. }
            | AppEvent::SessionCapabilities { .. }
            | AppEvent::FollowUpStatus { .. }
            | AppEvent::SharedView { .. }
            // Display requests are an owner-surface doorbell on THIS
            // daemon's dashboards; the peer rail deliberately does not
            // carry them (peers resolve nothing here).
            | AppEvent::DisplayRequestRaised { .. }
            | AppEvent::DisplayRequestResolved { .. }
            // Live CU action overlays are a LOCAL-dashboard presentation
            // lane; peer viewers don't render them (documented follow-up
            // in docs/src/computer-use-and-audio.md).
            | AppEvent::CuActionExecuted { .. }
            | AppEvent::BrowserWorkspaceChanged { .. }
            | AppEvent::SessionRenameResult { .. }
            | AppEvent::SessionAgentConfigResult { .. }
            | AppEvent::FileChanged { .. }
            | AppEvent::SessionFileActivity { .. }
            | AppEvent::UploadReady { .. }
            | AppEvent::UploadDeleted { .. }
            | AppEvent::SnapshotCreated { .. }
            | AppEvent::RolledBack { .. }
            | AppEvent::Redone { .. }
            | AppEvent::HistoryPruned { .. }
            | AppEvent::ConversationRollbackRequested { .. }
            | AppEvent::ConversationRolledBack { .. }
            // The agenda is home-scoped daemon state; the peer rail does
            // not carry another daemon's ledger (ratified v1 scope).
            | AppEvent::AgendaChanged { .. }
            | AppEvent::MemoryChanged { .. } => vec![],

            AppEvent::UserMessageEditStatus {
                user_turn_index,
                status,
                message,
                ..
            } => vec![log_event(
                if status == "failed" {
                    LogLevel::Error
                } else {
                    LogLevel::Info
                },
                "edit",
                format!("Edit user turn {user_turn_index} {status}: {message}"),
            )],

            AppEvent::CodexThreadActionResult {
                action,
                success,
                message,
                ..
            } => vec![log_event(
                if *success {
                    LogLevel::Info
                } else {
                    LogLevel::Warn
                },
                "codex-action",
                if *success {
                    format!("/{}: {}", action, message)
                } else {
                    format!("/{}: FAILED — {}", action, message)
                },
            )],

            // ---- Turn lifecycle ----
            AppEvent::TurnStarted {
                session_id, turn, ..
            } => {
                // Defensive cleanup: if a previous turn/agent never
                // closed explicitly (because the source dropped
                // DoneSignal, or emitted TurnStarted without a prior
                // closer), close them here so observers see a
                // consistent start/complete pairing instead of
                // orphaned Started events.
                let mut out = vec![];
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(ActivityOutcome::Success) {
                    out.push(closed);
                }
                // Seed the shared message ID for this turn so subsequent
                // deltas and the final ModelResponse all line up on it.
                self.current_message_id = Some(MessageId(format!("msg-turn-{turn}")));
                self.current_turn = Some(*turn);
                out.push(PeerEvent::ActivityStarted {
                    id: ActivityId(format!("turn-{turn}")),
                    kind: ActivityKind::ModelTurn,
                    label: format!("turn {turn}"),
                });
                out.extend(session_updated_events(
                    self.sessions.phase(session_id.as_deref(), "working"),
                ));
                out
            }

            AppEvent::ModelResponseDelta { text, .. } => {
                let id = self.current_or_new_message_id();
                vec![PeerEvent::Message {
                    id,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text { text: text.clone() },
                    partial: true,
                }]
            }

            AppEvent::ModelResponse {
                turn,
                content,
                usage,
                reasoning,
                source: _,
                ..
            } => {
                let msg_id = self.current_or_new_message_id();
                let mut out = vec![PeerEvent::Message {
                    id: msg_id,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text {
                        text: content.clone(),
                    },
                    partial: false,
                }];
                if let Some(reasoning_text) = reasoning {
                    out.push(PeerEvent::Message {
                        id: MessageId(format!("reasoning-turn-{turn}")),
                        role: MessageRole::Assistant,
                        content: MessageContent::Reasoning {
                            text: reasoning_text.clone(),
                        },
                        partial: false,
                    });
                }
                out.push(PeerEvent::Usage {
                    snapshot: UsageSnapshot {
                        tokens_in: usage.prompt_tokens,
                        tokens_out: usage.completion_tokens,
                        tokens_cached: usage.cached_tokens,
                        cost_usd: None,
                        by_model: vec![],
                    },
                });
                // End of the model's stream for this turn — clear so any
                // subsequent deltas (shouldn't happen, but be safe) start
                // a fresh message rather than silently reusing this ID.
                self.current_message_id = None;
                out
            }

            AppEvent::DoneSignal {
                session_id,
                message,
                ..
            } => {
                self.current_message_id = None;
                let mut out = vec![];
                // Close the in-flight agent first (if any), then the
                // turn. Both use the same ids their Started events
                // used so observers see matching start/complete pairs.
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(ActivityOutcome::Success) {
                    out.push(closed);
                } else {
                    // No turn tracked — DoneSignal arrived without a
                    // prior TurnStarted. Synthesize a completion so
                    // observers see *something*, but with a seq-based
                    // id since there's no turn to tie it to.
                    let seq = self.next_seq();
                    out.push(PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("done-{seq}")),
                        outcome: ActivityOutcome::Success,
                    });
                }
                if let Some(msg) = message {
                    out.push(log_event(LogLevel::Info, "agent", format!("done: {msg}")));
                }
                out.extend(session_updated_events(
                    self.sessions.phase(session_id.as_deref(), "done"),
                ));
                out
            }

            AppEvent::RoundComplete {
                round,
                turns_in_round,
                ..
            } => vec![log_event(
                LogLevel::Info,
                "agent",
                format!("round {round} complete ({turns_in_round} turns)"),
            )],

            // ---- Sub-agent / tool execution ----
            AppEvent::AgentStarted {
                session_id,
                turn,
                commands_preview,
                source,
                ..
            } => {
                let label = source.clone().unwrap_or_else(|| "agent".to_string());
                // Close any previous agent activity (defensive: if the
                // source emitted two AgentStarted events without an
                // intervening close signal, we don't want to leave an
                // orphaned Started event on the observer's feed).
                let mut out = vec![];
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                self.current_agent_turn = Some(*turn);
                out.push(PeerEvent::ActivityStarted {
                    id: ActivityId(format!("agent-{turn}")),
                    kind: ActivityKind::ToolCall,
                    label: format!("{label}: {commands_preview}"),
                });
                out.extend(session_updated_events(
                    self.sessions.phase(session_id.as_deref(), "working"),
                ));
                out
            }

            AppEvent::AgentOutput {
                stdout,
                stderr,
                source: _,
                ..
            } => {
                let mut out = vec![];
                if !stdout.is_empty() {
                    // Progress events reuse the id the matching
                    // AgentStarted used so observers can correlate
                    // them. If there's no tracked agent turn (output
                    // arrived without a prior AgentStarted — shouldn't
                    // happen but shouldn't crash either), fall back
                    // to a synthetic id so the event isn't dropped.
                    let id = match self.current_agent_turn {
                        Some(turn) => ActivityId(format!("agent-{turn}")),
                        None => {
                            let seq = self.next_seq();
                            ActivityId(format!("agent-orphan-{seq}"))
                        }
                    };
                    out.push(PeerEvent::ActivityProgress {
                        id,
                        text: Some(stdout.clone()),
                    });
                }
                if !stderr.is_empty() {
                    out.push(log_event(LogLevel::Warn, "agent", stderr.clone()));
                }
                out
            }

            AppEvent::SubAgentResult { formatted } => {
                let seq = self.next_seq();
                vec![
                    PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("subagent-{seq}")),
                        outcome: ActivityOutcome::Success,
                    },
                    log_event(LogLevel::Info, "subagent", formatted.clone()),
                ]
            }

            AppEvent::ContextManagement { turn } => vec![log_event(
                LogLevel::Debug,
                "context",
                format!("context management turn {turn}"),
            )],

            AppEvent::TaskComplete {
                session_id,
                reason,
                summary,
            } => {
                let outcome = match reason.as_str() {
                    "success" | "done" | "completed" => ActivityOutcome::Success,
                    "cancelled" | "canceled" => ActivityOutcome::Cancelled,
                    other => ActivityOutcome::Failed {
                        message: other.to_string(),
                    },
                };
                let mut out = vec![];
                // Close the in-flight agent and turn with the task's
                // outcome so the end-of-task state propagates through
                // the entire activity lifecycle. A failed TaskComplete
                // must *not* stamp the agent as Success — that would
                // produce contradictory completions (agent success,
                // turn failed) in the consumer's feed.
                if let Some(closed) = self.close_pending_agent(outcome.clone()) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(outcome.clone()) {
                    out.push(closed);
                } else {
                    // No turn in flight — synthesize a task-level
                    // completion. Happens for direct-mode single-turn
                    // runs where TaskComplete is the only lifecycle
                    // signal.
                    let seq = self.next_seq();
                    out.push(PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("task-{seq}")),
                        outcome,
                    });
                }
                self.current_message_id = None;
                if let Some(s) = summary {
                    out.push(log_event(LogLevel::Info, "task", s.clone()));
                }
                out.extend(session_updated_events(
                    self.sessions.phase(session_id.as_deref(), "done"),
                ));
                out
            }

            // ---- Session lifecycle ----
            AppEvent::SessionStarted { session_id, task } => {
                vec![PeerEvent::SessionStarted {
                    session: self.sessions.started(session_id, task.as_deref()),
                }]
            }

            AppEvent::SessionIdentity {
                session_id, source, ..
            } => session_updated_events(
                self.sessions
                    .update(session_id, |s| s.source = source.clone()),
            ),

            AppEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ephemeral,
            } => session_updated_events(self.sessions.update(child_session_id, |s| {
                s.parent_session_id = Some(parent_session_id.clone());
                s.relationship = relationship.clone();
                s.ephemeral = *ephemeral;
            })),

            AppEvent::SessionForkResult {
                parent_session_id,
                child_session_id,
                error,
                ..
            } => vec![log_event(
                LogLevel::Info,
                "session",
                match (child_session_id, error) {
                    (Some(child), _) => {
                        format!("session fork: {parent_session_id} -> {child}")
                    }
                    (_, Some(error)) => {
                        format!("session fork of {parent_session_id} failed: {error}")
                    }
                    _ => format!("session fork of {parent_session_id} completed"),
                },
            )],

            AppEvent::SessionGoal { session_id, goal } => session_updated_events(
                self.sessions.update(session_id, |s| s.goal = goal.clone()),
            ),

            AppEvent::SessionVitals { session_id, vitals } => session_updated_events(
                self.sessions
                    .update(session_id, |s| s.vitals = Some(vitals.clone())),
            ),

            AppEvent::SessionAttached { session_id, source } => vec![log_event(
                LogLevel::Info,
                "session",
                format!("session attached: {} ({})", session_id, source),
            )],

            // This daemon acknowledged an inbound peer delegation.
            // Narrate on the log rail only — the receipt's correlation
            // job happens on the *delegating* side (its wire upcaster
            // maps `OutboundEvent::TaskReceived` to
            // `PeerEvent::TaskReceipt`); locally the accepted task
            // already narrates itself via SessionStarted.
            AppEvent::TaskReceived {
                delegation_id,
                session_id,
            } => vec![log_event(
                LogLevel::Info,
                "session",
                format!(
                    "accepted delegated task {} as session {}",
                    delegation_id, session_id
                ),
            )],

            AppEvent::SessionEnded {
                session_id, reason, ..
            } => {
                self.sessions.ended(session_id);
                vec![PeerEvent::SessionEnded {
                    session_id: session_id.clone(),
                    reason: reason.clone(),
                }]
            }

            AppEvent::SessionDirChanged { path } => vec![log_event(
                LogLevel::Info,
                "session",
                format!("session dir → {}", path.display()),
            )],

            // ---- Approval flow ----
            AppEvent::ApprovalRequired {
                session_id,
                id,
                command_preview,
                category,
            } => {
                let mut out = vec![PeerEvent::ApprovalRequested {
                    request: ApprovalRequest {
                        request_id: id.to_string(),
                        category: action_category_wire(category),
                        preview: command_preview.clone(),
                        auto_resolvable: false,
                    },
                }];
                out.extend(session_updated_events(
                    self.sessions.approval_requested(*id, session_id.as_deref()),
                ));
                out
            }

            // Structured questions flatten to the approval vocabulary for
            // peers (same treatment as askHuman below): the preview names
            // the options, and a peer-side Accept/Decline maps to the
            // question's proceed-without-answer / dismiss paths. Questions
            // share the approval id space and resolve via ApprovalResolved,
            // so they feed the session fold's `needs_approval` the same way.
            AppEvent::UserQuestionRequired {
                session_id,
                id,
                questions,
            } => {
                let mut out = vec![PeerEvent::ApprovalRequested {
                    request: ApprovalRequest {
                        request_id: id.to_string(),
                        category: "human_question".to_string(),
                        preview: crate::external_output::user_question_preview(questions),
                        auto_resolvable: false,
                    },
                }];
                out.extend(session_updated_events(
                    self.sessions.approval_requested(*id, session_id.as_deref()),
                ));
                out
            }

            AppEvent::ApprovalResolved { id, action, .. } => {
                let mut out = vec![PeerEvent::ApprovalResolved {
                    request_id: id.to_string(),
                    decision: approval_decision_from_action(action),
                }];
                out.extend(session_updated_events(self.sessions.approval_resolved(*id)));
                out
            }

            AppEvent::AutoApproved { preview } => {
                let seq = self.next_seq();
                vec![
                    PeerEvent::ApprovalRequested {
                        request: ApprovalRequest {
                            request_id: format!("auto-{seq}"),
                            category: "auto".to_string(),
                            preview: preview.clone(),
                            auto_resolvable: true,
                        },
                    },
                    PeerEvent::ApprovalResolved {
                        request_id: format!("auto-{seq}"),
                        decision: ApprovalDecision::Accept,
                    },
                ]
            }

            AppEvent::HumanQuestionDetected { question } => {
                let seq = self.next_seq();
                vec![PeerEvent::ApprovalRequested {
                    request: ApprovalRequest {
                        request_id: format!("human-{seq}"),
                        category: "human_question".to_string(),
                        preview: question.clone(),
                        auto_resolvable: false,
                    },
                }]
            }

            AppEvent::HumanResponseSent => vec![log_event(
                LogLevel::Info,
                "human",
                "human response sent".to_string(),
            )],

            // ---- Display capability ----
            AppEvent::DisplayReady {
                display_id,
                width,
                height,
                agent_visible,
            } => self
                .displays
                .ready(*display_id, *width, *height, *agent_visible),

            AppEvent::DisplayResize {
                display_id,
                width,
                height,
            } => {
                let mut events = vec![log_event(
                    LogLevel::Info,
                    "display",
                    format!("display {display_id} resized to {width}x{height}"),
                )];
                events.extend(self.displays.resize(*display_id, *width, *height));
                events
            }

            AppEvent::DisplayTaken { display_id } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Display,
                detail: serde_json::json!({ "display_id": display_id, "state": "taken" }),
            }],

            AppEvent::DisplayReleased {
                display_id: _,
                note,
            } => vec![PeerEvent::CapabilityReleased {
                capability: Capability::Display,
                reason: note.clone(),
            }],

            AppEvent::DisplayCaptureLost { display_id, reason } => self
                .displays
                .lost(*display_id, Some(format!("capture_lost: {reason}"))),

            AppEvent::DisplayApprovalPending {
                display_id: _,
                backend,
            } => vec![log_event(
                LogLevel::Info,
                "display",
                format!("display approval pending on {backend}"),
            )],

            // Private views ride the same event with agent_visible=false;
            // peers are not told about them at all (not even a log line —
            // "the owner is privately viewing their screen" is nobody
            // else's telemetry).
            AppEvent::UserDisplayGranted {
                display_id,
                agent_visible,
            } => {
                if *agent_visible {
                    vec![log_event(
                        LogLevel::Info,
                        "display",
                        format!("user granted display {display_id}"),
                    )]
                } else {
                    vec![]
                }
            }

            AppEvent::UserDisplayRevoked { display_id, note } => {
                let note_str = note.as_deref().unwrap_or("");
                let mut events = vec![log_event(
                    LogLevel::Info,
                    "display",
                    format!("user revoked display {display_id}: {note_str}"),
                )];
                events.extend(self.displays.lost(
                    *display_id,
                    Some(note.clone().unwrap_or_else(|| "user display revoked".to_string())),
                ));
                events
            }

            AppEvent::DebugScreenReady { display_id } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Display,
                detail: serde_json::json!({
                    "display_id": display_id,
                    "kind": "debug_screen",
                }),
            }],

            AppEvent::DebugScreenTornDown { display_id: _ } => {
                vec![PeerEvent::CapabilityReleased {
                    capability: Capability::Display,
                    reason: Some("debug_screen_torn_down".to_string()),
                }]
            }

            // ---- Recording capability ----
            AppEvent::RecordingStarted { stream_name } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Recording,
                detail: serde_json::json!({ "stream": stream_name }),
            }],

            AppEvent::RecordingStopped { stream_name } => vec![PeerEvent::CapabilityReleased {
                capability: Capability::Recording,
                reason: Some(format!("stopped: {stream_name}")),
            }],

            AppEvent::RecordingError {
                stream_name,
                message,
            } => vec![log_event(
                LogLevel::Error,
                "recording",
                format!("{stream_name}: {message}"),
            )],

            AppEvent::RecordingDeleted { stream_name } => vec![log_event(
                LogLevel::Info,
                "recording",
                format!("{stream_name} deleted"),
            )],

            // ---- Presence / voice ----
            AppEvent::PresenceConnected { .. } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Voice,
                detail: serde_json::json!({ "kind": "presence" }),
            }],

            AppEvent::PresenceDisconnected => vec![PeerEvent::CapabilityReleased {
                capability: Capability::Voice,
                reason: Some("presence_disconnected".to_string()),
            }],

            AppEvent::PresenceReady => vec![log_event(
                LogLevel::Info,
                "presence",
                "presence ready".to_string(),
            )],

            AppEvent::PresenceLog {
                message,
                level,
                turn: _,
            } => {
                let lvl = level
                    .as_ref()
                    .map(upcast_log_level)
                    .unwrap_or(LogLevel::Info);
                vec![log_event(lvl, "presence", message.clone())]
            }

            AppEvent::PresenceCheckpointReceived {
                summary,
                last_event_seq,
            } => vec![log_event(
                LogLevel::Info,
                "presence",
                format!("checkpoint at seq {last_event_seq}: {summary}"),
            )],

            AppEvent::VoiceLog {
                text,
                seq: _,
                tool_context: _,
            } => vec![log_event(LogLevel::Info, "voice", text.clone())],

            AppEvent::VoiceDiagnostic { kind, detail } => vec![log_event(
                LogLevel::Warn,
                "voice",
                format!("{kind}: {detail}"),
            )],

            AppEvent::UserTranscript { text, seq: _ } => {
                let seq = self.next_seq();
                vec![PeerEvent::Message {
                    id: MessageId(format!("user-transcript-{seq}")),
                    role: MessageRole::User,
                    content: MessageContent::Text { text: text.clone() },
                    partial: false,
                }]
            }

            AppEvent::LiveAudioStarted { id, provider } => vec![PeerEvent::ActivityStarted {
                id: ActivityId(format!("live-audio-{id}")),
                kind: ActivityKind::Other,
                label: format!("live audio ({provider})"),
            }],

            AppEvent::LiveAudioProgress {
                id,
                state,
                elapsed_secs: _,
                transcript_preview,
            } => vec![PeerEvent::ActivityProgress {
                id: ActivityId(format!("live-audio-{id}")),
                text: Some(format!("{state}: {transcript_preview}")),
            }],

            AppEvent::LiveAudioCompleted {
                id,
                status,
                quarantine_count,
            } => {
                let outcome = if status == "ok" || status == "success" {
                    ActivityOutcome::Success
                } else {
                    ActivityOutcome::Failed {
                        message: format!("{status} (quarantined: {quarantine_count})"),
                    }
                };
                vec![PeerEvent::ActivityCompleted {
                    id: ActivityId(format!("live-audio-{id}")),
                    outcome,
                }]
            }

            // ---- Usage accounting ----
            AppEvent::PresenceUsageUpdate {
                total_tokens: _,
                context_window: _,
                usage_pct: _,
                provider,
                model,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                cache_creation_tokens,
            } => {
                let cost_usd = estimate_session_cost(
                    model,
                    *prompt_tokens,
                    *completion_tokens,
                    *cached_tokens,
                    *cache_creation_tokens,
                );
                vec![PeerEvent::Usage {
                    snapshot: UsageSnapshot {
                        tokens_in: *prompt_tokens,
                        tokens_out: *completion_tokens,
                        tokens_cached: *cached_tokens,
                        cost_usd,
                        by_model: vec![ModelUsage {
                            provider: provider.clone(),
                            model: model.clone(),
                            tokens_in: *prompt_tokens,
                            tokens_out: *completion_tokens,
                            cost_usd,
                        }],
                    },
                }]
            }

            AppEvent::LiveUsageUpdate {
                provider,
                model,
                input_tokens,
                output_tokens,
                cached_tokens,
                total_tokens: _,
                thinking_tokens,
                input_text_tokens,
                input_audio_tokens,
                input_image_tokens,
                cached_text_tokens,
                cached_audio_tokens,
                cached_image_tokens,
                output_text_tokens,
                output_audio_tokens,
            } => {
                let cost_usd = estimate_live_usage_cost(
                    model,
                    LiveUsageTokens {
                        input_tokens: *input_tokens,
                        output_tokens: *output_tokens,
                        cached_tokens: *cached_tokens,
                        thinking_tokens: *thinking_tokens,
                        input_text_tokens: *input_text_tokens,
                        input_audio_tokens: *input_audio_tokens,
                        input_image_tokens: *input_image_tokens,
                        cached_text_tokens: *cached_text_tokens,
                        cached_audio_tokens: *cached_audio_tokens,
                        cached_image_tokens: *cached_image_tokens,
                        output_text_tokens: *output_text_tokens,
                        output_audio_tokens: *output_audio_tokens,
                    },
                );
                vec![PeerEvent::Usage {
                    snapshot: UsageSnapshot {
                        tokens_in: *input_tokens,
                        tokens_out: *output_tokens,
                        tokens_cached: *cached_tokens,
                        cost_usd,
                        by_model: vec![ModelUsage {
                            provider: provider.clone(),
                            model: model.clone(),
                            tokens_in: *input_tokens,
                            tokens_out: *output_tokens,
                            cost_usd,
                        }],
                    },
                }]
            }

            AppEvent::UsageSnapshot {
                session_id, main, ..
            } => {
                let cost_usd = estimate_session_cost(
                    &main.model,
                    main.prompt_tokens,
                    main.completion_tokens,
                    main.cached_tokens,
                    main.cache_creation_tokens,
                );
                let mut out = vec![PeerEvent::Usage {
                    snapshot: UsageSnapshot {
                        tokens_in: main.prompt_tokens,
                        tokens_out: main.completion_tokens,
                        tokens_cached: main.cached_tokens,
                        cost_usd,
                        by_model: vec![ModelUsage {
                            provider: main.provider.clone(),
                            model: main.model.clone(),
                            tokens_in: main.prompt_tokens,
                            tokens_out: main.completion_tokens,
                            cost_usd,
                        }],
                    },
                }];
                if let Some(sid) = session_id.as_deref() {
                    let tokens = main.prompt_tokens + main.completion_tokens;
                    out.extend(session_updated_events(
                        self.sessions.update(sid, |s| s.tokens_used = Some(tokens)),
                    ));
                }
                out
            }

            // ---- Status ----
            AppEvent::StatusUpdate {
                phase,
                session_id,
                task,
                ..
            } => {
                let mut out = vec![PeerEvent::StatusChanged {
                    status: status_from_phase(phase),
                }];
                out.extend(session_updated_events(self.sessions.update(
                    session_id,
                    |s| {
                        s.phase = phase.clone();
                        if s.label.as_deref().unwrap_or("").is_empty() && !task.is_empty() {
                            s.label = Some(task.clone());
                        }
                    },
                )));
                out
            }

            AppEvent::ExternalAgentChanged { agent } => vec![log_event(
                LogLevel::Info,
                "config",
                format!(
                    "external agent changed → {}",
                    agent.as_deref().unwrap_or("none")
                ),
            )],

            AppEvent::AutonomyChanged { autonomy } => vec![log_event(
                LogLevel::Info,
                "config",
                format!("autonomy changed → {autonomy}"),
            )],

            AppEvent::CodexConfigChanged {
                command,
                managed_command,
                managed_command_cleared,
                sandbox,
                approval_policy,
                model,
                model_cleared,
                reasoning_effort,
                reasoning_effort_cleared,
                service_tier,
                service_tier_cleared,
                web_search,
                network_access,
                writable_roots,
                managed_context,
                context_archive,
            } => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(v) = command {
                    parts.push(format!("command={v}"));
                }
                if let Some(v) = managed_command {
                    parts.push(format!("managed_command={v}"));
                } else if *managed_command_cleared {
                    parts.push("managed_command=<vanilla fallback>".to_string());
                }
                if let Some(v) = sandbox {
                    parts.push(format!("sandbox={v}"));
                }
                if let Some(v) = approval_policy {
                    parts.push(format!("approval_policy={v}"));
                }
                if let Some(v) = model {
                    parts.push(format!("model={v}"));
                } else if *model_cleared {
                    parts.push("model=<default>".to_string());
                }
                if let Some(v) = reasoning_effort {
                    parts.push(format!("reasoning_effort={v}"));
                } else if *reasoning_effort_cleared {
                    parts.push("reasoning_effort=<default>".to_string());
                }
                if let Some(v) = service_tier {
                    parts.push(format!("service_tier={v}"));
                } else if *service_tier_cleared {
                    parts.push("service_tier=<inherit>".to_string());
                }
                if let Some(v) = web_search {
                    parts.push(format!("web_search={v}"));
                }
                if let Some(v) = network_access {
                    parts.push(format!("network_access={v}"));
                }
                if let Some(v) = writable_roots {
                    parts.push(format!("writable_roots=[{} path(s)]", v.len()));
                }
                if let Some(v) = managed_context {
                    parts.push(format!("managed_context={v}"));
                }
                if let Some(v) = context_archive {
                    parts.push(format!("context_archive={v}"));
                }
                if parts.is_empty() {
                    vec![]
                } else {
                    vec![log_event(
                        LogLevel::Info,
                        "config",
                        format!("codex config: {}", parts.join(", ")),
                    )]
                }
            }

            AppEvent::ClaudeConfigChanged {
                model,
                model_cleared,
                permission_mode,
                allowed_tools,
            } => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(v) = model {
                    parts.push(format!("model={v}"));
                } else if *model_cleared {
                    parts.push("model=<default>".to_string());
                }
                if let Some(v) = permission_mode {
                    parts.push(format!("permission_mode={v}"));
                }
                if let Some(v) = allowed_tools {
                    parts.push(format!("allowed_tools=[{} entry/entries]", v.len()));
                }
                if parts.is_empty() {
                    vec![]
                } else {
                    vec![log_event(
                        LogLevel::Info,
                        "config",
                        format!("claude config: {}", parts.join(", ")),
                    )]
                }
            }

            // ---- Budget / safety ----
            AppEvent::BudgetWarning { pct, remaining } => vec![log_event(
                LogLevel::Warn,
                "budget",
                format!("budget warning: {pct:.1}% remaining={remaining}"),
            )],

            AppEvent::BudgetExhausted { remaining } => vec![log_event(
                LogLevel::Warn,
                "budget",
                format!("budget exhausted, remaining={remaining}"),
            )],

            AppEvent::SafetyCapReached => vec![log_event(
                LogLevel::Warn,
                "safety",
                "safety cap reached".to_string(),
            )],

            AppEvent::LoopError(msg) => vec![log_event(LogLevel::Error, "agent", msg.clone())],

            AppEvent::JsonExtracted { preview } => vec![log_event(
                LogLevel::Debug,
                "agent",
                format!("json: {preview}"),
            )],

            // ---- Log passthrough ----
            AppEvent::LogEntry {
                level,
                source,
                content,
                turn: _,
                ..
            } => {
                let log_level = match level.as_str() {
                    "trace" => LogLevel::Trace,
                    "debug" | "detail" => LogLevel::Debug,
                    "info" | "model" | "agent" | "subagent" => LogLevel::Info,
                    "warn" | "warning" => LogLevel::Warn,
                    "error" => LogLevel::Error,
                    _ => LogLevel::Info,
                };
                vec![log_event(log_level, source, content.clone())]
            }
            // Display-only session note: forward the text as peer log
            // activity. Attachment URLs are daemon-local (`/api/session/
            // current/uploads/.../raw` on the origin daemon), so peers get
            // a count marker instead of unreachable references.
            AppEvent::SessionNote {
                text,
                attachments,
                source,
                ..
            } => vec![log_event(
                LogLevel::Info,
                source.as_deref().unwrap_or("note"),
                session_note_peer_log_text(text, attachments.len()),
            )],
            // Fire-and-forget agent→user notification: forward as peer log
            // activity (the peer dashboard's own toast/attention machinery
            // is for its local daemon, not relayed sessions).
            AppEvent::UserNotification {
                title,
                text,
                urgency,
                ..
            } => vec![log_event(
                user_notification_peer_log_level(*urgency),
                "notify",
                user_notification_peer_log_text(title.as_deref(), text, *urgency),
            )],
            AppEvent::UserMessageRewind {
                user_turn_index,
                turns_removed,
                ..
            } => vec![log_event(
                LogLevel::Warn,
                "system",
                if *turns_removed == 1 {
                    format!("Rewound user turn {user_turn_index}")
                } else {
                    format!(
                        "Rewound user turn {user_turn_index} and {} later turns",
                        turns_removed.saturating_sub(1)
                    )
                },
            )],
            AppEvent::UserMessageLog { content, .. } => {
                vec![log_event(LogLevel::Info, "User", content.clone())]
            }

            // ---- Interruption ----
            AppEvent::InterruptRequested { .. } => vec![log_event(
                LogLevel::Info,
                "agent",
                "interrupt requested".to_string(),
            )],
            AppEvent::Interrupted { reason, .. } => vec![log_event(
                LogLevel::Info,
                "agent",
                format!("interrupted: {reason}"),
            )],

            // ---- Mid-turn steering ----
            AppEvent::SteerRequested { text, id, .. } => {
                let preview: String = text.chars().take(80).collect();
                let suffix = if text.chars().count() > 80 { "..." } else { "" };
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                vec![log_event(
                    LogLevel::Info,
                    "agent",
                    format!("steer requested{id_part}: {preview}{suffix}"),
                )]
            }
            AppEvent::SteerQueued { id, reason, .. } => {
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                vec![log_event(
                    LogLevel::Info,
                    "agent",
                    format!("steer queued{id_part}: {reason}"),
                )]
            }
            AppEvent::SteerAccepted { id, reason, .. } => {
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                vec![log_event(
                    LogLevel::Info,
                    "agent",
                    format!("steer accepted{id_part}: {reason}"),
                )]
            }
            AppEvent::SteerDelivered { id, mid_turn, .. } => {
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                let mode = if *mid_turn {
                    "mid-turn"
                } else {
                    "turn boundary"
                };
                vec![log_event(
                    LogLevel::Info,
                    "agent",
                    format!("steer delivered{id_part} ({mode})"),
                )]
            }
            AppEvent::SteerCancelRequested { .. } => Vec::new(),
            AppEvent::SteerCancelled { id, reason, .. } => {
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                vec![log_event(
                    LogLevel::Info,
                    "agent",
                    format!("steer cancelled{id_part}: {reason}"),
                )]
            }
            AppEvent::SteerCancelFailed { id, reason, .. } => {
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                vec![log_event(
                    LogLevel::Warn,
                    "agent",
                    format!("steer cancel failed{id_part}: {reason}"),
                )]
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WireEventUpcaster — OutboundEvent → PeerEvent
// ---------------------------------------------------------------------------
//
// Used by `IntendantWsTransport` to map a peer Intendant's `/ws`
// wire stream into the `PeerEvent` vocabulary. Operates on typed
// [`OutboundEvent`] (derived Deserialize + `#[serde(other)] Unknown`
// for forward-compat) rather than raw JSON — the transport parses
// frames through serde, then feeds them here.
//
// Drift-prevention strategy: every AppEvent variant that passes
// through `app_event_to_outbound()` should produce the same
// `Vec<PeerEvent>` whether you route it through `AppEventUpcaster`
// directly or through the wire (`app_event_to_outbound()` +
// `WireEventUpcaster`). The parity tests at the bottom of this
// module enforce that invariant; intentional information loss is
// marked explicitly in each case with a brief rationale.

/// Stateful `OutboundEvent` → `PeerEvent` upcaster for wire-format
/// input (from a peer Intendant's `/ws`).
///
/// Mirrors `AppEventUpcaster`'s state machine exactly — the same
/// `current_message_id` / `current_turn` / `current_agent_turn`
/// tracking for streaming deltas and activity lifecycle. This is
/// the mechanical half of the drift guard: both upcasters derive
/// activity ids from the same tracked state so a `Started` event's
/// id always matches the corresponding `Progress` and `Completed`
/// events. Same outcome-threading contract on `close_pending_agent`
/// as well — TaskComplete propagates its failure/cancel outcome
/// down to any in-flight agent instead of marking it Success.
pub struct WireEventUpcaster {
    seq: u64,
    current_message_id: Option<MessageId>,
    current_turn: Option<usize>,
    current_agent_turn: Option<usize>,
    /// Per-session enrichment fold (see [`PeerSessionFold`]).
    sessions: PeerSessionFold,
    /// Display-availability fold (see [`PeerDisplayFold`]).
    displays: PeerDisplayFold,
}

impl Default for WireEventUpcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl WireEventUpcaster {
    pub fn new() -> Self {
        Self {
            seq: 0,
            current_message_id: None,
            current_turn: None,
            current_agent_turn: None,
            sessions: PeerSessionFold::default(),
            displays: PeerDisplayFold::default(),
        }
    }

    /// Record the peer daemon's own primary session id (learned by the
    /// transport from the `/ws` bootstrap `state_snapshot` frame, which
    /// is not an `OutboundEvent` and never reaches `upcast`). Sessions
    /// matching it are stamped `is_primary` in every emitted snapshot.
    pub fn set_primary_session_id(&mut self, session_id: &str) {
        self.sessions.set_primary_session_id(session_id);
    }

    /// Replay-lane upcast for the `/ws` bootstrap `log_replay` frame:
    /// fold ONLY the session-state effects of a replayed wire event,
    /// suppressing everything the live arms would re-fire as if it
    /// were happening now (messages, activities, logs, approval
    /// events, host status). This is how a late-joining consumer
    /// converges on the peer's current session state — including
    /// change-detected emissions that fired before the connection
    /// existed (an idle repo's git vitals never recur) — the same way
    /// a refreshed browser converges via the same replay. Wire-only:
    /// the in-process `AppEventUpcaster` never sees a replay.
    ///
    /// The live-stream correlation state (turn ids, message ids) is
    /// deliberately untouched so replayed turns can't corrupt the
    /// correlation of live events that follow.
    pub fn upcast_replayed(&mut self, event: &OutboundEvent) -> Vec<PeerEvent> {
        match event {
            OutboundEvent::SessionStarted { session_id, task } => {
                vec![PeerEvent::SessionStarted {
                    session: self.sessions.started(session_id, task.as_deref()),
                }]
            }
            OutboundEvent::SessionIdentity {
                session_id, source, ..
            } => session_updated_events(
                self.sessions
                    .update(session_id, |s| s.source = source.clone()),
            ),
            OutboundEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ephemeral,
            } => session_updated_events(self.sessions.update(child_session_id, |s| {
                s.parent_session_id = Some(parent_session_id.clone());
                s.relationship = relationship.clone();
                s.ephemeral = *ephemeral;
            })),
            OutboundEvent::SessionGoal { session_id, goal } => {
                session_updated_events(self.sessions.update(session_id, |s| s.goal = goal.clone()))
            }
            OutboundEvent::SessionVitals { session_id, vitals } => session_updated_events(
                self.sessions
                    .update(session_id, |s| s.vitals = Some(vitals.clone())),
            ),
            OutboundEvent::Status {
                phase,
                session_id,
                task,
                ..
            } => session_updated_events(self.sessions.update(session_id, |s| {
                s.phase = phase.clone();
                if s.label.as_deref().unwrap_or("").is_empty() && !task.is_empty() {
                    s.label = Some(task.clone());
                }
            })),
            OutboundEvent::TurnStarted { session_id, .. }
            | OutboundEvent::AgentStarted { session_id, .. } => {
                session_updated_events(self.sessions.phase(session_id.as_deref(), "working"))
            }
            OutboundEvent::DoneSignal { session_id, .. }
            | OutboundEvent::TaskComplete { session_id, .. } => {
                session_updated_events(self.sessions.phase(session_id.as_deref(), "done"))
            }
            OutboundEvent::Usage {
                session_id, main, ..
            }
            | OutboundEvent::UsageUpdate {
                session_id, main, ..
            } => match session_id.as_deref() {
                Some(sid) => {
                    let tokens = main.prompt_tokens + main.completion_tokens;
                    session_updated_events(
                        self.sessions.update(sid, |s| s.tokens_used = Some(tokens)),
                    )
                }
                None => vec![],
            },
            OutboundEvent::ApprovalRequired { session_id, id, .. } => {
                session_updated_events(self.sessions.approval_requested(*id, session_id.as_deref()))
            }
            OutboundEvent::ApprovalResolved { id, .. } => {
                session_updated_events(self.sessions.approval_resolved(*id))
            }
            OutboundEvent::SessionEnded {
                session_id, reason, ..
            } => {
                self.sessions.ended(session_id);
                vec![PeerEvent::SessionEnded {
                    session_id: session_id.clone(),
                    reason: reason.clone(),
                }]
            }
            _ => vec![],
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    fn current_or_new_message_id(&mut self) -> MessageId {
        if let Some(id) = &self.current_message_id {
            return id.clone();
        }
        let seq = self.next_seq();
        let id = MessageId(format!("msg-seq-{seq}"));
        self.current_message_id = Some(id.clone());
        id
    }

    /// Same contract as `AppEventUpcaster::close_pending_agent` —
    /// the caller supplies the outcome to stamp on the emitted
    /// `ActivityCompleted` so a failing task can propagate its
    /// failure down to the in-flight agent instead of contradicting
    /// it with Success.
    fn close_pending_agent(&mut self, outcome: ActivityOutcome) -> Option<PeerEvent> {
        self.current_agent_turn
            .take()
            .map(|turn| PeerEvent::ActivityCompleted {
                id: ActivityId(format!("agent-{turn}")),
                outcome,
            })
    }

    fn close_pending_turn(&mut self, outcome: ActivityOutcome) -> Option<PeerEvent> {
        self.current_turn
            .take()
            .map(|turn| PeerEvent::ActivityCompleted {
                id: ActivityId(format!("turn-{turn}")),
                outcome,
            })
    }

    /// Map a wire-format [`OutboundEvent`] to zero or more
    /// [`PeerEvent`]s.
    pub fn upcast(&mut self, event: &OutboundEvent) -> Vec<PeerEvent> {
        match event {
            // ---- Forward-compat + dropped metric streams ----
            //
            // FileChanged / Snapshot* / RolledBack / Redone / HistoryPruned
            // are dashboard local-state events. PeerAdded / PeerRemoved /
            // PeerStateChanged / PeerEventForwarded are registry control-
            // plane events emitted by the local translator.
            //
            // PeerEventForwarded specifically wraps a PeerEvent that came
            // from another peer's stream — re-upcasting it would falsely
            // attribute that activity to the local pipeline. Drop it here
            // and let the dashboard route the inner payload to the right
            // per-peer view directly.
            //
            // None of these are peer-originated activity *to this side*,
            // so they're intentionally absent from the peer event vocabulary.
            OutboundEvent::Unknown
            | OutboundEvent::DisplayMetrics { .. }
            | OutboundEvent::ContextSnapshot { .. }
            | OutboundEvent::AgendaChanged { .. }
            | OutboundEvent::MemoryChanged { .. }
            | OutboundEvent::FileChanged { .. }
            | OutboundEvent::UploadReady { .. }
            | OutboundEvent::UploadDeleted { .. }
            | OutboundEvent::SnapshotCreated { .. }
            | OutboundEvent::RolledBack { .. }
            | OutboundEvent::Redone { .. }
            | OutboundEvent::HistoryPruned { .. }
            | OutboundEvent::ConversationRolledBack { .. }
            | OutboundEvent::SessionCapabilities { .. }
            | OutboundEvent::FollowUpStatus { .. }
            | OutboundEvent::SharedView { .. }
            // A secondary's display-request doorbell rings on its own
            // dashboards; the peer rail does not mirror it (see the
            // AppEvent upcaster twin above).
            | OutboundEvent::DisplayRequestRaised { .. }
            | OutboundEvent::DisplayRequestResolved { .. }
            // A peer's live CU action overlays render on ITS dashboards;
            // the peer rail doesn't mirror them (same class as the
            // AppEvent upcaster twin above).
            | OutboundEvent::CuAction { .. }
            | OutboundEvent::BrowserWorkspaceChanged { .. }
            | OutboundEvent::SessionRenameResult { .. }
            | OutboundEvent::SessionAgentConfigResult { .. }
            | OutboundEvent::PeerAdded { .. }
            | OutboundEvent::PeerRemoved { .. }
            | OutboundEvent::PeerStateChanged { .. }
            | OutboundEvent::PeerEventForwarded { .. } => vec![],

            OutboundEvent::UserMessageEditStatus {
                user_turn_index,
                status,
                message,
                ..
            } => vec![log_event(
                if status == "failed" {
                    LogLevel::Error
                } else {
                    LogLevel::Info
                },
                "edit",
                format!("Edit user turn {user_turn_index} {status}: {message}"),
            )],

            // Peer-emitted WebRTC signaling. Upcasts 1:1 to the
            // matching `PeerEvent::WebRtcSignal` so the per-peer event
            // stream carries the peer's `Answer` and trickled
            // `IceCandidate`s back to the registry, which forwards
            // them via `PeerEventForwarded` to the browser.
            //
            // The wire `session_id: String` becomes the typed
            // `WebRtcSessionId` here so federation-side consumers
            // (registry, dashboard signaling relay) get the
            // newtyped value without re-parsing.
            OutboundEvent::WebRtcSignal {
                display_id,
                session_id,
                signal,
            } => vec![PeerEvent::WebRtcSignal {
                display_id: *display_id,
                session_id: crate::peer::WebRtcSessionId(session_id.clone()),
                signal: signal.clone(),
            }],

            OutboundEvent::PeerFileTransferSignal { session_id, signal } => {
                vec![PeerEvent::PeerFileTransferSignal {
                    session_id: crate::peer::WebRtcSessionId(session_id.clone()),
                    signal: signal.clone(),
                }]
            }

            OutboundEvent::PeerDashboardControlSignal { session_id, signal } => {
                vec![PeerEvent::PeerDashboardControlSignal {
                    session_id: crate::peer::WebRtcSessionId(session_id.clone()),
                    signal: signal.clone(),
                }]
            }

            OutboundEvent::CodexThreadActionRequested {
                action, session_id, ..
            } => vec![log_event(
                LogLevel::Info,
                "codex-action",
                format!(
                    "/{} requested{}",
                    action,
                    session_id
                        .as_deref()
                        .map(|id| format!(" for {}", id))
                        .unwrap_or_default()
                ),
            )],

            OutboundEvent::CodexThreadActionResult {
                action,
                success,
                message,
                ..
            } => vec![log_event(
                if *success {
                    LogLevel::Info
                } else {
                    LogLevel::Warn
                },
                "codex-action",
                if *success {
                    format!("/{}: {}", action, message)
                } else {
                    format!("/{}: FAILED — {}", action, message)
                },
            )],

            // ---- Turn lifecycle ----
            OutboundEvent::TurnStarted {
                session_id, turn, ..
            } => {
                let mut out = vec![];
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(ActivityOutcome::Success) {
                    out.push(closed);
                }
                self.current_message_id = Some(MessageId(format!("msg-turn-{turn}")));
                self.current_turn = Some(*turn);
                out.push(PeerEvent::ActivityStarted {
                    id: ActivityId(format!("turn-{turn}")),
                    kind: ActivityKind::ModelTurn,
                    label: format!("turn {turn}"),
                });
                out.extend(session_updated_events(
                    self.sessions.phase(session_id.as_deref(), "working"),
                ));
                out
            }

            OutboundEvent::ModelResponseDelta { text, .. } => {
                let id = self.current_or_new_message_id();
                vec![PeerEvent::Message {
                    id,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text { text: text.clone() },
                    partial: true,
                }]
            }

            // OutboundEvent::ModelResponse does NOT carry usage on the
            // wire — usage travels as a separate OutboundEvent::Usage /
            // UsageUpdate. That's the documented information-split: the
            // `AppEvent → AppEventUpcaster` path emits Message + Usage
            // from one ModelResponse, while the wire path emits
            // Message from this variant and relies on a sibling
            // OutboundEvent::Usage to carry the tokens. The parity
            // test `model_response_usage_accounting_drift` documents
            // this gap explicitly.
            OutboundEvent::ModelResponse {
                turn,
                summary,
                reasoning_summary,
                source: _,
                ..
            } => {
                let msg_id = self.current_or_new_message_id();
                let mut out = vec![PeerEvent::Message {
                    id: msg_id,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text {
                        text: summary.clone(),
                    },
                    partial: false,
                }];
                if let Some(reasoning_text) = reasoning_summary {
                    out.push(PeerEvent::Message {
                        id: MessageId(format!("reasoning-turn-{turn}")),
                        role: MessageRole::Assistant,
                        content: MessageContent::Reasoning {
                            text: reasoning_text.clone(),
                        },
                        partial: false,
                    });
                }
                self.current_message_id = None;
                out
            }

            OutboundEvent::ModelSummary {
                turn,
                summary,
                reasoning_summary,
            } => {
                // Same shape as ModelResponse but without a source
                // override. Emitted by some paths as a distilled
                // summary rather than a full response. Maps to the
                // same Message + Reasoning shape.
                let msg_id = MessageId(format!("summary-turn-{turn}"));
                let mut out = vec![PeerEvent::Message {
                    id: msg_id,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text {
                        text: summary.clone(),
                    },
                    partial: false,
                }];
                if let Some(reasoning_text) = reasoning_summary {
                    out.push(PeerEvent::Message {
                        id: MessageId(format!("summary-reasoning-turn-{turn}")),
                        role: MessageRole::Assistant,
                        content: MessageContent::Reasoning {
                            text: reasoning_text.clone(),
                        },
                        partial: false,
                    });
                }
                out
            }

            OutboundEvent::DoneSignal {
                session_id,
                message,
            } => {
                self.current_message_id = None;
                let mut out = vec![];
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(ActivityOutcome::Success) {
                    out.push(closed);
                } else {
                    let seq = self.next_seq();
                    out.push(PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("done-{seq}")),
                        outcome: ActivityOutcome::Success,
                    });
                }
                if let Some(msg) = message {
                    out.push(log_event(LogLevel::Info, "agent", format!("done: {msg}")));
                }
                out.extend(session_updated_events(
                    self.sessions.phase(session_id.as_deref(), "done"),
                ));
                out
            }

            OutboundEvent::RoundComplete {
                round,
                turns_in_round,
                ..
            } => vec![log_event(
                LogLevel::Info,
                "agent",
                format!("round {round} complete ({turns_in_round} turns)"),
            )],

            // ---- Sub-agent / tool execution ----
            OutboundEvent::AgentStarted {
                session_id,
                turn,
                commands_preview,
                source,
                ..
            } => {
                let label = source.clone().unwrap_or_else(|| "agent".to_string());
                let mut out = vec![];
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                self.current_agent_turn = Some(*turn);
                out.push(PeerEvent::ActivityStarted {
                    id: ActivityId(format!("agent-{turn}")),
                    kind: ActivityKind::ToolCall,
                    label: format!("{label}: {commands_preview}"),
                });
                out.extend(session_updated_events(
                    self.sessions.phase(session_id.as_deref(), "working"),
                ));
                out
            }

            OutboundEvent::AgentOutput {
                stdout,
                stderr,
                source: _,
                ..
            } => {
                let mut out = vec![];
                if !stdout.is_empty() {
                    let id = match self.current_agent_turn {
                        Some(turn) => ActivityId(format!("agent-{turn}")),
                        None => {
                            let seq = self.next_seq();
                            ActivityId(format!("agent-orphan-{seq}"))
                        }
                    };
                    out.push(PeerEvent::ActivityProgress {
                        id,
                        text: Some(stdout.clone()),
                    });
                }
                if !stderr.is_empty() {
                    out.push(log_event(LogLevel::Warn, "agent", stderr.clone()));
                }
                out
            }

            OutboundEvent::SubAgentResult { summary } => {
                let seq = self.next_seq();
                vec![
                    PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("subagent-{seq}")),
                        outcome: ActivityOutcome::Success,
                    },
                    log_event(LogLevel::Info, "subagent", summary.clone()),
                ]
            }

            OutboundEvent::ContextManagement { turn } => vec![log_event(
                LogLevel::Debug,
                "context",
                format!("context management turn {turn}"),
            )],

            OutboundEvent::TaskComplete {
                session_id,
                reason,
                summary,
            } => {
                let outcome = match reason.as_str() {
                    "success" | "done" | "completed" => ActivityOutcome::Success,
                    "cancelled" | "canceled" => ActivityOutcome::Cancelled,
                    other => ActivityOutcome::Failed {
                        message: other.to_string(),
                    },
                };
                let mut out = vec![];
                // Propagate the task's outcome to any in-flight
                // agent so a failed/cancelled task doesn't emit a
                // contradictory Success on the agent activity.
                if let Some(closed) = self.close_pending_agent(outcome.clone()) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(outcome.clone()) {
                    out.push(closed);
                } else {
                    let seq = self.next_seq();
                    out.push(PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("task-{seq}")),
                        outcome,
                    });
                }
                self.current_message_id = None;
                if let Some(s) = summary {
                    out.push(log_event(LogLevel::Info, "task", s.clone()));
                }
                out.extend(session_updated_events(
                    self.sessions.phase(session_id.as_deref(), "done"),
                ));
                out
            }

            // ---- Session lifecycle ----
            OutboundEvent::SessionStarted { session_id, task } => {
                vec![PeerEvent::SessionStarted {
                    session: self.sessions.started(session_id, task.as_deref()),
                }]
            }

            OutboundEvent::SessionIdentity {
                session_id, source, ..
            } => session_updated_events(
                self.sessions
                    .update(session_id, |s| s.source = source.clone()),
            ),

            OutboundEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ephemeral,
            } => session_updated_events(self.sessions.update(child_session_id, |s| {
                s.parent_session_id = Some(parent_session_id.clone());
                s.relationship = relationship.clone();
                s.ephemeral = *ephemeral;
            })),

            OutboundEvent::SessionForkResult {
                parent_session_id,
                child_session_id,
                error,
                ..
            } => vec![log_event(
                LogLevel::Info,
                "session",
                match (child_session_id, error) {
                    (Some(child), _) => {
                        format!("session fork: {parent_session_id} -> {child}")
                    }
                    (_, Some(error)) => {
                        format!("session fork of {parent_session_id} failed: {error}")
                    }
                    _ => format!("session fork of {parent_session_id} completed"),
                },
            )],

            OutboundEvent::SessionGoal { session_id, goal } => session_updated_events(
                self.sessions.update(session_id, |s| s.goal = goal.clone()),
            ),

            OutboundEvent::SessionVitals { session_id, vitals } => session_updated_events(
                self.sessions
                    .update(session_id, |s| s.vitals = Some(vitals.clone())),
            ),

            OutboundEvent::SessionAttached { session_id, source } => vec![log_event(
                LogLevel::Info,
                "session",
                format!("session attached: {} ({})", session_id, source),
            )],

            // Peer-delegation delivery receipt: the peer accepted a
            // task this side delegated and names its local session for
            // it. Upcast 1:1 so the per-peer actor can fold it into the
            // bounded receipt ledger `PeerHandle::delegate_task` awaits
            // (and so peers.jsonl keeps the durable acceptance record).
            OutboundEvent::TaskReceived {
                delegation_id,
                session_id,
            } => vec![PeerEvent::TaskReceipt {
                delegation_id: delegation_id.clone(),
                task: TaskId(session_id.clone()),
            }],

            OutboundEvent::SessionEnded {
                session_id, reason, ..
            } => {
                self.sessions.ended(session_id);
                vec![PeerEvent::SessionEnded {
                    session_id: session_id.clone(),
                    reason: reason.clone(),
                }]
            }

            // ---- Approval flow ----
            //
            // OutboundEvent::ApprovalRequired drops the ActionCategory
            // field from AppEvent (wire format only carries `command`,
            // not category). We default to "command_exec" which is
            // the overwhelmingly common case. Parity test documents
            // this as intentional loss — non-command-exec categories
            // (file_write, destructive, etc.) lose their specific
            // category name on the wire path.
            OutboundEvent::ApprovalRequired {
                session_id,
                id,
                command,
            } => {
                let mut out = vec![PeerEvent::ApprovalRequested {
                    request: ApprovalRequest {
                        request_id: id.to_string(),
                        category: "command_exec".to_string(),
                        preview: command.clone(),
                        auto_resolvable: false,
                    },
                }];
                out.extend(session_updated_events(
                    self.sessions.approval_requested(*id, session_id.as_deref()),
                ));
                out
            }

            OutboundEvent::ApprovalResolved { id, action, .. } => {
                let mut out = vec![PeerEvent::ApprovalResolved {
                    request_id: id.to_string(),
                    decision: approval_decision_from_action(action),
                }];
                out.extend(session_updated_events(self.sessions.approval_resolved(*id)));
                out
            }

            OutboundEvent::AutoApproved { preview } => {
                let seq = self.next_seq();
                vec![
                    PeerEvent::ApprovalRequested {
                        request: ApprovalRequest {
                            request_id: format!("auto-{seq}"),
                            category: "auto".to_string(),
                            preview: preview.clone(),
                            auto_resolvable: true,
                        },
                    },
                    PeerEvent::ApprovalResolved {
                        request_id: format!("auto-{seq}"),
                        decision: ApprovalDecision::Accept,
                    },
                ]
            }

            OutboundEvent::AskHuman { question } => {
                let seq = self.next_seq();
                vec![PeerEvent::ApprovalRequested {
                    request: ApprovalRequest {
                        request_id: format!("human-{seq}"),
                        category: "human_question".to_string(),
                        preview: question.clone(),
                        auto_resolvable: false,
                    },
                }]
            }

            // Structured questions flatten to the approval vocabulary the
            // same way askHuman does, but keep their real id so a peer's
            // Accept/Decline resolves the actual prompt. Questions share
            // the approval id space and resolve via ApprovalResolved, so
            // they feed the session fold's `needs_approval` the same way.
            OutboundEvent::UserQuestion {
                session_id,
                id,
                questions,
            } => {
                let mut out = vec![PeerEvent::ApprovalRequested {
                    request: ApprovalRequest {
                        request_id: id.to_string(),
                        category: "human_question".to_string(),
                        preview: crate::external_output::user_question_preview(questions),
                        auto_resolvable: false,
                    },
                }];
                out.extend(session_updated_events(
                    self.sessions.approval_requested(*id, session_id.as_deref()),
                ));
                out
            }

            OutboundEvent::HumanResponseSent => vec![log_event(
                LogLevel::Info,
                "human",
                "human response sent".to_string(),
            )],

            // ---- Display capability ----
            OutboundEvent::DisplayReady {
                display_id,
                width,
                height,
                agent_visible,
            } => self
                .displays
                .ready(*display_id, *width, *height, *agent_visible),

            OutboundEvent::DisplayResize {
                display_id,
                width,
                height,
            } => {
                let mut events = vec![log_event(
                    LogLevel::Info,
                    "display",
                    format!("display {display_id} resized to {width}x{height}"),
                )];
                events.extend(self.displays.resize(*display_id, *width, *height));
                events
            }

            OutboundEvent::DisplayTaken { display_id } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Display,
                detail: serde_json::json!({ "display_id": display_id, "state": "taken" }),
            }],

            OutboundEvent::DisplayReleased {
                display_id: _,
                note,
            } => {
                vec![PeerEvent::CapabilityReleased {
                    capability: Capability::Display,
                    reason: note.clone(),
                }]
            }

            OutboundEvent::DisplayCaptureLost { display_id, reason } => self
                .displays
                .lost(*display_id, Some(format!("capture_lost: {reason}"))),

            OutboundEvent::DisplayApprovalPending {
                display_id: _,
                backend,
            } => vec![log_event(
                LogLevel::Info,
                "display",
                format!("display approval pending on {backend}"),
            )],

            // Private views (agent_visible=false) are not surfaced to
            // peers at all — see the AppEvent twin. Old wires omit both
            // fields; serde defaults them to display 0 / agent-visible,
            // the pre-split meaning.
            OutboundEvent::UserDisplayGranted {
                display_id,
                agent_visible,
            } => {
                if *agent_visible {
                    vec![log_event(
                        LogLevel::Info,
                        "display",
                        format!("user granted display {display_id}"),
                    )]
                } else {
                    vec![]
                }
            }

            OutboundEvent::UserDisplayRevoked { display_id, note } => {
                let note_str = note.as_deref().unwrap_or("");
                let mut events = vec![log_event(
                    LogLevel::Info,
                    "display",
                    format!("user revoked display {display_id}: {note_str}"),
                )];
                events.extend(self.displays.lost(
                    *display_id,
                    Some(note.clone().unwrap_or_else(|| "user display revoked".to_string())),
                ));
                events
            }

            OutboundEvent::DebugScreenReady { display_id } => {
                vec![PeerEvent::CapabilityEngaged {
                    capability: Capability::Display,
                    detail: serde_json::json!({
                        "display_id": display_id,
                        "kind": "debug_screen",
                    }),
                }]
            }

            OutboundEvent::DebugScreenTornDown { display_id: _ } => {
                vec![PeerEvent::CapabilityReleased {
                    capability: Capability::Display,
                    reason: Some("debug_screen_torn_down".to_string()),
                }]
            }

            // ---- Recording capability ----
            OutboundEvent::RecordingStarted { stream_name } => {
                vec![PeerEvent::CapabilityEngaged {
                    capability: Capability::Recording,
                    detail: serde_json::json!({ "stream": stream_name }),
                }]
            }

            OutboundEvent::RecordingStopped { stream_name } => {
                vec![PeerEvent::CapabilityReleased {
                    capability: Capability::Recording,
                    reason: Some(format!("stopped: {stream_name}")),
                }]
            }

            OutboundEvent::RecordingError {
                stream_name,
                message,
            } => vec![log_event(
                LogLevel::Error,
                "recording",
                format!("{stream_name}: {message}"),
            )],

            OutboundEvent::RecordingDeleted { stream_name } => vec![log_event(
                LogLevel::Info,
                "recording",
                format!("{stream_name} deleted"),
            )],

            // ---- Presence (wire side has only PresenceLog, no
            // Connected/Disconnected — those are presence lifecycle
            // events that don't make it past app_event_to_outbound) ----
            OutboundEvent::PresenceLog { message, level } => {
                let lvl = level
                    .as_deref()
                    .map(wire_log_level)
                    .unwrap_or(LogLevel::Info);
                vec![log_event(lvl, "presence", message.clone())]
            }

            OutboundEvent::UserTranscript { text, seq: _ } => {
                let seq = self.next_seq();
                vec![PeerEvent::Message {
                    id: MessageId(format!("user-transcript-{seq}")),
                    role: MessageRole::User,
                    content: MessageContent::Text { text: text.clone() },
                    partial: false,
                }]
            }

            // ---- Usage accounting ----
            OutboundEvent::PresenceUsageUpdate {
                total_tokens: _,
                context_window: _,
                usage_pct: _,
                provider,
                model,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                cache_creation_tokens,
            } => {
                let cost_usd = estimate_session_cost(
                    model,
                    *prompt_tokens,
                    *completion_tokens,
                    *cached_tokens,
                    *cache_creation_tokens,
                );
                vec![PeerEvent::Usage {
                    snapshot: UsageSnapshot {
                        tokens_in: *prompt_tokens,
                        tokens_out: *completion_tokens,
                        tokens_cached: *cached_tokens,
                        cost_usd,
                        by_model: vec![ModelUsage {
                            provider: provider.clone(),
                            model: model.clone(),
                            tokens_in: *prompt_tokens,
                            tokens_out: *completion_tokens,
                            cost_usd,
                        }],
                    },
                }]
            }

            OutboundEvent::LiveUsageUpdate {
                provider,
                model,
                input_tokens,
                output_tokens,
                cached_tokens,
                total_tokens: _,
                thinking_tokens,
                input_text_tokens,
                input_audio_tokens,
                input_image_tokens,
                cached_text_tokens,
                cached_audio_tokens,
                cached_image_tokens,
                output_text_tokens,
                output_audio_tokens,
            } => {
                let cost_usd = estimate_live_usage_cost(
                    model,
                    LiveUsageTokens {
                        input_tokens: *input_tokens,
                        output_tokens: *output_tokens,
                        cached_tokens: *cached_tokens,
                        thinking_tokens: *thinking_tokens,
                        input_text_tokens: *input_text_tokens,
                        input_audio_tokens: *input_audio_tokens,
                        input_image_tokens: *input_image_tokens,
                        cached_text_tokens: *cached_text_tokens,
                        cached_audio_tokens: *cached_audio_tokens,
                        cached_image_tokens: *cached_image_tokens,
                        output_text_tokens: *output_text_tokens,
                        output_audio_tokens: *output_audio_tokens,
                    },
                );
                vec![PeerEvent::Usage {
                    snapshot: UsageSnapshot {
                        tokens_in: *input_tokens,
                        tokens_out: *output_tokens,
                        tokens_cached: *cached_tokens,
                        cost_usd,
                        by_model: vec![ModelUsage {
                            provider: provider.clone(),
                            model: model.clone(),
                            tokens_in: *input_tokens,
                            tokens_out: *output_tokens,
                            cost_usd,
                        }],
                    },
                }]
            }

            OutboundEvent::Usage {
                session_id, main, ..
            }
            | OutboundEvent::UsageUpdate {
                session_id, main, ..
            } => {
                let cost_usd = estimate_session_cost(
                    &main.model,
                    main.prompt_tokens,
                    main.completion_tokens,
                    main.cached_tokens,
                    main.cache_creation_tokens,
                );
                let mut out = vec![PeerEvent::Usage {
                    snapshot: UsageSnapshot {
                        tokens_in: main.prompt_tokens,
                        tokens_out: main.completion_tokens,
                        tokens_cached: main.cached_tokens,
                        cost_usd,
                        by_model: vec![ModelUsage {
                            provider: main.provider.clone(),
                            model: main.model.clone(),
                            tokens_in: main.prompt_tokens,
                            tokens_out: main.completion_tokens,
                            cost_usd,
                        }],
                    },
                }];
                if let Some(sid) = session_id.as_deref() {
                    let tokens = main.prompt_tokens + main.completion_tokens;
                    out.extend(session_updated_events(
                        self.sessions.update(sid, |s| s.tokens_used = Some(tokens)),
                    ));
                }
                out
            }

            // ---- Status ----
            OutboundEvent::Status {
                phase,
                session_id,
                task,
                ..
            } => {
                let mut out = vec![PeerEvent::StatusChanged {
                    status: status_from_phase(phase),
                }];
                out.extend(session_updated_events(self.sessions.update(
                    session_id,
                    |s| {
                        s.phase = phase.clone();
                        if s.label.as_deref().unwrap_or("").is_empty() && !task.is_empty() {
                            s.label = Some(task.clone());
                        }
                    },
                )));
                out
            }

            OutboundEvent::ExternalAgentChanged { agent } => vec![log_event(
                LogLevel::Info,
                "config",
                format!(
                    "external agent changed → {}",
                    agent.as_deref().unwrap_or("none")
                ),
            )],

            OutboundEvent::AutonomyChanged { autonomy } => vec![log_event(
                LogLevel::Info,
                "config",
                format!("autonomy changed → {autonomy}"),
            )],

            OutboundEvent::CodexConfigChanged {
                command,
                managed_command,
                managed_command_cleared,
                sandbox,
                approval_policy,
                model,
                model_cleared,
                reasoning_effort,
                reasoning_effort_cleared,
                service_tier,
                service_tier_cleared,
                web_search,
                network_access,
                writable_roots,
                managed_context,
                context_archive,
            } => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(v) = command {
                    parts.push(format!("command={v}"));
                }
                if let Some(v) = managed_command {
                    parts.push(format!("managed_command={v}"));
                } else if *managed_command_cleared {
                    parts.push("managed_command=<vanilla fallback>".to_string());
                }
                if let Some(v) = sandbox {
                    parts.push(format!("sandbox={v}"));
                }
                if let Some(v) = approval_policy {
                    parts.push(format!("approval_policy={v}"));
                }
                if let Some(v) = model {
                    parts.push(format!("model={v}"));
                } else if *model_cleared {
                    parts.push("model=<default>".to_string());
                }
                if let Some(v) = reasoning_effort {
                    parts.push(format!("reasoning_effort={v}"));
                } else if *reasoning_effort_cleared {
                    parts.push("reasoning_effort=<default>".to_string());
                }
                if let Some(v) = service_tier {
                    parts.push(format!("service_tier={v}"));
                } else if *service_tier_cleared {
                    parts.push("service_tier=<inherit>".to_string());
                }
                if let Some(v) = web_search {
                    parts.push(format!("web_search={v}"));
                }
                if let Some(v) = network_access {
                    parts.push(format!("network_access={v}"));
                }
                if let Some(v) = writable_roots {
                    parts.push(format!("writable_roots=[{} path(s)]", v.len()));
                }
                if let Some(v) = managed_context {
                    parts.push(format!("managed_context={v}"));
                }
                if let Some(v) = context_archive {
                    parts.push(format!("context_archive={v}"));
                }
                if parts.is_empty() {
                    vec![]
                } else {
                    vec![log_event(
                        LogLevel::Info,
                        "config",
                        format!("codex config: {}", parts.join(", ")),
                    )]
                }
            }

            OutboundEvent::ClaudeConfigChanged {
                model,
                model_cleared,
                permission_mode,
                allowed_tools,
            } => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(v) = model {
                    parts.push(format!("model={v}"));
                } else if *model_cleared {
                    parts.push("model=<default>".to_string());
                }
                if let Some(v) = permission_mode {
                    parts.push(format!("permission_mode={v}"));
                }
                if let Some(v) = allowed_tools {
                    parts.push(format!("allowed_tools=[{} entry/entries]", v.len()));
                }
                if parts.is_empty() {
                    vec![]
                } else {
                    vec![log_event(
                        LogLevel::Info,
                        "config",
                        format!("claude config: {}", parts.join(", ")),
                    )]
                }
            }

            // ---- Budget / safety ----
            OutboundEvent::BudgetWarning { pct, remaining } => vec![log_event(
                LogLevel::Warn,
                "budget",
                format!("budget warning: {pct:.1}% remaining={remaining}"),
            )],

            OutboundEvent::BudgetExhausted { remaining } => vec![log_event(
                LogLevel::Warn,
                "budget",
                format!("budget exhausted, remaining={remaining}"),
            )],

            OutboundEvent::SafetyCapReached => vec![log_event(
                LogLevel::Warn,
                "safety",
                "safety cap reached".to_string(),
            )],

            OutboundEvent::LoopError { message } => {
                vec![log_event(LogLevel::Error, "agent", message.clone())]
            }

            // ---- Log passthrough ----
            OutboundEvent::LogEntry {
                level,
                source,
                content,
                turn: _,
                session_id: _,
                user_turn_index: _,
                user_turn_revision: _,
                replacement_for_user_turn_index: _,
            } => {
                vec![log_event(wire_log_level(level), source, content.clone())]
            }
            // Same treatment as the AppEvent-side arm: note text as peer
            // log activity, attachments as a count (their URLs only
            // resolve on the origin daemon).
            OutboundEvent::SessionNote {
                text,
                attachments,
                source,
                ..
            } => vec![log_event(
                LogLevel::Info,
                source.as_deref().unwrap_or("note"),
                session_note_peer_log_text(text, attachments.len()),
            )],
            // Same treatment as the AppEvent-side arm: notification text as
            // peer log activity.
            OutboundEvent::UserNotification {
                title,
                text,
                urgency,
                ..
            } => vec![log_event(
                user_notification_peer_log_level(*urgency),
                "notify",
                user_notification_peer_log_text(title.as_deref(), text, *urgency),
            )],
            OutboundEvent::UserMessageRewind {
                user_turn_index,
                turns_removed,
                ..
            } => vec![log_event(
                LogLevel::Warn,
                "system",
                if *turns_removed == 1 {
                    format!("Rewound user turn {user_turn_index}")
                } else {
                    format!(
                        "Rewound user turn {user_turn_index} and {} later turns",
                        turns_removed.saturating_sub(1)
                    )
                },
            )],

            // ---- CommandResult: control-plane meta-event ----
            //
            // CommandResult is the ack for a ControlMsg — "the Approve
            // action succeeded", "the SetAutonomy call failed with
            // 'bad level'", etc. It has no direct AppEvent ancestor
            // (it's synthesized by control_plane.rs, not the agent
            // loop). For federation, it surfaces as an info/error
            // log so an observer sees what the peer's control plane
            // is doing.
            OutboundEvent::CommandResult {
                action,
                ok,
                message,
                data: _,
            } => {
                let level = if *ok { LogLevel::Info } else { LogLevel::Warn };
                vec![log_event(level, "control", format!("{action}: {message}"))]
            }

            // ---- Interruption ----
            OutboundEvent::InterruptRequested { .. } => vec![log_event(
                LogLevel::Info,
                "agent",
                "interrupt requested".to_string(),
            )],
            OutboundEvent::Interrupted { reason, .. } => vec![log_event(
                LogLevel::Info,
                "agent",
                format!("interrupted: {reason}"),
            )],

            // ---- Mid-turn steering ----
            OutboundEvent::SteerRequested { text, id, .. } => {
                let preview: String = text.chars().take(80).collect();
                let suffix = if text.chars().count() > 80 { "..." } else { "" };
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                vec![log_event(
                    LogLevel::Info,
                    "agent",
                    format!("steer requested{id_part}: {preview}{suffix}"),
                )]
            }
            OutboundEvent::SteerQueued { id, reason, .. } => {
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                vec![log_event(
                    LogLevel::Info,
                    "agent",
                    format!("steer queued{id_part}: {reason}"),
                )]
            }
            OutboundEvent::SteerAccepted { id, reason, .. } => {
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                vec![log_event(
                    LogLevel::Info,
                    "agent",
                    format!("steer accepted{id_part}: {reason}"),
                )]
            }
            OutboundEvent::SteerDelivered { id, mid_turn, .. } => {
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                let mode = if *mid_turn {
                    "mid-turn"
                } else {
                    "turn boundary"
                };
                vec![log_event(
                    LogLevel::Info,
                    "agent",
                    format!("steer delivered{id_part} ({mode})"),
                )]
            }
            OutboundEvent::SteerCancelled { id, reason, .. } => {
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                vec![log_event(
                    LogLevel::Info,
                    "agent",
                    format!("steer cancelled{id_part}: {reason}"),
                )]
            }
            OutboundEvent::SteerCancelFailed { id, reason, .. } => {
                let id_part = if id.is_empty() {
                    String::new()
                } else {
                    format!(" [{id}]")
                };
                vec![log_event(
                    LogLevel::Warn,
                    "agent",
                    format!("steer cancel failed{id_part}: {reason}"),
                )]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::TokenUsage;

    fn token_usage(prompt: u64, completion: u64) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cached_tokens: 0,
            ..Default::default()
        }
    }

    /// Turn start emits a single `ActivityStarted` with kind ModelTurn
    /// and a deterministic id derived from the turn number.
    #[test]
    fn turn_started_emits_activity_started() {
        let mut u = AppEventUpcaster::new();
        let out = u.upcast(&AppEvent::TurnStarted {
            session_id: None,
            turn: 3,
            budget_pct: 0.5,
            remaining: 1000,
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            PeerEvent::ActivityStarted { id, kind, .. } => {
                assert_eq!(id.0, "turn-3");
                assert_eq!(*kind, ActivityKind::ModelTurn);
            }
            _ => panic!("expected ActivityStarted, got {:?}", out[0]),
        }
    }

    /// Streaming deltas share a message ID across calls until a
    /// `ModelResponse` closes the turn. This is the core state
    /// mechanic of the upcaster.
    #[test]
    fn streaming_deltas_share_message_id_within_turn() {
        let mut u = AppEventUpcaster::new();
        // Open turn 5.
        let _ = u.upcast(&AppEvent::TurnStarted {
            session_id: None,
            turn: 5,
            budget_pct: 0.5,
            remaining: 100,
        });
        // Prime the current-turn message ID by emitting a ModelResponse
        // first — wait, that clears state. Instead, deltas without a
        // prior ModelResponse synthesize a fresh ID that's stable
        // across subsequent deltas in the same conversation-turn.
        let a = u.upcast(&AppEvent::ModelResponseDelta {
            session_id: None,
            text: "Hello ".into(),
        });
        let b = u.upcast(&AppEvent::ModelResponseDelta {
            session_id: None,
            text: "world".into(),
        });
        let id_a = match &a[0] {
            PeerEvent::Message { id, partial, .. } => {
                assert!(*partial, "delta should be partial");
                id.clone()
            }
            _ => panic!("expected Message"),
        };
        let id_b = match &b[0] {
            PeerEvent::Message { id, partial, .. } => {
                assert!(*partial);
                id.clone()
            }
            _ => panic!("expected Message"),
        };
        assert_eq!(id_a, id_b, "deltas within same turn must share id");
    }

    /// `ModelResponse` emits Message(final) + Usage and, when a
    /// streaming id was tracked, reuses it for the final message.
    #[test]
    fn model_response_final_shares_id_with_deltas() {
        let mut u = AppEventUpcaster::new();
        // Prime current_turn_message via a delta-first path.
        let _ = u.upcast(&AppEvent::TurnStarted {
            session_id: None,
            turn: 7,
            budget_pct: 0.5,
            remaining: 100,
        });
        let delta = u.upcast(&AppEvent::ModelResponseDelta {
            session_id: None,
            text: "Hello ".into(),
        });
        let delta_id = match &delta[0] {
            PeerEvent::Message { id, .. } => id.clone(),
            _ => panic!(),
        };
        // Now close the turn.
        let out = u.upcast(&AppEvent::ModelResponse {
            session_id: None,
            turn: 7,
            content: "Hello world".into(),
            usage: token_usage(10, 20),
            reasoning: None,
            source: None,
        });
        // Expect: Message(final) + Usage.
        assert_eq!(out.len(), 2);
        match &out[0] {
            PeerEvent::Message { id, partial, .. } => {
                assert!(!partial);
                // Current implementation: final uses message_id_for_turn
                // which looks up by turn #, and the delta used the same
                // turn's streaming id, so they should match.
                assert_eq!(
                    *id, delta_id,
                    "final message should reuse the streaming delta id"
                );
            }
            _ => panic!("expected Message, got {:?}", out[0]),
        }
        assert!(matches!(out[1], PeerEvent::Usage { .. }));
    }

    /// `ModelResponse` with reasoning emits Message + Reasoning + Usage.
    #[test]
    fn model_response_with_reasoning_adds_reasoning_message() {
        let mut u = AppEventUpcaster::new();
        let out = u.upcast(&AppEvent::ModelResponse {
            session_id: None,
            turn: 1,
            content: "final".into(),
            usage: token_usage(5, 5),
            reasoning: Some("thinking...".into()),
            source: None,
        });
        assert_eq!(out.len(), 3);
        assert!(matches!(
            &out[1],
            PeerEvent::Message {
                content: MessageContent::Reasoning { .. },
                ..
            }
        ));
    }

    /// Internal TUI events get dropped entirely — no noise on the
    /// peer event stream.
    #[test]
    fn internal_events_are_dropped() {
        let mut u = AppEventUpcaster::new();
        assert!(u.upcast(&AppEvent::Tick).is_empty());
        // ControlCommand carries arbitrary ControlMsg variants; Status is
        // a trivially-constructable one.
        assert!(u
            .upcast(&AppEvent::ControlCommand(
                crate::event::ControlMsg::Status { session_id: None }
            ))
            .is_empty());
    }

    /// `DisplayReady` engages the Display capability with detail
    /// carrying width/height, and `DisplayReleased` releases it.
    #[test]
    fn display_capability_lifecycle() {
        let mut u = AppEventUpcaster::new();
        // Availability is first-class now: display_ready folds into a
        // typed DisplayReady instead of a detail-blob CapabilityEngaged.
        let ready = u.upcast(&AppEvent::DisplayReady {
            display_id: 1,
            width: 1920,
            height: 1080,
            agent_visible: true,
        });
        assert_eq!(ready.len(), 1);
        match &ready[0] {
            PeerEvent::DisplayReady { display } => {
                assert_eq!(display.display_id, 1);
                assert_eq!((display.width, display.height), (1920, 1080));
            }
            other => panic!("expected DisplayReady, got {other:?}"),
        }
        // Control release stays a capability transition — the display
        // still exists, someone just let go of it.
        let released = u.upcast(&AppEvent::DisplayReleased {
            display_id: 1,
            note: Some("user revoked".into()),
        });
        match &released[0] {
            PeerEvent::CapabilityReleased { capability, reason } => {
                assert_eq!(*capability, Capability::Display);
                assert_eq!(reason.as_deref(), Some("user revoked"));
            }
            _ => panic!("expected CapabilityReleased"),
        }
        // Losing capture retires availability.
        let lost = u.upcast(&AppEvent::DisplayCaptureLost {
            display_id: 1,
            reason: "backend_crashed".into(),
        });
        assert_eq!(lost.len(), 1);
        assert!(matches!(
            &lost[0],
            PeerEvent::DisplayLost { display_id: 1, .. }
        ));
    }

    /// Recording lifecycle engages/releases the Recording capability.
    #[test]
    fn recording_capability_lifecycle() {
        let mut u = AppEventUpcaster::new();
        let started = u.upcast(&AppEvent::RecordingStarted {
            stream_name: "display-1".into(),
        });
        assert!(matches!(
            &started[0],
            PeerEvent::CapabilityEngaged {
                capability: Capability::Recording,
                ..
            }
        ));
        let stopped = u.upcast(&AppEvent::RecordingStopped {
            stream_name: "display-1".into(),
        });
        assert!(matches!(
            &stopped[0],
            PeerEvent::CapabilityReleased {
                capability: Capability::Recording,
                ..
            }
        ));
    }

    /// Presence connect/disconnect engages/releases the Voice capability.
    #[test]
    fn presence_voice_capability_lifecycle() {
        let mut u = AppEventUpcaster::new();
        let connected = u.upcast(&AppEvent::PresenceConnected {
            server_session_id: None,
            last_event_seq: 0,
            live_provider: Some("gemini".into()),
            live_model: Some("gemini-2.5-flash".into()),
        });
        assert!(matches!(
            &connected[0],
            PeerEvent::CapabilityEngaged {
                capability: Capability::Voice,
                ..
            }
        ));
        let disconnected = u.upcast(&AppEvent::PresenceDisconnected);
        assert!(matches!(
            &disconnected[0],
            PeerEvent::CapabilityReleased {
                capability: Capability::Voice,
                ..
            }
        ));
    }

    /// Approval required/resolved flow maps to ApprovalRequested/Resolved.
    #[test]
    fn approval_flow_maps_cleanly() {
        let mut u = AppEventUpcaster::new();
        let req = u.upcast(&AppEvent::ApprovalRequired {
            session_id: None,
            id: 42,
            command_preview: "rm -rf /tmp/foo".into(),
            category: crate::autonomy::ActionCategory::FileDelete,
        });
        match &req[0] {
            PeerEvent::ApprovalRequested { request } => {
                assert_eq!(request.request_id, "42");
                assert_eq!(request.category, "file_delete");
                assert!(!request.auto_resolvable);
            }
            _ => panic!("expected ApprovalRequested"),
        }
        let res = u.upcast(&AppEvent::ApprovalResolved {
            session_id: None,
            id: 42,
            action: "approve".into(),
        });
        match &res[0] {
            PeerEvent::ApprovalResolved {
                request_id,
                decision,
            } => {
                assert_eq!(request_id, "42");
                assert_eq!(*decision, ApprovalDecision::Accept);
            }
            _ => panic!("expected ApprovalResolved"),
        }
    }

    /// Status update with a known phase maps to the corresponding
    /// `PeerStatus` variant. Unknown phases collapse to `Idle`.
    #[test]
    fn status_update_phase_mapping() {
        let mut u = AppEventUpcaster::new();
        let cases: &[(&str, PeerStatus)] = &[
            ("idle", PeerStatus::Idle),
            ("thinking", PeerStatus::Working),
            ("waiting_approval", PeerStatus::NeedsApproval),
            ("failed", PeerStatus::Error),
            ("holographic", PeerStatus::Idle), // unknown → Idle
        ];
        for (phase, expected) in cases {
            let out = u.upcast(&AppEvent::StatusUpdate {
                turn: 0,
                phase: phase.to_string(),
                autonomy: "medium".into(),
                session_id: "s".into(),
                task: "t".into(),
            });
            match &out[0] {
                PeerEvent::StatusChanged { status } => assert_eq!(
                    status, expected,
                    "phase={phase}: expected {expected:?}, got {status:?}"
                ),
                _ => panic!("expected StatusChanged"),
            }
        }
    }

    /// Session start/end maps to SessionStarted/SessionEnded with
    /// the id preserved.
    #[test]
    fn session_lifecycle() {
        let mut u = AppEventUpcaster::new();
        let start = u.upcast(&AppEvent::SessionStarted {
            session_id: "sess-1".into(),
            task: Some("research".into()),
        });
        match &start[0] {
            PeerEvent::SessionStarted { session } => {
                assert_eq!(session.session_id, "sess-1");
                assert_eq!(session.label.as_deref(), Some("research"));
            }
            _ => panic!("expected SessionStarted"),
        }
        let end = u.upcast(&AppEvent::SessionEnded {
            session_id: "sess-1".into(),
            reason: "done".into(),
            error_kind: None,
        });
        match &end[0] {
            PeerEvent::SessionEnded { session_id, reason } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(reason, "done");
            }
            _ => panic!("expected SessionEnded"),
        }
    }

    /// ActivityId lifecycle contract: a model turn's Started and
    /// Completed events must share the same id so observers can
    /// correlate start→complete. Previously DoneSignal synthesized
    /// a `done-{seq}` id that had no relation to the TurnStarted's
    /// `turn-{N}` — two events with no way for the receiver to
    /// know they belonged to the same activity.
    #[test]
    fn model_turn_activity_ids_match_start_to_complete() {
        let mut u = AppEventUpcaster::new();
        let started = u.upcast(&AppEvent::TurnStarted {
            session_id: None,
            turn: 7,
            budget_pct: 0.5,
            remaining: 100,
        });
        let start_id = match started.last().unwrap() {
            PeerEvent::ActivityStarted { id, .. } => id.clone(),
            _ => panic!("expected ActivityStarted"),
        };
        assert_eq!(start_id.0, "turn-7");

        let done = u.upcast(&AppEvent::DoneSignal {
            session_id: None,
            message: None,
        });
        let complete_id = done
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityCompleted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("DoneSignal must emit an ActivityCompleted");
        assert_eq!(
            complete_id, start_id,
            "start and complete events must share the activity id"
        );
    }

    /// Agent activity lifecycle: started / progress / completed all
    /// share the same id so observers can correlate tool output
    /// with its tool call.
    #[test]
    fn agent_activity_ids_match_start_progress_complete() {
        let mut u = AppEventUpcaster::new();
        // Open a turn so the agent has a parent context (matches
        // typical usage; not strictly required).
        let _ = u.upcast(&AppEvent::TurnStarted {
            session_id: None,
            turn: 3,
            budget_pct: 0.5,
            remaining: 100,
        });
        // Start an agent activity.
        let started = u.upcast(&AppEvent::AgentStarted {
            session_id: None,
            turn: 3,
            commands_preview: "ls -la".into(),
            item_id: None,
            source: None,
        });
        let start_id = started
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityStarted { id, kind, .. } if *kind == ActivityKind::ToolCall => {
                    Some(id.clone())
                }
                _ => None,
            })
            .expect("AgentStarted must emit an ActivityStarted(ToolCall)");
        assert_eq!(start_id.0, "agent-3");

        // Stream some output.
        let output = u.upcast(&AppEvent::AgentOutput {
            session_id: None,
            stdout: "file1\nfile2".into(),
            stderr: String::new(),
            source: None,
            output_id: None,
            item_id: None,
        });
        let progress_id = output
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityProgress { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("AgentOutput with stdout must emit an ActivityProgress");
        assert_eq!(progress_id, start_id, "progress id must match started id");

        // Close the turn → agent activity should close with the same id.
        let done = u.upcast(&AppEvent::DoneSignal {
            session_id: None,
            message: None,
        });
        let completed_ids: Vec<_> = done
            .iter()
            .filter_map(|e| match e {
                PeerEvent::ActivityCompleted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        // Both the agent and the turn should close. The agent
        // completes first with agent-3, the turn with turn-3.
        assert!(
            completed_ids.iter().any(|id| *id == start_id),
            "DoneSignal must close the agent with its started id. \
             Got: {completed_ids:?}"
        );
    }

    /// Same lifecycle guarantee on the wire upcaster. Parity with
    /// AppEventUpcaster is enforced mechanically because both
    /// upcasters derive ids from the same tracked state.
    #[test]
    fn wire_model_turn_activity_ids_match_start_to_complete() {
        let mut u = WireEventUpcaster::new();
        let started = u.upcast(&OutboundEvent::TurnStarted {
            session_id: None,
            turn: 7,
            budget_pct: 0.5,
        });
        let start_id = started
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityStarted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("expected ActivityStarted");
        assert_eq!(start_id.0, "turn-7");

        let done = u.upcast(&OutboundEvent::DoneSignal {
            session_id: None,
            message: None,
        });
        let complete_id = done
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityCompleted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("DoneSignal must emit ActivityCompleted");
        assert_eq!(complete_id, start_id);
    }

    #[test]
    fn wire_agent_activity_ids_match_start_progress_complete() {
        let mut u = WireEventUpcaster::new();
        let _ = u.upcast(&OutboundEvent::TurnStarted {
            session_id: None,
            turn: 3,
            budget_pct: 0.5,
        });
        let started = u.upcast(&OutboundEvent::AgentStarted {
            session_id: None,
            turn: 3,
            commands_preview: "ls -la".into(),
            item_id: None,
            source: None,
        });
        let start_id = started
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityStarted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("expected ActivityStarted");
        assert_eq!(start_id.0, "agent-3");

        let output = u.upcast(&OutboundEvent::AgentOutput {
            session_id: None,
            stdout: "file1".into(),
            stderr: String::new(),
            source: None,
            output_id: None,
            item_id: None,
        });
        let progress_id = output
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityProgress { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("expected ActivityProgress");
        assert_eq!(progress_id, start_id);
    }

    /// A failing `TaskComplete` must propagate its failure outcome
    /// to *both* the in-flight agent and the turn. Before the
    /// outcome-threading fix, `close_pending_agent` hardcoded
    /// `ActivityOutcome::Success`, so a failed task emitted a
    /// Success ActivityCompleted for the agent and a Failed
    /// ActivityCompleted for the turn — contradictory events in
    /// the consumer's feed that would render as "the tool
    /// succeeded but the turn it ran in failed." Verifies both
    /// upcasters behave consistently on both failure and cancel.
    #[test]
    fn task_complete_failure_propagates_to_agent_and_turn() {
        for (reason, expected) in &[("failed", "failed"), ("cancelled", "cancelled")] {
            let mut u = AppEventUpcaster::new();
            // Open turn + agent, then fail.
            let _ = u.upcast(&AppEvent::TurnStarted {
                session_id: None,
                turn: 4,
                budget_pct: 0.5,
                remaining: 100,
            });
            let _ = u.upcast(&AppEvent::AgentStarted {
                session_id: None,
                turn: 4,
                commands_preview: "risky".into(),
                item_id: None,
                source: None,
            });
            let out = u.upcast(&AppEvent::TaskComplete {
                session_id: None,
                reason: (*reason).to_string(),
                summary: None,
            });
            let completions: Vec<_> = out
                .iter()
                .filter_map(|e| match e {
                    PeerEvent::ActivityCompleted { id, outcome } => {
                        Some((id.clone(), outcome.clone()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                completions.len(),
                2,
                "expected agent + turn completions for reason={reason}, \
                 got: {completions:?}"
            );
            for (id, outcome) in &completions {
                let outcome_matches = match (*expected, outcome) {
                    ("failed", ActivityOutcome::Failed { .. }) => true,
                    ("cancelled", ActivityOutcome::Cancelled) => true,
                    _ => false,
                };
                assert!(
                    outcome_matches,
                    "activity {id:?} for reason={reason} should have \
                     outcome matching {expected}, got {outcome:?}"
                );
            }
        }
    }

    /// Same guarantee on the wire upcaster.
    #[test]
    fn wire_task_complete_failure_propagates_to_agent_and_turn() {
        let mut u = WireEventUpcaster::new();
        let _ = u.upcast(&OutboundEvent::TurnStarted {
            session_id: None,
            turn: 4,
            budget_pct: 0.5,
        });
        let _ = u.upcast(&OutboundEvent::AgentStarted {
            session_id: None,
            turn: 4,
            commands_preview: "risky".into(),
            item_id: None,
            source: None,
        });
        let out = u.upcast(&OutboundEvent::TaskComplete {
            session_id: None,
            reason: "failed".to_string(),
            summary: None,
        });
        let completions: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                PeerEvent::ActivityCompleted { id, outcome } => Some((id.clone(), outcome.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(completions.len(), 2);
        for (id, outcome) in &completions {
            assert!(
                matches!(outcome, ActivityOutcome::Failed { .. }),
                "wire path: activity {id:?} should be Failed, got {outcome:?}"
            );
        }
    }

    /// Parity on DoneSignal: given a TurnStarted + DoneSignal
    /// sequence, the app and wire paths must produce the same
    /// activity ids. Both upcasters track `current_turn` the same
    /// way, so this test catches any future drift in how that
    /// state is used.
    ///
    /// Note: we can't use the single-event `assert_parity` helper
    /// here because DoneSignal's id depends on prior state
    /// (TurnStarted seeded current_turn). Drive both upcasters
    /// through the full sequence manually.
    #[test]
    fn parity_done_signal_uses_tracked_turn() {
        let mut app = AppEventUpcaster::new();
        let mut wire = WireEventUpcaster::new();

        // Seed both with TurnStarted turn=5.
        let _ = app.upcast(&AppEvent::TurnStarted {
            session_id: None,
            turn: 5,
            budget_pct: 0.5,
            remaining: 100,
        });
        let _ = wire.upcast(&OutboundEvent::TurnStarted {
            session_id: None,
            turn: 5,
            budget_pct: 0.5,
        });

        // Both see DoneSignal.
        let app_out = app.upcast(&AppEvent::DoneSignal {
            session_id: None,
            message: None,
        });
        let wire_out = wire.upcast(&OutboundEvent::DoneSignal {
            session_id: None,
            message: None,
        });

        let app_completed_ids: Vec<_> = app_out
            .iter()
            .filter_map(|e| match e {
                PeerEvent::ActivityCompleted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        let wire_completed_ids: Vec<_> = wire_out
            .iter()
            .filter_map(|e| match e {
                PeerEvent::ActivityCompleted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            app_completed_ids, wire_completed_ids,
            "DoneSignal activity ids must match across paths after \
             TurnStarted seeded current_turn"
        );
        assert!(app_completed_ids.iter().any(|id| id.0 == "turn-5"));
    }

    /// `LogEntry` passes through with level/source/message preserved.
    #[test]
    fn log_entry_passthrough() {
        let mut u = AppEventUpcaster::new();
        let out = u.upcast(&AppEvent::LogEntry {
            session_id: None,
            level: "warn".into(),
            source: "presence".into(),
            content: "something funny".into(),
            turn: Some(3),
        });
        match &out[0] {
            PeerEvent::Log {
                level,
                source,
                message,
                ..
            } => {
                assert_eq!(*level, LogLevel::Warn);
                assert_eq!(source, "presence");
                assert_eq!(message, "something funny");
            }
            _ => panic!("expected Log"),
        }
    }

    // ===================================================================
    // WireEventUpcaster tests — OutboundEvent → PeerEvent
    // ===================================================================

    /// `OutboundEvent` forward-compat: an unknown wire tag
    /// deserializes to `OutboundEvent::Unknown` and the upcaster
    /// drops it silently. This is the guardrail that lets us
    /// evolve the wire protocol without breaking older peers.
    #[test]
    fn outbound_unknown_variant_deserializes_and_drops() {
        let json = r#"{"event":"holographic_projection_started","intensity":"high"}"#;
        let parsed: OutboundEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(parsed, OutboundEvent::Unknown));
        let out = WireEventUpcaster::new().upcast(&parsed);
        assert!(out.is_empty());
    }

    /// Wire-format `TurnStarted` seeds current_message_id and emits
    /// ActivityStarted, same as the AppEvent path.
    #[test]
    fn wire_turn_started_emits_activity_started() {
        let mut u = WireEventUpcaster::new();
        let out = u.upcast(&OutboundEvent::TurnStarted {
            session_id: None,
            turn: 3,
            budget_pct: 0.5,
        });
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            PeerEvent::ActivityStarted {
                kind: ActivityKind::ModelTurn,
                ..
            }
        ));
    }

    /// Wire-format streaming deltas share an id with the final
    /// ModelResponse within the same turn. Same state machine
    /// as the AppEvent path — the parity is *mechanical*, not
    /// coincidental, because both upcasters use the same turn-id
    /// scheme when seeded by `TurnStarted`.
    #[test]
    fn wire_streaming_deltas_share_id_with_final_response() {
        let mut u = WireEventUpcaster::new();
        let _ = u.upcast(&OutboundEvent::TurnStarted {
            session_id: None,
            turn: 5,
            budget_pct: 0.5,
        });
        let delta = u.upcast(&OutboundEvent::ModelResponseDelta {
            session_id: None,
            text: "Hel".into(),
        });
        let final_ = u.upcast(&OutboundEvent::ModelResponse {
            session_id: None,
            turn: 5,
            summary: "Hello".into(),
            reasoning_summary: None,
            source: None,
        });
        let delta_id = match &delta[0] {
            PeerEvent::Message { id, partial, .. } => {
                assert!(*partial);
                id.clone()
            }
            _ => panic!("expected delta Message"),
        };
        let final_id = match &final_[0] {
            PeerEvent::Message { id, partial, .. } => {
                assert!(!partial);
                id.clone()
            }
            _ => panic!("expected final Message"),
        };
        assert_eq!(delta_id, final_id);
    }

    /// Wire-format `Status` maps phase strings to `PeerStatus` via
    /// the shared `status_from_phase` helper (so it's impossible
    /// for wire and app paths to diverge on phase interpretation).
    #[test]
    fn wire_status_phase_mapping() {
        let mut u = WireEventUpcaster::new();
        let out = u.upcast(&OutboundEvent::Status {
            turn: 1,
            phase: "thinking".into(),
            autonomy: "medium".into(),
            session_id: "s".into(),
            task: "t".into(),
            external_agent: None,
        });
        assert!(matches!(
            &out[0],
            PeerEvent::StatusChanged {
                status: PeerStatus::Working,
            }
        ));
    }

    /// Wire-format `CommandResult` — control-plane ack event that
    /// has no AppEvent ancestor — surfaces as a log.
    #[test]
    fn wire_command_result_logs() {
        let mut u = WireEventUpcaster::new();
        let ok = u.upcast(&OutboundEvent::CommandResult {
            action: "approve".into(),
            ok: true,
            message: "resolved".into(),
            data: None,
        });
        match &ok[0] {
            PeerEvent::Log {
                level,
                source,
                message,
                ..
            } => {
                assert_eq!(*level, LogLevel::Info);
                assert_eq!(source, "control");
                assert!(message.contains("approve"));
            }
            _ => panic!("expected Log"),
        }
        let fail = u.upcast(&OutboundEvent::CommandResult {
            action: "deny".into(),
            ok: false,
            message: "bad id".into(),
            data: None,
        });
        match &fail[0] {
            PeerEvent::Log { level, .. } => assert_eq!(*level, LogLevel::Warn),
            _ => panic!("expected Log"),
        }
    }

    // ===================================================================
    // Parity tests — AppEvent → AppEventUpcaster ≡
    //                AppEvent → app_event_to_outbound → WireEventUpcaster
    // ===================================================================
    //
    // The drift guard for the two upcasters. For every AppEvent
    // variant where `app_event_to_outbound` returns `Some(..)` AND
    // the wire projection preserves enough information for the
    // mapping to be lossless, both paths must produce structurally
    // equivalent `Vec<PeerEvent>`. Intentional information loss is
    // marked with its own test and documented — those are expected
    // drift, not parity bugs.

    /// Normalize a list of PeerEvents into JSON with timestamp fields
    /// replaced by a constant. Timestamps (`ts`, `started_at`) are
    /// generated at upcast time via `chrono::Utc::now()` so two
    /// otherwise-equivalent calls will differ on them — the
    /// normalization is what makes structural parity checkable.
    fn normalize(events: &[PeerEvent]) -> Vec<serde_json::Value> {
        fn strip_timestamps(v: &mut serde_json::Value) {
            match v {
                serde_json::Value::Object(obj) => {
                    for key in ["ts", "started_at"] {
                        if obj.contains_key(key) {
                            obj.insert(
                                key.to_string(),
                                serde_json::Value::String("NORMALIZED".into()),
                            );
                        }
                    }
                    for (_, child) in obj.iter_mut() {
                        strip_timestamps(child);
                    }
                }
                serde_json::Value::Array(arr) => {
                    for child in arr.iter_mut() {
                        strip_timestamps(child);
                    }
                }
                _ => {}
            }
        }
        events
            .iter()
            .map(|e| {
                let mut v = serde_json::to_value(e).unwrap();
                strip_timestamps(&mut v);
                v
            })
            .collect()
    }

    /// Run an AppEvent through both paths and assert the normalized
    /// outputs match. Fresh upcasters ensure seq counters start at
    /// zero on both sides so synthesized IDs line up.
    fn assert_parity(app_event: AppEvent) {
        let mut app_upcaster = AppEventUpcaster::new();
        let mut wire_upcaster = WireEventUpcaster::new();

        let path_a = app_upcaster.upcast(&app_event);

        let outbound = crate::event::app_event_to_outbound(&app_event).unwrap_or_else(|| {
            panic!(
                "app_event_to_outbound returned None for {:?} — not eligible for parity check",
                app_event
            )
        });
        let path_b = wire_upcaster.upcast(&outbound);

        let a = normalize(&path_a);
        let b = normalize(&path_b);

        assert_eq!(
            a, b,
            "parity failure for {app_event:?}\npath A (app) = {a:#?}\npath B (wire) = {b:#?}"
        );
    }

    #[test]
    fn parity_turn_started() {
        assert_parity(AppEvent::TurnStarted {
            session_id: None,
            turn: 7,
            budget_pct: 0.5,
            remaining: 100,
        });
    }

    #[test]
    fn parity_session_started() {
        assert_parity(AppEvent::SessionStarted {
            session_id: "sess-99".into(),
            task: Some("research".into()),
        });
    }

    #[test]
    fn parity_session_ended() {
        assert_parity(AppEvent::SessionEnded {
            session_id: "sess-99".into(),
            reason: "done".into(),
            error_kind: None,
        });
    }

    #[test]
    fn parity_display_ready() {
        assert_parity(AppEvent::DisplayReady {
            display_id: 1,
            width: 1920,
            height: 1080,
            agent_visible: true,
        });
    }

    #[test]
    fn parity_display_released() {
        assert_parity(AppEvent::DisplayReleased {
            display_id: 1,
            note: Some("user revoked".into()),
        });
    }

    #[test]
    fn parity_display_capture_lost() {
        assert_parity(AppEvent::DisplayCaptureLost {
            display_id: 1,
            reason: "backend_crashed".into(),
        });
    }

    // ---- Display-availability fold ----

    fn wire_display_ready(display_id: u32, width: u32, height: u32) -> OutboundEvent {
        OutboundEvent::DisplayReady {
            display_id,
            width,
            height,
            agent_visible: true,
        }
    }

    #[test]
    fn wire_display_fold_emits_change_only() {
        let mut up = WireEventUpcaster::new();

        // First announce emits.
        let first = up.upcast(&wire_display_ready(99, 1920, 1080));
        assert_eq!(first.len(), 1);
        assert!(matches!(
            &first[0],
            PeerEvent::DisplayReady { display }
                if *display == PeerDisplayInfo { display_id: 99, width: 1920, height: 1080 }
        ));

        // The gateway replays display_ready on every transport connect —
        // an identical repeat must be silent.
        assert!(up.upcast(&wire_display_ready(99, 1920, 1080)).is_empty());

        // A geometry change re-announces (display_resize path).
        let resized = up.upcast(&OutboundEvent::DisplayResize {
            display_id: 99,
            width: 1280,
            height: 720,
        });
        assert!(resized.iter().any(|event| matches!(
            event,
            PeerEvent::DisplayReady {
                display: PeerDisplayInfo {
                    display_id: 99,
                    width: 1280,
                    height: 720,
                }
            }
        )));
    }

    #[test]
    fn wire_display_fold_retires_on_capture_lost_and_revoke() {
        let mut up = WireEventUpcaster::new();
        up.upcast(&wire_display_ready(99, 1920, 1080));
        up.upcast(&wire_display_ready(0, 2560, 1440));

        let lost = up.upcast(&OutboundEvent::DisplayCaptureLost {
            display_id: 99,
            reason: "backend_crashed".into(),
        });
        assert_eq!(lost.len(), 1);
        assert!(matches!(
            &lost[0],
            PeerEvent::DisplayLost { display_id: 99, reason: Some(reason) }
                if reason == "capture_lost: backend_crashed"
        ));

        // Revoking the user display retires it too (alongside the
        // unconditional log line).
        let revoked = up.upcast(&OutboundEvent::UserDisplayRevoked {
            display_id: 0,
            note: None,
        });
        assert!(revoked
            .iter()
            .any(|event| matches!(event, PeerEvent::DisplayLost { display_id: 0, .. })));

        // Losing a display this connection never saw announces nothing.
        let unknown = up.upcast(&OutboundEvent::DisplayCaptureLost {
            display_id: 7,
            reason: "never seen".into(),
        });
        assert!(unknown.is_empty());
    }

    #[test]
    fn wire_display_fold_refuses_new_ids_at_cap() {
        let mut up = WireEventUpcaster::new();
        for id in 0..(MAX_TRACKED_PEER_DISPLAYS as u32) {
            assert_eq!(up.upcast(&wire_display_ready(id, 100, 100)).len(), 1);
        }
        // New id at cap: refused, silent.
        assert!(up
            .upcast(&wire_display_ready(
                MAX_TRACKED_PEER_DISPLAYS as u32,
                100,
                100
            ))
            .is_empty());
        // Existing ids keep updating.
        assert_eq!(up.upcast(&wire_display_ready(0, 200, 200)).len(), 1);
    }

    #[test]
    fn replay_lane_does_not_fold_displays() {
        // Historical display_ready in a served log_replay must not seed
        // *current* availability — the replay lane folds session state
        // only; live displays arrive as real wire events via the
        // gateway's on-connect replay.
        let mut up = WireEventUpcaster::new();
        assert!(up
            .upcast_replayed(&wire_display_ready(99, 1920, 1080))
            .is_empty());
        // And the live fold stayed clean: the same display arriving
        // live afterwards still announces.
        assert_eq!(up.upcast(&wire_display_ready(99, 1920, 1080)).len(), 1);
    }

    #[test]
    fn parity_recording_started() {
        assert_parity(AppEvent::RecordingStarted {
            stream_name: "display-1".into(),
        });
    }

    #[test]
    fn parity_recording_stopped() {
        assert_parity(AppEvent::RecordingStopped {
            stream_name: "display-1".into(),
        });
    }

    #[test]
    fn parity_recording_error() {
        assert_parity(AppEvent::RecordingError {
            stream_name: "display-1".into(),
            message: "encoder lost".into(),
        });
    }

    #[test]
    fn parity_round_complete() {
        assert_parity(AppEvent::RoundComplete {
            session_id: None,
            round: 3,
            turns_in_round: 7,
            native_message_count: None,
        });
    }

    #[test]
    fn parity_human_response_sent() {
        assert_parity(AppEvent::HumanResponseSent);
    }

    #[test]
    fn parity_safety_cap_reached() {
        assert_parity(AppEvent::SafetyCapReached);
    }

    #[test]
    fn parity_context_management() {
        assert_parity(AppEvent::ContextManagement { turn: 5 });
    }

    #[test]
    fn parity_budget_warning() {
        assert_parity(AppEvent::BudgetWarning {
            pct: 12.5,
            remaining: 1000,
        });
    }

    #[test]
    fn parity_budget_exhausted() {
        assert_parity(AppEvent::BudgetExhausted { remaining: 0 });
    }

    #[test]
    fn parity_external_agent_changed() {
        assert_parity(AppEvent::ExternalAgentChanged {
            agent: Some("codex".into()),
        });
    }

    #[test]
    fn parity_autonomy_changed() {
        assert_parity(AppEvent::AutonomyChanged {
            autonomy: "High".into(),
        });
    }

    #[test]
    fn parity_log_entry() {
        assert_parity(AppEvent::LogEntry {
            session_id: None,
            level: "warn".into(),
            source: "presence".into(),
            content: "something funny".into(),
            turn: Some(3),
        });
    }

    #[test]
    fn parity_loop_error() {
        assert_parity(AppEvent::LoopError("kaboom".to_string()));
    }

    #[test]
    fn parity_steer_requested() {
        assert_parity(AppEvent::SteerRequested {
            session_id: None,
            text: "look at tests/e2e/ first".into(),
            id: "steer-42".into(),
        });
    }

    #[test]
    fn parity_steer_requested_empty_id() {
        // Empty id is a valid "no correlation" sentinel; the log line
        // should simply omit the [id] part on both paths.
        assert_parity(AppEvent::SteerRequested {
            session_id: None,
            text: "never mind".into(),
            id: String::new(),
        });
    }

    #[test]
    fn parity_steer_queued() {
        assert_parity(AppEvent::SteerQueued {
            session_id: None,
            id: "steer-7".into(),
            reason: "Claude Code doesn't support mid-turn steering".into(),
        });
    }

    #[test]
    fn parity_steer_accepted() {
        assert_parity(AppEvent::SteerAccepted {
            session_id: None,
            id: "steer-8".into(),
            reason: "Codex accepted the steer".into(),
        });
    }

    #[test]
    fn parity_steer_delivered_mid_turn() {
        assert_parity(AppEvent::SteerDelivered {
            session_id: None,
            id: "steer-3".into(),
            mid_turn: true,
        });
    }

    #[test]
    fn parity_steer_delivered_followup() {
        assert_parity(AppEvent::SteerDelivered {
            session_id: None,
            id: "steer-3".into(),
            mid_turn: false,
        });
    }

    #[test]
    fn parity_steer_cancelled() {
        assert_parity(AppEvent::SteerCancelled {
            session_id: None,
            id: "steer-9".into(),
            reason: "cleared by user".into(),
        });
    }

    // -------------------------------------------------------------------
    // Documented drift — intentional information loss in the wire path.
    // These cases are NOT bugs; they're the wire protocol's documented
    // lossy projections. Each test captures the specific loss so a
    // future refactor that accidentally widens the drift trips one of
    // them.
    // -------------------------------------------------------------------

    /// `ModelResponse` emits Message + Usage on the app path, but on
    /// the wire path usage travels separately as `OutboundEvent::Usage`.
    /// Parity holds only on the Message prefix; Usage is verified
    /// separately to belong to the main-path output.
    #[test]
    fn drift_model_response_usage_is_separated_on_wire() {
        let app_event = AppEvent::ModelResponse {
            session_id: None,
            turn: 1,
            content: "Hello world".into(),
            usage: crate::provider::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cached_tokens: 2,
                ..Default::default()
            },
            reasoning: None,
            source: None,
        };
        let mut app_upcaster = AppEventUpcaster::new();
        let path_a = app_upcaster.upcast(&app_event);
        // Path A: Message + Usage (2 events).
        assert_eq!(path_a.len(), 2);
        assert!(matches!(&path_a[0], PeerEvent::Message { .. }));
        assert!(matches!(&path_a[1], PeerEvent::Usage { .. }));

        let outbound = crate::event::app_event_to_outbound(&app_event).expect("ModelResponse maps");
        let mut wire_upcaster = WireEventUpcaster::new();
        let path_b = wire_upcaster.upcast(&outbound);
        // Path B: just Message — usage arrives in a separate event.
        assert_eq!(path_b.len(), 1);
        assert!(matches!(&path_b[0], PeerEvent::Message { .. }));
        // The Message content should still agree between the two.
        let a_msg = normalize(&path_a[..1]);
        let b_msg = normalize(&path_b[..1]);
        assert_eq!(
            a_msg, b_msg,
            "Message half of ModelResponse must agree across paths"
        );
    }

    /// `ApprovalRequired` loses its `ActionCategory` field on the wire
    /// — the wire format only carries `id` + `command`. The wire
    /// path fills in `"command_exec"` as the default category. Path A
    /// preserves the actual category (e.g. "file_delete").
    #[test]
    fn drift_approval_required_category_is_dropped_on_wire() {
        let app_event = AppEvent::ApprovalRequired {
            session_id: None,
            id: 42,
            command_preview: "rm -rf /tmp/foo".into(),
            category: crate::autonomy::ActionCategory::FileDelete,
        };
        let mut app_upcaster = AppEventUpcaster::new();
        let path_a = app_upcaster.upcast(&app_event);
        let category_a = match &path_a[0] {
            PeerEvent::ApprovalRequested { request } => request.category.clone(),
            _ => panic!("expected ApprovalRequested"),
        };
        assert_eq!(category_a, "file_delete");

        let outbound =
            crate::event::app_event_to_outbound(&app_event).expect("ApprovalRequired maps");
        let mut wire_upcaster = WireEventUpcaster::new();
        let path_b = wire_upcaster.upcast(&outbound);
        let category_b = match &path_b[0] {
            PeerEvent::ApprovalRequested { request } => request.category.clone(),
            _ => panic!("expected ApprovalRequested"),
        };
        assert_eq!(
            category_b, "command_exec",
            "wire path uses default category because ActionCategory isn't on the wire"
        );
    }

    /// `UserDisplayGranted` carries `display_id` + `agent_visible` on
    /// the wire since the private-view split (it was fieldless before);
    /// both upcast paths preserve the id, and private views
    /// (`agent_visible: false`) are silent on BOTH paths — peers are
    /// never told about the owner's private screen view.
    #[test]
    fn user_display_granted_paths_agree_and_hide_private_views() {
        let app_event = AppEvent::UserDisplayGranted {
            display_id: 99,
            agent_visible: true,
        };
        let mut app_upcaster = AppEventUpcaster::new();
        let path_a = app_upcaster.upcast(&app_event);
        let msg_a = match &path_a[0] {
            PeerEvent::Log { message, .. } => message.clone(),
            _ => panic!("expected Log"),
        };
        assert!(
            msg_a.contains("99"),
            "app path preserves display_id in log: {msg_a}"
        );

        let outbound =
            crate::event::app_event_to_outbound(&app_event).expect("UserDisplayGranted maps");
        let mut wire_upcaster = WireEventUpcaster::new();
        let path_b = wire_upcaster.upcast(&outbound);
        let msg_b = match &path_b[0] {
            PeerEvent::Log { message, .. } => message.clone(),
            _ => panic!("expected Log"),
        };
        assert!(
            msg_b.contains("99"),
            "wire path preserves display_id since the private-view split: {msg_b}"
        );

        // Private view: silence on both paths.
        let private = AppEvent::UserDisplayGranted {
            display_id: 3,
            agent_visible: false,
        };
        assert!(
            app_upcaster.upcast(&private).is_empty(),
            "app path must not announce a private view to peers"
        );
        let outbound_private =
            crate::event::app_event_to_outbound(&private).expect("UserDisplayGranted maps");
        assert!(
            wire_upcaster.upcast(&outbound_private).is_empty(),
            "wire path must not announce a private view to peers"
        );

        // Legacy fieldless wire line: defaults keep the old meaning
        // (display 0, agent-visible) and still announce.
        let legacy: OutboundEvent =
            serde_json::from_str(r#"{"event":"user_display_granted"}"#).unwrap();
        let legacy_events = wire_upcaster.upcast(&legacy);
        assert_eq!(legacy_events.len(), 1, "legacy grant lines still log");
    }

    // ===================================================================
    // Per-session fold tests
    // ===================================================================

    fn vitals_with_git() -> crate::types::SessionVitals {
        crate::types::SessionVitals {
            git: Some(crate::types::SessionGitVitals {
                branch: "main".into(),
                dirty_files: 2,
                ahead: 1,
                behind: 0,
                primary_ref: "origin/main".into(),
                merge_parity: "clean".into(),
                unpushed: Some(0),
                primary_unpushed: None,
            }),
            cache: None,
            limits: Vec::new(),
            activity: None,
            config: None,
        }
    }

    /// The wire fold enriches one session's snapshot across the whole
    /// event stream — identity, relationship, goal, vitals, status,
    /// usage, approvals — and retires it on SessionEnded.
    #[test]
    fn wire_session_fold_enriches_across_event_stream() {
        let mut u = WireEventUpcaster::new();

        let out = u.upcast(&OutboundEvent::SessionStarted {
            session_id: "s1".into(),
            task: Some("port the vitals".into()),
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            PeerEvent::SessionStarted { session } => {
                assert_eq!(session.session_id, "s1");
                assert_eq!(session.label.as_deref(), Some("port the vitals"));
                assert!(!session.is_primary);
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }

        let out = u.upcast(&OutboundEvent::SessionIdentity {
            session_id: "s1".into(),
            source: "codex".into(),
            backend_session_id: "thread-1".into(),
        });
        match &out[0] {
            PeerEvent::SessionUpdated { session } => assert_eq!(session.source, "codex"),
            other => panic!("expected SessionUpdated, got {other:?}"),
        }

        let out = u.upcast(&OutboundEvent::SessionRelationship {
            parent_session_id: "s0".into(),
            child_session_id: "s1".into(),
            relationship: "subagent".into(),
            ephemeral: false,
        });
        match &out[0] {
            PeerEvent::SessionUpdated { session } => {
                assert_eq!(session.parent_session_id.as_deref(), Some("s0"));
                assert_eq!(session.relationship, "subagent");
                assert!(!session.ephemeral);
            }
            other => panic!("expected SessionUpdated, got {other:?}"),
        }

        let out = u.upcast(&OutboundEvent::SessionGoal {
            session_id: "s1".into(),
            goal: Some(crate::types::SessionGoal {
                objective: "ship it".into(),
                status: Some("active".into()),
                elapsed_seconds: None,
                tokens_used: Some(5),
                token_budget: Some(100),
            }),
        });
        match &out[0] {
            PeerEvent::SessionUpdated { session } => {
                assert_eq!(
                    session.goal.as_ref().and_then(|g| g.status.as_deref()),
                    Some("active")
                );
            }
            other => panic!("expected SessionUpdated, got {other:?}"),
        }

        let out = u.upcast(&OutboundEvent::SessionVitals {
            session_id: "s1".into(),
            vitals: vitals_with_git(),
        });
        match &out[0] {
            PeerEvent::SessionUpdated { session } => {
                assert_eq!(
                    session
                        .vitals
                        .as_ref()
                        .and_then(|v| v.git.as_ref())
                        .map(|g| g.dirty_files),
                    Some(2)
                );
            }
            other => panic!("expected SessionUpdated, got {other:?}"),
        }

        let out = u.upcast(&OutboundEvent::Status {
            turn: 3,
            phase: "working".into(),
            autonomy: "full".into(),
            session_id: "s1".into(),
            task: String::new(),
            external_agent: None,
        });
        assert_eq!(out.len(), 2, "StatusChanged + SessionUpdated");
        assert!(matches!(out[0], PeerEvent::StatusChanged { .. }));
        match &out[1] {
            PeerEvent::SessionUpdated { session } => assert_eq!(session.phase, "working"),
            other => panic!("expected SessionUpdated, got {other:?}"),
        }

        let out = u.upcast(&OutboundEvent::Usage {
            session_id: Some("s1".into()),
            main: crate::frontend::ModelUsageSnapshot {
                provider: "openai".into(),
                model: "gpt".into(),
                prompt_tokens: 100,
                completion_tokens: 50,
                ..Default::default()
            },
            presence: None,
        });
        assert_eq!(out.len(), 2, "Usage + SessionUpdated");
        match &out[1] {
            PeerEvent::SessionUpdated { session } => {
                assert_eq!(session.tokens_used, Some(150));
            }
            other => panic!("expected SessionUpdated, got {other:?}"),
        }

        let out = u.upcast(&OutboundEvent::ApprovalRequired {
            session_id: Some("s1".into()),
            id: 7,
            command: "rm -rf scratch".into(),
        });
        assert_eq!(out.len(), 2, "ApprovalRequested + SessionUpdated");
        match &out[1] {
            PeerEvent::SessionUpdated { session } => assert!(session.needs_approval),
            other => panic!("expected SessionUpdated, got {other:?}"),
        }

        let out = u.upcast(&OutboundEvent::ApprovalResolved {
            session_id: Some("s1".into()),
            id: 7,
            action: "approve".into(),
        });
        assert_eq!(out.len(), 2, "ApprovalResolved + SessionUpdated");
        match &out[1] {
            PeerEvent::SessionUpdated { session } => assert!(!session.needs_approval),
            other => panic!("expected SessionUpdated, got {other:?}"),
        }

        let out = u.upcast(&OutboundEvent::SessionEnded {
            session_id: "s1".into(),
            reason: "done".into(),
            error_kind: None,
        });
        assert_eq!(out.len(), 1, "ended retires the entry — no trailing update");
        assert!(matches!(out[0], PeerEvent::SessionEnded { .. }));
        assert!(u.sessions.sessions.is_empty());
        assert!(u.sessions.pending_approvals.is_empty());
    }

    /// Native daemon-lane sessions have no per-session `status` rail —
    /// their phases fold from the lifecycle events they do emit:
    /// TurnStarted/AgentStarted → working, DoneSignal/TaskComplete →
    /// done. (This is what makes remote native sessions show live
    /// phases at all; see the two-daemon rig.)
    #[test]
    fn wire_session_fold_derives_phase_from_lifecycle_events() {
        let mut u = WireEventUpcaster::new();
        let _ = u.upcast(&OutboundEvent::SessionStarted {
            session_id: "s1".into(),
            task: Some("delegated".into()),
        });

        let out = u.upcast(&OutboundEvent::TurnStarted {
            session_id: Some("s1".into()),
            turn: 1,
            budget_pct: 0.1,
        });
        let updated = out.iter().find_map(|e| match e {
            PeerEvent::SessionUpdated { session } => Some(session.clone()),
            _ => None,
        });
        assert_eq!(updated.expect("TurnStarted folds phase").phase, "working");

        // Same-phase repeat (AgentStarted while already working) must
        // not re-emit.
        let out = u.upcast(&OutboundEvent::AgentStarted {
            session_id: Some("s1".into()),
            turn: 1,
            commands_preview: "echo hi".into(),
            item_id: None,
            source: None,
        });
        assert!(
            !out.iter()
                .any(|e| matches!(e, PeerEvent::SessionUpdated { .. })),
            "no duplicate SessionUpdated for an unchanged phase"
        );

        let out = u.upcast(&OutboundEvent::TaskComplete {
            session_id: Some("s1".into()),
            reason: "success".into(),
            summary: None,
        });
        let updated = out.iter().find_map(|e| match e {
            PeerEvent::SessionUpdated { session } => Some(session.clone()),
            _ => None,
        });
        assert_eq!(updated.expect("TaskComplete folds phase").phase, "done");
    }

    /// The replay lane and the live lane must fold to the SAME
    /// session state for the same wire stream — the drift guard for
    /// `upcast_replayed`'s duplicated fold closures. The replay lane
    /// must also emit no live-activity events at all.
    #[test]
    fn replay_lane_folds_identically_to_live_lane() {
        let stream = [
            OutboundEvent::SessionStarted {
                session_id: "s1".into(),
                task: Some("delegated".into()),
            },
            OutboundEvent::SessionIdentity {
                session_id: "s1".into(),
                source: "codex".into(),
                backend_session_id: "b".into(),
            },
            OutboundEvent::SessionRelationship {
                parent_session_id: "s0".into(),
                child_session_id: "s1".into(),
                relationship: "subagent".into(),
                ephemeral: false,
            },
            OutboundEvent::SessionVitals {
                session_id: "s1".into(),
                vitals: vitals_with_git(),
            },
            OutboundEvent::TurnStarted {
                session_id: Some("s1".into()),
                turn: 1,
                budget_pct: 0.1,
            },
            OutboundEvent::Usage {
                session_id: Some("s1".into()),
                main: crate::frontend::ModelUsageSnapshot {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    ..Default::default()
                },
                presence: None,
            },
            OutboundEvent::ApprovalRequired {
                session_id: Some("s1".into()),
                id: 3,
                command: "x".into(),
            },
            OutboundEvent::TaskComplete {
                session_id: Some("s1".into()),
                reason: "success".into(),
                summary: None,
            },
        ];

        let mut live = WireEventUpcaster::new();
        let mut replay = WireEventUpcaster::new();
        for event in &stream {
            let _ = live.upcast(event);
            for out in replay.upcast_replayed(event) {
                assert!(
                    matches!(
                        out,
                        PeerEvent::SessionStarted { .. }
                            | PeerEvent::SessionUpdated { .. }
                            | PeerEvent::SessionEnded { .. }
                    ),
                    "replay lane leaked a live-activity event: {out:?}"
                );
            }
        }

        let normalize = |u: &WireEventUpcaster| {
            u.sessions
                .sessions
                .values()
                .map(|s| {
                    let mut v = serde_json::to_value(s).unwrap();
                    v["started_at"] = serde_json::Value::String("NORM".into());
                    v
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            normalize(&live),
            normalize(&replay),
            "replay fold diverged from live fold"
        );
    }

    #[test]
    fn parity_turn_started_with_session_folds_phase() {
        assert_parity(AppEvent::TurnStarted {
            session_id: Some("s1".into()),
            turn: 7,
            budget_pct: 0.5,
            remaining: 100,
        });
    }

    #[test]
    fn parity_task_complete_with_session_folds_phase() {
        assert_parity(AppEvent::TaskComplete {
            session_id: Some("s1".into()),
            reason: "success".into(),
            summary: Some("all done".into()),
        });
    }

    /// Chatty sources (per-tick status) must not spam SessionUpdated:
    /// an identical repeat emits only the host-level StatusChanged.
    #[test]
    fn wire_session_fold_emits_only_on_change() {
        let mut u = WireEventUpcaster::new();
        let status = OutboundEvent::Status {
            turn: 1,
            phase: "working".into(),
            autonomy: "full".into(),
            session_id: "s1".into(),
            task: "t".into(),
            external_agent: None,
        };
        let first = u.upcast(&status);
        assert_eq!(first.len(), 2, "first status creates the session entry");
        let second = u.upcast(&status);
        assert_eq!(
            second.len(),
            1,
            "identical status must not re-emit SessionUpdated"
        );
        assert!(matches!(second[0], PeerEvent::StatusChanged { .. }));
    }

    /// An event naming a session that was never announced (a primary
    /// that connected mid-flight) upserts the entry — the fold is
    /// self-healing across reconnects.
    #[test]
    fn wire_session_fold_upserts_unknown_sessions() {
        let mut u = WireEventUpcaster::new();
        let out = u.upcast(&OutboundEvent::SessionVitals {
            session_id: "ghost".into(),
            vitals: vitals_with_git(),
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            PeerEvent::SessionUpdated { session } => {
                assert_eq!(session.session_id, "ghost");
                assert!(session.vitals.is_some());
                assert!(!session.started_at.is_empty());
            }
            other => panic!("expected SessionUpdated, got {other:?}"),
        }
    }

    /// `set_primary_session_id` stamps `is_primary` whether it is
    /// learned before or after the session itself (the bootstrap
    /// frame's ordering relative to live events is connection-dependent).
    #[test]
    fn wire_session_fold_stamps_primary_before_and_after_learning() {
        let mut u = WireEventUpcaster::new();
        u.set_primary_session_id("main-1");
        let out = u.upcast(&OutboundEvent::SessionStarted {
            session_id: "main-1".into(),
            task: None,
        });
        match &out[0] {
            PeerEvent::SessionStarted { session } => assert!(session.is_primary),
            other => panic!("expected SessionStarted, got {other:?}"),
        }

        let mut u = WireEventUpcaster::new();
        let _ = u.upcast(&OutboundEvent::SessionStarted {
            session_id: "main-2".into(),
            task: None,
        });
        u.set_primary_session_id("main-2");
        let out = u.upcast(&OutboundEvent::SessionIdentity {
            session_id: "main-2".into(),
            source: "intendant".into(),
            backend_session_id: "x".into(),
        });
        match &out[0] {
            PeerEvent::SessionUpdated { session } => {
                assert!(session.is_primary, "retro-stamp must apply");
            }
            other => panic!("expected SessionUpdated, got {other:?}"),
        }
    }

    /// The fold is bounded: a peer that announces sessions without
    /// ever ending them evicts oldest-started at the cap.
    #[test]
    fn wire_session_fold_evicts_oldest_over_cap() {
        let mut u = WireEventUpcaster::new();
        for i in 0..(MAX_TRACKED_PEER_SESSIONS + 10) {
            let _ = u.upcast(&OutboundEvent::SessionStarted {
                session_id: format!("s{i:04}"),
                task: None,
            });
        }
        assert_eq!(u.sessions.sessions.len(), MAX_TRACKED_PEER_SESSIONS);
        assert!(
            u.sessions
                .sessions
                .contains_key(&format!("s{:04}", MAX_TRACKED_PEER_SESSIONS + 9)),
            "newest session survives"
        );
        assert!(
            !u.sessions.sessions.contains_key("s0000"),
            "oldest session evicted"
        );
    }

    // ---- Parity for the fold arms ----

    #[test]
    fn parity_session_identity() {
        assert_parity(AppEvent::SessionIdentity {
            session_id: "s1".into(),
            source: "codex".into(),
            backend_session_id: "b".into(),
        });
    }

    #[test]
    fn parity_session_relationship() {
        assert_parity(AppEvent::SessionRelationship {
            parent_session_id: "s0".into(),
            child_session_id: "s1".into(),
            relationship: "subagent".into(),
            ephemeral: true,
        });
    }

    #[test]
    fn parity_session_goal() {
        assert_parity(AppEvent::SessionGoal {
            session_id: "s1".into(),
            goal: Some(crate::types::SessionGoal {
                objective: "ship it".into(),
                status: Some("active".into()),
                elapsed_seconds: Some(60),
                tokens_used: Some(5),
                token_budget: Some(100),
            }),
        });
    }

    #[test]
    fn parity_session_vitals() {
        assert_parity(AppEvent::SessionVitals {
            session_id: "s1".into(),
            vitals: vitals_with_git(),
        });
    }

    #[test]
    fn parity_status_update_folds_session_phase() {
        assert_parity(AppEvent::StatusUpdate {
            turn: 2,
            phase: "working".into(),
            autonomy: "full".into(),
            session_id: "s1".into(),
            task: "t".into(),
        });
    }

    #[test]
    fn parity_usage_snapshot_with_session() {
        assert_parity(AppEvent::UsageSnapshot {
            session_id: Some("s1".into()),
            main: crate::frontend::ModelUsageSnapshot {
                provider: "openai".into(),
                model: "gpt".into(),
                prompt_tokens: 100,
                completion_tokens: 50,
                ..Default::default()
            },
            presence: None,
        });
    }

    /// Approval parity holds for `CommandExec` (the wire path hardcodes
    /// that category — the documented intentional loss covers the rest)
    /// and both paths append the same SessionUpdated fold event.
    #[test]
    fn parity_approval_lifecycle_folds_needs_approval() {
        assert_parity(AppEvent::ApprovalRequired {
            session_id: Some("s1".into()),
            id: 7,
            command_preview: "cargo test".into(),
            category: crate::autonomy::ActionCategory::CommandExec,
        });
        assert_parity(AppEvent::ApprovalResolved {
            session_id: Some("s1".into()),
            id: 7,
            action: "approve".into(),
        });
    }

    /// `OutboundEvent::WebRtcSignal` upcasts 1:1 to
    /// `PeerEvent::WebRtcSignal` so federation-side consumers
    /// (registry, dashboard) get the typed event without re-parsing
    /// the wire-level string session_id.
    #[test]
    fn webrtc_signal_outbound_upcasts_to_peer_event() {
        let mut u = WireEventUpcaster::new();
        let out = crate::types::OutboundEvent::WebRtcSignal {
            display_id: 42,
            session_id: "sess-uuid".into(),
            signal: crate::peer::WebRtcSignal::Answer {
                sdp: "v=0\r\nm=video".into(),
                binding: None,
            },
        };
        let events = u.upcast(&out);
        assert_eq!(events.len(), 1);
        match &events[0] {
            PeerEvent::WebRtcSignal {
                display_id,
                session_id,
                signal,
            } => {
                assert_eq!(*display_id, 42);
                assert_eq!(session_id.0, "sess-uuid");
                match signal {
                    crate::peer::WebRtcSignal::Answer { sdp, .. } => {
                        assert_eq!(sdp, "v=0\r\nm=video");
                    }
                    other => panic!("expected Answer, got {other:?}"),
                }
            }
            other => panic!("expected WebRtcSignal, got {other:?}"),
        }
    }
}

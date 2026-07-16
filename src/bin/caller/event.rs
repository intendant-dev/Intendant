//! Shared event infrastructure used across all frontends
//! (MCP, control socket, web gateway, presence, dashboard control).

use crate::autonomy::ActionCategory;
use crate::provider::TokenUsage;
use crate::types::{LogLevel, SessionCapabilities, SessionGoal};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_AGENT_OUTPUT_ID: AtomicU64 = AtomicU64::new(1);
const OUTBOUND_CONTEXT_SNAPSHOT_RAW_INLINE_LIMIT: usize = 128 * 1024;

pub fn next_agent_output_id() -> String {
    let seq = NEXT_AGENT_OUTPUT_ID.fetch_add(1, Ordering::Relaxed);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    format!("ao-{millis:x}-{seq:x}")
}

/// Source of a context injection item.
///
/// Used by the agent loop to decide which queued injections to discard between
/// tasks.  System injections (display take/release notifications, etc.) are
/// purged when a new task starts so they don't pollute the next task's context.
/// User injections (annotations sent from the web dashboard) are preserved so
/// the agent always sees them, even if they were queued while idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjectionSource {
    /// Originated from a human action (annotation Send button, etc.).
    User,
    /// Generated automatically by the runtime (display grants, etc.).
    System,
}

/// A context injection item: text with optional images.
#[derive(Clone)]
pub struct ContextInjection {
    pub text: String,
    pub images: Vec<crate::conversation::ImageData>,
    pub source: InjectionSource,
    /// Optional session/thread that a queued steer is meant for.
    ///
    /// `None` preserves legacy/global behavior for non-steer injections and
    /// for any older queued steer entries that predate targeted routing.
    pub target_session_id: Option<String>,
    /// When this injection was queued by a steer, this carries the steer id so a
    /// `SteerDelivered` event can be emitted with the right correlation id
    /// when the item is eventually drained into the agent's conversation.
    ///
    /// `None` for non-steer injections (display take/release, presence
    /// annotations, etc.) — those don't participate in the steer protocol.
    pub steer_id: Option<String>,
}

impl ContextInjection {
    /// Create a text-only system injection (no images).
    pub fn text(msg: String) -> Self {
        Self {
            text: msg,
            images: vec![],
            source: InjectionSource::System,
            target_session_id: None,
            steer_id: None,
        }
    }

    /// Create a text-only user injection (no images).
    #[allow(dead_code)]
    pub fn user_text(msg: String) -> Self {
        Self {
            text: msg,
            images: vec![],
            source: InjectionSource::User,
            target_session_id: None,
            steer_id: None,
        }
    }

    /// Create a user injection that originated from a queued steer. The id
    /// round-trips back out via `AppEvent::SteerDelivered` when the item is
    /// drained so frontends can correlate their pending-steer UI.
    #[allow(dead_code)]
    pub fn text_with_steer_id(msg: String, steer_id: String) -> Self {
        Self::text_with_steer_id_for_target(msg, steer_id, None)
    }

    /// Create a queued steer injection scoped to a specific session/thread.
    pub fn text_with_steer_id_for_target(
        msg: String,
        steer_id: String,
        target_session_id: Option<String>,
    ) -> Self {
        Self {
            text: msg,
            images: vec![],
            source: InjectionSource::User,
            target_session_id,
            steer_id: Some(steer_id),
        }
    }
}

/// Shared queue for context messages to inject into the agent conversation.
/// Used by display takeover/release to notify the agent between turns,
/// and by presence to inject mid-task interjections.
pub type ContextInjectionQueue = Arc<Mutex<Vec<ContextInjection>>>;

/// Shared registry for pending approval responders.
///
/// When the agent loop needs approval, it inserts a `oneshot::Sender` here
/// keyed by approval ID, then sends an `ApprovalRequired` event (without the
/// responder). Whichever frontend resolves the approval (TUI key press, web
/// button, control socket command) removes the sender and calls `.send()`.
pub type ApprovalRegistry =
    Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<ApprovalResponse>>>>;

/// All events flowing through the system.
#[derive(Debug, Clone)]
pub enum AppEvent {
    // Agent loop lifecycle
    TurnStarted {
        session_id: Option<String>,
        turn: usize,
        budget_pct: f64,
        #[allow(dead_code)]
        remaining: u64,
    },
    ModelResponse {
        session_id: Option<String>,
        turn: usize,
        content: String,
        usage: TokenUsage,
        reasoning: Option<String>,
        /// Override source label in Activity tab (e.g. "Codex").
        #[allow(dead_code)]
        source: Option<String>,
    },
    /// Incremental text delta from streaming model response.
    ModelResponseDelta {
        session_id: Option<String>,
        text: String,
    },
    JsonExtracted {
        preview: String,
    },
    DoneSignal {
        session_id: Option<String>,
        message: Option<String>,
    },
    AgentStarted {
        session_id: Option<String>,
        turn: usize,
        commands_preview: String,
        item_id: Option<String>,
        source: Option<String>,
    },
    AgentOutput {
        session_id: Option<String>,
        stdout: String,
        stderr: String,
        source: Option<String>,
        output_id: Option<String>,
        /// The originating tool call (`AgentStarted.item_id`) when the
        /// backend correlates output to calls — frontends group output
        /// under its command instead of coalescing consecutive tools.
        item_id: Option<String>,
    },
    SubAgentResult {
        formatted: String,
    },
    ContextManagement {
        turn: usize,
    },
    TaskComplete {
        session_id: Option<String>,
        reason: String,
        summary: Option<String>,
    },
    /// User requested interruption; broadcast to agent loops so they can cancel.
    InterruptRequested {
        session_id: Option<String>,
    },
    /// User requested that a managed session stop completely. External-agent
    /// loops listen for this and shut down their backend process.
    SessionStopRequested {
        session_id: Option<String>,
        reason: String,
    },
    /// The agent turn was interrupted. Emitted by the loop once cancellation completes.
    Interrupted {
        session_id: Option<String>,
        reason: String,
    },

    // ---- Mid-turn steering (interjection) ----
    /// User requested mid-turn steering. Re-emitted by the task dispatcher
    /// from `ControlMsg::Steer`. Agent loops subscribe to this and either
    /// call `ExternalAgent::steer_turn()` (for backends that support it) or
    /// fall back to queuing onto `context_injection`.
    ///
    /// `id` is always a String (empty = no correlation). This keeps
    /// downstream consumers simple — they never need to deal with a
    /// missing id, and comparing `id.is_empty()` is cheap.
    SteerRequested {
        session_id: Option<String>,
        text: String,
        id: String,
    },
    /// Native steering was accepted by the active backend/runtime. This only
    /// means the backend accepted responsibility for applying it at a runtime
    /// checkpoint; it does not prove the model has seen it yet.
    SteerAccepted {
        session_id: Option<String>,
        id: String,
        /// Short human-readable detail for UI display, e.g. "Codex accepted
        /// the steer; waiting for the next runtime checkpoint".
        reason: String,
    },
    /// Mid-turn steering could not be delivered natively by the current
    /// backend and fell back to non-native follow-up delivery. For native
    /// Intendant sessions this means `context_injection`; for external
    /// backends it may be an immediate follow-up turn. Paired with a later
    /// `SteerDelivered { mid_turn: false }` when the follow-up is sent.
    SteerQueued {
        session_id: Option<String>,
        id: String,
        /// Short human-readable explanation of why the queue fallback was
        /// used (e.g. "native mid-turn steering failed; queued as follow-up").
        reason: String,
    },
    /// Steering was observed in the agent conversation. `mid_turn = true`
    /// means a native backend reported the steered user message in the active
    /// conversation; `mid_turn = false` means a queued item was drained at
    /// turn boundary and delivered as part of the next user message.
    SteerDelivered {
        session_id: Option<String>,
        id: String,
        mid_turn: bool,
    },
    /// User/admin requested that queued steer state be cleared. Agent loops
    /// consume this to remove matching in-memory queue entries and retire the
    /// pending dashboard row with `SteerCancelled`.
    SteerCancelRequested {
        session_id: Option<String>,
        id: Option<String>,
        reason: String,
    },
    /// A queued or pending steer was explicitly cancelled/cleared. This is a
    /// terminal UI state distinct from delivery: the prompt should no longer be
    /// shown as waiting, but Intendant is not claiming the agent saw it.
    SteerCancelled {
        session_id: Option<String>,
        id: String,
        reason: String,
    },
    /// A cancel arrived but nothing was left to cancel — the steer already
    /// delivered, drained into a follow-up, or was handed to the runtime.
    /// Distinct from `SteerCancelled` so the dashboard never reports a
    /// clear that did not happen (the old handler fabricated a
    /// `SteerCancelled` here and the text still reached the model).
    SteerCancelFailed {
        session_id: Option<String>,
        id: String,
        reason: String,
    },
    /// Ordinary follow-up lifecycle for targets that cannot be steered
    /// mid-turn. Frontends use this to show "queued for next turn" rather
    /// than making a subagent appear unresponsive while the parent loop is
    /// still draining the current child turn.
    FollowUpStatus {
        session_id: Option<String>,
        id: String,
        text: Option<String>,
        status: String,
        reason: Option<String>,
    },
    /// User-message edit lifecycle for external sessions. This is separate
    /// from ordinary follow-up status because edits target a specific active
    /// user turn and may first need to attach an external backend.
    UserMessageEditStatus {
        session_id: Option<String>,
        user_turn_index: u32,
        status: String,
        message: String,
    },
    /// User/admin requested that a queued ordinary follow-up be cleared before
    /// the target session consumes it for the next turn.
    FollowUpCancelRequested {
        session_id: Option<String>,
        id: Option<String>,
        reason: String,
    },
    /// Internal request to send a follow-up to a backend-native child thread
    /// without waiting for the parent session's next turn. The active external
    /// agent drain consumes this directly; browsers only see FollowUpStatus.
    ExternalFollowUpRequested {
        session_id: String,
        text: String,
        attachments: Vec<crate::external_agent::AgentAttachment>,
        follow_up_id: Option<String>,
    },
    SessionStarted {
        session_id: String,
        task: Option<String>,
    },
    /// Peer-delegation delivery receipt: the session supervisor accepted a
    /// `StartTask` that carried a `delegation_id` and dispatched it as
    /// `session_id`. Broadcast to every connected client as
    /// `OutboundEvent::TaskReceived`; the delegating daemon's federation
    /// transport correlates it back to the in-flight
    /// `PeerOp::DelegateTask` (see `peer::transport::intendant`).
    /// Re-emitted verbatim (same `session_id`) when a duplicate
    /// `delegation_id` is deduped instead of dispatched.
    TaskReceived {
        delegation_id: String,
        session_id: String,
    },
    /// Links an Intendant wrapper/log session to a backend-native
    /// session/thread id. Frontends use this to route backend-specific actions
    /// without confusing the wrapper UUID with the provider's own id.
    SessionIdentity {
        session_id: String,
        source: String,
        backend_session_id: String,
    },
    /// Links two visible sessions so frontends can draw parent/child
    /// relationship affordances. `relationship` is intentionally stringly
    /// typed for cross-backend reuse: known values include "side", "fork",
    /// and "subagent".
    SessionRelationship {
        parent_session_id: String,
        child_session_id: String,
        relationship: String,
        ephemeral: bool,
    },
    /// Describes which frontend actions are supported for a visible session.
    /// Synthetic child sessions can use this to expose follow-ups while
    /// disabling controls the underlying backend cannot honor for that target.
    SessionCapabilities {
        session_id: String,
        capabilities: SessionCapabilities,
    },
    SessionGoal {
        session_id: String,
        goal: Option<SessionGoal>,
    },
    /// Per-session vitals (git / prompt-cache / rate limits) from the
    /// daemon-side producers; frontends render chips from the latest value.
    SessionVitals {
        session_id: String,
        vitals: crate::types::SessionVitals,
    },
    SessionAttached {
        session_id: String,
        source: String,
    },
    SessionEnded {
        session_id: String,
        reason: String,
        /// Structured failure class the UI can act on without parsing
        /// prose (e.g. "unfueled" → an Add-API-keys action; "no_project"
        /// → the project picker). None for normal ends and unclassified
        /// errors.
        error_kind: Option<String>,
    },
    DebugScreenReady {
        display_id: u32,
    },
    DebugScreenTornDown {
        display_id: u32,
    },
    BudgetWarning {
        pct: f64,
        remaining: u64,
    },
    BudgetExhausted {
        remaining: u64,
    },
    SafetyCapReached,
    LoopError(String),

    // askHuman
    HumanQuestionDetected {
        question: String,
    },
    HumanResponseSent,

    // Autonomy / approval
    ApprovalRequired {
        session_id: Option<String>,
        id: u64,
        command_preview: String,
        category: ActionCategory,
    },
    /// The agent asks the human structured question(s). Shares the approval
    /// id space and registry, but is a request for *input*, not permission:
    /// autonomy policy never auto-resolves it. Resolved by
    /// `ControlMsg::AnswerQuestion` (or deny/skip to dismiss), reported back
    /// as `ApprovalResolved`.
    UserQuestionRequired {
        session_id: Option<String>,
        id: u64,
        questions: Vec<crate::types::UserQuestion>,
    },
    ApprovalResolved {
        session_id: Option<String>,
        id: u64,
        action: String,
    },
    /// Fire-and-forget agent→user notification (`notify_user`). Display
    /// only: rendered by dashboards (toast + transcript row), persisted to
    /// the session log for replay, never injected into any model context,
    /// and never blocking. `urgency` escalates delivery (attention center,
    /// Connect nudge) — see [`crate::types::NotificationUrgency`].
    UserNotification {
        session_id: Option<String>,
        id: String,
        title: Option<String>,
        text: String,
        urgency: crate::types::NotificationUrgency,
        ts: u64,
    },

    // Vision display ready
    DisplayReady {
        display_id: u32,
        width: u32,
        height: u32,
        /// Whether agents may see this display (mirrors
        /// [`crate::display::DisplaySession::agent_visible`]). `false`
        /// marks a private user view: consumers that surface displays to
        /// agents or peers (presence display list, peer upcasters,
        /// recording auto-start) must skip it; only authority-checked
        /// owner/root dashboards use it to render the "private view" chip.
        agent_visible: bool,
    },

    /// Resolution changed on a live display (e.g. monitor mode switch, scale
    /// factor change).  Emitted by the capture bridge when incoming frame
    /// dimensions diverge from the current encoder dimensions.
    DisplayResize {
        display_id: u32,
        width: u32,
        height: u32,
    },

    // Display takeover
    DisplayTaken {
        display_id: u32,
    },
    DisplayReleased {
        display_id: u32,
        note: Option<String>,
    },

    // Display capture lost (backend crashed or portal session ended)
    DisplayCaptureLost {
        display_id: u32,
        reason: String,
    },

    /// The OS screen-share approval dialog has been raised on the guest
    /// desktop and we're waiting for the user to approve. The dashboard
    /// surfaces this as a banner so a remote user (e.g. via the web UI)
    /// knows to look at the physical display, not just sit on a blank
    /// video tab.  Cleared by `DisplayReady` (success) or
    /// `DisplayCaptureLost` (timeout / denial).
    DisplayApprovalPending {
        display_id: u32,
        backend: &'static str,
    },
    /// One successfully executed computer-use action on a display —
    /// emitted by [`crate::computer_use::CuActionObserver`] for the
    /// dashboard's live action overlays and per-display feed. EPHEMERAL by
    /// design: broadcast-lane only, never session-logged, never replayed
    /// (the Activity log carries the durable CU trace).
    CuActionExecuted {
        /// Unique id (`cu-<ts>-<seq>`) — the browser's dual-lane dedupe key.
        event_id: String,
        session_id: Option<String>,
        display_id: u32,
        /// Action vocabulary: `left_click`, `type`, `screenshot`, … — see
        /// `computer_use::cu_action_kind`.
        kind: String,
        /// Action point in display pixel space, when the action has one.
        x: Option<i32>,
        y: Option<i32>,
        /// Resolution the coordinates are relative to (0 = unknown; viewers
        /// fall back to the stream's intrinsic dimensions).
        ref_w: u32,
        ref_h: u32,
        /// Short raw call string for the feed (`left_click(612, 233)`).
        raw: String,
        /// Unix milliseconds at execution.
        ts: u64,
    },
    /// Agent-requested visual collaboration state for the dashboard.
    ///
    /// This is intentionally presentation-level: it does not grant input
    /// authority or mutate display sessions. Browsers use it to foreground a
    /// display, render a focus box, and expose a user-clickable input button
    /// when an agent asks for cooperation.
    SharedView {
        session_id: Option<String>,
        action: String,
        display_target: Option<String>,
        display_id: Option<u32>,
        reason: Option<String>,
        region: Option<crate::types::SharedViewRegion>,
        note: Option<String>,
    },

    // User session display grant/revoke
    UserDisplayGranted {
        /// The display ID that was granted.  0 = primary (default).
        display_id: u32,
        /// `true` = the classic grant: the display is shared with agents
        /// for computer use (and the autonomy user-display grant is set
        /// by the emitter). `false` = a private user view ("View this
        /// machine"): the capture session streams to the owner's
        /// dashboards only and stays out of every agent-facing lookup.
        /// A `true` grant upgrades an existing private view in place.
        agent_visible: bool,
    },
    UserDisplayRevoked {
        /// The display ID being revoked.  0 = primary (default).
        display_id: u32,
        note: Option<String>,
    },

    /// A scoped agent rang the user-display doorbell
    /// (`request_user_display`): dashboards raise the dedicated popup.
    /// Deliberately NOT `ApprovalRequired` — display requests never share
    /// the approval id space, so `approve`/`approve_all`/autonomy rules
    /// cannot reach them; the only resolution is
    /// `ControlMsg::ResolveDisplayRequest` from an owner surface.
    DisplayRequestRaised {
        session_id: Option<String>,
        /// Request id in the display-request registry's own id space.
        id: u64,
        /// "view" | "view_and_control" (crate::display_requests vocabulary).
        access: String,
        /// Short agent-provided justification shown verbatim to the user.
        reason: String,
        /// When the requesting tool call stops waiting (unix ms) — the
        /// popup auto-expires then.
        expires_unix_ms: u64,
    },
    /// A display request left the pending set: the user decided
    /// ("approved" / "denied" / "denied_for_session"), the wait window
    /// elapsed ("timeout"), or the requesting session ended ("cancelled").
    /// Dashboards drop the popup and the attention chain clears the item.
    DisplayRequestResolved {
        session_id: Option<String>,
        id: u64,
        outcome: String,
        /// Granted access level, present on "approved".
        access: Option<String>,
        /// Granted duration ("this_session" | "15m" | "until_revoked"),
        /// present on "approved".
        duration: Option<String>,
    },

    /// Browser workspace lifecycle/lease update. Browser workspaces are
    /// addressable browser-control surfaces (CDP/Playwright/Agent Browser now,
    /// federated peers later) and are intentionally separate from global
    /// desktop display input.
    BrowserWorkspaceChanged {
        kind: String,
        #[allow(dead_code)]
        workspace: Option<crate::browser_workspace::BrowserWorkspace>,
        #[allow(dead_code)]
        workspace_id: Option<String>,
        #[allow(dead_code)]
        message: Option<String>,
    },

    // Recording lifecycle
    RecordingStarted {
        stream_name: String,
    },
    RecordingStopped {
        stream_name: String,
    },
    RecordingError {
        stream_name: String,
        message: String,
    },
    RecordingDeleted {
        stream_name: String,
    },

    // Session directory changed (MCP per-task isolation)
    SessionDirChanged {
        path: std::path::PathBuf,
    },

    // Control socket
    ControlCommand(ControlMsg),

    // Auto-approved command visibility
    AutoApproved {
        preview: String,
    },

    // Presence layer token usage update
    PresenceUsageUpdate {
        total_tokens: u64,
        context_window: u64,
        usage_pct: f64,
        provider: String,
        model: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: u64,
        cache_creation_tokens: u64,
    },

    // Live model (Gemini Live / OpenAI Realtime) usage update from browser
    LiveUsageUpdate {
        provider: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        total_tokens: u64,
        thinking_tokens: u64,
        input_text_tokens: u64,
        input_audio_tokens: u64,
        input_image_tokens: u64,
        cached_text_tokens: u64,
        cached_audio_tokens: u64,
        cached_image_tokens: u64,
        output_text_tokens: u64,
        output_audio_tokens: u64,
    },

    /// Presence layer log message (shown in TUI log panel).
    /// `level` controls visibility: None defaults to Info.
    PresenceLog {
        message: String,
        #[allow(dead_code)]
        level: Option<LogLevel>,
        /// Presence interaction turn (for log grouping/collapse).
        turn: Option<usize>,
    },

    // Round lifecycle
    RoundComplete {
        session_id: Option<String>,
        round: usize,
        turns_in_round: usize,
        /// Length of the native `Conversation.messages` at the end of this
        /// round. Populated only when emitted from the native agent loop;
        /// the external-agent paths emit `None` because their conversation
        /// state lives inside the backend process (Codex thread, CC/Gemini
        /// session) rather than in a `Conversation` struct.
        ///
        /// The file watcher stores this on `HistoryRound.native_message_count`
        /// so a conversation-rollback request can truncate back to that
        /// length when rolling back to this round.
        native_message_count: Option<u32>,
    },

    /// Presence layer responded — switch to follow-up mode without logging
    /// a fake round completion. Emitted by the response forwarder after each
    /// presence narration so the user can type a follow-up.
    PresenceReady,

    /// Browser-side presence connected — server-side presence should pause.
    PresenceConnected {
        server_session_id: Option<String>,
        last_event_seq: u64,
        /// Live model provider name (e.g. "gemini", "openai").
        live_provider: Option<String>,
        /// Live model name (e.g. "gemini-2.5-flash-native-audio-preview-12-2025").
        live_model: Option<String>,
    },
    /// Browser-side presence disconnected — server-side presence should resume.
    PresenceDisconnected,
    /// Voice transcript log from browser presence model.
    VoiceLog {
        text: String,
        seq: u64,
        tool_context: Option<String>,
    },
    /// Context checkpoint received from browser presence model.
    PresenceCheckpointReceived {
        summary: String,
        last_event_seq: u64,
    },
    /// Diagnostic from browser voice/presence layer (errors, silence, disconnects).
    VoiceDiagnostic {
        kind: String,
        detail: String,
    },

    // Live audio sub-agent lifecycle
    LiveAudioStarted {
        id: String,
        provider: String,
    },
    LiveAudioProgress {
        id: String,
        state: String,
        elapsed_secs: f64,
        transcript_preview: String,
    },
    LiveAudioCompleted {
        id: String,
        status: String,
        quarantine_count: usize,
    },

    /// Server-side transcription of user speech (from Whisper API).
    UserTranscript {
        text: String,
        seq: u64,
    },

    /// Computed usage snapshot (emitted by TUI after accumulating tokens).
    UsageSnapshot {
        session_id: Option<String>,
        main: crate::frontend::ModelUsageSnapshot,
        presence: Option<crate::frontend::ModelUsageSnapshot>,
    },
    /// Parsed, otherwise raw model-context snapshot for dashboard inspection.
    ContextSnapshot {
        session_id: Option<String>,
        source: String,
        label: String,
        request_id: Option<String>,
        request_index: Option<u64>,
        turn: Option<usize>,
        format: String,
        token_count: Option<u64>,
        token_count_kind: Option<String>,
        context_window: Option<u64>,
        hard_context_window: Option<u64>,
        item_count: Option<usize>,
        /// The whole serialized provider request context — multi-MB for a
        /// long session. `Arc`'d so the broadcast ring slot and every
        /// subscriber's `AppEvent::clone` share one allocation instead of
        /// deep-copying the payload per subscriber.
        raw: Arc<serde_json::Value>,
    },

    /// Proactive status broadcast (emitted on turn start, phase change, etc.)
    StatusUpdate {
        turn: usize,
        phase: String,
        autonomy: String,
        session_id: String,
        task: String,
    },

    /// Emitted when the external agent backend changes (via Settings dropdown).
    /// Broadcasted by the ControlPlane after updating shared state.
    ExternalAgentChanged {
        agent: Option<String>,
    },

    /// Emitted when the daemon-level autonomy switch changes.
    AutonomyChanged {
        autonomy: String,
    },

    /// Emitted by the control plane when a `ControlMsg::CodexThreadAction`
    /// arrives, so the daemon-side action watcher (which owns the
    /// persistent agent) can pick it up. Carries the dispatched action and
    /// its params — one-way, no ack on this variant. The watcher emits a
    /// `CodexThreadActionResult` after the agent call returns.
    CodexThreadActionRequested {
        request_id: String,
        session_id: Option<String>,
        action: String,
        params: serde_json::Value,
        origin: Option<String>,
    },

    /// Emitted by the daemon-side action watcher after a CodexThreadAction
    /// has been executed (or failed). `success=true` → message is an
    /// informational status line; `success=false` → message is the error
    /// surfaced to the caller. Dashboard consumes to flash a toast.
    ///
    /// `record_id` carries the durable context-rewind record id for a
    /// successful `rewind_context` (and is `None` for failures and every
    /// other action), so consumers like the MCP pressure tracker don't have
    /// to parse it back out of the human-readable `message`.
    CodexThreadActionResult {
        session_id: Option<String>,
        action: String,
        success: bool,
        message: String,
        record_id: Option<String>,
    },

    /// Emitted after a generic session rename is persisted or rejected.
    SessionRenameResult {
        session_id: String,
        source: Option<String>,
        name: Option<String>,
        success: bool,
        message: String,
    },

    /// Emitted after a per-session external-agent launch config save is
    /// persisted or rejected. The dashboard waits for this before closing the
    /// launch-config modal so a failed/partial write cannot look successful.
    SessionAgentConfigResult {
        session_id: String,
        source: String,
        backend_session_id: Option<String>,
        intendant_session_id: Option<String>,
        persisted_session_ids: Vec<String>,
        success: bool,
        message: String,
    },

    /// Emitted when one or more fields of the Codex runtime configuration
    /// change. Fields not included in the event are unchanged from the
    /// previous state. Broadcast by the control plane; consumed by the
    /// dashboard's Control sub-tab to refresh its displayed values.
    ///
    /// The `_cleared` booleans distinguish "unchanged" (field None, bool
    /// false) from "explicitly set to empty / use Codex default" (field
    /// None, bool true). `Option<Option<_>>` would be cleaner in Rust
    /// but doesn't round-trip through our JSON as obviously.
    CodexConfigChanged {
        command: Option<String>,
        managed_command: Option<String>,
        managed_command_cleared: bool,
        sandbox: Option<String>,
        approval_policy: Option<String>,
        model: Option<String>,
        model_cleared: bool,
        reasoning_effort: Option<String>,
        reasoning_effort_cleared: bool,
        service_tier: Option<String>,
        service_tier_cleared: bool,
        web_search: Option<bool>,
        network_access: Option<bool>,
        writable_roots: Option<Vec<String>>,
        managed_context: Option<String>,
        context_archive: Option<String>,
    },

    /// Emitted when one or more Claude Code runtime fields change. Mirror of
    /// `GeminiConfigChanged` — fields omitted were not touched,
    /// `model_cleared` distinguishes "no change" from "override removed".
    ClaudeConfigChanged {
        model: Option<String>,
        model_cleared: bool,
        permission_mode: Option<String>,
        allowed_tools: Option<Vec<String>>,
    },

    /// Log entry broadcast to external consumers (web UI, control socket).
    /// Emitted by the TUI's `log_sourced` for events without their own
    /// `OutboundEvent` variant, and by backend code (e.g.
    /// `emit_task_dispatched_log`) so dispatch-style messages reach external
    /// consumers in headless mode where no TUI is running.
    LogEntry {
        session_id: Option<String>,
        level: String,
        source: String,
        content: String,
        turn: Option<usize>,
    },
    /// Display-only session note posted by an agent (`post_session_note`
    /// MCP tool / `intendant ctl session note`). Presentation rail only:
    /// broadcast to frontends and persisted to the session log, never
    /// injected into any model conversation (that path is
    /// [`ContextInjection`]). Attachments reference blobs already
    /// committed to the session upload store.
    SessionNote {
        session_id: Option<String>,
        note_id: String,
        text: String,
        attachments: Vec<crate::types::SessionNoteAttachment>,
        source: Option<String>,
        /// Unix epoch milliseconds when the note was posted.
        ts: u64,
    },
    /// Active user-message edit rewound already-rendered session context.
    UserMessageRewind {
        session_id: Option<String>,
        user_turn_index: u32,
        turns_removed: u32,
    },

    /// Editable user-message log entry for managed external sessions.
    ///
    /// This keeps the ordinary log surface generic while carrying the session
    /// and user-turn metadata the dashboard needs to request a Codex-style
    /// rewind and replacement.
    UserMessageLog {
        session_id: Option<String>,
        content: String,
        user_turn_index: Option<u32>,
        user_turn_revision: Option<u32>,
        replacement_for_user_turn_index: Option<u32>,
    },

    /// Display transport pipeline metrics snapshot.
    DisplayMetrics {
        snapshot: crate::display::DisplayMetricsSnapshot,
    },

    /// A file in the project directory was created, modified, or deleted.
    FileChanged {
        path: String,
        kind: crate::file_watcher::FileChangeKind,
        lines_added: u32,
        lines_removed: u32,
    },

    /// A user-uploaded file was committed to the upload store. Emitted by
    /// the `POST /api/upload` handler after the bytes land on disk. The
    /// dashboard keeps a list of these to let the user attach one or more
    /// to the next task (as `"upload:<id>"` strings in
    /// `ControlMsg::StartTask.attachments`).
    UploadReady {
        descriptor: crate::upload_store::UploadDescriptor,
    },
    /// A previously-uploaded file was deleted from the store. Mirror of
    /// `UploadReady`.
    UploadDeleted {
        /// Descriptor id (stable across browsers).
        id: String,
    },

    // ---- File snapshot history (per-round) ----
    /// A new per-round snapshot was recorded.
    SnapshotCreated {
        round_id: u64,
    },
    /// The project tree was rolled back to a prior round.
    RolledBack {
        from_id: u64,
        to_id: u64,
        files_reverted: u32,
    },
    /// `current_head_id` advanced forward along the linear history.
    Redone {
        to_id: u64,
    },
    /// Abandoned branches were pruned and orphaned blobs GC'd.
    HistoryPruned {
        branches_removed: u32,
        bytes_freed: u64,
    },

    // ---- Conversation rollback ----
    /// Emitted by the web gateway's rollback handler when the user
    /// requested `revert_conversation: true`. The active agent loop
    /// (native via `run_agent_loop` / `run_with_presence`, or the
    /// external-agent drain) subscribes to the bus and handles this by
    /// truncating / session-resetting its conversation state, then
    /// emits `ConversationRolledBack` with the result.
    ///
    /// The handler looks up the round's stored `native_message_count`
    /// (from `HistoryRound`) and `turn_count`; both are passed through
    /// here so the consumer doesn't have to look them up again.
    ConversationRollbackRequested {
        /// Target session. `None` preserves the legacy shape: the
        /// daemon's current/boot session (the headless outer loop).
        /// `Some(id)` targets a supervised session's parked drain, which
        /// resolves the round from its own ledger — delivery is
        /// drain-time (a mid-round session is not listening; the
        /// dashboard observes `ConversationRolledBack` for completion,
        /// as it already does on the legacy path).
        session_id: Option<String>,
        /// The round we are rolling back to. Echoed through to the
        /// completion event for UI correlation.
        round_id: u64,
        /// For the native agent: truncate `Conversation.messages` to
        /// this length. `None` for rollbacks targeting external-agent
        /// rounds (those fall back to backend rollback or session reset).
        target_native_message_count: Option<u32>,
        /// Number of turns that occurred between the current head and
        /// the target round. Passed through to external-agent backends
        /// that accept a `numTurns` parameter (Codex).
        turns_to_drop: u32,
    },

    /// Conversation was rolled back to a specific round.
    ConversationRolledBack {
        /// The session that rolled back; `None` = the legacy
        /// current-session shape.
        session_id: Option<String>,
        round_id: u64,
        /// Number of messages/turns removed from the conversation.
        /// Semantics are backend-dependent:
        ///   - native/Codex: turns dropped from the conversation tail.
        ///   - session-reset backends: best-effort estimate (the count
        ///     passed in `turns_to_drop`).
        turns_removed: u32,
        /// Which backend performed the rollback: `"native"`, `"codex"`,
        /// `"claude-code"`, or `"gemini"`.
        backend: String,
        /// How the rollback was performed: `"truncated"` for the native
        /// agent and Codex's `thread/rollback`, or `"session-reset"` for
        /// backends (CC, Gemini) that don't expose a protocol-level
        /// rollback and re-initialize the session from scratch.
        method: String,
    },

    /// The agenda ledger changed (item added, patched, or transitioned).
    /// Carries the item as it now stands plus fresh counts so frontends
    /// update live without refetching.
    AgendaChanged {
        item: crate::agenda::AgendaItem,
        counts: crate::agenda::AgendaCounts,
    },

    /// 1 Hz heartbeat from [`spawn_tick_timer`]. Only the stdio MCP event
    /// listener consumes it (stuck-phase detection), and only MCP mode
    /// spawns the timer.
    Tick,
}

fn context_snapshot_raw_is_compact(raw: &serde_json::Value) -> bool {
    raw.pointer("/_intendant_context/archive_mode")
        .and_then(|v| v.as_str())
        == Some("summary")
        || raw.pointer("/summary/kind").and_then(|v| v.as_str()) == Some("compact_context_snapshot")
        || raw.get("summary_parts").is_some()
}

fn mark_context_snapshot_exact_available(raw: &mut serde_json::Value) {
    if let Some(context) = raw
        .get_mut("_intendant_context")
        .and_then(|value| value.as_object_mut())
    {
        context.insert("raw_archived".to_string(), serde_json::Value::Bool(true));
        context.insert("raw_omitted".to_string(), serde_json::Value::Bool(true));
        context.insert(
            "exact_replay_available".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    if let Some(summary) = raw
        .get_mut("summary")
        .and_then(|value| value.as_object_mut())
    {
        summary.insert("raw_omitted".to_string(), serde_json::Value::Bool(true));
        summary.insert(
            "exact_replay_available".to_string(),
            serde_json::Value::Bool(true),
        );
    }
}

fn compact_context_snapshot_raw_for_outbound(
    raw: &serde_json::Value,
    request_id: Option<&str>,
    request_index: Option<u64>,
    format: &str,
) -> serde_json::Value {
    if context_snapshot_raw_is_compact(raw) {
        return raw.clone();
    }
    // Serialize exactly once: the same bytes decide the inline limit AND
    // feed the summary's len/hash fields. This used to serialize the
    // multi-MB tree once just for `.len()`, deep-clone it for a by-value
    // summary call that only borrowed it, and re-serialize it a second
    // time inside for the same len/hash.
    let raw_bytes = match serde_json::to_vec(raw) {
        Ok(bytes) => bytes,
        Err(_) => raw.to_string().into_bytes(),
    };
    if raw_bytes.len() <= OUTBOUND_CONTEXT_SNAPSHOT_RAW_INLINE_LIMIT {
        return raw.clone();
    }

    let request_id = request_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("live");
    let request_index = request_index.unwrap_or(0);
    let mut compact = crate::external_agent::codex::codex_context_archive_summary(
        raw,
        &raw_bytes,
        request_id,
        request_index,
        format,
    );
    mark_context_snapshot_exact_available(&mut compact);
    compact
}

/// Response from the approval system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResponse {
    Approve,
    Skip,
    Deny,
    ApproveAll,
    /// Structured reply to a `UserQuestionRequired` prompt: question text →
    /// chosen option label(s) (multi-select joined with ", ") or free text.
    /// Only meaningful for question ids; a command approval receiving this
    /// fails closed (denied).
    Answer {
        answers: std::collections::HashMap<String, String>,
    },
}

/// Commands received from the Unix control socket, MCP, and web gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ControlMsg {
    Status {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    Usage,
    Approve {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: u64,
    },
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: u64,
    },
    Skip {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: u64,
    },
    ApproveAll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: u64,
    },
    /// Answer a pending `user_question` prompt: question text → chosen
    /// option label(s) (multi-select joined with ", ") or free text.
    /// Routes through the approval registry under the same `id`.
    AnswerQuestion {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: u64,
        #[serde(default)]
        answers: std::collections::HashMap<String, String>,
    },
    Input {
        text: String,
    },
    SetAutonomy {
        level: String,
    },
    /// Set a per-category approval rule. Applies LIVE to the shared autonomy
    /// state and is persisted to `intendant.toml [approval]`. `category` is an
    /// `ApprovalConfig` field name (`file_read`, `file_write`, `file_delete`,
    /// `command_exec`, `network`, `destructive`, `display_control`,
    /// `tool_call`); `rule` is `auto`, `ask`, or `deny`.
    SetApprovalRule {
        category: String,
        rule: String,
    },
    SetExternalAgent {
        #[serde(default)]
        agent: Option<String>,
    },
    /// Set the Codex executable path or command name. `None`, missing, or
    /// an empty string falls back to `codex` on PATH. Applies to the NEXT
    /// task because changing this requires respawning the Codex process.
    SetCodexCommand {
        #[serde(default)]
        command: Option<String>,
    },
    /// Set the managed-capable Codex executable (the Intendant-aware
    /// fork). Managed-context sessions spawn this instead of the vanilla
    /// `command`; `None`/empty clears it (managed sessions then fall back
    /// to `command`). Applies to the NEXT task.
    SetCodexManagedCommand {
        #[serde(default)]
        command: Option<String>,
    },
    /// Set the Codex sandbox mode. Applies to the NEXT task because Codex
    /// locks the sandbox at `thread/start`. Valid values match
    /// `codex --sandbox <MODE>`: `"read-only"`, `"workspace-write"`,
    /// `"danger-full-access"`.
    SetCodexSandbox {
        mode: String,
    },
    /// Set the Codex approval policy. Applies to the NEXT task. Valid
    /// values match `codex --ask-for-approval <POLICY>`: `"untrusted"`,
    /// `"on-request"`, `"never"`.
    SetCodexApprovalPolicy {
        policy: String,
    },
    /// Set the Codex model override. `None` (or a JSON null / missing) lets
    /// Codex pick its default; otherwise the string is passed as `-m <MODEL>`
    /// equivalent via `thread/start` params. Applies to the NEXT task.
    SetCodexModel {
        #[serde(default)]
        model: Option<String>,
    },
    /// Set Codex reasoning effort. `None` or missing = use the model's
    /// default. Values match `-c model_reasoning_effort=...`. Applies to
    /// the NEXT task.
    SetCodexReasoningEffort {
        #[serde(default)]
        effort: Option<String>,
    },
    /// Set the Codex service-tier default for Intendant-managed Codex
    /// sessions. `None` / missing inherits Codex's own config. `"priority"`
    /// forces Fast, `"flex"` requests Flex, and `"standard"` sends an
    /// explicit `serviceTier: null` at thread start to force normal.
    SetCodexServiceTier {
        #[serde(default)]
        service_tier: Option<String>,
    },
    /// Toggle the Responses API `web_search` tool for Codex.
    /// Maps to `codex --search`. Applies to the NEXT task.
    SetCodexWebSearch {
        enabled: bool,
    },
    /// Toggle outbound network access inside the `workspace-write` sandbox.
    /// Maps to `-c sandbox_workspace_write.network_access=true|false`.
    /// Ignored by `read-only` and `danger-full-access`. Applies to the
    /// NEXT task.
    SetCodexNetworkAccess {
        enabled: bool,
    },
    /// Replace the list of extra writable roots (paths beyond the project
    /// root that Codex's sandbox may also write to). Maps to `--add-dir`.
    /// Applies to the NEXT task.
    SetCodexWritableRoots {
        #[serde(default)]
        roots: Vec<String>,
    },
    /// Set Codex's managed-context mode. `vanilla` is upstream/original-fork
    /// safe; `managed` enables Intendant's proactive rewind/fission tooling
    /// and disables Codex auto-compaction for the managed thread. Applies to
    /// the NEXT task.
    #[serde(alias = "set_codex_context_recovery")]
    SetCodexManagedContext {
        mode: String,
    },
    /// Set Codex context snapshot archive mode. `summary` keeps compact
    /// per-request visualization data, `exact` persists full provider
    /// payloads for raw replay, and `off` disables context capture. Applies
    /// to the NEXT task.
    SetCodexContextArchive {
        mode: String,
    },
    /// Invoke one of Codex's thread-level actions against the persistent
    /// agent. Mirrors the raw-codex slash-command surface: `/new`, `/compact`,
    /// `/fast`, `/fork`, `/side`, `/undo`, `/review`, `/rename`, `/goal`, `/init`,
    /// `/memory-reset`. Applies immediately (not "next task") because Codex's
    /// app-server accepts these as mid-session RPCs.
    ///
    /// `params` is a free-form JSON object whose shape depends on `op`:
    /// `/fork` accepts `{"name": "..."}`, `/side` accepts `{"prompt": "..."}`,
    /// `/side-close` accepts `{"threadId": "...", "parentThreadId": "..."}`,
    /// `/undo` accepts `{"turns": N}`, `/review` accepts `{"prompt": "..."}`, `/rename` accepts
    /// `{"name": "..."}`, `/goal` accepts `{"objective": "...",
    /// "tokenBudget": N, "status": "active|paused|budgetLimited|complete"}`,
    /// and the rest ignore params. Callers that don't need params may omit the
    /// field entirely.
    ///
    /// The variant's field is named `op` (not `action`) because ControlMsg's
    /// serde tag is already `action`, and nested fields can't share the tag.
    ///
    /// Despite the legacy Codex-flavored name, this is the backend-neutral
    /// per-session thread-action channel: the wire accepts the universal
    /// `thread_action` tag (canonical for new frontends) as well as the
    /// legacy `codex_thread_action`, and the owning drain dispatches through
    /// `ExternalAgent::thread_action` / `ForkHandling` for whatever backend
    /// runs the session. Claude Code advertises and accepts `compact` and
    /// `fork`. Frontends should gate ops on
    /// `SessionCapabilities.thread_actions` (with `codex_thread_actions` as
    /// the legacy fallback), never on backend identity.
    #[serde(alias = "thread_action")]
    CodexThreadAction {
        /// Target Codex thread/session. Kept optional on the wire for
        /// compatibility, but the control plane rejects missing values so an
        /// action cannot fan out to every live Codex loop.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        op: String,
        #[serde(default)]
        params: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
    /// Rename a session through Intendant's generic session-name abstraction.
    /// Backends with native rename support may map this to their own protocol;
    /// otherwise Intendant persists a local overlay keyed by source/session id.
    RenameSession {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        name: String,
    },
    /// Persist per-session external-agent launch settings. These override the
    /// global Settings pane when a historical session is resumed or reattached.
    ConfigureSessionAgent {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        intendant_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_sandbox: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_approval_policy: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_managed_context: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_context_archive: Option<String>,
        /// Claude Code launch pins. "inherit"/"global" (or empty) clears a
        /// pin; the tools list is comma-separated with "all"/"*" pinning the
        /// explicitly-unrestricted empty list. For the permission mode,
        /// "default" is a real mode and pins (only inherit/global clear).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_permission_mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_allowed_tools: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_effort: Option<String>,
    },
    /// Stop a live managed session. Unlike hiding a dashboard card, this
    /// removes the live session from daemon state and asks the backend process
    /// to shut down.
    StopSession {
        session_id: String,
    },
    /// Stop a live external-agent session and immediately resume it using the
    /// persisted launch config for that session.
    RestartSession {
        source: String,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_root: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direct: Option<bool>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_sandbox: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_approval_policy: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_managed_context: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_context_archive: Option<String>,
        /// Claude Code one-shot launch pins for the restarted process (same
        /// sentinel semantics as `ConfigureSessionAgent`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_permission_mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_allowed_tools: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_effort: Option<String>,
    },
    /// Set the Claude Code model override (`--model`). `None`/missing lets
    /// the claude CLI pick its configured default. Applies to the NEXT task
    /// because the model is latched at process spawn.
    SetClaudeModel {
        #[serde(default)]
        model: Option<String>,
    },
    /// Set the Claude Code permission mode (`--permission-mode`):
    /// `"default" | "acceptEdits" | "plan" | "auto" | "dontAsk" |
    /// "bypassPermissions"` (`"manual"` and empty normalize to `"default"`;
    /// see `project::normalize_claude_permission_mode`). Applies to the
    /// NEXT task.
    SetClaudePermissionMode {
        mode: String,
    },
    /// Replace the Claude Code allowed-tools list (`--allowedTools`).
    /// Empty list = all tools available, gated by the permission flow.
    /// Applies to the NEXT task.
    SetClaudeAllowedTools {
        #[serde(default)]
        tools: Vec<String>,
    },
    SetVerbosity {
        level: String,
    },
    ScheduleControllerRestart {
        controller_id: String,
        north_star_goal: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        restart_after: Option<String>,
        #[serde(default)]
        restart_command: Option<String>,
        #[serde(default)]
        auto_start_task: Option<bool>,
        #[serde(default)]
        max_attempts: Option<u32>,
        #[serde(default)]
        cooldown_sec: Option<u64>,
    },
    ControllerTurnComplete {
        restart_id: String,
        turn_complete_token: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        handoff_summary: Option<String>,
    },
    GetRestartStatus,
    CancelControllerRestart {
        #[serde(default)]
        restart_id: Option<String>,
    },
    RequestControllerLoopHalt {
        #[serde(default)]
        persistent: Option<bool>,
    },
    ClearControllerLoopHalt,
    InterveneControllerLoop {
        mode: String,
    },
    GetControllerLoopStatus,
    /// Explicitly create a new managed session and submit its first task.
    ///
    /// This is the forward-compatible dashboard/control-plane primitive for
    /// parallel local or external-agent sessions. `StartTask { session_id:
    /// None }` remains accepted for older clients, but new clients should use
    /// this variant when they intend to create a distinct session rather than
    /// continue whichever session a legacy frontend considers active.
    CreateSession {
        task: String,
        /// Optional display name for the session. The session id remains the
        /// stable identity; this is only persisted metadata used by
        /// dashboards/session listings.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Directory to use as the new session's project root. When omitted,
        /// the session supervisor uses the project root of this Intendant
        /// instance.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_root: Option<String>,
        /// Optional one-shot agent override for this session. Omitted means
        /// use the configured default. Accepted values are "internal",
        /// "codex", "claude-code", or "gemini".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        /// Optional one-shot executable path or command name for the selected
        /// external agent. Empty/missing falls back to the configured command.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_command: Option<String>,
        /// Optional one-shot Claude Code model override for this session.
        /// Only applies when the resolved agent is Claude Code.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_model: Option<String>,
        /// Optional one-shot Claude Code permission mode for this session.
        /// Only applies when the resolved agent is Claude Code.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_permission_mode: Option<String>,
        /// Optional one-shot Claude Code reasoning-effort level
        /// (low/medium/high/xhigh/max/ultracode) for this session. Only
        /// applies when the resolved agent is Claude Code.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_effort: Option<String>,
        /// Optional one-shot Codex model override for this session (e.g.
        /// "gpt-5.3-codex"). Launch-time only — Codex sessions cannot switch
        /// models mid-session, so unlike `claude_model` there is no matching
        /// per-session update path. Only applies when the resolved agent is
        /// Codex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_model: Option<String>,
        /// Optional one-shot Codex reasoning-effort override for this
        /// session. Accepted values match `model_reasoning_effort`; omitted
        /// inherits the global/Codex model default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_reasoning_effort: Option<String>,
        /// Optional one-shot Codex sandbox mode for this session. Only applies
        /// when the resolved agent is Codex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_sandbox: Option<String>,
        /// Optional one-shot Codex approval policy for this session. Only
        /// applies when the resolved agent is Codex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_approval_policy: Option<String>,
        /// Optional one-shot Codex managed-context mode for this session.
        /// Accepted values normalize to "vanilla" or "managed". Only applies
        /// when the resolved agent is Codex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_managed_context: Option<String>,
        /// Optional one-shot Codex context replay/archive mode for this
        /// session. Accepted values normalize to "summary", "exact", or
        /// "off". Only applies when the resolved agent is Codex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_context_archive: Option<String>,
        /// Optional one-shot Codex service tier for this session. "priority"
        /// enables Codex Fast; "standard" explicitly clears Fast and forces
        /// normal. Only applies when the resolved agent is Codex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_service_tier: Option<String>,
        #[serde(default)]
        orchestrate: Option<bool>,
        /// Bypass presence/orchestration, matching StartTask.direct.
        #[serde(default)]
        direct: Option<bool>,
        /// When present, routes to the ephemeral CU task runner instead of the
        /// regular agent loop.
        #[serde(default)]
        reference_frame_ids: Vec<String>,
        /// Explicit display target for CU actions (e.g. "user_session", "display_99").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_target: Option<String>,
        /// Frame/upload IDs attached via the dashboard.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
        /// Launch the session in a fresh git worktree branched off the
        /// resolved project root's HEAD; the worktree checkout becomes the
        /// session's effective project root. Requires the project root to
        /// be a git repository with at least one commit.
        #[serde(default)]
        worktree: Option<bool>,
        /// Branch name for the worktree (validated against a conservative
        /// git-ref subset). Omitted: derived from the session name, else
        /// `session-<short-id>`; collisions get a numeric suffix.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree_branch: Option<String>,
    },
    /// Delegate a task to a new supervised sub-agent under an existing
    /// internal session (the dashboard "delegate" action). The child is
    /// tracked in the same registry the parent's wait_sub_agents tool
    /// reads, and the parent is woken with a notification follow-up so the
    /// model knows the delegation happened. External-agent parents are
    /// refused — they spawn managed sessions through their own start_task
    /// tool.
    SpawnSubAgent {
        /// Parent session the child runs under.
        session_id: String,
        task: String,
        /// Optional display name for the child session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Role guidance prompt for the child (research / implementation /
        /// testing / …); unknown values fall back to the base prompt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        /// Child backend: "internal" (default), "codex", or "claude-code".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        /// Isolate the child in a fresh git worktree off the parent
        /// project's HEAD.
        #[serde(default)]
        worktree: Option<bool>,
    },
    StartTask {
        /// Optional target session. When omitted, daemon supervisors start a
        /// new managed session; when present, they route the text as a new
        /// turn in that session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        task: String,
        #[serde(default)]
        orchestrate: Option<bool>,
        /// When true, run in direct mode (no presence layer). Use for
        /// programmatic clients that submit tasks via WebSocket/control
        /// socket without a human on the other end.
        #[serde(default)]
        direct: Option<bool>,
        /// When present, routes to the ephemeral CU task runner instead of the
        /// regular agent loop.
        #[serde(default)]
        reference_frame_ids: Vec<String>,
        /// Explicit display target for CU actions (e.g. "user_session", "display_99").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_target: Option<String>,
        /// Frame IDs that the user attached to this task via the dashboard's
        /// "Attach" buttons (annotation toolbar, Video tab, clip toolbar).
        ///
        /// Resolved against the frame registry and prepended as image content
        /// to the first user message of the agent conversation. Works for both
        /// the internal agent (`add_user_with_images`) and external agents
        /// (Codex `LocalImage`, Gemini ACP `ContentBlock::Image`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
        /// Optional client-generated id for a targeted follow-up. Frontends
        /// use it to correlate queued/delivered/failed status updates.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        follow_up_id: Option<String>,
        /// Peer-delegation delivery-receipt correlation id. Set by a
        /// federating daemon's `PeerOp::DelegateTask` (see
        /// `peer::transport::intendant`): when a daemon supervisor
        /// dispatches a new-session StartTask carrying this id, it
        /// acknowledges acceptance by emitting
        /// `OutboundEvent::TaskReceived { delegation_id, session_id }`
        /// on its broadcast, and it dedups repeats of the same id
        /// (an already-accepted id re-acks with the original session
        /// identity instead of starting a duplicate task — the sender
        /// re-sends the same id after a reconnect). Absent on frames
        /// from browsers, ctl, and pre-receipt peer builds; receivers
        /// older than this field ignore it (serde default) and never
        /// ack. Carries no authority — IAM evaluation on the receiving
        /// gateway is unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delegation_id: Option<String>,
    },
    ResumeSession {
        /// Session source: "intendant", "codex", "claude-code", or "gemini".
        source: String,
        /// Display id from the Sessions tab. For Intendant this is the
        /// session log id; for external backends it is the native session id.
        session_id: String,
        /// Backend-specific resume token. Defaults to `session_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_id: Option<String>,
        /// Directory to use when launching the resumed session. External CLIs
        /// resolve session history relative to their project roots.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_root: Option<String>,
        /// Optional prompt to send after the session is attached. When omitted,
        /// the session is only opened/attached and no agent turn is started.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        /// Bypass presence/orchestration, matching StartTask.direct.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direct: Option<bool>,
        /// Frame/upload IDs attached to the first turn sent after resume.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
        /// Fork the resumed backend thread into a NEW session instead of
        /// attaching to it: the supervisor always creates a fresh wrapper
        /// session (attach dedupe is skipped) and the backend is spawned
        /// with its fork flag (Claude Code `--fork-session`), so the parent
        /// thread is left untouched and the child announces its own native
        /// id on the first turn.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        fork: bool,
        /// Lineage kind recorded for a `fork` resume and emitted as the
        /// parent→child relationship once the child announces its native id.
        /// `None` means a plain `fork`; `side` marks an ephemeral side
        /// conversation (`/btw`) on backends whose side conversations are
        /// respawned forks rather than in-process threads.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relationship_kind: Option<String>,
        /// Marks a frontend-initiated automatic escalation: a follow-up that
        /// failed "not managed by this daemon" being retried by the
        /// dashboard as a resume-with-task. The supervisor cancels an
        /// auto-attach resume when the user interrupted/stopped the target
        /// session after the prompt was sent — the resume would relaunch
        /// the very work the user tried to halt (observed live 2026-07-15).
        /// Deliberate resumes leave this false/absent and are never subject
        /// to that gate.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        auto_attach: bool,
        /// Per-session executable override. When omitted, the supervisor
        /// rehydrates the persisted session value before falling back to the
        /// global Settings value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_command: Option<String>,
        /// Per-session Codex sandbox override. Only applies when `source`
        /// resolves to Codex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_sandbox: Option<String>,
        /// Per-session Codex approval-policy override. Only applies when
        /// `source` resolves to Codex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_approval_policy: Option<String>,
        /// Per-session Codex managed-context override. Only applies when
        /// `source` resolves to Codex.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_managed_context: Option<String>,
        /// Per-session Codex context replay/archive override. Only applies
        /// when `source` resolves to Codex.
        /// (Claude Code carries no resume-time one-shots: a resumed claude
        /// session rehydrates its pins from the persisted launch overlay —
        /// use `ConfigureSessionAgent`/`RestartSession` to change them.)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_context_archive: Option<String>,
    },
    FollowUp {
        /// Optional target session. Omitted means "current active session"
        /// for legacy single-session frontends.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        text: String,
        /// When true, bypass the presence layer for this follow-up and
        /// dispatch it directly to the agent — mirrors `direct: true`
        /// on `StartTask`. Frontends set this when the user has the
        /// Direct toggle checked at the time of the follow-up. Absent
        /// (or `Some(false)`) means route through presence as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direct: Option<bool>,
        /// Optional client-generated id for queued-follow-up status updates.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        follow_up_id: Option<String>,
    },
    /// Clear one queued ordinary follow-up by correlation id. This cannot
    /// unsend text already delivered to a backend runtime.
    CancelFollowUp {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Replace a previous user message by rewinding the target session to the
    /// selected user turn and submitting replacement text.
    ///
    /// Only backends that expose precise conversation rollback support this.
    EditUserMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Optional source/resume context for a replayed external-agent
        /// session that may not be attached to this daemon yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_root: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direct: Option<bool>,
        user_turn_index: u32,
        /// Revision of the active user turn the frontend rendered. This lets
        /// the backend reject stale edit requests after a message has already
        /// been overwritten and replaced.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_turn_revision: Option<u32>,
        /// Original displayed text for the clicked user message. Managed
        /// context uses this to avoid branching from the wrong archived turn
        /// when an old turn number has been overwritten by later rewinds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        original_text: Option<String>,
        text: String,
        /// Frame/upload IDs attached via the dashboard. These are resolved
        /// just like StartTask.attachments before the replacement turn is sent.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    TakeDisplay {
        display_id: u32,
    },
    ReleaseDisplay {
        display_id: u32,
        #[serde(default)]
        note: Option<String>,
    },
    GrantUserDisplay {
        /// Optional display ID to grant.  When `None`, grants the primary
        /// display (id 0) -- backwards-compatible with single-monitor setups.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_id: Option<u32>,
        /// Whether the created display session is visible to agents.
        /// `None` (absent on the wire) means `true` — the pre-split
        /// message always meant "share with the agent for computer
        /// use", and old frontends keep that meaning. `Some(false)` is
        /// the dashboard's "View this machine": a private remote view
        /// streamed to authority-checked owner/root dashboards only,
        /// invisible to scoped dashboards and agent display
        /// enumeration/screenshot/CU paths, and never touching the autonomy
        /// user-display grant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_visible: Option<bool>,
    },
    RevokeUserDisplay {
        /// Optional display ID to revoke.  When `None`, revokes the primary
        /// display (id 0) -- backwards-compatible with single-monitor setups.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_id: Option<u32>,
        #[serde(default)]
        note: Option<String>,
    },
    /// Resolve a pending user-display request (the doorbell popup's Allow
    /// / Deny / Deny-for-this-session buttons). Owner-surface intent only:
    /// classified `PeerOperation::DisplayInput` like `GrantUserDisplay` —
    /// resolving a request is exactly as powerful as granting directly.
    /// On approve, the control plane mints the grant through the same
    /// state flip + events `grant_user_display` performs; deny arms the
    /// per-session cooldown; deny_session suppresses the session. NEVER
    /// reachable through the approval registry: `approve`/`approve_all`
    /// target command approvals and cannot touch display requests.
    ResolveDisplayRequest {
        /// The requesting session (the popup echoes it back verbatim).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Display-request registry id from `DisplayRequestRaised`.
        id: u64,
        /// "approve" | "deny" | "deny_session".
        decision: String,
        /// Grant duration on approve: "this_session" | "15m" |
        /// "until_revoked" (absent = until_revoked).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration: Option<String>,
    },
    /// Create a virtual (Xvfb) display from a frontend — the keyless path
    /// that gives a claimed headless box a working display without any
    /// agent tool call. The daemon allocates the display number, launches
    /// Xvfb through the same machinery agent sessions use, registers a
    /// capture session, and announces it with `DisplayReady`; failures
    /// surface as `DisplayCaptureLost` with an actionable reason. The
    /// created display is daemon-owned: closing its tile
    /// (`RevokeUserDisplay` on its id) destroys it, a graceful daemon exit
    /// drops its guard, and a hard kill leaves it to the standard Xvfb
    /// orphan-reclaim on the next allocation (signal shutdown is
    /// `process::exit` — no `Drop`). Linux-only mechanism — other
    /// platforms report a clear error.
    CreateVirtualDisplay {
        /// Optional resolution; defaults to 1920x1080. Values are clamped
        /// to sane bounds and rounded down to even (VP8 requirement).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
    },
    /// **Phase 0 visual-freshness diagnostic** (task #83). Toggle the
    /// per-display marker overlay that the pool-feed bridge stamps into
    /// the I420 Y plane. Off by default; operator flips it on for a
    /// smoke run, off when the NDJSON transcript is collected. The
    /// browser-side `PeerDisplayConnection` sampler reads the marker
    /// per video frame to measure visual freshness without depending on
    /// getStats counters that proved misleading on task #81.
    ///
    /// Visible to ALL viewers of the named display when on, since the
    /// marker is stamped pre-encoder and lands in every encoded layer.
    /// Acceptable for an opt-in diagnostic flag — see DisplaySession's
    /// `diagnostics_visual_marker` field for the rationale.
    SetDiagnosticsVisualMarker {
        /// Optional display ID to toggle. When `None`, toggles the
        /// primary display (id 0) — same convention as the other
        /// display-scoped ControlMsg variants.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_id: Option<u32>,
        enabled: bool,
    },
    /// Claim input authority for one display (phase 5 of the multi-viewer
    /// redesign).  When granted, this WebSocket connection becomes the
    /// sole source of `display_input` events for `display_id`; other
    /// connections' input messages are silently dropped on the gate.
    /// If another connection currently holds the authority, it is force-
    /// released and this connection takes over — matches Zoom's
    /// "granting remote control auto-revokes prior" UX.
    ///
    /// Unclaimed state (no connection has requested authority) is the
    /// backwards-compatible default: every connection's input flows
    /// through unchanged, same as pre-phase-5 behavior.  The authority
    /// gate only activates on the first claim.
    RequestDisplayInputAuthority {
        display_id: u32,
    },
    /// Release input authority for one display.  No-op if the calling
    /// connection doesn't currently hold the authority (prevents one
    /// browser from unclaiming another's control).  Releases the slot,
    /// returning input to the unclaimed-any-connection-can-input state.
    ReleaseDisplayInputAuthority {
        display_id: u32,
    },
    /// One leg of a direct browser-to-peer file-transfer WebRTC signaling
    /// exchange. The primary daemon forwards browser offers/ICE to the peer;
    /// the peer emits answers/ICE back as OutboundEvent::PeerFileTransferSignal.
    PeerFileTransferSignal {
        session_id: String,
        signal: crate::peer::WebRtcSignal,
    },
    /// One leg of a direct browser-to-peer dashboard-control WebRTC signaling
    /// exchange. The primary daemon forwards browser offers/ICE to the peer;
    /// the peer emits answers/ICE back as OutboundEvent::PeerDashboardControlSignal.
    PeerDashboardControlSignal {
        session_id: String,
        signal: crate::peer::WebRtcSignal,
    },
    CreateBrowserWorkspace {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_dir: Option<String>,
    },
    CloseBrowserWorkspace {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    AcquireBrowserWorkspace {
        workspace_id: String,
        holder_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        holder_kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default)]
        force: bool,
    },
    ReleaseBrowserWorkspace {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        holder_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    ListDisplays,
    QueryDetail {
        scope: String,
        #[serde(default)]
        target: Option<String>,
    },
    RecallMemory {
        #[serde(default)]
        keywords: Option<Vec<String>>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        channel: Option<String>,
    },
    InvokeSkill {
        skill_name: String,
        #[serde(default)]
        arguments: Option<String>,
    },
    Quit,
    SetupDebugScreen,
    TeardownDebugScreen,
    StartDebugRecording,
    StopDebugRecording,
    StartRecording {
        stream_name: String,
    },
    StopRecording {
        stream_name: String,
    },
    DeleteRecording {
        stream_name: String,
    },
    /// Request interruption of the current agent turn.
    Interrupt {
        /// Optional target session. Daemon supervisors use this to interrupt
        /// one managed session instead of broadcasting to every session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Optional precondition: only interrupt if this turn id is active.
        /// When None, interrupts whatever is currently running.
        #[serde(default)]
        expected_turn: Option<u64>,
    },
    /// Mid-turn steering: nudge the currently running agent turn with new
    /// user text without interrupting it. Backends that support native
    /// mid-turn steering (Codex `turn/steer`) accept the text for the active
    /// turn and may apply it at their next runtime checkpoint; backends that
    /// don't queue the text onto the shared `context_injection` queue so it's
    /// delivered at the start of the next turn.
    Steer {
        /// Optional target session. Daemon supervisors use this to deliver
        /// the steer to one managed session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        text: String,
        /// Optional attachment ids queued from the dashboard. These mirror
        /// `StartTask.attachments`: live frame ids or `upload:<id>` handles.
        /// Native mid-turn steer protocols are text-only today, so managed
        /// sessions queue steers with attachments as the next follow-up turn
        /// rather than silently dropping the attachments.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
        /// Optional correlation id supplied by the frontend. Frontends use
        /// it to track the lifecycle of one steer request across the
        /// `SteerRequested` → `SteerAccepted`/`SteerQueued` →
        /// `SteerDelivered` events. An
        /// empty string or missing id is treated as "no correlation" —
        /// the backend still honors the request, but frontends that didn't
        /// tag it can't match the delivery event back to a pending UI row.
        #[serde(default)]
        id: Option<String>,
    },
    /// Clear one queued steer, or all queued steers for the target session
    /// when `id` is omitted. This only controls Intendant's pending queue/UI
    /// state; it cannot unsend text already accepted by a backend runtime.
    CancelSteer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Federation-driven WebRTC signaling toward this daemon. Carries the
    /// browser's offer / trickled ICE candidates / close-request, routed
    /// through the connecting peer's primary daemon over the federation
    /// transport. The daemon's WS handler dispatches to its
    /// `DisplaySession::handle_offer` (for `Offer`) or
    /// `add_ice_candidate` (for `IceCandidate`), keying the per-session
    /// `WebRtcPeer` by `(display_id, session_id)`.
    ///
    /// Distinct from the local `display_offer` / `display_ice` raw-JSON
    /// frames the dashboard sends directly: this typed variant scopes
    /// the federation path so peer-side dispatch can apply different
    /// auth/lifecycle rules without reading the raw `t` discriminator.
    /// The matching peer→connector direction comes back as
    /// [`crate::types::OutboundEvent::WebRtcSignal`] over the same WS.
    ///
    /// Explicit `rename` because serde's default `rename_all = "snake_case"`
    /// mangles the "Rtc" acronym into `web_rtc_signal` — same class of
    /// bug as `PeerKind::A2A → a2_a`. Canonical wire name is
    /// `webrtc_signal` (no underscore in the acronym).
    #[serde(rename = "webrtc_signal")]
    WebRtcSignal {
        display_id: u32,
        session_id: String,
        signal: crate::peer::WebRtcSignal,
    },
}

/// The event bus sender. Cloneable for use in multiple tasks.
///
/// Backed by a bounded `broadcast::channel` so multiple consumers can
/// subscribe independently. Normal subscribers receive best-effort future
/// events and must tolerate `RecvError::Lagged`;
/// [`EventBus::subscribe_session_log`] is the lossless path for the
/// low-volume event subset persisted to `session.jsonl`, and
/// [`EventBus::subscribe_intents`] is the lossless path for user intents
/// (`ControlCommand`) and the session-bookkeeping events that route them.
#[derive(Clone)]
pub struct EventBus {
    tx: tokio::sync::broadcast::Sender<AppEvent>,
    session_log_sinks: Arc<Mutex<Vec<tokio::sync::mpsc::UnboundedSender<AppEvent>>>>,
    intent_sinks: Arc<Mutex<Vec<tokio::sync::mpsc::UnboundedSender<AppEvent>>>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(4096);
        Self {
            tx,
            session_log_sinks: Arc::new(Mutex::new(Vec::new())),
            intent_sinks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn send(&self, event: AppEvent) {
        if app_event_writes_to_session_log(&event) {
            if let Ok(mut sinks) = self.session_log_sinks.lock() {
                sinks.retain(|sink| sink.send(event.clone()).is_ok());
            }
        }
        if app_event_rides_intent_lane(&event) {
            if let Ok(mut sinks) = self.intent_sinks.lock() {
                sinks.retain(|sink| sink.send(event.clone()).is_ok());
            }
        }
        let _ = self.tx.send(event);
    }

    /// Create a new best-effort subscriber for future events.
    ///
    /// This receiver is backed by the bounded broadcast channel. Slow
    /// consumers can observe `RecvError::Lagged` and skip dropped events.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AppEvent> {
        self.tx.subscribe()
    }

    /// Create a lossless session-log subscriber for durable event capture.
    ///
    /// The normal broadcast channel is intentionally bounded for UI/control
    /// consumers. Session logging must not lose low-volume lifecycle events
    /// such as `SteerRequested` / `SteerQueued` during high-volume model
    /// streaming, so the bus keeps a separate unbounded fan-out for just the
    /// event kinds that the session log writer persists.
    pub fn subscribe_session_log(&self) -> tokio::sync::mpsc::UnboundedReceiver<AppEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        if let Ok(mut sinks) = self.session_log_sinks.lock() {
            sinks.push(tx);
        }
        rx
    }

    /// Create a lossless intent subscriber.
    ///
    /// User intents (`AppEvent::ControlCommand`) share the bounded broadcast
    /// ring with per-token `ModelResponseDelta`s and multi-MB
    /// `ContextSnapshot`s, so a flooded ring could silently drop a user's
    /// click (approve, interrupt, start task) — consumers that ACT on
    /// intents must never lose one. This lane mirrors the session-log
    /// fan-out: an unbounded mpsc fed at the emit point, carrying only the
    /// low-volume [`app_event_rides_intent_lane`] subset, delivered in
    /// emission order. Human actions and the lifecycle events ordered with
    /// them are low-volume, so the unbounded queue is bounded in practice;
    /// the failure mode of a wedged consumer is memory growth, never a
    /// silently dropped action.
    ///
    /// The broadcast copy of every intent still exists for display-only
    /// consumers (waiters, UIs) that tolerate loss.
    pub fn subscribe_intents(&self) -> tokio::sync::mpsc::UnboundedReceiver<AppEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        if let Ok(mut sinks) = self.intent_sinks.lock() {
            sinks.push(tx);
        }
        rx
    }
}

/// The event subset carried by the lossless intent lane
/// ([`EventBus::subscribe_intents`]): the user intents themselves plus the
/// low-volume lifecycle/bookkeeping events the intent-acting consumers
/// (control plane, task dispatcher, session supervisor, recording listener)
/// fold into the same ordered stream — alias/relationship mapping that routes
/// FUTURE intents, the end-of-session / display-revoke hygiene the control
/// plane owns, and display/task boundaries that start or stop recording.
/// `SharedView` rides along for the same hygiene: the control plane's
/// focus-annotation tracker (`shared_view_lifecycle`) must fold the
/// human-scale shared-view verbs in emission order to know what a later
/// revoke or session end has to clear. Everything here is rare; nothing
/// high-frequency may join this list.
pub fn app_event_rides_intent_lane(event: &AppEvent) -> bool {
    matches!(
        event,
        AppEvent::ControlCommand(_)
            | AppEvent::SessionIdentity { .. }
            | AppEvent::SessionRelationship { .. }
            | AppEvent::SessionEnded { .. }
            | AppEvent::UserDisplayRevoked { .. }
            | AppEvent::SharedView { .. }
            | AppEvent::DisplayReady { .. }
            | AppEvent::TaskComplete { .. }
    )
}

/// Convert an AppEvent to an OutboundEvent for external consumers.
/// Returns `None` for terminal-only events (Key, Resize, Tick, Quit, etc.)
/// that external consumers don't need.
pub fn app_event_to_outbound(event: &AppEvent) -> Option<crate::types::OutboundEvent> {
    use crate::types::OutboundEvent;

    match event {
        AppEvent::TurnStarted {
            session_id,
            turn,
            budget_pct,
            ..
        } => Some(OutboundEvent::TurnStarted {
            session_id: session_id.clone(),
            turn: *turn,
            budget_pct: *budget_pct,
        }),
        AppEvent::ModelResponse {
            session_id,
            turn,
            content,
            reasoning,
            source,
            ..
        } => {
            let summary = crate::types::format_model_summary(content);
            Some(OutboundEvent::ModelResponse {
                session_id: session_id.clone(),
                turn: *turn,
                summary,
                reasoning_summary: reasoning.clone(),
                source: source.clone(),
            })
        }
        AppEvent::ModelResponseDelta { session_id, text } => {
            Some(OutboundEvent::ModelResponseDelta {
                session_id: session_id.clone(),
                text: text.clone(),
            })
        }
        AppEvent::AgentStarted {
            session_id,
            turn,
            commands_preview,
            item_id,
            source,
        } => Some(OutboundEvent::AgentStarted {
            session_id: session_id.clone(),
            turn: *turn,
            commands_preview: commands_preview.clone(),
            item_id: item_id.clone(),
            source: source.clone(),
        }),
        AppEvent::AgentOutput {
            session_id,
            stdout,
            stderr,
            source,
            output_id,
            item_id,
        } => Some(OutboundEvent::AgentOutput {
            session_id: session_id.clone(),
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            source: source.clone(),
            output_id: output_id.clone(),
            item_id: item_id.clone(),
        }),
        AppEvent::DoneSignal {
            session_id,
            message,
        } => Some(OutboundEvent::DoneSignal {
            session_id: session_id.clone(),
            message: message.clone(),
        }),
        AppEvent::TaskComplete {
            session_id,
            reason,
            summary,
        } => Some(OutboundEvent::TaskComplete {
            session_id: session_id.clone(),
            reason: reason.clone(),
            summary: summary.clone(),
        }),
        AppEvent::InterruptRequested { session_id } => Some(OutboundEvent::InterruptRequested {
            session_id: session_id.clone(),
        }),
        AppEvent::SessionStopRequested { .. } => None,
        AppEvent::Interrupted { session_id, reason } => Some(OutboundEvent::Interrupted {
            session_id: session_id.clone(),
            reason: reason.clone(),
        }),
        AppEvent::SteerRequested {
            session_id,
            text,
            id,
        } => Some(OutboundEvent::SteerRequested {
            session_id: session_id.clone(),
            text: text.clone(),
            id: id.clone(),
        }),
        AppEvent::SteerQueued {
            session_id,
            id,
            reason,
        } => Some(OutboundEvent::SteerQueued {
            session_id: session_id.clone(),
            id: id.clone(),
            reason: reason.clone(),
        }),
        AppEvent::SteerAccepted {
            session_id,
            id,
            reason,
        } => Some(OutboundEvent::SteerAccepted {
            session_id: session_id.clone(),
            id: id.clone(),
            reason: reason.clone(),
        }),
        AppEvent::SteerDelivered {
            session_id,
            id,
            mid_turn,
        } => Some(OutboundEvent::SteerDelivered {
            session_id: session_id.clone(),
            id: id.clone(),
            mid_turn: *mid_turn,
        }),
        AppEvent::SteerCancelRequested { .. } => None,
        AppEvent::SteerCancelled {
            session_id,
            id,
            reason,
        } => Some(OutboundEvent::SteerCancelled {
            session_id: session_id.clone(),
            id: id.clone(),
            reason: reason.clone(),
        }),
        AppEvent::SteerCancelFailed {
            session_id,
            id,
            reason,
        } => Some(OutboundEvent::SteerCancelFailed {
            session_id: session_id.clone(),
            id: id.clone(),
            reason: reason.clone(),
        }),
        AppEvent::FollowUpStatus {
            session_id,
            id,
            text,
            status,
            reason,
        } => Some(OutboundEvent::FollowUpStatus {
            session_id: session_id.clone(),
            id: id.clone(),
            text: text.clone(),
            status: status.clone(),
            reason: reason.clone(),
        }),
        AppEvent::UserMessageEditStatus {
            session_id,
            user_turn_index,
            status,
            message,
        } => Some(OutboundEvent::UserMessageEditStatus {
            session_id: session_id.clone(),
            user_turn_index: *user_turn_index,
            status: status.clone(),
            message: message.clone(),
        }),
        AppEvent::FollowUpCancelRequested { .. } => None,
        AppEvent::SessionStarted { session_id, task } => Some(OutboundEvent::SessionStarted {
            session_id: session_id.clone(),
            task: task.clone(),
        }),
        AppEvent::TaskReceived {
            delegation_id,
            session_id,
        } => Some(OutboundEvent::TaskReceived {
            delegation_id: delegation_id.clone(),
            session_id: session_id.clone(),
        }),
        AppEvent::SessionIdentity {
            session_id,
            source,
            backend_session_id,
        } => Some(OutboundEvent::SessionIdentity {
            session_id: session_id.clone(),
            source: source.clone(),
            backend_session_id: backend_session_id.clone(),
        }),
        AppEvent::SessionRelationship {
            parent_session_id,
            child_session_id,
            relationship,
            ephemeral,
        } => Some(OutboundEvent::SessionRelationship {
            parent_session_id: parent_session_id.clone(),
            child_session_id: child_session_id.clone(),
            relationship: relationship.clone(),
            ephemeral: *ephemeral,
        }),
        AppEvent::SessionCapabilities {
            session_id,
            capabilities,
        } => Some(OutboundEvent::SessionCapabilities {
            session_id: session_id.clone(),
            capabilities: capabilities.clone(),
        }),
        AppEvent::SessionGoal { session_id, goal } => Some(OutboundEvent::SessionGoal {
            session_id: session_id.clone(),
            goal: goal.clone(),
        }),
        AppEvent::SessionVitals { session_id, vitals } => Some(OutboundEvent::SessionVitals {
            session_id: session_id.clone(),
            vitals: vitals.clone(),
        }),
        AppEvent::SessionAttached { session_id, source } => Some(OutboundEvent::SessionAttached {
            session_id: session_id.clone(),
            source: source.clone(),
        }),
        AppEvent::SessionEnded {
            session_id,
            reason,
            error_kind,
        } => Some(OutboundEvent::SessionEnded {
            session_id: session_id.clone(),
            reason: reason.clone(),
            error_kind: error_kind.clone(),
        }),
        AppEvent::DebugScreenReady { display_id } => Some(OutboundEvent::DebugScreenReady {
            display_id: *display_id,
        }),
        AppEvent::DebugScreenTornDown { display_id } => Some(OutboundEvent::DebugScreenTornDown {
            display_id: *display_id,
        }),
        AppEvent::ApprovalRequired {
            session_id,
            id,
            command_preview,
            ..
        } => Some(OutboundEvent::ApprovalRequired {
            session_id: session_id.clone(),
            id: *id,
            command: command_preview.clone(),
        }),
        AppEvent::UserQuestionRequired {
            session_id,
            id,
            questions,
        } => Some(OutboundEvent::UserQuestion {
            session_id: session_id.clone(),
            id: *id,
            questions: questions.clone(),
        }),
        AppEvent::UserNotification {
            session_id,
            id,
            title,
            text,
            urgency,
            ts,
        } => Some(OutboundEvent::UserNotification {
            session_id: session_id.clone(),
            id: id.clone(),
            title: title.clone(),
            text: text.clone(),
            urgency: *urgency,
            ts: *ts,
        }),
        AppEvent::AutoApproved { preview } => Some(OutboundEvent::AutoApproved {
            preview: preview.clone(),
        }),
        AppEvent::ApprovalResolved {
            session_id,
            id,
            action,
        } => Some(OutboundEvent::ApprovalResolved {
            session_id: session_id.clone(),
            id: *id,
            action: action.clone(),
        }),
        AppEvent::HumanQuestionDetected { question } => Some(OutboundEvent::AskHuman {
            question: question.clone(),
        }),
        AppEvent::HumanResponseSent => Some(OutboundEvent::HumanResponseSent),
        AppEvent::RoundComplete {
            session_id,
            round,
            turns_in_round,
            ..
        } => Some(OutboundEvent::RoundComplete {
            session_id: session_id.clone(),
            round: *round,
            turns_in_round: *turns_in_round,
        }),
        AppEvent::DisplayReady {
            display_id,
            width,
            height,
            agent_visible,
        } => Some(OutboundEvent::DisplayReady {
            display_id: *display_id,
            width: *width,
            height: *height,
            agent_visible: *agent_visible,
        }),
        AppEvent::DisplayResize {
            display_id,
            width,
            height,
        } => Some(OutboundEvent::DisplayResize {
            display_id: *display_id,
            width: *width,
            height: *height,
        }),
        AppEvent::CuActionExecuted {
            event_id,
            session_id,
            display_id,
            kind,
            x,
            y,
            ref_w,
            ref_h,
            raw,
            ts,
        } => Some(OutboundEvent::CuAction {
            event_id: event_id.clone(),
            session_id: session_id.clone(),
            display_id: *display_id,
            kind: kind.clone(),
            x: *x,
            y: *y,
            ref_w: *ref_w,
            ref_h: *ref_h,
            raw: raw.clone(),
            ts: *ts,
        }),
        AppEvent::DisplayTaken { display_id } => Some(OutboundEvent::DisplayTaken {
            display_id: *display_id,
        }),
        AppEvent::DisplayReleased { display_id, note } => Some(OutboundEvent::DisplayReleased {
            display_id: *display_id,
            note: note.clone(),
        }),
        AppEvent::UserDisplayGranted {
            display_id,
            agent_visible,
        } => Some(OutboundEvent::UserDisplayGranted {
            display_id: *display_id,
            agent_visible: *agent_visible,
        }),
        AppEvent::UserDisplayRevoked { display_id, note } => {
            Some(OutboundEvent::UserDisplayRevoked {
                display_id: *display_id,
                note: note.clone(),
            })
        }
        AppEvent::DisplayRequestRaised {
            session_id,
            id,
            access,
            reason,
            expires_unix_ms,
        } => Some(OutboundEvent::DisplayRequestRaised {
            session_id: session_id.clone(),
            id: *id,
            access: access.clone(),
            reason: reason.clone(),
            expires_unix_ms: *expires_unix_ms,
        }),
        AppEvent::DisplayRequestResolved {
            session_id,
            id,
            outcome,
            access,
            duration,
        } => Some(OutboundEvent::DisplayRequestResolved {
            session_id: session_id.clone(),
            id: *id,
            outcome: outcome.clone(),
            access: access.clone(),
            duration: duration.clone(),
        }),
        AppEvent::ContextManagement { turn } => {
            Some(OutboundEvent::ContextManagement { turn: *turn })
        }
        AppEvent::BudgetWarning { pct, remaining } => Some(OutboundEvent::BudgetWarning {
            pct: *pct,
            remaining: *remaining,
        }),
        AppEvent::BudgetExhausted { remaining } => Some(OutboundEvent::BudgetExhausted {
            remaining: *remaining,
        }),
        AppEvent::SafetyCapReached => Some(OutboundEvent::SafetyCapReached),
        AppEvent::LoopError(msg) => Some(OutboundEvent::LoopError {
            message: msg.clone(),
        }),
        AppEvent::SubAgentResult { formatted } => Some(OutboundEvent::SubAgentResult {
            summary: formatted.clone(),
        }),
        AppEvent::UserTranscript { text, seq } => Some(OutboundEvent::UserTranscript {
            text: text.clone(),
            seq: *seq,
        }),
        AppEvent::PresenceLog { message, level, .. } => Some(OutboundEvent::PresenceLog {
            message: message.clone(),
            level: level
                .as_ref()
                .map(|l| crate::frontend::log_level_to_str(l).to_string()),
        }),
        AppEvent::PresenceUsageUpdate {
            total_tokens,
            context_window,
            usage_pct,
            provider,
            model,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_creation_tokens,
        } => Some(OutboundEvent::PresenceUsageUpdate {
            total_tokens: *total_tokens,
            context_window: *context_window,
            usage_pct: *usage_pct,
            provider: provider.clone(),
            model: model.clone(),
            prompt_tokens: *prompt_tokens,
            completion_tokens: *completion_tokens,
            cached_tokens: *cached_tokens,
            cache_creation_tokens: *cache_creation_tokens,
        }),
        AppEvent::LiveUsageUpdate {
            provider,
            model,
            input_tokens,
            output_tokens,
            cached_tokens,
            total_tokens,
            thinking_tokens,
            input_text_tokens,
            input_audio_tokens,
            input_image_tokens,
            cached_text_tokens,
            cached_audio_tokens,
            cached_image_tokens,
            output_text_tokens,
            output_audio_tokens,
        } => Some(OutboundEvent::LiveUsageUpdate {
            provider: provider.clone(),
            model: model.clone(),
            input_tokens: *input_tokens,
            output_tokens: *output_tokens,
            cached_tokens: *cached_tokens,
            total_tokens: *total_tokens,
            thinking_tokens: *thinking_tokens,
            input_text_tokens: *input_text_tokens,
            input_audio_tokens: *input_audio_tokens,
            input_image_tokens: *input_image_tokens,
            cached_text_tokens: *cached_text_tokens,
            cached_audio_tokens: *cached_audio_tokens,
            cached_image_tokens: *cached_image_tokens,
            output_text_tokens: *output_text_tokens,
            output_audio_tokens: *output_audio_tokens,
        }),
        AppEvent::UsageSnapshot {
            session_id,
            main,
            presence,
        } => Some(OutboundEvent::UsageUpdate {
            session_id: session_id.clone(),
            main: main.clone(),
            presence: presence.clone(),
        }),
        AppEvent::ContextSnapshot {
            session_id,
            source,
            label,
            request_id,
            request_index,
            turn,
            format,
            token_count,
            token_count_kind,
            context_window,
            hard_context_window,
            item_count,
            raw,
        } => Some(OutboundEvent::ContextSnapshot {
            session_id: session_id.clone(),
            source: source.clone(),
            label: label.clone(),
            request_id: request_id.clone(),
            request_index: *request_index,
            turn: *turn,
            format: format.clone(),
            token_count: *token_count,
            token_count_kind: token_count_kind.clone(),
            context_window: *context_window,
            hard_context_window: *hard_context_window,
            item_count: *item_count,
            raw: compact_context_snapshot_raw_for_outbound(
                raw,
                request_id.as_deref(),
                *request_index,
                format,
            ),
        }),
        AppEvent::StatusUpdate {
            turn,
            phase,
            autonomy,
            session_id,
            task,
        } => Some(OutboundEvent::Status {
            turn: *turn,
            phase: phase.clone(),
            autonomy: autonomy.clone(),
            session_id: session_id.clone(),
            task: task.clone(),
            external_agent: None,
        }),
        AppEvent::ExternalAgentChanged { agent } => Some(OutboundEvent::ExternalAgentChanged {
            agent: agent.clone(),
        }),
        AppEvent::AutonomyChanged { autonomy } => Some(OutboundEvent::AutonomyChanged {
            autonomy: autonomy.clone(),
        }),
        AppEvent::CodexThreadActionRequested {
            request_id,
            session_id,
            action,
            ..
        } => Some(OutboundEvent::CodexThreadActionRequested {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            action: action.clone(),
        }),
        AppEvent::CodexThreadActionResult {
            session_id,
            action,
            success,
            message,
            record_id,
        } => Some(OutboundEvent::CodexThreadActionResult {
            session_id: session_id.clone(),
            action: action.clone(),
            success: *success,
            message: message.clone(),
            record_id: record_id.clone(),
        }),
        AppEvent::SessionRenameResult {
            session_id,
            source,
            name,
            success,
            message,
        } => Some(OutboundEvent::SessionRenameResult {
            session_id: session_id.clone(),
            source: source.clone(),
            name: name.clone(),
            success: *success,
            message: message.clone(),
        }),
        AppEvent::SessionAgentConfigResult {
            session_id,
            source,
            backend_session_id,
            intendant_session_id,
            persisted_session_ids,
            success,
            message,
        } => Some(OutboundEvent::SessionAgentConfigResult {
            session_id: session_id.clone(),
            source: source.clone(),
            backend_session_id: backend_session_id.clone(),
            intendant_session_id: intendant_session_id.clone(),
            persisted_session_ids: persisted_session_ids.clone(),
            success: *success,
            message: message.clone(),
        }),
        AppEvent::ExternalFollowUpRequested { .. } => None,
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
        } => Some(OutboundEvent::CodexConfigChanged {
            command: command.clone(),
            managed_command: managed_command.clone(),
            managed_command_cleared: *managed_command_cleared,
            sandbox: sandbox.clone(),
            approval_policy: approval_policy.clone(),
            model: model.clone(),
            model_cleared: *model_cleared,
            reasoning_effort: reasoning_effort.clone(),
            reasoning_effort_cleared: *reasoning_effort_cleared,
            service_tier: service_tier.clone(),
            service_tier_cleared: *service_tier_cleared,
            web_search: *web_search,
            network_access: *network_access,
            writable_roots: writable_roots.clone(),
            managed_context: managed_context.clone(),
            context_archive: context_archive.clone(),
        }),
        AppEvent::ClaudeConfigChanged {
            model,
            model_cleared,
            permission_mode,
            allowed_tools,
        } => Some(OutboundEvent::ClaudeConfigChanged {
            model: model.clone(),
            model_cleared: *model_cleared,
            permission_mode: permission_mode.clone(),
            allowed_tools: allowed_tools.clone(),
        }),
        AppEvent::LogEntry {
            session_id,
            level,
            source,
            content,
            turn,
        } => Some(OutboundEvent::LogEntry {
            level: level.clone(),
            source: source.clone(),
            content: content.clone(),
            turn: *turn,
            session_id: session_id.clone(),
            user_turn_index: None,
            user_turn_revision: None,
            replacement_for_user_turn_index: None,
        }),
        AppEvent::SessionNote {
            session_id,
            note_id,
            text,
            attachments,
            source,
            ts,
        } => Some(OutboundEvent::SessionNote {
            session_id: session_id.clone(),
            note_id: note_id.clone(),
            text: text.clone(),
            attachments: attachments.clone(),
            source: source.clone(),
            ts: *ts,
        }),
        AppEvent::UserMessageRewind {
            session_id,
            user_turn_index,
            turns_removed,
        } => Some(OutboundEvent::UserMessageRewind {
            session_id: session_id.clone(),
            user_turn_index: *user_turn_index,
            turns_removed: *turns_removed,
        }),
        AppEvent::UserMessageLog {
            session_id,
            content,
            user_turn_index,
            user_turn_revision,
            replacement_for_user_turn_index,
        } => Some(OutboundEvent::LogEntry {
            level: "info".to_string(),
            source: "User".to_string(),
            content: content.clone(),
            turn: None,
            session_id: session_id.clone(),
            user_turn_index: *user_turn_index,
            user_turn_revision: *user_turn_revision,
            replacement_for_user_turn_index: *replacement_for_user_turn_index,
        }),
        AppEvent::RecordingStarted { stream_name } => Some(OutboundEvent::RecordingStarted {
            stream_name: stream_name.clone(),
        }),
        AppEvent::RecordingStopped { stream_name } => Some(OutboundEvent::RecordingStopped {
            stream_name: stream_name.clone(),
        }),
        AppEvent::RecordingDeleted { stream_name } => Some(OutboundEvent::RecordingDeleted {
            stream_name: stream_name.clone(),
        }),
        AppEvent::RecordingError {
            stream_name,
            message,
        } => Some(OutboundEvent::RecordingError {
            stream_name: stream_name.clone(),
            message: message.clone(),
        }),
        AppEvent::DisplayMetrics { snapshot } => Some(OutboundEvent::DisplayMetrics {
            display_id: snapshot.display_id,
            capture_fps: snapshot.capture_fps,
            capture_drops: snapshot.capture_drops,
            encode_fps: snapshot.encode_fps,
            encode_freshness_avg_ms: snapshot.encode_freshness_avg_ms,
            encode_drops: snapshot.encode_drops,
            peer_count: snapshot.peer_count,
            peer_drops: snapshot.peer_drops,
            resolution_width: snapshot.resolution.0,
            resolution_height: snapshot.resolution.1,
            tile_damage_samples: snapshot.tile_damage_samples,
            tile_dirty_rects: snapshot.tile_dirty_rects,
            tile_dirty_tiles: snapshot.tile_dirty_tiles,
            tile_dirty_fraction_avg: snapshot.tile_dirty_fraction_avg,
            tile_delta_cadence_skips: snapshot.tile_delta_cadence_skips,
            tile_delta_records: snapshot.tile_delta_records,
            tile_delta_fps: snapshot.tile_delta_fps,
            tile_delta_kbps: snapshot.tile_delta_kbps,
            tile_snapshot_records: snapshot.tile_snapshot_records,
            tile_snapshot_frames: snapshot.tile_snapshot_frames,
            tile_snapshot_kbps: snapshot.tile_snapshot_kbps,
        }),
        AppEvent::DisplayCaptureLost { display_id, reason } => {
            Some(OutboundEvent::DisplayCaptureLost {
                display_id: *display_id,
                reason: reason.clone(),
            })
        }
        AppEvent::DisplayApprovalPending {
            display_id,
            backend,
        } => Some(OutboundEvent::DisplayApprovalPending {
            display_id: *display_id,
            backend: backend.to_string(),
        }),
        AppEvent::SharedView {
            session_id,
            action,
            display_target,
            display_id,
            reason,
            region,
            note,
        } => Some(OutboundEvent::SharedView {
            session_id: session_id.clone(),
            action: action.clone(),
            display_target: display_target.clone(),
            display_id: *display_id,
            reason: reason.clone(),
            region: region.clone(),
            note: note.clone(),
        }),
        AppEvent::BrowserWorkspaceChanged {
            kind,
            workspace,
            workspace_id,
            message,
        } => Some(OutboundEvent::BrowserWorkspaceChanged {
            kind: kind.clone(),
            workspace: workspace.clone(),
            workspace_id: workspace_id.clone(),
            message: message.clone(),
        }),
        AppEvent::UploadReady { descriptor } => Some(OutboundEvent::UploadReady {
            descriptor: descriptor.clone(),
        }),
        AppEvent::UploadDeleted { id } => Some(OutboundEvent::UploadDeleted { id: id.clone() }),
        AppEvent::FileChanged {
            path,
            kind,
            lines_added,
            lines_removed,
        } => {
            let kind_str = serde_json::to_value(kind)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "modified".to_string());
            Some(OutboundEvent::FileChanged {
                path: path.clone(),
                kind: kind_str,
                lines_added: *lines_added,
                lines_removed: *lines_removed,
            })
        }
        AppEvent::SnapshotCreated { round_id } => Some(OutboundEvent::SnapshotCreated {
            round_id: *round_id,
        }),
        AppEvent::RolledBack {
            from_id,
            to_id,
            files_reverted,
        } => Some(OutboundEvent::RolledBack {
            from_id: *from_id,
            to_id: *to_id,
            files_reverted: *files_reverted,
        }),
        AppEvent::Redone { to_id } => Some(OutboundEvent::Redone { to_id: *to_id }),
        AppEvent::HistoryPruned {
            branches_removed,
            bytes_freed,
        } => Some(OutboundEvent::HistoryPruned {
            branches_removed: *branches_removed,
            bytes_freed: *bytes_freed,
        }),
        AppEvent::ConversationRolledBack {
            session_id,
            round_id,
            turns_removed,
            backend,
            method,
        } => Some(OutboundEvent::ConversationRolledBack {
            session_id: session_id.clone(),
            round_id: *round_id,
            turns_removed: *turns_removed,
            backend: backend.clone(),
            method: method.clone(),
        }),
        AppEvent::AgendaChanged { item, counts } => Some(OutboundEvent::AgendaChanged {
            item: item.clone(),
            counts: *counts,
        }),
        // Input event for the agent loop — not broadcast to browsers.
        AppEvent::ConversationRollbackRequested { .. } => None,
        // Internal events — not broadcast to external consumers
        AppEvent::Tick
        | AppEvent::JsonExtracted { .. }
        | AppEvent::SessionDirChanged { .. }
        | AppEvent::ControlCommand(_)
        | AppEvent::PresenceReady
        | AppEvent::PresenceConnected { .. }
        | AppEvent::PresenceDisconnected
        | AppEvent::VoiceLog { .. }
        | AppEvent::PresenceCheckpointReceived { .. }
        | AppEvent::VoiceDiagnostic { .. }
        | AppEvent::LiveAudioStarted { .. }
        | AppEvent::LiveAudioProgress { .. }
        | AppEvent::LiveAudioCompleted { .. } => None,
    }
}

/// How long buffered `ModelResponseDelta` text may wait before the outbound
/// broadcaster flushes it (see [`spawn_outbound_broadcaster`]).
const DELTA_COALESCE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(40);

/// Safety flush: once this much delta text is buffered across sessions,
/// flush immediately instead of waiting for the tick — bounds worst-case
/// buffering (and the size of a single outbound line) under a sustained
/// flood that keeps the event receiver permanently ready.
const DELTA_COALESCE_MAX_BUFFERED_BYTES: usize = 64 * 1024;

/// Per-session buffered delta text, insertion-ordered so a flush emits
/// sessions in first-arrival order (per-session text order is exact).
#[derive(Default)]
struct DeltaCoalesceBuffer {
    pending: Vec<(Option<String>, String)>,
    buffered_bytes: usize,
}

impl DeltaCoalesceBuffer {
    fn push(&mut self, session_id: &Option<String>, text: &str) {
        self.buffered_bytes += text.len();
        if let Some((_, buffered)) = self
            .pending
            .iter_mut()
            .find(|(pending_session, _)| pending_session == session_id)
        {
            buffered.push_str(text);
            return;
        }
        self.pending.push((session_id.clone(), text.to_string()));
    }

    fn should_flush_early(&self) -> bool {
        self.buffered_bytes >= DELTA_COALESCE_MAX_BUFFERED_BYTES
    }

    /// Emit one merged `model_response_delta` line per buffered session.
    /// Payload shape is identical to an uncoalesced delta — only the text
    /// spans are concatenated — so consumers that append deltas observe
    /// the same byte stream.
    fn flush(&mut self, outbound_tx: &tokio::sync::broadcast::Sender<String>) {
        for (session_id, text) in self.pending.drain(..) {
            let outbound = crate::types::OutboundEvent::ModelResponseDelta { session_id, text };
            if let Ok(json) = serde_json::to_string(&outbound) {
                let _ = outbound_tx.send(json);
            }
        }
        self.buffered_bytes = 0;
    }
}

/// Spawn a task that converts AppEvents to OutboundEvents and broadcasts them.
///
/// This is the single point where AppEvents are converted to the external
/// format used by the control socket, web gateway, and JSON stdout.
///
/// Per-token `ModelResponseDelta` events are coalesced here — the single
/// outbound choke point — instead of fanning one wire line (and one WS
/// message per connection) out per token: delta text buffers per session
/// and flushes on a ~40ms tick, when the buffer crosses a size cap, or
/// IMMEDIATELY before any non-delta event is forwarded, so deltas never
/// reorder against other events and per-session text order is preserved.
pub fn spawn_outbound_broadcaster(
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    outbound_tx: tokio::sync::broadcast::Sender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut deltas = DeltaCoalesceBuffer::default();
        let mut flush_tick = tokio::time::interval(DELTA_COALESCE_INTERVAL);
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                recv = event_rx.recv() => match recv {
                    Ok(AppEvent::ModelResponseDelta { session_id, text }) => {
                        deltas.push(&session_id, &text);
                        if deltas.should_flush_early() {
                            deltas.flush(&outbound_tx);
                        }
                    }
                    Ok(event) => {
                        // Ordering barrier: buffered deltas were emitted
                        // before this event — flush them first.
                        deltas.flush(&outbound_tx);
                        if let Some(outbound) = app_event_to_outbound(&event) {
                            if let Ok(json) = serde_json::to_string(&outbound) {
                                let _ = outbound_tx.send(json);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        deltas.flush(&outbound_tx);
                        break;
                    }
                },
                _ = flush_tick.tick() => deltas.flush(&outbound_tx),
            }
        }
    })
}

/// Spawn a task that persists AppEvents to the session log on disk.
///
/// This is the single point where events flowing through the bus are written
/// to `session.jsonl`. Events that are already logged inline by the agent loop
/// (turn_start, model_response, agent_input/output, approval decisions, etc.)
/// are skipped here to avoid duplication — only events that would otherwise be
/// lost are handled.
///
/// Counterpart to `spawn_outbound_broadcaster` which handles the WebSocket/
/// control-socket broadcast path.
pub fn spawn_session_log_writer(
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    session_log: crate::SharedSessionLog,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            write_event_to_session_log(&session_log, &event);
        }
    })
}

fn app_event_writes_to_session_log(event: &AppEvent) -> bool {
    matches!(
        event,
        AppEvent::AgentStarted { .. }
            | AppEvent::DoneSignal { .. }
            | AppEvent::TaskComplete { .. }
            | AppEvent::SteerRequested { .. }
            | AppEvent::SteerQueued { .. }
            | AppEvent::SteerAccepted { .. }
            | AppEvent::SteerDelivered { .. }
            | AppEvent::SteerCancelled { .. }
            | AppEvent::SteerCancelFailed { .. }
            | AppEvent::InterruptRequested { .. }
            | AppEvent::Interrupted { .. }
            | AppEvent::SessionStarted { .. }
            | AppEvent::SessionIdentity { .. }
            | AppEvent::SessionRelationship { .. }
            | AppEvent::SessionCapabilities { .. }
            | AppEvent::SessionGoal { .. }
            | AppEvent::SessionVitals { .. }
            | AppEvent::SessionAttached { .. }
            | AppEvent::SessionEnded { .. }
            | AppEvent::SafetyCapReached
            | AppEvent::SubAgentResult { .. }
            | AppEvent::RoundComplete { .. }
            | AppEvent::AutoApproved { .. }
            | AppEvent::ApprovalResolved { .. }
            | AppEvent::HumanQuestionDetected { .. }
            | AppEvent::HumanResponseSent
            | AppEvent::SessionNote { .. }
            | AppEvent::UserNotification { .. }
            | AppEvent::DisplayReady { .. }
            | AppEvent::DisplayResize { .. }
            | AppEvent::DisplayTaken { .. }
            | AppEvent::DisplayReleased { .. }
            | AppEvent::DisplayCaptureLost { .. }
            | AppEvent::DisplayApprovalPending { .. }
            | AppEvent::SharedView { .. }
            | AppEvent::UserDisplayGranted { .. }
            | AppEvent::UserDisplayRevoked { .. }
            | AppEvent::DebugScreenReady { .. }
            | AppEvent::DebugScreenTornDown { .. }
            | AppEvent::RecordingStarted { .. }
            | AppEvent::RecordingStopped { .. }
            | AppEvent::RecordingError { .. }
            | AppEvent::RecordingDeleted { .. }
            | AppEvent::PresenceLog { .. }
            | AppEvent::PresenceUsageUpdate { .. }
            | AppEvent::LiveUsageUpdate { .. }
            | AppEvent::ContextSnapshot { .. }
            | AppEvent::LiveAudioStarted { .. }
            | AppEvent::LiveAudioProgress { .. }
            | AppEvent::LiveAudioCompleted { .. }
            | AppEvent::ModelResponse { .. }
            | AppEvent::FileChanged { .. }
            | AppEvent::SnapshotCreated { .. }
            | AppEvent::RolledBack { .. }
            | AppEvent::Redone { .. }
            | AppEvent::HistoryPruned { .. }
            | AppEvent::ConversationRolledBack { .. }
    )
}

/// Write a single AppEvent to the session log if it isn't already logged
/// inline by the agent loop.
fn write_event_to_session_log(session_log: &crate::SharedSessionLog, event: &AppEvent) {
    let Ok(mut log) = session_log.lock() else {
        return;
    };

    match event {
        // ---- Events NOT logged inline — this writer is their only path to disk ----

        // Agent lifecycle
        AppEvent::AgentStarted {
            session_id,
            turn,
            commands_preview,
            item_id,
            source,
        } => {
            log.agent_started_with_session_id(
                session_id.as_deref(),
                *turn,
                commands_preview,
                item_id.as_deref(),
                source.as_deref(),
            );
        }
        AppEvent::DoneSignal {
            session_id,
            message,
        } => {
            log.done_signal_for_session(session_id.as_deref(), message.as_deref());
        }
        AppEvent::TaskComplete {
            session_id,
            reason,
            summary,
        } => {
            log.task_complete_for_session(session_id.as_deref(), reason, summary.as_deref());
        }
        AppEvent::SteerRequested {
            session_id,
            text,
            id,
        } => {
            log.steer_requested(session_id.as_deref(), id, text);
        }
        AppEvent::SteerQueued {
            session_id,
            id,
            reason,
        } => {
            log.steer_queued(session_id.as_deref(), id, reason);
        }
        AppEvent::SteerAccepted {
            session_id,
            id,
            reason,
        } => {
            log.steer_accepted(session_id.as_deref(), id, reason);
        }
        AppEvent::SteerDelivered {
            session_id,
            id,
            mid_turn,
        } => {
            log.steer_delivered(session_id.as_deref(), id, *mid_turn);
        }
        AppEvent::SteerCancelled {
            session_id,
            id,
            reason,
        } => {
            log.steer_cancelled(session_id.as_deref(), id, reason);
        }
        AppEvent::SteerCancelFailed {
            session_id,
            id,
            reason,
        } => {
            log.steer_cancel_failed(session_id.as_deref(), id, reason);
        }
        AppEvent::InterruptRequested { .. } => {
            log.info("Interrupt requested");
        }
        AppEvent::Interrupted { reason, .. } => {
            log.info(&format!("Interrupted: {}", reason));
        }
        AppEvent::SessionStarted { session_id, task } => {
            log.session_started(session_id, task.as_deref());
        }
        AppEvent::SessionIdentity {
            session_id,
            source,
            backend_session_id,
        } => {
            log.session_identity(session_id, source, backend_session_id);
        }
        AppEvent::SessionRelationship {
            parent_session_id,
            child_session_id,
            relationship,
            ephemeral,
        } => {
            log.session_relationship(
                parent_session_id,
                child_session_id,
                relationship,
                *ephemeral,
            );
        }
        AppEvent::SessionCapabilities {
            session_id,
            capabilities,
        } => {
            log.session_capabilities(session_id, capabilities);
        }
        AppEvent::SessionGoal { session_id, goal } => {
            log.session_goal(session_id, goal.as_ref());
        }
        AppEvent::SessionVitals { session_id, vitals } => {
            log.session_vitals(session_id, vitals);
        }
        AppEvent::SessionAttached { session_id, source } => {
            log.session_attached(session_id, source);
        }
        AppEvent::SessionEnded {
            session_id, reason, ..
        } => {
            log.session_ended(session_id, reason);
        }
        AppEvent::SafetyCapReached => {
            log.safety_cap_reached();
        }
        AppEvent::SubAgentResult { formatted } => {
            log.sub_agent_result(formatted);
        }
        AppEvent::SessionNote {
            session_id,
            note_id,
            text,
            attachments,
            source,
            ts,
        } => {
            log.session_note(
                session_id.as_deref(),
                note_id,
                text,
                attachments,
                source.as_deref(),
                *ts,
            );
        }
        AppEvent::UserNotification {
            session_id,
            id,
            title,
            text,
            urgency,
            ts,
        } => {
            log.user_notification(
                session_id.as_deref(),
                id,
                title.as_deref(),
                text,
                *urgency,
                *ts,
            );
        }
        AppEvent::RoundComplete {
            round,
            turns_in_round,
            ..
        } => {
            log.round_complete(*round, *turns_in_round);
        }

        // Approval / human interaction
        AppEvent::AutoApproved { preview } => {
            log.auto_approved(preview);
        }
        AppEvent::ApprovalResolved { id, action, .. } => {
            log.approval_resolved(*id, action);
        }
        AppEvent::HumanQuestionDetected { question } => {
            log.human_question(question);
        }
        AppEvent::HumanResponseSent => {
            log.human_response_sent();
        }

        // Display / vision
        AppEvent::DisplayReady {
            display_id,
            width,
            height,
            agent_visible,
        } => {
            log.display_ready(*display_id, *width, *height, *agent_visible);
        }
        AppEvent::DisplayResize {
            display_id,
            width,
            height,
        } => {
            log.display_resize(*display_id, *width, *height);
        }
        AppEvent::DisplayTaken { display_id } => {
            log.display_taken(*display_id);
        }
        AppEvent::DisplayReleased { display_id, note } => {
            log.display_released(*display_id, note.as_deref());
        }
        AppEvent::DisplayCaptureLost { display_id, reason } => {
            log.warn(&format!("Display :{} capture lost: {}", display_id, reason));
        }
        AppEvent::DisplayApprovalPending {
            display_id,
            backend,
        } => {
            log.info(&format!(
                "Display :{} waiting for OS approval ({backend} portal)",
                display_id
            ));
        }
        AppEvent::SharedView {
            action,
            display_id,
            display_target,
            reason,
            note,
            ..
        } => {
            let target = display_id
                .map(|id| format!(":{}", id))
                .or_else(|| display_target.clone())
                .unwrap_or_else(|| "default display".to_string());
            let detail = reason
                .as_deref()
                .or(note.as_deref())
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!(": {}", s))
                .unwrap_or_default();
            log.info(&format!("Shared view {} on {}{}", action, target, detail));
        }
        AppEvent::UserDisplayGranted {
            display_id,
            agent_visible,
        } => {
            log.info(&format!(
                "User display {} recorded (display_id: {})",
                if *agent_visible {
                    "access grant"
                } else {
                    "private view"
                },
                display_id
            ));
        }
        AppEvent::UserDisplayRevoked { display_id, note } => {
            let msg = if let Some(n) = note {
                format!(
                    "User display access revoked (display_id: {}): {}",
                    display_id, n
                )
            } else {
                format!("User display access revoked (display_id: {})", display_id)
            };
            log.info(&msg);
        }
        AppEvent::DebugScreenReady { display_id } => {
            log.debug_screen_ready(*display_id);
        }
        AppEvent::DebugScreenTornDown { display_id } => {
            log.debug_screen_torn_down(*display_id);
        }

        // Recording
        AppEvent::RecordingStarted { stream_name } => {
            log.recording_started(stream_name);
        }
        AppEvent::RecordingStopped { stream_name } => {
            log.recording_stopped(stream_name);
        }
        AppEvent::RecordingError {
            stream_name,
            message,
        } => {
            log.recording_error(stream_name, message);
        }
        AppEvent::RecordingDeleted { stream_name } => {
            log.recording_deleted(stream_name);
        }

        // Presence / voice
        AppEvent::PresenceLog { message, level, .. } => {
            let level_str = level.as_ref().map(|l| crate::frontend::log_level_to_str(l));
            log.presence_log(message, level_str);
        }
        AppEvent::PresenceUsageUpdate {
            provider,
            model,
            total_tokens,
            context_window,
            usage_pct,
            ..
        } => {
            log.presence_usage_update(provider, model, *total_tokens, *context_window, *usage_pct);
        }
        AppEvent::LiveUsageUpdate {
            provider,
            model,
            total_tokens,
            ..
        } => {
            log.live_usage_update(provider, model, *total_tokens);
        }
        AppEvent::ContextSnapshot {
            session_id,
            source,
            label,
            request_id,
            request_index,
            turn,
            format,
            token_count,
            token_count_kind,
            context_window,
            hard_context_window,
            item_count,
            raw,
        } => {
            log.context_snapshot_for_session(
                session_id.as_deref(),
                source,
                label,
                request_id.as_deref(),
                *request_index,
                *turn,
                format,
                *token_count,
                token_count_kind.as_deref(),
                *context_window,
                *hard_context_window,
                *item_count,
                raw,
            );
        }

        // Live audio sub-agent lifecycle
        AppEvent::LiveAudioStarted { id, provider } => {
            log.live_audio_started(id, provider);
        }
        AppEvent::LiveAudioProgress {
            id,
            state,
            elapsed_secs,
            transcript_preview,
        } => {
            log.live_audio_progress(id, state, *elapsed_secs, transcript_preview);
        }
        AppEvent::LiveAudioCompleted {
            id,
            status,
            quarantine_count,
            ..
        } => {
            log.live_audio_completed(id, status, *quarantine_count);
        }

        AppEvent::ModelResponse {
            session_id,
            content,
            usage,
            reasoning,
            source,
            ..
        } => {
            if !content.is_empty() {
                log.model_response_for_session(
                    session_id.as_deref(),
                    content,
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.total_tokens,
                    usage.cached_tokens,
                    usage.cache_creation_tokens,
                    source.as_deref(),
                );
            }
            if let Some(ref r) = reasoning {
                log.reasoning_content_for_session(
                    session_id.as_deref(),
                    source.as_deref(),
                    Some(r.as_str()),
                    None,
                );
            }
        }

        // File watcher
        AppEvent::FileChanged {
            path,
            kind,
            lines_added,
            lines_removed,
        } => {
            let kind_str = serde_json::to_value(kind)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "modified".to_string());
            log.info(&format!(
                "file_{}: {} (+{}/-{})",
                kind_str, path, lines_added, lines_removed
            ));
        }
        AppEvent::SnapshotCreated { round_id } => {
            log.snapshot_created(*round_id);
        }
        AppEvent::RolledBack {
            from_id,
            to_id,
            files_reverted,
        } => {
            log.rolled_back(*from_id, *to_id, *files_reverted);
        }
        AppEvent::Redone { to_id } => {
            log.redone(*to_id);
        }
        AppEvent::HistoryPruned {
            branches_removed,
            bytes_freed,
        } => {
            log.history_pruned(*branches_removed, *bytes_freed);
        }
        AppEvent::ConversationRolledBack {
            session_id: _,
            round_id,
            turns_removed,
            backend,
            method,
        } => {
            log.conversation_rolled_back(*round_id, *turns_removed, backend, method);
        }
        AppEvent::ConversationRollbackRequested { .. } => {
            // Input event — the agent loop logs the completion
            // (ConversationRolledBack). No point logging the request.
        }

        // ---- Events already logged inline by the agent loop or web_gateway ----
        // turn_start, model_response, agent_input/output, approval decisions,
        // json_extracted, reasoning, budget warnings, loop errors, context
        // management, voice_log, voice_diagnostic, presence_connected/disconnected,
        // presence_checkpoint, user_transcript — all have slog() calls at their
        // point of origin.
        //
        // ---- Terminal-only / internal / high-frequency events ----
        // Tick, ControlCommand, SessionDirChanged,
        // ModelResponseDelta (too chatty), StatusUpdate (every tick),
        // UsageSnapshot (periodic, mainly for UI), LogEntry (meta/circular).
        _ => {}
    }
}

/// Spawns a tick timer that sends Tick events at a regular interval.
pub fn spawn_tick_timer(bus: EventBus, interval_ms: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
        loop {
            interval.tick().await;
            bus.send(AppEvent::Tick);
        }
    })
}

/// Spawns a file monitor for askHuman question files.
/// The `question_path` is the session-scoped path to the human_question file.
/// Shared path that can be updated when MCP tasks change session directories.
pub type SharedQuestionPath = std::sync::Arc<tokio::sync::RwLock<std::path::PathBuf>>;

pub fn shared_question_path(path: std::path::PathBuf) -> SharedQuestionPath {
    std::sync::Arc::new(tokio::sync::RwLock::new(path))
}

pub fn spawn_human_question_monitor(
    bus: EventBus,
    question_path: SharedQuestionPath,
) -> tokio::task::JoinHandle<()> {
    use tokio::time::{interval, Duration};

    tokio::spawn(async move {
        let mut interval = interval(Duration::from_millis(250));
        let mut last_seen = false;

        loop {
            interval.tick().await;

            let path = question_path.read().await.clone();
            if path.exists() {
                if !last_seen {
                    if let Ok(question) = tokio::fs::read_to_string(&path).await {
                        let question = question.trim().to_string();
                        if !question.is_empty() {
                            bus.send(AppEvent::HumanQuestionDetected { question });
                        }
                    }
                    last_seen = true;
                }
            } else {
                if last_seen {
                    bus.send(AppEvent::HumanResponseSent);
                    last_seen = false;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bus_send_receive() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            bus.send(AppEvent::Tick);
            bus.send(AppEvent::PresenceReady);

            match rx.recv().await.unwrap() {
                AppEvent::Tick => {}
                _ => panic!("expected Tick"),
            }
            match rx.recv().await.unwrap() {
                AppEvent::PresenceReady => {}
                _ => panic!("expected PresenceReady"),
            }
        });
    }

    #[test]
    fn event_bus_clone() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let bus2 = bus.clone();
            bus.send(AppEvent::Tick);
            bus2.send(AppEvent::PresenceReady);

            match rx.recv().await.unwrap() {
                AppEvent::Tick => {}
                _ => panic!("expected Tick"),
            }
            match rx.recv().await.unwrap() {
                AppEvent::PresenceReady => {}
                _ => panic!("expected PresenceReady"),
            }
        });
    }

    /// B1: user intents must survive a delta flood that laps the broadcast
    /// ring. The intent lane is fed at the emit point, so every
    /// `ControlCommand` arrives, in order, even though a plain broadcast
    /// subscriber provably lagged over the same window.
    #[tokio::test]
    async fn intent_lane_is_lossless_and_ordered_under_delta_flood() {
        let bus = EventBus::new();
        let mut intents = bus.subscribe_intents();
        let mut lossy = bus.subscribe();

        const FLOOD: usize = 10_000;
        const INTENTS: usize = 100;
        let deltas_per_intent = FLOOD / INTENTS;
        for batch in 0..INTENTS {
            for _ in 0..deltas_per_intent {
                bus.send(AppEvent::ModelResponseDelta {
                    session_id: Some("flood".to_string()),
                    text: "x".to_string(),
                });
            }
            bus.send(AppEvent::ControlCommand(ControlMsg::SetAutonomy {
                level: format!("level-{batch}"),
            }));
        }

        for expected in 0..INTENTS {
            let event = intents.recv().await.expect("intent lane closed early");
            match event {
                AppEvent::ControlCommand(ControlMsg::SetAutonomy { level }) => {
                    assert_eq!(level, format!("level-{expected}"), "intents reordered");
                }
                other => panic!("unexpected event on intent lane: {other:?}"),
            }
        }
        assert!(
            intents.try_recv().is_err(),
            "intent lane must carry exactly the sent intents"
        );

        // The same window overflowed the plain broadcast ring (10_100 sends
        // against a 4096-slot buffer with no interleaved recv), so the lossy
        // subscriber's first recv reports the lag the intent lane is immune to.
        match lossy.recv().await {
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                assert!(skipped > 0);
            }
            other => panic!("expected broadcast lag, got {other:?}"),
        }
    }

    /// B1: the intent lane carries the session-bookkeeping events its
    /// consumers act on, interleaved in emission order with the commands.
    #[tokio::test]
    async fn intent_lane_orders_bookkeeping_with_commands() {
        let bus = EventBus::new();
        let mut intents = bus.subscribe_intents();
        bus.send(AppEvent::ControlCommand(ControlMsg::SetAutonomy {
            level: "high".to_string(),
        }));
        bus.send(AppEvent::SessionEnded {
            session_id: "s1".to_string(),
            reason: "done".to_string(),
            error_kind: None,
        });
        // Not on the lane: high-frequency stream events.
        bus.send(AppEvent::ModelResponseDelta {
            session_id: None,
            text: "ignored".to_string(),
        });
        bus.send(AppEvent::SessionIdentity {
            session_id: "s2".to_string(),
            source: "codex".to_string(),
            backend_session_id: "thread-1".to_string(),
        });

        assert!(matches!(
            intents.recv().await,
            Some(AppEvent::ControlCommand(ControlMsg::SetAutonomy { .. }))
        ));
        assert!(matches!(
            intents.recv().await,
            Some(AppEvent::SessionEnded { .. })
        ));
        assert!(matches!(
            intents.recv().await,
            Some(AppEvent::SessionIdentity { .. })
        ));
        assert!(intents.try_recv().is_err());
    }

    #[tokio::test]
    async fn intent_lane_orders_recording_lifecycle_with_controls() {
        let bus = EventBus::new();
        let mut intents = bus.subscribe_intents();
        bus.send(AppEvent::DisplayReady {
            display_id: 7,
            width: 1280,
            height: 720,
            agent_visible: true,
        });
        bus.send(AppEvent::ControlCommand(ControlMsg::StopRecording {
            stream_name: "display_7".to_string(),
        }));
        bus.send(AppEvent::TaskComplete {
            session_id: Some("s1".to_string()),
            reason: "done".to_string(),
            summary: None,
        });

        assert!(matches!(
            intents.recv().await,
            Some(AppEvent::DisplayReady { display_id: 7, .. })
        ));
        assert!(matches!(
            intents.recv().await,
            Some(AppEvent::ControlCommand(ControlMsg::StopRecording { .. }))
        ));
        assert!(matches!(
            intents.recv().await,
            Some(AppEvent::TaskComplete { .. })
        ));
        assert!(intents.try_recv().is_err());
    }

    /// B5, deterministic core: the coalesce buffer concatenates per
    /// session (insertion-ordered across sessions) and one flush emits one
    /// wire line per session with the payload shape unchanged.
    #[test]
    fn delta_coalesce_buffer_merges_per_session_in_order() {
        let (outbound_tx, mut outbound_rx) = tokio::sync::broadcast::channel::<String>(16);
        let mut buffer = DeltaCoalesceBuffer::default();
        let s1 = Some("s1".to_string());
        let s2 = Some("s2".to_string());
        buffer.push(&s1, "a");
        buffer.push(&s2, "z");
        buffer.push(&s1, "b");
        buffer.push(&s1, "c");
        buffer.flush(&outbound_tx);

        let first: serde_json::Value =
            serde_json::from_str(&outbound_rx.try_recv().unwrap()).unwrap();
        assert_eq!(first["event"], "model_response_delta");
        assert_eq!(first["session_id"], "s1");
        assert_eq!(
            first["text"], "abc",
            "per-session text concatenates in order"
        );
        let second: serde_json::Value =
            serde_json::from_str(&outbound_rx.try_recv().unwrap()).unwrap();
        assert_eq!(second["session_id"], "s2");
        assert_eq!(second["text"], "z");
        assert!(
            outbound_rx.try_recv().is_err(),
            "one line per session per flush"
        );

        // Flushing again emits nothing (buffer drained).
        buffer.flush(&outbound_tx);
        assert!(outbound_rx.try_recv().is_err());
    }

    /// B5: no delta may be reordered past a later non-delta event, and no
    /// delta text may be lost, regardless of where the 40ms tick lands.
    #[tokio::test]
    async fn outbound_broadcaster_flushes_deltas_before_later_events() {
        let bus = EventBus::new();
        let (outbound_tx, mut outbound_rx) = tokio::sync::broadcast::channel::<String>(64);
        let _handle = spawn_outbound_broadcaster(bus.subscribe(), outbound_tx);

        for text in ["a", "b", "c"] {
            bus.send(AppEvent::ModelResponseDelta {
                session_id: Some("s1".to_string()),
                text: text.to_string(),
            });
        }
        bus.send(AppEvent::ModelResponseDelta {
            session_id: Some("s2".to_string()),
            text: "z".to_string(),
        });
        // Non-delta event: every delta sent above must flush before it.
        bus.send(AppEvent::DoneSignal {
            session_id: Some("s1".to_string()),
            message: None,
        });

        // A tick may split the merged run on a loaded machine, so assert
        // the invariants rather than one exact framing: concatenated text
        // per session is exact and complete BEFORE the done event arrives.
        let mut s1_text = String::new();
        let mut s2_text = String::new();
        loop {
            let line = tokio::time::timeout(std::time::Duration::from_secs(10), outbound_rx.recv())
                .await
                .expect("timed out waiting for outbound line")
                .unwrap();
            let json: serde_json::Value = serde_json::from_str(&line).unwrap();
            match json["event"].as_str() {
                Some("model_response_delta") => {
                    let text = json["text"].as_str().unwrap();
                    match json["session_id"].as_str() {
                        Some("s1") => s1_text.push_str(text),
                        Some("s2") => s2_text.push_str(text),
                        other => panic!("unexpected delta session {other:?}"),
                    }
                }
                Some("done_signal") => break,
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(s1_text, "abc", "all s1 delta text flushed before done");
        assert_eq!(s2_text, "z", "all s2 delta text flushed before done");
    }

    /// B5: deltas buffered AFTER a non-delta event flush after it — the
    /// barrier works in both directions across a flush boundary.
    #[tokio::test]
    async fn outbound_broadcaster_delta_after_event_stays_after_it() {
        let bus = EventBus::new();
        let (outbound_tx, mut outbound_rx) = tokio::sync::broadcast::channel::<String>(64);
        let _handle = spawn_outbound_broadcaster(bus.subscribe(), outbound_tx);

        bus.send(AppEvent::DoneSignal {
            session_id: Some("s1".to_string()),
            message: None,
        });
        bus.send(AppEvent::ModelResponseDelta {
            session_id: Some("s1".to_string()),
            text: "after".to_string(),
        });

        let first = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
            .await
            .expect("timed out waiting for done signal")
            .unwrap();
        assert!(first.contains("\"event\":\"done_signal\""));
        // The trailing delta arrives on the 40ms tick.
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
            .await
            .expect("timed out waiting for tick-flushed delta")
            .unwrap();
        let second_json: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(second_json["event"], "model_response_delta");
        assert_eq!(second_json["text"], "after");
    }

    #[test]
    fn outbound_agent_started_preserves_item_id() {
        let event = AppEvent::AgentStarted {
            session_id: Some("session-1".to_string()),
            turn: 3,
            commands_preview: "exec: pwd".to_string(),
            item_id: Some("call-abc".to_string()),
            source: Some("Codex".to_string()),
        };

        match app_event_to_outbound(&event).unwrap() {
            crate::types::OutboundEvent::AgentStarted {
                session_id,
                turn,
                commands_preview,
                item_id,
                source,
            } => {
                assert_eq!(session_id.as_deref(), Some("session-1"));
                assert_eq!(turn, 3);
                assert_eq!(commands_preview, "exec: pwd");
                assert_eq!(item_id.as_deref(), Some("call-abc"));
                assert_eq!(source.as_deref(), Some("Codex"));
            }
            other => panic!("expected AgentStarted, got {:?}", other),
        }
    }

    #[test]
    fn control_msg_status_deserialize() {
        let json = r#"{"action":"status"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Status { session_id } => assert!(session_id.is_none()),
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn control_msg_approve_deserialize() {
        let json = r#"{"action":"approve","id":42}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Approve { session_id, id } => {
                assert!(session_id.is_none());
                assert_eq!(id, 42);
            }
            _ => panic!("expected Approve"),
        }
    }

    #[test]
    fn control_msg_thread_action_accepts_universal_and_legacy_names() {
        // `thread_action` is the canonical backend-neutral name; the
        // Codex-flavored legacy tag must keep deserializing for old
        // frontends and replayed control logs.
        for action in ["thread_action", "codex_thread_action"] {
            let json = format!(r#"{{"action":"{action}","op":"compact","session_id":"sess-1"}}"#);
            let msg: ControlMsg = serde_json::from_str(&json).unwrap();
            match msg {
                ControlMsg::CodexThreadAction { session_id, op, .. } => {
                    assert_eq!(session_id.as_deref(), Some("sess-1"));
                    assert_eq!(op, "compact");
                }
                other => panic!("expected thread action for tag {action}, got {other:?}"),
            }
        }
    }

    #[test]
    fn control_msg_resume_session_fork_defaults_false() {
        let json = r#"{"action":"resume_session","source":"claude-code","session_id":"abc"}"#;
        match serde_json::from_str::<ControlMsg>(json).unwrap() {
            ControlMsg::ResumeSession { fork, .. } => assert!(!fork),
            other => panic!("expected ResumeSession, got {other:?}"),
        }
        let json =
            r#"{"action":"resume_session","source":"claude-code","session_id":"abc","fork":true}"#;
        match serde_json::from_str::<ControlMsg>(json).unwrap() {
            ControlMsg::ResumeSession { fork, .. } => assert!(fork),
            other => panic!("expected ResumeSession, got {other:?}"),
        }
    }

    #[test]
    fn control_msg_deny_deserialize() {
        let json = r#"{"action":"deny","id":7}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Deny { session_id, id } => {
                assert!(session_id.is_none());
                assert_eq!(id, 7);
            }
            _ => panic!("expected Deny"),
        }
    }

    #[test]
    fn control_msg_answer_question_deserialize() {
        let json = r#"{"action":"answer_question","id":3,"session_id":"sess-1","answers":{"Which DB?":"PostgreSQL"}}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::AnswerQuestion {
                session_id,
                id,
                answers,
            } => {
                assert_eq!(session_id.as_deref(), Some("sess-1"));
                assert_eq!(id, 3);
                assert_eq!(answers["Which DB?"], "PostgreSQL");
            }
            _ => panic!("expected AnswerQuestion"),
        }
    }

    #[test]
    fn user_question_required_maps_to_outbound_user_question() {
        let event = AppEvent::UserQuestionRequired {
            session_id: Some("sess-1".into()),
            id: 5,
            questions: vec![crate::types::UserQuestion {
                question: "Which DB?".into(),
                header: "Database".into(),
                options: vec![crate::types::UserQuestionOption {
                    label: "PostgreSQL".into(),
                    description: "Relational".into(),
                }],
                multi_select: false,
            }],
        };
        match app_event_to_outbound(&event).unwrap() {
            crate::types::OutboundEvent::UserQuestion {
                session_id,
                id,
                questions,
            } => {
                assert_eq!(session_id.as_deref(), Some("sess-1"));
                assert_eq!(id, 5);
                assert_eq!(questions.len(), 1);
                // Wire shape the dashboard renders from.
                let json = serde_json::to_value(&questions[0]).unwrap();
                assert_eq!(json["question"], "Which DB?");
                assert_eq!(json["header"], "Database");
                assert_eq!(json["options"][0]["label"], "PostgreSQL");
            }
            other => panic!("expected UserQuestion, got {:?}", other),
        }
    }

    #[test]
    fn control_msg_input_deserialize() {
        let json = r#"{"action":"input","text":"PostgreSQL"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Input { text } => assert_eq!(text, "PostgreSQL"),
            _ => panic!("expected Input"),
        }
    }

    #[test]
    fn control_msg_set_autonomy_deserialize() {
        let json = r#"{"action":"set_autonomy","level":"high"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetAutonomy { level } => assert_eq!(level, "high"),
            _ => panic!("expected SetAutonomy"),
        }
    }

    #[test]
    fn control_msg_set_approval_rule_deserialize() {
        let json = r#"{"action":"set_approval_rule","category":"tool_call","rule":"auto"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetApprovalRule { category, rule } => {
                assert_eq!(category, "tool_call");
                assert_eq!(rule, "auto");
            }
            _ => panic!("expected SetApprovalRule"),
        }
    }

    #[test]
    fn control_msg_set_external_agent_deserialize() {
        let json = r#"{"action":"set_external_agent","agent":"codex"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetExternalAgent { agent } => assert_eq!(agent, Some("codex".to_string())),
            _ => panic!("expected SetExternalAgent"),
        }
    }

    #[test]
    fn control_msg_set_external_agent_null_deserialize() {
        let json = r#"{"action":"set_external_agent","agent":null}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetExternalAgent { agent } => assert_eq!(agent, None),
            _ => panic!("expected SetExternalAgent"),
        }
    }

    #[test]
    fn control_msg_quit_deserialize() {
        let json = r#"{"action":"quit"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Quit => {}
            _ => panic!("expected Quit"),
        }
    }

    #[test]
    fn control_msg_schedule_restart_deserialize() {
        let json = r#"{"action":"schedule_controller_restart","controller_id":"codex","north_star_goal":"audit and improve","restart_after":"turn_end"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ScheduleControllerRestart {
                controller_id,
                north_star_goal,
                restart_after,
                ..
            } => {
                assert_eq!(controller_id, "codex");
                assert_eq!(north_star_goal, "audit and improve");
                assert_eq!(restart_after.as_deref(), Some("turn_end"));
            }
            _ => panic!("expected ScheduleControllerRestart"),
        }
    }

    #[test]
    fn control_msg_controller_turn_complete_deserialize() {
        let json = r#"{"action":"controller_turn_complete","restart_id":"abc","turn_complete_token":"tok"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ControllerTurnComplete {
                restart_id,
                turn_complete_token,
                ..
            } => {
                assert_eq!(restart_id, "abc");
                assert_eq!(turn_complete_token, "tok");
            }
            _ => panic!("expected ControllerTurnComplete"),
        }
    }

    #[test]
    fn control_msg_controller_loop_variants_deserialize() {
        let halt: ControlMsg =
            serde_json::from_str(r#"{"action":"request_controller_loop_halt","persistent":false}"#)
                .unwrap();
        match halt {
            ControlMsg::RequestControllerLoopHalt { persistent } => {
                assert_eq!(persistent, Some(false));
            }
            _ => panic!("expected RequestControllerLoopHalt"),
        }

        let intervene: ControlMsg =
            serde_json::from_str(r#"{"action":"intervene_controller_loop","mode":"stop"}"#)
                .unwrap();
        match intervene {
            ControlMsg::InterveneControllerLoop { mode } => assert_eq!(mode, "stop"),
            _ => panic!("expected InterveneControllerLoop"),
        }
    }

    #[test]
    fn control_msg_serialize_roundtrip() {
        let msgs = vec![
            ControlMsg::Status { session_id: None },
            ControlMsg::Approve {
                session_id: None,
                id: 1,
            },
            ControlMsg::Deny {
                session_id: None,
                id: 2,
            },
            ControlMsg::Input {
                text: "hello".to_string(),
            },
            ControlMsg::Skip {
                session_id: None,
                id: 3,
            },
            ControlMsg::ApproveAll {
                session_id: None,
                id: 4,
            },
            ControlMsg::SetAutonomy {
                level: "low".to_string(),
            },
            ControlMsg::SetExternalAgent {
                agent: Some("codex".to_string()),
            },
            ControlMsg::SetVerbosity {
                level: "verbose".to_string(),
            },
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: None,
                auto_start_task: Some(true),
                max_attempts: Some(1),
                cooldown_sec: Some(30),
            },
            ControlMsg::ControllerTurnComplete {
                restart_id: "id".to_string(),
                turn_complete_token: "token".to_string(),
                status: None,
                handoff_summary: None,
            },
            ControlMsg::GetRestartStatus,
            ControlMsg::CancelControllerRestart { restart_id: None },
            ControlMsg::RequestControllerLoopHalt {
                persistent: Some(true),
            },
            ControlMsg::ClearControllerLoopHalt,
            ControlMsg::InterveneControllerLoop {
                mode: "stop".to_string(),
            },
            ControlMsg::GetControllerLoopStatus,
            ControlMsg::CreateSession {
                task: "start fresh".to_string(),
                name: Some("Fresh start".to_string()),
                project_root: None,
                agent: Some("codex".to_string()),
                agent_command: Some("/opt/codex/bin/codex".to_string()),
                claude_model: None,
                claude_effort: None,
                claude_permission_mode: None,
                codex_model: Some("gpt-5.3-codex".to_string()),
                codex_reasoning_effort: Some("high".to_string()),
                codex_sandbox: Some("danger-full-access".to_string()),
                codex_approval_policy: Some("never".to_string()),
                codex_managed_context: Some("managed".to_string()),
                codex_context_archive: Some("summary".to_string()),
                codex_service_tier: Some("priority".to_string()),
                orchestrate: Some(false),
                direct: Some(true),
                reference_frame_ids: vec!["display_99-f00001".to_string()],
                display_target: Some("user_session".to_string()),
                attachments: vec!["upload:u1".to_string()],
                worktree: Some(true),
                worktree_branch: Some("feature-branch".to_string()),
            },
            ControlMsg::StartTask {
                session_id: None,
                task: "fix bug".to_string(),
                orchestrate: None,
                direct: None,
                reference_frame_ids: vec![],
                display_target: None,
                attachments: vec![],
                follow_up_id: None,
                delegation_id: None,
            },
            ControlMsg::FollowUp {
                session_id: None,
                text: "continue working".to_string(),
                direct: None,
                follow_up_id: None,
            },
            ControlMsg::QueryDetail {
                scope: "diff".to_string(),
                target: None,
            },
            ControlMsg::RecallMemory {
                keywords: Some(vec!["auth".to_string()]),
                tags: None,
                channel: Some("project_state".to_string()),
            },
            ControlMsg::TakeDisplay { display_id: 99 },
            ControlMsg::ReleaseDisplay {
                display_id: 99,
                note: Some("done testing".to_string()),
            },
            ControlMsg::GrantUserDisplay {
                display_id: None,
                agent_visible: None,
            },
            ControlMsg::RevokeUserDisplay {
                display_id: None,
                note: Some("done with user display".to_string()),
            },
            ControlMsg::CreateBrowserWorkspace {
                url: Some("http://localhost:8765".to_string()),
                label: Some("debug dashboard".to_string()),
                provider: Some("cdp".to_string()),
                peer_id: None,
                owner_session_id: Some("session-1".to_string()),
                profile_dir: None,
            },
            ControlMsg::CloseBrowserWorkspace {
                workspace_id: "bw-test".to_string(),
                reason: Some("done".to_string()),
            },
            ControlMsg::AcquireBrowserWorkspace {
                workspace_id: "bw-test".to_string(),
                holder_id: "agent-a".to_string(),
                holder_kind: Some("agent".to_string()),
                note: Some("visual check".to_string()),
                force: false,
            },
            ControlMsg::ReleaseBrowserWorkspace {
                workspace_id: "bw-test".to_string(),
                holder_id: Some("agent-a".to_string()),
                note: None,
            },
            ControlMsg::InvokeSkill {
                skill_name: "deploy".to_string(),
                arguments: Some("staging".to_string()),
            },
            ControlMsg::Usage,
            ControlMsg::Quit,
            ControlMsg::Interrupt {
                session_id: None,
                expected_turn: None,
            },
            ControlMsg::Steer {
                session_id: None,
                text: "use Python".to_string(),
                attachments: vec![],
                id: Some("s-1".to_string()),
            },
            ControlMsg::Steer {
                session_id: None,
                text: "never mind".to_string(),
                attachments: vec![],
                id: None,
            },
            ControlMsg::CancelSteer {
                session_id: Some("session-1".to_string()),
                id: Some("s-1".to_string()),
                reason: Some("cleared by user".to_string()),
            },
            ControlMsg::CancelSteer {
                session_id: None,
                id: None,
                reason: None,
            },
        ];
        for msg in msgs {
            let json = serde_json::to_string(&msg).unwrap();
            let _: ControlMsg = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn control_msg_steer_deserialize() {
        // Wire shape: `{"action":"steer","text":"...","id":"..."}`.
        // `id` is optional — frontends may omit it, in which case the
        // dispatcher will treat it as "no correlation".
        let json = r#"{"action":"steer","text":"switch to Python","id":"s-42"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Steer {
                session_id,
                text,
                attachments,
                id,
            } => {
                assert!(session_id.is_none());
                assert_eq!(text, "switch to Python");
                assert!(attachments.is_empty());
                assert_eq!(id.as_deref(), Some("s-42"));
            }
            _ => panic!("expected Steer"),
        }
    }

    #[test]
    fn control_msg_steer_without_id_deserialize() {
        let json = r#"{"action":"steer","text":"never mind"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Steer {
                session_id,
                text,
                attachments,
                id,
            } => {
                assert!(session_id.is_none());
                assert_eq!(text, "never mind");
                assert!(attachments.is_empty());
                assert_eq!(id, None);
            }
            _ => panic!("expected Steer"),
        }
    }

    #[test]
    fn control_msg_steer_with_attachments_deserialize() {
        let json =
            r#"{"action":"steer","text":"look here","attachments":["frame:latest","upload:u1"]}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Steer {
                session_id,
                text,
                attachments,
                id,
            } => {
                assert!(session_id.is_none());
                assert_eq!(text, "look here");
                assert_eq!(attachments, vec!["frame:latest", "upload:u1"]);
                assert_eq!(id, None);
            }
            _ => panic!("expected Steer"),
        }
    }

    #[test]
    fn control_msg_cancel_steer_deserialize() {
        let json = r#"{"action":"cancel_steer","session_id":"session-1","id":"s-42","reason":"cleared by user"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::CancelSteer {
                session_id,
                id,
                reason,
            } => {
                assert_eq!(session_id.as_deref(), Some("session-1"));
                assert_eq!(id.as_deref(), Some("s-42"));
                assert_eq!(reason.as_deref(), Some("cleared by user"));
            }
            _ => panic!("expected CancelSteer"),
        }
    }

    #[test]
    fn control_msg_usage_deserialize() {
        let json = r#"{"action":"usage"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ControlMsg::Usage));
    }

    #[test]
    fn control_msg_skip_deserialize() {
        let json = r#"{"action":"skip","id":5}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Skip { session_id, id } => {
                assert!(session_id.is_none());
                assert_eq!(id, 5);
            }
            _ => panic!("expected Skip"),
        }
    }

    #[test]
    fn control_msg_approve_all_deserialize() {
        let json = r#"{"action":"approve_all","id":10}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ApproveAll { session_id, id } => {
                assert!(session_id.is_none());
                assert_eq!(id, 10);
            }
            _ => panic!("expected ApproveAll"),
        }
    }

    #[test]
    fn control_msg_set_verbosity_deserialize() {
        let json = r#"{"action":"set_verbosity","level":"verbose"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetVerbosity { level } => assert_eq!(level, "verbose"),
            _ => panic!("expected SetVerbosity"),
        }
    }

    #[test]
    fn control_msg_start_task_deserialize() {
        let json = r#"{"action":"start_task","task":"fix bug"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::StartTask {
                task,
                orchestrate,
                reference_frame_ids,
                display_target,
                ..
            } => {
                assert_eq!(task, "fix bug");
                assert!(orchestrate.is_none());
                assert!(reference_frame_ids.is_empty());
                assert!(display_target.is_none());
            }
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn control_msg_create_session_deserialize() {
        let json = r#"{"action":"create_session","task":"fix bug","name":"Bugfix work","project_root":"/repo","agent":"codex","agent_command":"/opt/codex/bin/codex","codex_model":"gpt-5.6-sol","codex_reasoning_effort":"ultra","codex_sandbox":"danger-full-access","codex_approval_policy":"never","codex_managed_context":"managed","codex_context_archive":"exact","codex_service_tier":"priority","direct":true,"attachments":["upload:u1"]}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::CreateSession {
                task,
                name,
                project_root,
                agent,
                agent_command,
                claude_model,
                claude_permission_mode,
                claude_effort,
                codex_model,
                codex_reasoning_effort,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
                codex_service_tier,
                orchestrate,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
                worktree,
                worktree_branch,
            } => {
                assert_eq!(task, "fix bug");
                assert!(claude_model.is_none());
                assert!(claude_permission_mode.is_none());
                assert!(claude_effort.is_none());
                assert_eq!(name.as_deref(), Some("Bugfix work"));
                assert_eq!(project_root.as_deref(), Some("/repo"));
                assert_eq!(agent.as_deref(), Some("codex"));
                assert_eq!(agent_command.as_deref(), Some("/opt/codex/bin/codex"));
                assert_eq!(codex_model.as_deref(), Some("gpt-5.6-sol"));
                assert_eq!(codex_reasoning_effort.as_deref(), Some("ultra"));
                assert_eq!(codex_sandbox.as_deref(), Some("danger-full-access"));
                assert_eq!(codex_approval_policy.as_deref(), Some("never"));
                assert_eq!(codex_managed_context.as_deref(), Some("managed"));
                assert_eq!(codex_context_archive.as_deref(), Some("exact"));
                assert_eq!(codex_service_tier.as_deref(), Some("priority"));
                assert!(orchestrate.is_none());
                assert_eq!(direct, Some(true));
                assert!(reference_frame_ids.is_empty());
                assert!(display_target.is_none());
                assert_eq!(attachments, vec!["upload:u1"]);
                // Legacy payloads without the worktree fields parse as "off".
                assert!(worktree.is_none());
                assert!(worktree_branch.is_none());
            }
            _ => panic!("expected CreateSession"),
        }
    }

    #[test]
    fn control_msg_create_session_worktree_deserialize() {
        let json = r#"{"action":"create_session","task":"fix bug","worktree":true,"worktree_branch":"fix/login"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::CreateSession {
                worktree,
                worktree_branch,
                ..
            } => {
                assert_eq!(worktree, Some(true));
                assert_eq!(worktree_branch.as_deref(), Some("fix/login"));
            }
            _ => panic!("expected CreateSession"),
        }
    }

    #[test]
    fn control_msg_spawn_sub_agent_deserialize() {
        let json = r#"{"action":"spawn_sub_agent","session_id":"parent-1","task":"survey the schema","name":"schema-scout","role":"research","agent":"internal","worktree":true}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SpawnSubAgent {
                session_id,
                task,
                name,
                role,
                agent,
                worktree,
            } => {
                assert_eq!(session_id, "parent-1");
                assert_eq!(task, "survey the schema");
                assert_eq!(name.as_deref(), Some("schema-scout"));
                assert_eq!(role.as_deref(), Some("research"));
                assert_eq!(agent.as_deref(), Some("internal"));
                assert_eq!(worktree, Some(true));
            }
            _ => panic!("expected SpawnSubAgent"),
        }

        // Minimal form: everything but the parent and the task defaults.
        let json = r#"{"action":"spawn_sub_agent","session_id":"parent-1","task":"t"}"#;
        match serde_json::from_str(json).unwrap() {
            ControlMsg::SpawnSubAgent {
                name,
                role,
                agent,
                worktree,
                ..
            } => {
                assert!(name.is_none());
                assert!(role.is_none());
                assert!(agent.is_none());
                assert!(worktree.is_none());
            }
            _ => panic!("expected SpawnSubAgent"),
        }
    }

    #[test]
    fn control_msg_rename_session_deserialize() {
        let json = r#"{"action":"rename_session","session_id":"abc123","backend_session_id":"codex-thread-1","source":"codex","name":"UI polish"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RenameSession {
                session_id,
                backend_session_id,
                source,
                name,
            } => {
                assert_eq!(session_id, "abc123");
                assert_eq!(backend_session_id.as_deref(), Some("codex-thread-1"));
                assert_eq!(source.as_deref(), Some("codex"));
                assert_eq!(name, "UI polish");
            }
            _ => panic!("expected RenameSession"),
        }
    }

    #[test]
    fn control_msg_configure_session_agent_deserializes() {
        let json = r#"{"action":"configure_session_agent","session_id":"abc123","source":"codex","backend_session_id":"thread-1","intendant_session_id":"wrap-1","agent_command":"/tmp/codex","codex_sandbox":"danger-full-access","codex_approval_policy":"never","codex_managed_context":"managed","codex_context_archive":"off"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ConfigureSessionAgent {
                session_id,
                source,
                backend_session_id,
                intendant_session_id,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
                claude_model,
                claude_permission_mode,
                claude_allowed_tools,
                claude_effort,
            } => {
                assert_eq!(session_id, "abc123");
                assert_eq!(source.as_deref(), Some("codex"));
                assert_eq!(backend_session_id.as_deref(), Some("thread-1"));
                assert_eq!(intendant_session_id.as_deref(), Some("wrap-1"));
                assert_eq!(agent_command.as_deref(), Some("/tmp/codex"));
                assert_eq!(codex_sandbox.as_deref(), Some("danger-full-access"));
                assert_eq!(codex_approval_policy.as_deref(), Some("never"));
                assert_eq!(codex_managed_context.as_deref(), Some("managed"));
                assert_eq!(codex_context_archive.as_deref(), Some("off"));
                assert!(claude_model.is_none());
                assert!(claude_permission_mode.is_none());
                assert!(claude_allowed_tools.is_none());
                assert!(claude_effort.is_none());
            }
            _ => panic!("expected ConfigureSessionAgent"),
        }

        let json = r#"{"action":"configure_session_agent","session_id":"cc-1","source":"claude-code","claude_model":"opus","claude_permission_mode":"acceptEdits","claude_allowed_tools":"Read,Bash(cargo test *)","claude_effort":"xhigh"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ConfigureSessionAgent {
                claude_model,
                claude_permission_mode,
                claude_allowed_tools,
                claude_effort,
                ..
            } => {
                assert_eq!(claude_model.as_deref(), Some("opus"));
                assert_eq!(claude_permission_mode.as_deref(), Some("acceptEdits"));
                assert_eq!(
                    claude_allowed_tools.as_deref(),
                    Some("Read,Bash(cargo test *)")
                );
                assert_eq!(claude_effort.as_deref(), Some("xhigh"));
            }
            _ => panic!("expected ConfigureSessionAgent"),
        }
    }

    #[test]
    fn control_msg_resume_session_deserializes_launch_overrides() {
        let json = r#"{"action":"resume_session","source":"codex","session_id":"thread-1","resume_id":"thread-1","project_root":"/repo","task":"continue","direct":true,"attachments":["upload:u1"],"agent_command":"/tmp/codex","codex_sandbox":"danger-full-access","codex_approval_policy":"never","codex_managed_context":"managed","codex_context_archive":"summary"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ResumeSession {
                source,
                session_id,
                resume_id,
                relationship_kind,
                project_root,
                task,
                direct,
                attachments,
                fork,
                auto_attach,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
            } => {
                assert_eq!(source, "codex");
                assert!(!fork);
                assert!(!auto_attach, "absent auto_attach must default to false");
                assert_eq!(session_id, "thread-1");
                assert_eq!(resume_id.as_deref(), Some("thread-1"));
                assert_eq!(project_root.as_deref(), Some("/repo"));
                assert_eq!(task.as_deref(), Some("continue"));
                assert_eq!(direct, Some(true));
                assert_eq!(attachments, vec!["upload:u1"]);
                assert_eq!(agent_command.as_deref(), Some("/tmp/codex"));
                assert_eq!(codex_sandbox.as_deref(), Some("danger-full-access"));
                assert_eq!(codex_approval_policy.as_deref(), Some("never"));
                assert_eq!(codex_managed_context.as_deref(), Some("managed"));
                assert_eq!(codex_context_archive.as_deref(), Some("summary"));
                assert_eq!(relationship_kind, None, "absent field defaults to None");
            }
            _ => panic!("expected ResumeSession"),
        }
    }

    #[test]
    fn control_msg_stop_session_deserializes() {
        let json = r#"{"action":"stop_session","session_id":"thread-1"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::StopSession { session_id } => {
                assert_eq!(session_id, "thread-1");
            }
            _ => panic!("expected StopSession"),
        }
    }

    #[test]
    fn control_msg_restart_session_deserializes_launch_overrides() {
        let json = r#"{"action":"restart_session","source":"codex","session_id":"thread-1","resume_id":"thread-1","project_root":"/repo","direct":true,"agent_command":"/tmp/codex","codex_sandbox":"danger-full-access","codex_approval_policy":"never","codex_managed_context":"managed","codex_context_archive":"exact"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RestartSession {
                source,
                session_id,
                resume_id,
                project_root,
                task,
                direct,
                attachments,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
                claude_model,
                claude_permission_mode,
                claude_allowed_tools,
                claude_effort,
            } => {
                assert_eq!(source, "codex");
                assert_eq!(session_id, "thread-1");
                assert_eq!(resume_id.as_deref(), Some("thread-1"));
                assert_eq!(project_root.as_deref(), Some("/repo"));
                assert!(task.is_none());
                assert_eq!(direct, Some(true));
                assert!(attachments.is_empty());
                assert_eq!(agent_command.as_deref(), Some("/tmp/codex"));
                assert_eq!(codex_sandbox.as_deref(), Some("danger-full-access"));
                assert_eq!(codex_approval_policy.as_deref(), Some("never"));
                assert_eq!(codex_managed_context.as_deref(), Some("managed"));
                assert_eq!(codex_context_archive.as_deref(), Some("exact"));
                assert!(claude_model.is_none());
                assert!(claude_permission_mode.is_none());
                assert!(claude_allowed_tools.is_none());
                assert!(claude_effort.is_none());
            }
            _ => panic!("expected RestartSession"),
        }
    }

    #[test]
    fn control_msg_edit_user_message_deserializes_resume_context() {
        let json = r#"{"action":"edit_user_message","session_id":"019e5c7a","source":"codex","resume_id":"019e5c7a","project_root":"/repo","direct":true,"user_turn_index":2,"user_turn_revision":1,"text":"replacement"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::EditUserMessage {
                session_id,
                source,
                resume_id,
                project_root,
                direct,
                user_turn_index,
                user_turn_revision,
                original_text,
                text,
                attachments,
            } => {
                assert_eq!(session_id.as_deref(), Some("019e5c7a"));
                assert_eq!(source.as_deref(), Some("codex"));
                assert_eq!(resume_id.as_deref(), Some("019e5c7a"));
                assert_eq!(project_root.as_deref(), Some("/repo"));
                assert_eq!(direct, Some(true));
                assert_eq!(user_turn_index, 2);
                assert_eq!(user_turn_revision, Some(1));
                assert!(original_text.is_none());
                assert_eq!(text, "replacement");
                assert!(attachments.is_empty());
            }
            _ => panic!("expected EditUserMessage"),
        }
    }

    #[test]
    fn control_msg_start_task_roundtrip() {
        let msg = ControlMsg::StartTask {
            session_id: None,
            task: "deploy app".to_string(),
            orchestrate: Some(true),
            direct: None,
            reference_frame_ids: vec!["display_99-f00001".to_string()],
            display_target: Some("user_session".to_string()),
            attachments: vec!["ann-recording-1".to_string(), "ann-recording-2".to_string()],
            follow_up_id: None,
            delegation_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ControlMsg = serde_json::from_str(&json).unwrap();
        match parsed {
            ControlMsg::StartTask {
                task,
                orchestrate,
                reference_frame_ids,
                display_target,
                attachments,
                ..
            } => {
                assert_eq!(task, "deploy app");
                assert_eq!(orchestrate, Some(true));
                assert_eq!(reference_frame_ids.len(), 1);
                assert_eq!(display_target.as_deref(), Some("user_session"));
                assert_eq!(attachments.len(), 2);
                assert_eq!(attachments[0], "ann-recording-1");
            }
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn control_msg_session_target_roundtrip() {
        let start_json = r#"{"action":"start_task","session_id":"sess-123","task":"continue"}"#;
        let start: ControlMsg = serde_json::from_str(start_json).unwrap();
        match start {
            ControlMsg::StartTask {
                session_id, task, ..
            } => {
                assert_eq!(session_id.as_deref(), Some("sess-123"));
                assert_eq!(task, "continue");
            }
            _ => panic!("expected StartTask"),
        }

        let follow = ControlMsg::FollowUp {
            session_id: Some("sess-123".to_string()),
            text: "more detail".to_string(),
            direct: Some(true),
            follow_up_id: Some("follow-1".to_string()),
        };
        let follow_json = serde_json::to_string(&follow).unwrap();
        let parsed: ControlMsg = serde_json::from_str(&follow_json).unwrap();
        match parsed {
            ControlMsg::FollowUp {
                session_id,
                text,
                direct,
                follow_up_id,
            } => {
                assert_eq!(session_id.as_deref(), Some("sess-123"));
                assert_eq!(text, "more detail");
                assert_eq!(direct, Some(true));
                assert_eq!(follow_up_id.as_deref(), Some("follow-1"));
            }
            _ => panic!("expected FollowUp"),
        }

        let cancel_json =
            r#"{"action":"cancel_follow_up","session_id":"sess-123","id":"follow-1"}"#;
        let cancel: ControlMsg = serde_json::from_str(cancel_json).unwrap();
        match cancel {
            ControlMsg::CancelFollowUp {
                session_id,
                id,
                reason,
            } => {
                assert_eq!(session_id.as_deref(), Some("sess-123"));
                assert_eq!(id.as_deref(), Some("follow-1"));
                assert_eq!(reason, None);
            }
            _ => panic!("expected CancelFollowUp"),
        }
    }

    #[test]
    fn control_msg_start_task_attachments_default_empty() {
        let json = r#"{"action":"start_task","task":"do thing"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::StartTask { attachments, .. } => {
                assert!(attachments.is_empty());
            }
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn control_msg_start_task_attachments_parse() {
        let json = r#"{"action":"start_task","task":"do thing","attachments":["a","b","c"]}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::StartTask { attachments, .. } => {
                assert_eq!(attachments.len(), 3);
                assert_eq!(attachments[1], "b");
            }
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn approval_response_variants() {
        assert_ne!(ApprovalResponse::Approve, ApprovalResponse::Deny);
        assert_ne!(ApprovalResponse::Skip, ApprovalResponse::ApproveAll);
        assert_eq!(ApprovalResponse::Approve, ApprovalResponse::Approve);
    }

    #[test]
    fn control_msg_query_detail_deserialize() {
        let json = r#"{"action":"query_detail","scope":"diff"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::QueryDetail { scope, target } => {
                assert_eq!(scope, "diff");
                assert!(target.is_none());
            }
            _ => panic!("expected QueryDetail"),
        }
    }

    #[test]
    fn control_msg_query_detail_with_target() {
        let json = r#"{"action":"query_detail","scope":"file","target":"src/main.rs"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::QueryDetail { scope, target } => {
                assert_eq!(scope, "file");
                assert_eq!(target.as_deref(), Some("src/main.rs"));
            }
            _ => panic!("expected QueryDetail"),
        }
    }

    #[test]
    fn control_msg_recall_memory_deserialize() {
        let json =
            r#"{"action":"recall_memory","keywords":["auth","login"],"channel":"project_state"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RecallMemory {
                keywords,
                tags,
                channel,
            } => {
                assert_eq!(
                    keywords,
                    Some(vec!["auth".to_string(), "login".to_string()])
                );
                assert!(tags.is_none());
                assert_eq!(channel.as_deref(), Some("project_state"));
            }
            _ => panic!("expected RecallMemory"),
        }
    }

    #[test]
    fn control_msg_recall_memory_minimal() {
        let json = r#"{"action":"recall_memory"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RecallMemory {
                keywords,
                tags,
                channel,
            } => {
                assert!(keywords.is_none());
                assert!(tags.is_none());
                assert!(channel.is_none());
            }
            _ => panic!("expected RecallMemory"),
        }
    }

    #[test]
    fn control_msg_invoke_skill_deserialize() {
        let json = r#"{"action":"invoke_skill","skill_name":"deploy","arguments":"staging"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::InvokeSkill {
                skill_name,
                arguments,
            } => {
                assert_eq!(skill_name, "deploy");
                assert_eq!(arguments, Some("staging".to_string()));
            }
            _ => panic!("expected InvokeSkill"),
        }
    }

    #[test]
    fn control_msg_invoke_skill_no_args() {
        let json = r#"{"action":"invoke_skill","skill_name":"lint"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::InvokeSkill {
                skill_name,
                arguments,
            } => {
                assert_eq!(skill_name, "lint");
                assert!(arguments.is_none());
            }
            _ => panic!("expected InvokeSkill"),
        }
    }

    #[test]
    fn control_msg_grant_user_display_deserialize() {
        // The legacy wire shape (no agent_visible) must keep parsing —
        // and keep meaning "share with the agent" (None → true at the
        // control plane).
        let json = r#"{"action":"grant_user_display"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            ControlMsg::GrantUserDisplay {
                display_id: None,
                agent_visible: None,
            }
        ));

        // With explicit display_id
        let json = r#"{"action":"grant_user_display","display_id":2}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            ControlMsg::GrantUserDisplay {
                display_id: Some(2),
                agent_visible: None,
            }
        ));

        // The dashboard's "View this machine" — a private view.
        let json = r#"{"action":"grant_user_display","display_id":2,"agent_visible":false}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            ControlMsg::GrantUserDisplay {
                display_id: Some(2),
                agent_visible: Some(false),
            }
        ));
    }

    #[test]
    fn control_msg_revoke_user_display_deserialize() {
        let json = r#"{"action":"revoke_user_display","note":"testing done"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RevokeUserDisplay { display_id, note } => {
                assert_eq!(display_id, None);
                assert_eq!(note.as_deref(), Some("testing done"));
            }
            _ => panic!("expected RevokeUserDisplay"),
        }
    }

    #[test]
    fn control_msg_create_virtual_display_deserialize() {
        let json = r#"{"action":"create_virtual_display"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            ControlMsg::CreateVirtualDisplay {
                width: None,
                height: None
            }
        ));

        let json = r#"{"action":"create_virtual_display","width":1280,"height":800}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            ControlMsg::CreateVirtualDisplay {
                width: Some(1280),
                height: Some(800)
            }
        ));
    }

    #[test]
    fn control_msg_revoke_user_display_no_note() {
        let json = r#"{"action":"revoke_user_display"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RevokeUserDisplay { display_id, note } => {
                assert_eq!(display_id, None);
                assert!(note.is_none());
            }
            _ => panic!("expected RevokeUserDisplay"),
        }
    }

    #[test]
    fn control_msg_revoke_user_display_with_id() {
        let json = r#"{"action":"revoke_user_display","display_id":3,"note":"done"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RevokeUserDisplay { display_id, note } => {
                assert_eq!(display_id, Some(3));
                assert_eq!(note.as_deref(), Some("done"));
            }
            _ => panic!("expected RevokeUserDisplay"),
        }
    }

    #[test]
    fn control_msg_resolve_display_request_deserialize() {
        // The popup's wire shape: approve with a duration.
        let json = r#"{"action":"resolve_display_request","session_id":"sess-1","id":7,"decision":"approve","duration":"15m"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ResolveDisplayRequest {
                session_id,
                id,
                decision,
                duration,
            } => {
                assert_eq!(session_id.as_deref(), Some("sess-1"));
                assert_eq!(id, 7);
                assert_eq!(decision, "approve");
                assert_eq!(duration.as_deref(), Some("15m"));
            }
            _ => panic!("expected ResolveDisplayRequest"),
        }

        // Deny needs no duration; session_id may be absent ("main").
        let json = r#"{"action":"resolve_display_request","id":3,"decision":"deny"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ResolveDisplayRequest {
                session_id,
                id,
                decision,
                duration,
            } => {
                assert_eq!(session_id, None);
                assert_eq!(id, 3);
                assert_eq!(decision, "deny");
                assert_eq!(duration, None);
            }
            _ => panic!("expected ResolveDisplayRequest"),
        }

        // `id` and `decision` are required: a resolution cannot default
        // its target or its verdict.
        assert!(serde_json::from_str::<ControlMsg>(
            r#"{"action":"resolve_display_request","decision":"approve"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<ControlMsg>(
            r#"{"action":"resolve_display_request","id":3}"#
        )
        .is_err());
    }

    #[test]
    fn display_request_events_map_to_outbound_wire_shapes() {
        let raised = app_event_to_outbound(&AppEvent::DisplayRequestRaised {
            session_id: Some("sess-1".to_string()),
            id: 7,
            access: "view".to_string(),
            reason: "watch the migration".to_string(),
            expires_unix_ms: 123_456,
        })
        .expect("raised maps to an outbound event");
        let wire = serde_json::to_value(&raised).unwrap();
        assert_eq!(wire["event"], "display_request_raised");
        assert_eq!(wire["session_id"], "sess-1");
        assert_eq!(wire["id"], 7);
        assert_eq!(wire["access"], "view");
        assert_eq!(wire["reason"], "watch the migration");
        assert_eq!(wire["expires_unix_ms"], 123_456);

        let resolved = app_event_to_outbound(&AppEvent::DisplayRequestResolved {
            session_id: None,
            id: 7,
            outcome: "approved".to_string(),
            access: Some("view".to_string()),
            duration: Some("this_session".to_string()),
        })
        .expect("resolved maps to an outbound event");
        let wire = serde_json::to_value(&resolved).unwrap();
        assert_eq!(wire["event"], "display_request_resolved");
        assert_eq!(wire["outcome"], "approved");
        assert_eq!(wire["access"], "view");
        assert_eq!(wire["duration"], "this_session");
        assert!(wire.get("session_id").is_none());
    }

    #[test]
    fn control_msg_set_diagnostics_visual_marker_default_display() {
        let json = r#"{"action":"set_diagnostics_visual_marker","enabled":true}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetDiagnosticsVisualMarker {
                display_id,
                enabled,
            } => {
                assert_eq!(display_id, None);
                assert!(enabled);
            }
            _ => panic!("expected SetDiagnosticsVisualMarker"),
        }
    }

    #[test]
    fn control_msg_set_diagnostics_visual_marker_with_id_disable() {
        let json = r#"{"action":"set_diagnostics_visual_marker","display_id":2,"enabled":false}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetDiagnosticsVisualMarker {
                display_id,
                enabled,
            } => {
                assert_eq!(display_id, Some(2));
                assert!(!enabled);
            }
            _ => panic!("expected SetDiagnosticsVisualMarker"),
        }
    }

    #[test]
    fn control_msg_set_diagnostics_visual_marker_missing_enabled_rejected() {
        // `enabled` is required (no #[serde(default)]). Toggle requests
        // without it are operator typos that would otherwise default to
        // `false`, silently turning the marker off when an enable was
        // intended. Better to fail loud at the wire-parse layer.
        let json = r#"{"action":"set_diagnostics_visual_marker"}"#;
        assert!(serde_json::from_str::<ControlMsg>(json).is_err());
    }

    #[test]
    fn control_msg_request_display_input_authority_deserialize() {
        let json = r#"{"action":"request_display_input_authority","display_id":0}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RequestDisplayInputAuthority { display_id } => {
                assert_eq!(display_id, 0);
            }
            _ => panic!("expected RequestDisplayInputAuthority"),
        }
    }

    #[test]
    fn control_msg_release_display_input_authority_deserialize() {
        let json = r#"{"action":"release_display_input_authority","display_id":2}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ReleaseDisplayInputAuthority { display_id } => {
                assert_eq!(display_id, 2);
            }
            _ => panic!("expected ReleaseDisplayInputAuthority"),
        }
    }

    // --- app_event_to_outbound tests ---

    #[test]
    fn outbound_turn_started() {
        let event = AppEvent::TurnStarted {
            session_id: None,
            turn: 5,
            budget_pct: 42.0,
            remaining: 100_000,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"turn_started\""));
        assert!(json.contains("\"turn\":5"));
    }

    #[test]
    fn outbound_done_signal() {
        let event = AppEvent::DoneSignal {
            session_id: Some("sess-1".to_string()),
            message: Some("All done".to_string()),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"done_signal\""));
        assert!(json.contains("\"session_id\":\"sess-1\""));
        assert!(json.contains("\"All done\""));
    }

    #[test]
    fn outbound_skips_tick() {
        assert!(app_event_to_outbound(&AppEvent::Tick).is_none());
    }

    fn sample_cu_action_event() -> AppEvent {
        AppEvent::CuActionExecuted {
            event_id: "cu-1700000000000-7".to_string(),
            session_id: Some("sess-1".to_string()),
            display_id: 99,
            kind: "left_click".to_string(),
            x: Some(612),
            y: Some(233),
            ref_w: 1280,
            ref_h: 800,
            raw: "left_click(612, 233)".to_string(),
            ts: 1_700_000_000_000,
        }
    }

    #[test]
    fn outbound_cu_action_pins_the_wire_shape() {
        let outbound = app_event_to_outbound(&sample_cu_action_event()).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"cu_action\""), "{json}");
        assert!(json.contains("\"event_id\":\"cu-1700000000000-7\""));
        assert!(json.contains("\"session_id\":\"sess-1\""));
        assert!(json.contains("\"display_id\":99"));
        assert!(json.contains("\"kind\":\"left_click\""));
        assert!(json.contains("\"x\":612"));
        assert!(json.contains("\"y\":233"));
        assert!(json.contains("\"ref_w\":1280"));
        assert!(json.contains("\"ref_h\":800"));
        assert!(json.contains("\"raw\":\"left_click(612, 233)\""));
        assert!(json.contains("\"ts\":1700000000000"));
    }

    #[test]
    fn outbound_cu_action_omits_absent_point_and_session() {
        let event = AppEvent::CuActionExecuted {
            event_id: "cu-1-1".to_string(),
            session_id: None,
            display_id: 0,
            kind: "type".to_string(),
            x: None,
            y: None,
            ref_w: 0,
            ref_h: 0,
            raw: "type(\"hi\")".to_string(),
            ts: 1,
        };
        let json = serde_json::to_string(&app_event_to_outbound(&event).unwrap()).unwrap();
        assert!(!json.contains("\"session_id\""), "{json}");
        assert!(!json.contains("\"x\""), "{json}");
        assert!(!json.contains("\"y\""), "{json}");
    }

    #[test]
    fn cu_action_events_never_reach_the_session_log() {
        // The action-visualization lane is deliberately ephemeral: it rides
        // the broadcast ring only. A cu_action in session.jsonl (and hence
        // in log_replay) would violate the no-replay contract.
        assert!(!app_event_writes_to_session_log(&sample_cu_action_event()));
    }

    #[test]
    fn outbound_autonomy_changed() {
        let event = AppEvent::AutonomyChanged {
            autonomy: "High".to_string(),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"autonomy_changed\""));
        assert!(json.contains("\"autonomy\":\"High\""));
    }

    #[test]
    fn outbound_codex_thread_action_result_preserves_record_id() {
        let event = AppEvent::CodexThreadActionResult {
            session_id: Some("codex-thread".to_string()),
            action: "rewind_context".to_string(),
            success: true,
            message: "rewound to item call-3 (before)".to_string(),
            record_id: Some("rewind-abc123".to_string()),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"codex_thread_action_result\""));
        assert!(json.contains("\"record_id\":\"rewind-abc123\""));
    }

    #[test]
    fn outbound_codex_thread_action_requested_is_visible() {
        let event = AppEvent::CodexThreadActionRequested {
            request_id: "req-1".to_string(),
            session_id: Some("codex-thread".to_string()),
            action: "fork".to_string(),
            params: serde_json::json!({}),
            origin: None,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"codex_thread_action_requested\""));
        assert!(json.contains("\"request_id\":\"req-1\""));
        assert!(json.contains("\"action\":\"fork\""));
    }

    #[test]
    fn outbound_codex_thread_action_result_omits_absent_record_id() {
        let event = AppEvent::CodexThreadActionResult {
            session_id: None,
            action: "rename".to_string(),
            success: true,
            message: "Codex thread renamed".to_string(),
            record_id: None,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"codex_thread_action_result\""));
        assert!(!json.contains("record_id"));
    }

    #[test]
    fn outbound_agent_output() {
        let event = AppEvent::AgentOutput {
            session_id: None,
            stdout: "hello".to_string(),
            stderr: "".to_string(),
            source: None,
            output_id: None,
            item_id: None,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"agent_output\""));
        assert!(json.contains("\"hello\""));
    }

    #[test]
    fn outbound_log_entry_preserves_session_id() {
        let event = AppEvent::LogEntry {
            session_id: Some("sess-log".to_string()),
            level: "info".to_string(),
            source: "Codex".to_string(),
            content: "Codex compacted context".to_string(),
            turn: None,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"log_entry\""));
        assert!(json.contains("\"session_id\":\"sess-log\""));
    }

    #[test]
    fn outbound_session_relationship_preserves_link_metadata() {
        let event = AppEvent::SessionRelationship {
            parent_session_id: "parent-1".to_string(),
            child_session_id: "child-1".to_string(),
            relationship: "side".to_string(),
            ephemeral: true,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"session_relationship\""));
        assert!(json.contains("\"parent_session_id\":\"parent-1\""));
        assert!(json.contains("\"child_session_id\":\"child-1\""));
        assert!(json.contains("\"relationship\":\"side\""));
        assert!(json.contains("\"ephemeral\":true"));
    }

    #[test]
    fn outbound_session_capabilities_preserves_controls() {
        let event = AppEvent::SessionCapabilities {
            session_id: "child-1".to_string(),
            capabilities: SessionCapabilities {
                follow_up: true,
                steer: false,
                interrupt: false,
                thread_actions: Vec::new(),
                codex_thread_actions: vec!["undo".to_string()],
                codex_managed_context: Some("managed".to_string()),
                codex_sandbox: Some("danger-full-access".to_string()),
                codex_approval_policy: Some("never".to_string()),
                codex_context_archive: Some("summary".to_string()),
                codex_command: Some("/opt/codex/bin/codex".to_string()),
                codex_fast_mode: Some(true),
                codex_service_tier: Some("priority".to_string()),
            },
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"session_capabilities\""));
        assert!(json.contains("\"session_id\":\"child-1\""));
        assert!(json.contains("\"follow_up\":true"));
        assert!(json.contains("\"steer\":false"));
        assert!(json.contains("\"codex_thread_actions\":[\"undo\"]"));
        assert!(json.contains("\"codex_managed_context\":\"managed\""));
        assert!(json.contains("\"codex_sandbox\":\"danger-full-access\""));
        assert!(json.contains("\"codex_approval_policy\":\"never\""));
        assert!(json.contains("\"codex_context_archive\":\"summary\""));
        assert!(json.contains("\"codex_command\":\"/opt/codex/bin/codex\""));
        assert!(json.contains("\"codex_fast_mode\":true"));
        assert!(json.contains("\"codex_service_tier\":\"priority\""));
    }

    #[test]
    fn outbound_session_goal_preserves_goal_state() {
        let event = AppEvent::SessionGoal {
            session_id: "thread-1".to_string(),
            goal: Some(SessionGoal {
                objective: "Ship feature parity".to_string(),
                status: Some("active".to_string()),
                elapsed_seconds: Some(42),
                tokens_used: Some(10),
                token_budget: Some(1000),
            }),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"session_goal\""));
        assert!(json.contains("\"session_id\":\"thread-1\""));
        assert!(json.contains("\"objective\":\"Ship feature parity\""));
        assert!(json.contains("\"elapsed_seconds\":42"));
    }

    #[test]
    fn outbound_follow_up_status_preserves_correlation() {
        let event = AppEvent::FollowUpStatus {
            session_id: Some("subagent-1".to_string()),
            id: "follow-1".to_string(),
            text: Some("next step".to_string()),
            status: "queued".to_string(),
            reason: Some("queued for next turn".to_string()),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"follow_up_status\""));
        assert!(json.contains("\"session_id\":\"subagent-1\""));
        assert!(json.contains("\"id\":\"follow-1\""));
        assert!(json.contains("\"text\":\"next step\""));
        assert!(json.contains("\"status\":\"queued\""));
        assert!(json.contains("\"reason\":\"queued for next turn\""));
    }

    #[test]
    fn outbound_user_message_edit_status_preserves_turn() {
        let event = AppEvent::UserMessageEditStatus {
            session_id: Some("codex-thread".to_string()),
            user_turn_index: 117,
            status: "attaching".to_string(),
            message: "attaching Codex session before edit".to_string(),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"user_message_edit_status\""));
        assert!(json.contains("\"session_id\":\"codex-thread\""));
        assert!(json.contains("\"user_turn_index\":117"));
        assert!(json.contains("\"status\":\"attaching\""));
        assert!(json.contains("\"message\":\"attaching Codex session before edit\""));
    }

    #[test]
    fn outbound_user_message_rewind() {
        let event = AppEvent::UserMessageRewind {
            session_id: Some("sess-1".to_string()),
            user_turn_index: 4,
            turns_removed: 2,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"user_message_rewind\""));
        assert!(json.contains("\"session_id\":\"sess-1\""));
        assert!(json.contains("\"user_turn_index\":4"));
        assert!(json.contains("\"turns_removed\":2"));
    }

    #[test]
    fn outbound_user_message_log_preserves_replacement_turn() {
        let event = AppEvent::UserMessageLog {
            session_id: Some("sess-1".to_string()),
            content: "New prompt".to_string(),
            user_turn_index: Some(4),
            user_turn_revision: Some(2),
            replacement_for_user_turn_index: Some(4),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"log_entry\""));
        assert!(json.contains("\"user_turn_index\":4"));
        assert!(json.contains("\"user_turn_revision\":2"));
        assert!(json.contains("\"replacement_for_user_turn_index\":4"));
    }

    #[test]
    fn session_log_writer_skips_agent_output() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        let shared = std::sync::Arc::new(std::sync::Mutex::new(log));

        write_event_to_session_log(
            &shared,
            &AppEvent::AgentOutput {
                session_id: None,
                stdout: "already logged inline".to_string(),
                stderr: String::new(),
                source: Some("Codex".to_string()),
                output_id: Some("out-1".to_string()),
                item_id: None,
            },
        );
        drop(shared);

        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        assert!(!contents.contains("\"event\":\"agent_output\""));
        assert!(!contents.contains("already logged inline"));
    }

    #[test]
    fn session_log_writer_persists_steer_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        let shared = std::sync::Arc::new(std::sync::Mutex::new(log));
        let full_text = "Quick interjectory note:\nPause before merge/push.\nKeep the full body.";

        write_event_to_session_log(
            &shared,
            &AppEvent::SteerRequested {
                session_id: Some("thread-1".to_string()),
                text: full_text.to_string(),
                id: "steer-1".to_string(),
            },
        );
        write_event_to_session_log(
            &shared,
            &AppEvent::SteerQueued {
                session_id: Some("thread-1".to_string()),
                id: "steer-1".to_string(),
                reason: "native steer failed".to_string(),
            },
        );
        write_event_to_session_log(
            &shared,
            &AppEvent::SteerDelivered {
                session_id: Some("thread-1".to_string()),
                id: "steer-1".to_string(),
                mid_turn: false,
            },
        );
        write_event_to_session_log(
            &shared,
            &AppEvent::SteerCancelled {
                session_id: Some("thread-1".to_string()),
                id: "steer-2".to_string(),
                reason: "cleared by user".to_string(),
            },
        );
        drop(shared);

        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        assert!(contents.contains("\"event\":\"steer_requested\""));
        assert!(contents.contains("\"event\":\"steer_queued\""));
        assert!(contents.contains("\"event\":\"steer_delivered\""));
        assert!(contents.contains("\"event\":\"steer_cancelled\""));
        assert!(contents.contains("Keep the full body."));
    }

    #[tokio::test]
    async fn session_log_subscription_filters_high_volume_events_but_keeps_steers() {
        let bus = EventBus::new();
        let mut log_rx = bus.subscribe_session_log();

        for i in 0..5000 {
            bus.send(AppEvent::ModelResponseDelta {
                session_id: Some("thread-1".to_string()),
                text: format!("delta-{i}"),
            });
            bus.send(AppEvent::Tick);
        }
        bus.send(AppEvent::SteerRequested {
            session_id: Some("thread-1".to_string()),
            text: "full steer body".to_string(),
            id: "steer-1".to_string(),
        });

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv())
            .await
            .expect("session-log event should arrive")
            .expect("session-log channel should remain open");
        match event {
            AppEvent::SteerRequested {
                session_id,
                text,
                id,
            } => {
                assert_eq!(session_id.as_deref(), Some("thread-1"));
                assert_eq!(text, "full steer body");
                assert_eq!(id, "steer-1");
            }
            other => panic!("expected SteerRequested, got {:?}", other),
        }
        assert!(
            log_rx.try_recv().is_err(),
            "deltas and ticks must not be queued"
        );
    }

    #[test]
    fn outbound_context_snapshot() {
        let event = AppEvent::ContextSnapshot {
            session_id: Some("sess-ctx".to_string()),
            source: "codex".to_string(),
            label: "Codex thread".to_string(),
            request_id: Some("req-ctx".to_string()),
            request_index: Some(7),
            turn: Some(3),
            format: "codex.thread.read.v2".to_string(),
            token_count: Some(1200),
            token_count_kind: Some("backend_reported".to_string()),
            context_window: Some(128000),
            hard_context_window: Some(128000),
            item_count: Some(2),
            raw: Arc::new(serde_json::json!({"thread": {"turns": []}})),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"context_snapshot\""));
        assert!(json.contains("\"session_id\":\"sess-ctx\""));
        assert!(json.contains("\"source\":\"codex\""));
        assert!(json.contains("\"request_index\":7"));
        assert!(json.contains("\"raw\""));
    }

    /// The whole point of `Arc`-ing `ContextSnapshot::raw`: every bus
    /// subscriber receives the event via `AppEvent::clone`, and that clone
    /// must share the (multi-MB) payload allocation, not deep-copy it.
    #[test]
    fn context_snapshot_clone_shares_raw_allocation() {
        let event = AppEvent::ContextSnapshot {
            session_id: Some("sess-ctx".to_string()),
            source: "native".to_string(),
            label: "Internal agent request payload".to_string(),
            request_id: Some("req-share".to_string()),
            request_index: Some(1),
            turn: Some(1),
            format: "openai.responses.resolved_request.v1".to_string(),
            token_count: None,
            token_count_kind: None,
            context_window: Some(128000),
            hard_context_window: Some(128000),
            item_count: Some(1),
            raw: Arc::new(serde_json::json!({"input": [{"role": "user"}]})),
        };
        let cloned = event.clone();
        let (
            AppEvent::ContextSnapshot { raw: original, .. },
            AppEvent::ContextSnapshot { raw: copy, .. },
        ) = (&event, &cloned)
        else {
            panic!("expected two context snapshots");
        };
        assert!(
            Arc::ptr_eq(original, copy),
            "cloning a ContextSnapshot must refcount the raw payload, not deep-copy it"
        );
    }

    #[test]
    fn outbound_context_snapshot_compacts_large_raw() {
        let large_text = "large-context ".repeat(20_000);
        let event = AppEvent::ContextSnapshot {
            session_id: Some("sess-ctx".to_string()),
            source: "codex".to_string(),
            label: "Codex thread".to_string(),
            request_id: Some("req-large".to_string()),
            request_index: Some(9),
            turn: Some(4),
            format: "openai.responses.resolved_request.v1".to_string(),
            token_count: Some(80_000),
            token_count_kind: Some("backend_reported".to_string()),
            context_window: Some(128000),
            hard_context_window: Some(128000),
            item_count: Some(1),
            raw: Arc::new(serde_json::json!({
                "input": [{"role": "user", "content": large_text}],
                "model": "codex",
            })),
        };

        let outbound = app_event_to_outbound(&event).unwrap();
        let crate::types::OutboundEvent::ContextSnapshot { raw, .. } = outbound else {
            panic!("expected context snapshot outbound event");
        };

        assert_eq!(
            raw.pointer("/summary/kind").and_then(|v| v.as_str()),
            Some("compact_context_snapshot")
        );
        assert_eq!(
            raw.pointer("/summary/exact_replay_available")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            raw.pointer("/_intendant_context/raw_omitted")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            serde_json::to_string(&raw).unwrap().len() < 20_000,
            "outbound snapshot should stay compact"
        );
    }

    #[test]
    fn outbound_approval_required() {
        let event = AppEvent::ApprovalRequired {
            session_id: Some("sess-1".to_string()),
            id: 42,
            command_preview: "rm -rf /tmp".to_string(),
            category: crate::autonomy::ActionCategory::Destructive,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"approval_required\""));
        assert!(json.contains("\"session_id\":\"sess-1\""));
        assert!(json.contains("\"id\":42"));
    }

    #[test]
    fn outbound_budget_warning() {
        let event = AppEvent::BudgetWarning {
            pct: 85.0,
            remaining: 10_000,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"budget_warning\""));
    }

    #[test]
    fn outbound_model_response_delta() {
        let event = AppEvent::ModelResponseDelta {
            session_id: None,
            text: "hello".to_string(),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"model_response_delta\""));
    }

    #[test]
    fn outbound_auto_approved() {
        let event = AppEvent::AutoApproved {
            preview: "exec: ls".to_string(),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"auto_approved\""));
    }

    #[test]
    fn outbound_live_usage_update() {
        let event = AppEvent::LiveUsageUpdate {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash".to_string(),
            input_tokens: 1000,
            output_tokens: 500,
            cached_tokens: 200,
            total_tokens: 1700,
            thinking_tokens: 0,
            input_text_tokens: 0,
            input_audio_tokens: 1000,
            input_image_tokens: 0,
            cached_text_tokens: 0,
            cached_audio_tokens: 200,
            cached_image_tokens: 0,
            output_text_tokens: 0,
            output_audio_tokens: 500,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"live_usage_update\""));
        assert!(json.contains("\"input_tokens\":1000"));
        assert!(json.contains("\"provider\":\"gemini\""));
    }

    #[test]
    fn outbound_broadcast_multiple_subscribers() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx1 = bus.subscribe();
            let mut rx2 = bus.subscribe();
            bus.send(AppEvent::Tick);

            // Both subscribers should receive the event
            assert!(matches!(rx1.recv().await, Ok(AppEvent::Tick)));
            assert!(matches!(rx2.recv().await, Ok(AppEvent::Tick)));
        });
    }
}

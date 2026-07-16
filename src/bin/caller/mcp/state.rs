//! The MCP surface's shared state: `McpAppState` (one struct both the event
//! listener and the tool/control handlers mutate), its session-status caches,
//! the `TaskLauncher` seam, and phase/verbosity string helpers.

use super::*;

/// A boxed async closure that spawns an agent loop for the given task.
///
/// The closure receives the task string and an `EventBus` for communicating
/// events back to the MCP server. It returns a `JoinHandle` for the spawned
/// background task.
pub type TaskLauncher = Box<
    dyn Fn(String, EventBus) -> Pin<Box<dyn Future<Output = tokio::task::JoinHandle<()>> + Send>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// Shared state that both the event listener and MCP handlers access
// ---------------------------------------------------------------------------

/// Observable state mirroring what the TUI's App struct tracks.
/// Updated by the event listener task, read by MCP tool/resource handlers.
pub struct McpAppState {
    pub provider_name: String,
    pub model_name: String,
    pub turn: usize,
    pub budget_pct: f64,
    pub phase: Phase,
    pub phase_entered_at: std::time::Instant,
    pub autonomy: SharedAutonomy,
    pub verbosity: Verbosity,
    pub session_tokens: u64,
    pub session_prompt_tokens: u64,
    pub session_completion_tokens: u64,
    pub session_cached_tokens: u64,
    pub session_cache_creation_tokens: u64,
    pub context_window: u64,
    pub hard_context_window: Option<u64>,
    pub session_id: String,
    pub task_description: String,
    pub log_entries: std::collections::VecDeque<LogEntrySnapshot>,
    next_log_id: u64,
    pub pending_approval: Option<PendingApprovalState>,
    pub approval_registry: ApprovalRegistry,
    pub human_question: Option<String>,
    pub should_quit: bool,
    /// Session log directory for askHuman files.
    pub log_dir: std::path::PathBuf,
    /// The daemon's project root, when it serves one. Must match the web
    /// gateway's `project_root_for_changes` so upload-store writes made by
    /// MCP tools (`post_session_note` image commits) resolve to the same
    /// [`crate::global_store::StoreScope`] the gateway's
    /// `/api/session/current/uploads/<id>/raw` route reads from.
    pub project_root: Option<std::path::PathBuf>,
    pub(crate) controller_loop_dir_override: Option<std::path::PathBuf>,
    pub(crate) controller_loop_status_override: Option<serde_json::Value>,
    /// Test override for the home that anchors persisted-session lookup
    /// (path-form session ids must resolve inside `<home>/.intendant/logs`).
    pub(crate) session_logs_home_override: Option<std::path::PathBuf>,
    /// Short-TTL cache of the raw (state-independent) controller-loop
    /// collection — the `ps` spawn + run-dir/wrapper-index scan half of
    /// [`collect_controller_loop_status_inner`]. Interior mutability so the
    /// polled read paths (which only hold `&McpAppState`) can serve and
    /// re-seed it; see [`CONTROLLER_LOOP_RAW_STATUS_TTL`]. Guarded by a
    /// generation counter: invalidation bumps it, and a collection started
    /// before the bump refuses to store — otherwise a lock-free pre-warm
    /// racing a lifecycle mutation could reinstate a pre-mutation sample.
    controller_loop_raw_status_cache: std::sync::Mutex<ControllerLoopRawStatusCache>,
    /// Per-`session.jsonl` replay cursors for
    /// [`hydrate_requested_session_status_from_logs`]: session-scoped
    /// `get_status` used to re-read and re-fold the whole log on every call;
    /// the cursor limits each hydration to the appended tail. Validated
    /// against the live file on use, so a replaced/truncated log self-heals
    /// with one full replay.
    pub(crate) session_log_hydration_cursors:
        std::collections::HashMap<std::path::PathBuf, SessionJsonlCursor>,
    /// Set while [`hydrate_requested_session_status_from_logs`] folds a log
    /// into this state, naming the replay kind. It gates two hazards: a
    /// replayed `session_ended` line must not re-run the over-cap prune
    /// (the rebuild the query is performing would erase itself and record
    /// an EOF cursor against absent state), and replayed lifecycle rows
    /// must not invalidate the controller-loop raw cache (historic events
    /// do not change process reality — see
    /// [`McpAppState::note_live_session_lifecycle_change`]). The replay
    /// itself applies rows UNGATED: the ordering contract lives at the
    /// hydrate fold site in `events.rs`.
    pub(crate) hydration_replay: Option<HydrationReplayKind>,
    /// Optional launcher for starting tasks via MCP. Set by main.rs.
    pub launcher: Option<Arc<TaskLauncher>>,
    /// Handle to the currently running agent loop, if any.
    pub task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Mode override for the next task: None = auto, Some(true) = orchestrate,
    /// Some(false) = direct. Consumed (reset to None) when a task starts.
    pub next_task_orchestrate: Option<bool>,
    /// Pending or completed controller restart plan.
    pub controller_restart: Option<ControllerRestartState>,
    /// Current round number (for multi-round support).
    pub round: usize,
    /// Sender for follow-up messages (multi-round support).
    pub follow_up_tx: Option<tokio::sync::mpsc::Sender<FollowUpMessage>>,
    // Presence layer usage tracking
    pub presence_provider_name: Option<String>,
    pub presence_model_name: Option<String>,
    pub presence_tokens: u64,
    pub presence_context_window: u64,
    pub presence_usage_pct: f64,
    /// Frame registry for display frame access.
    pub frame_registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    /// Display session registry for CU action dispatch.
    pub session_registry: Option<crate::display::SharedSessionRegistry>,
    /// Federated peer registry for the peer tools (`list_peers`,
    /// `peer_send_message`, `peer_delegate_task`). `None` when this
    /// process runs without federation (standalone `--mcp`, tests).
    pub peer_registry: Option<crate::peer::PeerRegistry>,
    /// User-session display IDs with an activation/portal request already in
    /// flight, keyed by when MCP/ctl last requested activation. This keeps
    /// screenshot loops from queueing duplicate Wayland portal sessions while
    /// the operator is approving the first one, but still lets stale/canceled
    /// approval paths be refreshed.
    pub(crate) user_display_activation_pending: std::collections::HashMap<u32, std::time::Instant>,
    /// Displays whose latest observed capture state is ready. This is a
    /// reducer-level guard against stale portal-pending events arriving after
    /// a ready event or successful screenshot.
    display_capture_ready: HashSet<u32>,
    /// Directory for screenshot output.
    pub screenshot_dir: Option<std::path::PathBuf>,
    /// Persistent counter for screenshot filenames (avoids overwriting).
    pub screenshot_counter: std::sync::atomic::AtomicU64,
    /// External agent backend selected via web UI (deferred: takes effect on next task).
    pub external_agent: Option<crate::external_agent::AgentBackend>,
    /// Desired Codex managed-context mode for the next managed Codex task.
    pub configured_codex_managed_context: bool,
    /// Whether the active Codex backend supports Intendant's managed-context
    /// protocol.
    pub codex_managed_context: bool,
    /// Managed-context capability latched per Intendant/backend session id.
    pub session_codex_managed_context: std::collections::HashMap<String, bool>,
    /// Bidirectional aliases between Intendant wrapper ids and backend thread ids.
    pub(crate) session_aliases:
        std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// Latest backend usage sample by Intendant/backend session id.
    pub(crate) session_usage: std::collections::HashMap<String, frontend::ModelUsageSnapshot>,
    /// Latest observed phase by Intendant/backend session id.
    session_status: std::collections::HashMap<String, SessionStatusState>,
    /// Source for the currently active session, when it is known.
    pub active_session_source: Option<String>,
    /// Map Intendant wrapper session IDs and backend session IDs to their external source.
    pub session_sources: std::collections::HashMap<String, String>,
    /// Session ids whose persisted log resolved to a non-external session —
    /// the negative memo for [`mcp_state_session_source_for_id`]'s fallback,
    /// which otherwise re-reads and identity-scans the whole session log on
    /// every status poll of a native session.
    pub(crate) session_known_non_external: std::collections::HashSet<String>,
    /// Successful rewind records awaiting the next backend usage sample, keyed
    /// by Intendant/backend session id.
    pub(crate) pending_rewind_pressure_checks: std::collections::HashMap<String, String>,
    /// Last successful rewinds that did not reduce backend-reported pressure
    /// below the gate, keyed by Intendant/backend session id.
    pub(crate) insufficient_rewind_notices:
        std::collections::HashMap<String, InsufficientRewindNotice>,
    /// Successful managed-context rewinds that satisfied the current density
    /// handoff/follow-up requirement until the round completes or pressure
    /// reaches rewind-only, keyed by Intendant/backend session id.
    density_maintenance_satisfied:
        std::collections::HashMap<String, DensityMaintenanceSatisfaction>,
    /// Whether a human-facing frontend can (now or later) answer blocking
    /// questions raised through this server: the web-gateway shape sets it
    /// (a dashboard can always attach while the ask waits) and the stdio
    /// MCP shape sets it (the client supervisor sees the question and can
    /// answer). Defaults to `false` — with no answerable frontend,
    /// `ask_user` auto-answers immediately with best-judgment guidance
    /// instead of blocking on nobody.
    pub interactive_frontends: bool,
    /// Resource URIs the stdio MCP client subscribed to. The event listener
    /// only pushes `notifications/resources/updated` for these (MCP resource
    /// notifications are subscription-scoped); with no subscriptions the
    /// per-event stdout writes are skipped entirely.
    pub(crate) subscribed_resource_uris: std::collections::HashSet<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionStatusState {
    pub(crate) turn: usize,
    pub(crate) round: usize,
    pub(crate) phase: Phase,
    pub(crate) task: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InsufficientRewindNotice {
    pub(crate) record_id: String,
    pub(crate) used_tokens: u64,
    pub(crate) rewind_only_limit: u64,
    pub(crate) context_window: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DensityMaintenanceSatisfaction {
    record_id: String,
    used_tokens: u64,
    recommended_rewind_limit: u64,
    rewind_only_limit: u64,
    round: usize,
}

/// Soft cap on the per-session bookkeeping maps. Entries for every session
/// id and alias ever observed used to accumulate for the daemon's lifetime;
/// once the status map outgrows this, ended sessions are pruned on
/// [`McpAppState::note_session_ended`] instead of lingering.
pub(crate) const ENDED_SESSION_PRUNE_THRESHOLD: usize = 1024;

/// Which kind of log replay a hydration pass is performing. See
/// [`McpAppState::hydration_replay`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HydrationReplayKind {
    /// Replaying from byte 0 — re-applies history that live observation may
    /// already have superseded.
    Full,
    /// Replaying only bytes appended past a validated cursor — new
    /// information by construction.
    Tail,
}

/// RAII scope for [`McpAppState::hydration_replay`]: constructed via
/// [`McpAppState::begin_hydration_replay`], resets the flag on drop so a
/// panic (or early return) inside a replay can never leave the state
/// permanently in "hydrating" mode — which would suppress live lifecycle
/// invalidations and the over-cap prune indefinitely.
pub(crate) struct HydrationReplayGuard<'a> {
    state: &'a mut McpAppState,
}

impl HydrationReplayGuard<'_> {
    pub(crate) fn state(&mut self) -> &mut McpAppState {
        self.state
    }
}

impl Drop for HydrationReplayGuard<'_> {
    fn drop(&mut self) {
        self.state.hydration_replay = None;
    }
}

/// Cache slot for the raw controller-loop sample, with the invalidation
/// generation. See the field doc on
/// [`McpAppState::controller_loop_raw_status_cache`].
#[derive(Default)]
pub(crate) struct ControllerLoopRawStatusCache {
    generation: u64,
    entry: Option<(std::time::Instant, ControllerLoopRawStatus)>,
}

/// Tracks a pending approval info (responder is in the shared ApprovalRegistry).
pub struct PendingApprovalState {
    pub id: u64,
    pub command_preview: String,
    pub category: String,
}

impl McpAppState {
    pub fn new(
        provider_name: String,
        model_name: String,
        autonomy: SharedAutonomy,
        log_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            provider_name,
            model_name,
            turn: 0,
            budget_pct: 0.0,
            phase: Phase::Idle,
            phase_entered_at: std::time::Instant::now(),
            autonomy,
            verbosity: Verbosity::Normal,
            session_tokens: 0,
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
            session_cached_tokens: 0,
            session_cache_creation_tokens: 0,
            context_window: 0,
            hard_context_window: None,
            session_id: String::new(),
            task_description: String::new(),
            log_entries: std::collections::VecDeque::new(),
            next_log_id: 0,
            pending_approval: None,
            approval_registry: ApprovalRegistry::default(),
            human_question: None,
            should_quit: false,
            log_dir,
            project_root: None,
            controller_loop_dir_override: None,
            controller_loop_status_override: None,
            session_logs_home_override: None,
            controller_loop_raw_status_cache: std::sync::Mutex::new(
                ControllerLoopRawStatusCache::default(),
            ),
            session_log_hydration_cursors: std::collections::HashMap::new(),
            hydration_replay: None,
            launcher: None,
            task_handle: None,
            controller_restart: None,
            next_task_orchestrate: None,
            round: 0,
            follow_up_tx: None,
            presence_provider_name: None,
            presence_model_name: None,
            presence_tokens: 0,
            presence_context_window: 0,
            presence_usage_pct: 0.0,
            frame_registry: None,
            session_registry: None,
            peer_registry: None,
            user_display_activation_pending: std::collections::HashMap::new(),
            display_capture_ready: HashSet::new(),
            screenshot_dir: None,
            screenshot_counter: std::sync::atomic::AtomicU64::new(0),
            external_agent: None,
            configured_codex_managed_context: false,
            codex_managed_context: false,
            session_codex_managed_context: std::collections::HashMap::new(),
            session_aliases: std::collections::HashMap::new(),
            session_usage: std::collections::HashMap::new(),
            session_status: std::collections::HashMap::new(),
            active_session_source: None,
            session_sources: std::collections::HashMap::new(),
            session_known_non_external: std::collections::HashSet::new(),
            pending_rewind_pressure_checks: std::collections::HashMap::new(),
            insufficient_rewind_notices: std::collections::HashMap::new(),
            density_maintenance_satisfied: std::collections::HashMap::new(),
            interactive_frontends: false,
            subscribed_resource_uris: std::collections::HashSet::new(),
        }
    }

    pub(crate) fn set_phase(&mut self, phase: Phase) {
        if self.phase != phase {
            self.phase = phase;
            self.phase_entered_at = std::time::Instant::now();
        }
    }

    /// Probe the raw controller-loop cache: a still-fresh sample for
    /// `loop_dir` (a stale entry or one collected for a different loop dir
    /// misses) plus the current invalidation generation. Callers that miss
    /// collect WITHOUT any lock held, then store through
    /// [`Self::store_controller_loop_raw_status_at`] with this generation —
    /// if a lifecycle mutation invalidated meanwhile, the pre-mutation
    /// sample is discarded instead of reinstated.
    pub(crate) fn probe_controller_loop_raw_status(
        &self,
        loop_dir: &std::path::Path,
    ) -> (Option<ControllerLoopRawStatus>, u64) {
        let cache = self
            .controller_loop_raw_status_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let hit = cache.entry.as_ref().and_then(|(collected_at, raw)| {
            (collected_at.elapsed() < CONTROLLER_LOOP_RAW_STATUS_TTL && raw.loop_dir() == loop_dir)
                .then(|| raw.clone())
        });
        (hit, cache.generation)
    }

    /// Store a raw sample collected while the cache was at `generation`.
    /// A no-op when the generation has moved (an invalidation raced the
    /// collection): the sample predates the mutation that bumped it.
    pub(crate) fn store_controller_loop_raw_status_at(
        &self,
        generation: u64,
        raw: ControllerLoopRawStatus,
    ) {
        let mut cache = self
            .controller_loop_raw_status_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cache.generation == generation {
            cache.entry = Some((std::time::Instant::now(), raw));
        }
    }

    /// Enter a hydration-replay scope. The returned guard resets
    /// [`Self::hydration_replay`] on drop (panic-safe); mutate the state
    /// through [`HydrationReplayGuard::state`] for the replay's duration.
    pub(crate) fn begin_hydration_replay(
        &mut self,
        kind: HydrationReplayKind,
    ) -> HydrationReplayGuard<'_> {
        self.hydration_replay = Some(kind);
        HydrationReplayGuard { state: self }
    }

    /// Invalidate the raw controller-loop cache for a LIVE-observed session
    /// lifecycle transition (task dispatch, session start/end, completion,
    /// interruption): process reality just changed, so a pre-transition
    /// sample must not answer the next poll. Hydration replays of historic
    /// logs pass through the same fold arms and are ignored here — they do
    /// not change process reality.
    pub(crate) fn note_live_session_lifecycle_change(&self) {
        if self.hydration_replay.is_none() {
            self.invalidate_controller_loop_raw_status_cache();
        }
    }

    /// Drop the cached raw controller-loop sample and bump the generation.
    /// Called wherever process/marker reality changes — loop-marker tools
    /// (halt/intervene/clear), task dispatch, and observed session
    /// lifecycle transitions — so a status poll right after the mutation
    /// re-collects instead of serving a sub-TTL stale sample, and so an
    /// in-flight unlocked collection cannot reinstate one.
    pub(crate) fn invalidate_controller_loop_raw_status_cache(&self) {
        let mut cache = self
            .controller_loop_raw_status_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.generation = cache.generation.wrapping_add(1);
        cache.entry = None;
    }

    pub(crate) fn push_log(&mut self, level: LogLevel, content: String) {
        let id = self.next_log_id;
        self.next_log_id += 1;
        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        if self.log_entries.len() >= 10_000 {
            self.log_entries.pop_front();
        }
        self.log_entries.push_back(LogEntrySnapshot {
            id,
            ts,
            level: frontend::log_level_to_str(&level).to_string(),
            content,
        });
    }

    pub(crate) fn display_session_probe_now(&self, display_id: u32) -> Option<Option<(u32, u32)>> {
        let Some(registry) = self.session_registry.as_ref() else {
            return Some(None);
        };
        let Ok(registry) = registry.try_read() else {
            return None;
        };
        Some(registry.get(display_id).map(|session| session.resolution()))
    }

    pub(crate) fn display_session_resolution_now(&self, display_id: u32) -> Option<(u32, u32)> {
        self.display_session_probe_now(display_id).flatten()
    }

    pub(crate) fn display_capture_ready_now(&mut self, display_id: u32) -> bool {
        match self.display_session_probe_now(display_id) {
            Some(Some(_)) => {
                self.display_capture_ready.insert(display_id);
                true
            }
            Some(None) => {
                self.display_capture_ready.remove(&display_id);
                false
            }
            None => self.display_capture_ready.contains(&display_id),
        }
    }

    pub(crate) fn note_display_capture_ready(&mut self, display_id: u32) {
        self.user_display_activation_pending.remove(&display_id);
        self.display_capture_ready.insert(display_id);
    }

    pub(crate) fn note_display_capture_lost(&mut self, display_id: u32) {
        self.user_display_activation_pending.remove(&display_id);
        self.display_capture_ready.remove(&display_id);
    }

    pub(crate) fn note_display_approval_pending(&mut self, display_id: u32, backend: &str) -> bool {
        if self.display_capture_ready_now(display_id) {
            self.note_display_capture_ready(display_id);
            return false;
        }
        self.user_display_activation_pending
            .insert(display_id, std::time::Instant::now());
        self.push_log(
            LogLevel::Info,
            format!(
                "Display :{} waiting for OS portal approval ({backend}); enable Allow Remote Interaction before Share for Computer Use input",
                display_id
            ),
        );
        true
    }

    pub(crate) fn status_snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            session_id: self.session_id.clone(),
            task: self.task_description.clone(),
            provider: self.provider_name.clone(),
            model: self.model_name.clone(),
            turn: self.turn,
            budget_pct: self.budget_pct,
            phase: phase_to_str(&self.phase).to_string(),
            autonomy: "unknown".to_string(), // filled by caller with async read
            verbosity: verbosity_to_str(self.verbosity).to_string(),
            session_tokens: self.session_tokens,
            round: self.round,
        }
    }

    pub(crate) fn usage_snapshot(&self) -> crate::frontend::UsageSnapshot {
        crate::frontend::UsageSnapshot {
            main: crate::frontend::ModelUsageSnapshot {
                provider: self.provider_name.clone(),
                model: self.model_name.clone(),
                tokens_used: self.session_tokens,
                context_window: self.context_window,
                hard_context_window: self.hard_context_window,
                usage_pct: self.budget_pct,
                prompt_tokens: self.session_prompt_tokens,
                completion_tokens: self.session_completion_tokens,
                cached_tokens: self.session_cached_tokens,
                cache_creation_tokens: self.session_cache_creation_tokens,
                ..Default::default()
            },
            presence: self.presence_provider_name.as_ref().map(|p| {
                crate::frontend::ModelUsageSnapshot {
                    provider: p.clone(),
                    model: self.presence_model_name.clone().unwrap_or_default(),
                    tokens_used: self.presence_tokens,
                    context_window: self.presence_context_window,
                    hard_context_window: Some(self.presence_context_window),
                    usage_pct: self.presence_usage_pct,
                    ..Default::default()
                }
            }),
        }
    }

    pub(crate) fn usage_snapshot_for(
        &self,
        session_id: Option<&str>,
    ) -> crate::frontend::UsageSnapshot {
        let mut usage = self.usage_snapshot();
        if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
            if let Some(main) = self.session_usage_for_id(id) {
                usage.main = main.clone();
            } else if id != self.session_id {
                usage.main.provider.clear();
                usage.main.model.clear();
                usage.main.tokens_used = 0;
                usage.main.context_window = 0;
                usage.main.hard_context_window = None;
                usage.main.usage_pct = 0.0;
                usage.main.prompt_tokens = 0;
                usage.main.completion_tokens = 0;
                usage.main.cached_tokens = 0;
                usage.main.cache_creation_tokens = 0;
            }
        }
        usage
    }

    pub(crate) fn link_session_aliases(&mut self, session_id: &str, backend_session_id: &str) {
        let session_id = session_id.trim();
        let backend_session_id = backend_session_id.trim();
        if session_id.is_empty()
            || backend_session_id.is_empty()
            || session_id == backend_session_id
        {
            return;
        }
        self.session_aliases
            .entry(session_id.to_string())
            .or_default()
            .insert(backend_session_id.to_string());
        self.session_aliases
            .entry(backend_session_id.to_string())
            .or_default()
            .insert(session_id.to_string());
        if let Some(status) = self
            .session_status
            .get(session_id)
            .cloned()
            .or_else(|| self.session_status.get(backend_session_id).cloned())
        {
            self.session_status
                .insert(session_id.to_string(), status.clone());
            self.session_status
                .insert(backend_session_id.to_string(), status);
        }
        if let Some(usage) = self
            .session_usage
            .get(backend_session_id)
            .cloned()
            .or_else(|| self.session_usage.get(session_id).cloned())
        {
            self.session_usage
                .insert(session_id.to_string(), usage.clone());
            self.session_usage
                .insert(backend_session_id.to_string(), usage);
        }
    }

    pub(crate) fn session_related_ids(&self, session_id: &str) -> Vec<String> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(session_id.to_string());
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            out.push(id.clone());
            if let Some(aliases) = self.session_aliases.get(&id) {
                for alias in aliases {
                    if !seen.contains(alias) {
                        queue.push_back(alias.clone());
                    }
                }
            }
        }
        out
    }

    pub(crate) fn session_usage_for_id(
        &self,
        session_id: &str,
    ) -> Option<&frontend::ModelUsageSnapshot> {
        for related in self.session_related_ids(session_id) {
            if let Some(usage) = self.session_usage.get(&related) {
                return Some(usage);
            }
        }
        None
    }

    pub(crate) fn record_session_usage_snapshot(
        &mut self,
        session_id: Option<&str>,
        usage: frontend::ModelUsageSnapshot,
    ) {
        let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return;
        };
        let mut keys = self.session_related_ids(id);
        if keys.is_empty() {
            keys.push(id.to_string());
        }
        for key in keys {
            self.session_usage.insert(key, usage.clone());
        }
    }

    pub(crate) fn clear_session_usage_for_id(&mut self, session_id: &str) {
        let mut keys = self.session_related_ids(session_id);
        if keys.is_empty() {
            keys.push(session_id.trim().to_string());
        }
        keys.retain(|key| !key.is_empty());
        for key in &keys {
            self.session_usage.remove(key);
        }
        if keys.iter().any(|key| key == &self.session_id) {
            self.session_tokens = 0;
            self.session_prompt_tokens = 0;
            self.session_completion_tokens = 0;
            self.session_cached_tokens = 0;
            self.session_cache_creation_tokens = 0;
            self.context_window = 0;
            self.hard_context_window = None;
            self.budget_pct = 0.0;
        }
    }

    pub(crate) fn note_session_ended(&mut self, session_id: &str) {
        self.note_session_phase(Some(session_id), None, Phase::Done, None);
        self.clear_session_usage_for_id(session_id);
        if self.session_id == session_id
            || self
                .session_related_ids(session_id)
                .iter()
                .any(|related| related == &self.session_id)
        {
            self.set_phase(Phase::Done);
            self.active_session_source = None;
            self.codex_managed_context = self.configured_codex_managed_context;
        }
        self.remove_pending_rewind_pressure_check_for_key(session_id);
        self.remove_insufficient_rewind_notice_for_key(session_id);
        self.remove_density_maintenance_satisfied_for_key(session_id);
        // A session ending changes process reality: a cached raw
        // controller-loop sample from before the exit must not answer the
        // next status poll. (No-op during hydration replays.)
        self.note_live_session_lifecycle_change();
        // Never prune from a hydration replay: the replayed `session_ended`
        // line belongs to the very session a query is rebuilding — pruning
        // here would erase the rebuild and leave an EOF cursor pointing at
        // absent state, so a later query would answer with the daemon's
        // current (unrelated) status under the requested id. Query-driven
        // rebuilds stay resident instead.
        if self.hydration_replay.is_none()
            && self.session_status.len() > ENDED_SESSION_PRUNE_THRESHOLD
        {
            self.prune_ended_session_bookkeeping(session_id);
        }
    }

    /// Drop an ended session's per-session bookkeeping (status, usage,
    /// sources, aliases, managed-context latch). Only invoked once the
    /// status map outgrows [`ENDED_SESSION_PRUNE_THRESHOLD`]: under the cap
    /// an ended session stays resident so post-end queries (get_status of a
    /// finished session, get_logs by backend alias) answer from memory;
    /// over it, a later query rebuilds the entries from the persisted log
    /// via hydration — correct, just one full replay slower.
    fn prune_ended_session_bookkeeping(&mut self, session_id: &str) {
        let ids = self.session_related_ids(session_id);
        for id in &ids {
            self.session_status.remove(id);
            self.session_usage.remove(id);
            self.session_sources.remove(id);
            self.session_codex_managed_context.remove(id);
            self.session_known_non_external.remove(id);
            self.session_aliases.remove(id);
        }
        // Drop ONLY the ended component's hydration cursors (log-dir
        // basenames carry the session id, with the store's prefix-match
        // semantics), so live sessions keep theirs — on a long-lived daemon
        // permanently over the cap, wiping every cursor on every session
        // end would re-trigger the full-replay-under-write-lock cost this
        // module exists to avoid. A cursor a rename/meta mismatch leaves
        // behind is caught by hydration's pruned-state backstop, which
        // forces a full replay whenever the requested session has no folded
        // status.
        self.session_log_hydration_cursors.retain(|path, _| {
            let Some(dir_name) = path
                .parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str())
            else {
                return false;
            };
            !ids.iter().any(|id| {
                !id.is_empty() && (dir_name == id.as_str() || dir_name.starts_with(id.as_str()))
            })
        });
    }

    pub(crate) fn session_id_applies_to_current_session(&self, session_id: Option<&str>) -> bool {
        let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return true;
        };
        self.session_id.is_empty()
            || id == self.session_id
            || self
                .session_related_ids(id)
                .iter()
                .any(|related| related == &self.session_id)
    }

    pub(crate) fn session_status_for_id(&self, session_id: &str) -> Option<&SessionStatusState> {
        for related in self.session_related_ids(session_id) {
            if let Some(status) = self.session_status.get(&related) {
                return Some(status);
            }
        }
        None
    }

    pub(crate) fn session_source_for_id(&self, session_id: &str) -> Option<&str> {
        for related in self.session_related_ids(session_id) {
            if let Some(source) = self.session_sources.get(&related) {
                return Some(source.as_str());
            }
        }
        None
    }

    pub(crate) fn note_session_phase(
        &mut self,
        session_id: Option<&str>,
        turn: Option<usize>,
        phase: Phase,
        task: Option<&str>,
    ) {
        let target_id = session_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let id = self.session_id.trim();
                (!id.is_empty()).then(|| id.to_string())
            });
        let Some(target_id) = target_id else {
            if let Some(turn) = turn {
                self.turn = turn;
            }
            self.set_phase(phase);
            if let Some(task) = task.map(str::trim).filter(|task| !task.is_empty()) {
                self.task_description = task.to_string();
            }
            return;
        };

        let keys = {
            let related = self.session_related_ids(&target_id);
            if related.is_empty() {
                vec![target_id.clone()]
            } else {
                related
            }
        };
        let existing = keys
            .iter()
            .find_map(|key| self.session_status.get(key))
            .cloned();
        let applies_to_current = self.session_id.is_empty()
            || keys.iter().any(|key| key == &self.session_id)
            || self
                .session_related_ids(&self.session_id)
                .iter()
                .any(|key| keys.contains(key));
        let turn = turn
            .or_else(|| existing.as_ref().map(|status| status.turn))
            .unwrap_or(self.turn);
        let round = existing
            .as_ref()
            .map(|status| status.round)
            .unwrap_or(self.round);
        let task = task
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .map(str::to_string)
            .or_else(|| existing.as_ref().map(|status| status.task.clone()))
            .or_else(|| applies_to_current.then(|| self.task_description.clone()))
            .unwrap_or_default();
        let status = SessionStatusState {
            turn,
            round,
            phase: phase.clone(),
            task: task.clone(),
        };
        for key in keys {
            self.session_status.insert(key, status.clone());
        }
        if applies_to_current {
            self.turn = turn;
            if !task.is_empty() {
                self.task_description = task;
            }
            self.set_phase(phase);
        }
    }

    pub(crate) fn note_session_round(&mut self, session_id: Option<&str>, round: usize) {
        let target_id = session_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let id = self.session_id.trim();
                (!id.is_empty()).then(|| id.to_string())
            });
        let Some(target_id) = target_id else {
            self.round = round;
            return;
        };

        let keys = {
            let related = self.session_related_ids(&target_id);
            if related.is_empty() {
                vec![target_id.clone()]
            } else {
                related
            }
        };
        for key in &keys {
            let entry =
                self.session_status
                    .entry(key.clone())
                    .or_insert_with(|| SessionStatusState {
                        turn: self.turn,
                        round,
                        phase: self.phase.clone(),
                        task: self.task_description.clone(),
                    });
            entry.round = round;
        }
        if self.session_id.is_empty()
            || keys.iter().any(|key| key == &self.session_id)
            || self
                .session_related_ids(&self.session_id)
                .iter()
                .any(|key| keys.contains(key))
        {
            self.round = round;
        }
    }

    pub(crate) fn normalize_main_usage_snapshot(
        &self,
        session_id: Option<&str>,
        mut usage: frontend::ModelUsageSnapshot,
    ) -> frontend::ModelUsageSnapshot {
        let previous_hard = session_id
            .and_then(|id| {
                self.session_usage_for_id(id)
                    .and_then(|previous| previous.hard_context_window)
            })
            .or(self.hard_context_window)
            .filter(|hard| *hard > 0);
        let Some(previous_hard) = previous_hard else {
            return usage;
        };
        if usage.context_window == 0 {
            return usage;
        }

        let should_preserve = match usage.hard_context_window {
            Some(current_hard) if current_hard > 0 => {
                current_hard <= usage.context_window && previous_hard > current_hard
            }
            _ => previous_hard > usage.context_window,
        };
        if should_preserve {
            usage.hard_context_window = Some(previous_hard);
        }
        usage
    }

    pub(crate) fn apply_main_usage_snapshot(&mut self, usage: frontend::ModelUsageSnapshot) {
        let usage = self.normalize_main_usage_snapshot(None, usage);
        if !usage.provider.is_empty() {
            self.provider_name = usage.provider.clone();
        }
        if !usage.model.is_empty() {
            self.model_name = usage.model.clone();
        }
        self.session_tokens = usage.tokens_used;
        self.context_window = usage.context_window;
        self.hard_context_window = usage.hard_context_window;
        self.budget_pct = usage.usage_pct;
        self.session_prompt_tokens = usage.prompt_tokens;
        self.session_completion_tokens = usage.completion_tokens;
        self.session_cached_tokens = usage.cached_tokens;
        self.session_cache_creation_tokens = usage.cache_creation_tokens;
        self.complete_pending_rewind_pressure_check();
    }

    pub(crate) fn rewind_session_key(&self, session_id: Option<&str>) -> Option<String> {
        session_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let id = self.session_id.trim();
                if id.is_empty() {
                    None
                } else {
                    Some(id.to_string())
                }
            })
    }

    pub(crate) fn rewind_related_keys(&self, key: &str) -> Vec<String> {
        let related = self.session_related_ids(key);
        if related.is_empty() {
            vec![key.to_string()]
        } else {
            related
        }
    }

    pub(crate) fn remove_pending_rewind_pressure_check_for_key(
        &mut self,
        key: &str,
    ) -> Option<String> {
        let mut record_id = None;
        for related in self.rewind_related_keys(key) {
            if let Some(found) = self.pending_rewind_pressure_checks.remove(&related) {
                record_id.get_or_insert(found);
            }
        }
        record_id
    }

    pub(crate) fn remove_insufficient_rewind_notice_for_key(&mut self, key: &str) {
        for related in self.rewind_related_keys(key) {
            self.insufficient_rewind_notices.remove(&related);
        }
    }

    pub(crate) fn remove_density_maintenance_satisfied_for_key(&mut self, key: &str) {
        for related in self.rewind_related_keys(key) {
            self.density_maintenance_satisfied.remove(&related);
        }
    }

    pub(crate) fn insert_insufficient_rewind_notice_for_key(
        &mut self,
        key: &str,
        notice: InsufficientRewindNotice,
    ) {
        for related in self.rewind_related_keys(key) {
            self.insufficient_rewind_notices
                .insert(related, notice.clone());
        }
    }

    pub(crate) fn insert_density_maintenance_satisfied_for_key(
        &mut self,
        key: &str,
        satisfied: DensityMaintenanceSatisfaction,
    ) {
        for related in self.rewind_related_keys(key) {
            self.density_maintenance_satisfied
                .insert(related, satisfied.clone());
        }
    }

    pub(crate) fn note_context_rewind_result_for(
        &mut self,
        session_id: Option<&str>,
        success: bool,
        record_id: Option<&str>,
        message: &str,
    ) {
        let Some(key) = self.rewind_session_key(session_id) else {
            return;
        };
        if success {
            // The structured `record_id` on `CodexThreadActionResult` is the
            // primary source. Parsing the human-readable message is kept only
            // as a fallback for results that predate the structured field
            // (e.g. forwarded by an older daemon) — do not rely on it for new
            // emit sites; set `record_id` at the emitter instead.
            if let Some(record_id) = record_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .or_else(|| context_rewind_record_id_from_message(message))
            {
                for related in self.rewind_related_keys(&key) {
                    self.pending_rewind_pressure_checks
                        .insert(related, record_id.clone());
                }
                self.remove_insufficient_rewind_notice_for_key(&key);
                self.remove_density_maintenance_satisfied_for_key(&key);
                self.clear_session_usage_for_id(&key);
            }
        } else {
            // A failed rewind must not leave a pending pressure check behind: a
            // later (possibly stale) usage sample could otherwise resolve it into a
            // false "insufficient" notice against a record that never committed.
            self.remove_pending_rewind_pressure_check_for_key(&key);
            self.remove_density_maintenance_satisfied_for_key(&key);
        }
    }

    pub(crate) fn complete_pending_rewind_pressure_check(&mut self) {
        self.complete_pending_rewind_pressure_check_for(None);
    }

    pub(crate) fn complete_pending_rewind_pressure_check_for(&mut self, session_id: Option<&str>) {
        let Some(key) = self.rewind_session_key(session_id) else {
            return;
        };
        let Some(record_id) = self.pending_rewind_pressure_check_for(Some(&key)).cloned() else {
            return;
        };
        if !self.active_codex_managed_context_enabled_for(Some(&key), None) {
            return;
        }
        let (used_tokens, context_window, _hard_context_window) =
            self.session_usage_values(Some(&key));
        if context_window == 0 {
            return;
        }
        self.remove_pending_rewind_pressure_check_for_key(&key);
        if let Some((used_tokens, rewind_only_limit, _status)) =
            self.context_pressure_rewind_only_for(Some(&key))
        {
            self.remove_density_maintenance_satisfied_for_key(&key);
            self.insert_insufficient_rewind_notice_for_key(
                &key,
                InsufficientRewindNotice {
                    record_id,
                    used_tokens,
                    rewind_only_limit,
                    context_window,
                },
            );
        } else {
            self.remove_insufficient_rewind_notice_for_key(&key);
            if context_window > 0 {
                let recommended_rewind_limit =
                    (context_window as f64 * CONTEXT_PRESSURE_REWIND_THRESHOLD_PCT / 100.0).floor()
                        as u64;
                let round = self
                    .session_status_for_id(&key)
                    .map(|status| status.round)
                    .unwrap_or(self.round);
                self.insert_density_maintenance_satisfied_for_key(
                    &key,
                    DensityMaintenanceSatisfaction {
                        record_id,
                        used_tokens,
                        recommended_rewind_limit,
                        rewind_only_limit: context_window,
                        round,
                    },
                );
            } else {
                self.remove_density_maintenance_satisfied_for_key(&key);
            }
        }
    }

    pub(crate) fn pending_rewind_pressure_check_for(
        &self,
        session_id: Option<&str>,
    ) -> Option<&String> {
        let key = self.rewind_session_key(session_id)?;
        self.rewind_related_keys(&key)
            .into_iter()
            .find_map(|related| self.pending_rewind_pressure_checks.get(&related))
    }

    pub(crate) fn insufficient_rewind_notice_for(
        &self,
        session_id: Option<&str>,
    ) -> Option<&InsufficientRewindNotice> {
        let key = self.rewind_session_key(session_id)?;
        self.rewind_related_keys(&key)
            .into_iter()
            .find_map(|related| self.insufficient_rewind_notices.get(&related))
    }

    pub(crate) fn density_maintenance_satisfied_for(
        &self,
        session_id: Option<&str>,
        used_tokens: u64,
        recommended_rewind_limit: u64,
        rewind_only_limit: u64,
    ) -> Option<&DensityMaintenanceSatisfaction> {
        let key = self.rewind_session_key(session_id)?;
        let round = self
            .session_status_for_id(&key)
            .map(|status| status.round)
            .unwrap_or(self.round);
        self.rewind_related_keys(&key)
            .into_iter()
            .find_map(|related| self.density_maintenance_satisfied.get(&related))
            .filter(|satisfied| {
                satisfied.rewind_only_limit == rewind_only_limit
                    && used_tokens < rewind_only_limit
                    && recommended_rewind_limit == satisfied.recommended_rewind_limit
                    && round == satisfied.round
            })
    }

    pub(crate) fn managed_context_mode(enabled: bool) -> &'static str {
        if enabled {
            "managed"
        } else {
            "vanilla"
        }
    }

    pub(crate) fn session_usage_values(&self, session_id: Option<&str>) -> (u64, u64, Option<u64>) {
        if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
            // A concrete session that has not reported usage yet is *unknown* — do
            // not borrow the globally-active session's totals. Borrowing would let a
            // starting session A inherit a saturated session B's pressure and be
            // wrongly forced into rewind-only mode during the startup race.
            if let Some(usage) = self.session_usage_for_id(id) {
                return (
                    usage.tokens_used,
                    usage.context_window,
                    usage.hard_context_window,
                );
            }

            if self
                .session_related_ids(id)
                .iter()
                .any(|candidate| candidate == &self.session_id)
                && self.context_window > 0
            {
                return (
                    self.session_tokens,
                    self.context_window,
                    self.hard_context_window,
                );
            }

            return (0, 0, None);
        }
        (
            self.session_tokens,
            self.context_window,
            self.hard_context_window,
        )
    }

    pub(crate) fn context_pressure_snapshot_for(
        &self,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> serde_json::Value {
        let (used_tokens, context_window, hard_context_window) =
            self.session_usage_values(session_id);
        let managed_context =
            self.exposed_codex_managed_context_enabled_for(session_id, managed_context_override);
        if managed_context {
            if let Some(record_id) = self.pending_rewind_pressure_check_for(session_id) {
                return serde_json::json!({
                    "source": "stale_after_rewind",
                    "status": "refreshing",
                    "used_tokens": null,
                    "context_window": null,
                    "effective_context_window": null,
                    "remaining_tokens": null,
                    "remaining_hard_tokens": null,
                    "remaining_percent": null,
                    "recommended_rewind_limit": null,
                    "rewind_only_limit": null,
                    "hard_limit": hard_context_window,
                    "rewind_only": false,
                    "density_pressure": false,
                    "density_maintenance_recommended": false,
                    "normal_tools_allowed": true,
                    "broad_followup_allowed": true,
                    "narrow_inflight_validation_allowed": true,
                    "required_action": "continue_after_rewind_refresh_pending",
                    "message": "A managed-context rewind just succeeded, but Codex has not reported a fresh backend token count yet. The previous pressure reading is stale and must not trigger another recovery or density handoff. Continue the queued follow-up from the rewound context; if a later backend-reported status reaches rewind_only=true, recover then.",
                    "managed_context": Self::managed_context_mode(managed_context),
                    "last_rewind_insufficient": null,
                    "density_maintenance_satisfied": null,
                    "pending_rewind_record_id": record_id,
                    "stale_after_rewind": true,
                });
            }
        }
        if context_window == 0 {
            return serde_json::json!({
                "source": "backend_reported",
                "status": "unknown",
                "used_tokens": used_tokens,
                "context_window": null,
                "effective_context_window": null,
                "remaining_tokens": null,
                "remaining_hard_tokens": null,
                "remaining_percent": null,
                "recommended_rewind_limit": null,
                "rewind_only_limit": null,
                "hard_limit": null,
                "rewind_only": false,
                "density_pressure": false,
                "density_maintenance_recommended": false,
                "normal_tools_allowed": true,
                "broad_followup_allowed": true,
                "narrow_inflight_validation_allowed": true,
                "required_action": "continue",
                "message": "Backend-reported context pressure is unavailable. Normal tools are allowed unless a later status reports rewind_only=true; continue ordinary work while pressure is unknown.",
                "managed_context": Self::managed_context_mode(managed_context),
                "last_rewind_insufficient": null,
                "density_maintenance_satisfied": null,
            });
        }

        let recommended_rewind_limit =
            (context_window as f64 * CONTEXT_PRESSURE_REWIND_THRESHOLD_PCT / 100.0).floor() as u64;
        let rewind_only_limit = context_window;
        let remaining_tokens = context_window.saturating_sub(used_tokens);
        let remaining_percent = (remaining_tokens as f64 / context_window as f64 * 100.0).max(0.0);
        let remaining_hard_tokens =
            hard_context_window.map(|hard| hard.saturating_sub(used_tokens));
        let status = if hard_context_window.is_some_and(|hard| hard > 0 && used_tokens >= hard) {
            "critical"
        } else if used_tokens >= rewind_only_limit {
            "high"
        } else if used_tokens >= recommended_rewind_limit {
            "watch"
        } else {
            "ok"
        };
        let rewind_only = managed_context && (status == "high" || status == "critical");
        let density_pressure = used_tokens >= recommended_rewind_limit;
        let density_maintenance_satisfied = if managed_context && density_pressure && !rewind_only {
            self.density_maintenance_satisfied_for(
                session_id,
                used_tokens,
                recommended_rewind_limit,
                rewind_only_limit,
            )
        } else {
            None
        };
        let density_maintenance_recommended = managed_context
            && density_pressure
            && !rewind_only
            && density_maintenance_satisfied.is_none();
        let normal_tools_allowed = !rewind_only;
        let broad_followup_allowed = normal_tools_allowed && !density_maintenance_recommended;
        let narrow_inflight_validation_allowed = normal_tools_allowed;
        let required_action = if rewind_only {
            "rewind_context"
        } else if density_maintenance_recommended {
            "density_handoff_before_broad_work"
        } else if density_maintenance_satisfied.is_some() {
            "continue_after_density_rewind"
        } else if density_pressure {
            "continue_or_rewind_optional"
        } else {
            "continue"
        };
        let message = if rewind_only {
            "Managed context is in rewind-only mode. Use rewind_context before ordinary model-facing tools."
        } else if density_maintenance_satisfied.is_some() {
            "Managed context is above the recommended density threshold but below the rewind-only limit. A successful managed-context rewind already satisfied the current density handoff; continue the concrete follow-up work, and only repeat density maintenance after the round completes or if pressure reaches rewind-only."
        } else if density_pressure {
            if managed_context {
                "Managed context is above the recommended density threshold but below the rewind-only limit. Normal tools remain allowed for status/anchor inspection and one narrow in-flight validation or build to finish, but before broad follow-up work perform exact-anchor density maintenance when it materially improves density, or produce a concise no-rewind density handoff. Fission tools stay allowed at watch: delegating separable work to a fission branch is itself a valid density action."
            } else {
                "Context is above the recommended density threshold but below the rewind-only limit. Normal tools are allowed; at handoff or before broad follow-up work, exact-anchor density maintenance is optional only if it materially improves density."
            }
        } else {
            "Context is below the recommended density threshold. Normal tools are allowed and normal work continues. Routinely pruning a recent genuinely noisy or unexpectedly large output whose durable facts are already crystallized is normal at this pressure; do not browse anchors without such a noisy trigger."
        };

        serde_json::json!({
            "source": "backend_reported",
            "status": status,
            "used_tokens": used_tokens,
            "context_window": context_window,
            "effective_context_window": context_window,
            "remaining_tokens": remaining_tokens,
            "remaining_hard_tokens": remaining_hard_tokens,
            "remaining_percent": remaining_percent,
            "recommended_rewind_limit": recommended_rewind_limit,
            "rewind_only_limit": rewind_only_limit,
            "hard_limit": hard_context_window,
            "rewind_only": rewind_only,
            "density_pressure": density_pressure,
            "density_maintenance_recommended": density_maintenance_recommended,
            "normal_tools_allowed": normal_tools_allowed,
            "broad_followup_allowed": broad_followup_allowed,
            "narrow_inflight_validation_allowed": narrow_inflight_validation_allowed,
            "required_action": required_action,
            "message": message,
            "managed_context": Self::managed_context_mode(managed_context),
            "last_rewind_insufficient": self.insufficient_rewind_notice_for(session_id).map(|notice| {
                serde_json::json!({
                    "record_id": notice.record_id,
                    "used_tokens": notice.used_tokens,
                    "rewind_only_limit": notice.rewind_only_limit,
                    "context_window": notice.context_window,
                    "message": "The previous managed-context rewind did not reduce backend-reported pressure enough. If a current recovery catalog page is already in view, do not re-list: choose a deeper exact item_id from it now. Otherwise call list_rewind_anchors once to inspect recovery candidates; pass include_non_recovery=true only for diagnostics, and never pass a recovery_eligible=false audit row to rewind_context. Use inspect_rewind_anchor when a compact row is ambiguous, then choose an exact returned item_id and position whose row or inspection supports enough pruning, with a denser carry-forward primer before using ordinary tools.",
                })
            }),
            "density_maintenance_satisfied": density_maintenance_satisfied.map(|satisfied| {
                serde_json::json!({
                    "record_id": satisfied.record_id,
                    "used_tokens": satisfied.used_tokens,
                    "recommended_rewind_limit": satisfied.recommended_rewind_limit,
                    "rewind_only_limit": satisfied.rewind_only_limit,
                    "round": satisfied.round,
                    "valid_until": "round_complete_or_rewind_only",
                    "message": "A successful managed-context rewind already satisfied the current density handoff; forward progress is allowed until the round completes while pressure remains below rewind-only.",
                })
            }),
        })
    }

    #[allow(dead_code)]
    pub(crate) fn context_pressure_snapshot(&self) -> serde_json::Value {
        self.context_pressure_snapshot_for(None, None)
    }

    pub(crate) fn is_active_codex_session(&self) -> bool {
        self.active_session_source
            .as_deref()
            .is_some_and(|source| source.eq_ignore_ascii_case("codex"))
    }

    pub(crate) fn active_codex_managed_context_enabled(&self) -> bool {
        self.is_active_codex_session() && self.codex_managed_context
    }

    pub(crate) fn exposed_codex_managed_context_enabled(&self) -> bool {
        if self.is_active_codex_session() {
            self.codex_managed_context
        } else {
            self.configured_codex_managed_context
        }
    }

    pub(crate) fn exposed_codex_managed_context_enabled_for(
        &self,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> bool {
        if let Some(enabled) = managed_context_override {
            return enabled;
        }
        if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
            for related in self.session_related_ids(id) {
                if let Some(enabled) = self.session_codex_managed_context.get(&related) {
                    return *enabled;
                }
            }
        }
        self.exposed_codex_managed_context_enabled()
    }

    pub(crate) fn active_codex_managed_context_enabled_for(
        &self,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> bool {
        if let Some(enabled) = managed_context_override {
            return enabled;
        }
        if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
            for related in self.session_related_ids(id) {
                if let Some(enabled) = self.session_codex_managed_context.get(&related) {
                    return *enabled;
                }
            }
        }
        self.active_codex_managed_context_enabled()
    }

    pub(crate) fn context_pressure_rewind_only_for(
        &self,
        session_id: Option<&str>,
    ) -> Option<(u64, u64, &'static str)> {
        let (used_tokens, context_window, hard_context_window) =
            self.session_usage_values(session_id);
        if context_window == 0 {
            return None;
        }
        let rewind_only_limit = context_window;
        let status = if hard_context_window.is_some_and(|hard| hard > 0 && used_tokens >= hard) {
            "critical"
        } else if used_tokens >= rewind_only_limit {
            "high"
        } else {
            return None;
        };
        Some((used_tokens, rewind_only_limit, status))
    }

    pub(crate) fn context_pressure_density_watch_for(
        &self,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> bool {
        if !self.active_codex_managed_context_enabled_for(session_id, managed_context_override) {
            return false;
        }
        let (used_tokens, context_window, hard_context_window) =
            self.session_usage_values(session_id);
        if context_window == 0
            || used_tokens >= context_window
            || hard_context_window.is_some_and(|hard| hard > 0 && used_tokens >= hard)
        {
            return false;
        }
        let recommended_rewind_limit =
            (context_window as f64 * CONTEXT_PRESSURE_REWIND_THRESHOLD_PCT / 100.0).floor() as u64;
        used_tokens >= recommended_rewind_limit
    }

    pub(crate) fn rewind_anchor_recovery_candidates_only_for(
        &self,
        _session_id: Option<&str>,
        _requested: Option<bool>,
        include_non_recovery: bool,
    ) -> bool {
        !include_non_recovery
    }

    #[allow(dead_code)]
    pub(crate) fn rewind_only_gate_message(&self, tool_name: &str) -> Option<String> {
        self.rewind_only_gate_message_for(tool_name, None, None)
    }

    pub(crate) fn rewind_only_gate_message_for(
        &self,
        tool_name: &str,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> Option<String> {
        if !self.active_codex_managed_context_enabled_for(session_id, managed_context_override)
            || rewind_only_allowed_tool(tool_name)
        {
            return None;
        }
        let (used_tokens, rewind_only_limit, status) =
            self.context_pressure_rewind_only_for(session_id)?;
        let mut message = format!(
            "Backend-reported Codex context pressure is {status} ({used_tokens}/{rewind_only_limit} tokens). Managed context is now in density-preservation mode: model-facing tools are limited to get_status, list_rewind_anchors, inspect_rewind_anchor, rewind_context, and rewind_backout until pressure is reduced below the threshold. Read-only supervisor observability tools such as get_logs and controller status remain available. The Intendant MCP tools list_rewind_anchors and inspect_rewind_anchor are available; any earlier transcript claim that either is unavailable is stale. If a current recovery catalog page is already in view, do not re-list: choose one exact item_id from it and call rewind_context now. Otherwise call list_rewind_anchors once to inspect the compact valid recovery catalog; pass include_non_recovery=true only for diagnostics, and never pass a recovery_eligible=false audit row to rewind_context. Inspect a candidate if the compact row is ambiguous, then call rewind_context with an exact returned item_id, the returned position_hint or a value in positions, and a dense carry-forward primer before using other tools. A successful rewind only validates lineage; normal tools remain unavailable until backend-reported pressure is below the rewind-only limit. Do not synthesize anchor ids from prior failed tool calls."
        );
        if let Some(notice) = self.insufficient_rewind_notice_for(session_id) {
            message.push_str(&format!(
                " Previous managed-context record {} was insufficient; choose an exact returned item_id and position from list_rewind_anchors whose compact row or inspection supports enough additional pruning, with a denser carry-forward primer.",
                notice.record_id
            ));
        }
        Some(message)
    }

    pub(crate) fn approval_snapshot(&self) -> Option<ApprovalSnapshot> {
        self.pending_approval.as_ref().map(|p| ApprovalSnapshot {
            id: p.id,
            command_preview: p.command_preview.clone(),
            category: p.category.clone(),
        })
    }

    pub(crate) fn human_question_snapshot(&self) -> Option<HumanQuestionSnapshot> {
        self.human_question.as_ref().map(|q| HumanQuestionSnapshot {
            question: q.clone(),
        })
    }
}

pub type SharedMcpState = Arc<RwLock<McpAppState>>;

pub(crate) fn phase_to_str(phase: &Phase) -> &'static str {
    match phase {
        Phase::Thinking => "thinking",
        Phase::RunningAgent => "running_agent",
        Phase::Orchestrating => "orchestrating",
        Phase::WaitingApproval => "waiting_approval",
        Phase::WaitingHuman => "waiting_human",
        Phase::WaitingFollowUp => "waiting_follow_up",
        Phase::Idle => "idle",
        Phase::Done => "done",
        Phase::Interrupting => "interrupting",
        Phase::Interrupted => "interrupted",
    }
}

pub(crate) fn phase_from_status_str(phase: &str) -> Phase {
    match phase.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "thinking" => Phase::Thinking,
        "running" | "running_agent" => Phase::RunningAgent,
        "orchestrating" => Phase::Orchestrating,
        "waiting_approval" => Phase::WaitingApproval,
        "waiting_human" => Phase::WaitingHuman,
        "waiting_follow_up" | "waiting_followup" => Phase::WaitingFollowUp,
        "done" | "completed" => Phase::Done,
        "interrupting" => Phase::Interrupting,
        "interrupted" => Phase::Interrupted,
        _ => Phase::Idle,
    }
}

pub(crate) fn status_task_is_external_turn_progress(task: &str) -> bool {
    let normalized = task.trim().to_ascii_lowercase();
    (normalized.contains("turn") || normalized.contains("round"))
        && normalized.contains("in progress")
}

pub(crate) fn verbosity_to_str(v: Verbosity) -> &'static str {
    match v {
        Verbosity::Quiet => "quiet",
        Verbosity::Normal => "normal",
        Verbosity::Verbose => "verbose",
        Verbosity::Debug => "debug",
    }
}

pub(crate) fn parse_verbosity(s: &str) -> Option<Verbosity> {
    match s.to_lowercase().as_str() {
        "quiet" => Some(Verbosity::Quiet),
        "normal" => Some(Verbosity::Normal),
        "verbose" => Some(Verbosity::Verbose),
        "debug" => Some(Verbosity::Debug),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Event listener: consumes AppEvents and updates shared state
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};
    use crate::mcp::tests::test_state;

    #[test]
    fn managed_ok_context_pressure_allows_noise_triggered_pruning() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "codex-thread".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;
        s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
            provider: "openai".to_string(),
            model: "gpt-5.2-codex".to_string(),
            tokens_used: 42_000,
            context_window: 258_400,
            hard_context_window: Some(272_000),
            usage_pct: 16.3,
            prompt_tokens: 40_000,
            completion_tokens: 2_000,
            cached_tokens: 0,
            ..Default::default()
        });

        let pressure = s.context_pressure_snapshot();
        assert_eq!(pressure["status"], "ok");
        assert_eq!(pressure["rewind_only"], false);
        assert_eq!(pressure["normal_tools_allowed"], true);
        assert_eq!(pressure["required_action"], "continue");
        let message = pressure["message"].as_str().unwrap_or_default();
        // Normal work continues, and noise-triggered pruning is routine at ok
        // pressure — what needs a trigger is anchor browsing, not hygiene.
        assert!(message.contains("normal work continues"));
        assert!(message.contains("genuinely noisy or unexpectedly large"));
        assert!(message.contains("normal at this pressure"));
        assert!(message.contains("without such a noisy trigger"));
        assert!(!message.contains("no rewind preparation is needed"));
        assert!(!message.contains("list_rewind_anchors"));
    }

    #[test]
    fn insufficient_rewind_notice_is_session_scoped() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "session-a".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;
        s.session_codex_managed_context
            .insert("session-a".to_string(), true);
        s.session_codex_managed_context
            .insert("session-b".to_string(), true);

        // The structured record id wins over a conflicting id embedded in the
        // human-readable message.
        s.note_context_rewind_result_for(
            Some("session-b"),
            true,
            Some("rewind-b"),
            "Rewound Codex thread and saved record rewind-stale.",
        );
        s.session_usage.insert(
            "session-b".to_string(),
            frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 101_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 101.0,
                prompt_tokens: 97_000,
                completion_tokens: 4_000,
                cached_tokens: 0,
                ..Default::default()
            },
        );
        s.complete_pending_rewind_pressure_check_for(Some("session-b"));

        assert_eq!(
            s.context_pressure_snapshot_for(Some("session-a"), None)
                .pointer("/last_rewind_insufficient"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(
            s.context_pressure_snapshot_for(Some("session-b"), None)
                .pointer("/last_rewind_insufficient/record_id"),
            Some(&serde_json::Value::String("rewind-b".to_string()))
        );
    }

    #[test]
    fn resolve_pending_approval_approve() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.approval_registry.lock().unwrap().insert(1, tx);
            s.pending_approval = Some(PendingApprovalState {
                id: 1,
                command_preview: "rm -rf /tmp".to_string(),
                category: "destructive".to_string(),
            });

            let outcome = resolve_pending_approval(&mut s, ApprovalResponse::Approve);
            assert_eq!(outcome, ActionOutcome::Ok);
            assert!(s.pending_approval.is_none());
            assert_eq!(s.phase, Phase::RunningAgent);

            // Check the oneshot received the response
            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::Approve);
        });
    }

    #[test]
    fn resolve_pending_approval_deny() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.approval_registry.lock().unwrap().insert(2, tx);
            s.pending_approval = Some(PendingApprovalState {
                id: 2,
                command_preview: "curl evil.com".to_string(),
                category: "network".to_string(),
            });

            let outcome = resolve_pending_approval(&mut s, ApprovalResponse::Deny);
            assert_eq!(outcome, ActionOutcome::Ok);
            assert_eq!(s.phase, Phase::Done);

            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::Deny);
        });
    }

    #[test]
    fn resolve_pending_approval_skip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.approval_registry.lock().unwrap().insert(3, tx);
            s.pending_approval = Some(PendingApprovalState {
                id: 3,
                command_preview: "test".to_string(),
                category: "exec".to_string(),
            });

            let outcome = resolve_pending_approval(&mut s, ApprovalResponse::Skip);
            assert_eq!(outcome, ActionOutcome::Ok);
            assert_eq!(s.phase, Phase::RunningAgent);

            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::Skip);
        });
    }

    #[test]
    fn resolve_pending_approval_approve_all() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.approval_registry.lock().unwrap().insert(4, tx);
            s.pending_approval = Some(PendingApprovalState {
                id: 4,
                command_preview: "ls".to_string(),
                category: "exec".to_string(),
            });

            let outcome = resolve_pending_approval(&mut s, ApprovalResponse::ApproveAll);
            assert_eq!(outcome, ActionOutcome::Ok);

            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::ApproveAll);
        });
    }

    #[test]
    fn phase_to_str_all_variants() {
        assert_eq!(phase_to_str(&Phase::Thinking), "thinking");
        assert_eq!(phase_to_str(&Phase::RunningAgent), "running_agent");
        assert_eq!(phase_to_str(&Phase::Orchestrating), "orchestrating");
        assert_eq!(phase_to_str(&Phase::WaitingApproval), "waiting_approval");
        assert_eq!(phase_to_str(&Phase::WaitingHuman), "waiting_human");
        assert_eq!(phase_to_str(&Phase::WaitingFollowUp), "waiting_follow_up");
        assert_eq!(phase_to_str(&Phase::Idle), "idle");
        assert_eq!(phase_to_str(&Phase::Done), "done");
        assert_eq!(phase_to_str(&Phase::Interrupting), "interrupting");
        assert_eq!(phase_to_str(&Phase::Interrupted), "interrupted");
    }

    #[test]
    fn parse_verbosity_all_variants() {
        assert_eq!(parse_verbosity("quiet"), Some(Verbosity::Quiet));
        assert_eq!(parse_verbosity("normal"), Some(Verbosity::Normal));
        assert_eq!(parse_verbosity("verbose"), Some(Verbosity::Verbose));
        assert_eq!(parse_verbosity("debug"), Some(Verbosity::Debug));
        assert_eq!(parse_verbosity("QUIET"), Some(Verbosity::Quiet));
        assert_eq!(parse_verbosity("unknown"), None);
    }

    #[test]
    fn approval_snapshot_present_when_set() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.pending_approval = Some(PendingApprovalState {
                id: 42,
                command_preview: "rm -rf /".to_string(),
                category: "destructive".to_string(),
            });
            let snap = s.approval_snapshot().unwrap();
            assert_eq!(snap.id, 42);
            assert_eq!(snap.category, "destructive");
        });
    }

    #[test]
    fn over_cap_prune_drops_only_the_ended_components_cursors() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        let cursor = SessionJsonlCursor {
            lines: 2,
            bytes: 64,
            prefix_len: 32,
            prefix_hash: 7,
        };
        let ended_path = std::path::PathBuf::from("/tmp/logs/sess-ended/session.jsonl");
        let live_path = std::path::PathBuf::from("/tmp/logs/sess-live/session.jsonl");
        s.session_log_hydration_cursors
            .insert(ended_path.clone(), cursor);
        s.session_log_hydration_cursors
            .insert(live_path.clone(), cursor);
        s.link_session_aliases("sess-ended", "backend-ended");

        for i in 0..=ENDED_SESSION_PRUNE_THRESHOLD {
            s.note_session_phase(Some(&format!("filler-{i}")), Some(1), Phase::Thinking, None);
        }
        s.note_session_phase(Some("sess-ended"), Some(2), Phase::Thinking, None);
        s.note_session_ended("sess-ended");

        // Only the ended component's cursor is dropped; a long-lived daemon
        // permanently over the cap must not wipe live sessions' cursors on
        // every unrelated session end.
        assert!(!s.session_log_hydration_cursors.contains_key(&ended_path));
        assert!(s.session_log_hydration_cursors.contains_key(&live_path));
    }

    #[test]
    fn raw_status_cache_store_respects_invalidation_generation() {
        let s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        let loop_dir = tempfile::tempdir().unwrap();
        let (hit, generation) = s.probe_controller_loop_raw_status(loop_dir.path());
        assert!(hit.is_none());

        let raw = collect_controller_loop_raw_status(loop_dir.path(), loop_dir.path());
        s.store_controller_loop_raw_status_at(generation, raw.clone());
        assert!(s
            .probe_controller_loop_raw_status(loop_dir.path())
            .0
            .is_some());

        // Invalidation bumps the generation; a sample collected before the
        // bump (a lock-free pre-warm racing a lifecycle mutation) must not
        // be reinstated.
        s.invalidate_controller_loop_raw_status_cache();
        let (hit, new_generation) = s.probe_controller_loop_raw_status(loop_dir.path());
        assert!(hit.is_none());
        assert_ne!(generation, new_generation);
        s.store_controller_loop_raw_status_at(generation, raw);
        assert!(s
            .probe_controller_loop_raw_status(loop_dir.path())
            .0
            .is_none());
    }

    #[test]
    fn ended_sessions_keep_done_status_under_cap_and_prune_over_it() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );

        // Under the cap: an ended session's status (and aliases) stay
        // resident so post-end status/log queries answer from memory.
        s.link_session_aliases("wrapper-under", "backend-under");
        s.note_session_phase(Some("wrapper-under"), Some(3), Phase::Thinking, None);
        s.note_session_ended("wrapper-under");
        assert_eq!(
            s.session_status_for_id("backend-under")
                .map(|st| st.phase.clone()),
            Some(Phase::Done)
        );

        // Blow past the cap with unrelated sessions, then end one: its
        // whole related-id component must be pruned from the bookkeeping.
        for i in 0..=ENDED_SESSION_PRUNE_THRESHOLD {
            s.note_session_phase(Some(&format!("filler-{i}")), Some(1), Phase::Thinking, None);
        }
        s.link_session_aliases("wrapper-over", "backend-over");
        s.note_session_phase(Some("wrapper-over"), Some(5), Phase::Thinking, None);
        s.session_sources
            .insert("wrapper-over".to_string(), "codex".to_string());
        s.note_session_ended("wrapper-over");
        assert!(s.session_status_for_id("wrapper-over").is_none());
        assert!(s.session_status_for_id("backend-over").is_none());
        assert!(s.session_source_for_id("wrapper-over").is_none());
        assert!(!s.session_aliases.contains_key("wrapper-over"));
        assert!(!s.session_aliases.contains_key("backend-over"));
    }
}

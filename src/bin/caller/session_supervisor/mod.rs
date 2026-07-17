//! Daemon-side session lifecycle supervisor.
//!
//! The supervisor is the long-lived owner for sessions launched from the
//! control plane. It accepts `StartTask`, `ResumeSession`, and targeted
//! follow-up commands from the shared `EventBus`, creates per-session runtime
//! resources, and tracks the follow-up channel for each managed session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use super::*;

mod launch;
pub(crate) use launch::*;
mod routing;
mod sub_agents;
pub(crate) use routing::*;
mod agent_config;
pub(crate) use agent_config::*;
mod dispatch;
mod fork;
mod registry;

#[derive(Clone)]
pub struct SessionSupervisorConfig {
    pub bus: EventBus,
    /// The daemon's default project for sessions that don't carry their
    /// own. `None` = projectless daemon (launch dir had no project
    /// marker): creating or resuming a session then *requires* an
    /// explicit project root, and a CreateSession without one fails
    /// with the structured `no_project` error kind instead of silently
    /// adopting the launch cwd.
    pub project_root: Option<PathBuf>,
    pub autonomy: SharedAutonomy,
    pub shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    pub shared_codex_config: control_plane::SharedCodexConfig,
    pub shared_claude_config: control_plane::SharedClaudeConfig,
    pub frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    /// Live display sessions, when the daemon runs a display pipeline. CU
    /// screenshots prefer their in-memory frames over subprocess capture.
    pub session_registry: Option<display::SharedSessionRegistry>,
    /// Federated peer registry, when the daemon runs the web gateway.
    /// Backs the native `peer` tool in supervised sessions; None makes
    /// the tool answer with a federation-inactive note.
    pub peer_registry: Option<peer::PeerRegistry>,
    pub web_port: Option<u16>,
    pub flags_direct: bool,
    pub shared_session: Option<web_gateway::SharedActiveSession>,
    /// Injection point for native-session providers: when set, in-process
    /// sessions construct their ChatProvider from this factory instead of
    /// `provider::select_provider()` (which needs API keys). None in
    /// production; tests use it to run the loop against a mock provider.
    pub provider_factory: Option<Arc<dyn Fn() -> Box<dyn provider::ChatProvider> + Send + Sync>>,
    /// Injection point for the persisted-session home: resume/attach
    /// resolution (wrapper logs, the wrapper index, persisted launch
    /// configs) reads from here. None in production (the real home); tests
    /// pin it so a machine's live `~/.intendant` session history cannot
    /// change what they observe — a hardcoded wrapper id in a test can
    /// otherwise resolve against a real session log and flip the flow
    /// from follow-up routing to a fresh resume dispatch.
    pub logs_home_override: Option<PathBuf>,
    /// Git-vitals target registry: the supervisor registers each managed
    /// session's effective project root (the worktree checkout for
    /// worktree sessions) at launch, which is what puts the dirty /
    /// merge-parity / unpushed rows on dashboard-spawned sessions.
    /// `SessionEnded` prunes on the producer side. None when the daemon
    /// runs without the vitals producer (no web frontends).
    pub git_vitals_targets: Option<crate::session_vitals::GitVitalsTargets>,
    /// Daemon-owned IAM directory used to revalidate internal hosted lease
    /// provenance before a hosted-created session becomes an eligible target.
    /// None in hermetic tests and non-hosted execution shapes.
    pub hosted_control_cert_dir: Option<PathBuf>,
}

#[derive(Clone)]
pub struct SessionSupervisor {
    config: Arc<SessionSupervisorConfig>,
    state: Arc<AsyncMutex<SupervisorState>>,
}

const EXTERNAL_ATTACH_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const SESSION_STOP_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const SESSION_RESTART_DEDUPE_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
/// Bound on the peer-delegation dedup ledger (`delegation_receipts`).
/// Entries only need to outlive the delegating side's bounded re-send
/// window (~30 s), so a FIFO of this size is generous; the bound keeps
/// a peer that mints endless delegation ids from growing the map.
const MAX_DELEGATION_RECEIPTS: usize = 128;
const EXTERNAL_ATTACH_DEDUPE_WINDOW: std::time::Duration = EXTERNAL_ATTACH_READY_TIMEOUT;
/// Freshness window for [`SupervisorState::unmanaged_user_halts`]. Wide
/// enough to outlive a slow event lane's round trip (the observed live
/// escalation arrived 13s after the prompt; polling fallback lanes are
/// slower), narrow enough that a stale mark cannot block work minutes
/// later — and any newer prompt or deliberate resume clears it early.
const UNMANAGED_USER_HALT_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(not(test))]
const EDIT_ATTACH_ROUTE_TIMEOUT: std::time::Duration = EXTERNAL_ATTACH_READY_TIMEOUT;
#[cfg(test)]
const EDIT_ATTACH_ROUTE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
const EDIT_ATTACH_ROUTE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);
#[cfg(not(test))]
const TEXT_STEER_FALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const TEXT_STEER_FALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(20);

#[derive(Default)]
pub(crate) struct SupervisorState {
    sessions: HashMap<String, ManagedSession>,
    session_aliases: HashMap<String, String>,
    related_sessions: HashMap<String, RelatedSession>,
    active_session_id: Option<String>,
    next_session_instance: u64,
    restart_dedupe: HashMap<String, std::time::Instant>,
    external_attach_dedupe: HashMap<String, std::time::Instant>,
    /// Ids (wrapper AND native) of every external session that announced a
    /// SessionIdentity on this bus — including sessions the supervisor does
    /// NOT manage, like the CLI main loop's own agent. The thread-action
    /// fallback responder stays silent for these: their owning drain
    /// answers, and a false "not attached" here would race a real result.
    known_external_sessions: std::collections::HashSet<String>,
    /// Thread-action ops each session's live loop advertised via
    /// `SessionCapabilities` (native sessions advertise the goal* family).
    /// The fallback responder defers to the advertising loop for exactly
    /// these ops instead of false-rejecting non-external sessions.
    advertised_thread_actions: HashMap<String, std::collections::HashSet<String>>,
    /// Peer-delegation dedup ledger: delegation id → the session the
    /// task was dispatched as. A `StartTask` re-sent with an
    /// already-recorded `delegation_id` (the delegating daemon's
    /// at-least-once retry after a connection drop) re-acks with the
    /// original session instead of starting a duplicate task. Bounded
    /// by [`MAX_DELEGATION_RECEIPTS`], oldest-accepted evicted
    /// (tracked in `delegation_receipt_order`).
    delegation_receipts: HashMap<String, String>,
    delegation_receipt_order: std::collections::VecDeque<String>,
    /// Session ids the user explicitly halted (interrupt / stop) while no
    /// session here answered to them, with the halt time. A frontend
    /// auto-attach escalation (`ResumeSession { auto_attach: true, task:
    /// Some(..) }`) arriving inside [`UNMANAGED_USER_HALT_WINDOW`] is
    /// cancelled instead of launching the very work the user tried to halt;
    /// any newer follow-up or deliberate resume for the id clears the mark
    /// (latest intent wins).
    unmanaged_user_halts: HashMap<String, std::time::Instant>,
}

#[derive(Debug, Clone)]
pub(crate) struct RelatedSession {
    parent_session_id: String,
    relationship: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelatedSessionRecord {
    parent_session_id: String,
    child_session_id: String,
    relationship: String,
}

/// Default cap on concurrently running sub-agent children per parent
/// session when `[orchestrator] max_parallel_agents` is not set.
const DEFAULT_MAX_PARALLEL_SUB_AGENTS: usize = 4;

/// Maximum delegation depth below a root session: a root (depth 0) can
/// spawn workers (depth 1), which may themselves delegate once more
/// (depth 2); deeper spawns are refused. Uncapped depth let confused
/// children re-delegate their own task in an unbounded chain (observed
/// live before the cap).
const MAX_SUB_AGENT_DEPTH: usize = 2;

/// Launch parameters for a supervised sub-agent session (the
/// `spawn_sub_agent` tool).
pub struct SubAgentSpawnParams {
    pub task: String,
    /// Resolves the child's system prompt (SysPrompt role files); custom
    /// strings fall back to the base prompt.
    pub role: sub_agent::SubAgentRole,
    /// Replaces the role's file-resolved system prompt wholesale.
    pub system_prompt: Option<String>,
    /// `None` runs the native in-process loop; `Some` supervises an
    /// external coding agent as the worker.
    pub backend: Option<external_agent::AgentBackend>,
    /// Isolate the child in a fresh git worktree branched off the parent
    /// project's HEAD.
    pub worktree: bool,
    /// Inject the project knowledge store into the child's conversation.
    pub name: Option<String>,
}

/// What `start_sub_agent_session` hands back to the spawning loop.
pub struct SubAgentSpawnStarted {
    pub child_session_id: String,
    pub child_name: String,
    pub worktree_path: Option<PathBuf>,
    pub completion_rx: oneshot::Receiver<SubAgentCompletion>,
}

/// Terminal report for a sub-agent child, resolved when the child session
/// finishes (submitted via the submit_result tool, or synthesized from the
/// child's final state).
#[derive(Debug)]
pub struct SubAgentCompletion {
    pub child_session_id: String,
    pub name: String,
    pub result: sub_agent::SubAgentResult,
}

/// A child spawned by a session, tracked on the parent side by the
/// spawn_sub_agent / wait_sub_agents tool handlers.
pub struct SubAgentChild {
    pub name: String,
    /// Pending completion; present until the child finishes.
    pub rx: Option<oneshot::Receiver<SubAgentCompletion>>,
    /// Resolved completion not yet returned through a wait call.
    pub completed: Option<SubAgentCompletion>,
    /// The completion was already returned by a wait call.
    pub delivered: bool,
}

/// Per-session registry of spawned sub-agent children, keyed by child
/// session id. One instance is shared between the session's in-loop
/// orchestration handle (the spawn/wait tool handlers) and the
/// supervisor's `ManagedSession` entry, so dashboard-delegated children
/// land in the same registry the model's wait_sub_agents reads.
pub type SubAgentChildrenMap = Arc<std::sync::Mutex<HashMap<String, SubAgentChild>>>;

/// Orchestration handle carried by every supervised native session. Grants
/// the in-process loop the spawn capability — any supervised internal
/// session may delegate; orchestration is a capability, not a role — and,
/// for sessions that are themselves sub-agents, the submit_result slot.
#[derive(Clone)]
pub struct SessionOrchestration {
    pub supervisor: SessionSupervisor,
    pub session_id: String,
    /// How many spawn generations below a root session this session sits
    /// (0 = root). Spawns beyond `MAX_SUB_AGENT_DEPTH` are refused.
    pub depth: usize,
    /// `Some` when this session was spawned as a sub-agent: the structured
    /// result the child submits via the submit_result tool.
    pub submitted_result: Option<Arc<std::sync::Mutex<Option<sub_agent::SubAgentResult>>>>,
    /// Children this session has spawned, keyed by child session id.
    /// Shared with the supervisor's `ManagedSession` entry (dashboard
    /// delegation inserts here too).
    pub children: SubAgentChildrenMap,
}

/// Internal wiring `spawn_agent_session` needs to run a session as a
/// sub-agent child: launch config for the native loop plus the result slot
/// and completion channel back to the parent.
pub(crate) struct SubAgentWiring {
    completion_tx: oneshot::Sender<SubAgentCompletion>,
    submitted_result: Arc<std::sync::Mutex<Option<sub_agent::SubAgentResult>>>,
    child_name: String,
    role: sub_agent::SubAgentRole,
    system_prompt: Option<String>,
    /// The child's delegation depth (parent depth + 1).
    depth: usize,
}

struct ManagedSession {
    session_id: String,
    source: String,
    name: Option<String>,
    phase: String,
    project_root: PathBuf,
    session_dir: PathBuf,
    follow_up_tx: mpsc::Sender<FollowUpMessage>,
    approval_registry: event::ApprovalRegistry,
    instance_id: u64,
    finished_rx: Option<oneshot::Receiver<()>>,
    /// How many delegation levels below a root session this session runs
    /// (0 = root); dashboard delegation enforces the same depth cap the
    /// spawn_sub_agent tool does.
    depth: usize,
    /// Native sessions: the same children registry the session's in-loop
    /// wait_sub_agents reads (dashboard delegation inserts into it).
    /// `None` for external-agent sessions — they manage their own
    /// sub-agents through their injected start_task tool.
    sub_agent_children: Option<SubAgentChildrenMap>,
}

pub(crate) struct StoppedManagedSession {
    session_id: String,
    source: String,
    finished_rx: Option<oneshot::Receiver<()>>,
}

#[derive(Clone)]
pub(crate) struct EditRouteTarget {
    managed_id: String,
    source: String,
    project_root: PathBuf,
    session_dir: PathBuf,
    follow_up_tx: mpsc::Sender<FollowUpMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditAttachRequest {
    source: String,
    resume_id: Option<String>,
    project_root: Option<String>,
    direct: Option<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct EditUserMessageRequest {
    requested_id: String,
    user_turn_index: u32,
    user_turn_revision: Option<u32>,
    original_text: Option<String>,
    text: String,
    attachments: Vec<String>,
}

impl SupervisorState {
    fn resolve_session_id(&self, session_id: &str) -> Option<String> {
        if self.sessions.contains_key(session_id) {
            return Some(session_id.to_string());
        }

        let mut current = session_id;
        for _ in 0..8 {
            let next = self.session_aliases.get(current)?;
            if self.sessions.contains_key(next) {
                return Some(next.clone());
            }
            if next == current {
                return None;
            }
            current = next;
        }
        None
    }

    fn session_is_managed(&self, session_id: &str) -> bool {
        self.resolve_session_id(session_id).is_some()
    }

    fn apply_related_session(
        &mut self,
        parent_session_id: &str,
        child_session_id: &str,
        relationship: &str,
    ) -> bool {
        let relationship = relationship.trim().to_ascii_lowercase();
        if !matches!(relationship.as_str(), "side" | "subagent") {
            return false;
        }
        let parent = parent_session_id.trim();
        let child = child_session_id.trim();
        if parent.is_empty() || child.is_empty() || parent == child {
            return false;
        }
        let Some(parent_key) = self.resolve_session_id(parent) else {
            return false;
        };
        self.session_aliases
            .insert(child.to_string(), parent_key.clone());
        self.related_sessions.insert(
            child.to_string(),
            RelatedSession {
                parent_session_id: parent_key,
                relationship,
            },
        );
        true
    }

    fn remove_session(&mut self, session_id: &str) -> Option<(String, ManagedSession)> {
        let canonical = self.resolve_session_id(session_id)?;
        let removed = self.sessions.remove(&canonical)?;
        self.session_aliases
            .retain(|alias, target| alias != &canonical && target != &canonical);
        self.related_sessions
            .retain(|child, rel| child != &canonical && rel.parent_session_id != canonical);
        if self.active_session_id.as_deref() == Some(&canonical)
            || self.active_session_id.as_deref() == Some(session_id)
        {
            self.active_session_id = self.sessions.keys().next().cloned();
        }
        Some((canonical, removed))
    }

    fn remove_session_instance(
        &mut self,
        session_id: &str,
        instance_id: u64,
    ) -> Option<(String, ManagedSession)> {
        let canonical = self.resolve_session_id(session_id)?;
        if self
            .sessions
            .get(&canonical)
            .map(|session| session.instance_id != instance_id)
            .unwrap_or(true)
        {
            return None;
        }
        self.remove_session(&canonical)
    }

    fn mark_restart_requested(&mut self, key: &str) -> bool {
        let now = std::time::Instant::now();
        self.restart_dedupe
            .retain(|_, expires_at| *expires_at > now);
        if self.restart_dedupe.contains_key(key) {
            return false;
        }
        self.restart_dedupe
            .insert(key.to_string(), now + SESSION_RESTART_DEDUPE_WINDOW);
        true
    }

    fn mark_external_attach_requested(&mut self, keys: &[String]) -> bool {
        if keys.is_empty() {
            return false;
        }
        let now = std::time::Instant::now();
        self.external_attach_dedupe
            .retain(|_, expires_at| *expires_at > now);
        if keys
            .iter()
            .any(|key| self.external_attach_dedupe.contains_key(key))
        {
            return false;
        }
        let expires_at = now + EXTERNAL_ATTACH_DEDUPE_WINDOW;
        for key in keys {
            self.external_attach_dedupe
                .insert(key.to_string(), expires_at);
        }
        true
    }

    fn clear_external_attach_requested(&mut self, keys: &[String]) {
        for key in keys {
            self.external_attach_dedupe.remove(key);
        }
    }

    /// Record a user stop/interrupt aimed at ids no session here answers
    /// to (see the field docs on `unmanaged_user_halts`). Prunes expired
    /// marks as a side effect so the map cannot grow unbounded.
    fn mark_unmanaged_user_halts<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>) {
        let now = std::time::Instant::now();
        self.unmanaged_user_halts
            .retain(|_, at| now.duration_since(*at) < UNMANAGED_USER_HALT_WINDOW);
        for id in ids {
            let id = id.trim();
            if id.is_empty() {
                continue;
            }
            self.unmanaged_user_halts.insert(id.to_string(), now);
        }
    }

    /// Drop any user-halt marks for `ids`: a newer prompt or a deliberate
    /// resume supersedes an earlier halt (latest intent wins).
    fn clear_unmanaged_user_halts<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>) {
        for id in ids {
            self.unmanaged_user_halts.remove(id.trim());
        }
    }

    /// True when any of `ids` was user-halted within
    /// [`UNMANAGED_USER_HALT_WINDOW`]. Prunes expired marks as a side effect.
    fn unmanaged_user_halt_active<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>) -> bool {
        let now = std::time::Instant::now();
        self.unmanaged_user_halts
            .retain(|_, at| now.duration_since(*at) < UNMANAGED_USER_HALT_WINDOW);
        ids.into_iter()
            .any(|id| self.unmanaged_user_halts.contains_key(id.trim()))
    }

    /// The session a delegation id was already dispatched as, if any.
    fn recorded_delegation_session(&self, delegation_id: &str) -> Option<String> {
        self.delegation_receipts.get(delegation_id).cloned()
    }

    /// Record an accepted delegation for dedup, evicting the oldest
    /// entry beyond [`MAX_DELEGATION_RECEIPTS`]. First writer wins —
    /// a delegation id is never re-pointed at a different session.
    fn record_delegation(&mut self, delegation_id: &str, session_id: &str) {
        if self.delegation_receipts.contains_key(delegation_id) {
            return;
        }
        while self.delegation_receipt_order.len() >= MAX_DELEGATION_RECEIPTS {
            match self.delegation_receipt_order.pop_front() {
                Some(evicted) => {
                    self.delegation_receipts.remove(&evicted);
                }
                None => break,
            }
        }
        self.delegation_receipt_order
            .push_back(delegation_id.to_string());
        self.delegation_receipts
            .insert(delegation_id.to_string(), session_id.to_string());
    }
}

impl SessionSupervisor {
    pub fn new(config: SessionSupervisorConfig) -> Self {
        Self {
            config: Arc::new(config),
            state: Arc::new(AsyncMutex::new(SupervisorState::default())),
        }
    }

    /// Home used for persisted-session resolution (wrapper logs, wrapper
    /// index, launch configs). The real home in production; tests inject
    /// `logs_home_override` for hermetic resolution.
    fn logs_home(&self) -> PathBuf {
        self.config
            .logs_home_override
            .clone()
            .unwrap_or_else(crate::platform::home_dir)
    }

    /// Act on one intent-lane event. Split from the receive loops so the
    /// primary supervisor and the resume listener share exactly one
    /// action path; `filter_session_control` is the resume listener's
    /// `should_handle_session_control` gate.
    async fn handle_intent_lane_event(&self, event: AppEvent, filter_session_control: bool) {
        match event {
            AppEvent::ControlCommand(msg) => {
                if !filter_session_control || self.should_handle_session_control(&msg).await {
                    self.handle_control_msg(msg).await;
                }
            }
            AppEvent::SessionIdentity {
                session_id,
                source,
                backend_session_id,
            } => {
                self.apply_session_identity(session_id, source, backend_session_id)
                    .await;
            }
            AppEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ..
            } => {
                self.apply_session_relationship(parent_session_id, child_session_id, relationship)
                    .await;
            }
            AppEvent::SessionEnded { session_id, .. } => {
                self.remove_session_alias(&session_id).await;
            }
            _ => {}
        }
    }

    /// The supervisor's receive loop, shared by [`Self::spawn`] and
    /// [`Self::spawn_resume_listener`].
    ///
    /// Two lanes, one loop:
    /// - the lossless intent lane ([`EventBus::subscribe_intents`]) carries
    ///   everything the supervisor ACTS on — `ControlCommand` dispatch plus
    ///   the identity/relationship/end bookkeeping that routes future
    ///   commands. Losing one of these corrupts routing state, so they must
    ///   never drop to `RecvError::Lagged`.
    /// - the broadcast ring still feeds `observe_lifecycle_event` (phase
    ///   chips): best-effort by design — a lagged phase update is cosmetic
    ///   and the next status event heals it.
    ///
    /// `biased` drains intents first so a user command is never queued
    /// behind an observation backlog. Cross-lane skew is tolerable because
    /// the observation side is display-only; intent-lane events are NOT
    /// re-observed here (they'd double-apply phase updates when the
    /// broadcast copy arrives).
    ///
    /// Receivers are subscribed by the caller BEFORE the task is spawned:
    /// daemon startup sends `ResumeSession` immediately after `spawn()`
    /// returns and relies on the subscription already existing.
    async fn run_event_loop(
        self,
        mut intent_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        mut rx: tokio::sync::broadcast::Receiver<AppEvent>,
        filter_session_control: bool,
    ) {
        loop {
            tokio::select! {
                biased;
                intent = intent_rx.recv() => match intent {
                    Some(event) => {
                        self.handle_intent_lane_event(event, filter_session_control)
                            .await;
                    }
                    None => break,
                },
                event = rx.recv() => match event {
                    Ok(event) => self.observe_lifecycle_event(&event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    }

    pub fn spawn(self) -> JoinHandle<()> {
        let intent_rx = self.config.bus.subscribe_intents();
        let rx = self.config.bus.subscribe();
        tokio::spawn(self.run_event_loop(intent_rx, rx, false))
    }

    pub fn spawn_resume_listener(self) -> JoinHandle<()> {
        let intent_rx = self.config.bus.subscribe_intents();
        let rx = self.config.bus.subscribe();
        tokio::spawn(self.run_event_loop(intent_rx, rx, true))
    }

    pub async fn run(self) {
        let handle = self.spawn();
        let _ = handle.await;
    }

    fn attachment_store_scopes(&self, primary: &Path) -> Vec<crate::global_store::StoreScope> {
        let mut scopes = vec![crate::global_store::StoreScope::Project(
            primary.to_path_buf(),
        )];
        match self.config.project_root.as_deref() {
            Some(default_root) => {
                if default_root != primary {
                    scopes.push(crate::global_store::StoreScope::Project(
                        default_root.to_path_buf(),
                    ));
                }
            }
            // Projectless daemon: dashboard-staged uploads live in the
            // daemon-global store, not under any project root.
            None => scopes.push(crate::global_store::StoreScope::resolve(None)),
        }
        scopes
    }

    async fn resolve_session_attachments(
        &self,
        attachments: &[String],
        session_dir: &Path,
        primary_project_root: &Path,
    ) -> Vec<external_agent::AgentAttachment> {
        if attachments.is_empty() {
            return Vec::new();
        }
        let scopes = self.attachment_store_scopes(primary_project_root);
        resolve_attachments_with_scopes(
            attachments,
            &self.config.frame_registry,
            session_dir,
            &scopes,
        )
        .await
    }
}

fn normalize_supervisor_phase(phase: &str) -> String {
    match phase.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "" => "idle".to_string(),
        "running_agent" => "running".to_string(),
        "waiting_follow_up" | "waiting_followup" => "idle".to_string(),
        other => other.to_string(),
    }
}

fn managed_session_accepts_external_input(session: &ManagedSession) -> bool {
    !matches!(
        normalize_supervisor_phase(&session.phase).as_str(),
        "done" | "interrupted"
    )
}

fn path_file_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn write_session_meta(
    session_log: &Arc<std::sync::Mutex<session_log::SessionLog>>,
    project_root: &Path,
    task: Option<&str>,
    name: Option<&str>,
) {
    if let Ok(log) = session_log.lock() {
        log.write_meta_with_name(Some(project_root), task, name);
    }
}

pub(crate) fn short_session(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn managed_session(id: &str, source: &str) -> ManagedSession {
        let (tx, _rx) = mpsc::channel(1);
        ManagedSession {
            session_id: id.to_string(),
            source: source.to_string(),
            name: None,
            phase: "idle".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            session_dir: PathBuf::from("/tmp/session"),
            follow_up_tx: tx,
            approval_registry: event::ApprovalRegistry::default(),
            instance_id: 0,
            finished_rx: None,
            depth: 0,
            // Mirror registration: native sessions carry a children
            // registry, external ones do not.
            sub_agent_children: (source == "intendant")
                .then(|| Arc::new(std::sync::Mutex::new(HashMap::new()))),
        }
    }

    pub(crate) fn test_supervisor(project_root: PathBuf, bus: EventBus) -> SessionSupervisor {
        SessionSupervisor::new(SessionSupervisorConfig {
            bus,
            project_root: Some(project_root),
            autonomy: crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            session_registry: None,
            peer_registry: None,
            shared_external_agent: Arc::new(tokio::sync::RwLock::new(None)),
            shared_codex_config: Arc::new(tokio::sync::RwLock::new(
                control_plane::CodexRuntimeConfig {
                    command: "codex".to_string(),
                    managed_command: None,
                    sandbox: "workspace-write".to_string(),
                    approval_policy: "on-request".to_string(),
                    model: None,
                    reasoning_effort: None,
                    service_tier: None,
                    web_search: false,
                    network_access: false,
                    writable_roots: Vec::new(),
                    managed_context: "vanilla".to_string(),
                    context_archive: "summary".to_string(),
                },
            )),
            shared_claude_config: Arc::new(tokio::sync::RwLock::new(
                control_plane::ClaudeRuntimeConfig {
                    model: None,
                    permission_mode: "default".to_string(),
                    allowed_tools: Vec::new(),
                },
            )),
            frame_registry: Arc::new(tokio::sync::RwLock::new(frames::FrameRegistry::new(
                std::env::temp_dir().as_path(),
            ))),
            web_port: None,
            flags_direct: false,
            shared_session: None,
            provider_factory: None,
            // Hermetic by default: supervisor tests must never resolve
            // persisted sessions against the machine's real ~/.intendant —
            // a box with live session history (a dev box, the peer-testing
            // Dell) can otherwise match a test's hardcoded wrapper id. The
            // dir is never created unless a test writes through it.
            // PID alone is not unique across runs (recycled PIDs inherit a
            // previous run's scratch — the state_paths precedent); a nanos
            // component makes the scratch per process INSTANCE. Sub-agent
            // and rename flows now WRITE through this home, not just read.
            logs_home_override: Some(std::env::temp_dir().join(format!(
                "intendant-test-logs-home-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ))),
            git_vitals_targets: None,
            hosted_control_cert_dir: None,
        })
    }

    pub(crate) fn test_supervisor_with_mock_provider(
        project_root: PathBuf,
        bus: EventBus,
    ) -> SessionSupervisor {
        let mut config = (*test_supervisor(project_root, bus).config).clone();
        config.provider_factory = Some(Arc::new(|| {
            Box::new(provider::mock::MockOrchestrationProvider::new())
                as Box<dyn provider::ChatProvider>
        }));
        SessionSupervisor::new(config)
    }

    /// The delegation dedup ledger: first writer wins for a given id,
    /// and the FIFO bound evicts the oldest acceptance, never the
    /// newest.
    #[test]
    fn delegation_ledger_dedups_bounds_and_first_writer_wins() {
        let mut state = SupervisorState::default();
        state.record_delegation("dg-a", "sess-original");
        // A re-record for the same id must NOT re-point it — the
        // re-ack contract promises the ORIGINAL session identity.
        state.record_delegation("dg-a", "sess-imposter");
        assert_eq!(
            state.recorded_delegation_session("dg-a").as_deref(),
            Some("sess-original")
        );

        for i in 0..MAX_DELEGATION_RECEIPTS {
            state.record_delegation(&format!("dg-fill-{i}"), &format!("sess-{i}"));
        }
        assert_eq!(
            state.recorded_delegation_session("dg-a"),
            None,
            "oldest entry is evicted at the bound"
        );
        assert!(
            state
                .recorded_delegation_session(&format!("dg-fill-{}", MAX_DELEGATION_RECEIPTS - 1))
                .is_some(),
            "newest entry survives"
        );
        assert!(state.delegation_receipts.len() <= MAX_DELEGATION_RECEIPTS);
        assert_eq!(
            state.delegation_receipts.len(),
            state.delegation_receipt_order.len(),
            "map and eviction order stay in lockstep"
        );
    }

    #[test]
    fn supervisor_state_resolves_and_removes_session_aliases() {
        let mut state = SupervisorState::default();
        state
            .sessions
            .insert("backend".to_string(), managed_session("backend", "codex"));
        state
            .session_aliases
            .insert("wrapper".to_string(), "backend".to_string());
        state.active_session_id = Some("backend".to_string());

        assert_eq!(
            state.resolve_session_id("wrapper").as_deref(),
            Some("backend")
        );
        assert!(state.session_is_managed("wrapper"));

        let removed = state.remove_session("wrapper");
        assert!(removed.is_some());
        assert!(!state.session_is_managed("wrapper"));
        assert!(!state.session_is_managed("backend"));
    }

    #[test]
    fn supervisor_state_resolves_side_child_alias_to_parent_session() {
        let mut state = SupervisorState::default();
        state
            .sessions
            .insert("parent".to_string(), managed_session("parent", "codex"));
        state
            .session_aliases
            .insert("side-child".to_string(), "parent".to_string());

        assert_eq!(
            state.resolve_session_id("side-child").as_deref(),
            Some("parent")
        );
        state.session_aliases.remove("side-child");
        assert!(!state.session_is_managed("side-child"));
        assert!(state.session_is_managed("parent"));
    }

    #[test]
    fn supervisor_state_tracks_subagent_child_as_related_parent_target() {
        let mut state = SupervisorState::default();
        state
            .sessions
            .insert("parent".to_string(), managed_session("parent", "codex"));
        assert!(state.apply_related_session("parent", "sub-child", "subagent"));

        assert_eq!(
            state.resolve_session_id("sub-child").as_deref(),
            Some("parent")
        );
        assert_eq!(
            state
                .related_sessions
                .get("sub-child")
                .map(|rel| rel.relationship.as_str()),
            Some("subagent")
        );

        let removed = state.remove_session("parent");
        assert!(removed.is_some());
        assert!(!state.session_is_managed("sub-child"));
        assert!(!state.related_sessions.contains_key("sub-child"));
    }

    #[test]
    fn supervisor_state_does_not_remove_newer_session_instance() {
        let mut state = SupervisorState::default();
        let mut session = managed_session("thread", "codex");
        session.instance_id = 1;
        state.sessions.insert("thread".to_string(), session);

        assert!(state.remove_session_instance("thread", 2).is_none());
        assert!(state.session_is_managed("thread"));
        assert!(state.remove_session_instance("thread", 1).is_some());
        assert!(!state.session_is_managed("thread"));
    }

    #[test]
    fn supervisor_state_dedupes_concurrent_restart_requests() {
        let mut state = SupervisorState::default();

        assert!(state.mark_restart_requested("codex:thread"));
        assert!(!state.mark_restart_requested("codex:thread"));
        assert!(state.mark_restart_requested("codex:other-thread"));
    }

    /// The user-halt ledger behind the auto-attach cancel: marks are
    /// per-id, clearable (newer intent wins), and expire after
    /// [`UNMANAGED_USER_HALT_WINDOW`] instead of blocking work forever.
    #[test]
    fn unmanaged_user_halts_mark_clear_and_expire() {
        let mut state = SupervisorState::default();
        state.mark_unmanaged_user_halts(["ghost-a", "ghost-b", "  ", ""]);
        assert!(state.unmanaged_user_halt_active(["ghost-a"]));
        assert!(state.unmanaged_user_halt_active(["unrelated", "ghost-b"]));
        assert!(!state.unmanaged_user_halt_active(["unrelated"]));
        assert_eq!(state.unmanaged_user_halts.len(), 2, "blank ids ignored");

        state.clear_unmanaged_user_halts(["ghost-a"]);
        assert!(!state.unmanaged_user_halt_active(["ghost-a"]));
        assert!(state.unmanaged_user_halt_active(["ghost-b"]));

        // Stale marks expire (and are pruned) instead of cancelling a
        // resume minutes later.
        if let Some(stale) = std::time::Instant::now()
            .checked_sub(UNMANAGED_USER_HALT_WINDOW + std::time::Duration::from_secs(1))
        {
            state
                .unmanaged_user_halts
                .insert("ghost-b".to_string(), stale);
            assert!(!state.unmanaged_user_halt_active(["ghost-b"]));
            assert!(state.unmanaged_user_halts.is_empty());
        }
    }
}

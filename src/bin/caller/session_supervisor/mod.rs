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
    pub provider_factory:
        Option<Arc<dyn Fn() -> Box<dyn provider::ChatProvider> + Send + Sync>>,
    /// Injection point for the persisted-session home: resume/attach
    /// resolution (wrapper logs, the wrapper index, persisted launch
    /// configs) reads from here. None in production (the real home); tests
    /// pin it so a machine's live `~/.intendant` session history cannot
    /// change what they observe — a hardcoded wrapper id in a test can
    /// otherwise resolve against a real session log and flip the flow
    /// from follow-up routing to a fresh resume dispatch.
    pub logs_home_override: Option<PathBuf>,
}

#[derive(Clone)]
pub struct SessionSupervisor {
    config: Arc<SessionSupervisorConfig>,
    state: Arc<AsyncMutex<SupervisorState>>,
}

const EXTERNAL_ATTACH_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const SESSION_STOP_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const SESSION_RESTART_DEDUPE_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
const EXTERNAL_ATTACH_DEDUPE_WINDOW: std::time::Duration = EXTERNAL_ATTACH_READY_TIMEOUT;
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
struct SupervisorState {
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
}

#[derive(Debug, Clone)]
struct RelatedSession {
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
    pub inherit_memory: bool,
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
    inherit_memory: bool,
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

struct StoppedManagedSession {
    session_id: String,
    source: String,
    finished_rx: Option<oneshot::Receiver<()>>,
}

#[derive(Clone)]
struct EditRouteTarget {
    managed_id: String,
    source: String,
    project_root: PathBuf,
    session_dir: PathBuf,
    follow_up_tx: mpsc::Sender<FollowUpMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditAttachRequest {
    source: String,
    resume_id: Option<String>,
    project_root: Option<String>,
    direct: Option<bool>,
}

#[derive(Debug, Clone)]
struct EditUserMessageRequest {
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

    pub fn spawn(self) -> JoinHandle<()> {
        let mut rx = self.config.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        self.observe_lifecycle_event(&event).await;
                        match event {
                            AppEvent::ControlCommand(msg) => {
                                self.handle_control_msg(msg).await;
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
                                self.apply_session_relationship(
                                    parent_session_id,
                                    child_session_id,
                                    relationship,
                                )
                                .await;
                            }
                            AppEvent::SessionEnded { session_id, .. } => {
                                self.remove_session_alias(&session_id).await;
                            }
                            _ => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    pub fn spawn_resume_listener(self) -> JoinHandle<()> {
        let mut rx = self.config.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        self.observe_lifecycle_event(&event).await;
                        match event {
                            AppEvent::ControlCommand(msg) => {
                                if self.should_handle_session_control(&msg).await {
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
                                self.apply_session_relationship(
                                    parent_session_id,
                                    child_session_id,
                                    relationship,
                                )
                                .await;
                            }
                            AppEvent::SessionEnded { session_id, .. } => {
                                self.remove_session_alias(&session_id).await;
                            }
                            _ => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    pub async fn run(self) {
        let handle = self.spawn();
        let _ = handle.await;
    }

    fn attachment_project_roots(&self, primary: &Path) -> Vec<PathBuf> {
        let mut roots = vec![primary.to_path_buf()];
        if let Some(default_root) = self.config.project_root.as_deref() {
            if default_root != primary {
                roots.push(default_root.to_path_buf());
            }
        }
        roots
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
        let roots = self.attachment_project_roots(primary_project_root);
        resolve_attachments_with_project_roots(
            attachments,
            &self.config.frame_registry,
            session_dir,
            &roots,
        )
        .await
    }

    async fn handle_control_msg(&self, msg: event::ControlMsg) {
        match msg {
            event::ControlMsg::CreateSession {
                task,
                name,
                project_root,
                agent,
                agent_command,
                claude_model,
                claude_permission_mode,
                claude_effort,
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
            } => {
                if let Some(parsed) = parse_codex_slash_command(&task) {
                    match parsed {
                        Ok(command) if command.op == "fast" => {
                            let agent = match codex_fast_new_session_agent(agent.as_deref()) {
                                Ok(agent) => Some(agent),
                                Err(message) => {
                                    self.loop_error(message);
                                    return;
                                }
                            };
                            if !reference_frame_ids.is_empty()
                                || display_target.is_some()
                                || !attachments.is_empty()
                            {
                                self.warn(
                                    "/fast creates an idle Codex session; attachments and display metadata were ignored",
                                );
                            }
                            self.start_new_session(
                                String::new(),
                                name,
                                project_root,
                                agent,
                                agent_command,
                                None,
                                None,
                                None,
                                codex_sandbox,
                                codex_approval_policy,
                                codex_managed_context,
                                codex_context_archive,
                                orchestrate,
                                direct,
                                Vec::new(),
                                None,
                                Vec::new(),
                                Some(
                                    crate::external_agent::codex::CODEX_FAST_SERVICE_TIER
                                        .to_string(),
                                ),
                            )
                            .await;
                            return;
                        }
                        Ok(_) | Err(_) => {}
                    }
                    if !reference_frame_ids.is_empty()
                        || display_target.is_some()
                        || agent.is_some()
                        || agent_command.is_some()
                        || claude_model.is_some()
                        || claude_permission_mode.is_some()
                        || claude_effort.is_some()
                        || codex_sandbox.is_some()
                        || codex_approval_policy.is_some()
                        || codex_managed_context.is_some()
                        || codex_context_archive.is_some()
                        || codex_service_tier.is_some()
                        || name.is_some()
                    {
                        self.warn(
                            "Slash command dropped new-session metadata; routing to active Codex session",
                        );
                    }
                    self.route_follow_up(None, task, direct, attachments, None)
                        .await;
                    return;
                }
                self.start_new_session(
                    task,
                    name,
                    project_root,
                    agent,
                    agent_command,
                    claude_model,
                    claude_permission_mode,
                    claude_effort,
                    codex_sandbox,
                    codex_approval_policy,
                    codex_managed_context,
                    codex_context_archive,
                    orchestrate,
                    direct,
                    reference_frame_ids,
                    display_target,
                    attachments,
                    codex_service_tier,
                )
                .await;
            }
            event::ControlMsg::StartTask {
                session_id: Some(session_id),
                task,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
                follow_up_id,
                ..
            } => {
                if !reference_frame_ids.is_empty() || display_target.is_some() {
                    self.warn(&format!(
                        "Targeted StartTask for {} dropped reference frame/display metadata; routing text as follow-up",
                        short_session(&session_id)
                    ));
                }
                self.route_follow_up(Some(session_id), task, direct, attachments, follow_up_id)
                    .await;
            }
            event::ControlMsg::StartTask {
                session_id: None,
                task,
                orchestrate,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
                follow_up_id: _,
            } => {
                if let Some(parsed) = parse_codex_slash_command(&task) {
                    match parsed {
                        Ok(command) if command.op == "fast" => {
                            if !reference_frame_ids.is_empty()
                                || display_target.is_some()
                                || !attachments.is_empty()
                            {
                                self.warn(
                                    "/fast creates an idle Codex session; attachments and display metadata were ignored",
                                );
                            }
                            self.start_new_session(
                                String::new(),
                                None,
                                None,
                                Some("codex".to_string()),
                                None,
                                None,
                                None,
                                None,
                                None,
                                None,
                                None,
                                None,
                                orchestrate,
                                direct,
                                Vec::new(),
                                None,
                                Vec::new(),
                                Some(
                                    crate::external_agent::codex::CODEX_FAST_SERVICE_TIER
                                        .to_string(),
                                ),
                            )
                            .await;
                            return;
                        }
                        Ok(_) | Err(_) => {}
                    }
                    if !reference_frame_ids.is_empty() || display_target.is_some() {
                        self.warn(
                            "Slash command dropped reference frame/display metadata; routing to active Codex session",
                        );
                    }
                    self.route_follow_up(None, task, direct, attachments, None)
                        .await;
                    return;
                }
                self.start_new_session(
                    task,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    orchestrate,
                    direct,
                    reference_frame_ids,
                    display_target,
                    attachments,
                    None,
                )
                .await;
            }
            event::ControlMsg::SpawnSubAgent {
                session_id,
                task,
                name,
                role,
                agent,
                worktree,
            } => {
                self.delegate_sub_agent(session_id, task, name, role, agent, worktree)
                    .await;
            }
            event::ControlMsg::ResumeSession {
                source,
                session_id,
                resume_id,
                project_root,
                task,
                direct,
                attachments,
                fork,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
            } => {
                self.resume_session(
                    source,
                    session_id,
                    resume_id,
                    project_root,
                    task,
                    direct,
                    attachments,
                    fork,
                    LaunchOverrides {
                        agent_command,
                        codex_sandbox,
                        codex_approval_policy,
                        codex_managed_context,
                        codex_context_archive,
                        ..Default::default()
                    },
                    false,
                )
                .await;
            }
            event::ControlMsg::StopSession { session_id } => {
                self.stop_managed_session(Some(session_id), "stopped by user")
                    .await;
            }
            event::ControlMsg::RestartSession {
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
                self.restart_session(
                    source,
                    session_id,
                    resume_id,
                    project_root,
                    task,
                    direct,
                    attachments,
                    LaunchOverrides {
                        agent_command,
                        codex_sandbox,
                        codex_approval_policy,
                        codex_managed_context,
                        codex_context_archive,
                        claude_model,
                        claude_permission_mode,
                        claude_allowed_tools,
                        claude_effort,
                    },
                )
                .await;
            }
            event::ControlMsg::FollowUp {
                session_id,
                text,
                direct,
                follow_up_id,
            } => {
                self.route_follow_up(session_id, text, direct, vec![], follow_up_id)
                    .await;
            }
            event::ControlMsg::EditUserMessage {
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
                self.route_edit_user_message(
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
                )
                .await;
            }
            event::ControlMsg::Interrupt {
                session_id,
                expected_turn: _,
            } => {
                self.route_interrupt(session_id).await;
            }
            event::ControlMsg::Steer {
                session_id,
                text,
                id,
                attachments,
            } => {
                self.route_steer(session_id, text, id, attachments).await;
            }
            event::ControlMsg::CancelSteer {
                session_id,
                id,
                reason,
            } => {
                self.route_cancel_steer(session_id, id, reason).await;
            }
            event::ControlMsg::CancelFollowUp {
                session_id,
                id,
                reason,
            } => {
                self.route_cancel_follow_up(session_id, id, reason).await;
            }
            event::ControlMsg::Approve { session_id, id } => {
                self.resolve_approval(session_id, id, event::ApprovalResponse::Approve, "approve")
                    .await;
            }
            event::ControlMsg::Deny { session_id, id } => {
                self.resolve_approval(session_id, id, event::ApprovalResponse::Deny, "deny")
                    .await;
            }
            event::ControlMsg::Skip { session_id, id } => {
                self.resolve_approval(session_id, id, event::ApprovalResponse::Skip, "skip")
                    .await;
            }
            event::ControlMsg::ApproveAll { session_id, id } => {
                self.resolve_approval(
                    session_id,
                    id,
                    event::ApprovalResponse::ApproveAll,
                    "approve_all",
                )
                .await;
            }
            event::ControlMsg::AnswerQuestion {
                session_id,
                id,
                answers,
            } => {
                self.resolve_approval(
                    session_id,
                    id,
                    event::ApprovalResponse::Answer { answers },
                    "answer",
                )
                .await;
            }
            event::ControlMsg::RenameSession {
                session_id,
                backend_session_id,
                source,
                name,
            } => {
                self.rename_session(session_id, backend_session_id, source, name)
                    .await;
            }
            event::ControlMsg::ConfigureSessionAgent {
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
                self.configure_session_agent(
                    session_id,
                    source,
                    backend_session_id,
                    intendant_session_id,
                    LaunchOverrides {
                        agent_command,
                        codex_sandbox,
                        codex_approval_policy,
                        codex_managed_context,
                        codex_context_archive,
                        claude_model,
                        claude_permission_mode,
                        claude_allowed_tools,
                        claude_effort,
                    },
                )
                .await;
            }
            event::ControlMsg::CodexThreadAction { session_id, op, .. } => {
                self.report_unattached_codex_thread_action(session_id, op)
                    .await;
            }
            _ => {}
        }
    }

    async fn should_handle_session_control(&self, msg: &event::ControlMsg) -> bool {
        match msg {
            event::ControlMsg::CreateSession { .. } => true,
            // Always claimed so an unmanaged parent gets an explicit
            // "Delegate failed" instead of a silently dropped message.
            event::ControlMsg::SpawnSubAgent { .. } => true,
            event::ControlMsg::ResumeSession { .. } => true,
            event::ControlMsg::RestartSession { .. } => true,
            event::ControlMsg::StopSession { .. } => true,
            event::ControlMsg::RenameSession { .. } => true,
            event::ControlMsg::ConfigureSessionAgent { .. } => true,
            event::ControlMsg::CodexThreadAction { .. } => true,
            msg if control_msg_can_attach_unmanaged_session(msg) => true,
            _ => {
                if let Some(session_id) = control_target_session_id(msg) {
                    self.session_is_managed(session_id).await
                } else {
                    false
                }
            }
        }
    }

    async fn report_unattached_codex_thread_action(&self, session_id: Option<String>, op: String) {
        let Some(target_id) = session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return;
        };

        let failure = {
            let state = self.state.lock().await;
            let unattached = || {
                Some(format!(
                    "target session {} is not attached to this daemon; attach it before /{}",
                    short_session(target_id),
                    op
                ))
            };
            // A live loop advertised this exact op for this session (e.g.
            // the native presence loop's goal* family): it answers; a
            // failure here would race a real result.
            let op_advertised = |id: &str| {
                state
                    .advertised_thread_actions
                    .get(id)
                    .is_some_and(|ops| ops.contains(&op))
            };
            if op_advertised(target_id) {
                None
            } else {
                match state.resolve_session_id(target_id) {
                    Some(managed_id) if op_advertised(&managed_id) => None,
                    Some(managed_id) => match state.sessions.get(&managed_id) {
                        // Any live external backend: the owning drain dispatches
                        // (and answers) the action — stay silent here.
                        Some(session)
                            if external_agent::AgentBackend::from_str_loose(&session.source)
                                .is_some() =>
                        {
                            None
                        }
                        Some(session) => Some(format!(
                            "target session {} is a {} session that does not advertise /{} — thread actions need a loop that answers them",
                            short_session(target_id),
                            session.source,
                            op
                        )),
                        None => unattached(),
                    },
                    // Not supervisor-managed, but a live session on this bus
                    // announced this id (e.g. the CLI main loop's own agent):
                    // its drain answers; a failure here would race a real result.
                    None if state.known_external_sessions.contains(target_id) => None,
                    None => unattached(),
                }
            }
        };

        if let Some(message) = failure {
            self.config.bus.send(AppEvent::CodexThreadActionResult {
                session_id,
                action: op,
                success: false,
                message,
                record_id: None,
            });
        }
    }

    /// Count of still-running sub-agent children of `parent_session_id`.
    async fn running_sub_agent_children(&self, parent_session_id: &str) -> usize {
        let state = self.state.lock().await;
        state
            .related_sessions
            .iter()
            .filter(|(child, rel)| {
                rel.relationship == "subagent"
                    && rel.parent_session_id == parent_session_id
                    && state.sessions.contains_key(child.as_str())
            })
            .count()
    }

    /// Dashboard "delegate" action (`ControlMsg::SpawnSubAgent`): spawn a
    /// sub-agent under a managed native session on the user's behalf. The
    /// child lands in the same children registry the parent's
    /// wait_sub_agents tool reads, and the parent is woken with a
    /// notification follow-up so the model knows the delegation happened
    /// and can collect the result exactly like one of its own spawns.
    async fn delegate_sub_agent(
        &self,
        parent_session_id: String,
        task: String,
        name: Option<String>,
        role: Option<String>,
        agent: Option<String>,
        worktree: Option<bool>,
    ) {
        let parent = {
            let state = self.state.lock().await;
            state.resolve_session_id(&parent_session_id).map(|id| {
                let session = state
                    .sessions
                    .get(&id)
                    .expect("resolve_session_id returns live keys");
                (
                    id.clone(),
                    session.project_root.clone(),
                    session.depth,
                    session.sub_agent_children.clone(),
                )
            })
        };
        let Some((parent_id, parent_root, parent_depth, children)) = parent else {
            self.loop_error(format!(
                "Delegate failed: session {} is not managed by this daemon",
                short_session(&parent_session_id)
            ));
            return;
        };
        let Some(children) = children else {
            self.loop_error(format!(
                "Delegate failed: session {} runs an external agent, which manages its \
                 own sub-agents — send it a follow-up asking it to delegate instead",
                short_session(&parent_id)
            ));
            return;
        };
        let backend = match agent.as_deref().map(str::trim).unwrap_or("internal") {
            "internal" | "" | "intendant" => None,
            "codex" => Some(external_agent::AgentBackend::Codex),
            "claude-code" | "claude_code" => Some(external_agent::AgentBackend::ClaudeCode),
            other => {
                self.loop_error(format!(
                    "Delegate failed: unknown sub-agent backend `{other}`; use internal, \
                     codex, or claude-code"
                ));
                return;
            }
        };
        let parent_project = match Project::from_root(parent_root) {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Delegate failed: parent project load failed: {e}"));
                return;
            }
        };
        let params = SubAgentSpawnParams {
            task: task.clone(),
            role: sub_agent::SubAgentRole::from_str(
                role.as_deref()
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .unwrap_or("worker"),
            ),
            system_prompt: None,
            backend,
            worktree: worktree.unwrap_or(false),
            inherit_memory: false,
            name,
        };
        let started = match self
            .start_sub_agent_session(&parent_id, &parent_project, parent_depth, params)
            .await
        {
            Ok(started) => started,
            Err(e) => {
                self.loop_error(format!("Delegate failed: {e}"));
                return;
            }
        };
        {
            let mut children = children.lock().unwrap_or_else(|e| e.into_inner());
            children.insert(
                started.child_session_id.clone(),
                SubAgentChild {
                    name: started.child_name.clone(),
                    rx: Some(started.completion_rx),
                    completed: None,
                    delivered: false,
                },
            );
        }
        self.config.bus.send(AppEvent::LogEntry {
            session_id: Some(parent_id.clone()),
            level: "info".to_string(),
            source: "session-supervisor".to_string(),
            content: format!(
                "Delegated sub-agent {} (session {}) under session {}",
                started.child_name,
                short_session(&started.child_session_id),
                short_session(&parent_id)
            ),
            turn: None,
        });
        let mut notice = format!(
            "[dashboard] The user delegated a task to a new sub-agent of this session:\n\
             - name: {}\n- child_session_id: {}\n- task: {}",
            started.child_name, started.child_session_id, task
        );
        if let Some(path) = &started.worktree_path {
            notice.push_str(&format!("\n- worktree: {}", path.display()));
        }
        notice.push_str(
            "\nIt is already running as its own supervised session. Collect its result \
             with wait_sub_agents when you need it and fold it into your work.",
        );
        self.route_follow_up(Some(parent_id), notice, None, Vec::new(), None)
            .await;
    }

    /// Spawn a supervised child session on behalf of `parent_session_id`
    /// (the spawn_sub_agent tool). The child is a full managed session —
    /// dashboard row, approvals, steering, lineage — linked to its parent
    /// with the same "subagent" relationship Codex-spawned children get.
    /// Returns the child's identity plus a receiver that resolves with the
    /// child's terminal result.
    ///
    /// Returns a boxed future (not an `async fn`) to break the opaque-type
    /// cycle: this future contains `spawn_agent_session`, whose child loop's
    /// spawn_sub_agent handler calls back into this method.
    pub fn start_sub_agent_session<'a>(
        &'a self,
        parent_session_id: &'a str,
        parent_project: &'a Project,
        parent_depth: usize,
        params: SubAgentSpawnParams,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SubAgentSpawnStarted, String>> + Send + 'a>,
    > {
        Box::pin(self.start_sub_agent_session_inner(
            parent_session_id,
            parent_project,
            parent_depth,
            params,
        ))
    }

    async fn start_sub_agent_session_inner(
        &self,
        parent_session_id: &str,
        parent_project: &Project,
        parent_depth: usize,
        params: SubAgentSpawnParams,
    ) -> Result<SubAgentSpawnStarted, String> {
        let child_depth = parent_depth.saturating_add(1);
        if child_depth > MAX_SUB_AGENT_DEPTH {
            return Err(format!(
                "sub-agent depth cap reached: this session is already {parent_depth} delegation \
                 level(s) below the root and cannot spawn further sub-agents. Do the task \
                 yourself and report with submit_result."
            ));
        }
        let cap = parent_project
            .config
            .orchestrator
            .max_parallel_agents
            .unwrap_or(DEFAULT_MAX_PARALLEL_SUB_AGENTS)
            .max(1);
        let running = self.running_sub_agent_children(parent_session_id).await;
        if running >= cap {
            return Err(format!(
                "sub-agent cap reached: {running} of {cap} children of this session are still \
                 running. Call wait_sub_agents to collect one before spawning more \
                 (cap: [orchestrator] max_parallel_agents)."
            ));
        }
        if params.task.trim().is_empty() {
            return Err("sub-agent task must not be empty".to_string());
        }

        let session_name = normalize_session_name_option(params.name.as_deref())
            .map_err(|e| format!("invalid sub-agent name: {e}"))?;

        let log_dir = session_log::SessionLog::resolve_path(None);
        let session_log = session_log::SessionLog::open(log_dir.clone())
            .map(|log| Arc::new(std::sync::Mutex::new(log)))
            .map_err(|e| format!("sub-agent session log failed: {e}"))?;
        let child_session_id = session_log
            .lock()
            .map(|log| log.session_id().to_string())
            .unwrap_or_else(|_| path_file_name(&log_dir));
        let child_name = session_name.clone().unwrap_or_else(|| {
            format!(
                "{}-{}",
                params.role.as_str(),
                short_session(&child_session_id)
            )
        });

        // Worktree isolation: branch off the parent project's HEAD, same
        // machinery fission branches use.
        let worktree_path = if params.worktree {
            let wt = worktree::create(
                &parent_project.root,
                &format!("subagent-{}", short_session(&child_session_id)),
                "HEAD",
            )
            .map_err(|e| format!("sub-agent worktree creation failed: {e}"))?;
            Some(wt.path)
        } else {
            None
        };
        let child_root = worktree_path
            .clone()
            .unwrap_or_else(|| parent_project.root.clone());
        let project = Project::from_root(child_root)
            .map_err(|e| format!("sub-agent project load failed: {e}"))?;
        let project = self
            .project_with_runtime_config(project.root.clone(), params.backend.as_ref())
            .await
            .map_err(|e| format!("sub-agent project load failed: {e}"))?;
        let mut codex_home = None;
        if let Some(backend) = params.backend.as_ref() {
            let config = crate::session_config::from_project(backend, &project);
            if matches!(backend, external_agent::AgentBackend::Codex) {
                codex_home = config.codex_home.clone();
            }
            if let Err(e) = crate::session_config::write_log_dir_config(&log_dir, &config) {
                self.warn(&format!(
                    "Session launch config was not persisted for sub-agent {}: {}",
                    short_session(&child_session_id),
                    e
                ));
            }
        }

        write_session_meta(
            &session_log,
            &project.root,
            Some(params.task.as_str()),
            session_name.as_deref(),
        );

        // Record the parent link before the child runs: synchronously in
        // supervisor state (the spawn cap counts it), in the child's own
        // session log (relationship rehydration on daemon restart reads
        // from there), and on the bus for frontends — the same
        // "subagent" relationship kind Codex children use, so the
        // dashboard treats both identically.
        {
            let mut state = self.state.lock().await;
            state.apply_related_session(parent_session_id, &child_session_id, "subagent");
        }
        slog(&session_log, |l| {
            l.session_relationship(parent_session_id, &child_session_id, "subagent", false);
        });
        self.config.bus.send(AppEvent::SessionRelationship {
            parent_session_id: parent_session_id.to_string(),
            child_session_id: child_session_id.clone(),
            relationship: "subagent".to_string(),
            ephemeral: false,
        });

        let emit_session_started_after_identity = params.backend.is_some();
        if !emit_session_started_after_identity {
            self.config.bus.send(AppEvent::SessionStarted {
                session_id: child_session_id.clone(),
                task: Some(params.task.clone()),
            });
        }
        emit_task_dispatched_log(&self.config.bus, &session_log, &params.task, 0);

        let (completion_tx, completion_rx) = oneshot::channel();
        let wiring = SubAgentWiring {
            completion_tx,
            submitted_result: Arc::new(std::sync::Mutex::new(None)),
            child_name: child_name.clone(),
            role: params.role,
            system_prompt: params.system_prompt,
            inherit_memory: params.inherit_memory,
            depth: child_depth,
        };
        let source = params
            .backend
            .as_ref()
            .map(|b| b.as_short_str().to_string())
            .unwrap_or_else(|| "intendant".to_string());
        self.spawn_agent_session(
            child_session_id.clone(),
            source,
            params.task,
            project,
            session_log,
            log_dir,
            params.backend,
            true,
            UserAttachments::default(),
            session_name,
            None,
            None,
            emit_session_started_after_identity,
            None,
            None,
            codex_home,
            Some(wiring),
        )
        .await;

        Ok(SubAgentSpawnStarted {
            child_session_id,
            child_name,
            worktree_path,
            completion_rx,
        })
    }

    async fn route_follow_up(
        &self,
        session_id: Option<String>,
        text: String,
        _direct: Option<bool>,
        attachments: Vec<String>,
        follow_up_id: Option<String>,
    ) {
        let (target_id, entry) = {
            let state = self.state.lock().await;
            let requested_id = session_id.or_else(|| state.active_session_id.clone());
            let Some(requested_id) = requested_id else {
                drop(state);
                self.warn("FollowUp dropped: no active managed session");
                return;
            };
            let target_id = state
                .resolve_session_id(&requested_id)
                .unwrap_or_else(|| requested_id.clone());
            let entry = state.sessions.get(&target_id).map(|s| {
                let relation = state.related_sessions.get(&requested_id).cloned();
                (
                    s.session_id.clone(),
                    s.source.clone(),
                    s.project_root.clone(),
                    s.session_dir.clone(),
                    s.follow_up_tx.clone(),
                    requested_id.clone(),
                    relation,
                )
            });
            (target_id, entry)
        };
        let (target_id, entry) = if entry.is_none() {
            if let Some(live_id) = self.resolve_persisted_external_managed_id(&target_id).await {
                let state = self.state.lock().await;
                let target_id = state
                    .resolve_session_id(&live_id)
                    .unwrap_or_else(|| live_id.clone());
                let entry = state.sessions.get(&target_id).map(|s| {
                    let relation = state.related_sessions.get(&target_id).cloned();
                    (
                        s.session_id.clone(),
                        s.source.clone(),
                        s.project_root.clone(),
                        s.session_dir.clone(),
                        s.follow_up_tx.clone(),
                        target_id.clone(),
                        relation,
                    )
                });
                (target_id, entry)
            } else {
                (target_id, entry)
            }
        } else {
            (target_id, entry)
        };

        match entry {
            Some((managed_id, source, project_root, session_dir, tx, requested_id, relation)) => {
                if let Some(parsed) = parse_codex_slash_command(&text) {
                    match parsed {
                        Ok(command) => {
                            // Dispatch for every source — the attached loop
                            // (or the unattached-session responder) reports
                            // per-backend support honestly, so /goal works
                            // wherever a goal engine answers.
                            if source == "codex"
                                && relation
                                    .as_ref()
                                    .is_some_and(|rel| rel.relationship == "subagent")
                            {
                                self.warn(&format!(
                                    "Slash command /{} is not supported for Codex subagent session {}",
                                    command.op,
                                    short_session(&requested_id)
                                ));
                                return;
                            }
                            if !attachments.is_empty() {
                                self.warn(&format!(
                                    "Slash command /{} for {} session {} ignored {} attachment(s)",
                                    command.op,
                                    source,
                                    short_session(&managed_id),
                                    attachments.len()
                                ));
                            }
                            self.config.bus.send(AppEvent::ControlCommand(
                                event::ControlMsg::CodexThreadAction {
                                    session_id: Some(managed_id),
                                    op: command.op,
                                    params: command.params,
                                    origin: None,
                                },
                            ));
                        }
                        Err(message) => self.warn(&message),
                    }
                    return;
                }

                let resolved_attachments = self
                    .resolve_session_attachments(&attachments, &session_dir, &project_root)
                    .await;
                if resolved_attachments.len() < attachments.len() {
                    self.warn(&format!(
                        "Only resolved {} of {} requested attachment(s) for {} session {}",
                        resolved_attachments.len(),
                        attachments.len(),
                        source,
                        short_session(&managed_id)
                    ));
                }
                if relation
                    .as_ref()
                    .is_some_and(|rel| rel.relationship == "side")
                    && source == "codex"
                {
                    if tx.is_closed() {
                        emit_follow_up_status(
                            &self.config.bus,
                            Some(requested_id.clone()),
                            &follow_up_id,
                            None,
                            "failed",
                            Some("target session is not accepting input"),
                        );
                        self.warn(&format!(
                            "FollowUp dropped: {} side session {} in {} is not accepting input",
                            source,
                            short_session(&requested_id),
                            project_root.display()
                        ));
                    } else {
                        self.config.bus.send(AppEvent::ExternalFollowUpRequested {
                            session_id: requested_id.clone(),
                            text: text.clone(),
                            attachments: resolved_attachments,
                            follow_up_id: follow_up_id.clone(),
                        });
                        emit_follow_up_status(
                            &self.config.bus,
                            Some(requested_id),
                            &follow_up_id,
                            Some(&text),
                            "queued",
                            Some("queued for side conversation"),
                        );
                    }
                    return;
                }
                let msg = FollowUpMessage::with_attachments(
                    text.clone(),
                    UserAttachments::from_items(resolved_attachments),
                )
                .for_target(Some(requested_id.clone()))
                .with_follow_up_id(follow_up_id.clone());
                if tx.send(msg).await.is_err() {
                    emit_follow_up_status(
                        &self.config.bus,
                        Some(requested_id.clone()),
                        &follow_up_id,
                        None,
                        "failed",
                        Some("target session is not accepting input"),
                    );
                    let message = format!(
                        "FollowUp dropped: {} session {} in {} is not accepting input",
                        source,
                        short_session(&managed_id),
                        project_root.display()
                    );
                    eprintln!("[supervisor] {}", message);
                    self.warn(&message);
                } else {
                    // Queued and delivered are recorded on both sides of the
                    // channel: this daemon-log line pairs with the session
                    // log's "Follow-up … delivered" — a queued without a
                    // delivered means the session loop stopped draining its
                    // queue. (eprintln reaches the daemon log via the fd tee;
                    // bus log entries are dashboard-only.)
                    eprintln!(
                        "[supervisor] FollowUp {} queued for {} session {}",
                        follow_up_id.as_deref().unwrap_or("(no id)"),
                        source,
                        short_session(&managed_id),
                    );
                    emit_follow_up_status(
                        &self.config.bus,
                        Some(requested_id),
                        &follow_up_id,
                        Some(&text),
                        "queued",
                        Some("queued for next turn"),
                    );
                }
            }
            None => {
                emit_follow_up_status(
                    &self.config.bus,
                    Some(target_id.clone()),
                    &follow_up_id,
                    Some(&text),
                    "failed",
                    Some("target session is not managed by this daemon"),
                );
                let message = format!(
                    "FollowUp dropped: session {} is not managed by this daemon",
                    short_session(&target_id)
                );
                eprintln!("[supervisor] {}", message);
                self.warn(&message);
            }
        }
    }

    async fn route_edit_user_message(
        &self,
        session_id: Option<String>,
        source: Option<String>,
        resume_id: Option<String>,
        project_root: Option<String>,
        direct: Option<bool>,
        user_turn_index: u32,
        user_turn_revision: Option<u32>,
        original_text: Option<String>,
        text: String,
        attachments: Vec<String>,
    ) {
        let requested_id = {
            let state = self.state.lock().await;
            let requested_id = session_id.or_else(|| state.active_session_id.clone());
            let Some(requested_id) = requested_id else {
                drop(state);
                self.warn("Edit dropped: no active managed session");
                self.emit_edit_user_message_status(
                    None,
                    user_turn_index,
                    "failed",
                    "no active managed session",
                );
                return;
            };
            requested_id
        };

        self.emit_edit_user_message_status(
            Some(requested_id.clone()),
            user_turn_index,
            "requested",
            format!("edit requested for user turn {}", user_turn_index),
        );

        let request = EditUserMessageRequest {
            requested_id: requested_id.clone(),
            user_turn_index,
            user_turn_revision,
            original_text,
            text,
            attachments,
        };

        let (target_id, entry, relation) = self.lookup_edit_route_target(&requested_id).await;
        if entry.is_none() {
            if let Some(attach) = edit_attach_request(source, resume_id, project_root, direct) {
                let lookup_id = attach
                    .resume_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .unwrap_or(&requested_id)
                    .to_string();
                self.emit_edit_user_message_status(
                    Some(requested_id.clone()),
                    user_turn_index,
                    "attaching",
                    format!(
                        "attaching {} session {} before editing user turn {}",
                        attach.source,
                        short_session(&lookup_id),
                        user_turn_index
                    ),
                );
                self.queue_edit_user_message_after_attach(lookup_id.clone(), request);
                self.resume_session(
                    attach.source,
                    requested_id.clone(),
                    Some(lookup_id.clone()),
                    attach.project_root,
                    None,
                    Some(attach.direct.unwrap_or(true)),
                    Vec::new(),
                    false,
                    LaunchOverrides::default(),
                    false,
                )
                .await;
                return;
            }
        }

        let Some(target) = entry else {
            self.warn(&format!(
                "Edit dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            self.emit_edit_user_message_status(
                Some(requested_id),
                user_turn_index,
                "failed",
                "target session is not managed by this daemon",
            );
            return;
        };
        self.deliver_edit_user_message(request, target, relation)
            .await;
    }

    fn emit_edit_user_message_status(
        &self,
        session_id: Option<String>,
        user_turn_index: u32,
        status: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.config.bus.send(AppEvent::UserMessageEditStatus {
            session_id,
            user_turn_index,
            status: status.into(),
            message: message.into(),
        });
    }

    fn queue_edit_user_message_after_attach(
        &self,
        lookup_id: String,
        request: EditUserMessageRequest,
    ) {
        self.config.bus.send(AppEvent::LogEntry {
            session_id: Some(request.requested_id.clone()),
            level: "info".to_string(),
            source: "session-supervisor".to_string(),
            content: format!(
                "Edit waiting for session {} to become routable before user turn {}",
                short_session(&lookup_id),
                request.user_turn_index
            ),
            turn: None,
        });
        let supervisor = self.clone();
        let mut attach_rx = self.config.bus.subscribe();
        tokio::spawn(async move {
            let (target_id, entry, relation) = supervisor
                .wait_for_edit_route_target_after_attach(
                    &lookup_id,
                    Some(&request.requested_id),
                    &mut attach_rx,
                )
                .await;
            let Some(target) = entry else {
                supervisor.warn(&format!(
                    "Edit dropped: session {} was not routable after attach",
                    short_session(&target_id)
                ));
                supervisor.emit_edit_user_message_status(
                    Some(request.requested_id),
                    request.user_turn_index,
                    "failed",
                    "session was not routable after attach",
                );
                return;
            };
            supervisor
                .deliver_edit_user_message(request, target, relation)
                .await;
        });
    }

    async fn wait_for_edit_route_target_after_attach(
        &self,
        primary_id: &str,
        fallback_id: Option<&str>,
        attach_rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    ) -> (String, Option<EditRouteTarget>, Option<RelatedSession>) {
        let started_at = std::time::Instant::now();
        loop {
            let primary = self.lookup_edit_route_target(primary_id).await;
            if primary.1.is_some() {
                return primary;
            }

            if let Some(fallback_id) = fallback_id.filter(|id| *id != primary_id) {
                let fallback = self.lookup_edit_route_target(fallback_id).await;
                if fallback.1.is_some() {
                    return fallback;
                }
            }

            if started_at.elapsed() >= EDIT_ATTACH_ROUTE_TIMEOUT {
                return if let Some(fallback_id) = fallback_id.filter(|id| *id != primary_id) {
                    let fallback = self.lookup_edit_route_target(fallback_id).await;
                    if fallback.1.is_some() {
                        fallback
                    } else {
                        primary
                    }
                } else {
                    primary
                };
            }

            tokio::select! {
                event = attach_rx.recv() => {
                    match event {
                        Ok(event) if edit_attach_event_matches(&event, primary_id, fallback_id) => {}
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    }
                }
                _ = tokio::time::sleep(EDIT_ATTACH_ROUTE_POLL_INTERVAL) => {}
            }
        }
    }

    async fn deliver_edit_user_message(
        &self,
        request: EditUserMessageRequest,
        target: EditRouteTarget,
        relation: Option<RelatedSession>,
    ) {
        let Some(backend) = external_agent::AgentBackend::from_str_loose(&target.source) else {
            self.warn(&format!(
                "Edit dropped: unknown external-agent source {} for session {}",
                target.source,
                short_session(&target.managed_id)
            ));
            self.emit_edit_user_message_status(
                Some(request.requested_id),
                request.user_turn_index,
                "failed",
                format!("unknown external-agent source {}", target.source),
            );
            return;
        };
        if !backend.supports_user_message_rewind() {
            self.warn(&format!(
                "Edit dropped: {} session {} does not support user-message rewind yet",
                backend,
                short_session(&target.managed_id)
            ));
            self.emit_edit_user_message_status(
                Some(request.requested_id),
                request.user_turn_index,
                "failed",
                format!("{} does not support user-message rewind yet", backend),
            );
            return;
        }
        if request.user_turn_index == 0 {
            self.warn(&format!(
                "Edit dropped: invalid user turn index 0 for {} session {}",
                backend,
                short_session(&target.managed_id)
            ));
            self.emit_edit_user_message_status(
                Some(request.requested_id),
                request.user_turn_index,
                "failed",
                "invalid user turn index 0",
            );
            return;
        }
        let Some(user_turn_revision) = request.user_turn_revision else {
            self.warn(&format!(
                "Edit dropped: missing active-message revision for {} session {} user turn {}",
                backend,
                short_session(&target.managed_id),
                request.user_turn_index
            ));
            self.emit_edit_user_message_status(
                Some(request.requested_id),
                request.user_turn_index,
                "failed",
                "missing active-message revision",
            );
            return;
        };

        let resolved_attachments = self
            .resolve_session_attachments(
                &request.attachments,
                &target.session_dir,
                &target.project_root,
            )
            .await;
        if resolved_attachments.len() < request.attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} edit attachment(s) for {} session {}",
                resolved_attachments.len(),
                request.attachments.len(),
                backend,
                short_session(&target.managed_id)
            ));
        }
        let target_session_id = relation
            .as_ref()
            .filter(|rel| matches!(rel.relationship.as_str(), "side" | "subagent"))
            .map(|_| request.requested_id.clone());
        let msg = FollowUpMessage::edit_user_message(
            request.text,
            UserAttachments::from_items(resolved_attachments),
            request.user_turn_index,
            user_turn_revision,
            request.original_text,
            request.attachments,
        )
        .for_target(target_session_id);
        match target.follow_up_tx.try_send(msg) {
            Ok(()) => {
                self.emit_edit_user_message_status(
                    Some(request.requested_id.clone()),
                    request.user_turn_index,
                    "queued",
                    format!(
                        "edit queued for {} session {} user turn {}",
                        backend,
                        short_session(&target.managed_id),
                        request.user_turn_index
                    ),
                );
                self.config.bus.send(AppEvent::LogEntry {
                    session_id: Some(target.managed_id.clone()),
                    level: "info".to_string(),
                    source: "session-supervisor".to_string(),
                    content: format!(
                        "Edit queued for {} session {} user turn {}",
                        backend,
                        short_session(&target.managed_id),
                        request.user_turn_index
                    ),
                    turn: None,
                });
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.warn(&format!(
                    "Edit dropped: {} session {} in {} is not accepting input",
                    backend,
                    short_session(&target.managed_id),
                    target.project_root.display()
                ));
                self.emit_edit_user_message_status(
                    Some(request.requested_id),
                    request.user_turn_index,
                    "failed",
                    format!("{} session is not accepting input", backend),
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.warn(&format!(
                    "Edit dropped: {} session {} in {} input queue is full",
                    backend,
                    short_session(&target.managed_id),
                    target.project_root.display()
                ));
                self.emit_edit_user_message_status(
                    Some(request.requested_id),
                    request.user_turn_index,
                    "failed",
                    format!("{} session input queue is full", backend),
                );
            }
        }
    }

    async fn lookup_edit_route_target(
        &self,
        requested_id: &str,
    ) -> (String, Option<EditRouteTarget>, Option<RelatedSession>) {
        let home = self.logs_home();
        self.lookup_edit_route_target_in_home(requested_id, &home)
            .await
    }

    async fn lookup_edit_route_target_in_home(
        &self,
        requested_id: &str,
        home: &Path,
    ) -> (String, Option<EditRouteTarget>, Option<RelatedSession>) {
        let initial = {
            let state = self.state.lock().await;
            lookup_edit_route_target_in_state(&state, requested_id)
        };
        if initial.1.is_some() {
            return initial;
        }

        if let Some(target_id) = self
            .resolve_indexed_external_wrapper_managed_id_in_home(home, requested_id)
            .await
        {
            let state = self.state.lock().await;
            let routed = lookup_edit_route_target_in_state(&state, &target_id);
            if routed.1.is_some() {
                return routed;
            }
        }

        let mut fallback_candidates = vec![requested_id.to_string()];
        if initial.0 != requested_id {
            fallback_candidates.push(initial.0.clone());
        }
        for candidate in fallback_candidates {
            if !may_be_persisted_external_wrapper_id(&candidate) {
                continue;
            }
            if let Some(live_id) = self.resolve_persisted_external_managed_id(&candidate).await {
                let state = self.state.lock().await;
                let routed = lookup_edit_route_target_in_state(&state, &live_id);
                if routed.1.is_some() {
                    return routed;
                }
            }
        }

        initial
    }

    async fn route_interrupt(&self, session_id: Option<String>) {
        let requested_id = session_id.clone();
        let Some(target_id) = self.resolve_target_session_id(session_id).await else {
            self.warn("Interrupt dropped: no active managed session");
            return;
        };
        if let Some(requested_id) = requested_id.as_deref() {
            let state = self.state.lock().await;
            // Related sessions that are managed sessions in their own right
            // (native sub-agents) take interrupts directly; only related
            // backend threads inside a parent's process (Codex subagents)
            // cannot.
            if state
                .related_sessions
                .get(requested_id)
                .is_some_and(|rel| rel.relationship == "subagent")
                && !state.sessions.contains_key(requested_id)
            {
                drop(state);
                self.warn(&format!(
                    "Interrupt dropped: Codex subagent session {} does not support interrupts",
                    short_session(requested_id)
                ));
                return;
            }
        }
        if !self.session_is_managed(&target_id).await {
            self.warn(&format!(
                "Interrupt dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            return;
        }
        self.config.bus.send(AppEvent::InterruptRequested {
            session_id: requested_id.or(Some(target_id)),
        });
    }

    async fn stop_managed_session(
        &self,
        session_id: Option<String>,
        reason: &str,
    ) -> Option<StoppedManagedSession> {
        let reason = reason.trim();
        let reason = if reason.is_empty() {
            "stopped by user"
        } else {
            reason
        };
        let removed = {
            let mut state = self.state.lock().await;
            let requested_id = session_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .or_else(|| state.active_session_id.clone());
            let Some(requested_id) = requested_id else {
                drop(state);
                self.warn("Stop session dropped: no active managed session");
                return None;
            };
            // Related sessions that are managed sessions in their own right
            // (native sub-agents) can be stopped directly; only related
            // backend threads inside a parent's process (Codex threads)
            // must be stopped via their parent.
            if state.related_sessions.contains_key(&requested_id)
                && !state.sessions.contains_key(&requested_id)
            {
                drop(state);
                self.warn(&format!(
                    "Stop session dropped: {} is a related Codex thread; stop the parent session instead",
                    short_session(&requested_id)
                ));
                return None;
            }
            let Some(target_id) = state.resolve_session_id(&requested_id) else {
                drop(state);
                self.warn(&format!(
                    "Stop session dropped: session {} is not managed by this daemon",
                    short_session(&requested_id)
                ));
                return None;
            };
            state.remove_session(&target_id)
        };

        let Some((canonical, session)) = removed else {
            self.warn("Stop session dropped: no matching managed session");
            return None;
        };
        self.config.bus.send(AppEvent::SessionStopRequested {
            session_id: Some(canonical.clone()),
            reason: reason.to_string(),
        });
        self.config.bus.send(AppEvent::SessionEnded {
            session_id: canonical.clone(),
            reason: reason.to_string(),
            error_kind: None,
        });
        Some(StoppedManagedSession {
            session_id: canonical,
            source: session.source,
            finished_rx: session.finished_rx,
        })
    }

    async fn wait_for_stopped_session(&self, mut stopped: StoppedManagedSession) {
        let Some(finished_rx) = stopped.finished_rx.take() else {
            return;
        };
        match tokio::time::timeout(SESSION_STOP_WAIT_TIMEOUT, finished_rx).await {
            Ok(Ok(())) | Ok(Err(_)) => {}
            Err(_) => {
                self.warn(&format!(
                    "Restarting {} session {} before the previous backend confirmed shutdown",
                    stopped.source,
                    short_session(&stopped.session_id)
                ));
            }
        }
    }

    async fn restart_session(
        &self,
        source: String,
        session_id: String,
        resume_id: Option<String>,
        project_root: Option<String>,
        task: Option<String>,
        direct: Option<bool>,
        attachments: Vec<String>,
        overrides: LaunchOverrides,
    ) {
        let source_norm = source.trim().to_lowercase();
        if source_norm == "intendant" {
            self.warn("Restart with saved config is only available for external-agent sessions");
            return;
        }
        if external_agent::AgentBackend::from_str_loose(&source_norm).is_none() {
            self.loop_error(format!("Unsupported session source: {}", source));
            return;
        }
        let resume_token = resume_id.clone().unwrap_or_else(|| session_id.clone());
        let restart_key = format!("{}:{}", source_norm, resume_token);
        {
            let mut state = self.state.lock().await;
            if !state.mark_restart_requested(&restart_key) {
                drop(state);
                self.warn(&format!(
                    "Restart session ignored: {} was already restarted recently",
                    short_session(&resume_token)
                ));
                return;
            }
        }
        if let Some(existing_id) = self
            .find_managed_session_id(&source_norm, &session_id, &resume_token)
            .await
        {
            if let Some(stopped) = self
                .stop_managed_session(Some(existing_id), "restarting session")
                .await
            {
                self.wait_for_stopped_session(stopped).await;
            }
        }
        self.resume_session(
            source_norm,
            session_id,
            resume_id,
            project_root,
            task,
            direct,
            attachments,
            false,
            overrides,
            true,
        )
        .await;
    }

    async fn route_steer(
        &self,
        session_id: Option<String>,
        text: String,
        id: Option<String>,
        attachments: Vec<String>,
    ) {
        let requested_id = session_id.clone();
        let Some(target_id) = self.resolve_target_session_id(session_id).await else {
            self.warn("Steer dropped: no active managed session");
            return;
        };
        let entry = {
            let state = self.state.lock().await;
            let target_id = state
                .resolve_session_id(&target_id)
                .unwrap_or_else(|| target_id.clone());
            let requested_is_managed = requested_id
                .as_deref()
                .map(|id| state.sessions.contains_key(id))
                .unwrap_or(false);
            state.sessions.get(&target_id).map(|s| {
                let relation = requested_id
                    .as_deref()
                    .and_then(|id| state.related_sessions.get(id))
                    .cloned();
                (
                    s.session_id.clone(),
                    s.source.clone(),
                    s.project_root.clone(),
                    s.session_dir.clone(),
                    s.follow_up_tx.clone(),
                    relation,
                    requested_is_managed,
                )
            })
        };
        let Some((managed_id, source, project_root, session_dir, tx, relation, requested_is_managed)) =
            entry
        else {
            self.warn(&format!(
                "Steer dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            return;
        };
        // Related sessions that are managed sessions in their own right
        // (native sub-agents) take steers directly; only related backend
        // threads inside a parent's process (Codex subagents) cannot.
        if relation
            .as_ref()
            .is_some_and(|rel| rel.relationship == "subagent")
            && !requested_is_managed
        {
            self.warn(&format!(
                "Steer dropped: Codex subagent session {} does not support mid-turn steering; send a follow-up instead",
                short_session(requested_id.as_deref().unwrap_or(&managed_id))
            ));
            return;
        }

        let steer_id = id.unwrap_or_default();
        let event_session_id = requested_id.clone().or(Some(managed_id.clone()));
        if let Some(parsed) = parse_codex_slash_command(&text) {
            match parsed {
                Ok(command) => {
                    // Dispatch for every source — the attached loop (or the
                    // unattached-session responder) reports per-backend
                    // support honestly, so /goal works wherever a goal
                    // engine answers.
                    if source == "codex"
                        && relation
                            .as_ref()
                            .is_some_and(|rel| rel.relationship == "side")
                    {
                        self.warn(&format!(
                            "Slash command /{} is not supported for Codex side session {}; use the parent thread instead",
                            command.op,
                            short_session(requested_id.as_deref().unwrap_or(&managed_id))
                        ));
                        return;
                    }
                    if !attachments.is_empty() {
                        self.warn(&format!(
                            "Slash command /{} for {} session {} ignored {} steer attachment(s)",
                            command.op,
                            source,
                            short_session(&managed_id),
                            attachments.len()
                        ));
                    }
                    self.config.bus.send(AppEvent::ControlCommand(
                        event::ControlMsg::CodexThreadAction {
                            session_id: Some(managed_id),
                            op: command.op,
                            params: command.params,
                            origin: None,
                        },
                    ));
                    if !steer_id.trim().is_empty() {
                        self.config.bus.send(AppEvent::SteerDelivered {
                            session_id: event_session_id,
                            id: steer_id,
                            mid_turn: false,
                        });
                    }
                }
                Err(message) => self.warn(&message),
            }
            return;
        }
        if attachments.is_empty() {
            let ack_rx = self.config.bus.subscribe();
            self.config.bus.send(AppEvent::SteerRequested {
                session_id: event_session_id.clone(),
                text: text.clone(),
                id: steer_id.clone(),
            });
            if !steer_id.trim().is_empty() {
                spawn_text_steer_fallback(
                    self.config.bus.clone(),
                    ack_rx,
                    tx,
                    text,
                    steer_id,
                    event_session_id,
                );
            }
            return;
        }

        let resolved_attachments = self
            .resolve_session_attachments(&attachments, &session_dir, &project_root)
            .await;
        if resolved_attachments.len() < attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} steer attachment(s) for {} session {}",
                resolved_attachments.len(),
                attachments.len(),
                source,
                short_session(&managed_id)
            ));
        }
        let msg = FollowUpMessage::steer(
            text,
            UserAttachments::from_items(resolved_attachments),
            steer_id.clone(),
        )
        .for_target(requested_id.clone().or(Some(managed_id.clone())));
        if tx.send(msg).await.is_err() {
            self.warn(&format!(
                "Steer dropped: {} session {} in {} is not accepting input",
                source,
                short_session(&managed_id),
                project_root.display()
            ));
            return;
        }
        self.config.bus.send(AppEvent::SteerQueued {
            session_id: requested_id.or(Some(managed_id)),
            id: steer_id,
            reason: "attachments are queued for the next turn".to_string(),
        });
    }

    async fn route_cancel_steer(
        &self,
        session_id: Option<String>,
        id: Option<String>,
        reason: Option<String>,
    ) {
        let requested_id = session_id.clone();
        let event_session_id =
            if let Some(target_id) = self.resolve_target_session_id(session_id).await {
                let state = self.state.lock().await;
                let managed_id = state.resolve_session_id(&target_id).unwrap_or(target_id);
                requested_id.or(Some(managed_id))
            } else {
                requested_id
            };
        self.config.bus.send(AppEvent::SteerCancelRequested {
            session_id: event_session_id,
            id,
            reason: reason.unwrap_or_else(|| "cleared by user".to_string()),
        });
    }

    async fn route_cancel_follow_up(
        &self,
        session_id: Option<String>,
        id: Option<String>,
        reason: Option<String>,
    ) {
        let requested_id = session_id.clone();
        let event_session_id =
            if let Some(target_id) = self.resolve_target_session_id(session_id).await {
                let state = self.state.lock().await;
                let managed_id = state.resolve_session_id(&target_id).unwrap_or(target_id);
                requested_id.or(Some(managed_id))
            } else {
                requested_id
            };
        let reason = reason.unwrap_or_else(|| "cleared by user".to_string());
        self.config.bus.send(AppEvent::FollowUpCancelRequested {
            session_id: event_session_id.clone(),
            id: id.clone(),
            reason: reason.clone(),
        });
        emit_follow_up_status(
            &self.config.bus,
            event_session_id,
            &id,
            None,
            "cancelled",
            Some(&reason),
        );
    }

    async fn resolve_approval(
        &self,
        session_id: Option<String>,
        approval_id: u64,
        response: event::ApprovalResponse,
        action: &str,
    ) {
        let Some(target_id) = self.resolve_target_session_id(session_id).await else {
            self.warn("Approval response dropped: no active managed session");
            return;
        };
        let registry = {
            let state = self.state.lock().await;
            let target_id = state
                .resolve_session_id(&target_id)
                .unwrap_or_else(|| target_id.clone());
            state
                .sessions
                .get(&target_id)
                .map(|session| session.approval_registry.clone())
        };
        let Some(registry) = registry else {
            self.warn(&format!(
                "Approval response dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            return;
        };
        let responder = registry.lock().unwrap().remove(&approval_id);
        match responder {
            Some(tx) => {
                let _ = tx.send(response);
                self.config.bus.send(AppEvent::ApprovalResolved {
                    session_id: Some(target_id),
                    id: approval_id,
                    action: action.to_string(),
                });
            }
            None => {
                self.warn(&format!(
                    "Approval response dropped: id {} is not pending for session {}",
                    approval_id,
                    short_session(&target_id)
                ));
            }
        }
    }

    async fn rename_session(
        &self,
        session_id: String,
        backend_session_id: Option<String>,
        source: Option<String>,
        name: String,
    ) {
        let managed = {
            let state = self.state.lock().await;
            let resolved_id = state
                .resolve_session_id(&session_id)
                .unwrap_or_else(|| session_id.clone());
            state
                .sessions
                .get(&resolved_id)
                .map(|session| (session.session_id.clone(), session.source.clone()))
        };

        if let Some((managed_id, managed_source)) = managed.as_ref() {
            if managed_source == "codex" {
                self.config.bus.send(AppEvent::ControlCommand(
                    event::ControlMsg::CodexThreadAction {
                        session_id: Some(managed_id.clone()),
                        op: "rename".to_string(),
                        params: serde_json::json!({ "name": name }),
                        origin: None,
                    },
                ));
                return;
            }
        }

        let source = managed
            .map(|(_, source)| source)
            .or(source)
            .unwrap_or_else(|| "intendant".to_string());
        let normalized_source = crate::session_names::normalize_source(&source);
        let persistence_session_id = if normalized_source == "intendant" {
            session_id.as_str()
        } else {
            backend_session_id.as_deref().unwrap_or(&session_id)
        };
        let result = match dirs::home_dir() {
            Some(home) => crate::session_names::rename_session(
                &home,
                &normalized_source,
                persistence_session_id,
                &name,
            ),
            None => Err("could not resolve home directory".to_string()),
        };

        match result {
            Ok(name) => {
                self.config.bus.send(AppEvent::SessionRenameResult {
                    session_id,
                    source: Some(normalized_source),
                    name: Some(name.clone()),
                    success: true,
                    message: format!("Renamed session to {}", name),
                });
            }
            Err(message) => {
                self.config.bus.send(AppEvent::SessionRenameResult {
                    session_id,
                    source: Some(normalized_source),
                    name: None,
                    success: false,
                    message,
                });
            }
        }
    }

    async fn configure_session_agent(
        &self,
        session_id: String,
        source: Option<String>,
        backend_session_id: Option<String>,
        intendant_session_id: Option<String>,
        overrides: LaunchOverrides,
    ) {
        let managed = {
            let state = self.state.lock().await;
            state
                .resolve_session_id(&session_id)
                .and_then(|resolved_id| state.sessions.get(&resolved_id))
                .map(|session| {
                    (
                        session.session_id.clone(),
                        session.source.clone(),
                        session.session_dir.clone(),
                    )
                })
        };

        let normalized_source = managed
            .as_ref()
            .map(|(_, source, _)| source.clone())
            .or(source)
            .map(|source| crate::session_names::normalize_source(&source))
            .unwrap_or_default();
        let Some(backend) = external_agent::AgentBackend::from_str_loose(&normalized_source) else {
            let message = "Session config failed: choose an external agent session".to_string();
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: normalized_source,
                backend_session_id,
                intendant_session_id,
                persisted_session_ids: Vec::new(),
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
            return;
        };
        let is_codex = matches!(backend, external_agent::AgentBackend::Codex);
        let is_claude = matches!(backend, external_agent::AgentBackend::ClaudeCode);
        let clear_codex_sandbox =
            is_codex && session_config_clear_value(overrides.codex_sandbox.as_deref());
        let clear_codex_approval_policy =
            is_codex && session_config_clear_value(overrides.codex_approval_policy.as_deref());
        // The clear sentinel must be checked on the RAW wire value, before
        // from_wire's normalization, and re-applied after the merge passes
        // below re-fill cleared fields from the persisted configs — same
        // dance as sandbox/approval. Otherwise "inherit" would either pin
        // the default into the overlay or be resurrected by the merge.
        let clear_codex_managed_context =
            is_codex && session_config_clear_value(overrides.codex_managed_context.as_deref());
        let clear_codex_context_archive =
            is_codex && session_config_clear_value(overrides.codex_context_archive.as_deref());
        let clear_claude_model =
            is_claude && session_config_clear_value(overrides.claude_model.as_deref());
        // "default" is a REAL permission mode (pinnable under a stricter
        // global); only inherit/global/empty clear it.
        let clear_claude_permission_mode = is_claude
            && session_config_clear_value_keeping_default(
                overrides.claude_permission_mode.as_deref(),
            );
        let clear_claude_allowed_tools =
            is_claude && session_config_clear_value(overrides.claude_allowed_tools.as_deref());
        let clear_claude_effort =
            is_claude && session_config_clear_value(overrides.claude_effort.as_deref());
        let mut config = crate::session_config::from_wire_fields(
            overrides.as_wire_fields(backend.as_short_str()),
        );
        let home = self.logs_home();
        if let Some(existing) = crate::session_config::load_for_resume(
            &home,
            backend.as_short_str(),
            &session_id,
            backend_session_id.as_deref(),
        ) {
            config.merge_missing_from(existing);
        }
        if let Some((_, _, session_dir)) = managed.as_ref() {
            if let Some(existing) = crate::session_config::read_log_dir_config(session_dir) {
                config.merge_missing_from(existing);
            }
        }
        if let Some(intendant_id) = intendant_session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            if let Some(dir) = session_log::SessionLog::find_session_by_id(intendant_id) {
                if let Some(existing) = crate::session_config::read_log_dir_config(&dir) {
                    config.merge_missing_from(existing);
                }
            }
        }
        if clear_codex_sandbox {
            config.codex_sandbox = None;
        }
        if clear_codex_approval_policy {
            config.codex_approval_policy = None;
        }
        if clear_codex_managed_context {
            config.codex_managed_context = None;
        }
        if clear_codex_context_archive {
            config.codex_context_archive = None;
        }
        if clear_claude_model {
            config.claude_model = None;
        }
        if clear_claude_permission_mode {
            config.claude_permission_mode = None;
        }
        if clear_claude_allowed_tools {
            config.claude_allowed_tools = None;
        }
        if clear_claude_effort {
            config.claude_effort = None;
        }
        if config.is_empty() {
            let message = "Session config failed: no launch settings supplied".to_string();
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids: Vec::new(),
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
            return;
        }

        let mut errors = Vec::new();
        let mut persisted_session_ids = Vec::new();
        let mut note_persisted = |id: &str| {
            let id = id.trim();
            if !id.is_empty() && !persisted_session_ids.iter().any(|existing| existing == id) {
                persisted_session_ids.push(id.to_string());
            }
        };
        if let Some((managed_id, _, session_dir)) = managed.as_ref() {
            if let Err(e) = crate::session_config::write_log_dir_config(session_dir, &config) {
                errors.push(e);
            } else {
                note_persisted(managed_id);
            }
        }
        let intendant_id = intendant_session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if let Some(intendant_id) = intendant_id {
            if let Some(dir) = session_log::SessionLog::find_session_by_id(intendant_id) {
                if let Err(e) = crate::session_config::write_log_dir_config(&dir, &config) {
                    errors.push(e);
                } else {
                    note_persisted(intendant_id);
                }
            }
        }

        let external_ids = [
            backend_session_id.as_deref(),
            Some(session_id.as_str()),
            managed
                .as_ref()
                .map(|(managed_id, _, _)| managed_id.as_str()),
        ];
        let mut wrote_external = false;
        for external_id in external_ids
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            if !external_agent::source_session_id_is_canonical(backend.as_short_str(), external_id)
            {
                continue;
            }
            wrote_external = true;
            if let Err(e) = crate::session_config::replace_external_overlay(
                &home,
                backend.as_short_str(),
                external_id,
                &config,
            ) {
                errors.push(e);
            } else {
                note_persisted(external_id);
            }
        }

        if !wrote_external && managed.is_none() && intendant_id.is_none() {
            let message = "Session config failed: no persistable session id".to_string();
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids,
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
            return;
        }
        if errors.is_empty() {
            let message = format!(
                "Session {} launch config saved for {} (takes effect on next attach/resume)",
                short_session(&session_id),
                backend.as_short_str()
            );
            self.info(&message);
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids,
                success: true,
                message,
            });
        } else {
            let message = format!("Session config partially failed: {}", errors.join("; "));
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids,
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
        }
    }

    async fn apply_session_identity(
        &self,
        session_id: String,
        source: String,
        backend_session_id: String,
    ) {
        let source = crate::session_names::normalize_source(&source);
        if !external_agent::source_session_id_is_canonical(&source, &backend_session_id) {
            return;
        }
        {
            // Record the identity even for sessions this supervisor does not
            // manage (e.g. the CLI main loop's agent) so the thread-action
            // fallback responder knows another owner will answer for them.
            let mut state = self.state.lock().await;
            state.known_external_sessions.insert(session_id.clone());
            state
                .known_external_sessions
                .insert(backend_session_id.clone());
        }
        if session_id == backend_session_id {
            return;
        }

        let name_to_persist = {
            let mut state = self.state.lock().await;
            let Some(current_key) = state.resolve_session_id(&session_id) else {
                return;
            };
            if current_key == backend_session_id {
                state
                    .session_aliases
                    .insert(session_id, backend_session_id.clone());
                state
                    .sessions
                    .get(&backend_session_id)
                    .and_then(|session| session.name.clone())
            } else if state.sessions.contains_key(&backend_session_id) {
                let existing_name = state
                    .sessions
                    .get(&backend_session_id)
                    .and_then(|session| session.name.clone())
                    .or_else(|| {
                        state
                            .sessions
                            .get(&current_key)
                            .and_then(|session| session.name.clone())
                    });
                let name = if let Some(mut session) = state.sessions.remove(&current_key) {
                    if session.name.is_none() {
                        session.name = existing_name.clone();
                    }
                    let name = session.name.clone();
                    session.session_id = backend_session_id.clone();
                    session.source = source.clone();
                    state.sessions.insert(backend_session_id.clone(), session);
                    state.session_aliases.retain(|alias, target| {
                        alias != &backend_session_id && target != &current_key
                    });
                    state
                        .session_aliases
                        .insert(session_id.clone(), backend_session_id.clone());
                    state
                        .session_aliases
                        .insert(current_key.clone(), backend_session_id.clone());
                    name
                } else {
                    state
                        .session_aliases
                        .insert(session_id.clone(), backend_session_id.clone());
                    state
                        .session_aliases
                        .insert(current_key.clone(), backend_session_id.clone());
                    existing_name
                };
                if state.active_session_id.as_deref() == Some(&session_id)
                    || state.active_session_id.as_deref() == Some(&current_key)
                    || state.active_session_id.as_deref() == Some(&backend_session_id)
                {
                    state.active_session_id = Some(backend_session_id.clone());
                }
                name
            } else {
                let Some(mut session) = state.sessions.remove(&current_key) else {
                    return;
                };
                let name = session.name.clone();
                session.session_id = backend_session_id.clone();
                session.source = source.clone();
                state.sessions.insert(backend_session_id.clone(), session);
                // The entry is now directly keyed by the backend id; drop the
                // pre-identity alias register_session added under that id so
                // no alias entry shadows a live key.
                state.session_aliases.remove(&backend_session_id);
                state
                    .session_aliases
                    .insert(session_id.clone(), backend_session_id.clone());
                state
                    .session_aliases
                    .insert(current_key.clone(), backend_session_id.clone());
                if state.active_session_id.as_deref() == Some(&session_id)
                    || state.active_session_id.as_deref() == Some(&current_key)
                {
                    state.active_session_id = Some(backend_session_id.clone());
                }
                name
            }
        };

        if let Some(name) = name_to_persist {
            persist_external_session_name(&self.config.bus, &source, &backend_session_id, &name);
        }
    }

    async fn observe_lifecycle_event(&self, event: &AppEvent) {
        match event {
            AppEvent::SessionStarted { session_id, .. } => {
                self.update_session_phase(Some(session_id), "thinking")
                    .await;
            }
            AppEvent::TurnStarted { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "thinking")
                    .await;
            }
            AppEvent::AgentStarted { session_id, .. }
            | AppEvent::AgentOutput { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "running")
                    .await;
            }
            AppEvent::ApprovalRequired { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "waiting_approval")
                    .await;
            }
            AppEvent::HumanQuestionDetected { .. } => {
                self.update_session_phase(None, "waiting_human").await;
            }
            AppEvent::InterruptRequested { session_id } => {
                self.update_session_phase(session_id.as_deref(), "interrupting")
                    .await;
            }
            AppEvent::Interrupted { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "interrupted")
                    .await;
            }
            AppEvent::RoundComplete { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "idle")
                    .await;
            }
            AppEvent::TaskComplete { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "done")
                    .await;
            }
            AppEvent::SessionEnded { session_id, .. } => {
                self.update_session_phase(Some(session_id), "done").await;
            }
            AppEvent::StatusUpdate {
                session_id, phase, ..
            } => {
                self.update_session_phase(Some(session_id), phase).await;
            }
            AppEvent::SessionCapabilities {
                session_id,
                capabilities,
            } => {
                // Remember which ops a live loop serves for this session so
                // the thread-action fallback defers to it (see
                // report_unattached_codex_thread_action).
                let ops: std::collections::HashSet<String> = capabilities
                    .thread_actions
                    .iter()
                    .map(|op| op.trim().to_string())
                    .filter(|op| !op.is_empty())
                    .collect();
                let mut state = self.state.lock().await;
                if ops.is_empty() {
                    state.advertised_thread_actions.remove(session_id);
                } else {
                    state
                        .advertised_thread_actions
                        .insert(session_id.clone(), ops);
                }
            }
            _ => {}
        }
    }

    async fn apply_session_relationship(
        &self,
        parent_session_id: String,
        child_session_id: String,
        relationship: String,
    ) {
        let mut state = self.state.lock().await;
        state.apply_related_session(&parent_session_id, &child_session_id, &relationship);
    }

    async fn remove_session_alias(&self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        state.session_aliases.remove(session_id);
        state.related_sessions.remove(session_id);
        state.known_external_sessions.remove(session_id);
        state.advertised_thread_actions.remove(session_id);
    }

    async fn update_session_phase(&self, session_id: Option<&str>, phase: &str) {
        let phase = normalize_supervisor_phase(phase);
        let mut state = self.state.lock().await;
        let target_id = session_id
            .and_then(|id| state.resolve_session_id(id))
            .or_else(|| state.active_session_id.clone());
        let Some(target_id) = target_id else {
            return;
        };
        if let Some(session) = state.sessions.get_mut(&target_id) {
            session.phase = phase;
        }
    }

    async fn resolve_target_session_id(&self, session_id: Option<String>) -> Option<String> {
        let state = self.state.lock().await;
        let requested = session_id.or_else(|| state.active_session_id.clone())?;
        Some(state.resolve_session_id(&requested).unwrap_or(requested))
    }

    async fn session_is_managed(&self, session_id: &str) -> bool {
        let state = self.state.lock().await;
        state.session_is_managed(session_id)
    }

    async fn resolve_persisted_external_managed_id(&self, session_id: &str) -> Option<String> {
        let (source, backend_session_id) =
            persisted_external_identity_for_session_in_home(&self.logs_home(), session_id)?;
        let state = self.state.lock().await;
        let resolved_id = state.resolve_session_id(&backend_session_id)?;
        state
            .sessions
            .get(&resolved_id)
            .filter(|session| session.source == source)
            .map(|session| session.session_id.clone())
    }

    async fn resolve_indexed_external_wrapper_managed_id_in_home(
        &self,
        home: &Path,
        backend_session_id: &str,
    ) -> Option<String> {
        let backend_session_id = backend_session_id.trim();
        if backend_session_id.is_empty() {
            return None;
        }
        let candidates = [external_agent::AgentBackend::Codex]
            .into_iter()
            .filter(|backend| {
                backend.supports_user_message_rewind()
                    && external_agent::source_session_id_is_canonical(
                        backend.as_short_str(),
                        backend_session_id,
                    )
            })
            .flat_map(|backend| {
                let source = backend.as_short_str().to_string();
                crate::external_wrapper_index::wrappers_for(home, &source, backend_session_id)
                    .into_iter()
                    .map(move |record| (source.clone(), record.intendant_session_id))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }
        let state = self.state.lock().await;
        candidates.into_iter().find_map(|(source, wrapper_id)| {
            let resolved_id = state.resolve_session_id(&wrapper_id)?;
            state
                .sessions
                .get(&resolved_id)
                .filter(|session| {
                    session.source == source && managed_session_accepts_external_input(session)
                })
                .map(|session| session.session_id.clone())
        })
    }

    async fn clear_external_attach_request(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        state.clear_external_attach_requested(keys);
    }

    async fn register_session(
        &self,
        session_id: String,
        source: String,
        phase: String,
        project_root: PathBuf,
        session_dir: PathBuf,
        follow_up_tx: mpsc::Sender<FollowUpMessage>,
        approval_registry: event::ApprovalRegistry,
        name: Option<String>,
        finished_rx: Option<oneshot::Receiver<()>>,
        identity_alias: Option<String>,
        depth: usize,
        sub_agent_children: Option<SubAgentChildrenMap>,
    ) -> u64 {
        let rehydrated_related = load_related_sessions_from_log(&session_dir);
        let mut state = self.state.lock().await;
        state.next_session_instance = state.next_session_instance.saturating_add(1);
        let instance_id = state.next_session_instance;
        state.active_session_id = Some(session_id.clone());
        state.session_aliases.remove(&session_id);
        state.sessions.insert(
            session_id.clone(),
            ManagedSession {
                session_id: session_id.clone(),
                source,
                name,
                phase,
                project_root,
                session_dir,
                follow_up_tx,
                approval_registry,
                instance_id,
                finished_rx,
                depth,
                sub_agent_children,
            },
        );
        // Pre-identity alias: a resumed external session is only addressable
        // by its backend/resume token once the backend reports its identity
        // (several seconds after spawn). Registering the token as an alias in
        // the same lock closes that window — concurrent resumes of the same
        // thread dedupe against this wrapper instead of spawning a duplicate,
        // and follow-ups targeted at the token queue into this session's
        // channel instead of failing "not managed by this daemon".
        // apply_session_identity() drops the alias when the entry is re-keyed
        // to the backend id itself.
        if let Some(alias) = identity_alias.filter(|alias| alias != &session_id) {
            state.session_aliases.insert(alias, session_id);
        }
        for rel in rehydrated_related {
            state.apply_related_session(
                &rel.parent_session_id,
                &rel.child_session_id,
                &rel.relationship,
            );
        }
        instance_id
    }

    async fn finish_session(
        &self,
        session_id: String,
        session_instance_id: u64,
        session_log: SharedSessionLog,
        task: String,
        result: Result<LoopStats, CallerError>,
    ) {
        let reason = match &result {
            Ok(stats) => {
                let outcome = stats.terminal_outcome.as_deref().unwrap_or("completed");
                slog(&session_log, |log| {
                    log.write_summary_with_rounds(&task, outcome, stats.turns, Some(stats.rounds));
                });
                outcome.to_string()
            }
            Err(e) => {
                slog(&session_log, |log| {
                    log.write_summary(&task, &format!("error: {}", e), 0);
                });
                format!("error: {}", e)
            }
        };
        let error_kind = result
            .as_ref()
            .err()
            .and_then(|e| e.session_end_kind())
            .map(str::to_string);

        let (ended_session_id, orphaned_children) = {
            let mut state = self.state.lock().await;
            // Sub-agent children die with their parent, like Codex
            // subagent threads do. Capture them before remove_session
            // purges the relationship records.
            let orphaned_children: Vec<String> = state
                .related_sessions
                .iter()
                .filter(|(child, rel)| {
                    rel.relationship == "subagent"
                        && rel.parent_session_id == session_id
                        && state.sessions.contains_key(child.as_str())
                })
                .map(|(child, _)| child.clone())
                .collect();
            let ended = if session_instance_id == 0 {
                Some(
                    state
                        .remove_session(&session_id)
                        .map(|(canonical, _)| canonical)
                        .unwrap_or_else(|| session_id.clone()),
                )
            } else {
                state
                    .remove_session_instance(&session_id, session_instance_id)
                    .map(|(canonical, _)| canonical)
            };
            // Only cascade when the parent actually ended (not when a
            // superseded instance retired).
            let orphaned_children = if ended.is_some() {
                orphaned_children
            } else {
                Vec::new()
            };
            (ended, orphaned_children)
        };

        if let Some(ended_session_id) = ended_session_id.clone() {
            self.config.bus.send(AppEvent::SessionEnded {
                session_id: ended_session_id.clone(),
                reason,
                error_kind,
            });
        }

        for child_id in orphaned_children {
            self.warn(&format!(
                "Stopping sub-agent {} because its parent session ended",
                short_session(&child_id)
            ));
            if let Some(stopped) = self
                .stop_managed_session(Some(child_id), "parent session ended")
                .await
            {
                self.wait_for_stopped_session(stopped).await;
            }
        }

        if let Some(ref shared_session) = self.config.shared_session {
            let mut state = shared_session.write().await;
            let matches_current = state
                .session_log
                .as_ref()
                .map(|log| {
                    let log_session_id = log.lock().ok().map(|log| log.session_id().to_string());
                    Arc::ptr_eq(log, &session_log)
                        || log_session_id.as_deref() == Some(&session_id)
                        || ended_session_id
                            .as_deref()
                            .is_some_and(|id| log_session_id.as_deref() == Some(id))
                })
                .unwrap_or(false);
            if matches_current {
                state.session_log = None;
                state.query_ctx = None;
            }
        }
    }

    async fn activate_shared_session(&self, session_log: SharedSessionLog) {
        if let Some(ref shared_session) = self.config.shared_session {
            let mut state = shared_session.write().await;
            state.session_log = Some(session_log);
        }
    }

    async fn project_with_runtime_config(
        &self,
        root: PathBuf,
        backend: Option<&external_agent::AgentBackend>,
    ) -> Result<Project, CallerError> {
        let mut project = Project::from_root(root)?;
        match backend {
            Some(external_agent::AgentBackend::Codex) => {
                let current = self.config.shared_codex_config.read().await.clone();
                let cfg = &mut project.config.agent.codex;
                cfg.command = current.command;
                cfg.managed_command = current.managed_command;
                cfg.sandbox = current.sandbox;
                cfg.approval_policy = current.approval_policy;
                cfg.model = current.model;
                cfg.reasoning_effort = current.reasoning_effort;
                cfg.service_tier = current.service_tier;
                cfg.web_search = current.web_search;
                cfg.network_access = current.network_access;
                cfg.writable_roots = current.writable_roots;
                cfg.managed_context = current.managed_context;
                cfg.context_archive = current.context_archive;
            }
            Some(external_agent::AgentBackend::ClaudeCode) => {
                let current = self.config.shared_claude_config.read().await.clone();
                let cfg = &mut project.config.agent.claude_code;
                cfg.model = current.model;
                cfg.permission_mode = current.permission_mode;
                cfg.allowed_tools = current.allowed_tools;
            }
            None => {}
        }
        Ok(project)
    }

    fn loop_error(&self, message: String) {
        self.config.bus.send(AppEvent::LoopError(message));
    }

    fn warn(&self, message: &str) {
        self.config.bus.send(AppEvent::LogEntry {
            session_id: None,
            level: "warn".to_string(),
            source: "session-supervisor".to_string(),
            content: message.to_string(),
            turn: None,
        });
    }

    fn info(&self, message: &str) {
        self.config.bus.send(AppEvent::LogEntry {
            session_id: None,
            level: "info".to_string(),
            source: "session-supervisor".to_string(),
            content: message.to_string(),
            turn: None,
        });
    }

    async fn emit_attached_status(&self, session_id: &str, source: &str) {
        let autonomy = self.config.autonomy.read().await.level.to_string();
        let phase = {
            let state = self.state.lock().await;
            state
                .resolve_session_id(session_id)
                .and_then(|id| state.sessions.get(&id).map(|session| session.phase.clone()))
                .unwrap_or_else(|| "idle".to_string())
        };
        self.config.bus.send(AppEvent::StatusUpdate {
            turn: 0,
            phase,
            autonomy,
            session_id: session_id.to_string(),
            task: format!("Open {} session {}", source, short_session(session_id)),
        });
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

fn lookup_edit_route_target_in_state(
    state: &SupervisorState,
    requested_id: &str,
) -> (String, Option<EditRouteTarget>, Option<RelatedSession>) {
    let relation = state.related_sessions.get(requested_id).cloned();
    let target_id = state
        .resolve_session_id(requested_id)
        .unwrap_or_else(|| requested_id.to_string());
    let entry = state
        .sessions
        .get(&target_id)
        .filter(|session| managed_session_accepts_external_input(session))
        .map(|s| EditRouteTarget {
            managed_id: s.session_id.clone(),
            source: s.source.clone(),
            project_root: s.project_root.clone(),
            session_dir: s.session_dir.clone(),
            follow_up_tx: s.follow_up_tx.clone(),
        });
    (target_id, entry, relation)
}

fn may_be_persisted_external_wrapper_id(session_id: &str) -> bool {
    uuid::Uuid::parse_str(session_id.trim()).is_ok()
}

fn path_file_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionAgentSelection {
    Configured,
    Internal,
    External(external_agent::AgentBackend),
}

impl SessionAgentSelection {
    fn from_wire(agent: Option<&str>) -> Result<Self, String> {
        let Some(agent) = agent else {
            return Ok(Self::Configured);
        };
        let trimmed = agent.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("configured") {
            return Ok(Self::Configured);
        }
        let lowered = trimmed.to_ascii_lowercase();
        if matches!(
            lowered.as_str(),
            "internal" | "intendant" | "native" | "none"
        ) {
            return Ok(Self::Internal);
        }
        external_agent::AgentBackend::from_str_loose(trimmed)
            .map(Self::External)
            .ok_or_else(|| {
                format!(
                    "unknown agent '{}' (expected internal, codex, or claude-code)",
                    trimmed
                )
            })
    }
}

fn codex_fast_new_session_agent(agent: Option<&str>) -> Result<String, String> {
    match SessionAgentSelection::from_wire(agent)? {
        SessionAgentSelection::Configured => Ok("codex".to_string()),
        SessionAgentSelection::External(external_agent::AgentBackend::Codex) => {
            Ok("codex".to_string())
        }
        SessionAgentSelection::Internal => {
            Err("/fast can only start a new Codex external-agent session".to_string())
        }
        SessionAgentSelection::External(other) => Err(format!(
            "/fast can only start a new Codex external-agent session; selected {other}"
        )),
    }
}

fn normalize_session_agent_command(command: Option<&str>) -> Option<String> {
    command
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_session_codex_managed_context(mode: Option<&str>) -> Option<String> {
    crate::session_config::normalize_codex_managed_context(mode)
}

/// One-shot per-session launch overrides carried by a configure, resume, or
/// restart request. Raw wire values (clear sentinels intact) — backend
/// gating and normalization happen in `session_config::from_wire_fields`.
#[derive(Debug, Default)]
struct LaunchOverrides {
    agent_command: Option<String>,
    codex_sandbox: Option<String>,
    codex_approval_policy: Option<String>,
    codex_managed_context: Option<String>,
    codex_context_archive: Option<String>,
    claude_model: Option<String>,
    claude_permission_mode: Option<String>,
    claude_allowed_tools: Option<String>,
    claude_effort: Option<String>,
}

impl LaunchOverrides {
    /// The matching normalizer input for `session_config::from_wire_fields`.
    fn as_wire_fields<'a>(
        &'a self,
        source: &'a str,
    ) -> crate::session_config::WireSessionAgentFields<'a> {
        crate::session_config::WireSessionAgentFields {
            source: Some(source),
            agent_command: self.agent_command.as_deref(),
            codex_sandbox: self.codex_sandbox.as_deref(),
            codex_approval_policy: self.codex_approval_policy.as_deref(),
            codex_managed_context: self.codex_managed_context.as_deref(),
            codex_context_archive: self.codex_context_archive.as_deref(),
            codex_service_tier: None,
            claude_model: self.claude_model.as_deref(),
            claude_permission_mode: self.claude_permission_mode.as_deref(),
            claude_allowed_tools: self.claude_allowed_tools.as_deref(),
            claude_effort: self.claude_effort.as_deref(),
        }
    }
}

fn session_config_clear_value(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .map(|value| value.is_empty() || matches!(value, "inherit" | "default" | "global"))
        .unwrap_or(false)
}

/// Clear sentinel for the Claude permission-mode field, where "default" is a
/// real pinnable mode (unlike every other launch field).
fn session_config_clear_value_keeping_default(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .map(|value| value.is_empty() || matches!(value, "inherit" | "global"))
        .unwrap_or(false)
}

fn normalize_session_codex_sandbox(mode: Option<&str>) -> Option<String> {
    crate::session_config::normalize_codex_sandbox(mode)
}

fn normalize_session_codex_approval_policy(policy: Option<&str>) -> Option<String> {
    crate::session_config::normalize_codex_approval_policy(policy)
}

fn normalize_session_codex_context_archive(mode: Option<&str>) -> Option<String> {
    crate::session_config::normalize_codex_context_archive(mode)
}

fn normalize_session_codex_service_tier(tier: Option<&str>) -> Option<String> {
    crate::project::normalize_codex_service_tier(tier)
}

fn normalize_session_name_option(name: Option<&str>) -> Result<Option<String>, String> {
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => crate::session_names::normalize_session_name(name).map(Some),
        None => Ok(None),
    }
}

fn apply_session_agent_command(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    command: String,
) {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.command = command;
        }
        external_agent::AgentBackend::ClaudeCode => {
            project.config.agent.claude_code.command = command;
        }
    }
}

fn apply_session_codex_managed_context(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.managed_context =
                crate::project::normalize_codex_managed_context(&mode);
            Ok(())
        }
        _ => Err("codex_managed_context requires Codex".to_string()),
    }
}

fn apply_session_claude_model(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    model: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::ClaudeCode => {
            project.config.agent.claude_code.model = Some(model);
            Ok(())
        }
        _ => Err("claude_model requires Claude Code".to_string()),
    }
}

fn apply_session_claude_permission_mode(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::ClaudeCode => {
            project.config.agent.claude_code.permission_mode =
                crate::project::normalize_claude_permission_mode(&mode);
            Ok(())
        }
        _ => Err("claude_permission_mode requires Claude Code".to_string()),
    }
}

fn apply_session_claude_effort(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    effort: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::ClaudeCode => {
            project.config.agent.claude_code.effort =
                crate::project::normalize_claude_effort(Some(&effort));
            Ok(())
        }
        _ => Err("claude_effort requires Claude Code".to_string()),
    }
}

fn apply_session_codex_sandbox(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.sandbox = crate::project::normalize_sandbox_mode(&mode);
            Ok(())
        }
        _ => Err("codex_sandbox requires Codex".to_string()),
    }
}

fn apply_session_codex_approval_policy(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    policy: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.approval_policy =
                crate::project::normalize_approval_policy(&policy);
            Ok(())
        }
        _ => Err("codex_approval_policy requires Codex".to_string()),
    }
}

fn apply_session_codex_context_archive(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.context_archive =
                crate::project::normalize_codex_context_archive(&mode);
            Ok(())
        }
        _ => Err("codex_context_archive requires Codex".to_string()),
    }
}

fn effective_session_agent_config_from_project(
    backend: &external_agent::AgentBackend,
    project: &Project,
    overrides: Option<&crate::session_config::SessionAgentConfig>,
) -> crate::session_config::SessionAgentConfig {
    let mut config = crate::session_config::from_project(backend, project);
    if matches!(backend, external_agent::AgentBackend::Codex) {
        if let Some(overrides) = overrides {
            if overrides.codex_service_tier.is_some() {
                config.codex_service_tier = overrides.codex_service_tier.clone();
            }
            if overrides.codex_home.is_some() {
                config.codex_home = overrides.codex_home.clone();
            }
        }
    }
    // Fork lineage is a per-session fact, never derivable from the project.
    if let Some(overrides) = overrides {
        if overrides.forked_from.is_some() {
            config.forked_from = overrides.forked_from.clone();
        }
    }
    config
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

fn persist_external_session_name(bus: &EventBus, source: &str, session_id: &str, name: &str) {
    let source = crate::session_names::normalize_source(source);
    if source == "intendant" || name.trim().is_empty() {
        return;
    }
    let result = dirs::home_dir()
        .ok_or_else(|| "could not resolve home directory".to_string())
        .and_then(|home| crate::session_names::rename_session(&home, &source, session_id, name));
    if let Err(message) = result {
        bus.send(AppEvent::LogEntry {
            session_id: Some(session_id.to_string()),
            level: "warn".to_string(),
            source: "session-supervisor".to_string(),
            content: format!("Failed to persist session name: {}", message),
            turn: None,
        });
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CodexSlashCommand {
    op: String,
    params: serde_json::Value,
}

fn parse_codex_slash_command(text: &str) -> Option<Result<CodexSlashCommand, String>> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut split = rest.splitn(2, char::is_whitespace);
    let name = split.next()?.trim().to_ascii_lowercase();
    let args = split.next().unwrap_or("").trim();

    match name.as_str() {
        "fork" => {
            let mut params = serde_json::Map::new();
            let fork_name = unquote_slash_value(args);
            if !fork_name.is_empty() {
                params.insert("name".to_string(), serde_json::Value::String(fork_name));
            }
            Some(Ok(CodexSlashCommand {
                op: "fork".to_string(),
                params: serde_json::Value::Object(params),
            }))
        }
        "side" | "btw" => {
            let mut params = serde_json::Map::new();
            let prompt = unquote_slash_value(args);
            if !prompt.is_empty() {
                params.insert("prompt".to_string(), serde_json::Value::String(prompt));
            }
            Some(Ok(CodexSlashCommand {
                op: "side".to_string(),
                params: serde_json::Value::Object(params),
            }))
        }
        "fast" => {
            if !args.is_empty() {
                return Some(Err("/fast does not accept arguments".to_string()));
            }
            Some(Ok(CodexSlashCommand {
                op: "fast".to_string(),
                params: serde_json::json!({}),
            }))
        }
        "goal" => Some(parse_goal_slash_command(args)),
        _ => None,
    }
}

fn parse_goal_slash_command(args: &str) -> Result<CodexSlashCommand, String> {
    let exact = args.trim().to_ascii_lowercase();
    let exact_op = match exact.as_str() {
        "" | "status" | "show" | "get" => Some("goal"),
        "edit" => Some("goal-edit"),
        "clear" | "reset" => Some("goal-clear"),
        "pause" | "paused" => Some("goal-pause"),
        "resume" | "active" => Some("goal-resume"),
        "complete" | "completed" | "done" => Some("goal-complete"),
        "budget-limited" | "budget_limited" => Some("goal-budget-limited"),
        _ => None,
    };
    if let Some(op) = exact_op {
        return Ok(CodexSlashCommand {
            op: op.to_string(),
            params: serde_json::json!({}),
        });
    }

    let mut op = "goal".to_string();
    let mut params = serde_json::Map::new();
    let mut objective_parts = Vec::new();
    let mut parts = args.split_whitespace().peekable();

    while let Some(part) = parts.next() {
        match part {
            "--clear" => {
                return Ok(CodexSlashCommand {
                    op: "goal-clear".to_string(),
                    params: serde_json::json!({}),
                });
            }
            "--pause" => op = "goal-pause".to_string(),
            "--resume" => op = "goal-resume".to_string(),
            "--edit" => op = "goal-edit".to_string(),
            "--complete" => op = "goal-complete".to_string(),
            "--budget-limited" => op = "goal-budget-limited".to_string(),
            "--clear-budget" | "--no-budget" => {
                params.insert("tokenBudget".to_string(), serde_json::Value::Null);
            }
            "--status" => {
                let Some(value) = parts.next() else {
                    return Err("/goal failed: --status requires a value".to_string());
                };
                params.insert(
                    "status".to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
            "--budget" | "--token-budget" | "--tokens" => {
                let Some(value) = parts.next() else {
                    return Err("/goal failed: token budget must be a positive integer".to_string());
                };
                let budget = parse_positive_budget(value)?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--status=") => {
                let value = other.trim_start_matches("--status=");
                if value.is_empty() {
                    return Err("/goal failed: --status requires a value".to_string());
                }
                params.insert(
                    "status".to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
            other if other.starts_with("--budget=") => {
                let budget = parse_positive_budget(other.trim_start_matches("--budget="))?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--token-budget=") => {
                let budget = parse_positive_budget(other.trim_start_matches("--token-budget="))?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--tokens=") => {
                let budget = parse_positive_budget(other.trim_start_matches("--tokens="))?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other => objective_parts.push(other),
        }
    }

    let objective = unquote_slash_value(&objective_parts.join(" "));
    if !objective.is_empty() {
        let chars = objective.chars().count();
        if chars > 4000 {
            return Err("/goal failed: objective must be 4000 characters or fewer".to_string());
        }
        params.insert(
            "objective".to_string(),
            serde_json::Value::String(objective),
        );
    }

    Ok(CodexSlashCommand {
        op,
        params: serde_json::Value::Object(params),
    })
}

fn parse_positive_budget(value: &str) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err("/goal failed: token budget must be a positive integer".to_string()),
    }
}

fn unquote_slash_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return trimmed[1..trimmed.len() - 1].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn control_target_session_id(msg: &event::ControlMsg) -> Option<&str> {
    match msg {
        event::ControlMsg::Status { session_id }
        | event::ControlMsg::Approve { session_id, .. }
        | event::ControlMsg::Deny { session_id, .. }
        | event::ControlMsg::Skip { session_id, .. }
        | event::ControlMsg::ApproveAll { session_id, .. }
        | event::ControlMsg::AnswerQuestion { session_id, .. }
        | event::ControlMsg::Interrupt { session_id, .. }
        | event::ControlMsg::Steer { session_id, .. }
        | event::ControlMsg::CancelSteer { session_id, .. }
        | event::ControlMsg::StartTask { session_id, .. }
        | event::ControlMsg::EditUserMessage { session_id, .. }
        | event::ControlMsg::FollowUp { session_id, .. }
        | event::ControlMsg::CancelFollowUp { session_id, .. } => session_id.as_deref(),
        event::ControlMsg::RenameSession { session_id, .. } => Some(session_id.as_str()),
        event::ControlMsg::ConfigureSessionAgent { session_id, .. } => Some(session_id.as_str()),
        event::ControlMsg::SpawnSubAgent { session_id, .. } => Some(session_id.as_str()),
        event::ControlMsg::StopSession { session_id } => Some(session_id.as_str()),
        event::ControlMsg::ResumeSession { .. } | event::ControlMsg::RestartSession { .. } => None,
        _ => None,
    }
}

fn edit_attach_request(
    source: Option<String>,
    resume_id: Option<String>,
    project_root: Option<String>,
    direct: Option<bool>,
) -> Option<EditAttachRequest> {
    let backend = source
        .as_deref()
        .and_then(external_agent::AgentBackend::from_str_loose)?;
    if !backend.supports_user_message_rewind() {
        return None;
    }

    Some(EditAttachRequest {
        source: backend.as_short_str().to_string(),
        resume_id: resume_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty()),
        project_root: project_root
            .map(|root| root.trim().to_string())
            .filter(|root| !root.is_empty()),
        direct,
    })
}

fn edit_attach_event_matches(
    event: &AppEvent,
    primary_id: &str,
    fallback_id: Option<&str>,
) -> bool {
    let AppEvent::SessionAttached { session_id, .. } = event else {
        return false;
    };
    session_id == primary_id || fallback_id.is_some_and(|id| session_id == id)
}

fn control_msg_can_attach_unmanaged_session(msg: &event::ControlMsg) -> bool {
    match msg {
        event::ControlMsg::EditUserMessage {
            source: Some(source),
            ..
        } => external_agent::AgentBackend::from_str_loose(source)
            .is_some_and(|backend| backend.supports_user_message_rewind()),
        _ => false,
    }
}

pub(crate) fn short_session(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn emit_follow_up_status(
    bus: &EventBus,
    session_id: Option<String>,
    id: &Option<String>,
    text: Option<&str>,
    status: &str,
    reason: Option<&str>,
) {
    let Some(id) = id.as_deref().map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    bus.send(AppEvent::FollowUpStatus {
        session_id,
        id: id.to_string(),
        text: text.map(str::to_string),
        status: status.to_string(),
        reason: reason.map(str::to_string),
    });
}

fn spawn_text_steer_fallback(
    bus: EventBus,
    mut ack_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    follow_up_tx: mpsc::Sender<FollowUpMessage>,
    text: String,
    steer_id: String,
    target_session_id: Option<String>,
) {
    tokio::spawn(async move {
        let timeout = tokio::time::sleep(TEXT_STEER_FALLBACK_TIMEOUT);
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                _ = &mut timeout => break,
                event = ack_rx.recv() => {
                    match event {
                        Ok(AppEvent::SteerAccepted { session_id, id, .. })
                        | Ok(AppEvent::SteerQueued { session_id, id, .. })
                        | Ok(AppEvent::SteerDelivered { session_id, id, .. })
                        | Ok(AppEvent::SteerCancelled { session_id, id, .. })
                            if id == steer_id
                                && steer_ack_targets_session(
                                    &session_id,
                                    &target_session_id,
                                ) =>
                        {
                            return;
                        }
                        Ok(AppEvent::SteerCancelRequested { session_id, id, .. })
                            if id
                                .as_deref()
                                .map(|id| id == steer_id.as_str())
                                .unwrap_or(true)
                                && steer_ack_targets_session(
                                    &session_id,
                                    &target_session_id,
                                ) =>
                        {
                            return;
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }

        let msg = FollowUpMessage::steer(text, UserAttachments::default(), steer_id.clone())
            .for_target(target_session_id.clone());
        match follow_up_tx.send(msg).await {
            Ok(()) => bus.send(AppEvent::SteerQueued {
                session_id: target_session_id,
                id: steer_id,
                reason: "native steer was not acknowledged; queued as follow-up".to_string(),
            }),
            Err(_) => bus.send(AppEvent::LogEntry {
                session_id: target_session_id,
                level: "warn".to_string(),
                source: "Intendant".to_string(),
                content:
                    "Steer dropped: target session stopped before native steer was acknowledged"
                        .to_string(),
                turn: None,
            }),
        }
    });
}

fn steer_ack_targets_session(actual: &Option<String>, expected: &Option<String>) -> bool {
    match (actual.as_deref(), expected.as_deref()) {
        (Some(actual), Some(expected)) => actual == expected,
        (None, _) | (_, None) => true,
    }
}

fn load_related_sessions_from_log(session_dir: &Path) -> Vec<RelatedSessionRecord> {
    let path = session_dir.join("session.jsonl");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("session_relationship"))
        .filter_map(|entry| {
            let data = entry.get("data")?;
            let parent_session_id = data
                .get("parent_session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let child_session_id = data
                .get("child_session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let relationship = data
                .get("relationship")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if parent_session_id.is_empty()
                || child_session_id.is_empty()
                || parent_session_id == child_session_id
                || !matches!(relationship.as_str(), "side" | "subagent")
            {
                return None;
            }
            Some(RelatedSessionRecord {
                parent_session_id,
                child_session_id,
                relationship,
            })
        })
        .collect()
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
            logs_home_override: Some(
                std::env::temp_dir()
                    .join(format!("intendant-test-logs-home-{}", std::process::id())),
            ),
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

    /// The scripted mock provider behind a test-held gate: every chat call
    /// waits for the test to release the gate first. This pins a spawned
    /// child session provably alive — its loop cannot get a model response,
    /// so it cannot complete and retire the state under assertion — turning
    /// "assert before the child happens to finish" races into deterministic
    /// sequencing. A dropped sender (test panicked) errors the call instead
    /// of hanging the child loop forever.
    struct GatedMockProvider {
        inner: provider::mock::MockOrchestrationProvider,
        release: tokio::sync::watch::Receiver<bool>,
    }

    #[async_trait::async_trait]
    impl provider::ChatProvider for GatedMockProvider {
        async fn chat(
            &self,
            messages: &[crate::conversation::Message],
        ) -> Result<provider::ChatResponse, crate::error::CallerError> {
            let mut release = self.release.clone();
            release
                .wait_for(|released| *released)
                .await
                .map_err(|_| crate::error::CallerError::Config("provider gate dropped".into()))?;
            self.inner.chat(messages).await
        }
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn model(&self) -> &str {
            self.inner.model()
        }
        fn context_window(&self) -> u64 {
            self.inner.context_window()
        }
        fn max_output_tokens(&self) -> u64 {
            self.inner.max_output_tokens()
        }
        fn use_tools(&self) -> bool {
            self.inner.use_tools()
        }
    }

    /// [`test_supervisor_with_mock_provider`], but every spawned loop's
    /// provider blocks until the returned sender publishes `true`.
    fn test_supervisor_with_gated_mock_provider(
        project_root: PathBuf,
        bus: EventBus,
    ) -> (SessionSupervisor, tokio::sync::watch::Sender<bool>) {
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let mut config = (*test_supervisor(project_root, bus).config).clone();
        config.provider_factory = Some(Arc::new(move || {
            Box::new(GatedMockProvider {
                inner: provider::mock::MockOrchestrationProvider::new(),
                release: release_rx.clone(),
            }) as Box<dyn provider::ChatProvider>
        }));
        (SessionSupervisor::new(config), release_tx)
    }

    fn slash(text: &str) -> CodexSlashCommand {
        parse_codex_slash_command(text)
            .expect("recognized slash command")
            .expect("valid slash command")
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

    #[tokio::test]
    async fn thread_action_fallback_defers_to_advertised_ops() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);

        let drain_results = |rx: &mut tokio::sync::broadcast::Receiver<AppEvent>| {
            let mut results = Vec::new();
            while let Ok(event) = rx.try_recv() {
                if let AppEvent::CodexThreadActionResult {
                    action, message, ..
                } = event
                {
                    results.push((action, message));
                }
            }
            results
        };

        // The native presence loop advertised the goal family for its
        // session: the fallback must stay silent for those ops (the loop
        // answers; a failure here would race the real result).
        supervisor
            .observe_lifecycle_event(&AppEvent::SessionCapabilities {
                session_id: "native-1".to_string(),
                capabilities: crate::thread_actions::native_session_capabilities(),
            })
            .await;
        supervisor
            .report_unattached_codex_thread_action(
                Some("native-1".to_string()),
                "goal-set".to_string(),
            )
            .await;
        assert!(
            drain_results(&mut rx).is_empty(),
            "advertised op must not be false-rejected"
        );

        // An op the loop did NOT advertise still fails honestly for a
        // managed non-external session.
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("native-1".to_string(), managed_session("native-1", "intendant"));
        }
        supervisor
            .report_unattached_codex_thread_action(
                Some("native-1".to_string()),
                "side".to_string(),
            )
            .await;
        let results = drain_results(&mut rx);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "side");
        assert!(
            results[0].1.contains("does not advertise /side"),
            "got: {}",
            results[0].1
        );

        // Session end clears the advertisement: the goal op now reports
        // (the managed entry still resolves, so the source-shaped message
        // fires instead of silence).
        supervisor.remove_session_alias("native-1").await;
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.remove("native-1");
        }
        supervisor
            .report_unattached_codex_thread_action(
                Some("native-1".to_string()),
                "goal-set".to_string(),
            )
            .await;
        let results = drain_results(&mut rx);
        assert_eq!(results.len(), 1);
        assert!(
            results[0].1.contains("not attached"),
            "got: {}",
            results[0].1
        );
    }

    #[tokio::test]
    async fn external_identity_moves_wrapper_session_to_backend_id() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session("wrapper", "codex");
            session.phase = "thinking".to_string();
            state.sessions.insert("wrapper".to_string(), session);
            state.active_session_id = Some("wrapper".to_string());
        }

        supervisor
            .apply_session_identity(
                "wrapper".to_string(),
                "codex".to_string(),
                "backend".to_string(),
            )
            .await;

        let state = supervisor.state.lock().await;
        assert!(!state.sessions.contains_key("wrapper"));
        assert_eq!(
            state.resolve_session_id("wrapper").as_deref(),
            Some("backend")
        );
        assert_eq!(
            state.resolve_session_id("backend").as_deref(),
            Some("backend")
        );
        assert_eq!(state.active_session_id.as_deref(), Some("backend"));
        assert_eq!(
            state
                .sessions
                .get("backend")
                .map(|session| session.phase.as_str()),
            Some("thinking")
        );
    }

    #[tokio::test]
    async fn external_identity_replaces_stale_backend_entry_with_new_wrapper() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        let (old_tx, mut old_rx) = mpsc::channel(1);
        let (new_tx, mut new_rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            let mut old_session = managed_session("backend", "codex");
            old_session.name = Some("saved name".to_string());
            old_session.phase = "done".to_string();
            old_session.follow_up_tx = old_tx;
            old_session.instance_id = 1;
            state.sessions.insert("backend".to_string(), old_session);

            let mut new_session = managed_session("wrapper-new", "codex");
            new_session.phase = "idle".to_string();
            new_session.follow_up_tx = new_tx;
            new_session.instance_id = 2;
            state
                .sessions
                .insert("wrapper-new".to_string(), new_session);
            state.active_session_id = Some("wrapper-new".to_string());
        }

        supervisor
            .apply_session_identity(
                "wrapper-new".to_string(),
                "codex".to_string(),
                "backend".to_string(),
            )
            .await;

        {
            let state = supervisor.state.lock().await;
            assert!(!state.sessions.contains_key("wrapper-new"));
            assert_eq!(
                state.resolve_session_id("wrapper-new").as_deref(),
                Some("backend")
            );
            let session = state.sessions.get("backend").expect("backend session");
            assert_eq!(session.phase, "idle");
            assert_eq!(session.instance_id, 2);
            assert_eq!(session.name.as_deref(), Some("saved name"));
            assert_eq!(state.active_session_id.as_deref(), Some("backend"));
        }

        supervisor
            .route_edit_user_message(
                Some("backend".to_string()),
                None,
                None,
                None,
                Some(true),
                117,
                Some(1),
                Some("old prompt".to_string()),
                "new prompt".to_string(),
                Vec::new(),
            )
            .await;

        assert!(old_rx.try_recv().is_err());
        let msg = new_rx
            .try_recv()
            .expect("edit should route to the newly attached wrapper");
        assert_eq!(msg.text, "new prompt");
        assert_eq!(msg.edit_user_turn_index, Some(117));
        assert_eq!(msg.edit_user_turn_revision, Some(1));
    }

    #[tokio::test]
    async fn identity_rekey_drops_pre_identity_alias_without_shadowing() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        let (tx, _rx) = mpsc::channel(1);
        supervisor
            .register_session(
                "wrapper-1".to_string(),
                "codex".to_string(),
                "idle".to_string(),
                PathBuf::from("/tmp/project"),
                PathBuf::from("/tmp/session"),
                tx,
                event::ApprovalRegistry::default(),
                None,
                None,
                Some("backend-thread".to_string()),
                0,
                None,
            )
            .await;
        supervisor
            .apply_session_identity(
                "wrapper-1".to_string(),
                "codex".to_string(),
                "backend-thread".to_string(),
            )
            .await;

        let state = supervisor.state.lock().await;
        // Re-keyed entry is addressable by both ids...
        assert_eq!(
            state.resolve_session_id("backend-thread").as_deref(),
            Some("backend-thread")
        );
        assert_eq!(
            state.resolve_session_id("wrapper-1").as_deref(),
            Some("backend-thread")
        );
        // ...and no alias entry shadows the live backend key.
        assert!(!state.session_aliases.contains_key("backend-thread"));
    }

    #[tokio::test]
    async fn done_external_session_does_not_swallow_pre_attach_edit() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        let (old_tx, _old_rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session("backend", "codex");
            session.phase = "done".to_string();
            session.follow_up_tx = old_tx;
            state.sessions.insert("backend".to_string(), session);
        }

        let (target_id, entry, relation) = supervisor.lookup_edit_route_target("backend").await;

        assert_eq!(target_id, "backend");
        assert!(
            entry.is_none(),
            "terminal retained session should attach first"
        );
        assert!(relation.is_none());
    }

    #[tokio::test]
    async fn session_ended_marks_managed_session_done() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session("backend", "codex");
            session.phase = "running".to_string();
            state.sessions.insert("backend".to_string(), session);
            state
                .session_aliases
                .insert("wrapper".to_string(), "backend".to_string());
            state.active_session_id = Some("backend".to_string());
        }

        supervisor
            .observe_lifecycle_event(&AppEvent::SessionEnded {
                session_id: "wrapper".to_string(),
                reason: "Process stdout closed".to_string(),
                error_kind: None,
            })
            .await;

        let state = supervisor.state.lock().await;
        assert_eq!(
            state
                .sessions
                .get("backend")
                .map(|session| session.phase.as_str()),
            Some("done")
        );
    }

    #[tokio::test]
    async fn codex_thread_action_unattached_target_reports_failure() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);

        supervisor
            .handle_control_msg(event::ControlMsg::CodexThreadAction {
                session_id: Some("019ee2e4".to_string()),
                op: "fork".to_string(),
                params: serde_json::json!({}),
                origin: None,
            })
            .await;

        let event = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("expected failure event")
            .expect("bus event");
        match event {
            AppEvent::CodexThreadActionResult {
                session_id,
                action,
                success,
                message,
                ..
            } => {
                assert_eq!(session_id.as_deref(), Some("019ee2e4"));
                assert_eq!(action, "fork");
                assert!(!success);
                assert!(message.contains("not attached to this daemon"));
                assert!(message.contains("attach it before /fork"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn codex_thread_action_live_target_is_not_rejected_by_supervisor() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "live-codex".to_string(),
                managed_session("live-codex", "codex"),
            );
        }

        supervisor
            .handle_control_msg(event::ControlMsg::CodexThreadAction {
                session_id: Some("live-codex".to_string()),
                op: "fork".to_string(),
                params: serde_json::json!({}),
                origin: None,
            })
            .await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "live Codex action should be handled by the external-agent watcher"
        );
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

    #[tokio::test]
    async fn stop_managed_session_broadcasts_stop_and_removes_live_session() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                managed_session("parent-thread", "codex"),
            );
        }

        let stopped = supervisor
            .stop_managed_session(Some("parent-thread".to_string()), "stopped by user")
            .await
            .expect("managed session should stop");
        assert_eq!(stopped.session_id, "parent-thread");

        {
            let state = supervisor.state.lock().await;
            assert!(!state.session_is_managed("parent-thread"));
        }

        let mut saw_stop_request = false;
        let mut saw_session_ended = false;
        while let Ok(event) = bus_rx.try_recv() {
            match event {
                AppEvent::SessionStopRequested { session_id, reason }
                    if session_id.as_deref() == Some("parent-thread")
                        && reason == "stopped by user" =>
                {
                    saw_stop_request = true;
                }
                AppEvent::SessionEnded {
                    session_id, reason, ..
                } if session_id == "parent-thread" && reason == "stopped by user" =>
                {
                    saw_session_ended = true;
                }
                _ => {}
            }
        }
        assert!(saw_stop_request, "expected SessionStopRequested");
        assert!(saw_session_ended, "expected SessionEnded");
    }

    #[tokio::test]
    async fn stop_targets_native_sub_agent_child_directly() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("parent".to_string(), managed_session("parent", "intendant"));
            state
                .sessions
                .insert("child".to_string(), managed_session("child", "intendant"));
            assert!(state.apply_related_session("parent", "child", "subagent"));
        }

        let stopped = supervisor
            .stop_managed_session(Some("child".to_string()), "test stop")
            .await;
        assert_eq!(
            stopped.map(|s| s.session_id),
            Some("child".to_string()),
            "a native sub-agent child is a managed session in its own right and must be stoppable"
        );
        let state = supervisor.state.lock().await;
        assert!(!state.sessions.contains_key("child"));
        assert!(state.sessions.contains_key("parent"));
    }

    #[tokio::test]
    async fn stop_still_refuses_codex_thread_children() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("parent".to_string(), managed_session("parent", "codex"));
            // Codex subagent thread: related, but not independently managed.
            assert!(state.apply_related_session("parent", "thread-child", "subagent"));
        }
        let stopped = supervisor
            .stop_managed_session(Some("thread-child".to_string()), "test stop")
            .await;
        assert!(
            stopped.is_none(),
            "codex threads still stop via their parent"
        );
        let state = supervisor.state.lock().await;
        assert!(state.sessions.contains_key("parent"));
    }

    #[tokio::test]
    async fn sub_agent_spawn_refuses_beyond_depth_cap() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        let result = supervisor
            .start_sub_agent_session(
                "parent",
                &project,
                MAX_SUB_AGENT_DEPTH,
                SubAgentSpawnParams {
                    task: "recurse further".to_string(),
                    role: sub_agent::SubAgentRole::Custom("worker".to_string()),
                    system_prompt: None,
                    backend: None,
                    worktree: false,
                    inherit_memory: false,
                    name: None,
                },
            )
            .await;
        match result {
            Err(err) => assert!(err.contains("depth cap"), "unexpected error: {err}"),
            Ok(_) => panic!("spawn beyond the depth cap must be refused"),
        }
    }

    #[tokio::test]
    async fn running_sub_agent_children_counts_only_live_managed_children() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("parent".to_string(), managed_session("parent", "intendant"));
            state.sessions.insert(
                "live-child".to_string(),
                managed_session("live-child", "intendant"),
            );
            assert!(state.apply_related_session("parent", "live-child", "subagent"));
            // Finished child: relationship record without a live session.
            assert!(state.apply_related_session("parent", "gone-child", "subagent"));
            // A side session of the same parent does not count.
            state.sessions.insert(
                "side-child".to_string(),
                managed_session("side-child", "codex"),
            );
            assert!(state.apply_related_session("parent", "side-child", "side"));
        }
        assert_eq!(supervisor.running_sub_agent_children("parent").await, 1);
        assert_eq!(supervisor.running_sub_agent_children("other").await, 0);
    }

    #[tokio::test]
    async fn finish_session_cascades_stop_to_running_sub_agent_children() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("parent".to_string(), managed_session("parent", "intendant"));
            state
                .sessions
                .insert("child".to_string(), managed_session("child", "intendant"));
            assert!(state.apply_related_session("parent", "child", "subagent"));
        }

        let dir = tempfile::tempdir().unwrap();
        let session_log = Arc::new(std::sync::Mutex::new(
            session_log::SessionLog::open(dir.path().join("session")).unwrap(),
        ));
        supervisor
            .finish_session(
                "parent".to_string(),
                0,
                session_log,
                "task".to_string(),
                Ok(LoopStats::default()),
            )
            .await;

        {
            let state = supervisor.state.lock().await;
            assert!(!state.sessions.contains_key("parent"));
            assert!(
                !state.sessions.contains_key("child"),
                "sub-agent children die with their parent"
            );
        }
        let mut ended = Vec::new();
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::SessionEnded { session_id, .. } = event {
                ended.push(session_id);
            }
        }
        assert!(ended.contains(&"parent".to_string()));
        assert!(ended.contains(&"child".to_string()));
    }

    /// Dashboard delegation (`ControlMsg::SpawnSubAgent`), keyless: the
    /// child lands in the same children registry the parent's
    /// wait_sub_agents reads, the relationship is recorded, the parent is
    /// woken with a notification follow-up, and the completion resolves
    /// through the registry like a model-spawned child's would.
    ///
    /// The child's provider is gated: it cannot get its first model
    /// response — so it provably cannot complete and retire its state —
    /// until every spawn-time assertion has run. Without the gate the mock
    /// child completes near-instantly and `finish_session` →
    /// `remove_session` purges `related_sessions`, so state reads raced
    /// child completion and flaked under CI load.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dashboard_delegate_tracks_child_in_parent_registry_and_wakes_parent() {
        let bus = EventBus::new();
        // Subscribe before the spawn: the relationship assertion below reads
        // the bus, and broadcast subscribers only see events sent after they
        // subscribe.
        let mut bus_rx = bus.subscribe();
        let project_dir = tempfile::tempdir().unwrap();
        let (supervisor, release_gate) =
            test_supervisor_with_gated_mock_provider(project_dir.path().to_path_buf(), bus.clone());
        let (follow_up_tx, mut follow_up_rx) = mpsc::channel(4);
        let children: SubAgentChildrenMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent".to_string(),
                ManagedSession {
                    session_id: "parent".to_string(),
                    source: "intendant".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: project_dir.path().to_path_buf(),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: Some(children.clone()),
                },
            );
        }

        supervisor
            .handle_control_msg(event::ControlMsg::SpawnSubAgent {
                session_id: "parent".to_string(),
                task: "MOCK-RESEARCH: inspect the schema".to_string(),
                name: Some("delegated-researcher".to_string()),
                role: Some("research".to_string()),
                agent: Some("internal".to_string()),
                worktree: None,
            })
            .await;

        // The child is tracked in the parent's registry and linked.
        let (child_id, completion_rx) = {
            let mut map = children.lock().unwrap();
            assert_eq!(map.len(), 1, "delegated child must land in the registry");
            let (id, child) = map.iter_mut().next().unwrap();
            assert_eq!(child.name, "delegated-researcher");
            (
                id.clone(),
                child.rx.take().expect("completion receiver present"),
            )
        };
        // The relationship is recorded in supervisor state (the gate keeps
        // the child alive, so the entry cannot have been retired) …
        {
            let state = supervisor.state.lock().await;
            let relation = state
                .related_sessions
                .get(&child_id)
                .expect("relationship recorded");
            assert_eq!(relation.parent_session_id, "parent");
            assert_eq!(relation.relationship, "subagent");
        }
        // … and announced on the bus — the durable signal frontends
        // consume, emitted synchronously during the spawn.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let event = tokio::time::timeout_at(deadline, bus_rx.recv())
                .await
                .expect("SessionRelationship event should arrive")
                .expect("bus stays open");
            if let AppEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ephemeral,
            } = event
            {
                assert_eq!(parent_session_id, "parent");
                assert_eq!(child_session_id, child_id);
                assert_eq!(relationship, "subagent");
                assert!(!ephemeral);
                break;
            }
        }

        // The parent was woken with a notification naming the child.
        let notice = tokio::time::timeout(std::time::Duration::from_secs(5), follow_up_rx.recv())
            .await
            .expect("notification follow-up should arrive")
            .expect("follow-up channel open");
        assert!(
            notice.text.contains("delegated-researcher") && notice.text.contains(&child_id),
            "notice should identify the child: {}",
            notice.text
        );
        assert!(
            notice.text.contains("wait_sub_agents"),
            "notice should point the model at wait_sub_agents: {}",
            notice.text
        );

        // Spawn-time state is verified — release the child and let it run.
        release_gate.send(true).expect("child loop holds a receiver");

        // The completion resolves through the registry exactly like a
        // model-spawned child's (mock research child ends text-only; the
        // supervisor synthesizes its result from last_response).
        let completion = tokio::time::timeout(std::time::Duration::from_secs(60), completion_rx)
            .await
            .expect("delegated child should finish")
            .expect("completion delivered");
        assert_eq!(completion.name, "delegated-researcher");
        assert!(
            sub_agent::format_result_message(&completion.result).contains("research findings ABC"),
            "unexpected result: {:?}",
            completion.result
        );
    }

    #[tokio::test]
    async fn dashboard_delegate_refuses_external_parent_and_depth_cap() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("codex-parent".to_string(), managed_session("codex-parent", "codex"));
            let mut deep = managed_session("deep-parent", "intendant");
            deep.depth = MAX_SUB_AGENT_DEPTH;
            state.sessions.insert("deep-parent".to_string(), deep);
        }

        for (parent, expect) in [
            ("codex-parent", "external agent"),
            ("deep-parent", "depth cap"),
            ("missing-parent", "not managed"),
        ] {
            supervisor
                .delegate_sub_agent(
                    parent.to_string(),
                    "do something".to_string(),
                    None,
                    None,
                    None,
                    None,
                )
                .await;
            let mut seen = None;
            while let Ok(event) = bus_rx.try_recv() {
                if let AppEvent::LoopError(message) = event {
                    seen = Some(message);
                    break;
                }
            }
            let message = seen.unwrap_or_else(|| panic!("no LoopError for parent {parent}"));
            assert!(
                message.contains("Delegate failed") && message.contains(expect),
                "parent {parent}: unexpected error {message}"
            );
        }
        let state = supervisor.state.lock().await;
        assert!(
            state.related_sessions.is_empty(),
            "refused delegations must not record relationships"
        );
    }

    #[tokio::test]
    async fn finish_session_writes_terminal_outcome_to_summary() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log = Arc::new(std::sync::Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), EventBus::new());
        let mut stats = LoopStats::default();
        stats.turns = 1;
        stats.rounds = 1;
        stats.terminal_outcome = Some("stopped by user".to_string());

        supervisor
            .finish_session(
                "session-id".to_string(),
                0,
                session_log,
                "task".to_string(),
                Ok(stats),
            )
            .await;

        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(log_dir.join("summary.json")).unwrap())
                .unwrap();
        assert_eq!(summary["outcome"], "stopped by user");
    }

    #[tokio::test]
    async fn side_follow_up_routes_to_external_follow_up_event() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
            assert!(state.apply_related_session("parent-thread", "side-thread", "side"));
        }

        supervisor
            .route_follow_up(
                Some("side-thread".to_string()),
                "continue side".to_string(),
                Some(true),
                Vec::new(),
                Some("follow-1".to_string()),
            )
            .await;

        assert!(rx.try_recv().is_err());
        match bus_rx.recv().await.expect("side follow-up event") {
            AppEvent::ExternalFollowUpRequested {
                session_id,
                text,
                attachments,
                follow_up_id,
            } => {
                assert_eq!(session_id, "side-thread");
                assert_eq!(text, "continue side");
                assert!(attachments.is_empty());
                assert_eq!(follow_up_id.as_deref(), Some("follow-1"));
            }
            other => panic!("expected external follow-up request, got {other:?}"),
        }

        let state = supervisor.state.lock().await;
        assert!(state.session_is_managed("parent-thread"));
        assert!(state.session_is_managed("side-thread"));
    }

    #[tokio::test]
    async fn side_edit_preserves_child_target_on_parent_channel() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
            assert!(state.apply_related_session("parent-thread", "side-thread", "side"));
        }

        supervisor
            .route_edit_user_message(
                Some("side-thread".to_string()),
                None,
                None,
                None,
                Some(true),
                1,
                Some(1),
                None,
                "replacement side prompt".to_string(),
                Vec::new(),
            )
            .await;

        let msg = rx
            .try_recv()
            .expect("side edit should queue on parent runner");
        assert_eq!(msg.text, "replacement side prompt");
        assert_eq!(msg.edit_user_turn_index, Some(1));
        assert_eq!(msg.edit_user_turn_revision, Some(1));
        assert_eq!(msg.target_session_id.as_deref(), Some("side-thread"));
    }

    #[tokio::test]
    async fn edit_queued_before_attach_delivers_after_session_identity() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);

        supervisor.queue_edit_user_message_after_attach(
            "codex-thread".to_string(),
            EditUserMessageRequest {
                requested_id: "codex-thread".to_string(),
                user_turn_index: 2,
                user_turn_revision: Some(5),
                original_text: Some("continue".to_string()),
                text: "edited continue".to_string(),
                attachments: Vec::new(),
            },
        );

        tokio::time::sleep(EDIT_ATTACH_ROUTE_POLL_INTERVAL * 2).await;
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "wrapper-session".to_string(),
                ManagedSession {
                    session_id: "wrapper-session".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
            state
                .session_aliases
                .insert("codex-thread".to_string(), "wrapper-session".to_string());
        }
        bus.send(AppEvent::SessionAttached {
            session_id: "codex-thread".to_string(),
            source: "codex".to_string(),
        });

        let msg = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
            .await
            .expect("queued edit should be delivered after alias registration")
            .expect("follow-up channel should stay open");
        assert_eq!(msg.text, "edited continue");
        assert_eq!(msg.edit_user_turn_index, Some(2));
        assert_eq!(msg.edit_user_turn_revision, Some(5));
        assert_eq!(msg.edit_original_text.as_deref(), Some("continue"));
        assert_eq!(msg.target_session_id, None);
    }

    #[tokio::test]
    async fn edit_queued_after_attach_polls_for_routable_session() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);

        supervisor.queue_edit_user_message_after_attach(
            "codex-thread".to_string(),
            EditUserMessageRequest {
                requested_id: "codex-thread".to_string(),
                user_turn_index: 2,
                user_turn_revision: Some(5),
                original_text: Some("continue".to_string()),
                text: "edited continue".to_string(),
                attachments: Vec::new(),
            },
        );

        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "codex-thread".to_string(),
                ManagedSession {
                    session_id: "codex-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
        }

        let msg = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
            .await
            .expect("queued edit should deliver once the live route exists")
            .expect("follow-up channel should stay open");
        assert_eq!(msg.text, "edited continue");
        assert_eq!(msg.edit_user_turn_index, Some(2));
        assert_eq!(msg.edit_user_turn_revision, Some(5));
        assert_eq!(msg.edit_original_text.as_deref(), Some("continue"));
    }

    #[tokio::test]
    async fn edit_route_uses_wrapper_index_before_alias_event() {
        let home = tempfile::tempdir().unwrap();
        let backend_id = "019ea99e-af1d-7c23-a57a-55a89c77f90b";
        let wrapper_id = "6036429e-54f9-4f93-b74d-04c060c79054";
        let wrapper_dir = home.path().join(".intendant").join("logs").join(wrapper_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            backend_id,
            wrapper_id,
            &wrapper_dir,
            None,
        )
        .unwrap();

        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert(wrapper_id.to_string(), managed_session(wrapper_id, "codex"));
        }

        let (target_id, entry, relation) = supervisor
            .lookup_edit_route_target_in_home(backend_id, home.path())
            .await;

        assert_eq!(target_id, wrapper_id);
        assert_eq!(
            entry.map(|target| target.managed_id).as_deref(),
            Some(wrapper_id)
        );
        assert!(relation.is_none());
    }

    #[tokio::test]
    async fn text_steer_falls_back_to_follow_up_without_native_ack() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "thinking".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
        }

        supervisor
            .route_steer(
                Some("parent-thread".to_string()),
                "Pause for a moment".to_string(),
                Some("steer-1".to_string()),
                Vec::new(),
            )
            .await;

        match bus_rx.recv().await.expect("steer requested event") {
            AppEvent::SteerRequested {
                session_id,
                text,
                id,
            } => {
                assert_eq!(session_id.as_deref(), Some("parent-thread"));
                assert_eq!(text, "Pause for a moment");
                assert_eq!(id, "steer-1");
            }
            other => panic!("expected steer requested event, got {other:?}"),
        }

        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("unacknowledged steer should be queued")
            .expect("follow-up channel should stay open");
        assert_eq!(msg.text, "Pause for a moment");
        assert_eq!(msg.steer_id.as_deref(), Some("steer-1"));
        assert_eq!(msg.target_session_id.as_deref(), Some("parent-thread"));

        let mut saw_queued = false;
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::SteerQueued {
                session_id,
                id,
                reason,
            } = event
            {
                assert_eq!(session_id.as_deref(), Some("parent-thread"));
                assert_eq!(id, "steer-1");
                assert!(reason.contains("not acknowledged"), "got: {reason}");
                saw_queued = true;
            }
        }
        assert!(saw_queued, "fallback should emit SteerQueued");
    }

    #[tokio::test]
    async fn text_steer_native_ack_prevents_follow_up_fallback() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "thinking".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
        }

        supervisor
            .route_steer(
                Some("parent-thread".to_string()),
                "pause for a moment".to_string(),
                Some("steer-2".to_string()),
                Vec::new(),
            )
            .await;

        match bus_rx.recv().await.expect("steer requested event") {
            AppEvent::SteerRequested { id, .. } => assert_eq!(id, "steer-2"),
            other => panic!("expected steer requested event, got {other:?}"),
        }
        bus.send(AppEvent::SteerAccepted {
            session_id: Some("parent-thread".to_string()),
            id: "steer-2".to_string(),
            reason: "Codex accepted the steer".to_string(),
        });

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            rx.try_recv().is_err(),
            "acknowledged steer should not also queue a follow-up"
        );

        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::SteerQueued { id, .. } = event {
                assert_ne!(id, "steer-2", "acknowledged steer should not queue");
            }
        }
    }

    #[test]
    fn loads_related_sessions_from_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.session_relationship("parent", "sub-child", "subagent", false);
        log.session_relationship("parent", "side-child", "side", true);
        log.session_relationship("parent", "fork-child", "fork", false);
        drop(log);

        let related = load_related_sessions_from_log(&log_dir);
        assert_eq!(
            related,
            vec![
                RelatedSessionRecord {
                    parent_session_id: "parent".to_string(),
                    child_session_id: "sub-child".to_string(),
                    relationship: "subagent".to_string(),
                },
                RelatedSessionRecord {
                    parent_session_id: "parent".to_string(),
                    child_session_id: "side-child".to_string(),
                    relationship: "side".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parses_fork_slash_command_with_name() {
        let command = slash("/fork dashboard branch");
        assert_eq!(command.op, "fork");
        assert_eq!(command.params["name"], "dashboard branch");
    }

    #[test]
    fn parses_side_slash_command_with_prompt() {
        let command = slash("/side why is this failing?");
        assert_eq!(command.op, "side");
        assert_eq!(command.params["prompt"], "why is this failing?");
    }

    #[test]
    fn parses_btw_alias_as_side_slash_command() {
        let command = slash("/btw \"quick context check\"");
        assert_eq!(command.op, "side");
        assert_eq!(command.params["prompt"], "quick context check");
    }

    #[test]
    fn parses_goal_slash_command_with_objective_and_budget() {
        let command = slash("/goal Ship multi-session UX --budget 200000");
        assert_eq!(command.op, "goal");
        assert_eq!(command.params["objective"], "Ship multi-session UX");
        assert_eq!(command.params["tokenBudget"], 200000);
    }

    #[test]
    fn parses_goal_status_aliases() {
        assert_eq!(slash("/goal clear").op, "goal-clear");
        assert_eq!(slash("/goal edit").op, "goal-edit");
        assert_eq!(slash("/goal pause").op, "goal-pause");
        assert_eq!(slash("/goal resume").op, "goal-resume");
        assert_eq!(slash("/goal done").op, "goal-complete");
    }

    #[test]
    fn parses_fast_slash_command() {
        let command = slash("/fast");
        assert_eq!(command.op, "fast");
        assert_eq!(command.params, serde_json::json!({}));

        let err = parse_codex_slash_command("/fast now")
            .expect("recognized slash command")
            .unwrap_err();
        assert!(err.contains("does not accept arguments"), "got: {err}");
    }

    #[test]
    fn fast_new_session_forces_or_accepts_codex_agent() {
        assert_eq!(
            codex_fast_new_session_agent(None).unwrap(),
            "codex".to_string()
        );
        assert_eq!(
            codex_fast_new_session_agent(Some("configured")).unwrap(),
            "codex".to_string()
        );
        assert_eq!(
            codex_fast_new_session_agent(Some("codex")).unwrap(),
            "codex".to_string()
        );

        let err = codex_fast_new_session_agent(Some("claude-code")).unwrap_err();
        assert!(err.contains("Codex"), "got: {err}");
        let err = codex_fast_new_session_agent(Some("internal")).unwrap_err();
        assert!(err.contains("Codex"), "got: {err}");
        // Retired backend: "gemini" fails as an unknown agent, not a
        // non-Codex selection.
        let err = codex_fast_new_session_agent(Some("gemini")).unwrap_err();
        assert!(err.contains("unknown agent"), "got: {err}");
    }

    #[test]
    fn ignores_non_codex_slash_commands() {
        assert!(parse_codex_slash_command("/help").is_none());
    }

    #[test]
    fn edit_attach_request_accepts_only_rewind_capable_external_sources() {
        let attach = edit_attach_request(
            Some("Codex".to_string()),
            Some(" 019e5c7a ".to_string()),
            Some(" /tmp/project ".to_string()),
            None,
        )
        .expect("codex edit should be attachable");
        assert_eq!(attach.source, "codex");
        assert_eq!(attach.resume_id.as_deref(), Some("019e5c7a"));
        assert_eq!(attach.project_root.as_deref(), Some("/tmp/project"));

        assert!(edit_attach_request(
            Some("gemini".to_string()),
            Some("gemini-session".to_string()),
            None,
            None,
        )
        .is_none());
        assert!(edit_attach_request(None, None, None, None).is_none());
    }

    #[test]
    fn external_codex_edit_control_can_be_handled_before_attach() {
        let msg = event::ControlMsg::EditUserMessage {
            session_id: Some("019e5c7a".to_string()),
            source: Some("codex".to_string()),
            resume_id: Some("019e5c7a".to_string()),
            project_root: Some("/tmp/project".to_string()),
            direct: Some(true),
            user_turn_index: 1,
            user_turn_revision: Some(1),
            original_text: None,
            text: "replacement".to_string(),
            attachments: Vec::new(),
        };
        assert!(control_msg_can_attach_unmanaged_session(&msg));
    }

    #[test]
    fn parses_session_agent_selection() {
        assert_eq!(
            SessionAgentSelection::from_wire(None).unwrap(),
            SessionAgentSelection::Configured
        );
        assert_eq!(
            SessionAgentSelection::from_wire(Some("internal")).unwrap(),
            SessionAgentSelection::Internal
        );
        // Retired backend: "gemini" must no longer resolve to a live backend.
        assert!(SessionAgentSelection::from_wire(Some("gemini")).is_err());
        assert!(SessionAgentSelection::from_wire(Some("unknown")).is_err());
    }

    #[test]
    fn applies_session_agent_command_to_selected_backend() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        apply_session_agent_command(
            &mut project,
            &external_agent::AgentBackend::ClaudeCode,
            "/opt/claude/bin/claude".to_string(),
        );
        assert_eq!(
            project.config.agent.claude_code.command,
            "/opt/claude/bin/claude"
        );
    }

    /// The create/resume wire normalizers must treat the `inherit` sentinel
    /// (and empty strings) as "no per-session override" — not pin the
    /// project-level default. Explicit values still pin, and absent stays
    /// absent. This is what lets a launch-config save change only the
    /// binary path without permanently pinning vanilla into the session.
    #[test]
    fn session_codex_managed_context_inherit_means_no_override() {
        for sentinel in [
            Some("inherit"),
            Some("default"),
            Some("global"),
            Some(""),
            None,
        ] {
            assert_eq!(
                normalize_session_codex_managed_context(sentinel),
                None,
                "{sentinel:?} should not produce a managed-context override"
            );
            assert_eq!(
                normalize_session_codex_context_archive(sentinel),
                None,
                "{sentinel:?} should not produce a context-archive override"
            );
        }
        assert_eq!(
            normalize_session_codex_managed_context(Some("managed")).as_deref(),
            Some("managed")
        );
        assert_eq!(
            normalize_session_codex_managed_context(Some("vanilla")).as_deref(),
            Some("vanilla")
        );
        assert_eq!(
            normalize_session_codex_context_archive(Some("exact")).as_deref(),
            Some("exact")
        );
        // The configure_session_agent clear flags use the same sentinel set.
        assert!(session_config_clear_value(Some("inherit")));
        assert!(session_config_clear_value(Some("")));
        assert!(!session_config_clear_value(Some("managed")));
        assert!(!session_config_clear_value(Some("vanilla")));
        assert!(!session_config_clear_value(None));
        // The Claude permission-mode variant keeps "default" pinnable.
        assert!(session_config_clear_value_keeping_default(Some("inherit")));
        assert!(session_config_clear_value_keeping_default(Some("global")));
        assert!(session_config_clear_value_keeping_default(Some("")));
        assert!(!session_config_clear_value_keeping_default(Some("default")));
        assert!(!session_config_clear_value_keeping_default(Some(
            "acceptEdits"
        )));
        assert!(!session_config_clear_value_keeping_default(None));
    }

    #[test]
    fn launch_overrides_map_to_wire_fields_and_gate_by_source() {
        let overrides = LaunchOverrides {
            agent_command: Some("/tmp/claude".to_string()),
            claude_model: Some("sonnet".to_string()),
            claude_permission_mode: Some("plan".to_string()),
            claude_allowed_tools: Some("Read, Bash(cargo test *)".to_string()),
            claude_effort: Some("high".to_string()),
            ..Default::default()
        };
        // The claude configure path: fields normalize into pins.
        let config = crate::session_config::from_wire_fields(
            overrides.as_wire_fields("claude-code"),
        );
        assert_eq!(config.agent_command.as_deref(), Some("/tmp/claude"));
        assert_eq!(config.claude_model.as_deref(), Some("sonnet"));
        assert_eq!(config.claude_permission_mode.as_deref(), Some("plan"));
        assert_eq!(
            config.claude_allowed_tools.as_deref(),
            Some(&["Read".to_string(), "Bash(cargo test *)".into()][..])
        );
        assert_eq!(config.claude_effort.as_deref(), Some("high"));
        // The same overrides against a codex session never leak claude pins.
        let cross = crate::session_config::from_wire_fields(
            overrides.as_wire_fields("codex"),
        );
        assert!(cross.claude_model.is_none());
        assert!(cross.claude_permission_mode.is_none());
        assert!(cross.claude_allowed_tools.is_none());
        assert!(cross.claude_effort.is_none());
    }

    #[test]
    fn applies_session_codex_managed_context_to_codex_only() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        apply_session_codex_managed_context(
            &mut project,
            &external_agent::AgentBackend::Codex,
            "on".to_string(),
        )
        .unwrap();
        assert_eq!(project.config.agent.codex.managed_context, "managed");

        let err = apply_session_codex_managed_context(
            &mut project,
            &external_agent::AgentBackend::ClaudeCode,
            "managed".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("requires Codex"));
    }

    #[test]
    fn applies_session_codex_context_archive_to_codex_only() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        apply_session_codex_context_archive(
            &mut project,
            &external_agent::AgentBackend::Codex,
            "raw".to_string(),
        )
        .unwrap();
        assert_eq!(project.config.agent.codex.context_archive, "exact");

        let err = apply_session_codex_context_archive(
            &mut project,
            &external_agent::AgentBackend::ClaudeCode,
            "summary".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("requires Codex"));
    }

    #[test]
    fn normalizes_optional_session_name() {
        assert_eq!(
            normalize_session_name_option(Some("  Dashboard   work  ")).unwrap(),
            Some("Dashboard work".to_string())
        );
        assert_eq!(normalize_session_name_option(Some("   ")).unwrap(), None);
        assert_eq!(normalize_session_name_option(None).unwrap(), None);
    }
}

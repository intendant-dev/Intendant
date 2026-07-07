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
mod sub_agents;
mod routing;
pub(crate) use routing::*;
mod agent_config;
pub(crate) use agent_config::*;

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

}

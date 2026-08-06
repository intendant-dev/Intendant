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

mod capacity_gate;
mod exec;
mod launch;
pub(crate) use launch::*;
mod routing;
mod sub_agents;
pub(crate) use routing::*;
mod agent_config;
pub(crate) use agent_config::*;
mod claude_edit;
pub(crate) use claude_edit::{CLAUDE_EDIT_INPLACE_STOP_REASON, CLAUDE_EDIT_SUPERSEDED_STOP_REASON};
mod dispatch;
mod fork;
mod registry;
pub(crate) mod resume_lineage;

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
    pub shared_kimi_config: control_plane::SharedKimiConfig,
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
    /// Test seam for the off-intake launch executor: when set, the slow
    /// launch bodies (session create / resume) await this gate flipping
    /// true before doing any work — a deterministic stand-in for a
    /// multi-second worktree checkout, so tests can hold a launch provably
    /// in-flight while asserting what the intake still serves. None in
    /// production (zero cost).
    pub launch_gate_for_tests: Option<tokio::sync::watch::Receiver<bool>>,
    /// Test seam for the claude edit ladder's CLI capability probe: when
    /// set, the ladder uses this verdict instead of scanning the
    /// project-configured executable — the real probe reads a
    /// quarter-gigabyte binary off the machine, which hermetic tests
    /// must never touch. None in production.
    pub claude_rewind_capability_for_tests:
        Option<external_agent::claude_code::ClaudeRewindWireCapability>,
    /// The daemon's agenda authority, when this process runs one (the
    /// gateway shapes wire it through). The ask-delivery arm records
    /// whether a recorded answer reached a live asking session
    /// (`record_ask_delivery` — the "answered · awaiting pickup" marker).
    /// `None` (hermetic tests, shapes without an agenda) skips the marker
    /// write-back, never the delivery itself.
    pub agenda: Option<Arc<crate::agenda::AgendaHandle>>,
    /// The daemon-handover runtime (Track HS3): the drain refusal gate
    /// (`ControlMsg::creates_session` intents refuse while draining) and
    /// the exit-at-last-session condition read it. `None` (hermetic
    /// tests, shapes without a gateway) disables both — today's
    /// semantics.
    pub handover: Option<Arc<crate::handover::HandoverRuntime>>,
    /// The capacity controller (memory-headroom stages + the resident
    /// bound): the admission gate, the deferred-admission queue, the park
    /// census, and the capacity monitor all read it. `None` (hermetic
    /// tests, `[capacity] enabled = false`) disables all of them —
    /// pre-slice behavior, the fail-open shape.
    pub capacity: Option<Arc<crate::capacity::CapacityController>>,
}

#[derive(Clone)]
pub struct SessionSupervisor {
    config: Arc<SessionSupervisorConfig>,
    state: Arc<AsyncMutex<SupervisorState>>,
    /// Off-intake executor: per-session ordered queues for slow launch
    /// bodies and the commands deferred behind them (see exec.rs and
    /// `dispatch_control_msg`).
    exec: Arc<exec::IntakeExecutor>,
    /// Drain wait-set memo (see [`DrainWaitMemo`]): the exit check runs
    /// on every bus event while draining, and the named holdout rows it
    /// reports read durable meta — this gates the rebuild to real
    /// changes plus a slow refresh beat.
    drain_wait_memo: Arc<std::sync::Mutex<DrainWaitMemo>>,
}

/// Last reported drain wait set: its (session id, phase) fingerprint and
/// when the rows were last rebuilt. Limit-park marker edges ride phase
/// edges (parking and releasing both emit a status update), so the
/// fingerprint catches every ordinary change; [`DRAIN_WAIT_REFRESH`]
/// catches a marker whose durable write landed just after its phase
/// event was observed.
#[derive(Default)]
struct DrainWaitMemo {
    fingerprint: Vec<(String, String)>,
    reported_at: Option<std::time::Instant>,
}

/// Refresh beat for the drain wait-set rows when nothing else changed.
const DRAIN_WAIT_REFRESH: std::time::Duration = std::time::Duration::from_secs(5);

/// The foreground web shape needs session supervision before its primary
/// task ends (for parallel create/resume/fork requests), but must leave
/// untargeted primary-session controls to the foreground dispatcher. Once
/// both ordered intent streams reach the primary session's `SessionEnded`
/// marker, the dispatcher exits and this same receiver promotes in place to
/// the daemon's full supervisor. Reusing one lossless intent subscription
/// avoids both a handoff gap and duplicate launches.
pub(crate) struct ForegroundSupervisorHandle {
    handle: JoinHandle<()>,
}

impl ForegroundSupervisorHandle {
    pub(crate) async fn wait(self) {
        let _ = self.handle.await;
    }
}

const EXTERNAL_ATTACH_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const SESSION_STOP_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
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
    /// Sessions whose background-task park DIED with a backend restart
    /// (`SessionActivity` publishes with non-empty died fields) — the
    /// honest attention state. An idle member no longer holds the drain:
    /// its wait is on nobody (the tasks are dead, re-running is an owner
    /// decision the successor daemon serves just as well), so counting it
    /// as held work would strand the drain behind a wait that can never
    /// end. Membership clears when the session demonstrably works again
    /// (any activity publish without died fields).
    died_park_sessions: std::collections::HashSet<String>,
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
    /// Deferred admissions (capacity gate): create intents held FIFO with
    /// their minted reservations until headroom returns. See
    /// `capacity_gate.rs`.
    capacity_queue: std::collections::VecDeque<capacity_gate::QueuedAdmission>,
    /// Sessions carrying the honest capacity-park mark (park stage's
    /// longest-idle census). A mark blocks nothing user-initiated; it
    /// clears on activity, on session end, or when pressure eases.
    capacity_parked: std::collections::HashSet<String>,
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
    /// Optional caller-selected name for the child session.
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
    /// Credential-reload lifecycle, `None` until a reload is first
    /// requested; overwritten whole on each `requested` stamp (latest
    /// request wins) and by each loop progress event.
    reload: Option<ReloadLifecycle>,
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

/// One row of the Vault sign-in cards' "live sessions to reload" list: a
/// LIVE registry entry (`SupervisorState::sessions`) of the provider's
/// backend — the exact candidate set
/// [`SessionSupervisor::route_reload_credentials`] accepts. The list is
/// served from the live registry, never the disk catalog: registry
/// membership is liveness (the Stop-button doctrine at the source), so a
/// parked or rate-limit-parked session stays listed and a session gone
/// from the registry never does. Phase rides along as a chip, not a
/// filter.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ReloadCandidate {
    pub(crate) session_id: String,
    pub(crate) source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    pub(crate) phase: String,
    /// The daemon-owned reload lifecycle for this row, when a reload was
    /// ever requested this registry lifetime — the ONLY reload state the
    /// Vault card renders (reload_lifecycle_is_daemon_owned_and_served).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reload: Option<ReloadLifecycle>,
}

/// Daemon-owned per-session credential-reload lifecycle: stamped
/// `requested` by [`SessionSupervisor::route_reload_credentials`] (and
/// the reload-all fan-out), then advanced by the loop's typed
/// [`event::CredentialReloadProgress`] events to `respawning` →
/// `done`/`failed`, and served verbatim on [`ReloadCandidate::reload`].
/// It lives on the registry row and dies with the session, so terminal
/// states linger only as history beside an always-available Reload
/// button — never as a gate on re-requesting
/// (terminal_states_always_restore_the_button).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ReloadLifecycle {
    pub(crate) state: ReloadLifecycleState,
    pub(crate) at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

impl ReloadLifecycle {
    pub(crate) fn stamped_now(state: ReloadLifecycleState, error: Option<String>) -> Self {
        let at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            state,
            at_unix_ms,
            error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReloadLifecycleState {
    Requested,
    Respawning,
    Done,
    Failed,
}

/// Read-side view of the live managed-session registry for lanes outside
/// the supervisor (the sign-in ceremony status payloads). Holds a `Weak`
/// so the read side never extends the state's lifetime: a handle whose
/// supervisor is gone truthfully reports no live sessions.
#[derive(Clone)]
pub(crate) struct LiveSessionRegistry {
    state: std::sync::Weak<AsyncMutex<SupervisorState>>,
}

impl LiveSessionRegistry {
    /// Live sessions whose backend is `source`
    /// (`AgentBackend::as_short_str` vocabulary), sorted by session id for
    /// stable rendering. Every registry entry of the source is a candidate
    /// — parked, rate-limit-parked, and mid-turn alike — with no row cap:
    /// the disk catalog's truncation and status heuristics never apply
    /// here.
    pub(crate) async fn reload_candidates_for_source(&self, source: &str) -> Vec<ReloadCandidate> {
        let Some(state) = self.state.upgrade() else {
            return Vec::new();
        };
        let state = state.lock().await;
        let mut rows: Vec<ReloadCandidate> = state
            .sessions
            .values()
            .filter(|session| session.source == source)
            .map(|session| ReloadCandidate {
                session_id: session.session_id.clone(),
                source: session.source.clone(),
                name: session.name.clone(),
                phase: session.phase.clone(),
                reload: session.reload.clone(),
            })
            .collect();
        rows.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        rows
    }

    /// Wrapper-liveness snapshot for the session catalog's boot-era
    /// join: session ids whose registry entry still holds an open
    /// follow-up channel — the `live_external_wrapper_for` doctrine
    /// (`launch.rs`): membership plus the open channel is "this daemon
    /// still drives it", never phase. Non-blocking because the catalog
    /// build runs on blocking-pool threads and inside async handlers
    /// alike: lock contention yields `None` and the caller omits the
    /// join rather than serve a wrong liveness bit; a gone supervisor
    /// truthfully reports no live sessions.
    ///
    /// Alias-closed: `apply_session_identity` re-keys an external entry
    /// to its backend id once the backend announces, leaving the wrapper
    /// log-dir id behind as an alias — but catalog rows are keyed by the
    /// log-dir id, so an entry-keys-only set read every post-identity
    /// live session as dead and its row served `ghost:true` (the
    /// readopt-successor false-ghost class, five specimens 2026-07-29).
    /// Every alias that resolves to a live entry is therefore a live id
    /// too; dangling aliases resolve to nothing and drop out on their
    /// own.
    pub(crate) fn live_wrapper_ids(&self) -> Option<std::collections::HashSet<String>> {
        let Some(state) = self.state.upgrade() else {
            return Some(std::collections::HashSet::new());
        };
        let guard = state.try_lock().ok()?;
        let mut live: std::collections::HashSet<String> = guard
            .sessions
            .values()
            .filter(|session| !session.follow_up_tx.is_closed())
            .map(|session| session.session_id.clone())
            .collect();
        for alias in guard.session_aliases.keys() {
            if live.contains(alias) {
                continue;
            }
            let alias_is_live = guard.resolve_session_id(alias).is_some_and(|key| {
                guard
                    .sessions
                    .get(&key)
                    .is_some_and(|session| !session.follow_up_tx.is_closed())
            });
            if alias_is_live {
                live.insert(alias.clone());
            }
        }
        Some(live)
    }
}

static PUBLISHED_LIVE_SESSION_REGISTRY: std::sync::OnceLock<LiveSessionRegistry> =
    std::sync::OnceLock::new();

/// Publish the daemon's live-session registry for read-side lanes,
/// mirroring `session_vitals::publish_git_vitals_targets`: called from
/// the startup paths that construct a supervisor, never from tests
/// (which read their own instance's [`SessionSupervisor::live_session_registry`]
/// directly).
pub(crate) fn publish_live_session_registry(registry: LiveSessionRegistry) {
    let _ = PUBLISHED_LIVE_SESSION_REGISTRY.set(registry);
}

/// The published live registry, when a startup path wired one.
pub(crate) fn published_live_session_registry() -> Option<&'static LiveSessionRegistry> {
    PUBLISHED_LIVE_SESSION_REGISTRY.get()
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
        self.died_park_sessions.remove(&canonical);
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
            exec: exec::IntakeExecutor::new(),
            drain_wait_memo: Arc::new(std::sync::Mutex::new(DrainWaitMemo::default())),
        }
    }

    /// Read-side handle over this supervisor's live registry (see
    /// [`LiveSessionRegistry`]); startup publishes it via
    /// [`publish_live_session_registry`].
    pub(crate) fn live_session_registry(&self) -> LiveSessionRegistry {
        LiveSessionRegistry {
            state: Arc::downgrade(&self.state),
        }
    }

    /// Test seam (`SessionSupervisorConfig::launch_gate_for_tests`): hold
    /// a slow launch body deterministically in-flight. No-op in
    /// production.
    async fn wait_for_launch_gate_in_tests(&self) {
        if let Some(gate) = self.config.launch_gate_for_tests.as_ref() {
            let mut gate = gate.clone();
            let _ = gate.wait_for(|open| *open).await;
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
    /// primary supervisor and the foreground listener share exactly one
    /// action path; `filter_session_control` is the foreground listener's
    /// `should_handle_session_control` gate.
    async fn handle_intent_lane_event(&self, event: AppEvent, filter_session_control: bool) {
        match event {
            AppEvent::ControlCommand(msg) => {
                if !filter_session_control || self.should_handle_session_control(&msg).await {
                    self.dispatch_control_msg(msg).await;
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
            // A recorded outcome on an agenda-backed ask: deliver it into
            // the still-live asking session as ordinary follow-up input
            // (queued mid-turn, injected at the boundary). Rides the
            // lossless lane — losing one silently drops the owner's reply.
            AppEvent::AgendaAskOutcome {
                item,
                action,
                inline_waiter,
            } => {
                self.deliver_agenda_ask_outcome(item, &action, inline_waiter)
                    .await;
            }
            _ => {}
        }
    }

    /// The supervisor's receive loop, shared by [`Self::spawn`] and
    /// [`Self::spawn_foreground_listener`].
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
    /// The lane is drained strictly in order, but a command's BODY no
    /// longer necessarily runs inline: `dispatch_control_msg` does the
    /// fast, ordering-critical work here (validate, reserve/mint identity,
    /// dedup) and hands slow launch bodies — session create with a
    /// worktree checkout, resume, restart, fork, delegation spawns — to
    /// the per-session ordered executor (exec.rs), so one session's
    /// multi-second checkout no longer head-of-line-blocks every other
    /// session's approvals/steers/interrupts. Commands for one session
    /// still execute in arrival order (a busy session's commands defer
    /// onto its queue); the identity/relationship/end bookkeeping stays
    /// inline, so routing state is always applied in lane order.
    ///
    /// Receivers are subscribed by the caller BEFORE the task is spawned:
    /// daemon startup sends `ResumeSession` immediately after `spawn()`
    /// returns and relies on the subscription already existing.
    async fn run_event_loop(
        self,
        mut intent_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        mut rx: tokio::sync::broadcast::Receiver<AppEvent>,
        foreground_primary_session_id: Option<String>,
    ) {
        let mut full_session_control = foreground_primary_session_id.is_none();
        loop {
            tokio::select! {
                biased;
                intent = intent_rx.recv() => match intent {
                    Some(event) => {
                        let promote_after_event = foreground_primary_session_id
                            .as_ref()
                            .is_some_and(|primary| {
                                matches!(
                                    &event,
                                    AppEvent::SessionEnded { session_id, .. }
                                        if session_id == primary
                                )
                            });
                        self.handle_intent_lane_event(event, !full_session_control)
                            .await;
                        if promote_after_event {
                            full_session_control = true;
                        }
                    }
                    None => break,
                },
                event = rx.recv() => match event {
                    Ok(event) => self.observe_lifecycle_event(&event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
            // Track HS3 (§3.2 step 6): a DRAINING daemon whose last
            // supervised session ended exits gracefully — this loop is
            // the daemon's lifetime (`run_daemon` awaits it). Checked on
            // every event; one atomic read when not draining. The settle
            // beat lets concurrently-subscribed tasks (the scheduler's
            // terminal write-backs for the very session that just ended)
            // drain their queued events before the process goes away —
            // a bounded courtesy, not a synchronization (v1; a handshake
            // is a named follow-up if the window ever bites).
            if self.drain_exit_ready().await {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if self.drain_exit_ready().await {
                    break;
                }
            }
        }
    }

    /// The drain exit condition: draining, no supervised session still
    /// HOLDING WORK, and no launch body executing (a create accepted
    /// before drain-entry registers its session moments later — exiting
    /// under it would orphan the launch). "Holding work" is any phase but
    /// `done`: draining exists to protect in-flight work, and a session
    /// parked after its DoneSignal has none — scheduled sessions park
    /// after done as a matter of course, and a strict count would strand
    /// every drain behind them forever. An `idle` session still holds
    /// (it may be pre-first-turn, or mid-conversation awaiting the
    /// owner — the intake's "steer or stop stragglers" lane); parked
    /// conversations stay resumable on the successor. A LIMIT-PARKED
    /// wrapper also holds — for as long as its in-memory park runs
    /// (potentially hours) — which is exactly why the wait set is
    /// NAMED: the holdout rows (with each session's durable
    /// `limit_park` marker) reach the status surface and the presence
    /// record via [`HandoverRuntime::set_drain_wait_set`], so every
    /// drain surface can say WHO holds and until WHEN instead of
    /// rendering a silent banner. Records the terminal `exited`
    /// presence state when satisfied.
    async fn drain_exit_ready(&self) -> bool {
        let Some(runtime) = self.config.handover.as_ref() else {
            return false;
        };
        if !runtime.is_draining() {
            return false;
        }
        let holding: Vec<(String, String, Option<String>, String, PathBuf)> = {
            let state = self.state.lock().await;
            state
                .sessions
                .values()
                .filter(|session| {
                    session_holds_drain(
                        &session.phase,
                        state.died_park_sessions.contains(&session.session_id),
                    )
                })
                .map(|session| {
                    (
                        session.session_id.clone(),
                        session.source.clone(),
                        session.name.clone(),
                        session.phase.clone(),
                        session.session_dir.clone(),
                    )
                })
                .collect()
        };
        // Rebuild + report the named rows only when the set moved or the
        // refresh beat elapsed: this check runs per bus event, and the
        // marker resolution reads each holding session's durable meta.
        let rebuild = {
            let mut memo = match self.drain_wait_memo.lock() {
                Ok(memo) => memo,
                Err(poisoned) => poisoned.into_inner(),
            };
            let mut fingerprint: Vec<(String, String)> = holding
                .iter()
                .map(|(id, _, _, phase, _)| (id.clone(), phase.clone()))
                .collect();
            fingerprint.sort();
            if memo.fingerprint != fingerprint
                || memo
                    .reported_at
                    .is_none_or(|at| at.elapsed() >= DRAIN_WAIT_REFRESH)
            {
                memo.fingerprint = fingerprint;
                memo.reported_at = Some(std::time::Instant::now());
                true
            } else {
                false
            }
        };
        if rebuild {
            runtime.set_drain_wait_set(drain_holdout_rows(&holding));
        }
        if !holding.is_empty() || self.exec.latest_pending_heavy_key().is_some() {
            return false;
        }
        // Intake §3.3's exit parenthetical (the HS3 ruling's N5): a
        // transfer append mid-copy holds the daemon open — cutting a
        // multi-GB spool at exit wastes it entirely. (Mid-stream HTTP
        // uploads are the priced residual: request-scoped, usually
        // session-accompanied, and re-uploadable against the successor.)
        if crate::transfer_store::any_transfer_appending() {
            return false;
        }
        runtime.mark_exited();
        true
    }

    pub fn spawn(self) -> JoinHandle<()> {
        let intent_rx = self.config.bus.subscribe_intents();
        let rx = self.config.bus.subscribe();
        tokio::spawn(self.run_event_loop(intent_rx, rx, None))
    }

    pub(crate) fn spawn_foreground_listener(
        self,
        primary_session_id: String,
    ) -> ForegroundSupervisorHandle {
        let intent_rx = self.config.bus.subscribe_intents();
        let rx = self.config.bus.subscribe();
        let handle = tokio::spawn(self.run_event_loop(intent_rx, rx, Some(primary_session_id)));
        ForegroundSupervisorHandle { handle }
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
    ) -> UserAttachments {
        if attachments.is_empty() {
            return UserAttachments::default();
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

/// The one drain-hold rule ([`SessionSupervisor::drain_exit_ready`]'s
/// filter): holding work is any phase but `done` — EXCEPT an idle
/// session whose background-task park DIED with a backend restart
/// (`died_park`). Its wait is on tasks that no longer exist; re-running
/// them is an owner decision the successor daemon serves just as well
/// (the durable `bg_park` marker survives the exit), so counting it as
/// held work would strand the drain behind a wait that can never end.
/// Any other phase — an approval, a live turn, an interrupt — holds
/// regardless of the died mark.
pub(crate) fn session_holds_drain(phase: &str, died_park: bool) -> bool {
    phase != "done" && !(phase == "idle" && died_park)
}

/// The named drain wait set: one row per holding session, with the
/// durable limit-park marker resolved from its `session_meta.json` —
/// "parked until T" is decisive information (an in-memory park can hold
/// the drain for hours), so the reset instant must reach every surface
/// that renders the drain. Parked rows sort first (earliest reset
/// leading) so capped renders keep the decisive ones; the rest keep id
/// order for stable rendering.
fn drain_holdout_rows(
    holding: &[(String, String, Option<String>, String, PathBuf)],
) -> Vec<crate::handover::DrainHoldout> {
    let mut rows: Vec<crate::handover::DrainHoldout> = holding
        .iter()
        .map(|(session_id, source, name, phase, session_dir)| {
            let meta = std::fs::read_to_string(session_dir.join("session_meta.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<session_log::SessionMeta>(&raw).ok());
            let (limit_park, bg_park) = meta
                .map(|meta| (meta.limit_park, meta.bg_park))
                .unwrap_or((None, None));
            crate::handover::DrainHoldout {
                session_id: session_id.clone(),
                source: source.clone(),
                name: name.clone(),
                phase: normalize_supervisor_phase(phase),
                limit_park,
                bg_park,
            }
        })
        .collect();
    let park_key = |row: &crate::handover::DrainHoldout| match &row.limit_park {
        Some(park) => (0u8, park.resets_at_epoch.unwrap_or(u64::MAX)),
        None => (1u8, 0),
    };
    rows.sort_by(|a, b| {
        park_key(a)
            .cmp(&park_key(b))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    rows
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

    /// The drain wait-set arithmetic with a died-park member (the
    /// parked-task respawn honesty card's pin): an idle session whose
    /// bg park died is RELEASABLE — every other hold stands, including
    /// a died-marked session that still owes an approval or a turn.
    #[test]
    fn drain_holding_arithmetic_releases_only_the_died_park_idle() {
        // (phase, died_park, holds)
        for (phase, died_park, holds) in [
            ("done", false, false),
            ("done", true, false),
            ("idle", false, true),
            ("idle", true, false), // the died park: its wait is on nobody
            ("running", false, true),
            ("running", true, true), // mid-turn work holds regardless
            ("waiting_approval", true, true),
            ("waiting_human", true, true),
            ("interrupted", true, true),
        ] {
            assert_eq!(
                session_holds_drain(phase, died_park),
                holds,
                "phase={phase} died_park={died_park}"
            );
        }
        // Arithmetic over a mixed set: one done, one live idle, one
        // died-park idle, one died-marked approval-waiter → 2 hold.
        let sessions = [
            ("done", false),
            ("idle", false),
            ("idle", true),
            ("waiting_approval", true),
        ];
        let holding = sessions
            .iter()
            .filter(|(phase, died)| session_holds_drain(phase, *died))
            .count();
        assert_eq!(holding, 2);
    }

    /// Died-park membership follows the activity carrier: non-empty
    /// died fields set it (managed sessions only), any later publish
    /// without them clears it, and removal purges it.
    #[tokio::test]
    async fn died_park_membership_follows_session_activity() {
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), EventBus::new());
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "s-died".to_string(),
                managed_session("s-died", "claude-code"),
            );
        }
        let died = crate::types::SessionActivityVitals {
            died_background_tasks: vec!["cargo test battery".into()],
            died_tasks_cause: Some("the credential-reload restart".into()),
            ..Default::default()
        };
        supervisor
            .observe_lifecycle_event(&AppEvent::SessionActivity {
                session_id: Some("s-died".into()),
                activity: died.clone(),
            })
            .await;
        assert!(supervisor
            .state
            .lock()
            .await
            .died_park_sessions
            .contains("s-died"));
        // A foreign session's died fields never grow the set.
        supervisor
            .observe_lifecycle_event(&AppEvent::SessionActivity {
                session_id: Some("s-foreign".into()),
                activity: died,
            })
            .await;
        assert!(!supervisor
            .state
            .lock()
            .await
            .died_park_sessions
            .contains("s-foreign"));
        // Work resumes (any publish without died fields) → cleared.
        supervisor
            .observe_lifecycle_event(&AppEvent::SessionActivity {
                session_id: Some("s-died".into()),
                activity: crate::types::SessionActivityVitals::default(),
            })
            .await;
        assert!(supervisor.state.lock().await.died_park_sessions.is_empty());
    }

    pub(crate) fn managed_session(id: &str, source: &str) -> ManagedSession {
        let (tx, _rx) = mpsc::channel(1);
        ManagedSession {
            session_id: id.to_string(),
            source: source.to_string(),
            name: None,
            phase: "idle".to_string(),
            reload: None,
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
        SessionSupervisor::new(test_supervisor_config(project_root, bus))
    }

    /// The hermetic config behind [`test_supervisor`], exposed so tests
    /// can override one field (a draining handover runtime, a provider
    /// factory) without duplicating the literal.
    pub(crate) fn test_supervisor_config(
        project_root: PathBuf,
        bus: EventBus,
    ) -> SessionSupervisorConfig {
        SessionSupervisorConfig {
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
                    effort: None,
                    permission_mode: "default".to_string(),
                    allowed_tools: Vec::new(),
                },
            )),
            shared_kimi_config: Arc::new(tokio::sync::RwLock::new(
                control_plane::KimiRuntimeConfig {
                    command: "kimi".to_string(),
                    model: None,
                    thinking: None,
                    permission_mode: "manual".to_string(),
                    allowed_tools: None,
                    plan_mode: false,
                    swarm_mode: false,
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
            // component makes the scratch per process INSTANCE, and the
            // atomic counter makes it per SUPERVISOR (macOS SystemTime has
            // microsecond granularity, so two supervisors minted by
            // concurrently running tests could otherwise share a home —
            // observed as a launched-session scan flake). Sub-agent and
            // rename flows WRITE through this home, not just read.
            logs_home_override: Some(std::env::temp_dir().join(format!(
                "intendant-test-logs-home-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
                {
                    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                }
            ))),
            git_vitals_targets: None,
            hosted_control_cert_dir: None,
            launch_gate_for_tests: None,
            claude_rewind_capability_for_tests: None,
            agenda: None,
            handover: None,
        }
    }

    /// Track HS3: the funnel gate — session-creating intents refuse with
    /// the structured `daemon_draining` error; in-flight-work intents
    /// pass the gate untouched (whatever their arms then say about
    /// unknown test sessions, it is never a drain refusal).
    #[tokio::test]
    async fn drain_serves_in_flight_approvals_and_steers() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::handover::HandoverRuntime::initialize(
            home.path(),
            7001,
            0,
        ));
        assert_eq!(
            runtime.request_drain(None),
            crate::handover::DrainRequest::Entered
        );
        let bus = EventBus::new();
        let mut config = test_supervisor_config(project.path().to_path_buf(), bus.clone());
        config.handover = Some(runtime);
        let supervisor = SessionSupervisor::new(config);
        let mut rx = bus.subscribe();

        let creating = [
            r#"{"action":"create_session","task":"t"}"#,
            r#"{"action":"start_task","task":"t"}"#,
            r#"{"action":"resume_session","source":"intendant","session_id":"s"}"#,
        ];
        for json in creating {
            supervisor
                .handle_control_msg(serde_json::from_str(json).unwrap())
                .await;
        }
        let mut refusals = 0;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::LoopError(message) = &event {
                assert!(
                    message.starts_with("daemon_draining"),
                    "the only loop errors here are drain refusals: {message}"
                );
                refusals += 1;
            }
        }
        assert_eq!(refusals, 3, "every creating intent refused, structured");

        let serving = [
            r#"{"action":"start_task","task":"t","session_id":"missing"}"#,
            r#"{"action":"follow_up","session_id":"missing","text":"t"}"#,
            r#"{"action":"steer","session_id":"missing","text":"t"}"#,
            r#"{"action":"interrupt","session_id":"missing"}"#,
            r#"{"action":"approve","id":7}"#,
        ];
        for json in serving {
            supervisor
                .handle_control_msg(serde_json::from_str(json).unwrap())
                .await;
        }
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::LoopError(message) = &event {
                assert!(
                    !message.starts_with("daemon_draining"),
                    "in-flight-work intents must never hit the drain gate: {message}"
                );
            }
        }
    }

    /// Track HS3: the exit condition — draining with zero live sessions
    /// (and no launch body in flight) is ready; a live session holds the
    /// daemon open and the NAMED wait set — each holdout with its phase
    /// and durable limit-park marker — reaches the status surface and
    /// the presence record (the drain-holdout-honesty commission: the
    /// banner must say WHO holds and until WHEN, never wait silently).
    #[tokio::test]
    async fn drain_exit_readiness_follows_live_sessions() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::handover::HandoverRuntime::initialize(
            home.path(),
            7001,
            0,
        ));
        let bus = EventBus::new();
        let mut config = test_supervisor_config(project.path().to_path_buf(), bus.clone());
        config.handover = Some(runtime.clone());
        let supervisor = SessionSupervisor::new(config);

        assert!(
            !supervisor.drain_exit_ready().await,
            "not draining ⇒ never exit-ready"
        );
        assert_eq!(
            runtime.request_drain(None),
            crate::handover::DrainRequest::Entered
        );
        let parked_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            parked_dir.path().join("session_meta.json"),
            serde_json::json!({
                "session_id": "s-1",
                "created_at": "2026-08-01 09:00:00",
                "status": "running",
                "limit_park": {"resets_at_epoch": 1_754_000_000_u64, "has_pending": true},
            })
            .to_string(),
        )
        .unwrap();
        {
            let mut state = supervisor.state.lock().await;
            let mut parked = managed_session("s-1", "claude-code");
            parked.name = Some("nightly build".to_string());
            parked.phase = "waiting_rate_limit".to_string();
            parked.session_dir = parked_dir.path().to_path_buf();
            state.sessions.insert("s-1".to_string(), parked);
        }
        assert!(
            !supervisor.drain_exit_ready().await,
            "a live supervised session holds the drainer open"
        );
        let status = runtime.status_json();
        assert_eq!(status["holdouts"][0]["session_id"], "s-1");
        assert_eq!(status["holdouts"][0]["source"], "claude-code");
        assert_eq!(status["holdouts"][0]["name"], "nightly build");
        assert_eq!(status["holdouts"][0]["phase"], "waiting_rate_limit");
        assert_eq!(
            status["holdouts"][0]["limit_park"]["resets_at_epoch"], 1_754_000_000_u64,
            "the parked-until instant is the decisive fact — it must reach the surface"
        );
        let record = crate::handover::read_presence_records(home.path())
            .into_iter()
            .find(|record| record.state == "draining")
            .expect("draining presence record");
        assert_eq!(record.session_count, Some(1));
        assert_eq!(
            record.holdouts.as_ref().map(Vec::len),
            Some(1),
            "the successor-side channel carries the named rows"
        );

        supervisor.state.lock().await.sessions.remove("s-1");
        assert!(
            supervisor.drain_exit_ready().await,
            "last session gone ⇒ graceful exit"
        );
        assert_eq!(
            runtime.status_json()["holdouts"].as_array().map(Vec::len),
            Some(0),
            "an emptied wait set reports empty, never stale rows"
        );
    }

    /// Parked holdouts sort first (earliest reset leading) so capped
    /// renders keep the decisive rows; phases normalize; a dir with no
    /// meta yields an honest markerless row rather than being dropped.
    #[test]
    fn drain_holdout_rows_resolve_markers_and_sort_parked_first() {
        let write_meta = |resets_at: u64| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("session_meta.json"),
                serde_json::json!({
                    "session_id": "x",
                    "created_at": "2026-08-01 09:00:00",
                    "limit_park": {"resets_at_epoch": resets_at, "has_pending": true},
                })
                .to_string(),
            )
            .unwrap();
            dir
        };
        let late = write_meta(2_000);
        let early = write_meta(1_000);
        let rows = drain_holdout_rows(&[
            (
                "s-plain".to_string(),
                "codex".to_string(),
                None,
                "Running-Agent".to_string(),
                PathBuf::from("/nonexistent/session-dir"),
            ),
            (
                "s-late".to_string(),
                "claude-code".to_string(),
                None,
                "waiting_rate_limit".to_string(),
                late.path().to_path_buf(),
            ),
            (
                "s-early".to_string(),
                "claude-code".to_string(),
                Some("weekly digest".to_string()),
                "waiting_rate_limit".to_string(),
                early.path().to_path_buf(),
            ),
        ]);
        let ids: Vec<&str> = rows.iter().map(|row| row.session_id.as_str()).collect();
        assert_eq!(ids, vec!["s-early", "s-late", "s-plain"]);
        assert_eq!(
            rows[0]
                .limit_park
                .as_ref()
                .and_then(|park| park.resets_at_epoch),
            Some(1_000)
        );
        assert_eq!(rows[2].limit_park, None);
        assert_eq!(
            rows[2].phase, "running",
            "phases normalize for stable rendering"
        );
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

    /// The Vault reload list's corpus is the LIVE registry, filtered by
    /// backend source — id/source/name/phase ride verbatim, native
    /// sessions and other backends never appear, and the order is
    /// stable.
    #[tokio::test]
    async fn reload_candidates_derive_from_live_registry() {
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), EventBus::new());
        {
            let mut state = supervisor.state.lock().await;
            let mut claude = managed_session("claude-b", "claude-code");
            claude.name = Some("steward pass".to_string());
            claude.phase = "running".to_string();
            state.sessions.insert("claude-b".to_string(), claude);
            state.sessions.insert(
                "claude-a".to_string(),
                managed_session("claude-a", "claude-code"),
            );
            state
                .sessions
                .insert("codex-1".to_string(), managed_session("codex-1", "codex"));
            state.sessions.insert(
                "native-1".to_string(),
                managed_session("native-1", "intendant"),
            );
        }
        let registry = supervisor.live_session_registry();
        let candidates = registry.reload_candidates_for_source("claude-code").await;
        assert_eq!(
            candidates,
            vec![
                ReloadCandidate {
                    session_id: "claude-a".to_string(),
                    source: "claude-code".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    reload: None,
                },
                ReloadCandidate {
                    session_id: "claude-b".to_string(),
                    source: "claude-code".to_string(),
                    name: Some("steward pass".to_string()),
                    phase: "running".to_string(),
                    reload: None,
                },
            ]
        );
        assert_eq!(
            registry.reload_candidates_for_source("codex").await.len(),
            1,
            "other backends filter by their own source"
        );
    }

    /// Parked, rate-limit-parked, and mid-turn sessions are all reload
    /// candidates (registry membership is liveness — phase is never a
    /// filter), and no row cap applies: the disk catalog's limit:100
    /// truncation that aged parked sessions off the Vault list has no
    /// analogue here.
    #[tokio::test]
    async fn parked_sessions_remain_reload_candidates() {
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), EventBus::new());
        {
            let mut state = supervisor.state.lock().await;
            for (id, phase) in [
                ("parked", "idle"),
                ("rate-limited", "waiting_rate_limit"),
                ("mid-turn", "running"),
                ("waiting", "waiting_approval"),
            ] {
                let mut session = managed_session(id, "codex");
                session.phase = phase.to_string();
                state.sessions.insert(id.to_string(), session);
            }
            for i in 0..150 {
                let id = format!("bulk-{i:03}");
                state
                    .sessions
                    .insert(id.clone(), managed_session(&id, "codex"));
            }
        }
        let candidates = supervisor
            .live_session_registry()
            .reload_candidates_for_source("codex")
            .await;
        assert_eq!(
            candidates.len(),
            154,
            "every live session of the source is a candidate — no truncation"
        );
        for id in ["parked", "rate-limited", "mid-turn", "waiting"] {
            assert!(
                candidates.iter().any(|c| c.session_id == id),
                "{id} must stay a reload candidate"
            );
        }
    }

    /// Sessions absent from the live registry are never candidates: a
    /// finished session drops off the moment the supervisor removes it,
    /// and a dead read-handle (supervisor gone) reports none — the disk
    /// catalog's summary-less "in_progress forever" dirs can't leak in.
    #[tokio::test]
    async fn dead_sessions_never_listed_as_live() {
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), EventBus::new());
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("live-1".to_string(), managed_session("live-1", "codex"));
            state
                .sessions
                .insert("ends".to_string(), managed_session("ends", "codex"));
        }
        let registry = supervisor.live_session_registry();
        assert_eq!(
            registry.reload_candidates_for_source("codex").await.len(),
            2
        );

        {
            let mut state = supervisor.state.lock().await;
            state.remove_session("ends");
        }
        let candidates = registry.reload_candidates_for_source("codex").await;
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["live-1"],
            "a session removed from the registry is no longer a candidate"
        );

        drop(supervisor);
        assert!(
            registry
                .reload_candidates_for_source("codex")
                .await
                .is_empty(),
            "a dead registry handle truthfully reports no live sessions"
        );
    }

    /// The served lifecycle is the daemon's own bookkeeping: a routed
    /// reload stamps `requested` on the exact row it accepts, the loop's
    /// typed progress events advance it (respawning → done / failed with
    /// the error served verbatim), and the candidate list carries it —
    /// the client renders THIS, never local request memory. A re-request
    /// over a terminal state simply restarts the lifecycle: the daemon
    /// keeps no dedup latch, so a stale request can never block a fresh
    /// ceremony.
    #[tokio::test]
    async fn reload_lifecycle_is_daemon_owned_and_served() {
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), EventBus::new());
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("ext-1".to_string(), managed_session("ext-1", "claude-code"));
        }
        let registry = supervisor.live_session_registry();
        let lifecycle = |candidates: Vec<ReloadCandidate>| candidates[0].reload.clone();

        assert_eq!(
            lifecycle(registry.reload_candidates_for_source("claude-code").await),
            None,
            "no lifecycle serves until a reload is first requested"
        );

        supervisor
            .route_reload_credentials("ext-1".to_string())
            .await;
        let requested = lifecycle(registry.reload_candidates_for_source("claude-code").await)
            .expect("an accepted reload stamps requested");
        assert_eq!(requested.state, ReloadLifecycleState::Requested);
        assert!(requested.at_unix_ms > 0, "stamps carry their time");

        supervisor
            .update_reload_lifecycle(Some("ext-1"), &event::CredentialReloadProgress::Respawning)
            .await;
        assert_eq!(
            lifecycle(registry.reload_candidates_for_source("claude-code").await)
                .expect("progress serves")
                .state,
            ReloadLifecycleState::Respawning
        );

        supervisor
            .update_reload_lifecycle(
                Some("ext-1"),
                &event::CredentialReloadProgress::Failed {
                    error: "could not respawn claude-code: exec failed".to_string(),
                },
            )
            .await;
        let failed = lifecycle(registry.reload_candidates_for_source("claude-code").await)
            .expect("failure serves");
        assert_eq!(failed.state, ReloadLifecycleState::Failed);
        assert_eq!(
            failed.error.as_deref(),
            Some("could not respawn claude-code: exec failed"),
            "the respawn error rides the served row"
        );

        // A fresh request over the terminal state restarts the lifecycle
        // — no latch survives.
        supervisor
            .route_reload_credentials("ext-1".to_string())
            .await;
        let restarted = lifecycle(registry.reload_candidates_for_source("claude-code").await)
            .expect("re-request stamps requested again");
        assert_eq!(restarted.state, ReloadLifecycleState::Requested);
        assert_eq!(restarted.error, None, "the old failure never lingers");

        supervisor
            .update_reload_lifecycle(Some("ext-1"), &event::CredentialReloadProgress::Done)
            .await;
        assert_eq!(
            lifecycle(registry.reload_candidates_for_source("claude-code").await)
                .expect("done serves")
                .state,
            ReloadLifecycleState::Done
        );
    }

    /// Reload-all fans out over the supervisor's OWN live registry — the
    /// exact set served as candidates: every row of the source is stamped
    /// `requested` atomically with membership and gets its own
    /// per-session reload event; other backends and native rows are
    /// untouched, and a native/empty source is refused outright.
    #[tokio::test]
    async fn reload_all_rides_the_served_candidate_set() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            for (id, source) in [
                ("ext-b", "claude-code"),
                ("ext-a", "claude-code"),
                ("codex-1", "codex"),
                ("native-1", "intendant"),
            ] {
                state
                    .sessions
                    .insert(id.to_string(), managed_session(id, source));
            }
        }
        supervisor
            .route_reload_credentials_all("claude-code".to_string())
            .await;

        let registry = supervisor.live_session_registry();
        let claude = registry.reload_candidates_for_source("claude-code").await;
        assert!(
            claude.iter().all(|candidate| candidate
                .reload
                .as_ref()
                .is_some_and(|reload| reload.state == ReloadLifecycleState::Requested)),
            "every row of the source is stamped requested"
        );
        assert_eq!(
            registry.reload_candidates_for_source("codex").await[0].reload,
            None,
            "other backends' rows are untouched"
        );

        let mut reloaded = Vec::new();
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::ReloadBackendCredentials { session_id } = event {
                reloaded.push(session_id.expect("fan-out events are targeted"));
            }
        }
        assert_eq!(
            reloaded,
            vec!["ext-a".to_string(), "ext-b".to_string()],
            "one per-session event per matching row, in stable order"
        );

        // Native and empty sources are refused before any stamp or event.
        supervisor
            .route_reload_credentials_all("intendant".to_string())
            .await;
        supervisor
            .route_reload_credentials_all("  ".to_string())
            .await;
        while let Ok(event) = bus_rx.try_recv() {
            assert!(
                !matches!(event, AppEvent::ReloadBackendCredentials { .. }),
                "a refused source fans nothing out"
            );
        }
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

    /// The readopt-successor false-ghost class (five live specimens,
    /// 2026-07-29): `apply_session_identity` re-keys a post-identity
    /// entry to its backend id and leaves the wrapper log-dir id behind
    /// as an alias, while catalog rows key by the log-dir id — an
    /// entry-keys-only live set therefore read every post-identity live
    /// session as dead and its card wore the ghost chip (fold-order
    /// roulette against the dead twin). The published set is
    /// alias-closed: aliases resolving to a live entry count; aliases of
    /// closed-channel or removed entries do not. The grid half attaches
    /// the successor's row under the closed set and must serve
    /// live_wrapper:true / ghost:false.
    #[test]
    fn readopt_successor_card_does_not_false_ghost() {
        let state = Arc::new(AsyncMutex::new(SupervisorState::default()));
        let (open_tx, _open_rx) = mpsc::channel(1);
        {
            let mut guard = state.try_lock().unwrap();
            // Post-identity live entry: keyed by the backend id, wrapper
            // dir id aliased to it, follow-up channel open.
            let mut entry = managed_session("rsg-backend-b", "claude-code");
            entry.follow_up_tx = open_tx.clone();
            guard.sessions.insert("rsg-backend-b".to_string(), entry);
            guard
                .session_aliases
                .insert("rsg-wrapper-dir".to_string(), "rsg-backend-b".to_string());
            // A finished entry's alias must not read live (closed
            // channel)…
            guard.sessions.insert(
                "rsg-closed".to_string(),
                managed_session("rsg-closed", "claude-code"),
            );
            guard
                .session_aliases
                .insert("rsg-closed-alias".to_string(), "rsg-closed".to_string());
            // …and a dangling alias resolves to nothing.
            guard
                .session_aliases
                .insert("rsg-dangling".to_string(), "rsg-gone".to_string());
        }
        let registry = LiveSessionRegistry {
            state: Arc::downgrade(&state),
        };
        let live = registry.live_wrapper_ids().expect("uncontended lock");
        assert!(live.contains("rsg-backend-b"));
        assert!(
            live.contains("rsg-wrapper-dir"),
            "the wrapper log-dir alias of a live entry is live — the catalog row keys by it"
        );
        assert!(!live.contains("rsg-closed"));
        assert!(!live.contains("rsg-closed-alias"));
        assert!(!live.contains("rsg-dangling"));

        // Grid half: the successor's catalog row (keyed by its log dir)
        // stops false-ghosting under the closed set even though its
        // transcript predates the boot watershed.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("rsg-wrapper-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl"), b"{}\n").unwrap();
        let watershed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600;
        let joins =
            crate::web_gateway::session_catalog::grid_envelope::GridEnvelopeJoins::for_tests(
                Some(watershed),
                Some(live),
                None,
                None,
            );
        let mut row = serde_json::json!({});
        joins.attach(&mut row, "rsg-wrapper-dir", &dir);
        assert_eq!(
            row["boot"]["live_wrapper"], true,
            "the alias-closed live set must reach the row"
        );
        assert_eq!(
            row["boot"]["ghost"], false,
            "a live readopt successor's card never wears the ghost chip"
        );
        assert_eq!(row["boot"]["era"], "current");
        drop(open_tx);
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

//! Owner-facing per-backend CLI install lane (the Vault cards' Install
//! button, deferred from the #791 wave).
//!
//! Daemon-runs-installers is arbitrary code execution, so the whole lane
//! is built around the approval rail:
//!
//! 1. The **install-command matrix** below is the single declaration of
//!    each backend's official installer per platform. The served
//!    `/api/external-agents` payload, the dashboard button availability,
//!    and the approval-wall copy all derive from it (derive-don't-mirror).
//! 2. `POST /api/external-agents/install` (Settings-grade — hosted
//!    provenance `role:none` never reaches it, like the skills S3/S4
//!    walls) **proposes**: it never executes. Under the CommandExec wall
//!    conventions the proposal consults the daemon's live autonomy state:
//!    a Deny rule covering shell execution refuses by name with nothing
//!    raised; Low/Medium raise an `ApprovalRequired` on the shared rail
//!    (category `command_exec`, the exact command verbatim in the
//!    preview); High/Full — where the dial says commands run unprompted —
//!    run the owner's clicked command directly, exactly as the native
//!    loop would.
//! 3. On approval the command runs through the **sandboxed runtime lane**
//!    (`agent_runner::run_install_batch`): the keyless `intendant-runtime`
//!    executor with its provider-key and host-credential env scrub, and on
//!    macOS the sensitive-directory Seatbelt profile. The per-session
//!    *write* restriction is deliberately not applied — installers write
//!    package prefixes (`~/.local/bin`, npm/brew trees) by definition, and
//!    a write set scoped to session grants would fail every install by
//!    construction. Full output lands under
//!    `<state root>/logs/installs/<backend>-<ts>/`, and the outcome
//!    (exit code, output tail, log path) surfaces honestly on the card.
//! 4. The resolution waiter mirrors the live-audio consent gate: it
//!    observes `AppEvent::ControlCommand` approval verbs on the bus, and
//!    the session supervisor consults [`install_pending`] before warning
//!    about an approval id it does not own.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use crate::autonomy::{ActionCategory, ApprovalRule, AutonomyState};
use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::external_agent::AgentBackend;

// ---------------------------------------------------------------------------
// The install-command matrix (declared ONCE; everything else derives)
// ---------------------------------------------------------------------------

/// Platform axis of the matrix. Compile-time: a daemon only ever serves
/// (and executes) its own platform's lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallPlatform {
    MacOs,
    Linux,
    Windows,
}

impl InstallPlatform {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MacOs => "macos",
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }

    /// The platform this daemon runs on. `None` on an untargeted OS —
    /// the matrix then has no lane and the button honestly never renders.
    pub(crate) fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::MacOs)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else if cfg!(windows) {
            Some(Self::Windows)
        } else {
            None
        }
    }
}

/// One cell of the matrix: the official, vendor-documented install
/// command for `backend` on `platform`, verbatim as the approval wall
/// announces and the runtime executes it.
pub(crate) struct InstallLane {
    pub(crate) backend: AgentBackend,
    pub(crate) platform: InstallPlatform,
    pub(crate) command: &'static str,
}

/// The matrix. Sources ([external] claims, fetched 2026-08-08):
///
/// - **Claude Code** — code.claude.com/docs/en/setup, "Native Install
///   (Recommended)": `curl -fsSL https://claude.ai/install.sh | bash`
///   (macOS/Linux/WSL); the Windows CMD lane is the vendor's own CMD tab
///   (the runtime's Windows shell is `cmd.exe /C`). npm/brew/winget lanes
///   exist but the native installer is the documented primary.
/// - **Codex** — github.com/openai/codex README: shell installer
///   `curl -fsSL https://chatgpt.com/codex/install.sh | sh` (macOS/Linux)
///   and the vendor's own PowerShell one-liner for Windows (documented in
///   the exact `powershell -ExecutionPolicy ByPass -c "…"` form, which is
///   cmd.exe-invokable). npm (`npm install -g @openai/codex`) and
///   `brew install --cask codex` are documented alternatives.
/// - **Kimi Code** — moonshotai.github.io/kimi-code getting-started:
///   install script (recommended, no Node required)
///   `curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash`;
///   Windows is the vendor's `irm https://code.kimi.com/kimi-code/install.ps1 | iex`
///   PowerShell lane, wrapped for the runtime's cmd.exe shell the same
///   way the Codex vendor documents theirs. npm alternative:
///   `npm install -g @moonshot-ai/kimi-code`.
/// - **Pi** deliberately has no lane: no vendor-documented single-command
///   installer of the same class, so the matrix says so and no button
///   renders for it.
const INSTALL_MATRIX: &[InstallLane] = &[
    InstallLane {
        backend: AgentBackend::ClaudeCode,
        platform: InstallPlatform::MacOs,
        command: "curl -fsSL https://claude.ai/install.sh | bash",
    },
    InstallLane {
        backend: AgentBackend::ClaudeCode,
        platform: InstallPlatform::Linux,
        command: "curl -fsSL https://claude.ai/install.sh | bash",
    },
    InstallLane {
        backend: AgentBackend::ClaudeCode,
        platform: InstallPlatform::Windows,
        command: "curl -fsSL https://claude.ai/install.cmd -o install.cmd && install.cmd && del install.cmd",
    },
    InstallLane {
        backend: AgentBackend::Codex,
        platform: InstallPlatform::MacOs,
        command: "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
    },
    InstallLane {
        backend: AgentBackend::Codex,
        platform: InstallPlatform::Linux,
        command: "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
    },
    InstallLane {
        backend: AgentBackend::Codex,
        platform: InstallPlatform::Windows,
        command: "powershell -ExecutionPolicy ByPass -c \"irm https://chatgpt.com/codex/install.ps1 | iex\"",
    },
    InstallLane {
        backend: AgentBackend::Kimi,
        platform: InstallPlatform::MacOs,
        command: "curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash",
    },
    InstallLane {
        backend: AgentBackend::Kimi,
        platform: InstallPlatform::Linux,
        command: "curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash",
    },
    InstallLane {
        backend: AgentBackend::Kimi,
        platform: InstallPlatform::Windows,
        command: "powershell -ExecutionPolicy Bypass -c \"irm https://code.kimi.com/kimi-code/install.ps1 | iex\"",
    },
];

/// The matrix cell for (`backend`, `platform`), if declared.
pub(crate) fn install_command(
    backend: &AgentBackend,
    platform: InstallPlatform,
) -> Option<&'static str> {
    INSTALL_MATRIX
        .iter()
        .find(|lane| lane.backend == *backend && lane.platform == platform)
        .map(|lane| lane.command)
}

/// The approval-rail preview: the backend, the EXACT command verbatim on
/// its own line, and the plain sentence saying what approving does. This
/// is the whole approval-wall copy — it derives from the matrix cell and
/// nothing else re-states the command.
pub(crate) fn install_approval_preview(backend: &AgentBackend, command: &str) -> String {
    format!(
        "Install {backend}\n\n{command}\n\nIntendant will run this in a terminal session on this machine."
    )
}

// ---------------------------------------------------------------------------
// Install state registry
// ---------------------------------------------------------------------------

/// Lifecycle of one backend's install lane. `Declined` and `Refused` are
/// deliberately distinct from `Failed`: they mean **nothing executed**.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InstallState {
    Idle,
    /// Proposed and waiting on the approval rail. Nothing has executed.
    WaitingApproval {
        approval_id: u64,
    },
    Running,
    Succeeded {
        exit_code: i32,
        output_tail: String,
        log_dir: PathBuf,
        finished_ms: u64,
    },
    Failed {
        detail: String,
        exit_code: Option<i32>,
        output_tail: String,
        log_dir: Option<PathBuf>,
        finished_ms: u64,
    },
    /// The user declined/skipped the approval, or it timed out unanswered.
    /// Nothing executed.
    Declined {
        detail: String,
        finished_ms: u64,
    },
    /// Refused before the wall (approval-policy deny, no matrix lane,
    /// unknown backend). Nothing executed and no approval was raised.
    Refused {
        detail: String,
        finished_ms: u64,
    },
}

impl InstallState {
    fn is_in_flight(&self) -> bool {
        matches!(self, Self::WaitingApproval { .. } | Self::Running)
    }
}

/// Per-backend install states, keyed by `AgentBackend::as_short_str`.
/// Injectable so tests never share the process-global map.
#[derive(Clone, Default)]
pub(crate) struct InstallRegistry(Arc<StdMutex<HashMap<String, InstallState>>>);

impl InstallRegistry {
    pub(crate) fn state(&self, backend: &AgentBackend) -> InstallState {
        self.0
            .lock()
            .ok()
            .and_then(|map| map.get(backend.as_short_str()).cloned())
            .unwrap_or(InstallState::Idle)
    }

    fn set(&self, backend: &AgentBackend, state: InstallState) {
        if let Ok(mut map) = self.0.lock() {
            map.insert(backend.as_short_str().to_string(), state);
        }
    }
}

/// The daemon's live registry (what the gateway serves and mutates).
pub(crate) fn global_registry() -> InstallRegistry {
    static REGISTRY: OnceLock<InstallRegistry> = OnceLock::new();
    REGISTRY.get_or_init(InstallRegistry::default).clone()
}

/// Approval ids of install proposals currently blocked on a decision.
/// Same contract as `live_audio::spawn_consent_pending` /
/// `mcp::ask_user_question_pending`: the gate's own waiter resolves the
/// id, and the session supervisor consults this set so the id is not
/// misreported as unknown. Process-wide on purpose — ids are process-wide.
fn pending_install_approvals() -> &'static StdMutex<std::collections::HashSet<u64>> {
    static PENDING: OnceLock<StdMutex<std::collections::HashSet<u64>>> = OnceLock::new();
    PENDING.get_or_init(|| StdMutex::new(std::collections::HashSet::new()))
}

/// Whether `id` is an install proposal still waiting for a decision.
pub(crate) fn install_pending(id: u64) -> bool {
    pending_install_approvals()
        .lock()
        .map(|set| set.contains(&id))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Autonomy consultation (the CommandExec wall conventions)
// ---------------------------------------------------------------------------

static AUTONOMY: OnceLock<crate::autonomy::SharedAutonomy> = OnceLock::new();

/// Hand the install gate the daemon's live autonomy state. Called once at
/// startup where the shared state is built; later calls are no-ops.
pub(crate) fn register_autonomy(handle: crate::autonomy::SharedAutonomy) {
    let _ = AUTONOMY.set(handle);
}

/// What the proposal does next, per the CommandExec wall conventions at
/// daemon scope (session `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallGateDecision {
    /// A Deny rule covers shell execution: refuse by name, raise nothing.
    Refused(String),
    /// Low/Medium: raise the approval wall and wait.
    Wall,
    /// High/Full: the dial says commands run unprompted — the click is
    /// the human act; run the announced command directly.
    Run,
}

/// Pure decision over an autonomy snapshot — pinned exhaustively in tests.
pub(crate) fn install_gate_decision(state: &AutonomyState) -> InstallGateDecision {
    if state
        .effective_rules(None)
        .rule_for(ActionCategory::CommandExec)
        == ApprovalRule::Deny
    {
        return InstallGateDecision::Refused(
            "Denied by approval policy: an [approval] deny rule covers shell execution on this \
             daemon, so the installer was not proposed."
                .to_string(),
        );
    }
    if state.needs_approval(None, ActionCategory::CommandExec) {
        InstallGateDecision::Wall
    } else {
        InstallGateDecision::Run
    }
}

async fn live_gate_decision() -> InstallGateDecision {
    match AUTONOMY.get() {
        Some(shared) => install_gate_decision(&*shared.read().await),
        // No registered autonomy (shouldn't happen in served shapes):
        // fail toward asking, never toward running.
        None => InstallGateDecision::Wall,
    }
}

// ---------------------------------------------------------------------------
// Proposal + gate waiter + executor
// ---------------------------------------------------------------------------

/// How long a raised install approval waits on the rail before returning
/// to a decline-shaped terminal state. Generous: the owner may be reading
/// the command.
const INSTALL_APPROVAL_WAIT: Duration = Duration::from_secs(600);

/// Per-command timeout handed to the runtime (installers legitimately run
/// minutes on cold networks). The batch hard timeout adds a margin so the
/// runtime's own kill fires first and its salvage tails survive.
const INSTALL_COMMAND_TIMEOUT_MS: u64 = 900_000;
const INSTALL_BATCH_HARD_TIMEOUT: Duration = Duration::from_secs(960);

/// Bound on the output tail kept in the served status (full output stays
/// in the log dir).
const INSTALL_OUTPUT_TAIL_CHARS: usize = 2000;

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Named refusals for `propose_install` (HTTP 4xx shapes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProposeRefusal {
    UnknownBackend(String),
    /// The matrix has no lane for this backend on this platform.
    NoLane {
        backend: String,
        platform: &'static str,
    },
}

/// Propose installing `backend` on this machine. Never executes in this
/// call: it either refuses by name, records a policy refusal, raises the
/// approval wall (spawning a detached waiter), or — when the autonomy
/// dial says commands run unprompted — spawns the executor. Returns the
/// resulting state for the response body. Idempotent while in flight:
/// re-clicks return the current state instead of double-arming.
pub(crate) async fn propose_install(
    bus: EventBus,
    registry: InstallRegistry,
    state_root: PathBuf,
    backend_raw: &str,
) -> Result<InstallState, ProposeRefusal> {
    let Some(backend) = AgentBackend::from_str_loose(backend_raw) else {
        return Err(ProposeRefusal::UnknownBackend(backend_raw.to_string()));
    };
    let Some(platform) = InstallPlatform::current() else {
        return Err(ProposeRefusal::NoLane {
            backend: backend.as_short_str().to_string(),
            platform: "unsupported",
        });
    };
    let Some(command) = install_command(&backend, platform) else {
        return Err(ProposeRefusal::NoLane {
            backend: backend.as_short_str().to_string(),
            platform: platform.as_str(),
        });
    };

    let current = registry.state(&backend);
    if current.is_in_flight() {
        return Ok(current);
    }

    match live_gate_decision().await {
        InstallGateDecision::Refused(detail) => {
            let state = InstallState::Refused {
                detail,
                finished_ms: unix_ms(),
            };
            registry.set(&backend, state.clone());
            Ok(state)
        }
        InstallGateDecision::Wall => {
            let approval_id = crate::event::next_approval_id();
            let state = InstallState::WaitingApproval { approval_id };
            registry.set(&backend, state.clone());
            if let Ok(mut set) = pending_install_approvals().lock() {
                set.insert(approval_id);
            }
            tokio::spawn(install_gate_waiter(
                bus,
                registry,
                state_root,
                backend,
                command,
                approval_id,
            ));
            Ok(state)
        }
        InstallGateDecision::Run => {
            registry.set(&backend, InstallState::Running);
            tokio::spawn(run_install(registry.clone(), state_root, backend, command));
            Ok(InstallState::Running)
        }
    }
}

/// Wait for the rail decision on `approval_id`, then execute or record the
/// decline. Subscribes BEFORE announcing (an instant resolution must find
/// the waiter listening) and always clears the pending entry + emits
/// `ApprovalResolved`, so a dead prompt never leaves a zombie on the rail.
async fn install_gate_waiter(
    bus: EventBus,
    registry: InstallRegistry,
    state_root: PathBuf,
    backend: AgentBackend,
    command: &'static str,
    approval_id: u64,
) {
    let mut events = bus.subscribe();
    bus.send(AppEvent::ApprovalRequired {
        session_id: None,
        id: approval_id,
        command_preview: install_approval_preview(&backend, command),
        category: ActionCategory::CommandExec,
    });

    let resolve = |action: &str| {
        // Record the consumption BEFORE dropping the pending entry: the
        // session supervisor observes the same decision broadcast
        // concurrently and checks `install_pending` then the
        // recently-resolved register — this order leaves no window where
        // the id is in neither and a first, legitimate decision gets
        // misreported as stale (routing.rs `resolve_approval`).
        crate::event::record_approval_resolved(approval_id);
        if let Ok(mut set) = pending_install_approvals().lock() {
            set.remove(&approval_id);
        }
        bus.send(AppEvent::ApprovalResolved {
            session_id: None,
            id: approval_id,
            action: action.to_string(),
        });
    };

    let deadline = tokio::time::Instant::now() + INSTALL_APPROVAL_WAIT;
    let verdict: Result<(), String> = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Err(_) => {
                break Err(format!(
                    "No approval arrived within {}s — the installer was not run. Propose it \
                     again when you are ready to approve.",
                    INSTALL_APPROVAL_WAIT.as_secs()
                ));
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                break Err(
                    "The approval channel closed before a decision arrived — the \
                           installer was not run."
                        .to_string(),
                );
            }
            Ok(Ok(AppEvent::ControlCommand(msg))) => match msg {
                // Approval ids are process-wide — match on id alone (the
                // same contract as ask_user and the live-audio gate).
                ControlMsg::Approve { id, .. } | ControlMsg::ApproveAll { id, .. }
                    if id == approval_id =>
                {
                    break Ok(());
                }
                ControlMsg::Deny { id, .. } if id == approval_id => {
                    break Err("You declined the install — nothing was run.".to_string());
                }
                ControlMsg::Skip { id, .. } if id == approval_id => {
                    break Err("You skipped the install — nothing was run.".to_string());
                }
                _ => continue,
            },
            Ok(Ok(_)) => continue,
        }
    };

    match verdict {
        Ok(()) => {
            resolve("approve");
            registry.set(&backend, InstallState::Running);
            run_install(registry, state_root, backend, command).await;
        }
        Err(detail) => {
            let action = if detail.starts_with("You declined") {
                "deny"
            } else if detail.starts_with("You skipped") {
                "skip"
            } else {
                "timeout"
            };
            resolve(action);
            registry.set(
                &backend,
                InstallState::Declined {
                    detail,
                    finished_ms: unix_ms(),
                },
            );
        }
    }
}

/// The runtime result line for nonce 1, decoded. `data` is either the
/// exec JSON (`exit_code`, `stdout_tail`, `stderr_tail`) or an
/// `"Error: …"` string from the runtime.
fn decode_install_result(stdout: &str) -> Option<(Option<i32>, String)> {
    for line in stdout.lines() {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if parsed.get("type").and_then(|v| v.as_str()) != Some("result")
            || parsed.get("nonce").and_then(|v| v.as_u64()) != Some(1)
        {
            continue;
        }
        let data = parsed.get("data").and_then(|v| v.as_str()).unwrap_or("");
        if let Ok(exec) = serde_json::from_str::<serde_json::Value>(data) {
            let exit_code = exec
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .map(|c| c as i32);
            let mut tail = String::new();
            for key in ["stdout_tail", "stderr_tail"] {
                if let Some(text) = exec.get(key).and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        if !tail.is_empty() {
                            tail.push('\n');
                        }
                        tail.push_str(text);
                    }
                }
            }
            if let Some(err) = exec.get("error").and_then(|v| v.as_str()) {
                if !tail.is_empty() {
                    tail.push('\n');
                }
                tail.push_str(err);
            }
            return Some((exit_code, tail));
        }
        // Non-JSON data: the runtime's own error string.
        return Some((None, data.to_string()));
    }
    None
}

fn bounded_tail(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= INSTALL_OUTPUT_TAIL_CHARS {
        return text.to_string();
    }
    chars[chars.len() - INSTALL_OUTPUT_TAIL_CHARS..]
        .iter()
        .collect()
}

/// Execute the announced command through the sandboxed runtime lane and
/// record the honest outcome. `state_root` is injected (hermeticity).
async fn run_install(
    registry: InstallRegistry,
    state_root: PathBuf,
    backend: AgentBackend,
    command: &'static str,
) {
    let log_dir = state_root.join("logs").join("installs").join(format!(
        "{}-{}",
        backend.as_short_str(),
        unix_ms()
    ));
    if let Err(err) = std::fs::create_dir_all(&log_dir) {
        registry.set(
            &backend,
            InstallState::Failed {
                detail: format!("could not create the install log dir: {err}"),
                exit_code: None,
                output_tail: String::new(),
                log_dir: None,
                finished_ms: unix_ms(),
            },
        );
        return;
    }

    let input = serde_json::json!({
        "commands": [{
            "function": "execAsAgent",
            "command": command,
            "nonce": 1,
            "timeout_ms": INSTALL_COMMAND_TIMEOUT_MS,
        }]
    })
    .to_string();

    let outcome =
        crate::agent_runner::run_install_batch(&input, &log_dir, INSTALL_BATCH_HARD_TIMEOUT).await;

    let finished_ms = unix_ms();
    let state = match outcome {
        Ok(output) => match decode_install_result(&output.stdout) {
            Some((Some(0), tail)) => InstallState::Succeeded {
                exit_code: 0,
                output_tail: bounded_tail(&tail),
                log_dir: log_dir.clone(),
                finished_ms,
            },
            Some((exit_code, tail)) => InstallState::Failed {
                detail: match exit_code {
                    Some(-3) => format!(
                        "the installer was killed after {}s without finishing",
                        INSTALL_COMMAND_TIMEOUT_MS / 1000
                    ),
                    Some(code) => format!("the installer exited with code {code}"),
                    None => "the runtime could not run the installer".to_string(),
                },
                exit_code,
                output_tail: bounded_tail(&tail),
                log_dir: Some(log_dir.clone()),
                finished_ms,
            },
            None => InstallState::Failed {
                detail: "the runtime returned no result for the install command".to_string(),
                exit_code: None,
                output_tail: bounded_tail(&output.stderr),
                log_dir: Some(log_dir.clone()),
                finished_ms,
            },
        },
        Err(err) => InstallState::Failed {
            detail: format!("could not spawn the sandboxed runtime: {err}"),
            exit_code: None,
            output_tail: String::new(),
            log_dir: Some(log_dir.clone()),
            finished_ms,
        },
    };
    registry.set(&backend, state);
    // No broadcast here: the dashboard's in-flight poll re-fetches
    // /api/external-agents through the explicit refresh lane until the
    // install reaches a terminal state, and the availability serving
    // cache (web_gateway::settings, the #823 lane) itself broadcasts
    // `ExternalAgentsChanged` when a re-probe observes the `installed`
    // flip — every open dashboard repaints without this executor knowing
    // about frontends. `ExternalAgentChanged` (singular) is deliberately
    // NOT reused — it means "the selected backend changed" and frontends
    // feed it into the backend picker.
}

// ---------------------------------------------------------------------------
// Served shape (rides each /api/external-agents row)
// ---------------------------------------------------------------------------

/// The `install` object served on each `/api/external-agents` row —
/// derived from the matrix (availability + command) and the registry
/// (state). The dashboard renders the button from `available`, shows
/// `command` verbatim, and narrates `state` honestly.
pub(crate) fn install_status_json(
    registry: &InstallRegistry,
    backend: &AgentBackend,
) -> serde_json::Value {
    let lane = InstallPlatform::current().and_then(|platform| install_command(backend, platform));
    let mut value = serde_json::json!({
        "available": lane.is_some(),
        "command": lane,
        "state": "idle",
    });
    let obj = value.as_object_mut().expect("install status is an object");
    match registry.state(backend) {
        InstallState::Idle => {}
        InstallState::WaitingApproval { approval_id } => {
            obj.insert("state".into(), "waiting_approval".into());
            obj.insert("approval_id".into(), approval_id.into());
        }
        InstallState::Running => {
            obj.insert("state".into(), "running".into());
        }
        InstallState::Succeeded {
            exit_code,
            output_tail,
            log_dir,
            finished_ms,
        } => {
            obj.insert("state".into(), "succeeded".into());
            obj.insert("exit_code".into(), exit_code.into());
            obj.insert("output_tail".into(), output_tail.into());
            obj.insert("log_dir".into(), log_dir.display().to_string().into());
            obj.insert("finished_ms".into(), finished_ms.into());
        }
        InstallState::Failed {
            detail,
            exit_code,
            output_tail,
            log_dir,
            finished_ms,
        } => {
            obj.insert("state".into(), "failed".into());
            obj.insert("detail".into(), detail.into());
            if let Some(code) = exit_code {
                obj.insert("exit_code".into(), code.into());
            }
            obj.insert("output_tail".into(), output_tail.into());
            if let Some(dir) = log_dir {
                obj.insert("log_dir".into(), dir.display().to_string().into());
            }
            obj.insert("finished_ms".into(), finished_ms.into());
        }
        InstallState::Declined {
            detail,
            finished_ms,
        } => {
            obj.insert("state".into(), "declined".into());
            obj.insert("detail".into(), detail.into());
            obj.insert("finished_ms".into(), finished_ms.into());
        }
        InstallState::Refused {
            detail,
            finished_ms,
        } => {
            obj.insert("state".into(), "refused".into());
            obj.insert("detail".into(), detail.into());
            obj.insert("finished_ms".into(), finished_ms.into());
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::AutonomyLevel;

    fn backends() -> [AgentBackend; 3] {
        [
            AgentBackend::ClaudeCode,
            AgentBackend::Codex,
            AgentBackend::Kimi,
        ]
    }

    const PLATFORMS: [InstallPlatform; 3] = [
        InstallPlatform::MacOs,
        InstallPlatform::Linux,
        InstallPlatform::Windows,
    ];

    /// Matrix shape: every supported backend has a lane on every
    /// platform, exactly one per cell, and every command is a single
    /// non-empty line (the approval wall shows it verbatim on one line;
    /// a newline would let copy smuggle a second command past review).
    #[test]
    fn matrix_covers_every_backend_platform_cell_exactly_once() {
        for backend in backends() {
            for platform in PLATFORMS {
                let cells: Vec<_> = INSTALL_MATRIX
                    .iter()
                    .filter(|lane| lane.backend == backend && lane.platform == platform)
                    .collect();
                assert_eq!(
                    cells.len(),
                    1,
                    "{}/{} must have exactly one matrix cell",
                    backend.as_short_str(),
                    platform.as_str()
                );
                let command = cells[0].command;
                assert!(!command.trim().is_empty());
                assert!(
                    !command.contains('\n') && !command.contains('\r'),
                    "install commands are single-line by contract"
                );
            }
        }
        // Pi deliberately has no lane on any platform.
        assert!(
            INSTALL_MATRIX
                .iter()
                .all(|lane| lane.backend != AgentBackend::Pi),
            "pi has no vendor install lane by decision — adding one belongs in the matrix comment"
        );
    }

    /// Matrix parity: the served install object equals the matrix cell
    /// for this platform — availability, command, both directions.
    #[test]
    fn served_install_set_matches_the_matrix() {
        let registry = InstallRegistry::default();
        let platform = InstallPlatform::current().expect("test hosts are supported platforms");
        for backend in backends() {
            let served = install_status_json(&registry, &backend);
            assert_eq!(served["available"], serde_json::json!(true));
            assert_eq!(
                served["command"].as_str(),
                install_command(&backend, platform),
                "{} serves its matrix command",
                backend.as_short_str()
            );
            assert_eq!(served["state"], "idle");
        }
        let pi = install_status_json(&registry, &AgentBackend::Pi);
        assert_eq!(pi["available"], serde_json::json!(false));
        assert!(pi["command"].is_null(), "no lane serves no command");
    }

    /// The announced-command copy: backend name, the exact command
    /// verbatim, and the plain terminal-session sentence.
    #[test]
    fn approval_preview_carries_backend_command_and_sentence() {
        let command = install_command(&AgentBackend::ClaudeCode, InstallPlatform::MacOs).unwrap();
        let preview = install_approval_preview(&AgentBackend::ClaudeCode, command);
        assert!(preview.contains("Install Claude Code"));
        assert!(
            preview.contains(command),
            "the exact command rides verbatim"
        );
        assert!(preview.contains("Intendant will run this in a terminal session on this machine."));
    }

    /// The CommandExec wall conventions, exhaustively: default (Medium)
    /// walls; Low walls; High and Full run; a deny rule anywhere in the
    /// shell-reachable set refuses without raising anything.
    #[test]
    fn gate_decision_follows_the_command_exec_wall_conventions() {
        let mut state = AutonomyState::default();
        assert_eq!(
            state.level,
            AutonomyLevel::Medium,
            "default level is Medium"
        );
        assert_eq!(install_gate_decision(&state), InstallGateDecision::Wall);

        state.level = AutonomyLevel::Low;
        assert_eq!(install_gate_decision(&state), InstallGateDecision::Wall);

        state.level = AutonomyLevel::High;
        assert_eq!(install_gate_decision(&state), InstallGateDecision::Run);

        state.level = AutonomyLevel::Full;
        assert_eq!(install_gate_decision(&state), InstallGateDecision::Run);

        // A deny rule on any shell-reachable effect refuses at every level.
        for level in [
            AutonomyLevel::Low,
            AutonomyLevel::Medium,
            AutonomyLevel::High,
            AutonomyLevel::Full,
        ] {
            let denied_rules = crate::autonomy::ApprovalConfig {
                destructive: ApprovalRule::Deny,
                ..crate::autonomy::ApprovalConfig::default()
            };
            let denied = AutonomyState::new(level, denied_rules);
            assert!(
                matches!(
                    install_gate_decision(&denied),
                    InstallGateDecision::Refused(_)
                ),
                "deny rule refuses at {level:?}"
            );
        }
    }

    /// The negative pin: an un-approved click executes nothing. Under the
    /// default (Medium) autonomy the proposal parks on the wall — the
    /// exact command rides the ApprovalRequired preview, the state is
    /// WaitingApproval, and no install log dir exists under the injected
    /// state root (the executor's first observable act). A Deny verb then
    /// lands Declined — still nothing on disk, and the pending id clears.
    #[tokio::test]
    async fn unapproved_click_executes_nothing_and_deny_declines() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let registry = InstallRegistry::default();
        let root = tempfile::tempdir().unwrap();

        let state = propose_install(
            bus.clone(),
            registry.clone(),
            root.path().to_path_buf(),
            "codex",
        )
        .await
        .expect("codex has a lane on every test platform");
        let InstallState::WaitingApproval { approval_id } = state else {
            panic!("default autonomy must wall the proposal, got {state:?}");
        };
        assert!(install_pending(approval_id), "the id registers as pending");

        // The wall announces the exact command + category command_exec.
        let announced = loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("ApprovalRequired must be announced")
            {
                Ok(AppEvent::ApprovalRequired {
                    session_id,
                    id,
                    command_preview,
                    category,
                }) if id == approval_id => {
                    assert_eq!(session_id, None, "install approvals are daemon-scoped");
                    assert_eq!(category, ActionCategory::CommandExec);
                    break command_preview;
                }
                Ok(_) => continue,
                Err(err) => panic!("bus closed: {err}"),
            }
        };
        let platform = InstallPlatform::current().unwrap();
        let command = install_command(&AgentBackend::Codex, platform).unwrap();
        assert!(
            announced.contains(command),
            "the preview carries the command verbatim"
        );

        // Nothing executed: the executor's first observable act (its log
        // dir) must not exist while the wall stands.
        let installs_dir = root.path().join("logs").join("installs");
        assert!(!installs_dir.exists(), "no execution before approval");
        assert_eq!(
            registry.state(&AgentBackend::Codex),
            InstallState::WaitingApproval { approval_id }
        );

        // Re-clicking while walled is idempotent — same pending approval.
        let again = propose_install(
            bus.clone(),
            registry.clone(),
            root.path().to_path_buf(),
            "codex",
        )
        .await
        .unwrap();
        assert_eq!(again, InstallState::WaitingApproval { approval_id });

        // Deny → Declined, still nothing on disk, pending id cleared,
        // ApprovalResolved emitted.
        bus.send(AppEvent::ControlCommand(ControlMsg::Deny {
            session_id: None,
            id: approval_id,
        }));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "deny must settle the proposal"
            );
            match registry.state(&AgentBackend::Codex) {
                InstallState::Declined { detail, .. } => {
                    assert!(detail.contains("declined"));
                    break;
                }
                InstallState::WaitingApproval { .. } => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                other => panic!("deny must land Declined, got {other:?}"),
            }
        }
        assert!(!installs_dir.exists(), "a declined install never executes");
        assert!(!install_pending(approval_id), "pending entry cleared");
        assert!(
            crate::event::approval_recently_resolved(approval_id),
            "the consumed id lands in the recently-resolved register, so a \
             duplicate decision reads as benign instead of stale"
        );
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("ApprovalResolved must be emitted")
            {
                Ok(AppEvent::ApprovalResolved { id, action, .. }) if id == approval_id => {
                    assert_eq!(action, "deny");
                    break;
                }
                Ok(_) => continue,
                Err(err) => panic!("bus closed: {err}"),
            }
        }
    }

    /// Unknown backends and lane-less backends refuse by name without
    /// touching the registry or the rail.
    #[tokio::test]
    async fn propose_refuses_unknown_and_laneless_backends_by_name() {
        let bus = EventBus::new();
        let registry = InstallRegistry::default();
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            propose_install(
                bus.clone(),
                registry.clone(),
                root.path().into(),
                "not-a-backend"
            )
            .await,
            Err(ProposeRefusal::UnknownBackend("not-a-backend".to_string()))
        );
        let pi = propose_install(bus, registry.clone(), root.path().into(), "pi").await;
        assert!(
            matches!(pi, Err(ProposeRefusal::NoLane { .. })),
            "pi has no matrix lane: {pi:?}"
        );
        assert_eq!(registry.state(&AgentBackend::Pi), InstallState::Idle);
    }

    /// The dashboard bundle's install lane, pinned daemon-side (the #791
    /// `spa_signin_cards_preflight_missing_clis` pattern): the Vault card
    /// component and its propose call, the waiting-on-the-rail and
    /// PATH-footgun honesty copy, and the unfueled empty-state block.
    #[test]
    fn spa_install_lane_renders_from_the_matrix() {
        let app = include_str!("../../../static/app.html");
        for needle in [
            "function agentInstallSection",
            "api_external_agent_install",
            "the exact command is on the approval rail",
            "The daemon inherited its PATH at launch",
            "Intendant can run its official installer here, with your approval first.",
            "log-empty-install",
        ] {
            assert!(
                app.contains(needle),
                "the dashboard bundle lost the install lane: {needle}"
            );
        }
    }

    /// Hosted-unreachability composition (the skills S3/S4 precedent):
    /// the proposal route is Settings-grade — its IAM permission id is
    /// `settings.manage` on both the HTTP row and the tunnel twin — and
    /// the immutable hosted floor `role:none` carries no permissions at
    /// all, so hosted provenance can never reach the button's action.
    #[test]
    fn install_route_is_settings_grade_and_hosted_floor_has_no_permissions() {
        let route = crate::gateway_routes::ROUTES
            .iter()
            .find(|route| {
                matches!(
                    route.handler,
                    crate::gateway_routes::RouteHandlerId::ExternalAgentInstall
                )
            })
            .expect("the install route is declared");
        let op = route
            .tunnel_operation()
            .expect("the install route derives an IAM operation");
        assert_eq!(
            crate::access::iam::operation_permission_id(op),
            "settings.manage"
        );

        let roles = crate::access::iam::builtin_role_templates();
        let none = roles
            .iter()
            .find(|role| role.id == "role:none")
            .expect("the hosted floor role exists");
        assert!(
            none.permissions.is_empty(),
            "role:none must stay permissionless — hosted provenance never reaches Settings"
        );
    }

    /// The runtime result decoding: exec JSON success/failure shapes and
    /// the runtime's own error-string shape.
    #[test]
    fn decode_install_result_reads_the_runtime_protocol() {
        let ok = r#"{"type":"result","nonce":1,"data":"{\"nonce\":1,\"pid\":7,\"exit_code\":0,\"stdout_tail\":\"installed\",\"stderr_tail\":\"\"}"}"#;
        assert_eq!(
            decode_install_result(ok),
            Some((Some(0), "installed".to_string()))
        );

        let failed = r#"{"type":"result","nonce":1,"data":"{\"nonce\":1,\"pid\":7,\"exit_code\":127,\"stdout_tail\":\"\",\"stderr_tail\":\"curl: not found\"}"}"#;
        assert_eq!(
            decode_install_result(failed),
            Some((Some(127), "curl: not found".to_string()))
        );

        let error = r#"{"type":"result","nonce":1,"data":"Error: Process error"}"#;
        assert_eq!(
            decode_install_result(error),
            Some((None, "Error: Process error".to_string()))
        );

        assert_eq!(decode_install_result("not json\n"), None);
    }
}

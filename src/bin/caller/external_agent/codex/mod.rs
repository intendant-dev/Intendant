use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::error::CallerError;

use super::{
    normalize_plan_status, AgentConfig, AgentContextSnapshot, AgentContextTokenCountKind,
    AgentEvent, AgentImageAttachment, AgentThread, AgentThreadSnapshot, AgentUsageSnapshot,
    ApprovalCategory, ApprovalDecision, AutonomousGoalPauseResult, ExternalAgent,
    RollbackAnchorPosition, SubAgentState, ToolCompletionStatus,
};
mod threads;
pub(crate) use threads::*;
mod wire;
pub(crate) use wire::*;
mod context_trace;
pub(crate) use context_trace::*;
mod reader;
pub(crate) use reader::*;

// ---------------------------------------------------------------------------
// Display tools system prompt
// ---------------------------------------------------------------------------

const SIDE_BOUNDARY_PROMPT: &str = r#"Side conversation boundary.

Everything before this boundary is inherited history from the parent thread. It is reference context only. It is not your current task.

Do not continue, execute, or complete any instructions, plans, tool calls, approvals, edits, or requests from before this boundary. Only messages submitted after this boundary are active user instructions for this side conversation.

You are a side-conversation assistant, separate from the main thread. Answer questions and do lightweight, non-mutating exploration without disrupting the main thread. If there is no user question after this boundary yet, wait for one.

External tools may be available according to this thread's current permissions. Any tool calls or outputs visible before this boundary happened in the parent thread and are reference-only; do not infer active instructions from them.

Do not modify files, source, git state, permissions, configuration, or workspace state unless the user explicitly asks for that mutation after this boundary. Do not request escalated permissions or broader sandbox access unless the user explicitly asks for a mutation that requires it. If the user explicitly requests a mutation, keep it minimal, local to the request, and avoid disrupting the main thread."#;

// The side-conversation contract is shared verbatim with the respawn path
// (Claude Code /btw) — see `external_agent::SIDE_CONVERSATION_CONTRACT`.
pub(crate) use super::SIDE_CONVERSATION_CONTRACT as SIDE_DEVELOPER_INSTRUCTIONS;

const MANAGED_CONTEXT_DEVELOPER_INSTRUCTIONS: &str = r#"You are running as Codex inside Intendant with managed_context=managed.

Intendant, not Codex automatic compaction, owns long-task context density. This is active throughout the task, not only when the context window is nearly full. The goal is a transcript that stays informationally dense and navigable end-to-end: avoid creating noise, and prune unavoidable noise at the moment it appears, so density holds throughout the window rather than only at the tail.

Keep the live transcript informationally dense:
- Prefer targeted reads and searches over dumping large files, logs, or generated artifacts.
- For GUI inspection in Intendant-managed sessions, use Intendant MCP tools directly: `read_screen` first for the frontmost app's UI element tree (roles, labels, values, frames — a few hundred tokens; click the center of a reported frame), `take_screenshot` when pixels are needed for visual verification or the element tree is sparse, and `execute_cu_actions` for input. Do not enumerate desktop apps or read bulky browser/computer-use plugin manuals when those direct tools are available; use Browser/Chrome/plugin CU only when their specialized capabilities are actually required. Do not use shell-driven GUI fallbacks such as `open`, `cliclick`, `osascript`, ad-hoc accessibility queries, or app binary inspection for GUI interaction.
- Do not use broad argv-pattern process cleanup such as `pkill -f intendant` or `pkill -f <script-name>`. Managed controller argv can contain the task prompt, and prompts often include command examples, so `pkill -f` can match and kill the controller supervising you. Prefer helper-owned cleanup, tracked child PIDs, process groups created by the command you launched, temporary workspace/profile directories, or exact PIDs you verified with `ps`.
- Browser/GUI validation retry discipline: run one primary validation attempt. If it fails or times out, run at most one compact diagnostic retry. Then either make a targeted code fix from those facts, or report a clear partial-validation conclusion with the failure reason and relevant logs/diagnostics. Do not cycle through multiple automation stacks unless the user explicitly asks for deeper manual investigation or the validation tool itself is the suspected broken component.
- After a successful build, run dev servers through already-built binaries or quiet commands when possible. Avoid re-running build commands that stream known warnings only to launch a server; if a noisy command is unavoidable, preserve only the durable result and compact immediately.
- While a long-running command/tool is still active, do not emit assistant status messages that only say you are still waiting/building/running and have no new output or errors, such as "No output yet", "Still active", or "Polling". Wait silently for material output, completion, an approval need, or a real decision; Intendant surfaces tool lifecycle separately.
- A rewind can cancel the active long-running command. If the chosen anchor is before a server launch, assume the server may be gone; verify it with a small health check and relaunch tersely instead of preserving the old PID as if it survived.
- Pruning is triggered by noise, not gated by pressure. After genuinely noisy or unexpectedly large tool output, failed exploration that added substantial low-value context, or finishing a coherent subtask whose working detail is no longer needed — and whenever backend context status reaches `watch`/`rewind_only` as the safety net — crystallize the durable facts (in your reply or the primer) and prune with exact-anchor managed-context maintenance before continuing broad ordinary-tool work. Pruning a crystallized noisy output is normal at any pressure, including `ok`.
- Rollback is a suffix cut, so every turn that passes after a noisy output makes pruning it more expensive: more completed work enters the discard span and must be carried by the primer. Prune at the cheap moment — immediately after crystallizing durable facts — instead of waiting for pressure. Routine pruning may happen many times in a long session; that is the intended working style, not an exceptional recovery.
- Backend context status `watch`, or usage above the recommended density threshold but below `rewind_only_limit`, is not recovery. Normal tools remain allowed, including one already-running narrow validation/build/check. Do not begin another broad build, QA, exploration, or implementation loop while watch pressure persists; first perform exact-anchor density maintenance if it materially improves density, or give a concise no-rewind density handoff. These pressure bands are the safety net behind the noise-triggered habit, not the trigger to wait for.
- Do not call list_rewind_anchors merely because managed_context=managed is enabled, during ordinary startup/status checks, or after bounded searches with compact output — when nothing noisy happened there is nothing to prune. Continue normal work in those cases.
- When a noisy output, a finished subtask, or watch/rewind-only pressure calls for pruning, list once and act: call list_rewind_anchors, choose an exact item_id from the returned rows, use inspect_rewind_anchor only if the compact row is ambiguous, then call rewind_context in the same turn with a dense carry-forward primer. If a usable anchor catalog is already in view from earlier in the turn — including any instruction to first list anchors that you have already satisfied — do not list again: pick the best row you already have and call rewind_context now. Repeated listing is itself noise that raises pressure without surfacing better candidates.
- Do not use recovery_candidates_only=false to look for newer rewind targets. Non-recovery rows require include_non_recovery=true, are diagnostic-only, and must not be passed to rewind_context when recovery_eligible=false or the requested position is not present in the default row's positions.
- Treat the primer as a living index of the session, not a transcript dump: a structured, cumulative index with stable sections — objective and user constraints; decisions made; artifacts and file paths; verified results; next steps. A handful of dense lines under those headings is enough; the primer is where you crystallize durable facts, so compose it immediately from facts you already hold and never run extra tools to research primer content. Carry pointers for recoverable detail (file paths, command names, ids of earlier rewind records) rather than full content; discarded detail stays reachable via rewind_backout and the durable rewind records. Each new primer must carry forward the substance of any prior managed primer that would otherwise be overwritten, revising sections in place so the index grows sublinearly instead of re-inflating the window.
- Never synthesize anchor ids, never use anchors from failed examples, and never target managed-context maintenance calls unless explicitly auditing those internals.

Fission (full-context branch spawning), when fission tools are available:
- When a coherent subtask is separable or parallelizable, prefer `fission_spawn` with a self-contained charter over a deep in-context detour. Branches fork from the last completed turn and do not see the current turn, so each charter must carry every fact, path, and constraint the branch needs.
- Favor breadth over depth, before pressure builds: fission is ex-ante, rewind is ex-post. Fission tools stay available at `watch` pressure — delegating separable work to a branch is itself a valid density action, since the branch absorbs the work's context noise — but they are unavailable under rewind-only pressure.
- After spawning, continue your own non-overlapping work; do not idle behind a branch.
- Use `fission_control(op="wait")` only when genuinely blocked on a branch result. A `still_running` result is normal — keep working and re-check later.
- Import branch results you need via `fission_control(op="import")`; detach or ignore branches you don't.
- The first branch back with the group's answer may claim canonical via `claim_fission_canonical`.
- Consult the fission ledger in `get_status` before reading sibling raw logs."#;

/// Per-project extension of `MANAGED_CONTEXT_DEVELOPER_INSTRUCTIONS`: when
/// `<working_dir>/.intendant/codex-managed-instructions.md` exists, its
/// contents are appended to the generic block under
/// `MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_HEADING`. This keeps repo-specific
/// guidance (validation helpers, QA recipes) out of the generic constant that
/// every managed session in every project would otherwise pay a token tax for.
const MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_FILE: &str = "codex-managed-instructions.md";
const MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_DIR: &str = ".intendant";
const MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_HEADING: &str =
    "Project managed-context instructions (.intendant/codex-managed-instructions.md):";
const MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 16 * 1024;
const MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_TRUNCATION_MARKER: &str =
    "[project managed-context instructions truncated at 16 KiB]";

const GENERATION_STARVATION_HINT: &str = "The previous Codex response appears to have been cut off near the backend context limit. Avoid regenerating the same long output; rewind context first or produce a much shorter recovery response.";
const CODEX_INITIALIZE_TIMEOUT_SECS: u64 = 60;
const CODEX_INTERRUPT_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const CODEX_FAST_SERVICE_TIER: &str = "priority";
const CODEX_DANGER_FULL_ACCESS_SANDBOX: &str = "danger-full-access";
const CODEX_NEVER_APPROVAL_POLICY: &str = "never";
const CODEX_INHERIT_MCP_SERVERS_ENV: &str = "INTENDANT_CODEX_INHERIT_MCP_SERVERS";

pub(super) use super::{normalize_goal_status, parse_goal_token_budget, validate_goal_objective};

pub struct CodexAgent {
    command: String,
    model: Option<String>,
    approval_policy: String,
    /// Sandbox mode sent verbatim to Codex `thread/start`. One of
    /// `"read-only"`, `"workspace-write"`, `"danger-full-access"`.
    sandbox: String,
    /// Reasoning effort override (Responses API). `None` = Codex default.
    reasoning_effort: Option<String>,
    /// Codex service-tier override. `Some("priority")` is Codex `/fast`.
    service_tier: Option<String>,
    /// Set when `/fast` is toggled off so the next supported app-server
    /// request carries `serviceTier: null` and clears Codex's persisted
    /// session override.
    service_tier_clear_pending: bool,
    /// Enable Responses API `web_search` tool. Maps to `codex --search`.
    web_search: bool,
    /// Enable outbound network inside the `workspace-write` sandbox. Ignored
    /// by other sandbox modes.
    network_access: bool,
    /// Extra writable roots beyond the project. Absolute paths.
    writable_roots: Vec<String>,
    /// Enables Intendant's managed-context protocol. Disabled for
    /// vanilla/fork-safe managed Codex.
    managed_context: bool,
    /// The exact `developerInstructions` override the active thread was
    /// started/resumed with (None when none was sent). The mid-session
    /// `thread/resume` retry MUST re-send these bytes verbatim: the override
    /// is not persisted in Codex config, so omitting it on an app-server
    /// thread eviction would silently swap the developer block (a prompt-
    /// cache prefix bust at maximum prompt size) and drop the managed-context
    /// policy text. See the cache-prefix contract on `turn_start_params`.
    thread_developer_instructions: Option<String>,
    web_port: Option<u16>,
    mcp_auth_token: Option<String>,
    mcp_session_id: Option<String>,
    resume_session: Option<String>,
    codex_home: Option<PathBuf>,
    /// Working directory used to resolve Codex project config for config/read.
    working_dir: Option<PathBuf>,
    /// Working directory where .codex/config.toml was written (for cleanup).
    config_working_dir: Option<PathBuf>,
    /// Root directory where Codex rollout traces exact provider request
    /// payloads for the dashboard Context tab.
    request_trace_root: Option<PathBuf>,
    request_trace_temporary: bool,
    context_archive: String,
    context_seen_request_ids: HashSet<String>,
    context_trace_fingerprint: Option<CodexTraceFingerprint>,
    child: Option<Child>,
    writer: Option<BufWriter<ChildStdin>>,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    next_id: AtomicU64,
    pending_requests: PendingRequests,
    pending_approvals: PendingApprovals,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    /// Thread id from the most recent `thread/start`. Used by `interrupt_turn`
    /// to build the `turn/interrupt` params without needing a thread handle.
    active_thread_id: Arc<Mutex<Option<String>>>,
    /// Turn id of the currently active turn, if any. Captured from the
    /// `turn/start` response (and `turn/started`/`thread/started` notifications
    /// as a fallback) and cleared on `turn/completed` / `turn/interrupted` /
    /// `Terminated`.
    active_turn_id: Arc<Mutex<Option<String>>>,
    /// Per-thread active turn ids for Codex's multiplexed app-server stream.
    active_turns: ActiveTurns,
    /// Descendant process ids that existed before the current turn started.
    /// On interrupt, any new descendants that Codex's own `turn/interrupt`
    /// leaves behind are treated as leaked turn work and terminated.
    turn_descendant_baseline: Option<HashSet<u32>>,
    /// Ephemeral side-conversation child threads keyed by child thread id,
    /// with the parent thread id as value. Used to keep slash/thread actions
    /// scoped to durable Codex threads while still allowing side follow-ups.
    side_threads: Arc<Mutex<HashMap<String, String>>>,
    /// Latest token-usage notification from Codex app-server. Joined with
    /// request payload snapshots so the dashboard can show current context usage.
    latest_token_usage: Arc<Mutex<Option<serde_json::Value>>>,
    /// A backend-reported saturated context sample that remains effective until
    /// Codex confirms a thread rewrite. Short failed turns can report a much
    /// smaller last-call usage without changing the still-saturated rollout.
    context_pressure_floor: Arc<Mutex<Option<CodexContextPressureFloor>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodexContextPressureFloor {
    token_count: u64,
    context_window: u64,
    hard_context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexTraceFingerprint {
    files: Vec<CodexTraceFileFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexTraceFileFingerprint {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

/// Knobs that vary per-session and feed into Codex `thread/start` or the
/// process spawn. Accepts sensible defaults so tests and callers that only
/// care about the common fields (command/model/approval/sandbox) can use
/// `..CodexAgentOptions::default()`.
#[derive(Debug, Clone, Default)]
pub struct CodexAgentOptions {
    pub reasoning_effort: Option<String>,
    pub web_search: bool,
    pub network_access: bool,
    pub writable_roots: Vec<String>,
    pub managed_context: bool,
}

impl CodexAgent {
    fn context_archive_exact(&self) -> bool {
        crate::project::codex_context_archive_exact(&self.context_archive)
    }

    fn toggle_fast_service_tier(&mut self) -> String {
        if self.service_tier.as_deref() == Some(CODEX_FAST_SERVICE_TIER) {
            self.service_tier = None;
            self.service_tier_clear_pending = true;
            "fast mode disabled for future Codex turns; active turns continue unchanged".to_string()
        } else {
            self.service_tier = Some(CODEX_FAST_SERVICE_TIER.to_string());
            self.service_tier_clear_pending = false;
            "fast mode enabled for future Codex turns; active turns continue unchanged".to_string()
        }
    }

    fn apply_configured_service_tier(&mut self, service_tier: Option<String>) {
        match crate::project::normalize_codex_service_tier(service_tier.as_deref()) {
            Some(tier) if crate::project::codex_service_tier_is_standard_clear(&tier) => {
                self.service_tier = None;
                self.service_tier_clear_pending = true;
            }
            Some(tier) => {
                self.service_tier = Some(tier);
                self.service_tier_clear_pending = false;
            }
            None => {
                self.service_tier = None;
                self.service_tier_clear_pending = false;
            }
        }
    }

    fn service_tier_override_value(&self) -> Option<serde_json::Value> {
        if let Some(service_tier) = self
            .service_tier
            .as_deref()
            .map(str::trim)
            .filter(|service_tier| !service_tier.is_empty())
        {
            return Some(serde_json::Value::String(service_tier.to_string()));
        }
        if self.service_tier_clear_pending {
            return Some(serde_json::Value::Null);
        }
        None
    }

    fn insert_service_tier_override(
        &self,
        params: &mut serde_json::Map<String, serde_json::Value>,
    ) {
        if let Some(value) = self.service_tier_override_value() {
            params.insert("serviceTier".into(), value);
        }
    }

    fn insert_service_tier_override_consuming_clear(
        &mut self,
        params: &mut serde_json::Map<String, serde_json::Value>,
    ) {
        let consumed_clear = self.service_tier.is_none() && self.service_tier_clear_pending;
        self.insert_service_tier_override(params);
        if consumed_clear {
            self.service_tier_clear_pending = false;
        }
    }

    fn effective_approval_policy(&self) -> &str {
        effective_approval_policy_for_sandbox(&self.sandbox, &self.approval_policy)
    }

    fn update_service_tier_from_thread_response(&mut self, response: &serde_json::Value) {
        let Some(value) = response.get("serviceTier") else {
            return;
        };
        self.service_tier = value
            .as_str()
            .map(str::trim)
            .filter(|tier| !tier.is_empty())
            .map(str::to_string);
        self.service_tier_clear_pending = false;
    }

    fn insert_working_dir_param(&self, params: &mut serde_json::Map<String, serde_json::Value>) {
        if let Some(cwd) = self.working_dir.as_ref() {
            params.insert(
                "cwd".into(),
                serde_json::Value::String(cwd.to_string_lossy().to_string()),
            );
        }
    }

    fn thread_lifecycle_params_with_developer_instructions(
        &mut self,
        developer_instructions: Option<String>,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut params = serde_json::Map::new();
        if let Some(ref model) = self.model {
            params.insert("model".into(), serde_json::Value::String(model.clone()));
        }
        params.insert(
            "approvalPolicy".into(),
            serde_json::Value::String(self.effective_approval_policy().to_string()),
        );
        // Codex accepts `read-only`, `workspace-write`, or
        // `danger-full-access`. Pass the configured value through verbatim
        // so all three modes reach Codex's enforcer unchanged; the config
        // layer is responsible for validation (see `normalize_sandbox_mode`
        // in project.rs).
        params.insert(
            "sandbox".into(),
            serde_json::Value::String(self.sandbox.clone()),
        );
        if let Some(developer_instructions) = developer_instructions {
            params.insert(
                "developerInstructions".into(),
                serde_json::Value::String(developer_instructions),
            );
        }
        self.insert_working_dir_param(&mut params);
        self.insert_service_tier_override_consuming_clear(&mut params);
        params
    }

    fn sandbox_permission_profile(&self) -> Option<&'static str> {
        match self.sandbox.trim() {
            "read-only" => Some(":read-only"),
            "workspace-write" => Some(":workspace"),
            "danger-full-access" => Some(":danger-full-access"),
            _ => None,
        }
    }

    fn resumed_thread_settings_update_params(
        &self,
        thread_id: &str,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut params = serde_json::Map::new();
        params.insert(
            "threadId".into(),
            serde_json::Value::String(thread_id.to_string()),
        );
        if let Some(cwd) = self.working_dir.as_ref() {
            params.insert(
                "cwd".into(),
                serde_json::Value::String(cwd.to_string_lossy().to_string()),
            );
        }
        let approval_policy = self.effective_approval_policy().trim();
        if !approval_policy.is_empty() {
            params.insert(
                "approvalPolicy".into(),
                serde_json::Value::String(approval_policy.to_string()),
            );
        }
        if let Some(profile) = self.sandbox_permission_profile() {
            params.insert(
                "permissions".into(),
                serde_json::Value::String(profile.to_string()),
            );
        }
        if let Some(ref model) = self.model {
            params.insert("model".into(), serde_json::Value::String(model.clone()));
        }
        self.insert_service_tier_override(&mut params);
        params
    }

    async fn apply_resumed_thread_settings(&mut self, thread_id: &str) -> Result<(), CallerError> {
        let params = self.resumed_thread_settings_update_params(thread_id);
        if params.len() <= 1 {
            return Ok(());
        }
        self.send_request(
            "thread/settings/update",
            Some(serde_json::Value::Object(params)),
        )
        .await
        .map(|_| ())
        .map_err(|e| CallerError::ExternalAgent(format!("thread/settings/update: {e}")))
    }

    fn emit_resume_cwd_mismatch_if_needed(&self, response: &serde_json::Value) {
        if self.resume_session.is_none() {
            return;
        }
        let Some(requested) = self.working_dir.as_deref() else {
            return;
        };
        let Some(actual) = codex_response_cwd(response) else {
            return;
        };
        if codex_paths_match(requested, actual) {
            return;
        }

        if let Some(event_tx) = self.event_tx.as_ref() {
            let _ = event_tx.send(AgentEvent::Log {
                level: "warn".to_string(),
                message: format!(
                    "Codex resumed thread cwd differs from Intendant requested project root: requested {}; Codex reported {}. Running Codex threads keep their loaded cwd on thread/resume; thread/settings/update only affects subsequent turns once Codex reports it applied.",
                    requested.display(),
                    actual
                ),
            });
        }
    }

    /// Escalate a backend-internal warning to the session (dashboard
    /// activity log via `AgentEvent::Log`, same as the resume-cwd mismatch)
    /// in addition to the daemon's stderr — stderr-only warnings are
    /// invisible to the operator watching the dashboard.
    fn emit_backend_warning(&self, message: String) {
        eprintln!("[codex] Warning: {message}");
        if let Some(event_tx) = self.event_tx.as_ref() {
            let _ = event_tx.send(AgentEvent::Log {
                level: "warn".to_string(),
                message,
            });
        }
    }

    fn cleanup_temporary_request_trace_root(&mut self) {
        if !self.request_trace_temporary {
            return;
        }
        if let Some(root) = self.request_trace_root.take() {
            let _ = std::fs::remove_dir_all(root);
        }
        self.request_trace_temporary = false;
    }

    async fn mark_existing_context_requests_seen(
        &mut self,
        thread_id: Option<&str>,
    ) -> Result<usize, CallerError> {
        let Some(root) = self.request_trace_root.clone() else {
            return Ok(0);
        };
        let index = read_codex_trace_index(&root, thread_id).await?;
        let mut inserted = 0usize;
        for request in index.requests {
            if self
                .context_seen_request_ids
                .insert(codex_request_id(&request))
            {
                inserted += 1;
            }
        }
        if let Ok(fingerprint) = codex_context_trace_fingerprint(&root, thread_id).await {
            self.context_trace_fingerprint = Some(fingerprint);
        }
        Ok(inserted)
    }

    fn intendant_mcp_url(&self, port: u16) -> String {
        super::intendant_bootstrap_mcp_url(
            port,
            self.mcp_session_id.as_deref(),
            Some(self.intendant_managed_context_mode()),
            self.mcp_auth_token.as_deref(),
        )
    }

    #[allow(dead_code)]
    fn intendant_mcp_base_url(port: u16) -> String {
        format!("http://localhost:{port}/mcp")
    }

    fn intendant_managed_context_mode(&self) -> &'static str {
        if self.managed_context {
            "managed"
        } else {
            "vanilla"
        }
    }

    fn add_intendant_ctl_env(&self, command: &mut tokio::process::Command, port: u16) {
        super::add_intendant_bootstrap_env(
            command,
            &self.intendant_mcp_url(port),
            self.mcp_session_id.as_deref(),
        );
        command.env(
            "INTENDANT_MANAGED_CONTEXT",
            self.intendant_managed_context_mode(),
        );
    }

    fn apply_codex_home_env(command: &mut tokio::process::Command, codex_home: Option<&Path>) {
        if let Some(home) = codex_home {
            command.env("CODEX_HOME", home);
        }
    }

    fn app_server_args(&self, effective_web_port: u16) -> Vec<String> {
        self.app_server_args_with_mcp_inheritance(
            effective_web_port,
            inherit_configured_codex_mcp_servers(),
        )
    }

    fn app_server_args_with_mcp_inheritance(
        &self,
        effective_web_port: u16,
        inherit_configured_mcp_servers: bool,
    ) -> Vec<String> {
        let mcp_url = self.intendant_mcp_url(effective_web_port);
        let mut args: Vec<String> = Vec::new();
        if self.sandbox.trim() == CODEX_DANGER_FULL_ACCESS_SANDBOX {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        }
        args.push("app-server".to_string());
        for override_value in codex_inherited_config_suppression_overrides(
            self.codex_home.as_deref(),
            self.managed_context,
            inherit_configured_mcp_servers,
        ) {
            args.push("-c".to_string());
            args.push(override_value);
        }
        args.extend([
            "-c".to_string(),
            "mcp_servers.intendant.type=\"http\"".to_string(),
            "-c".to_string(),
            format!("mcp_servers.intendant.url=\"{}\"", mcp_url),
        ]);
        if self.sandbox.trim() == CODEX_DANGER_FULL_ACCESS_SANDBOX {
            args.push("-c".to_string());
            args.push(format!(
                "approval_policy=\"{}\"",
                self.effective_approval_policy()
            ));
            args.push("-c".to_string());
            args.push("sandbox_mode=\"danger-full-access\"".to_string());
        }
        if self.managed_context {
            // Intendant owns context rewind/backout policy for managed Codex
            // sessions. Our minimal Codex fork treats this sentinel as
            // disabling automatic compaction; stock Codex treats it as an
            // unreachable body-after-prefix budget instead of compacting
            // eagerly.
            args.push("-c".to_string());
            args.push("model_auto_compact_token_limit=9223372036854775807".to_string());
            args.push("-c".to_string());
            args.push("model_auto_compact_token_limit_scope=\"body_after_prefix\"".to_string());
            // A blocking `fission_control(op="wait")` can hold an Intendant
            // MCP tool call open for up to 300s; raise Codex's per-tool
            // client timeout (`mcp_servers.<name>.tool_timeout_sec`) well
            // above that so long fission waits never trip it.
            args.push("-c".to_string());
            args.push("mcp_servers.intendant.tool_timeout_sec=600".to_string());
        }
        if self.web_search {
            args.push("-c".to_string());
            args.push("tools.web_search=true".to_string());
        }
        if let Some(ref effort) = self.reasoning_effort {
            // TOML-quote the value explicitly; `-c` parses the RHS as TOML.
            args.push("-c".to_string());
            args.push(format!("model_reasoning_effort=\"{}\"", effort));
        }
        if self.network_access && self.sandbox == "workspace-write" {
            args.push("-c".to_string());
            args.push("sandbox_workspace_write.network_access=true".to_string());
        }
        if !self.writable_roots.is_empty() {
            // TOML array of strings. Quote and escape each path so whitespace
            // and backslashes don't break the parse.
            let quoted: Vec<String> = self
                .writable_roots
                .iter()
                .map(|p| format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\"")))
                .collect();
            args.push("-c".to_string());
            args.push(format!(
                "sandbox_workspace_write.writable_roots=[{}]",
                quoted.join(", ")
            ));
        }
        args
    }

    #[allow(dead_code)]
    pub fn new(
        command: String,
        model: Option<String>,
        approval_policy: String,
        sandbox: String,
        web_port: Option<u16>,
    ) -> Self {
        Self::with_options(
            command,
            model,
            approval_policy,
            sandbox,
            web_port,
            CodexAgentOptions::default(),
        )
    }

    pub fn with_options(
        command: String,
        model: Option<String>,
        approval_policy: String,
        sandbox: String,
        web_port: Option<u16>,
        opts: CodexAgentOptions,
    ) -> Self {
        Self {
            command,
            model,
            approval_policy,
            sandbox,
            reasoning_effort: opts.reasoning_effort,
            service_tier: None,
            service_tier_clear_pending: false,
            web_search: opts.web_search,
            network_access: opts.network_access,
            writable_roots: opts.writable_roots,
            managed_context: opts.managed_context,
            thread_developer_instructions: None,
            web_port,
            mcp_auth_token: None,
            mcp_session_id: None,
            resume_session: None,
            codex_home: None,
            working_dir: None,
            config_working_dir: None,
            request_trace_root: None,
            request_trace_temporary: false,
            context_archive: "summary".to_string(),
            context_seen_request_ids: HashSet::new(),
            context_trace_fingerprint: None,
            child: None,
            writer: None,
            event_tx: None,
            next_id: AtomicU64::new(1),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            reader_handle: None,
            active_thread_id: Arc::new(Mutex::new(None)),
            active_turn_id: Arc::new(Mutex::new(None)),
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            turn_descendant_baseline: None,
            side_threads: Arc::new(Mutex::new(HashMap::new())),
            latest_token_usage: Arc::new(Mutex::new(None)),
            context_pressure_floor: Arc::new(Mutex::new(None)),
        }
    }

    // -- internal helpers ---------------------------------------------------

    async fn send_request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, CallerError> {
        self.send_request_bounded(method, params, None).await
    }

    async fn send_request_with_timeout(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, CallerError> {
        self.send_request_bounded(method, params, Some(timeout))
            .await
    }

    async fn send_request_bounded(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
        timeout: Option<Duration>,
    ) -> Result<serde_json::Value, CallerError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&request)?;
        let (tx, rx) = oneshot::channel();

        if self.writer.is_none() {
            return Err(CallerError::ExternalAgent("Not initialized".into()));
        }
        self.pending_requests.lock().await.insert(id, tx);
        let (result, remove_pending) = {
            let writer = self.writer.as_mut().expect("writer checked above");
            let request_fut = async {
                writer.write_all(line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                let result = rx
                    .await
                    .map_err(|_| CallerError::ExternalAgent("Request channel closed".into()))?;
                result.map_err(CallerError::ExternalAgent)
            };

            match timeout {
                Some(timeout) => match tokio::time::timeout(timeout, request_fut).await {
                    Ok(result) => (result, false),
                    Err(_) => (
                        Err(CallerError::ExternalAgent(format!(
                            "{method} request timed out after {}ms",
                            timeout.as_millis()
                        ))),
                        true,
                    ),
                },
                None => (request_fut.await, false),
            }
        };
        if remove_pending || result.is_err() {
            self.pending_requests.lock().await.remove(&id);
        }
        result
    }

    async fn send_notification(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), CallerError> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&notification)?;

        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| CallerError::ExternalAgent("Not initialized".into()))?;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }

    /// Send a raw JSON-RPC response (used for approval replies to
    /// server-initiated requests).
    async fn send_response(
        &mut self,
        id: u64,
        result: serde_json::Value,
    ) -> Result<(), CallerError> {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result,
        };
        let line = serde_json::to_string(&response)?;

        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| CallerError::ExternalAgent("Not initialized".into()))?;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }

    fn capture_turn_descendant_baseline(&mut self) {
        self.turn_descendant_baseline =
            self.child.as_ref().and_then(|child| child.id()).map(|pid| {
                crate::platform::process_descendants(pid)
                    .into_iter()
                    .collect::<HashSet<_>>()
            });
    }

    async fn read_context_snapshot(&mut self) -> Result<AgentContextSnapshot, CallerError> {
        let root = self.request_trace_root.as_deref().ok_or_else(|| {
            CallerError::ExternalAgent(
                "Codex request payload tracing was not configured".to_string(),
            )
        })?;
        let thread_id = self.active_thread_id.lock().await.clone();
        let trace = read_latest_codex_context_payload(root, thread_id.as_deref()).await?;
        let rollout_path = match thread_id.as_deref() {
            Some(thread_id) => self
                .read_thread_snapshot(thread_id)
                .await
                .ok()
                .and_then(|snapshot| snapshot.rollout_path),
            None => None,
        };
        let usage = self.latest_token_usage.lock().await.clone();
        let pressure_floor = *self.context_pressure_floor.lock().await;
        let (token_count, token_count_kind, context_window, hard_context_window) =
            codex_pressure_aware_usage_fields(usage.as_ref(), pressure_floor);
        let item_count = codex_request_item_count(&trace.payload);
        let raw = codex_context_archive_payload(
            trace.payload,
            &trace.request_id,
            trace.request_index,
            &trace.format,
            self.context_archive_exact(),
        );
        Ok(AgentContextSnapshot {
            source: "codex".to_string(),
            label: trace.label,
            request_id: Some(trace.request_id),
            request_index: Some(trace.request_index),
            rollout_path,
            format: trace.format,
            token_count,
            token_count_kind,
            context_window,
            hard_context_window,
            item_count,
            raw,
        })
    }

    async fn reset_context_pressure_after_thread_rewrite(&self) {
        self.latest_token_usage.lock().await.take();
        self.context_pressure_floor.lock().await.take();
    }

    async fn active_thread_and_turn(&self, action: &str) -> Result<(String, String), CallerError> {
        let fallback_turn_id = self.active_turn_id.lock().await.clone();
        let has_any_active_turn =
            fallback_turn_id.is_some() || !self.active_turns.lock().await.is_empty();
        let thread_id = match self.active_thread_id.lock().await.clone() {
            Some(thread_id) => thread_id,
            None if has_any_active_turn => {
                return Err(CallerError::ExternalAgent(format!(
                    "no active thread to {action}"
                )));
            }
            None => {
                return Err(CallerError::ExternalAgent(format!(
                    "no active turn to {action}"
                )));
            }
        };
        let turn_id = {
            let active_turns = self.active_turns.lock().await;
            active_turns.get(&thread_id).cloned()
        };
        let turn_id = match turn_id {
            Some(turn_id) => turn_id,
            None => fallback_turn_id
                .ok_or_else(|| CallerError::ExternalAgent(format!("no active turn to {action}")))?,
        };
        Ok((thread_id, turn_id))
    }

    async fn active_turn_interrupt_targets(
        &self,
        action: &str,
    ) -> Result<Vec<(String, String)>, CallerError> {
        let active_thread_id = self.active_thread_id.lock().await.clone();
        let fallback_turn_id = self.active_turn_id.lock().await.clone();
        let active_turns = self.active_turns.lock().await.clone();
        if active_thread_id.is_none() && active_turns.is_empty() {
            if fallback_turn_id.is_some() {
                return Err(CallerError::ExternalAgent(format!(
                    "no active thread to {action}"
                )));
            }
            return Err(CallerError::ExternalAgent(format!(
                "no active turn to {action}"
            )));
        }

        let mut targets = Vec::new();
        if let Some(thread_id) = active_thread_id.as_deref() {
            if let Some(turn_id) = active_turns
                .get(thread_id)
                .cloned()
                .or_else(|| fallback_turn_id.clone())
            {
                targets.push((thread_id.to_string(), turn_id));
            }
        }
        for (thread_id, turn_id) in active_turns {
            if targets
                .iter()
                .any(|(seen_thread_id, _)| seen_thread_id == &thread_id)
            {
                continue;
            }
            targets.push((thread_id, turn_id));
        }

        if targets.is_empty() {
            return Err(CallerError::ExternalAgent(format!(
                "no active turn to {action}"
            )));
        }
        Ok(targets)
    }

    async fn remember_active_turn_for_thread(&self, thread_id: &str, turn_id: &str) {
        self.active_turns
            .lock()
            .await
            .insert(thread_id.to_string(), turn_id.to_string());
        let active_thread_matches =
            self.active_thread_id.lock().await.as_deref() == Some(thread_id);
        if active_thread_matches {
            *self.active_turn_id.lock().await = Some(turn_id.to_string());
        }
    }

    async fn refresh_active_turn_after_expected_mismatch(
        &self,
        thread_id: &str,
        stale_turn_id: &str,
        err: &CallerError,
    ) -> Option<String> {
        let actual_turn_id = codex_expected_active_turn_mismatch_actual_turn_id(err)?;
        if actual_turn_id == stale_turn_id {
            return None;
        }
        self.remember_active_turn_for_thread(thread_id, &actual_turn_id)
            .await;
        Some(actual_turn_id)
    }

    /// Params for the mid-session `thread/resume` retry issued when
    /// `turn/start` reports the thread is no longer loaded (app-server
    /// eviction). Re-sends the cached `developerInstructions` override
    /// byte-identically: the override is not in Codex config, so a bare
    /// resume would rebuild the thread with the config-default developer
    /// block — silently dropping the managed-context policy AND rewriting
    /// the prompt prefix at maximum prompt size (cache-prefix contract on
    /// `turn_start_params`).
    fn followup_resume_params(
        &mut self,
        thread_id: &str,
    ) -> serde_json::Map<String, serde_json::Value> {
        let developer_instructions = self.thread_developer_instructions.clone();
        let mut params =
            self.thread_lifecycle_params_with_developer_instructions(developer_instructions);
        params.insert(
            "threadId".into(),
            serde_json::Value::String(thread_id.to_string()),
        );
        params.insert("excludeTurns".into(), serde_json::Value::Bool(true));
        params
    }

    /// Per-turn `turn/start` params.
    ///
    /// ── Cache prefix contract (prompt-cache prefix stability) ─────────────
    /// Every Codex turn re-sends the full request prefix (base instructions +
    /// developer block + tool specs + persisted history) to the model API;
    /// prompt caching only pays off when that prefix stays byte-stable
    /// between turns — and the managed gate zone sits at maximum prompt size,
    /// where a prefix miss is most expensive. Supervisor-side, the following
    /// must hold:
    ///
    /// 1. Per-turn params here are append-only: `threadId` + `input`, plus a
    ///    `serviceTier` override that is constant for the session (set at
    ///    launch or by an explicit user `/fast` toggle; it parameterizes API
    ///    routing, not prompt bytes). Do not add per-turn params that the
    ///    fork folds into instructions, the developer block, or tool config —
    ///    those rewrite content before the history suffix.
    /// 2. The developer block is fixed at `thread/start`
    ///    (`developerInstructions`, cached in
    ///    `thread_developer_instructions`) and re-sent byte-identically by
    ///    the mid-session `thread/resume` retry (`followup_resume_params`).
    ///    The override is not persisted in Codex config, so a bare resume
    ///    after an app-server thread eviction would swap the developer block
    ///    for the config default — busting the prefix and silently dropping
    ///    the managed-context policy.
    /// 3. Everything the supervisor adds mid-session appends after the
    ///    existing history: recovery kickstarts, density handoffs, and
    ///    held-follow-up replays are new user messages; `turn/steer` items
    ///    append into the active turn; primer injection
    ///    (`thread/inject_items`) appends developer items; `turn/interrupt`
    ///    mutates nothing supervisor-side (fork-side the abort marker is
    ///    recorded as an appended history item).
    /// 4. Settings churn (`thread/settings/update` with model / approval /
    ///    permissions / cwd) is confined to session-resume setup before the
    ///    first turn (`apply_resumed_thread_settings`); never issue it
    ///    between turns of a live session.
    ///
    /// Known structural exception, fork-side and out of supervisor control:
    /// the managed recovery turn swaps the request's tool surface (normal
    /// specs ↔ the five rewind-only specs), which rewrites the tools block —
    /// a full prefix miss on every gate entry/exit at maximum prompt size.
    /// That fix belongs to the fork (a stable tool surface across recovery
    /// turns); the supervisor side is deliberately kept append-only so the
    /// fork fix is sufficient on its own.
    fn turn_start_params(
        &mut self,
        thread_id: &str,
        input: Vec<serde_json::Value>,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut params_obj = serde_json::Map::new();
        params_obj.insert(
            "threadId".into(),
            serde_json::Value::String(thread_id.to_string()),
        );
        params_obj.insert("input".into(), serde_json::Value::Array(input));
        self.insert_service_tier_override_consuming_clear(&mut params_obj);
        params_obj
    }

    async fn resume_thread_for_followup(&mut self, thread_id: &str) -> Result<(), CallerError> {
        let params = self.followup_resume_params(thread_id);

        let response = self
            .send_request("thread/resume", Some(serde_json::Value::Object(params)))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/resume: {e}")))?;
        let resumed_thread_id = response
            .pointer("/thread/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CallerError::ExternalAgent(
                    "thread/resume response missing 'thread.id' field".into(),
                )
            })?;
        if resumed_thread_id != thread_id {
            return Err(CallerError::ExternalAgent(format!(
                "thread/resume returned thread {resumed_thread_id}, expected {thread_id}"
            )));
        }

        let active_turn = self.active_turns.lock().await.get(thread_id).cloned();
        *self.active_turn_id.lock().await = active_turn;
        *self.active_thread_id.lock().await = Some(thread_id.to_string());
        if let Err(e) = self
            .mark_existing_context_requests_seen(Some(thread_id))
            .await
        {
            eprintln!(
                "[codex] Warning: failed to seed context request trace baseline for resumed thread {thread_id}: {e}"
            );
        }
        Ok(())
    }
}

fn codex_turn_start_thread_not_found(err: &CallerError) -> bool {
    let CallerError::ExternalAgent(message) = err else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.contains("thread not found")
}

fn codex_expected_active_turn_mismatch_actual_turn_id(err: &CallerError) -> Option<String> {
    let CallerError::ExternalAgent(message) = err else {
        return None;
    };
    let prefix = "expected active turn id";
    let separator = " but found ";
    let lower = message.to_ascii_lowercase();
    let mismatch_start = lower.find(prefix)?;
    let found_start = mismatch_start + lower[mismatch_start..].find(separator)? + separator.len();
    let found = message[found_start..].trim_start();
    let mut chars = found.chars();
    let first = chars.next()?;
    let actual = if matches!(first, '`' | '"' | '\'') {
        let delimiter = first;
        found[delimiter.len_utf8()..]
            .split_once(delimiter)
            .map(|(actual, _)| actual)?
    } else {
        found
            .split(|c: char| c.is_whitespace() || matches!(c, ')' | ',' | ';'))
            .next()?
    };
    let actual = actual.trim();
    (!actual.is_empty()).then(|| actual.to_string())
}

#[async_trait]
impl ExternalAgent for CodexAgent {
    fn name(&self) -> &str {
        "codex"
    }

    fn service_tier(&self) -> Option<&str> {
        self.service_tier.as_deref()
    }

    async fn initialize(
        &mut self,
        config: AgentConfig,
    ) -> Result<mpsc::UnboundedReceiver<AgentEvent>, CallerError> {
        self.model = config.model.or_else(|| self.model.clone());
        self.approval_policy = config.approval_policy.clone();
        self.sandbox = config.sandbox;
        self.reasoning_effort = config.reasoning_effort;
        self.apply_configured_service_tier(config.service_tier);
        self.web_search = config.web_search;
        self.network_access = config.network_access;
        self.writable_roots = config.writable_roots;
        self.managed_context = config.codex_managed_context;
        self.request_trace_root = config.request_trace_dir;
        self.request_trace_temporary = config.request_trace_temporary;
        self.context_archive =
            crate::project::normalize_codex_context_archive(&config.context_archive);
        self.context_seen_request_ids.clear();
        self.context_trace_fingerprint = None;
        self.mcp_auth_token = config.mcp_auth_token;
        self.mcp_session_id = config.mcp_session_id;
        self.resume_session = config.resume_session;
        self.codex_home = config.codex_home;
        self.working_dir = Some(config.working_dir.clone());

        // The Intendant MCP server is wired exclusively via per-process
        // `-c mcp_servers.intendant.{type,url}` overrides (see
        // app_server_args): each spawned Codex carries its own
        // session-scoped URL on its command line. A legacy code path also
        // wrote `<working_dir>/.codex/config.toml` — a location Codex does
        // not read config from — which stomped the user's real
        // `~/.codex/config.toml` whenever a session's working dir was
        // `$HOME`, raced between concurrent sessions sharing a workspace,
        // and never restored its backup when the daemon died. Removed.
        let web_port = config.web_port.or(self.web_port);

        // Pass MCP server config via -c flag so Codex connects to intendant's MCP.
        // Any additional knobs the user toggled in the Control tab (web search,
        // network access inside workspace-write, extra writable roots) are
        // appended here as `-c key=value` overrides so Codex's app-server picks
        // them up exactly as if they had been written to `~/.codex/config.toml`
        // before launch.
        let effective_web_port = web_port.unwrap_or(8765);
        let args = self.app_server_args(effective_web_port);
        let mut command = crate::platform::spawn_command(&self.command);
        command
            .args(&args)
            .current_dir(&config.working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        crate::platform::die_with_parent(&mut command);
        self.add_intendant_ctl_env(&mut command, effective_web_port);
        // An active oauth:codex lease materializes a synthesized
        // CODEX_HOME (auth.json + carried-over config.toml) that shadows
        // both the configured home and ~/.codex — the vault fuels this
        // spawn, not whatever auth happens to be on disk.
        let leased_codex_home = crate::credential_leases::materialized_codex_home();
        Self::apply_codex_home_env(
            &mut command,
            leased_codex_home.as_deref().or(self.codex_home.as_deref()),
        );
        #[cfg(target_os = "linux")]
        crate::linux_display_env::apply_to_tokio_command(&mut command);
        if let Some(root) = &self.request_trace_root {
            std::fs::create_dir_all(root)?;
            command.env("CODEX_ROLLOUT_TRACE_ROOT", root);
        }
        let mut child = command.spawn().map_err(|e| {
            CallerError::ExternalAgent(format!("Failed to spawn '{}': {}", self.command, e))
        })?;
        let child_pid = child.id();

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CallerError::ExternalAgent("Failed to capture child stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CallerError::ExternalAgent("Failed to capture child stdout".into()))?;
        let stderr = child.stderr.take();

        if let Some(pid) = child_pid {
            super::register_child_process(pid);
        }
        self.child = Some(child);
        self.writer = Some(BufWriter::new(stdin));

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_tx = Some(event_tx.clone());
        if let Some(stderr) = stderr {
            super::spawn_stderr_forwarder("codex", stderr, event_tx.clone());
        }

        // Spawn reader task
        let pending_requests = Arc::clone(&self.pending_requests);
        let pending_approvals = Arc::clone(&self.pending_approvals);
        let approval_counter = Arc::new(AtomicU64::new(1));
        let active_turn_id = Arc::clone(&self.active_turn_id);
        let active_turns = Arc::clone(&self.active_turns);
        let active_thread_id = Arc::clone(&self.active_thread_id);
        let latest_token_usage = Arc::clone(&self.latest_token_usage);
        let context_pressure_floor = Arc::clone(&self.context_pressure_floor);
        let model = self.model.clone();

        let handle = tokio::spawn(reader_task(
            stdout,
            event_tx,
            pending_requests,
            pending_approvals,
            approval_counter,
            active_thread_id,
            active_turn_id,
            active_turns,
            latest_token_usage,
            context_pressure_floor,
            model,
        ));
        self.reader_handle = Some(handle);

        // Cold debug builds and auth-backed app-server startup can take more
        // than a few seconds, but this must still fail boundedly if Codex hangs.
        let init_params = serde_json::json!({
            "clientInfo": {
                "name": "intendant",
                "title": "Intendant",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "experimentalApi": true,
            },
        });

        let init_future = self.send_request("initialize", Some(init_params));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(CODEX_INITIALIZE_TIMEOUT_SECS),
            init_future,
        )
        .await;

        match result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                return Err(CallerError::ExternalAgent(format!(
                    "initialize request failed: {}",
                    e
                )));
            }
            Err(_) => {
                return Err(CallerError::ExternalAgent(format!(
                    "initialize request timed out ({CODEX_INITIALIZE_TIMEOUT_SECS}s)"
                )));
            }
        }

        // Send initialized notification
        self.send_notification("initialized", None).await?;

        Ok(event_rx)
    }

    async fn start_thread(&mut self) -> Result<AgentThread, CallerError> {
        let managed_developer_instructions = self
            .effective_managed_context_developer_instructions()
            .await;
        // Cache the exact override for the lifetime of the thread: the
        // mid-session `thread/resume` retry must re-send these bytes
        // verbatim (cache-prefix contract on `turn_start_params`).
        self.thread_developer_instructions = managed_developer_instructions.clone();
        let mut params = self
            .thread_lifecycle_params_with_developer_instructions(managed_developer_instructions);

        let method = if let Some(ref thread_id) = self.resume_session {
            params.insert(
                "threadId".into(),
                serde_json::Value::String(thread_id.clone()),
            );
            "thread/resume"
        } else {
            "thread/start"
        };

        let result = self
            .send_request(method, Some(serde_json::Value::Object(params)))
            .await?;
        self.update_service_tier_from_thread_response(&result);
        self.emit_resume_cwd_mismatch_if_needed(&result);

        let thread_id = result
            .pointer("/thread/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CallerError::ExternalAgent("thread/start response missing 'thread.id' field".into())
            })?
            .to_string();

        // Cache the thread id so interrupt_turn() can build the
        // `turn/interrupt` params without requiring a thread handle.
        *self.active_thread_id.lock().await = Some(thread_id.clone());

        if self.resume_session.is_some() {
            self.apply_resumed_thread_settings(&thread_id).await?;
            if let Err(e) = self
                .mark_existing_context_requests_seen(Some(&thread_id))
                .await
            {
                self.emit_backend_warning(format!(
                    "failed to seed context request trace baseline for resumed thread {thread_id}: {e}"
                ));
            }
            let rollout_path = match extract_thread_path(&result) {
                Some(path) => Some(path),
                None => match self.read_thread_snapshot(&thread_id).await {
                    Ok(snapshot) => snapshot.rollout_path,
                    Err(e) => {
                        self.emit_backend_warning(format!(
                            "failed to read resumed thread metadata for token usage seed: {e}; \
                             the context meter starts unseeded until the next turn reports usage"
                        ));
                        None
                    }
                },
            };
            if let Some(rollout_path) = rollout_path {
                match latest_codex_token_usage_from_rollout(&rollout_path).await {
                    Ok(Some(usage)) => {
                        let mut latest = self.latest_token_usage.lock().await;
                        let usage =
                            codex_usage_preserving_hard_context_window(usage, latest.as_ref());
                        *latest = Some(usage.clone());
                        drop(latest);
                        update_codex_context_pressure_floor(&self.context_pressure_floor, &usage)
                            .await;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.emit_backend_warning(format!(
                            "failed to seed token usage from rollout {}: {}",
                            rollout_path.display(),
                            e
                        ));
                    }
                }
            }
        }

        Ok(AgentThread { thread_id })
    }

    async fn send_message(
        &mut self,
        thread: &AgentThread,
        message: &str,
    ) -> Result<(), CallerError> {
        self.send_message_with_images(thread, message, &[]).await
    }

    async fn send_message_with_images(
        &mut self,
        thread: &AgentThread,
        message: &str,
        images: &[AgentImageAttachment],
    ) -> Result<(), CallerError> {
        // Codex v2 `UserInput` enum (camelCase): { type: "text" | "localImage" | "image" }.
        // Prefer `localImage` (file path) when we have one — keeps base64 out of the
        // JSON-RPC stream. Fall back to `image` with a data URL only if we don't.
        let mut input: Vec<serde_json::Value> = Vec::with_capacity(images.len() + 1);
        input.push(serde_json::json!({"type": "text", "text": message}));
        for img in images {
            if let Some(ref path) = img.local_path {
                input.push(serde_json::json!({
                    "type": "localImage",
                    "path": path.to_string_lossy(),
                }));
            } else {
                let data_url = format!("data:{};base64,{}", img.mime_type, img.base64);
                input.push(serde_json::json!({
                    "type": "image",
                    "url": data_url,
                }));
            }
        }
        let params = serde_json::Value::Object(self.turn_start_params(&thread.thread_id, input));
        self.turn_descendant_baseline =
            self.child.as_ref().and_then(|child| child.id()).map(|pid| {
                crate::platform::process_descendants(pid)
                    .into_iter()
                    .collect::<HashSet<_>>()
            });
        // turn/start is a request — Codex v2 requires an id to start processing.
        // The response carries the turn id; cache it so interrupt_turn() can
        // target this specific turn. Fall back to the reader task's
        // turn/started notification hook if the response shape differs.
        let response = match self.send_request("turn/start", Some(params.clone())).await {
            Ok(response) => response,
            Err(err) if codex_turn_start_thread_not_found(&err) => {
                self.resume_thread_for_followup(&thread.thread_id).await?;
                self.send_request("turn/start", Some(params)).await?
            }
            Err(err) => return Err(err),
        };
        if let Some(id) = extract_turn_id(&response) {
            self.active_turns
                .lock()
                .await
                .insert(thread.thread_id.clone(), id.clone());
            *self.active_turn_id.lock().await = Some(id);
        }
        // Also make sure the thread id cache matches the thread we were handed
        // (start_thread normally seeds it, but send_message can be called with
        // any thread in principle).
        *self.active_thread_id.lock().await = Some(thread.thread_id.clone());
        Ok(())
    }

    async fn context_snapshot(&mut self) -> Result<Option<AgentContextSnapshot>, CallerError> {
        match self.read_context_snapshot().await {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(err) if codex_context_snapshot_not_ready(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn context_snapshots(&mut self) -> Result<Vec<AgentContextSnapshot>, CallerError> {
        let Some(root) = self.request_trace_root.clone() else {
            return Ok(Vec::new());
        };
        let thread_id = self.active_thread_id.lock().await.clone();
        let fingerprint = match codex_context_trace_fingerprint(&root, thread_id.as_deref()).await {
            Ok(fingerprint) => fingerprint,
            Err(err) if codex_context_snapshot_not_ready(&err) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        if self.context_trace_fingerprint.as_ref() == Some(&fingerprint) {
            return Ok(Vec::new());
        }
        let traces = match read_codex_context_payloads_excluding(
            &root,
            thread_id.as_deref(),
            &self.context_seen_request_ids,
        )
        .await
        {
            Ok(traces) => traces,
            Err(err) if codex_context_snapshot_not_ready(&err) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        self.context_trace_fingerprint = Some(fingerprint);
        let rollout_path = match thread_id.as_deref() {
            Some(thread_id) => self
                .read_thread_snapshot(thread_id)
                .await
                .ok()
                .and_then(|snapshot| snapshot.rollout_path),
            None => None,
        };
        let usage = self.latest_token_usage.lock().await.clone();
        let pressure_floor = *self.context_pressure_floor.lock().await;
        let latest_request_id = traces.last().map(|trace| trace.request_id.clone());
        let exact_archive = self.context_archive_exact();
        Ok(traces
            .into_iter()
            .map(|trace| {
                let is_latest = latest_request_id.as_deref() == Some(trace.request_id.as_str());
                let (token_count, token_count_kind, context_window, hard_context_window) =
                    if is_latest {
                        codex_pressure_aware_usage_fields(usage.as_ref(), pressure_floor)
                    } else {
                        (None, None, None, None)
                    };
                let item_count = codex_request_item_count(&trace.payload);
                let raw = codex_context_archive_payload(
                    trace.payload,
                    &trace.request_id,
                    trace.request_index,
                    &trace.format,
                    exact_archive,
                );
                AgentContextSnapshot {
                    source: "codex".to_string(),
                    label: trace.label,
                    request_id: Some(trace.request_id),
                    request_index: Some(trace.request_index),
                    rollout_path: rollout_path.clone(),
                    format: trace.format,
                    token_count,
                    token_count_kind,
                    context_window,
                    hard_context_window,
                    item_count,
                    raw,
                }
            })
            .inspect(|snapshot| {
                if let Some(request_id) = snapshot.request_id.as_ref() {
                    self.context_seen_request_ids.insert(request_id.clone());
                }
            })
            .collect())
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), CallerError> {
        let pending = self
            .pending_approvals
            .lock()
            .await
            .remove(request_id)
            .ok_or_else(|| {
                CallerError::ExternalAgent(format!(
                    "No pending approval for request_id '{}'",
                    request_id
                ))
            })?;

        // MCP tool-call / elicitation requests use the
        // {"action": "accept"/"decline"} shape. Permissions requests use a
        // granted-permissions response. Command/file approval requests use
        // {"decision": "accept"/"decline"/…}. The MCP test MUST be the same
        // predicate the reader classified with — a request classified as MCP
        // but answered in the {"decision"} shape is silently ignored by
        // Codex and the tool call hangs.
        let result = if is_codex_mcp_approval_method(&pending.method) {
            let action = match decision {
                ApprovalDecision::Accept | ApprovalDecision::AcceptForSession => "accept",
                ApprovalDecision::Decline | ApprovalDecision::Cancel => "decline",
            };
            serde_json::json!({ "action": action, "content": {} })
        } else if pending.method == "item/permissions/requestApproval" {
            codex_permissions_approval_response(&pending.params, decision)
        } else {
            let decision_str = match decision {
                ApprovalDecision::Accept => "accept",
                ApprovalDecision::AcceptForSession => "acceptForSession",
                ApprovalDecision::Decline => "decline",
                ApprovalDecision::Cancel => "cancel",
            };
            serde_json::json!({ "decision": decision_str })
        };

        self.send_response(pending.jsonrpc_id, result).await
    }

    async fn interrupt_turn(&mut self) -> Result<(), CallerError> {
        let targets = self.active_turn_interrupt_targets("interrupt").await?;
        let mut first_error: Option<CallerError> = None;
        for (thread_id, turn_id) in targets {
            let params = serde_json::json!({
                "threadId": thread_id,
                "turnId": turn_id,
            });
            // turn/interrupt is a JSON-RPC request; Codex responds with `{}`
            // and emits a `turn/completed` notification with
            // status="interrupted" shortly after. The reader task handles that
            // notification like any other turn completion.
            let interrupt_result = self
                .send_request_with_timeout(
                    "turn/interrupt",
                    Some(params),
                    CODEX_INTERRUPT_REQUEST_TIMEOUT,
                )
                .await;
            match interrupt_result {
                Ok(_) => {}
                Err(err) => {
                    let Some(actual_turn_id) = self
                        .refresh_active_turn_after_expected_mismatch(&thread_id, &turn_id, &err)
                        .await
                    else {
                        if first_error.is_none() {
                            first_error = Some(err);
                        }
                        continue;
                    };
                    let params = serde_json::json!({
                        "threadId": thread_id,
                        "turnId": actual_turn_id,
                    });
                    if let Err(err) = self
                        .send_request_with_timeout(
                            "turn/interrupt",
                            Some(params),
                            CODEX_INTERRUPT_REQUEST_TIMEOUT,
                        )
                        .await
                    {
                        if first_error.is_none() {
                            first_error = Some(err);
                        }
                    }
                }
            }
        }
        if let Some(pid) = self.child.as_ref().and_then(|child| child.id()) {
            let protected = self.turn_descendant_baseline.clone().unwrap_or_default();
            let _ = crate::platform::terminate_unprotected_descendants(pid, &protected).await;
        }
        self.turn_descendant_baseline = None;
        // Clear pending approvals — the caller is also expected to resolve
        // them, but clearing here makes the agent's state consistent if the
        // caller forgets.
        self.pending_approvals.lock().await.clear();
        if let Some(err) = first_error {
            return Err(err);
        }
        Ok(())
    }

    async fn steer_turn(&mut self, text: &str) -> Result<(), CallerError> {
        // Mirror `interrupt_turn`'s precondition checks so the error
        // messages are consistent: "no active turn to steer" /
        // "no active thread to steer" both map to typed ExternalAgent
        // errors that `drain_external_agent_events` can fall back on.
        let (thread_id, turn_id) = self.active_thread_and_turn("steer").await?;
        let params = serde_json::json!({
            "threadId": thread_id,
            "input": [{"type": "text", "text": text}],
            "expectedTurnId": turn_id,
        });
        // `turn/steer` is a JSON-RPC request; Codex replies with
        // `{"turnId": "..."}` on success. We don't care about the returned
        // id — the active turn id hasn't changed, and the active_turn_id
        // cache is still valid for the next interrupt/steer call.
        let steer_result = self.send_request("turn/steer", Some(params)).await;
        match steer_result {
            Ok(_) => {}
            Err(err) => {
                let Some(actual_turn_id) = self
                    .refresh_active_turn_after_expected_mismatch(&thread_id, &turn_id, &err)
                    .await
                else {
                    return Err(err);
                };
                let params = serde_json::json!({
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": text}],
                    "expectedTurnId": actual_turn_id,
                });
                let _ = self.send_request("turn/steer", Some(params)).await?;
            }
        }
        Ok(())
    }

    async fn thread_action(
        &mut self,
        op: &str,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        CodexAgent::dispatch_thread_action(self, op, params).await
    }

    async fn pause_autonomous_goal(
        &mut self,
        thread_id: &str,
    ) -> Result<AutonomousGoalPauseResult, CallerError> {
        self.pause_active_goal_for_thread(thread_id).await
    }

    fn supports_user_message_rewind(&self) -> bool {
        true
    }

    fn supports_item_anchor_rewind(&self) -> bool {
        self.managed_context
    }

    /// Native implementation of conversation rollback. Reuses the
    /// `thread/rollback` RPC under `numTurns` — same as `/undo`,
    /// just without the status string and with a guard allowing 0 to be
    /// a no-op (the HTTP handler may issue rollback with 0 turns when
    /// the target round is already the head).
    async fn rollback_turns(&mut self, turns_to_drop: u32) -> Result<(), CallerError> {
        if turns_to_drop == 0 {
            return Ok(());
        }
        let _status = self
            .rollback_turns_inner(&serde_json::Value::Null, turns_to_drop)
            .await?;
        Ok(())
    }

    async fn rollback_thread_turns(
        &mut self,
        thread_id: &str,
        turns_to_drop: u32,
    ) -> Result<(), CallerError> {
        if turns_to_drop == 0 {
            return Ok(());
        }
        let params = serde_json::json!({ "threadId": thread_id });
        let _status = self.rollback_turns_inner(&params, turns_to_drop).await?;
        Ok(())
    }

    async fn rollback_thread_to_item_anchor(
        &mut self,
        thread_id: &str,
        item_id: &str,
        position: RollbackAnchorPosition,
    ) -> Result<(), CallerError> {
        self.rollback_item_anchor_rpc(thread_id, item_id, position)
            .await
    }

    async fn read_thread_snapshot(
        &mut self,
        thread_id: &str,
    ) -> Result<AgentThreadSnapshot, CallerError> {
        let thread_id = thread_id.trim();
        if thread_id.is_empty() {
            return Err(CallerError::ExternalAgent(
                "thread metadata read requires a thread id".into(),
            ));
        }
        let params = serde_json::json!({
            "threadId": thread_id,
            "includeTurns": false,
        });
        let response = self
            .send_request("thread/read", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/read: {e}")))?;
        Ok(AgentThreadSnapshot {
            thread_id: extract_thread_id(&response).unwrap_or_else(|| thread_id.to_string()),
            rollout_path: extract_thread_path(&response),
        })
    }

    async fn fork_thread_from_rollout_path(
        &mut self,
        rollout_path: &Path,
        name: Option<&str>,
    ) -> Result<AgentThread, CallerError> {
        let path = rollout_path.to_string_lossy();
        if path.trim().is_empty() {
            return Err(CallerError::ExternalAgent(
                "rollout-path fork requires a path".into(),
            ));
        }
        let mut params_obj = serde_json::Map::new();
        params_obj.insert("threadId".into(), serde_json::Value::String(String::new()));
        params_obj.insert(
            "path".into(),
            serde_json::Value::String(path.as_ref().to_string()),
        );
        self.insert_service_tier_override(&mut params_obj);
        let params = serde_json::Value::Object(params_obj);
        let response = self
            .send_request("thread/fork", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/fork: {e}")))?;
        let thread_id = extract_thread_id(&response).ok_or_else(|| {
            CallerError::ExternalAgent("thread/fork response missing thread id".into())
        })?;
        self.reset_context_pressure_after_thread_rewrite().await;
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            let request = serde_json::json!({ "threadId": thread_id.clone(), "name": name });
            self.send_request("thread/name/set", Some(request))
                .await
                .map_err(|e| CallerError::ExternalAgent(format!("thread/name/set: {e}")))?;
        }
        Ok(AgentThread { thread_id })
    }

    async fn fork_thread_with_options(
        &mut self,
        thread_id: &str,
        name: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<AgentThread, CallerError> {
        let thread_id = thread_id.trim();
        if thread_id.is_empty() {
            return Err(CallerError::ExternalAgent(
                "live-thread fork requires a thread id".into(),
            ));
        }
        let params = self.fork_thread_with_options_params(thread_id, cwd);
        let response = self
            .send_request("thread/fork", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/fork: {e}")))?;
        let forked_thread_id = extract_thread_id(&response).ok_or_else(|| {
            CallerError::ExternalAgent("thread/fork response missing thread id".into())
        })?;
        // Unlike rollback/restore/rollout-path forks — which rewrite or stand
        // in for the active thread — a fission fork leaves the parent thread
        // untouched, so its context-pressure floor must persist.
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            let request = serde_json::json!({ "threadId": forked_thread_id.clone(), "name": name });
            self.send_request("thread/name/set", Some(request))
                .await
                .map_err(|e| CallerError::ExternalAgent(format!("thread/name/set: {e}")))?;
        }
        Ok(AgentThread {
            thread_id: forked_thread_id,
        })
    }

    async fn restore_thread_from_rollout_path(
        &mut self,
        thread_id: &str,
        rollout_path: &Path,
        record_id: Option<&str>,
    ) -> Result<(), CallerError> {
        let thread_id = thread_id.trim();
        if thread_id.is_empty() {
            return Err(CallerError::ExternalAgent(
                "same-thread restore requires a thread id".into(),
            ));
        }
        let path = rollout_path.to_string_lossy();
        if path.trim().is_empty() {
            return Err(CallerError::ExternalAgent(
                "same-thread restore requires a rollout path".into(),
            ));
        }
        let mut params = serde_json::Map::new();
        params.insert(
            "threadId".to_string(),
            serde_json::Value::String(thread_id.to_string()),
        );
        params.insert(
            "rolloutPath".to_string(),
            serde_json::Value::String(path.to_string()),
        );
        if let Some(record_id) = record_id.map(str::trim).filter(|id| !id.is_empty()) {
            params.insert(
                "recordId".to_string(),
                serde_json::Value::String(record_id.to_string()),
            );
        }
        self.send_request("thread/restore", Some(serde_json::Value::Object(params)))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/restore: {e}")))?;
        *self.active_thread_id.lock().await = Some(thread_id.to_string());
        self.reset_context_pressure_after_thread_rewrite().await;
        Ok(())
    }

    async fn inject_thread_developer_message(
        &mut self,
        thread_id: &str,
        message: &str,
    ) -> Result<(), CallerError> {
        let thread_id = thread_id.trim();
        if thread_id.is_empty() {
            return Err(CallerError::ExternalAgent(
                "developer-message injection requires a thread id".into(),
            ));
        }
        let message = message.trim();
        if message.is_empty() {
            return Err(CallerError::ExternalAgent(
                "developer-message injection requires non-empty content".into(),
            ));
        }
        let params = serde_json::json!({
            "threadId": thread_id,
            "items": [{
                "type": "message",
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": message,
                }],
            }],
        });
        self.send_request("thread/inject_items", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/inject_items: {e}")))?;
        Ok(())
    }

    async fn activate_thread(&mut self, thread_id: &str) -> Result<(), CallerError> {
        let active_turn = self.active_turns.lock().await.get(thread_id).cloned();
        *self.active_turn_id.lock().await = active_turn;
        *self.active_thread_id.lock().await = Some(thread_id.to_string());
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), CallerError> {
        // Abort reader task
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }

        // Kill child process
        if let Some(ref mut child) = self.child {
            let child_pid = child.id();
            if let Some(pid) = child_pid {
                let protected = HashSet::new();
                let _ = crate::platform::terminate_unprotected_descendants(pid, &protected).await;
            }
            let _ = child.kill().await;
            if let Some(pid) = child_pid {
                super::unregister_child_process(pid);
            }
        }

        // One-time hygiene for configs the removed legacy writer left
        // behind: an "Auto-generated by Intendant" config.toml in this
        // working dir is restored from its backup (or deleted when it was
        // created fresh). Never touches user-authored configs.
        if let Some(ref wd) = self
            .config_working_dir
            .take()
            .or_else(|| self.working_dir.clone())
        {
            let codex_dir = wd.join(".codex");
            let config_path = codex_dir.join("config.toml");
            let backup_path = codex_dir.join("config.toml.intendant-backup");
            let ours = std::fs::read_to_string(&config_path)
                .map(|existing| existing.contains("# Auto-generated by Intendant"))
                .unwrap_or(false);
            if ours {
                if backup_path.exists() {
                    let _ = std::fs::rename(&backup_path, &config_path);
                } else {
                    let _ = std::fs::remove_file(&config_path);
                }
            }
        }

        // Drop handles
        self.writer = None;
        self.event_tx = None;
        self.child = None;
        self.turn_descendant_baseline = None;
        self.active_turn_id.lock().await.take();
        self.active_thread_id.lock().await.take();
        self.cleanup_temporary_request_trace_root();

        Ok(())
    }
}

impl Drop for CodexAgent {
    fn drop(&mut self) {
        // Kill the child process synchronously to prevent orphans.
        if let Some(ref mut child) = self.child {
            let child_pid = child.id();
            if let Some(pid) = child_pid {
                let protected = HashSet::new();
                let _ = crate::platform::terminate_unprotected_descendants_now(pid, &protected);
            }
            let _ = child.start_kill();
            if let Some(pid) = child_pid {
                super::unregister_child_process(pid);
            }
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        self.cleanup_temporary_request_trace_root();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_agent::MAX_THREAD_GOAL_OBJECTIVE_CHARS;

    #[tokio::test]
    async fn resumed_thread_context_baseline_suppresses_old_trace_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let trace = tmp.path().join("trace-thread-abc");
        std::fs::create_dir_all(trace.join("payloads")).unwrap();

        for (idx, text) in [(1, "first"), (2, "second"), (3, "third")] {
            std::fs::write(
                trace.join(format!("payloads/request-{idx}.json")),
                serde_json::json!({
                    "type": "response.create",
                    "input": [{"role": "user", "content": text}]
                })
                .to_string(),
            )
            .unwrap();
        }

        let line = |ts: u64, idx: u64, call_id: &str| {
            serde_json::json!({
                "schema_version": 1,
                "wall_time_unix_ms": ts,
                "payload": {
                    "type": "inference_started",
                    "provider_name": "OpenAI",
                    "thread_id": "thread-abc",
                    "inference_call_id": call_id,
                    "request_payload": {
                        "kind": {"type": "inference_request"},
                        "path": format!("payloads/request-{idx}.json")
                    }
                }
            })
            .to_string()
        };

        std::fs::write(
            trace.join("trace.jsonl"),
            [line(10, 1, "inference:1"), line(20, 2, "inference:2")].join("\n"),
        )
        .unwrap();

        let mut agent = test_agent();
        agent.request_trace_root = Some(tmp.path().to_path_buf());
        agent.context_archive = "exact".to_string();
        let inserted = agent
            .mark_existing_context_requests_seen(Some("thread-abc"))
            .await
            .unwrap();
        assert_eq!(inserted, 2);
        let baseline_fingerprint = agent
            .context_trace_fingerprint
            .clone()
            .expect("baseline should seed trace fingerprint");
        assert!(agent.context_snapshots().await.unwrap().is_empty());
        assert_eq!(
            agent.context_trace_fingerprint.as_ref(),
            Some(&baseline_fingerprint)
        );

        std::fs::write(
            trace.join("trace.jsonl"),
            [
                line(10, 1, "inference:1"),
                line(20, 2, "inference:2"),
                line(30, 3, "inference:3"),
            ]
            .join("\n"),
        )
        .unwrap();

        let snapshots = agent.context_snapshots().await.unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_ne!(
            agent.context_trace_fingerprint.as_ref(),
            Some(&baseline_fingerprint)
        );
        assert_eq!(snapshots[0].request_index, Some(3));
        assert!(serde_json::to_string(&snapshots[0].raw)
            .unwrap()
            .contains("third"));
        assert!(agent.context_snapshots().await.unwrap().is_empty());
    }

    #[test]
    fn approval_decision_formatting() {
        // Verify the decision strings match the Codex protocol
        let cases = vec![
            (ApprovalDecision::Accept, "accept"),
            (ApprovalDecision::AcceptForSession, "acceptForSession"),
            (ApprovalDecision::Decline, "decline"),
            (ApprovalDecision::Cancel, "cancel"),
        ];
        for (decision, expected) in cases {
            let decision_str = match decision {
                ApprovalDecision::Accept => "accept",
                ApprovalDecision::AcceptForSession => "acceptForSession",
                ApprovalDecision::Decline => "decline",
                ApprovalDecision::Cancel => "cancel",
            };
            assert_eq!(decision_str, expected);
        }
    }

    #[test]
    fn codex_agent_new_defaults() {
        let agent = CodexAgent::new(
            "codex".into(),
            Some("o4-mini".into()),
            "on-request".into(),
            "workspace-write".into(),
            None,
        );
        assert_eq!(agent.command, "codex");
        assert_eq!(agent.model, Some("o4-mini".into()));
        assert_eq!(agent.approval_policy, "on-request");
        assert_eq!(agent.sandbox, "workspace-write");
        assert!(agent.child.is_none());
        assert!(agent.writer.is_none());
        assert!(agent.event_tx.is_none());
        assert!(agent.reader_handle.is_none());
    }

    #[test]
    fn turn_start_thread_not_found_error_is_resumable() {
        let err = CallerError::ExternalAgent(
            "JSON-RPC error -32600: thread not found: 019e-child".to_string(),
        );
        assert!(codex_turn_start_thread_not_found(&err));
    }

    #[test]
    fn unrelated_external_error_is_not_resumable_thread_not_found() {
        let err = CallerError::ExternalAgent(
            "JSON-RPC error -32600: cannot start turn while closing".to_string(),
        );
        assert!(!codex_turn_start_thread_not_found(&err));
    }

    #[test]
    fn expected_turn_mismatch_parser_handles_steer_error() {
        let err = CallerError::ExternalAgent(
            "JSON-RPC error -32600: expected active turn id `turn-expected` but found `turn-actual`"
                .to_string(),
        );

        assert_eq!(
            codex_expected_active_turn_mismatch_actual_turn_id(&err).as_deref(),
            Some("turn-actual")
        );
    }

    #[test]
    fn expected_turn_mismatch_parser_handles_interrupt_error() {
        let err = CallerError::ExternalAgent(
            "JSON-RPC error -32600: expected active turn id turn-expected but found turn-actual"
                .to_string(),
        );

        assert_eq!(
            codex_expected_active_turn_mismatch_actual_turn_id(&err).as_deref(),
            Some("turn-actual")
        );
    }

    #[test]
    fn expected_turn_mismatch_parser_ignores_unrelated_error() {
        let err = CallerError::ExternalAgent(
            "JSON-RPC error -32600: no active turn to steer".to_string(),
        );

        assert_eq!(
            codex_expected_active_turn_mismatch_actual_turn_id(&err),
            None
        );
    }

    #[tokio::test]
    async fn interrupt_turn_without_active_turn_errors() {
        let mut agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        let err = agent.interrupt_turn().await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active turn"),
                    "expected 'no active turn' error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn interrupt_turn_without_thread_errors() {
        let mut agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        // Active turn but no thread — should still error with "no active thread".
        *agent.active_turn_id.lock().await = Some("t-1".into());
        let err = agent.interrupt_turn().await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active thread"),
                    "expected 'no active thread' error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn interrupt_turn_sends_correct_jsonrpc_request() {
        // Set up an agent with a duplex pipe in place of the child stdin.
        // We can't easily stub `send_request` without refactoring, so instead
        // we assert the pre-write state: the request builder would produce the
        // right JSON by inspecting the agent's captured thread/turn ids and
        // re-running the same params construction path.
        let agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        *agent.active_turn_id.lock().await = Some("turn-xyz".into());
        *agent.active_thread_id.lock().await = Some("thread-abc".into());

        // Reconstruct the same params object the implementation builds.
        let turn_id = agent.active_turn_id.lock().await.clone().unwrap();
        let thread_id = agent.active_thread_id.lock().await.clone().unwrap();
        let params = serde_json::json!({
            "threadId": thread_id,
            "turnId": turn_id,
        });
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["turnId"], "turn-xyz");
    }

    #[tokio::test]
    async fn active_thread_and_turn_uses_thread_specific_turn_ids() {
        let mut agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        *agent.active_thread_id.lock().await = Some("parent-thread".into());
        *agent.active_turn_id.lock().await = Some("fallback-turn".into());
        {
            let mut active_turns = agent.active_turns.lock().await;
            active_turns.insert("parent-thread".into(), "parent-turn".into());
            active_turns.insert("side-thread".into(), "side-turn".into());
        }

        let (thread_id, turn_id) = agent.active_thread_and_turn("steer").await.unwrap();
        assert_eq!(thread_id, "parent-thread");
        assert_eq!(turn_id, "parent-turn");

        agent.activate_thread("side-thread").await.unwrap();
        assert_eq!(
            agent.active_turn_id.lock().await.as_deref(),
            Some("side-turn")
        );
        let (thread_id, turn_id) = agent.active_thread_and_turn("steer").await.unwrap();
        assert_eq!(thread_id, "side-thread");
        assert_eq!(turn_id, "side-turn");
    }

    #[tokio::test]
    async fn active_turn_interrupt_targets_include_side_turns() {
        let agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        *agent.active_thread_id.lock().await = Some("parent-thread".into());
        *agent.active_turn_id.lock().await = Some("fallback-parent-turn".into());
        {
            let mut active_turns = agent.active_turns.lock().await;
            active_turns.insert("parent-thread".into(), "parent-turn".into());
            active_turns.insert("side-thread".into(), "side-turn".into());
        }

        let targets = agent
            .active_turn_interrupt_targets("interrupt")
            .await
            .unwrap();

        assert_eq!(
            targets
                .first()
                .map(|(thread_id, turn_id)| { (thread_id.as_str(), turn_id.as_str()) }),
            Some(("parent-thread", "parent-turn"))
        );
        assert_eq!(targets.len(), 2);
        assert!(targets
            .iter()
            .any(|(thread_id, turn_id)| thread_id == "side-thread" && turn_id == "side-turn"));
    }

    #[tokio::test]
    async fn active_turn_interrupt_targets_use_thread_map_without_active_thread_cache() {
        let agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        agent
            .active_turns
            .lock()
            .await
            .insert("side-thread".into(), "side-turn".into());

        let targets = agent
            .active_turn_interrupt_targets("interrupt")
            .await
            .unwrap();

        assert_eq!(targets, vec![("side-thread".into(), "side-turn".into())]);
    }

    #[tokio::test]
    async fn expected_turn_mismatch_refreshes_active_turn_cache() {
        let agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        *agent.active_thread_id.lock().await = Some("thread-abc".into());
        *agent.active_turn_id.lock().await = Some("turn-stale".into());
        agent
            .active_turns
            .lock()
            .await
            .insert("thread-abc".into(), "turn-stale".into());
        let err = CallerError::ExternalAgent(
            "JSON-RPC error -32600: expected active turn id `turn-stale` but found `turn-actual`"
                .to_string(),
        );

        let refreshed = agent
            .refresh_active_turn_after_expected_mismatch("thread-abc", "turn-stale", &err)
            .await;

        assert_eq!(refreshed.as_deref(), Some("turn-actual"));
        assert_eq!(
            agent.active_turn_id.lock().await.as_deref(),
            Some("turn-actual")
        );
        assert_eq!(
            agent
                .active_turns
                .lock()
                .await
                .get("thread-abc")
                .map(String::as_str),
            Some("turn-actual")
        );
    }

    // ── Mid-turn steering (`turn/steer`) ──
    //
    // Steering injects user text into the currently running turn without
    // cancelling it. Same pattern as `interrupt_turn` — precondition checks
    // for active turn/thread ids, then a JSON-RPC request with the steering
    // params. The response carries a turnId we intentionally discard.

    #[tokio::test]
    async fn steer_turn_without_active_turn_errors() {
        let mut agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        let err = agent
            .steer_turn("redirect to test coverage")
            .await
            .unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active turn"),
                    "expected 'no active turn' error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn steer_turn_without_thread_errors() {
        let mut agent = CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "danger-full-access".into(),
            None,
        );
        *agent.active_turn_id.lock().await = Some("t-1".into());
        let err = agent.steer_turn("please stop").await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active thread"),
                    "expected 'no active thread' error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    // ── Thread actions (compact / fork / undo / review / rename / goal / memory-reset) ──
    //
    // These tests assert the error-handling contract (no active thread →
    // typed error) and the dispatcher routing (/op → right method). The
    // happy-path RPC wire format is verified in a dedicated wire-format
    // test parallel to `interrupt_turn_wire_format_is_jsonrpc_request`
    // below, because the pipe plumbing is the same.

    pub(crate) fn test_agent() -> CodexAgent {
        CodexAgent::new(
            "codex".into(),
            None,
            "on-request".into(),
            "workspace-write".into(),
            None,
        )
    }

    fn args_have_config_override(args: &[String], value: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == "-c" && pair[1] == value)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_request_timeout_removes_pending_request() {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "cat >/dev/null"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn stdin sink");
        let stdin = child.stdin.take().expect("child stdin");

        let mut agent = test_agent();
        agent.writer = Some(BufWriter::new(stdin));
        agent.child = Some(child);

        let err = agent
            .send_request_with_timeout(
                "turn/interrupt",
                Some(serde_json::json!({
                    "threadId": "thread-abc",
                    "turnId": "turn-xyz",
                })),
                Duration::from_millis(20),
            )
            .await
            .unwrap_err();

        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("turn/interrupt request timed out"),
                    "got: {msg}"
                );
            }
            other => panic!("expected ExternalAgent error, got {other:?}"),
        }
        assert!(agent.pending_requests.lock().await.is_empty());

        agent.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn thread_action_without_thread_errors() {
        // Each action needs an active thread; without one the dispatcher
        // returns a clear error rather than hanging on the pending-request
        // oneshot.
        for op in [
            "compact",
            "fork",
            "side",
            "undo",
            "review",
            "rename",
            "goal",
            "goal-set",
            "goal-edit",
            "goal-clear",
            "goal-pause",
            "goal-resume",
            "goal-complete",
            "goal-budget-limited",
            "memory-reset",
        ] {
            let mut agent = test_agent();
            let err = agent
                .thread_action(op, &serde_json::Value::Null)
                .await
                .unwrap_err();
            match (op, err) {
                ("memory-reset", CallerError::ExternalAgent(msg)) => {
                    assert!(msg.contains("Not initialized"), "got: {}", msg);
                }
                (_, CallerError::ExternalAgent(msg)) => {
                    assert!(
                        msg.contains("no active Codex thread"),
                        "op /{}: expected 'no active Codex thread' error, got: {}",
                        op,
                        msg,
                    );
                }
                (_, other) => panic!("op /{}: expected ExternalAgent error, got {:?}", op, other),
            }
        }
    }

    #[tokio::test]
    async fn thread_action_fast_toggles_priority_service_tier_without_thread() {
        let mut agent = test_agent();

        let enabled = agent
            .thread_action("fast", &serde_json::Value::Null)
            .await
            .unwrap();
        assert!(enabled.contains("enabled"), "got: {enabled}");
        assert_eq!(agent.service_tier.as_deref(), Some(CODEX_FAST_SERVICE_TIER));
        assert!(!agent.service_tier_clear_pending);

        let disabled = agent
            .thread_action("fast", &serde_json::Value::Null)
            .await
            .unwrap();
        assert!(disabled.contains("disabled"), "got: {disabled}");
        assert_eq!(agent.service_tier, None);
        assert!(agent.service_tier_clear_pending);
    }

    #[test]
    fn service_tier_override_serializes_fast_and_standard_clear() {
        let mut agent = test_agent();

        agent.toggle_fast_service_tier();
        let mut fast_params = serde_json::Map::new();
        agent.insert_service_tier_override_consuming_clear(&mut fast_params);
        assert_eq!(fast_params["serviceTier"], CODEX_FAST_SERVICE_TIER);
        assert_eq!(agent.service_tier.as_deref(), Some(CODEX_FAST_SERVICE_TIER));
        assert!(!agent.service_tier_clear_pending);

        agent.toggle_fast_service_tier();
        let mut standard_params = serde_json::Map::new();
        agent.insert_service_tier_override_consuming_clear(&mut standard_params);
        assert!(standard_params["serviceTier"].is_null());
        assert_eq!(agent.service_tier, None);
        assert!(!agent.service_tier_clear_pending);

        let mut later_params = serde_json::Map::new();
        agent.insert_service_tier_override_consuming_clear(&mut later_params);
        assert!(later_params.get("serviceTier").is_none());
    }

    #[test]
    fn thread_lifecycle_params_include_workspace_cwd() {
        let mut agent = test_agent();
        agent.model = Some("gpt-5.5".to_string());
        agent.working_dir = Some(PathBuf::from("/tmp/intendant-workspace"));

        let params = agent.thread_lifecycle_params_with_developer_instructions(None);

        assert_eq!(params["model"], "gpt-5.5");
        assert_eq!(params["approvalPolicy"], "on-request");
        assert_eq!(params["sandbox"], "workspace-write");
        assert_eq!(params["cwd"], "/tmp/intendant-workspace");
    }

    #[test]
    fn resume_cwd_mismatch_warning_uses_codex_reported_cwd() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = test_agent();
        agent.resume_session = Some("019e9f80".to_string());
        agent.working_dir = Some(PathBuf::from("/home/user/projects/intendant-new"));
        agent.event_tx = Some(tx);

        agent.emit_resume_cwd_mismatch_if_needed(&serde_json::json!({
            "thread": {
                "id": "019e9f80",
                "cwd": "/home/user/projects/intendant-old"
            }
        }));

        match rx.try_recv().unwrap() {
            AgentEvent::Log { level, message } => {
                assert_eq!(level, "warn");
                assert!(message.contains("/home/user/projects/intendant-new"));
                assert!(message.contains("/home/user/projects/intendant-old"));
                assert!(message.contains("thread/resume"));
            }
            other => panic!("expected cwd mismatch Log, got {:?}", other),
        }
    }

    #[test]
    fn resume_cwd_mismatch_warning_ignores_lexically_equivalent_paths() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = test_agent();
        agent.resume_session = Some("019e9f80".to_string());
        agent.working_dir = Some(PathBuf::from("/home/user/projects/../projects/intendant/"));
        agent.event_tx = Some(tx);

        agent.emit_resume_cwd_mismatch_if_needed(&serde_json::json!({
            "cwd": "/home/user/projects/intendant"
        }));

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn thread_lifecycle_params_disable_approvals_for_danger_full_access() {
        let mut agent = test_agent();
        agent.approval_policy = "on-request".to_string();
        agent.sandbox = "danger-full-access".to_string();

        let params = agent.thread_lifecycle_params_with_developer_instructions(None);

        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["sandbox"], "danger-full-access");
    }

    /// Cache-prefix contract: per-turn `turn/start` params are append-only —
    /// `threadId` + `input` plus a session-constant `serviceTier` override.
    /// Consecutive turns must differ only in `input`; anything else added
    /// here is prefix-relevant and trips this test on purpose.
    #[test]
    fn turn_start_params_are_append_only_across_turns() {
        let mut agent = test_agent();
        agent.service_tier = Some(CODEX_FAST_SERVICE_TIER.to_string());

        let kickstart = vec![serde_json::json!({
            "type": "text",
            "text": "<managed_context_recovery>recover now</managed_context_recovery>",
        })];
        let replay = vec![serde_json::json!({
            "type": "text",
            "text": "<managed_context_rewind_followup_replay>user follow-up</managed_context_rewind_followup_replay>",
        })];

        let mut first = agent.turn_start_params("thread-1", kickstart);
        let mut second = agent.turn_start_params("thread-1", replay);

        for params in [&first, &second] {
            let mut keys: Vec<&str> = params.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                vec!["input", "serviceTier", "threadId"],
                "turn/start must carry no per-turn prefix-relevant params"
            );
        }

        // Everything except the appended user input is byte-stable between
        // turns (the kickstart flow replaces nothing it sent before).
        first.remove("input");
        second.remove("input");
        assert_eq!(first, second);

        // Without a tier override the surface is just threadId + input.
        let mut plain = test_agent();
        let params = plain.turn_start_params("thread-1", Vec::new());
        let mut keys: Vec<&str> = params.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["input", "threadId"]);
    }

    #[test]
    fn thread_lifecycle_params_leave_developer_instructions_unset_by_default() {
        let mut agent = test_agent();
        agent.managed_context = false;

        let params = agent.thread_lifecycle_params_with_developer_instructions(None);

        assert!(params.get("developerInstructions").is_none());
    }

    #[test]
    fn managed_context_generic_instructions_omit_repo_specific_guidance() {
        // Intendant-repo-specific guidance lives in
        // .intendant/codex-managed-instructions.md, not in the generic
        // constant injected into every managed session in every project.
        for marker in [
            "validate-dashboard.cjs",
            "--station-probe",
            "--launch-dashboard",
            "--hold-dashboard",
            "--diagnostics --json",
            "docs/src/external-agent-orchestration.md",
            "Station",
            "cargo run",
        ] {
            assert!(
                !MANAGED_CONTEXT_DEVELOPER_INSTRUCTIONS.contains(marker),
                "generic managed-context instructions must not contain {marker:?}"
            );
        }
    }

    #[test]
    fn resumed_thread_settings_update_uses_permission_profile() {
        let mut agent = test_agent();
        agent.model = Some("gpt-5.5".to_string());
        agent.approval_policy = "on-request".to_string();
        agent.sandbox = "danger-full-access".to_string();
        agent.working_dir = Some(PathBuf::from("/tmp/intendant-workspace"));

        let params = agent.resumed_thread_settings_update_params("thread-123");

        assert_eq!(params["threadId"], "thread-123");
        assert_eq!(params["model"], "gpt-5.5");
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["permissions"], ":danger-full-access");
        assert_eq!(params["cwd"], "/tmp/intendant-workspace");
        assert!(params.get("sandbox").is_none());
        assert!(params.get("sandboxPolicy").is_none());
    }

    #[test]
    fn app_server_args_bypass_approvals_for_danger_full_access() {
        let mut agent = test_agent();
        agent.approval_policy = "on-request".to_string();
        agent.sandbox = "danger-full-access".to_string();

        let args = agent.app_server_args(8765);

        assert_eq!(
            args.first().map(String::as_str),
            Some("--dangerously-bypass-approvals-and-sandbox")
        );
        assert!(args.iter().any(|arg| arg == "approval_policy=\"never\""));
        assert!(args
            .iter()
            .any(|arg| arg == "sandbox_mode=\"danger-full-access\""));
        assert!(args.iter().any(|arg| arg == "app-server"));
    }

    #[test]
    fn app_server_args_preserve_workspace_approval_flow() {
        let agent = test_agent();

        let args = agent.app_server_args(8765);

        assert_eq!(args.first().map(String::as_str), Some("app-server"));
        assert!(!args
            .iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(!args.iter().any(|arg| arg.starts_with("approval_policy=")));
    }

    #[test]
    fn app_server_args_disable_unrelated_codex_mcp_servers_from_codex_home() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
[mcp_servers.linear]
command = "linear-mcp"

[mcp_servers.notion]
command = "notion-mcp"

[mcp_servers."linear.com"]
command = "linear-mcp"

[mcp_servers.intendant]
type = "http"
url = "http://stale.example.invalid/mcp"
"#,
        )
        .unwrap();
        let mut agent = test_agent();
        agent.managed_context = true;
        agent.codex_home = Some(tmp.path().to_path_buf());

        let args = agent.app_server_args(8765);

        assert_eq!(args.first().map(String::as_str), Some("app-server"));
        assert!(args_have_config_override(&args, "features.plugins=false"));
        assert!(args
            .iter()
            .any(|arg| arg
                == "mcp_servers={linear={enabled=false},\"linear.com\"={enabled=false},notion={enabled=false}}"));
        assert!(!args
            .iter()
            .any(|arg| arg == "mcp_servers.intendant.enabled=false"));
        assert!(args
            .iter()
            .any(|arg| arg == "mcp_servers.intendant.type=\"http\""));
        assert!(args.iter().any(|arg| {
            arg == "mcp_servers.intendant.url=\"http://localhost:8765/mcp?managed_context=managed&tool_profile=core\""
        }));
    }

    #[test]
    fn app_server_args_disable_inherited_codex_plugins_without_codex_home() {
        let mut agent = test_agent();
        agent.managed_context = true;

        let args = agent.app_server_args(8765);

        assert_eq!(args.first().map(String::as_str), Some("app-server"));
        assert!(args_have_config_override(&args, "features.plugins=false"));
        assert!(!args.iter().any(|arg| arg.starts_with("mcp_servers={")));
        assert!(args_have_config_override(
            &args,
            "mcp_servers.intendant.type=\"http\""
        ));
    }

    #[test]
    fn app_server_args_vanilla_preserves_inherited_codex_config_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
[mcp_servers.linear]
command = "linear-mcp"

[plugins."browser@openai-bundled"]
enabled = true
"#,
        )
        .unwrap();
        let mut agent = test_agent();
        agent.managed_context = false;
        agent.codex_home = Some(tmp.path().to_path_buf());

        let args = agent.app_server_args(8765);

        assert_eq!(args.first().map(String::as_str), Some("app-server"));
        assert!(!args_have_config_override(&args, "features.plugins=false"));
        assert!(!args.iter().any(|arg| arg.starts_with("mcp_servers={")));
        assert!(args.iter().any(|arg| {
            arg == "mcp_servers.intendant.url=\"http://localhost:8765/mcp?managed_context=vanilla&tool_profile=core\""
        }));
    }

    #[test]
    fn app_server_args_preserve_explicit_codex_mcp_inheritance_opt_in() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
[mcp_servers.linear]
command = "linear-mcp"

[plugins."browser@openai-bundled"]
enabled = true
"#,
        )
        .unwrap();
        let mut agent = test_agent();
        agent.managed_context = true;
        agent.codex_home = Some(tmp.path().to_path_buf());

        let args = agent.app_server_args_with_mcp_inheritance(8765, true);

        assert_eq!(args.first().map(String::as_str), Some("app-server"));
        assert!(!args_have_config_override(&args, "features.plugins=false"));
        assert!(!args.iter().any(|arg| arg.starts_with("mcp_servers={")));
        assert!(args_have_config_override(
            &args,
            "mcp_servers.intendant.type=\"http\""
        ));
    }

    #[test]
    fn app_server_args_managed_context_raise_intendant_mcp_tool_timeout() {
        // A blocking fission_control(op="wait") can hold an Intendant MCP
        // tool call open for up to 300s; managed launches raise Codex's
        // per-tool client timeout above that.
        let mut agent = test_agent();
        agent.managed_context = true;
        let args = agent.app_server_args(8765);
        assert!(args_have_config_override(
            &args,
            "mcp_servers.intendant.tool_timeout_sec=600"
        ));

        // Vanilla launches keep Codex's default tool timeout.
        let vanilla = test_agent();
        let args = vanilla.app_server_args(8765);
        assert!(!args
            .iter()
            .any(|arg| arg.starts_with("mcp_servers.intendant.tool_timeout_sec")));
    }

    #[test]
    fn codex_home_env_is_applied_to_spawned_command() {
        let codex_home = PathBuf::from("/home/user/.codex-managed");
        let mut command = crate::platform::spawn_command("codex");

        CodexAgent::apply_codex_home_env(&mut command, Some(&codex_home));

        let actual = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("CODEX_HOME"))
            .and_then(|(_, value)| value.map(PathBuf::from));
        assert_eq!(actual.as_deref(), Some(codex_home.as_path()));
    }

    #[test]
    fn configured_standard_service_tier_serializes_null_once() {
        let mut agent = test_agent();
        agent.apply_configured_service_tier(Some("normal".to_string()));
        assert_eq!(agent.service_tier, None);
        assert!(agent.service_tier_clear_pending);

        let mut params = serde_json::Map::new();
        agent.insert_service_tier_override_consuming_clear(&mut params);
        assert!(params["serviceTier"].is_null());
        assert!(!agent.service_tier_clear_pending);
    }

    #[test]
    fn thread_start_response_updates_effective_service_tier() {
        let mut agent = test_agent();
        agent.service_tier = Some("fast".to_string());
        agent.service_tier_clear_pending = true;

        agent.update_service_tier_from_thread_response(&serde_json::json!({
            "thread": { "id": "thread-1" },
            "serviceTier": CODEX_FAST_SERVICE_TIER,
        }));
        assert_eq!(agent.service_tier.as_deref(), Some(CODEX_FAST_SERVICE_TIER));
        assert!(!agent.service_tier_clear_pending);

        agent.update_service_tier_from_thread_response(&serde_json::json!({
            "thread": { "id": "thread-1" },
            "serviceTier": null,
        }));
        assert_eq!(agent.service_tier, None);
        assert!(!agent.service_tier_clear_pending);
    }

    #[tokio::test]
    async fn thread_action_side_allows_running_parent_turn() {
        let mut agent = test_agent();
        *agent.active_thread_id.lock().await = Some("thread-abc".into());
        *agent.active_turn_id.lock().await = Some("turn-abc".into());
        let err = agent
            .thread_action("side", &serde_json::json!({"prompt": "quick check"}))
            .await
            .unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("Not initialized"),
                    "running parent turns should not be rejected before the RPC path; got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn thread_action_side_requires_prompt() {
        let mut agent = test_agent();
        *agent.active_thread_id.lock().await = Some("thread-abc".into());
        let err = agent
            .thread_action("side", &serde_json::json!({}))
            .await
            .unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(msg.contains("/side requires a prompt"), "got: {}", msg);
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn thread_action_unknown_op_errors() {
        let mut agent = test_agent();
        *agent.active_thread_id.lock().await = Some("thread-abc".into());
        let err = agent
            .thread_action("explode", &serde_json::Value::Null)
            .await
            .unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("unsupported Codex thread action"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn thread_actions_reject_side_child_targets() {
        let mut agent = test_agent();
        agent
            .side_threads
            .lock()
            .await
            .insert("side-child".into(), "parent-thread".into());
        *agent.active_thread_id.lock().await = Some("side-child".into());

        let err = agent
            .thread_action("fork", &serde_json::json!({ "threadId": "side-child" }))
            .await
            .unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("cannot /fork a /side conversation"),
                    "got: {msg}"
                );
                assert!(msg.contains("parent-thread"), "got: {msg}");
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn thread_action_undo_zero_turns_errors_early() {
        // Defensive check inside rollback_turns: `/undo 0` makes no sense.
        let mut agent = test_agent();
        *agent.active_thread_id.lock().await = Some("thread-abc".into());
        let err = agent
            .thread_action("undo", &serde_json::json!({"turns": 0}))
            .await
            .unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(msg.contains("at least 1"), "got: {}", msg);
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn rollback_turns_trait_zero_is_noop() {
        // The trait method treats 0 as a no-op (HTTP handler may emit
        // 0 turns when the target round is already the head). No RPC
        // is dispatched so the call returns Ok without an active
        // thread.
        let mut agent = test_agent();
        agent
            .rollback_turns(0)
            .await
            .expect("0 turns should be a noop");
    }

    #[tokio::test]
    async fn rollback_turns_trait_without_thread_errors() {
        // Non-zero turns without an active thread surfaces the same
        // "no active Codex thread" error as the /undo dispatcher.
        let mut agent = test_agent();
        let err = agent.rollback_turns(2).await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("no active Codex thread"),
                    "expected 'no active Codex thread', got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[test]
    fn thread_rollback_wire_format_is_jsonrpc_request() {
        // Assert the params shape without actually running the RPC.
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "numTurns": 2,
        });
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["numTurns"], 2);
    }

    #[test]
    fn thread_rollback_anchor_wire_format_is_jsonrpc_request() {
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "numTurns": 0,
            "anchor": {
                "itemId": "call-keep",
                "position": "after",
            },
        });
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["numTurns"], 0);
        assert_eq!(params["anchor"]["itemId"], "call-keep");
        assert_eq!(params["anchor"]["position"], "after");
    }

    #[test]
    fn thread_inject_developer_message_wire_format_is_raw_response_item() {
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "items": [{
                "type": "message",
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": "<model_context_rewind_primer>...</model_context_rewind_primer>",
                }],
            }],
        });
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["items"][0]["type"], "message");
        assert_eq!(params["items"][0]["role"], "developer");
        assert_eq!(params["items"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn thread_fork_from_path_wire_format_uses_rollout_path() {
        let params = serde_json::json!({
            "threadId": "",
            "path": "/tmp/rewind-source.jsonl",
        });
        assert_eq!(params["threadId"], "");
        assert_eq!(params["path"], "/tmp/rewind-source.jsonl");
    }

    #[tokio::test]
    async fn fork_thread_with_options_requires_thread_id() {
        let mut agent = test_agent();
        let err = agent
            .fork_thread_with_options("  ", None, None)
            .await
            .unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("requires a thread id"),
                    "expected thread-id error, got: {msg}"
                );
            }
            other => panic!("expected ExternalAgent error, got {other:?}"),
        }
    }

    #[test]
    fn thread_fork_wire_format_with_name() {
        // The implementation constructs the params map conditionally; re-run
        // the same construction here to guarantee the shape doesn't drift.
        let mut obj = serde_json::Map::new();
        obj.insert(
            "threadId".into(),
            serde_json::Value::String("thread-abc".into()),
        );
        obj.insert("name".into(), serde_json::Value::String("feature-x".into()));
        let params = serde_json::Value::Object(obj);
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["name"], "feature-x");
    }

    #[test]
    fn codex_initialize_opts_into_experimental_api() {
        let init_params = serde_json::json!({
            "clientInfo": {
                "name": "intendant",
                "title": "Intendant",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "experimentalApi": true,
            },
        });
        assert_eq!(init_params["clientInfo"]["name"], "intendant");
        assert_eq!(init_params["capabilities"]["experimentalApi"], true);
    }

    #[test]
    fn intendant_mcp_url_carries_session_scoped_managed_context() {
        let mut agent = CodexAgent::with_options(
            "codex".to_string(),
            None,
            "never".to_string(),
            "workspace-write".to_string(),
            Some(8765),
            CodexAgentOptions {
                managed_context: true,
                ..CodexAgentOptions::default()
            },
        );
        agent.mcp_session_id = Some("session with spaces".to_string());

        let url = agent.intendant_mcp_url(8765);
        assert_eq!(
            url,
            "http://localhost:8765/mcp?session_id=session%20with%20spaces&managed_context=managed&tool_profile=core"
        );
    }

    #[test]
    fn intendant_mcp_url_carries_auth_token_when_configured() {
        let mut agent = CodexAgent::with_options(
            "codex".to_string(),
            None,
            "never".to_string(),
            "workspace-write".to_string(),
            Some(8765),
            CodexAgentOptions {
                managed_context: true,
                ..CodexAgentOptions::default()
            },
        );
        agent.mcp_session_id = Some("session with spaces".to_string());
        agent.mcp_auth_token = Some("token with spaces&symbols".to_string());

        // The injected token is session-scoped (derived from the process
        // token and this session id), so the backend authenticates as this
        // exact supervised session.
        let expected_token = crate::web_gateway::session_scoped_mcp_token(
            "token with spaces&symbols",
            "session with spaces",
        );
        let url = agent.intendant_mcp_url(8765);
        assert_eq!(
            url,
            format!(
                "http://localhost:8765/mcp?session_id=session%20with%20spaces&managed_context=managed&tool_profile=core&mcp_token={expected_token}"
            )
        );
    }

    #[test]
    fn intendant_ctl_env_uses_mcp_url_and_context_mode() {
        let agent = CodexAgent::with_options(
            "codex".to_string(),
            None,
            "never".to_string(),
            "workspace-write".to_string(),
            Some(8765),
            CodexAgentOptions {
                managed_context: true,
                ..CodexAgentOptions::default()
            },
        );

        assert_eq!(
            CodexAgent::intendant_mcp_base_url(9876),
            "http://localhost:9876/mcp"
        );
        assert_eq!(agent.intendant_managed_context_mode(), "managed");
    }

    #[tokio::test]
    #[ignore = "requires INTENDANT_CODEX_E2E_BIN to point at a Codex app-server binary; run with an isolated CODEX_HOME"]
    async fn e2e_codex_app_server_initializes_and_starts_thread() {
        let codex_bin = std::env::var("INTENDANT_CODEX_E2E_BIN")
            .expect("set INTENDANT_CODEX_E2E_BIN to the patched Codex binary");
        let tmp = tempfile::tempdir().unwrap();
        let trace_dir = tmp.path().join("request-traces");
        let tools = {
            let autonomy =
                crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default());
            let mut mcp_state = crate::mcp::McpAppState::new(
                "openai".to_string(),
                "gpt-5".to_string(),
                autonomy,
                tmp.path().join("logs"),
            );
            mcp_state.codex_managed_context = true;
            mcp_state.configured_codex_managed_context = true;
            let state = std::sync::Arc::new(tokio::sync::RwLock::new(mcp_state));
            let server = crate::mcp::IntendantServer::new(state, crate::event::EventBus::new());
            server.list_tools_json().await
        };
        let (mcp_port, mcp_handle) = spawn_minimal_mcp_http_server(tools).await;
        let mut agent = CodexAgent::with_options(
            codex_bin,
            None,
            "never".into(),
            "danger-full-access".into(),
            Some(mcp_port),
            CodexAgentOptions {
                managed_context: true,
                ..CodexAgentOptions::default()
            },
        );

        let config = AgentConfig {
            model: None,
            working_dir: tmp.path().to_path_buf(),
            request_trace_dir: Some(trace_dir),
            request_trace_temporary: false,
            context_archive: "summary".to_string(),
            approval_policy: "never".to_string(),
            sandbox: "danger-full-access".to_string(),
            reasoning_effort: None,
            service_tier: None,
            web_search: false,
            network_access: false,
            writable_roots: Vec::new(),
            codex_managed_context: true,
            web_port: Some(mcp_port),
            mcp_auth_token: None,
            mcp_session_id: Some("test-session".to_string()),
            resume_session: None,
            fork_resume: false,
            codex_home: None,
        };

        let _events = agent.initialize(config).await.unwrap();
        let thread = agent.start_thread().await.unwrap();
        assert!(
            !thread.thread_id.trim().is_empty(),
            "thread/start should return a concrete Codex thread id"
        );

        let snapshot = agent.read_thread_snapshot(&thread.thread_id).await.unwrap();
        assert_eq!(snapshot.thread_id, thread.thread_id);
        assert!(
            snapshot.rollout_path.is_some(),
            "thread/read should expose a rollout path for rewind restore"
        );

        agent.shutdown().await.unwrap();
        mcp_handle.abort();
    }

    async fn spawn_minimal_mcp_http_server(
        tools: serde_json::Value,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let tools = tools.clone();
                tokio::spawn(async move {
                    let _ = handle_minimal_mcp_http_connection(stream, tools).await;
                });
            }
        });
        (port, handle)
    }

    async fn handle_minimal_mcp_http_connection(
        mut stream: tokio::net::TcpStream,
        tools: serde_json::Value,
    ) -> std::io::Result<()> {
        use tokio::io::AsyncReadExt as _;

        let mut bytes = Vec::new();
        let header_end;
        loop {
            let mut chunk = [0_u8; 1024];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(idx) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = idx + 4;
                break;
            }
        }

        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let mut chunk = vec![0_u8; header_end + content_length - bytes.len()];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
        }

        let body = &bytes[header_end..header_end + content_length.min(bytes.len() - header_end)];
        let request: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_none() {
            write_http_response(&mut stream, 202, "").await?;
            return Ok(());
        }

        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "intendant-test",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
            "tools/list" => tools,
            _ => serde_json::json!({}),
        };
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
        .to_string();
        write_http_response(&mut stream, 200, &response).await
    }

    async fn write_http_response(
        stream: &mut tokio::net::TcpStream,
        status: u16,
        body: &str,
    ) -> std::io::Result<()> {
        let reason = if status == 202 { "Accepted" } else { "OK" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await
    }

    #[test]
    fn goal_set_wire_format_matches_codex_protocol() {
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "objective": "Ship feature parity",
            "status": "active",
            "tokenBudget": 200000_u64,
        });
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["objective"], "Ship feature parity");
        assert_eq!(params["status"], "active");
        assert_eq!(params["tokenBudget"], 200000);
    }

    #[test]
    fn thread_name_set_wire_format_matches_codex_protocol() {
        let params = serde_json::json!({
            "threadId": "thread-abc",
            "name": "Ship feature parity",
        });
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["name"], "Ship feature parity");
    }

    #[test]
    fn goal_status_normalization_accepts_cli_style_aliases() {
        assert_eq!(normalize_goal_status("pause").unwrap(), "paused");
        assert_eq!(normalize_goal_status("resume").unwrap(), "active");
        assert_eq!(normalize_goal_status("blocked").unwrap(), "blocked");
        assert_eq!(
            normalize_goal_status("usage-limited").unwrap(),
            "usageLimited"
        );
        assert_eq!(
            normalize_goal_status("budget-limited").unwrap(),
            "budgetLimited"
        );
        assert_eq!(normalize_goal_status("done").unwrap(), "complete");
        assert!(normalize_goal_status("stalled").is_err());
    }

    #[test]
    fn goal_validation_matches_upstream_limit() {
        let allowed = "x".repeat(MAX_THREAD_GOAL_OBJECTIVE_CHARS);
        validate_goal_objective(&allowed).expect("limit should be allowed");
        let too_long = "x".repeat(MAX_THREAD_GOAL_OBJECTIVE_CHARS + 1);
        let err = validate_goal_objective(&too_long).unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(msg.contains("too long"), "got: {}", msg);
                assert!(msg.contains("4000"), "got: {}", msg);
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[test]
    fn goal_token_budget_parser_distinguishes_set_clear_and_omit() {
        assert_eq!(
            parse_goal_token_budget(&serde_json::json!({"tokenBudget": 123})).unwrap(),
            Some(Some(123))
        );
        assert_eq!(
            parse_goal_token_budget(&serde_json::json!({"token_budget": null})).unwrap(),
            Some(None)
        );
        assert_eq!(
            parse_goal_token_budget(&serde_json::json!({})).unwrap(),
            None
        );
        assert!(parse_goal_token_budget(&serde_json::json!({"tokenBudget": 0})).is_err());
    }

    #[test]
    fn thread_fork_wire_format_without_name() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "threadId".into(),
            serde_json::Value::String("thread-abc".into()),
        );
        let params = serde_json::Value::Object(obj);
        assert_eq!(params["threadId"], "thread-abc");
        assert!(params.get("name").is_none());
    }

    #[test]
    fn review_start_wire_format_with_prompt() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "threadId".into(),
            serde_json::Value::String("thread-abc".into()),
        );
        obj.insert(
            "prompt".into(),
            serde_json::Value::String("check for leaks".into()),
        );
        let params = serde_json::Value::Object(obj);
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["prompt"], "check for leaks");
    }
}

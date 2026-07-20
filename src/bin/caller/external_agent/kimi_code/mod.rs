//! Kimi Code external-agent adapter.
//!
//! The adapter supervises `kimi server run` rather than the narrower ACP
//! facade. Kimi's authenticated REST/WS surface exposes native fork/undo/
//! compaction, true queued-prompt steering, goals, side agents, background
//! tasks, structured approvals/questions, multimodal files, live profile
//! switches, and full sub-agent telemetry.

mod bridge;
mod events;
mod review;
mod rpc;
mod websocket;
mod wire;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;

use crate::error::CallerError;

pub(crate) use self::bridge::sync_managed_bridges_to_primary;
use self::bridge::{
    choose_mcp_server_name, prepare_bridge_home, sync_bridge_home_to_primary, BridgeMcpConfig,
};
use self::events::{
    child_thread_id, normalize_goal_status, question_answer_body, split_child_thread_id,
    KimiSharedState,
};
use self::rpc::{KimiGoalBudgetLimits, KimiRpcApi, KimiRpcModel, KimiRpcTool};
use self::websocket::{await_driver_shutdown, spawn_driver, WsCommand};
use self::wire::{
    active_prompt_id, external, pending_prompt_ids, prompt_is_pending, validate_meta, KimiApi,
};
use super::{
    AgentAttachment, AgentConfig, AgentContextSnapshot, AgentContextTokenCountKind, AgentEvent,
    AgentImageAttachment, AgentThread, AgentThreadSnapshot, ApprovalDecision,
    AutonomousGoalPauseResult, ExternalAgent,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(25);
const KIMI_MAIN_AGENT_ID: &str = "main";
const REVIEW_PROMPT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const JSON_SAFE_INTEGER_MAX: u64 = 9_007_199_254_740_991;
const KIMI_REVIEW_CONTRACT: &str = "\
Perform a read-only code review. Identify concrete correctness, security, \
reliability, performance, and test-coverage problems. Report findings with \
file and line references, ordered by severity. Do not modify files, run \
commands, invoke skills, select additional tools, launch tasks or agents, \
change goals, schedules, or configuration, or call MCP tools. If no concrete \
finding remains, say so explicitly.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiLaunchConfig {
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub permission_mode: String,
    /// `None` delegates to the selected Kimi profile, `Some([])` disables
    /// all optional tools, and a non-empty list is the exact active set.
    pub allowed_tools: Option<Vec<String>>,
    pub plan_mode: bool,
    pub swarm_mode: bool,
}

pub struct KimiCodeAgent {
    command: String,
    launch: KimiLaunchConfig,
    web_port: Option<u16>,
    working_dir: Option<PathBuf>,
    child: Option<Child>,
    child_pid: Option<u32>,
    api: Option<KimiApi>,
    rpc: Option<KimiRpcApi>,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    ws_tx: Option<mpsc::UnboundedSender<WsCommand>>,
    ws_handle: Option<tokio::task::JoinHandle<()>>,
    stdout_handle: Option<tokio::task::JoinHandle<()>>,
    stderr_handle: Option<tokio::task::JoinHandle<()>>,
    shared: Arc<KimiSharedState>,
    protocol_watch: Option<super::protocol_watch::ProtocolWatchHandle>,
    resume_session: Option<String>,
    fork_resume: bool,
    /// One-shot anchor-fork staging. Kimi can only fork at the persisted
    /// head, so an arbitrary turn boundary is composed as head-fork followed
    /// by one atomic native undo on the newly materialized idle child.
    fork_rollback_turns: Option<u32>,
    fork_expected_horizon:
        Option<crate::web_gateway::session_catalog::kimi_history::KimiTurnHorizon>,
    mcp_auth_token: Option<String>,
    mcp_session_id: Option<String>,
    bridge_home: Option<PathBuf>,
    review_lease: Arc<tokio::sync::Mutex<Option<KimiReviewToolLease>>>,
    review_monitor: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Clone)]
struct KimiReviewToolLease {
    nonce: String,
    session_id: String,
    agent_id: String,
    prompt_id: Option<String>,
    baseline_prompt_ids: HashSet<String>,
    previous_tools: Vec<String>,
    review_tools: Vec<String>,
}

impl KimiCodeAgent {
    pub fn new(command: String, launch: KimiLaunchConfig, web_port: Option<u16>) -> Self {
        Self {
            command,
            launch,
            web_port,
            working_dir: None,
            child: None,
            child_pid: None,
            api: None,
            rpc: None,
            event_tx: None,
            ws_tx: None,
            ws_handle: None,
            stdout_handle: None,
            stderr_handle: None,
            shared: Arc::new(KimiSharedState::default()),
            protocol_watch: None,
            resume_session: None,
            fork_resume: false,
            fork_rollback_turns: None,
            fork_expected_horizon: None,
            mcp_auth_token: None,
            mcp_session_id: None,
            bridge_home: None,
            review_lease: Arc::new(tokio::sync::Mutex::new(None)),
            review_monitor: None,
        }
    }

    fn api(&self) -> Result<&KimiApi, CallerError> {
        self.api
            .as_ref()
            .ok_or_else(|| external("Kimi agent is not initialized"))
    }

    fn rpc(&self) -> Result<&KimiRpcApi, CallerError> {
        self.rpc
            .as_ref()
            .ok_or_else(|| external("Kimi agent RPC is not initialized"))
    }

    fn current_session_id(&self) -> Result<String, CallerError> {
        self.shared
            .session_id()
            .ok_or_else(|| external("Kimi session has not been started"))
    }

    fn emit(&self, event: AgentEvent) {
        if let Some(tx) = self.event_tx.as_ref() {
            let _ = tx.send(event);
        }
    }

    async fn fork_resumed_session(&mut self, parent: &str) -> Result<Value, CallerError> {
        let expected = self.fork_expected_horizon.take();
        if self.fork_rollback_turns.is_some() && expected.is_none() {
            return Err(external(
                "refusing Kimi anchor fork without its expected-head horizon",
            ));
        }
        let mut rollback_target = None;
        if let Some(expected) = expected.as_ref() {
            self.verify_fork_horizon(parent, expected).await?;
            if let Some(count) = self.fork_rollback_turns {
                if count > expected.undoable_turns {
                    return Err(external(format!(
                        "refusing Kimi anchor fork rollback beyond the verified undo horizon \
                         (requested {count}, available {})",
                        expected.undoable_turns
                    )));
                }
                rollback_target = Some(expected.after_rollback(count).ok_or_else(|| {
                    external(
                        "refusing Kimi anchor fork because its expected-head horizon \
                         cannot prove the exact post-undo target",
                    )
                })?);
            }
        }
        let session = self
            .api()?
            .session_action(parent, "fork", serde_json::json!({}))
            .await?;
        let session_id = session
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| external("Kimi fork response omitted session id"))?
            .trim()
            .to_string();
        if session_id.is_empty() || session_id == parent {
            return Err(external(
                "Kimi fork response did not identify a distinct child session",
            ));
        }

        // Kimi 0.27's fork response is returned only after the child session
        // and its agents have been materialized from copied wire history.
        // `:undo` prechecks the entire count before mutating that history, so
        // this one request is atomic and leaves the parent untouched. This
        // helper runs before shared-state publication and WS subscription: no
        // consumer or first prompt can observe the temporary head-fork state.
        if let Some(expected) = expected.as_ref() {
            if let Err(error) = self.verify_fork_horizon(&session_id, expected).await {
                let _ = self
                    .api()?
                    .session_action(&session_id, "archive", serde_json::json!({}))
                    .await;
                return Err(error);
            }
        }
        if let Some(count) = self.fork_rollback_turns.take() {
            if let Err(error) = self
                .api()?
                .session_action(&session_id, "undo", serde_json::json!({ "count": count }))
                .await
            {
                // Kimi has no session-delete REST route. Hide a failed staging
                // child from normal history where possible, while preserving
                // the original undo error for diagnosis.
                let _ = self
                    .api()?
                    .session_action(&session_id, "archive", serde_json::json!({}))
                    .await;
                return Err(error);
            }
            let target = rollback_target
                .as_ref()
                .ok_or_else(|| external("Kimi anchor fork lost its verified post-undo target"))?;
            if let Err(error) = self.verify_fork_horizon(&session_id, target).await {
                let _ = self
                    .api()?
                    .session_action(&session_id, "archive", serde_json::json!({}))
                    .await;
                return Err(external(format!(
                    "Kimi fork rollback did not materialize the verified target horizon: {error}"
                )));
            }
        }
        Ok(session)
    }

    async fn verify_fork_horizon(
        &self,
        session_id: &str,
        expected: &crate::web_gateway::session_catalog::kimi_history::KimiTurnHorizon,
    ) -> Result<(), CallerError> {
        let bridge = self
            .bridge_home
            .as_deref()
            .ok_or_else(|| external("Kimi bridge home is unavailable for fork validation"))?;
        // The server materializes forked wire data before replying, but use a
        // short bounded retry for filesystems whose directory visibility
        // trails the REST response.
        let mut last_mismatch = None;
        for attempt in 0..20 {
            if let Some(actual) =
                crate::web_gateway::session_catalog::kimi_history::kimi_turn_horizon_in(
                    bridge, session_id,
                )
            {
                match verify_expected_horizon(expected, &actual) {
                    Ok(()) => return Ok(()),
                    Err(error) => last_mismatch = Some(error),
                }
            }
            if attempt < 19 {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        if let Some(error) = last_mismatch {
            return Err(error);
        }
        Err(external(format!(
            "Kimi session {session_id} did not materialize a verifiable fork horizon"
        )))
    }

    fn intendant_mcp_url(&self, port: u16, include_token: bool) -> String {
        super::intendant_bootstrap_mcp_url(
            port,
            self.mcp_session_id.as_deref(),
            None,
            include_token
                .then_some(self.mcp_auth_token.as_deref())
                .flatten(),
        )
    }

    fn session_agent_config(&self) -> Value {
        let mut config = serde_json::Map::new();
        if let Some(model) = self
            .launch
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            config.insert("model".into(), Value::String(model.to_string()));
        }
        if let Some(thinking) = self
            .launch
            .thinking
            .as_deref()
            .map(str::trim)
            .filter(|thinking| !thinking.is_empty())
        {
            config.insert("thinking".into(), Value::String(thinking.to_string()));
        }
        if let Some(tools) = self.launch.allowed_tools.as_ref() {
            config.insert(
                "tools".into(),
                Value::Array(tools.iter().cloned().map(Value::String).collect()),
            );
        }
        config.insert(
            "permission_mode".into(),
            Value::String(self.launch.permission_mode.clone()),
        );
        config.insert("plan_mode".into(), Value::Bool(self.launch.plan_mode));
        config.insert("swarm_mode".into(), Value::Bool(self.launch.swarm_mode));
        Value::Object(config)
    }

    async fn submit_content(
        &mut self,
        thread: &AgentThread,
        content: Vec<Value>,
    ) -> Result<Value, CallerError> {
        let current = self.current_session_id()?;
        let (session_id, agent_id) = match split_child_thread_id(&thread.thread_id) {
            Some((session, agent)) => (session.to_string(), agent.to_string()),
            None => (thread.thread_id.clone(), KIMI_MAIN_AGENT_ID.to_string()),
        };
        if session_id != current {
            return Err(external(format!(
                "Kimi thread {} does not belong to active session {}",
                thread.thread_id, current
            )));
        }
        let mut overrides = serde_json::Map::new();
        if agent_id != KIMI_MAIN_AGENT_ID {
            overrides.insert("agent_id".into(), Value::String(agent_id.clone()));
            self.shared.set_active_agent_id(Some(agent_id.clone()));
        } else {
            self.shared.set_active_agent_id(None);
        }
        let result = self
            .api()?
            .submit_prompt(&session_id, content, Value::Object(overrides))
            .await?;
        let status = result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("running");
        if status == "blocked" {
            return Err(external("Kimi blocked the prompt before starting a turn"));
        }
        if let Some(prompt_id) = result.get("prompt_id").and_then(Value::as_str) {
            self.shared
                .set_prompt_id(&session_id, &agent_id, Some(prompt_id.to_string()));
        }
        Ok(result)
    }

    async fn attachment_content(
        &self,
        message: &str,
        attachments: &[AgentAttachment],
    ) -> Result<Vec<Value>, CallerError> {
        let mut content = vec![serde_json::json!({ "type": "text", "text": message })];
        for attachment in attachments {
            match attachment {
                AgentAttachment::Image(image) => {
                    let data = if !image.base64.is_empty() {
                        image.base64.clone()
                    } else if let Some(path) = image.local_path.as_ref() {
                        let bytes = tokio::fs::read(path).await.map_err(|error| {
                            external(format!(
                                "failed to read Kimi image attachment {}: {error}",
                                path.display()
                            ))
                        })?;
                        base64::engine::general_purpose::STANDARD.encode(bytes)
                    } else {
                        return Err(external("Kimi image attachment has no data"));
                    };
                    content.push(serde_json::json!({
                        "type": "image",
                        "source": {
                            "kind": "base64",
                            "media_type": image.mime_type,
                            "data": data,
                        }
                    }));
                }
                AgentAttachment::File(file) => {
                    let uploaded = self
                        .api()?
                        .upload_file(&file.local_path, &file.name, &file.mime_type)
                        .await?;
                    let file_id = uploaded
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| external("Kimi file upload omitted file id"))?;
                    content.push(serde_json::json!({
                        "type": "file",
                        "file_id": file_id,
                        "name": uploaded
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(&file.name),
                        "media_type": uploaded
                            .get("media_type")
                            .and_then(Value::as_str)
                            .unwrap_or(&file.mime_type),
                        "size": uploaded
                            .get("size")
                            .and_then(Value::as_u64)
                            .unwrap_or(file.size),
                    }));
                }
            }
        }
        Ok(content)
    }

    async fn pending_question(&self, request_id: &str) -> Result<Option<Value>, CallerError> {
        if let Some(question) = self.shared.question(request_id) {
            return Ok(Some(question.request));
        }
        let session_id = self.current_session_id()?;
        let listed = self.api()?.list_questions(&session_id).await?;
        Ok(find_interaction(&listed, "question_id", request_id))
    }

    async fn current_prompt(&self) -> Result<Option<String>, CallerError> {
        let session_id = self.current_session_id()?;
        let agent_id = self
            .shared
            .active_agent_id()
            .unwrap_or_else(|| KIMI_MAIN_AGENT_ID.to_string());
        if let Some(prompt_id) = self.shared.prompt_id(&session_id, &agent_id) {
            return Ok(Some(prompt_id));
        }
        // Kimi 0.27's prompt-list route exposes only the main agent. Falling
        // back to it while a child thread is selected could make a child
        // interrupt/steer target the parent's prompt.
        if agent_id != KIMI_MAIN_AGENT_ID {
            return Ok(None);
        }
        let prompts = self.api()?.list_prompts(&session_id).await?;
        Ok(active_prompt_id(&prompts))
    }

    async fn update_profile(&mut self, patch: Value) -> Result<Value, CallerError> {
        let session_id = self.current_session_id()?;
        self.api()?.update_profile(&session_id, patch).await
    }

    async fn emit_session_warnings(&self, session_id: &str) {
        match self.api().cloned() {
            Ok(api) => match api.warnings(session_id).await {
                Ok(value) => {
                    for warning in value
                        .get("warnings")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let severity = warning
                            .get("severity")
                            .and_then(Value::as_str)
                            .unwrap_or("warning");
                        let level = match severity {
                            "info" => "info",
                            "error" => "error",
                            _ => "warn",
                        };
                        let code = warning
                            .get("code")
                            .and_then(Value::as_str)
                            .map(|code| bounded_wire_text(code, 128));
                        let message = bounded_wire_text(
                            warning
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("Kimi session warning"),
                            4096,
                        );
                        self.emit(AgentEvent::Log {
                            level: level.into(),
                            message: code
                                .map(|code| format!("Kimi warning {code}: {message}"))
                                .unwrap_or_else(|| format!("Kimi warning: {message}")),
                        });
                    }
                }
                Err(error) => self.emit(AgentEvent::Log {
                    level: "warn".into(),
                    message: format!(
                        "Could not inspect Kimi session warnings: {}",
                        bounded_wire_text(&error.to_string(), 1024)
                    ),
                }),
            },
            Err(error) => self.emit(AgentEvent::Log {
                level: "warn".into(),
                message: bounded_wire_text(&error.to_string(), 1024),
            }),
        }
    }

    async fn task_action(&mut self, op: &str, params: &Value) -> Result<String, CallerError> {
        let session_id = self.action_session_id(params)?;
        if matches!(op, "tasks" | "task-list" | "task_list") {
            let listed = self.api()?.list_tasks(&session_id).await?;
            let items = listed
                .get("items")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if items.is_empty() {
                return Ok("no Kimi background tasks".into());
            }
            let lines = items
                .iter()
                .take(128)
                .map(|task| {
                    let id = bounded_wire_text(
                        task.get("taskId")
                            .or_else(|| task.get("task_id"))
                            .or_else(|| task.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        256,
                    );
                    let status = bounded_wire_text(
                        task.get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        64,
                    );
                    let description = bounded_wire_text(
                        task.get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("Kimi background task"),
                        512,
                    );
                    format!("{id}\t{status}\t{description}")
                })
                .collect::<Vec<_>>();
            return Ok(lines.join("\n"));
        }
        let task_id = string_param(params, &["task_id", "taskId", "id"], "task id")?;
        match op {
            "task-output" | "task_output" => {
                let output_bytes = params
                    .get("output_bytes")
                    .or_else(|| params.get("outputBytes"))
                    .or_else(|| params.get("max_bytes"))
                    .and_then(Value::as_u64)
                    .unwrap_or(65_536)
                    .min(1_048_576) as usize;
                let task = self
                    .api()?
                    .task(&session_id, &task_id, output_bytes)
                    .await?;
                let output = task
                    .get("output_preview")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let total = task.get("output_bytes").and_then(Value::as_u64);
                crate::background_tasks::record_inline_output(
                    &session_id,
                    &task_id,
                    output.as_bytes(),
                    total,
                );
                let output = bounded_wire_text(output, output_bytes.max(1));
                let status = bounded_wire_text(
                    task.get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    64,
                );
                Ok(match (output.is_empty(), total) {
                    (true, Some(total)) => {
                        format!("Kimi task {task_id} is {status}; output is empty ({total} bytes)")
                    }
                    (true, None) => format!("Kimi task {task_id} is {status}; no output available"),
                    (false, Some(total)) => {
                        format!("Kimi task {task_id} is {status} ({total} bytes):\n{output}")
                    }
                    (false, None) => format!("Kimi task {task_id} is {status}:\n{output}"),
                })
            }
            "task-cancel" | "task_cancel" => {
                let result = self.api()?.cancel_task(&session_id, &task_id).await?;
                if result.get("cancelled").and_then(Value::as_bool) == Some(true) {
                    Ok(format!("cancelled Kimi task {task_id}"))
                } else {
                    let status = result
                        .get("status")
                        .and_then(Value::as_str)
                        .map(|value| bounded_wire_text(value, 64))
                        .unwrap_or_else(|| "terminal".into());
                    Ok(format!(
                        "Kimi task {task_id} was already {status}; no cancellation was needed"
                    ))
                }
            }
            _ => Err(external(format!("unsupported Kimi task action /{op}"))),
        }
    }

    async fn refresh_profile_facts(
        &mut self,
        session_id: &str,
        echoed: bool,
    ) -> Result<(), CallerError> {
        let profile = self.rpc()?.profile(session_id, KIMI_MAIN_AGENT_ID).await?;
        self.launch.model = profile.model_alias;
        self.launch.thinking = profile.thinking_level;
        self.launch.allowed_tools = match profile.active_tool_names {
            Some(names) => Some(sorted_unique_tool_names(names)),
            None => Some(active_tool_names(
                &self.rpc()?.tools(session_id, KIMI_MAIN_AGENT_ID).await?,
            )),
        };
        self.emit_config_facts(echoed);
        Ok(())
    }

    async fn tools_report(&self, session_id: &str, agent_id: &str) -> Result<String, CallerError> {
        let mut tools = self.rpc()?.tools(session_id, agent_id).await?;
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        if tools.is_empty() {
            return Ok("Kimi reports no registered tools".into());
        }
        let lines = tools
            .into_iter()
            .map(|tool| {
                format!(
                    "{}\t{}\t{}",
                    if tool.active { "active" } else { "inactive" },
                    bounded_wire_text(&tool.name, 256),
                    bounded_wire_text(&tool.source, 128)
                )
            })
            .collect::<Vec<_>>();
        let active = lines
            .iter()
            .filter(|line| line.starts_with("active\t"))
            .count();
        Ok(format!(
            "{}Kimi tool inventory:\n{}",
            if active == 0 {
                "Kimi reports no active tools.\n"
            } else {
                ""
            },
            lines.join("\n")
        ))
    }

    async fn set_active_tools_checked(
        &mut self,
        session_id: &str,
        agent_id: &str,
        names: Vec<String>,
    ) -> Result<String, CallerError> {
        let inventory = self.rpc()?.tools(session_id, agent_id).await?;
        validate_requested_tool_names(&inventory, &names)?;
        self.rpc()?
            .set_active_tools(session_id, agent_id, &names)
            .await?;
        if agent_id == KIMI_MAIN_AGENT_ID {
            self.launch.allowed_tools = Some(names.clone());
        }
        if names.is_empty() {
            Ok("all optional Kimi tools disabled".into())
        } else {
            Ok(format!(
                "Kimi active tools replaced with: {}",
                names.join(", ")
            ))
        }
    }

    async fn tools_action(
        &mut self,
        op: &str,
        session_id: &str,
        agent_id: &str,
        params: &Value,
    ) -> Result<String, CallerError> {
        if matches!(op, "tools-set" | "tools_set" | "tools-all" | "tools_all")
            && self.review_lease.lock().await.is_some()
        {
            return Err(external(
                "cannot change Kimi's active tools while an enforced read-only review is pending",
            ));
        }
        match op {
            "tools" | "tool-list" | "tool_list" => self.tools_report(session_id, agent_id).await,
            "tools-set" | "tools_set" => {
                let names = exact_tool_names_param(params)?;
                self.set_active_tools_checked(session_id, agent_id, names)
                    .await
            }
            "tools-all" | "tools_all" => {
                let inventory = self.rpc()?.tools(session_id, agent_id).await?;
                let names =
                    sorted_unique_tool_names(inventory.into_iter().map(|tool| tool.name).collect());
                self.rpc()?
                    .set_active_tools(session_id, agent_id, &names)
                    .await?;
                if agent_id == KIMI_MAIN_AGENT_ID {
                    self.launch.allowed_tools = Some(names.clone());
                }
                Ok(format!("all {} registered Kimi tools enabled", names.len()))
            }
            _ => Err(external(format!("unsupported Kimi tool action /{op}"))),
        }
    }

    async fn models_report(&self) -> Result<String, CallerError> {
        let mut models = self.rpc()?.models().await?;
        models.sort_by(|left, right| left.model.cmp(&right.model));
        if models.is_empty() {
            return Ok("Kimi reports no configured models".into());
        }
        let lines = models
            .into_iter()
            .map(|model| {
                format!(
                    "{}\t{}\t{}\t{} tokens",
                    bounded_wire_text(&model.model, 256),
                    bounded_wire_text(model.display_name.as_deref().unwrap_or("-"), 256),
                    bounded_wire_text(&model.provider, 128),
                    model.max_context_size
                )
            })
            .collect::<Vec<_>>();
        Ok(format!("Kimi model catalog:\n{}", lines.join("\n")))
    }

    async fn review_action(&mut self, params: &Value) -> Result<String, CallerError> {
        {
            let lease = self.review_lease.lock().await;
            if lease.is_some() {
                return Err(external(
                    "a Kimi read-only review is already active or queued",
                ));
            }
        }
        let session_id = self.current_session_id()?;
        let api = self.api()?.clone();
        let rpc = self.rpc()?.clone();
        let working_dir = self
            .working_dir
            .as_deref()
            .ok_or_else(|| external("Kimi review has no workspace"))?;
        let baseline_prompt_ids = pending_prompt_ids(&api.list_prompts(&session_id).await?)?;
        if !baseline_prompt_ids.is_empty() {
            return Err(external(
                "Kimi read-only review requires an idle session; stop or finish active and queued prompts first",
            ));
        }
        let evidence = review::build_review_evidence(working_dir)
            .await
            .map_err(|error| external(format!("failed to build Kimi review evidence: {error}")))?;
        let inventory = rpc.tools(&session_id, KIMI_MAIN_AGENT_ID).await?;
        let previous_tools = active_tool_names(&inventory);

        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let mut lease = KimiReviewToolLease {
            nonce: nonce.clone(),
            session_id: session_id.clone(),
            agent_id: KIMI_MAIN_AGENT_ID.to_string(),
            prompt_id: None,
            baseline_prompt_ids,
            previous_tools,
            // Kimi's built-in Read/Glob/Grep accept absolute paths, including
            // its own data home. The only honest enforced-review boundary is
            // an exactly empty live tool set plus controller-built evidence.
            review_tools: Vec::new(),
        };
        {
            let mut slot = self.review_lease.lock().await;
            if slot.is_some() {
                return Err(external(
                    "a Kimi read-only review became active concurrently",
                ));
            }
            *slot = Some(lease.clone());
        }

        if let Err(error) = rpc
            .set_active_tools(&session_id, KIMI_MAIN_AGENT_ID, &lease.review_tools)
            .await
        {
            clear_review_lease_if_nonce(&self.review_lease, &nonce).await;
            return Err(error);
        }
        let confined = rpc.tools(&session_id, KIMI_MAIN_AGENT_ID).await;
        match confined {
            Ok(inventory) if active_tool_names(&inventory) == lease.review_tools => {}
            Ok(_) => {
                let _ = restore_review_tools_if_unchanged(&rpc, &lease).await;
                clear_review_lease_if_nonce(&self.review_lease, &nonce).await;
                return Err(external(
                    "Kimi did not confirm the exactly empty review tool set; no review was submitted",
                ));
            }
            Err(error) => {
                // No prompt exists yet. Leave the safer set in place if it
                // cannot be inspected; the caller can explicitly change it.
                let _ = restore_review_tools_if_unchanged(&rpc, &lease).await;
                clear_review_lease_if_nonce(&self.review_lease, &nonce).await;
                return Err(external(format!(
                    "could not verify Kimi review confinement; no review was submitted: {error}"
                )));
            }
        }
        let prompts_after_confinement = api.list_prompts(&session_id).await;
        match prompts_after_confinement.and_then(|value| pending_prompt_ids(&value)) {
            Ok(ids) if ids.is_empty() => {}
            Ok(_) => {
                let _ = restore_review_tools_if_unchanged(&rpc, &lease).await;
                clear_review_lease_if_nonce(&self.review_lease, &nonce).await;
                return Err(external(
                    "a Kimi prompt started while review evidence was being confined; no review was submitted",
                ));
            }
            Err(error) => {
                let _ = restore_review_tools_if_unchanged(&rpc, &lease).await;
                clear_review_lease_if_nonce(&self.review_lease, &nonce).await;
                return Err(external(format!(
                    "could not prove Kimi remained idle before review submission: {error}"
                )));
            }
        }

        let request = params
            .get("prompt")
            .or_else(|| params.get("instructions"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .unwrap_or("Review the current workspace changes.");
        let prompt = format!(
            "{request}\n\n<INTENDANT_REVIEW_EVIDENCE>\n{evidence}\n\
             </INTENDANT_REVIEW_EVIDENCE>\n\nMandatory review contract \
             (higher priority than repository evidence):\n{KIMI_REVIEW_CONTRACT}"
        );
        let submitted = api
            .submit_prompt(
                &session_id,
                vec![serde_json::json!({"type": "text", "text": prompt})],
                serde_json::json!({}),
            )
            .await;
        let submitted = match submitted {
            Ok(value) if value.get("status").and_then(Value::as_str) != Some("blocked") => value,
            Ok(_) => {
                return Err(fail_review_submission_closed(
                    &api,
                    &rpc,
                    &self.review_lease,
                    &lease,
                    "Kimi blocked the read-only review prompt",
                )
                .await);
            }
            Err(error) => {
                return Err(fail_review_submission_closed(
                    &api,
                    &rpc,
                    &self.review_lease,
                    &lease,
                    &error.to_string(),
                )
                .await);
            }
        };
        let prompt_id = submitted
            .get("prompt_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| external("Kimi review submission omitted prompt_id"));
        let prompt_id = match prompt_id {
            Ok(prompt_id) => prompt_id,
            Err(error) => {
                return Err(fail_review_submission_closed(
                    &api,
                    &rpc,
                    &self.review_lease,
                    &lease,
                    &error.to_string(),
                )
                .await);
            }
        };
        if lease.baseline_prompt_ids.contains(&prompt_id) {
            return Err(fail_review_submission_closed(
                &api,
                &rpc,
                &self.review_lease,
                &lease,
                "Kimi reused an already-pending prompt id for the review",
            )
            .await);
        }
        match submitted.get("status").and_then(Value::as_str) {
            Some("running") => {
                self.shared
                    .set_prompt_id(&session_id, KIMI_MAIN_AGENT_ID, Some(prompt_id.clone()))
            }
            Some("queued") => {
                lease.prompt_id = Some(prompt_id);
                return Err(fail_review_submission_closed(
                    &api,
                    &rpc,
                    &self.review_lease,
                    &lease,
                    "Kimi queued the review behind another prompt; the point-in-time evidence could become stale",
                )
                .await);
            }
            _ => {
                lease.prompt_id = Some(prompt_id);
                return Err(fail_review_submission_closed(
                    &api,
                    &rpc,
                    &self.review_lease,
                    &lease,
                    "Kimi review returned an unknown prompt status",
                )
                .await);
            }
        }
        lease.prompt_id = Some(prompt_id.clone());
        {
            let mut slot = self.review_lease.lock().await;
            if slot.as_ref().is_some_and(|current| current.nonce == nonce) {
                *slot = Some(lease.clone());
            } else {
                drop(slot);
                return Err(fail_review_submission_closed(
                    &api,
                    &rpc,
                    &self.review_lease,
                    &lease,
                    "Kimi review lease was lost before monitoring",
                )
                .await);
            }
        }

        if let Some(handle) = self.review_monitor.take() {
            handle.abort();
        }
        let api = self.api()?.clone();
        let rpc = self.rpc()?.clone();
        let leases = Arc::clone(&self.review_lease);
        let events = self.event_tx.clone();
        self.review_monitor = Some(tokio::spawn(async move {
            monitor_review_prompt(api, rpc, leases, events, lease).await;
        }));
        Ok(format!(
            "Kimi enforced tool-free read-only review started as prompt {prompt_id}"
        ))
    }

    fn emit_config_facts(&self, echoed: bool) {
        self.emit(AgentEvent::ConfigFacts {
            facts: crate::types::SessionConfigVitals {
                model: self.launch.model.clone(),
                effort: self.launch.thinking.clone(),
                permission_mode: Some(self.launch.permission_mode.clone()),
                permission_kind: kimi_permission_kind(&self.launch.permission_mode)
                    .map(str::to_string),
                permission_echoed: echoed,
            },
        });
    }

    async fn set_model(&mut self, params: &Value) -> Result<String, CallerError> {
        let requested = string_param(params, &["model", "value"], "model")?;
        let models = self.rpc()?.models().await?;
        let model = resolve_catalog_model(&models, &requested)?.model.clone();
        let result = self
            .rpc()?
            .set_model(&self.current_session_id()?, KIMI_MAIN_AGENT_ID, &model)
            .await?;
        self.launch.model = Some(result.model.clone());
        self.emit_config_facts(true);
        Ok(format!(
            "model switched to {} for the running Kimi session",
            result.model
        ))
    }

    async fn toggle_fast_model(&mut self) -> Result<String, CallerError> {
        let session_id = self.current_session_id()?;
        let profile = self.rpc()?.profile(&session_id, KIMI_MAIN_AGENT_ID).await?;
        let current_alias = profile
            .model_alias
            .as_deref()
            .ok_or_else(|| external("Kimi profile omitted its current model alias"))?;
        let models = self.rpc()?.models().await?;
        let current = resolve_catalog_model(&models, current_alias)?;
        let target = paired_fast_model(&models, current)?;
        let result = self
            .rpc()?
            .set_model(&session_id, KIMI_MAIN_AGENT_ID, &target.model)
            .await?;
        self.launch.model = Some(result.model.clone());
        self.launch.thinking = profile.thinking_level;
        self.launch.allowed_tools = Some(match profile.active_tool_names {
            Some(names) => sorted_unique_tool_names(names),
            None => active_tool_names(&self.rpc()?.tools(&session_id, KIMI_MAIN_AGENT_ID).await?),
        });
        self.emit_config_facts(true);
        Ok(format!(
            "Kimi fast mode {} via {}",
            if model_is_highspeed(target) {
                "enabled"
            } else {
                "disabled"
            },
            result.model
        ))
    }

    async fn set_thinking(&mut self, params: &Value) -> Result<String, CallerError> {
        let thinking = string_param(params, &["thinking", "effort", "value"], "thinking")?;
        self.update_profile(serde_json::json!({ "thinking": thinking }))
            .await?;
        self.launch.thinking = Some(thinking.clone());
        self.emit_config_facts(true);
        Ok(format!(
            "thinking effort switched to {thinking} for the running Kimi session"
        ))
    }

    async fn set_permission(&mut self, params: &Value) -> Result<String, CallerError> {
        let mode = string_param(
            params,
            &["mode", "permission_mode", "value"],
            "permission mode",
        )?;
        validate_permission_mode(&mode)?;
        self.update_profile(serde_json::json!({ "permission_mode": mode }))
            .await?;
        self.launch.permission_mode = mode.clone();
        self.emit_config_facts(true);
        Ok(format!(
            "permission mode switched to {mode} for the running Kimi session"
        ))
    }

    async fn set_boolean_mode(
        &mut self,
        params: &Value,
        field: &str,
        label: &str,
    ) -> Result<String, CallerError> {
        let enabled = boolean_param(params, field)?;
        self.update_profile(serde_json::json!({ field: enabled }))
            .await?;
        match field {
            "plan_mode" => self.launch.plan_mode = enabled,
            "swarm_mode" => self.launch.swarm_mode = enabled,
            _ => {}
        }
        Ok(format!(
            "{label} {} for the running Kimi session",
            if enabled { "enabled" } else { "disabled" }
        ))
    }

    async fn goal_action(&mut self, op: &str, params: &Value) -> Result<String, CallerError> {
        let session_id = self.current_session_id()?;
        let report = op == "goal" || op == "goal-get";
        if op == "goal-get" && !goal_params_are_read_only(params) {
            return Err(external(
                "goal-get is read-only and accepts no mutation fields",
            ));
        }
        if op == "goal-get" || (report && goal_params_are_read_only(params)) {
            return match self.rpc()?.goal_snapshot(&session_id).await? {
                Some(goal) => {
                    self.emit_goal(&goal);
                    Ok(goal_status_message(&goal))
                }
                None => {
                    self.emit(AgentEvent::GoalCleared);
                    Ok("no active goal".into())
                }
            };
        }
        if op == "goal-budget-limited" {
            return Err(external(
                "Kimi derives budget exhaustion from enforced native limits; \
                 goal-budget-limited is not a setter",
            ));
        }
        if op == "goal-complete" {
            let reason = params
                .get("reason")
                .or_else(|| params.get("message"))
                .and_then(Value::as_str);
            return match self.rpc()?.mark_goal_complete(&session_id, reason).await? {
                Some(goal) => {
                    self.emit_goal(&goal);
                    Ok(goal_status_message(&goal))
                }
                None => Ok("no active goal".into()),
            };
        }
        if op == "goal-clear" || params.get("clear").and_then(Value::as_bool) == Some(true) {
            if self.rpc()?.goal_snapshot(&session_id).await?.is_none() {
                return Ok("no active goal".into());
            }
            self.update_profile(serde_json::json!({ "goal_control": "cancel" }))
                .await?;
            self.emit(AgentEvent::GoalCleared);
            return Ok("goal cleared".into());
        }
        let control = match op {
            "goal-pause" => Some("pause"),
            "goal-resume" => Some("resume"),
            _ => params
                .get("status")
                .and_then(Value::as_str)
                .and_then(|status| match status {
                    "paused" => Some("pause"),
                    "active" => Some("resume"),
                    _ => None,
                }),
        };
        if let Some(control) = control {
            self.update_profile(serde_json::json!({ "goal_control": control }))
                .await?;
            let goal = self.api()?.goal(&session_id).await?;
            self.emit_goal(&goal);
            return Ok(goal_status_message(&goal));
        }
        let limits = goal_budget_limits(params)?;
        let has_limits = limits.token_budget.is_some()
            || limits.turn_budget.is_some()
            || limits.wall_clock_budget_ms.is_some();
        let objective = params
            .get("objective")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|objective| !objective.is_empty())
            .map(str::to_string);
        if let Some(objective) = objective {
            let current = self.rpc()?.goal_snapshot(&session_id).await?;
            if current.is_some() {
                // Kimi has no atomic edit endpoint. This explicit edit/set action
                // is implemented as native cancel + create; validation above runs
                // before the existing goal is touched.
                self.update_profile(serde_json::json!({ "goal_control": "cancel" }))
                    .await?;
            }
            self.update_profile(serde_json::json!({ "goal_objective": objective }))
                .await?;
        } else if !has_limits {
            return Err(external(
                "goal action requires a non-empty objective or at least one budget limit",
            ));
        }
        let goal = if has_limits {
            if self.rpc()?.goal_snapshot(&session_id).await?.is_none() {
                return Err(external(
                    "cannot set Kimi goal budgets without an active goal",
                ));
            }
            self.rpc()?
                .set_goal_budget_limits(&session_id, limits)
                .await?
        } else {
            self.rpc()?
                .goal_snapshot(&session_id)
                .await?
                .ok_or_else(|| external("Kimi did not create the requested goal"))?
        };
        self.emit_goal(&goal);
        Ok(goal_status_message(&goal))
    }

    fn emit_goal(&self, goal: &Value) {
        if goal.is_null() {
            self.emit(AgentEvent::GoalCleared);
            return;
        }
        let Some(objective) = goal.get("objective").and_then(Value::as_str) else {
            return;
        };
        self.emit(AgentEvent::GoalUpdated {
            goal: crate::types::SessionGoal {
                objective: objective.to_string(),
                status: normalize_goal_status(goal),
                elapsed_seconds: goal
                    .get("wallClockMs")
                    .and_then(Value::as_u64)
                    .map(|milliseconds| milliseconds / 1000),
                tokens_used: goal.get("tokensUsed").and_then(Value::as_u64),
                token_budget: goal
                    .get("budget")
                    .and_then(|budget| budget.get("tokenBudget"))
                    .and_then(Value::as_u64),
            },
        });
    }

    async fn start_side(&mut self, params: &Value) -> Result<String, CallerError> {
        let parent = self.current_session_id()?;
        let prompt = crate::thread_actions::side_session_prompt_from_params(params)
            .ok_or_else(|| external("side conversation requires a prompt"))?;
        let result = self
            .api()?
            .session_action(&parent, "btw", serde_json::json!({}))
            .await?;
        let agent_id = result
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| external("Kimi :btw response omitted agent_id"))?
            .to_string();
        let child = child_thread_id(&parent, &agent_id);
        let text = format!(
            "{}\n\nSide-conversation request:\n{}",
            super::SIDE_CONVERSATION_CONTRACT,
            prompt
        );
        let submitted = self
            .api()?
            .submit_prompt(
                &parent,
                vec![serde_json::json!({"type": "text", "text": text})],
                serde_json::json!({ "agent_id": agent_id }),
            )
            .await?;
        if submitted.get("status").and_then(Value::as_str) == Some("blocked") {
            return Err(external("Kimi blocked the side-conversation prompt"));
        }
        self.shared.set_active_agent_id(Some(agent_id.clone()));
        if let Some(prompt_id) = submitted.get("prompt_id").and_then(Value::as_str) {
            self.shared
                .set_prompt_id(&parent, &agent_id, Some(prompt_id.to_string()));
        }
        Ok(format!(
            "side conversation started in thread {child} from parent {parent}"
        ))
    }

    fn action_session_id(&self, params: &Value) -> Result<String, CallerError> {
        self.action_target(params).map(|(session, _)| session)
    }

    fn action_target(&self, params: &Value) -> Result<(String, String), CallerError> {
        let current = self.current_session_id()?;
        let target = params
            .get("threadId")
            .or_else(|| params.get("thread_id"))
            .and_then(Value::as_str);
        match target {
            Some(target) => {
                let (session, agent) =
                    split_child_thread_id(target).unwrap_or((target, KIMI_MAIN_AGENT_ID));
                if session != current {
                    return Err(external(format!(
                        "Kimi thread {target} does not belong to active session {current}"
                    )));
                }
                Ok((session.to_string(), agent.to_string()))
            }
            None => Ok((current, KIMI_MAIN_AGENT_ID.to_string())),
        }
    }
}

#[async_trait]
impl ExternalAgent for KimiCodeAgent {
    fn name(&self) -> &str {
        "kimi"
    }

    fn launch_config_snapshot(&self) -> Option<crate::session_config::SessionAgentConfig> {
        Some(crate::session_config::SessionAgentConfig {
            source: Some("kimi".into()),
            project_root: self
                .working_dir
                .as_deref()
                .map(|path| path.to_string_lossy().to_string()),
            agent_command: Some(self.command.clone()),
            kimi_model: self.launch.model.clone(),
            kimi_thinking: self.launch.thinking.clone(),
            kimi_permission_mode: Some(self.launch.permission_mode.clone()),
            kimi_allowed_tools: self.launch.allowed_tools.clone(),
            kimi_plan_mode: Some(self.launch.plan_mode),
            kimi_swarm_mode: Some(self.launch.swarm_mode),
            kimi_home: self
                .bridge_home
                .as_deref()
                .map(|path| path.to_string_lossy().to_string()),
            ..Default::default()
        })
    }

    async fn initialize(
        &mut self,
        config: AgentConfig,
    ) -> Result<mpsc::UnboundedReceiver<AgentEvent>, CallerError> {
        if self.child.is_some() {
            return Err(external("Kimi agent is already initialized"));
        }
        validate_permission_mode(&self.launch.permission_mode)?;
        self.working_dir = Some(config.working_dir.clone());
        if config.model.is_some() {
            self.launch.model = config.model.clone();
        }
        self.launch.allowed_tools = config.kimi_allowed_tools.clone();
        self.resume_session = config
            .resume_session
            .as_deref()
            .map(str::trim)
            .filter(|session| !session.is_empty())
            .map(str::to_string);
        self.fork_resume = config.fork_resume && self.resume_session.is_some();
        self.fork_rollback_turns = config.kimi_fork_rollback_turns;
        self.fork_expected_horizon = config
            .kimi_fork_expected_horizon
            .as_deref()
            .map(|encoded| {
                serde_json::from_str(encoded).map_err(|error| {
                    external(format!(
                        "invalid internal Kimi fork expected-head horizon: {error}"
                    ))
                })
            })
            .transpose()?;
        validate_fork_staging(
            self.resume_session.as_deref(),
            self.fork_resume,
            self.fork_rollback_turns,
            self.fork_expected_horizon.is_some(),
        )?;
        self.web_port = config.web_port.or(self.web_port);
        self.mcp_auth_token = config.mcp_auth_token;
        self.mcp_session_id = config.mcp_session_id;
        self.protocol_watch = config.protocol_watch;

        let primary_home = crate::credential_leases::materialized_kimi_code_home()
            .or_else(|| std::env::var_os("KIMI_CODE_HOME").map(PathBuf::from))
            .or_else(|| dirs::home_dir().map(|home| home.join(".kimi-code")))
            .ok_or_else(|| external("could not resolve Kimi data home"))?;
        let identity = self
            .mcp_session_id
            .clone()
            .or_else(|| self.resume_session.clone())
            .unwrap_or_else(|| format!("unscoped:{}", config.working_dir.display()));
        let mcp = match self.web_port {
            Some(port) => Some(BridgeMcpConfig {
                server_name: choose_mcp_server_name(&config.working_dir, &identity).map_err(
                    |error| {
                        external(format!(
                            "failed to inspect project Kimi MCP config: {error}"
                        ))
                    },
                )?,
                url: self.intendant_mcp_url(port, false),
                bearer_token_env_var: super::intendant_mcp_bearer_token(
                    self.mcp_auth_token.as_deref(),
                    self.mcp_session_id.as_deref(),
                )
                .map(|_| super::INTENDANT_MCP_BEARER_TOKEN_ENV.to_string()),
            }),
            None => None,
        };
        let bridge_home = tokio::task::spawn_blocking({
            let primary_home = primary_home.clone();
            let identity = identity.clone();
            move || prepare_bridge_home(&primary_home, &identity, mcp.as_ref())
        })
        .await
        .map_err(|error| external(format!("Kimi bridge preparation panicked: {error}")))?
        .map_err(|error| external(format!("failed to prepare Kimi bridge home: {error}")))?;

        let mut command = crate::platform::spawn_command(&self.command);
        command
            .args([
                "server",
                "run",
                "--foreground",
                "--port",
                "0",
                "--log-level",
                "silent",
            ])
            .current_dir(&config.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        super::apply_external_child_env_policy(&mut command);
        command.env("KIMI_CODE_HOME", &bridge_home);
        if let Some(port) = self.web_port {
            super::add_intendant_bootstrap_env(
                &mut command,
                &self.intendant_mcp_url(port, true),
                self.mcp_session_id.as_deref(),
                self.mcp_auth_token.as_deref(),
            );
        }
        crate::platform::die_with_parent(&mut command);
        #[cfg(target_os = "linux")]
        crate::linux_display_env::apply_to_tokio_command(&mut command);
        let mut child = crate::credential_leases::spawn_with_dns_credential_scrub(
            &mut command,
            config.dns_credential_env.as_deref(),
            config.dns_credential_store.as_deref(),
        )
        .map_err(|error| {
            external(format!(
                "failed to spawn Kimi command '{}': {error}",
                self.command
            ))
        })?;
        let child_pid = child.id();
        if let Some(pid) = child_pid {
            super::register_child_process(pid);
        }
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
                return Err(external("failed to capture Kimi server stdout"));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
                return Err(external("failed to capture Kimi server stderr"));
            }
        };
        let (origin, stdout_reader) = match wait_for_server_origin(stdout).await {
            Ok(ready) => ready,
            Err(error) => {
                terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
                return Err(error);
            }
        };
        let token = match wait_for_server_token(&bridge_home.join("server.token")).await {
            Ok(token) => token,
            Err(error) => {
                terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
                return Err(error);
            }
        };
        let rpc = match KimiRpcApi::new(origin.clone(), token.clone()) {
            Ok(api) => api,
            Err(error) => {
                terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
                return Err(error);
            }
        };
        let api = match KimiApi::new(origin, token) {
            Ok(api) => api,
            Err(error) => {
                terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
                return Err(error);
            }
        };
        if let Err(error) = api.health().await {
            terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
            return Err(error);
        }
        let meta = match api.meta().await.and_then(|meta| validate_meta(&meta)) {
            Ok(version) => version,
            Err(error) => {
                terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
                return Err(error);
            }
        };
        if let Err(error) = rpc.validate_required_methods().await {
            terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
            return Err(error);
        }
        if let Err(error) = remove_captured_server_token(&bridge_home.join("server.token")).await {
            terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
            return Err(error);
        }
        if let Some(watch) = self.protocol_watch.as_ref() {
            watch.mark_observed(Some(meta));
        }

        let stdout_handle = tokio::spawn(drain_silently(stdout_reader));
        let stderr_handle = tokio::spawn(drain_silently(stderr));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (ws_tx, ws_handle) = match spawn_driver(
            api.clone(),
            Arc::clone(&self.shared),
            event_tx.clone(),
            self.protocol_watch.clone(),
        )
        .await
        {
            Ok(driver) => driver,
            Err(error) => {
                stdout_handle.abort();
                stderr_handle.abort();
                terminate_spawned_child(child_pid, &mut child, &bridge_home).await;
                return Err(error);
            }
        };

        self.child = Some(child);
        self.child_pid = child_pid;
        self.api = Some(api);
        self.rpc = Some(rpc);
        self.event_tx = Some(event_tx);
        self.ws_tx = Some(ws_tx);
        self.ws_handle = Some(ws_handle);
        self.stdout_handle = Some(stdout_handle);
        self.stderr_handle = Some(stderr_handle);
        self.bridge_home = Some(bridge_home);
        self.emit(AgentEvent::CwdAnnounced {
            cwd: config.working_dir.to_string_lossy().to_string(),
        });
        self.emit_config_facts(false);
        Ok(event_rx)
    }

    async fn start_thread(&mut self) -> Result<AgentThread, CallerError> {
        let disposable = self.resume_session.is_none() || self.fork_resume;
        let session = if let Some(resume) = self.resume_session.clone() {
            if self.fork_resume {
                self.fork_resumed_session(&resume).await?
            } else {
                self.api()?.get_session(&resume).await?
            }
        } else {
            let working_dir = self
                .working_dir
                .as_deref()
                .ok_or_else(|| external("Kimi working directory is unavailable"))?;
            self.api()?
                .create_session(working_dir, self.session_agent_config())
                .await?
        };
        let session_id = session
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| external("Kimi session response omitted id"))?
            .trim()
            .to_string();
        if session_id.is_empty() {
            return Err(external("Kimi session response returned an empty id"));
        }
        if let Some(resume) = self.resume_session.as_deref() {
            if !self.fork_resume && session_id != resume {
                return Err(external(
                    "Kimi resume response identified a different session",
                ));
            }
        }
        // Kimi 0.27 validates `agent_config` on session creation but its
        // create route does not apply the field. The profile route is the
        // authoritative live mutation path, so apply launch pins here for
        // new, resumed, and forked sessions before publication/subscription.
        let configured: Result<Value, CallerError> = async {
            let session = self
                .api()?
                .update_profile(&session_id, self.session_agent_config())
                .await?;
            if let Some(names) = self.launch.allowed_tools.clone() {
                let names = sorted_unique_tool_names(names);
                let inventory = self.rpc()?.tools(&session_id, KIMI_MAIN_AGENT_ID).await?;
                validate_requested_tool_names(&inventory, &names)?;
                self.rpc()?
                    .set_active_tools(&session_id, KIMI_MAIN_AGENT_ID, &names)
                    .await?;
                self.launch.allowed_tools = Some(names);
            }
            self.refresh_profile_facts(&session_id, false).await?;
            Ok(session)
        }
        .await;
        let session = match configured {
            Ok(session) => session,
            Err(error) => {
                if disposable {
                    let _ = self
                        .api()?
                        .session_action(&session_id, "archive", serde_json::json!({}))
                        .await;
                }
                return Err(error);
            }
        };
        self.emit_session_warnings(&session_id).await;
        crate::background_tasks::clear_session(&session_id);
        self.shared.set_session_id(Some(session_id.clone()));
        self.shared.set_active_agent_id(None);
        if let Err(error) = self
            .ws_tx
            .as_ref()
            .ok_or_else(|| external("Kimi event stream is unavailable"))?
            .send(WsCommand::Subscribe {
                session_id: session_id.clone(),
                snapshot_first: true,
            })
        {
            self.shared.set_session_id(None);
            if disposable {
                let _ = self
                    .api()?
                    .session_action(&session_id, "archive", serde_json::json!({}))
                    .await;
            }
            return Err(external(format!(
                "Kimi event stream stopped before subscription: {error}"
            )));
        }
        self.emit(AgentEvent::NativeSessionId {
            session_id: session_id.clone(),
        });
        if let Some(cwd) = session
            .get("metadata")
            .and_then(|metadata| metadata.get("cwd"))
            .and_then(Value::as_str)
        {
            self.emit(AgentEvent::CwdAnnounced {
                cwd: cwd.to_string(),
            });
        }
        Ok(AgentThread {
            thread_id: session_id,
        })
    }

    async fn send_message(
        &mut self,
        thread: &AgentThread,
        message: &str,
    ) -> Result<(), CallerError> {
        self.submit_content(
            thread,
            vec![serde_json::json!({"type": "text", "text": message})],
        )
        .await?;
        Ok(())
    }

    async fn send_message_with_images(
        &mut self,
        thread: &AgentThread,
        message: &str,
        images: &[AgentImageAttachment],
    ) -> Result<(), CallerError> {
        let attachments = images
            .iter()
            .cloned()
            .map(AgentAttachment::Image)
            .collect::<Vec<_>>();
        let content = self.attachment_content(message, &attachments).await?;
        self.submit_content(thread, content).await?;
        Ok(())
    }

    async fn send_message_with_attachments(
        &mut self,
        thread: &AgentThread,
        message: &str,
        attachments: &[AgentAttachment],
    ) -> Result<(), CallerError> {
        let content = self.attachment_content(message, attachments).await?;
        self.submit_content(thread, content).await?;
        Ok(())
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    async fn context_snapshot(&mut self) -> Result<Option<AgentContextSnapshot>, CallerError> {
        let Some(session_id) = self.shared.session_id() else {
            return Ok(None);
        };
        let agent_id = self
            .shared
            .active_agent_id()
            .unwrap_or_else(|| KIMI_MAIN_AGENT_ID.to_string());
        let context = self.rpc()?.context(&session_id, &agent_id).await?;

        // The context itself remains useful if a concurrently edited profile
        // or model catalog cannot be read. Context-window metadata is emitted
        // only when Kimi names an exact configured alias in both services.
        let context_window = match self.rpc()?.profile(&session_id, &agent_id).await {
            Ok(profile) => match profile.model_alias {
                Some(alias) => self
                    .rpc()?
                    .models()
                    .await
                    .ok()
                    .and_then(|models| {
                        models
                            .into_iter()
                            .find(|model| model.model == alias)
                            .map(|model| model.max_context_size)
                    })
                    .filter(|window| *window > 0),
                None => None,
            },
            Err(_) => None,
        };
        let scope = if agent_id == KIMI_MAIN_AGENT_ID {
            session_id
        } else {
            child_thread_id(&session_id, &agent_id)
        };
        let token_count = context.token_count;
        let item_count = context.history.len();
        let raw = serde_json::to_value(context).map_err(|error| {
            external(format!(
                "failed to encode Kimi's native context snapshot: {error}"
            ))
        })?;
        Ok(Some(AgentContextSnapshot {
            source: "kimi".to_string(),
            label: format!("Kimi current model context ({scope})"),
            request_id: None,
            request_index: None,
            rollout_path: None,
            format: "kimi.agent_rpc.context.v1".to_string(),
            token_count: Some(token_count),
            token_count_kind: Some(AgentContextTokenCountKind::BackendReported),
            context_window,
            hard_context_window: context_window,
            item_count: Some(item_count),
            raw,
        }))
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), CallerError> {
        let session_id = self.current_session_id()?;
        if !self.shared.is_approval(request_id)
            && self.pending_question(request_id).await?.is_some()
        {
            if matches!(
                decision,
                ApprovalDecision::Decline | ApprovalDecision::Cancel
            ) {
                self.api()?
                    .dismiss_question(&session_id, request_id)
                    .await?;
                self.shared.remove_question(request_id);
                return Ok(());
            }
            return Err(external(
                "Kimi question requires structured answers, not an approval",
            ));
        }
        if !self.shared.is_approval(request_id) {
            let listed = self.api()?.list_approvals(&session_id).await?;
            if find_interaction(&listed, "approval_id", request_id).is_none() {
                return Err(external(format!("no pending Kimi approval {request_id}")));
            }
        }
        let (wire_decision, scope) = match decision {
            ApprovalDecision::Accept => ("approved", None),
            ApprovalDecision::AcceptForSession => ("approved", Some("session")),
            ApprovalDecision::Decline => ("rejected", None),
            ApprovalDecision::Cancel => ("cancelled", None),
        };
        let mut body = serde_json::json!({ "decision": wire_decision });
        if let Some(scope) = scope {
            body["scope"] = Value::String(scope.into());
        }
        self.api()?
            .resolve_approval(&session_id, request_id, body)
            .await?;
        self.shared.remove_approval(request_id);
        Ok(())
    }

    async fn resolve_user_question(
        &mut self,
        request_id: &str,
        answers: &HashMap<String, String>,
    ) -> Result<(), CallerError> {
        let session_id = self.current_session_id()?;
        let question = self
            .pending_question(request_id)
            .await?
            .ok_or_else(|| external(format!("no pending Kimi question {request_id}")))?;
        self.api()?
            .resolve_question(
                &session_id,
                request_id,
                question_answer_body(&question, answers),
            )
            .await?;
        self.shared.remove_question(request_id);
        Ok(())
    }

    async fn interrupt_turn(&mut self) -> Result<(), CallerError> {
        let session_id = self.current_session_id()?;
        let agent_id = self
            .shared
            .active_agent_id()
            .unwrap_or_else(|| KIMI_MAIN_AGENT_ID.to_string());
        if let Some(prompt_id) = self.current_prompt().await? {
            self.api()?.abort_prompt(&session_id, &prompt_id).await?;
            self.shared
                .clear_prompt_id(&session_id, &agent_id, Some(&prompt_id));
        } else if agent_id != KIMI_MAIN_AGENT_ID {
            return Err(external(
                "Kimi has no tracked prompt for the selected child; refusing to abort the parent",
            ));
        } else {
            self.api()?
                .session_action(&session_id, "abort", serde_json::json!({}))
                .await?;
            self.shared.set_prompt_id(&session_id, &agent_id, None);
        }
        Ok(())
    }

    async fn steer_turn(&mut self, text: &str) -> Result<(), CallerError> {
        let session_id = self.current_session_id()?;
        let agent_id = self
            .shared
            .active_agent_id()
            .unwrap_or_else(|| KIMI_MAIN_AGENT_ID.to_string());
        if self.current_prompt().await?.is_none() {
            return Err(external("no active turn to steer"));
        }
        let mut overrides = serde_json::Map::new();
        if agent_id != KIMI_MAIN_AGENT_ID {
            overrides.insert("agent_id".into(), Value::String(agent_id.clone()));
        }
        let submitted = self
            .api()?
            .submit_prompt(
                &session_id,
                vec![serde_json::json!({"type": "text", "text": text})],
                Value::Object(overrides),
            )
            .await?;
        let prompt_id = submitted
            .get("prompt_id")
            .and_then(Value::as_str)
            .ok_or_else(|| external("Kimi steer submission omitted prompt_id"))?
            .to_string();
        match submitted.get("status").and_then(Value::as_str) {
            Some("queued") => {
                self.api()?
                    .steer_prompts(&session_id, std::slice::from_ref(&prompt_id))
                    .await?;
            }
            Some("running") => {
                // The active turn ended in the submission race. Kimi started
                // this as an immediate follow-up, which preserves the text.
                self.shared
                    .set_prompt_id(&session_id, &agent_id, Some(prompt_id));
            }
            Some("blocked") => return Err(external("Kimi blocked the steer submission")),
            _ => return Err(external("Kimi returned an unknown steer status")),
        }
        Ok(())
    }

    async fn thread_action(&mut self, op: &str, params: &Value) -> Result<String, CallerError> {
        let (session_id, agent_id) = self.action_target(params)?;
        let targets_child = agent_id != KIMI_MAIN_AGENT_ID;
        if targets_child
            && !matches!(
                op,
                "side-close"
                    | "side_close"
                    | "context-clear"
                    | "context_clear"
                    | "tools"
                    | "tool-list"
                    | "tool_list"
                    | "tools-set"
                    | "tools_set"
                    | "tools-all"
                    | "tools_all"
            )
        {
            return Err(external(format!(
                "Kimi 0.27 cannot apply /{op} to one :btw agent; target the parent session"
            )));
        }
        match op {
            "compact" => {
                let body = params
                    .get("instruction")
                    .and_then(Value::as_str)
                    .map(|instruction| serde_json::json!({"instruction": instruction}))
                    .unwrap_or_else(|| serde_json::json!({}));
                self.api()?
                    .session_action(&session_id, "compact", body)
                    .await?;
                Ok("Kimi compaction requested".into())
            }
            "fork" => {
                let mut body = serde_json::Map::new();
                if let Some(title) = params
                    .get("name")
                    .or_else(|| params.get("title"))
                    .and_then(Value::as_str)
                {
                    body.insert("title".into(), Value::String(title.to_string()));
                }
                let fork = self
                    .api()?
                    .session_action(&session_id, "fork", Value::Object(body))
                    .await?;
                let id = fork
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty() && *id != session_id)
                    .ok_or_else(|| {
                        external("Kimi fork response did not identify a distinct session")
                    })?;
                Ok(format!("forked into thread {id}"))
            }
            "side" | "btw" => self.start_side(params).await,
            "side-close" | "side_close" => {
                self.shared.set_active_agent_id(None);
                Ok("side conversation closed".into())
            }
            "undo" | "rollback" => {
                let count = params
                    .get("count")
                    .or_else(|| params.get("turns"))
                    .and_then(Value::as_u64)
                    .unwrap_or(1);
                if count == 0 || count > u32::MAX as u64 {
                    return Err(external("Kimi undo count must be a positive integer"));
                }
                self.api()?
                    .session_action(&session_id, "undo", serde_json::json!({ "count": count }))
                    .await?;
                Ok(format!("Kimi removed {count} conversation turn(s)"))
            }
            "archive" | "restore" => {
                self.api()?
                    .session_action(&session_id, op, serde_json::json!({}))
                    .await?;
                Ok(format!("Kimi session {op}d"))
            }
            "rename" => {
                let title = string_param(params, &["name", "title", "value"], "title")?;
                self.api()?.update_title(&session_id, &title).await?;
                Ok(format!("Kimi session renamed to {title}"))
            }
            "review" => self.review_action(params).await,
            "fast" => self.toggle_fast_model().await,
            "context-clear" | "context_clear" => {
                self.rpc()?.clear_context(&session_id, &agent_id).await?;
                Ok(if targets_child {
                    format!("Kimi agent {agent_id} context cleared")
                } else {
                    "Kimi conversation context cleared".into()
                })
            }
            op if op == "goal" || op.starts_with("goal-") => self.goal_action(op, params).await,
            "model" | "model-set" | "set-model" => self.set_model(params).await,
            "models" | "model-list" | "model_list" => self.models_report().await,
            "thinking" | "thinking-set" | "effort" => self.set_thinking(params).await,
            "permission-mode" | "permission_mode" | "permissions" => {
                self.set_permission(params).await
            }
            "plan-mode" | "plan_mode" => {
                self.set_boolean_mode(params, "plan_mode", "plan mode")
                    .await
            }
            "swarm-mode" | "swarm_mode" => {
                self.set_boolean_mode(params, "swarm_mode", "swarm mode")
                    .await
            }
            "tasks" | "task-list" | "task_list" | "task-output" | "task_output" | "task-cancel"
            | "task_cancel" => self.task_action(op, params).await,
            "tools" | "tool-list" | "tool_list" | "tools-set" | "tools_set" | "tools-all"
            | "tools_all" => self.tools_action(op, &session_id, &agent_id, params).await,
            other => Err(external(format!(
                "thread action /{other} not supported by Kimi"
            ))),
        }
    }

    async fn pause_autonomous_goal(
        &mut self,
        thread_id: &str,
    ) -> Result<AutonomousGoalPauseResult, CallerError> {
        let session_id = split_child_thread_id(thread_id)
            .map(|(session, _)| session)
            .unwrap_or(thread_id);
        let goal = self.api()?.goal(session_id).await?;
        if goal.is_null() {
            return Ok(AutonomousGoalPauseResult {
                goal: None,
                goal_absent: true,
                paused: false,
            });
        }
        let was_active = goal.get("status").and_then(Value::as_str) == Some("active");
        if was_active {
            self.api()?
                .update_profile(session_id, serde_json::json!({ "goal_control": "pause" }))
                .await?;
        }
        let latest = self.api()?.goal(session_id).await?;
        let normalized = session_goal(&latest);
        if let Some(goal) = normalized.clone() {
            self.emit(AgentEvent::GoalUpdated { goal });
        }
        Ok(AutonomousGoalPauseResult {
            goal: normalized,
            goal_absent: false,
            paused: was_active,
        })
    }

    async fn read_thread_snapshot(
        &mut self,
        thread_id: &str,
    ) -> Result<AgentThreadSnapshot, CallerError> {
        let session_id = split_child_thread_id(thread_id)
            .map(|(session, _)| session)
            .unwrap_or(thread_id);
        self.api()?.get_session(session_id).await?;
        Ok(AgentThreadSnapshot {
            thread_id: thread_id.to_string(),
            rollout_path: None,
        })
    }

    async fn fork_thread_with_options(
        &mut self,
        thread_id: &str,
        name: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<AgentThread, CallerError> {
        if split_child_thread_id(thread_id).is_some() {
            return Err(external(
                "Kimi 0.27 cannot fork one composite :btw conversation",
            ));
        }
        if let Some(cwd) = cwd {
            let source = self.api()?.get_session(thread_id).await?;
            let source_cwd = source
                .get("metadata")
                .and_then(|metadata| metadata.get("cwd"))
                .and_then(Value::as_str)
                .ok_or_else(|| external("Kimi source session omitted its working directory"))?;
            if Path::new(source_cwd) != cwd {
                // Kimi's native :fork cannot change cwd. Fission objectives are
                // deliberately self-contained, so create an isolated session
                // in the requested worktree and seed it with the charter below
                // rather than pretending the native fork moved directories.
                let created = self
                    .api()?
                    .create_session(cwd, self.session_agent_config())
                    .await?;
                let id = created
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| external("Kimi isolated session response omitted id"))?
                    .trim()
                    .to_string();
                if id.is_empty() || id == thread_id {
                    return Err(external(
                        "Kimi isolated-session response did not identify a distinct session",
                    ));
                }
                let configured: Result<(), CallerError> = async {
                    self.api()?
                        .update_profile(&id, self.session_agent_config())
                        .await?;
                    if let Some(names) = self.launch.allowed_tools.clone() {
                        let inventory = self.rpc()?.tools(&id, KIMI_MAIN_AGENT_ID).await?;
                        validate_requested_tool_names(&inventory, &names)?;
                        self.rpc()?
                            .set_active_tools(&id, KIMI_MAIN_AGENT_ID, &names)
                            .await?;
                    }
                    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
                        self.api()?.update_title(&id, name).await?;
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = configured {
                    let _ = self
                        .api()?
                        .session_action(&id, "archive", serde_json::json!({}))
                        .await;
                    return Err(error);
                }
                return Ok(AgentThread { thread_id: id });
            }
        }
        let session_id = thread_id;
        let mut body = serde_json::Map::new();
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            body.insert("title".into(), Value::String(name.to_string()));
        }
        let fork = self
            .api()?
            .session_action(session_id, "fork", Value::Object(body))
            .await?;
        let id = fork
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && *id != session_id)
            .ok_or_else(|| external("Kimi fork response did not identify a distinct session"))?;
        Ok(AgentThread {
            thread_id: id.to_string(),
        })
    }

    async fn inject_thread_developer_message(
        &mut self,
        thread_id: &str,
        message: &str,
    ) -> Result<(), CallerError> {
        if split_child_thread_id(thread_id).is_some() {
            return Err(external(
                "Kimi cannot seed a composite :btw child as a fission branch",
            ));
        }
        let message = message.trim();
        if message.is_empty() {
            return Err(external("Kimi fission charter must not be empty"));
        }
        self.api()?.get_session(thread_id).await?;
        // Kimi 0.27 has no public developer-role append. A fission branch is
        // supposed to begin work immediately, so submit Intendant's delimited
        // charter as its first user-origin prompt. The branch's resumed
        // supervisor attaches to that live turn and does not enqueue a second
        // kickoff prompt.
        let submitted = self
            .api()?
            .submit_prompt(
                thread_id,
                vec![serde_json::json!({
                    "type": "text",
                    "text": format!(
                        "<intendant_developer_instruction>\n{message}\n</intendant_developer_instruction>"
                    ),
                })],
                serde_json::json!({}),
            )
            .await?;
        match submitted.get("status").and_then(Value::as_str) {
            Some("running") | Some("queued") => Ok(()),
            Some("blocked") => Err(external("Kimi blocked the fission charter prompt")),
            _ => Err(external(
                "Kimi returned an unknown status for the fission charter prompt",
            )),
        }
    }

    fn supports_user_message_rewind(&self) -> bool {
        true
    }

    async fn rollback_turns(&mut self, turns_to_drop: u32) -> Result<(), CallerError> {
        let session_id = self.current_session_id()?;
        self.api()?
            .session_action(
                &session_id,
                "undo",
                serde_json::json!({ "count": turns_to_drop }),
            )
            .await?;
        Ok(())
    }

    async fn rollback_thread_turns(
        &mut self,
        thread_id: &str,
        turns_to_drop: u32,
    ) -> Result<(), CallerError> {
        if split_child_thread_id(thread_id).is_some() {
            return Err(external("Kimi 0.27 cannot target undo at one :btw agent"));
        }
        self.api()?
            .session_action(
                thread_id,
                "undo",
                serde_json::json!({ "count": turns_to_drop }),
            )
            .await?;
        Ok(())
    }

    async fn activate_thread(&mut self, thread_id: &str) -> Result<(), CallerError> {
        let current = self.current_session_id()?;
        if let Some((session, agent)) = split_child_thread_id(thread_id) {
            if session != current {
                return Err(external("Kimi child belongs to a different session"));
            }
            self.shared.set_active_agent_id(Some(agent.to_string()));
        } else if thread_id == current {
            self.shared.set_active_agent_id(None);
        } else {
            return Err(external("Kimi thread is not loaded in this server adapter"));
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), CallerError> {
        if let Some(handle) = self.review_monitor.take() {
            handle.abort();
        }
        let review_lease = self.review_lease.lock().await.take();
        let review_restore = if let (Some(lease), Some(api), Some(rpc)) =
            (review_lease, self.api.as_ref(), self.rpc.as_ref())
        {
            match stop_review_prompt_before_restore(api, &lease).await {
                Ok(true) => restore_review_tools_if_unchanged(rpc, &lease)
                    .await
                    .map(|_| ()),
                Ok(false) => {
                    self.emit(AgentEvent::Log {
                        level: "warn".into(),
                        message: "Kimi review shutdown could not prove the prompt stopped; leaving the read-only tool set in place before terminating the server"
                            .into(),
                    });
                    Ok(())
                }
                Err(error) => {
                    self.emit(AgentEvent::Log {
                        level: "warn".into(),
                        message: format!(
                            "Kimi review shutdown could not inspect the exact prompt; leaving tools confined: {}",
                            bounded_wire_text(&error.to_string(), 1024)
                        ),
                    });
                    Ok(())
                }
            }
        } else {
            Ok(())
        };
        if let Some(tx) = self.ws_tx.take() {
            let _ = tx.send(WsCommand::Shutdown);
        }
        if let Some(handle) = self.ws_handle.take() {
            await_driver_shutdown(handle, Duration::from_secs(3)).await;
        }
        if let Some(watch) = self.protocol_watch.take() {
            watch.flush_async().await;
        }
        if let Some(session_id) = self.shared.session_id() {
            crate::background_tasks::clear_session(&session_id);
        }
        if let Some(pid) = self.child_pid.take() {
            crate::platform::terminate_process_tree_now(pid);
            super::unregister_child_process(pid);
        }
        if let Some(mut child) = self.child.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
        if let Some(handle) = self.stdout_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_handle.take() {
            handle.abort();
        }
        let bridge_sync = match self.bridge_home.take() {
            Some(bridge) => {
                tokio::task::spawn_blocking(move || sync_bridge_home_to_primary(&bridge))
                    .await
                    .map_err(|error| external(format!("Kimi bridge sync panicked: {error}")))?
                    .map_err(|error| {
                        external(format!(
                            "failed to persist Kimi copy-fallback session data: {error}"
                        ))
                    })
            }
            None => Ok(()),
        };
        self.api = None;
        self.rpc = None;
        self.event_tx = None;
        match (review_restore, bridge_sync) {
            (Err(review), Err(bridge)) => Err(external(format!(
                "failed to restore Kimi review confinement: {review}; {bridge}"
            ))),
            (Err(review), Ok(())) => Err(external(format!(
                "failed to restore Kimi review confinement: {review}"
            ))),
            (Ok(()), result) => result,
        }
    }
}

impl Drop for KimiCodeAgent {
    fn drop(&mut self) {
        if let Some(pid) = self.child_pid.take() {
            crate::platform::terminate_process_tree_now(pid);
            super::unregister_child_process(pid);
        } else if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        if let Some(handle) = self.ws_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.review_monitor.take() {
            handle.abort();
        }
        if let Some(handle) = self.stdout_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_handle.take() {
            handle.abort();
        }
        if let Some(bridge) = self.bridge_home.take() {
            // Drop cannot await, but a synchronous best-effort pass preserves
            // copy-fallback history on normal unwinding. A later bridge
            // preparation repeats the same recovery after abrupt exits.
            let _ = sync_bridge_home_to_primary(&bridge);
        }
    }
}

async fn wait_for_server_origin(
    stdout: tokio::process::ChildStdout,
) -> Result<(String, BufReader<tokio::process::ChildStdout>), CallerError> {
    let mut reader = BufReader::new(stdout);
    let future = async {
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader
                .read_line(&mut line)
                .await
                .map_err(|_| external("failed to read Kimi server readiness output"))?;
            if read == 0 {
                return Err(external("Kimi server exited before becoming ready"));
            }
            if let Some(origin) = extract_loopback_origin(&line) {
                return Ok(origin);
            }
            // Deliberately do not retain or forward banner lines: the ready
            // banner contains Kimi's bearer token.
        }
    };
    let origin = tokio::time::timeout(STARTUP_TIMEOUT, future)
        .await
        .map_err(|_| external("timed out waiting for Kimi server readiness"))??;
    Ok((origin, reader))
}

fn extract_loopback_origin(line: &str) -> Option<String> {
    let clean = super::strip_ansi_escapes(line);
    for prefix in ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"] {
        let Some(start) = clean.find(prefix) else {
            continue;
        };
        let rest = &clean[start..];
        let end = rest
            .find(|character: char| character == '#' || character.is_whitespace())
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        if reqwest::Url::parse(candidate)
            .ok()
            .and_then(|url| url.port())
            .is_some()
        {
            return Some(candidate.trim_end_matches('/').to_string());
        }
    }
    None
}

async fn wait_for_server_token(path: &Path) -> Result<String, CallerError> {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::fs::read_to_string(path).await {
            Ok(token) => {
                validate_token_permissions(path).await?;
                let token = token.trim();
                if token.len() < 16
                    || token.len() > 4096
                    || !token.bytes().all(|byte| byte.is_ascii_graphic())
                {
                    return Err(external("Kimi server token file is malformed"));
                }
                return Ok(token.to_string());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(external("failed to read Kimi server token file")),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(external("timed out waiting for Kimi server token file"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(unix)]
async fn validate_token_permissions(path: &Path) -> Result<(), CallerError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| external("failed to inspect Kimi server token file"))?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(external(
            "refusing Kimi server token readable by group or other users",
        ));
    }
    Ok(())
}

/// Kimi persists its loopback bearer for its own `server ps/kill` commands.
/// Intendant owns the child PID and holds the captured token in memory, so the
/// file is unnecessary after handshake and would only expose the private v2
/// control surface to Kimi's own absolute-path file tools.
async fn remove_captured_server_token(path: &Path) -> Result<(), CallerError> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| external("failed to inspect captured Kimi server token"))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(external(
            "refusing a non-regular Kimi server token before cleanup",
        ));
    }
    tokio::fs::remove_file(path).await.map_err(|error| {
        external(format!(
            "failed to remove captured Kimi server token: {error}"
        ))
    })?;
    match tokio::fs::symlink_metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(external("Kimi server token remained on disk after cleanup")),
        Err(error) => Err(external(format!(
            "could not verify Kimi server token cleanup: {error}"
        ))),
    }
}

#[cfg(windows)]
async fn validate_token_permissions(path: &Path) -> Result<(), CallerError> {
    crate::platform::validate_owner_private_permissions(path).map_err(|error| {
        external(format!(
            "refusing Kimi server token outside an owner-private Windows ACL boundary: {error}"
        ))
    })
}

#[cfg(not(any(unix, windows)))]
async fn validate_token_permissions(_path: &Path) -> Result<(), CallerError> {
    Ok(())
}

async fn drain_silently(mut reader: impl AsyncRead + Unpin) {
    let mut buffer = [0u8; 4096];
    while reader
        .read(&mut buffer)
        .await
        .ok()
        .is_some_and(|read| read > 0)
    {}
}

async fn terminate_spawned_child(pid: Option<u32>, child: &mut Child, bridge_home: &Path) {
    // A failed handshake must not leave the loopback bearer on disk. This is
    // safe before and after capture: NotFound is the ordinary pre-token case.
    let _ = tokio::fs::remove_file(bridge_home.join("server.token")).await;
    if let Some(pid) = pid {
        crate::platform::terminate_process_tree_now(pid);
        super::unregister_child_process(pid);
    } else {
        let _ = child.start_kill();
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
}

async fn clear_review_lease_if_nonce(
    slot: &Arc<tokio::sync::Mutex<Option<KimiReviewToolLease>>>,
    nonce: &str,
) {
    let mut lease = slot.lock().await;
    if lease.as_ref().is_some_and(|lease| lease.nonce == nonce) {
        *lease = None;
    }
}

async fn retain_review_lease_if_empty(
    slot: &Arc<tokio::sync::Mutex<Option<KimiReviewToolLease>>>,
    lease: &KimiReviewToolLease,
) {
    let mut current = slot.lock().await;
    if current.is_none() {
        *current = Some(lease.clone());
    }
}

/// Recover a failed or protocol-drifted review submission without ever
/// widening tools around a prompt whose absence has not been proved.
async fn fail_review_submission_closed(
    api: &KimiApi,
    rpc: &KimiRpcApi,
    leases: &Arc<tokio::sync::Mutex<Option<KimiReviewToolLease>>>,
    lease: &KimiReviewToolLease,
    reason: &str,
) -> CallerError {
    match stop_review_prompt_before_restore(api, lease).await {
        Ok(true) => {
            let restore = restore_review_tools_if_unchanged(rpc, lease).await;
            clear_review_lease_if_nonce(leases, &lease.nonce).await;
            match restore {
                Ok(_) => external(reason.to_string()),
                Err(error) => external(format!(
                    "{reason}; the prompt is stopped, but Kimi's prior tools could not be restored: {error}"
                )),
            }
        }
        Ok(false) => {
            retain_review_lease_if_empty(leases, lease).await;
            external(format!(
                "{reason}; Intendant could not prove the submitted prompt stopped, so Kimi remains confined to zero active tools"
            ))
        }
        Err(error) => {
            retain_review_lease_if_empty(leases, lease).await;
            external(format!(
                "{reason}; Kimi prompt state could not be verified ({error}), so Intendant left zero active tools and blocked widening"
            ))
        }
    }
}

/// Restore the pre-review set only while the live set is still exactly the
/// temporary review set. A dashboard/CLI/Kimi-UI tool change is authoritative
/// and must not be overwritten by a late review-completion poll.
async fn restore_review_tools_if_unchanged(
    rpc: &KimiRpcApi,
    lease: &KimiReviewToolLease,
) -> Result<bool, CallerError> {
    let current = rpc.tools(&lease.session_id, &lease.agent_id).await?;
    if active_tool_names(&current) != lease.review_tools {
        return Ok(false);
    }
    rpc.set_active_tools(&lease.session_id, &lease.agent_id, &lease.previous_tools)
        .await?;
    Ok(true)
}

/// Stop the exact review prompt and prove it left both active and queued
/// slots before any broader tool set is restored. `false` is a safe timeout:
/// callers leave the review tools confined and terminate the server.
async fn stop_review_prompt_before_restore(
    api: &KimiApi,
    lease: &KimiReviewToolLease,
) -> Result<bool, CallerError> {
    if lease
        .prompt_id
        .as_ref()
        .is_some_and(|id| lease.baseline_prompt_ids.contains(id))
    {
        return Err(external(
            "Kimi review prompt id collided with a pre-existing prompt",
        ));
    }

    // Even when submission returned an error or omitted its id, the HTTP
    // request may have reached Kimi. Observe a full grace window, aborting
    // only ids absent from the pre-submit snapshot.
    for _ in 0..20 {
        let prompts = api.list_prompts(&lease.session_id).await?;
        let current = pending_prompt_ids(&prompts)?;
        let mut review_ids = current
            .difference(&lease.baseline_prompt_ids)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(prompt_id) = lease.prompt_id.as_ref() {
            if current.contains(prompt_id) && !review_ids.contains(prompt_id) {
                review_ids.push(prompt_id.clone());
            }
        }
        for prompt_id in review_ids {
            api.abort_prompt(&lease.session_id, &prompt_id).await?;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let prompts = api.list_prompts(&lease.session_id).await?;
    let current = pending_prompt_ids(&prompts)?;
    let no_new_prompt = current.is_subset(&lease.baseline_prompt_ids);
    let exact_prompt_absent = lease
        .prompt_id
        .as_ref()
        .is_none_or(|prompt_id| !current.contains(prompt_id));
    Ok(no_new_prompt && exact_prompt_absent)
}

async fn monitor_review_prompt(
    api: KimiApi,
    rpc: KimiRpcApi,
    leases: Arc<tokio::sync::Mutex<Option<KimiReviewToolLease>>>,
    events: Option<mpsc::UnboundedSender<AgentEvent>>,
    lease: KimiReviewToolLease,
) {
    let Some(prompt_id) = lease.prompt_id.as_deref() else {
        return;
    };
    loop {
        let still_owned = leases
            .lock()
            .await
            .as_ref()
            .is_some_and(|current| current.nonce == lease.nonce);
        if !still_owned {
            return;
        }
        match api.list_prompts(&lease.session_id).await {
            Ok(prompts) => {
                let pending = match prompt_is_pending(&prompts, prompt_id) {
                    Ok(pending) => pending,
                    Err(error) => {
                        if let Some(events) = events.as_ref() {
                            let _ = events.send(AgentEvent::Log {
                                level: "warn".into(),
                                message: format!(
                                    "Kimi returned malformed review prompt state; restoration will retry: {}",
                                    bounded_wire_text(&error.to_string(), 1024)
                                ),
                            });
                        }
                        tokio::time::sleep(REVIEW_PROMPT_POLL_INTERVAL).await;
                        continue;
                    }
                };
                match rpc.tools(&lease.session_id, &lease.agent_id).await {
                    Ok(inventory) if active_tool_names(&inventory) != lease.review_tools => {
                        if pending {
                            if let Err(error) = api.abort_prompt(&lease.session_id, prompt_id).await
                            {
                                if let Some(events) = events.as_ref() {
                                    let _ = events.send(AgentEvent::Log {
                                        level: "warn".into(),
                                        message: format!(
                                            "Kimi review tool confinement changed and prompt abort will retry: {}",
                                            bounded_wire_text(&error.to_string(), 1024)
                                        ),
                                    });
                                }
                            }
                            tokio::time::sleep(REVIEW_PROMPT_POLL_INTERVAL).await;
                            continue;
                        }
                        clear_review_lease_if_nonce(&leases, &lease.nonce).await;
                        if let Some(events) = events.as_ref() {
                            let _ = events.send(AgentEvent::Log {
                                level: "warn".into(),
                                message: "Kimi review aborted because the active tool set changed; preserving the operator's newer tool set"
                                    .into(),
                            });
                        }
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        if let Some(events) = events.as_ref() {
                            let _ = events.send(AgentEvent::Log {
                                level: "warn".into(),
                                message: format!(
                                    "Could not verify Kimi review tool confinement; aborting the review: {}",
                                    bounded_wire_text(&error.to_string(), 1024)
                                ),
                            });
                        }
                        if pending {
                            let _ = api.abort_prompt(&lease.session_id, prompt_id).await;
                            tokio::time::sleep(REVIEW_PROMPT_POLL_INTERVAL).await;
                            continue;
                        }
                        clear_review_lease_if_nonce(&leases, &lease.nonce).await;
                        if let Some(events) = events.as_ref() {
                            let _ = events.send(AgentEvent::Log {
                                level: "warn".into(),
                                message: "Kimi review ended, but current tools could not be verified; preserving them instead of risking an overwrite"
                                    .into(),
                            });
                        }
                        return;
                    }
                }
                if pending {
                    tokio::time::sleep(REVIEW_PROMPT_POLL_INTERVAL).await;
                    continue;
                }
                match restore_review_tools_if_unchanged(&rpc, &lease).await {
                    Ok(restored) => {
                        clear_review_lease_if_nonce(&leases, &lease.nonce).await;
                        if !restored {
                            if let Some(events) = events.as_ref() {
                                let _ = events.send(AgentEvent::Log {
                                level: "info".into(),
                                message: "Kimi review finished; preserving a newer operator tool-set change"
                                    .into(),
                            });
                            }
                        }
                        return;
                    }
                    Err(error) => {
                        if let Some(events) = events.as_ref() {
                            let _ = events.send(AgentEvent::Log {
                                level: "warn".into(),
                                message: format!(
                                    "Kimi review finished, but tool restoration will retry: {}",
                                    bounded_wire_text(&error.to_string(), 1024)
                                ),
                            });
                        }
                    }
                }
            }
            Err(error) => {
                if let Some(events) = events.as_ref() {
                    let _ = events.send(AgentEvent::Log {
                        level: "warn".into(),
                        message: format!(
                            "Could not inspect Kimi review prompt state; restoration will retry: {}",
                            bounded_wire_text(&error.to_string(), 1024)
                        ),
                    });
                }
            }
        }
        tokio::time::sleep(REVIEW_PROMPT_POLL_INTERVAL).await;
    }
}

fn validate_permission_mode(mode: &str) -> Result<(), CallerError> {
    if matches!(mode, "manual" | "auto" | "yolo") {
        Ok(())
    } else {
        Err(external(format!(
            "invalid Kimi permission mode {mode:?}; expected manual, auto, or yolo"
        )))
    }
}

fn kimi_permission_kind(permission: &str) -> Option<&'static str> {
    match permission {
        "manual" => Some(intendant_core::vitals::PERMISSION_KIND_ASK),
        "auto" => Some(intendant_core::vitals::PERMISSION_KIND_AUTO_SAFE),
        "yolo" => Some(intendant_core::vitals::PERMISSION_KIND_BYPASS),
        _ => None,
    }
}

fn string_param(params: &Value, names: &[&str], label: &str) -> Result<String, CallerError> {
    names
        .iter()
        .find_map(|name| params.get(*name))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| external(format!("{label} requires a non-empty value")))
}

fn boolean_param(params: &Value, field: &str) -> Result<bool, CallerError> {
    params
        .get(field)
        .or_else(|| params.get("enabled"))
        .or_else(|| params.get("value"))
        .and_then(|value| {
            value.as_bool().or_else(|| {
                value.as_str().and_then(|value| match value {
                    "on" | "true" | "enabled" | "yes" => Some(true),
                    "off" | "false" | "disabled" | "no" => Some(false),
                    _ => None,
                })
            })
        })
        .ok_or_else(|| external(format!("{field} requires true or false")))
}

fn exact_tool_names_param(params: &Value) -> Result<Vec<String>, CallerError> {
    let values = params
        .get("names")
        .or_else(|| params.get("tools"))
        .and_then(Value::as_array)
        .ok_or_else(|| external("tools-set requires a names array"))?;
    if values.len() > 512 {
        return Err(external("Kimi active-tool replacement exceeds 512 names"));
    }
    let mut names = Vec::with_capacity(values.len());
    for value in values {
        let name = value
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| external("Kimi tool names must be non-empty strings"))?;
        if name.chars().count() > 256 {
            return Err(external("Kimi tool name exceeds 256 characters"));
        }
        names.push(name.to_string());
    }
    Ok(sorted_unique_tool_names(names))
}

fn sorted_unique_tool_names(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names
}

fn active_tool_names(inventory: &[KimiRpcTool]) -> Vec<String> {
    sorted_unique_tool_names(
        inventory
            .iter()
            .filter(|tool| tool.active)
            .map(|tool| tool.name.clone())
            .collect(),
    )
}

fn validate_requested_tool_names(
    inventory: &[KimiRpcTool],
    names: &[String],
) -> Result<(), CallerError> {
    let registered = inventory
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<HashSet<_>>();
    let unknown = names
        .iter()
        .filter(|name| !registered.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(external(format!(
            "unknown Kimi tool name(s): {}",
            unknown.join(", ")
        )))
    }
}

fn resolve_catalog_model<'a>(
    models: &'a [KimiRpcModel],
    requested: &str,
) -> Result<&'a KimiRpcModel, CallerError> {
    let requested = requested.trim();
    let mut matches = models.iter().filter(|model| {
        model.model.eq_ignore_ascii_case(requested)
            || model
                .display_name
                .as_deref()
                .is_some_and(|display| display.eq_ignore_ascii_case(requested))
    });
    let Some(found) = matches.next() else {
        return Err(external(format!(
            "Kimi model {requested:?} is not present in the configured catalog"
        )));
    };
    if matches.next().is_some() {
        return Err(external(format!(
            "Kimi model label {requested:?} is ambiguous; use its full model id"
        )));
    }
    Ok(found)
}

fn model_is_highspeed(model: &KimiRpcModel) -> bool {
    model.model.ends_with("-highspeed")
        || model
            .display_name
            .as_deref()
            .is_some_and(|name| name.to_ascii_lowercase().contains("highspeed"))
}

fn paired_fast_model<'a>(
    models: &'a [KimiRpcModel],
    current: &KimiRpcModel,
) -> Result<&'a KimiRpcModel, CallerError> {
    let target = if model_is_highspeed(current) {
        current
            .model
            .strip_suffix("-highspeed")
            .ok_or_else(|| {
                external(format!(
                    "Kimi model {} is labelled highspeed but has no reversible alias",
                    current.model
                ))
            })?
            .to_string()
    } else {
        format!("{}-highspeed", current.model)
    };
    models
        .iter()
        .find(|candidate| candidate.model == target && candidate.provider == current.provider)
        .ok_or_else(|| {
            external(format!(
                "Kimi model {} has no configured same-provider {} companion",
                current.model,
                if model_is_highspeed(current) {
                    "standard"
                } else {
                    "highspeed"
                }
            ))
        })
}

fn positive_safe_u64_param(
    params: &Value,
    names: &[&str],
    label: &str,
) -> Result<Option<u64>, CallerError> {
    let Some(value) = names.iter().find_map(|name| params.get(*name)) else {
        return Ok(None);
    };
    let value = value
        .as_u64()
        .ok_or_else(|| external(format!("{label} must be a positive integer")))?;
    if value == 0 || value > JSON_SAFE_INTEGER_MAX {
        return Err(external(format!(
            "{label} must be between 1 and {JSON_SAFE_INTEGER_MAX}"
        )));
    }
    Ok(Some(value))
}

fn goal_budget_limits(params: &Value) -> Result<KimiGoalBudgetLimits, CallerError> {
    let token_budget =
        positive_safe_u64_param(params, &["token_budget", "tokenBudget"], "token budget")?;
    let turn_budget =
        positive_safe_u64_param(params, &["turn_budget", "turnBudget"], "turn budget")?;
    let wall_clock_budget_ms = positive_safe_u64_param(
        params,
        &["wall_clock_budget_ms", "wallClockBudgetMs"],
        "wall-clock budget milliseconds",
    )?;
    let wall_clock_budget_seconds = positive_safe_u64_param(
        params,
        &[
            "wall_clock_budget_seconds",
            "wallClockBudgetSeconds",
            "wall_clock_seconds",
        ],
        "wall-clock budget seconds",
    )?;
    if wall_clock_budget_ms.is_some() && wall_clock_budget_seconds.is_some() {
        return Err(external(
            "provide wall-clock goal budget in milliseconds or seconds, not both",
        ));
    }
    let wall_clock_budget_ms = match (wall_clock_budget_ms, wall_clock_budget_seconds) {
        (Some(milliseconds), None) => Some(milliseconds),
        (None, Some(seconds)) => Some(
            seconds
                .checked_mul(1000)
                .ok_or_else(|| external("wall-clock goal budget seconds overflow milliseconds"))?,
        ),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!(),
    };
    if wall_clock_budget_ms.is_some_and(|value| value > JSON_SAFE_INTEGER_MAX) {
        return Err(external(format!(
            "wall-clock goal budget must not exceed {JSON_SAFE_INTEGER_MAX} milliseconds"
        )));
    }
    Ok(KimiGoalBudgetLimits {
        token_budget,
        turn_budget,
        wall_clock_budget_ms,
    })
}

fn goal_params_are_read_only(params: &Value) -> bool {
    params.as_object().is_none_or(|params| {
        params
            .keys()
            .all(|key| matches!(key.as_str(), "threadId" | "thread_id"))
    })
}

fn bounded_wire_text(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for character in value.chars().take(max_chars) {
        if character.is_control() {
            output.push(' ');
        } else {
            output.push(character);
        }
    }
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

fn find_interaction(list: &Value, id_field: &str, request_id: &str) -> Option<Value> {
    list.get("items")
        .and_then(Value::as_array)?
        .iter()
        .find(|item| item.get(id_field).and_then(Value::as_str) == Some(request_id))
        .cloned()
}

fn session_goal(goal: &Value) -> Option<crate::types::SessionGoal> {
    Some(crate::types::SessionGoal {
        objective: goal.get("objective")?.as_str()?.to_string(),
        status: normalize_goal_status(goal),
        elapsed_seconds: goal
            .get("wallClockMs")
            .and_then(Value::as_u64)
            .map(|milliseconds| milliseconds / 1000),
        tokens_used: goal.get("tokensUsed").and_then(Value::as_u64),
        token_budget: goal
            .get("budget")
            .and_then(|budget| budget.get("tokenBudget"))
            .and_then(Value::as_u64),
    })
}

fn goal_status_message(goal: &Value) -> String {
    session_goal(goal)
        .map(|projected| {
            let mut message = format!(
                "goal {}: {}",
                projected.status.as_deref().unwrap_or("active"),
                projected.objective
            );
            if let Some(budget) = goal.get("budget").and_then(Value::as_object) {
                let mut limits = Vec::new();
                for (wire, label) in [
                    ("tokenBudget", "token_budget"),
                    ("turnBudget", "turn_budget"),
                    ("wallClockBudgetMs", "wall_clock_budget_ms"),
                ] {
                    if let Some(value) = budget.get(wire).and_then(Value::as_u64) {
                        limits.push(format!("{label}={value}"));
                    }
                }
                if !limits.is_empty() {
                    message.push_str(" [");
                    message.push_str(&limits.join(", "));
                    message.push(']');
                }
            }
            message
        })
        .unwrap_or_else(|| "no active goal".into())
}

fn verify_expected_horizon(
    expected: &crate::web_gateway::session_catalog::kimi_history::KimiTurnHorizon,
    actual: &crate::web_gateway::session_catalog::kimi_history::KimiTurnHorizon,
) -> Result<(), CallerError> {
    if expected == actual {
        return Ok(());
    }
    Err(external(format!(
        "Kimi fork source changed after the anchor was planned \
         (expected {} active/{} undoable turns, observed {}/{}); retry from the refreshed history",
        expected.active_turns, expected.undoable_turns, actual.active_turns, actual.undoable_turns,
    )))
}

fn validate_fork_staging(
    resume_session: Option<&str>,
    fork_resume: bool,
    rollback_turns: Option<u32>,
    has_expected_horizon: bool,
) -> Result<(), CallerError> {
    if rollback_turns == Some(0) {
        return Err(external(
            "Kimi fork rollback count must be a positive integer",
        ));
    }
    if rollback_turns.is_some() && !fork_resume {
        return Err(external(
            "Kimi fork rollback staging requires a native fork resume",
        ));
    }
    if has_expected_horizon && !fork_resume {
        return Err(external(
            "Kimi fork expected-head validation requires a native fork resume",
        ));
    }
    if rollback_turns.is_some() && !has_expected_horizon {
        return Err(external(
            "Kimi anchor-fork rollback requires an expected-head horizon",
        ));
    }
    if fork_resume && resume_session.and_then(split_child_thread_id).is_some() {
        return Err(external(
            "Kimi cannot fork a composite :btw child conversation",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    use super::*;

    fn launch() -> KimiLaunchConfig {
        KimiLaunchConfig {
            model: Some("k2.7 coding".into()),
            thinking: Some("high".into()),
            permission_mode: "manual".into(),
            allowed_tools: None,
            plan_mode: false,
            swarm_mode: true,
        }
    }

    #[tokio::test]
    async fn captured_server_token_is_removed_after_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.token");
        tokio::fs::write(&path, "0123456789abcdef").await.unwrap();

        remove_captured_server_token(&path).await.unwrap();

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn child_thread_actions_target_the_exact_kimi_agent() {
        let (origin, requests, server) = sequential_mock_server(vec![
            serde_json::json!({
                "code": 0,
                "data": [{
                    "name": "Read",
                    "description": "read files",
                    "active": true,
                    "source": "builtin"
                }]
            }),
            serde_json::json!({"code": 0, "data": null}),
        ])
        .await;
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        agent.rpc = Some(KimiRpcApi::new(origin, "test-token".into()).unwrap());
        agent
            .shared
            .set_session_id(Some("session-parent".to_string()));
        let params = serde_json::json!({"threadId": "session-parent:agent-0"});

        let report = agent.thread_action("tools", &params).await.unwrap();
        assert!(report.contains("Read"));
        assert_eq!(
            agent.thread_action("context-clear", &params).await.unwrap(),
            "Kimi agent agent-0 context cleared"
        );

        let requests = requests.await.unwrap();
        let (tools_line, _) = request_line_and_body(&requests[0]);
        assert_eq!(
            tools_line,
            "POST /api/v2/session/session-parent/agent/agent-0/agentRPCService/getTools HTTP/1.1"
        );
        let (clear_line, _) = request_line_and_body(&requests[1]);
        assert_eq!(
            clear_line,
            "POST /api/v2/session/session-parent/agent/agent-0/agentRPCService/clearContext HTTP/1.1"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn context_snapshot_is_backend_native_and_exactly_child_scoped() {
        let context = serde_json::json!({
            "history": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "side question"}],
                    "toolCalls": []
                },
                {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "side answer"}],
                    "toolCalls": []
                }
            ],
            "tokenCount": 731
        });
        let (origin, requests, server) = sequential_mock_server(vec![
            serde_json::json!({"code": 0, "data": context.clone()}),
            serde_json::json!({
                "code": 0,
                "data": {
                    "modelAlias": "kimi-code/kimi-for-coding",
                    "profileName": "default",
                    "thinkingLevel": "high"
                }
            }),
            serde_json::json!({
                "code": 0,
                "data": [{
                    "provider": "kimi-code",
                    "model": "kimi-code/kimi-for-coding",
                    "display_name": "K2.7 Coding",
                    "max_context_size": 262144
                }]
            }),
        ])
        .await;
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        agent.rpc = Some(KimiRpcApi::new(origin, "test-token".into()).unwrap());
        agent
            .shared
            .set_session_id(Some("session-parent".to_string()));
        agent
            .shared
            .set_active_agent_id(Some("agent-0".to_string()));

        let snapshot = agent.context_snapshot().await.unwrap().unwrap();
        assert_eq!(snapshot.source, "kimi");
        assert_eq!(
            snapshot.label,
            "Kimi current model context (session-parent:agent-0)"
        );
        assert_eq!(snapshot.format, "kimi.agent_rpc.context.v1");
        assert_eq!(snapshot.token_count, Some(731));
        assert_eq!(
            snapshot.token_count_kind,
            Some(AgentContextTokenCountKind::BackendReported)
        );
        assert_eq!(snapshot.context_window, Some(262_144));
        assert_eq!(snapshot.hard_context_window, Some(262_144));
        assert_eq!(snapshot.item_count, Some(2));
        assert_eq!(snapshot.request_id, None);
        assert_eq!(snapshot.raw, context);

        let requests = requests.await.unwrap();
        assert_eq!(
            request_line_and_body(&requests[0]).0,
            "POST /api/v2/session/session-parent/agent/agent-0/agentRPCService/getContext HTTP/1.1"
        );
        assert_eq!(
            request_line_and_body(&requests[1]).0,
            "POST /api/v2/session/session-parent/agent/agent-0/agentProfileService/data HTTP/1.1"
        );
        assert_eq!(
            request_line_and_body(&requests[2]).0,
            "POST /api/v2/modelCatalogService/listModels HTTP/1.1"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn context_snapshot_is_capability_gated_until_a_native_session_exists() {
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        assert!(agent.context_snapshot().await.unwrap().is_none());
    }

    async fn sequential_mock_server(
        responses: Vec<Value>,
    ) -> (
        String,
        oneshot::Receiver<Vec<Vec<u8>>>,
        tokio::task::JoinHandle<()>,
    ) {
        sequential_mock_server_with_hook(responses, std::sync::Arc::new(|_| {})).await
    }

    async fn sequential_mock_server_with_hook(
        responses: Vec<Value>,
        hook: std::sync::Arc<dyn Fn(usize) + Send + Sync>,
    ) -> (
        String,
        oneshot::Receiver<Vec<Vec<u8>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (requests_tx, requests_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for (index, response) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                let expected_len = loop {
                    let read = stream.read(&mut buf).await.unwrap();
                    assert!(read > 0, "mock client closed before request completed");
                    request.extend_from_slice(&buf[..read]);
                    let Some(header_end) = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|position| position + 4)
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    break header_end + content_len;
                };
                while request.len() < expected_len {
                    let read = stream.read(&mut buf).await.unwrap();
                    assert!(read > 0, "mock client closed before body completed");
                    request.extend_from_slice(&buf[..read]);
                }
                hook(index);
                let body = response.to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).await.unwrap();
                requests.push(request);
            }
            let _ = requests_tx.send(requests);
        });
        (format!("http://{address}"), requests_rx, handle)
    }

    fn request_line_and_body(request: &[u8]) -> (&str, Value) {
        let raw = std::str::from_utf8(request).unwrap();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        (
            headers.lines().next().unwrap(),
            serde_json::from_str(body).unwrap(),
        )
    }

    fn seed_kimi_history(
        bridge_home: &Path,
        session_id: &str,
        prompts: &[&str],
    ) -> crate::web_gateway::session_catalog::kimi_history::KimiTurnHorizon {
        let session = bridge_home
            .join("sessions")
            .join("workspace")
            .join(session_id);
        let agent = session.join("agents/main");
        fs::create_dir_all(&agent).unwrap();
        fs::write(
            session.join("state.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": session_id,
                "agents": {
                    "main": {
                        "type": "main",
                        "parentAgentId": null
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let wire = prompts
            .iter()
            .enumerate()
            .map(|(index, prompt)| {
                serde_json::json!({
                    "type": "turn.prompt",
                    "input": [{"type": "text", "text": prompt}],
                    "origin": {"kind": "user"},
                    "time": 1_700_000_000_000i64 + index as i64,
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(agent.join("wire.jsonl"), wire).unwrap();
        crate::web_gateway::session_catalog::kimi_history::kimi_turn_horizon_in(
            bridge_home,
            session_id,
        )
        .unwrap()
    }

    fn fork_fixture(
        agent: &mut KimiCodeAgent,
        parent_prompts: &[&str],
        child_prompts: &[&str],
    ) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let bridge = primary.join("intendant-bridges").join("session-test");
        fs::create_dir_all(&bridge).unwrap();
        let expected = seed_kimi_history(&bridge, "session_parent", parent_prompts);
        seed_kimi_history(&bridge, "session_child", child_prompts);
        agent.bridge_home = Some(bridge);
        agent.fork_expected_horizon = Some(expected);
        temp
    }

    #[test]
    fn constructor_and_session_config_preserve_rich_launch_options() {
        let agent = KimiCodeAgent::new("kimi".into(), launch(), Some(8765));
        assert_eq!(
            agent.session_agent_config(),
            serde_json::json!({
                "model": "k2.7 coding",
                "thinking": "high",
                "permission_mode": "manual",
                "plan_mode": false,
                "swarm_mode": true,
            })
        );
    }

    #[test]
    fn banner_parser_drops_fragment_bearing_token() {
        let line = "\u{1b}[36mLocal:    http://127.0.0.1:51035/#token=top-secret\u{1b}[0m";
        let origin = extract_loopback_origin(line).unwrap();
        assert_eq!(origin, "http://127.0.0.1:51035");
        assert!(!origin.contains("secret"));
    }

    #[test]
    fn permission_and_boolean_modes_fail_closed() {
        assert!(validate_permission_mode("manual").is_ok());
        assert!(validate_permission_mode("default").is_err());
        assert_eq!(
            boolean_param(&serde_json::json!({"value": "enabled"}), "plan_mode").unwrap(),
            true
        );
        assert!(boolean_param(&serde_json::json!({"value": "maybe"}), "plan_mode").is_err());
    }

    #[tokio::test]
    async fn unknown_review_submission_aborts_only_new_prompt_before_restore() {
        let existing = serde_json::json!({
            "active": null,
            "queued": [{"prompt_id": "existing"}]
        });
        let mut responses = vec![
            serde_json::json!({
                "code": 0,
                "data": {
                    "active": {"prompt_id": "review-new"},
                    "queued": [{"prompt_id": "existing"}]
                }
            }),
            serde_json::json!({"code": 0, "data": {"aborted": true}}),
        ];
        responses.extend(
            std::iter::repeat_with(|| serde_json::json!({"code": 0, "data": existing.clone()}))
                .take(20),
        );
        let (origin, requests, server) = sequential_mock_server(responses).await;
        let api = KimiApi::new(origin, "test-token".into()).unwrap();
        let lease = KimiReviewToolLease {
            nonce: "nonce".into(),
            session_id: "session_review".into(),
            agent_id: "main".into(),
            prompt_id: None,
            baseline_prompt_ids: HashSet::from(["existing".to_string()]),
            previous_tools: vec!["Write".into()],
            review_tools: Vec::new(),
        };

        assert!(stop_review_prompt_before_restore(&api, &lease)
            .await
            .unwrap());
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 22);
        assert!(String::from_utf8_lossy(&requests[1])
            .starts_with("POST /api/v1/sessions/session_review/prompts/review-new:abort "));
        assert!(!requests
            .iter()
            .any(|request| String::from_utf8_lossy(request).contains("prompts/existing:abort")));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn review_prompt_id_collision_never_aborts_baseline_prompt() {
        let api = KimiApi::new("http://127.0.0.1:1".into(), "test-token".into()).unwrap();
        let lease = KimiReviewToolLease {
            nonce: "nonce".into(),
            session_id: "session_review".into(),
            agent_id: "main".into(),
            prompt_id: Some("existing".into()),
            baseline_prompt_ids: HashSet::from(["existing".to_string()]),
            previous_tools: vec!["Write".into()],
            review_tools: Vec::new(),
        };
        assert!(stop_review_prompt_before_restore(&api, &lease)
            .await
            .unwrap_err()
            .to_string()
            .contains("collided"));
    }

    #[test]
    fn goal_projection_keeps_native_budget_and_elapsed_time() {
        let goal = session_goal(&serde_json::json!({
            "objective": "ship it",
            "status": "active",
            "wallClockMs": 4500,
            "tokensUsed": 123,
            "budget": {"tokenBudget": 900}
        }))
        .unwrap();
        assert_eq!(goal.objective, "ship it");
        assert_eq!(goal.elapsed_seconds, Some(4));
        assert_eq!(goal.tokens_used, Some(123));
        assert_eq!(goal.token_budget, Some(900));
    }

    #[test]
    fn native_side_id_round_trips() {
        let child = child_thread_id("session_x", "agent-0");
        assert_eq!(child, "session_x:agent-0");
        assert_eq!(
            split_child_thread_id(&child),
            Some(("session_x", "agent-0"))
        );
    }

    #[test]
    fn launch_config_has_requested_public_shape() {
        let launch = launch();
        assert_eq!(launch.model.as_deref(), Some("k2.7 coding"));
        assert_eq!(launch.thinking.as_deref(), Some("high"));
        assert_eq!(launch.permission_mode, "manual");
        assert!(!launch.plan_mode);
        assert!(launch.swarm_mode);
    }

    #[test]
    fn tool_completion_status_import_stays_live() {
        let status = super::super::ToolCompletionStatus::Cancelled;
        assert!(matches!(
            status,
            super::super::ToolCompletionStatus::Cancelled
        ));
    }

    #[test]
    fn anchor_fork_staging_rejects_stale_and_composite_inputs() {
        assert!(validate_fork_staging(Some("session_parent"), true, Some(3), true).is_ok());
        assert!(validate_fork_staging(Some("session_parent"), true, None, true).is_ok());
        assert!(validate_fork_staging(Some("session_parent"), true, Some(3), false).is_err());
        assert!(validate_fork_staging(Some("session_parent"), true, Some(0), true).is_err());
        assert!(validate_fork_staging(Some("session_parent"), false, Some(3), true).is_err());
        assert!(validate_fork_staging(Some("session_parent:btw-1"), true, Some(3), true).is_err());
    }

    #[tokio::test]
    async fn anchor_fork_rolls_back_child_before_exposing_it() {
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        agent.fork_rollback_turns = Some(3);
        let _fixture = fork_fixture(
            &mut agent,
            &["one", "two", "three", "four"],
            &["one", "two", "three", "four"],
        );
        let bridge = agent.bridge_home.clone().unwrap();
        let (origin, requests, server) = sequential_mock_server_with_hook(
            vec![
                serde_json::json!({
                    "code": 0,
                    "data": {"id": "session_child", "metadata": {"cwd": "/repo"}}
                }),
                serde_json::json!({
                    "code": 0,
                    "data": {
                        "messages": {"items": [], "has_more": false},
                        "status": {"busy": false}
                    }
                }),
            ],
            std::sync::Arc::new(move |index| {
                if index == 1 {
                    seed_kimi_history(&bridge, "session_child", &["one"]);
                }
            }),
        )
        .await;
        agent.api = Some(KimiApi::new(origin, "test-token".into()).unwrap());

        let session = agent.fork_resumed_session("session_parent").await.unwrap();
        assert_eq!(session["id"], "session_child");
        assert!(agent.shared.session_id().is_none());
        assert_eq!(agent.fork_rollback_turns, None);

        let requests = requests.await.unwrap();
        let (fork_line, fork_body) = request_line_and_body(&requests[0]);
        assert_eq!(
            fork_line,
            "POST /api/v1/sessions/session_parent:fork HTTP/1.1"
        );
        assert_eq!(fork_body, serde_json::json!({}));
        let (undo_line, undo_body) = request_line_and_body(&requests[1]);
        assert_eq!(
            undo_line,
            "POST /api/v1/sessions/session_child:undo HTTP/1.1"
        );
        assert_eq!(undo_body, serde_json::json!({"count": 3}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn successful_undo_response_without_the_proven_horizon_archives_child() {
        let (origin, requests, server) = sequential_mock_server(vec![
            serde_json::json!({"code": 0, "data": {"id": "session_child"}}),
            serde_json::json!({
                "code": 0,
                "data": {
                    "messages": {"items": [], "has_more": false},
                    "status": {"busy": false}
                }
            }),
            serde_json::json!({"code": 0, "data": {"archived": true}}),
        ])
        .await;
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        agent.api = Some(KimiApi::new(origin, "test-token".into()).unwrap());
        agent.fork_rollback_turns = Some(1);
        let _fixture = fork_fixture(&mut agent, &["one", "two"], &["one", "two"]);

        let error = agent
            .fork_resumed_session("session_parent")
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("did not materialize the verified target horizon"));
        assert!(agent.shared.session_id().is_none());

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 3);
        let (archive_line, archive_body) = request_line_and_body(&requests[2]);
        assert_eq!(
            archive_line,
            "POST /api/v1/sessions/session_child:archive HTTP/1.1"
        );
        assert_eq!(archive_body, serde_json::json!({}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn failed_anchor_rollback_archives_unpublished_child() {
        let (origin, requests, server) = sequential_mock_server(vec![
            serde_json::json!({"code": 0, "data": {"id": "session_child"}}),
            serde_json::json!({
                "code": 40033,
                "msg": "Nothing to undo: only 1 of 3 requested turn(s) available",
                "data": null
            }),
            serde_json::json!({"code": 0, "data": {"archived": true}}),
        ])
        .await;
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        agent.api = Some(KimiApi::new(origin, "test-token".into()).unwrap());
        agent.fork_rollback_turns = Some(3);
        let _fixture = fork_fixture(
            &mut agent,
            &["one", "two", "three", "four"],
            &["one", "two", "three", "four"],
        );

        let error = agent
            .fork_resumed_session("session_parent")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("only 1 of 3"));
        assert!(agent.shared.session_id().is_none());

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 3);
        let (archive_line, archive_body) = request_line_and_body(&requests[2]);
        assert_eq!(
            archive_line,
            "POST /api/v1/sessions/session_child:archive HTTP/1.1"
        );
        assert_eq!(archive_body, serde_json::json!({}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_fork_response_never_undoes_or_archives_the_parent() {
        let (origin, requests, server) = sequential_mock_server(vec![serde_json::json!({
            "code": 0,
            "data": {"id": "session_parent"}
        })])
        .await;
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        agent.api = Some(KimiApi::new(origin, "test-token".into()).unwrap());

        let error = agent
            .fork_resumed_session("session_parent")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("distinct child"));

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 1);
        let (fork_line, _) = request_line_and_body(&requests[0]);
        assert_eq!(
            fork_line,
            "POST /api/v1/sessions/session_parent:fork HTTP/1.1"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn anchor_fork_rejects_a_parent_that_moved_after_planning() {
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        agent.api = Some(KimiApi::new("http://127.0.0.1:1".into(), "test-token".into()).unwrap());
        agent.fork_rollback_turns = Some(1);
        let _fixture = fork_fixture(&mut agent, &["planned"], &["planned"]);
        let bridge = agent.bridge_home.clone().unwrap();
        seed_kimi_history(&bridge, "session_parent", &["planned", "raced"]);

        let error = agent
            .fork_resumed_session("session_parent")
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("changed after the anchor was planned"));
        assert_eq!(agent.fork_rollback_turns, Some(1));
    }

    #[tokio::test]
    async fn anchor_fork_archives_a_child_with_a_mismatched_head() {
        let (origin, requests, server) = sequential_mock_server(vec![
            serde_json::json!({"code": 0, "data": {"id": "session_child"}}),
            serde_json::json!({"code": 0, "data": {"archived": true}}),
        ])
        .await;
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        agent.api = Some(KimiApi::new(origin, "test-token".into()).unwrap());
        agent.fork_rollback_turns = Some(1);
        let _fixture = fork_fixture(&mut agent, &["planned"], &["planned", "unexpected"]);

        let error = agent
            .fork_resumed_session("session_parent")
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("changed after the anchor was planned"));
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        let (archive_line, archive_body) = request_line_and_body(&requests[1]);
        assert_eq!(
            archive_line,
            "POST /api/v1/sessions/session_child:archive HTTP/1.1"
        );
        assert_eq!(archive_body, serde_json::json!({}));
        server.await.unwrap();
    }

    #[test]
    fn equal_turn_counts_with_different_head_fingerprints_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let expected = seed_kimi_history(temp.path(), "session_parent", &["one", "two"]);
        let mut actual = expected.clone();
        actual.head_fingerprint = "b".repeat(64);
        assert!(verify_expected_horizon(&expected, &actual).is_err());
    }

    #[tokio::test]
    async fn start_thread_applies_launch_profile_before_subscription() {
        let profile = serde_json::json!({
            "id": "session_child",
            "metadata": {"cwd": "/repo"},
            "agent_config": {
                "model": "k2.7 coding",
                "thinking": "high",
                "permission_mode": "manual",
                "plan_mode": false,
                "swarm_mode": true
            }
        });
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        agent.launch.allowed_tools = Some(Vec::new());
        agent.resume_session = Some("session_parent".into());
        agent.fork_resume = true;
        agent.fork_rollback_turns = Some(2);
        let _fixture = fork_fixture(
            &mut agent,
            &["one", "two", "three"],
            &["one", "two", "three"],
        );
        let bridge = agent.bridge_home.clone().unwrap();
        let (origin, requests, server) = sequential_mock_server_with_hook(
            vec![
                serde_json::json!({"code": 0, "data": {"id": "session_child"}}),
                serde_json::json!({
                    "code": 0,
                    "data": {
                        "messages": {"items": [], "has_more": false},
                        "status": {"busy": false}
                    }
                }),
                serde_json::json!({"code": 0, "data": profile}),
                serde_json::json!({
                    "code": 0,
                    "data": [{
                        "name": "Read",
                        "description": "read files",
                        "active": true,
                        "source": "builtin"
                    }]
                }),
                serde_json::json!({"code": 0, "data": null}),
                serde_json::json!({
                    "code": 0,
                    "data": {
                        "modelAlias": "k2.7 coding",
                        "thinkingLevel": "high",
                        "activeToolNames": []
                    }
                }),
                serde_json::json!({"code": 0, "data": {"warnings": []}}),
            ],
            std::sync::Arc::new(move |index| {
                if index == 1 {
                    seed_kimi_history(&bridge, "session_child", &["one"]);
                }
            }),
        )
        .await;
        agent.api = Some(KimiApi::new(origin.clone(), "test-token".into()).unwrap());
        agent.rpc = Some(KimiRpcApi::new(origin, "test-token".into()).unwrap());
        let (ws_tx, mut ws_rx) = mpsc::unbounded_channel();
        agent.ws_tx = Some(ws_tx);

        let thread = agent.start_thread().await.unwrap();
        assert_eq!(thread.thread_id, "session_child");
        assert_eq!(agent.shared.session_id().as_deref(), Some("session_child"));

        let requests = requests.await.unwrap();
        let (profile_line, profile_body) = request_line_and_body(&requests[2]);
        assert_eq!(
            profile_line,
            "POST /api/v1/sessions/session_child/profile HTTP/1.1"
        );
        assert_eq!(
            profile_body,
            serde_json::json!({"agent_config": agent.session_agent_config()})
        );
        let (tools_line, _) = request_line_and_body(&requests[3]);
        assert!(tools_line.contains("/agentRPCService/getTools "));
        let (set_tools_line, set_tools_body) = request_line_and_body(&requests[4]);
        assert!(set_tools_line.contains("/agentRPCService/setActiveTools "));
        assert_eq!(set_tools_body, serde_json::json!({"names": []}));
        assert!(matches!(
            ws_rx.try_recv().unwrap(),
            WsCommand::Subscribe {
                session_id,
                snapshot_first: true
            } if session_id == "session_child"
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn failed_new_session_configuration_archives_the_unpublished_session() {
        let (origin, requests, server) = sequential_mock_server(vec![
            serde_json::json!({"code": 0, "data": {"id": "session_new"}}),
            serde_json::json!({
                "code": 40031,
                "msg": "invalid profile",
                "data": null
            }),
            serde_json::json!({"code": 0, "data": {"archived": true}}),
        ])
        .await;
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = KimiCodeAgent::new("kimi".into(), launch(), None);
        agent.api = Some(KimiApi::new(origin, "test-token".into()).unwrap());
        agent.working_dir = Some(workspace.path().to_path_buf());

        let error = agent.start_thread().await.unwrap_err();
        assert!(error.to_string().contains("invalid profile"));
        assert!(agent.shared.session_id().is_none());

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 3);
        let (archive_line, archive_body) = request_line_and_body(&requests[2]);
        assert_eq!(
            archive_line,
            "POST /api/v1/sessions/session_new:archive HTTP/1.1"
        );
        assert_eq!(archive_body, serde_json::json!({}));
        server.await.unwrap();
    }
}

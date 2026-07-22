//! Pi coding-agent external-agent adapter.
//!
//! Intendant runs the upstream CLI unchanged in RPC mode. Discovery of user
//! and project extensions is disabled and one private, Intendant-owned
//! extension is loaded explicitly. That extension is the fail-closed policy
//! seam: Pi's read-only built-ins pass, while every mutating or unknown tool
//! blocks on Intendant's existing approval rail.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::error::CallerError;
use crate::session_activity::{ActivityMachine, ActivityObservation};

use super::{
    AgentConfig, AgentEvent, AgentImageAttachment, AgentThread, AgentUsageSnapshot,
    ApprovalCategory, ApprovalDecision, ExternalAgent, ForkHandling, ToolCompletionStatus,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(25);
const STARTUP_PRELUDE_LIMIT: usize = 1_024;
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const COMPACTION_TIMEOUT: Duration = Duration::from_secs(300);
const APPROVAL_MARKER: &str = "INTENDANT_PI_APPROVAL_V1:";
const APPROVE_ONCE: &str = "Approve once";
const APPROVE_SESSION: &str = "Approve this tool for session";
const DENY: &str = "Deny";
const MAX_PREVIEW_CHARS: usize = 2_000;
pub(crate) const PI_PERMISSION_MODE: &str = "intendant-gated";

/// The only extension admitted into a supervised Pi process. Extension
/// discovery is disabled on argv, so neither a project checkout nor the
/// operator's Pi home can run additional load-time code inside this child.
const APPROVAL_EXTENSION: &str = r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { realpath } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const MARKER = "INTENDANT_PI_APPROVAL_V1:";
const READ_ONLY = new Set(["read", "grep", "find", "ls"]);
const APPROVE_ONCE = "Approve once";
const APPROVE_SESSION = "Approve this tool for session";
const DENY = "Deny";
const AGENT_DIR = resolve(process.env.PI_CODING_AGENT_DIR || join(homedir(), ".pi", "agent"));
const UNICODE_SPACES = /[\u00A0\u2000-\u200A\u202F\u205F\u3000]/g;

function samePathOrChild(path: string, parent: string): boolean {
  const fold = (value: string) => process.platform === "win32" ? value.toLowerCase() : value;
  const candidate = fold(path);
  const root = fold(parent);
  return candidate === root || candidate.startsWith(root + sep);
}

async function targetsAgentHome(input: Record<string, unknown>): Promise<boolean> {
  let requested = typeof input.path === "string" ? input.path : ".";
  // Match Pi's own path normalizer. A lexical check that forgot any of these
  // aliases would let `read` reach auth.json without entering the approval
  // rail even though the tool later expands it to the protected directory.
  requested = requested.replace(UNICODE_SPACES, " ");
  if (requested.startsWith("@")) requested = requested.slice(1);
  if (requested === "~") requested = homedir();
  else if (requested.startsWith("~/") || (process.platform === "win32" && requested.startsWith("~\\"))) {
    requested = join(homedir(), requested.slice(2));
  }
  try {
    if (/^file:\/\//.test(requested)) requested = fileURLToPath(requested);
  } catch {
    return true;
  }
  const lexical = resolve(process.cwd(), requested);
  if (samePathOrChild(lexical, AGENT_DIR)) return true;
  try {
    return samePathOrChild(await realpath(lexical), await realpath(AGENT_DIR));
  } catch {
    return false;
  }
}

function clip(value: string, limit = 2000): string {
  const chars = Array.from(value);
  return chars.length <= limit ? value : `${chars.slice(0, limit).join("")}…`;
}

function renderInput(toolName: string, input: Record<string, unknown>): string {
  if (toolName === "bash" && typeof input.command === "string") return clip(input.command);
  if (typeof input.path === "string") return clip(input.path);
  try { return clip(JSON.stringify(input)); } catch { return "<unserializable tool input>"; }
}

export default function (pi: ExtensionAPI) {
  const approvedTools = new Set<string>();
  pi.on("tool_call", async (event, ctx) => {
    const input = (event.input ?? {}) as Record<string, unknown>;
    const protectedRead = READ_ONLY.has(event.toolName) && await targetsAgentHome(input);
    if (approvedTools.has(event.toolName) || (READ_ONLY.has(event.toolName) && !protectedRead)) return undefined;
    if (!ctx.hasUI) return { block: true, reason: "Intendant approval UI unavailable" };

    const path = typeof input.path === "string" ? clip(input.path) : undefined;
    const envelope = {
      toolCallId: event.toolCallId,
      toolName: event.toolName,
      category: event.toolName === "write" || event.toolName === "edit"
        ? "file_change"
        : protectedRead ? "permission_grant" : "command_execution",
      preview: renderInput(event.toolName, input),
      path,
    };
    const choice = await ctx.ui.select(MARKER + JSON.stringify(envelope), [
      APPROVE_ONCE,
      APPROVE_SESSION,
      DENY,
    ]);
    if (choice === APPROVE_ONCE) return undefined;
    if (choice === APPROVE_SESSION) {
      approvedTools.add(event.toolName);
      return undefined;
    }
    return { block: true, reason: "Blocked by Intendant supervision" };
  });
}
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiLaunchConfig {
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Default)]
struct PiSessionState {
    session_id: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    context_window: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalEnvelope {
    tool_call_id: String,
    tool_name: String,
    category: String,
    preview: String,
    path: Option<String>,
}

type SharedWriter = Arc<Mutex<ChildStdin>>;
type PendingRpc = Arc<StdMutex<HashMap<String, oneshot::Sender<Value>>>>;

pub struct PiAgent {
    command: String,
    launch: PiLaunchConfig,
    web_port: Option<u16>,
    child: Option<Child>,
    child_pid: Option<u32>,
    writer: Option<SharedWriter>,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    extension_dir: Option<tempfile::TempDir>,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    pending_approvals: Arc<StdMutex<HashSet<String>>>,
    pending_rpc: PendingRpc,
    state: Arc<StdMutex<PiSessionState>>,
    activity: Arc<StdMutex<ActivityMachine>>,
    resume_session: Option<String>,
    fork_resume: bool,
    mcp_auth_token: Option<String>,
    mcp_session_id: Option<String>,
    protocol_watch: Option<super::protocol_watch::ProtocolWatchHandle>,
}

impl PiAgent {
    pub fn new(command: String, launch: PiLaunchConfig, web_port: Option<u16>) -> Self {
        Self {
            command,
            launch,
            web_port,
            child: None,
            child_pid: None,
            writer: None,
            reader_handle: None,
            extension_dir: None,
            event_tx: None,
            pending_approvals: Arc::new(StdMutex::new(HashSet::new())),
            pending_rpc: Arc::new(StdMutex::new(HashMap::new())),
            state: Arc::new(StdMutex::new(PiSessionState::default())),
            activity: Arc::new(StdMutex::new(ActivityMachine::default())),
            resume_session: None,
            fork_resume: false,
            mcp_auth_token: None,
            mcp_session_id: None,
            protocol_watch: None,
        }
    }

    fn external(message: impl Into<String>) -> CallerError {
        CallerError::ExternalAgent(message.into())
    }

    fn writer(&self) -> Result<SharedWriter, CallerError> {
        self.writer
            .clone()
            .ok_or_else(|| Self::external("Pi agent is not initialized"))
    }

    fn current_session_id(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .session_id
            .clone()
    }

    fn intendant_mcp_url(&self, port: u16) -> String {
        super::intendant_bootstrap_mcp_url(
            port,
            self.mcp_session_id.as_deref(),
            None,
            self.mcp_auth_token.as_deref(),
        )
    }

    async fn write_value(writer: &SharedWriter, value: &Value) -> Result<(), CallerError> {
        let mut bytes = serde_json::to_vec(value)
            .map_err(|error| Self::external(format!("serialize Pi RPC command: {error}")))?;
        bytes.push(b'\n');
        let mut writer = writer.lock().await;
        writer
            .write_all(&bytes)
            .await
            .map_err(|error| Self::external(format!("write Pi RPC command: {error}")))?;
        writer
            .flush()
            .await
            .map_err(|error| Self::external(format!("flush Pi RPC command: {error}")))
    }

    async fn rpc_call_with_timeout(
        &self,
        mut request: Value,
        timeout: Duration,
    ) -> Result<Value, CallerError> {
        let writer = self.writer()?;
        let id = format!("intendant-{}", uuid::Uuid::new_v4().simple());
        request
            .as_object_mut()
            .ok_or_else(|| Self::external("Pi RPC request must be an object"))?
            .insert("id".into(), Value::String(id.clone()));
        let (tx, rx) = oneshot::channel();
        self.pending_rpc
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), tx);
        if let Err(error) = Self::write_value(&writer, &request).await {
            self.pending_rpc
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            return Err(error);
        }
        let response = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                self.pending_rpc
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                return Err(Self::external("Pi RPC reader stopped before responding"));
            }
            Err(_) => {
                self.pending_rpc
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                return Err(Self::external("Pi RPC response timed out"));
            }
        };
        if response.get("success").and_then(Value::as_bool) == Some(true) {
            Ok(response)
        } else {
            Err(Self::external(
                response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Pi rejected the RPC request"),
            ))
        }
    }

    async fn rpc_call(&self, request: Value) -> Result<Value, CallerError> {
        self.rpc_call_with_timeout(request, RPC_TIMEOUT).await
    }

    fn observe_activity(&self, observation: ActivityObservation) {
        let Some(tx) = self.event_tx.as_ref() else {
            return;
        };
        observe_activity(&self.activity, tx, observation);
    }

    fn emit_config_facts(&self, echoed: bool) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(tx) = self.event_tx.as_ref() {
            let _ = tx.send(AgentEvent::ConfigFacts {
                facts: crate::types::SessionConfigVitals {
                    model: state.model.clone().or_else(|| self.launch.model.clone()),
                    effort: state
                        .thinking
                        .clone()
                        .or_else(|| self.launch.thinking.clone()),
                    permission_mode: Some(PI_PERMISSION_MODE.to_string()),
                    permission_kind: Some(intendant_core::vitals::PERMISSION_KIND_ASK.to_string()),
                    permission_echoed: echoed,
                    ..Default::default()
                },
            });
        }
    }

    async fn send_prompt(
        &self,
        message: &str,
        images: &[AgentImageAttachment],
    ) -> Result<(), CallerError> {
        let mut request = json!({ "type": "prompt", "message": message });
        if !images.is_empty() {
            request.as_object_mut().expect("object").insert(
                "images".into(),
                Value::Array(
                    images
                        .iter()
                        .map(|image| {
                            json!({
                                "type": "image",
                                "data": image.base64,
                                "mimeType": image.mime_type,
                            })
                        })
                        .collect(),
                ),
            );
        }
        self.observe_activity(ActivityObservation::TurnDispatched);
        if let Err(error) = self.rpc_call(request).await {
            self.observe_activity(ActivityObservation::TurnSettled);
            return Err(error);
        }
        Ok(())
    }

    async fn set_model_live(&mut self, params: &Value) -> Result<String, CallerError> {
        let requested = params
            .get("model")
            .or_else(|| params.get("value"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(requested) = requested else {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            return Ok(match (&state.provider, &state.model) {
                (Some(provider), Some(model)) => format!("Pi model: {provider}/{model}"),
                (_, Some(model)) => format!("Pi model: {model}"),
                _ => "Pi model is not reported".to_string(),
            });
        };
        let explicit_provider = params
            .get("provider")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let (provider, model) = if let Some(provider) = explicit_provider {
            (provider.to_string(), requested.to_string())
        } else if let Some((provider, model)) = requested.split_once('/') {
            (provider.to_string(), model.to_string())
        } else {
            let provider = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .provider
                .clone()
                .ok_or_else(|| {
                    Self::external(
                        "Pi model switch needs provider/model until Pi reports a provider",
                    )
                })?;
            (provider, requested.to_string())
        };
        let response = self
            .rpc_call(json!({ "type": "set_model", "provider": provider, "modelId": model }))
            .await?;
        if let Some(model_value) = response.get("data") {
            update_model_state(&self.state, model_value);
        }
        self.launch.model = Some(format!("{provider}/{model}"));
        self.emit_config_facts(true);
        Ok(format!("Pi model set to {provider}/{model}"))
    }

    async fn set_thinking_live(&mut self, params: &Value) -> Result<String, CallerError> {
        let level = params
            .get("thinking")
            .or_else(|| params.get("effort"))
            .or_else(|| params.get("level"))
            .or_else(|| params.get("value"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(level) = level else {
            return Ok(self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .thinking
                .as_ref()
                .map(|level| format!("Pi thinking level: {level}"))
                .unwrap_or_else(|| "Pi thinking level is not reported".to_string()));
        };
        self.rpc_call(json!({ "type": "set_thinking_level", "level": level }))
            .await?;
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .thinking = Some(level.to_string());
        self.launch.thinking = Some(level.to_string());
        self.emit_config_facts(true);
        Ok(format!("Pi thinking level set to {level}"))
    }

    async fn rename_session(&self, params: &Value) -> Result<String, CallerError> {
        let name = params
            .get("name")
            .or_else(|| params.get("title"))
            .or_else(|| params.get("value"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Self::external("Pi rename requires a non-empty name"))?;
        self.rpc_call(json!({ "type": "set_session_name", "name": name }))
            .await?;
        Ok(format!("Pi session renamed to {name}"))
    }
}

#[async_trait]
impl ExternalAgent for PiAgent {
    fn name(&self) -> &str {
        "Pi"
    }

    fn launch_config_snapshot(&self) -> Option<crate::session_config::SessionAgentConfig> {
        Some(crate::session_config::SessionAgentConfig {
            source: Some("pi".to_string()),
            agent_command: Some(self.command.clone()),
            pi_model: self.launch.model.clone(),
            pi_thinking: self.launch.thinking.clone(),
            pi_allowed_tools: self.launch.allowed_tools.clone(),
            ..Default::default()
        })
    }

    async fn initialize(
        &mut self,
        config: AgentConfig,
    ) -> Result<mpsc::UnboundedReceiver<AgentEvent>, CallerError> {
        if self.child.is_some() || self.writer.is_some() {
            return Err(Self::external("Pi agent is already initialized"));
        }
        let extension_dir = tempfile::Builder::new()
            .prefix("intendant-pi-")
            .tempdir()
            .map_err(|error| Self::external(format!("create Pi extension directory: {error}")))?;
        let extension_path = extension_dir.path().join("intendant-supervision.ts");
        std::fs::write(&extension_path, APPROVAL_EXTENSION)
            .map_err(|error| Self::external(format!("write Pi approval extension: {error}")))?;

        self.resume_session = config.resume_session.clone();
        self.fork_resume = config.fork_resume;
        self.mcp_auth_token = config.mcp_auth_token.clone();
        self.mcp_session_id = config.mcp_session_id.clone();
        self.protocol_watch = config.protocol_watch.clone();
        let session_id = pi_session_id(config.mcp_session_id.as_deref());

        let args = build_pi_args(
            &extension_path,
            &session_id,
            self.resume_session.as_deref(),
            self.fork_resume,
            &self.launch,
        )?;

        let mut command = crate::platform::spawn_command(&self.command);
        command
            .args(&args)
            .current_dir(&config.working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        super::apply_external_child_env_policy(&mut command);
        command.env("PI_SKIP_VERSION_CHECK", "1");
        command.env("PI_TELEMETRY", "0");
        if let Some(port) = config.web_port.or(self.web_port) {
            let url = self.intendant_mcp_url(port);
            super::add_intendant_bootstrap_env(
                &mut command,
                &url,
                self.mcp_session_id.as_deref(),
                self.mcp_auth_token.as_deref(),
                Some(&config.working_dir),
            );
        }
        if let Some(dir) = crate::credential_leases::materialized_pi_agent_dir() {
            command.env("PI_CODING_AGENT_DIR", dir);
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
            Self::external(format!(
                "failed to spawn Pi command '{}': {error}",
                self.command
            ))
        })?;
        let child_pid = child.id();
        if let Some(pid) = child_pid {
            super::register_child_process(pid);
        }
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                terminate_spawned_child(child_pid, &mut child).await;
                return Err(Self::external("failed to capture Pi stdin"));
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                terminate_spawned_child(child_pid, &mut child).await;
                return Err(Self::external("failed to capture Pi stdout"));
            }
        };
        let stderr = child.stderr.take();

        let writer = Arc::new(Mutex::new(stdin));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        if let Some(stderr) = stderr {
            super::spawn_stderr_forwarder("Pi", stderr, event_tx.clone());
        }

        let init_id = format!("intendant-init-{}", uuid::Uuid::new_v4().simple());
        let mut lines = BufReader::new(stdout).lines();
        let handshake = tokio::time::timeout(STARTUP_TIMEOUT, async {
            Self::write_value(&writer, &json!({ "id": init_id, "type": "get_state" })).await?;
            let mut prelude = Vec::new();
            let state_value = loop {
                let line = lines
                    .next_line()
                    .await
                    .map_err(|error| Self::external(format!("read Pi RPC handshake: {error}")))?
                    .ok_or_else(|| Self::external("Pi exited before its RPC handshake"))?;
                let value: Value = serde_json::from_str(&line).map_err(|error| {
                    Self::external(format!("invalid JSON during Pi handshake: {error}"))
                })?;
                record_protocol_findings(self.protocol_watch.as_ref(), &value, &event_tx);
                if value.get("id").and_then(Value::as_str) == Some(init_id.as_str()) {
                    if value.get("success").and_then(Value::as_bool) != Some(true) {
                        return Err(Self::external(
                            value
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("Pi get_state handshake failed"),
                        ));
                    }
                    break value
                        .get("data")
                        .cloned()
                        .ok_or_else(|| Self::external("Pi get_state response omitted data"))?;
                }
                if prelude.len() >= STARTUP_PRELUDE_LIMIT {
                    return Err(Self::external(
                        "Pi emitted too many events before its get_state response",
                    ));
                }
                prelude.push(value);
            };
            update_session_state(&self.state, &state_value)?;
            Ok::<_, CallerError>((state_value, prelude))
        })
        .await
        .map_err(|_| Self::external("Pi RPC handshake timed out"))?;
        let (_state_value, prelude) = match handshake {
            Ok(values) => values,
            Err(error) => {
                terminate_spawned_child(child_pid, &mut child).await;
                return Err(error);
            }
        };
        if let Some(watch) = self.protocol_watch.as_ref() {
            watch.mark_observed(None);
        }

        self.child = Some(child);
        self.child_pid = child_pid;
        self.writer = Some(Arc::clone(&writer));
        self.extension_dir = Some(extension_dir);
        self.event_tx = Some(event_tx.clone());
        self.emit_config_facts(true);
        let _ = event_tx.send(AgentEvent::CwdAnnounced {
            cwd: config.working_dir.to_string_lossy().to_string(),
        });

        let state = Arc::clone(&self.state);
        let activity = Arc::clone(&self.activity);
        let pending_approvals = Arc::clone(&self.pending_approvals);
        let pending_rpc = Arc::clone(&self.pending_rpc);
        let watch = self.protocol_watch.clone();
        self.reader_handle = Some(tokio::spawn(async move {
            let mut reader = PiReader::new(state, Arc::clone(&activity));
            for value in prelude {
                process_reader_value(
                    &mut reader,
                    value,
                    &event_tx,
                    &writer,
                    &pending_approvals,
                    &pending_rpc,
                )
                .await;
            }
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                        Ok(value) => {
                            record_protocol_findings(watch.as_ref(), &value, &event_tx);
                            process_reader_value(
                                &mut reader,
                                value,
                                &event_tx,
                                &writer,
                                &pending_approvals,
                                &pending_rpc,
                            )
                            .await;
                        }
                        Err(error) => {
                            if let Some(watch) = watch.as_ref() {
                                for message in watch.observe_all([
                                    super::protocol_watch::ProtocolFinding::malformed(),
                                ]) {
                                    let _ = event_tx.send(AgentEvent::Log {
                                        level: "error".to_string(),
                                        message,
                                    });
                                }
                            }
                            let _ = event_tx.send(AgentEvent::BackendError {
                                message: format!("Pi emitted invalid RPC JSON: {error}"),
                                code: Some("pi_protocol_error".to_string()),
                                details: None,
                                will_retry: false,
                                likely_generation_starvation: false,
                                recovery_hint: Some(
                                    "Check the passive Pi compatibility report before retrying"
                                        .to_string(),
                                ),
                            });
                            break;
                        }
                    },
                    Ok(None) => break,
                    Err(error) => {
                        let _ = event_tx.send(AgentEvent::BackendError {
                            message: format!("Pi RPC stream failed: {error}"),
                            code: Some("pi_rpc_read".to_string()),
                            details: None,
                            will_retry: false,
                            likely_generation_starvation: false,
                            recovery_hint: None,
                        });
                        break;
                    }
                }
            }
            reader.close_open_tools(&event_tx);
            fail_pending_rpc(&pending_rpc);
            observe_activity(&activity, &event_tx, ActivityObservation::TurnSettled);
            let _ = event_tx.send(AgentEvent::Terminated {
                reason: "Pi RPC stream closed".to_string(),
                exit_code: None,
            });
        }));

        Ok(event_rx)
    }

    async fn start_thread(&mut self) -> Result<AgentThread, CallerError> {
        let thread_id = self
            .current_session_id()
            .ok_or_else(|| Self::external("Pi handshake did not report a session id"))?;
        Ok(AgentThread { thread_id })
    }

    async fn send_message(
        &mut self,
        _thread: &AgentThread,
        message: &str,
    ) -> Result<(), CallerError> {
        self.send_prompt(message, &[]).await
    }

    async fn send_message_with_images(
        &mut self,
        _thread: &AgentThread,
        message: &str,
        images: &[AgentImageAttachment],
    ) -> Result<(), CallerError> {
        self.send_prompt(message, images).await
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), CallerError> {
        let pending = self
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(request_id);
        if !pending {
            return Err(Self::external(format!(
                "Pi approval request is not pending: {request_id}"
            )));
        }
        let response = match decision {
            ApprovalDecision::Accept => {
                json!({ "type": "extension_ui_response", "id": request_id, "value": APPROVE_ONCE })
            }
            ApprovalDecision::AcceptForSession => {
                json!({ "type": "extension_ui_response", "id": request_id, "value": APPROVE_SESSION })
            }
            ApprovalDecision::Decline => {
                json!({ "type": "extension_ui_response", "id": request_id, "value": DENY })
            }
            ApprovalDecision::Cancel => {
                json!({ "type": "extension_ui_response", "id": request_id, "cancelled": true })
            }
        };
        Self::write_value(&self.writer()?, &response).await?;
        if decision == ApprovalDecision::Cancel {
            let _ = self.rpc_call(json!({ "type": "abort" })).await;
        }
        Ok(())
    }

    async fn interrupt_turn(&mut self) -> Result<(), CallerError> {
        self.rpc_call(json!({ "type": "abort" })).await?;
        Ok(())
    }

    async fn steer_turn(&mut self, text: &str) -> Result<(), CallerError> {
        self.rpc_call(json!({ "type": "steer", "message": text }))
            .await?;
        Ok(())
    }

    fn fork_handling(&self) -> ForkHandling {
        ForkHandling::RespawnResume {
            thread_id: self.current_session_id(),
        }
    }

    async fn thread_action(&mut self, op: &str, params: &Value) -> Result<String, CallerError> {
        match op {
            "compact" => {
                let mut request = json!({ "type": "compact" });
                if let Some(instruction) = params
                    .get("instruction")
                    .or_else(|| params.get("customInstructions"))
                    .and_then(Value::as_str)
                {
                    request.as_object_mut().expect("object").insert(
                        "customInstructions".into(),
                        Value::String(instruction.to_string()),
                    );
                }
                self.rpc_call_with_timeout(request, COMPACTION_TIMEOUT)
                    .await?;
                Ok("Pi compacted the conversation in place".to_string())
            }
            "model" | "model-set" | "set-model" => self.set_model_live(params).await,
            "thinking" | "effort" | "reasoning-effort" | "reasoning_effort" => {
                self.set_thinking_live(params).await
            }
            "rename" => self.rename_session(params).await,
            other => Err(Self::external(format!(
                "thread action /{other} not supported by Pi (supported: compact, fork, side, rename, model, thinking)"
            ))),
        }
    }

    async fn shutdown(&mut self) -> Result<(), CallerError> {
        if let Some(writer) = self.writer.as_ref() {
            let _ = Self::write_value(writer, &json!({ "type": "abort" })).await;
        }
        self.writer = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        if let Some(pid) = self.child_pid.take() {
            super::unregister_child_process(pid);
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        fail_pending_rpc(&self.pending_rpc);
        self.pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.extension_dir = None;
        Ok(())
    }
}

async fn terminate_spawned_child(child_pid: Option<u32>, child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
    if let Some(pid) = child_pid {
        super::unregister_child_process(pid);
    }
}

struct PiReader {
    state: Arc<StdMutex<PiSessionState>>,
    activity: Arc<StdMutex<ActivityMachine>>,
    reasoning: String,
    tool_outputs: HashMap<String, String>,
    open_tools: HashSet<String>,
}

impl PiReader {
    fn new(state: Arc<StdMutex<PiSessionState>>, activity: Arc<StdMutex<ActivityMachine>>) -> Self {
        Self {
            state,
            activity,
            reasoning: String::new(),
            tool_outputs: HashMap::new(),
            open_tools: HashSet::new(),
        }
    }

    fn process(&mut self, value: &Value) -> ReaderOutcome {
        let mut out = ReaderOutcome::default();
        match value.get("type").and_then(Value::as_str) {
            Some("agent_start") | Some("turn_start") => {
                out.activity.push(ActivityObservation::StreamByte);
            }
            Some("message_start") => {
                out.activity.push(ActivityObservation::StreamByte);
                if let Some(message) = value.get("message") {
                    if message.get("role").and_then(Value::as_str) == Some("user") {
                        let text = message_text(message);
                        if !text.is_empty() {
                            out.events.push(AgentEvent::UserMessage { text });
                        }
                    }
                }
            }
            Some("message_update") => {
                let event = value.get("assistantMessageEvent").unwrap_or(&Value::Null);
                match event.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = event.get("delta").and_then(Value::as_str) {
                            if !text.is_empty() {
                                out.events.push(AgentEvent::MessageDelta {
                                    text: text.to_string(),
                                });
                                out.activity.push(ActivityObservation::ResponseDelta);
                            }
                        }
                    }
                    Some("thinking_start") => {
                        out.activity.push(ActivityObservation::ReasoningStarted {
                            delta_heartbeat: true,
                        })
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = event.get("delta").and_then(Value::as_str) {
                            self.reasoning.push_str(text);
                            out.activity.push(ActivityObservation::ReasoningDelta);
                        }
                    }
                    Some("thinking_end") => self.flush_reasoning(&mut out.events),
                    Some("toolcall_start") | Some("toolcall_delta") | Some("toolcall_end") => {
                        out.activity.push(ActivityObservation::ResponseDelta)
                    }
                    Some("error") => {
                        out.events.push(AgentEvent::BackendError {
                            message: event
                                .get("error")
                                .and_then(Value::as_str)
                                .or_else(|| {
                                    event
                                        .get("error")
                                        .and_then(|error| error.get("errorMessage"))
                                        .and_then(Value::as_str)
                                })
                                .unwrap_or("Pi model stream failed")
                                .to_string(),
                            code: Some("pi_model_stream".to_string()),
                            details: event
                                .get("reason")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            will_retry: false,
                            likely_generation_starvation: false,
                            recovery_hint: None,
                        });
                    }
                    _ => {}
                }
            }
            Some("message_end") => {
                self.flush_reasoning(&mut out.events);
                if let Some(message) = value.get("message") {
                    self.capture_usage(message, &mut out.events);
                    if message.get("role").and_then(Value::as_str) == Some("assistant")
                        && matches!(
                            message.get("stopReason").and_then(Value::as_str),
                            Some("error")
                        )
                    {
                        out.events.push(AgentEvent::BackendError {
                            message: message
                                .get("errorMessage")
                                .and_then(Value::as_str)
                                .unwrap_or("Pi assistant response failed")
                                .to_string(),
                            code: Some("pi_assistant_error".to_string()),
                            details: None,
                            will_retry: false,
                            likely_generation_starvation: false,
                            recovery_hint: None,
                        });
                    }
                }
            }
            Some("tool_execution_start") => {
                if let (Some(id), Some(name)) = (
                    value.get("toolCallId").and_then(Value::as_str),
                    value.get("toolName").and_then(Value::as_str),
                ) {
                    self.open_tools.insert(id.to_string());
                    let args = value.get("args").unwrap_or(&Value::Null);
                    out.events.push(AgentEvent::ToolStarted {
                        item_id: id.to_string(),
                        tool_name: name.to_string(),
                        preview: tool_preview(name, args),
                        message_uuid: None,
                    });
                    if matches!(name, "write" | "edit") {
                        if let Some(path) = args.get("path").and_then(Value::as_str) {
                            out.events.push(AgentEvent::FileActivity {
                                paths: vec![path.to_string()],
                            });
                        }
                    }
                    out.activity.push(ActivityObservation::ToolsRunning);
                }
            }
            Some("tool_execution_update") => {
                if let Some(id) = value.get("toolCallId").and_then(Value::as_str) {
                    let current = result_text(value.get("partialResult"));
                    let previous = self.tool_outputs.entry(id.to_string()).or_default();
                    let delta = if current.starts_with(previous.as_str()) {
                        current[previous.len()..].to_string()
                    } else {
                        current.clone()
                    };
                    *previous = current;
                    if !delta.is_empty() {
                        out.events.push(AgentEvent::ToolOutputDelta {
                            item_id: id.to_string(),
                            text: delta,
                            message_uuid: None,
                        });
                    }
                    out.activity.push(ActivityObservation::ToolsRunning);
                }
            }
            Some("tool_execution_end") => {
                if let Some(id) = value.get("toolCallId").and_then(Value::as_str) {
                    let final_text = result_text(value.get("result"));
                    let previous = self.tool_outputs.remove(id).unwrap_or_default();
                    let delta = if final_text.starts_with(&previous) {
                        final_text[previous.len()..].to_string()
                    } else if final_text != previous {
                        final_text.clone()
                    } else {
                        String::new()
                    };
                    if !delta.is_empty() {
                        out.events.push(AgentEvent::ToolOutputDelta {
                            item_id: id.to_string(),
                            text: delta,
                            message_uuid: None,
                        });
                    }
                    self.open_tools.remove(id);
                    let status = if value.get("isError").and_then(Value::as_bool) == Some(true) {
                        ToolCompletionStatus::Failed {
                            message: if final_text.is_empty() {
                                "Pi tool failed".to_string()
                            } else {
                                final_text
                            },
                        }
                    } else {
                        ToolCompletionStatus::Success
                    };
                    out.events.push(AgentEvent::ToolCompleted {
                        item_id: id.to_string(),
                        status,
                        message_uuid: None,
                    });
                    out.activity.push(ActivityObservation::SegmentSettled);
                }
            }
            Some("agent_settled") => {
                self.flush_reasoning(&mut out.events);
                out.activity.push(ActivityObservation::TurnSettled);
                out.events.push(AgentEvent::TurnCompleted { message: None });
            }
            Some("thinking_level_changed") => {
                if let Some(level) = value.get("level").and_then(Value::as_str) {
                    self.state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .thinking = Some(level.to_string());
                    let state = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    out.events.push(AgentEvent::ConfigFacts {
                        facts: crate::types::SessionConfigVitals {
                            model: state.model.clone(),
                            effort: state.thinking.clone(),
                            permission_mode: Some(PI_PERMISSION_MODE.to_string()),
                            permission_kind: Some(
                                intendant_core::vitals::PERMISSION_KIND_ASK.to_string(),
                            ),
                            permission_echoed: true,
                            ..Default::default()
                        },
                    });
                }
            }
            Some("auto_retry_start") | Some("summarization_retry_attempt_start") => {
                out.events.push(AgentEvent::Log {
                    level: "warn".to_string(),
                    message: "Pi is retrying a transient model or summarization failure"
                        .to_string(),
                });
                out.activity.push(ActivityObservation::SegmentSettled);
            }
            Some("extension_error") => out.events.push(AgentEvent::BackendError {
                message: "Pi supervision extension failed; the affected tool was blocked"
                    .to_string(),
                code: Some("pi_extension_error".to_string()),
                details: value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                will_retry: false,
                likely_generation_starvation: false,
                recovery_hint: Some(
                    "Inspect the Pi compatibility report and Intendant activity log".to_string(),
                ),
            }),
            Some("extension_ui_request") => self.extension_ui(value, &mut out),
            Some("response") => {}
            _ => {}
        }
        out
    }

    fn flush_reasoning(&mut self, events: &mut Vec<AgentEvent>) {
        let text = std::mem::take(&mut self.reasoning);
        if !text.trim().is_empty() {
            events.push(AgentEvent::Reasoning { text });
        }
    }

    fn capture_usage(&self, message: &Value, events: &mut Vec<AgentEvent>) {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let usage = message.get("usage").unwrap_or(&Value::Null);
        let input = u64_field(usage, "input");
        let output = u64_field(usage, "output");
        let cache_read = u64_field(usage, "cacheRead");
        let cache_write = u64_field(usage, "cacheWrite");
        let total = u64_field(usage, "totalTokens").max(input + output + cache_read + cache_write);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(provider) = message.get("provider").and_then(Value::as_str) {
            state.provider = Some(provider.to_string());
        }
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            state.model = Some(model.to_string());
        }
        let context_tokens = input + cache_read + cache_write;
        let context_window = state.context_window;
        let usage_pct = if context_window == 0 {
            0.0
        } else {
            (context_tokens as f64 / context_window as f64 * 100.0).min(100.0)
        };
        events.push(AgentEvent::Usage {
            usage: AgentUsageSnapshot {
                provider: state.provider.clone().unwrap_or_else(|| "pi".to_string()),
                model: state.model.clone().unwrap_or_default(),
                tokens_used: total,
                context_window,
                hard_context_window: (context_window > 0).then_some(context_window),
                usage_pct,
                prompt_tokens: context_tokens,
                completion_tokens: output,
                cached_tokens: cache_read,
                cache_creation_tokens: cache_write,
                last_cache_read_tokens: cache_read,
                last_cache_creation_tokens: cache_write,
                last_uncached_input_tokens: input,
                cache_ttl_seconds: None,
                limits: Vec::new(),
            },
        });
    }

    fn extension_ui(&mut self, value: &Value, out: &mut ReaderOutcome) {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            out.events
                .push(protocol_error("Pi extension UI request omitted id"));
            return;
        };
        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
        let title = value.get("title").and_then(Value::as_str).unwrap_or("");
        if method == "select" {
            let options_are_exact =
                value
                    .get("options")
                    .and_then(Value::as_array)
                    .is_some_and(|options| {
                        options.len() == 3
                            && options[0].as_str() == Some(APPROVE_ONCE)
                            && options[1].as_str() == Some(APPROVE_SESSION)
                            && options[2].as_str() == Some(DENY)
                    });
            if let Some(raw) = title.strip_prefix(APPROVAL_MARKER) {
                match serde_json::from_str::<ApprovalEnvelope>(raw) {
                    Ok(envelope)
                        if options_are_exact && !envelope.tool_call_id.trim().is_empty() =>
                    {
                        let category = match envelope.category.as_str() {
                            "file_change" => ApprovalCategory::FileChange,
                            "permission_grant" => ApprovalCategory::PermissionGrant,
                            _ => ApprovalCategory::CommandExecution,
                        };
                        out.approval_id = Some(id.to_string());
                        if category == ApprovalCategory::FileChange {
                            out.events.push(AgentEvent::FileApprovalRequest {
                                request_id: id.to_string(),
                                path: envelope.path.unwrap_or_else(|| envelope.tool_name.clone()),
                                diff: bounded(&envelope.preview, MAX_PREVIEW_CHARS),
                            });
                        } else {
                            out.events.push(AgentEvent::ApprovalRequest {
                                request_id: id.to_string(),
                                command: bounded(&envelope.preview, MAX_PREVIEW_CHARS),
                                category,
                            });
                        }
                        return;
                    }
                    _ => {}
                }
            }
            out.events.push(protocol_error(
                "Pi emitted an unrecognized extension UI selection; it was cancelled",
            ));
            out.cancel_ui_id = Some(id.to_string());
        } else if matches!(method, "confirm" | "input" | "editor") {
            out.events.push(protocol_error(
                "Pi emitted an unrecognized extension dialog; it was cancelled",
            ));
            out.cancel_ui_id = Some(id.to_string());
        } else if !matches!(
            method,
            "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text"
        ) {
            // The documented methods above are fire-and-forget. Treat any
            // future method as potentially blocking: cancelling it is safer
            // than leaving Pi wedged on an input promise we do not understand.
            out.events.push(protocol_error(
                "Pi emitted an unknown extension UI request; it was cancelled",
            ));
            out.cancel_ui_id = Some(id.to_string());
        }
    }

    fn close_open_tools(&mut self, tx: &mpsc::UnboundedSender<AgentEvent>) {
        for id in self.open_tools.drain() {
            let _ = tx.send(AgentEvent::ToolCompleted {
                item_id: id,
                status: ToolCompletionStatus::Cancelled,
                message_uuid: None,
            });
        }
    }
}

#[derive(Default)]
struct ReaderOutcome {
    events: Vec<AgentEvent>,
    activity: Vec<ActivityObservation>,
    approval_id: Option<String>,
    cancel_ui_id: Option<String>,
}

async fn process_reader_value(
    reader: &mut PiReader,
    value: Value,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    writer: &SharedWriter,
    pending_approvals: &Arc<StdMutex<HashSet<String>>>,
    pending_rpc: &PendingRpc,
) {
    if value.get("type").and_then(Value::as_str) == Some("response") {
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            if let Some(tx) = pending_rpc
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(id)
            {
                let _ = tx.send(value);
                return;
            }
        }
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            let _ = event_tx.send(AgentEvent::BackendError {
                message: value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Pi RPC request failed")
                    .to_string(),
                code: Some("pi_rpc_error".to_string()),
                details: value
                    .get("command")
                    .and_then(Value::as_str)
                    .map(|command| format!("command: {command}")),
                will_retry: false,
                likely_generation_starvation: false,
                recovery_hint: None,
            });
        }
        return;
    }
    let outcome = reader.process(&value);
    if let Some(id) = outcome.approval_id {
        pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id);
    }
    if let Some(id) = outcome.cancel_ui_id {
        let _ = PiAgent::write_value(
            writer,
            &json!({ "type": "extension_ui_response", "id": id, "cancelled": true }),
        )
        .await;
    }
    for observation in outcome.activity {
        observe_activity(&reader.activity, event_tx, observation);
    }
    for event in outcome.events {
        let _ = event_tx.send(event);
    }
}

fn observe_activity(
    activity: &Arc<StdMutex<ActivityMachine>>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    observation: ActivityObservation,
) {
    let update = activity
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .observe(observation, now_secs());
    if let Some(activity) = update {
        let _ = tx.send(AgentEvent::ActivityUpdate { activity });
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn pi_session_id(candidate: Option<&str>) -> String {
    let candidate = candidate.map(str::trim).unwrap_or_default();
    let valid = !candidate.is_empty()
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && candidate
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && candidate
            .as_bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric());
    if valid {
        candidate.to_string()
    } else {
        format!("intendant-{}", uuid::Uuid::new_v4().simple())
    }
}

fn build_pi_args(
    extension_path: &Path,
    session_id: &str,
    resume_session: Option<&str>,
    fork_resume: bool,
    launch: &PiLaunchConfig,
) -> Result<Vec<String>, CallerError> {
    let mut args = vec![
        "--mode".to_string(),
        "rpc".to_string(),
        "--no-extensions".to_string(),
        "--no-approve".to_string(),
        "--extension".to_string(),
        extension_path.to_string_lossy().to_string(),
        "--append-system-prompt".to_string(),
        intendant_system_prompt(),
    ];
    let resume = resume_session
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if fork_resume {
        let parent =
            resume.ok_or_else(|| PiAgent::external("Pi fork requires a source session id"))?;
        // Pi deliberately permits --fork + --session-id: the former names
        // the source transcript while the latter assigns the new child id.
        args.extend(["--fork".to_string(), parent.to_string()]);
        args.extend(["--session-id".to_string(), session_id.to_string()]);
    } else if let Some(resume) = resume {
        args.extend(["--session".to_string(), resume.to_string()]);
    } else {
        args.extend(["--session-id".to_string(), session_id.to_string()]);
    }
    if let Some(model) = launch.model.as_deref() {
        args.extend(["--model".to_string(), model.to_string()]);
    }
    if let Some(thinking) = launch.thinking.as_deref() {
        args.extend(["--thinking".to_string(), thinking.to_string()]);
    }
    match launch.allowed_tools.as_ref() {
        Some(tools) if tools.is_empty() => args.push("--no-tools".to_string()),
        Some(tools) => {
            args.push("--tools".to_string());
            args.push(tools.join(","));
        }
        None => {}
    }
    Ok(args)
}

fn intendant_system_prompt() -> String {
    "You are running as a Pi cognitive engine supervised by Intendant. Intendant owns approvals, session lifecycle, and platform effects. For Intendant capabilities that Pi does not expose natively (including computer use, shared displays, peer machines, agenda, and memory), inspect the private bootstrap with `\"$INTENDANT\" ctl --help` and invoke only the scoped commands needed for the task. Do not claim that Pi has built-in MCP or Intendant tools; use the bootstrap command when needed.".to_string()
}

fn update_session_state(
    state: &Arc<StdMutex<PiSessionState>>,
    value: &Value,
) -> Result<(), CallerError> {
    let session_id = value
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PiAgent::external("Pi get_state omitted sessionId"))?;
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.session_id = Some(session_id.to_string());
    state.thinking = value
        .get("thinkingLevel")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(model) = value.get("model") {
        update_model_state_locked(&mut state, model);
    }
    Ok(())
}

fn update_model_state(state: &Arc<StdMutex<PiSessionState>>, model: &Value) {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    update_model_state_locked(&mut state, model);
}

fn update_model_state_locked(state: &mut PiSessionState, model: &Value) {
    if let Some(provider) = model.get("provider").and_then(Value::as_str) {
        state.provider = Some(provider.to_string());
    }
    if let Some(id) = model.get("id").and_then(Value::as_str) {
        state.model = Some(id.to_string());
    }
    if let Some(window) = model.get("contextWindow").and_then(Value::as_u64) {
        state.context_window = window;
    }
}

fn record_protocol_findings(
    watch: Option<&super::protocol_watch::ProtocolWatchHandle>,
    value: &Value,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let Some(watch) = watch else {
        return;
    };
    for message in watch.observe_all(super::protocol_watch::pi_findings(value)) {
        let _ = tx.send(AgentEvent::Log {
            level: "warn".to_string(),
            message,
        });
    }
}

fn fail_pending_rpc(pending: &PendingRpc) {
    pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

pub(crate) fn result_text(result: Option<&Value>) -> String {
    result
        .and_then(|result| result.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| {
            (part.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| part.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("")
}

pub(crate) fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                (part.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| part.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

pub(crate) fn tool_preview(name: &str, args: &Value) -> String {
    let preview = if name == "bash" {
        args.get("command")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        args.get("path").and_then(Value::as_str).map(str::to_string)
    }
    .unwrap_or_else(|| serde_json::to_string(args).unwrap_or_else(|_| "<tool input>".to_string()));
    bounded(&preview, MAX_PREVIEW_CHARS)
}

fn bounded(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let mut output: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        output.push('…');
    }
    output
}

fn u64_field(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or_default()
}

fn protocol_error(message: &str) -> AgentEvent {
    AgentEvent::BackendError {
        message: message.to_string(),
        code: Some("pi_supervision_protocol".to_string()),
        details: None,
        will_retry: false,
        likely_generation_starvation: false,
        recovery_hint: Some("Refuse the tool and inspect Pi compatibility status".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader() -> PiReader {
        PiReader::new(
            Arc::new(StdMutex::new(PiSessionState {
                session_id: Some("session-1".to_string()),
                provider: Some("openai-codex".to_string()),
                model: Some("gpt-5.6".to_string()),
                thinking: Some("high".to_string()),
                context_window: 200_000,
            })),
            Arc::new(StdMutex::new(ActivityMachine::default())),
        )
    }

    #[test]
    fn approval_extension_gates_mutating_and_unknown_tools_fail_closed() {
        assert!(APPROVAL_EXTENSION.contains("READ_ONLY.has(event.toolName)"));
        assert!(APPROVAL_EXTENSION.contains("ctx.ui.select"));
        assert!(APPROVAL_EXTENSION.contains("block: true"));
        assert!(APPROVAL_EXTENSION.contains(APPROVAL_MARKER));
        assert!(APPROVAL_EXTENSION.contains("targetsAgentHome"));
        assert!(APPROVAL_EXTENSION.contains("fileURLToPath(requested)"));
        assert!(APPROVAL_EXTENSION.contains("requested === \"~\""));
        assert!(APPROVAL_EXTENSION.contains("requested.startsWith(\"@\")"));
        assert!(APPROVAL_EXTENSION.contains("realpath(lexical)"));
        assert!(APPROVAL_EXTENSION.contains("protectedRead ? \"permission_grant\""));
    }

    #[test]
    fn pi_session_ids_follow_upstreams_exact_validation_rule() {
        assert_eq!(pi_session_id(Some("abc-DEF_1.2")), "abc-DEF_1.2");
        assert!(pi_session_id(Some("/bad/session")).starts_with("intendant-"));
        assert!(pi_session_id(Some("-bad")).starts_with("intendant-"));
        assert!(pi_session_id(Some("bad-")).starts_with("intendant-"));
    }

    #[test]
    fn pi_argv_is_deterministic_for_fresh_resume_and_fork() {
        let launch = PiLaunchConfig {
            model: Some("openai-codex/gpt-5.6-sol".to_string()),
            thinking: Some("high".to_string()),
            allowed_tools: Some(vec!["read".to_string(), "bash".to_string()]),
        };
        let fresh = build_pi_args(
            Path::new("/private/intendant-supervision.ts"),
            "fresh-id",
            None,
            false,
            &launch,
        )
        .unwrap();
        assert_eq!(
            fresh[0..6],
            [
                "--mode",
                "rpc",
                "--no-extensions",
                "--no-approve",
                "--extension",
                "/private/intendant-supervision.ts"
            ]
        );
        assert!(fresh
            .windows(2)
            .any(|pair| pair == ["--session-id", "fresh-id"]));
        assert!(fresh
            .windows(2)
            .any(|pair| pair == ["--model", "openai-codex/gpt-5.6-sol"]));
        assert!(fresh.windows(2).any(|pair| pair == ["--thinking", "high"]));
        assert!(fresh
            .windows(2)
            .any(|pair| pair == ["--tools", "read,bash"]));

        let resumed = build_pi_args(
            Path::new("extension.ts"),
            "ignored-child-id",
            Some("parent-id"),
            false,
            &PiLaunchConfig {
                model: None,
                thinking: None,
                allowed_tools: None,
            },
        )
        .unwrap();
        assert!(resumed
            .windows(2)
            .any(|pair| pair == ["--session", "parent-id"]));
        assert!(!resumed.iter().any(|arg| arg == "--session-id"));

        let forked = build_pi_args(
            Path::new("extension.ts"),
            "child-id",
            Some("parent-id"),
            true,
            &PiLaunchConfig {
                model: None,
                thinking: None,
                allowed_tools: Some(Vec::new()),
            },
        )
        .unwrap();
        assert!(forked
            .windows(2)
            .any(|pair| pair == ["--fork", "parent-id"]));
        assert!(forked
            .windows(2)
            .any(|pair| pair == ["--session-id", "child-id"]));
        assert!(forked.iter().any(|arg| arg == "--no-tools"));
    }

    #[test]
    fn pi_fork_requires_a_source_session() {
        let error = build_pi_args(
            Path::new("extension.ts"),
            "child-id",
            None,
            true,
            &PiLaunchConfig {
                model: None,
                thinking: None,
                allowed_tools: None,
            },
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("Pi fork requires a source session id"));
    }

    #[test]
    fn reader_streams_text_reasoning_tools_and_usage() {
        let mut reader = reader();
        let text = reader.process(&json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "text_delta", "delta": "hello"}
        }));
        assert!(matches!(
            text.events.as_slice(),
            [AgentEvent::MessageDelta { text }] if text == "hello"
        ));
        reader.process(&json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "thinking_delta", "delta": "consider"}
        }));
        let thought = reader.process(&json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "thinking_end"}
        }));
        assert!(matches!(
            thought.events.as_slice(),
            [AgentEvent::Reasoning { text }] if text == "consider"
        ));

        let started = reader.process(&json!({
            "type": "tool_execution_start",
            "toolCallId": "call-1",
            "toolName": "edit",
            "args": {"path": "src/main.rs", "oldText": "a", "newText": "b"}
        }));
        assert!(started.events.iter().any(|event| matches!(
            event,
            AgentEvent::FileActivity { paths } if paths == &["src/main.rs".to_string()]
        )));
        let ended = reader.process(&json!({
            "type": "tool_execution_end",
            "toolCallId": "call-1",
            "toolName": "edit",
            "result": {"content": [{"type": "text", "text": "done"}]},
            "isError": false
        }));
        assert!(ended.events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCompleted {
                status: ToolCompletionStatus::Success,
                ..
            }
        )));

        let usage = reader.process(&json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "provider": "openai-codex",
                "model": "gpt-5.6",
                "usage": {"input": 1000, "output": 100, "cacheRead": 500, "cacheWrite": 0, "totalTokens": 1600},
                "stopReason": "stop"
            }
        }));
        assert!(usage.events.iter().any(|event| matches!(
            event,
            AgentEvent::Usage { usage }
                if usage.prompt_tokens == 1500 && usage.completion_tokens == 100
        )));
    }

    #[test]
    fn only_marked_select_becomes_an_intendant_approval() {
        let mut reader = reader();
        let title = format!(
            "{}{}",
            APPROVAL_MARKER,
            json!({
                "toolCallId": "call-2",
                "toolName": "bash",
                "category": "command_execution",
                "preview": "cargo test",
            })
        );
        let approval = reader.process(&json!({
            "type": "extension_ui_request",
            "id": "ui-1",
            "method": "select",
            "title": title,
            "options": [APPROVE_ONCE, APPROVE_SESSION, DENY]
        }));
        assert_eq!(approval.approval_id.as_deref(), Some("ui-1"));
        assert!(matches!(
            approval.events.as_slice(),
            [AgentEvent::ApprovalRequest { command, .. }] if command == "cargo test"
        ));

        let unknown = reader.process(&json!({
            "type": "extension_ui_request",
            "id": "ui-2",
            "method": "select",
            "title": "some other extension",
            "options": ["yes", "no"]
        }));
        assert_eq!(unknown.cancel_ui_id.as_deref(), Some("ui-2"));
        assert!(matches!(
            unknown.events.as_slice(),
            [AgentEvent::BackendError { .. }]
        ));

        let spoofed_options = reader.process(&json!({
            "type": "extension_ui_request",
            "id": "ui-3",
            "method": "select",
            "title": format!("{}{}", APPROVAL_MARKER, json!({
                "toolCallId": "call-3",
                "toolName": "bash",
                "category": "command_execution",
                "preview": "dangerous command"
            })),
            "options": [APPROVE_ONCE, "Attacker-controlled approval", DENY]
        }));
        assert!(spoofed_options.approval_id.is_none());
        assert_eq!(spoofed_options.cancel_ui_id.as_deref(), Some("ui-3"));
        assert!(matches!(
            spoofed_options.events.as_slice(),
            [AgentEvent::BackendError { .. }]
        ));

        let future = reader.process(&json!({
            "type": "extension_ui_request",
            "id": "ui-4",
            "method": "futureBlockingDialog"
        }));
        assert_eq!(future.cancel_ui_id.as_deref(), Some("ui-4"));
        assert!(matches!(
            future.events.as_slice(),
            [AgentEvent::BackendError { .. }]
        ));

        let notify = reader.process(&json!({
            "type": "extension_ui_request",
            "id": "ui-5",
            "method": "notify",
            "message": "hello"
        }));
        assert!(notify.cancel_ui_id.is_none());
        assert!(notify.events.is_empty());
    }

    #[test]
    fn accumulated_tool_updates_emit_only_new_suffixes() {
        let mut reader = reader();
        reader.process(&json!({
            "type": "tool_execution_start",
            "toolCallId": "call-3",
            "toolName": "bash",
            "args": {"command": "printf hi"}
        }));
        let first = reader.process(&json!({
            "type": "tool_execution_update",
            "toolCallId": "call-3",
            "partialResult": {"content": [{"type": "text", "text": "hel"}]}
        }));
        let second = reader.process(&json!({
            "type": "tool_execution_update",
            "toolCallId": "call-3",
            "partialResult": {"content": [{"type": "text", "text": "hello"}]}
        }));
        assert!(matches!(
            first.events.as_slice(),
            [AgentEvent::ToolOutputDelta { text, .. }] if text == "hel"
        ));
        assert!(matches!(
            second.events.as_slice(),
            [AgentEvent::ToolOutputDelta { text, .. }] if text == "lo"
        ));
    }
}

//! The settings surface: payload (de)serialization + normalization,
//! runtime overrides, POST application + control-msg dispatch, API-key
//! set/status, and the thin per-route stream handlers.

use super::*;

/// Settings payload for GET/POST /api/settings.
/// Flattened view of intendant.toml sections relevant to the web dashboard.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettingsPayload {
    // Computer Use
    pub cu_provider: Option<String>,
    pub cu_model: Option<String>,
    pub cu_backend: String,
    /// Read-only: `[experimental] cu_first_routing` from intendant.toml.
    /// The dashboard shows the CU provider/model rows only when the
    /// vaulted routing is enabled; the flag itself is file-only.
    #[serde(default)]
    pub cu_first_routing: bool,
    // Presence
    pub presence_enabled: bool,
    pub presence_provider: Option<String>,
    pub presence_model: Option<String>,
    pub presence_live_provider: Option<String>,
    pub presence_live_model: Option<String>,
    // Transcription
    pub transcription_enabled: bool,
    pub transcription_provider: String,
    pub transcription_model: String,
    pub transcription_endpoint: Option<String>,
    pub transcription_language: Option<String>,
    // Recording
    pub recording_enabled: bool,
    pub recording_framerate: u32,
    pub recording_quality: String,
    // Live Audio
    pub live_audio_enabled: bool,
    pub live_audio_timeout_secs: u64,
    // External agent default (persisted to `[agent] default_backend`).
    // Values: "codex" | "claude-code" | "gemini" | None (internal agent).
    #[serde(default)]
    pub external_agent: Option<String>,
    // Codex runtime config (persisted to `[agent.codex]`). Mirrored here so
    // the Activity → Control sub-tab can load in one fetch.
    #[serde(default)]
    pub codex_command: Option<String>,
    /// Managed-capable (Intendant-aware fork) codex binary; managed
    /// sessions spawn it instead of `codex_command`. Empty string clears.
    #[serde(default)]
    pub codex_managed_command: Option<String>,
    #[serde(default)]
    pub codex_sandbox: Option<String>,
    #[serde(default)]
    pub codex_approval_policy: Option<String>,
    #[serde(default)]
    pub codex_model: Option<String>,
    #[serde(default)]
    pub codex_reasoning_effort: Option<String>,
    // Empty / omitted = inherit Codex config; "standard" sends an explicit
    // normal/clear override for Intendant-managed Codex sessions.
    #[serde(default)]
    pub codex_service_tier: Option<String>,
    #[serde(default)]
    pub codex_web_search: bool,
    #[serde(default)]
    pub codex_network_access: bool,
    #[serde(default)]
    pub codex_writable_roots: Vec<String>,
    #[serde(default, alias = "codex_context_recovery")]
    pub codex_managed_context: Option<String>,
    #[serde(default)]
    pub codex_context_archive: Option<String>,
    // Other external-agent executable commands. The Settings pane does not
    // edit these today, but the New Session pane uses them as per-launch
    // command/path defaults.
    #[serde(default)]
    pub claude_command: Option<String>,
    // Claude Code runtime config (persisted to `[agent.claude_code]`).
    // Mirrors the Codex/Gemini fields for the Activity → Control sub-tab.
    #[serde(default)]
    pub claude_model: Option<String>,
    #[serde(default)]
    pub claude_permission_mode: Option<String>,
    #[serde(default)]
    pub claude_allowed_tools: Option<Vec<String>>,
    // Per-category approval rules (persisted to `[approval]`). Exposed here
    // for the dashboard's "Approval rules" controls to populate the selects.
    // Live edits flow through the `set_approval_rule` ControlMsg, not through
    // `apply_settings_payload`, so these are display/read-only in the payload.
    // Values: "auto" | "ask" | "deny".
    #[serde(default = "default_settings_approval_auto")]
    pub approval_file_read: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_file_write: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_file_delete: String,
    #[serde(default = "default_settings_approval_auto")]
    pub approval_command_exec: String,
    #[serde(default = "default_settings_approval_auto")]
    pub approval_network: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_destructive: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_display_control: String,
    #[serde(default = "default_settings_approval_auto")]
    pub approval_tool_call: String,
    // Env var overrides (read-only, shown in UI)
    #[serde(default)]
    pub env_overrides: std::collections::HashMap<String, String>,
}

pub(crate) fn default_settings_approval_auto() -> String {
    crate::autonomy::ApprovalRule::Auto.as_str().to_string()
}

pub(crate) fn default_settings_approval_ask() -> String {
    crate::autonomy::ApprovalRule::Ask.as_str().to_string()
}

pub(crate) fn normalize_settings_codex_command(input: Option<&str>) -> String {
    normalize_settings_agent_command(input, "codex")
}

pub(crate) fn normalize_settings_agent_command(input: Option<&str>, fallback: &str) -> String {
    let trimmed = input.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn settings_payload_from_config(config: &crate::project::ProjectConfig) -> SettingsPayload {
    let mut env_overrides = std::collections::HashMap::new();
    for (key, var) in [
        ("CU_PROVIDER", "CU_PROVIDER"),
        ("CU_MODEL", "CU_MODEL"),
        ("PRESENCE_PROVIDER", "PRESENCE_PROVIDER"),
        ("PRESENCE_MODEL", "PRESENCE_MODEL"),
        ("PROVIDER", "PROVIDER"),
        ("MODEL_NAME", "MODEL_NAME"),
    ] {
        if let Ok(val) = std::env::var(var) {
            env_overrides.insert(key.to_string(), val);
        }
    }
    SettingsPayload {
        cu_provider: config.computer_use.provider.clone(),
        cu_model: config.computer_use.model.clone(),
        cu_backend: config.computer_use.backend.clone(),
        cu_first_routing: config.experimental.cu_first_routing,
        presence_enabled: config.presence.enabled,
        presence_provider: config.presence.provider.clone(),
        presence_model: config.presence.model.clone(),
        presence_live_provider: config.presence.live_provider.clone(),
        presence_live_model: config.presence.live_model.clone(),
        transcription_enabled: config.transcription.enabled,
        transcription_provider: config.transcription.provider.clone(),
        transcription_model: config.transcription.model.clone(),
        transcription_endpoint: config.transcription.endpoint.clone(),
        transcription_language: config.transcription.language.clone(),
        recording_enabled: config.recording.enabled,
        recording_framerate: config.recording.framerate,
        recording_quality: config.recording.quality.clone(),
        live_audio_enabled: config.live_audio.enabled,
        live_audio_timeout_secs: config.live_audio.default_timeout_secs,
        external_agent: config.agent.default_backend.clone(),
        codex_command: Some(config.agent.codex.command.clone()),
        codex_managed_command: config.agent.codex.managed_command.clone(),
        codex_sandbox: Some(crate::project::normalize_sandbox_mode(
            &config.agent.codex.sandbox,
        )),
        codex_approval_policy: Some(crate::project::normalize_approval_policy(
            &config.agent.codex.approval_policy,
        )),
        codex_model: config.agent.codex.model.clone(),
        codex_reasoning_effort: crate::project::normalize_reasoning_effort(
            config.agent.codex.reasoning_effort.as_deref(),
        ),
        codex_service_tier: crate::project::normalize_codex_service_tier(
            config.agent.codex.service_tier.as_deref(),
        ),
        codex_web_search: config.agent.codex.web_search,
        codex_network_access: config.agent.codex.network_access,
        codex_writable_roots: config.agent.codex.writable_roots.clone(),
        codex_managed_context: Some(crate::project::normalize_codex_managed_context(
            &config.agent.codex.managed_context,
        )),
        codex_context_archive: Some(crate::project::normalize_codex_context_archive(
            &config.agent.codex.context_archive,
        )),
        claude_command: Some(config.agent.claude_code.command.clone()),
        claude_model: config.agent.claude_code.model.clone(),
        claude_permission_mode: Some(crate::project::normalize_claude_permission_mode(
            &config.agent.claude_code.permission_mode,
        )),
        claude_allowed_tools: Some(config.agent.claude_code.allowed_tools.clone()),
        approval_file_read: config.approval.file_read.as_str().to_string(),
        approval_file_write: config.approval.file_write.as_str().to_string(),
        approval_file_delete: config.approval.file_delete.as_str().to_string(),
        approval_command_exec: config.approval.command_exec.as_str().to_string(),
        approval_network: config.approval.network.as_str().to_string(),
        approval_destructive: config.approval.destructive.as_str().to_string(),
        approval_display_control: config.approval.display_control.as_str().to_string(),
        approval_tool_call: config.approval.tool_call.as_str().to_string(),
        env_overrides,
    }
}

pub(crate) async fn settings_payload_with_runtime_overrides(
    config: &crate::project::ProjectConfig,
    runtime: &RuntimeSettingsState,
) -> SettingsPayload {
    let mut payload = settings_payload_from_config(config);
    if let Some(presence_enabled) = runtime.presence_enabled {
        payload.presence_enabled = presence_enabled;
    }
    if let Some(shared_external_agent) = &runtime.external_agent {
        payload.external_agent = shared_external_agent
            .read()
            .await
            .as_ref()
            .map(|backend| backend.as_short_str().to_string());
    }
    payload
}

pub(crate) async fn settings_get_response_body(
    project_root: Option<&Path>,
    runtime_settings: &RuntimeSettingsState,
) -> String {
    match project_root {
        Some(root) => match crate::project::Project::from_root(root.to_path_buf()) {
            Ok(proj) => {
                let payload =
                    settings_payload_with_runtime_overrides(&proj.config, runtime_settings).await;
                serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        },
        None => serde_json::json!({"error": "No project root"}).to_string(),
    }
}

pub(crate) fn apply_settings_payload(config: &mut crate::project::ProjectConfig, payload: &SettingsPayload) {
    config.computer_use.provider = payload.cu_provider.clone();
    config.computer_use.model = payload.cu_model.clone();
    config.computer_use.backend = payload.cu_backend.clone();
    config.presence.enabled = payload.presence_enabled;
    config.presence.provider = payload.presence_provider.clone();
    config.presence.model = payload.presence_model.clone();
    config.presence.live_provider = payload.presence_live_provider.clone();
    config.presence.live_model = payload.presence_live_model.clone();
    config.transcription.enabled = payload.transcription_enabled;
    config.transcription.provider = payload.transcription_provider.clone();
    config.transcription.model = payload.transcription_model.clone();
    config.transcription.endpoint = payload.transcription_endpoint.clone();
    config.transcription.language = payload.transcription_language.clone();
    config.recording.enabled = payload.recording_enabled;
    config.recording.framerate = payload.recording_framerate;
    config.recording.quality = payload.recording_quality.clone();
    config.live_audio.enabled = payload.live_audio_enabled;
    config.live_audio.default_timeout_secs = payload.live_audio_timeout_secs;
    // Normalize empty strings to None so the TOML doesn't end up with
    // `default_backend = ""` — the loader treats "" as a valid override
    // and would try to resolve it to a backend.
    config.agent.default_backend =
        payload
            .external_agent
            .as_ref()
            .and_then(|s| if s.is_empty() { None } else { Some(s.clone()) });
    if payload.codex_command.is_some() {
        config.agent.codex.command =
            normalize_settings_codex_command(payload.codex_command.as_deref());
    }
    if payload.codex_managed_command.is_some() {
        // Empty clears the override (managed sessions fall back to
        // `command`); anything else is the fork binary path.
        config.agent.codex.managed_command = payload
            .codex_managed_command
            .as_deref()
            .map(str::trim)
            .filter(|cmd| !cmd.is_empty())
            .map(str::to_string);
    }
    if let Some(mode) = payload.codex_sandbox.as_deref() {
        config.agent.codex.sandbox = crate::project::normalize_sandbox_mode(mode);
    }
    if let Some(policy) = payload.codex_approval_policy.as_deref() {
        config.agent.codex.approval_policy = crate::project::normalize_approval_policy(policy);
    }
    if payload.codex_service_tier.is_some() {
        config.agent.codex.service_tier =
            crate::project::normalize_codex_service_tier(payload.codex_service_tier.as_deref());
    }
    if let Some(mode) = payload.codex_managed_context.as_deref() {
        config.agent.codex.managed_context = crate::project::normalize_codex_managed_context(mode);
    }
    if let Some(mode) = payload.codex_context_archive.as_deref() {
        config.agent.codex.context_archive = crate::project::normalize_codex_context_archive(mode);
    }
    if payload.claude_command.is_some() {
        config.agent.claude_code.command =
            normalize_settings_agent_command(payload.claude_command.as_deref(), "claude");
    }
    if payload.claude_model.is_some() {
        // Empty clears the override (claude picks its configured default).
        config.agent.claude_code.model = payload
            .claude_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
    }
    if let Some(mode) = payload.claude_permission_mode.as_deref() {
        config.agent.claude_code.permission_mode =
            crate::project::normalize_claude_permission_mode(mode);
    }
    if let Some(tools) = payload.claude_allowed_tools.as_ref() {
        config.agent.claude_code.allowed_tools = tools
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
    }
}

pub(crate) fn settings_post_result(
    body_text: &str,
    project_root: Option<&Path>,
    bus: &EventBus,
) -> (u16, String) {
    let Some(root) = project_root else {
        return (
            400,
            serde_json::json!({"error": "No project root"}).to_string(),
        );
    };
    let payload = match serde_json::from_str::<SettingsPayload>(body_text) {
        Ok(payload) => payload,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("Invalid settings: {}", e)}).to_string(),
            );
        }
    };
    let mut proj = match crate::project::Project::from_root(root.to_path_buf()) {
        Ok(proj) => proj,
        Err(e) => {
            return (500, serde_json::json!({"error": e.to_string()}).to_string());
        }
    };
    apply_settings_payload(&mut proj.config, &payload);
    match proj.save_config() {
        Ok(()) => {
            dispatch_codex_settings_control_msgs(bus, &payload);
            (200, serde_json::json!({"ok": true}).to_string())
        }
        Err(e) => (500, serde_json::json!({"error": e.to_string()}).to_string()),
    }
}

/// Mirror just-persisted `[agent.codex]` settings into the live control plane.
///
/// `apply_settings_payload` + `save_config` update the TOML, but session
/// launches read the live `CodexRuntimeConfig`, which OVERLAYS the TOML
/// (`project_with_runtime_config`). Without this, an API client that POSTs
/// `codex_managed_context: "managed"` sees /api/settings echo the new value
/// while sessions keep launching with the stale live value until a daemon
/// restart. Frontends stay display-only, so we don't write shared state here:
/// we emit the same `ControlMsg`s a dashboard would, and the control plane
/// (the single writer) updates shared state, broadcasts `CodexConfigChanged`,
/// and re-persists the normalized value. That second persist is intentional
/// and idempotent — both paths run the same normalizers, and the gateway's
/// own synchronous TOML write (kept above) is what makes an immediate
/// GET /api/settings read back the saved values.
///
/// Only fields actually present in the payload are dispatched, mirroring
/// `apply_settings_payload`'s conditional writes; only codex fields with a
/// live control-plane setter are covered.
pub(crate) fn dispatch_codex_settings_control_msgs(bus: &EventBus, payload: &SettingsPayload) {
    if payload.codex_command.is_some() {
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexCommand {
            command: payload.codex_command.clone(),
        }));
    }
    if payload.codex_managed_command.is_some() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexManagedCommand {
                command: payload.codex_managed_command.clone(),
            },
        ));
    }
    if let Some(mode) = payload.codex_sandbox.clone() {
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexSandbox {
            mode,
        }));
    }
    if let Some(policy) = payload.codex_approval_policy.clone() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexApprovalPolicy { policy },
        ));
    }
    if payload.codex_service_tier.is_some() {
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexServiceTier {
            service_tier: payload.codex_service_tier.clone(),
        }));
    }
    if let Some(mode) = payload.codex_managed_context.clone() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexManagedContext { mode },
        ));
    }
    if let Some(mode) = payload.codex_context_archive.clone() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexContextArchive { mode },
        ));
    }
}

/// Return JSON with boolean flags indicating which API keys are usable —
/// an active credential lease counts the same as a configured .env key.
/// Field names derive from [`crate::provider::PROVIDER_KEY_ENV_VARS`]
/// (`OPENAI_API_KEY` → `openai`), the single authoritative key list.
pub(crate) fn get_api_key_status_json() -> String {
    let mut map = serde_json::Map::new();
    for name in crate::provider::PROVIDER_KEY_ENV_VARS {
        let short = name.trim_end_matches("_API_KEY").to_ascii_lowercase();
        map.insert(
            short,
            serde_json::Value::Bool(crate::credential_leases::provider_api_key(name).is_some()),
        );
    }
    serde_json::Value::Object(map).to_string()
}

pub(crate) fn api_key_status_response_body() -> String {
    get_api_key_status_json()
}

/// Whether any provider credential is usable at all — the aggregate of
/// [`get_api_key_status_json`], safe to expose at presence level.
pub(crate) fn any_provider_credential_usable() -> bool {
    crate::provider::PROVIDER_KEY_ENV_VARS
        .iter()
        .any(|name| crate::credential_leases::provider_api_key(name).is_some())
}

pub(crate) fn project_root_response_body(project_root: Option<&Path>) -> String {
    serde_json::json!({
        "project_root": project_root.map(|root| root.to_string_lossy().to_string())
    })
    .to_string()
}

/// Availability of the external-agent backends (Codex, Claude Code):
/// the configured command, whether it resolves to an executable, and
/// when this daemon last ran a session with it. Deliberately independent
/// of provider fueling — external agents bring their own credentials, so
/// the dashboard pairs this with the `fueled` flag instead of letting the
/// first-run nudge claim an unfueled daemon can't do anything. `home`
/// arrives from the transport edge (last-run recency and local-login
/// probes read under it), so tests inject a tempdir instead of reading
/// the live account (the CLAUDE.md tests-are-hermetic convention).
pub(crate) fn external_agents_response_body(project_root: Option<&Path>, home: &Path) -> String {
    let agent_config = project_root
        .and_then(|root| crate::project::Project::from_root(root.to_path_buf()).ok())
        .map(|project| project.config.agent)
        .unwrap_or_default();
    serde_json::json!({
        "external_agents":
            crate::external_agent::backend_availability_json(&agent_config, home),
    })
    .to_string()
}

/// Payload for POST /api/api-keys.
#[derive(serde::Deserialize)]
pub(crate) struct SetApiKeysPayload {
    keys: std::collections::HashMap<String, String>,
}

/// The `.env` file POST /api/api-keys persists provider keys to
/// (`<config_dir>/intendant/.env`). Resolved here — at the transport
/// edges — so the persist core takes the path as a parameter and stays
/// hermetically testable (tests inject a tempdir path; the CLAUDE.md
/// tests-are-hermetic convention). `None` when the platform reports no
/// config directory.
pub(crate) fn api_keys_env_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("intendant").join(".env"))
}

/// POST /api/api-keys persist core: validate the payload against the
/// authoritative provider key list, persist to `env_path`, and set the
/// keys in the current process so future provider instantiations pick
/// them up without a restart. Failures report in the body — the
/// endpoint's historical contract answers 200 either way.
pub(crate) fn set_api_keys_result(env_path: Option<&Path>, body: &str) -> String {
    let payload: SetApiKeysPayload = match serde_json::from_str(body) {
        Ok(p) => p,
        Err(e) => {
            return serde_json::json!({"error": format!("Invalid payload: {}", e)}).to_string();
        }
    };

    // Only allow known key names (the authoritative provider key list).
    for key in payload.keys.keys() {
        if !crate::provider::PROVIDER_KEY_ENV_VARS.contains(&key.as_str()) {
            return serde_json::json!({"error": format!("Unknown key: {}", key)}).to_string();
        }
    }

    let Some(env_path) = env_path else {
        return serde_json::json!({"error": "Cannot determine config directory"}).to_string();
    };

    // Ensure the directory exists.
    if let Some(config_dir) = env_path.parent() {
        if let Err(e) = std::fs::create_dir_all(config_dir) {
            return serde_json::json!({"error": format!("Cannot create config dir: {}", e)})
                .to_string();
        }
    }

    // Read existing content (may not exist yet).
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();

    // Build updated content: replace existing lines, append new ones.
    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
    let mut written_keys = std::collections::HashSet::new();

    for line in &mut lines {
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq_pos) = trimmed.find('=') {
            let var_name = trimmed[..eq_pos].trim().to_string();
            if let Some(new_val) = payload.keys.get(&var_name) {
                *line = format!("{}={}", var_name, new_val);
                written_keys.insert(var_name);
            }
        }
    }

    // Append keys that weren't already in the file.
    for (key, val) in &payload.keys {
        if !written_keys.contains(key.as_str()) {
            lines.push(format!("{}={}", key, val));
        }
    }

    let new_content = lines.join("\n") + "\n";

    if let Err(e) = crate::file_watcher::atomic_write(&env_path, new_content.as_bytes()) {
        return serde_json::json!({"error": format!("Write failed: {}", e)}).to_string();
    }

    // Set env vars in the current process so future provider instantiations
    // pick them up without requiring a restart.
    for (key, val) in &payload.keys {
        std::env::set_var(key, val);
    }

    serde_json::json!({"ok": true}).to_string()
}

// ── Transport-neutral cores (transport-unification design §2.1, S5):
//    the settings/keys family. Each fn is the single response builder
//    both lanes render — the HTTP shims below hand them to
//    `write_api_response`; the tunnel twins frame them through the
//    dispatch adapters.

/// JSON under the bare wildcard-CORS tail (`Access-Control-Allow-Origin:
/// *` + `Connection: close`, NO `Cache-Control`) — the historical
/// framing of the api-keys POST and the diagnostics sink.
pub(crate) fn bare_wildcard_json_response(status: u16, body: String) -> ApiResponse {
    ApiResponse::Json {
        status,
        body: JsonBody::PreSerialized(body),
        headers: vec![
            ("Access-Control-Allow-Origin", "*".to_string()),
            ("Connection", "close".to_string()),
        ],
    }
}

/// GET /api/settings + the tunnel's `api_settings`: the flattened
/// settings payload — or the historical error body, still under 200.
pub(crate) async fn settings_get_api_response(
    project_root: Option<&Path>,
    runtime_settings: &RuntimeSettingsState,
) -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(settings_get_response_body(project_root, runtime_settings).await),
    )
}

/// POST /api/settings + the tunnel's `api_settings_save`.
pub(crate) fn settings_post_api_response(
    body_text: &str,
    project_root: Option<&Path>,
    bus: &EventBus,
) -> ApiResponse {
    let (status, body) = settings_post_result(body_text, project_root, bus);
    ApiResponse::json(status, JsonBody::PreSerialized(body))
}

/// POST /api/api-keys + the tunnel's `api_api_keys_save`: always 200 —
/// the historical lane reports failures in the body — under the bare
/// wildcard tail. `env_path` arrives from the transport edge
/// ([`api_keys_env_path`]).
pub(crate) fn api_keys_save_api_response(
    env_path: Option<&Path>,
    body_text: &str,
) -> ApiResponse {
    bare_wildcard_json_response(200, set_api_keys_result(env_path, body_text))
}

/// GET /api/api-key-status + the tunnel's `api_key_status`.
pub(crate) fn api_key_status_api_response() -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(api_key_status_response_body()),
    )
}

/// GET /api/project-root + the tunnel's `api_project_root`.
pub(crate) fn project_root_api_response(project_root: Option<&Path>) -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(project_root_response_body(project_root)),
    )
}

/// GET /api/external-agents + the tunnel's `api_external_agents`.
/// `home` arrives from the transport edge (hermeticity convention).
pub(crate) fn external_agents_api_response(
    project_root: Option<&Path>,
    home: &Path,
) -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(external_agents_response_body(project_root, home)),
    )
}

// ---------------------------------------------------------------------------
// MCP-over-HTTP (Streamable HTTP) types
// ---------------------------------------------------------------------------
//
// rmcp's Streamable HTTP transport expects:
//   - Requests (with `id`):   200 OK + application/json body
//   - Notifications (no `id`): 202 Accepted + empty body
//
// Returning 200+JSON for notifications causes rmcp to try deserializing the
// body as ServerJsonRpcMessage, which fails because there's no valid `id`.


pub(crate) async fn handle_project_root(
    stream: DemuxStream,
    project_root: Option<PathBuf>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = project_root_api_response(project_root.as_deref());
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_settings_post(
    stream: DemuxStream,
    body_text: String,
    bus: EventBus,
    project_root: Option<PathBuf>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = settings_post_api_response(&body_text, project_root.as_deref(), &bus);
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_settings_get(
    stream: DemuxStream,
    project_root: Option<PathBuf>,
    runtime_settings: RuntimeSettingsState,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = settings_get_api_response(project_root.as_deref(), &runtime_settings).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_api_keys_post(
    stream: DemuxStream,
    body_text: String,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // The transport edge resolves the ambient env path; the neutral core
    // below it is path-parameterized (hermeticity convention).
    let response = api_keys_save_api_response(api_keys_env_path().as_deref(), &body_text);
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_api_key_status(
    stream: DemuxStream,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    write_api_response(stream, api_key_status_api_response(), cors, fleet_origin).await;
}

pub(crate) async fn handle_external_agents(
    stream: DemuxStream,
    project_root: Option<PathBuf>,
    home: PathBuf,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = external_agents_api_response(project_root.as_deref(), &home);
    write_api_response(stream, response, cors, fleet_origin).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_payload_accepts_settings_tab_save_without_agent_runtime_fields() {
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex"
        })
        .to_string();

        let payload: SettingsPayload = serde_json::from_str(&body).unwrap();
        assert_eq!(payload.external_agent.as_deref(), Some("codex"));
        assert_eq!(payload.codex_sandbox, None);
        assert_eq!(payload.codex_approval_policy, None);
        assert_eq!(payload.codex_managed_context, None);

        let mut config = crate::project::ProjectConfig::default();
        config.agent.codex.command = "/opt/codex/bin/codex".to_string();
        config.agent.codex.sandbox = "danger-full-access".to_string();
        config.agent.codex.approval_policy = "never".to_string();
        config.agent.codex.managed_context = "managed".to_string();
        config.agent.codex.service_tier = Some("priority".to_string());
        apply_settings_payload(&mut config, &payload);

        assert_eq!(config.agent.default_backend.as_deref(), Some("codex"));
        assert_eq!(config.agent.codex.command, "/opt/codex/bin/codex");
        assert_eq!(config.agent.codex.sandbox, "danger-full-access");
        assert_eq!(config.agent.codex.approval_policy, "never");
        assert_eq!(config.agent.codex.managed_context, "managed");
        assert_eq!(config.agent.codex.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn settings_payload_round_trips_codex_command() {
        let mut config = crate::project::ProjectConfig::default();
        config.agent.codex.command = "/usr/local/bin/codex".to_string();
        config.agent.codex.managed_context = "managed".to_string();
        config.agent.codex.service_tier = Some("priority".to_string());
        config.agent.claude_code.command = "/usr/local/bin/claude".to_string();

        let payload = settings_payload_from_config(&config);
        assert_eq!(
            payload.codex_command.as_deref(),
            Some("/usr/local/bin/codex")
        );
        assert_eq!(payload.codex_sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(payload.codex_approval_policy.as_deref(), Some("on-request"));
        assert_eq!(payload.codex_managed_context.as_deref(), Some("managed"));
        assert_eq!(payload.codex_service_tier.as_deref(), Some("priority"));
        assert_eq!(
            payload.claude_command.as_deref(),
            Some("/usr/local/bin/claude")
        );

        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex",
            "codex_command": "  /opt/homebrew/bin/codex  ",
            "codex_sandbox": "danger-full-access",
            "codex_approval_policy": "never",
            "codex_service_tier": "normal",
            "codex_managed_context": "true",
            "claude_command": "  /opt/claude/bin/claude  "
        })
        .to_string();

        let payload: SettingsPayload = serde_json::from_str(&body).unwrap();
        apply_settings_payload(&mut config, &payload);

        assert_eq!(config.agent.codex.command, "/opt/homebrew/bin/codex");
        assert_eq!(config.agent.codex.sandbox, "danger-full-access");
        assert_eq!(config.agent.codex.approval_policy, "never");
        assert_eq!(config.agent.codex.service_tier.as_deref(), Some("standard"));
        assert_eq!(config.agent.codex.managed_context, "managed");
        assert_eq!(config.agent.claude_code.command, "/opt/claude/bin/claude");
    }

    #[test]
    fn settings_post_result_rejects_invalid_json_with_bad_request() {
        let (status, body) = settings_post_result(
            "{\"external_agent\":",
            Some(Path::new(".")),
            &EventBus::new(),
        );

        assert_eq!(status, 400);
        assert!(body.contains("Invalid settings"));
    }

    #[test]
    fn settings_post_result_rejects_missing_project_root_with_bad_request() {
        let (status, body) = settings_post_result("{}", None, &EventBus::new());

        assert_eq!(status, 400);
        assert!(body.contains("No project root"));
    }

    /// POST /api/settings must keep the LIVE codex runtime config coherent,
    /// not just the TOML: launches read the shared `CodexRuntimeConfig`,
    /// which overrides the file. The gateway does that by re-dispatching
    /// the codex fields as control-plane intents after a successful save.
    #[test]
    fn settings_post_dispatches_codex_control_msgs_for_live_state() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex",
            "codex_command": "/opt/codex/bin/codex",
            "codex_sandbox": "danger-full-access",
            "codex_approval_policy": "never",
            "codex_service_tier": "priority",
            "codex_managed_context": "managed",
            "codex_context_archive": "exact"
        })
        .to_string();

        let (status, _) = settings_post_result(&body, Some(dir.path()), &bus);
        assert_eq!(status, 200);

        let mut saw_command = false;
        let mut saw_sandbox = false;
        let mut saw_approval = false;
        let mut saw_service_tier = false;
        let mut saw_managed = false;
        let mut saw_archive = false;
        while let Ok(event) = rx.try_recv() {
            let AppEvent::ControlCommand(msg) = event else {
                continue;
            };
            match msg {
                ControlMsg::SetCodexCommand { command } => {
                    assert_eq!(command.as_deref(), Some("/opt/codex/bin/codex"));
                    saw_command = true;
                }
                ControlMsg::SetCodexSandbox { mode } => {
                    assert_eq!(mode, "danger-full-access");
                    saw_sandbox = true;
                }
                ControlMsg::SetCodexApprovalPolicy { policy } => {
                    assert_eq!(policy, "never");
                    saw_approval = true;
                }
                ControlMsg::SetCodexServiceTier { service_tier } => {
                    assert_eq!(service_tier.as_deref(), Some("priority"));
                    saw_service_tier = true;
                }
                ControlMsg::SetCodexManagedContext { mode } => {
                    assert_eq!(mode, "managed");
                    saw_managed = true;
                }
                ControlMsg::SetCodexContextArchive { mode } => {
                    assert_eq!(mode, "exact");
                    saw_archive = true;
                }
                _ => {}
            }
        }
        assert!(saw_command, "SetCodexCommand was not dispatched");
        assert!(saw_sandbox, "SetCodexSandbox was not dispatched");
        assert!(saw_approval, "SetCodexApprovalPolicy was not dispatched");
        assert!(saw_service_tier, "SetCodexServiceTier was not dispatched");
        assert!(saw_managed, "SetCodexManagedContext was not dispatched");
        assert!(saw_archive, "SetCodexContextArchive was not dispatched");

        // The synchronous TOML write still happened (read-after-write
        // consistency for an immediate GET /api/settings).
        let saved = std::fs::read_to_string(dir.path().join("intendant.toml")).unwrap();
        assert!(saved.contains("managed_context = \"managed\""));
    }

    // ── S5 golden transcripts: settings / keys family ──
    //
    // Byte-exact pins of the settings GET/POST, api-keys POST,
    // api-key-status, and project-root HTTP responses, captured before
    // the transport-neutral conversion (transport-unification design
    // §6 S5, risk R1) and kept as the conversion's proof. The expected
    // framing is hand-written below — never built through the response
    // helpers under conversion. Environment-dependent bodies (the
    // key-status booleans read process env / leases; the settings
    // payload mirrors env overrides) are computed through the body
    // builders the conversion does not touch and spliced into the
    // hand-written framing — the framing is the pin, and the fixtures
    // never write outside their tempdirs (the api-keys pins use the
    // pre-persist rejection paths, which return before any config-dir
    // resolution).

    /// Run one stream-consuming handler and collect every byte it wrote.
    async fn collect_settings_handler_response<Fut>(
        run: impl FnOnce(DemuxStream) -> Fut,
    ) -> Vec<u8>
    where
        Fut: std::future::Future<Output = ()>,
    {
        use tokio::io::AsyncReadExt;
        let (mut client, server) = tokio::io::duplex(1 << 20);
        run(Box::pin(server)).await;
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("collect handler response");
        response
    }

    /// The canonical JSON framing (`Cache-Control` + `Connection` tail):
    /// settings GET/POST, key-status, and project-root, spelled out
    /// literally.
    fn golden_settings_json_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// The bare wildcard-CORS JSON framing (`Access-Control-Allow-Origin:
    /// *` + `Connection` tail, NO `Cache-Control`): the api-keys POST
    /// shape, spelled out literally.
    fn golden_settings_bare_wildcard_json_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn golden_settings_transcript(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// The CORS posture dispatch hands the shim — read from the route
    /// table so a row-posture change fails these byte pins instead of
    /// silently changing the wire.
    fn settings_route_cors(method: &str, path: &str) -> crate::gateway_routes::CorsPosture {
        crate::gateway_routes::match_route(method, path)
            .expect("settings route declared")
            .0
            .cors
    }

    #[tokio::test]
    async fn golden_project_root_transcripts() {
        let cors = settings_route_cors("GET", "/api/project-root");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let root_json = serde_json::json!(root.to_string_lossy()).to_string();
        let response = collect_settings_handler_response(|stream| {
            handle_project_root(stream, Some(root.clone()), cors, None)
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_json_transcript(
                "200 OK",
                &format!(r#"{{"project_root":{root_json}}}"#)
            )
        );

        let response = collect_settings_handler_response(|stream| {
            handle_project_root(stream, None, cors, None)
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_json_transcript("200 OK", r#"{"project_root":null}"#)
        );
    }

    #[tokio::test]
    async fn golden_settings_get_no_root_transcript() {
        // Historical shape: the missing-project-root ERROR body still
        // answers 200 OK on the GET lane.
        let response = collect_settings_handler_response(|stream| {
            handle_settings_get(
                stream,
                None,
                RuntimeSettingsState::default(),
                settings_route_cors("GET", "/api/settings"),
                None,
            )
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_json_transcript("200 OK", r#"{"error":"No project root"}"#)
        );
    }

    #[tokio::test]
    async fn golden_settings_get_temp_root_transcript() {
        // A tempdir project root loads the default config; the payload
        // body mirrors process env (env_overrides), so it is computed
        // through the payload builder and spliced — the 200 framing is
        // the byte-exact pin.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let runtime_settings = RuntimeSettingsState::default();
        let body = settings_get_response_body(Some(&root), &runtime_settings).await;
        assert!(
            body.contains("cu_backend"),
            "default-config payload expected: {body}"
        );
        let response = collect_settings_handler_response(|stream| {
            handle_settings_get(
                stream,
                Some(root.clone()),
                runtime_settings.clone(),
                settings_route_cors("GET", "/api/settings"),
                None,
            )
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_json_transcript("200 OK", &body)
        );
    }

    #[tokio::test]
    async fn golden_settings_post_transcripts() {
        let cors = settings_route_cors("POST", "/api/settings");
        // Missing project root: 400 under the canonical tail.
        let response = collect_settings_handler_response(|stream| {
            handle_settings_post(stream, "{}".to_string(), EventBus::new(), None, cors, None)
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_json_transcript("400 Bad Request", r#"{"error":"No project root"}"#)
        );

        // Invalid payload: 400 with serde's wording for this exact input
        // (derived through the same parse, framing hand-written). The
        // parse rejects before the project root is ever read.
        let parse_dir = tempfile::tempdir().unwrap();
        let invalid = "{\"external_agent\":";
        let serde_error = serde_json::from_str::<SettingsPayload>(invalid).unwrap_err();
        let expected_body =
            serde_json::json!({"error": format!("Invalid settings: {}", serde_error)})
                .to_string();
        let response = collect_settings_handler_response(|stream| {
            handle_settings_post(
                stream,
                invalid.to_string(),
                EventBus::new(),
                Some(parse_dir.path().to_path_buf()),
                cors,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_json_transcript("400 Bad Request", &expected_body)
        );

        // Success on a tempdir root: 200 {"ok":true} and the TOML written.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": null
        })
        .to_string();
        let response = collect_settings_handler_response(|stream| {
            handle_settings_post(
                stream,
                body,
                EventBus::new(),
                Some(root.clone()),
                cors,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_json_transcript("200 OK", r#"{"ok":true}"#)
        );
        assert!(root.join("intendant.toml").exists());
    }

    #[tokio::test]
    async fn golden_api_keys_post_transcripts() {
        let cors = settings_route_cors("POST", "/api/api-keys");
        // Unknown key: rejected before any config-dir resolution — and
        // still 200 OK (the historical always-200 POST lane) under the
        // bare wildcard tail.
        let response = collect_settings_handler_response(|stream| {
            handle_api_keys_post(
                stream,
                r#"{"keys":{"NOT_A_KNOWN_KEY":"x"}}"#.to_string(),
                cors,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_bare_wildcard_json_transcript(
                "200 OK",
                r#"{"error":"Unknown key: NOT_A_KNOWN_KEY"}"#
            )
        );

        // Invalid payload: serde wording derived through the same parse
        // (match instead of unwrap_err — the payload type has no Debug).
        let invalid = "not json";
        let serde_error = match serde_json::from_str::<SetApiKeysPayload>(invalid) {
            Err(error) => error,
            Ok(_) => panic!("fixture input must not parse"),
        };
        let expected_body =
            serde_json::json!({"error": format!("Invalid payload: {}", serde_error)}).to_string();
        let response = collect_settings_handler_response(|stream| {
            handle_api_keys_post(stream, invalid.to_string(), cors, None)
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_bare_wildcard_json_transcript("200 OK", &expected_body)
        );
    }

    /// The persist core writes the injected env path — the hermetic
    /// success pin (an empty keys map exercises the full write path
    /// without touching process env; the transport edges resolve the
    /// real path via api_keys_env_path).
    #[test]
    fn set_api_keys_persists_to_injected_env_path() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("intendant").join(".env");
        let body = serde_json::json!({"keys": {}}).to_string();
        let result = set_api_keys_result(Some(&env_path), &body);
        assert_eq!(result, r#"{"ok":true}"#);
        assert_eq!(std::fs::read_to_string(&env_path).unwrap(), "\n");
    }

    /// No config directory: the persist core reports the historical
    /// error body (still a 200 lane).
    #[test]
    fn set_api_keys_without_config_dir_reports_error_body() {
        let body = serde_json::json!({"keys": {}}).to_string();
        assert_eq!(
            set_api_keys_result(None, &body),
            r#"{"error":"Cannot determine config directory"}"#
        );
    }

    #[tokio::test]
    async fn golden_external_agents_transcripts() {
        // S5 second slice (info/displays/diagnostics). The availability
        // body probes the configured commands on PATH, so it is
        // computed through the untouched builder over an injected temp
        // home (no live-account reads) and spliced — the canonical 200
        // framing is the byte-exact pin.
        let home = tempfile::tempdir().unwrap();
        let body = external_agents_response_body(None, home.path());
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            parsed["external_agents"].is_array(),
            "availability array expected: {body}"
        );
        let cors = settings_route_cors("GET", "/api/external-agents");
        let home_path = home.path().to_path_buf();
        let response = collect_settings_handler_response(|stream| {
            handle_external_agents(stream, None, home_path, cors, None)
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_json_transcript("200 OK", &body)
        );

        // A tempdir project root loads the default agent config — same
        // framing, same builder.
        let root = tempfile::tempdir().unwrap();
        let body =
            external_agents_response_body(Some(root.path()), home.path());
        let root_path = root.path().to_path_buf();
        let home_path = home.path().to_path_buf();
        let response = collect_settings_handler_response(|stream| {
            handle_external_agents(stream, Some(root_path), home_path, cors, None)
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_json_transcript("200 OK", &body)
        );
    }

    #[tokio::test]
    async fn golden_api_key_status_transcript() {
        // The per-provider booleans read process env + lease state; the
        // body is computed through the status builder and spliced — the
        // 200 canonical framing is the byte-exact pin.
        let body = api_key_status_response_body();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed.is_object(), "status body is an object: {body}");
        let response = collect_settings_handler_response(|stream| {
            handle_api_key_status(
                stream,
                settings_route_cors("GET", "/api/api-key-status"),
                None,
            )
        })
        .await;
        assert_eq!(
            golden_settings_transcript(&response),
            golden_settings_json_transcript("200 OK", &body)
        );
    }

    /// Codex fields absent from the payload must not be re-dispatched —
    /// a partial settings save must not clobber live state with defaults.
    #[test]
    fn settings_post_skips_codex_control_msgs_for_absent_fields() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex"
        })
        .to_string();

        let (status, _) = settings_post_result(&body, Some(dir.path()), &bus);
        assert_eq!(status, 200);

        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(msg) = event {
                assert!(
                    !matches!(
                        msg,
                        ControlMsg::SetCodexCommand { .. }
                            | ControlMsg::SetCodexSandbox { .. }
                            | ControlMsg::SetCodexApprovalPolicy { .. }
                            | ControlMsg::SetCodexServiceTier { .. }
                            | ControlMsg::SetCodexManagedContext { .. }
                            | ControlMsg::SetCodexContextArchive { .. }
                    ),
                    "unexpected codex control msg for absent payload field: {msg:?}"
                );
            }
        }
    }
}

//! Per-session billing selection for supervised Claude Code. The ordinary
//! external-child environment remains scrubbed; only an explicitly selected
//! API-key session receives the resolved Anthropic key after that scrub.
//!
//! Keep credentials out of argv, persisted launch config, and diagnostics.
//! Probe the same CLI/settings/environment before sending any model request:
//! an inherited login or apiKeyHelper must not change the selected billing.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use tokio::process::Command;

use crate::project::ClaudeAuthMode;
use crate::provider::ProjectEnvKeys;

const AUTH_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const HOST_MANAGED_ENV: &str = "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST";
const MISSING_API_KEY: &str = "Claude Code API-key billing requires a usable Anthropic API key. \
    Add or unlock one in Settings → API Keys, then try again.";

// These settings can otherwise route a nominally subscribed/API-key session
// through a different provider or credential. Clear both process environment
// and the CLI's lower-precedence user/project/local settings, without changing
// unrelated hooks, permissions, MCP servers, or the user's files.
const COMPETING_AUTH_ENV: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_PROFILE",
    "ANTHROPIC_FEDERATION_RULE_ID",
    "ANTHROPIC_ORGANIZATION_ID",
    "ANTHROPIC_WORKSPACE_ID",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_GATEWAY",
    "CLAUDE_CODE_SIMPLE",
    "ANTHROPIC_UNIX_SOCKET",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_GATEWAY_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_WEBSOCKET_AUTH_FILE_DESCRIPTOR",
    "CCR_AGENT_PROXY_TOKEN_FILE_DESCRIPTOR",
    "CCR_OAUTH_TOKEN_FILE",
    "CLAUDE_CODE_HOST_AUTH_ENV_VAR",
    "CLAUDE_CODE_HOST_CREDS_FILE",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "AWS_BEARER_TOKEN_BEDROCK",
];

/// Check credential availability before saving a billing change or stopping a
/// working session. Subscription does not read/unseal provider API keys.
pub(crate) fn validate_auth_mode(mode: ClaudeAuthMode, working_dir: &Path) -> Result<(), String> {
    auth_key_for_mode(mode, || resolve_api_key(working_dir)).map(|_| ())
}

fn auth_key_for_mode(
    mode: ClaudeAuthMode,
    load_key: impl FnOnce() -> Option<String>,
) -> Result<Option<String>, String> {
    match mode {
        ClaudeAuthMode::Subscription => Ok(None),
        ClaudeAuthMode::ApiKey => load_key()
            .filter(|key| !key.trim().is_empty())
            .map(Some)
            .ok_or_else(|| MISSING_API_KEY.to_string()),
    }
}

fn select_api_key(primary: Option<String>, project_keys: &ProjectEnvKeys) -> Option<String> {
    primary
        .filter(|value| !value.trim().is_empty())
        .or_else(|| project_keys.get(API_KEY_ENV))
}

fn resolve_api_key(working_dir: &Path) -> Option<String> {
    // Same precedence as native Anthropic requests: active lease, daemon env,
    // custody, then the selected project's .env. An off-box HTTP relay cannot
    // supply a key to an external CLI and is deliberately not a fallback.
    select_api_key(
        crate::credential_leases::provider_api_key(API_KEY_ENV),
        &ProjectEnvKeys::load(working_dir),
    )
}

/// Resolved once per spawn, so the auth probe and the model process see the
/// same key and transcript/config home. Deliberately not Debug/Serialize.
pub(super) struct PreparedClaudeAuth {
    mode: ClaudeAuthMode,
    api_key: Option<String>,
    config_dir: Option<PathBuf>,
}

impl PreparedClaudeAuth {
    pub(super) fn resolve(mode: ClaudeAuthMode, working_dir: &Path) -> Result<Self, String> {
        let api_key = auth_key_for_mode(mode, || resolve_api_key(working_dir))?;
        Ok(Self {
            mode,
            api_key,
            // Retain this even in API mode: the leased home also carries
            // transcripts required to resume the existing native session ID.
            config_dir: crate::credential_leases::materialized_claude_config_dir(),
        })
    }

    /// Call after `apply_external_child_env_policy`, and before any subcommand
    /// (`--settings` is a root option; `auth status --settings` is rejected).
    pub(super) fn apply(&self, command: &mut Command) {
        let mut env = serde_json::Map::new();
        command.env_remove(API_KEY_ENV).env_remove(HOST_MANAGED_ENV);
        for name in COMPETING_AUTH_ENV {
            command.env_remove(name);
            env.insert((*name).into(), json!(""));
        }
        match self.mode {
            ClaudeAuthMode::Subscription => {
                env.insert(API_KEY_ENV.into(), json!(""));
                // The host-managed flag disables stored OAuth too. It is
                // only suitable for the explicitly injected API-key path.
                env.insert(HOST_MANAGED_ENV.into(), json!(""));
            }
            ClaudeAuthMode::ApiKey => {
                // No key appears in the settings JSON on argv. Host-managed
                // routing prevents user settings from replacing/blanking this
                // key or selecting a saved subscription/helper instead.
                command.env(HOST_MANAGED_ENV, "1");
                if let Some(key) = &self.api_key {
                    command.env(API_KEY_ENV, key);
                }
            }
        }
        // Empty string suppresses lower-precedence helpers; JSON null does
        // not. Verified against the official Claude Code 2.1.304 binary.
        command
            .arg("--settings")
            .arg(json!({ "apiKeyHelper": "", "env": env }).to_string());
        if let Some(dir) = &self.config_dir {
            command.env("CLAUDE_CONFIG_DIR", dir);
        }
    }

    /// `auth status` resolves local authentication without making a model
    /// request. Reject unknown/unsupported results rather than bill a different
    /// account. Never include CLI stdout/stderr (potential secrets) in errors.
    pub(super) async fn verify(
        &self,
        executable: &str,
        working_dir: &Path,
        dns_credential_env: Option<&str>,
        dns_credential_store: Option<&Path>,
    ) -> Result<(), String> {
        let mut command =
            super::spawn_backend_command(&super::AgentBackend::ClaudeCode, executable);
        command
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        super::apply_external_child_env_policy(&mut command);
        self.apply(&mut command);
        command.args(["auth", "status"]);
        crate::platform::die_with_parent(&mut command);
        let child = crate::credential_leases::spawn_with_dns_credential_scrub(
            &mut command,
            dns_credential_env,
            dns_credential_store,
        )
        .map_err(|_| {
            "Could not start Claude Code to verify the selected billing mode.".to_string()
        })?;
        let output = tokio::time::timeout(AUTH_PROBE_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| "Claude Code billing verification timed out. Try again.".to_string())?
            .map_err(|_| "Could not read Claude Code billing verification.".to_string())?;
        verify_auth_status(self.mode, output.status.success(), &output.stdout)
    }
}

fn verify_auth_status(mode: ClaudeAuthMode, success: bool, stdout: &[u8]) -> Result<(), String> {
    let text = String::from_utf8_lossy(stdout);
    let status = text
        .find('{')
        .zip(text.rfind('}'))
        .and_then(|(start, end)| {
            (start <= end)
                .then(|| serde_json::from_str::<serde_json::Value>(&text[start..=end]).ok())
                .flatten()
        });
    let matched = status.as_ref().is_some_and(|status| {
        success
            && status.get("loggedIn").and_then(|v| v.as_bool()) == Some(true)
            && status
                .get("apiProvider")
                .is_none_or(|v| v.as_str() == Some("firstParty"))
            && match mode {
                ClaudeAuthMode::Subscription => {
                    status.get("authMethod").and_then(|v| v.as_str()) == Some("claude.ai")
                }
                ClaudeAuthMode::ApiKey => {
                    status.get("authMethod").and_then(|v| v.as_str()) == Some("api_key")
                        && status.get("apiKeySource").and_then(|v| v.as_str()) == Some(API_KEY_ENV)
                }
            }
    });
    if matched {
        return Ok(());
    }
    Err(match mode {
        ClaudeAuthMode::Subscription => {
            "Claude Code could not verify Subscription billing. \
            Sign in to Claude Code with your subscription and retry, or select API key billing. \
            Your Claude Code version must support `claude auth status` and per-run settings."
        }
        ClaudeAuthMode::ApiKey => {
            "Claude Code could not verify API-key billing with the selected \
            Anthropic key. Update Claude Code and check for managed authentication settings, then \
            retry. The session has not fallen back to subscription billing."
        }
    }
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMMY_KEY: &str = "dummy-anthropic-key-never-send";

    fn prepared(mode: ClaudeAuthMode, config_dir: Option<PathBuf>) -> PreparedClaudeAuth {
        PreparedClaudeAuth {
            mode,
            api_key: (mode == ClaudeAuthMode::ApiKey).then(|| DUMMY_KEY.to_string()),
            config_dir,
        }
    }

    fn explicit_env(command: &Command, name: &str) -> Option<String> {
        command
            .as_std()
            .get_envs()
            .find_map(|(key, value)| {
                (key == name).then(|| value.map(|v| v.to_string_lossy().into_owned()))
            })
            .flatten()
    }

    #[test]
    fn api_key_is_injected_only_for_explicit_api_mode_and_never_on_argv() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [ClaudeAuthMode::Subscription, ClaudeAuthMode::ApiKey] {
            let mut command = Command::new("fixture-claude");
            command
                .env_clear()
                .env(API_KEY_ENV, "ambient-key")
                .env("ANTHROPIC_AUTH_TOKEN", "ambient-token");
            prepared(mode, Some(dir.path().to_path_buf())).apply(&mut command);
            let args: Vec<_> = command
                .as_std()
                .get_args()
                .map(|v| v.to_string_lossy().into_owned())
                .collect();
            assert_eq!(args[0], "--settings");
            assert!(!args.join(" ").contains(DUMMY_KEY));
            assert!(!args.join(" ").contains("ambient-key"));
            assert_eq!(explicit_env(&command, "ANTHROPIC_AUTH_TOKEN"), None);
            assert_eq!(
                explicit_env(&command, "CLAUDE_CONFIG_DIR"),
                Some(dir.path().to_string_lossy().into_owned())
            );
            match mode {
                ClaudeAuthMode::Subscription => {
                    assert_eq!(explicit_env(&command, API_KEY_ENV), None);
                    assert_eq!(explicit_env(&command, HOST_MANAGED_ENV), None);
                }
                ClaudeAuthMode::ApiKey => {
                    assert_eq!(
                        explicit_env(&command, API_KEY_ENV).as_deref(),
                        Some(DUMMY_KEY)
                    );
                    assert_eq!(
                        explicit_env(&command, HOST_MANAGED_ENV).as_deref(),
                        Some("1")
                    );
                }
            }
            let settings: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
            assert_eq!(settings["apiKeyHelper"], "");
            for name in COMPETING_AUTH_ENV {
                assert_eq!(settings["env"][name], "", "{name}");
            }
            if mode == ClaudeAuthMode::Subscription {
                assert_eq!(settings["env"][API_KEY_ENV], "");
            } else {
                assert!(settings["env"].get(API_KEY_ENV).is_none());
            }
        }
    }

    #[test]
    fn subscription_never_unseals_a_key_and_api_mode_requires_one() {
        assert_eq!(
            auth_key_for_mode(ClaudeAuthMode::Subscription, || panic!(
                "must not read a key"
            )),
            Ok(None)
        );
        for key in [None, Some(String::new()), Some("  ".to_string())] {
            assert_eq!(
                auth_key_for_mode(ClaudeAuthMode::ApiKey, || key),
                Err(MISSING_API_KEY.to_string())
            );
        }
        assert_eq!(
            auth_key_for_mode(ClaudeAuthMode::ApiKey, || Some(DUMMY_KEY.into())),
            Ok(Some(DUMMY_KEY.into()))
        );
    }

    #[test]
    fn project_key_fallback_does_not_override_lease_env_or_custody_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            format!("ANTHROPIC_API_KEY={DUMMY_KEY}\nANTHROPIC_BASE_URL=https://invalid.example\n"),
        )
        .unwrap();
        let keys = ProjectEnvKeys::load(dir.path());
        assert_eq!(
            select_api_key(Some("selected-key".into()), &keys).as_deref(),
            Some("selected-key")
        );
        assert_eq!(select_api_key(None, &keys).as_deref(), Some(DUMMY_KEY));
        assert_eq!(
            select_api_key(Some(" ".into()), &keys).as_deref(),
            Some(DUMMY_KEY)
        );
        assert_eq!(select_api_key(None, &ProjectEnvKeys::none()), None);
        assert!(validate_auth_mode(ClaudeAuthMode::Subscription, dir.path()).is_ok());
    }

    #[test]
    fn auth_status_must_match_selected_billing_in_both_directions() {
        let subscription =
            br#"{"loggedIn":true,"authMethod":"claude.ai","apiKeySource":"/login managed key"}"#;
        let api = br#"{"loggedIn":true,"authMethod":"api_key","apiKeySource":"ANTHROPIC_API_KEY"}"#;
        assert!(verify_auth_status(ClaudeAuthMode::Subscription, true, subscription).is_ok());
        assert!(verify_auth_status(ClaudeAuthMode::ApiKey, true, api).is_ok());
        assert!(verify_auth_status(ClaudeAuthMode::Subscription, true, api).is_err());
        assert!(verify_auth_status(ClaudeAuthMode::ApiKey, true, subscription).is_err());
        assert!(verify_auth_status(ClaudeAuthMode::ApiKey, false, api).is_err());
        let saved_console =
            br#"{"loggedIn":true,"authMethod":"api_key","apiKeySource":"/login managed key"}"#;
        assert!(verify_auth_status(ClaudeAuthMode::ApiKey, true, saved_console).is_err());
        let third_party = br#"{"loggedIn":true,"authMethod":"api_key","apiKeySource":"ANTHROPIC_API_KEY","apiProvider":"bedrock"}"#;
        assert!(verify_auth_status(ClaudeAuthMode::ApiKey, true, third_party).is_err());
    }

    #[test]
    fn missing_unknown_or_malformed_auth_never_falls_back_or_echoes_credentials() {
        for mode in [ClaudeAuthMode::Subscription, ClaudeAuthMode::ApiKey] {
            for output in [
                r#"{"loggedIn":false,"authMethod":"none"}"#,
                r#"{"loggedIn":true,"authMethod":"api_key_helper"}"#,
                r#"{"loggedIn":true,"authMethod":"new-unknown-mode"}"#,
                DUMMY_KEY,
                "}{",
            ] {
                let error = verify_auth_status(mode, true, output.as_bytes()).unwrap_err();
                assert!(!error.contains(DUMMY_KEY));
            }
        }
    }
}

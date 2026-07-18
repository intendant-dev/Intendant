//! Ambient host-credential classification for child-process env scrubs.
//!
//! The controller's process env carries more than provider keys: agent
//! sockets, cloud credentials, forge tokens, and session-bus addresses
//! inherited from the launching shell. Model-driven children (the runtime
//! and its shells, supervised external CLIs) must not inherit ambient
//! authority the user never granted the session, so spawn boundaries
//! combine this classifier with their provider-key scrubs.

/// Exact env names that carry (or point at) ambient host credentials.
/// Matched on the ASCII-uppercased form. Public so spawn boundaries can
/// remove the canonical names unconditionally (mirroring the provider-key
/// scrub's shape) in addition to sweeping the inherited-name view through
/// [`is_ambient_credential_env`].
pub const AMBIENT_CREDENTIAL_ENV_VARS: &[&str] = &[
    // SSH agent socket: holding it means signing with the user's keys.
    "SSH_AUTH_SOCK",
    // Cloud-provider secrets and their session forms.
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_SECURITY_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    // Forge tokens.
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    // Registry/publish tokens.
    "NPM_TOKEN",
    "NODE_AUTH_TOKEN",
    "CARGO_REGISTRY_TOKEN",
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
    // Pointers to credential stores; removal falls back to the tool's
    // default lookup rather than breaking it.
    "KUBECONFIG",
    "DOCKER_CONFIG",
    "NETRC",
    // Session bus: an unlocked desktop keyring answers over it.
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Suffixes that conventionally mark secret-bearing names.
const AMBIENT_CREDENTIAL_ENV_SUFFIXES: &[&str] = &[
    "_TOKEN",
    "_SECRET",
    "_PASSWORD",
    "_PASSWD",
    "_CREDENTIALS",
    "_PRIVATE_KEY",
    "_ACCESS_KEY",
];

/// True when `name` names ambient host-credential material that must not
/// reach model-driven children. `INTENDANT_*` names are the
/// controller→child control channel (the mock-provider e2e rig rides
/// `INTENDANT_MOCK_*` into children) and are never classified as
/// credentials. Matching is on the ASCII-uppercased name: Windows env
/// names are case-insensitive, and `.env` files preserve arbitrary casing.
pub fn is_ambient_credential_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("INTENDANT_") {
        return false;
    }
    AMBIENT_CREDENTIAL_ENV_VARS.contains(&upper.as_str())
        || AMBIENT_CREDENTIAL_ENV_SUFFIXES
            .iter()
            .any(|s| upper.ends_with(s))
}

/// Controller-side env var holding comma-separated exact env names
/// (case-insensitive) the user deliberately exempts from the ambient
/// scrub at spawn boundaries — e.g. `SSH_AUTH_SOCK` for agent shells that
/// must push over SSH. Provider/model-API keys are never exempted.
pub const ENV_PASSTHROUGH_VAR: &str = "INTENDANT_ENV_PASSTHROUGH";

/// Parse a raw [`ENV_PASSTHROUGH_VAR`] value into the exemption set:
/// comma-separated exact names, trimmed and ASCII-uppercased so lookups
/// are case-insensitive. Takes the raw value as a parameter — spawn
/// boundaries read the process env, tests inject — and `None`/empty
/// yields the empty set.
pub fn env_passthrough_set(raw: Option<&str>) -> std::collections::HashSet<String> {
    raw.map(|raw| {
        raw.split(',')
            .map(|name| name.trim().to_ascii_uppercase())
            .filter(|name| !name.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambient_credentials_are_classified() {
        for name in [
            "SSH_AUTH_SOCK",
            "ssh_auth_sock",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "KUBECONFIG",
            "DOCKER_CONFIG",
            "DBUS_SESSION_BUS_ADDRESS",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "MY_SERVICE_TOKEN",
            "DB_PASSWORD",
            "AZURE_CLIENT_SECRET",
            "gitlab_private_key",
            "MINIO_ACCESS_KEY",
            "POSTGRES_PASSWD",
        ] {
            assert!(is_ambient_credential_env(name), "{name} must be scrubbed");
        }
    }

    #[test]
    fn benign_and_control_names_survive() {
        for name in [
            "PATH",
            "HOME",
            "TERM",
            "LANG",
            "SHELL",
            "TMPDIR",
            "USER",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "XDG_RUNTIME_DIR",
            "GITHUB_REPOSITORY",
            "CARGO_TARGET_DIR",
            "PROVIDER",
            "INTENDANT_MOCK_SCRIPT",
            "INTENDANT_MCP_URL",
            "intendant_sandbox_write_paths",
        ] {
            assert!(!is_ambient_credential_env(name), "{name} must survive");
        }
    }

    #[test]
    fn passthrough_set_parses_names_case_insensitively() {
        assert!(env_passthrough_set(None).is_empty());
        assert!(env_passthrough_set(Some("")).is_empty());
        assert!(env_passthrough_set(Some(" , ,")).is_empty());

        let set = env_passthrough_set(Some(" ssh_auth_sock , GH_TOKEN,,Kubeconfig "));
        assert_eq!(set.len(), 3);
        for name in ["SSH_AUTH_SOCK", "GH_TOKEN", "KUBECONFIG"] {
            assert!(set.contains(name), "{name} must be in the passthrough set");
        }
        assert!(
            !set.contains("ssh_auth_sock"),
            "entries are stored uppercased"
        );
    }
}

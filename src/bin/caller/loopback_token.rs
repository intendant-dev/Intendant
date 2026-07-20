//! The per-boot loopback admission token: the credential that turns "can
//! reach 127.0.0.1" back into "can read the owner's state root".
//!
//! Bare loopback historically bound the root-capable trusted-local
//! posture from transport facts alone (direct ingress + loopback peer +
//! loopback Host + no proxy provenance). That made every same-box
//! process — any uid, since TCP loopback is not uid-scoped — an owner
//! surface, and on Linux/Windows even the *sandboxed* runtime could
//! reach it (no per-port deny exists under Landlock/restricted tokens;
//! macOS closes the port itself via the Seatbelt guard, #463). The
//! admission token ends tokenless drive-by on every platform and for
//! every daemon posture, `--no-tls` dev/QA daemons included: the daemon
//! mints a fresh random secret each boot and owner-posture surfaces
//! refuse loopback requests that do not present it.
//!
//! Custody and the honest envelope:
//! - The secret lives in process memory (lazy, first gate consult or
//!   startup persist) and in one 0600 file per daemon instance,
//!   `<state root>/loopback-tokens/<port>.token`, written atomically at
//!   gateway wiring. A same-uid **unsandboxed** process reads it by
//!   design — that is the unchanged owner surface (`local_process`).
//! - macOS: the Seatbelt credential clause denies the sandboxed runtime
//!   reads of `loopback-tokens/` (and the port guard denies the socket
//!   outright), so possession really is scoped to owner processes.
//! - Linux/Windows: the sandbox cannot block same-uid file reads
//!   (Landlock is allowlist-only; restricted tokens are write-only), so
//!   a sandboxed shell that can read the home can read the token. The
//!   token still ends tokenless drive-by and cross-uid loopback access;
//!   airtight same-uid separation arrives with credential custody.
//! - The token never appears in the F1.5 CLI-discovery descriptor
//!   (secrets-free by contract), never in an env var the daemon *reads*
//!   (children inherit env; the daemon always mints), and never on
//!   stderr/stdout (the daemon log tee lands in `logs/`, the one
//!   subtree the write sandbox grants — see [`print_owner_url_to_tty`]).
//!
//! This token authenticates *admission to the loopback owner posture*;
//! it does not create a principal class. Surfaces that admit it mint
//! exactly the principals they minted before (`trusted-dashboard-http`,
//! `TrustedLocal`, `local_process`). It is deliberately independent of
//! the `/mcp` token ladder (`mcp_gate::loopback_mcp_auth_token`): that
//! secret derives per-session MCP tokens and rides supervised-backend
//! bootstrap URLs, this one is the on-disk owner credential — coupling
//! them would make one file a forgery oracle for the other ladder.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Header a client attaches to present the token. Query (`?token=`) and
/// `Authorization: Bearer` are accepted as well so the dashboard can
/// reuse the existing federation-token plumbing; see
/// [`loopback_token_presented`].
pub(crate) const LOOPBACK_TOKEN_HEADER: &str = "x-intendant-loopback-token";

/// Client-side override consumed by `ctl` and harnesses; the daemon
/// itself NEVER reads this (a daemon-side env intake would leak into
/// child processes and defeat the macOS file denies — the daemon always
/// mints).
pub(crate) const LOOPBACK_TOKEN_ENV: &str = "INTENDANT_LOOPBACK_TOKEN";

/// Subdirectory of the state root holding one token file per running
/// daemon instance, namespaced by bound gateway port so concurrent
/// daemons sharing a home never clobber each other. Denied to the
/// macOS sandboxed runtime by the Seatbelt credential clause.
pub(crate) const LOOPBACK_TOKEN_DIR: &str = "loopback-tokens";

static LOOPBACK_ADMISSION_TOKEN: OnceLock<String> = OnceLock::new();

/// The daemon's in-memory admission token — the sole authority the
/// gates consult. Lazily minted so unit-test gateways get a real token
/// with no file I/O; the startup wiring persists this same value for
/// clients via [`persist_for_instance`].
pub(crate) fn loopback_admission_token() -> &'static str {
    LOOPBACK_ADMISSION_TOKEN.get_or_init(|| {
        use ring::rand::SecureRandom;
        let rng = ring::rand::SystemRandom::new();
        let mut bytes = [0u8; 32];
        rng.fill(&mut bytes)
            .expect("system CSPRNG unavailable — cannot mint loopback admission token");
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    })
}

pub(crate) fn loopback_token_dir(state_root: &Path) -> PathBuf {
    state_root.join(LOOPBACK_TOKEN_DIR)
}

/// The per-instance token file: a single line holding the raw token
/// (trailing newline), so `$(cat …)` works in shells and rigs.
pub(crate) fn loopback_token_path(state_root: &Path, port: u16) -> PathBuf {
    loopback_token_dir(state_root).join(format!("{port}.token"))
}

/// Per-instance sidecar with non-secret operational facts a client
/// needs *before* it can talk to the daemon — today just the dashboard
/// scheme, so `ctl dashboard-url` can compose the owner URL without
/// probing. 0600 like the token (same subtree, same denies); NOT the
/// F1.5 discovery descriptor, which is a 0644 secrets-free pointer with
/// its own contract.
pub(crate) fn loopback_sidecar_path(state_root: &Path, port: u16) -> PathBuf {
    loopback_token_dir(state_root).join(format!("{port}.json"))
}

/// Persist the in-memory token (and its sidecar) for `port` under
/// `state_root`, 0600 via stage + atomic rename, overwriting whatever a
/// previous boot on this port left behind. Files for OTHER ports are
/// never touched — another live daemon on this home may own them.
pub(crate) fn persist_for_instance(state_root: &Path, port: u16, tls: bool) -> io::Result<PathBuf> {
    let dir = loopback_token_dir(state_root);
    intendant_core::state_paths::create_private_dir_all(&dir)?;
    let token_path = loopback_token_path(state_root, port);
    write_private_atomic(
        &dir,
        &token_path,
        format!("{}\n", loopback_admission_token()).as_bytes(),
    )?;
    let sidecar = serde_json::json!({
        "v": 1,
        "scheme": if tls { "https" } else { "http" },
    });
    write_private_atomic(
        &dir,
        &loopback_sidecar_path(state_root, port),
        format!("{sidecar}\n").as_bytes(),
    )?;
    Ok(token_path)
}

fn write_private_atomic(dir: &Path, dest: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let (mut staged, staged_path) = crate::file_watcher::stage_in(dir)?;
    let write = staged.write_all(contents).and_then(|()| staged.sync_all());
    drop(staged);
    if let Err(err) = write {
        let _ = std::fs::remove_file(&staged_path);
        return Err(err);
    }
    crate::file_watcher::persist_staged(&staged_path, dest)
}

/// Constant-time equality against the daemon's admission token.
fn matches_admission_token(candidate: &str) -> bool {
    ring::constant_time::verify_slices_are_equal(
        candidate.trim().as_bytes(),
        loopback_admission_token().as_bytes(),
    )
    .is_ok()
}

/// Whether this request presents the loopback admission token on any
/// accepted channel: the dedicated header, a `?token=` query parameter
/// (the dashboard's existing WS plumbing), or `Authorization: Bearer`
/// (the dashboard's existing fetch plumbing). Every candidate is
/// checked — a non-matching value on a shared channel (a federation
/// bearer, an MCP token) simply doesn't bind, mirroring the
/// `McpTokenBinding::Missing` tolerance for shared namespaces.
pub(crate) fn loopback_token_presented(header_text: &str) -> bool {
    if let Some(value) = crate::web_gateway::http_header_value(header_text, LOOPBACK_TOKEN_HEADER) {
        if matches_admission_token(value) {
            return true;
        }
    }
    let request_line = header_text.lines().next().unwrap_or("");
    if let Some(value) = crate::web_gateway::query_param(request_line, "token") {
        if matches_admission_token(&value) {
            return true;
        }
    }
    if let Some(value) = crate::web_gateway::http_header_value(header_text, "authorization") {
        let value = value.trim();
        if let Some(bearer) = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
        {
            if matches_admission_token(bearer) {
                return true;
            }
        }
    }
    false
}

/// The named, actionable refusal every owner-posture surface emits for
/// tokenless (or stale-token'd) loopback requests. One builder so HTTP,
/// WS, datachannel signaling, and the /mcp ladder cite identically.
pub(crate) fn refusal_error_message() -> String {
    let path = match crate::sandbox::gateway_loopback_port() {
        Some(port) => loopback_token_path(&crate::platform::intendant_home(), port)
            .display()
            .to_string(),
        None => loopback_token_dir(&crate::platform::intendant_home())
            .join("<port>.token")
            .display()
            .to_string(),
    };
    format!(
        "loopback owner surfaces require this daemon's per-boot admission token: \
         send header {LOOPBACK_TOKEN_HEADER} (or ?token= / Authorization: Bearer) \
         with the contents of {path} — a local owner process reads that file, or \
         sets {LOOPBACK_TOKEN_ENV}; tokens rotate every daemon boot. \
         See docs/src/trust-architecture.md (Loopback trust vs. the runtime sandbox)."
    )
}

/// Client-side discovery for `ctl` and rigs: explicit env override
/// first, then the per-instance file under `state_root`. Returns the
/// trimmed token, or `None` when neither source yields one (the caller
/// proceeds tokenless and surfaces the daemon's named refusal).
/// `env_override` is passed in by the transport edge (`ctl` reads the
/// process env; tests inject) so this stays hermetic.
pub(crate) fn discover_client_token(
    env_override: Option<&str>,
    state_root: &Path,
    port: u16,
) -> Option<String> {
    if let Some(explicit) = env_override.map(str::trim) {
        if !explicit.is_empty() {
            return Some(explicit.to_string());
        }
    }
    let raw = std::fs::read_to_string(loopback_token_path(state_root, port)).ok()?;
    let token = raw.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Print the token-bearing owner URL directly to the controlling
/// terminal, NEVER through stdout/stderr: the daemon log tee mirrors
/// those into a session-scoped `daemon.log` under `logs/` — the one
/// state-root subtree the write sandbox grants and (macOS included)
/// leaves readable — so a token printed there would hand the secret to
/// every sandboxed shell. No controlling terminal (services, CI rigs)
/// means no print; those consumers read the token file instead.
/// Mock-provider daemons (`PROVIDER=mock`) skip the courtesy print too:
/// e2e rigs boot dozens in parallel with piped stderr but an inherited
/// controlling terminal, and the raw lines would interleave over the
/// invoking test harness — rigs read the token file by contract.
pub(crate) fn print_owner_url_to_tty(line: &str) {
    use std::io::Write;
    if std::env::var("PROVIDER").is_ok_and(|provider| provider.eq_ignore_ascii_case("mock")) {
        return;
    }
    #[cfg(unix)]
    let tty = std::fs::OpenOptions::new().write(true).open("/dev/tty");
    #[cfg(windows)]
    let tty = std::fs::OpenOptions::new().write(true).open("CONOUT$");
    #[cfg(not(any(unix, windows)))]
    let tty: io::Result<std::fs::File> =
        Err(io::Error::new(io::ErrorKind::Unsupported, "no tty path"));
    if let Ok(mut tty) = tty {
        let _ = writeln!(tty, "{line}");
    }
}

/// The tokened URL an owner pastes into a browser. Loopback display
/// hosts only — the token is loopback admission, meaningless (and a
/// leak hazard) on a LAN/mTLS URL.
pub(crate) fn tokened_dashboard_url(scheme: &str, port: u16) -> String {
    format!(
        "{scheme}://127.0.0.1:{port}/?token={}",
        loopback_admission_token()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_paths_are_port_namespaced_under_the_state_root() {
        let root = Path::new("/tmp/x");
        assert_eq!(
            loopback_token_path(root, 8765),
            Path::new("/tmp/x/loopback-tokens/8765.token")
        );
        assert_eq!(
            loopback_sidecar_path(root, 18800),
            Path::new("/tmp/x/loopback-tokens/18800.json")
        );
    }

    #[test]
    fn persist_writes_owner_only_files_and_overwrites_prior_boot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = persist_for_instance(tmp.path(), 4321, false).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.trim(), loopback_admission_token());
        let sidecar: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(loopback_sidecar_path(tmp.path(), 4321)).unwrap(),
        )
        .unwrap();
        assert_eq!(sidecar["scheme"], "http");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "token file must be owner-only");
            let dir_mode = std::fs::metadata(loopback_token_dir(tmp.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "token dir must be owner-only");
        }
        // A prior boot's stale file (different contents) is replaced.
        std::fs::write(&path, "stale\n").unwrap();
        let path2 = persist_for_instance(tmp.path(), 4321, true).unwrap();
        assert_eq!(path, path2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            loopback_admission_token()
        );
        let sidecar: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(loopback_sidecar_path(tmp.path(), 4321)).unwrap(),
        )
        .unwrap();
        assert_eq!(sidecar["scheme"], "https");
    }

    #[test]
    fn presented_on_header_query_and_bearer_channels_only_when_matching() {
        let token = loopback_admission_token();
        let with_header = format!(
            "GET /api/sessions HTTP/1.1\r\nHost: h\r\n{LOOPBACK_TOKEN_HEADER}: {token}\r\n\r\n"
        );
        assert!(loopback_token_presented(&with_header));
        let with_query = format!("GET /ws?token={token} HTTP/1.1\r\nHost: h\r\n\r\n");
        assert!(loopback_token_presented(&with_query));
        let with_bearer =
            format!("GET /api/me HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer {token}\r\n\r\n");
        assert!(loopback_token_presented(&with_bearer));

        assert!(!loopback_token_presented(
            "GET /api/sessions HTTP/1.1\r\nHost: h\r\n\r\n"
        ));
        // Non-matching values on the shared channels do not bind (they
        // may be federation or MCP tokens) — and do not error.
        assert!(!loopback_token_presented(
            "GET /ws?token=nope HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer other\r\n\r\n"
        ));
        let stale = format!("GET / HTTP/1.1\r\nHost: h\r\n{LOOPBACK_TOKEN_HEADER}: stale\r\n\r\n");
        assert!(!loopback_token_presented(&stale));
    }

    #[test]
    fn discovery_prefers_env_override_then_file_and_trims() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(discover_client_token(None, tmp.path(), 9), None);
        assert_eq!(
            discover_client_token(Some("  from-env "), tmp.path(), 9),
            Some("from-env".to_string())
        );
        // Empty override falls through to the file.
        persist_for_instance(tmp.path(), 9, false).unwrap();
        assert_eq!(
            discover_client_token(Some("   "), tmp.path(), 9),
            Some(loopback_admission_token().to_string())
        );
        assert_eq!(
            discover_client_token(None, tmp.path(), 9),
            Some(loopback_admission_token().to_string())
        );
    }

    #[test]
    fn refusal_message_names_file_env_and_docs() {
        let message = refusal_error_message();
        assert!(message.contains(LOOPBACK_TOKEN_DIR));
        assert!(message.contains(LOOPBACK_TOKEN_ENV));
        assert!(message.contains(LOOPBACK_TOKEN_HEADER));
        assert!(message.contains("trust-architecture.md"));
    }

    #[test]
    fn tokened_url_targets_loopback_and_carries_the_token() {
        let url = tokened_dashboard_url("https", 8765);
        assert!(url.starts_with("https://127.0.0.1:8765/?token="));
        assert!(url.ends_with(loopback_admission_token()));
    }
}

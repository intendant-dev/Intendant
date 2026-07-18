use std::path::{Path, PathBuf};

/// Escape a path for a double-quoted Seatbelt profile string literal.
/// Paths that cannot be represented safely (non-UTF-8 or control bytes)
/// are refused — the caller fails loudly rather than producing a profile
/// that means something else.
#[cfg(target_os = "macos")]
pub(crate) fn seatbelt_path_literal(path: &Path) -> Result<String, String> {
    let Some(text) = path.to_str() else {
        return Err(format!(
            "sandbox path {} is not valid UTF-8",
            path.display()
        ));
    };
    if text.chars().any(|c| c.is_control()) {
        return Err(format!("sandbox path {text:?} contains control characters"));
    }
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Best-effort canonicalization for a policy path that may not exist yet:
/// Seatbelt rules and the consent-flow forbidden-path checks match REAL
/// paths, so resolve through symlinked parents (`/tmp`, `/var`, `/etc`)
/// even when the leaf itself is absent. Cross-platform — the consent
/// classifier uses it on every OS.
fn canonicalize_for_profile(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|real| real.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

/// Every `.env` the controller's provider-key search could load:
/// `dotenvy::dotenv()` walks `start` (the launch cwd) and its ancestors,
/// and the project-root layer is always `start` or one of its ancestors,
/// so the walk covers that layer too. Candidates are kept even when the
/// file does not exist — a Seatbelt rule on an absent path simply never
/// matches, and denying creation also stops a sandboxed command from
/// planting a `.env` that a future controller start would auto-load into
/// its own environment.
#[cfg(target_os = "macos")]
fn env_file_candidates(start: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut dir = Some(start);
    while let Some(current) = dir {
        candidates.push(current.join(".env"));
        dir = current.parent();
    }
    candidates
}

/// Compose the sensitive deny rule from directory subpaths and single-file
/// literals. Empty input yields an empty clause — a filterless
/// `(deny file-read* file-write*)` would deny *everything*.
#[cfg(target_os = "macos")]
fn seatbelt_deny_clause_for(dirs: &[PathBuf], files: &[PathBuf]) -> Result<String, String> {
    let mut filters = Vec::new();
    for path in dirs {
        filters.push(format!("(subpath {})", seatbelt_path_literal(path)?));
    }
    for path in files {
        filters.push(format!("(literal {})", seatbelt_path_literal(path)?));
    }
    if filters.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "(deny file-read* file-write* {})\n",
        filters.join(" ")
    ))
}

/// Seatbelt deny rules for the user-secret directories the runtime's
/// `validate_path` denylist protects (`~/.ssh`, `~/.gnupg`). The denylist
/// only guards structured tool arguments (editFile/inspectPath) — command
/// strings run by executeCommand bypass it entirely, and no string
/// inspection can close that honestly. This clause is the always-on
/// baseline: it rides every macOS runtime profile including the
/// `--no-sandbox` sensitive-only wrap.
///
/// Appended LAST to a profile it wins over every allow (last-match-wins).
/// `/proc`, `/sys`, and `/etc/shadow` from the denylist are Linux paths
/// with no macOS counterpart, and `/dev` cannot be blanket-denied (every
/// process needs its tty and /dev/null). Returns an empty clause when
/// nothing is resolvable to protect.
#[cfg(target_os = "macos")]
pub(crate) fn seatbelt_sensitive_deny_clause() -> Result<String, String> {
    let mut deny_dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        let home = std::fs::canonicalize(&home).unwrap_or(home);
        deny_dirs.push(home.join(".ssh"));
        deny_dirs.push(home.join(".gnupg"));
    }
    seatbelt_deny_clause_for(&deny_dirs, &[])
}

/// Seatbelt deny rules for the provider-credential files the controller
/// loads at startup: the per-user intendant config home
/// (`dirs::config_dir()/intendant`, which holds the global `.env`
/// fallback) and every `.env` on the `dotenvy` search path (launch cwd +
/// ancestors, covering the project root). The controller strips key
/// variables from the runtime's environment, but the *files* those keys
/// live in would otherwise stay readable — `curl -d @.env` is exactly the
/// exfiltration the runtime/controller split exists to prevent.
///
/// This clause rides the write-restricted (sandbox-enabled) profiles and
/// the scoped-shell profile, NOT the `--no-sandbox` sensitive-only wrap:
/// the explicit opt-out restores the agent's ability to work on the
/// project's own `.env` when the operator accepts that trade.
///
/// Linux has no equivalent: Landlock is allowlist-only and cannot
/// subtract read access from a granted tree, so there the denylist on
/// structured tools plus the write sandbox remain the whole story, and
/// project/config `.env` files stay readable to sandboxed commands —
/// moving keys out of agent-readable files (credential custody) is the
/// tracked fix (see docs/src/architecture.md).
#[cfg(target_os = "macos")]
pub(crate) fn seatbelt_credential_deny_clause() -> Result<String, String> {
    let mut deny_dirs: Vec<PathBuf> = Vec::new();
    if let Some(config) = dirs::config_dir() {
        deny_dirs.push(canonicalize_for_profile(&config.join("intendant")));
    }
    let env_files: Vec<PathBuf> = std::env::current_dir()
        .map(|cwd| env_file_candidates(&canonicalize_for_profile(&cwd)))
        .unwrap_or_default()
        .iter()
        .map(|candidate| canonicalize_for_profile(candidate))
        .collect();
    seatbelt_deny_clause_for(&deny_dirs, &env_files)
}

/// The always-on macOS profile for the agent runtime when no write
/// sandbox is configured: everything allowed except the user-secret
/// locations (`seatbelt_sensitive_deny_clause`). This is the
/// executeCommand twin of `validate_path` — policy the structured tools
/// already enforce, extended to the whole process tree.
#[cfg(target_os = "macos")]
pub(crate) fn seatbelt_sensitive_only_profile() -> Result<String, String> {
    Ok(format!(
        "(version 1)\n\
         (allow default)\n\
         {}",
        seatbelt_sensitive_deny_clause()?
    ))
}

/// What `configure_sandbox_env` resolved at startup, recorded so the
/// dashboard settings surface and the denial-consent flow can recompute
/// and live-apply the grant environment without re-plumbing `CliFlags`.
pub(crate) struct SandboxRuntimeState {
    /// The default grant set (project/projectless + toolchain caches),
    /// BEFORE `extra_write_paths` — the stable base every recompute
    /// starts from.
    pub base_write_paths: Vec<PathBuf>,
    /// Anchor for resolving relative `extra_write_paths` entries.
    pub project_write_scope: Option<PathBuf>,
    /// `Some(true)` = `--sandbox`, `Some(false)` = `--no-sandbox`: a CLI
    /// flag pins the state for the daemon's lifetime and live settings
    /// changes only persist intent for the next start.
    pub flag_lock: Option<bool>,
}

static SANDBOX_RUNTIME_STATE: std::sync::OnceLock<SandboxRuntimeState> = std::sync::OnceLock::new();

/// Record the startup resolution (idempotent; first call wins).
pub(crate) fn record_sandbox_startup(state: SandboxRuntimeState) {
    let _ = SANDBOX_RUNTIME_STATE.set(state);
}

pub(crate) fn sandbox_runtime_state() -> Option<&'static SandboxRuntimeState> {
    SANDBOX_RUNTIME_STATE.get()
}

pub(crate) fn sandbox_flag_lock() -> Option<bool> {
    SANDBOX_RUNTIME_STATE.get().and_then(|s| s.flag_lock)
}

/// True when the write sandbox is live for runtime spawns (the spawn wrap
/// keys on the grant env var's presence).
pub(crate) fn sandbox_active() -> bool {
    std::env::var_os("INTENDANT_SANDBOX_WRITE_PATHS").is_some_and(|v| !v.is_empty())
}

/// The effective write-grant set as the next runtime spawn will see it.
pub(crate) fn effective_write_paths() -> Vec<PathBuf> {
    match std::env::var("INTENDANT_SANDBOX_WRITE_PATHS") {
        Ok(raw) => std::env::split_paths(&raw)
            .filter(|p| !p.as_os_str().is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Resolve one `extra_write_paths` entry against the recorded project
/// scope. Relative entries without a scope are dropped (fail-closed, same
/// as startup).
fn resolve_extra_write_path(entry: &str, scope: Option<&Path>) -> Option<PathBuf> {
    let path = Path::new(entry);
    if path.as_os_str().is_empty() {
        return None;
    }
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    scope.map(|root| root.join(path))
}

/// Recompute the grant set from the recorded base + `extra` and apply it
/// to the live environment (set when enabled, removed when disabled), so
/// the NEXT runtime spawn picks it up — no restart. Returns the effective
/// set. Callers gate on [`sandbox_flag_lock`] first: a flag-pinned daemon
/// only persists intent. Errors when startup never recorded a state
/// (non-daemon shapes).
pub(crate) fn apply_sandbox_state(enabled: bool, extra: &[String]) -> Result<Vec<PathBuf>, String> {
    let state = sandbox_runtime_state()
        .ok_or_else(|| "sandbox runtime state not recorded at startup".to_string())?;
    if !enabled {
        std::env::remove_var("INTENDANT_SANDBOX_WRITE_PATHS");
        return Ok(Vec::new());
    }
    let mut paths = state.base_write_paths.clone();
    for entry in extra {
        if let Some(path) = resolve_extra_write_path(entry, state.project_write_scope.as_deref()) {
            paths.push(path);
        }
    }
    paths.sort();
    paths.dedup();
    set_write_paths_env(&paths)?;
    Ok(paths)
}

/// Append one grant to the live env var (the "always allow" consent
/// resolution). No-op when the sandbox is off or the path is covered.
pub(crate) fn add_live_write_grant(path: &Path) -> Result<(), String> {
    if !sandbox_active() {
        return Ok(());
    }
    let mut paths = effective_write_paths();
    if path_within_grants(path, &paths) {
        return Ok(());
    }
    paths.push(path.to_path_buf());
    set_write_paths_env(&paths)
}

/// Platform-correct list encoding (':' on Unix, ';' on Windows) via
/// `env::join_paths`. A path containing the list separator cannot be
/// encoded; drop it loudly — the runtime then simply never allows writes
/// there (fail-closed).
pub(crate) fn set_write_paths_env(paths: &[PathBuf]) -> Result<(), String> {
    let encodable: Vec<&PathBuf> = paths
        .iter()
        .filter(|p| {
            let ok = std::env::join_paths([p]).is_ok();
            if !ok {
                eprintln!(
                    "[sandbox] write path {} contains the PATH separator and cannot \
                     be passed to the runtime; writes there will be denied",
                    p.display()
                );
            }
            ok
        })
        .collect();
    match std::env::join_paths(encodable) {
        Ok(joined) => {
            std::env::set_var("INTENDANT_SANDBOX_WRITE_PATHS", joined);
            Ok(())
        }
        Err(e) => Err(format!("failed to encode write paths: {e}")),
    }
}

/// True when `path` equals or sits beneath any granted path.
pub(crate) fn path_within_grants(path: &Path, grants: &[PathBuf]) -> bool {
    grants.iter().any(|grant| path.starts_with(grant))
}

/// Paths the consent flow must never OFFER to grant: the user-secret
/// directories and the credential files the sandbox exists to protect.
/// On Linux there is no deny layer under a grant (Landlock is
/// allowlist-only), so a grant here would genuinely open the material —
/// the consent card simply never proposes it.
pub(crate) fn grant_offer_forbidden(path: &Path) -> bool {
    if path
        .file_name()
        .is_some_and(|name| name.to_str().is_some_and(|n| n == ".env"))
    {
        return true;
    }
    let mut protected: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        let home = std::fs::canonicalize(&home).unwrap_or(home);
        protected.push(home.join(".ssh"));
        protected.push(home.join(".gnupg"));
    }
    if let Some(config) = dirs::config_dir() {
        protected.push(canonicalize_for_profile(&config.join("intendant")));
    }
    let candidate = canonicalize_for_profile(path);
    protected
        .iter()
        .any(|p| candidate.starts_with(p) || p.starts_with(&candidate))
}

/// The path a consent grant would actually cover for `denied`: the path
/// itself when it exists, else the nearest existing ancestor (grant
/// mechanisms cannot cover a not-yet-existing path — Landlock needs an
/// openable fd, Windows needs a stampable DACL). The card shows this
/// grant target verbatim, so a wide ancestor is visible before approval.
/// A filesystem root is never a target — granting it would be the sandbox
/// off in disguise, so those denials stay note-only.
pub(crate) fn grant_target_for(denied: &Path) -> Option<PathBuf> {
    if !denied.is_absolute() {
        return None;
    }
    let mut current = Some(denied);
    while let Some(path) = current {
        // Filesystem root ("/", "C:\") — never offered.
        path.parent()?;
        if path.exists() {
            return Some(path.to_path_buf());
        }
        current = path.parent();
    }
    None
}

/// Signature strings a sandbox write denial produces in tool output
/// across the three platforms (EACCES, Seatbelt's EPERM, Windows'
/// ACCESS_DENIED). Best-effort by construction — callers additionally
/// require an active sandbox and an out-of-grant path.
pub(crate) fn permission_denied_signature(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("access is denied")
}

/// Classify one tool result as a sandbox write denial worth a consent
/// offer. `file_path` is the structured target (`writeFile`/`editFile`);
/// exec results fall back to extracting a `<path>: Permission denied`
/// shape from the output. Returns the grant target. Pure given
/// `grants`; `sandbox_active` + dedup are the caller's job.
pub(crate) fn sandbox_denial_grant_offer(
    function: &str,
    file_path: Option<&str>,
    result_text: &str,
    grants: &[PathBuf],
) -> Option<PathBuf> {
    if !permission_denied_signature(result_text) {
        return None;
    }
    let denied: PathBuf = match function {
        "writeFile" | "editFile" => PathBuf::from(file_path?),
        "execAsAgent" | "execPty" => extract_denied_path(result_text)?,
        _ => return None,
    };
    if !denied.is_absolute() {
        return None;
    }
    let target = grant_target_for(&denied)?;
    if path_within_grants(&denied, grants) || path_within_grants(&target, grants) {
        // Denied inside the grant set = plain filesystem permissions
        // (root-owned file, read-only mount), not the sandbox.
        return None;
    }
    if grant_offer_forbidden(&denied) || grant_offer_forbidden(&target) {
        return None;
    }
    Some(target)
}

/// Extract the failing path from shell denial output like
/// `sh: /Users/x/file: Permission denied`,
/// `sh: 1: cannot create /target/f: Permission denied`, or
/// `mkdir: /target: Permission denied`.
fn extract_denied_path(text: &str) -> Option<PathBuf> {
    for line in text.lines() {
        if !permission_denied_signature(line) {
            continue;
        }
        for segment in line.split(':') {
            let trimmed = segment.trim().trim_matches(['\'', '"', '`']);
            // Prose prefixes ("cannot create /x") keep the path at the
            // first '/' of the segment.
            let candidate = match trimmed.find('/') {
                Some(idx) => &trimmed[idx..],
                None => trimmed,
            };
            let candidate = candidate.trim_end_matches(['\'', '"', '`', '.', ',']);
            let path = Path::new(candidate);
            if path.is_absolute() && candidate.len() > 1 {
                return Some(path.to_path_buf());
            }
        }
        // Windows drive-letter paths survive the ':' split as
        // "C" + "\path…" — rejoin heuristically, preferring the
        // single-quoted form PowerShell emits ("Access to the path
        // 'C:\x' is denied").
        #[cfg(windows)]
        {
            if let Some(idx) = line.find(":\\") {
                if idx >= 1 {
                    let start = idx - 1;
                    let tail = &line[start..];
                    let end = tail
                        .find('\'')
                        .or_else(|| tail.find(": "))
                        .or_else(|| tail.find(':').filter(|i| *i > 2))
                        .unwrap_or(tail.len());
                    let candidate = tail[..end].trim().trim_matches(['\'', '"']);
                    let path = Path::new(candidate);
                    if path.is_absolute() {
                        return Some(path.to_path_buf());
                    }
                }
            }
        }
    }
    None
}

/// Configuration for Landlock filesystem sandboxing.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SandboxConfig {
    /// Paths the sandboxed process may read.
    pub read_paths: Vec<PathBuf>,
    /// Paths the sandboxed process may write (implies read).
    pub write_paths: Vec<PathBuf>,
    /// Whether sandboxing is enabled.
    pub enabled: bool,
}

/// Resolve a toolchain home the way its tool does: the override
/// environment value when non-empty, else `<home>/<fallback>`. Pure so
/// tests inject values; the constructor edge resolves the live
/// environment.
fn env_or_home_dir(
    env_value: Option<std::ffi::OsString>,
    home: Option<&Path>,
    fallback: &str,
) -> Option<PathBuf> {
    if let Some(value) = env_value.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(value));
    }
    home.map(|home| home.join(fallback))
}

/// Build caches a standard dev workflow writes even when fully warm.
/// Without these grants the default-on write sandbox breaks ordinary
/// builds, which would push users toward `--no-sandbox` wholesale:
/// - cargo home: `cargo` takes its `.package-cache` lock on every
///   invocation, so even a fully cached `cargo build` needs the write.
/// - rustup home: a `rust-toolchain` pin triggers a toolchain install.
/// - the user cache dir (`~/.cache` on Linux, `~/Library/Caches` on
///   macOS): npm, pip, uv, pnpm and friends cache there.
///
/// Pure (injected roots) for hermetic tests; absent entries drop out.
fn toolchain_cache_write_paths(
    cargo_home: Option<PathBuf>,
    rustup_home: Option<PathBuf>,
    user_cache_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    cargo_home
        .into_iter()
        .chain(rustup_home)
        .chain(user_cache_dir)
        .collect()
}

#[allow(dead_code)]
impl SandboxConfig {
    /// Build a default config for the given project.
    /// - Read: `/` (everything)
    /// - Write: project root, the OS scratch dir(s), log directory, home
    ///   `.intendant`, and the toolchain caches
    ///   (`toolchain_cache_write_paths`)
    pub fn default_for_project(project_root: &Path, log_dir: &Path) -> Self {
        let mut config = Self::projectless(log_dir);
        config.write_paths.insert(0, project_root.to_path_buf());
        config
    }

    /// Default config for a daemon with **no project** (projectless): the
    /// same base scope as `default_for_project` minus the project root —
    /// scratch dirs, log dir, `~/.intendant`, and the toolchain caches
    /// only. Absence of a project must shrink the write scope, never
    /// widen it: in particular the daemon's launch cwd is *not* writable.
    pub fn projectless(log_dir: &Path) -> Self {
        // Scratch: the live platform temp dir (honors `TMPDIR`/`%TEMP%` —
        // on Linux there is no separate TMPDIR composition like the macOS
        // Seatbelt profile, so tempfile consumers need the live value
        // granted) plus the literal `/tmp` every Unix tool assumes.
        let mut write_paths = vec![std::env::temp_dir()];
        #[cfg(unix)]
        write_paths.push(PathBuf::from("/tmp"));
        write_paths.push(log_dir.to_path_buf());

        // Allow writes to the daemon state root (~/.intendant by default,
        // $INTENDANT_HOME when overridden).
        write_paths.push(crate::platform::intendant_home());

        // Toolchain caches (rationale on toolchain_cache_write_paths).
        // The user cache dir is skipped on Windows: it is %LOCALAPPDATA%
        // wholesale, far too broad for a default ACE grant, and %TEMP%
        // (granted above) already lives inside it.
        let home = dirs::home_dir();
        write_paths.extend(toolchain_cache_write_paths(
            env_or_home_dir(std::env::var_os("CARGO_HOME"), home.as_deref(), ".cargo"),
            env_or_home_dir(std::env::var_os("RUSTUP_HOME"), home.as_deref(), ".rustup"),
            if cfg!(windows) {
                None
            } else {
                dirs::cache_dir()
            },
        ));

        Self {
            read_paths: vec![PathBuf::from("/")],
            write_paths,
            enabled: true,
        }
    }

    /// Build a maximally restrictive config for untrusted live audio agents.
    /// - Read: `/` (for shared libraries, system config)
    /// - Write: ONLY the session log dir and quarantine dir
    /// - No project root, no /tmp, no ~/.intendant
    ///
    /// Note: currently for documentation/future use. In-process live audio
    /// tasks use code-level isolation (zero tools, restricted write paths)
    /// rather than process-level Landlock.
    pub fn untrusted_live_audio(session_log_dir: &Path, quarantine_dir: &Path) -> Self {
        Self {
            read_paths: vec![PathBuf::from("/")],
            write_paths: vec![session_log_dir.to_path_buf(), quarantine_dir.to_path_buf()],
            enabled: true,
        }
    }

    /// Generate a Seatbelt (sandbox-exec) profile mirroring this config's
    /// Landlock posture on macOS: reads stay open (`read_paths` is `/` for
    /// the agent runtime), writes are denied everywhere except
    /// `write_paths` plus the scratch locations every Unix process assumes
    /// (`/dev` tty nodes, `/tmp`, `/var/tmp`, the per-user `TMPDIR`).
    /// Seatbelt rules are last-match-wins and evaluate REAL paths, so
    /// write paths are canonicalized first — a rule on a symlinked root
    /// (`/tmp`, `/var`, `/etc`) would otherwise never match.
    #[cfg(target_os = "macos")]
    pub fn seatbelt_write_only_profile(&self) -> Result<String, String> {
        let mut write_literals: Vec<String> = Vec::new();
        for path in ["/dev", "/private/tmp", "/private/var/tmp"] {
            write_literals.push(seatbelt_path_literal(Path::new(path))?);
        }
        if let Ok(tmpdir) = std::env::var("TMPDIR") {
            let canonical =
                std::fs::canonicalize(&tmpdir).unwrap_or_else(|_| PathBuf::from(&tmpdir));
            write_literals.push(seatbelt_path_literal(&canonical)?);
        }
        for path in &self.write_paths {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            write_literals.push(seatbelt_path_literal(&canonical)?);
        }
        let subpaths = write_literals
            .iter()
            .map(|literal| format!("(subpath {literal})"))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(format!(
            "(version 1)\n\
             (allow default)\n\
             (deny file-write*)\n\
             (allow file-write* {subpaths})\n\
             {sensitive}{credential}",
            sensitive = seatbelt_sensitive_deny_clause()?,
            credential = seatbelt_credential_deny_clause()?,
        ))
    }

    /// Apply Landlock restrictions to the current process.
    /// Returns Ok(true) if restrictions were applied, Ok(false) if Landlock
    /// is not supported by the kernel, Err on actual errors.
    pub fn apply_to_current_process(&self) -> Result<bool, String> {
        if !self.enabled {
            return Ok(false);
        }

        #[cfg(target_os = "linux")]
        {
            use landlock::{
                AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
            };

            let abi = ABI::V5;

            let read_access = AccessFs::from_read(abi);
            let write_access = AccessFs::from_read(abi) | AccessFs::from_write(abi);

            let mut ruleset_created = Ruleset::default()
                .handle_access(write_access)
                .map_err(|e| format!("Landlock ruleset creation failed: {}", e))?
                .create()
                .map_err(|e| format!("Landlock ruleset create failed: {}", e))?;

            // Add read-only paths
            for path in &self.read_paths {
                if path.exists() {
                    if let Ok(fd) = PathFd::new(path) {
                        let rule = PathBeneath::new(fd, read_access);
                        ruleset_created = ruleset_created
                            .add_rule(rule)
                            .map_err(|e| format!("Landlock add read rule failed: {}", e))?;
                    }
                }
            }

            // Add read-write paths
            for path in &self.write_paths {
                if path.exists() {
                    if let Ok(fd) = PathFd::new(path) {
                        let rule = PathBeneath::new(fd, write_access);
                        ruleset_created = ruleset_created
                            .add_rule(rule)
                            .map_err(|e| format!("Landlock add write rule failed: {}", e))?;
                    }
                }
            }

            let status = ruleset_created
                .restrict_self()
                .map_err(|e| format!("Landlock restrict_self failed: {}", e))?;

            Ok(status.ruleset != landlock::RulesetStatus::NotEnforced)
        }

        #[cfg(not(target_os = "linux"))]
        {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_denied_signature_matches_all_three_platforms() {
        assert!(permission_denied_signature(
            "sh: /Users/x/file: Permission denied"
        ));
        assert!(permission_denied_signature(
            "Operation not permitted (os error 1)"
        ));
        assert!(permission_denied_signature(
            "Access is denied. (os error 5)"
        ));
        assert!(!permission_denied_signature("No such file or directory"));
    }

    #[test]
    fn grant_target_resolves_to_nearest_existing_ancestor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let existing = tmp.path().join("present.txt");
        std::fs::write(&existing, "x").unwrap();
        // Existing file: the grant covers exactly it.
        assert_eq!(grant_target_for(&existing), Some(existing.clone()));
        // Not-yet-existing file: the nearest existing ancestor.
        let fresh = tmp.path().join("newdir").join("new.txt");
        assert_eq!(grant_target_for(&fresh), Some(tmp.path().to_path_buf()));
        // Relative paths are never grant targets.
        assert_eq!(grant_target_for(Path::new("relative/x")), None);
        // A denial whose only existing ancestor is the filesystem root
        // gets no offer — granting "/" would be the sandbox off.
        #[cfg(unix)]
        assert_eq!(
            grant_target_for(Path::new("/intendant-nonexistent-zone/x.txt")),
            None
        );
    }

    #[test]
    fn grant_offers_never_cover_credential_paths() {
        if let Some(home) = dirs::home_dir() {
            assert!(grant_offer_forbidden(&home.join(".ssh")));
            assert!(grant_offer_forbidden(&home.join(".ssh").join("config")));
            assert!(grant_offer_forbidden(&home.join(".gnupg")));
        }
        if let Some(config) = dirs::config_dir() {
            assert!(grant_offer_forbidden(&config.join("intendant")));
            assert!(grant_offer_forbidden(
                &config.join("intendant").join(".env")
            ));
        }
        assert!(grant_offer_forbidden(Path::new("/some/project/.env")));
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(!grant_offer_forbidden(tmp.path()));
    }

    #[test]
    fn sandbox_denial_offer_classifies_structured_and_exec_results() {
        let tmp = tempfile::TempDir::new().unwrap();
        let granted = tmp.path().join("granted");
        std::fs::create_dir_all(&granted).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let grants = vec![granted.clone()];
        let outside_file = outside.join("f.txt");
        std::fs::write(&outside_file, "x").unwrap();

        // Structured write denied outside the grant set → offer the path.
        assert_eq!(
            sandbox_denial_grant_offer(
                "writeFile",
                Some(outside_file.to_str().unwrap()),
                "Permission denied (os error 13)",
                &grants,
            ),
            Some(outside_file.clone())
        );
        // Same path denied but inside the grant set: plain filesystem
        // permissions, not the sandbox — no offer.
        assert_eq!(
            sandbox_denial_grant_offer(
                "writeFile",
                Some(outside_file.to_str().unwrap()),
                "Permission denied (os error 13)",
                &[tmp.path().to_path_buf()],
            ),
            None
        );
        // Success output → no offer.
        assert_eq!(
            sandbox_denial_grant_offer(
                "writeFile",
                Some(outside_file.to_str().unwrap()),
                "wrote 12 bytes",
                &grants,
            ),
            None
        );
        // Credential paths never get an offer even when denied.
        assert_eq!(
            sandbox_denial_grant_offer(
                "writeFile",
                Some("/some/project/.env"),
                "Permission denied (os error 13)",
                &grants,
            ),
            None
        );
        // Exec output with the `<path>: Permission denied` shape.
        let exec_text = format!("sh: {}: Permission denied", outside_file.display());
        assert_eq!(
            sandbox_denial_grant_offer("execAsAgent", None, &exec_text, &grants),
            Some(outside_file.clone())
        );
        // Non-write tools never classify.
        assert_eq!(
            sandbox_denial_grant_offer(
                "inspectPath",
                Some(outside_file.to_str().unwrap()),
                "Permission denied",
                &grants,
            ),
            None
        );
    }

    #[test]
    fn extract_denied_path_reads_shell_error_shapes() {
        assert_eq!(
            extract_denied_path("bash: /Users/vm/.zshrc: Permission denied"),
            Some(PathBuf::from("/Users/vm/.zshrc"))
        );
        assert_eq!(
            extract_denied_path("mkdir: /denied-root: Permission denied"),
            Some(PathBuf::from("/denied-root"))
        );
        // dash/POSIX-sh prose shape.
        assert_eq!(
            extract_denied_path("sh: 1: cannot create /denied/f.txt: Permission denied"),
            Some(PathBuf::from("/denied/f.txt"))
        );
        // Seatbelt denials surface as EPERM.
        assert_eq!(
            extract_denied_path("sh: /denied/f.txt: Operation not permitted"),
            Some(PathBuf::from("/denied/f.txt"))
        );
        assert_eq!(
            extract_denied_path("some ordinary output\nno denial here"),
            None
        );
        // Relative path in the message: not a grantable target.
        assert_eq!(
            extract_denied_path("cat: file.txt: Permission denied"),
            None
        );
    }

    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_write_only_profile_embeds_canonical_write_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config = SandboxConfig {
            read_paths: vec![PathBuf::from("/")],
            write_paths: vec![project.clone()],
            enabled: true,
        };
        let profile = config.seatbelt_write_only_profile().unwrap();
        assert!(profile.contains("(allow default)"));
        assert!(profile.contains("(deny file-write*)"));
        // TempDir lives under the /var/folders symlink; the profile must
        // carry the real /private/var path or the rule would never match.
        let canonical = std::fs::canonicalize(&project).unwrap();
        assert!(
            profile.contains(&format!("(subpath \"{}\")", canonical.display())),
            "profile missing canonicalized project path: {profile}"
        );
        assert!(profile.contains("(subpath \"/dev\")"));
    }

    /// Run the generated profile through the real Seatbelt compiler and
    /// kernel: writes inside the configured path succeed, writes outside
    /// are denied, reads stay open — the Linux Landlock posture.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_write_only_profile_enforces_like_landlock() {
        let tmp = tempfile::TempDir::new().unwrap();
        let allowed = tmp.path().join("allowed");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let config = SandboxConfig {
            read_paths: vec![PathBuf::from("/")],
            write_paths: vec![allowed.clone()],
            enabled: true,
        };
        // TMPDIR is allowed wholesale in the profile (runtime scratch), and
        // TempDir lives under it — probe with TMPDIR pointed elsewhere so
        // the `outside` write exercises the deny rule.
        let profile = {
            let saved = std::env::var("TMPDIR").ok();
            std::env::remove_var("TMPDIR");
            let profile = config.seatbelt_write_only_profile().unwrap();
            if let Some(saved) = saved {
                std::env::set_var("TMPDIR", saved);
            }
            profile
        };
        let script = format!(
            "echo in > {allowed}/probe.txt && echo WRITE_IN_OK; \
             echo out > {outside}/probe.txt 2>/dev/null || echo WRITE_OUT_DENIED; \
             head -c 1 /etc/hosts > /dev/null && echo READ_OK",
            allowed = allowed.display(),
            outside = outside.display(),
        );
        let output = std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sandbox-exec runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("WRITE_IN_OK"), "{stdout} / {profile}");
        assert!(stdout.contains("WRITE_OUT_DENIED"), "{stdout}");
        assert!(stdout.contains("READ_OK"), "{stdout}");
        assert!(!outside.join("probe.txt").exists());
    }

    /// The always-on macOS runtime profile: user-secret directories are
    /// denied to the whole process tree — including plain shell commands,
    /// the executeCommand lane validate_path never sees — while normal
    /// reads and writes stay open. Probed through the real kernel.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_sensitive_only_profile_blocks_secret_dirs_for_exec() {
        let home = dirs::home_dir().expect("home dir");
        if !home.join(".ssh").exists() {
            eprintln!("skipping: no ~/.ssh on this machine");
            return;
        }
        let profile = seatbelt_sensitive_only_profile().unwrap();
        assert!(profile.contains("(allow default)"));
        let tmp = tempfile::TempDir::new().unwrap();
        let script = format!(
            "ls {ssh} 2>/dev/null && echo SSH_LISTED || echo SSH_DENIED; \
             echo w > {tmp}/w.txt && echo WRITE_OK; \
             head -c 1 /etc/hosts > /dev/null && echo READ_OK",
            ssh = home.join(".ssh").display(),
            tmp = tmp.path().display(),
        );
        let output = std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sandbox-exec runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("SSH_DENIED"), "{stdout} / {profile}");
        assert!(stdout.contains("WRITE_OK"), "{stdout}");
        assert!(stdout.contains("READ_OK"), "{stdout}");
    }

    /// The write-only profile carries the sensitive AND credential deny
    /// clauses appended last, so ~/.ssh and the `.env` walk stay denied
    /// even when a configured write path (e.g. a project rooted at $HOME)
    /// would otherwise cover them.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_write_only_profile_keeps_secrets_denied_inside_write_paths() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let config = SandboxConfig {
            read_paths: vec![PathBuf::from("/")],
            write_paths: vec![home.clone()],
            enabled: true,
        };
        let profile = config.seatbelt_write_only_profile().unwrap();
        let canonical_home = std::fs::canonicalize(&home).unwrap_or(home);
        let allow_line_idx = profile
            .lines()
            .position(|line| line.starts_with("(allow file-write*"))
            .expect("write-path allow present");
        let deny_lines: Vec<(usize, &str)> = profile
            .lines()
            .enumerate()
            .filter(|(_, line)| line.starts_with("(deny file-read* file-write*"))
            .collect();
        assert!(
            deny_lines
                .iter()
                .any(|(_, line)| line.contains(&format!("{}/.ssh", canonical_home.display()))),
            "{profile}"
        );
        assert!(
            deny_lines.iter().any(|(_, line)| line.contains(".env")),
            "{profile}"
        );
        // Appended last: every deny clause follows the write-path allow
        // that covers $HOME, and the profile ends on one.
        assert!(deny_lines.iter().all(|(idx, _)| *idx > allow_line_idx));
        assert!(profile
            .trim_end()
            .lines()
            .last()
            .unwrap()
            .starts_with("(deny file-read* file-write*"));
    }

    /// The composed deny clause is enforced by the real Seatbelt kernel:
    /// a denied directory and a denied `.env` literal are unreadable and
    /// unwritable inside an `(allow default)` profile, a *nonexistent*
    /// denied literal cannot be created, and unrelated files stay
    /// readable. Hermetic — probes an injected tempdir, not live state.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_deny_clause_for_blocks_denied_paths_via_kernel() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let config_home = root.join("config-home");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(config_home.join("cred.txt"), "k").unwrap();
        let env_file = root.join(".env");
        std::fs::write(&env_file, "KEY=value").unwrap();
        let absent_env = root.join("fresh").join(".env");
        std::fs::create_dir_all(absent_env.parent().unwrap()).unwrap();
        let readable = root.join("readable.txt");
        std::fs::write(&readable, "ok").unwrap();

        let clause = seatbelt_deny_clause_for(
            std::slice::from_ref(&config_home),
            &[env_file.clone(), absent_env.clone()],
        )
        .unwrap();
        let profile = format!("(version 1)\n(allow default)\n{clause}");
        let script = format!(
            "cat {env} 2>/dev/null && echo ENV_READ || echo ENV_DENIED; \
             cat {cred} 2>/dev/null && echo CRED_READ || echo CRED_DENIED; \
             echo x >> {env} 2>/dev/null && echo ENV_WRITE || echo ENV_WRITE_DENIED; \
             echo x > {absent} 2>/dev/null && echo ABSENT_CREATED || echo ABSENT_DENIED; \
             cat {readable} > /dev/null && echo OTHER_READ_OK",
            env = env_file.display(),
            cred = config_home.join("cred.txt").display(),
            absent = absent_env.display(),
            readable = readable.display(),
        );
        let output = std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sandbox-exec runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("ENV_DENIED"), "{stdout} / {profile}");
        assert!(stdout.contains("CRED_DENIED"), "{stdout}");
        assert!(stdout.contains("ENV_WRITE_DENIED"), "{stdout}");
        assert!(stdout.contains("ABSENT_DENIED"), "{stdout}");
        assert!(stdout.contains("OTHER_READ_OK"), "{stdout}");
        assert!(!absent_env.exists());
    }

    /// An empty filter set must compose to an empty clause — a filterless
    /// `(deny file-read* file-write*)` would deny everything.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_deny_clause_for_empty_input_is_empty() {
        assert_eq!(seatbelt_deny_clause_for(&[], &[]).unwrap(), "");
    }

    /// The live sensitive clause carries the credential-file coverage: the
    /// intendant config home and the `.env` walk over the launch cwd and
    /// its ancestors (the dotenvy search path, which includes the project
    /// root).
    #[cfg(target_os = "macos")]
    #[test]
    fn sensitive_deny_clause_covers_credential_files() {
        // The credential clause carries the config home + .env walk; the
        // always-on sensitive clause deliberately does NOT (the
        // --no-sandbox opt-out restores agent access to the project .env).
        let sensitive = seatbelt_sensitive_deny_clause().unwrap();
        assert!(!sensitive.contains(".env"), "{sensitive}");
        let clause = seatbelt_credential_deny_clause().unwrap();
        if let Some(config) = dirs::config_dir() {
            let config = canonicalize_for_profile(&config.join("intendant"));
            assert!(
                clause.contains(&format!("(subpath \"{}\")", config.display())),
                "{clause}"
            );
        }
        let cwd = canonicalize_for_profile(&std::env::current_dir().unwrap());
        for candidate in env_file_candidates(&cwd) {
            let literal = canonicalize_for_profile(&candidate);
            assert!(
                clause.contains(&format!("(literal \"{}\")", literal.display())),
                "missing {} in {clause}",
                literal.display()
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn env_file_candidates_walk_covers_ancestors() {
        let candidates = env_file_candidates(Path::new("/a/b/c"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/a/b/c/.env"),
                PathBuf::from("/a/b/.env"),
                PathBuf::from("/a/.env"),
                PathBuf::from("/.env"),
            ]
        );
    }

    #[test]
    fn env_or_home_dir_prefers_nonempty_override() {
        let home = Path::new("/home/user");
        assert_eq!(
            env_or_home_dir(
                Some(std::ffi::OsString::from("/custom/cargo")),
                Some(home),
                ".cargo"
            ),
            Some(PathBuf::from("/custom/cargo"))
        );
        // Empty override behaves like unset (matching cargo/rustup).
        assert_eq!(
            env_or_home_dir(Some(std::ffi::OsString::new()), Some(home), ".cargo"),
            Some(PathBuf::from("/home/user/.cargo"))
        );
        assert_eq!(
            env_or_home_dir(None, Some(home), ".rustup"),
            Some(PathBuf::from("/home/user/.rustup"))
        );
        assert_eq!(env_or_home_dir(None, None, ".cargo"), None);
    }

    #[test]
    fn toolchain_cache_write_paths_drops_absent_entries() {
        assert!(toolchain_cache_write_paths(None, None, None).is_empty());
        let paths = toolchain_cache_write_paths(
            Some(PathBuf::from("/home/user/.cargo")),
            None,
            Some(PathBuf::from("/home/user/.cache")),
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/user/.cargo"),
                PathBuf::from("/home/user/.cache"),
            ]
        );
    }

    #[test]
    fn projectless_config_grants_the_live_temp_dir() {
        // TMPDIR-honoring scratch: tempfile consumers must stay inside the
        // grant set even when TMPDIR points away from /tmp.
        let config = SandboxConfig::projectless(Path::new("/tmp/logs"));
        assert!(config.write_paths.contains(&std::env::temp_dir()));
    }

    #[test]
    fn default_config_includes_project_and_tmp() {
        let config = SandboxConfig::default_for_project(
            Path::new("/home/user/project"),
            Path::new("/tmp/logs"),
        );
        assert!(config.enabled);
        assert!(config
            .write_paths
            .contains(&PathBuf::from("/home/user/project")));
        let scratch = if cfg!(windows) {
            std::env::temp_dir()
        } else {
            PathBuf::from("/tmp")
        };
        assert!(config.write_paths.contains(&scratch));
        assert!(config.write_paths.contains(&PathBuf::from("/tmp/logs")));
        assert!(config.read_paths.contains(&PathBuf::from("/")));
    }

    #[test]
    fn projectless_config_is_the_project_config_minus_the_project_root() {
        let log_dir = Path::new("/tmp/logs");
        let projectless = SandboxConfig::projectless(log_dir);
        let mut with_project =
            SandboxConfig::default_for_project(Path::new("/home/user/project"), log_dir);
        // Exactly one path apart: the project root. No cwd, no widening.
        assert!(!projectless
            .write_paths
            .contains(&PathBuf::from("/home/user/project")));
        with_project
            .write_paths
            .retain(|p| p != Path::new("/home/user/project"));
        assert_eq!(projectless.write_paths, with_project.write_paths);
        assert!(projectless.enabled);
    }

    #[test]
    fn disabled_config_skips_apply() {
        let mut config =
            SandboxConfig::default_for_project(Path::new("/tmp/test"), Path::new("/tmp/logs"));
        config.enabled = false;
        assert_eq!(config.apply_to_current_process().unwrap(), false);
    }

    #[test]
    fn config_has_write_paths() {
        let config = SandboxConfig::default_for_project(
            Path::new("/home/user/myproject"),
            Path::new("/var/log/intendant"),
        );
        assert!(config.write_paths.len() >= 3);
    }
}

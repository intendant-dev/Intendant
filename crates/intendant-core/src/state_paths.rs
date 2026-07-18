//! State-path seam: the daemon home (`~/.intendant` or
//! `$INTENDANT_HOME`) and the explicit-home variant every
//! `home: &Path`-parameterized helper routes through. Lives in core so
//! both the platform crate and content modules (skills) can reach it
//! without a dependency cycle.
//!
//! Also home to the private-permissions helpers for state under that root
//! ([`create_private_dir_all`], [`private_file_options`],
//! [`write_private_file`]): daemon state is single-user by design, so
//! directories are created 0700 and files 0600 on Unix — at creation,
//! never write-then-chmod. Windows relies on the profile directory's
//! default ACLs.

/// Resolve the current user's home directory as a `PathBuf`.
///
/// This is the single source of truth for "where does `~/.intendant` and
/// `~/.codex` live" across the caller. It exists because the historical
/// `std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())` pattern is
/// Unix-only: Windows does not set `HOME`, so that pattern silently resolved
/// to `C:\tmp`, while external agents (Codex, Claude Code, Gemini) and the
/// session-log writer use the *real* user profile (`C:\Users\<user>`). The
/// mismatch meant the dashboard scanned the wrong directory on Windows and
/// never discovered standalone external-agent sessions.
///
/// - **Unix/macOS**: preserve the exact prior behavior — honor `$HOME` first
///   (so test overrides via `set_var("HOME", ...)` keep working), then fall
///   back to `/tmp` when it is unset.
/// - **Windows**: prefer `%USERPROFILE%`, then the platform-resolved home
///   (`dirs::home_dir()`, which also consults `USERPROFILE`/`HOMEDRIVE`),
///   then `$HOME` if a Unix-style env was injected, and only `C:\tmp` as a
///   last resort to mirror the Unix fallback.
pub fn home_dir() -> std::path::PathBuf {
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
    }
    #[cfg(windows)]
    {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            if !profile.trim().is_empty() {
                return std::path::PathBuf::from(profile);
            }
        }
        if let Some(home) = dirs::home_dir() {
            return home;
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return std::path::PathBuf::from(home);
            }
        }
        std::path::PathBuf::from("C:\\tmp")
    }
}

/// The Intendant state root — where the daemon keeps session logs, the
/// session-index cache, recordings, quarantine, leased credentials, and most
/// other machine-local daemon state. It is `~/.intendant` by default.
///
/// Known platform/product exceptions do not use this seam: Windows keeps its
/// access-certificate/IAM store under the OS data directory, the durable
/// daemon identity key uses the OS data directory on every platform, and the
/// current macOS durable Memory plane still hard-codes
/// `~/.intendant/memory-plane`.
///
/// `$INTENDANT_HOME` overrides the root for the whole process (scratch
/// daemons, hermetic harnesses, packaged installs): an absolute value is
/// used verbatim as the state root — no `.intendant` component is appended —
/// and a relative value resolves against the current directory at first use.
///
/// The resolution is computed **once at first use** and cached for the
/// process lifetime (a state root that moved mid-process would split daemon
/// state across two trees), so mutating `INTENDANT_HOME` after startup has
/// no effect. Tests must thread explicit paths instead of mutating the
/// environment (which races the parallel test runner anyway).
///
/// In THIS CRATE's unit-test builds (`cfg(test)`) the unset-`INTENDANT_HOME`
/// default swaps from the live `~/.intendant` to a per-instance scratch
/// root under the OS temp dir: unit tests exercising call-time defaults
/// must never read or write the developer's real daemon state (fixture
/// session rows used to pollute the live dashboard, and tests observed the
/// live daemon's concurrent writes — a flake class).
///
/// IMPORTANT LIMIT: `cfg(test)` does not cross crates. When a DEPENDENT
/// crate's test binary (e.g. the intendant bin's 2.5k unit tests) is
/// built, this crate compiles WITHOUT `cfg(test)` and the default here is
/// the live home. Dependent crates therefore get no ambient protection
/// from this branch — their tests must thread explicit `home`/`_in(path)`
/// parameters (the seam's primary convention; see
/// `diagnostics::append_visual_freshness_record_in` for the pattern and
/// the incident that proved the gap). The scratch root keeps a trailing
/// `.intendant` component so shape-sensitive code
/// (`external_wrapper_index::home_from_log_dir`) parses it like a real
/// home's state root. This mirrors the `credential_audit::trail_path`
/// precedent, now subsumed by this seam.
pub fn intendant_home() -> std::path::PathBuf {
    static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root =
            intendant_home_override(std::env::var_os("INTENDANT_HOME")).unwrap_or_else(|| {
                #[cfg(test)]
                {
                    // PID alone is NOT unique across runs: busy CI boxes recycle
                    // PIDs fast, and a recycled PID inherits a previous test
                    // process's scratch home — stale state files included (seen
                    // live: a diagnostics append test read a prior run's records
                    // on the loaded Linux runner). A startup-time nanos component
                    // makes the root unique per process INSTANCE; OnceLock keeps
                    // it stable within the process.
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    std::env::temp_dir()
                        .join(format!(
                            "intendant-test-home-{}-{nanos}",
                            std::process::id()
                        ))
                        .join(".intendant")
                }
                #[cfg(not(test))]
                {
                    home_dir().join(".intendant")
                }
            });
        // Create the root owner-only at first resolution: every piece of
        // daemon state (session logs, leases, certs, quarantine) lives
        // under this directory, and the scattered `create_dir_all` callers
        // would otherwise mint it with the umask default (0755). An
        // existing root is left untouched; failure is ignored — read-only
        // and sandboxed callers may lack write access, and writers surface
        // their own errors.
        let _ = create_private_dir_all(&root);
        root
    })
    .clone()
}

/// Create `dir` and any missing ancestors owner-only: mode 0700 on Unix
/// (applied to every directory this call creates), default profile ACLs on
/// Windows. Succeeds if `dir` already exists — like
/// [`std::fs::create_dir_all`], and existing directories keep their
/// permissions.
pub fn create_private_dir_all(dir: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// `OpenOptions` preset for state files: `write` + `create`, with mode
/// 0600 on Unix so a file is owner-only from the moment it exists (never
/// write-then-chmod). The mode applies only when the open actually
/// creates the file — pair with `append`/`truncate` as the call site
/// needs, and use [`write_private_file`] when replacing a whole file.
pub fn private_file_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// `std::fs::write` twin for private state files: the file is created
/// 0600 on Unix. Because the mode only applies at creation, any
/// pre-existing file is removed first so a copy written before this
/// hardening (or otherwise loosened) cannot keep its old bits.
pub fn write_private_file(
    path: &std::path::Path,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    #[cfg(unix)]
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    use std::io::Write as _;
    let mut file = private_file_options().truncate(true).open(path)?;
    file.write_all(contents.as_ref())
}

/// Interpret an `INTENDANT_HOME` value: absolute paths pass through,
/// relative ones resolve against the current directory, unset/empty means
/// "no override". Split from [`intendant_home`] so tests can pin every
/// branch without racing the parallel runner over process-global env.
fn intendant_home_override(raw: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    let raw = raw?;
    if raw.is_empty() {
        return None;
    }
    let path = std::path::PathBuf::from(raw);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(
            std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join(path),
        )
    }
}

/// The state root for an explicit `home`: `<home>/.intendant`, except that
/// the process's own home routes through [`intendant_home`] so the
/// `$INTENDANT_HOME` override (and the unit-test scratch default) is
/// honored. This is the seam for the `home: &Path`-parameterized helpers
/// (session catalog, wrapper index, session names/config): an explicit
/// alternate home — a test tempdir, a browsed peer home — stays scoped to
/// that home, and only the daemon's own home picks up the process override.
/// Same convention as `backend_lists::codex_dir` with `CODEX_HOME`.
pub fn intendant_home_in(home: &std::path::Path) -> std::path::PathBuf {
    if home == home_dir() {
        intendant_home()
    } else {
        home.join(".intendant")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_dir_is_nonempty() {
        assert!(!home_dir().as_os_str().is_empty());
    }

    #[test]
    fn intendant_home_override_absolute_passes_through() {
        let abs = if cfg!(windows) {
            std::path::PathBuf::from("C:\\scratch\\state")
        } else {
            std::path::PathBuf::from("/scratch/state")
        };
        assert_eq!(
            intendant_home_override(Some(abs.clone().into_os_string())),
            Some(abs)
        );
    }

    #[test]
    fn intendant_home_override_relative_resolves_against_cwd() {
        let resolved = intendant_home_override(Some("scratch-state".into())).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("scratch-state"));
    }

    #[test]
    fn intendant_home_override_unset_and_empty_mean_no_override() {
        assert_eq!(intendant_home_override(None), None);
        assert_eq!(intendant_home_override(Some("".into())), None);
    }

    /// In unit-test builds the unset default is the per-process scratch
    /// root, never the live `~/.intendant` — the property the whole seam
    /// exists to guarantee. (The `INTENDANT_HOME` env branch itself is
    /// pinned via `intendant_home_override` above; the prod default is
    /// covered behaviorally by the e2e suite's fake-home daemons.)
    #[test]
    fn intendant_home_in_tests_is_process_scratch_not_live_home() {
        let root = intendant_home();
        assert!(root.starts_with(std::env::temp_dir()));
        assert!(!root.starts_with(home_dir().join(".intendant")));
        // `.intendant`-shaped tail, so shape-walking resolvers
        // (external_wrapper_index::home_from_log_dir) treat it like a
        // real home's state root.
        assert!(root.ends_with(".intendant"));
        // Cached: two reads agree.
        assert_eq!(root, intendant_home());
    }

    #[test]
    fn intendant_home_in_scopes_explicit_homes_but_overrides_process_home() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(intendant_home_in(tmp.path()), tmp.path().join(".intendant"));
        assert_eq!(intendant_home_in(&home_dir()), intendant_home());
    }

    /// First resolution creates the root (the scratch root, in this
    /// crate's test build) — owner-only on Unix.
    #[test]
    fn intendant_home_is_created_private_on_first_resolution() {
        let root = intendant_home();
        assert!(root.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_helpers_create_owner_only_paths() {
        use std::os::unix::fs::PermissionsExt;
        let mode_of =
            |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let tmp = tempfile::tempdir().unwrap();

        // Every directory the call creates is 0700; existing dirs are fine.
        let dir = tmp.path().join("outer").join("inner");
        create_private_dir_all(&dir).unwrap();
        assert_eq!(mode_of(&tmp.path().join("outer")), 0o700);
        assert_eq!(mode_of(&dir), 0o700);
        create_private_dir_all(&dir).unwrap();

        // Truncate-write path: 0600 from creation, and a loosened
        // pre-existing file is recreated private rather than kept.
        let file = dir.join("secret");
        write_private_file(&file, b"x").unwrap();
        assert_eq!(mode_of(&file), 0o600);
        assert_eq!(std::fs::read(&file).unwrap(), b"x");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private_file(&file, b"y").unwrap();
        assert_eq!(mode_of(&file), 0o600);
        assert_eq!(std::fs::read(&file).unwrap(), b"y");

        // Append-create path (session/transcript logs): also 0600.
        let log = dir.join("log.jsonl");
        {
            use std::io::Write as _;
            let mut f = private_file_options().append(true).open(&log).unwrap();
            f.write_all(b"1\n").unwrap();
        }
        assert_eq!(mode_of(&log), 0o600);
    }

    #[cfg(not(unix))]
    #[test]
    fn private_helpers_still_create_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("outer").join("inner");
        create_private_dir_all(&dir).unwrap();
        assert!(dir.is_dir());
        let file = dir.join("secret");
        write_private_file(&file, b"x").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"x");
    }
}

//! State-path seam: the daemon home (`~/.intendant` or
//! `$INTENDANT_HOME`) and the explicit-home variant every
//! `home: &Path`-parameterized helper routes through. Lives in core so
//! both the platform crate and content modules (skills) can reach it
//! without a dependency cycle.

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
/// session-index cache, recordings, quarantine, leased credentials, access
/// certs, and every other piece of machine-local daemon state.
/// `~/.intendant` by default.
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
        })
    })
    .clone()
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
}

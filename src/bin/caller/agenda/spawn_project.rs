//! Project-root resolution for agenda-spawned sessions.
//!
//! The scheduled lane never launches a session project-less: a spawn on a
//! daemon without a default project used to die instantly with the
//! structured `no_project` create failure (live QA, 2026-07-21). Every
//! spawn now resolves a concrete project root *before* dispatch, in the
//! ratified order:
//!
//! 1. the manifest's explicit `project_root` (the confirm sheet's pick,
//!    validated at mint time and re-checked at fire time),
//! 2. the **parking session's** recorded project root (item provenance →
//!    session record → `session_meta.json`, with the external-wrapper
//!    index as the fallback for pruned wrapper log dirs),
//! 3. the daemon's default project root,
//! 4. otherwise: a named refusal — never a dead session.
//!
//! Everything here takes its roots as parameters (`home`, the context
//! struct) so tests stay hermetic; the wiring edge resolves the real
//! environment once at daemon startup.

use std::path::{Path, PathBuf};

/// Daemon-level facts the agenda needs to resolve spawn projects: the
/// state home (session records live under it) and the daemon's default
/// project root, if it has one. Constructed once at wiring; tests inject
/// temp dirs. The default context ([`AgendaHandle::new`] without
/// `with_spawn_context`) resolves nothing — it never reads the real home.
#[derive(Debug, Clone)]
pub(crate) struct SessionSpawnContext {
    /// The user home the daemon's session records resolve under (the
    /// `intendant_home_in` base), NOT the intendant state dir itself.
    pub(crate) home: PathBuf,
    /// The daemon's default project root (`None` on a projectless daemon).
    pub(crate) default_project_root: Option<PathBuf>,
}

/// Where a resolved spawn project came from — surfaced in refusals, item
/// write-backs, and the confirm sheet's honesty line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnProjectSource {
    /// Explicit pick (the sheet's confirmed parameter / `--project`).
    Explicit,
    /// Inherited from the parking session's recorded project root.
    Provenance,
    /// The daemon's default project root.
    DaemonDefault,
}

impl SpawnProjectSource {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Provenance => "provenance",
            Self::DaemonDefault => "daemon_default",
        }
    }
}

/// The recorded project root of one session id (native log dir or external
/// wrapper), or `None` when nothing on this daemon resolves it. Reads the
/// session's own `session_meta.json` first; a pruned wrapper log dir falls
/// back to the external-wrapper index record. Purely a lookup — existence
/// of the returned directory is the caller's check.
pub(crate) fn recorded_session_project_root(home: &Path, session_id: &str) -> Option<PathBuf> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    let meta_path = crate::platform::intendant_home_in(home)
        .join("logs")
        .join(session_id)
        .join("session_meta.json");
    if let Ok(raw) = std::fs::read_to_string(&meta_path) {
        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(root) = meta
                .get("project_root")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|root| !root.is_empty())
            {
                return Some(PathBuf::from(root));
            }
        }
    }
    // Wrapper whose own log dir is gone (superseded incarnation, pruned
    // logs): the raw index still carries the project root the wrapper
    // recorded — deliberately not dir-filtered, like the conversation
    // resolution provenance rides on.
    crate::external_wrapper_index::recorded_project_root_for_wrapper(home, session_id)
        .map(PathBuf::from)
}

fn usable_project_dir(path: &Path) -> bool {
    path.is_absolute() && path.is_dir()
}

/// Resolve the project a spawned session runs under, in the ratified
/// order (explicit → provenance → daemon default), or refuse with a
/// message naming exactly what is missing. An explicit pick must be an
/// absolute existing directory — it is the reviewed statement and never
/// silently substituted; the fallback lanes skip roots that no longer
/// exist (a deleted worktree checkout) instead of dying on them.
pub(crate) fn resolve_spawn_project(
    explicit: Option<&str>,
    provenance_session_id: Option<&str>,
    ctx: &SessionSpawnContext,
) -> Result<(PathBuf, SpawnProjectSource), String> {
    if let Some(raw) = explicit.map(str::trim).filter(|raw| !raw.is_empty()) {
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            return Err(format!(
                "project_root must be an absolute path (got {raw:?})"
            ));
        }
        if !path.is_dir() {
            return Err(format!(
                "project_root {raw:?} is not an existing directory on this daemon"
            ));
        }
        return Ok((path, SpawnProjectSource::Explicit));
    }
    if let Some(root) = provenance_session_id
        .and_then(|sid| recorded_session_project_root(&ctx.home, sid))
        .filter(|root| usable_project_dir(root))
    {
        return Ok((root, SpawnProjectSource::Provenance));
    }
    if let Some(root) = ctx
        .default_project_root
        .as_deref()
        .filter(|root| usable_project_dir(root))
    {
        return Ok((root.to_path_buf(), SpawnProjectSource::DaemonDefault));
    }
    Err(no_spawn_project_reason(provenance_session_id.is_some()))
}

/// The named refusal for an unresolvable spawn project. States exactly
/// what is missing so the fix is obvious from any surface.
pub(crate) fn no_spawn_project_reason(had_provenance: bool) -> String {
    let provenance = if had_provenance {
        "the parking session recorded no usable project root"
    } else {
        "the item has no parking-session provenance"
    };
    format!(
        "no project for the session: {provenance} and this daemon runs without a \
         default project — pick a project directory (the Start-now sheet's Project \
         field, or `--project` on ctl)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_meta(home: &Path, session_id: &str, meta: serde_json::Value) {
        let dir = crate::platform::intendant_home_in(home)
            .join("logs")
            .join(session_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session_meta.json"), meta.to_string()).unwrap();
    }

    fn ctx(home: &Path, default: Option<&Path>) -> SessionSpawnContext {
        SessionSpawnContext {
            home: home.to_path_buf(),
            default_project_root: default.map(Path::to_path_buf),
        }
    }

    /// The ratified order end to end: explicit beats provenance beats the
    /// daemon default; a projectless daemon with no provenance refuses
    /// with the message naming both missing pieces.
    #[test]
    fn resolution_order_explicit_provenance_default_refusal() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let default = tempfile::tempdir().unwrap();
        let explicit = tempfile::tempdir().unwrap();
        write_meta(
            home.path(),
            "sess-parker",
            serde_json::json!({
                "session_id": "sess-parker",
                "created_at": "now",
                "project_root": project.path().to_string_lossy(),
            }),
        );

        let full = ctx(home.path(), Some(default.path()));
        // Explicit wins over everything.
        let (root, source) = resolve_spawn_project(
            Some(&explicit.path().to_string_lossy()),
            Some("sess-parker"),
            &full,
        )
        .unwrap();
        assert_eq!(root, explicit.path());
        assert_eq!(source, SpawnProjectSource::Explicit);
        // Provenance beats the default.
        let (root, source) = resolve_spawn_project(None, Some("sess-parker"), &full).unwrap();
        assert_eq!(root, project.path());
        assert_eq!(source, SpawnProjectSource::Provenance);
        // No provenance ⇒ the daemon default.
        let (root, source) = resolve_spawn_project(None, None, &full).unwrap();
        assert_eq!(root, default.path());
        assert_eq!(source, SpawnProjectSource::DaemonDefault);
        // Projectless daemon, no provenance ⇒ named refusal.
        let err = resolve_spawn_project(None, None, &ctx(home.path(), None)).unwrap_err();
        assert!(err.contains("no project for the session"), "{err}");
        assert!(err.contains("no parking-session provenance"), "{err}");
        // Projectless daemon, provenance session unknown ⇒ refusal names
        // the unusable provenance instead.
        let err =
            resolve_spawn_project(None, Some("sess-gone"), &ctx(home.path(), None)).unwrap_err();
        assert!(err.contains("no usable project root"), "{err}");
    }

    /// Explicit picks are the reviewed statement: relative or missing
    /// paths refuse instead of falling through to a substitute.
    #[test]
    fn explicit_pick_is_validated_never_substituted() {
        let home = tempfile::tempdir().unwrap();
        let default = tempfile::tempdir().unwrap();
        let full = ctx(home.path(), Some(default.path()));
        let err = resolve_spawn_project(Some("relative/path"), None, &full).unwrap_err();
        assert!(err.contains("absolute"), "{err}");
        let missing = default.path().join("never-created");
        let err = resolve_spawn_project(Some(&missing.to_string_lossy()), None, &full).unwrap_err();
        assert!(err.contains("not an existing directory"), "{err}");
    }

    /// A provenance root that no longer exists (deleted worktree) is
    /// skipped, not fatal: the daemon default carries the spawn.
    #[test]
    fn stale_provenance_root_falls_through_to_default() {
        let home = tempfile::tempdir().unwrap();
        let default = tempfile::tempdir().unwrap();
        write_meta(
            home.path(),
            "sess-worktree",
            serde_json::json!({
                "session_id": "sess-worktree",
                "created_at": "now",
                "project_root": home.path().join("deleted-worktree").to_string_lossy(),
            }),
        );
        let (root, source) = resolve_spawn_project(
            None,
            Some("sess-worktree"),
            &ctx(home.path(), Some(default.path())),
        )
        .unwrap();
        assert_eq!(root, default.path());
        assert_eq!(source, SpawnProjectSource::DaemonDefault);
    }

    /// Wrapper fallback: a parking wrapper whose log dir was pruned still
    /// resolves through the external-wrapper index record.
    #[test]
    fn pruned_wrapper_resolves_project_root_via_the_index() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let wrap = crate::platform::intendant_home_in(home.path())
            .join("logs")
            .join("sess-wrap");
        std::fs::create_dir_all(&wrap).unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "claude-code",
            "conv-1",
            "sess-wrap",
            &wrap,
            Some(project.path()),
        )
        .unwrap();
        std::fs::remove_dir_all(&wrap).unwrap();
        assert_eq!(
            recorded_session_project_root(home.path(), "sess-wrap"),
            Some(project.path().to_path_buf())
        );
        // And the full resolver treats it as provenance.
        let (root, source) =
            resolve_spawn_project(None, Some("sess-wrap"), &ctx(home.path(), None)).unwrap();
        assert_eq!(root, project.path());
        assert_eq!(source, SpawnProjectSource::Provenance);
    }
}

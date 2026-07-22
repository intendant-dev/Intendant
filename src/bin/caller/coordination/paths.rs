//! Space-dir resolution (Track C, C1): one seam decides which
//! coordination space a process writes into.
//!
//! Default derivation is `<intendant-home>/coordination/<space-key>`
//! with the worktree-normalized key from `space_key`. The
//! `INTENDANT_COORDINATION_DIR` override names a space dir directly —
//! the parent exports it to sub-agent / external-agent children so an
//! isolated worktree child lands in the PARENT's space (worktree
//! normalization already agrees for same-repo worktrees; the override
//! covers detached temp clones and deliberate space grouping). Env is
//! read only at the process edge (`env_override`); everything below
//! takes explicit paths (the repo's hermeticity rule).
use std::path::{Path, PathBuf};

pub(crate) const COORDINATION_DIR_ENV: &str = "INTENDANT_COORDINATION_DIR";

/// The coordination root under a resolved intendant home (the
/// `~/.intendant` directory itself, already override-aware upstream).
pub(crate) fn coordination_root(intendant_home: &Path) -> PathBuf {
    intendant_home.join("coordination")
}

/// Derived space dir + key for a project root.
pub(crate) fn space_dir_under(intendant_home: &Path, project_root: &Path) -> (PathBuf, String) {
    let key = super::space_key(project_root);
    (coordination_root(intendant_home).join(&key), key)
}

/// Resolution order: explicit override (already read from env at the
/// edge) wins; otherwise derive. The space label for an override is
/// its basename, sanitized only if it strays outside the grammar
/// (space-key output can exceed `sanitize_key`'s 64-char clamp, so a
/// well-formed key must pass through untouched).
pub(crate) fn resolve_space_dir(
    override_dir: Option<&Path>,
    intendant_home: &Path,
    project_root: &Path,
) -> (PathBuf, String) {
    match override_dir {
        Some(dir) => {
            let raw = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let grammar_ok = !raw.is_empty()
                && raw.len() <= 96
                && raw
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            let label = if grammar_ok {
                raw
            } else {
                super::sanitize_key(&raw)
            };
            (dir.to_path_buf(), label)
        }
        None => space_dir_under(intendant_home, project_root),
    }
}

/// The process-edge env read. Tests never touch this — they pass
/// explicit overrides to `resolve_space_dir`.
pub(crate) fn env_override() -> Option<PathBuf> {
    std::env::var_os(COORDINATION_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_and_override_agree_on_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("state");
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        let (derived, key) = resolve_space_dir(None, &home, &project);
        assert_eq!(derived, home.join("coordination").join(&key));
        assert!(key.starts_with("proj-"), "{key}");

        // Override wins wholesale and reuses its basename as the label.
        let (dir, label) = resolve_space_dir(Some(&derived), &home, tmp.path());
        assert_eq!(dir, derived);
        assert_eq!(label, key, "well-formed key passes through unclamped");
    }

    #[test]
    fn hostile_override_basename_is_sanitized() {
        let tmp = tempfile::tempdir().unwrap();
        let odd = tmp.path().join("Weird Space！");
        let (_, label) = resolve_space_dir(Some(&odd), tmp.path(), tmp.path());
        assert_eq!(label, "weird-space");
    }
}

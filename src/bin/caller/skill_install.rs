//! Install the skills shipped inside the Intendant binary into the Agent
//! Skills standard personal directory.
//!
//! This is deliberately a machine-scoped install, never a per-session
//! project materialization. Project-scoped personal skills remain owned by
//! the user in the external backend's project directory; starting an
//! external agent must not write skill copies into its checkout.
//!
//! Every daemon-installed directory carries [`INSTALL_MARKER`].
//! Content-identical installs are no-ops, stale marked copies are removed,
//! and an unmarked user-owned directory with the same name always wins.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

/// Ownership marker for a directory created by this installer.
const INSTALL_MARKER: &str = ".intendant-installed";

/// Report from one global-install pass.
#[derive(Debug, Default)]
pub(crate) struct GlobalInstallReport {
    pub(crate) installed: Vec<String>,
    pub(crate) unchanged: usize,
    pub(crate) skipped_user_owned: Vec<String>,
    pub(crate) removed_stale: Vec<String>,
}

/// Install every shipped skill into `~/.agents/skills/` — the Agent Skills
/// standard personal path that Codex, Intendant itself, and (through the
/// setup-script `~/.claude/skills` alias) Claude Code all read.
fn install_global_skills() -> io::Result<GlobalInstallReport> {
    let Some(home) = dirs::home_dir() else {
        return Ok(GlobalInstallReport::default());
    };
    install_global_skills_in(&home)
}

/// Home-injectable core of [`install_global_skills`].
fn install_global_skills_in(home: &Path) -> io::Result<GlobalInstallReport> {
    let mut report = GlobalInstallReport::default();
    let target_dir = home.join(".agents").join("skills");

    let shipped = crate::builtin_skills::BUILTIN_SKILLS;

    // Sweep marked dirs that are no longer shipped globally. Renames and
    // removals clean up on the next daemon start.
    if let Ok(entries) = std::fs::read_dir(&target_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || !path.join(INSTALL_MARKER).is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !shipped.iter().any(|skill| skill.name == name) {
                std::fs::remove_dir_all(&path)?;
                report.removed_stale.push(name);
            }
        }
    }

    for skill in shipped {
        let dest = target_dir.join(skill.name);
        let marker = dest.join(INSTALL_MARKER);
        let dest_metadata = std::fs::symlink_metadata(&dest).ok();
        let dest_is_directory = dest_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
        if dest_metadata.is_some() && (!dest_is_directory || !marker.is_file()) {
            report.skipped_user_owned.push(skill.name.to_string());
            continue;
        }
        if installed_skill_is_current(&dest, skill) {
            report.unchanged += 1;
            continue;
        }
        if dest_metadata.is_some() {
            std::fs::remove_dir_all(&dest)?;
        }
        std::fs::create_dir_all(&dest)?;
        std::fs::write(&marker, "source: builtin (daemon-installed)\n")?;
        std::fs::write(dest.join("SKILL.md"), skill.skill_md)?;
        for (relative, bytes) in skill.support_files {
            let target = dest.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, bytes)?;
        }
        report.installed.push(skill.name.to_string());
    }
    Ok(report)
}

fn installed_skill_is_current(dest: &Path, skill: &crate::builtin_skills::BuiltinSkill) -> bool {
    if !dest.join(INSTALL_MARKER).is_file()
        || !std::fs::read_to_string(dest.join("SKILL.md"))
            .is_ok_and(|current| current == skill.skill_md)
    {
        return false;
    }
    for (relative, expected) in skill.support_files {
        if !std::fs::read(dest.join(relative)).is_ok_and(|current| current == *expected) {
            return false;
        }
    }

    let mut actual = BTreeSet::new();
    if collect_installed_files(dest, dest, &mut actual).is_err() {
        return false;
    }
    let mut expected = BTreeSet::from([PathBuf::from(INSTALL_MARKER), PathBuf::from("SKILL.md")]);
    expected.extend(
        skill
            .support_files
            .iter()
            .map(|(relative, _)| PathBuf::from(relative)),
    );
    actual == expected
}

fn collect_installed_files(
    root: &Path,
    dir: &Path,
    files: &mut BTreeSet<PathBuf>,
) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_installed_files(root, &path, files)?;
        } else {
            files.insert(
                path.strip_prefix(root)
                    .map_err(io::Error::other)?
                    .to_path_buf(),
            );
        }
    }
    Ok(())
}

/// Startup wrapper for session-serving modes: run the install and log one
/// line when it changed anything.
pub(crate) fn install_global_skills_at_startup() {
    match install_global_skills() {
        Ok(report) => {
            if !report.installed.is_empty() || !report.removed_stale.is_empty() {
                let kept = if report.skipped_user_owned.is_empty() {
                    String::new()
                } else {
                    format!(", {} user-owned kept", report.skipped_user_owned.len())
                };
                eprintln!(
                    "[skills] global install: {} installed, {} unchanged, {} stale removed{kept}",
                    report.installed.len(),
                    report.unchanged,
                    report.removed_stale.len(),
                );
            }
        }
        Err(error) => eprintln!("[skills] global install failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_install_is_complete_idempotent_and_ownership_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let expected = crate::builtin_skills::BUILTIN_SKILLS;

        // A user-authored dir colliding with one shipped skill, plus a
        // stale marked leftover from an older daemon.
        let target = home.join(".agents").join("skills");
        let user_owned = target.join(expected[0].name);
        std::fs::create_dir_all(&user_owned).unwrap();
        std::fs::write(user_owned.join("SKILL.md"), "user copy").unwrap();
        let stale = target.join("retired-builtin");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join(INSTALL_MARKER), "old").unwrap();

        let first = install_global_skills_in(home).unwrap();
        assert_eq!(first.installed.len(), expected.len() - 1);
        assert_eq!(first.skipped_user_owned, vec![expected[0].name.to_string()]);
        assert_eq!(first.removed_stale, vec!["retired-builtin".to_string()]);
        assert!(!stale.exists());
        assert_eq!(
            std::fs::read_to_string(user_owned.join("SKILL.md")).unwrap(),
            "user copy"
        );
        for skill in expected.iter().skip(1) {
            let dest = target.join(skill.name);
            assert!(dest.join("SKILL.md").exists(), "{} missing", skill.name);
            assert!(
                dest.join(INSTALL_MARKER).exists(),
                "{} unmarked",
                skill.name
            );
            for (relative, bytes) in skill.support_files {
                assert_eq!(
                    std::fs::read(dest.join(relative)).unwrap(),
                    *bytes,
                    "{}/{} missing or stale",
                    skill.name,
                    relative
                );
            }
        }

        // A changed support file or extra file refreshes the whole managed
        // directory from the embedded manifest.
        let with_support = expected
            .iter()
            .find(|skill| !skill.support_files.is_empty())
            .expect("at least one shipped skill has support files");
        let managed = target.join(with_support.name);
        let (support_path, support_bytes) = with_support.support_files[0];
        std::fs::write(managed.join(support_path), "stale").unwrap();
        std::fs::write(managed.join("unexpected.txt"), "stale").unwrap();
        let refreshed = install_global_skills_in(home).unwrap();
        assert_eq!(refreshed.installed, vec![with_support.name.to_string()]);
        assert_eq!(
            std::fs::read(managed.join(support_path)).unwrap(),
            support_bytes
        );
        assert!(!managed.join("unexpected.txt").exists());

        // The following run is a pure no-op.
        let unchanged = install_global_skills_in(home).unwrap();
        assert!(unchanged.installed.is_empty(), "{unchanged:?}");
        assert!(unchanged.removed_stale.is_empty(), "{unchanged:?}");
        assert_eq!(unchanged.unchanged, expected.len() - 1);
    }
}

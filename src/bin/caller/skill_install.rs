//! Install the skills shipped inside the Intendant binary into the global
//! skill directories read by Intendant's supported coding agents.
//!
//! This is deliberately a machine-scoped install, never a per-session
//! project materialization. Project-scoped personal skills remain owned by
//! the user in the external backend's project directory; starting an
//! external agent must not write skill copies into its checkout.
//!
//! The two roots are independent: Intendant never aliases or replaces either
//! root. Every daemon-installed skill directory carries [`INSTALL_MARKER`].
//! Content-identical installs are no-ops, stale marked copies are removed, and
//! an unmarked user-owned directory with the same name always wins.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

/// Ownership marker for a directory created by this installer.
const INSTALL_MARKER: &str = ".intendant-installed";

/// Report from one directly managed skill root.
#[derive(Debug, Default)]
struct SkillInstallReport {
    installed: Vec<String>,
    unchanged: usize,
    skipped_user_owned: Vec<String>,
    removed_stale: Vec<String>,
}

#[derive(Debug)]
enum SkillRootInstallOutcome {
    Installed(SkillInstallReport),
    SkippedUserOwnedRoot,
    Failed(String),
}

#[derive(Debug)]
struct SkillRootInstallReport {
    display_path: &'static str,
    outcome: SkillRootInstallOutcome,
}

/// Report from one global-install pass across both independent roots.
#[derive(Debug, Default)]
struct GlobalInstallReport {
    roots: Vec<SkillRootInstallReport>,
}

/// Install every shipped skill independently for Agent Skills consumers
/// (`~/.agents/skills/`) and Claude Code (`~/.claude/skills/`).
fn install_global_skills() -> GlobalInstallReport {
    let Some(home) = dirs::home_dir() else {
        return GlobalInstallReport::default();
    };
    install_global_skills_in(&home)
}

/// Home-injectable core of [`install_global_skills`].
fn install_global_skills_in(home: &Path) -> GlobalInstallReport {
    let targets = [
        ("~/.agents/skills", home.join(".agents").join("skills")),
        ("~/.claude/skills", home.join(".claude").join("skills")),
    ];
    let roots = targets
        .into_iter()
        .map(|(display_path, target_dir)| {
            let outcome = match install_skills_in_root(&target_dir) {
                Ok(Some(report)) => SkillRootInstallOutcome::Installed(report),
                Ok(None) => SkillRootInstallOutcome::SkippedUserOwnedRoot,
                Err(error) => SkillRootInstallOutcome::Failed(error.to_string()),
            };
            SkillRootInstallReport {
                display_path,
                outcome,
            }
        })
        .collect();
    GlobalInstallReport { roots }
}

/// Install the shipped catalog below one normal directory.
///
/// A link, junction, file, or other object at the root is user-owned and is
/// never followed or replaced. `read_link` recognizes Windows junctions as
/// well as symbolic links, while `symlink_metadata` keeps broken links visible.
fn install_skills_in_root(target_dir: &Path) -> io::Result<Option<SkillInstallReport>> {
    match std::fs::symlink_metadata(target_dir) {
        Ok(metadata) if !is_direct_directory(target_dir, &metadata) => return Ok(None),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut report = SkillInstallReport::default();
    let shipped = crate::builtin_skills::BUILTIN_SKILLS;

    // Sweep marked dirs that are no longer shipped. Renames and removals
    // clean up on the next daemon start.
    if let Ok(entries) = std::fs::read_dir(target_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir()
                || !is_direct_directory(
                    &path,
                    &match std::fs::symlink_metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(_) => continue,
                    },
                )
                || !path.join(INSTALL_MARKER).is_file()
            {
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
            .is_some_and(|metadata| is_direct_directory(&dest, metadata));
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
    Ok(Some(report))
}

fn is_direct_directory(path: &Path, metadata: &std::fs::Metadata) -> bool {
    metadata.is_dir() && !metadata.file_type().is_symlink() && std::fs::read_link(path).is_err()
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
/// line for changes, collisions, skipped roots, or failures.
pub(crate) fn install_global_skills_at_startup() {
    for root in install_global_skills().roots {
        match root.outcome {
            SkillRootInstallOutcome::Installed(report)
                if !report.installed.is_empty()
                    || !report.removed_stale.is_empty()
                    || !report.skipped_user_owned.is_empty() =>
            {
                let kept = if report.skipped_user_owned.is_empty() {
                    String::new()
                } else {
                    format!(", {} user-owned kept", report.skipped_user_owned.len())
                };
                eprintln!(
                    "[skills] {}: {} installed, {} unchanged, {} stale removed{kept}",
                    root.display_path,
                    report.installed.len(),
                    report.unchanged,
                    report.removed_stale.len(),
                );
            }
            SkillRootInstallOutcome::Installed(_) => {}
            SkillRootInstallOutcome::SkippedUserOwnedRoot => eprintln!(
                "[skills] {} is a link or non-directory; left untouched",
                root.display_path
            ),
            SkillRootInstallOutcome::Failed(error) => {
                eprintln!("[skills] {} install failed: {error}", root.display_path)
            }
        }
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

        // A user-authored Agent Skills collision must not suppress the
        // independent Claude copy. Both roots also sweep stale marked skills.
        let agents_target = home.join(".agents").join("skills");
        let claude_target = home.join(".claude").join("skills");
        let user_owned = agents_target.join(expected[0].name);
        std::fs::create_dir_all(&user_owned).unwrap();
        std::fs::write(user_owned.join("SKILL.md"), "user copy").unwrap();
        for target in [&agents_target, &claude_target] {
            let stale = target.join("retired-builtin");
            std::fs::create_dir_all(&stale).unwrap();
            std::fs::write(stale.join(INSTALL_MARKER), "old").unwrap();
        }

        let first = install_global_skills_in(home);
        let agents = installed_report(&first, "~/.agents/skills");
        let claude = installed_report(&first, "~/.claude/skills");
        assert_eq!(agents.installed.len(), expected.len() - 1);
        assert_eq!(
            agents.skipped_user_owned,
            vec![expected[0].name.to_string()]
        );
        assert_eq!(agents.removed_stale, vec!["retired-builtin".to_string()]);
        assert_eq!(claude.installed.len(), expected.len());
        assert!(claude.skipped_user_owned.is_empty());
        assert_eq!(claude.removed_stale, vec!["retired-builtin".to_string()]);
        assert!(!agents_target.join("retired-builtin").exists());
        assert!(!claude_target.join("retired-builtin").exists());
        assert_eq!(
            std::fs::read_to_string(user_owned.join("SKILL.md")).unwrap(),
            "user copy"
        );
        for (target, skip_first) in [(&agents_target, true), (&claude_target, false)] {
            for skill in expected.iter().skip(usize::from(skip_first)) {
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
        }

        // Changing one Claude copy refreshes only that root.
        let with_support = expected
            .iter()
            .find(|skill| !skill.support_files.is_empty())
            .expect("at least one shipped skill has support files");
        let managed = claude_target.join(with_support.name);
        let (support_path, support_bytes) = with_support.support_files[0];
        std::fs::write(managed.join(support_path), "stale").unwrap();
        std::fs::write(managed.join("unexpected.txt"), "stale").unwrap();
        let refreshed = install_global_skills_in(home);
        assert!(installed_report(&refreshed, "~/.agents/skills")
            .installed
            .is_empty());
        assert_eq!(
            installed_report(&refreshed, "~/.claude/skills").installed,
            vec![with_support.name.to_string()]
        );
        assert_eq!(
            std::fs::read(managed.join(support_path)).unwrap(),
            support_bytes
        );
        assert!(!managed.join("unexpected.txt").exists());

        // The following run is a pure no-op.
        let unchanged = install_global_skills_in(home);
        let agents = installed_report(&unchanged, "~/.agents/skills");
        let claude = installed_report(&unchanged, "~/.claude/skills");
        assert!(agents.installed.is_empty(), "{unchanged:?}");
        assert!(claude.installed.is_empty(), "{unchanged:?}");
        assert!(agents.removed_stale.is_empty(), "{unchanged:?}");
        assert!(claude.removed_stale.is_empty(), "{unchanged:?}");
        assert_eq!(agents.unchanged, expected.len() - 1);
        assert_eq!(claude.unchanged, expected.len());
    }

    #[test]
    fn non_directory_global_root_is_left_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let claude_root = home.join(".claude").join("skills");
        std::fs::create_dir_all(claude_root.parent().unwrap()).unwrap();
        std::fs::write(&claude_root, "user-owned").unwrap();

        let report = install_global_skills_in(home);
        assert!(matches!(
            outcome(&report, "~/.claude/skills"),
            SkillRootInstallOutcome::SkippedUserOwnedRoot
        ));
        assert_eq!(std::fs::read_to_string(&claude_root).unwrap(), "user-owned");
        assert_eq!(
            installed_report(&report, "~/.agents/skills")
                .installed
                .len(),
            crate::builtin_skills::BUILTIN_SKILLS.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn global_root_symlink_is_never_followed_or_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let linked_target = home.join("user-catalog");
        let claude_root = home.join(".claude").join("skills");
        std::fs::create_dir_all(&linked_target).unwrap();
        std::fs::create_dir_all(claude_root.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&linked_target, &claude_root).unwrap();

        let report = install_global_skills_in(home);
        assert!(matches!(
            outcome(&report, "~/.claude/skills"),
            SkillRootInstallOutcome::SkippedUserOwnedRoot
        ));
        assert_eq!(std::fs::read_link(&claude_root).unwrap(), linked_target);
        assert_eq!(std::fs::read_dir(&linked_target).unwrap().count(), 0);
    }

    fn outcome<'a>(
        report: &'a GlobalInstallReport,
        display_path: &str,
    ) -> &'a SkillRootInstallOutcome {
        &report
            .roots
            .iter()
            .find(|root| root.display_path == display_path)
            .unwrap()
            .outcome
    }

    fn installed_report<'a>(
        report: &'a GlobalInstallReport,
        display_path: &str,
    ) -> &'a SkillInstallReport {
        match outcome(report, display_path) {
            SkillRootInstallOutcome::Installed(report) => report,
            other => panic!("{display_path} was not installed: {other:?}"),
        }
    }
}

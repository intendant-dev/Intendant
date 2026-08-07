//! Install the skills shipped inside the Intendant binary into the global
//! skill directories read by Intendant's supported coding agents.
//!
//! This is deliberately a machine-scoped install, never a per-session
//! project materialization. Project-scoped personal skills remain owned by
//! the user in the external backend's project directory; starting an
//! external agent must not write skill copies into its checkout.
//!
//! The two roots are independent: Intendant never aliases or replaces either
//! root. Every daemon-installed skill directory carries [`INSTALL_MARKER`]
//! naming its source (builtin or an enabled plugin). Content-identical
//! installs are no-ops, stale marked copies are removed, and an unmarked
//! user-owned directory with the same name always wins.
//!
//! Besides the unconditional [`crate::builtin_skills`] catalog, each pass
//! takes the currently ACTIVE plugin skill payloads
//! ([`crate::plugin_registry::active_plugin_skills`]): those materialize
//! with `source: plugin:<id>` provenance while their plugin stays enabled
//! and ready, and are swept like retired builtins the moment it is not.
//!
//! Each pass also subtracts the persisted disabled-set
//! ([`crate::skill_state`]) from the BUILTIN half of the desired set —
//! the set outranks the sweep: startup installs, plugin toggles, and the
//! skill toggle all reconcile through here, so a re-materialization can
//! never resurrect a deactivated builtin. Plugin payloads are deliberately
//! NOT per-skill subtractable — their one lifecycle authority is the
//! plugin's own toggle (the intake's per-kind law).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

/// Ownership marker for a directory created by this installer.
const INSTALL_MARKER: &str = ".intendant-installed";

/// Marker content for skills sourced from the unconditional builtin
/// catalog. Plugin-sourced skills carry `source: plugin:<id> …` instead,
/// so an installed directory names where it came from.
const BUILTIN_MARKER_CONTENT: &str = "source: builtin (daemon-installed)\n";

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
pub(crate) struct GlobalInstallReport {
    roots: Vec<SkillRootInstallReport>,
}

impl GlobalInstallReport {
    /// Compact JSON for API responses: per root — the outcome and what
    /// changed. Serves the plugin enable/disable handlers so their reply
    /// reports what actually happened instead of pretending success.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let roots: Vec<serde_json::Value> = self
            .roots
            .iter()
            .map(|root| {
                let (outcome, detail) = match &root.outcome {
                    SkillRootInstallOutcome::Installed(report) => (
                        "applied",
                        serde_json::json!({
                            "installed": report.installed,
                            "unchanged": report.unchanged,
                            "removed_stale": report.removed_stale,
                            "skipped_user_owned": report.skipped_user_owned,
                        }),
                    ),
                    SkillRootInstallOutcome::SkippedUserOwnedRoot => {
                        ("root_user_owned", serde_json::Value::Null)
                    }
                    SkillRootInstallOutcome::Failed(error) => ("failed", serde_json::json!(error)),
                };
                serde_json::json!({
                    "root": root.display_path,
                    "outcome": outcome,
                    "detail": detail,
                })
            })
            .collect();
        serde_json::Value::Array(roots)
    }
}

/// Install every shipped skill independently for Agent Skills consumers
/// (`~/.agents/skills/`) and Claude Code (`~/.claude/skills/`).
fn install_global_skills(
    plugin_skills: &[(&str, &crate::builtin_skills::BuiltinSkill)],
    disabled: &BTreeSet<String>,
) -> GlobalInstallReport {
    let Some(home) = dirs::home_dir() else {
        return GlobalInstallReport::default();
    };
    install_global_skills_in(&home, plugin_skills, disabled)
}

/// Home-injectable core of [`install_global_skills`].
fn install_global_skills_in(
    home: &Path,
    plugin_skills: &[(&str, &crate::builtin_skills::BuiltinSkill)],
    disabled: &BTreeSet<String>,
) -> GlobalInstallReport {
    let targets = [
        ("~/.agents/skills", home.join(".agents").join("skills")),
        ("~/.claude/skills", home.join(".claude").join("skills")),
    ];
    let roots = targets
        .into_iter()
        .map(|(display_path, target_dir)| {
            let outcome = match install_skills_in_root(&target_dir, plugin_skills, disabled) {
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
fn install_skills_in_root(
    target_dir: &Path,
    plugin_skills: &[(&str, &crate::builtin_skills::BuiltinSkill)],
    disabled: &BTreeSet<String>,
) -> io::Result<Option<SkillInstallReport>> {
    match std::fs::symlink_metadata(target_dir) {
        Ok(metadata) if !is_direct_directory(target_dir, &metadata) => return Ok(None),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut report = SkillInstallReport::default();
    // The disabled-set subtracts BUILTINS only: a deactivated builtin
    // leaves the desired set, so the sweep below removes its marked
    // copies exactly like a retired builtin. Plugin payloads are never
    // per-skill subtractable (their lifecycle is the plugin's toggle), so
    // a stray or foreign entry naming one cannot sweep it from here.
    let mut desired: Vec<(&crate::builtin_skills::BuiltinSkill, String)> =
        crate::builtin_skills::BUILTIN_SKILLS
            .iter()
            .filter(|skill| !disabled.contains(skill.name))
            .map(|skill| (skill, BUILTIN_MARKER_CONTENT.to_string()))
            .collect();
    for (plugin_id, skill) in plugin_skills {
        // Collisions are forbidden by the registry parity tests; if one
        // ever slips through, the builtin wins and the plugin copy is
        // skipped rather than the two fighting over one directory.
        if desired
            .iter()
            .any(|(existing, _)| existing.name == skill.name)
        {
            continue;
        }
        desired.push((
            skill,
            format!("source: plugin:{plugin_id} (daemon-installed)\n"),
        ));
    }

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
            if !desired.iter().any(|(skill, _)| skill.name == name) {
                std::fs::remove_dir_all(&path)?;
                report.removed_stale.push(name);
            }
        }
    }

    for (skill, marker_content) in &desired {
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
        if installed_skill_is_current(&dest, skill, marker_content) {
            report.unchanged += 1;
            continue;
        }
        if dest_metadata.is_some() {
            std::fs::remove_dir_all(&dest)?;
        }
        std::fs::create_dir_all(&dest)?;
        std::fs::write(&marker, marker_content)?;
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

fn installed_skill_is_current(
    dest: &Path,
    skill: &crate::builtin_skills::BuiltinSkill,
    marker_content: &str,
) -> bool {
    std::fs::read_to_string(dest.join(INSTALL_MARKER))
        .is_ok_and(|current| current == marker_content)
        && installed_payload_is_current(dest, skill)
}

/// Marker-agnostic payload currency: SKILL.md and every support file match
/// the embedded bytes, and no extra files ride along. Shared by the
/// installer (which adds a marker-content check on top) and the read-only
/// status helper (which cannot know the expected provenance line).
fn installed_payload_is_current(dest: &Path, skill: &crate::builtin_skills::BuiltinSkill) -> bool {
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

/// Read-only per-root install status for one skill payload, consumed by
/// the plugin catalog body: `installed` (marked, payload current), `stale`
/// (marked, payload drifted), `user_owned` (unmarked collision), `absent`,
/// or `root_user_owned` (the whole root is a link or non-directory the
/// installer never touches).
pub(crate) fn skill_install_status_in(
    home: &Path,
    skill: &crate::builtin_skills::BuiltinSkill,
) -> Vec<(&'static str, &'static str)> {
    [
        ("~/.agents/skills", home.join(".agents").join("skills")),
        ("~/.claude/skills", home.join(".claude").join("skills")),
    ]
    .into_iter()
    .map(|(display_path, root)| {
        let root_status = match std::fs::symlink_metadata(&root) {
            Ok(metadata) if !is_direct_directory(&root, &metadata) => Some("root_user_owned"),
            Err(_) => Some("absent"),
            Ok(_) => None,
        };
        let status = root_status.unwrap_or_else(|| {
            let dest = root.join(skill.name);
            match std::fs::symlink_metadata(&dest) {
                Err(_) => "absent",
                Ok(metadata)
                    if !is_direct_directory(&dest, &metadata)
                        || !dest.join(INSTALL_MARKER).is_file() =>
                {
                    "user_owned"
                }
                Ok(_) if installed_payload_is_current(&dest, skill) => "installed",
                Ok(_) => "stale",
            }
        });
        (display_path, status)
    })
    .collect()
}

/// Re-run the global install against the current builtin + active-plugin
/// set minus the persisted disabled-set. The plugin and skill toggle
/// handlers call this right after a state write so materialization and
/// sweep land in the same request; because the disabled-set is re-read
/// here on every pass, the set outranks the sweep at every call site.
pub(crate) fn reconcile_global_skills() -> GlobalInstallReport {
    install_global_skills(
        &crate::plugin_registry::active_plugin_skills(),
        &crate::skill_state::disabled_skill_names(),
    )
}

/// Startup wrapper for session-serving modes: run the install and log one
/// line for changes, collisions, skipped roots, or failures.
pub(crate) fn install_global_skills_at_startup() {
    for root in install_global_skills(
        &crate::plugin_registry::active_plugin_skills(),
        &crate::skill_state::disabled_skill_names(),
    )
    .roots
    {
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

        let first = install_global_skills_in(home, &[], &BTreeSet::new());
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
        let refreshed = install_global_skills_in(home, &[], &BTreeSet::new());
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
        let unchanged = install_global_skills_in(home, &[], &BTreeSet::new());
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

        let report = install_global_skills_in(home, &[], &BTreeSet::new());
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

        let report = install_global_skills_in(home, &[], &BTreeSet::new());
        assert!(matches!(
            outcome(&report, "~/.claude/skills"),
            SkillRootInstallOutcome::SkippedUserOwnedRoot
        ));
        assert_eq!(std::fs::read_link(&claude_root).unwrap(), linked_target);
        assert_eq!(std::fs::read_dir(&linked_target).unwrap().count(), 0);
    }

    #[test]
    fn plugin_skills_materialize_with_provenance_and_sweep_on_deactivation() {
        static PLUGIN_SKILL: crate::builtin_skills::BuiltinSkill =
            crate::builtin_skills::BuiltinSkill {
                name: "test-plugin-skill",
                skill_md: "---\nname: test-plugin-skill\ndescription: test\n---\nbody\n",
                support_files: &[],
            };
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let payload = [("test-plugin", &PLUGIN_SKILL)];

        // Active plugin: materializes in both roots with plugin provenance.
        install_global_skills_in(home, &payload, &BTreeSet::new());
        for root in [
            home.join(".agents").join("skills"),
            home.join(".claude").join("skills"),
        ] {
            let dest = root.join(PLUGIN_SKILL.name);
            assert_eq!(
                std::fs::read_to_string(dest.join("SKILL.md")).unwrap(),
                PLUGIN_SKILL.skill_md
            );
            assert_eq!(
                std::fs::read_to_string(dest.join(INSTALL_MARKER)).unwrap(),
                "source: plugin:test-plugin (daemon-installed)\n"
            );
        }
        assert_eq!(
            skill_install_status_in(home, &PLUGIN_SKILL),
            vec![
                ("~/.agents/skills", "installed"),
                ("~/.claude/skills", "installed")
            ]
        );

        // A repeat pass with the same payload is a pure no-op — the
        // marker-content check must not churn plugin provenance.
        let second = install_global_skills_in(home, &payload, &BTreeSet::new());
        assert!(installed_report(&second, "~/.claude/skills")
            .installed
            .is_empty());

        // Payload drift reads as stale.
        std::fs::write(
            home.join(".claude")
                .join("skills")
                .join(PLUGIN_SKILL.name)
                .join("SKILL.md"),
            "tampered",
        )
        .unwrap();
        assert_eq!(
            skill_install_status_in(home, &PLUGIN_SKILL)[1],
            ("~/.claude/skills", "stale")
        );

        // Deactivated: swept exactly like a retired builtin.
        let swept = install_global_skills_in(home, &[], &BTreeSet::new());
        for display in ["~/.agents/skills", "~/.claude/skills"] {
            assert_eq!(
                installed_report(&swept, display).removed_stale,
                vec![PLUGIN_SKILL.name.to_string()]
            );
        }
        assert_eq!(
            skill_install_status_in(home, &PLUGIN_SKILL),
            vec![
                ("~/.agents/skills", "absent"),
                ("~/.claude/skills", "absent")
            ]
        );

        // An unmarked user-owned copy is never replaced on activation and
        // never removed on deactivation.
        let user_copy = home.join(".agents").join("skills").join(PLUGIN_SKILL.name);
        std::fs::create_dir_all(&user_copy).unwrap();
        std::fs::write(user_copy.join("SKILL.md"), "user copy").unwrap();
        let materialized = install_global_skills_in(home, &payload, &BTreeSet::new());
        assert_eq!(
            installed_report(&materialized, "~/.agents/skills").skipped_user_owned,
            vec![PLUGIN_SKILL.name.to_string()]
        );
        assert_eq!(
            skill_install_status_in(home, &PLUGIN_SKILL)[0],
            ("~/.agents/skills", "user_owned")
        );
        install_global_skills_in(home, &[], &BTreeSet::new());
        assert_eq!(
            std::fs::read_to_string(user_copy.join("SKILL.md")).unwrap(),
            "user copy"
        );
    }

    /// The S3 invariant: THE SET OUTRANKS THE SWEEP. A deactivated
    /// builtin is swept from BOTH discovery roots, stays absent across
    /// repeat passes, and — the resurrection simulation — even a copy
    /// re-materialized behind the daemon's back (a rebuild, an older
    /// binary's install pass) is swept again on the next reconcile, from
    /// every call site, because each pass re-reads the set. Re-enable
    /// restores byte-identical installs.
    #[test]
    fn disabled_builtins_are_swept_from_both_roots_and_never_resurrect() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let skill = crate::builtin_skills::BUILTIN_SKILLS
            .iter()
            .find(|skill| !skill.support_files.is_empty())
            .expect("at least one shipped skill has support files");
        let roots = [
            home.join(".agents").join("skills"),
            home.join(".claude").join("skills"),
        ];

        // Baseline install, then deactivate: swept from both roots
        // in one pass, reported as removed.
        install_global_skills_in(home, &[], &BTreeSet::new());
        let disabled = BTreeSet::from([skill.name.to_string()]);
        let swept = install_global_skills_in(home, &[], &disabled);
        for display in ["~/.agents/skills", "~/.claude/skills"] {
            assert_eq!(
                installed_report(&swept, display).removed_stale,
                vec![skill.name.to_string()]
            );
        }
        assert_eq!(
            skill_install_status_in(home, skill),
            vec![
                ("~/.agents/skills", "absent"),
                ("~/.claude/skills", "absent")
            ]
        );

        // A repeat pass with the set neither reinstalls nor re-sweeps.
        let repeat = install_global_skills_in(home, &[], &disabled);
        for display in ["~/.agents/skills", "~/.claude/skills"] {
            let report = installed_report(&repeat, display);
            assert!(report.installed.is_empty());
            assert!(report.removed_stale.is_empty());
        }

        // Resurrection simulation: a marked copy reappears (as a rebuild
        // or an older binary's pass would leave it) — the next sweep
        // under the set removes it again.
        for root in &roots {
            let dest = root.join(skill.name);
            std::fs::create_dir_all(&dest).unwrap();
            std::fs::write(dest.join(INSTALL_MARKER), BUILTIN_MARKER_CONTENT).unwrap();
            std::fs::write(dest.join("SKILL.md"), skill.skill_md).unwrap();
        }
        let re_swept = install_global_skills_in(home, &[], &disabled);
        for display in ["~/.agents/skills", "~/.claude/skills"] {
            assert_eq!(
                installed_report(&re_swept, display).removed_stale,
                vec![skill.name.to_string()],
                "a re-materialized disabled skill must be swept again"
            );
        }
        for root in &roots {
            assert!(!root.join(skill.name).exists());
        }

        // Re-enable: the next pass restores byte-identical installs.
        install_global_skills_in(home, &[], &BTreeSet::new());
        for root in &roots {
            let dest = root.join(skill.name);
            assert_eq!(
                std::fs::read_to_string(dest.join("SKILL.md")).unwrap(),
                skill.skill_md
            );
            assert_eq!(
                std::fs::read_to_string(dest.join(INSTALL_MARKER)).unwrap(),
                BUILTIN_MARKER_CONTENT
            );
            for (relative, bytes) in skill.support_files {
                assert_eq!(std::fs::read(dest.join(relative)).unwrap(), *bytes);
            }
        }
        assert_eq!(
            skill_install_status_in(home, skill),
            vec![
                ("~/.agents/skills", "installed"),
                ("~/.claude/skills", "installed")
            ]
        );
    }

    /// The per-kind law at the installer layer: the disabled-set
    /// subtracts builtins only. An entry naming a plugin payload (stray
    /// or foreign) neither blocks its materialization nor sweeps it —
    /// plugin payloads have exactly one lifecycle authority, the plugin
    /// toggle.
    #[test]
    fn disabled_set_never_touches_plugin_payloads() {
        static PLUGIN_SKILL: crate::builtin_skills::BuiltinSkill =
            crate::builtin_skills::BuiltinSkill {
                name: "test-plugin-skill",
                skill_md: "---\nname: test-plugin-skill\ndescription: test\n---\nbody\n",
                support_files: &[],
            };
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let payload = [("test-plugin", &PLUGIN_SKILL)];
        let disabled = BTreeSet::from([PLUGIN_SKILL.name.to_string()]);

        install_global_skills_in(home, &payload, &disabled);
        assert_eq!(
            skill_install_status_in(home, &PLUGIN_SKILL),
            vec![
                ("~/.agents/skills", "installed"),
                ("~/.claude/skills", "installed")
            ],
            "a disabled-set entry must not suppress a plugin payload"
        );

        let repeat = install_global_skills_in(home, &payload, &disabled);
        for display in ["~/.agents/skills", "~/.claude/skills"] {
            assert!(
                installed_report(&repeat, display).removed_stale.is_empty(),
                "a disabled-set entry must not sweep a plugin payload"
            );
        }
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

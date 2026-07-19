//! Materialize the daemon's effective skill catalog into the standard
//! per-backend project scan paths, so supervised external agents discover
//! the same skills native sessions do.
//!
//! Verified scan behavior (2026-07-19, claude 2.1.215 / codex 0.144.6):
//! Claude Code reads `<project>/.claude/skills/` (not `.agents/`); Codex
//! reads `<project>/.agents/skills/` (plus `~/.agents/skills/` and
//! `$CODEX_HOME/skills/`). Home-directory injection is NOT viable as the
//! general mechanism: `CODEX_HOME`/`CLAUDE_CONFIG_DIR` are passthrough
//! env that only vault leases override, so in the common subscription
//! mode the CLIs run on the user's real homes, which the daemon must
//! never mutate.
//!
//! Ownership contract: every materialized skill directory carries a
//! [`MATERIALIZED_MARKER`] file, written BEFORE the copy so a crash
//! mid-copy leaves a marked (sweepable) partial, never an orphan that
//! looks user-authored. Provisioning deletes ONLY marked directories
//! before rewriting; a user-authored directory with the same name is
//! never touched — the materialized copy is skipped and the user wins.
//! Native discovery skips marked directories
//! (`intendant_core::skills`), so a derived copy can never shadow its
//! source. Materialized names are hidden from git through a managed
//! block in `.git/info/exclude` (the common git dir, shared across
//! worktrees) — never a committed ignore file.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use intendant_core::skills::{discover_skills_in, MATERIALIZED_MARKER};

/// Backend project scan paths that receive materialized copies, relative
/// to the session's project root.
const TARGET_DIRS: [&[&str]; 2] = [&[".agents", "skills"], &[".claude", "skills"]];

/// Per-skill byte budget for a materialized copy (SKILL.md + support
/// files). Oversized skills are skipped with a report entry rather than
/// bloating every supervised checkout.
const SKILL_MATERIALIZE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Kill switch: any non-empty value disables provisioning entirely.
const SKIP_ENV: &str = "INTENDANT_SKIP_SKILL_PROVISION";

const EXCLUDE_BLOCK_BEGIN: &str = "# BEGIN intendant skills_sync (managed)";
const EXCLUDE_BLOCK_END: &str = "# END intendant skills_sync (managed)";

#[derive(Debug, Default)]
pub(crate) struct ProvisionReport {
    /// Skill names freshly materialized this pass (per name, not per target).
    pub(crate) materialized: Vec<String>,
    /// Names skipped because an unmarked (user-authored) directory already
    /// occupies the target path.
    pub(crate) skipped_user_owned: Vec<String>,
    /// Names skipped for exceeding [`SKILL_MATERIALIZE_MAX_BYTES`].
    pub(crate) skipped_oversize: Vec<String>,
}

/// Materialize the effective skill catalog for `project_root` into every
/// backend target dir. Honors the [`SKIP_ENV`] kill switch. Idempotent
/// and cheap; call it on every supervised spawn so refreshes ride session
/// boundaries.
pub(crate) fn provision_project_skills(project_root: &Path) -> io::Result<ProvisionReport> {
    if std::env::var_os(SKIP_ENV).is_some_and(|v| !v.is_empty()) {
        return Ok(ProvisionReport::default());
    }
    provision_project_skills_in(project_root, dirs::home_dir().as_deref())
}

/// Home-injectable core of [`provision_project_skills`] (hermetic tests
/// pin `home`).
fn provision_project_skills_in(
    project_root: &Path,
    home: Option<&Path>,
) -> io::Result<ProvisionReport> {
    let skills = discover_skills_in(Some(project_root), home);
    let mut report = ProvisionReport::default();
    let mut materialized = BTreeSet::new();
    let mut skipped_user = BTreeSet::new();
    let mut skipped_size = BTreeSet::new();
    // Exclude patterns carry (target, name) precision: a name materialized
    // in one target but user-owned in the other must never get the user's
    // directory excluded from git.
    let mut exclude_patterns = BTreeSet::new();

    for target_parts in TARGET_DIRS {
        let target_dir = target_parts
            .iter()
            .fold(project_root.to_path_buf(), |p, part| p.join(part));
        sweep_marked(&target_dir)?;
        if skills.is_empty() {
            continue;
        }
        std::fs::create_dir_all(&target_dir)?;
        let canonical_target = target_dir.canonicalize()?;

        for skill in &skills {
            let Some(source_dir) = skill.source_path.parent() else {
                continue;
            };
            // Self-copy guard: a skill already living in this target dir
            // is its own materialization.
            if source_dir
                .canonicalize()
                .is_ok_and(|src| src.parent() == Some(canonical_target.as_path()))
            {
                continue;
            }
            let dest = target_dir.join(&skill.config.name);
            if dest.exists() {
                // Post-sweep survivor = unmarked = user-authored. The
                // user's directory wins; never touch it.
                skipped_user.insert(skill.config.name.clone());
                continue;
            }
            match copy_skill_dir(source_dir, &dest, &skill.source_path)? {
                CopyOutcome::Copied => {
                    materialized.insert(skill.config.name.clone());
                    exclude_patterns.insert(format!(
                        "/{}/{}/",
                        target_parts.join("/"),
                        skill.config.name
                    ));
                }
                CopyOutcome::Oversize => {
                    skipped_size.insert(skill.config.name.clone());
                }
            }
        }
    }

    report.materialized = materialized.into_iter().collect();
    report.skipped_user_owned = skipped_user.into_iter().collect();
    report.skipped_oversize = skipped_size.into_iter().collect();
    let patterns: Vec<String> = exclude_patterns.into_iter().collect();
    update_git_excludes(project_root, &patterns)?;
    Ok(report)
}

/// Remove every marked (previously materialized) skill dir under
/// `target_dir`, leaving user-authored dirs alone.
fn sweep_marked(target_dir: &Path) -> io::Result<()> {
    let Ok(entries) = std::fs::read_dir(target_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join(MATERIALIZED_MARKER).exists() {
            std::fs::remove_dir_all(&path)?;
        }
    }
    Ok(())
}

enum CopyOutcome {
    Copied,
    Oversize,
}

/// Copy one skill directory into `dest`, marker-first for crash safety.
/// Symlinked entries are skipped (a materialized copy must be
/// self-contained and must never alias back into user-owned trees).
fn copy_skill_dir(source_dir: &Path, dest: &Path, source_md: &Path) -> io::Result<CopyOutcome> {
    std::fs::create_dir_all(dest)?;
    std::fs::write(
        dest.join(MATERIALIZED_MARKER),
        format!("source: {}\n", source_md.display()),
    )?;
    let mut budget = SKILL_MATERIALIZE_MAX_BYTES;
    if copy_dir_capped(source_dir, dest, &mut budget)? {
        Ok(CopyOutcome::Copied)
    } else {
        std::fs::remove_dir_all(dest)?;
        Ok(CopyOutcome::Oversize)
    }
}

/// Recursive capped copy; returns false when the byte budget ran out.
fn copy_dir_capped(source: &Path, dest: &Path, budget: &mut u64) -> io::Result<bool> {
    for entry in std::fs::read_dir(source)?.flatten() {
        let path = entry.path();
        let file_type = entry.file_type()?;
        let target = dest.join(entry.file_name());
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)?;
            if !copy_dir_capped(&path, &target, budget)? {
                return Ok(false);
            }
        } else {
            let size = entry.metadata()?.len();
            if size > *budget {
                return Ok(false);
            }
            *budget -= size;
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(true)
}

/// Maintain the managed exclude block in the repository's shared
/// `info/exclude` so materialized copies never show up as untracked
/// files. No-op outside a git checkout. The common git dir is used so one
/// block covers every worktree. `patterns` are exact
/// `/{target}/{name}/` lines for directories this pass materialized —
/// never broader, so user-authored dirs stay visible to git.
fn update_git_excludes(project_root: &Path, patterns: &[String]) -> io::Result<()> {
    let Some(common_dir) = git_common_dir(project_root) else {
        return Ok(());
    };
    let exclude_path = common_dir.join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();

    let mut kept: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in existing.lines() {
        if line.trim() == EXCLUDE_BLOCK_BEGIN {
            in_block = true;
            continue;
        }
        if line.trim() == EXCLUDE_BLOCK_END {
            in_block = false;
            continue;
        }
        if !in_block {
            kept.push(line);
        }
    }

    let mut next = kept.join("\n");
    if !patterns.is_empty() {
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(EXCLUDE_BLOCK_BEGIN);
        next.push('\n');
        for pattern in patterns {
            next.push_str(pattern);
            next.push('\n');
        }
        next.push_str(EXCLUDE_BLOCK_END);
        next.push('\n');
    } else if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }

    if next == existing {
        return Ok(());
    }
    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&exclude_path, next)
}

/// The checkout's common git dir (shared across worktrees), or None
/// outside a git repository.
fn git_common_dir(project_root: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    })
}

/// Report from one global-install pass.
#[derive(Debug, Default)]
pub(crate) struct GlobalInstallReport {
    pub(crate) installed: Vec<String>,
    pub(crate) unchanged: usize,
    pub(crate) skipped_user_owned: Vec<String>,
    pub(crate) removed_stale: Vec<String>,
}

/// Install every `distribution: global` builtin skill into
/// `~/.agents/skills/` — the Agent Skills standard personal path that
/// Codex, Intendant itself, and (via the setup-script symlink) Claude
/// Code all read. Same ownership contract as project materialization:
/// marker-first writes, marked-only sweeps, user-authored directories
/// always win. Content-identical installs are no-ops, so restarts and
/// concurrent daemons do not churn the folder; sweeps remove only marked
/// dirs, which are derived copies that regenerate from their sources.
/// Honors [`SKIP_ENV`].
pub(crate) fn install_global_skills() -> io::Result<GlobalInstallReport> {
    if std::env::var_os(SKIP_ENV).is_some_and(|v| !v.is_empty()) {
        return Ok(GlobalInstallReport::default());
    }
    let Some(home) = dirs::home_dir() else {
        return Ok(GlobalInstallReport::default());
    };
    install_global_skills_in(&home)
}

/// Home-injectable core of [`install_global_skills`].
fn install_global_skills_in(home: &Path) -> io::Result<GlobalInstallReport> {
    let mut report = GlobalInstallReport::default();
    let target_dir = home.join(".agents").join("skills");

    let globals: Vec<(&str, &str)> = crate::builtin_skills::BUILTIN_SKILLS
        .iter()
        .filter(|(name, content)| {
            intendant_core::skills::parse_skill_md(content, Path::new(name))
                .map(|(config, _)| config.is_global())
                .unwrap_or(false)
        })
        .copied()
        .collect();

    // Sweep marked dirs that are no longer (or never were) in the global
    // set — renames and demotions clean up on the next daemon start.
    if let Ok(entries) = std::fs::read_dir(&target_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || !path.join(MATERIALIZED_MARKER).exists() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !globals.iter().any(|(n, _)| *n == name) {
                std::fs::remove_dir_all(&path)?;
                report.removed_stale.push(name);
            }
        }
    }

    for (name, content) in globals {
        let dest = target_dir.join(name);
        let marker = dest.join(MATERIALIZED_MARKER);
        let skill_md = dest.join("SKILL.md");
        if dest.exists() && !marker.exists() {
            // User-authored (possibly via a symlinked personal skills
            // setup) — the user's copy always wins.
            report.skipped_user_owned.push(name.to_string());
            continue;
        }
        if marker.exists()
            && std::fs::read_to_string(&skill_md).is_ok_and(|current| current == content)
        {
            report.unchanged += 1;
            continue;
        }
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        std::fs::create_dir_all(&dest)?;
        std::fs::write(&marker, "source: builtin (daemon-installed)\n")?;
        std::fs::write(&skill_md, content)?;
        report.installed.push(name.to_string());
    }
    Ok(report)
}

/// Startup wrapper for the session-serving modes: run the install and
/// log one line when it changed anything.
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
        Err(e) => eprintln!("[skills] global install failed: {e}"),
    }
}

/// Log-friendly one-line summary used by the spawn sites.
pub(crate) fn describe_report(report: &ProvisionReport) -> Option<String> {
    if report.materialized.is_empty()
        && report.skipped_user_owned.is_empty()
        && report.skipped_oversize.is_empty()
    {
        return None;
    }
    let mut parts = vec![format!("materialized {}", report.materialized.len())];
    if !report.skipped_user_owned.is_empty() {
        parts.push(format!(
            "kept {} user-owned",
            report.skipped_user_owned.len()
        ));
    }
    if !report.skipped_oversize.is_empty() {
        parts.push(format!(
            "skipped {} oversize",
            report.skipped_oversize.len()
        ));
    }
    Some(parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, name: &str, extra: Option<(&str, &[u8])>) {
        let skill = dir.join(name);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill\n---\nbody\n"),
        )
        .unwrap();
        if let Some((file, bytes)) = extra {
            std::fs::write(skill.join(file), bytes).unwrap();
        }
    }

    #[test]
    fn materializes_into_both_targets_with_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_skill(&root.join("skills"), "demo", Some(("ref.md", b"support")));

        let report = provision_project_skills_in(root, None).unwrap();
        assert_eq!(report.materialized, vec!["demo".to_string()]);
        for target in [".agents/skills", ".claude/skills"] {
            let dest = root.join(target).join("demo");
            assert!(dest.join("SKILL.md").exists(), "{target} missing SKILL.md");
            assert!(
                dest.join("ref.md").exists(),
                "{target} missing support file"
            );
            assert!(
                dest.join(MATERIALIZED_MARKER).exists(),
                "{target} missing marker"
            );
        }
    }

    #[test]
    fn reprovision_refreshes_marked_and_preserves_user_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_skill(&root.join("skills"), "demo", None);

        // A stale marked copy with old content, and a user-authored dir
        // colliding with a source skill name.
        let stale = root.join(".agents").join("skills").join("demo");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("SKILL.md"), "stale").unwrap();
        std::fs::write(stale.join(MATERIALIZED_MARKER), "old").unwrap();
        write_skill(&root.join(".claude").join("skills"), "demo", None);
        // Strip the marker so it reads as user-authored.
        let user_owned = root.join(".claude").join("skills").join("demo");
        std::fs::write(user_owned.join("SKILL.md"), "user copy").unwrap();

        let report = provision_project_skills_in(root, None).unwrap();
        // The stale marked copy was swept and rewritten from source…
        let refreshed = std::fs::read_to_string(root.join(".agents/skills/demo/SKILL.md")).unwrap();
        assert!(refreshed.contains("test skill"), "{refreshed}");
        // …while the user's unmarked dir survived byte-for-byte.
        assert_eq!(
            std::fs::read_to_string(user_owned.join("SKILL.md")).unwrap(),
            "user copy"
        );
        assert!(report.skipped_user_owned.contains(&"demo".to_string()));
    }

    #[test]
    fn source_in_target_dir_is_not_copied_onto_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_skill(&root.join(".agents").join("skills"), "native", None);

        let report = provision_project_skills_in(root, None).unwrap();
        // Not rewritten in place (still unmarked)…
        let source = root.join(".agents/skills/native");
        assert!(!source.join(MATERIALIZED_MARKER).exists());
        // …but materialized into the OTHER backend's target.
        assert!(root.join(".claude/skills/native/SKILL.md").exists());
        assert_eq!(report.materialized, vec!["native".to_string()]);
    }

    #[test]
    fn oversize_skill_is_skipped_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let big = vec![0u8; (SKILL_MATERIALIZE_MAX_BYTES + 1) as usize];
        write_skill(&root.join("skills"), "huge", Some(("blob.bin", &big)));

        let report = provision_project_skills_in(root, None).unwrap();
        assert_eq!(report.skipped_oversize, vec!["huge".to_string()]);
        assert!(!root.join(".agents/skills/huge").exists());
        assert!(!root.join(".claude/skills/huge").exists());
    }

    #[test]
    fn git_exclude_block_is_managed_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let git_ok = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("init")
            .arg("-q")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(git_ok, "git init failed");
        write_skill(&root.join("skills"), "demo", None);
        // A user-authored dir colliding in one target: its path must NOT
        // be excluded even though the name materializes in the other.
        write_skill(&root.join(".claude").join("skills"), "demo", None);

        provision_project_skills_in(root, None).unwrap();
        provision_project_skills_in(root, None).unwrap();

        let exclude =
            std::fs::read_to_string(root.join(".git").join("info").join("exclude")).unwrap();
        assert_eq!(exclude.matches(EXCLUDE_BLOCK_BEGIN).count(), 1, "{exclude}");
        assert!(exclude.contains("/.agents/skills/demo/"), "{exclude}");
        assert!(
            !exclude.contains("/.claude/skills/demo/"),
            "user-owned path must stay visible to git: {exclude}"
        );

        // Materialized copies are invisible to git; the user's own
        // untracked dir stays visible.
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&status.stdout);
        assert!(
            !listing.contains(".agents/"),
            "materialized copies leaked into git status: {listing}"
        );
        assert!(
            listing.contains(".claude/"),
            "user-owned dir vanished from git status: {listing}"
        );
    }

    #[test]
    fn global_install_is_idempotent_and_ownership_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let expected_globals: Vec<&str> = crate::builtin_skills::BUILTIN_SKILLS
            .iter()
            .filter(|(name, content)| {
                intendant_core::skills::parse_skill_md(content, Path::new(name))
                    .map(|(config, _)| config.is_global())
                    .unwrap_or(false)
            })
            .map(|(name, _)| *name)
            .collect();
        assert!(
            !expected_globals.is_empty(),
            "at least one builtin skill must be distribution: global"
        );

        // A user-authored dir colliding with one global, and a stale
        // marked leftover from an older daemon.
        let target = home.join(".agents").join("skills");
        let user_owned = target.join(expected_globals[0]);
        std::fs::create_dir_all(&user_owned).unwrap();
        std::fs::write(user_owned.join("SKILL.md"), "user copy").unwrap();
        let stale = target.join("retired-builtin");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join(MATERIALIZED_MARKER), "old").unwrap();

        let first = install_global_skills_in(home).unwrap();
        assert_eq!(first.installed.len(), expected_globals.len() - 1);
        assert_eq!(
            first.skipped_user_owned,
            vec![expected_globals[0].to_string()]
        );
        assert_eq!(first.removed_stale, vec!["retired-builtin".to_string()]);
        assert!(!stale.exists());
        assert_eq!(
            std::fs::read_to_string(user_owned.join("SKILL.md")).unwrap(),
            "user copy"
        );
        for name in expected_globals.iter().skip(1) {
            let dest = target.join(name);
            assert!(dest.join("SKILL.md").exists(), "{name} missing");
            assert!(dest.join(MATERIALIZED_MARKER).exists(), "{name} unmarked");
        }

        // Second run: pure no-op.
        let second = install_global_skills_in(home).unwrap();
        assert!(second.installed.is_empty(), "{second:?}");
        assert!(second.removed_stale.is_empty(), "{second:?}");
        assert_eq!(second.unchanged, expected_globals.len() - 1);
    }

    #[test]
    fn worktree_provisioning_writes_the_shared_common_exclude() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args([
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "user.name=test",
                ])
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed in {}", dir.display());
        };
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "root"]);
        let wt = tmp.path().join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wtb"],
        );
        write_skill(&wt.join("skills"), "demo", None);

        provision_project_skills_in(&wt, None).unwrap();

        // The managed block lands in the MAIN checkout's shared git dir…
        let exclude =
            std::fs::read_to_string(main.join(".git").join("info").join("exclude")).unwrap();
        assert!(exclude.contains("/.agents/skills/demo/"), "{exclude}");
        // …and the worktree's status is clean of materialized copies.
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&status.stdout);
        assert!(
            !listing.contains(".agents/") && !listing.contains(".claude/"),
            "materialized copies leaked into worktree git status: {listing}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_entries_are_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let secret = root.join("secret.txt");
        std::fs::write(&secret, "outside").unwrap();
        write_skill(&root.join("skills"), "linky", None);
        std::os::unix::fs::symlink(&secret, root.join("skills/linky/alias.txt")).unwrap();

        provision_project_skills_in(root, None).unwrap();
        let dest = root.join(".agents/skills/linky");
        assert!(dest.join("SKILL.md").exists());
        assert!(!dest.join("alias.txt").exists(), "symlink must be skipped");
    }
}

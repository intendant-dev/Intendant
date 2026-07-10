use crate::error::CallerError;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Worktree {
    pub branch_name: String,
    pub path: PathBuf,
    /// Base ref the worktree branched from. Only the merge/list flows read
    /// it; live callers (the fission spawn path) construct-and-drop it.
    #[allow(dead_code)]
    pub base_branch: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MergeResult {
    Clean,
    Conflict(String),
}

/// Ceiling for user-supplied worktree branch names. Git itself allows far
/// longer refs, but the branch doubles as the worktree directory name under
/// `.intendant/worktrees/`, so keep it filesystem-friendly.
const MAX_BRANCH_NAME_LEN: usize = 120;

/// Validate a user-supplied branch name for a session worktree.
///
/// A deliberately conservative subset of `git check-ref-format` that is
/// also path-safe: the branch becomes a directory under
/// `.intendant/worktrees/`, so `..`, absolute separators, and other
/// traversal shapes are rejected outright rather than left for git to
/// maybe-accept.
pub fn validate_branch_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("branch name is empty".to_string());
    }
    if name.len() > MAX_BRANCH_NAME_LEN {
        return Err(format!(
            "branch name is longer than {MAX_BRANCH_NAME_LEN} characters"
        ));
    }
    if name.starts_with('-') {
        return Err("branch name must not start with '-'".to_string());
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err("branch name must not start or end with '/'".to_string());
    }
    if name.ends_with('.') {
        return Err("branch name must not end with '.'".to_string());
    }
    if name.contains("@{") {
        return Err("branch name must not contain '@{'".to_string());
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')))
    {
        return Err(format!(
            "branch name may only contain letters, digits, '.', '_', '-' and '/' (got {bad:?})"
        ));
    }
    for component in name.split('/') {
        if component.is_empty() {
            return Err("branch name must not contain empty path segments ('//')".to_string());
        }
        if component.starts_with('.') {
            return Err(format!(
                "branch name segment {component:?} must not start with '.'"
            ));
        }
        if component.ends_with(".lock") {
            return Err("branch name segments must not end with '.lock'".to_string());
        }
    }
    Ok(name.to_string())
}

/// Derive a worktree branch name when the user did not supply one: a slug
/// of the session name when present, otherwise `session-<short-id>`.
pub fn derive_branch_name(session_name: Option<&str>, session_id: &str) -> String {
    if let Some(name) = session_name.map(str::trim).filter(|name| !name.is_empty()) {
        let mut slug = String::new();
        let mut last_dash = true; // suppress a leading dash
        for c in name.chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() {
                slug.push(c);
                last_dash = false;
            } else if !last_dash {
                slug.push('-');
                last_dash = true;
            }
            if slug.len() >= 40 {
                break;
            }
        }
        let slug = slug.trim_matches('-').to_string();
        if !slug.is_empty() {
            return slug;
        }
    }
    let short_id: String = session_id.chars().take(8).collect();
    format!("session-{short_id}")
}

/// First branch name in `base`, `base-2`, `base-3`, … that neither exists
/// as a local branch nor collides with an existing worktree directory.
/// After a bounded scan it falls back to a nanos suffix so the launch can
/// never loop forever on a pathological repo.
pub fn unique_branch_name(project_root: &Path, base: &str) -> String {
    let taken = |candidate: &str| {
        branch_exists(project_root, candidate)
            || project_root
                .join(".intendant")
                .join("worktrees")
                .join(candidate)
                .exists()
    };
    if !taken(base) {
        return base.to_string();
    }
    for n in 2..=50 {
        let candidate = format!("{base}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{base}-{nanos}")
}

pub fn branch_exists(project_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(project_root)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The commit `HEAD` resolves to in `project_root`.
///
/// Doubles as the "can this directory host a session worktree?" preflight:
/// a non-repo directory and a repo with no commits both fail here with an
/// actionable message, before `git worktree add` gets a chance to emit a
/// more cryptic one.
pub fn head_commit(project_root: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project_root)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        let inside_repo = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(project_root)
            .output()
            .map(|out| {
                out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true"
            })
            .unwrap_or(false);
        if !inside_repo {
            return Err(format!(
                "{} is not a git repository — worktree sessions need a git project \
                 (run `git init` there or launch without the worktree option)",
                project_root.display()
            ));
        }
        return Err(format!(
            "{} has no commits yet — a worktree branches from HEAD, so make an \
             initial commit first",
            project_root.display()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The branch currently checked out at `path`, `None` when detached (or
/// not a repo).
pub fn current_branch(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["symbolic-ref", "--short", "-q", "HEAD"])
        .current_dir(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

pub fn create(project_root: &Path, branch: &str, base: &str) -> Result<Worktree, CallerError> {
    let worktree_path = project_root
        .join(".intendant")
        .join("worktrees")
        .join(branch);

    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            branch,
            &worktree_path.to_string_lossy(),
            base,
        ])
        .current_dir(project_root)
        .output()
        .map_err(|e| CallerError::SubAgent(format!("Failed to run git worktree add: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::SubAgent(format!(
            "git worktree add failed: {}",
            stderr.trim()
        )));
    }

    Ok(Worktree {
        branch_name: branch.to_string(),
        path: worktree_path,
        base_branch: base.to_string(),
    })
}

/// Remove a fission-created worktree checkout and force-delete its branch.
///
/// Dashboard cleanup uses `worktree_inventory::remove_worktree_if_safe`,
/// which removes only the checkout after merge/dirty-state checks and leaves
/// the branch ref intact.
pub fn remove_worktree_and_branch(project_root: &Path, wt: &Worktree) -> Result<(), CallerError> {
    let output = Command::new("git")
        .args(["worktree", "remove", &wt.path.to_string_lossy()])
        .current_dir(project_root)
        .output()
        .map_err(|e| CallerError::SubAgent(format!("Failed to run git worktree remove: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::SubAgent(format!(
            "git worktree remove failed: {}",
            stderr.trim()
        )));
    }

    // Clean up the branch
    let branch_delete = Command::new("git")
        .args(["branch", "-D", &wt.branch_name])
        .current_dir(project_root)
        .output();
    match branch_delete {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let branch_ref = format!("refs/heads/{}", wt.branch_name);
            let branch_exists = Command::new("git")
                .args(["show-ref", "--verify", "--quiet", &branch_ref])
                .current_dir(project_root)
                .status()
                .map(|status| status.success())
                .unwrap_or(true);
            if branch_exists {
                eprintln!(
                    "[worktree] git branch -D {} failed: {}",
                    wt.branch_name,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Err(err) => {
            eprintln!(
                "[worktree] failed to run git branch -D {}: {}",
                wt.branch_name, err
            );
        }
    }

    Ok(())
}

/// Merge the worktree branch into the current checkout at `project_root`.
///
/// `current_checkout_label` is used only for diagnostics; this helper does
/// not check out or verify that label before running `git merge`.
pub fn merge(
    project_root: &Path,
    wt: &Worktree,
    current_checkout_label: &str,
) -> Result<MergeResult, CallerError> {
    let output = Command::new("git")
        .args(["merge", &wt.branch_name, "--no-edit"])
        .current_dir(project_root)
        .env("GIT_WORK_TREE", project_root)
        .output()
        .map_err(|e| CallerError::SubAgent(format!("Failed to run git merge: {}", e)))?;

    if output.status.success() {
        Ok(MergeResult::Clean)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        // Abort the failed merge to leave repo in clean state
        let _ = Command::new("git")
            .args(["merge", "--abort"])
            .current_dir(project_root)
            .output();

        Ok(MergeResult::Conflict(format!(
            "Merge conflict merging {} into {}: {} {}",
            wt.branch_name,
            current_checkout_label,
            stdout.trim(),
            stderr.trim()
        )))
    }
}

pub fn list(project_root: &Path) -> Result<Vec<Worktree>, CallerError> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_root)
        .output()
        .map_err(|e| CallerError::SubAgent(format!("Failed to run git worktree list: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::SubAgent(format!(
            "git worktree list failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in stdout.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path_str));
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            // branch refs/heads/branch_name
            let branch_name = branch_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_ref)
                .to_string();
            current_branch = Some(branch_name);
        } else if line.is_empty() {
            if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                worktrees.push(Worktree {
                    branch_name: branch,
                    path,
                    base_branch: String::new(), // not available from list output
                });
            }
        }
    }

    // Handle last entry (may not end with empty line)
    if let (Some(path), Some(branch)) = (current_path, current_branch) {
        worktrees.push(Worktree {
            branch_name: branch,
            path,
            base_branch: String::new(),
        });
    }

    Ok(worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_test_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo)
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(repo)
            .output()
            .unwrap();

        // Create initial commit
        std::fs::write(repo.join("README.md"), "# Test\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .output()
            .unwrap();

        dir
    }

    #[test]
    fn create_worktree() {
        let dir = init_test_repo();
        let repo = dir.path();

        let wt = create(repo, "feature-1", "HEAD").unwrap();
        assert_eq!(wt.branch_name, "feature-1");
        assert!(wt.path.exists());
        assert_eq!(wt.base_branch, "HEAD");

        // Verify the worktree has files
        assert!(wt.path.join("README.md").exists());
    }

    #[test]
    fn create_worktree_duplicate_branch_fails() {
        let dir = init_test_repo();
        let repo = dir.path();

        create(repo, "dup-branch", "HEAD").unwrap();
        let result = create(repo, "dup-branch", "HEAD");
        assert!(result.is_err());
    }

    #[test]
    fn list_worktrees() {
        let dir = init_test_repo();
        let repo = dir.path();

        create(repo, "list-test-1", "HEAD").unwrap();
        create(repo, "list-test-2", "HEAD").unwrap();

        let wts = list(repo).unwrap();
        // Main worktree + 2 created
        assert!(wts.len() >= 3);

        let branch_names: Vec<&str> = wts.iter().map(|w| w.branch_name.as_str()).collect();
        assert!(branch_names.contains(&"list-test-1"));
        assert!(branch_names.contains(&"list-test-2"));
    }

    #[test]
    fn remove_worktree_and_branch_removes_checkout() {
        let dir = init_test_repo();
        let repo = dir.path();

        let wt = create(repo, "to-remove", "HEAD").unwrap();
        assert!(wt.path.exists());

        remove_worktree_and_branch(repo, &wt).unwrap();
        assert!(!wt.path.exists());
    }

    #[test]
    fn merge_clean() {
        let dir = init_test_repo();
        let repo = dir.path();

        let wt = create(repo, "merge-clean", "HEAD").unwrap();

        // Make a change in the worktree
        std::fs::write(wt.path.join("new_file.txt"), "hello\n").unwrap();
        Command::new("git")
            .args(["add", "new_file.txt"])
            .current_dir(&wt.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add new file"])
            .current_dir(&wt.path)
            .output()
            .unwrap();

        // Merge into main
        let result = merge(repo, &wt, "master").unwrap();
        assert_eq!(result, MergeResult::Clean);

        // Verify the file exists in main
        assert!(repo.join("new_file.txt").exists());
    }

    #[test]
    fn merge_conflict() {
        let dir = init_test_repo();
        let repo = dir.path();

        let wt = create(repo, "merge-conflict", "HEAD").unwrap();

        // Modify same file in main
        std::fs::write(repo.join("README.md"), "# Main changes\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main change"])
            .current_dir(repo)
            .output()
            .unwrap();

        // Modify same file in worktree
        std::fs::write(wt.path.join("README.md"), "# Worktree changes\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&wt.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "worktree change"])
            .current_dir(&wt.path)
            .output()
            .unwrap();

        // Merge should detect conflict
        let result = merge(repo, &wt, "master").unwrap();
        match result {
            MergeResult::Conflict(msg) => {
                assert!(msg.contains("merge-conflict"));
            }
            MergeResult::Clean => panic!("Expected conflict"),
        }
    }

    #[test]
    fn validate_branch_name_accepts_sane_names() {
        for name in [
            "feature-1",
            "feat/worktree-sessions",
            "user.branch_2",
            "  padded  ",
        ] {
            let validated = validate_branch_name(name).unwrap();
            assert_eq!(validated, name.trim());
        }
    }

    #[test]
    fn validate_branch_name_rejects_traversal_and_weird_refs() {
        for bad in [
            "",
            "   ",
            "../escape",
            "a/../b",
            "a/..",
            "..",
            ".hidden",
            "a/.hidden",
            "-flag",
            "/rooted",
            "trailing/",
            "double//slash",
            "dot.",
            "ref@{1}",
            "has space",
            "semi;colon",
            "back\\slash",
            "tilde~1",
            "caret^2",
            "colon:ref",
            "quest?ion",
            "star*",
            "brack[et",
            "locky.lock",
            "deep/locky.lock",
        ] {
            assert!(
                validate_branch_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        let too_long = "a".repeat(200);
        assert!(validate_branch_name(&too_long).is_err());
    }

    #[test]
    fn derive_branch_name_slugs_session_name_or_falls_back_to_id() {
        assert_eq!(
            derive_branch_name(Some("Fix the Login Bug!"), "abcd1234-rest"),
            "fix-the-login-bug"
        );
        assert_eq!(
            derive_branch_name(Some("  --- "), "abcd1234-rest"),
            "session-abcd1234"
        );
        assert_eq!(derive_branch_name(None, "abcd1234-rest"), "session-abcd1234");
        // Long names are capped and never end on a dash.
        let long = derive_branch_name(Some(&"word ".repeat(30)), "abcd1234");
        assert!(long.len() <= 40, "{long}");
        assert!(!long.ends_with('-'), "{long}");
    }

    #[test]
    fn unique_branch_name_suffixes_on_collision() {
        let dir = init_test_repo();
        let repo = dir.path();
        assert_eq!(unique_branch_name(repo, "fresh"), "fresh");

        create(repo, "taken", "HEAD").unwrap();
        assert_eq!(unique_branch_name(repo, "taken"), "taken-2");
        create(repo, "taken-2", "HEAD").unwrap();
        assert_eq!(unique_branch_name(repo, "taken"), "taken-3");
    }

    #[test]
    fn head_commit_reports_non_repo_and_empty_repo_clearly() {
        let plain = tempfile::tempdir().unwrap();
        let err = head_commit(plain.path()).unwrap_err();
        assert!(err.contains("not a git repository"), "{err}");

        let empty = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(empty.path())
            .output()
            .unwrap();
        let err = head_commit(empty.path()).unwrap_err();
        assert!(err.contains("no commits yet"), "{err}");

        let dir = init_test_repo();
        let sha = head_commit(dir.path()).unwrap();
        assert_eq!(sha.len(), 40, "{sha}");
    }

    #[test]
    fn current_branch_reads_checkout_branch() {
        let dir = init_test_repo();
        let branch = current_branch(dir.path()).expect("fresh repo is on a branch");
        assert!(!branch.is_empty());
        let wt = create(dir.path(), "branch-probe", "HEAD").unwrap();
        assert_eq!(current_branch(&wt.path).as_deref(), Some("branch-probe"));
    }

    #[test]
    fn full_worktree_lifecycle() {
        let dir = init_test_repo();
        let repo = dir.path();

        // Create
        let wt = create(repo, "lifecycle", "HEAD").unwrap();
        assert!(wt.path.exists());

        // Modify
        std::fs::write(wt.path.join("lifecycle.txt"), "test\n").unwrap();
        Command::new("git")
            .args(["add", "lifecycle.txt"])
            .current_dir(&wt.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "lifecycle change"])
            .current_dir(&wt.path)
            .output()
            .unwrap();

        // List
        let wts = list(repo).unwrap();
        let names: Vec<&str> = wts.iter().map(|w| w.branch_name.as_str()).collect();
        assert!(names.contains(&"lifecycle"));

        // Merge
        let result = merge(repo, &wt, "master").unwrap();
        assert_eq!(result, MergeResult::Clean);

        // Remove
        remove_worktree_and_branch(repo, &wt).unwrap();
        assert!(!wt.path.exists());

        // Verify merged content
        assert!(repo.join("lifecycle.txt").exists());
    }
}

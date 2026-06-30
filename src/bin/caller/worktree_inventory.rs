use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

const MAX_SESSION_ROOTS: usize = 120;
const MAX_DISCOVERY_DIRS_PER_ROOT: usize = 12_000;
const MAX_REPOS: usize = 256;
const MAX_WORKTREES: usize = 1_000;
const MAX_SIZE_ENTRIES_PER_WORKTREE: usize = 75_000;
const MAX_SIZE_ENTRIES_PER_SCAN: usize = 300_000;
const MAX_STATUS_FILES_PER_INSPECT: usize = 300;
const DISCOVERY_DEPTH: usize = 4;
const STALE_DAYS: i64 = 14;

#[derive(Debug, Clone, Default)]
pub struct WorktreeSessionHint {
    pub session_id: String,
    pub source: String,
    pub status: String,
    pub project_root: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorktreeScan {
    pub scanned_at: String,
    pub roots: Vec<WorktreeScanRoot>,
    pub summary: WorktreeSummary,
    pub worktrees: Vec<WorktreeEntry>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeScanRoot {
    pub path: PathBuf,
    pub kind: String,
    pub exists: bool,
    pub repo_count: usize,
    pub truncated: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorktreeSummary {
    pub worktrees: usize,
    pub repos: usize,
    pub total_bytes: u64,
    pub dirty: usize,
    pub unmerged: usize,
    pub active: usize,
    pub stale: usize,
    pub cleanup_candidates: usize,
    pub truncated_sizes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub repo_root: PathBuf,
    pub repo_name: String,
    pub branch: Option<String>,
    pub branch_ref: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub is_main: bool,
    pub head: Option<String>,
    pub head_short: Option<String>,
    pub head_author_time: Option<String>,
    pub head_age_days: Option<i64>,
    pub last_changed_at: Option<String>,
    pub last_changed_age_days: Option<i64>,
    pub default_branch: Option<String>,
    pub default_ahead: i64,
    pub default_behind: i64,
    pub upstream: Option<String>,
    pub ahead: i64,
    pub behind: i64,
    pub merge_status: String,
    pub merged_targets: Vec<String>,
    pub dirty: bool,
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicted: usize,
    pub locked: bool,
    pub locked_reason: Option<String>,
    pub git_prunable: bool,
    pub prunable_reason: Option<String>,
    pub size_bytes: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub size_truncated: bool,
    pub active_sessions: usize,
    pub related_session_count: usize,
    pub related_sessions: Vec<RelatedSession>,
    pub labels: Vec<String>,
    pub safe_to_remove: bool,
    pub recommended_action: String,
    pub safety: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedSession {
    pub session_id: String,
    pub source: String,
    pub status: String,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeRemoveRequest {
    pub repo_root: PathBuf,
    pub path: PathBuf,
    pub expected_head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInspectRequest {
    pub repo_root: PathBuf,
    pub path: PathBuf,
    pub expected_head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInspectResponse {
    pub ok: bool,
    pub entry: WorktreeEntry,
    pub reasons: Vec<WorktreeReviewReason>,
    pub status_files: Vec<WorktreeStatusFile>,
    pub status_truncated: bool,
    pub status_total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeReviewReason {
    pub code: String,
    pub severity: String,
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeStatusFile {
    pub path: String,
    pub original_path: Option<String>,
    pub index_status: String,
    pub worktree_status: String,
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeRemoveResponse {
    pub ok: bool,
    pub path: PathBuf,
    pub repo_root: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub size_bytes: u64,
    pub safety: String,
}

#[derive(Debug, Default)]
struct RawWorktree {
    path: PathBuf,
    head: Option<String>,
    branch_ref: Option<String>,
    detached: bool,
    bare: bool,
    locked: bool,
    locked_reason: Option<String>,
    git_prunable: bool,
    prunable_reason: Option<String>,
}

#[derive(Debug, Default)]
struct StatusInfo {
    branch_head: Option<String>,
    upstream: Option<String>,
    ahead: i64,
    behind: i64,
    staged: usize,
    unstaged: usize,
    untracked: usize,
    conflicted: usize,
}

#[derive(Debug, Default)]
struct WorktreeStatusFiles {
    files: Vec<WorktreeStatusFile>,
    total: usize,
    truncated: bool,
}

#[derive(Debug, Default)]
struct TreeMeasure {
    bytes: u64,
    files: u64,
    dirs: u64,
    latest_mtime: Option<SystemTime>,
    truncated: bool,
}

#[derive(Debug)]
struct SizeBudget {
    remaining_entries: usize,
}

impl SizeBudget {
    fn new(remaining_entries: usize) -> Self {
        Self { remaining_entries }
    }

    fn take_entry(&mut self) -> bool {
        if self.remaining_entries == 0 {
            return false;
        }
        self.remaining_entries -= 1;
        true
    }
}

pub fn empty_scan() -> WorktreeScan {
    WorktreeScan {
        scanned_at: String::new(),
        roots: Vec::new(),
        summary: WorktreeSummary::default(),
        worktrees: Vec::new(),
        errors: Vec::new(),
    }
}

pub fn scan_worktrees(
    home: &Path,
    project_root: Option<&Path>,
    session_hints: &[WorktreeSessionHint],
) -> WorktreeScan {
    scan_worktrees_with_size_budget(home, project_root, session_hints, MAX_SIZE_ENTRIES_PER_SCAN)
}

pub fn inspect_worktree(
    request: WorktreeInspectRequest,
    session_hints: &[WorktreeSessionHint],
) -> Result<WorktreeInspectResponse, String> {
    if !request.repo_root.is_absolute() {
        return Err("repo_root must be an absolute path".to_string());
    }
    if !request.path.is_absolute() {
        return Err("worktree path must be an absolute path".to_string());
    }

    let repo_root = git_repo_root(&request.repo_root).ok_or_else(|| {
        format!(
            "{} is not the root of a Git repository",
            request.repo_root.display()
        )
    })?;
    if !same_path(&repo_root, &request.repo_root) {
        return Err(format!(
            "repo_root resolves to {}; scan again before inspecting",
            repo_root.display()
        ));
    }

    let raw = list_git_worktrees(&repo_root)?
        .into_iter()
        .find(|raw| same_path(&raw.path, &request.path))
        .ok_or_else(|| {
            format!(
                "{} is not registered as a worktree for {}",
                request.path.display(),
                repo_root.display()
            )
        })?;

    let mut default_branches = HashMap::new();
    default_branches.insert(path_key(&repo_root), default_branch_for_repo(&repo_root));
    let mut size_budget = SizeBudget::new(MAX_SIZE_ENTRIES_PER_WORKTREE);
    let entry = enrich_worktree(raw, &default_branches, session_hints, &mut size_budget)?;
    let status = worktree_status_files(&entry.path, MAX_STATUS_FILES_PER_INSPECT)?;
    let mut reasons = worktree_review_reasons(&entry);
    if let Some(expected) = request
        .expected_head
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if entry.head.as_deref() != Some(expected) {
            reasons.push(WorktreeReviewReason {
                code: "head-changed".to_string(),
                severity: "warning".to_string(),
                label: "HEAD changed".to_string(),
                detail: "The worktree HEAD changed since the cached scan; scan again before removing.".to_string(),
            });
        }
    }

    Ok(WorktreeInspectResponse {
        ok: true,
        entry,
        reasons,
        status_files: status.files,
        status_truncated: status.truncated,
        status_total: status.total,
    })
}

fn scan_worktrees_with_size_budget(
    home: &Path,
    project_root: Option<&Path>,
    session_hints: &[WorktreeSessionHint],
    max_size_entries: usize,
) -> WorktreeScan {
    let mut roots = default_scan_roots(home, project_root, session_hints);
    let (repo_roots, mut errors) = discover_repos(&mut roots);
    let mut default_branches: HashMap<String, Option<String>> = HashMap::new();
    let mut raw_entries = Vec::new();
    let mut size_budget = SizeBudget::new(max_size_entries);

    for repo in repo_roots.iter().take(MAX_REPOS) {
        match list_git_worktrees(repo) {
            Ok(mut listed) => {
                let default_branch = default_branch_for_repo(repo);
                default_branches.insert(path_key(repo), default_branch);
                raw_entries.append(&mut listed);
            }
            Err(e) => errors.push(format!("{}: {}", repo.display(), e)),
        }
        if raw_entries.len() >= MAX_WORKTREES {
            errors.push(format!(
                "worktree scan capped at {} entries; narrow scan roots to see more",
                MAX_WORKTREES
            ));
            raw_entries.truncate(MAX_WORKTREES);
            break;
        }
    }

    let mut seen_worktrees = HashSet::new();
    let mut worktrees = Vec::new();
    for raw in raw_entries {
        if !seen_worktrees.insert(path_key(&raw.path)) {
            continue;
        }
        match enrich_worktree(raw, &default_branches, session_hints, &mut size_budget) {
            Ok(entry) => worktrees.push(entry),
            Err(e) => errors.push(e),
        }
    }

    worktrees.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut summary = WorktreeSummary {
        worktrees: worktrees.len(),
        repos: repo_roots.len(),
        ..WorktreeSummary::default()
    };
    for wt in &worktrees {
        summary.total_bytes = summary.total_bytes.saturating_add(wt.size_bytes);
        if wt.dirty {
            summary.dirty += 1;
        }
        if wt.merge_status == "unmerged" {
            summary.unmerged += 1;
        }
        if wt.active_sessions > 0 {
            summary.active += 1;
        }
        if wt.labels.iter().any(|l| l == "stale") {
            summary.stale += 1;
        }
        if wt.safe_to_remove {
            summary.cleanup_candidates += 1;
        }
        if wt.size_truncated {
            summary.truncated_sizes += 1;
        }
    }

    WorktreeScan {
        scanned_at: chrono::Utc::now().to_rfc3339(),
        roots,
        summary,
        worktrees,
        errors,
    }
}

pub fn remove_worktree_if_safe(
    request: WorktreeRemoveRequest,
    session_hints: &[WorktreeSessionHint],
) -> Result<WorktreeRemoveResponse, String> {
    if !request.repo_root.is_absolute() {
        return Err("repo_root must be an absolute path".to_string());
    }
    if !request.path.is_absolute() {
        return Err("worktree path must be an absolute path".to_string());
    }

    let repo_root = git_repo_root(&request.repo_root).ok_or_else(|| {
        format!(
            "{} is not the root of a Git repository",
            request.repo_root.display()
        )
    })?;
    if !same_path(&repo_root, &request.repo_root) {
        return Err(format!(
            "repo_root resolves to {}; scan again before removing",
            repo_root.display()
        ));
    }

    let raw = list_git_worktrees(&repo_root)?
        .into_iter()
        .find(|raw| same_path(&raw.path, &request.path))
        .ok_or_else(|| {
            format!(
                "{} is not registered as a worktree for {}",
                request.path.display(),
                repo_root.display()
            )
        })?;

    let mut default_branches = HashMap::new();
    default_branches.insert(path_key(&repo_root), default_branch_for_repo(&repo_root));
    let mut size_budget = SizeBudget::new(MAX_SIZE_ENTRIES_PER_WORKTREE);
    let entry = enrich_worktree(raw, &default_branches, session_hints, &mut size_budget)?;

    if let Some(expected) = request
        .expected_head
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if entry.head.as_deref() != Some(expected) {
            return Err(
                "worktree HEAD changed since the last scan; scan again before removing".to_string(),
            );
        }
    }

    if !entry.safe_to_remove {
        return Err(format!("safety check refused removal: {}", entry.safety));
    }

    let output = Command::new("git")
        .arg("-c")
        .arg("color.ui=false")
        .args(["worktree", "remove"])
        .arg(&entry.path)
        .current_dir(&entry.repo_root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "git worktree remove failed".to_string()
        };
        return Err(detail);
    }

    Ok(WorktreeRemoveResponse {
        ok: true,
        path: entry.path,
        repo_root: entry.repo_root,
        branch: entry.branch,
        head: entry.head,
        size_bytes: entry.size_bytes,
        safety: entry.safety,
    })
}

fn default_scan_roots(
    home: &Path,
    project_root: Option<&Path>,
    session_hints: &[WorktreeSessionHint],
) -> Vec<WorktreeScanRoot> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |path: PathBuf, kind: &str| {
        if path.as_os_str().is_empty() {
            return;
        }
        if kind.starts_with("session") && should_skip_session_root(home, &path) {
            return;
        }
        let key = path_key(&path);
        if !seen.insert(key) {
            return;
        }
        let exists = path.exists();
        roots.push(WorktreeScanRoot {
            path,
            kind: kind.to_string(),
            exists,
            repo_count: 0,
            truncated: false,
            error: None,
        });
    };

    if let Some(root) = project_root {
        add(root.to_path_buf(), "current-project");
    }

    for hint in session_hints.iter().take(MAX_SESSION_ROOTS) {
        if let Some(path) = hint.project_root.as_ref() {
            add(path.clone(), "session-project");
        }
        if let Some(path) = hint.cwd.as_ref() {
            add(path.clone(), "session-cwd");
        }
    }

    add(home.join("projects"), "common-projects");
    add(home.join(".intendant").join("worktrees"), "common");
    add(home.join(".codex").join("worktrees"), "common");
    add(home.join(".claude").join("worktrees"), "common");

    roots
}

fn discover_repos(roots: &mut [WorktreeScanRoot]) -> (Vec<PathBuf>, Vec<String>) {
    let mut repos = Vec::new();
    let mut seen = HashSet::new();
    let mut errors = Vec::new();

    for root in roots {
        if !root.exists {
            continue;
        }
        if let Some(repo) = git_repo_root(&root.path) {
            if seen.insert(path_key(&repo)) {
                root.repo_count += 1;
                repos.push(repo);
            }
            continue;
        }
        if !root.path.is_dir() {
            continue;
        }

        let before = repos.len();
        let mut visited = 0usize;
        let mut stack = vec![(root.path.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            visited += 1;
            if visited >= MAX_DISCOVERY_DIRS_PER_ROOT {
                root.truncated = true;
                break;
            }
            if depth > DISCOVERY_DEPTH {
                continue;
            }
            if has_git_marker(&dir) {
                if let Some(repo) = git_repo_root(&dir) {
                    if seen.insert(path_key(&repo)) {
                        repos.push(repo);
                    }
                }
                continue;
            }
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) => {
                    if root.error.is_none() {
                        root.error = Some(e.to_string());
                    }
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if should_skip_discovery_dir(&name) {
                    continue;
                }
                if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                    stack.push((path, depth + 1));
                }
            }
        }
        root.repo_count += repos.len().saturating_sub(before);
        if root.truncated {
            errors.push(format!(
                "{}: discovery capped at {} directories",
                root.path.display(),
                MAX_DISCOVERY_DIRS_PER_ROOT
            ));
        }
        if repos.len() >= MAX_REPOS {
            errors.push(format!(
                "repository discovery capped at {} repositories",
                MAX_REPOS
            ));
            repos.truncate(MAX_REPOS);
            break;
        }
    }

    (repos, errors)
}

fn enrich_worktree(
    raw: RawWorktree,
    default_branches: &HashMap<String, Option<String>>,
    session_hints: &[WorktreeSessionHint],
    size_budget: &mut SizeBudget,
) -> Result<WorktreeEntry, String> {
    let repo_root = git_common_repo_root(&raw.path).unwrap_or_else(|| {
        git_repo_root(&raw.path)
            .or_else(|| raw.path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| raw.path.clone())
    });
    let repo_key = path_key(&repo_root);
    let default_branch = default_branches
        .get(&repo_key)
        .cloned()
        .unwrap_or_else(|| default_branch_for_repo(&repo_root));
    let status = if raw.path.is_dir() {
        status_info(&raw.path).unwrap_or_default()
    } else {
        StatusInfo::default()
    };
    let branch_ref = raw.branch_ref.clone();
    let branch = branch_ref
        .as_deref()
        .and_then(|r| r.strip_prefix("refs/heads/").or(Some(r)))
        .map(ToString::to_string)
        .or_else(|| status.branch_head.clone().filter(|b| b != "(detached)"));

    let mut target_refs: Vec<String> = Vec::new();
    if let Some(default_branch) = default_branch.as_ref() {
        target_refs.push(default_branch.clone());
    }
    if let Some(upstream) = status.upstream.as_ref() {
        if !target_refs.iter().any(|t| t == upstream) {
            target_refs.push(upstream.clone());
        }
    }

    let is_main = raw.path == repo_root || same_path(&raw.path, &repo_root);
    let (default_ahead, default_behind) = match (raw.head.as_ref(), default_branch.as_ref()) {
        (Some(head), Some(default_branch)) if git_ref_exists(&repo_root, default_branch) => {
            git_ahead_behind(&repo_root, head, default_branch).unwrap_or((0, 0))
        }
        _ => (0, 0),
    };
    let mut merged_targets = Vec::new();
    if let Some(head) = raw.head.as_ref() {
        for target in &target_refs {
            if git_ref_exists(&repo_root, target) && git_is_ancestor(&repo_root, head, target) {
                merged_targets.push(target.clone());
            }
        }
    }

    let merge_status = if raw.git_prunable {
        "prunable"
    } else if target_refs.is_empty() || raw.head.is_none() {
        "unknown"
    } else if merged_targets.is_empty() {
        "unmerged"
    } else {
        "merged"
    }
    .to_string();

    let tree = if raw.path.is_dir() && !is_main {
        measure_tree_with_budget(&raw.path, size_budget)
    } else {
        TreeMeasure::default()
    };
    let head_author_secs = if raw.path.is_dir() {
        git_i64(&raw.path, &["log", "-1", "--format=%ct"]).ok()
    } else {
        None
    };
    let now = chrono::Utc::now().timestamp();
    let head_age_days = head_author_secs.map(|secs| seconds_to_days(now.saturating_sub(secs)));
    let head_author_time = head_author_secs.map(epoch_to_rfc3339);
    let last_changed_age_days = tree
        .latest_mtime
        .and_then(system_time_secs)
        .map(|secs| seconds_to_days(now.saturating_sub(secs)));
    let last_changed_at = tree.latest_mtime.map(system_time_to_rfc3339);

    let related = related_sessions(&raw.path, session_hints);
    let active_sessions = related
        .iter()
        .filter(|s| is_active_session_status(&s.status))
        .count();
    let dirty =
        status.staged > 0 || status.unstaged > 0 || status.untracked > 0 || status.conflicted > 0;

    let stale = head_age_days
        .or(last_changed_age_days)
        .map(|days| days >= STALE_DAYS)
        .unwrap_or(false);
    let safe_to_remove = !is_main
        && active_sessions == 0
        && !raw.locked
        && !dirty
        && (merge_status == "merged" || raw.git_prunable);

    let mut labels = Vec::new();
    if is_main {
        labels.push("main".to_string());
    }
    if raw.locked {
        labels.push("locked".to_string());
    }
    if active_sessions > 0 {
        labels.push("active".to_string());
    }
    if dirty {
        labels.push("dirty".to_string());
    }
    if status.untracked > 0 {
        labels.push("untracked".to_string());
    }
    if status.conflicted > 0 {
        labels.push("conflicts".to_string());
    }
    if merge_status == "merged" {
        labels.push("merged".to_string());
    } else if merge_status == "unmerged" {
        labels.push("unmerged".to_string());
    } else if merge_status == "unknown" {
        labels.push("unknown-merge".to_string());
    }
    if stale && !is_main && active_sessions == 0 {
        labels.push("stale".to_string());
    }
    if raw.git_prunable {
        labels.push("git-prunable".to_string());
    }
    if safe_to_remove {
        labels.push("cleanup-candidate".to_string());
    }

    let safety = safety_text(
        is_main,
        active_sessions,
        raw.locked,
        raw.locked_reason.as_deref(),
        dirty,
        &merge_status,
        &merged_targets,
        raw.git_prunable,
    );
    let recommended_action = if is_main || active_sessions > 0 || raw.locked {
        "keep"
    } else if safe_to_remove {
        "remove-candidate"
    } else {
        "review"
    }
    .to_string();

    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| repo_root.display().to_string());
    let head_short = raw
        .head
        .as_deref()
        .map(|h| h.chars().take(12).collect::<String>());

    Ok(WorktreeEntry {
        path: raw.path,
        repo_root,
        repo_name,
        branch,
        branch_ref,
        detached: raw.detached,
        bare: raw.bare,
        is_main,
        head: raw.head,
        head_short,
        head_author_time,
        head_age_days,
        last_changed_at,
        last_changed_age_days,
        default_branch,
        default_ahead,
        default_behind,
        upstream: status.upstream,
        ahead: status.ahead,
        behind: status.behind,
        merge_status,
        merged_targets,
        dirty,
        staged: status.staged,
        unstaged: status.unstaged,
        untracked: status.untracked,
        conflicted: status.conflicted,
        locked: raw.locked,
        locked_reason: raw.locked_reason,
        git_prunable: raw.git_prunable,
        prunable_reason: raw.prunable_reason,
        size_bytes: tree.bytes,
        file_count: tree.files,
        dir_count: tree.dirs,
        size_truncated: tree.truncated,
        active_sessions,
        related_session_count: related.len(),
        related_sessions: related.into_iter().take(8).collect(),
        labels,
        safe_to_remove,
        recommended_action,
        safety,
    })
}

fn safety_text(
    is_main: bool,
    active_sessions: usize,
    locked: bool,
    locked_reason: Option<&str>,
    dirty: bool,
    merge_status: &str,
    merged_targets: &[String],
    git_prunable: bool,
) -> String {
    if is_main {
        return "Main worktree for this repository; keep it.".to_string();
    }
    if locked {
        return match locked_reason {
            Some(reason) if !reason.is_empty() => {
                format!("Git marks this worktree locked: {reason}")
            }
            _ => "Git marks this worktree locked.".to_string(),
        };
    }
    if active_sessions > 0 {
        return format!("Linked to {active_sessions} active session(s).");
    }
    if dirty {
        return "Has local changes, untracked files, or conflicts.".to_string();
    }
    if git_prunable {
        return "Git says this worktree metadata is prunable.".to_string();
    }
    if merge_status == "merged" {
        let targets = if merged_targets.is_empty() {
            "a configured target".to_string()
        } else {
            merged_targets.join(", ")
        };
        return format!("Clean and HEAD is reachable from {targets}.");
    }
    if merge_status == "unmerged" {
        return "HEAD is not reachable from the default branch or upstream.".to_string();
    }
    "Merge status is unknown; review manually.".to_string()
}

fn review_reason(
    code: &str,
    severity: &str,
    label: &str,
    detail: impl Into<String>,
) -> WorktreeReviewReason {
    WorktreeReviewReason {
        code: code.to_string(),
        severity: severity.to_string(),
        label: label.to_string(),
        detail: detail.into(),
    }
}

fn worktree_review_reasons(entry: &WorktreeEntry) -> Vec<WorktreeReviewReason> {
    let mut reasons = Vec::new();
    if entry.is_main {
        reasons.push(review_reason(
            "main",
            "keep",
            "Main worktree",
            "This is the repository's main checkout.",
        ));
    }
    if entry.locked {
        reasons.push(review_reason(
            "locked",
            "warning",
            "Locked by Git",
            entry
                .locked_reason
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("Git marks this worktree locked."),
        ));
    }
    if entry.active_sessions > 0 {
        reasons.push(review_reason(
            "active-sessions",
            "warning",
            "Active sessions",
            format!("{} active session(s) are linked to this worktree.", entry.active_sessions),
        ));
    }
    if entry.conflicted > 0 {
        reasons.push(review_reason(
            "conflicts",
            "danger",
            "Conflicted files",
            format!("{} file(s) have unresolved conflicts.", entry.conflicted),
        ));
    }
    if entry.staged > 0 || entry.unstaged > 0 {
        reasons.push(review_reason(
            "tracked-changes",
            "warning",
            "Tracked changes",
            format!(
                "{} staged and {} unstaged tracked file(s).",
                entry.staged, entry.unstaged
            ),
        ));
    }
    if entry.untracked > 0 {
        reasons.push(review_reason(
            "untracked",
            "warning",
            "Untracked files",
            format!("{} untracked file(s).", entry.untracked),
        ));
    }
    match entry.merge_status.as_str() {
        "unmerged" => reasons.push(review_reason(
            "unmerged",
            "warning",
            "Not merged",
            "HEAD is not reachable from the default branch or upstream.",
        )),
        "unknown" => reasons.push(review_reason(
            "unknown-merge",
            "warning",
            "Unknown merge state",
            "Merge status is unknown; review manually.",
        )),
        "prunable" => reasons.push(review_reason(
            "git-prunable",
            "ok",
            "Prunable metadata",
            entry
                .prunable_reason
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("Git says this worktree metadata is prunable."),
        )),
        _ => {}
    }
    if entry.size_truncated {
        reasons.push(review_reason(
            "size-truncated",
            "warning",
            "Large tree",
            "Disk usage scan was capped for this worktree.",
        ));
    }
    if entry.safe_to_remove {
        reasons.push(review_reason(
            "ready",
            "ok",
            "Ready to remove",
            entry.safety.clone(),
        ));
    } else if reasons.is_empty() {
        reasons.push(review_reason(
            "review",
            "warning",
            "Review manually",
            entry.safety.clone(),
        ));
    }
    reasons
}

fn list_git_worktrees(repo: &Path) -> Result<Vec<RawWorktree>, String> {
    let output = git_string(repo, &["worktree", "list", "--porcelain"])?;
    let mut out = Vec::new();
    let mut cur = RawWorktree::default();
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if !cur.path.as_os_str().is_empty() {
                out.push(cur);
                cur = RawWorktree::default();
            }
            cur.path = PathBuf::from(path);
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            cur.head = Some(head.to_string());
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            cur.branch_ref = Some(branch_ref.to_string());
        } else if line == "detached" {
            cur.detached = true;
        } else if line == "bare" {
            cur.bare = true;
        } else if let Some(reason) = line.strip_prefix("locked") {
            cur.locked = true;
            let reason = reason.trim();
            if !reason.is_empty() {
                cur.locked_reason = Some(reason.to_string());
            }
        } else if let Some(reason) = line.strip_prefix("prunable") {
            cur.git_prunable = true;
            let reason = reason.trim();
            if !reason.is_empty() {
                cur.prunable_reason = Some(reason.to_string());
            }
        } else if line.trim().is_empty() && !cur.path.as_os_str().is_empty() {
            out.push(cur);
            cur = RawWorktree::default();
        }
    }
    if !cur.path.as_os_str().is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn status_info(path: &Path) -> Result<StatusInfo, String> {
    let output = git_string(
        path,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ],
    )?;
    let mut info = StatusInfo::default();
    for line in output.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            info.branch_head = Some(head.to_string());
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            info.upstream = Some(upstream.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    info.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    info.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("? ") {
            info.untracked += 1;
        } else if line.starts_with("u ") {
            info.conflicted += 1;
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let bytes = line.as_bytes();
            if bytes.len() >= 4 {
                if bytes[2] != b'.' {
                    info.staged += 1;
                }
                if bytes[3] != b'.' {
                    info.unstaged += 1;
                }
            }
        }
    }
    Ok(info)
}

fn worktree_status_files(path: &Path, limit: usize) -> Result<WorktreeStatusFiles, String> {
    let output = git_string(
        path,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
            "--renames",
        ],
    )?;
    let mut out = WorktreeStatusFiles::default();
    for line in output.lines() {
        if line.len() < 3 {
            continue;
        }
        out.total += 1;
        if out.files.len() >= limit {
            out.truncated = true;
            continue;
        }
        let bytes = line.as_bytes();
        let x = bytes[0] as char;
        let y = bytes[1] as char;
        let raw_path = line[3..].to_string();
        let (path, original_path) = split_porcelain_rename_path(raw_path);
        out.files.push(WorktreeStatusFile {
            path,
            original_path,
            index_status: x.to_string(),
            worktree_status: y.to_string(),
            category: status_file_category(x, y).to_string(),
        });
    }
    Ok(out)
}

fn split_porcelain_rename_path(path: String) -> (String, Option<String>) {
    if let Some((old, new)) = path.split_once(" -> ") {
        return (new.to_string(), Some(old.to_string()));
    }
    (path, None)
}

fn status_file_category(index: char, worktree: char) -> &'static str {
    if index == '?' && worktree == '?' {
        return "untracked";
    }
    if is_conflicted_status(index, worktree) {
        return "conflicted";
    }
    let staged = index != ' ' && index != '.' && index != '?' && index != '!';
    let unstaged = worktree != ' ' && worktree != '.' && worktree != '?' && worktree != '!';
    match (staged, unstaged) {
        (true, true) => "staged+unstaged",
        (true, false) => "staged",
        (false, true) => "unstaged",
        (false, false) => "clean",
    }
}

fn is_conflicted_status(index: char, worktree: char) -> bool {
    matches!(
        (index, worktree),
        ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A')
    ) || index == 'U'
        || worktree == 'U'
}

fn default_branch_for_repo(repo: &Path) -> Option<String> {
    if let Ok(origin_head) = git_string(
        repo,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        let trimmed = origin_head.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    for candidate in ["main", "master"] {
        if git_ref_exists(repo, candidate) {
            return Some(candidate.to_string());
        }
    }
    git_string(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn git_repo_root(path: &Path) -> Option<PathBuf> {
    git_string(path, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
}

fn git_common_repo_root(path: &Path) -> Option<PathBuf> {
    git_string(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .and_then(|s| {
        let git_dir = PathBuf::from(s.trim());
        git_dir.parent().map(Path::to_path_buf)
    })
}

fn git_ref_exists(repo: &Path, target: &str) -> bool {
    git_status(repo, &["rev-parse", "--verify", "--quiet", target])
}

fn git_is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    git_status(repo, &["merge-base", "--is-ancestor", ancestor, descendant])
}

fn git_ahead_behind(repo: &Path, left: &str, right: &str) -> Option<(i64, i64)> {
    let range = format!("{left}...{right}");
    let output = git_string(repo, &["rev-list", "--left-right", "--count", &range]).ok()?;
    let mut parts = output.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

fn git_i64(path: &Path, args: &[&str]) -> Result<i64, String> {
    git_string(path, args)?
        .trim()
        .parse::<i64>()
        .map_err(|e| e.to_string())
}

fn git_status(path: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-c")
        .arg("color.ui=false")
        .args(args)
        .current_dir(path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_string(path: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-c")
        .arg("color.ui=false")
        .args(args)
        .current_dir(path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
fn measure_tree(root: &Path) -> TreeMeasure {
    let mut budget = SizeBudget::new(MAX_SIZE_ENTRIES_PER_WORKTREE);
    measure_tree_with_budget(root, &mut budget)
}

fn measure_tree_with_budget(root: &Path, size_budget: &mut SizeBudget) -> TreeMeasure {
    let mut measure = TreeMeasure::default();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    // Track inodes of multiply-linked files so each is counted once.
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();
    while let Some(path) = stack.pop() {
        if visited >= MAX_SIZE_ENTRIES_PER_WORKTREE || !size_budget.take_entry() {
            measure.truncated = true;
            break;
        }
        visited += 1;
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if let Ok(modified) = meta.modified() {
            measure.latest_mtime = Some(match measure.latest_mtime {
                Some(prev) if prev > modified => prev,
                _ => modified,
            });
        }
        if meta.is_dir() {
            measure.dirs += 1;
            if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                continue;
            }
            let entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                stack.push(entry.path());
            }
        } else {
            measure.files += 1;
            // Count actual on-disk allocation (512-byte blocks) and de-duplicate
            // hardlinked files by (dev, ino) so a single inode with N links is
            // counted once — matching `du` and reflecting the space actually
            // reclaimed by deleting the worktree. `meta.len()` (apparent size)
            // over-counts both sparse files and hardlink-dense trees like Cargo
            // `target/` dirs (e.g. a 5.6 GiB worktree reported as 9+ GiB).
            if crate::platform::metadata_is_multiply_linked(&meta)
                && !seen_inodes.insert(crate::platform::metadata_dev_ino(&meta))
            {
                continue;
            }
            measure.bytes = measure
                .bytes
                .saturating_add(crate::platform::metadata_on_disk_bytes(&meta));
        }
    }
    measure
}

fn related_sessions(path: &Path, session_hints: &[WorktreeSessionHint]) -> Vec<RelatedSession> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for hint in session_hints {
        if hint.session_id.is_empty() {
            continue;
        }
        let related = hint
            .cwd
            .as_ref()
            .map(|cwd| is_path_within(cwd, path))
            .unwrap_or(false)
            || hint
                .project_root
                .as_ref()
                .map(|root| same_path(root, path))
                .unwrap_or(false);
        if !related || !seen.insert(hint.session_id.clone()) {
            continue;
        }
        out.push(RelatedSession {
            session_id: hint.session_id.clone(),
            source: hint.source.clone(),
            status: hint.status.clone(),
            updated_at: hint.updated_at.clone(),
        });
    }
    out
}

fn has_git_marker(dir: &Path) -> bool {
    dir.join(".git").exists()
}

fn should_skip_discovery_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | ".cache"
            | ".cargo"
            | ".rustup"
            | ".worktrees"
            | ".Trash"
            | "Library"
            | "Applications"
            | "Downloads"
    )
}

fn should_skip_session_root(home: &Path, path: &Path) -> bool {
    if same_path(path, home) {
        return true;
    }
    if path.parent().is_none() {
        return true;
    }
    let s = path.to_string_lossy();
    if matches!(s.as_ref(), "/tmp" | "/private/tmp" | "/var/tmp" | "/") {
        return true;
    }
    s.starts_with("/private/var/folders/") || s.starts_with("/var/folders/")
}

fn is_active_session_status(status: &str) -> bool {
    matches!(status, "running" | "in_progress" | "thinking")
}

fn is_path_within(path: &Path, root: &Path) -> bool {
    let child_key = path_key(path);
    let root_key = path_key(root);
    child_key == root_key || child_key.starts_with(&(root_key + "/"))
}

fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

fn path_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string()
}

fn system_time_secs(time: SystemTime) -> Option<i64> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn system_time_to_rfc3339(time: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = time.into();
    dt.to_rfc3339()
}

fn epoch_to_rfc3339(secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

fn seconds_to_days(secs: i64) -> i64 {
    (secs / 86_400).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo_at(&repo);
        tmp
    }

    fn tempdir_in_target() -> tempfile::TempDir {
        let base = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("worktree-inventory-tests");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("scan-")
            .tempdir_in(base)
            .unwrap()
    }

    fn init_repo_at(repo: &Path) {
        std::fs::create_dir_all(repo).unwrap();
        git(&repo, &["init"]);
        git(&repo, &["checkout", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test User"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "initial"]);
    }

    fn repo_path(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("repo")
    }

    fn canonical_child_path(path: &Path) -> PathBuf {
        path.parent()
            .and_then(|parent| parent.canonicalize().ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    }

    #[test]
    fn scan_marks_clean_merged_worktree_as_candidate() {
        let tmp = init_repo();
        let repo = repo_path(&tmp);
        let wt = tmp.path().join("clean-worktree");
        let wt_str = wt.to_string_lossy().to_string();
        git(
            &repo,
            &["worktree", "add", "-b", "cleanup", &wt_str, "main"],
        );

        let scan = scan_worktrees(tmp.path(), Some(&repo), &[]);
        let found = scan
            .worktrees
            .iter()
            .find(|entry| entry.branch.as_deref() == Some("cleanup"))
            .expect("cleanup worktree found");

        assert_eq!(found.merge_status, "merged");
        assert!(!found.dirty);
        assert!(found.safe_to_remove);
        assert!(found.labels.iter().any(|l| l == "cleanup-candidate"));
    }

    #[test]
    fn scan_marks_dirty_worktree_as_not_safe() {
        let tmp = init_repo();
        let repo = repo_path(&tmp);
        let wt = tmp.path().join("dirty-worktree");
        let wt_str = wt.to_string_lossy().to_string();
        git(&repo, &["worktree", "add", "-b", "dirty", &wt_str, "main"]);
        std::fs::write(wt.join("scratch.txt"), "local\n").unwrap();

        let scan = scan_worktrees(tmp.path(), Some(&repo), &[]);
        let found = scan
            .worktrees
            .iter()
            .find(|entry| entry.branch.as_deref() == Some("dirty"))
            .expect("dirty worktree found");

        assert!(found.dirty);
        assert_eq!(found.untracked, 1);
        assert!(!found.safe_to_remove);
        assert!(found.safety.contains("local changes") || found.safety.contains("untracked"));
    }

    #[test]
    fn inspect_dirty_worktree_reports_reasons_and_files() {
        let tmp = init_repo();
        let repo = repo_path(&tmp);
        let wt = tmp.path().join("inspect-dirty-worktree");
        let wt_str = wt.to_string_lossy().to_string();
        git(
            &repo,
            &["worktree", "add", "-b", "inspect-dirty", &wt_str, "main"],
        );
        std::fs::write(wt.join("README.md"), "changed\n").unwrap();
        std::fs::write(wt.join("scratch.txt"), "local\n").unwrap();

        let inspected = inspect_worktree(
            WorktreeInspectRequest {
                repo_root: repo.clone(),
                path: wt.clone(),
                expected_head: None,
            },
            &[],
        )
        .expect("dirty worktree inspected");

        assert!(!inspected.entry.safe_to_remove);
        assert!(inspected
            .reasons
            .iter()
            .any(|reason| reason.code == "tracked-changes"));
        assert!(inspected
            .reasons
            .iter()
            .any(|reason| reason.code == "untracked"));
        assert!(inspected.status_files.iter().any(|file| {
            file.path == "README.md" && file.category == "unstaged"
        }));
        assert!(inspected.status_files.iter().any(|file| {
            file.path == "scratch.txt" && file.category == "untracked"
        }));
    }

    #[test]
    fn scan_reports_default_branch_divergence() {
        let tmp = init_repo();
        let repo = repo_path(&tmp);
        let wt = tmp.path().join("behind-default-worktree");
        let wt_str = wt.to_string_lossy().to_string();
        git(
            &repo,
            &["worktree", "add", "-b", "behind-default", &wt_str, "main"],
        );
        std::fs::write(repo.join("README.md"), "hello\nnew main work\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "advance main"]);

        let scan = scan_worktrees(tmp.path(), Some(&repo), &[]);
        let found = scan
            .worktrees
            .iter()
            .find(|entry| entry.branch.as_deref() == Some("behind-default"))
            .expect("behind-default worktree found");

        assert_eq!(found.default_branch.as_deref(), Some("main"));
        assert_eq!(found.default_ahead, 0);
        assert_eq!(found.default_behind, 1);
        assert_eq!(found.merge_status, "merged");
    }

    #[test]
    fn scan_discovers_agent_observed_repo_worktrees_from_session_hint() {
        let tmp = tempdir_in_target();
        let projects = tmp.path().join("projects");
        let current = projects.join("intendant");
        let sibling = projects.join("codex");
        init_repo_at(&current);
        init_repo_at(&sibling);

        let wt_parent = sibling.join(".worktrees");
        std::fs::create_dir_all(&wt_parent).unwrap();
        let wt = wt_parent.join("vanilla-upstream");
        let wt_str = wt.to_string_lossy().to_string();
        git(&sibling, &["worktree", "add", "--detach", &wt_str, "main"]);

        let hints = vec![WorktreeSessionHint {
            session_id: "codex-session".to_string(),
            source: "codex".to_string(),
            status: "external".to_string(),
            project_root: Some(sibling.clone()),
            cwd: Some(sibling.clone()),
            updated_at: None,
        }];
        let scan = scan_worktrees(tmp.path(), Some(&current), &hints);
        assert!(scan.roots.iter().any(|root| {
            root.kind == "session-project"
                && same_path(&root.path, &sibling)
                && root.repo_count >= 1
        }));
        let found = scan
            .worktrees
            .iter()
            .find(|entry| same_path(&entry.path, &wt))
            .expect("sibling repo worktree found");

        assert_eq!(found.repo_name, "codex");
        assert!(found.detached);
    }

    #[test]
    fn scan_discovers_sibling_project_repo_worktrees_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let current = projects.join("intendant");
        let sibling = projects.join("codex");
        init_repo_at(&current);
        init_repo_at(&sibling);

        let wt_parent = sibling.join(".worktrees");
        std::fs::create_dir_all(&wt_parent).unwrap();
        let wt = wt_parent.join("minimal-lineage-upstream");
        let wt_str = wt.to_string_lossy().to_string();
        git(
            &sibling,
            &["worktree", "add", "-b", "minimal-lineage", &wt_str, "main"],
        );

        let scan = scan_worktrees(tmp.path(), Some(&current), &[]);
        assert!(scan.roots.iter().any(|root| {
            root.kind == "common-projects"
                && same_path(&root.path, &projects)
                && root.repo_count >= 1
        }));
        let found = scan
            .worktrees
            .iter()
            .find(|entry| same_path(&entry.path, &wt))
            .expect("sibling project worktree found");

        assert_eq!(found.repo_name, "codex");
        assert_eq!(found.branch.as_deref(), Some("minimal-lineage"));
    }

    #[test]
    fn scan_keeps_worktrees_when_size_budget_is_exhausted() {
        let tmp = init_repo();
        let repo = repo_path(&tmp);
        let first = tmp.path().join("first-worktree");
        let second = tmp.path().join("second-worktree");
        let first_str = first.to_string_lossy().to_string();
        let second_str = second.to_string_lossy().to_string();
        git(
            &repo,
            &["worktree", "add", "-b", "first", &first_str, "main"],
        );
        git(
            &repo,
            &["worktree", "add", "-b", "second", &second_str, "main"],
        );

        let scan = scan_worktrees_with_size_budget(tmp.path(), Some(&repo), &[], 1);
        let first_found = scan
            .worktrees
            .iter()
            .find(|entry| same_path(&entry.path, &first))
            .expect("first worktree found");
        let second_found = scan
            .worktrees
            .iter()
            .find(|entry| same_path(&entry.path, &second))
            .expect("second worktree found");

        assert!(first_found.size_truncated);
        assert!(second_found.size_truncated);
        assert!(scan.summary.truncated_sizes >= 2);
    }

    #[test]
    fn related_active_sessions_block_cleanup() {
        let tmp = init_repo();
        let repo = repo_path(&tmp);
        let wt = tmp.path().join("active-worktree");
        let wt_str = wt.to_string_lossy().to_string();
        git(&repo, &["worktree", "add", "-b", "active", &wt_str, "main"]);
        let hints = vec![WorktreeSessionHint {
            session_id: "session-1".to_string(),
            source: "codex".to_string(),
            status: "in_progress".to_string(),
            project_root: Some(wt.clone()),
            cwd: Some(wt.join("src")),
            updated_at: None,
        }];

        let scan = scan_worktrees(tmp.path(), Some(&repo), &hints);
        let found = scan
            .worktrees
            .iter()
            .find(|entry| entry.branch.as_deref() == Some("active"))
            .expect("active worktree found");

        assert_eq!(found.active_sessions, 1);
        assert!(!found.safe_to_remove);
        assert!(found.labels.iter().any(|l| l == "active"));
    }

    #[test]
    fn remove_safe_worktree_deletes_clean_merged_worktree() {
        let tmp = init_repo();
        let repo = repo_path(&tmp);
        let wt = tmp.path().join("remove-worktree");
        let wt_str = wt.to_string_lossy().to_string();
        git(
            &repo,
            &["worktree", "add", "-b", "remove-me", &wt_str, "main"],
        );

        let scan = scan_worktrees(tmp.path(), Some(&repo), &[]);
        let found = scan
            .worktrees
            .iter()
            .find(|entry| entry.branch.as_deref() == Some("remove-me"))
            .expect("remove-me worktree found");
        assert!(found.safe_to_remove);

        let response = remove_worktree_if_safe(
            WorktreeRemoveRequest {
                repo_root: repo.clone(),
                path: wt.clone(),
                expected_head: found.head.clone(),
            },
            &[],
        )
        .expect("safe worktree removed");

        assert!(response.ok);
        assert_eq!(
            canonical_child_path(&response.path),
            canonical_child_path(&wt)
        );
        assert!(!wt.exists());
        let scan = scan_worktrees(tmp.path(), Some(&repo), &[]);
        assert!(!scan
            .worktrees
            .iter()
            .any(|entry| entry.branch.as_deref() == Some("remove-me")));
    }

    #[test]
    fn remove_dirty_worktree_is_refused() {
        let tmp = init_repo();
        let repo = repo_path(&tmp);
        let wt = tmp.path().join("dirty-remove-worktree");
        let wt_str = wt.to_string_lossy().to_string();
        git(
            &repo,
            &["worktree", "add", "-b", "dirty-remove", &wt_str, "main"],
        );
        std::fs::write(wt.join("scratch.txt"), "local\n").unwrap();

        let scan = scan_worktrees(tmp.path(), Some(&repo), &[]);
        let found = scan
            .worktrees
            .iter()
            .find(|entry| entry.branch.as_deref() == Some("dirty-remove"))
            .expect("dirty-remove worktree found");
        assert!(!found.safe_to_remove);

        let err = remove_worktree_if_safe(
            WorktreeRemoveRequest {
                repo_root: repo,
                path: wt.clone(),
                expected_head: found.head.clone(),
            },
            &[],
        )
        .expect_err("dirty worktree refused");

        assert!(err.contains("safety check refused"));
        assert!(wt.exists());
    }

    #[test]
    fn remove_worktree_refuses_changed_head() {
        let tmp = init_repo();
        let repo = repo_path(&tmp);
        let wt = tmp.path().join("changed-head-worktree");
        let wt_str = wt.to_string_lossy().to_string();
        git(
            &repo,
            &["worktree", "add", "-b", "changed-head", &wt_str, "main"],
        );

        let err = remove_worktree_if_safe(
            WorktreeRemoveRequest {
                repo_root: repo,
                path: wt.clone(),
                expected_head: Some("0000000000000000000000000000000000000000".to_string()),
            },
            &[],
        )
        .expect_err("changed head refused");

        assert!(err.contains("HEAD changed"));
        assert!(wt.exists());
    }

    // du-style block accounting + hardlink de-dup is Unix-only semantics
    // (`MetadataExt::blocks`/`nlink`). On Windows the disk-usage walk falls
    // back to apparent `len()` with no inode de-dup, so this assertion does
    // not apply — see `crate::platform::metadata_on_disk_bytes`.
    #[cfg(unix)]
    #[test]
    fn measure_tree_counts_hardlinks_once() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tree");
        std::fs::create_dir(&dir).unwrap();

        // A file big enough to occupy real blocks, plus a hardlink to it.
        let original = dir.join("original.bin");
        std::fs::write(&original, vec![b'x'; 64 * 1024]).unwrap();
        std::fs::hard_link(&original, dir.join("hardlink.bin")).unwrap();

        // du-style allocation of the single shared inode.
        let single = std::fs::symlink_metadata(&original).unwrap().blocks() * 512;
        assert!(single > 0, "test file should occupy at least one block");

        let measure = measure_tree(&dir);
        // Both directory entries are seen as files...
        assert_eq!(measure.files, 2);
        // ...but the shared inode's blocks are counted only once (not doubled).
        assert_eq!(measure.bytes, single);

        // An independent file accumulates on top — guards against over-dedup.
        let other = dir.join("other.bin");
        std::fs::write(&other, vec![b'y'; 64 * 1024]).unwrap();
        let other_alloc = std::fs::symlink_metadata(&other).unwrap().blocks() * 512;
        let measure = measure_tree(&dir);
        assert_eq!(measure.files, 3);
        assert_eq!(measure.bytes, single + other_alloc);
    }
}

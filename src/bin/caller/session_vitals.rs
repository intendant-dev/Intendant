//! Per-session vitals producers — the operator-statusline port.
//!
//! Two producers write through a shared hub that merges sections and
//! emits `AppEvent::SessionVitals` on change (frontends render chips from
//! the latest value; the session log replays it on reconnect):
//!
//! - **Git segment**: fetch-free probes of a session's working directory —
//!   branch, dirty count, ahead/behind vs `origin/<primary>` (local
//!   fallback), a merge-parity preview via in-memory
//!   `git merge-tree --write-tree` (git ≥ 2.38, cached by SHA pair), and
//!   unpushed counts for the current and primary branches. Periodic, for
//!   the fixed target list.
//! - **Cache segment**: a bus listener over `AppEvent::UsageSnapshot` —
//!   every backend's usage rail converges there (external drains, the
//!   native derivation in `tui/app.rs`), so one listener covers Claude
//!   Code, Codex, and native sessions uniformly. Computes the latest
//!   request's cache-hit receipt and carries the TTL anchor; the countdown
//!   itself derives client-side from `last_activity_epoch + ttl_seconds`
//!   (no per-second events).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::event::{AppEvent, EventBus};
use crate::frontend::ModelUsageSnapshot;
use crate::types::{SessionCacheVitals, SessionGitVitals, SessionVitals};

/// Probe cadence. Each tick is a handful of subprocess ref reads per
/// target; emission only happens when the probed state changes.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn git_count(cwd: &Path, range: &str) -> Option<u32> {
    git(cwd, &["rev-list", "--count", range])
        .await?
        .parse()
        .ok()
}

/// Modern `git merge-tree --write-tree` needs git ≥ 2.38; probed once per
/// process. Older git silently skips the merge-parity preview.
fn merge_tree_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .ok()
            .map(|out| {
                git_version_at_least(&String::from_utf8_lossy(&out.stdout), 2, 38)
            })
            .unwrap_or(false)
    })
}

fn git_version_at_least(version_line: &str, want_major: u32, want_minor: u32) -> bool {
    let mut parts = version_line
        .split_whitespace()
        .find(|token| token.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or("")
        .split('.');
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    major > want_major || (major == want_major && minor >= want_minor)
}

/// Git prober with a per-(HEAD, primary) merge-parity cache — the
/// expensive in-memory merge only reruns when either side moves.
#[derive(Default)]
pub(crate) struct GitVitalsProber {
    merge_cache: HashMap<(String, String), String>,
}

impl GitVitalsProber {
    pub(crate) async fn probe(&mut self, cwd: &Path) -> Option<SessionGitVitals> {
        git(cwd, &["rev-parse", "--git-dir"]).await?;
        let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap_or_default();
        let dirty_files = git(cwd, &["--no-optional-locks", "status", "--porcelain"])
            .await
            .map(|out| out.lines().filter(|l| !l.trim().is_empty()).count() as u32)
            .unwrap_or(0);

        // Primary branch: origin's default when known, else local main/master.
        let mut primary_branch = git(cwd, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
            .await
            .map(|s| s.trim_start_matches("origin/").to_string());
        if primary_branch.is_none() {
            for candidate in ["main", "master"] {
                let refname = format!("refs/heads/{candidate}");
                if git(cwd, &["show-ref", "--verify", "--quiet", &refname])
                    .await
                    .is_some()
                {
                    primary_branch = Some(candidate.to_string());
                    break;
                }
            }
        }

        let mut ahead = 0;
        let mut behind = 0;
        let mut primary_ref = String::new();
        let mut merge_parity = String::new();
        let mut primary_unpushed = None;
        if let Some(primary_branch) = primary_branch.as_deref() {
            // Prefer origin/<primary> — a stale local primary would misread
            // fresh worktrees cut from the remote tip.
            let remote_primary = format!("origin/{primary_branch}");
            primary_ref = if git(cwd, &["rev-parse", "--verify", "--quiet", &remote_primary])
                .await
                .is_some()
            {
                remote_primary
            } else {
                primary_branch.to_string()
            };
            ahead = git_count(cwd, &format!("{primary_ref}..HEAD")).await.unwrap_or(0);
            behind = git_count(cwd, &format!("HEAD..{primary_ref}")).await.unwrap_or(0);

            merge_parity = if (ahead > 0) != (behind > 0) {
                // Fast-forward in one direction: trivially clean.
                "clean".to_string()
            } else if ahead > 0 && behind > 0 && merge_tree_supported() {
                self.merge_parity(cwd, &primary_ref).await.unwrap_or_default()
            } else {
                String::new()
            };

            if branch != primary_branch {
                let primary_upstream = format!("{primary_branch}@{{upstream}}");
                if git(cwd, &["rev-parse", "--verify", "--quiet", "--abbrev-ref", &primary_upstream])
                    .await
                    .is_some()
                {
                    primary_unpushed =
                        git_count(cwd, &format!("{primary_upstream}..{primary_branch}")).await;
                }
            }
        }

        let unpushed = if git(cwd, &["rev-parse", "--verify", "--quiet", "--abbrev-ref", "@{upstream}"])
            .await
            .is_some()
        {
            git_count(cwd, "@{upstream}..HEAD").await
        } else {
            None
        };

        Some(SessionGitVitals {
            branch,
            dirty_files,
            ahead,
            behind,
            primary_ref,
            merge_parity,
            unpushed,
            primary_unpushed,
        })
    }

    /// Would merging HEAD and the primary conflict? In-memory 3-way merge,
    /// cached by the SHA pair so it only reruns when something moves.
    async fn merge_parity(&mut self, cwd: &Path, primary_ref: &str) -> Option<String> {
        let head = git(cwd, &["rev-parse", "HEAD"]).await?;
        let primary = git(cwd, &["rev-parse", primary_ref]).await?;
        let key = (head, primary);
        if let Some(cached) = self.merge_cache.get(&key) {
            return Some(cached.clone());
        }
        let clean = tokio::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["merge-tree", "--write-tree", "HEAD", primary_ref])
            .output()
            .await
            .ok()?
            .status
            .success();
        let state = if clean { "clean" } else { "conflict" }.to_string();
        // The cache only grows while refs churn; entries are tiny and the
        // pair space a session actually visits is small.
        if self.merge_cache.len() > 256 {
            self.merge_cache.clear();
        }
        self.merge_cache.insert(key, state.clone());
        Some(state)
    }
}

/// Shared per-session vitals state. Producers merge their section in via
/// [`SessionVitalsHub::apply`]; the hub emits the combined snapshot on the
/// bus whenever an update actually changed something (the initial
/// all-empty state never emits).
struct SessionVitalsHub {
    bus: EventBus,
    sessions: Mutex<HashMap<String, SessionVitals>>,
}

impl SessionVitalsHub {
    fn new(bus: EventBus) -> Arc<Self> {
        Arc::new(Self {
            bus,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    fn apply(&self, session_id: &str, update: impl FnOnce(&mut SessionVitals)) {
        let changed = {
            let mut sessions = self.sessions.lock().expect("vitals state lock");
            let entry = sessions.entry(session_id.to_string()).or_default();
            let before = entry.clone();
            update(entry);
            (*entry != before).then(|| entry.clone())
        };
        if let Some(vitals) = changed {
            self.bus.send(AppEvent::SessionVitals {
                session_id: session_id.to_string(),
                vitals,
            });
        }
    }

    fn remove(&self, session_id: &str) {
        self.sessions
            .lock()
            .expect("vitals state lock")
            .remove(session_id);
    }
}

/// Unix seconds now — the cache-countdown anchor.
fn epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fold one usage snapshot into a session's cache section. Returns `None`
/// when the snapshot carries no per-request cache sample (nothing to
/// learn). `previous_ttl` keeps the last known TTL flavor across read-only
/// responses — only cache writes state a flavor.
fn cache_vitals_from_usage(
    main: &ModelUsageSnapshot,
    previous_ttl: Option<u32>,
    now_epoch: u64,
) -> Option<SessionCacheVitals> {
    let read = main.last_cache_read_tokens;
    let sample_total = read + main.last_cache_creation_tokens + main.last_uncached_input_tokens;
    if sample_total == 0 {
        return None;
    }
    let hit_pct = ((read * 100 + sample_total / 2) / sample_total).min(100) as u8;
    let ttl_seconds = main
        .cache_ttl_seconds
        .or(previous_ttl)
        // Anthropic's default flavor is 5 minutes; a read-only first sample
        // still means a live cache. Other providers (undocumented TTLs)
        // show the hit receipt without a countdown.
        .or_else(|| (main.provider == "anthropic" && read > 0).then_some(300));
    Some(SessionCacheVitals {
        hit_pct: Some(hit_pct),
        last_activity_epoch: now_epoch,
        ttl_seconds,
    })
}

/// Bus listener feeding the cache section: every backend's usage rail
/// converges on `AppEvent::UsageSnapshot`, so this one consumer covers
/// native, Claude Code, and Codex sessions alike. Sessions are pruned on
/// `SessionEnded`.
fn spawn_cache_vitals_listener(
    bus: EventBus,
    hub: Arc<SessionVitalsHub>,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(AppEvent::UsageSnapshot {
                    session_id: Some(session_id),
                    main,
                    ..
                }) => {
                    hub.apply(&session_id, |vitals| {
                        let previous_ttl = vitals.cache.as_ref().and_then(|c| c.ttl_seconds);
                        if let Some(cache) =
                            cache_vitals_from_usage(&main, previous_ttl, epoch_seconds())
                        {
                            vitals.cache = Some(cache);
                        }
                        // Sticky: rate limits move slowly and not every
                        // usage emission re-states them.
                        if !main.limits.is_empty() {
                            vitals.limits = main.limits.clone();
                        }
                    });
                }
                Ok(AppEvent::SessionEnded { session_id, .. }) => hub.remove(&session_id),
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Vitals producer: spawns the cache listener and runs the periodic git
/// prober for a fixed set of (session id, working dir) targets — today the
/// primary session. All emission flows through the change-detecting hub;
/// the session log persists each emission so reconnecting frontends replay
/// the latest.
pub(crate) fn spawn_session_vitals_producer(
    bus: EventBus,
    targets: Vec<(String, PathBuf)>,
) -> tokio::task::JoinHandle<()> {
    let hub = SessionVitalsHub::new(bus.clone());
    let _cache_listener = spawn_cache_vitals_listener(bus, hub.clone());
    tokio::spawn(async move {
        let mut prober = GitVitalsProber::default();
        loop {
            for (session_id, cwd) in &targets {
                let probed = prober.probe(cwd).await;
                hub.apply(session_id, |vitals| vitals.git = probed);
            }
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .status()
            .expect("spawn");
        assert!(status.success(), "command failed: {args:?}");
    }

    fn git_cmd(cwd: &Path, args: &[&str]) {
        let mut full = vec!["git", "-c", "user.email=t@e2e", "-c", "user.name=t", "-c", "commit.gpgsign=false"];
        full.extend_from_slice(args);
        sh(cwd, &full);
    }

    #[test]
    fn git_version_gate_parses_common_formats() {
        assert!(git_version_at_least("git version 2.39.5", 2, 38));
        assert!(git_version_at_least("git version 2.38.0", 2, 38));
        assert!(!git_version_at_least("git version 2.37.9", 2, 38));
        assert!(git_version_at_least("git version 3.0.0", 2, 38));
        assert!(!git_version_at_least("garbage", 2, 38));
    }

    #[tokio::test]
    async fn probe_reports_branch_dirty_and_divergence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git_cmd(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git_cmd(root, &["add", "."]);
        git_cmd(root, &["commit", "-qm", "base"]);

        // Feature branch one commit ahead of main, plus a dirty file.
        git_cmd(root, &["checkout", "-qb", "feature"]);
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        git_cmd(root, &["add", "."]);
        git_cmd(root, &["commit", "-qm", "feature work"]);
        std::fs::write(root.join("a.txt"), "one modified\n").unwrap();

        let mut prober = GitVitalsProber::default();
        let vitals = prober.probe(root).await.expect("git repo probes");
        assert_eq!(vitals.branch, "feature");
        assert_eq!(vitals.dirty_files, 1);
        assert_eq!(vitals.primary_ref, "main");
        assert_eq!(vitals.ahead, 1);
        assert_eq!(vitals.behind, 0);
        // One-sided divergence is trivially clean; no upstreams exist.
        assert_eq!(vitals.merge_parity, "clean");
        assert_eq!(vitals.unpushed, None);
        assert_eq!(vitals.primary_unpushed, None);

        // Non-repo directories probe to None (section hidden).
        let empty = tempfile::tempdir().expect("tempdir");
        assert!(prober.probe(empty.path()).await.is_none());
    }

    fn usage_with_sample(
        provider: &str,
        read: u64,
        creation: u64,
        uncached: u64,
        ttl: Option<u32>,
    ) -> ModelUsageSnapshot {
        ModelUsageSnapshot {
            provider: provider.to_string(),
            last_cache_read_tokens: read,
            last_cache_creation_tokens: creation,
            last_uncached_input_tokens: uncached,
            cache_ttl_seconds: ttl,
            ..Default::default()
        }
    }

    #[test]
    fn cache_vitals_hit_math_and_ttl_resolution() {
        // 90 read / 10 fresh → 90% hit; explicit flavor wins.
        let vitals =
            cache_vitals_from_usage(&usage_with_sample("anthropic", 90, 5, 5, Some(3600)), None, 42)
                .expect("sample present");
        assert_eq!(vitals.hit_pct, Some(90));
        assert_eq!(vitals.last_activity_epoch, 42);
        assert_eq!(vitals.ttl_seconds, Some(3600));

        // Read-only response: no flavor statement — sticky TTL survives.
        let sticky =
            cache_vitals_from_usage(&usage_with_sample("anthropic", 100, 0, 0, None), Some(3600), 43)
                .expect("sample present");
        assert_eq!(sticky.hit_pct, Some(100));
        assert_eq!(sticky.ttl_seconds, Some(3600));

        // Anthropic read without prior flavor: the 5-minute default.
        let default_flavor =
            cache_vitals_from_usage(&usage_with_sample("anthropic", 50, 0, 50, None), None, 44)
                .expect("sample present");
        assert_eq!(default_flavor.hit_pct, Some(50));
        assert_eq!(default_flavor.ttl_seconds, Some(300));

        // OpenAI: hit receipt without a countdown (TTL undocumented).
        let openai = cache_vitals_from_usage(&usage_with_sample("openai", 75, 0, 25, None), None, 45)
            .expect("sample present");
        assert_eq!(openai.hit_pct, Some(75));
        assert_eq!(openai.ttl_seconds, None);

        // No per-request sample → nothing to learn.
        assert!(cache_vitals_from_usage(&usage_with_sample("anthropic", 0, 0, 0, None), None, 46)
            .is_none());
    }

    #[tokio::test]
    async fn hub_merges_sections_and_emits_on_change_only() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let hub = SessionVitalsHub::new(bus);

        // A no-op update on a fresh entry (git stays None) never emits.
        hub.apply("s1", |v| v.git = None);

        // First real section → one emission with just that section.
        let git = SessionGitVitals {
            branch: "feature".into(),
            dirty_files: 1,
            ..Default::default()
        };
        hub.apply("s1", |v| v.git = Some(git.clone()));
        // Cache joins → merged snapshot carries both sections.
        hub.apply("s1", |v| {
            v.cache = Some(SessionCacheVitals {
                hit_pct: Some(90),
                last_activity_epoch: 1,
                ttl_seconds: Some(300),
            })
        });
        // Identical rewrite → no emission.
        hub.apply("s1", |v| v.git = Some(git.clone()));
        hub.remove("s1");
        // Post-removal update starts from a fresh default entry.
        hub.apply("s1", |v| v.git = Some(git.clone()));

        let mut emissions = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::SessionVitals { session_id, vitals } = event {
                emissions.push((session_id, vitals));
            }
        }
        assert_eq!(emissions.len(), 3, "changes emit, no-ops and rewrites do not");
        assert_eq!(emissions[0].1.git.as_ref().unwrap().branch, "feature");
        assert!(emissions[0].1.cache.is_none());
        assert!(emissions[1].1.git.is_some(), "merged snapshot keeps git");
        assert_eq!(emissions[1].1.cache.as_ref().unwrap().hit_pct, Some(90));
        assert!(
            emissions[2].1.cache.is_none(),
            "removed session re-registers from scratch"
        );
    }

    #[tokio::test]
    async fn cache_listener_folds_usage_snapshots() {
        let bus = EventBus::new();
        let hub = SessionVitalsHub::new(bus.clone());
        let _listener = spawn_cache_vitals_listener(bus.clone(), hub.clone());
        let mut rx = bus.subscribe();

        let mut with_limits = usage_with_sample("anthropic", 80, 10, 10, Some(300));
        with_limits.limits = vec![crate::types::SessionLimitWindow {
            label: "7d".into(),
            used_pct: 49,
            resets_at_epoch: Some(1_783_807_200),
        }];
        bus.send(AppEvent::UsageSnapshot {
            session_id: Some("s7".into()),
            main: with_limits,
            presence: None,
        });
        let vitals = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(AppEvent::SessionVitals { session_id, vitals }) = rx.recv().await {
                    assert_eq!(session_id, "s7");
                    return vitals;
                }
            }
        })
        .await
        .expect("listener emits vitals");
        let cache = vitals.cache.expect("cache section");
        assert_eq!(cache.hit_pct, Some(80));
        assert_eq!(cache.ttl_seconds, Some(300));
        assert!(cache.last_activity_epoch > 0);
        assert_eq!(vitals.limits.len(), 1);
        assert_eq!(vitals.limits[0].used_pct, 49);

        // Limits are sticky: a later usage emission without them keeps the
        // last known gauges.
        bus.send(AppEvent::UsageSnapshot {
            session_id: Some("s7".into()),
            main: usage_with_sample("anthropic", 90, 0, 10, None),
            presence: None,
        });
        let vitals = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(AppEvent::SessionVitals { session_id, vitals }) = rx.recv().await {
                    if session_id == "s7" && vitals.cache.as_ref().and_then(|c| c.hit_pct) == Some(90)
                    {
                        return vitals;
                    }
                }
            }
        })
        .await
        .expect("listener emits updated vitals");
        assert_eq!(vitals.limits.len(), 1, "limits survive limit-less usage");
        assert_eq!(vitals.limits[0].label, "7d");
    }

    #[tokio::test]
    async fn probe_flags_conflicting_divergence() {
        if !merge_tree_supported() {
            return; // old git: the preview is skipped by design
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git_cmd(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "base\n").unwrap();
        git_cmd(root, &["add", "."]);
        git_cmd(root, &["commit", "-qm", "base"]);
        git_cmd(root, &["checkout", "-qb", "feature"]);
        std::fs::write(root.join("a.txt"), "feature\n").unwrap();
        git_cmd(root, &["commit", "-qam", "feature edit"]);
        git_cmd(root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("a.txt"), "main\n").unwrap();
        git_cmd(root, &["commit", "-qam", "main edit"]);
        git_cmd(root, &["checkout", "-q", "feature"]);

        let mut prober = GitVitalsProber::default();
        let vitals = prober.probe(root).await.expect("git repo probes");
        assert_eq!(vitals.ahead, 1);
        assert_eq!(vitals.behind, 1);
        assert_eq!(vitals.merge_parity, "conflict");
        // Cached by SHA pair: a second probe reuses the verdict.
        assert_eq!(prober.merge_cache.len(), 1);
        let again = prober.probe(root).await.expect("git repo probes");
        assert_eq!(again.merge_parity, "conflict");
        assert_eq!(prober.merge_cache.len(), 1);
    }
}

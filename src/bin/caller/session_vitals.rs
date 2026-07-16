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
//!   native derivation in `usage_rail.rs`), so one listener covers Claude
//!   Code, Codex, and native sessions uniformly. Computes the latest
//!   request's cache-hit receipt and carries the TTL anchor; the countdown
//!   itself derives client-side from `last_activity_epoch + ttl_seconds`
//!   (no per-second events).
//! - **Activity segment**: the same listener folds
//!   `AppEvent::SessionActivity` — each backend's wire-fact activity
//!   machine (`session_activity.rs`) publishes through its drain (or the
//!   native loop directly), and this one consumer covers them all.
//!   Epochs + raw state ride the wire; elapsed/stall derivation ticks
//!   client-side.
//!
//! The two producers arrive keyed by different members of an external
//! session's identity group (git = wrapper/log id, usage = backend-native
//! id), so the hub folds `SessionIdentity` linkages and canonicalizes
//! every write — one entry, and one complete emitted snapshot, per
//! logical session.

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
            .map(|out| git_version_at_least(&String::from_utf8_lossy(&out.stdout), 2, 38))
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
        let mut primary_branch = git(
            cwd,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
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
            ahead = git_count(cwd, &format!("{primary_ref}..HEAD"))
                .await
                .unwrap_or(0);
            behind = git_count(cwd, &format!("HEAD..{primary_ref}"))
                .await
                .unwrap_or(0);

            merge_parity = if (ahead > 0) != (behind > 0) {
                // Fast-forward in one direction: trivially clean.
                "clean".to_string()
            } else if ahead > 0 && behind > 0 && merge_tree_supported() {
                self.merge_parity(cwd, &primary_ref)
                    .await
                    .unwrap_or_default()
            } else {
                String::new()
            };

            if branch != primary_branch {
                let primary_upstream = format!("{primary_branch}@{{upstream}}");
                if git(
                    cwd,
                    &[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        "--abbrev-ref",
                        &primary_upstream,
                    ],
                )
                .await
                .is_some()
                {
                    primary_unpushed =
                        git_count(cwd, &format!("{primary_upstream}..{primary_branch}")).await;
                }
            }
        }

        let unpushed = if git(
            cwd,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--abbrev-ref",
                "@{upstream}",
            ],
        )
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
///
/// External sessions run under two ids — the wrapper/log id and the
/// backend-native id announced mid-flight by `SessionIdentity` — and the
/// two vitals producers arrive keyed by *different* members of that pair
/// (git probes ride the registered wrapper id, usage snapshots the id the
/// drain stamps, native for Codex/Claude Code). Without folding, each
/// logical session holds two half-empty hub entries whose emissions
/// overwrite each other on the frontend (the identity-seam drop class:
/// the git family blanks to "not reported" the moment usage wins the
/// race). The alias map canonicalizes every apply/remove so one entry —
/// and therefore every emitted snapshot — carries all sections.
struct SessionVitalsHub {
    bus: EventBus,
    sessions: Mutex<HashMap<String, SessionVitals>>,
    /// alias id → canonical id, fed by `SessionIdentity` (native →
    /// wrapper). Chains are flattened at link time so `resolve` is one
    /// hop in practice; the hop cap is a cycle guard only.
    aliases: Mutex<HashMap<String, String>>,
}

impl SessionVitalsHub {
    fn new(bus: EventBus) -> Arc<Self> {
        Arc::new(Self {
            bus,
            sessions: Mutex::new(HashMap::new()),
            aliases: Mutex::new(HashMap::new()),
        })
    }

    /// Canonical id for any member of an identity group.
    fn resolve(&self, session_id: &str) -> String {
        let aliases = self.aliases.lock().expect("vitals alias lock");
        let mut id = session_id.trim();
        for _ in 0..4 {
            match aliases.get(id) {
                Some(next) => id = next,
                None => break,
            }
        }
        id.to_string()
    }

    /// Record `alias` (backend-native id) as pointing at `canonical`
    /// (wrapper/log id) and fold any sections that already accumulated
    /// under the alias — usage snapshots can arrive before the
    /// `SessionIdentity` linkage lands — into the canonical entry.
    fn link_alias(&self, alias: &str, canonical: &str) {
        let alias = alias.trim();
        let canonical = self.resolve(canonical);
        if alias.is_empty() || canonical.is_empty() || alias == canonical {
            return;
        }
        {
            let mut aliases = self.aliases.lock().expect("vitals alias lock");
            aliases.insert(alias.to_string(), canonical.clone());
            // Flatten: anything that named `alias` canonical follows it.
            for target in aliases.values_mut() {
                if target == alias {
                    *target = canonical.clone();
                }
            }
        }
        let orphan = self
            .sessions
            .lock()
            .expect("vitals state lock")
            .remove(alias);
        if let Some(orphan) = orphan {
            self.apply(&canonical, |vitals| {
                if vitals.git.is_none() {
                    vitals.git = orphan.git;
                }
                if vitals.cache.is_none() {
                    vitals.cache = orphan.cache;
                }
                if vitals.limits.is_empty() {
                    vitals.limits = orphan.limits;
                }
                if vitals.activity.is_none() {
                    vitals.activity = orphan.activity;
                }
            });
        }
    }

    fn apply(&self, session_id: &str, update: impl FnOnce(&mut SessionVitals)) {
        let session_id = self.resolve(session_id);
        let changed = {
            let mut sessions = self.sessions.lock().expect("vitals state lock");
            let entry = sessions.entry(session_id.clone()).or_default();
            let before = entry.clone();
            update(entry);
            (*entry != before).then(|| entry.clone())
        };
        if let Some(vitals) = changed {
            self.bus
                .send(AppEvent::SessionVitals { session_id, vitals });
        }
    }

    fn remove(&self, session_id: &str) {
        let canonical = self.resolve(session_id);
        {
            let mut sessions = self.sessions.lock().expect("vitals state lock");
            sessions.remove(session_id.trim());
            sessions.remove(&canonical);
        }
        // Drop the group's alias records too — an ended session's ids
        // never come back, and the map otherwise grows for daemon-life.
        self.aliases
            .lock()
            .expect("vitals alias lock")
            .retain(|alias, target| {
                alias != session_id.trim() && target != &canonical && alias != &canonical
            });
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

/// Bus listener feeding the cache and activity sections: every backend's
/// usage rail converges on `AppEvent::UsageSnapshot` and every activity
/// machine on `AppEvent::SessionActivity`, so this one consumer covers
/// native, Claude Code, and Codex sessions alike. `SessionIdentity`
/// linkages feed the hub's alias map so usage keyed by the backend-native
/// id and git probes keyed by the wrapper id land in one entry (split
/// entries emit half-empty snapshots that blank each other's chips).
/// Sessions are pruned on `SessionEnded`.
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
                Ok(AppEvent::SessionActivity {
                    session_id: Some(session_id),
                    activity,
                }) => {
                    hub.apply(&session_id, |vitals| vitals.activity = Some(activity));
                }
                Ok(AppEvent::SessionIdentity {
                    session_id,
                    backend_session_id,
                    ..
                }) => hub.link_alias(&backend_session_id, &session_id),
                Ok(AppEvent::SessionEnded { session_id, .. }) => hub.remove(&session_id),
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Live registry of (session id → working dir) git-probe targets. The
/// daemon seeds the primary session at startup; the session supervisor
/// registers every managed session at launch — which is what puts the
/// dirty-count / merge-parity / unpushed rows on dashboard-spawned
/// sessions and on projectless daemons (whose primary has no repo).
/// `SessionEnded` prunes entries, so a handle owner only has to register.
#[derive(Clone, Default)]
pub(crate) struct GitVitalsTargets {
    targets: Arc<Mutex<HashMap<String, PathBuf>>>,
}

impl GitVitalsTargets {
    /// Register (or retarget) a session's git probe root. No-ops on empty
    /// ids so callers can pass through unresolved values unchecked.
    pub(crate) fn register(&self, session_id: &str, cwd: PathBuf) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        self.targets
            .lock()
            .expect("git vitals targets lock")
            .insert(session_id.to_string(), cwd);
    }

    pub(crate) fn remove(&self, session_id: &str) {
        self.targets
            .lock()
            .expect("git vitals targets lock")
            .remove(session_id.trim());
    }

    fn snapshot(&self) -> Vec<(String, PathBuf)> {
        self.targets
            .lock()
            .expect("git vitals targets lock")
            .iter()
            .map(|(id, cwd)| (id.clone(), cwd.clone()))
            .collect()
    }
}

/// Vitals producer: spawns the cache listener and runs the periodic git
/// prober over the live target registry (seeded with the primary session,
/// fed by the session supervisor as sessions launch). All emission flows
/// through the change-detecting hub; the session log persists each
/// emission so reconnecting frontends replay the latest.
///
/// The seed may be empty: the cache/limits sections are usage-driven and
/// backend-agnostic, so the listener runs wherever a bus exists — a
/// projectless daemon still reports cache and rate-limit vitals for every
/// session; only the git segment needs a repo target.
pub(crate) fn spawn_session_vitals_producer(
    bus: EventBus,
    seed_targets: Vec<(String, PathBuf)>,
) -> (GitVitalsTargets, tokio::task::JoinHandle<()>) {
    let registry = GitVitalsTargets::default();
    for (session_id, cwd) in seed_targets {
        registry.register(&session_id, cwd);
    }
    let hub = SessionVitalsHub::new(bus.clone());
    let _cache_listener = spawn_cache_vitals_listener(bus.clone(), hub.clone());
    let _target_pruner = spawn_git_target_pruner(bus, registry.clone(), hub.clone());
    let handle = tokio::spawn({
        let registry = registry.clone();
        async move {
            let mut prober = GitVitalsProber::default();
            loop {
                for (session_id, cwd) in registry.snapshot() {
                    let probed = prober.probe(&cwd).await;
                    hub.apply(&session_id, |vitals| vitals.git = probed);
                }
                tokio::time::sleep(PROBE_INTERVAL).await;
            }
        }
    });
    (registry, handle)
}

/// Prune git targets when their session ends — mirrors the cache
/// listener's `SessionEnded` hygiene so registered sessions never leak
/// probe work past their lifetime. Resolution runs through the hub's
/// alias map: resume lanes register the live (backend-native) id while
/// the end event carries the wrapper id, and vice versa.
fn spawn_git_target_pruner(
    bus: EventBus,
    registry: GitVitalsTargets,
    hub: Arc<SessionVitalsHub>,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(AppEvent::SessionEnded { session_id, .. }) => {
                    registry.remove(&session_id);
                    let ended = hub.resolve(&session_id);
                    for (id, _) in registry.snapshot() {
                        if hub.resolve(&id) == ended {
                            registry.remove(&id);
                        }
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
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
        let mut full = vec![
            "git",
            "-c",
            "user.email=t@e2e",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ];
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
        let vitals = cache_vitals_from_usage(
            &usage_with_sample("anthropic", 90, 5, 5, Some(3600)),
            None,
            42,
        )
        .expect("sample present");
        assert_eq!(vitals.hit_pct, Some(90));
        assert_eq!(vitals.last_activity_epoch, 42);
        assert_eq!(vitals.ttl_seconds, Some(3600));

        // Read-only response: no flavor statement — sticky TTL survives.
        let sticky = cache_vitals_from_usage(
            &usage_with_sample("anthropic", 100, 0, 0, None),
            Some(3600),
            43,
        )
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
        let openai =
            cache_vitals_from_usage(&usage_with_sample("openai", 75, 0, 25, None), None, 45)
                .expect("sample present");
        assert_eq!(openai.hit_pct, Some(75));
        assert_eq!(openai.ttl_seconds, None);

        // No per-request sample → nothing to learn.
        assert!(
            cache_vitals_from_usage(&usage_with_sample("anthropic", 0, 0, 0, None), None, 46)
                .is_none()
        );
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
        assert_eq!(
            emissions.len(),
            3,
            "changes emit, no-ops and rewrites do not"
        );
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
    async fn identity_alias_folds_split_sections_into_one_snapshot() {
        // The live failure shape (2026-07-15 screenshot): git probes rode
        // the wrapper id while usage rode the codex-native id — two hub
        // entries whose alternating emissions blanked each other's chips
        // on the frontend. With the alias fold, everything lands in the
        // wrapper entry and every snapshot carries all sections.
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let hub = SessionVitalsHub::new(bus.clone());
        let _listener = spawn_cache_vitals_listener(bus.clone(), hub.clone());

        // Usage arrives under the native id BEFORE the identity linkage.
        bus.send(AppEvent::UsageSnapshot {
            session_id: Some("native-1".into()),
            main: usage_with_sample("anthropic", 80, 10, 10, Some(300)),
            presence: None,
        });
        // Git probes land under the wrapper id (registered at launch).
        let git = SessionGitVitals {
            branch: "main".into(),
            dirty_files: 2,
            ..Default::default()
        };
        let deadline = std::time::Duration::from_secs(5);
        async fn wait_vitals(
            rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
        ) -> (String, SessionVitals) {
            loop {
                if let Ok(AppEvent::SessionVitals { session_id, vitals }) = rx.recv().await {
                    return (session_id, vitals);
                }
            }
        }
        let (sid, _) = tokio::time::timeout(deadline, wait_vitals(&mut rx))
            .await
            .expect("pre-identity usage emission");
        assert_eq!(sid, "native-1", "pre-linkage usage stays under native id");
        hub.apply("wrapper-1", |v| v.git = Some(git.clone()));
        let (sid, vitals) = tokio::time::timeout(deadline, wait_vitals(&mut rx))
            .await
            .expect("git emission");
        assert_eq!(sid, "wrapper-1");
        assert!(vitals.cache.is_none(), "entries still split pre-linkage");

        // The identity linkage migrates the native orphan into the
        // wrapper entry and re-emits a complete snapshot.
        bus.send(AppEvent::SessionIdentity {
            session_id: "wrapper-1".into(),
            source: "codex".into(),
            backend_session_id: "native-1".into(),
        });
        let (sid, vitals) = tokio::time::timeout(deadline, wait_vitals(&mut rx))
            .await
            .expect("post-linkage merged emission");
        assert_eq!(sid, "wrapper-1");
        assert_eq!(vitals.git.as_ref().unwrap().dirty_files, 2);
        assert_eq!(vitals.cache.as_ref().unwrap().hit_pct, Some(80));

        // Later usage keyed by the native id resolves to the wrapper
        // entry: the git section survives (the user-visible regression
        // was exactly this write blanking it).
        bus.send(AppEvent::UsageSnapshot {
            session_id: Some("native-1".into()),
            main: usage_with_sample("anthropic", 40, 40, 20, Some(300)),
            presence: None,
        });
        let (sid, vitals) = tokio::time::timeout(deadline, wait_vitals(&mut rx))
            .await
            .expect("post-linkage usage emission");
        assert_eq!(sid, "wrapper-1", "usage now emits under the canonical id");
        assert!(
            vitals.git.is_some(),
            "usage writes must not blank the git section"
        );
        assert_eq!(vitals.cache.as_ref().unwrap().hit_pct, Some(40));

        // Group teardown clears both ids and the alias record.
        hub.remove("native-1");
        hub.apply("native-1", |v| v.git = Some(git.clone()));
        let (sid, _) = tokio::time::timeout(deadline, wait_vitals(&mut rx))
            .await
            .expect("post-removal emission");
        assert_eq!(
            sid, "native-1",
            "removal drops the alias — the id starts a fresh group"
        );
    }

    #[tokio::test]
    async fn registered_target_probes_and_session_end_prunes() {
        // Supervisor-registered sessions get git rows without a restart;
        // SessionEnded prunes the target (registry drops the entry).
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        git_cmd(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git_cmd(root, &["add", "."]);
        git_cmd(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("b.txt"), "dirty\n").unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (targets, _producer) = spawn_session_vitals_producer(bus.clone(), Vec::new());
        targets.register("supervised-1", root.to_path_buf());

        let deadline = std::time::Duration::from_secs(20);
        loop {
            let event = tokio::time::timeout(deadline, rx.recv())
                .await
                .expect("git vitals emission before timeout")
                .expect("bus open");
            if let AppEvent::SessionVitals { session_id, vitals } = event {
                assert_eq!(session_id, "supervised-1");
                let git = vitals.git.expect("git section probed");
                assert_eq!(git.branch, "main");
                assert_eq!(git.dirty_files, 1);
                break;
            }
        }

        bus.send(AppEvent::SessionEnded {
            session_id: "supervised-1".into(),
            reason: "done".into(),
            error_kind: None,
        });
        // The pruner runs on the bus; poll until the entry is gone.
        let start = std::time::Instant::now();
        while !targets.snapshot().is_empty() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "SessionEnded did not prune the git target"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn producer_with_no_git_targets_still_serves_cache_vitals() {
        // A projectless daemon has no git target, but cache/limits vitals
        // are usage-driven and must keep flowing for every session
        // (regression: the listener used to die with the git gating,
        // blanking the dashboard's Prompt cache row daemon-wide).
        let bus = EventBus::new();
        let _producer = spawn_session_vitals_producer(bus.clone(), Vec::new());
        let mut rx = bus.subscribe();
        bus.send(AppEvent::UsageSnapshot {
            session_id: Some("cc-session".into()),
            main: usage_with_sample("anthropic", 80, 10, 10, Some(3600)),
            presence: None,
        });
        let deadline = std::time::Duration::from_secs(5);
        loop {
            let event = tokio::time::timeout(deadline, rx.recv())
                .await
                .expect("vitals emission before timeout")
                .expect("bus open");
            if let AppEvent::SessionVitals { session_id, vitals } = event {
                assert_eq!(session_id, "cc-session");
                assert_eq!(vitals.cache.as_ref().unwrap().hit_pct, Some(80));
                assert_eq!(vitals.cache.as_ref().unwrap().ttl_seconds, Some(3600));
                assert!(vitals.git.is_none(), "no git target, no git section");
                break;
            }
        }
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
            used_pct: Some(49),
            resets_at_epoch: Some(1_783_807_200),
            status: None,
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
        assert_eq!(vitals.limits[0].used_pct, Some(49));

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
                    if session_id == "s7"
                        && vitals.cache.as_ref().and_then(|c| c.hit_pct) == Some(90)
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
    async fn activity_listener_folds_snapshots_without_blanking_sections() {
        let bus = EventBus::new();
        let hub = SessionVitalsHub::new(bus.clone());
        let _listener = spawn_cache_vitals_listener(bus.clone(), hub.clone());
        let mut rx = bus.subscribe();

        // Seed another producer's section first.
        hub.apply("s9", |v| {
            v.git = Some(SessionGitVitals {
                branch: "main".into(),
                ..Default::default()
            })
        });
        let activity = crate::types::SessionActivityVitals {
            state: crate::types::SessionActivityState::Reasoning,
            since_epoch: 100,
            last_stream_byte_epoch: 104,
            stalled_after_seconds: Some(20),
            effort: Some("max".into()),
            resets_at_epoch: None,
        };
        bus.send(AppEvent::SessionActivity {
            session_id: Some("s9".into()),
            activity: activity.clone(),
        });
        let vitals = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(AppEvent::SessionVitals { session_id, vitals }) = rx.recv().await {
                    if session_id == "s9" && vitals.activity.is_some() {
                        return vitals;
                    }
                }
            }
        })
        .await
        .expect("listener folds the activity section");
        assert_eq!(vitals.activity.as_ref(), Some(&activity));
        assert!(
            vitals.git.is_some(),
            "activity writes must not blank other sections"
        );

        // Id-less snapshots (a native loop without a session id) are
        // skipped, not folded under an empty key.
        bus.send(AppEvent::SessionActivity {
            session_id: None,
            activity: activity.clone(),
        });
        bus.send(AppEvent::SessionActivity {
            session_id: Some("s9".into()),
            activity: crate::types::SessionActivityVitals {
                state: crate::types::SessionActivityState::Idle,
                since_epoch: 200,
                last_stream_byte_epoch: 104,
                ..Default::default()
            },
        });
        let vitals = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(AppEvent::SessionVitals { session_id, vitals }) = rx.recv().await {
                    if session_id == "s9"
                        && vitals.activity.as_ref().map(|a| a.since_epoch) == Some(200)
                    {
                        return vitals;
                    }
                }
            }
        })
        .await
        .expect("listener folds the follow-up snapshot");
        assert_eq!(
            vitals.activity.as_ref().map(|a| a.state),
            Some(crate::types::SessionActivityState::Idle)
        );
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

    /// The dashboard's vitals symbol catalog (static/app/39-session-windows.js,
    /// VITALS_SYMBOLS_BEGIN..END) is the single source for every vitals chip,
    /// rail tooltip, and tap-to-explain popover — and the backend-parity
    /// surface: all three backends render the same symbol grammar from these
    /// wire fields. Pin both directions so a `SessionVitals` schema change or
    /// a catalog refactor fails here instead of shipping as silent drift.
    #[test]
    fn vitals_symbol_catalog_covers_wire_fields() {
        let fragment = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("static/app/39-session-windows.js"),
        )
        .expect("dashboard session-windows fragment");
        let begin = fragment
            .find("VITALS_SYMBOLS_BEGIN")
            .expect("catalog begin marker");
        let end = fragment
            .find("VITALS_SYMBOLS_END")
            .expect("catalog end marker");
        let catalog = &fragment[begin..end];
        for key in [
            "health:",
            "activity:",
            "branch:",
            "worktree:",
            "dirty:",
            "divergence:",
            "parity:",
            "unpushed:",
            "'primary-unpushed':",
            "'cache-hit':",
            "'cache-ttl':",
            "limit:",
        ] {
            assert!(catalog.contains(key), "catalog lost symbol {key}");
        }
        // The serde-camelCase wire fields of SessionVitals/SessionGitVitals/
        // SessionCacheVitals/SessionLimitWindow/SessionActivityVitals the
        // catalog must consume.
        for field in [
            "branch",
            "dirtyFiles",
            "primaryRef",
            "ahead",
            "behind",
            "mergeParity",
            "unpushed",
            "primaryUnpushed",
            "hitPct",
            "ttlSeconds",
            "lastActivityEpoch",
            "usedPct",
            "resetsAtEpoch",
            "status",
            "label",
            "state",
            "sinceEpoch",
            "lastStreamByteEpoch",
            "stalledAfterSeconds",
            "effort",
        ] {
            assert!(
                catalog.contains(field),
                "catalog stopped consuming wire field {field} — SessionVitals and VITALS_SYMBOLS drifted"
            );
        }
        // The wire spellings of the activity states the catalog renders —
        // and the derived-only `stalled` — must all be explained.
        for state in [
            "reasoning",
            "responding",
            "tool-running",
            "awaiting-api",
            "rate-limited",
            "stalled",
        ] {
            assert!(
                catalog.contains(state),
                "catalog stopped handling activity state {state}"
            );
        }
    }
}

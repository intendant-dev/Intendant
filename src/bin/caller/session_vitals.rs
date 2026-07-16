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
//!   the live target registry. Each target follows the session's write
//!   activity: when `AppEvent::SessionFileActivity` paths resolve inside a
//!   different git checkout than the registered root (e.g. a worktree the
//!   session entered by absolute path without registering it), the probe
//!   target switches to that checkout — most-recent-wins with mild
//!   hysteresis, registered root kept as the fallback identity.
//! - **Cache segment**: a bus listener over `AppEvent::UsageSnapshot` —
//!   every backend's usage rail converges there (external drains, the
//!   native derivation in `tui/app.rs`), so one listener covers Claude
//!   Code, Codex, and native sessions uniformly. Computes the latest
//!   request's cache-hit receipt and carries the TTL anchor; the countdown
//!   itself derives client-side from `last_activity_epoch + ttl_seconds`
//!   (no per-second events).
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

/// Bus listener feeding the cache section: every backend's usage rail
/// converges on `AppEvent::UsageSnapshot`, so this one consumer covers
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

/// Activity events that must resolve to the same non-current checkout
/// before the probe target switches to it (mild anti-flap hysteresis; the
/// same threshold applies to switching back to the registered root).
const LOCUS_SWITCH_SIGHTINGS: u32 = 2;

/// Resolve the git checkout containing `path`: the nearest ancestor with a
/// `.git` entry — a directory for ordinary checkouts, a FILE for linked
/// worktrees — via pure filesystem stats (no subprocess). `None` when no
/// ancestor is a checkout. The leaf itself may not exist yet (a write
/// about to create it): nonexistent components simply don't match and the
/// walk climbs on.
fn resolve_git_checkout_root(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(dir) = current {
        if dir.join(".git").symlink_metadata().is_ok() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// One session's probe target: the registered root (the session's durable
/// identity — never lost) plus the activity-locus overlay.
struct GitTarget {
    registered_root: PathBuf,
    /// Checkout the session's recent write activity resolved into, when it
    /// differs from the registered root's checkout. Overrides the probe
    /// target while set; cleared when activity returns home or the locus
    /// stops probing (worktree deleted).
    active_locus: Option<PathBuf>,
    /// Pending switch: (proposed override, activity events seen). A `None`
    /// proposal is "switch back to the registered root". Reset whenever an
    /// event confirms the current target instead.
    candidate: Option<(Option<PathBuf>, u32)>,
}

impl GitTarget {
    fn new(registered_root: PathBuf) -> Self {
        Self {
            registered_root,
            active_locus: None,
            candidate: None,
        }
    }

    fn effective(&self) -> PathBuf {
        self.active_locus
            .clone()
            .unwrap_or_else(|| self.registered_root.clone())
    }

    /// Fold one activity event's write paths into the locus state.
    /// Relative paths and paths outside any checkout are ignored; each
    /// distinct checkout counts once per event, so a burst of writes in
    /// one batch is a single sighting.
    fn observe_write_activity(&mut self, paths: &[String]) {
        let home_checkout = resolve_git_checkout_root(&self.registered_root);
        let mut seen_this_event: Vec<PathBuf> = Vec::new();
        for path in paths {
            let path = Path::new(path);
            if !path.is_absolute() {
                continue;
            }
            let Some(checkout) = resolve_git_checkout_root(path) else {
                continue;
            };
            if seen_this_event.contains(&checkout) {
                continue;
            }
            seen_this_event.push(checkout.clone());
            let proposal = if Some(&checkout) == home_checkout.as_ref() {
                None
            } else {
                Some(checkout)
            };
            if proposal == self.active_locus {
                // Activity confirms the current target — most-recent-wins
                // means a stale candidate no longer represents the recent
                // trend, so drop it.
                self.candidate = None;
                continue;
            }
            match &mut self.candidate {
                Some((candidate, sightings)) if *candidate == proposal => {
                    *sightings += 1;
                    if *sightings >= LOCUS_SWITCH_SIGHTINGS {
                        self.active_locus = proposal;
                        self.candidate = None;
                    }
                }
                _ => self.candidate = Some((proposal, 1)),
            }
        }
    }
}

/// Live registry of (session id → working dir) git-probe targets. The
/// daemon seeds the primary session at startup; the session supervisor
/// registers every managed session at launch — which is what puts the
/// dirty-count / merge-parity / unpushed rows on dashboard-spawned
/// sessions and on projectless daemons (whose primary has no repo) —
/// and a boot-time scan restores targets for the store's non-ended
/// sessions (see [`register_restored_session_targets`]) so idle
/// session windows keep their chips across daemon restarts.
/// `SessionEnded` prunes entries, so a handle owner only has to register.
///
/// Each entry also tracks the session's activity locus: write paths from
/// `AppEvent::SessionFileActivity` that resolve inside a different git
/// checkout retarget the probe (see [`GitTarget`]), so the git chip shows
/// the checkout actually being worked in — e.g. a nested worktree the
/// session entered by absolute path without registering it.
#[derive(Clone, Default)]
pub(crate) struct GitVitalsTargets {
    targets: Arc<Mutex<HashMap<String, GitTarget>>>,
}

impl GitVitalsTargets {
    /// Register (or retarget) a session's git probe root. No-ops on empty
    /// ids so callers can pass through unresolved values unchecked.
    /// Re-registering resets any activity-locus override — registration is
    /// an explicit statement of where the session lives.
    pub(crate) fn register(&self, session_id: &str, cwd: PathBuf) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        self.targets
            .lock()
            .expect("git vitals targets lock")
            .insert(session_id.to_string(), GitTarget::new(cwd));
    }

    /// Register a session RESTORED from the on-disk session store at
    /// daemon boot, unless the id already has a live registration —
    /// launch/resume registrations (and the primary seed) carry the
    /// freshest root and an explicit statement of where the session
    /// lives, so the restore scan must never clobber one or reset its
    /// activity locus. Returns whether the target was inserted.
    pub(crate) fn register_restored(&self, session_id: &str, cwd: PathBuf) -> bool {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return false;
        }
        match self
            .targets
            .lock()
            .expect("git vitals targets lock")
            .entry(session_id.to_string())
        {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(GitTarget::new(cwd));
                true
            }
        }
    }

    pub(crate) fn remove(&self, session_id: &str) {
        self.targets
            .lock()
            .expect("git vitals targets lock")
            .remove(session_id.trim());
    }

    /// Effective probe targets: the activity locus where one is active,
    /// the registered root otherwise.
    fn snapshot(&self) -> Vec<(String, PathBuf)> {
        self.targets
            .lock()
            .expect("git vitals targets lock")
            .iter()
            .map(|(id, target)| (id.clone(), target.effective()))
            .collect()
    }

    /// Fold a session's write activity into its locus state (no-op for
    /// unregistered ids).
    fn observe_write_activity(&self, session_id: &str, paths: &[String]) {
        let mut targets = self.targets.lock().expect("git vitals targets lock");
        if let Some(target) = targets.get_mut(session_id.trim()) {
            target.observe_write_activity(paths);
        }
    }

    /// The active locus stopped probing (worktree deleted, repo gone):
    /// fall back to the registered root. Returns the root to re-probe when
    /// `failed_target` was indeed the session's active locus.
    fn demote_locus(&self, session_id: &str, failed_target: &Path) -> Option<PathBuf> {
        let mut targets = self.targets.lock().expect("git vitals targets lock");
        let target = targets.get_mut(session_id.trim())?;
        if target.active_locus.as_deref() != Some(failed_target) {
            return None;
        }
        target.active_locus = None;
        target.candidate = None;
        Some(target.registered_root.clone())
    }
}

/// Cap on boot-time registration of restored sessions. A long-lived
/// store accumulates thousands of non-ended session dirs (a real store
/// measured ~2.1k across ~100 distinct roots), and every registered
/// target costs a per-tick probe plus an emission — and a session-log
/// write — for every session sharing a root whenever that root's git
/// state changes. The newest N by meta mtime cover the session windows
/// a dashboard realistically shows; older idle sessions regain their
/// chips the moment they are resumed (launch-time registration, as
/// before).
const RESTORED_TARGET_CAP: usize = 64;

/// Register git-probe targets for sessions RESTORED from the on-disk
/// session store (`<home>/.intendant/logs`) — the daemon-boot
/// complement to the supervisor's launch/resume registration. Without
/// it a restart empties the registry, and idle session windows lose
/// their git/health chips until the next resume touches them.
///
/// Scope mirrors the `SessionEnded` prune: a `completed` meta means the
/// session ended before the restart (the prune would have dropped it),
/// so it stays unregistered; `idle` / `interrupted` / stale `running`
/// sessions were live in a daemon's registry when it died and come
/// back. Worktree sessions register their CHECKOUT
/// (`meta.worktree.path`), exactly like launch-time registration.
/// The newest [`RESTORED_TARGET_CAP`] sessions win (meta-file mtime —
/// re-stamped on every lifecycle transition — is the recency key), and
/// registration is insert-if-absent so the primary seed and any
/// launch/resume racing the scan keep their fresher roots.
///
/// Returns how many sessions were registered. Synchronous filesystem
/// walk — call it from a blocking context.
pub(crate) fn register_restored_session_targets(home: &Path, registry: &GitVitalsTargets) -> usize {
    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
    let Ok(entries) = std::fs::read_dir(&logs_dir) else {
        return 0;
    };
    // (meta mtime, session id, effective root)
    let mut restored: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let meta_path = entry.path().join("session_meta.json");
        let Ok(raw) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<crate::session_log::SessionMeta>(&raw) else {
            continue;
        };
        // Parity with the SessionEnded prune: completed = ended.
        if meta.status.as_deref() == Some("completed") {
            continue;
        }
        let root = meta
            .worktree
            .as_ref()
            .map(|worktree| worktree.path.clone())
            .or(meta.project_root);
        let Some(root) = root.filter(|root| !root.trim().is_empty()) else {
            continue;
        };
        let session_id = meta.session_id.trim().to_string();
        if session_id.is_empty() {
            continue;
        }
        let mtime = meta_path
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        restored.push((mtime, session_id, PathBuf::from(root)));
    }
    // Newest first; the cap keeps probe work and emission fan-out bounded.
    restored.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    restored
        .into_iter()
        .take(RESTORED_TARGET_CAP)
        .filter(|(_, session_id, root)| registry.register_restored(session_id, root.clone()))
        .count()
}

/// Probe `cwd` through the per-tick cache: sessions sharing a checkout
/// (the common shape once restored sessions register at boot — many
/// idle sessions per project root) pay for one probe per tick instead
/// of one per session. Git state is a pure function of the cwd within
/// a tick, so the shared result is exact.
async fn probe_cached(
    prober: &mut GitVitalsProber,
    tick_cache: &mut HashMap<PathBuf, Option<SessionGitVitals>>,
    cwd: &Path,
) -> Option<SessionGitVitals> {
    if let Some(cached) = tick_cache.get(cwd) {
        return cached.clone();
    }
    let probed = prober.probe(cwd).await;
    tick_cache.insert(cwd.to_path_buf(), probed.clone());
    probed
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
    let _target_maintainer = spawn_git_target_maintainer(bus, registry.clone(), hub.clone());
    let handle = tokio::spawn({
        let registry = registry.clone();
        async move {
            let mut prober = GitVitalsProber::default();
            loop {
                let mut tick_cache: HashMap<PathBuf, Option<SessionGitVitals>> = HashMap::new();
                for (session_id, cwd) in registry.snapshot() {
                    let mut probed = probe_cached(&mut prober, &mut tick_cache, &cwd).await;
                    if probed.is_none() {
                        // A dead activity locus (worktree deleted, checkout
                        // gone) falls back to the registered root in the
                        // same tick so the chip never blanks.
                        if let Some(root) = registry.demote_locus(&session_id, &cwd) {
                            probed = probe_cached(&mut prober, &mut tick_cache, &root).await;
                        }
                    }
                    hub.apply(&session_id, |vitals| vitals.git = probed);
                }
                tokio::time::sleep(PROBE_INTERVAL).await;
            }
        }
    });
    (registry, handle)
}

/// Bus maintenance for the git-target registry:
///
/// - `SessionEnded` prunes targets — mirrors the cache listener's hygiene
///   so registered sessions never leak probe work past their lifetime.
/// - `SessionFileActivity` folds a session's structured write paths into
///   its activity-locus state, retargeting the probe when the work moved
///   into a different checkout (see [`GitTarget::observe_write_activity`]).
///
/// Resolution runs through the hub's alias map both ways: resume lanes
/// register the live (backend-native) id while events may carry the
/// wrapper id, and vice versa.
fn spawn_git_target_maintainer(
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
                Ok(AppEvent::SessionFileActivity {
                    session_id: Some(session_id),
                    paths,
                }) => {
                    let canonical = hub.resolve(&session_id);
                    for (id, _) in registry.snapshot() {
                        if hub.resolve(&id) == canonical {
                            registry.observe_write_activity(&id, &paths);
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

    #[test]
    fn checkout_resolution_walks_to_git_dir_or_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        // Ordinary checkout: `.git` directory.
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        // Linked worktree nested INSIDE the repo (the real failure case:
        // `<root>/.claude/worktrees/<name>`): `.git` is a FILE.
        let worktree = repo.join(".claude/worktrees/session-fork");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere\n").unwrap();

        assert_eq!(
            resolve_git_checkout_root(&repo.join("src/main.rs")),
            Some(repo.clone()),
            "file under an ordinary checkout resolves to it"
        );
        assert_eq!(
            resolve_git_checkout_root(&worktree.join("src/lib.rs")),
            Some(worktree.clone()),
            "a nested worktree's .git FILE wins over the enclosing repo"
        );
        // The leaf need not exist (a write about to create it).
        assert_eq!(
            resolve_git_checkout_root(&worktree.join("deep/new/dir/file.rs")),
            Some(worktree.clone()),
            "nonexistent leading components climb to the checkout"
        );
        assert_eq!(
            resolve_git_checkout_root(&worktree),
            Some(worktree.clone()),
            "the checkout root itself resolves to itself"
        );
        // No checkout anywhere up the chain.
        let bare = root.join("no-repo/sub");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(resolve_git_checkout_root(&bare.join("f.txt")), None);
    }

    #[test]
    fn write_activity_switches_target_with_hysteresis_both_ways() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let worktree = repo.join(".claude/worktrees/wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere\n").unwrap();

        let targets = GitVitalsTargets::default();
        targets.register("s1", repo.clone());
        let effective = |targets: &GitVitalsTargets| targets.snapshot()[0].1.clone();
        let wt_file = || vec![worktree.join("src/a.rs").to_string_lossy().into_owned()];
        let home_file = || vec![repo.join("src/b.rs").to_string_lossy().into_owned()];

        // One sighting is not enough (anti-flap).
        targets.observe_write_activity("s1", &wt_file());
        assert_eq!(effective(&targets), repo, "single sighting must not switch");
        // Second sighting switches to the worktree.
        targets.observe_write_activity("s1", &wt_file());
        assert_eq!(effective(&targets), worktree, "two sightings switch");
        // Alternating activity never flaps: a home sighting arms a
        // candidate, but the next worktree write confirms the current
        // target and clears it.
        targets.observe_write_activity("s1", &home_file());
        assert_eq!(effective(&targets), worktree);
        targets.observe_write_activity("s1", &wt_file());
        assert_eq!(effective(&targets), worktree);
        targets.observe_write_activity("s1", &home_file());
        assert_eq!(
            effective(&targets),
            worktree,
            "one home sighting is not enough"
        );
        // Two consecutive home sightings switch back to the registered root.
        targets.observe_write_activity("s1", &home_file());
        assert_eq!(
            effective(&targets),
            repo,
            "activity back home switches back"
        );

        // Relative paths and non-checkout paths are ignored entirely.
        targets.observe_write_activity("s1", &["relative/path.rs".to_string()]);
        targets.observe_write_activity("s1", &["relative/path.rs".to_string()]);
        let outside = temp.path().join("no-repo/x.rs");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        let outside = vec![outside.to_string_lossy().into_owned()];
        targets.observe_write_activity("s1", &outside);
        targets.observe_write_activity("s1", &outside);
        assert_eq!(
            effective(&targets),
            repo,
            "relative / checkout-less paths never retarget"
        );

        // A multi-path burst in ONE event counts once per checkout.
        let burst = vec![
            worktree.join("one.rs").to_string_lossy().into_owned(),
            worktree.join("two.rs").to_string_lossy().into_owned(),
        ];
        targets.observe_write_activity("s1", &burst);
        assert_eq!(effective(&targets), repo, "one burst = one sighting");
        targets.observe_write_activity("s1", &burst);
        assert_eq!(
            effective(&targets),
            worktree,
            "second event completes the switch"
        );

        // Re-registering resets the locus override.
        targets.register("s1", repo.clone());
        assert_eq!(effective(&targets), repo, "re-register resets the locus");
    }

    #[test]
    fn dead_locus_demotes_to_registered_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let worktree = repo.join(".claude/worktrees/wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere\n").unwrap();

        let targets = GitVitalsTargets::default();
        targets.register("s1", repo.clone());
        let path = vec![worktree.join("f.rs").to_string_lossy().into_owned()];
        targets.observe_write_activity("s1", &path);
        targets.observe_write_activity("s1", &path);
        assert_eq!(targets.snapshot()[0].1, worktree);

        // Demoting a path that is NOT the active locus is a no-op.
        assert_eq!(targets.demote_locus("s1", &repo), None);
        assert_eq!(targets.snapshot()[0].1, worktree);
        // The active locus failing its probe falls back to the root.
        assert_eq!(targets.demote_locus("s1", &worktree), Some(repo.clone()));
        assert_eq!(targets.snapshot()[0].1, repo);
        // Unknown sessions are a calm no-op.
        assert_eq!(targets.demote_locus("nope", &worktree), None);
    }

    /// Seed one restored-session record (`session_meta.json`) under
    /// `<home>/.intendant/logs/<id>/`, the store layout the boot scan
    /// walks. Returns the meta path (tests stagger its mtime).
    fn write_restored_meta(
        home: &Path,
        session_id: &str,
        status: &str,
        project_root: Option<&Path>,
        worktree_path: Option<&Path>,
    ) -> PathBuf {
        let dir = home.join(".intendant").join("logs").join(session_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = crate::session_log::SessionMeta {
            session_id: session_id.to_string(),
            created_at: "2026-07-16T00:00:00".to_string(),
            created_at_ms: None,
            project_root: project_root.map(|p| p.to_string_lossy().to_string()),
            name: None,
            task: None,
            status: Some(status.to_string()),
            last_turn: None,
            role: None,
            rounds: None,
            worktree: worktree_path.map(|p| crate::session_log::SessionWorktreeMeta {
                branch: "wt-branch".to_string(),
                path: p.to_string_lossy().to_string(),
                base_root: home.to_string_lossy().to_string(),
                base_branch: None,
                base_sha: None,
            }),
        };
        let meta_path = dir.join("session_meta.json");
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
        meta_path
    }

    #[tokio::test]
    async fn restore_scan_registers_non_ended_sessions_and_first_tick_emits() {
        // The daemon-restart gap: sessions restored from the store used to
        // stay unregistered until resumed, so idle windows lost their git
        // chips. The boot scan must register every non-ended session (the
        // SessionEnded-prune parity) and the first probe tick must emit a
        // git section for it — registration alone repopulates the hub.
        let home = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        git_cmd(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("a.txt"), "one\n").unwrap();
        git_cmd(repo.path(), &["add", "."]);
        git_cmd(repo.path(), &["commit", "-qm", "base"]);

        write_restored_meta(
            home.path(),
            "restored-idle",
            "idle",
            Some(repo.path()),
            None,
        );
        // Ended before the restart: the prune would have dropped it.
        write_restored_meta(home.path(), "ended", "completed", Some(repo.path()), None);
        // No root recorded: nothing to probe.
        write_restored_meta(home.path(), "rootless", "idle", None, None);

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (targets, _producer) = spawn_session_vitals_producer(bus.clone(), Vec::new());
        assert_eq!(
            register_restored_session_targets(home.path(), &targets),
            1,
            "only the non-ended rooted session registers"
        );
        let snapshot = targets.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].0, "restored-idle");
        assert_eq!(snapshot[0].1, repo.path());

        let deadline = std::time::Duration::from_secs(20);
        let vitals = tokio::time::timeout(deadline, async {
            loop {
                if let Ok(AppEvent::SessionVitals { session_id, vitals }) = rx.recv().await {
                    assert_eq!(
                        session_id, "restored-idle",
                        "only the restored session emits"
                    );
                    return vitals;
                }
            }
        })
        .await
        .expect("restored session emits git vitals on the first tick");
        assert_eq!(vitals.git.expect("git section").branch, "main");
    }

    #[test]
    fn restore_scan_prefers_worktree_checkout_over_project_root() {
        // Worktree sessions must register their CHECKOUT (a `.git`-FILE
        // linked worktree), mirroring launch-time registration — even when
        // a later resume rewrote meta.project_root to the base root.
        let home = tempfile::tempdir().expect("tempdir");
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let checkout = repo.join(".claude/worktrees/wt");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join(".git"), "gitdir: elsewhere\n").unwrap();

        write_restored_meta(
            home.path(),
            "wt-session",
            "idle",
            Some(&repo),
            Some(&checkout),
        );

        let targets = GitVitalsTargets::default();
        assert_eq!(register_restored_session_targets(home.path(), &targets), 1);
        let snapshot = targets.snapshot();
        assert_eq!(snapshot[0].0, "wt-session");
        assert_eq!(
            snapshot[0].1, checkout,
            "the worktree checkout wins over the recorded project root"
        );
    }

    #[test]
    fn restore_scan_caps_at_newest_and_never_clobbers_live_registrations() {
        let home = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("tempdir");
        // CAP + 3 non-ended sessions with deterministic, strictly
        // increasing mtimes: s-0..s-2 are the oldest and must lose.
        let total = RESTORED_TARGET_CAP + 3;
        for i in 0..total {
            let meta_path = write_restored_meta(
                home.path(),
                &format!("s-{i}"),
                "idle",
                Some(root.path()),
                None,
            );
            let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + i as u64);
            std::fs::File::options()
                .write(true)
                .open(&meta_path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        }

        // One of the newest ids is already live-registered (launch/resume
        // raced the scan): the restore pass must not clobber its root.
        let live_root = tempfile::tempdir().expect("tempdir");
        let targets = GitVitalsTargets::default();
        let live_id = format!("s-{}", total - 1);
        targets.register(&live_id, live_root.path().to_path_buf());

        let registered = register_restored_session_targets(home.path(), &targets);
        assert_eq!(
            registered,
            RESTORED_TARGET_CAP - 1,
            "cap slots minus the already-live session"
        );
        let snapshot: HashMap<String, PathBuf> = targets.snapshot().into_iter().collect();
        assert_eq!(snapshot.len(), RESTORED_TARGET_CAP);
        for i in 0..3 {
            assert!(
                !snapshot.contains_key(&format!("s-{i}")),
                "oldest sessions fall past the cap"
            );
        }
        assert_eq!(
            snapshot.get(&live_id),
            Some(&live_root.path().to_path_buf()),
            "live registration survives the restore scan"
        );
        assert_eq!(
            snapshot.get(&format!("s-{}", total - 2)),
            Some(&root.path().to_path_buf()),
            "newest restored sessions register their recorded root"
        );
    }

    #[tokio::test]
    async fn same_root_sessions_share_one_probe_and_both_emit() {
        // Restored stores routinely hold many idle sessions per project
        // root; the per-tick probe cache must serve them all one probe's
        // worth of git state without dropping anyone's emission.
        let repo = tempfile::tempdir().expect("tempdir");
        git_cmd(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("a.txt"), "one\n").unwrap();
        git_cmd(repo.path(), &["add", "."]);
        git_cmd(repo.path(), &["commit", "-qm", "base"]);

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (targets, _producer) = spawn_session_vitals_producer(bus.clone(), Vec::new());
        targets.register("shared-a", repo.path().to_path_buf());
        targets.register("shared-b", repo.path().to_path_buf());

        let deadline = std::time::Duration::from_secs(20);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        tokio::time::timeout(deadline, async {
            while seen.len() < 2 {
                if let Ok(AppEvent::SessionVitals { session_id, vitals }) = rx.recv().await {
                    let git = vitals.git.expect("git section probed");
                    assert_eq!(git.branch, "main");
                    seen.insert(session_id);
                }
            }
        })
        .await
        .expect("both same-root sessions emit git vitals");
        assert!(seen.contains("shared-a") && seen.contains("shared-b"));
    }

    #[tokio::test]
    async fn activity_locus_follows_worktree_and_falls_back_when_deleted() {
        // End-to-end through the producer: a session registered at the
        // repo root writes (via absolute paths) inside a nested linked
        // worktree it never registered — the git chip must follow the
        // activity and report the worktree's branch, then fall back to the
        // registered root when the worktree disappears.
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        git_cmd(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git_cmd(root, &["add", "."]);
        git_cmd(root, &["commit", "-qm", "base"]);
        let worktree = root.join(".claude/worktrees/session-fork");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        git_cmd(
            root,
            &[
                "worktree",
                "add",
                "-b",
                "worktree-session-fork",
                worktree.to_str().unwrap(),
            ],
        );

        let bus = EventBus::new();
        // Subscribed before the producer spawns, so every change-only hub
        // emission is queued here — sequential waits never miss one.
        let mut rx = bus.subscribe();
        let (targets, _producer) = spawn_session_vitals_producer(bus.clone(), Vec::new());
        targets.register("s-fork", root.to_path_buf());

        let deadline = std::time::Duration::from_secs(20);
        async fn wait_for_branch(rx: &mut tokio::sync::broadcast::Receiver<AppEvent>, want: &str) {
            loop {
                if let Ok(AppEvent::SessionVitals { session_id, vitals }) = rx.recv().await {
                    assert_eq!(session_id, "s-fork");
                    if vitals.git.as_ref().is_some_and(|g| g.branch == want) {
                        return;
                    }
                }
            }
        }
        tokio::time::timeout(deadline, wait_for_branch(&mut rx, "main"))
            .await
            .expect("registered root probes on main before activity");

        // Two write-activity events inside the (unregistered) worktree.
        let wt_path = worktree.join("src/new.rs").to_string_lossy().into_owned();
        for _ in 0..2 {
            bus.send(AppEvent::SessionFileActivity {
                session_id: Some("s-fork".into()),
                paths: vec![wt_path.clone()],
            });
        }
        tokio::time::timeout(deadline, wait_for_branch(&mut rx, "worktree-session-fork"))
            .await
            .expect("git chip follows the activity into the worktree");

        // Worktree deleted out from under the session: fall back to the
        // registered root (same tick — the chip never blanks).
        std::fs::remove_dir_all(&worktree).unwrap();
        tokio::time::timeout(deadline, wait_for_branch(&mut rx, "main"))
            .await
            .expect("dead locus falls back to the registered root");
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
        // SessionCacheVitals/SessionLimitWindow the catalog must consume.
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
        ] {
            assert!(
                catalog.contains(field),
                "catalog stopped consuming wire field {field} — SessionVitals and VITALS_SYMBOLS drifted"
            );
        }
    }
}

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
use crate::types::{SessionCacheVitals, SessionConfigVitals, SessionGitVitals, SessionVitals};

/// Probe cadence. Each tick is a couple of subprocess ref reads per
/// distinct checkout; emission only happens when the probed state changes.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Hard ceiling on any single git subprocess. Probes are local ref reads
/// that normally finish in milliseconds; a hung git (checkout on a dead
/// network filesystem, a wedged lock) must fail its probe — feeding the
/// existing `demote_locus` fallback — instead of freezing the sequential
/// producer loop, and with it every session's git chip, forever.
const GIT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run one git subprocess under the anti-wedge guards: `kill_on_drop` so
/// a timed-out child is reaped instead of orphaned, and `timeout` so no
/// single invocation can stall the producer loop. `None` on spawn failure
/// or timeout — callers treat both like the command failing.
async fn run_git(
    program: &std::ffi::OsStr,
    timeout: std::time::Duration,
    cwd: &Path,
    args: &[&str],
) -> Option<std::process::Output> {
    let output = tokio::process::Command::new(program)
        .arg("-C")
        .arg(cwd)
        .args(args)
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(timeout, output).await.ok()?.ok()
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

/// Loose bound on the prober's per-path/per-checkout caches: entries are
/// tiny, the live target set is small, and a full clear simply re-resolves
/// on the next tick — cheap insurance against daemon-lifetime growth.
const PROBER_CACHE_CAP: usize = 256;

/// Facts one `git status --porcelain=v2 --branch` run yields — the
/// collapsed replacement for the old separate branch / dirty-count /
/// upstream-verify / unpushed-count subprocess chain.
#[derive(Debug, Default, PartialEq, Eq)]
struct StatusFacts {
    /// `branch.head`, mapped to the spellings the old
    /// `rev-parse --abbrev-ref HEAD` emitted: `HEAD` when detached, empty
    /// on an unborn branch (where the old probe failed).
    branch: String,
    /// `branch.oid`: HEAD's sha — the merge-parity cache key's HEAD side.
    /// `None` on an unborn branch (`(initial)`).
    head_oid: Option<String>,
    /// `branch.ab` as (ahead, behind) vs the upstream. Git prints the line
    /// only when the upstream ref actually resolves — exactly the old
    /// `@{upstream}` verify condition — and the ahead column IS the
    /// unpushed count, so `None` here means "no upstream to check".
    upstream_ab: Option<(u32, u32)>,
    /// Non-header entry lines: changed + unmerged + untracked. One line
    /// per path, matching porcelain v1's line count (renames included —
    /// both formats spend one line per rename).
    dirty_files: u32,
}

fn parse_status_v2(output: &str) -> StatusFacts {
    let mut facts = StatusFacts::default();
    let mut unborn = false;
    for line in output.lines() {
        if let Some(header) = line.strip_prefix("# ") {
            if let Some(head) = header.strip_prefix("branch.head ") {
                facts.branch = match head.trim() {
                    "(detached)" => "HEAD".to_string(),
                    name => name.to_string(),
                };
            } else if let Some(oid) = header.strip_prefix("branch.oid ") {
                match oid.trim() {
                    "(initial)" => unborn = true,
                    oid => facts.head_oid = Some(oid.to_string()),
                }
            } else if let Some(ab) = header.strip_prefix("branch.ab ") {
                facts.upstream_ab = parse_status_ab(ab);
            }
        } else if !line.trim().is_empty() {
            facts.dirty_files += 1;
        }
    }
    if unborn {
        // Parity with the old chain: `rev-parse --abbrev-ref HEAD` fails
        // on an unborn branch, so the emitted branch was empty.
        facts.branch = String::new();
    }
    facts
}

/// `branch.ab` payload: `+<ahead> -<behind>`.
fn parse_status_ab(ab: &str) -> Option<(u32, u32)> {
    let mut cols = ab.split_whitespace();
    let ahead = cols.next()?.strip_prefix('+')?.parse().ok()?;
    let behind = cols.next()?.strip_prefix('-')?.parse().ok()?;
    Some((ahead, behind))
}

/// Git prober. One collapsed status + rev-list chain per checkout per
/// tick, plus three caches that keep the steady state at two subprocesses
/// where the old chain ran a dozen:
///
/// - per-(HEAD, primary) merge-parity verdicts — the expensive in-memory
///   merge only reruns when either side moves;
/// - registered path → checkout toplevel — resolved once per distinct
///   path, and the per-tick dedup key, so same-checkout targets share one
///   probe;
/// - checkout → primary branch discovery — rediscovered only when the
///   cached primary ref stops resolving.
pub(crate) struct GitVitalsProber {
    merge_cache: HashMap<(String, String), String>,
    /// Registered path → its checkout's toplevel (`--show-toplevel`).
    /// Dropped when the checkout stops probing so a deleted or replaced
    /// checkout re-resolves instead of pinning stale state.
    toplevel_cache: HashMap<PathBuf, PathBuf>,
    /// Checkout toplevel → (primary branch, resolved comparison ref).
    /// Invalidated by a failed ahead/behind rev-list (see
    /// [`Self::probe_checkout`]); never caches a negative — a repo with no
    /// discoverable primary re-runs discovery each tick, as the old
    /// per-tick chain did.
    primary_cache: HashMap<PathBuf, (String, String)>,
    /// Program spawned for every probe — `git` in production; tests inject
    /// a stub to prove the timeout guard without a hung real git.
    git_program: std::ffi::OsString,
    /// Per-subprocess ceiling ([`GIT_PROBE_TIMEOUT`]); tests shrink it.
    probe_timeout: std::time::Duration,
    /// Checkout probes actually run — the tick-dedup observability seam
    /// (same-checkout targets must share one probe per tick).
    checkout_probes: u64,
}

impl Default for GitVitalsProber {
    fn default() -> Self {
        Self {
            merge_cache: HashMap::new(),
            toplevel_cache: HashMap::new(),
            primary_cache: HashMap::new(),
            git_program: "git".into(),
            probe_timeout: GIT_PROBE_TIMEOUT,
            checkout_probes: 0,
        }
    }
}

impl GitVitalsProber {
    async fn git(&self, cwd: &Path, args: &[&str]) -> Option<String> {
        let output = run_git(&self.git_program, self.probe_timeout, cwd, args).await?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn git_count(&self, cwd: &Path, range: &str) -> Option<u32> {
        self.git(cwd, &["rev-list", "--count", range])
            .await?
            .parse()
            .ok()
    }

    /// Canonical probe key for `cwd`: its checkout's toplevel, resolved
    /// once per distinct registered path and cached. Distinct paths inside
    /// one checkout (the root and a subdirectory) share a toplevel — and
    /// therefore one probe per tick — while linked worktrees have their
    /// own toplevels and correctly keep their own probes. `None` when
    /// `cwd` is not inside a working tree.
    async fn toplevel_for(&mut self, cwd: &Path) -> Option<PathBuf> {
        if let Some(toplevel) = self.toplevel_cache.get(cwd) {
            return Some(toplevel.clone());
        }
        let toplevel = PathBuf::from(self.git(cwd, &["rev-parse", "--show-toplevel"]).await?);
        if self.toplevel_cache.len() > PROBER_CACHE_CAP {
            self.toplevel_cache.clear();
        }
        self.toplevel_cache
            .insert(cwd.to_path_buf(), toplevel.clone());
        Some(toplevel)
    }

    pub(crate) async fn probe(&mut self, cwd: &Path) -> Option<SessionGitVitals> {
        let toplevel = self.toplevel_for(cwd).await?;
        let probed = self.probe_checkout(&toplevel).await;
        if probed.is_none() {
            // The checkout stopped probing (worktree deleted, repo gone,
            // git wedged): drop the cached resolutions so the next attempt
            // rediscovers from scratch instead of pinning stale state.
            self.toplevel_cache.remove(cwd);
            self.primary_cache.remove(&toplevel);
        }
        probed
    }

    async fn probe_checkout(&mut self, toplevel: &Path) -> Option<SessionGitVitals> {
        self.checkout_probes += 1;
        let status = self
            .git(
                toplevel,
                &[
                    "--no-optional-locks",
                    "status",
                    "--porcelain=v2",
                    "--branch",
                ],
            )
            .await?;
        let facts = parse_status_v2(&status);
        let unpushed = facts.upstream_ab.map(|(ahead, _)| ahead);

        let mut ahead = 0;
        let mut behind = 0;
        let mut primary_ref = String::new();
        let mut merge_parity = String::new();
        let mut primary_unpushed = None;
        if let Some((primary_branch, resolved_ref)) = self.primary_for(toplevel).await {
            primary_ref = resolved_ref;
            match self.ahead_behind(toplevel, &primary_ref).await {
                Some((a, b)) => {
                    ahead = a;
                    behind = b;
                }
                // The cached primary stopped resolving (remote-tracking
                // ref pruned, branch deleted): degrade to 0/0 for this
                // tick — the old chain's per-call `unwrap_or(0)` — and
                // drop the cache entry so the next tick rediscovers.
                None => {
                    self.primary_cache.remove(toplevel);
                }
            }

            merge_parity = if (ahead > 0) != (behind > 0) {
                // Fast-forward in one direction: trivially clean.
                "clean".to_string()
            } else if ahead > 0 && behind > 0 && merge_tree_supported() {
                match facts.head_oid.as_deref() {
                    Some(head_oid) => self
                        .merge_parity(toplevel, &primary_ref, head_oid)
                        .await
                        .unwrap_or_default(),
                    None => String::new(),
                }
            } else {
                String::new()
            };

            if facts.branch != primary_branch {
                // No `rev-parse --verify` gate on `@{upstream}` first: the
                // `rev-list --count` fails to `None` identically when the
                // upstream doesn't resolve, so the verify subprocess added
                // no information.
                let primary_upstream = format!("{primary_branch}@{{upstream}}");
                primary_unpushed = self
                    .git_count(toplevel, &format!("{primary_upstream}..{primary_branch}"))
                    .await;
            }
        }

        Some(SessionGitVitals {
            branch: facts.branch,
            dirty_files: facts.dirty_files,
            ahead,
            behind,
            primary_ref,
            merge_parity,
            unpushed,
            primary_unpushed,
        })
    }

    /// Primary-branch discovery, cached per checkout: origin's default
    /// (`symbolic-ref refs/remotes/origin/HEAD`) when known, else local
    /// main/master. The comparison ref prefers `origin/<primary>` — a
    /// stale local primary would misread fresh worktrees cut from the
    /// remote tip. Returns `(primary branch, comparison ref)`.
    async fn primary_for(&mut self, toplevel: &Path) -> Option<(String, String)> {
        if let Some(cached) = self.primary_cache.get(toplevel) {
            return Some(cached.clone());
        }
        let mut primary_branch = self
            .git(
                toplevel,
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            )
            .await
            .map(|s| s.trim_start_matches("origin/").to_string());
        if primary_branch.is_none() {
            for candidate in ["main", "master"] {
                let refname = format!("refs/heads/{candidate}");
                if self
                    .git(toplevel, &["show-ref", "--verify", "--quiet", &refname])
                    .await
                    .is_some()
                {
                    primary_branch = Some(candidate.to_string());
                    break;
                }
            }
        }
        let primary_branch = primary_branch?;
        let remote_primary = format!("origin/{primary_branch}");
        let primary_ref = if self
            .git(
                toplevel,
                &["rev-parse", "--verify", "--quiet", &remote_primary],
            )
            .await
            .is_some()
        {
            remote_primary
        } else {
            primary_branch.clone()
        };
        if self.primary_cache.len() > PROBER_CACHE_CAP {
            self.primary_cache.clear();
        }
        self.primary_cache.insert(
            toplevel.to_path_buf(),
            (primary_branch.clone(), primary_ref.clone()),
        );
        Some((primary_branch, primary_ref))
    }

    /// Divergence vs the primary in ONE subprocess: symmetric three-dot
    /// `rev-list --left-right --count` prints `<left>\t<right>`, where the
    /// LEFT column counts commits reachable only from `primary_ref`
    /// (= behind) and the RIGHT column commits reachable only from HEAD
    /// (= ahead). Returns `(ahead, behind)`.
    async fn ahead_behind(&self, toplevel: &Path, primary_ref: &str) -> Option<(u32, u32)> {
        let counts = self
            .git(
                toplevel,
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{primary_ref}...HEAD"),
                ],
            )
            .await?;
        let mut cols = counts.split_whitespace();
        let behind = cols.next()?.parse().ok()?;
        let ahead = cols.next()?.parse().ok()?;
        Some((ahead, behind))
    }

    /// Would merging HEAD and the primary conflict? In-memory 3-way merge,
    /// cached by the SHA pair so it only reruns when something moves. The
    /// HEAD side of the key arrives from the status probe (`branch.oid`) —
    /// no extra `rev-parse HEAD`.
    async fn merge_parity(
        &mut self,
        cwd: &Path,
        primary_ref: &str,
        head_oid: &str,
    ) -> Option<String> {
        let primary = self.git(cwd, &["rev-parse", primary_ref]).await?;
        let key = (head_oid.to_string(), primary);
        if let Some(cached) = self.merge_cache.get(&key) {
            return Some(cached.clone());
        }
        // The exit status IS the verdict (0 clean, non-zero conflict), so
        // this reads the raw output: only spawn failure or timeout is a
        // `None` (no parity statement).
        let clean = run_git(
            &self.git_program,
            self.probe_timeout,
            cwd,
            &["merge-tree", "--write-tree", "HEAD", primary_ref],
        )
        .await?
        .status
        .success();
        let state = if clean { "clean" } else { "conflict" }.to_string();
        // The cache only grows while refs churn; entries are tiny and the
        // pair space a session actually visits is small.
        if self.merge_cache.len() > PROBER_CACHE_CAP {
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
                if vitals.config.is_none() {
                    vitals.config = orphan.config;
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

    /// Fold a partial config-facts emission into a session's `config`
    /// section, sticky per field: producers emit what a protocol seam just
    /// taught them (launch snapshot, a model echo, an accepted mode
    /// switch), and a known value is never blanked by a later partial
    /// that omits it. The permission fields travel as one datum — a mode
    /// update carries its display kind and echo provenance with it.
    fn apply_config_facts(&self, session_id: &str, update: SessionConfigVitals) {
        if update == SessionConfigVitals::default() {
            return;
        }
        self.apply(session_id, |vitals| {
            let config = vitals.config.get_or_insert_with(Default::default);
            if update.model.is_some() {
                config.model = update.model;
            }
            if update.effort.is_some() {
                config.effort = update.effort;
            }
            if update.permission_mode.is_some() {
                config.permission_mode = update.permission_mode;
                config.permission_kind = update.permission_kind;
                config.permission_echoed = update.permission_echoed;
            }
        });
    }

    /// The daemon-global autonomy level changed: update every session
    /// whose permission facts are autonomy-backed (native sessions —
    /// external backends carry their own modes). The level is shared
    /// state, so this fold is exact, not a heuristic.
    fn apply_autonomy_change(&self, level: &str) {
        let autonomy_sessions: Vec<String> = {
            let sessions = self.sessions.lock().expect("vitals state lock");
            sessions
                .iter()
                .filter(|(_, vitals)| {
                    vitals.config.as_ref().is_some_and(|config| {
                        config.permission_kind.as_deref()
                            == Some(intendant_core::vitals::PERMISSION_KIND_AUTONOMY)
                    })
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for session_id in autonomy_sessions {
            self.apply(&session_id, |vitals| {
                if let Some(config) = vitals.config.as_mut() {
                    config.permission_mode = Some(level.to_string());
                }
            });
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
    // Floor, never round: 100 must mean the entire sample was served from
    // the cache (read == sample_total exactly). Rounding displayed a
    // 99.5%+ hit as a lying "100%" — a fraction of a percent of fresh
    // input is still fresh input, and the number should say so.
    let hit_pct = ((read * 100) / sample_total).min(100) as u8;
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
                Ok(AppEvent::SessionConfigFacts {
                    session_id: Some(session_id),
                    facts,
                }) => hub.apply_config_facts(&session_id, facts),
                // The autonomy level is daemon-global shared state; the
                // fold updates every autonomy-backed config section so a
                // Control-tab change shows up mid-session.
                Ok(AppEvent::AutonomyChanged { autonomy }) => {
                    hub.apply_autonomy_change(&autonomy);
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

/// Probe `cwd` through the per-tick cache, keyed by checkout TOPLEVEL:
/// sessions sharing a checkout (the common shape once restored sessions
/// register at boot — many idle sessions per project root, or the root
/// and a subdirectory of one checkout) pay for one probe per tick
/// instead of one per session. Git state is a pure function of the
/// checkout within a tick, so the shared result is exact; linked
/// worktrees have distinct toplevels and correctly keep their own
/// probes. Paths outside any checkout cache their miss under the raw
/// path.
async fn probe_cached(
    prober: &mut GitVitalsProber,
    tick_cache: &mut HashMap<PathBuf, Option<SessionGitVitals>>,
    cwd: &Path,
) -> Option<SessionGitVitals> {
    if let Some(cached) = tick_cache.get(cwd) {
        return cached.clone();
    }
    let Some(key) = prober.toplevel_for(cwd).await else {
        tick_cache.insert(cwd.to_path_buf(), None);
        return None;
    };
    if let Some(cached) = tick_cache.get(&key) {
        return cached.clone();
    }
    // Re-resolves the toplevel through the prober's cache (no
    // subprocess) — keeps the drop-caches-on-failure path in one place.
    let probed = prober.probe(cwd).await;
    tick_cache.insert(key, probed.clone());
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

    #[test]
    fn status_v2_parser_maps_branch_facts() {
        // The everyday shape: branch + resolving upstream + entries. Entry
        // lines count one per path — ordinary change, rename (one line in
        // v2, exactly as in v1), unmerged, untracked.
        let facts = parse_status_v2(concat!(
            "# branch.oid 2066c7d7db74e9e097ad8526b47c53a0fa0c39a9\n",
            "# branch.head main\n",
            "# branch.upstream origin/main\n",
            "# branch.ab +3 -1\n",
            "1 .M N... 100644 100644 100644 aaaa bbbb src/lib.rs\n",
            "2 R. N... 100644 100644 100644 cccc cccc R100 new.rs\told.rs\n",
            "u UU N... 100644 100644 100644 100644 dddd eeee ffff conflict.rs\n",
            "? scratch.txt\n",
        ));
        assert_eq!(facts.branch, "main");
        assert_eq!(
            facts.head_oid.as_deref(),
            Some("2066c7d7db74e9e097ad8526b47c53a0fa0c39a9")
        );
        assert_eq!(facts.upstream_ab, Some((3, 1)));
        assert_eq!(facts.dirty_files, 4);

        // Upstream configured but its ref gone: git prints branch.upstream
        // without branch.ab — exactly the old failed `@{upstream}` verify,
        // so there is no unpushed statement.
        let facts = parse_status_v2(
            "# branch.oid 2066c7d7\n# branch.head main\n# branch.upstream origin/main\n",
        );
        assert_eq!(facts.upstream_ab, None);
        assert_eq!(facts.dirty_files, 0);

        // Detached HEAD keeps the old `rev-parse --abbrev-ref` spelling.
        let facts = parse_status_v2("# branch.oid 2066c7d7\n# branch.head (detached)\n");
        assert_eq!(facts.branch, "HEAD");
        assert_eq!(facts.upstream_ab, None);

        // Unborn branch: the old probe's rev-parse failed, so the emitted
        // branch stays empty; there is no HEAD oid to key parity with.
        let facts = parse_status_v2("# branch.oid (initial)\n# branch.head main\n? f.txt\n");
        assert_eq!(facts.branch, "");
        assert_eq!(facts.head_oid, None);
        assert_eq!(facts.dirty_files, 1);
    }

    /// Build a repo with a local bare `origin` whose `main` is pushed and
    /// tracked (upstream configured) — the shape the upstream/primary
    /// probes need, without any network. Returns the working checkout.
    fn repo_with_origin(root: &Path) -> PathBuf {
        let remote = root.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        git_cmd(root, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        git_cmd(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        git_cmd(&work, &["add", "."]);
        git_cmd(&work, &["commit", "-qm", "base"]);
        git_cmd(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git_cmd(&work, &["push", "-q", "-u", "origin", "main"]);
        work
    }

    #[tokio::test]
    async fn probe_reports_upstream_unpushed_and_remote_primary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = repo_with_origin(dir.path());

        // In sync with the upstream: unpushed is a visible zero ("checked
        // and synced") and the primary comparison rides the remote ref.
        let mut prober = GitVitalsProber::default();
        let vitals = prober.probe(&work).await.expect("repo probes");
        assert_eq!(vitals.branch, "main");
        assert_eq!(vitals.primary_ref, "origin/main");
        assert_eq!(vitals.unpushed, Some(0));
        assert_eq!((vitals.ahead, vitals.behind), (0, 0));
        assert_eq!(vitals.merge_parity, "");
        assert_eq!(vitals.primary_unpushed, None, "on the primary itself");

        // A local commit: ahead of the upstream (unpushed) and of the
        // remote primary (ahead) by the same one commit.
        std::fs::write(work.join("b.txt"), "two\n").unwrap();
        git_cmd(&work, &["add", "."]);
        git_cmd(&work, &["commit", "-qm", "local work"]);
        let vitals = prober.probe(&work).await.expect("repo probes");
        assert_eq!(vitals.unpushed, Some(1));
        assert_eq!((vitals.ahead, vitals.behind), (1, 0));
        assert_eq!(vitals.merge_parity, "clean");

        // A branch without an upstream: unpushed hides entirely, while the
        // primary's own unpushed count appears (main is 1 ahead of its
        // upstream and the session is no longer on main).
        git_cmd(&work, &["checkout", "-qb", "feature"]);
        std::fs::write(work.join("c.txt"), "three\n").unwrap();
        git_cmd(&work, &["add", "."]);
        git_cmd(&work, &["commit", "-qm", "feature work"]);
        let vitals = prober.probe(&work).await.expect("repo probes");
        assert_eq!(vitals.branch, "feature");
        assert_eq!(vitals.unpushed, None, "no upstream, nothing to check");
        assert_eq!(vitals.primary_unpushed, Some(1));
        assert_eq!((vitals.ahead, vitals.behind), (2, 0));

        // Detached HEAD keeps the old rev-parse spelling.
        git_cmd(&work, &["checkout", "-q", "--detach"]);
        let vitals = prober.probe(&work).await.expect("repo probes");
        assert_eq!(vitals.branch, "HEAD");
        assert_eq!(vitals.unpushed, None);
    }

    #[tokio::test]
    async fn left_right_divergence_mapping_pinned() {
        // Ahead 2 / behind 3 of the primary — asymmetric on purpose so a
        // swapped left/right mapping in the combined rev-list cannot pass.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git_cmd(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git_cmd(root, &["add", "."]);
        git_cmd(root, &["commit", "-qm", "base"]);
        git_cmd(root, &["checkout", "-qb", "feature"]);
        for i in 0..2 {
            std::fs::write(root.join(format!("f{i}.txt")), "x\n").unwrap();
            git_cmd(root, &["add", "."]);
            git_cmd(root, &["commit", "-qm", "feature work"]);
        }
        git_cmd(root, &["checkout", "-q", "main"]);
        for i in 0..3 {
            std::fs::write(root.join(format!("m{i}.txt")), "x\n").unwrap();
            git_cmd(root, &["add", "."]);
            git_cmd(root, &["commit", "-qm", "main work"]);
        }
        git_cmd(root, &["checkout", "-q", "feature"]);

        let mut prober = GitVitalsProber::default();
        let vitals = prober.probe(root).await.expect("repo probes");
        assert_eq!(vitals.primary_ref, "main");
        assert_eq!(vitals.ahead, 2, "right column = commits only on HEAD");
        assert_eq!(
            vitals.behind, 3,
            "left column = commits only on the primary"
        );
        if merge_tree_supported() {
            assert_eq!(vitals.merge_parity, "clean", "disjoint files merge clean");
        }
    }

    #[tokio::test]
    async fn same_checkout_subdir_targets_share_one_probe_per_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git_cmd(root, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.txt"), "one\n").unwrap();
        git_cmd(root, &["add", "."]);
        git_cmd(root, &["commit", "-qm", "base"]);

        let mut prober = GitVitalsProber::default();
        let mut tick_cache: HashMap<PathBuf, Option<SessionGitVitals>> = HashMap::new();
        let at_root = probe_cached(&mut prober, &mut tick_cache, root).await;
        let at_subdir = probe_cached(&mut prober, &mut tick_cache, &root.join("src")).await;
        assert_eq!(at_root.as_ref().map(|g| g.branch.as_str()), Some("main"));
        assert_eq!(at_root, at_subdir, "one checkout, one shared result");
        assert_eq!(
            prober.checkout_probes, 1,
            "root and subdir share a single probe within the tick"
        );
        assert_eq!(tick_cache.len(), 1, "cached under the shared toplevel key");

        // Next tick: fresh per-tick cache, still one probe per checkout —
        // the path→toplevel resolutions are already cached.
        let mut next_tick: HashMap<PathBuf, Option<SessionGitVitals>> = HashMap::new();
        probe_cached(&mut prober, &mut next_tick, &root.join("src")).await;
        probe_cached(&mut prober, &mut next_tick, root).await;
        assert_eq!(prober.checkout_probes, 2);

        // Paths outside any checkout cache their per-tick miss under the
        // raw path and never reach a checkout probe.
        let outside = tempfile::tempdir().expect("tempdir");
        let mut tick: HashMap<PathBuf, Option<SessionGitVitals>> = HashMap::new();
        assert!(probe_cached(&mut prober, &mut tick, outside.path())
            .await
            .is_none());
        assert!(probe_cached(&mut prober, &mut tick, outside.path())
            .await
            .is_none());
        assert_eq!(prober.checkout_probes, 2);
        assert!(tick.contains_key(outside.path()), "miss cached per tick");
    }

    #[tokio::test]
    async fn primary_cache_invalidation_rediscovers_after_ref_loss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = repo_with_origin(dir.path());
        git_cmd(&work, &["checkout", "-qb", "feature"]);
        std::fs::write(work.join("b.txt"), "two\n").unwrap();
        git_cmd(&work, &["add", "."]);
        git_cmd(&work, &["commit", "-qm", "feature work"]);

        let mut prober = GitVitalsProber::default();
        let vitals = prober.probe(&work).await.expect("repo probes");
        assert_eq!(vitals.primary_ref, "origin/main");
        assert_eq!((vitals.ahead, vitals.behind), (1, 0));
        assert_eq!(prober.primary_cache.len(), 1, "discovery cached");

        // The remote-tracking ref vanishes (remote pruned): the cached
        // primary stops resolving. The failing tick degrades to 0/0 —
        // the old chain's unwrap_or(0) — and invalidates the entry...
        git_cmd(&work, &["update-ref", "-d", "refs/remotes/origin/main"]);
        let vitals = prober.probe(&work).await.expect("repo probes");
        assert_eq!(vitals.primary_ref, "origin/main", "stale name for one tick");
        assert_eq!((vitals.ahead, vitals.behind), (0, 0));
        assert!(
            prober.primary_cache.is_empty(),
            "failed rev-list drops the discovery cache entry"
        );

        // ...so the next tick rediscovers: local main is the primary now.
        let vitals = prober.probe(&work).await.expect("repo probes");
        assert_eq!(vitals.primary_ref, "main");
        assert_eq!((vitals.ahead, vitals.behind), (1, 0));
    }

    /// A git stand-in that hangs far longer than the probe timeout —
    /// proves the anti-wedge guard without a hung real git. Hermetic on
    /// the process ledger too: the unix script `exec`s its sleep so the
    /// pid `kill_on_drop` reaps IS the sleeper (no shell child to
    /// orphan), and the Windows sleeper self-bounds at ~3s.
    fn write_hanging_git_stub(dir: &Path) -> PathBuf {
        #[cfg(unix)]
        {
            let path = dir.join("hung-git.sh");
            std::fs::write(&path, "#!/bin/sh\nexec sleep 30\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
        #[cfg(windows)]
        {
            let path = dir.join("hung-git.bat");
            // `ping -n` is the canonical batch sleep (`timeout` refuses
            // the null stdin the probe runner hands its children); the
            // short count bounds any orphan outliving the killed cmd.exe.
            std::fs::write(&path, "@ping -n 4 127.0.0.1 > nul\r\n").unwrap();
            path
        }
    }

    #[tokio::test]
    async fn hung_git_times_out_and_fails_the_probe_promptly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = write_hanging_git_stub(dir.path());
        let mut prober = GitVitalsProber {
            git_program: stub.into(),
            probe_timeout: std::time::Duration::from_millis(200),
            ..Default::default()
        };
        let start = std::time::Instant::now();
        assert!(
            prober.probe(dir.path()).await.is_none(),
            "hung git must fail the probe, not wedge it"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "the timeout bounds the probe: {:?}",
            start.elapsed()
        );
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
        // 100% is honest here: creation + uncached == 0, so the whole
        // sample really was served from cache.
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

    /// The hit percentage floors — it never rounds a near-miss up to a
    /// dishonest 100 (or any x.5+ up to the next integer). 100 appears
    /// only when creation + uncached == 0, i.e. read == sample_total.
    #[test]
    fn cache_hit_pct_floors_never_rounds() {
        // 999 cached / 1 fresh = 99.9% — rounding displayed 100 (the
        // observed live lie); floor says 99.
        let near_miss =
            cache_vitals_from_usage(&usage_with_sample("anthropic", 999, 0, 1, None), None, 47)
                .expect("sample present");
        assert_eq!(near_miss.hit_pct, Some(99));

        // Same fraction with the fresh tokens split across creation and
        // uncached input — both count against the hit.
        let split =
            cache_vitals_from_usage(&usage_with_sample("anthropic", 1998, 1, 1, None), None, 48)
                .expect("sample present");
        assert_eq!(split.hit_pct, Some(99));

        // Mid-range: 2/3 = 66.7% floors to 66, never rounds to 67.
        let mid = cache_vitals_from_usage(&usage_with_sample("openai", 2, 0, 1, None), None, 49)
            .expect("sample present");
        assert_eq!(mid.hit_pct, Some(66));

        // Exactly everything cached is the one honest 100.
        let full = cache_vitals_from_usage(&usage_with_sample("openai", 7, 0, 0, None), None, 50)
            .expect("sample present");
        assert_eq!(full.hit_pct, Some(100));
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

    /// The config-facts fold is sticky per field: partial emissions from
    /// different protocol seams (launch snapshot, model echo, mode
    /// switch) accumulate instead of blanking each other, the permission
    /// fields travel as one datum, and a mid-session autonomy change
    /// updates exactly the autonomy-backed sections.
    #[tokio::test]
    async fn config_facts_fold_sticky_and_autonomy_change_scoped() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let hub = SessionVitalsHub::new(bus.clone());
        let _listener = spawn_cache_vitals_listener(bus.clone(), hub.clone());

        let deadline = std::time::Duration::from_secs(5);
        async fn wait_config(
            rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
        ) -> (String, SessionConfigVitals) {
            loop {
                if let Ok(AppEvent::SessionVitals { session_id, vitals }) = rx.recv().await {
                    if let Some(config) = vitals.config {
                        return (session_id, config);
                    }
                }
            }
        }

        // Launch snapshot: unconfirmed mode + configured effort.
        bus.send(AppEvent::SessionConfigFacts {
            session_id: Some("cc-1".into()),
            facts: SessionConfigVitals {
                model: Some("claude-fable-5".into()),
                effort: Some("max".into()),
                permission_mode: Some("bypassPermissions".into()),
                permission_kind: Some("bypass".into()),
                permission_echoed: false,
            },
        });
        let (sid, config) = tokio::time::timeout(deadline, wait_config(&mut rx))
            .await
            .expect("launch facts emit");
        assert_eq!(sid, "cc-1");
        assert_eq!(config.model.as_deref(), Some("claude-fable-5"));
        assert!(!config.permission_echoed);

        // Init mode echo: partial — model/effort must survive the fold.
        bus.send(AppEvent::SessionConfigFacts {
            session_id: Some("cc-1".into()),
            facts: SessionConfigVitals {
                permission_mode: Some("bypassPermissions".into()),
                permission_kind: Some("bypass".into()),
                permission_echoed: true,
                ..Default::default()
            },
        });
        let (_, config) = tokio::time::timeout(deadline, wait_config(&mut rx))
            .await
            .expect("echo upgrade emits");
        assert!(config.permission_echoed, "echo provenance upgraded");
        assert_eq!(
            config.model.as_deref(),
            Some("claude-fable-5"),
            "partial mode echo must not blank the model"
        );
        assert_eq!(config.effort.as_deref(), Some("max"));

        // A native session with autonomy-backed permissions, plus the CC
        // session above: an autonomy change updates only the former.
        bus.send(AppEvent::SessionConfigFacts {
            session_id: Some("native-loop".into()),
            facts: SessionConfigVitals {
                model: Some("gpt-5.5".into()),
                permission_mode: Some("Medium".into()),
                permission_kind: Some(intendant_core::vitals::PERMISSION_KIND_AUTONOMY.to_string()),
                permission_echoed: true,
                ..Default::default()
            },
        });
        let (sid, _) = tokio::time::timeout(deadline, wait_config(&mut rx))
            .await
            .expect("native facts emit");
        assert_eq!(sid, "native-loop");

        bus.send(AppEvent::AutonomyChanged {
            autonomy: "Full".into(),
        });
        let (sid, config) = tokio::time::timeout(deadline, wait_config(&mut rx))
            .await
            .expect("autonomy fold emits");
        assert_eq!(
            sid, "native-loop",
            "only the autonomy-backed session updates"
        );
        assert_eq!(config.permission_mode.as_deref(), Some("Full"));
        assert_eq!(
            config.permission_kind.as_deref(),
            Some(intendant_core::vitals::PERMISSION_KIND_AUTONOMY)
        );

        // The CC session's mode is untouched (no further cc-1 emission —
        // the autonomy fold matched nothing there, so nothing changed).
        let cc = hub
            .sessions
            .lock()
            .expect("vitals state lock")
            .get("cc-1")
            .cloned()
            .expect("cc entry");
        assert_eq!(
            cc.config
                .as_ref()
                .and_then(|c| c.permission_mode.as_deref()),
            Some("bypassPermissions")
        );

        // An all-empty facts emission is a no-op, never an empty section.
        hub.apply_config_facts("fresh", SessionConfigVitals::default());
        assert!(
            !hub.sessions
                .lock()
                .expect("vitals state lock")
                .contains_key("fresh"),
            "empty facts must not materialize an entry"
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
            ..Default::default()
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
            "model:",
            "permissions:",
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
        // SessionCacheVitals/SessionLimitWindow/SessionActivityVitals/
        // SessionConfigVitals the catalog must consume.
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
            "backgroundTasks",
            "model",
            "permissionMode",
            "permissionKind",
            "permissionEchoed",
        ] {
            assert!(
                catalog.contains(field),
                "catalog stopped consuming wire field {field} — SessionVitals and VITALS_SYMBOLS drifted"
            );
        }
        // The permission display kinds (the daemon-side catalog in
        // intendant-core vitals.rs) must each have plain-language copy in
        // the frontend catalog — one vocabulary, two responsibilities.
        for kind in intendant_core::vitals::PERMISSION_DISPLAY_KINDS {
            assert!(
                catalog.contains(kind),
                "catalog lost permission display kind {kind} — extend PERMISSION_KIND_COPY alongside PERMISSION_DISPLAY_KINDS"
            );
        }
        // The wire spellings of the activity states the catalog renders —
        // and the derived-only `stalled` — must all be explained.
        for state in [
            "reasoning",
            "responding",
            "tool-running",
            "awaiting-api",
            "parked-on-tasks",
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

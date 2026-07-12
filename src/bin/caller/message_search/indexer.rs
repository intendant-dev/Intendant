//! The wiring edge of the message-search program (plan §5–6): a
//! background sweep that enumerates every message source on this box —
//! intendant session logs, Codex rollouts (user home, leased-active
//! homes, staged lease remnants), Claude Code transcripts (same three
//! places) — runs the per-source extractors, and publishes the resulting
//! shards to the store. Everything below [`spawn_indexer`] takes its
//! roots as parameters; only that production edge resolves the real
//! environment.
//!
//! Freshness model: a 30-second cursor-driven sweep. Stored
//! [`SourceCursor`]s make the steady state cheap (metadata + a 4 KiB
//! prefix hash per known file; unchanged sources are skipped without
//! reading), so no event plumbing is needed for correctness — the C1
//! query side adds on-demand refresh of the queried session where
//! sub-sweep freshness matters.
//!
//! Sources that publish nothing — wrapper session logs (canonical in the
//! external backend's own file), rollouts with no message content yet —
//! are remembered in an in-process cache rather than the store: an empty
//! shard's `newest_ts_ms` of 0 would make retention GC evict and the next
//! sweep re-parse them forever.

use super::cursor::{CursorCheck, SourceCursor};
use super::extract_claude::extract_claude_session;
use super::extract_codex::extract_codex_session;
use super::extract_intendant::extract_intendant_session;
use super::record::{Generation, Source};
use super::store::{PublishOutcome, SessionShard, Snapshot, Store, RETENTION_MS};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Sweep cadence; also the boot delay, so one-shot CLI runs exit before
/// ever paying for a sweep while a daemon converges within one interval.
pub(crate) const SWEEP_INTERVAL_SECS: u64 = 30;
/// Retention GC rides every Nth sweep (hourly at the 30s cadence) plus
/// the first, replacing nothing — `startup_gc` still runs at boot.
const GC_EVERY_SWEEPS: u64 = 120;

/// Everything one sweep looks at. Each root is optional on disk — absent
/// directories are silently empty.
#[derive(Debug, Default, Clone)]
pub(crate) struct SweepRoots {
    pub store_root: PathBuf,
    /// `~/.intendant/logs` — one subdirectory per session.
    pub intendant_logs: PathBuf,
    /// Codex roots: each directly contains `sessions/` and/or
    /// `archived_sessions/` (the user's `codex_dir`, leased-active homes,
    /// staged lease entries).
    pub codex_roots: Vec<PathBuf>,
    /// Claude project roots: each directly contains per-project dirs of
    /// `<uuid>.jsonl` mains and `<uuid>/subagents/agent-*.jsonl` files.
    pub claude_project_roots: Vec<PathBuf>,
    /// Staged lease entries (whole entry dirs): deleted once every
    /// transcript file inside was published — the drain half of the
    /// custody design (deletion never waited on us; indexing consumes the
    /// remnant at leisure).
    pub staged_entries: Vec<PathBuf>,
}

/// What a sweep did — the tests' observability and the log line's body.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SweepStats {
    pub parsed: usize,
    pub published: usize,
    pub skipped_unchanged: usize,
    pub source_gone: usize,
    pub drained_entries: usize,
    pub failures: usize,
}

/// Cross-sweep in-process memory (see the module doc): sources that
/// produced nothing publishable, keyed by path with the cursor that
/// proved it, so unchanged ones are skipped without re-parsing.
#[derive(Default)]
pub(crate) struct Indexer {
    unpublishable: HashMap<PathBuf, SourceCursor>,
    sweeps: u64,
}

impl Indexer {
    pub(crate) fn sweep(&mut self, roots: &SweepRoots) -> SweepStats {
        let mut stats = SweepStats::default();
        let store = match Store::open(&roots.store_root) {
            Ok(store) => store,
            Err(err) => {
                eprintln!("[message-search] store open failed: {err}");
                stats.failures += 1;
                return stats;
            }
        };
        if self.sweeps.is_multiple_of(GC_EVERY_SWEEPS) {
            if let Err(err) = store.gc(now_ms()) {
                eprintln!("[message-search] retention gc failed: {err}");
            }
        }
        self.sweeps += 1;

        let snapshot = store.snapshot();
        // Path → (session key, cursor) over every published source file.
        let mut cursor_by_path: HashMap<PathBuf, (String, SourceCursor)> = HashMap::new();
        for (key, entry) in &snapshot.manifest.sessions {
            for cursor in &entry.cursors {
                cursor_by_path.insert(cursor.path.clone(), (key.clone(), cursor.clone()));
            }
        }
        let mut seen_paths: HashSet<PathBuf> = HashSet::new();
        let mut failed_paths: Vec<PathBuf> = Vec::new();
        let horizon = now_ms().saturating_sub(RETENTION_MS);

        self.sweep_intendant(
            roots,
            &store,
            &snapshot,
            &cursor_by_path,
            &mut seen_paths,
            &mut failed_paths,
            horizon,
            &mut stats,
        );
        self.sweep_codex(
            roots,
            &store,
            &snapshot,
            &cursor_by_path,
            &mut seen_paths,
            &mut failed_paths,
            horizon,
            &mut stats,
        );
        self.sweep_claude(
            roots,
            &store,
            &snapshot,
            &cursor_by_path,
            &mut seen_paths,
            &mut failed_paths,
            horizon,
            &mut stats,
        );

        // Coverage: a published session whose every source file vanished
        // (lease cleanup, manual deletion) keeps serving from the shard
        // with the `source_gone` badge until retention expires it.
        for (key, entry) in &snapshot.manifest.sessions {
            if entry.source_gone || entry.cursors.is_empty() {
                continue;
            }
            let all_gone = entry
                .cursors
                .iter()
                .all(|cursor| !seen_paths.contains(&cursor.path) && !cursor.path.exists());
            if all_gone {
                match store.mark_source_gone(key) {
                    Ok(()) => stats.source_gone += 1,
                    Err(err) => {
                        eprintln!("[message-search] mark_source_gone {key} failed: {err}");
                        stats.failures += 1;
                    }
                }
            }
        }

        // Drain staged lease entries whose every transcript file is now
        // in the store (published earlier or this sweep, and none failed).
        for entry in &roots.staged_entries {
            let mut jsonl = Vec::new();
            collect_suffix_files(entry, ".jsonl", 8, &mut jsonl);
            let fully_covered = jsonl
                .iter()
                .all(|path| seen_paths.contains(path) && !failed_paths.contains(path));
            if fully_covered {
                match std::fs::remove_dir_all(entry) {
                    Ok(()) => stats.drained_entries += 1,
                    Err(err) => {
                        eprintln!("[message-search] drain {} failed: {err}", entry.display());
                        stats.failures += 1;
                    }
                }
            }
        }

        stats
    }

    #[allow(clippy::too_many_arguments)] // one sweep's shared frame, threaded to each lane
    fn sweep_intendant(
        &mut self,
        roots: &SweepRoots,
        store: &Store,
        snapshot: &Snapshot,
        cursor_by_path: &HashMap<PathBuf, (String, SourceCursor)>,
        seen_paths: &mut HashSet<PathBuf>,
        failed_paths: &mut Vec<PathBuf>,
        horizon_ms: i64,
        stats: &mut SweepStats,
    ) {
        let Ok(entries) = std::fs::read_dir(&roots.intendant_logs) else {
            return;
        };
        for dir in entries.flatten() {
            let dir_path = dir.path();
            if !dir_path.is_dir() {
                continue;
            }
            let log_path = dir_path.join("session.jsonl");
            if !log_path.is_file() || file_mtime_ms(&log_path) < horizon_ms {
                continue;
            }
            seen_paths.insert(log_path.clone());
            if self.skip_unchanged(&log_path, cursor_by_path, stats) {
                continue;
            }
            let session_key = format!(
                "{}:{}",
                Source::Intendant.as_str(),
                dir_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default()
            );
            if snapshot.manifest.tombstones.contains_key(&session_key) {
                continue;
            }
            stats.parsed += 1;
            match extract_intendant_session(&dir_path) {
                Ok(extraction) => {
                    if extraction.wrapper || extraction.shard.records.is_empty() {
                        // Wrapper session (canonical in the external
                        // backend's own log) or no message content:
                        // remember in-process, never in the store
                        // (module doc).
                        self.remember_unpublishable(&log_path, extraction.cursors);
                        continue;
                    }
                    self.publish(
                        store,
                        &session_key,
                        extraction.shard,
                        extraction.cursors,
                        &log_path,
                        failed_paths,
                        stats,
                    );
                }
                Err(err) => {
                    eprintln!(
                        "[message-search] intendant extract {} failed: {err}",
                        dir_path.display()
                    );
                    failed_paths.push(log_path);
                    stats.failures += 1;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // one sweep's shared frame, threaded to each lane
    fn sweep_codex(
        &mut self,
        roots: &SweepRoots,
        store: &Store,
        snapshot: &Snapshot,
        cursor_by_path: &HashMap<PathBuf, (String, SourceCursor)>,
        seen_paths: &mut HashSet<PathBuf>,
        failed_paths: &mut Vec<PathBuf>,
        horizon_ms: i64,
        stats: &mut SweepStats,
    ) {
        for root in &roots.codex_roots {
            let mut files = Vec::new();
            collect_suffix_files(&root.join("sessions"), ".jsonl", 6, &mut files);
            collect_suffix_files(&root.join("archived_sessions"), ".jsonl", 6, &mut files);
            for path in files {
                if file_mtime_ms(&path) < horizon_ms {
                    continue;
                }
                seen_paths.insert(path.clone());
                let check = cursor_by_path.get(&path).map(|(_, cursor)| cursor.check());
                if matches!(check, Some(CursorCheck::Unchanged)) {
                    stats.skipped_unchanged += 1;
                    continue;
                }
                if check.is_none() && self.skip_known_unpublishable(&path, stats) {
                    continue;
                }
                // Key-first: the id comes from a bounded streaming read,
                // so prior generations survive even when the same session
                // arrives from a NEW path (a staged copy of a leased
                // home's rollout).
                let Some(session_id) =
                    crate::external_agent::codex::rollout::codex_session_file_id(&path)
                else {
                    // No identity: nothing to key a shard on.
                    let cursor = SourceCursor::capture(&path, 0);
                    self.remember_unpublishable(&path, cursor.into_iter().collect());
                    continue;
                };
                let session_key = format!("{}:{}", Source::Codex.as_str(), session_id);
                if snapshot.manifest.tombstones.contains_key(&session_key) {
                    continue;
                }
                let prior = snapshot.read_shard(&session_key);
                let current_generation: Generation = prior
                    .as_ref()
                    .map(|shard| {
                        shard
                            .records
                            .iter()
                            .map(|record| record.generation)
                            .max()
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);
                // A rewritten source is a same-thread restore: the fresh
                // parse is a NEW branch; everything published so far is
                // retained under its old generation numbers. On any other
                // pass the fresh parse REPLACES the current generation,
                // and only strictly older generations ride along.
                let (generation, retained): (Generation, Option<SessionShard>) =
                    if matches!(check, Some(CursorCheck::Rewritten)) {
                        (
                            current_generation + 1,
                            prior.filter(|shard| !shard.records.is_empty()),
                        )
                    } else {
                        let retained = prior.and_then(|shard| {
                            let records: Vec<_> = shard
                                .records
                                .iter()
                                .filter(|record| record.generation < current_generation)
                                .cloned()
                                .collect();
                            if records.is_empty() {
                                return None;
                            }
                            // The extractor re-adds the active-generation
                            // restore mark; dropping our copy keeps the
                            // mark list from growing by one per sweep.
                            let marks = shard
                                .marks
                                .iter()
                                .filter(|mark| {
                                    !matches!(
                                        mark,
                                        super::record::SupersessionMark::GenerationRestore {
                                            active_generation,
                                            ..
                                        } if *active_generation == current_generation
                                    )
                                })
                                .cloned()
                                .collect();
                            Some(SessionShard { records, marks })
                        });
                        (current_generation, retained)
                    };
                stats.parsed += 1;
                match extract_codex_session(&path, retained.as_ref(), generation) {
                    Ok((shard, cursors)) => {
                        if shard.records.is_empty() {
                            self.remember_unpublishable(&path, cursors);
                            continue;
                        }
                        self.publish(
                            store,
                            &session_key,
                            shard,
                            cursors,
                            &path,
                            failed_paths,
                            stats,
                        );
                    }
                    Err(err) => {
                        eprintln!(
                            "[message-search] codex extract {} failed: {err}",
                            path.display()
                        );
                        failed_paths.push(path);
                        stats.failures += 1;
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // one sweep's shared frame, threaded to each lane
    fn sweep_claude(
        &mut self,
        roots: &SweepRoots,
        store: &Store,
        snapshot: &Snapshot,
        cursor_by_path: &HashMap<PathBuf, (String, SourceCursor)>,
        seen_paths: &mut HashSet<PathBuf>,
        failed_paths: &mut Vec<PathBuf>,
        horizon_ms: i64,
        stats: &mut SweepStats,
    ) {
        for root in &roots.claude_project_roots {
            // One pass builds both sides of a session: `<uuid>.jsonl`
            // mains per project dir, and `<uuid>/subagents/agent-*.jsonl`
            // — the subagent dir can live under a DIFFERENT project dir
            // than its main after a worktree relocation, so both maps
            // span the whole root.
            let mut mains: HashMap<String, PathBuf> = HashMap::new();
            let mut subagents: HashMap<String, Vec<PathBuf>> = HashMap::new();
            let Ok(projects) = std::fs::read_dir(root) else {
                continue;
            };
            for project in projects.flatten() {
                let project_path = project.path();
                if !project_path.is_dir() {
                    continue;
                }
                let Ok(children) = std::fs::read_dir(&project_path) else {
                    continue;
                };
                for child in children.flatten() {
                    let child_path = child.path();
                    let name = child.file_name().to_string_lossy().to_string();
                    if child_path.is_file() {
                        if let Some(stem) = name.strip_suffix(".jsonl") {
                            mains.insert(stem.to_string(), child_path);
                        }
                    } else if child_path.is_dir() {
                        let mut agent_files = Vec::new();
                        collect_suffix_files(
                            &child_path.join("subagents"),
                            ".jsonl",
                            2,
                            &mut agent_files,
                        );
                        if !agent_files.is_empty() {
                            agent_files.sort();
                            subagents.entry(name).or_default().extend(agent_files);
                        }
                    }
                }
            }

            for (session_id, main_path) in mains {
                let agent_paths = subagents.remove(&session_id).unwrap_or_default();
                let newest_mtime = std::iter::once(&main_path)
                    .chain(agent_paths.iter())
                    .map(|path| file_mtime_ms(path))
                    .max()
                    .unwrap_or(0);
                if newest_mtime < horizon_ms {
                    continue;
                }
                seen_paths.insert(main_path.clone());
                for path in &agent_paths {
                    seen_paths.insert(path.clone());
                }
                let all_unchanged =
                    std::iter::once(&main_path)
                        .chain(agent_paths.iter())
                        .all(|path| {
                            cursor_by_path
                                .get(path)
                                .is_some_and(|(_, cursor)| cursor.check() == CursorCheck::Unchanged)
                        });
                if all_unchanged {
                    stats.skipped_unchanged += 1;
                    continue;
                }
                if !cursor_by_path.contains_key(&main_path)
                    && self.skip_known_unpublishable(&main_path, stats)
                {
                    continue;
                }
                let session_key = format!("{}:{}", Source::ClaudeCode.as_str(), session_id);
                if snapshot.manifest.tombstones.contains_key(&session_key) {
                    continue;
                }
                stats.parsed += 1;
                match extract_claude_session(&session_id, &main_path, &agent_paths) {
                    Ok((shard, cursors)) => {
                        if shard.records.is_empty() {
                            self.remember_unpublishable(&main_path, cursors);
                            continue;
                        }
                        self.publish(
                            store,
                            &session_key,
                            shard,
                            cursors,
                            &main_path,
                            failed_paths,
                            stats,
                        );
                    }
                    Err(err) => {
                        eprintln!(
                            "[message-search] claude extract {} failed: {err}",
                            main_path.display()
                        );
                        failed_paths.push(main_path);
                        stats.failures += 1;
                    }
                }
            }
        }
    }

    fn skip_unchanged(
        &mut self,
        path: &Path,
        cursor_by_path: &HashMap<PathBuf, (String, SourceCursor)>,
        stats: &mut SweepStats,
    ) -> bool {
        if let Some((_, cursor)) = cursor_by_path.get(path) {
            if cursor.check() == CursorCheck::Unchanged {
                stats.skipped_unchanged += 1;
                return true;
            }
            return false;
        }
        self.skip_known_unpublishable(path, stats)
    }

    fn skip_known_unpublishable(&mut self, path: &Path, stats: &mut SweepStats) -> bool {
        if let Some(cursor) = self.unpublishable.get(path) {
            if cursor.check() == CursorCheck::Unchanged {
                stats.skipped_unchanged += 1;
                return true;
            }
            self.unpublishable.remove(path);
        }
        false
    }

    fn remember_unpublishable(&mut self, path: &Path, cursors: Vec<SourceCursor>) {
        if let Some(cursor) = cursors
            .into_iter()
            .find(|cursor| cursor.path == path)
            .or_else(|| SourceCursor::capture(path, 0))
        {
            self.unpublishable.insert(path.to_path_buf(), cursor);
        }
    }

    #[allow(clippy::too_many_arguments)] // one sweep's shared frame, threaded to each lane
    fn publish(
        &mut self,
        store: &Store,
        session_key: &str,
        shard: SessionShard,
        cursors: Vec<SourceCursor>,
        source_path: &Path,
        failed_paths: &mut Vec<PathBuf>,
        stats: &mut SweepStats,
    ) {
        match store.publish_session(session_key, &shard, cursors, false) {
            Ok(PublishOutcome::Published) => stats.published += 1,
            // A concurrent daemon got there first with fresher progress;
            // ours was derived from the same sources — nothing lost.
            Ok(PublishOutcome::RejectedStale) => {}
            Ok(PublishOutcome::RejectedTombstoned) => {}
            Err(err) => {
                eprintln!("[message-search] publish {session_key} failed: {err}");
                failed_paths.push(source_path.to_path_buf());
                stats.failures += 1;
            }
        }
    }
}

/// The production sweep state: one shared [`Indexer`] serves the 30s loop
/// AND the query edge's freshness refresh, so both share the
/// unpublishable-source cache and never sweep concurrently.
fn shared_indexer() -> &'static std::sync::Mutex<Indexer> {
    static SHARED: std::sync::OnceLock<std::sync::Mutex<Indexer>> = std::sync::OnceLock::new();
    SHARED.get_or_init(|| std::sync::Mutex::new(Indexer::default()))
}

/// Epoch ms of the last COMPLETED production sweep; 0 = never (the boot
/// backfill hasn't finished).
static LAST_SWEEP_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// One production sweep over freshly resolved roots (blocking; callers
/// use `spawn_blocking`).
pub(crate) fn sweep_shared_production() -> SweepStats {
    let roots = resolve_production_roots();
    let mut indexer = shared_indexer()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let stats = indexer.sweep(&roots);
    LAST_SWEEP_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    stats
}

/// Query-edge freshness (plan §7 acceptance: native ~1 s): sweep now if
/// the last completed sweep is older than `max_age_ms`. Deliberately a
/// no-op before the FIRST sweep completes — the boot backfill can take
/// seconds and must never run inline with a query; coverage reports
/// `building` until it lands.
pub(crate) fn refresh_if_stale(max_age_ms: i64) {
    let last = LAST_SWEEP_MS.load(std::sync::atomic::Ordering::Relaxed);
    if last == 0 || now_ms().saturating_sub(last) <= max_age_ms {
        return;
    }
    let _ = sweep_shared_production();
}

/// Resolve the box's real message sources: the user's default homes, the
/// lease-active registry (live materialized homes, indexed DURING the
/// lease), and staged lease remnants awaiting drain. The one
/// production-edge function here — everything else takes roots.
pub(crate) fn resolve_production_roots() -> SweepRoots {
    let staging = crate::lease_transcript_staging::default_paths();
    let user_home = crate::platform::home_dir();
    let mut roots = SweepRoots {
        store_root: Store::default_root(),
        intendant_logs: crate::platform::intendant_home().join("logs"),
        codex_roots: vec![
            crate::web_gateway::session_catalog::backend_lists::codex_dir(&user_home),
        ],
        claude_project_roots: vec![user_home.join(".claude").join("projects")],
        staged_entries: Vec::new(),
    };
    add_registry_and_staged_roots(&staging.active, &staging.staging, &mut roots);
    roots
}

/// The active-registry + staging halves of root resolution, parameterized
/// for tests (`resolve_production_roots` is the only environment reader).
pub(crate) fn add_registry_and_staged_roots(
    active_root: &Path,
    staging_root: &Path,
    roots: &mut SweepRoots,
) {
    if let Ok(entries) = std::fs::read_dir(active_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let Some(home) = value.get("home").and_then(|v| v.as_str()) else {
                continue;
            };
            let home = PathBuf::from(home);
            match value.get("source").and_then(|v| v.as_str()) {
                // Leased homes contain their transcript dirs directly
                // (`sessions/` / `projects/`) — see the staging module.
                Some("codex") => roots.codex_roots.push(home),
                Some("claude-code") => roots.claude_project_roots.push(home.join("projects")),
                _ => {}
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(staging_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Entry layout mirrors the home it came from; a manifest
            // names the source, but the dirs speak for themselves and a
            // manifest-less entry (write failure) still drains.
            if path.join("sessions").is_dir() || path.join("archived_sessions").is_dir() {
                roots.codex_roots.push(path.clone());
            }
            if path.join("projects").is_dir() {
                roots.claude_project_roots.push(path.join("projects"));
            }
            roots.staged_entries.push(path);
        }
    }
}

/// Boot wiring: the periodic sweep task. The first sweep runs one full
/// interval after boot (one-shot CLI runs exit before paying for it).
pub(crate) fn spawn_indexer() {
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the immediate first tick — skip it
        loop {
            ticker.tick().await;
            // Sweeps read and hash real files: keep them off the async
            // executor's worker threads. The shared instance also serves
            // the query edge's refresh_if_stale.
            let stats = match tokio::task::spawn_blocking(sweep_shared_production).await {
                Ok(stats) => stats,
                Err(err) => {
                    eprintln!("[message-search] sweep task failed: {err}");
                    continue;
                }
            };
            if stats.published > 0
                || stats.failures > 0
                || stats.source_gone > 0
                || stats.drained_entries > 0
            {
                eprintln!(
                    "[message-search] sweep: {} published, {} parsed, {} unchanged, {} gone, {} drained, {} failures",
                    stats.published,
                    stats.parsed,
                    stats.skipped_unchanged,
                    stats.source_gone,
                    stats.drained_entries,
                    stats.failures
                );
            }
        }
    });
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn file_mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Depth-bounded recursive collection of files with `suffix` (strict
/// suffix match — `.jsonl.backup` siblings never qualify).
fn collect_suffix_files(root: &Path, suffix: &str, max_depth: usize, out: &mut Vec<PathBuf>) {
    if max_depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_suffix_files(&path, suffix, max_depth - 1, out);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(suffix))
        {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::record::SupersessionMark;
    use super::*;
    use std::io::Write;

    fn now_iso(offset_secs: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::seconds(offset_secs)).to_rfc3339()
    }

    fn rig(tmp: &Path) -> SweepRoots {
        SweepRoots {
            store_root: tmp.join("store"),
            intendant_logs: tmp.join("logs"),
            codex_roots: vec![tmp.join("codex-home")],
            claude_project_roots: vec![tmp.join("claude-projects")],
            staged_entries: Vec::new(),
        }
    }

    fn write_intendant_session(logs_root: &Path, session_id: &str, texts: &[&str]) -> PathBuf {
        let dir = logs_root.join(session_id);
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        for (index, text) in texts.iter().enumerate() {
            body.push_str(
                &serde_json::json!({
                    "ts": "10:00:00.000",
                    "ts_ms": now_ms() - 1_000 + index as i64,
                    "event": "conversation_message",
                    "data": {"message_id": format!("mid-{index}"),
                             "message_seq": index as u64 + 1,
                             "role": "user", "provenance": "task", "text": text},
                })
                .to_string(),
            );
            body.push('\n');
        }
        std::fs::write(dir.join("session.jsonl"), body).unwrap();
        dir
    }

    fn codex_meta(id: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": now_iso(-60),
            "type": "session_meta",
            "payload": { "id": id, "session_id": "parent-thread-decoy" }
        })
    }

    fn codex_user(text: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": now_iso(-30),
            "type": "event_msg",
            "payload": { "type": "user_message", "message": text }
        })
    }

    fn write_lines(path: &Path, lines: &[serde_json::Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn append_line(path: &Path, line: &serde_json::Value) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{line}").unwrap();
    }

    fn claude_main_line(uuid_val: &str, session: &str, text: &str) -> String {
        serde_json::json!({
            "parentUuid": null, "isSidechain": false, "userType": "external",
            "type": "user",
            "message": { "role": "user", "content": text },
            "uuid": uuid_val, "timestamp": now_iso(-20), "sessionId": session,
            "version": "2.1.207",
        })
        .to_string()
    }

    fn claude_sidechain_line(uuid_val: &str, session: &str, text: &str) -> String {
        serde_json::json!({
            "parentUuid": null, "isSidechain": true, "agentId": "agent-1",
            "type": "user",
            "message": { "role": "user", "content": text },
            "uuid": uuid_val, "timestamp": now_iso(-10), "sessionId": session,
            "version": "2.1.207",
        })
        .to_string()
    }

    const CLAUDE_SESSION: &str = "128ce827-c24f-42b8-8111-bec913f4098f";

    fn build_all_sources(tmp: &Path) -> SweepRoots {
        let roots = rig(tmp);
        write_intendant_session(&roots.intendant_logs, "native-1", &["find the widget"]);
        write_lines(
            &roots.codex_roots[0]
                .join("sessions")
                .join("2026")
                .join("07")
                .join("rollout-1.jsonl"),
            &[codex_meta("codex-1"), codex_user("codex task text")],
        );
        // Main under one project dir, subagents under ANOTHER (worktree
        // relocation shape).
        let projects = &roots.claude_project_roots[0];
        std::fs::create_dir_all(projects.join("proj-a")).unwrap();
        std::fs::write(
            projects
                .join("proj-a")
                .join(format!("{CLAUDE_SESSION}.jsonl")),
            claude_main_line("cm-1", CLAUDE_SESSION, "claude main text") + "\n",
        )
        .unwrap();
        let agent_dir = projects
            .join("proj-b")
            .join(CLAUDE_SESSION)
            .join("subagents");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("agent-1.jsonl"),
            claude_sidechain_line("cs-1", CLAUDE_SESSION, "subagent text") + "\n",
        )
        .unwrap();
        roots
    }

    #[test]
    fn sweep_indexes_all_three_sources_then_skips_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = build_all_sources(tmp.path());
        let mut indexer = Indexer::default();

        let stats = indexer.sweep(&roots);
        assert_eq!(stats.parsed, 3, "one parse per source session");
        assert_eq!(stats.published, 3);
        assert_eq!(stats.failures, 0);

        let store = Store::open(&roots.store_root).unwrap();
        let snapshot = store.snapshot();
        let native = snapshot.read_shard("intendant:native-1").unwrap();
        assert_eq!(native.records[0].text, "find the widget");
        let codex = snapshot.read_shard("codex:codex-1").unwrap();
        assert_eq!(codex.records[0].text, "codex task text");
        let claude = snapshot
            .read_shard(&format!("claude-code:{CLAUDE_SESSION}"))
            .unwrap();
        let texts: Vec<(&str, bool)> = claude
            .records
            .iter()
            .map(|record| (record.text.as_str(), record.subagent))
            .collect();
        assert!(texts.contains(&("claude main text", false)));
        assert!(
            texts.contains(&("subagent text", true)),
            "relocated subagent dir joined its main: {texts:?}"
        );

        let stats = indexer.sweep(&roots);
        assert_eq!(stats.parsed, 0, "unchanged sources are not re-read");
        assert_eq!(stats.published, 0);
        assert_eq!(stats.skipped_unchanged, 3);
    }

    #[test]
    fn codex_rewrite_retains_generations_and_appends_stay_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = rig(tmp.path());
        let rollout = roots.codex_roots[0].join("sessions").join("r.jsonl");
        write_lines(
            &rollout,
            &[codex_meta("codex-2"), codex_user("first branch")],
        );
        let mut indexer = Indexer::default();
        assert_eq!(indexer.sweep(&roots).published, 1);

        // Append: same generation, more content.
        append_line(&rollout, &codex_user("more of the first branch"));
        assert_eq!(indexer.sweep(&roots).published, 1);

        // Same-thread restore: the file is rewritten shorter with new
        // content — the old branch's records must survive as an older
        // generation.
        write_lines(
            &rollout,
            &[codex_meta("codex-2"), codex_user("second branch")],
        );
        assert_eq!(indexer.sweep(&roots).published, 1);
        let store = Store::open(&roots.store_root).unwrap();
        let shard = store.snapshot().read_shard("codex:codex-2").unwrap();
        let by_generation: Vec<(u32, &str)> = shard
            .records
            .iter()
            .map(|record| (record.generation, record.text.as_str()))
            .collect();
        assert!(by_generation.contains(&(0, "first branch")));
        assert!(by_generation.contains(&(0, "more of the first branch")));
        assert!(by_generation.contains(&(1, "second branch")));

        // Two more appended sweeps: the retained generation rides along
        // and the restore-mark list does not grow per sweep.
        append_line(&rollout, &codex_user("second branch grows"));
        assert_eq!(indexer.sweep(&roots).published, 1);
        append_line(&rollout, &codex_user("and grows again"));
        assert_eq!(indexer.sweep(&roots).published, 1);
        let shard = store.snapshot().read_shard("codex:codex-2").unwrap();
        assert!(shard
            .records
            .iter()
            .any(|record| record.generation == 0 && record.text == "first branch"));
        let restore_marks = shard
            .marks
            .iter()
            .filter(|mark| matches!(mark, SupersessionMark::GenerationRestore { .. }))
            .count();
        assert_eq!(restore_marks, 1, "restore marks must not grow per sweep");
    }

    #[test]
    fn wrapper_and_empty_sources_stay_out_of_the_store_without_reparse() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = rig(tmp.path());
        // A wrapper session log: session_identity + no canonical rows.
        let dir = roots.intendant_logs.join("wrapped-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session.jsonl"),
            serde_json::json!({
                "ts": "09:00:00.000", "ts_ms": now_ms(), "event": "session_identity",
                "data": {"session_id": "wrapped-1", "source": "codex",
                          "backend_session_id": "abc"},
            })
            .to_string()
                + "\n",
        )
        .unwrap();
        // A rollout with an identity but no message content yet.
        write_lines(
            &roots.codex_roots[0].join("sessions").join("empty.jsonl"),
            &[codex_meta("codex-empty")],
        );

        let mut indexer = Indexer::default();
        let stats = indexer.sweep(&roots);
        assert_eq!(stats.parsed, 2);
        assert_eq!(stats.published, 0);
        assert!(Store::open(&roots.store_root)
            .unwrap()
            .snapshot()
            .manifest
            .sessions
            .is_empty());

        let stats = indexer.sweep(&roots);
        assert_eq!(stats.parsed, 0, "unpublishable sources are remembered");
        assert_eq!(stats.skipped_unchanged, 2);
    }

    #[test]
    fn staged_entries_drain_after_publish_and_gone_sources_flip_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        let mut roots = rig(tmp.path());
        // A staged lease remnant + an active-registry home, resolved the
        // way production does.
        let staging_root = tmp.path().join("staging");
        let active_root = tmp.path().join("leased-active");
        let entry = staging_root.join("codex-123-1");
        write_lines(
            &entry.join("sessions").join("staged.jsonl"),
            &[
                codex_meta("codex-staged"),
                codex_user("staged rollout text"),
            ],
        );
        let leased_home = tmp.path().join("leased-codex-home");
        write_lines(
            &leased_home.join("sessions").join("live.jsonl"),
            &[
                codex_meta("codex-leased"),
                codex_user("leased rollout text"),
            ],
        );
        std::fs::create_dir_all(&active_root).unwrap();
        std::fs::write(
            active_root.join("codex.json"),
            serde_json::json!({
                "schema": 1, "dir_name": "codex", "source": "codex",
                "home": leased_home.to_string_lossy(),
                "materialized_at_ms": now_ms(),
            })
            .to_string(),
        )
        .unwrap();
        add_registry_and_staged_roots(&active_root, &staging_root, &mut roots);

        let mut indexer = Indexer::default();
        let stats = indexer.sweep(&roots);
        assert_eq!(stats.published, 2);
        assert_eq!(stats.drained_entries, 1);
        assert!(!entry.exists(), "fully published staged entry is drained");

        // Next sweep re-resolves (the entry is gone): the staged
        // session's only source path vanished with the drain — coverage
        // flips, records stay readable.
        let mut roots = rig(tmp.path());
        add_registry_and_staged_roots(&active_root, &staging_root, &mut roots);
        let stats = indexer.sweep(&roots);
        assert_eq!(stats.source_gone, 1);
        let store = Store::open(&roots.store_root).unwrap();
        let snapshot = store.snapshot();
        let staged_entry = snapshot
            .manifest
            .sessions
            .get("codex:codex-staged")
            .unwrap();
        assert!(staged_entry.source_gone);
        assert_eq!(
            snapshot.read_shard("codex:codex-staged").unwrap().records[0].text,
            "staged rollout text"
        );
        let leased_entry = snapshot
            .manifest
            .sessions
            .get("codex:codex-leased")
            .unwrap();
        assert!(!leased_entry.source_gone);
    }

    #[test]
    fn tombstoned_sessions_are_never_reindexed() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = rig(tmp.path());
        write_intendant_session(&roots.intendant_logs, "native-t", &["short lived"]);
        let mut indexer = Indexer::default();
        assert_eq!(indexer.sweep(&roots).published, 1);

        let store = Store::open(&roots.store_root).unwrap();
        store.delete_session("intendant:native-t").unwrap();
        // The source keeps changing; the tombstone still wins.
        write_intendant_session(&roots.intendant_logs, "native-t", &["short lived", "again"]);
        let stats = indexer.sweep(&roots);
        assert_eq!(stats.published, 0);
        assert!(store.snapshot().read_shard("intendant:native-t").is_none());
    }

    #[test]
    fn appended_sources_republish_incrementally() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = rig(tmp.path());
        let dir = write_intendant_session(&roots.intendant_logs, "native-2", &["first"]);
        let mut indexer = Indexer::default();
        assert_eq!(indexer.sweep(&roots).published, 1);

        let row = serde_json::json!({
            "ts": "10:00:01.000", "ts_ms": now_ms(), "event": "conversation_message",
            "data": {"message_id": "mid-9", "message_seq": 9,
                     "role": "user", "provenance": "follow_up", "text": "second"},
        });
        append_line(&dir.join("session.jsonl"), &row);
        let stats = indexer.sweep(&roots);
        assert_eq!(stats.published, 1);
        let shard = Store::open(&roots.store_root)
            .unwrap()
            .snapshot()
            .read_shard("intendant:native-2")
            .unwrap();
        let texts: Vec<&str> = shard.records.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["first", "second"]);
    }
}

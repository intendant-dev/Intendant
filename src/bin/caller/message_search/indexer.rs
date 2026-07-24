//! The wiring edge of the message-search program (plan §5–6): a
//! background sweep that enumerates every message source on this box —
//! intendant session logs, Codex rollouts (user home, leased-active
//! homes, staged lease remnants), Claude Code transcripts (same three
//! places), Kimi wires and Pi session trees — runs the per-source extractors, and publishes the resulting
//! shards to the store. Machine-global external stores are swept by design
//! even under a scratch `INTENDANT_HOME` (a Codex/Claude conversation
//! belongs to the machine, not to one daemon home);
//! [`DAEMON_HOME_ONLY_ENV`] is the explicit hermetic-rig off-switch, gating
//! root assembly rather than query-time filtering. Everything below
//! [`spawn_indexer`] takes its roots as parameters; only that production
//! edge resolves the real environment.
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
use super::extract_kimi::extract_kimi_session;
use super::extract_pi::extract_pi_session;
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
    /// Kimi homes: each directly contains `session_index.jsonl` and
    /// `sessions/<workdir-key>/session_<uuid>/`.
    pub kimi_roots: Vec<PathBuf>,
    /// Pi agent dirs: each directly contains
    /// `sessions/--<encoded-cwd>--/<timestamp>_<id>.jsonl`.
    pub pi_roots: Vec<PathBuf>,
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
/// Publishes queued per sweep before a batched manifest flush; bounds how
/// many derived shards sit in memory awaiting the flush.
const PUBLISH_BATCH_MAX: usize = 16;

#[derive(Default)]
pub(crate) struct Indexer {
    unpublishable: HashMap<PathBuf, SourceCursor>,
    /// (source path for failure attribution, queued publish).
    pending_publishes: Vec<(PathBuf, super::store::PendingPublish)>,
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
        self.sweep_kimi(
            roots,
            &store,
            &snapshot,
            &cursor_by_path,
            &mut seen_paths,
            &mut failed_paths,
            horizon,
            &mut stats,
        );
        self.sweep_pi(
            roots,
            &store,
            &snapshot,
            &cursor_by_path,
            &mut seen_paths,
            &mut failed_paths,
            horizon,
            &mut stats,
        );
        // Everything derived this sweep persists before the coverage and
        // drain passes below read `failed_paths` / the on-disk manifest.
        self.flush_pending_publishes(&store, &mut failed_paths, &mut stats);

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
                let mut agent_paths = subagents.remove(&session_id).unwrap_or_default();
                // Relocated project dirs can carry HARDLINKED twins of the
                // same subagent transcripts (one session observed under
                // both its real project path and a volume-alias path, same
                // inodes) — extracting both would duplicate every subagent
                // record. Dedup by file identity, path as the fallback.
                agent_paths.sort();
                agent_paths.dedup();
                let mut seen_identities: Vec<crate::platform::FileIdentity> = Vec::new();
                agent_paths.retain(
                    |path| match crate::platform::FileIdentity::from_path(path) {
                        Ok(identity) if identity.is_reliable() => {
                            if seen_identities.contains(&identity) {
                                false
                            } else {
                                seen_identities.push(identity);
                                true
                            }
                        }
                        _ => true,
                    },
                );
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
                let main_check = cursor_by_path
                    .get(&main_path)
                    .map(|(_, cursor)| cursor.check());
                // Only transcript-shaped agent files can affect the shard
                // (the extractor filters to exactly this set), so only
                // they participate in the skip/fold decisions — a foreign
                // .jsonl under subagents/ no longer forces a re-parse of
                // an otherwise-unchanged session every sweep.
                let transcript_agents =
                    super::extract_claude::claude_transcript_agents(&agent_paths);
                let agents_unchanged = transcript_agents.iter().all(|path| {
                    cursor_by_path
                        .get(path.as_path())
                        .is_some_and(|(_, cursor)| cursor.check() == CursorCheck::Unchanged)
                });
                if matches!(main_check, Some(CursorCheck::Unchanged)) && agents_unchanged {
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
                // Main-only append with a byte-unchanged, set-unchanged
                // subagent side: fold the appended suffix into the
                // published shard (claude records are line-local) instead
                // of re-parsing tens of MB per live session per sweep.
                // Every guard failure falls through to the full parse.
                if matches!(main_check, Some(CursorCheck::Appended)) && agents_unchanged {
                    let saved_for_session = snapshot
                        .manifest
                        .sessions
                        .get(&session_key)
                        .map(|entry| entry.cursors.len())
                        .unwrap_or(0);
                    // Exact source-set match (no removed transcript may
                    // linger in the fold; a removed source must shrink the
                    // shard exactly as a full re-extract would), and the
                    // main cursor must carry BOTH rewrite-detection
                    // windows — a pre-tail-window cursor never resumes.
                    let sources_match = saved_for_session == 1 + transcript_agents.len()
                        && cursor_by_path.get(&main_path).is_some_and(|(key, cursor)| {
                            key == &session_key && cursor.supports_incremental_resume()
                        })
                        && transcript_agents.iter().all(|path| {
                            cursor_by_path
                                .get(path.as_path())
                                .is_some_and(|(key, _)| key == &session_key)
                        });
                    let prior = if sources_match {
                        snapshot
                            .read_shard(&session_key)
                            .filter(|shard| !shard.records.is_empty())
                    } else {
                        None
                    };
                    if let Some(prior) = prior {
                        let (_, main_cursor) = cursor_by_path
                            .get(&main_path)
                            .expect("main cursor checked above");
                        match super::extract_claude::fold_claude_main_append(
                            &session_id,
                            &main_path,
                            main_cursor,
                            &prior,
                        ) {
                            Ok(Some((shard, new_main_cursor))) => {
                                stats.parsed += 1;
                                let mut cursors = Vec::with_capacity(1 + transcript_agents.len());
                                cursors.push(new_main_cursor);
                                cursors.extend(transcript_agents.iter().map(|path| {
                                    cursor_by_path
                                        .get(path.as_path())
                                        .expect("agent cursor checked above")
                                        .1
                                        .clone()
                                }));
                                self.publish(
                                    store,
                                    &session_key,
                                    shard,
                                    cursors,
                                    &main_path,
                                    failed_paths,
                                    stats,
                                );
                                continue;
                            }
                            Ok(None) => {}
                            Err(err) => {
                                eprintln!(
                                    "[message-search] claude incremental fold {} failed \
                                     (falling back to full parse): {err}",
                                    main_path.display()
                                );
                            }
                        }
                    }
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

    #[allow(clippy::too_many_arguments)] // one sweep's shared frame, threaded to each lane
    fn sweep_kimi(
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
        let mut locations = HashMap::<
            String,
            crate::web_gateway::session_catalog::kimi_history::KimiSessionLocation,
        >::new();
        for root in &roots.kimi_roots {
            for location in crate::web_gateway::session_catalog::kimi_history::list_kimi_sessions_in(
                root,
                crate::web_gateway::session_catalog::kimi_history::KIMI_SESSION_SCAN_LIMIT,
            ) {
                let replace = locations
                    .get(&location.session_id)
                    .map(|current| location.activity_mtime() > current.activity_mtime())
                    .unwrap_or(true);
                if replace {
                    locations.insert(location.session_id.clone(), location);
                }
            }
        }
        let mut locations = locations.into_values().collect::<Vec<_>>();
        locations.sort_by(|left, right| left.session_id.cmp(&right.session_id));

        for location in locations {
            let source_paths = location
                .all_dependency_paths()
                .map(Path::to_path_buf)
                .collect::<Vec<_>>();
            let newest_mtime = source_paths
                .iter()
                .map(|path| file_mtime_ms(path))
                .max()
                .unwrap_or(0);
            if newest_mtime < horizon_ms {
                continue;
            }
            for path in &source_paths {
                seen_paths.insert(path.clone());
            }
            let session_key = format!("{}:{}", Source::Kimi.as_str(), location.session_id);
            if snapshot.manifest.tombstones.contains_key(&session_key) {
                continue;
            }
            let saved_source_count = snapshot
                .manifest
                .sessions
                .get(&session_key)
                .map(|entry| entry.cursors.len())
                .unwrap_or(0);
            let all_unchanged = saved_source_count == source_paths.len()
                && source_paths.iter().all(|path| {
                    cursor_by_path.get(path).is_some_and(|(key, cursor)| {
                        key == &session_key && cursor.check() == CursorCheck::Unchanged
                    })
                });
            if all_unchanged {
                stats.skipped_unchanged += 1;
                continue;
            }
            let primary = location
                .agents
                .iter()
                .find(|agent| {
                    agent.id == crate::web_gateway::session_catalog::kimi_history::KIMI_MAIN_AGENT
                })
                .map(|agent| agent.wire_path.clone())
                .unwrap_or_else(|| location.state_path.clone());
            if !cursor_by_path.contains_key(&primary)
                && self.skip_known_unpublishable(&primary, stats)
            {
                continue;
            }
            stats.parsed += 1;
            match extract_kimi_session(location) {
                Ok((shard, cursors)) => {
                    if shard.records.is_empty() {
                        self.remember_unpublishable(&primary, cursors);
                        continue;
                    }
                    self.publish(
                        store,
                        &session_key,
                        shard,
                        cursors,
                        &primary,
                        failed_paths,
                        stats,
                    );
                }
                Err(err) => {
                    eprintln!(
                        "[message-search] Kimi extract {} failed: {err}",
                        primary.display()
                    );
                    failed_paths.push(primary);
                    stats.failures += 1;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // one sweep's shared frame, threaded to each lane
    fn sweep_pi(
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
        let mut locations = HashMap::<
            String,
            crate::web_gateway::session_catalog::pi_history::PiSessionLocation,
        >::new();
        for root in &roots.pi_roots {
            for location in crate::web_gateway::session_catalog::pi_history::list_pi_sessions_in(
                root,
                crate::web_gateway::session_catalog::pi_history::PI_SESSION_SCAN_LIMIT,
            ) {
                let replace = locations
                    .get(&location.session_id)
                    .map(|current| location.updated_millis > current.updated_millis)
                    .unwrap_or(true);
                if replace {
                    locations.insert(location.session_id.clone(), location);
                }
            }
        }
        let mut locations = locations.into_values().collect::<Vec<_>>();
        locations.sort_by(|left, right| left.session_id.cmp(&right.session_id));

        for location in locations {
            let path = location.path.clone();
            if file_mtime_ms(&path) < horizon_ms {
                continue;
            }
            seen_paths.insert(path.clone());
            let session_key = format!("{}:{}", Source::Pi.as_str(), location.session_id);
            if snapshot.manifest.tombstones.contains_key(&session_key) {
                continue;
            }
            if cursor_by_path.get(&path).is_some_and(|(key, cursor)| {
                key == &session_key && cursor.check() == CursorCheck::Unchanged
            }) {
                stats.skipped_unchanged += 1;
                continue;
            }
            if !cursor_by_path.contains_key(&path) && self.skip_known_unpublishable(&path, stats) {
                continue;
            }
            stats.parsed += 1;
            match extract_pi_session(location) {
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
                Err(error) => {
                    eprintln!(
                        "[message-search] Pi extract {} failed: {error}",
                        path.display()
                    );
                    failed_paths.push(path);
                    stats.failures += 1;
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
    /// Queue a derived shard for the batched flush; publishing per session
    /// paid one writer lock + full manifest read + full manifest rewrite
    /// EACH. Flushes early at [`PUBLISH_BATCH_MAX`] to bound memory.
    #[allow(clippy::too_many_arguments)] // mirrors the sweep frame it serves
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
        self.pending_publishes.push((
            source_path.to_path_buf(),
            super::store::PendingPublish {
                session_key: session_key.to_string(),
                shard,
                cursors,
                source_gone: false,
            },
        ));
        if self.pending_publishes.len() >= PUBLISH_BATCH_MAX {
            self.flush_pending_publishes(store, failed_paths, stats);
        }
    }

    fn flush_pending_publishes(
        &mut self,
        store: &Store,
        failed_paths: &mut Vec<PathBuf>,
        stats: &mut SweepStats,
    ) {
        if self.pending_publishes.is_empty() {
            return;
        }
        let staged = std::mem::take(&mut self.pending_publishes);
        let (source_paths, batch): (Vec<PathBuf>, Vec<super::store::PendingPublish>) =
            staged.into_iter().unzip();
        let session_keys: Vec<String> = batch
            .iter()
            .map(|pending| pending.session_key.clone())
            .collect();
        for ((outcome, source_path), session_key) in store
            .publish_sessions(batch)
            .into_iter()
            .zip(source_paths)
            .zip(session_keys)
        {
            match outcome {
                Ok(PublishOutcome::Published) => stats.published += 1,
                // A concurrent daemon got there first with fresher
                // progress; ours was derived from the same sources —
                // nothing lost.
                Ok(PublishOutcome::RejectedStale) => {}
                Ok(PublishOutcome::RejectedTombstoned) => {}
                Err(err) => {
                    eprintln!("[message-search] publish {session_key} failed: {err}");
                    failed_paths.push(source_path);
                    stats.failures += 1;
                }
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

/// The hermetic-rig off-switch for machine-global sweeping. External-agent
/// stores (`~/.codex`, `~/.claude/projects`, `~/.kimi-code`, `~/.pi/agent`
/// and their `CODEX_HOME`-class env overrides) belong to the machine, not to
/// one daemon state root, so a scratch-`INTENDANT_HOME` daemon still sweeps
/// them BY DESIGN. A rig that must not see the box's real corpus sets this
/// truthy (`1`/`true`/`yes`/`on`, case-insensitive) to confine source
/// discovery to the daemon's own state root — the machine stores are not
/// swept, and persisted per-session backend homes at or inside them are
/// excluded too (sessions launched without an explicit backend home persist
/// the machine-global default); explicit non-default homes are swept
/// wherever they live. Every other value — unset, empty, typos — keeps the
/// machine-global default, so misconfiguration degrades to current
/// behavior, never to a silent exclusion.
pub(crate) const DAEMON_HOME_ONLY_ENV: &str = "INTENDANT_MESSAGE_SEARCH_DAEMON_HOME_ONLY";

/// Interpret a raw [`DAEMON_HOME_ONLY_ENV`] value. Split from
/// [`resolve_production_roots`] so tests can pin the vocabulary without
/// racing the parallel runner over process-global env (the
/// `state_paths::intendant_home_override` convention).
pub(crate) fn daemon_home_only(raw: Option<std::ffi::OsString>) -> bool {
    raw.and_then(|value| value.into_string().ok())
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// The four machine-global external-agent store roots as resolved for this
/// process. Computed even in daemon-home-only mode: knowing where the
/// machine stores ARE is how persisted per-session homes that point back
/// into them get excluded — computing a path for comparison is not
/// sweeping it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineStores {
    pub codex: PathBuf,
    pub claude_projects: PathBuf,
    pub kimi: PathBuf,
    pub pi: PathBuf,
}

impl MachineStores {
    /// Resolve from a user home. For the process's own home the backend
    /// resolvers honor `CODEX_HOME` / `KIMI_CODE_HOME` /
    /// `PI_CODING_AGENT_DIR`; an explicit alternate home (tests) stays
    /// hermetic — no env consult.
    pub(crate) fn resolve(user_home: &Path) -> Self {
        Self {
            codex: crate::web_gateway::session_catalog::backend_lists::codex_dir(user_home),
            claude_projects: user_home.join(".claude").join("projects"),
            kimi: crate::web_gateway::session_catalog::kimi_history::kimi_home_in(user_home),
            pi: crate::web_gateway::session_catalog::pi_history::pi_agent_dir_in(user_home),
        }
    }
}

/// Resolve the box's real message sources: the user's default homes, the
/// lease-active registry (live materialized homes, indexed DURING the
/// lease), and staged lease remnants awaiting drain. The one
/// production-edge function here — everything else takes roots.
pub(crate) fn resolve_production_roots() -> SweepRoots {
    let staging = crate::lease_transcript_staging::default_paths();
    assemble_sweep_roots(
        Store::default_root(),
        crate::platform::intendant_home().join("logs"),
        &MachineStores::resolve(&crate::platform::home_dir()),
        daemon_home_only(std::env::var_os(DAEMON_HOME_ONLY_ENV)),
        &staging.active,
        &staging.staging,
    )
}

/// The assembly half of [`resolve_production_roots`], parameterized for
/// tests. With `daemon_home_only: false` (the default) the four
/// machine-global stores are swept alongside everything discovered from the
/// daemon's own state root. `true` is the [`DAEMON_HOME_ONLY_ENV`] posture:
/// the machine stores are not swept, AND persisted per-session backend
/// homes that sit at or inside one of them are excluded too — a session
/// launched without an explicit backend home persists the machine-global
/// default into its config (Codex verbatim; Kimi as a bridge subdirectory
/// whose entries mirror the store), and re-adding it would reopen the hole
/// the knob closes. Explicit non-default homes are swept wherever they live
/// (a rig's tempdir backend home keeps indexing), as do the lease
/// registry/staging roots, which are materialized under the state root by
/// construction. The knob gates assembly here, not query-time filtering: an
/// excluded store is simply never swept.
pub(crate) fn assemble_sweep_roots(
    store_root: PathBuf,
    intendant_logs: PathBuf,
    machine_stores: &MachineStores,
    daemon_home_only: bool,
    active_root: &Path,
    staging_root: &Path,
) -> SweepRoots {
    let mut roots = SweepRoots {
        store_root,
        intendant_logs,
        ..SweepRoots::default()
    };
    if !daemon_home_only {
        roots.codex_roots.push(machine_stores.codex.clone());
        roots
            .claude_project_roots
            .push(machine_stores.claude_projects.clone());
        roots.kimi_roots.push(machine_stores.kimi.clone());
        roots.pi_roots.push(machine_stores.pi.clone());
    }
    add_registry_and_staged_roots(active_root, staging_root, &mut roots);
    let logs_root = roots.intendant_logs.clone();
    let (codex_excluded, kimi_excluded): (&[PathBuf], &[PathBuf]) = if daemon_home_only {
        (
            std::slice::from_ref(&machine_stores.codex),
            std::slice::from_ref(&machine_stores.kimi),
        )
    } else {
        (&[], &[])
    };
    add_session_codex_home_roots(&logs_root, codex_excluded, &mut roots);
    add_session_kimi_home_roots(&logs_root, kimi_excluded, &mut roots);
    roots
}

/// Per-session Kimi home overrides mirror the Codex override lane. The
/// launch/config writer owns the field; this read-only sweep merely makes
/// those persisted sessions searchable.
///
/// `excluded_stores` (daemon-home-only mode) drops persisted homes at or
/// inside a machine-global store by path containment, not equality: the
/// only writer of `kimi_home` is the bridge materializer, which persists
/// `<primary-home>/intendant-bridges/<hash>` — a subdirectory whose entries
/// symlink (or copy) the primary home's history, so a bridge under a
/// machine store IS that store's content.
pub(crate) fn add_session_kimi_home_roots(
    logs_root: &Path,
    excluded_stores: &[PathBuf],
    roots: &mut SweepRoots,
) {
    for home in crate::session_config::persisted_kimi_homes_in_logs(logs_root) {
        if excluded_stores.iter().any(|store| home.starts_with(store)) {
            continue;
        }
        if !roots.kimi_roots.contains(&home) {
            roots.kimi_roots.push(home);
        }
    }
}

/// Per-session `codex_home` overrides (docs-audit finding 2026-07-12): a
/// session configured with a custom Codex home writes its rollouts
/// there, invisible to the default/leased/staged roots. Each session
/// dir's `session_agent_config.json` is tiny; reading the field per
/// sweep costs less than one shard parse.
///
/// `excluded_stores` (daemon-home-only mode) drops persisted homes at or
/// inside a machine-global store: `external_mode` persists
/// `effective_codex_home` — the `$CODEX_HOME`/`~/.codex` default — for
/// every Codex session launched without an explicit override (the P2 found
/// by Codex review on #581), and re-adding that path would reopen the hole
/// the knob closes.
pub(crate) fn add_session_codex_home_roots(
    logs_root: &Path,
    excluded_stores: &[PathBuf],
    roots: &mut SweepRoots,
) {
    let Ok(entries) = std::fs::read_dir(logs_root) else {
        return;
    };
    for entry in entries.flatten() {
        let config_path = entry.path().join("session_agent_config.json");
        let Ok(raw) = std::fs::read_to_string(&config_path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(home) = value.get("codex_home").and_then(|v| v.as_str()) else {
            continue;
        };
        let home = PathBuf::from(home);
        if excluded_stores.iter().any(|store| home.starts_with(store)) {
            continue;
        }
        if home.is_dir() && !roots.codex_roots.contains(&home) {
            roots.codex_roots.push(home);
        }
    }
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
                Some("kimi") => roots.kimi_roots.push(home),
                Some("pi") => roots.pi_roots.push(home),
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
            // `sessions/` is shared by Codex and Kimi, so the staging
            // manifest is authoritative when present. A manifest-less
            // remnant falls back to the producer shape (`state.json` +
            // `agents/` marks Kimi); this keeps cleanup best-effort.
            let source = std::fs::read_to_string(path.join("manifest.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|value| {
                    value
                        .get("source")
                        .and_then(|source| source.as_str())
                        .map(str::to_string)
                });
            match source.as_deref() {
                Some("kimi") => roots.kimi_roots.push(path.clone()),
                Some("pi") => roots.pi_roots.push(path.clone()),
                Some("claude-code") => {
                    roots.claude_project_roots.push(path.join("projects"));
                }
                Some("codex") => roots.codex_roots.push(path.clone()),
                _ if staged_entry_looks_like_kimi(&path) => roots.kimi_roots.push(path.clone()),
                _ if staged_entry_looks_like_pi(&path) => roots.pi_roots.push(path.clone()),
                _ => {
                    if path.join("sessions").is_dir() || path.join("archived_sessions").is_dir() {
                        roots.codex_roots.push(path.clone());
                    }
                    if path.join("projects").is_dir() {
                        roots.claude_project_roots.push(path.join("projects"));
                    }
                }
            }
            roots.staged_entries.push(path);
        }
    }
}

fn staged_entry_looks_like_kimi(entry: &Path) -> bool {
    let mut state_files = Vec::new();
    collect_suffix_files(&entry.join("sessions"), "state.json", 4, &mut state_files);
    state_files.into_iter().any(|state| {
        state
            .parent()
            .is_some_and(|session| session.join("agents").is_dir())
    })
}

fn staged_entry_looks_like_pi(entry: &Path) -> bool {
    let mut session_files = Vec::new();
    collect_suffix_files(&entry.join("sessions"), ".jsonl", 4, &mut session_files);
    session_files.into_iter().any(|path| {
        crate::web_gateway::session_catalog::pi_history::read_pi_session_header(&path).is_some()
    })
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
            kimi_roots: vec![tmp.join("kimi-home")],
            pi_roots: vec![tmp.join("pi-home")],
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

    /// Machine-store resolution from an injected user home pins the four
    /// default paths. Hermetic — an explicit non-process home never
    /// consults `CODEX_HOME`-class env.
    #[test]
    fn machine_stores_resolve_pins_the_default_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let user_home = tmp.path().join("fake-user-home");
        let stores = MachineStores::resolve(&user_home);
        assert_eq!(stores.codex, user_home.join(".codex"));
        assert_eq!(
            stores.claude_projects,
            user_home.join(".claude").join("projects")
        );
        assert_eq!(stores.kimi, user_home.join(".kimi-code"));
        assert_eq!(stores.pi, user_home.join(".pi").join("agent"));
    }

    /// Default assembly (`daemon_home_only: false`): the four machine-global
    /// stores are swept as given.
    #[test]
    fn assemble_sweep_roots_default_includes_machine_global_stores() {
        let tmp = tempfile::tempdir().unwrap();
        let user_home = tmp.path().join("fake-user-home");
        let state = tmp.path().join("state");
        let stores = MachineStores::resolve(&user_home);
        let roots = assemble_sweep_roots(
            state.join("store"),
            state.join("logs"),
            &stores,
            false,
            &state.join("leased-active"),
            &state.join("staging"),
        );
        assert_eq!(roots.store_root, state.join("store"));
        assert_eq!(roots.intendant_logs, state.join("logs"));
        assert_eq!(roots.codex_roots, vec![user_home.join(".codex")]);
        assert_eq!(
            roots.claude_project_roots,
            vec![user_home.join(".claude").join("projects")]
        );
        assert_eq!(roots.kimi_roots, vec![user_home.join(".kimi-code")]);
        assert_eq!(roots.pi_roots, vec![user_home.join(".pi").join("agent")]);
        assert!(roots.staged_entries.is_empty());
    }

    /// Daemon-home-only assembly: no machine-global store is enumerated,
    /// while every source discovered from the daemon's own state root keeps
    /// working — per-session persisted Codex/Kimi homes recorded under the
    /// logs root (explicit, non-default), the leased-active registry, and
    /// staged lease remnants.
    #[test]
    fn assemble_sweep_roots_daemon_home_only_keeps_state_root_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let logs_root = state.join("logs");

        // Per-session Codex home override persisted under the logs root.
        let session_codex_home = tmp.path().join("session-codex-home");
        std::fs::create_dir_all(&session_codex_home).unwrap();
        let session_dir = logs_root.join("cfg-session");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("session_agent_config.json"),
            serde_json::json!({"codex_home": session_codex_home.to_string_lossy()}).to_string(),
        )
        .unwrap();
        // Per-session Kimi bridge home persisted under the logs root.
        let bridge_home = tmp.path().join("kimi-bridge-home");
        std::fs::create_dir_all(&bridge_home).unwrap();
        let config = crate::session_config::SessionAgentConfig {
            source: Some("kimi".to_string()),
            kimi_home: Some(bridge_home.to_string_lossy().to_string()),
            ..Default::default()
        };
        crate::session_config::write_log_dir_config(&logs_root.join("kimi-wrapper"), &config)
            .unwrap();
        // A live leased home in the active registry + a staged remnant.
        let active_root = state.join("leased-active");
        let leased_home = tmp.path().join("leased-codex-home");
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
        let staging_root = state.join("staging");
        let staged_entry = staging_root.join("kimi-123-1");
        std::fs::create_dir_all(&staged_entry).unwrap();
        std::fs::write(
            staged_entry.join("manifest.json"),
            serde_json::json!({"source": "kimi"}).to_string(),
        )
        .unwrap();

        let roots = assemble_sweep_roots(
            state.join("store"),
            logs_root.clone(),
            &MachineStores::resolve(&tmp.path().join("fake-user-home")),
            true,
            &active_root,
            &staging_root,
        );
        assert_eq!(roots.intendant_logs, logs_root);
        assert_eq!(
            roots.codex_roots,
            vec![leased_home, session_codex_home],
            "state-root-discovered Codex homes survive; no machine-global root"
        );
        assert!(
            roots.claude_project_roots.is_empty(),
            "no machine-global Claude projects root: {:?}",
            roots.claude_project_roots
        );
        assert_eq!(
            roots.kimi_roots,
            vec![staged_entry.clone(), bridge_home],
            "staged remnant + persisted bridge home survive; no machine-global root"
        );
        assert!(
            roots.pi_roots.is_empty(),
            "no machine-global Pi root: {:?}",
            roots.pi_roots
        );
        assert_eq!(roots.staged_entries, vec![staged_entry]);
    }

    /// The #581 P2 (found by Codex review): a Codex session launched
    /// without an explicit override persists `effective_codex_home` — the
    /// machine-global default — into its scratch-scoped session config, and
    /// a Kimi session persists a bridge subdirectory under the machine
    /// store whose entries mirror it. In daemon-home-only mode both
    /// persisted shapes are excluded (equality for Codex, containment for
    /// the Kimi bridge); in default mode both are swept as before.
    #[test]
    fn daemon_home_only_drops_persisted_machine_store_homes() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let logs_root = state.join("logs");
        // Stands in for the resolver output, so the same fixture covers
        // the plain `~/.codex`-class default AND a `CODEX_HOME`-class env
        // override (production resolves either into these fields).
        let stores = MachineStores {
            codex: tmp.path().join("machine-codex-store"),
            claude_projects: tmp.path().join("machine-claude-projects"),
            kimi: tmp.path().join("machine-kimi-store"),
            pi: tmp.path().join("machine-pi-store"),
        };

        // A Codex session that persisted the machine default verbatim.
        std::fs::create_dir_all(&stores.codex).unwrap();
        let default_session = logs_root.join("codex-defaulted");
        std::fs::create_dir_all(&default_session).unwrap();
        std::fs::write(
            default_session.join("session_agent_config.json"),
            serde_json::json!({"codex_home": stores.codex.to_string_lossy()}).to_string(),
        )
        .unwrap();
        // A Kimi session that persisted a bridge UNDER the machine store.
        let machine_bridge = stores.kimi.join("intendant-bridges").join("s-1234");
        std::fs::create_dir_all(&machine_bridge).unwrap();
        let config = crate::session_config::SessionAgentConfig {
            source: Some("kimi".to_string()),
            kimi_home: Some(machine_bridge.to_string_lossy().to_string()),
            ..Default::default()
        };
        crate::session_config::write_log_dir_config(&logs_root.join("kimi-defaulted"), &config)
            .unwrap();
        // Explicit non-default homes (a rig's tempdirs) in both lanes.
        let explicit_codex = tmp.path().join("explicit-codex-home");
        std::fs::create_dir_all(&explicit_codex).unwrap();
        let explicit_session = logs_root.join("codex-explicit");
        std::fs::create_dir_all(&explicit_session).unwrap();
        std::fs::write(
            explicit_session.join("session_agent_config.json"),
            serde_json::json!({"codex_home": explicit_codex.to_string_lossy()}).to_string(),
        )
        .unwrap();
        let explicit_kimi = tmp.path().join("explicit-kimi-bridge");
        std::fs::create_dir_all(&explicit_kimi).unwrap();
        let config = crate::session_config::SessionAgentConfig {
            source: Some("kimi".to_string()),
            kimi_home: Some(explicit_kimi.to_string_lossy().to_string()),
            ..Default::default()
        };
        crate::session_config::write_log_dir_config(&logs_root.join("kimi-explicit"), &config)
            .unwrap();

        let assemble = |daemon_home_only: bool| {
            assemble_sweep_roots(
                state.join("store"),
                logs_root.clone(),
                &stores,
                daemon_home_only,
                &state.join("leased-active"),
                &state.join("staging"),
            )
        };

        let restricted = assemble(true);
        assert_eq!(
            restricted.codex_roots,
            vec![explicit_codex.clone()],
            "persisted machine-default Codex home is excluded; explicit home survives"
        );
        assert_eq!(
            restricted.kimi_roots,
            vec![explicit_kimi.clone()],
            "persisted bridge under the machine Kimi store is excluded; explicit home survives"
        );

        let unrestricted = assemble(false);
        assert_eq!(
            unrestricted.codex_roots,
            vec![stores.codex.clone(), explicit_codex],
            "default mode: machine store swept once (persisted twin deduped), explicit home swept"
        );
        // Persisted homes surface in directory-iteration order — compare
        // sorted.
        let mut kimi_roots = unrestricted.kimi_roots.clone();
        kimi_roots.sort();
        let mut expected_kimi = vec![stores.kimi.clone(), machine_bridge, explicit_kimi];
        expected_kimi.sort();
        assert_eq!(
            kimi_roots, expected_kimi,
            "default mode: machine store, its persisted bridge, and the explicit home all swept"
        );
    }

    /// The env vocabulary is pinned: only an explicit truthy value flips to
    /// daemon-home-only; unset, empty, falsy, and garbage all keep the
    /// machine-global default (misconfiguration degrades to CURRENT
    /// behavior, never to a silent exclusion).
    #[test]
    fn daemon_home_only_vocabulary_is_pinned() {
        for truthy in ["1", "true", "yes", "on", "TRUE", "On", " 1 "] {
            assert!(daemon_home_only(Some(truthy.into())), "{truthy:?}");
        }
        for default in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("off"),
            Some("maybe"),
        ] {
            assert!(
                !daemon_home_only(default.map(std::ffi::OsString::from)),
                "{default:?}"
            );
        }
    }

    #[test]
    fn per_session_codex_home_overrides_are_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let mut roots = rig(tmp.path());
        // A session configured with a custom codex home; its rollout
        // lives outside every default root.
        let custom_home = tmp.path().join("custom-codex-home");
        write_lines(
            &custom_home.join("sessions").join("r.jsonl"),
            &[
                codex_meta("codex-custom"),
                codex_user("override rollout text"),
            ],
        );
        let session_dir = roots.intendant_logs.join("cfg-session");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("session_agent_config.json"),
            serde_json::json!({"codex_home": custom_home.to_string_lossy()}).to_string(),
        )
        .unwrap();
        // A dangling override (dir gone) and a duplicate are ignored.
        let dup_dir = roots.intendant_logs.join("cfg-dup");
        std::fs::create_dir_all(&dup_dir).unwrap();
        std::fs::write(
            dup_dir.join("session_agent_config.json"),
            serde_json::json!({"codex_home": custom_home.to_string_lossy()}).to_string(),
        )
        .unwrap();
        let gone_dir = roots.intendant_logs.join("cfg-gone");
        std::fs::create_dir_all(&gone_dir).unwrap();
        std::fs::write(
            gone_dir.join("session_agent_config.json"),
            serde_json::json!({"codex_home": tmp.path().join("nope").to_string_lossy()})
                .to_string(),
        )
        .unwrap();

        let logs_root = roots.intendant_logs.clone();
        add_session_codex_home_roots(&logs_root, &[], &mut roots);
        assert_eq!(
            roots
                .codex_roots
                .iter()
                .filter(|root| **root == custom_home)
                .count(),
            1,
            "override collected exactly once: {:?}",
            roots.codex_roots
        );

        let mut indexer = Indexer::default();
        assert_eq!(indexer.sweep(&roots).published, 1);
        let store = Store::open(&roots.store_root).unwrap();
        assert_eq!(
            store
                .snapshot()
                .read_shard("codex:codex-custom")
                .unwrap()
                .records[0]
                .text,
            "override rollout text"
        );
    }

    fn write_kimi_session(root: &Path, session_id: &str, text: &str) {
        let dir = root.join("sessions").join("wd_repo").join(session_id);
        std::fs::create_dir_all(dir.join("agents/main")).unwrap();
        std::fs::write(
            dir.join("state.json"),
            serde_json::json!({
                "createdAt": now_iso(-30),
                "updatedAt": now_iso(-10),
                "title": text,
                "lastPrompt": text,
                "workDir": "/repo",
                "agents": {"main": {"type": "main", "parentAgentId": null}}
            })
            .to_string(),
        )
        .unwrap();
        write_lines(
            &dir.join("agents/main/wire.jsonl"),
            &[
                serde_json::json!({
                    "type":"turn.prompt",
                    "input":[{"type":"text","text":text}],
                    "origin":{"kind":"user"},
                    "time":now_ms() - 2_000
                }),
                serde_json::json!({
                    "type":"context.append_loop_event",
                    "event":{"type":"content.part","uuid":"answer","part":{"type":"text","text":format!("{text} answer")}},
                    "time":now_ms() - 1_000
                }),
            ],
        );
    }

    #[test]
    fn persisted_kimi_bridge_home_is_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let mut roots = rig(tmp.path());
        roots.kimi_roots.clear();
        let bridge = tmp.path().join("private-kimi-bridge");
        let session_id = "session_99999999-aaaa-bbbb-cccc-dddddddddddd";
        write_kimi_session(&bridge, session_id, "bridge-only Kimi searchable");
        let config_dir = roots.intendant_logs.join("kimi-wrapper");
        let config = crate::session_config::SessionAgentConfig {
            source: Some("kimi".to_string()),
            kimi_home: Some(bridge.to_string_lossy().to_string()),
            ..Default::default()
        };
        crate::session_config::write_log_dir_config(&config_dir, &config).unwrap();

        let logs_root = roots.intendant_logs.clone();
        add_session_kimi_home_roots(&logs_root, &[], &mut roots);
        assert_eq!(roots.kimi_roots, vec![bridge]);
        let mut indexer = Indexer::default();
        assert_eq!(indexer.sweep(&roots).published, 1);
        assert_eq!(
            Store::open(&roots.store_root)
                .unwrap()
                .snapshot()
                .read_shard(&format!("kimi:{session_id}"))
                .unwrap()
                .records[0]
                .text,
            "bridge-only Kimi searchable"
        );
    }

    #[test]
    fn duplicate_kimi_roots_publish_only_the_newest_session_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let mut roots = rig(tmp.path());
        roots.kimi_roots.clear();
        let stale = tmp.path().join("stale-kimi");
        let current = tmp.path().join("current-kimi");
        let session_id = "session_88888888-aaaa-bbbb-cccc-dddddddddddd";
        write_kimi_session(&stale, session_id, "stale duplicate");
        std::thread::sleep(std::time::Duration::from_millis(25));
        write_kimi_session(&current, session_id, "current duplicate");

        // The stale mirror is deliberately last: root iteration order must
        // not decide which copy owns the session shard.
        roots.kimi_roots = vec![current.clone(), stale];
        let mut indexer = Indexer::default();
        assert_eq!(indexer.sweep(&roots).published, 1);
        let snapshot = Store::open(&roots.store_root).unwrap().snapshot();
        let key = format!("kimi:{session_id}");
        let shard = snapshot.read_shard(&key).unwrap();
        assert_eq!(shard.records[0].text, "current duplicate");
        assert!(
            snapshot.manifest.sessions[&key]
                .cursors
                .iter()
                .all(|cursor| cursor.path.starts_with(&current)),
            "only the newest mirror may own persisted cursors"
        );
    }

    #[test]
    fn kimi_active_and_staged_roots_publish_real_session_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let mut roots = rig(tmp.path());
        roots.kimi_roots.clear();
        let active_root = tmp.path().join("leased-active");
        let staging_root = tmp.path().join("staging");
        let live_home = tmp.path().join("leased-kimi-home");
        let staged_home = staging_root.join("kimi-home-1");
        write_kimi_session(
            &live_home,
            "session_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "live Kimi searchable",
        );
        write_kimi_session(
            &staged_home,
            "session_11111111-2222-3333-4444-555555555555",
            "staged Kimi searchable",
        );
        std::fs::create_dir_all(&active_root).unwrap();
        std::fs::write(
            active_root.join("kimi.json"),
            serde_json::json!({
                "schema":1,
                "dir_name":"kimi-home",
                "source":"kimi",
                "home":live_home
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            staged_home.join("manifest.json"),
            serde_json::json!({
                "schema":1,
                "dir_name":"kimi-home",
                "source":"kimi",
                "dirs":["sessions"]
            })
            .to_string(),
        )
        .unwrap();
        add_registry_and_staged_roots(&active_root, &staging_root, &mut roots);
        assert!(roots.kimi_roots.contains(&live_home));
        assert!(roots.kimi_roots.contains(&staged_home));
        assert!(
            !roots.codex_roots.contains(&live_home) && !roots.codex_roots.contains(&staged_home),
            "shared sessions/ layout must not misclassify Kimi as Codex"
        );

        let mut indexer = Indexer::default();
        let stats = indexer.sweep(&roots);
        assert_eq!(stats.published, 2);
        assert_eq!(stats.drained_entries, 1);
        let snapshot = Store::open(&roots.store_root).unwrap().snapshot();
        for (id, text) in [
            (
                "session_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                "live Kimi searchable",
            ),
            (
                "session_11111111-2222-3333-4444-555555555555",
                "staged Kimi searchable",
            ),
        ] {
            let shard = snapshot
                .read_shard(&format!("kimi:{id}"))
                .expect("Kimi shard");
            assert_eq!(shard.records[0].source, Source::Kimi);
            assert_eq!(shard.records[0].text, text);
        }
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

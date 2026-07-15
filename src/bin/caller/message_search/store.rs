//! The rolling shard store (message-search plan §6): per-session
//! generations of [`MessageRecord`]s published through a single manifest,
//! safe for the several daemons that share `~/.intendant` on one box.
//!
//! Coordination model, kept honest per the plan: shards are
//! content-deterministic from their sources, so a lost race self-heals on
//! the next pass — the advisory lock (an `O_EXCL` lockfile with stale
//! takeover) buys query-visible stability and avoids N-daemon duplicate
//! work, NOT loss protection. The correctness gate is the publish path:
//! it re-reads the latest manifest under the lock, merges, and REJECTS
//! any write that would lower a session's source watermark or downgrade
//! `parser_version`. Generation files are immutable and content-named,
//! so an open snapshot keeps reading the files it resolved even while
//! newer manifests land.

use super::cursor::SourceCursor;
use super::record::{MessageRecord, SupersessionMark, PARSER_VERSION};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MANIFEST_NAME: &str = "manifest.json";
const LOCK_NAME: &str = "writer.lock";
/// A lockfile older than this is a crashed holder; take it over. The lock
/// guards efficiency, not correctness, so takeover errs brisk.
const LOCK_STALE_MS: i64 = 5 * 60 * 1000;
/// Retention window (plan v1: fixed 14 days on message `ts_ms`).
pub(crate) const RETENTION_MS: i64 = 14 * 24 * 60 * 60 * 1000;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub schema: u32,
    pub parser_version: u32,
    pub updated_at_ms: i64,
    /// Monotonic write counter — the query side's snapshot watermark.
    /// `updated_at_ms` alone cannot pin a snapshot: two writes inside one
    /// clock millisecond read as unchanged (caught live by the merge
    /// queue's Linux leg). Additive; pre-revision manifests read 0.
    #[serde(default)]
    pub revision: u64,
    /// Keyed by `<source>:<session_id>`.
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionEntry>,
    /// Deliberately deleted sessions (dashboard/API flow): the key and
    /// when. A tombstoned session is never re-published from stale
    /// sources; tombstones expire with the retention window.
    #[serde(default)]
    pub tombstones: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionEntry {
    /// Content-named generation file under `generations/`.
    pub generation_file: String,
    pub records: usize,
    pub newest_ts_ms: i64,
    /// Monotonic per-session source progress: the max
    /// `last_complete_line_offset` sum the publisher had consumed. A
    /// publish carrying a LOWER watermark is a stale writer and is
    /// rejected (plan §6).
    pub source_watermark: u64,
    #[serde(default)]
    pub cursors: Vec<SourceCursor>,
    /// Coverage niceties surfaced by C1 (plan §7).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_gone: bool,
}

/// One session's shard content — records + supersession marks, from which
/// readers derive active status (never stored).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SessionShard {
    pub records: Vec<MessageRecord>,
    #[serde(default)]
    pub marks: Vec<SupersessionMark>,
}

pub(crate) struct Store {
    root: PathBuf,
}

/// A point-in-time view: the manifest as resolved at open time. Generation
/// files are immutable, so reads through this snapshot are stable across
/// concurrent publishes (plan §7's snapshot-pinned pagination builds on
/// this; GC can invalidate very old snapshots, surfaced as missing files).
pub(crate) struct Snapshot {
    root: PathBuf,
    pub manifest: Manifest,
}

pub(crate) enum PublishOutcome {
    Published,
    /// A newer writer got there first (higher watermark or newer parser);
    /// the caller's derivation is stale — re-derive on the next pass.
    RejectedStale,
    /// The session was deliberately deleted; do not resurrect.
    RejectedTombstoned,
}

/// One queued session publish for [`Store::publish_sessions`].
pub(crate) struct PendingPublish {
    pub session_key: String,
    pub shard: SessionShard,
    pub cursors: Vec<SourceCursor>,
    pub source_gone: bool,
}

impl Store {
    pub(crate) fn open(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root.join("generations"))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// The production root: `~/.intendant/cache/message_search/v1`.
    pub(crate) fn default_root() -> PathBuf {
        crate::platform::intendant_home()
            .join("cache")
            .join("message_search")
            .join("v1")
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST_NAME)
    }

    fn read_manifest(&self) -> Manifest {
        let raw = match std::fs::read_to_string(self.manifest_path()) {
            Ok(raw) => raw,
            Err(_) => return Manifest::default(),
        };
        match serde_json::from_str::<Manifest>(&raw) {
            Ok(manifest) => manifest,
            Err(err) => {
                // Corrupt manifest: quarantine it and rebuild from empty —
                // generations are re-derivable from sources (plan §6).
                eprintln!("[message-search] corrupt manifest ({err}); quarantining");
                let _ = std::fs::rename(
                    self.manifest_path(),
                    self.root.join(format!("manifest.corrupt-{}", now_ms())),
                );
                Manifest::default()
            }
        }
    }

    /// Open a stable read view.
    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            root: self.root.clone(),
            manifest: self.read_manifest(),
        }
    }

    /// Publish one session's freshly derived shard. `watermark` is the
    /// publisher's consumed-source progress (sum of cursor offsets).
    /// Production publishes ride [`Self::publish_sessions`] (one manifest
    /// write per batch); this single-session form serves the tests.
    #[cfg(test)]
    pub(crate) fn publish_session(
        &self,
        session_key: &str,
        shard: &SessionShard,
        cursors: Vec<SourceCursor>,
        source_gone: bool,
    ) -> std::io::Result<PublishOutcome> {
        let _lock = WriterLock::acquire(&self.root)?;
        let mut manifest = self.read_manifest();
        let outcome = self.apply_publish(&mut manifest, session_key, shard, cursors, source_gone)?;
        if matches!(outcome, PublishOutcome::Published) {
            self.write_published_manifest(&mut manifest)?;
        }
        Ok(outcome)
    }

    /// Publish a whole batch under ONE writer lock, ONE manifest read and
    /// (at most) ONE manifest write. The per-session publish used to
    /// read + parse and pretty-serialize + rewrite the whole manifest for
    /// every published session, every sweep — measured ~7 GB/day of
    /// manifest write traffic on a busy box. Staleness/tombstone checks
    /// run per session against the same locked view they always ran under.
    pub(crate) fn publish_sessions(
        &self,
        batch: Vec<PendingPublish>,
    ) -> Vec<std::io::Result<PublishOutcome>> {
        fn replicate_error(error: &std::io::Error) -> std::io::Error {
            std::io::Error::new(error.kind(), error.to_string())
        }
        if batch.is_empty() {
            return Vec::new();
        }
        let _lock = match WriterLock::acquire(&self.root) {
            Ok(lock) => lock,
            Err(error) => return batch.iter().map(|_| Err(replicate_error(&error))).collect(),
        };
        let mut manifest = self.read_manifest();
        let mut published_any = false;
        let outcomes: Vec<std::io::Result<PublishOutcome>> = batch
            .into_iter()
            .map(|pending| {
                let outcome = self.apply_publish(
                    &mut manifest,
                    &pending.session_key,
                    &pending.shard,
                    pending.cursors,
                    pending.source_gone,
                );
                if matches!(outcome, Ok(PublishOutcome::Published)) {
                    published_any = true;
                }
                outcome
            })
            .collect();
        if published_any {
            if let Err(error) = self.write_published_manifest(&mut manifest) {
                // Nothing persisted: report the write failure for every
                // entry (orphaned generation files are GC'd as
                // unreferenced).
                return outcomes
                    .iter()
                    .map(|_| Err(replicate_error(&error)))
                    .collect();
            }
        }
        outcomes
    }

    fn write_published_manifest(&self, manifest: &mut Manifest) -> std::io::Result<()> {
        manifest.schema = 1;
        manifest.parser_version = PARSER_VERSION;
        manifest.updated_at_ms = now_ms();
        self.write_manifest(manifest)
    }

    /// The caller holds the writer lock and owns writing `manifest` back.
    fn apply_publish(
        &self,
        manifest: &mut Manifest,
        session_key: &str,
        shard: &SessionShard,
        cursors: Vec<SourceCursor>,
        source_gone: bool,
    ) -> std::io::Result<PublishOutcome> {
        if manifest.tombstones.contains_key(session_key) {
            return Ok(PublishOutcome::RejectedTombstoned);
        }
        if manifest.parser_version > PARSER_VERSION {
            // Never let an older binary clobber a newer format (plan §6
            // no-downgrade rule).
            return Ok(PublishOutcome::RejectedStale);
        }
        let watermark: u64 = cursors
            .iter()
            .map(|cursor| cursor.last_complete_line_offset)
            .sum();
        if let Some(existing) = manifest.sessions.get(session_key) {
            // A lower watermark from the SAME source state is a stale
            // writer (lost race) — reject; the loser re-derives next pass.
            // A lower watermark with CHANGED source fingerprints OR a
            // changed cursor set is a legitimate rebuild (rewritten/
            // shrunk source, or a publisher that stopped double-counting
            // a source file — the soak's hardlink-twin dedup halved a
            // session's watermark and must not be rejected forever).
            let same_paths = existing.cursors.len() == cursors.len()
                && existing
                    .cursors
                    .iter()
                    .all(|old| cursors.iter().any(|new| new.path == old.path));
            let same_source_state = same_paths
                && existing.cursors.iter().all(|old| {
                    cursors
                        .iter()
                        .find(|new| new.path == old.path)
                        .is_none_or(|new| {
                            new.prefix_hash16 == old.prefix_hash16 && new.identity == old.identity
                        })
                });
            if watermark < existing.source_watermark && same_source_state {
                return Ok(PublishOutcome::RejectedStale);
            }
        }

        let body = serde_json::to_string(shard).map_err(std::io::Error::other)?;
        let generation_file = format!("{}.json", crate::session_log::content_hash_hex16(&body));
        let generation_path = self.root.join("generations").join(&generation_file);
        if !generation_path.exists() {
            crate::file_watcher::atomic_write(&generation_path, body.as_bytes())?;
        }

        let newest_ts_ms = shard
            .records
            .iter()
            .map(|record| record.ts_ms)
            .max()
            .unwrap_or(0);
        manifest.sessions.insert(
            session_key.to_string(),
            SessionEntry {
                generation_file,
                records: shard.records.len(),
                newest_ts_ms,
                source_watermark: watermark,
                cursors,
                source_gone,
            },
        );
        Ok(PublishOutcome::Published)
    }

    /// Flip a session's `source_gone` coverage flag without touching its
    /// records or watermark (the source file vanished; the shard serves
    /// "I remember seeing this" until window expiry — plan §6).
    pub(crate) fn mark_source_gone(&self, session_key: &str) -> std::io::Result<()> {
        let _lock = WriterLock::acquire(&self.root)?;
        let mut manifest = self.read_manifest();
        if let Some(entry) = manifest.sessions.get_mut(session_key) {
            if !entry.source_gone {
                entry.source_gone = true;
                manifest.updated_at_ms = now_ms();
                self.write_manifest(&mut manifest)?;
            }
        }
        Ok(())
    }

    /// Deliberate session deletion: drop the shard and tombstone the key.
    pub(crate) fn delete_session(&self, session_key: &str) -> std::io::Result<()> {
        let _lock = WriterLock::acquire(&self.root)?;
        let mut manifest = self.read_manifest();
        manifest.sessions.remove(session_key);
        manifest
            .tombstones
            .insert(session_key.to_string(), now_ms());
        manifest.updated_at_ms = now_ms();
        self.write_manifest(&mut manifest)?;
        self.gc_unreferenced_generations(&manifest);
        Ok(())
    }

    /// Retention GC (plan §6): drop sessions whose newest message left the
    /// window, expire old tombstones, delete unreferenced generations.
    pub(crate) fn gc(&self, now_ms_value: i64) -> std::io::Result<()> {
        let _lock = WriterLock::acquire(&self.root)?;
        let mut manifest = self.read_manifest();
        manifest
            .sessions
            .retain(|_, entry| now_ms_value.saturating_sub(entry.newest_ts_ms) <= RETENTION_MS);
        manifest
            .tombstones
            .retain(|_, at| now_ms_value.saturating_sub(*at) <= RETENTION_MS);
        manifest.updated_at_ms = now_ms_value;
        self.write_manifest(&mut manifest)?;
        self.gc_unreferenced_generations(&manifest);
        Ok(())
    }

    fn gc_unreferenced_generations(&self, manifest: &Manifest) {
        let referenced: std::collections::HashSet<&str> = manifest
            .sessions
            .values()
            .map(|entry| entry.generation_file.as_str())
            .collect();
        let Ok(entries) = std::fs::read_dir(self.root.join("generations")) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !referenced.contains(name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    fn write_manifest(&self, manifest: &mut Manifest) -> std::io::Result<()> {
        manifest.revision += 1;
        // Compact, not pretty: the manifest is machine-read only, grows
        // with session count (1.2 MB at ~1,600 sessions), and is rewritten
        // on every publish flush — indentation was pure write
        // amplification.
        let body = serde_json::to_string(manifest).map_err(std::io::Error::other)?;
        crate::file_watcher::atomic_write(&self.manifest_path(), body.as_bytes())
    }
}

impl Snapshot {
    pub(crate) fn read_shard(&self, session_key: &str) -> Option<SessionShard> {
        let entry = self.manifest.sessions.get(session_key)?;
        let raw =
            std::fs::read_to_string(self.root.join("generations").join(&entry.generation_file))
                .ok()?;
        serde_json::from_str(&raw).ok()
    }
}

/// `O_EXCL` lockfile with stale takeover — see the module doc for why
/// this (and not flock/LockFileEx) is adequate here.
struct WriterLock {
    path: PathBuf,
}

impl WriterLock {
    fn acquire(root: &Path) -> std::io::Result<Self> {
        Self::acquire_with(root, LOCK_STALE_MS)
    }

    fn acquire_with(root: &Path, stale_ms: i64) -> std::io::Result<Self> {
        let path = root.join(LOCK_NAME);
        for attempt in 0..50u32 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = write!(file, "{} {}", std::process::id(), now_ms());
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Crashed holder? Take over by age; otherwise wait.
                    let stale = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| now_ms().saturating_sub(d.as_millis() as i64) > stale_ms)
                        .unwrap_or(true);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20 * (attempt as u64 + 1)));
                }
                Err(err) => return Err(err),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "message-search writer lock is busy",
        ))
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::super::record::{Locator, Role, Source};
    use super::*;

    fn record(ts_ms: i64, text: &str) -> MessageRecord {
        MessageRecord {
            source: Source::Intendant,
            session_id: "s1".into(),
            role: Role::User,
            ts_ms,
            text: text.into(),
            locator: Locator::NativeMessageId {
                message_id: format!("id-{ts_ms}"),
            },
            seq: None,
            user_turn: None,
            item_id: None,
            subagent: false,
            generation: 0,
            truncated: false,
        }
    }

    fn cursor_at(dir: &Path, offset: u64) -> SourceCursor {
        let path = dir.join("source.jsonl");
        if !path.exists() {
            std::fs::write(&path, "line\n".repeat(64)).unwrap();
        }
        SourceCursor::capture(&path, offset).unwrap()
    }

    #[test]
    fn publish_read_roundtrip_and_snapshot_stability() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let shard = SessionShard {
            records: vec![record(100, "hello world")],
            marks: vec![],
        };
        let cursors = vec![cursor_at(tmp.path(), 10)];
        assert!(matches!(
            store
                .publish_session("intendant:s1", &shard, cursors, false)
                .unwrap(),
            PublishOutcome::Published
        ));

        let snapshot = store.snapshot();
        let read = snapshot.read_shard("intendant:s1").unwrap();
        assert_eq!(read.records[0].text, "hello world");

        // A newer publish lands; the OLD snapshot still resolves its
        // generation (immutable, content-named files).
        let shard2 = SessionShard {
            records: vec![record(100, "hello world"), record(200, "second")],
            marks: vec![],
        };
        let cursors2 = vec![cursor_at(tmp.path(), 20)];
        store
            .publish_session("intendant:s1", &shard2, cursors2, false)
            .unwrap();
        assert_eq!(
            snapshot.read_shard("intendant:s1").unwrap().records.len(),
            1
        );
        assert_eq!(
            store
                .snapshot()
                .read_shard("intendant:s1")
                .unwrap()
                .records
                .len(),
            2
        );
    }

    #[test]
    fn stale_watermark_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let shard = SessionShard {
            records: vec![record(100, "fresh")],
            marks: vec![],
        };
        store
            .publish_session(
                "intendant:s1",
                &shard,
                vec![cursor_at(tmp.path(), 50)],
                false,
            )
            .unwrap();

        let stale = SessionShard {
            records: vec![record(90, "stale")],
            marks: vec![],
        };
        assert!(matches!(
            store
                .publish_session(
                    "intendant:s1",
                    &stale,
                    vec![cursor_at(tmp.path(), 10)],
                    false
                )
                .unwrap(),
            PublishOutcome::RejectedStale
        ));
        assert_eq!(
            store.snapshot().read_shard("intendant:s1").unwrap().records[0].text,
            "fresh"
        );
    }

    #[test]
    fn rewritten_source_rebuild_may_lower_the_watermark() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let path = tmp.path().join("source.jsonl");
        std::fs::write(&path, "old content old content\n".repeat(8)).unwrap();
        let first = SourceCursor::capture(&path, 100).unwrap();
        let shard = SessionShard {
            records: vec![record(100, "long history")],
            marks: vec![],
        };
        store
            .publish_session("intendant:s1", &shard, vec![first], false)
            .unwrap();

        // The source is rewritten SHORTER (same-thread restore): the
        // rebuild's watermark is lower, but its prefix fingerprint
        // changed — allowed.
        std::fs::write(&path, "new\n").unwrap();
        let rebuilt_cursor = SourceCursor::capture(&path, 4).unwrap();
        let rebuilt = SessionShard {
            records: vec![record(200, "rebuilt view")],
            marks: vec![],
        };
        assert!(matches!(
            store
                .publish_session("intendant:s1", &rebuilt, vec![rebuilt_cursor], false)
                .unwrap(),
            PublishOutcome::Published
        ));
        assert_eq!(
            store.snapshot().read_shard("intendant:s1").unwrap().records[0].text,
            "rebuilt view"
        );
    }

    #[test]
    fn changed_cursor_set_republish_may_lower_the_watermark() {
        // A publisher that stops double-counting a source (the soak's
        // hardlink-twin dedup) publishes FEWER cursors summing to a lower
        // watermark over unchanged files — a legitimate rebuild, not a
        // stale writer.
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let path = tmp.path().join("main.jsonl");
        std::fs::write(&path, "line\n".repeat(64)).unwrap();
        let twin = SourceCursor::capture(&path, 100).unwrap();
        let shard = SessionShard {
            records: vec![record(100, "doubled")],
            marks: vec![],
        };
        store
            .publish_session(
                "claude-code:s1",
                &shard,
                vec![twin.clone(), twin.clone()],
                false,
            )
            .unwrap();

        let deduped = SessionShard {
            records: vec![record(100, "single")],
            marks: vec![],
        };
        assert!(matches!(
            store
                .publish_session("claude-code:s1", &deduped, vec![twin], false)
                .unwrap(),
            PublishOutcome::Published
        ));
        assert_eq!(
            store
                .snapshot()
                .read_shard("claude-code:s1")
                .unwrap()
                .records[0]
                .text,
            "single"
        );
    }

    #[test]
    fn mark_source_gone_flips_coverage_without_touching_records() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let shard = SessionShard {
            records: vec![record(100, "kept")],
            marks: vec![],
        };
        store
            .publish_session(
                "intendant:s1",
                &shard,
                vec![cursor_at(tmp.path(), 7)],
                false,
            )
            .unwrap();
        store.mark_source_gone("intendant:s1").unwrap();
        let snapshot = store.snapshot();
        let entry = snapshot.manifest.sessions.get("intendant:s1").unwrap();
        assert!(entry.source_gone);
        assert_eq!(entry.source_watermark, 7);
        assert_eq!(
            snapshot.read_shard("intendant:s1").unwrap().records[0].text,
            "kept"
        );
    }

    #[test]
    fn tombstoned_sessions_never_resurrect() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let shard = SessionShard {
            records: vec![record(100, "x")],
            marks: vec![],
        };
        store
            .publish_session(
                "intendant:s1",
                &shard,
                vec![cursor_at(tmp.path(), 5)],
                false,
            )
            .unwrap();
        store.delete_session("intendant:s1").unwrap();
        assert!(store.snapshot().read_shard("intendant:s1").is_none());
        assert!(matches!(
            store
                .publish_session(
                    "intendant:s1",
                    &shard,
                    vec![cursor_at(tmp.path(), 9)],
                    false
                )
                .unwrap(),
            PublishOutcome::RejectedTombstoned
        ));
    }

    #[test]
    fn gc_drops_expired_sessions_tombstones_and_orphan_generations() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let old = SessionShard {
            records: vec![record(1_000, "ancient")],
            marks: vec![],
        };
        let fresh_ts = now_ms();
        let fresh = SessionShard {
            records: vec![record(fresh_ts, "current")],
            marks: vec![],
        };
        store
            .publish_session("intendant:old", &old, vec![cursor_at(tmp.path(), 1)], false)
            .unwrap();
        store
            .publish_session(
                "intendant:new",
                &fresh,
                vec![cursor_at(tmp.path(), 2)],
                false,
            )
            .unwrap();
        store.delete_session("intendant:dead").unwrap();

        store.gc(fresh_ts).unwrap();
        let snapshot = store.snapshot();
        assert!(snapshot.read_shard("intendant:old").is_none());
        assert!(snapshot.read_shard("intendant:new").is_some());
        assert!(snapshot.manifest.tombstones.contains_key("intendant:dead"));

        // Far future (decisively past the window — the tombstone's stamp
        // is a few ms after fresh_ts): everything expires.
        store.gc(fresh_ts + 2 * RETENTION_MS).unwrap();
        let snapshot = store.snapshot();
        assert!(snapshot.manifest.sessions.is_empty());
        assert!(snapshot.manifest.tombstones.is_empty());
        let generations: Vec<_> = std::fs::read_dir(tmp.path().join("generations"))
            .unwrap()
            .flatten()
            .collect();
        assert!(generations.is_empty(), "orphan generations GC'd");
    }

    #[test]
    fn stale_writer_lock_is_taken_over() {
        let tmp = tempfile::tempdir().unwrap();
        // A "crashed" holder left a lockfile.
        std::fs::write(tmp.path().join(super::LOCK_NAME), "999999 0").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        // With an injected 10ms staleness horizon, acquire takes it over
        // instead of waiting out the full production TTL.
        let lock = WriterLock::acquire_with(tmp.path(), 10).unwrap();
        drop(lock);
        assert!(
            !tmp.path().join(super::LOCK_NAME).exists(),
            "lock released on drop"
        );
    }

    #[test]
    fn held_fresh_lock_blocks_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let held = WriterLock::acquire(tmp.path()).unwrap();
        // A second writer with a tiny retry budget cannot get in while the
        // fresh lock is held (verifies the wait path, bounded for tests by
        // takeover-horizon 0 being ineligible — the lock is fresh).
        let root = tmp.path().to_path_buf();
        let contender =
            std::thread::spawn(move || WriterLock::acquire_with(&root, LOCK_STALE_MS).is_ok());
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(held);
        assert!(contender.join().unwrap(), "acquires after release");
    }

    #[test]
    fn corrupt_manifest_is_quarantined_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        std::fs::write(tmp.path().join(MANIFEST_NAME), "{not json").unwrap();
        let snapshot = store.snapshot();
        assert!(snapshot.manifest.sessions.is_empty());
        let quarantined = std::fs::read_dir(tmp.path()).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("manifest.corrupt-")
        });
        assert!(quarantined);
    }
}

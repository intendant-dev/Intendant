//! The agenda op-log store: an append-only JSONL file plus the folded
//! in-memory state. The daemon's control plane owns exactly one
//! [`AgendaStore`] and is its single writer; everything here takes explicit
//! paths (tests thread tempdirs — never the live state root).

use super::types::{
    apply_op, counts, AgendaActor, AgendaCommand, AgendaCounts, AgendaItem, AgendaOp,
    AgendaOpRecord, AgendaPatch, AgendaStatus, AGENDA_LOG_VERSION, MAX_BODY_BYTES, MAX_TAGS,
    MAX_TAG_CHARS, MAX_TITLE_CHARS,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Command intake errors. The gateway maps `NotFound` to 404 and the two
/// rejection variants to 400; `Io` is a daemon-side 500.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AgendaError {
    #[error("agenda item not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Transition(String),
    #[error("agenda log I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// The op names this build folds. Lines whose `op.type` is not listed are
/// preserved on disk but skipped at load (forward compatibility: a newer
/// build's vocabulary — effects, journal curation — must not brick an older
/// daemon's ledger).
const KNOWN_OPS: [&str; 5] = ["add", "patch", "complete", "reopen", "retire"];

const LOG_FILE: &str = "agenda.jsonl";

pub(crate) struct AgendaStore {
    log_path: PathBuf,
    log: std::fs::File,
    items: BTreeMap<String, AgendaItem>,
    /// The largest item id this store has ever seen — folded from disk or
    /// minted here. Fresh mints are floored against it (same-millisecond
    /// mints increment it), so id order equals creation order across
    /// restarts and refolds, not just within one process's generator.
    /// (A fresh `ulid::Generator` per open is only monotonic within
    /// itself — CI's warm Linux runner reopened the store fast enough to
    /// mint a smaller id in the same millisecond.)
    last_id: Option<ulid::Ulid>,
    /// Records folded from disk plus records appended this process.
    ops: u64,
    /// Load-time lines preserved but not folded (torn tail, unknown op
    /// vocabulary, newer line version). Surfaced so frontends can show that
    /// history holds more than this build renders.
    skipped_lines: u64,
    /// Log bytes reflected in `items` (including any terminator this
    /// process wrote). Concurrent daemons on one home share the log file —
    /// a length mismatch on disk means another instance appended, and
    /// [`Self::refresh_if_stale`] refolds. Appends are `O_APPEND`, so
    /// interleaved single-line writes stay whole.
    folded_len: u64,
}

/// Fold raw log bytes into derived state: `(items, ops folded, lines skipped)`.
fn fold_bytes(bytes: &[u8]) -> (BTreeMap<String, AgendaItem>, u64, u64) {
    let text = String::from_utf8_lossy(bytes);
    let mut items = BTreeMap::new();
    let mut ops = 0u64;
    let mut skipped_lines = 0u64;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_record(line) {
            Ok(record) => {
                if let Some(reason) = apply_op(&mut items, &record) {
                    eprintln!("[agenda] fold: {reason}");
                }
                ops += 1;
            }
            Err(reason) => {
                skipped_lines += 1;
                eprintln!("[agenda] skipping log line ({reason}): {line}");
            }
        }
    }
    (items, ops, skipped_lines)
}

impl AgendaStore {
    /// Open (creating if absent) the agenda under `dir`, replaying the op
    /// log into derived state. A file that does not end in a newline (torn
    /// final line from a crash mid-append) is terminated so subsequent
    /// appends start on a fresh line; the torn line itself is preserved and
    /// skipped. Ops are never destroyed or rewritten here.
    pub(crate) fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let log_path = dir.join(LOG_FILE);
        let bytes = match std::fs::read(&log_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        let (items, ops, skipped_lines) = fold_bytes(&bytes);
        let mut folded_len = bytes.len() as u64;

        let mut log = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&log_path)?;
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            log.write_all(b"\n")?;
            folded_len += 1;
        }
        let last_id = max_item_id(&items);
        Ok(Self {
            log_path,
            log,
            items,
            last_id,
            ops,
            skipped_lines,
            folded_len,
        })
    }

    /// Refold when the on-disk log has bytes this store has not seen.
    /// Multiple daemons on one home (the normal topology on a dev box)
    /// share `~/.intendant/agenda`; this keeps their views convergent
    /// without any cross-process coordination beyond `O_APPEND`. Call
    /// before reads and writes — a stat per call, a refold only on change.
    pub(crate) fn refresh_if_stale(&mut self) -> std::io::Result<()> {
        let disk_len = match std::fs::metadata(&self.log_path) {
            Ok(meta) => meta.len(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err),
        };
        if disk_len == self.folded_len {
            return Ok(());
        }
        // Shorter than folded means the append-only contract was broken
        // externally; refolding what's there is the honest recovery either way.
        let bytes = std::fs::read(&self.log_path)?;
        let (items, ops, skipped_lines) = fold_bytes(&bytes);
        self.items = items;
        self.ops = ops;
        self.skipped_lines = skipped_lines;
        self.folded_len = bytes.len() as u64;
        // Never lower the mint floor: our own last mint is on disk, so the
        // folded max normally covers it, but a shrunk/tampered file must
        // not let a future mint sort below an id we already handed out.
        self.last_id = self.last_id.max(max_item_id(&self.items));
        // Another instance's torn tail: terminate it (as `open` does) so
        // our next append starts on a fresh line.
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            self.log.write_all(b"\n")?;
            self.folded_len += 1;
        }
        Ok(())
    }

    /// Validate a frontend intent against current state, append the durable
    /// op, fold it, and return the item as it now stands. This is the only
    /// write path — strictness lives here, not in the tolerant fold.
    pub(crate) fn apply_command(
        &mut self,
        cmd: AgendaCommand,
        actor: Option<AgendaActor>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        // Validate against the freshest state another instance may have left.
        self.refresh_if_stale()?;
        let op = self.command_to_op(cmd)?;
        let item_id = op.item_id().to_string();
        let record = AgendaOpRecord {
            v: AGENDA_LOG_VERSION,
            at_ms: now_ms,
            actor,
            op,
        };
        let mut line = serde_json::to_string(&record)
            .map_err(|err| AgendaError::Invalid(format!("encoding op: {err}")))?;
        line.push('\n');
        // One write_all per record: a crash tears at most the final line,
        // which `open` terminates and skips. Durability is append + flush
        // by ratified scope (no fsync in v1 — the delivery-critical
        // occurrence journal in a later slice adds it where it matters).
        self.log.write_all(line.as_bytes())?;
        self.log.flush()?;
        self.folded_len += line.len() as u64;
        if let Some(reason) = apply_op(&mut self.items, &record) {
            // Unreachable by construction: the command was validated
            // against the exact state the fold sees.
            eprintln!("[agenda] fold rejected a validated command: {reason}");
        }
        self.ops += 1;
        self.items
            .get(&item_id)
            .cloned()
            .ok_or_else(|| AgendaError::Invalid("internal: item missing after fold".into()))
    }

    fn command_to_op(&mut self, cmd: AgendaCommand) -> Result<AgendaOp, AgendaError> {
        match cmd {
            AgendaCommand::Add {
                kind,
                title,
                body,
                tags,
                due_ms,
            } => Ok(AgendaOp::Add {
                id: self.mint_id()?,
                kind,
                title: validate_title(&title)?,
                body: validate_body(body)?,
                tags: validate_tags(tags)?,
                due_ms,
            }),
            AgendaCommand::Patch { id, patch } => {
                self.require(&id)?;
                if patch.is_empty() {
                    return Err(AgendaError::Invalid("patch changes nothing".into()));
                }
                let AgendaPatch {
                    title,
                    body,
                    tags,
                    due_ms,
                } = patch;
                let patch = AgendaPatch {
                    title: title.as_deref().map(validate_title).transpose()?,
                    body: body.map(validate_body).transpose()?,
                    tags: tags.map(validate_tags).transpose()?,
                    due_ms,
                };
                Ok(AgendaOp::Patch { id, patch })
            }
            AgendaCommand::Complete { id } => match self.require(&id)?.status {
                AgendaStatus::Open => Ok(AgendaOp::Complete { id }),
                AgendaStatus::Done => Err(AgendaError::Transition(format!("{id} is already done"))),
                AgendaStatus::Retired => Err(AgendaError::Transition(format!(
                    "{id} is retired — reopen it first"
                ))),
            },
            AgendaCommand::Reopen { id } => match self.require(&id)?.status {
                AgendaStatus::Done | AgendaStatus::Retired => Ok(AgendaOp::Reopen { id }),
                AgendaStatus::Open => Err(AgendaError::Transition(format!("{id} is already open"))),
            },
            AgendaCommand::Retire { id } => match self.require(&id)?.status {
                AgendaStatus::Retired => {
                    Err(AgendaError::Transition(format!("{id} is already retired")))
                }
                AgendaStatus::Open | AgendaStatus::Done => Ok(AgendaOp::Retire { id }),
            },
        }
    }

    fn require(&self, id: &str) -> Result<&AgendaItem, AgendaError> {
        self.items
            .get(id)
            .ok_or_else(|| AgendaError::NotFound(id.to_string()))
    }

    /// Mint the next item id: a fresh ULID, floored against the largest id
    /// ever seen so mint order equals id order even when the clock has not
    /// advanced past the previous mint (same-millisecond restarts, refolds
    /// of another instance's appends).
    fn mint_id(&mut self) -> Result<String, AgendaError> {
        let candidate = ulid::Ulid::new();
        let minted = match self.last_id {
            Some(prev) if candidate <= prev => prev
                .increment()
                .ok_or_else(|| AgendaError::Invalid("id space exhausted; retry".into()))?,
            _ => candidate,
        };
        self.last_id = Some(minted);
        Ok(minted.to_string())
    }

    #[cfg(test)]
    pub(crate) fn get(&self, id: &str) -> Option<&AgendaItem> {
        self.items.get(id)
    }

    /// All items, oldest first (ULID order). Retired items included —
    /// frontends filter; history stays reachable.
    pub(crate) fn snapshot(&self) -> Vec<AgendaItem> {
        self.items.values().cloned().collect()
    }

    pub(crate) fn counts(&self) -> AgendaCounts {
        counts(&self.items)
    }

    #[cfg(test)]
    pub(crate) fn ops(&self) -> u64 {
        self.ops
    }

    pub(crate) fn skipped_lines(&self) -> u64 {
        self.skipped_lines
    }

    #[cfg(test)]
    pub(crate) fn log_path(&self) -> &Path {
        &self.log_path
    }

    #[cfg(test)]
    pub(crate) fn force_last_id_for_tests(&mut self, id: &str) {
        self.last_id = ulid::Ulid::from_string(id).ok();
    }
}

/// The largest item id in the fold, as the mint floor. BTreeMap keys are
/// canonical ULID strings, whose lexicographic max is the numeric max.
fn max_item_id(items: &BTreeMap<String, AgendaItem>) -> Option<ulid::Ulid> {
    items
        .last_key_value()
        .and_then(|(id, _)| ulid::Ulid::from_string(id).ok())
}

/// Two-phase line parse: shape-check the envelope before the typed parse so
/// forward-compatible skips (newer version, unknown op vocabulary) are
/// distinguished from corruption.
fn parse_record(line: &str) -> Result<AgendaOpRecord, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|err| format!("not JSON: {err}"))?;
    let version = value.get("v").and_then(serde_json::Value::as_u64);
    match version {
        Some(v) if v > u64::from(AGENDA_LOG_VERSION) => {
            return Err(format!("line version {v} is newer than this build"));
        }
        Some(_) => {}
        None => return Err("missing version".into()),
    }
    let op_type = value
        .get("op")
        .and_then(|op| op.get("type"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "missing op.type".to_string())?
        .to_string();
    if !KNOWN_OPS.contains(&op_type.as_str()) {
        return Err(format!("unknown op type {op_type:?}"));
    }
    serde_json::from_value(value).map_err(|err| format!("malformed {op_type} op: {err}"))
}

fn validate_title(title: &str) -> Result<String, AgendaError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AgendaError::Invalid("title must not be empty".into()));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(AgendaError::Invalid(format!(
            "title exceeds {MAX_TITLE_CHARS} characters"
        )));
    }
    Ok(title.to_string())
}

fn validate_body(body: String) -> Result<String, AgendaError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(AgendaError::Invalid(format!(
            "body exceeds {MAX_BODY_BYTES} bytes"
        )));
    }
    Ok(body)
}

/// Trim, drop duplicates (first occurrence wins), reject empties and
/// oversizes. Case is preserved — tags are the owner's vocabulary.
fn validate_tags(tags: Vec<String>) -> Result<Vec<String>, AgendaError> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(AgendaError::Invalid("tags must not be empty".into()));
        }
        if tag.chars().count() > MAX_TAG_CHARS {
            return Err(AgendaError::Invalid(format!(
                "tag exceeds {MAX_TAG_CHARS} characters"
            )));
        }
        if !out.iter().any(|existing| existing == tag) {
            out.push(tag.to_string());
        }
    }
    if out.len() > MAX_TAGS {
        return Err(AgendaError::Invalid(format!("more than {MAX_TAGS} tags")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::types::AgendaKind;
    use super::*;

    fn add_cmd(title: &str) -> AgendaCommand {
        AgendaCommand::Add {
            kind: AgendaKind::Task,
            title: title.to_string(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
        }
    }

    fn owner() -> Option<AgendaActor> {
        Some(AgendaActor {
            principal: Some("owner".into()),
            session_id: None,
            kind: None,
        })
    }

    #[test]
    fn add_persists_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        assert!(store.snapshot().is_empty());

        let item = store
            .apply_command(
                AgendaCommand::Add {
                    kind: AgendaKind::Note,
                    title: "  remember the milk  ".into(),
                    body: "whole, not oat".into(),
                    tags: vec![" grocery ".into(), "grocery".into(), "later".into()],
                    due_ms: Some(1_752_000_000_000),
                },
                owner(),
                1000,
            )
            .unwrap();
        assert_eq!(item.title, "remember the milk");
        assert_eq!(item.tags, vec!["grocery".to_string(), "later".to_string()]);
        assert_eq!(item.status, AgendaStatus::Open);
        assert_eq!(item.provenance.principal.as_deref(), Some("owner"));
        assert_eq!(item.provenance.created_ms, 1000);
        assert_eq!(item.id.len(), 26);

        store
            .apply_command(
                AgendaCommand::Complete {
                    id: item.id.clone(),
                },
                None,
                2000,
            )
            .unwrap();

        // The A1 acceptance property at unit level: restart ⇒ history intact.
        drop(store);
        let store = AgendaStore::open(dir.path()).unwrap();
        let items = store.snapshot();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, AgendaStatus::Done);
        assert_eq!(items[0].completed_ms, Some(2000));
        assert_eq!(items[0].title, "remember the milk");
        assert_eq!(store.ops(), 2);
        assert_eq!(store.skipped_lines(), 0);
    }

    /// Mint order must equal id order even when the wall clock has not
    /// advanced past the largest id already on disk — the
    /// same-millisecond store reopen a warm CI runner caught live
    /// (fresh `Generator` per open has no cross-instance monotonicity).
    /// Deterministic here: the floor is forced into the far future, so
    /// every fresh ULID sorts below it and must take the increment path;
    /// the floor must also survive a reopen via the folded max.
    #[test]
    fn mint_floor_keeps_ids_ordered_when_clock_stalls() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let future = "7ZZZZZZZZZ0000000000000000";
        store.force_last_id_for_tests(future);

        let first = store.apply_command(add_cmd("first"), None, 1).unwrap().id;
        let second = store.apply_command(add_cmd("second"), None, 2).unwrap().id;
        assert!(first.as_str() > future, "{first} must be above the floor");
        assert!(second > first);

        // Reopen: the floor is re-derived from the folded max, so a mint
        // in the (real-clock) past still sorts after everything on disk.
        drop(store);
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let third = store.apply_command(add_cmd("third"), None, 3).unwrap().id;
        assert!(third > second);
        let titles: Vec<String> = store.snapshot().into_iter().map(|i| i.title).collect();
        assert_eq!(
            titles,
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ]
        );
    }

    #[test]
    fn ids_are_unique_and_creation_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let ids: Vec<String> = (0..20)
            .map(|i| {
                store
                    .apply_command(add_cmd(&format!("t{i}")), None, 1)
                    .unwrap()
                    .id
            })
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids, sorted, "mint order must equal id order");
        // Snapshot iterates the same order.
        let snap_ids: Vec<String> = store.snapshot().into_iter().map(|i| i.id).collect();
        assert_eq!(snap_ids, ids);
    }

    #[test]
    fn transition_rules_are_strict_at_intake() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let id = store.apply_command(add_cmd("t"), None, 1).unwrap().id;

        let complete = AgendaCommand::Complete { id: id.clone() };
        let reopen = AgendaCommand::Reopen { id: id.clone() };
        let retire = AgendaCommand::Retire { id: id.clone() };

        assert!(matches!(
            store.apply_command(
                AgendaCommand::Complete {
                    id: "01UNKNOWN".into()
                },
                None,
                2
            ),
            Err(AgendaError::NotFound(_))
        ));
        assert!(matches!(
            store.apply_command(reopen.clone(), None, 2),
            Err(AgendaError::Transition(_))
        ));
        store.apply_command(complete.clone(), None, 3).unwrap();
        assert!(matches!(
            store.apply_command(complete.clone(), None, 4),
            Err(AgendaError::Transition(_))
        ));
        store.apply_command(retire.clone(), None, 5).unwrap();
        assert!(matches!(
            store.apply_command(complete, None, 6),
            Err(AgendaError::Transition(_))
        ));
        assert!(matches!(
            store.apply_command(retire, None, 7),
            Err(AgendaError::Transition(_))
        ));
        // Reopen resurrects retired.
        store.apply_command(reopen, None, 8).unwrap();
        assert_eq!(store.get(&id).unwrap().status, AgendaStatus::Open);
        // Only the accepted ops reached the log.
        assert_eq!(store.ops(), 4);
    }

    #[test]
    fn validation_rejects_bad_input_without_appending() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        for cmd in [
            add_cmd("   "),
            add_cmd(&"x".repeat(MAX_TITLE_CHARS + 1)),
            AgendaCommand::Add {
                kind: AgendaKind::Note,
                title: "t".into(),
                body: "b".repeat(MAX_BODY_BYTES + 1),
                tags: Vec::new(),
                due_ms: None,
            },
            AgendaCommand::Add {
                kind: AgendaKind::Note,
                title: "t".into(),
                body: String::new(),
                tags: vec!["  ".into()],
                due_ms: None,
            },
            AgendaCommand::Add {
                kind: AgendaKind::Note,
                title: "t".into(),
                body: String::new(),
                tags: (0..=MAX_TAGS).map(|i| format!("t{i}")).collect(),
                due_ms: None,
            },
        ] {
            assert!(matches!(
                store.apply_command(cmd, None, 1),
                Err(AgendaError::Invalid(_))
            ));
        }
        let id = store.apply_command(add_cmd("ok"), None, 2).unwrap().id;
        assert!(matches!(
            store.apply_command(
                AgendaCommand::Patch {
                    id,
                    patch: AgendaPatch::default(),
                },
                None,
                3,
            ),
            Err(AgendaError::Invalid(_))
        ));
        assert_eq!(store.ops(), 1);
        // Rejected commands left no trace in the log.
        drop(store);
        let store = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(store.ops(), 1);
        assert_eq!(store.snapshot().len(), 1);
    }

    #[test]
    fn patch_edits_presentation_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let id = store.apply_command(add_cmd("before"), None, 1).unwrap().id;
        let item = store
            .apply_command(
                AgendaCommand::Patch {
                    id: id.clone(),
                    patch: AgendaPatch {
                        title: Some("after".into()),
                        due_ms: Some(Some(42)),
                        ..AgendaPatch::default()
                    },
                },
                None,
                2,
            )
            .unwrap();
        assert_eq!(item.title, "after");
        assert_eq!(item.due_ms, Some(42));
        let item = store
            .apply_command(
                AgendaCommand::Patch {
                    id,
                    patch: AgendaPatch {
                        due_ms: Some(None),
                        ..AgendaPatch::default()
                    },
                },
                None,
                3,
            )
            .unwrap();
        assert_eq!(item.due_ms, None);
        assert_eq!(item.title, "after");
    }

    #[test]
    fn torn_tail_is_terminated_skipped_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        store.apply_command(add_cmd("first"), None, 1).unwrap();
        let log_path = store.log_path().to_path_buf();
        drop(store);

        // Simulate a crash mid-append: a torn, newline-less final line.
        let mut file = std::fs::File::options()
            .append(true)
            .open(&log_path)
            .unwrap();
        file.write_all(b"{\"v\":1,\"at_ms\":9,\"op\":{\"ty")
            .unwrap();
        drop(file);

        let mut store = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(store.snapshot().len(), 1);
        assert_eq!(store.skipped_lines(), 1);
        // Appends after the torn tail land on their own line.
        store.apply_command(add_cmd("second"), None, 10).unwrap();
        drop(store);

        let store = AgendaStore::open(dir.path()).unwrap();
        let titles: Vec<String> = store.snapshot().into_iter().map(|i| i.title).collect();
        assert_eq!(titles, vec!["first".to_string(), "second".to_string()]);
        assert_eq!(store.skipped_lines(), 1);
        // The torn line is preserved on disk, not repaired away.
        let raw = std::fs::read_to_string(&log_path).unwrap();
        assert!(raw.contains("{\"v\":1,\"at_ms\":9,\"op\":{\"ty\n"));
    }

    /// Two daemons on one home share the log; each converges on the
    /// other's appends via the stat-cheap staleness check.
    #[test]
    fn concurrent_instances_converge_via_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = AgendaStore::open(dir.path()).unwrap();
        let mut b = AgendaStore::open(dir.path()).unwrap();

        let from_a = a.apply_command(add_cmd("from a"), None, 1).unwrap();
        // B validates against fresh state, so it can act on A's item.
        b.apply_command(
            AgendaCommand::Complete {
                id: from_a.id.clone(),
            },
            None,
            2,
        )
        .unwrap();
        assert_eq!(b.snapshot().len(), 1);

        a.refresh_if_stale().unwrap();
        assert_eq!(a.get(&from_a.id).unwrap().status, AgendaStatus::Done);
        assert_eq!(a.ops(), 2);

        // No-op refresh when nothing changed.
        let before = a.snapshot();
        a.refresh_if_stale().unwrap();
        assert_eq!(a.snapshot(), before);
    }

    #[test]
    fn foreign_vocabulary_lines_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        store.apply_command(add_cmd("mine"), None, 1).unwrap();
        let log_path = store.log_path().to_path_buf();
        drop(store);

        let mut file = std::fs::File::options()
            .append(true)
            .open(&log_path)
            .unwrap();
        // A future build's op, a future line version, and junk.
        file.write_all(
            b"{\"v\":1,\"at_ms\":2,\"op\":{\"type\":\"propose_effect\",\"id\":\"x\"}}\n\
              {\"v\":2,\"at_ms\":3,\"op\":{\"type\":\"add\",\"id\":\"y\"}}\n\
              not json at all\n",
        )
        .unwrap();
        drop(file);

        let store = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(store.snapshot().len(), 1);
        assert_eq!(store.ops(), 1);
        assert_eq!(store.skipped_lines(), 3);
    }
}

//! Message-search shard store (B1 of the message-search program; plan
//! `~/message-search-plan.md` §5–6, docs/src/session-logging.md for the
//! event lane it consumes).
//!
//! Layering: extractors (B2–B4) derive [`MessageRecord`]s +
//! [`SupersessionMark`]s per session from the canonical sources and
//! publish them here; the store owns durability (immutable content-named
//! generations behind one manifest), multi-daemon coordination (advisory
//! lock + watermark rejection — see `store.rs`), retention, and stable
//! snapshots. Matching, normalization, and the byte-budget arena are the
//! query side's concern (C1) and deliberately absent here. Active vs
//! superseded is ALWAYS derived at read time ([`derive_active`]) — never
//! stored — because Codex restores can reactivate messages (plan D2).

// The store's API surface (types AND the re-exports below) is a
// deliberately parked seed until its consumers land (B2–B4 extractors,
// C1 query side — the very next program units); `startup_gc` is the one
// production consumer wired today. Remove both allows as the extractors
// adopt the API.
#![allow(dead_code, unused_imports)]

mod cursor;
mod extract_codex;
mod record;
mod store;

pub(crate) use cursor::{read_complete_lines_from, CursorCheck, SourceCursor};
pub(crate) use record::{
    cap_text, derive_active, Locator, MessageRecord, Role, Source, SupersessionMark,
    MESSAGE_TEXT_CAP_BYTES, PARSER_VERSION,
};
pub(crate) use store::{PublishOutcome, SessionShard, Snapshot, Store, RETENTION_MS};

/// Boot-time retention GC over the production store root (plan §6):
/// expired shards and tombstones must not accumulate while no extractor
/// or drainer is running yet.
pub(crate) fn startup_gc() {
    let root = Store::default_root();
    if !root.exists() {
        return;
    }
    match Store::open(&root) {
        Ok(store) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if let Err(err) = store.gc(now) {
                eprintln!("[message-search] startup gc failed: {err}");
            }
        }
        Err(err) => eprintln!("[message-search] store open failed: {err}"),
    }
}

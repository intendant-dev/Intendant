//! Message-search shard store + extraction pipeline (plan
//! `~/message-search-plan.md` §5–6, docs/src/session-logging.md for the
//! native event lane it consumes).
//!
//! Layering: the per-source extractors (`extract_intendant` /
//! `extract_codex` / `extract_claude`) derive [`MessageRecord`]s +
//! [`SupersessionMark`]s per session from the canonical sources; the
//! [`indexer`] sweep enumerates this box's sources (session logs, Codex
//! and Claude homes, leased-active homes, staged lease remnants) and
//! publishes shards to the store, which owns durability (immutable
//! content-named generations behind one manifest), multi-daemon
//! coordination (advisory lock + watermark rejection — see `store.rs`),
//! retention, and stable snapshots. Matching, normalization, and the
//! byte-budget arena are the query side's concern (C1) and deliberately
//! absent here. Active vs superseded is ALWAYS derived at read time
//! ([`record::derive_active`]) — never stored — because Codex restores
//! can reactivate messages (plan D2).

mod cursor;
mod extract_claude;
mod extract_codex;
mod extract_intendant;
mod indexer;
mod query;
mod record;
mod store;

// The session-detail `locate=` resolver (web_gateway/session_catalog/
// locate.rs, plan §7 C2) verifies locators exactly the way the extractors
// mint them, so it consumes the frozen locator type and the legacy
// follow-up line parser from here.
pub(crate) use extract_intendant::parse_round_follow_up;
pub(crate) use indexer::{refresh_if_stale, spawn_indexer};
pub(crate) use query::{parse_sources, run_message_search, MessageSearchParams};
pub(crate) use record::{Locator, MESSAGE_TEXT_CAP_BYTES};
pub(crate) use store::Store;

/// Boot-time retention GC over the production store root (plan §6):
/// expired shards and tombstones must not accumulate before the first
/// sweep runs (the indexer's own GC rides a slow cadence).
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

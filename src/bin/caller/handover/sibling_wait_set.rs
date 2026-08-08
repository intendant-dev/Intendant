//! Successor-side widening of a draining sibling's named wait set past
//! the presence cap (card 01KZD84XEC, the 17-of-22 night's tail): the
//! presence record itemizes at most [`super::presence::PRESENCE_HOLDOUT_ROWS_CAP`]
//! holdout rows — it crosses the handover wire and deliberately does
//! NOT grow — so on a >cap takeover the successor's drain map covered
//! only the first 16 sessions and rows past the cap still read
//! preboot/ghost. The fix rides the sibling doorway the takeover lane
//! already uses (loopback + the drainer's per-port admission token from
//! the shared state root): fetch the DRAINER'S OWN uncapped wait set —
//! `GET /api/daemon/handover`, whose top-level `holdouts` block is the
//! drainer's authoritative, deliberately uncapped self-report from the
//! same live supervisor registry its catalog rows derive from — cache
//! it per (state root, boot), and let the drain-map and status
//! consumers widen the capped presence rows with it.
//!
//! Trust posture, in order:
//! - **The presence rows are the FLOOR.** They arrive instantly by disk
//!   read (the fast-path hint at boot) and win on per-session conflict;
//!   the fetched set only ever ADDS sessions the cap hid. A fetch that
//!   never lands, fails, or ages out degrades to exactly the floor —
//!   never less than the presence lane alone.
//! - **The provable-liveness gate is consume-side and presence-locked.**
//!   Consumers call [`resolve_sibling_wait_set`] only for a record that
//!   passes the same gate as the presence rows themselves (draining
//!   state + boot lock HELD, probed at serve time), and the fetch task
//!   re-checks that gate before any network. A cached claim from a
//!   now-dead drainer is therefore inert: the serve-time probe fails
//!   and the whole sibling's rows — floor and fetched alike — drop.
//! - **Admission is verified.** A response is cached only when the
//!   answering daemon names the expected `boot_id` and self-reports
//!   `draining` — a port re-used by some other process cannot inject
//!   rows.
//!
//! Fetches are scheduled from sync serve paths onto the ambient tokio
//! runtime (catalog builds run on `spawn_blocking` threads, which keep
//! the runtime context); without a runtime — plain unit tests — nothing
//! is scheduled and consumers serve the floor, deterministically.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::presence::{sort_drain_holdout_rows, DrainHoldout, PresenceRecord};

/// Spacing between fetch attempts against one sibling — the wait set
/// shrinks as the drain progresses, so cached rows refresh on this
/// beat while a drain surface keeps polling.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Stale-while-revalidate bound: fetched rows older than this stop
/// serving (degrade to the presence floor) rather than over-claim a
/// wait set the drainer stopped answering about a minute ago. The
/// consume-side liveness gate already drops a DEAD drainer instantly;
/// this bound is about a live-but-unreachable one.
const SERVE_MAX_AGE: Duration = Duration::from_secs(60);

/// The doorway fetch is loopback to a live co-homed daemon — tight.
const FETCH_TIMEOUT: Duration = Duration::from_secs(4);

/// One sibling's cached fetch state.
#[derive(Default)]
struct Entry {
    /// The last admitted uncapped wait set and when it landed.
    rows: Option<(Vec<DrainHoldout>, Instant)>,
    /// Last attempt start — success or failure — for the retry beat.
    last_attempt: Option<Instant>,
    /// A fetch task is in flight; do not stack another.
    in_flight: bool,
}

/// Cache keyed by (state root, boot id): one daemon process serves one
/// home, but tests run many fixture homes through one process and a
/// boot id must never leak across them.
fn cache() -> &'static Mutex<HashMap<(PathBuf, String), Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<(PathBuf, String), Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Did the presence cap (or a pre-holdouts-era record) truncate this
/// record's named rows? `session_count` carries the full truth; absent
/// count means truncation cannot be established (floor-only, as today).
pub(crate) fn sibling_record_truncated(record: &PresenceRecord) -> bool {
    let floor = record.holdouts.as_ref().map_or(0, Vec::len);
    record
        .session_count
        .is_some_and(|count| count as usize > floor)
}

/// The fetched uncapped wait set for a draining live sibling, or `None`
/// when the record is not truncated (the floor is already whole), no
/// admitted fetch is in hand, or the last one aged out. Schedules a
/// background refresh on the ambient runtime when due. The CALLER holds
/// the liveness gate: only invoke for a record that reads `draining`
/// with its boot lock probed HELD at serve time.
pub(crate) fn resolve_sibling_wait_set(
    state_root: &Path,
    record: &PresenceRecord,
) -> Option<Vec<DrainHoldout>> {
    if !sibling_record_truncated(record) {
        return None;
    }
    let key = (state_root.to_path_buf(), record.boot_id.clone());
    let mut cache = match cache().lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    let entry = cache.entry(key).or_default();
    let fresh = entry
        .rows
        .as_ref()
        .is_some_and(|(_, at)| at.elapsed() < REFRESH_INTERVAL);
    let attempted_recently = entry
        .last_attempt
        .is_some_and(|at| at.elapsed() < REFRESH_INTERVAL);
    if !fresh && !entry.in_flight && !attempted_recently {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            entry.in_flight = true;
            entry.last_attempt = Some(Instant::now());
            handle.spawn(fetch_sibling_wait_set(
                state_root.to_path_buf(),
                record.boot_id.clone(),
                record.port,
            ));
        }
    }
    entry
        .rows
        .as_ref()
        .filter(|(_, at)| at.elapsed() <= SERVE_MAX_AGE)
        .map(|(rows, _)| rows.clone())
}

/// Union: the presence floor verbatim (it wins on per-session conflict
/// — it is the disk-fresh copy rewritten per supervisor event), plus
/// every fetched session the cap hid, in the ONE holdout ordering rule
/// (parked rows first, earliest reset leading).
pub(crate) fn widen_holdout_rows(
    floor: &[DrainHoldout],
    fetched: &[DrainHoldout],
) -> Vec<DrainHoldout> {
    let mut rows: Vec<DrainHoldout> = floor.to_vec();
    let seen: HashSet<&str> = floor.iter().map(|row| row.session_id.as_str()).collect();
    rows.extend(
        fetched
            .iter()
            .filter(|row| !seen.contains(row.session_id.as_str()))
            .cloned(),
    );
    sort_drain_holdout_rows(&mut rows);
    rows
}

/// The background doorway fetch. Bounded and infallible: every failure
/// path just leaves the cache as it was (the consumers keep serving the
/// presence floor) and the retry beat tries again.
async fn fetch_sibling_wait_set(state_root: PathBuf, boot_id: String, port: u16) {
    let rows = fetch_uncapped_holdouts(&state_root, &boot_id, port).await;
    let changed = {
        let mut cache = match cache().lock() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = cache.entry((state_root, boot_id)).or_default();
        entry.in_flight = false;
        match rows {
            Some(rows) => {
                let changed = entry.rows.as_ref().map(|(prior, _)| prior) != Some(&rows);
                entry.rows = Some((rows, Instant::now()));
                changed
            }
            // Failure: keep any prior rows (stale-while-revalidate up to
            // SERVE_MAX_AGE); the floor is the fallback after that.
            None => false,
        }
    };
    if changed {
        // The list responses are cached ~30s; a landed (or moved) wait
        // set must reach the grid on its next poll, not a TTL later.
        crate::web_gateway::invalidate_session_list_response_caches();
    }
}

/// GET the sibling's `/api/daemon/handover` over the loopback doorway
/// and admit its uncapped `holdouts` block. `None` = do not admit
/// (unreachable, refused, wrong daemon, not draining, unparseable).
async fn fetch_uncapped_holdouts(
    state_root: &Path,
    boot_id: &str,
    port: u16,
) -> Option<Vec<DrainHoldout>> {
    // Re-check the liveness gate from disk before any network: a dead
    // or no-longer-draining drainer is never fetched, however stale the
    // scheduling snapshot was.
    let still_draining = super::read_presence_records(state_root)
        .into_iter()
        .any(|record| record.boot_id == boot_id && record.state == "draining");
    if !still_draining || !super::boot_id_is_live(state_root, boot_id) {
        return None;
    }
    // The sibling doorway: the drainer's scheme sidecar + per-port
    // admission token from the shared state root — the same same-user
    // trust class the takeover POST rides.
    let scheme = std::fs::read_to_string(crate::loopback_token::loopback_sidecar_path(
        state_root, port,
    ))
    .ok()
    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    .and_then(|meta| meta.get("scheme")?.as_str().map(str::to_string))
    .unwrap_or_else(|| "http".to_string());
    let token =
        std::fs::read_to_string(crate::loopback_token::loopback_token_path(state_root, port))
            .map(|raw| raw.trim().to_string())
            .unwrap_or_default();
    // Loopback may serve the daemon's self-signed TLS cert: the per-port
    // admission token is the authority on this lane, not WebPKI.
    let client = reqwest::Client::builder()
        .no_proxy()
        .danger_accept_invalid_certs(true)
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    let response = client
        .get(format!("{scheme}://127.0.0.1:{port}/api/daemon/handover"))
        .header(crate::loopback_token::LOOPBACK_TOKEN_HEADER, token)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    // Admission: the answering daemon must BE the expected sibling and
    // self-report draining — a re-used port cannot inject rows.
    if body.get("boot_id").and_then(|value| value.as_str()) != Some(boot_id)
        || body.get("draining").and_then(|value| value.as_bool()) != Some(true)
    {
        return None;
    }
    serde_json::from_value::<Vec<DrainHoldout>>(body.get("holdouts")?.clone()).ok()
}

/// Test seam: plant an admitted fetch result as if the doorway fetch
/// just landed. Keyed like production — a fixture home's seed can never
/// bleed into another test's.
#[cfg(test)]
pub(crate) fn seed_for_tests(state_root: &Path, boot_id: &str, rows: Vec<DrainHoldout>) {
    let mut cache = match cache().lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    cache.insert(
        (state_root.to_path_buf(), boot_id.to_string()),
        Entry {
            rows: Some((rows, Instant::now())),
            last_attempt: Some(Instant::now()),
            in_flight: false,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(session_count: Option<u64>, holdout_rows: usize) -> PresenceRecord {
        serde_json::from_value(serde_json::json!({
            "v": 1,
            "boot_id": "boot-x",
            "pid": std::process::id() + 1,
            "port": 8899,
            "version": {"pkg": "0.0.0", "git_sha": "x", "built_at": "x"},
            "state": "draining",
            "session_count": session_count,
            "holdouts": (0..holdout_rows).map(|n| serde_json::json!({
                "session_id": format!("sess-{n:02}"),
                "source": "intendant",
                "phase": "idle",
            })).collect::<Vec<_>>(),
            "updated_ms": 5_000,
        }))
        .expect("fixture record")
    }

    #[test]
    fn truncation_reads_count_against_named_rows() {
        // Count exceeds the named rows: the cap (or a pre-holdouts-era
        // record) hid sessions.
        assert!(sibling_record_truncated(&record(Some(18), 16)));
        assert!(sibling_record_truncated(&record(Some(2), 0)));
        // Whole floor: nothing to widen.
        assert!(!sibling_record_truncated(&record(Some(16), 16)));
        assert!(!sibling_record_truncated(&record(Some(3), 3)));
        // No count: truncation cannot be established — floor-only.
        assert!(!sibling_record_truncated(&record(None, 16)));
    }

    #[test]
    fn untruncated_record_never_consults_or_schedules() {
        let root = tempfile::tempdir().unwrap();
        // Even with rows seeded under this key, a whole floor asks for
        // no widening at all.
        seed_for_tests(root.path(), "boot-x", vec![holdout("sess-99", "running")]);
        assert_eq!(
            resolve_sibling_wait_set(root.path(), &record(Some(3), 3)),
            None
        );
    }

    #[test]
    fn truncated_record_serves_seeded_rows_and_fails_open_without_them() {
        let root = tempfile::tempdir().unwrap();
        // Fail-open: truncated but nothing fetched (and no runtime here
        // to schedule one) — the consumer keeps the presence floor.
        assert_eq!(
            resolve_sibling_wait_set(root.path(), &record(Some(18), 16)),
            None
        );
        let full: Vec<DrainHoldout> = (0..18)
            .map(|n| holdout(&format!("sess-{n:02}"), "idle"))
            .collect();
        seed_for_tests(root.path(), "boot-x", full.clone());
        assert_eq!(
            resolve_sibling_wait_set(root.path(), &record(Some(18), 16)),
            Some(full),
        );
        // Another home's identical boot id sees nothing — the key is
        // (state root, boot id).
        let other = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_sibling_wait_set(other.path(), &record(Some(18), 16)),
            None
        );
    }

    fn holdout(session_id: &str, phase: &str) -> DrainHoldout {
        DrainHoldout {
            session_id: session_id.to_string(),
            source: "intendant".to_string(),
            name: None,
            phase: phase.to_string(),
            limit_park: None,
            bg_park: None,
        }
    }

    #[test]
    fn widen_keeps_floor_rows_verbatim_and_adds_hidden_sessions() {
        let mut parked = holdout("sess-b", "waiting_rate_limit");
        parked.limit_park = Some(crate::session_log::SessionLimitParkMeta {
            resets_at_epoch: Some(1_754_000_000),
            has_pending: true,
        });
        let floor = vec![holdout("sess-a", "running"), holdout("sess-c", "idle")];
        // The fetched copy disagrees about sess-a's phase (it is a few
        // seconds staler than the per-event presence mirror): the floor
        // wins. sess-b and sess-d are the cap-hidden additions.
        let fetched = vec![
            holdout("sess-a", "idle"),
            parked.clone(),
            holdout("sess-d", "running"),
        ];
        let widened = widen_holdout_rows(&floor, &fetched);
        assert_eq!(
            widened
                .iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            // The one ordering rule: parked first, then id order.
            vec!["sess-b", "sess-a", "sess-c", "sess-d"],
        );
        assert_eq!(
            widened
                .iter()
                .find(|row| row.session_id == "sess-a")
                .map(|row| row.phase.as_str()),
            Some("running"),
            "the presence floor wins on per-session conflict"
        );
        // Degenerate shapes stay honest.
        assert_eq!(widen_holdout_rows(&[], &[]), Vec::<DrainHoldout>::new());
        assert_eq!(
            widen_holdout_rows(&floor, &[])
                .iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["sess-a", "sess-c"],
        );
    }
}

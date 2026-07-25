//! The render-time join (Track PR ruling 7): live PR state served
//! BESIDE the agenda — never stored on items, never an op, never
//! recomputed on list render. Two tiers, two disciplines:
//!
//! - **Tier 1** — what the scanner's list poll already returned (open
//!   state, draft flag, live title, branches, author, `updated_at`) —
//!   cached in memory by the scanner and served as a `pull_requests`
//!   sibling map on the agenda snapshot response, keyed by the anchors'
//!   url-ref locators (the `agenda_sessions_join` shape: the item DTO
//!   stays the pure fold product; a locator with no entry claims
//!   nothing). Pure memory read at render; a 304 poll refreshes the
//!   timestamps because the validator proved nothing changed.
//! - **Tier 2** — checks, reviews, mergeability — fetched through on
//!   card expand only: single-flight per PR (N open dashboards, one
//!   GitHub exchange), a freshness floor so re-expands within it serve
//!   the cache, bounded retention. Absent data claims nothing: every
//!   failure degrades to `unavailable`, the card never errors.
//!
//! The scanner loop owns the one GitHub client and publishes it here;
//! with no client published (integration unconfigured/paused) both
//! tiers serve honestly empty.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use super::client::{ChecksSummary, GithubAppClient, PrSummary, ReviewSummary};

/// Tier-2 entries younger than this are served without a re-fetch; a
/// re-expand inside the floor costs nothing.
const TIER2_FRESHNESS_FLOOR_MS: u64 = 60_000;
/// Bounded retention for the tier-2 cache (anchors come and go).
const TIER2_MAX_ENTRIES: usize = 256;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One PR's tier-1 state as last served by the list poll.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Tier1State {
    pub(crate) title: String,
    pub(crate) draft: bool,
    pub(crate) author: String,
    pub(crate) base: String,
    pub(crate) head: String,
    pub(crate) updated_at: Option<String>,
    /// When the daemon last *confirmed* this state (a 304 confirms).
    pub(crate) fetched_at_ms: u64,
}

/// The tier-1 cache: locator → state. Core ops on the struct so tests
/// drive local instances; the process global is the transport edge
/// (the `background_tasks` split).
#[derive(Default)]
pub(crate) struct Tier1Cache {
    entries: RwLock<HashMap<String, Tier1State>>,
}

impl Tier1Cache {
    /// Replace one repo's entries from a fresh list read. Locators not
    /// in the fresh set are dropped for that repo — a PR that left the
    /// open set has no tier-1 "open" state to claim.
    pub(crate) fn update_repo(&self, repo: &str, open: &[PrSummary]) {
        let prefix = format!("https://github.com/{repo}/pull/");
        let now = now_ms();
        let mut entries = self.entries.write().expect("tier1 poisoned");
        entries.retain(|locator, _| !locator.starts_with(&prefix));
        for pr in open {
            entries.insert(
                pr.html_url.clone(),
                Tier1State {
                    title: pr.title.clone(),
                    draft: pr.draft,
                    author: pr.user.login.clone(),
                    base: pr.base.branch.clone(),
                    head: pr.head.branch.clone(),
                    updated_at: pr.updated_at.clone(),
                    fetched_at_ms: now,
                },
            );
        }
    }

    /// A 304 confirmed the repo's set is unchanged: the cached states
    /// are current as of now — refresh their confirmation stamp.
    pub(crate) fn touch_repo(&self, repo: &str) {
        let prefix = format!("https://github.com/{repo}/pull/");
        let now = now_ms();
        let mut entries = self.entries.write().expect("tier1 poisoned");
        for (locator, state) in entries.iter_mut() {
            if locator.starts_with(&prefix) {
                state.fetched_at_ms = now;
            }
        }
    }

    /// The sibling-map rows for the locators a snapshot response is
    /// about to serve. Missing locators simply have no row.
    pub(crate) fn for_locators<'a>(
        &self,
        locators: impl Iterator<Item = &'a str>,
    ) -> serde_json::Map<String, serde_json::Value> {
        let entries = self.entries.read().expect("tier1 poisoned");
        let mut map = serde_json::Map::new();
        for locator in locators {
            if let Some(state) = entries.get(locator) {
                if let Ok(value) = serde_json::to_value(state) {
                    map.insert(locator.to_string(), value);
                }
            }
        }
        map
    }
}

/// The process-global tier-1 cache (transport edges only; the scanner
/// and tests hold `&Tier1Cache`).
pub(crate) fn tier1() -> &'static Tier1Cache {
    static CACHE: OnceLock<Tier1Cache> = OnceLock::new();
    CACHE.get_or_init(Tier1Cache::default)
}

/// The scanner publishes its client here whenever it (re)builds; `None`
/// while the integration is unconfigured or paused. Tier-2 reads borrow
/// it — the scanner stays the one client owner, one rate budget.
pub(crate) fn published_client() -> &'static RwLock<Option<Arc<GithubAppClient>>> {
    static CLIENT: OnceLock<RwLock<Option<Arc<GithubAppClient>>>> = OnceLock::new();
    CLIENT.get_or_init(|| RwLock::new(None))
}

pub(crate) fn publish_client(client: Option<Arc<GithubAppClient>>) {
    *published_client().write().expect("client slot poisoned") = client;
}

/// One PR's tier-2 state as last fetched on expand.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Tier2State {
    pub(crate) pr_state: Option<String>,
    pub(crate) merged: bool,
    pub(crate) draft: bool,
    pub(crate) title: Option<String>,
    /// `null` while GitHub is still computing mergeability.
    pub(crate) mergeable: Option<bool>,
    pub(crate) checks: ChecksSummary,
    pub(crate) review: ReviewSummary,
    pub(crate) fetched_at_ms: u64,
}

struct Tier2Entry {
    state: Tier2State,
}

/// Tier-2 fetch-through cache with per-locator single-flight: the lock
/// map hands one fetcher the keyhole; concurrent expanders await the
/// same exchange (the `replay_cache` discipline).
#[derive(Default)]
pub(crate) struct Tier2Cache {
    entries: Mutex<HashMap<String, Tier2Entry>>,
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Tier2Cache {
    fn fresh(&self, locator: &str) -> Option<Tier2State> {
        let entries = self.entries.lock().expect("tier2 poisoned");
        entries.get(locator).and_then(|entry| {
            (now_ms().saturating_sub(entry.state.fetched_at_ms) < TIER2_FRESHNESS_FLOOR_MS)
                .then(|| entry.state.clone())
        })
    }

    fn store(&self, locator: &str, state: Tier2State) {
        let mut entries = self.entries.lock().expect("tier2 poisoned");
        if entries.len() >= TIER2_MAX_ENTRIES && !entries.contains_key(locator) {
            // Bounded retention: drop the stalest entry.
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.state.fetched_at_ms)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(locator.to_string(), Tier2Entry { state });
    }

    fn flight(&self, locator: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut flights = self.flights.lock().expect("tier2 flights poisoned");
        flights
            .entry(locator.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// The expand-time read: fresh cache → serve; otherwise one flight
    /// fetches (detail + checks + reviews) while concurrent expanders
    /// wait and re-read. Any failure serves `None` — the caller renders
    /// "state unavailable", never an error.
    pub(crate) async fn fetch_through(
        &self,
        client: &GithubAppClient,
        repo: &str,
        number: u64,
        locator: &str,
    ) -> Option<Tier2State> {
        if let Some(state) = self.fresh(locator) {
            return Some(state);
        }
        let flight = self.flight(locator);
        let _keyhole = flight.lock().await;
        if let Some(state) = self.fresh(locator) {
            return Some(state);
        }
        let detail = client.get_pull(repo, number).await.ok()?;
        let checks = match detail.head.as_ref().and_then(|h| h.sha.as_deref()) {
            Some(sha) => client
                .check_runs_summary(repo, sha)
                .await
                .unwrap_or_default(),
            None => ChecksSummary::default(),
        };
        let review = client
            .reviews_summary(repo, number)
            .await
            .unwrap_or_default();
        let state = Tier2State {
            pr_state: detail.state.clone(),
            merged: detail.merged,
            draft: detail.draft,
            title: detail.title.clone(),
            mergeable: detail.mergeable,
            checks,
            review,
            fetched_at_ms: now_ms(),
        };
        self.store(locator, state.clone());
        Some(state)
    }
}

pub(crate) fn tier2() -> &'static Tier2Cache {
    static CACHE: OnceLock<Tier2Cache> = OnceLock::new();
    CACHE.get_or_init(Tier2Cache::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(number: u64, title: &str, draft: bool) -> PrSummary {
        serde_json::from_value(crate::github_pr::client::test_fixture::pull(
            number, title, draft,
        ))
        .unwrap()
    }

    #[test]
    fn tier1_updates_touches_and_serves_by_locator() {
        let cache = Tier1Cache::default();
        cache.update_repo("o/r", &[summary(1, "one", false), summary(2, "two", true)]);
        let map = cache.for_locators(
            [
                "https://github.com/o/r/pull/2",
                "https://github.com/o/r/pull/9",
            ]
            .into_iter(),
        );
        assert_eq!(map.len(), 1, "unknown locators claim nothing");
        assert_eq!(map["https://github.com/o/r/pull/2"]["draft"], true);
        assert_eq!(map["https://github.com/o/r/pull/2"]["title"], "two");

        // A fresh list without PR 2 drops its open-state row.
        cache.update_repo("o/r", &[summary(1, "one", false)]);
        let map = cache.for_locators(["https://github.com/o/r/pull/2"].into_iter());
        assert!(map.is_empty());

        // Another repo's rows are untouched by o/r updates (the fixture
        // helper hardcodes o/r urls, so build the x/y row by hand).
        let other: PrSummary = serde_json::from_value(serde_json::json!({
            "number": 7,
            "title": "other",
            "draft": false,
            "html_url": "https://github.com/x/y/pull/7",
            "user": {"login": "octocat"},
            "head": {"ref": "f"},
            "base": {"ref": "main"},
        }))
        .unwrap();
        cache.update_repo("x/y", &[other]);
        cache.update_repo("o/r", &[]);
        let map = cache.for_locators(["https://github.com/x/y/pull/7"].into_iter());
        assert_eq!(map.len(), 1);
    }
}

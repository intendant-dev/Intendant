//! The landing shepherd: a daemon-side closed-loop watch over the
//! fleet's OPEN pull requests, waking the seat that owns a wedged
//! landing — or parking a needs-you agenda item when that seat is gone.
//!
//! The fleet's landing protection was prose, not machinery: CLAUDE.md
//! teaches that an armed auto-merge on a CONFLICTING/DIRTY pre-queue PR
//! parks forever (the PR #293 class, 2026-07-13), but a seat that ends
//! or dies after arming leaves NOTHING watching (PR #669, 2026-08-02:
//! limit-killed seconds after arming, caught by the owner hours later).
//! Seats end at queued+report by law — this shepherd is the fleet-wide
//! replacement for per-seat babysitting, not an addition to it.
//!
//! One bounded poll (the coordination radar's `gh` posture: silent
//! degrade when `gh` is absent or failing) observes every open PR whose
//! head branch exists in a supervised checkout's repository, classifies
//! transitions with a pure fold ([`ShepherdLedger`]), and reacts to
//! exactly four conditions:
//!
//! - **Conflict** — `mergeStateStatus: DIRTY` / `mergeable: CONFLICTING`
//!   on an armed or ready (open, non-draft) PR. Terminal at first sight:
//!   armed auto-merge survives a conflict and waits forever, so this
//!   fires on the first poll that sees it, no prior state needed.
//! - **QueueEjected** — the PR left the merge queue without merging
//!   (flaked or failed speculative check, manual dequeue).
//! - **Disarmed** — auto-merge switched off outside the queue. In-queue
//!   `autoMergeRequest` nulling is normal transit noise and never fires.
//! - **ArmedStall** — armed, checks settled (nothing pending), no queue
//!   entry for [`DEFAULT_STALL_SECS`]: the stuck-check-run class (a job
//!   that died mid-run and auto-recovered can leave its per-commit check
//!   run at `failure` while the workflow run shows success; auto-merge
//!   reads the check run and waits forever).
//!
//! Ownership derives from session records + `gh`, never a second
//! bookkeeping: the supervised-session registry's checkouts map to
//! branches through `git worktree list` (plus each live session's
//! durable `SessionMeta.worktree.branch` record), and a PR's
//! `headRefName` joins against them. A live owner (open follow-up
//! channel) is woken through the existing task lane —
//! `ControlMsg::StartTask { session_id }` routes the reconcile ritual as
//! a follow-up turn — and a gone owner parks ONE open needs-you agenda
//! item per PR (fold-deduplicated like the PR scanner's anchors, so a
//! daemon restart re-parks nothing).
//!
//! TRAP (pinned): observe-and-wake ONLY. The shepherd never merges,
//! never resolves, never re-arms — GitHub's queue stays the sole
//! authority; every wake and park carries the ritual text and stops
//! there. ONE shepherd per daemon ([`SHEPHERD_SPAWNED`]), and under the
//! scheduler lease (Track HS2) it acts on the holder only.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::agenda::{
    AgendaActor, AgendaCommand, AgendaHandle, AgendaItem, AgendaKind, AgendaRefSpec, AgendaRefType,
    AgendaStatus,
};
use crate::event::{AgentLaunchConfig, AppEvent, ControlMsg, EventBus};
use crate::session_vitals::{run_git, GIT_PROBE_TIMEOUT};

/// The shepherd's self-described lane label — on every op it writes,
/// beside (never inside) the `daemon` actor attribution.
pub(crate) const SHEPHERD_SOURCE: &str = "landing-shepherd";
/// Every parked item's find keys: the shepherd's own dedup tag plus the
/// attention tag the owner filters on.
pub(crate) const SHEPHERD_TAG: &str = "landing-shepherd";
pub(crate) const NEEDS_YOU_TAG: &str = "needs-you";

/// Default poll cadence. `[landing_shepherd] poll_secs` overrides
/// (floor 5 s); `INTENDANT_LANDING_SHEPHERD_POLL_MS` overrides both for
/// rigs (floor 250 ms so a typo cannot spin).
const DEFAULT_POLL_SECS: u64 = 60;
/// The green-armed-but-unqueued grace: how long an armed PR with
/// settled checks may sit outside the queue before the stall fires
/// (`[landing_shepherd] stall_secs` overrides, floor 30 s).
const DEFAULT_STALL_SECS: u64 = 300;
/// Re-alert pacing while ONE condition persists on one PR: the first
/// sight fires immediately, repeats wait this long. A condition that
/// clears and re-appears is a new edge and fires immediately again.
const ALERT_COOLDOWN_MS: u64 = 30 * 60 * 1000;
/// Idle recheck while disabled or not holding the scheduler lease.
const IDLE_RECHECK: Duration = Duration::from_secs(60);
/// Defensive parse bounds (the radar's shape).
const MAX_PRS: usize = 64;
const MAX_CHECKS_PER_PR: usize = 256;
/// Distinct repositories shepherded per tick (supervised sessions
/// rarely span more than one; the bound keeps a pathological registry
/// from turning the tick into a `gh` storm).
const MAX_REPOS: usize = 8;
/// Session-record reads per tick (one small JSON per live session).
const MAX_META_READS: usize = 64;

// ---------------------------------------------------------------------------
// Config edge
// ---------------------------------------------------------------------------

/// Effective on/off: the env kill switch outranks config in both
/// directions (`INTENDANT_LANDING_SHEPHERD=0` disables an enabled
/// config, `=1` re-enables a disabled one) — the `INTENDANT_BOOT_READOPT`
/// convention. Pure over the env VALUE so tests never mutate the
/// process environment.
pub(crate) fn shepherd_enabled(config_enabled: bool, env: Option<&str>) -> bool {
    match env {
        Some(raw) => !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => config_enabled,
    }
}

/// Effective poll interval from config seconds + the rig env override.
pub(crate) fn poll_interval(config_poll_secs: Option<u64>, env_ms: Option<&str>) -> Duration {
    if let Some(ms) = env_ms.and_then(|raw| raw.trim().parse::<u64>().ok()) {
        return Duration::from_millis(ms.max(250));
    }
    Duration::from_secs(config_poll_secs.unwrap_or(DEFAULT_POLL_SECS).max(5))
}

/// Effective stall grace in milliseconds.
pub(crate) fn stall_ms(config_stall_secs: Option<u64>) -> u64 {
    config_stall_secs.unwrap_or(DEFAULT_STALL_SECS).max(30) * 1000
}

// ---------------------------------------------------------------------------
// Observation parsing (pure; fixtures in tests, `gh` only at the edge)
// ---------------------------------------------------------------------------

/// One open PR as observed by one poll — the classifier's whole input
/// vocabulary. `queued` is joined from the repo merge-queue query at the
/// poll edge (queue presence is not a `gh pr list` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrObservation {
    pub number: u64,
    pub head_ref: String,
    pub title: String,
    pub draft: bool,
    /// `state == OPEN`. Merged/closed PRs are observed once so the fold
    /// can prune their state, and never alert.
    pub open: bool,
    /// `mergeStateStatus`, verbatim (`DIRTY`, `CLEAN`, `BLOCKED`, …).
    pub merge_state: String,
    /// `mergeable`, verbatim (`CONFLICTING`, `MERGEABLE`, `UNKNOWN`).
    pub mergeable: String,
    /// `autoMergeRequest != null`.
    pub armed: bool,
    /// The PR has a merge-queue entry right now.
    pub queued: bool,
    /// Check runs / status contexts in a failed bucket.
    pub checks_failing: u32,
    /// Check runs / status contexts still pending (queued, in progress,
    /// expected). `EXPECTED` counts as pending on purpose: a required
    /// check that has not reported keeps the stall timer from starting.
    pub checks_pending: u32,
}

impl PrObservation {
    pub(crate) fn conflicted(&self) -> bool {
        self.merge_state == "DIRTY" || self.mergeable == "CONFLICTING"
    }

    fn checks_settled(&self) -> bool {
        self.checks_pending == 0
    }
}

/// Parse `gh pr list --json number,title,headRefName,isDraft,state,
/// mergeStateStatus,mergeable,autoMergeRequest,statusCheckRollup`.
/// `None` on shape trouble — the caller skips the repo this tick rather
/// than classify against a half-read.
pub(crate) fn parse_pr_list(bytes: &[u8]) -> Option<Vec<PrObservation>> {
    #[derive(serde::Deserialize)]
    struct GhCheck {
        #[serde(default)]
        status: String,
        #[serde(default)]
        conclusion: String,
        #[serde(default)]
        state: String,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct GhPr {
        number: u64,
        #[serde(default)]
        title: String,
        #[serde(default)]
        head_ref_name: String,
        #[serde(default)]
        is_draft: bool,
        #[serde(default)]
        state: String,
        #[serde(default)]
        merge_state_status: String,
        #[serde(default)]
        mergeable: String,
        #[serde(default)]
        auto_merge_request: Option<serde_json::Value>,
        #[serde(default)]
        status_check_rollup: Vec<GhCheck>,
    }
    let prs: Vec<GhPr> = serde_json::from_slice(bytes).ok()?;
    Some(
        prs.into_iter()
            .take(MAX_PRS)
            .map(|pr| {
                let mut failing = 0u32;
                let mut pending = 0u32;
                for check in pr.status_check_rollup.iter().take(MAX_CHECKS_PER_PR) {
                    // CheckRun rows carry status/conclusion; StatusContext
                    // rows carry state. Anything not clearly settled-green
                    // is either failing or pending — never silently green.
                    let conclusion = check.conclusion.to_ascii_uppercase();
                    let status = check.status.to_ascii_uppercase();
                    let state = check.state.to_ascii_uppercase();
                    if matches!(
                        conclusion.as_str(),
                        "FAILURE"
                            | "TIMED_OUT"
                            | "CANCELLED"
                            | "ACTION_REQUIRED"
                            | "STARTUP_FAILURE"
                    ) || matches!(state.as_str(), "FAILURE" | "ERROR")
                    {
                        failing += 1;
                    } else if (!status.is_empty() && status != "COMPLETED")
                        || matches!(state.as_str(), "PENDING" | "EXPECTED")
                    {
                        pending += 1;
                    }
                }
                PrObservation {
                    number: pr.number,
                    title: pr.title,
                    head_ref: pr.head_ref_name,
                    draft: pr.is_draft,
                    open: pr.state.eq_ignore_ascii_case("OPEN"),
                    merge_state: pr.merge_state_status.to_ascii_uppercase(),
                    mergeable: pr.mergeable.to_ascii_uppercase(),
                    armed: pr
                        .auto_merge_request
                        .as_ref()
                        .is_some_and(|value| !value.is_null()),
                    queued: false, // joined from the queue query by the caller
                    checks_failing: failing,
                    checks_pending: pending,
                }
            })
            .collect(),
    )
}

/// Parse the repo-wide merge-queue GraphQL response into the queued PR
/// numbers. A repository without a merge queue reads as an empty set
/// (`mergeQueue: null`); shape trouble is `None` and the caller skips
/// the repo this tick — ejection/stall classes must never fire against
/// an unknown queue truth.
pub(crate) fn parse_queue_numbers(bytes: &[u8]) -> Option<BTreeSet<u64>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if value.get("errors").is_some_and(|errors| !errors.is_null()) {
        return None;
    }
    let queue = value.pointer("/data/repository/mergeQueue")?;
    if queue.is_null() {
        return Some(BTreeSet::new());
    }
    let nodes = queue.pointer("/entries/nodes")?.as_array()?;
    Some(
        nodes
            .iter()
            .filter_map(|node| node.pointer("/pullRequest/number")?.as_u64())
            .collect(),
    )
}

/// Parse `gh repo view --json nameWithOwner` into the `owner/name` slug.
pub(crate) fn parse_name_with_owner(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let slug = value.get("nameWithOwner")?.as_str()?.trim();
    let (owner, name) = slug.split_once('/')?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(slug.to_string())
}

/// Parse `git worktree list --porcelain` into (checkout path, branch)
/// rows. Detached checkouts carry no branch and drop out.
pub(crate) fn parse_worktree_branches(bytes: &[u8]) -> Vec<(PathBuf, String)> {
    let text = String::from_utf8_lossy(bytes);
    let mut rows = Vec::new();
    let mut current: Option<PathBuf> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(path));
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(path) = current.clone() {
                let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
                if !short.is_empty() {
                    rows.push((path, short.to_string()));
                }
            }
        } else if line.is_empty() {
            current = None;
        }
    }
    rows
}

/// Parse `git for-each-ref --format=%(refname:short) refs/heads` into
/// the local branch set — the shepherd's scope gate: a PR whose head
/// branch exists locally was authored from this machine.
pub(crate) fn parse_local_branches(bytes: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// The transition classifier (pure fold)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum AlertClass {
    /// DIRTY/CONFLICTING on an armed or ready PR — terminal at first
    /// sight, no prior observation needed.
    Conflict,
    /// Left the merge queue without merging.
    QueueEjected,
    /// Auto-merge disarmed outside the queue.
    Disarmed,
    /// Armed, checks settled, no queue entry past the stall grace.
    ArmedStall,
}

impl AlertClass {
    pub(crate) fn label(self) -> &'static str {
        match self {
            AlertClass::Conflict => "conflicting while armed/ready",
            AlertClass::QueueEjected => "ejected from the merge queue",
            AlertClass::Disarmed => "auto-merge disarmed",
            AlertClass::ArmedStall => "armed but never queued",
        }
    }
}

/// One condition on one PR that needs its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrAlert {
    pub class: AlertClass,
    pub observation: PrObservation,
}

/// Per-PR fold state: previous observations for the edge classes,
/// stall clocks, and the raised/cooldown ledger that keeps a persisting
/// condition from re-waking every tick. Keys carry the repo slug so one
/// shepherd folds every watched repository.
#[derive(Default)]
pub(crate) struct ShepherdLedger {
    prev: HashMap<(String, u64), PrObservation>,
    stall_since: HashMap<(String, u64), u64>,
    /// (slug, number, class) → raised (condition held at last tick).
    active: HashSet<(String, u64, AlertClass)>,
    last_alert_ms: HashMap<(String, u64, AlertClass), u64>,
}

impl ShepherdLedger {
    /// Fold one repo's full observation set for one tick and return the
    /// alerts to deliver now. `observed` must be the COMPLETE set for
    /// `slug` this tick: a PR absent from it (merged, closed, foreign
    /// again) is pruned, so a later reappearance starts clean.
    pub(crate) fn fold(
        &mut self,
        slug: &str,
        observed: &[PrObservation],
        now_ms: u64,
        stall_grace_ms: u64,
    ) -> Vec<PrAlert> {
        let mut alerts = Vec::new();
        let seen: HashSet<u64> = observed.iter().map(|pr| pr.number).collect();
        self.prune(slug, &seen);

        for pr in observed {
            let key = (slug.to_string(), pr.number);
            let (prev_queued, prev_armed_outside_queue) = self
                .prev
                .get(&key)
                .map(|p| (p.queued, p.armed && !p.queued))
                .unwrap_or((false, false));
            if !pr.open {
                // Terminal on GitHub: nothing to shepherd, state pruned
                // below by the next tick's absence (merged PRs usually
                // leave the open list immediately; belt and braces).
                continue;
            }

            // Level condition: Conflict — first sight fires.
            let conflict = !pr.draft && pr.conflicted();
            if let Some(alert) =
                self.level_condition(&key, AlertClass::Conflict, conflict, now_ms, pr)
            {
                alerts.push(alert);
            }

            // Edge condition: QueueEjected — needs a prior in-queue
            // observation; still-open is the "without merging" half.
            let ejected = prev_queued && !pr.queued;
            // Edge condition: Disarmed — armed→unarmed with the queue
            // out of the picture on BOTH sides (in-queue reads of
            // `armed` are transit noise, and an ejection usually also
            // disarms — the ejection is the stronger diagnosis).
            let disarmed = prev_armed_outside_queue && !pr.armed && !pr.queued && !ejected;
            for (class, hit) in [
                (AlertClass::QueueEjected, ejected),
                (AlertClass::Disarmed, disarmed),
            ] {
                if hit {
                    if let Some(alert) = self.edge_condition(&key, class, now_ms, pr) {
                        alerts.push(alert);
                    }
                }
            }

            // Level condition with a clock: ArmedStall.
            let stalled_shape =
                pr.armed && !pr.queued && !pr.draft && !pr.conflicted() && pr.checks_settled();
            if stalled_shape {
                let since = *self.stall_since.entry(key.clone()).or_insert(now_ms);
                let ripe = now_ms.saturating_sub(since) >= stall_grace_ms;
                if let Some(alert) =
                    self.level_condition(&key, AlertClass::ArmedStall, ripe, now_ms, pr)
                {
                    alerts.push(alert);
                }
            } else {
                self.stall_since.remove(&key);
                self.clear(&key, AlertClass::ArmedStall);
            }

            self.prev.insert(key, pr.clone());
        }
        alerts
    }

    /// Drop every ledger entry for `slug` PRs outside `seen`, and — via
    /// [`Self::retain_slugs`] — for repos that stopped being shepherded.
    fn prune(&mut self, slug: &str, seen: &HashSet<u64>) {
        self.prev.retain(|(s, n), _| s != slug || seen.contains(n));
        self.stall_since
            .retain(|(s, n), _| s != slug || seen.contains(n));
        self.active
            .retain(|(s, n, _)| s != slug || seen.contains(n));
        self.last_alert_ms
            .retain(|(s, n, _), _| s != slug || seen.contains(n));
    }

    /// Drop state for repos no longer in this tick's shepherded set —
    /// bounded memory even as sessions come and go across checkouts.
    pub(crate) fn retain_slugs(&mut self, live: &BTreeSet<String>) {
        self.prev.retain(|(s, _), _| live.contains(s));
        self.stall_since.retain(|(s, _), _| live.contains(s));
        self.active.retain(|(s, _, _)| live.contains(s));
        self.last_alert_ms.retain(|(s, _, _), _| live.contains(s));
    }

    /// A level condition (holds across ticks): fire on the raise edge,
    /// re-fire a persisting condition only past the cooldown, clear on
    /// release so the next raise is a fresh edge.
    fn level_condition(
        &mut self,
        key: &(String, u64),
        class: AlertClass,
        holds: bool,
        now_ms: u64,
        pr: &PrObservation,
    ) -> Option<PrAlert> {
        let id = (key.0.clone(), key.1, class);
        if !holds {
            self.clear(key, class);
            return None;
        }
        let raised = self.active.contains(&id);
        self.active.insert(id.clone());
        if raised && !self.cooldown_elapsed(&id, now_ms) {
            return None;
        }
        self.last_alert_ms.insert(id, now_ms);
        Some(PrAlert {
            class,
            observation: pr.clone(),
        })
    }

    /// An edge condition (a momentary transition): fires when observed,
    /// paced by the same per-(pr, class) cooldown so a flapping queue
    /// cannot wake the owner every tick.
    fn edge_condition(
        &mut self,
        key: &(String, u64),
        class: AlertClass,
        now_ms: u64,
        pr: &PrObservation,
    ) -> Option<PrAlert> {
        let id = (key.0.clone(), key.1, class);
        if !self.cooldown_elapsed(&id, now_ms) {
            return None;
        }
        self.last_alert_ms.insert(id, now_ms);
        Some(PrAlert {
            class,
            observation: pr.clone(),
        })
    }

    fn cooldown_elapsed(&self, id: &(String, u64, AlertClass), now_ms: u64) -> bool {
        self.last_alert_ms
            .get(id)
            .is_none_or(|last| now_ms.saturating_sub(*last) >= ALERT_COOLDOWN_MS)
    }

    fn clear(&mut self, key: &(String, u64), class: AlertClass) {
        let id = (key.0.clone(), key.1, class);
        self.active.remove(&id);
        self.last_alert_ms.remove(&id);
    }
}

// ---------------------------------------------------------------------------
// Wake and park text (pure)
// ---------------------------------------------------------------------------

/// The reconcile ritual every wake and park carries. Merge, never
/// rebase; app.html conflicts resolve in the fragments and regenerate.
fn reconcile_ritual(number: u64) -> String {
    format!(
        "Reconcile ritual: merge, never rebase — `git fetch origin && git merge origin/main`, \
         resolve conflicts at the source, rerun the local battery, push. A `static/app.html` \
         conflict is resolved in the `static/app/` fragments, then `cargo run -p \
         app-html-assembler` and `git add static/app.html` — never hand-edit the generated \
         artifact. Afterwards re-arm with `gh pr merge {number} --merge --auto` and confirm the \
         PR actually enters the merge queue. The shepherd observes and wakes only — it never \
         merges, resolves, or re-arms."
    )
}

fn condition_line(alert: &PrAlert) -> String {
    let pr = &alert.observation;
    match alert.class {
        AlertClass::Conflict => format!(
            "is CONFLICTING/DIRTY against main while {} — GitHub will never queue it, and an \
             armed auto-merge parks forever (the PR #293 class)",
            if pr.armed || pr.queued {
                "auto-merge is armed"
            } else {
                "ready (non-draft)"
            }
        ),
        AlertClass::QueueEjected => "left the merge queue WITHOUT merging — usually a failed or \
             flaked speculative group check, sometimes a manual dequeue. Inspect `gh pr checks`, \
             heal or rerun the failure"
            .to_string(),
        AlertClass::Disarmed => "had its auto-merge DISARMED outside the queue (check failure, \
             base change, or a manual disarm) — nothing will land it until it is re-armed"
            .to_string(),
        AlertClass::ArmedStall => format!(
            "is armed with settled checks ({} failing) but has NO merge-queue entry after the \
             stall grace — the stuck-check-run class: a job that died mid-run and auto-recovered \
             can leave its per-commit check run at `failure` while the workflow run shows \
             success, and auto-merge reads the check run. Compare `gh pr checks {}` against `gh \
             run view`; remint with `gh run rerun --job <id>`",
            pr.checks_failing, pr.number
        ),
    }
}

/// The follow-up text a woken owner receives.
pub(crate) fn wake_text(slug: &str, alert: &PrAlert) -> String {
    let pr = &alert.observation;
    format!(
        "[landing-shepherd] Your PR #{number} ({slug}, branch `{branch}`) {condition}. \
         {ritual}",
        number = pr.number,
        branch = pr.head_ref,
        condition = condition_line(alert),
        ritual = reconcile_ritual(pr.number),
    )
}

/// Parked-item title: bounded like the PR scanner's anchor titles.
pub(crate) fn park_title(alert: &PrAlert) -> String {
    let pr = &alert.observation;
    let mut title = format!(
        "Landing needs you: PR #{} {} — {}",
        pr.number,
        pr.title.trim(),
        alert.class.label()
    );
    if title.chars().count() > 500 {
        title = title.chars().take(499).collect::<String>() + "…";
    }
    title
}

/// Parked-item body: the condition, the gone-seat statement, the ritual.
pub(crate) fn park_body(slug: &str, alert: &PrAlert) -> String {
    let pr = &alert.observation;
    format!(
        "PR #{number} ({slug}, branch `{branch}`) {condition}.\n\nNo live session owns branch \
         `{branch}` on this daemon — the seat that armed it has ended (seats end at \
         queued+report; this parked item is the shepherd's fallback lane). Reconcile by hand or \
         commission a fresh seat.\n\n{ritual}",
        number = pr.number,
        branch = pr.head_ref,
        condition = condition_line(alert),
        ritual = reconcile_ritual(pr.number),
    )
}

// ---------------------------------------------------------------------------
// Ownership (session records + gh; no second bookkeeping)
// ---------------------------------------------------------------------------

/// branch → owning session ids (sorted, deduped), from the two session
/// records the daemon already keeps: supervised checkouts joined
/// through `git worktree list` rows, and live sessions' durable
/// `SessionMeta.worktree.branch`. Pure over injected rows.
pub(crate) fn branch_owners(
    session_roots: &[(String, PathBuf)],
    worktree_branches: &[(PathBuf, String)],
    meta_branches: &[(String, String)],
) -> BTreeMap<String, Vec<String>> {
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (session_id, root) in session_roots {
        // Longest-prefix match: a session rooted in a subdirectory of a
        // worktree belongs to that worktree's branch.
        let branch = worktree_branches
            .iter()
            .filter(|(path, _)| root.starts_with(path))
            .max_by_key(|(path, _)| path.as_os_str().len())
            .map(|(_, branch)| branch.clone());
        if let Some(branch) = branch {
            owners.entry(branch).or_default().push(session_id.clone());
        }
    }
    for (session_id, branch) in meta_branches {
        owners
            .entry(branch.clone())
            .or_default()
            .push(session_id.clone());
    }
    for sessions in owners.values_mut() {
        sessions.sort();
        sessions.dedup();
    }
    owners
}

/// The live session to wake for a PR's head branch: the first (stable
/// order) owner whose follow-up channel is still open. `None` = the
/// seat is gone and the park lane takes it.
pub(crate) fn owning_live_session(
    owners: &BTreeMap<String, Vec<String>>,
    head_ref: &str,
    live: &HashSet<String>,
) -> Option<String> {
    owners
        .get(head_ref)?
        .iter()
        .find(|id| live.contains(*id))
        .cloned()
}

/// Open shepherd-parked items per PR number for `slug`, reconstructed
/// from the agenda fold (the PR scanner's anchor pattern): the fold is
/// the park lane's only durable dedup state, so a restart re-parks
/// nothing while an item stays open, and a completed/retired item
/// re-parks only if the condition still holds.
pub(crate) fn open_shepherd_prs(snapshot: &[AgendaItem], slug: &str) -> HashSet<u64> {
    let mut open = HashSet::new();
    for item in snapshot {
        if item.status != AgendaStatus::Open
            || item.provenance.kind.as_deref() != Some("daemon")
            || item.provenance.source.as_deref() != Some(SHEPHERD_SOURCE)
        {
            continue;
        }
        let Some((item_repo, number)) = item
            .refs
            .iter()
            .find_map(|r| crate::github_pr::scanner::parse_pr_locator(&r.locator))
        else {
            continue;
        };
        if item_repo == slug {
            open.insert(number);
        }
    }
    open
}

// ---------------------------------------------------------------------------
// Delivery: wake the owner, or park the needs-you item
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delivery {
    Woke {
        session_id: String,
    },
    Parked {
        item_id: String,
    },
    /// An open shepherd item already summons the owner for this PR.
    AlreadyParked,
    ParkFailed {
        error: String,
    },
}

/// Deliver one alert. The caller resolves the owner (so this stays
/// hermetically testable): a live owner is woken through the existing
/// task lane — `StartTask { session_id }` routes the ritual as a
/// follow-up turn in that session — and a gone owner parks one open
/// needs-you item per PR.
pub(crate) fn deliver_alert(
    agenda: &AgendaHandle,
    bus: &EventBus,
    slug: &str,
    alert: &PrAlert,
    owner: Option<&str>,
    already_parked: &HashSet<u64>,
) -> Delivery {
    if let Some(session_id) = owner {
        bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
            session_id: Some(session_id.to_string()),
            task: wake_text(slug, alert),
            orchestrate: None,
            direct: None,
            project_root: None,
            reference_frame_ids: Vec::new(),
            display_target: None,
            attachments: Vec::new(),
            follow_up_id: None,
            delegation_id: None,
            session_name: None,
            launch_config: AgentLaunchConfig::default(),
        }));
        return Delivery::Woke {
            session_id: session_id.to_string(),
        };
    }
    if already_parked.contains(&alert.observation.number) {
        return Delivery::AlreadyParked;
    }
    let parked = agenda.apply(
        AgendaCommand::Add {
            kind: AgendaKind::Task,
            title: park_title(alert),
            body: park_body(slug, alert),
            tags: vec![SHEPHERD_TAG.to_string(), NEEDS_YOU_TAG.to_string()],
            due_ms: None,
            source: Some(SHEPHERD_SOURCE.to_string()),
            refs: vec![AgendaRefSpec {
                ref_type: AgendaRefType::Url,
                locator: format!(
                    "https://github.com/{slug}/pull/{}",
                    alert.observation.number
                ),
                must_read: false,
                label: None,
            }],
        },
        Some(AgendaActor::daemon()),
    );
    match parked {
        Ok(item) => Delivery::Parked { item_id: item.id },
        Err(error) => Delivery::ParkFailed {
            error: error.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// The poll edge (gh + git subprocesses; everything above stays pure)
// ---------------------------------------------------------------------------

/// One shepherded repository this tick: the checkout the `gh` calls run
/// in, its resolved slug, and the sessions rooted in it.
struct RepoGroup {
    toplevel: PathBuf,
    session_roots: Vec<(String, PathBuf)>,
}

/// Group supervised session roots by their main-repository toplevel
/// (worktrees join their main checkout, the coordination space's
/// normalization), seeded with the daemon's own project root so a
/// daemon with no live sessions still shepherds its repo's parked PRs.
async fn collect_repo_groups(
    project_root: Option<&Path>,
    session_roots: &[(String, PathBuf)],
    toplevel_cache: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> BTreeMap<PathBuf, RepoGroup> {
    let mut groups: BTreeMap<PathBuf, RepoGroup> = BTreeMap::new();
    let seed = project_root.map(|root| ("".to_string(), root.to_path_buf()));
    for (session_id, root) in seed.iter().chain(session_roots.iter()) {
        let toplevel = match toplevel_cache.get(root) {
            Some(cached) => cached.clone(),
            None => {
                let resolved = main_repo_toplevel(root).await;
                if toplevel_cache.len() > 256 {
                    toplevel_cache.clear();
                }
                toplevel_cache.insert(root.clone(), resolved.clone());
                resolved
            }
        };
        let Some(toplevel) = toplevel else {
            continue; // not a checkout: nothing to shepherd for it
        };
        let group = groups.entry(toplevel.clone()).or_insert_with(|| RepoGroup {
            toplevel,
            session_roots: Vec::new(),
        });
        if !session_id.is_empty() {
            group.session_roots.push((session_id.clone(), root.clone()));
        }
    }
    groups
}

/// A checkout's MAIN repository toplevel: the parent of its git common
/// dir, so linked worktrees group with the repository whose branches
/// and PRs they share.
async fn main_repo_toplevel(root: &Path) -> Option<PathBuf> {
    let out = run_git(
        "git".as_ref(),
        GIT_PROBE_TIMEOUT,
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await
    .filter(|out| out.status.success())?;
    let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    common.parent().map(Path::to_path_buf)
}

/// `gh` under the radar's anti-wedge guards; `None` on any trouble —
/// the tick skips the repo rather than classify a half-read.
async fn run_gh(cwd: &Path, args: &[&str]) -> Option<std::process::Output> {
    let output = tokio::process::Command::new("gh")
        .current_dir(cwd)
        .args(args)
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(GIT_PROBE_TIMEOUT, output)
        .await
        .ok()?
        .ok()
        .filter(|out| out.status.success())
}

/// One repo's full poll: PR list + queue join + ownership rows. `None`
/// skips the repo this tick (gh absent/failing, queue unreadable).
struct RepoPoll {
    slug: String,
    observations: Vec<PrObservation>,
    owners: BTreeMap<String, Vec<String>>,
}

async fn poll_repo(
    group: &RepoGroup,
    slug_cache: &mut HashMap<PathBuf, String>,
    home: &Path,
) -> Option<RepoPoll> {
    let toplevel = &group.toplevel;
    let slug = match slug_cache.get(toplevel) {
        Some(slug) => slug.clone(),
        None => {
            let out = run_gh(toplevel, &["repo", "view", "--json", "nameWithOwner"]).await?;
            let slug = parse_name_with_owner(&out.stdout)?;
            if slug_cache.len() > 64 {
                slug_cache.clear();
            }
            slug_cache.insert(toplevel.clone(), slug.clone());
            slug
        }
    };

    // Local branch scope: a PR whose head branch exists in this
    // repository was authored here; everything else is foreign and
    // never alerted (a fleet of daemons must not all park items for
    // one machine's PR).
    let refs = run_git(
        "git".as_ref(),
        GIT_PROBE_TIMEOUT,
        toplevel,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
    .await
    .filter(|out| out.status.success())?;
    let local_branches = parse_local_branches(&refs.stdout);

    let worktrees = run_git(
        "git".as_ref(),
        GIT_PROBE_TIMEOUT,
        toplevel,
        &["worktree", "list", "--porcelain"],
    )
    .await
    .filter(|out| out.status.success())?;
    let worktree_branches = parse_worktree_branches(&worktrees.stdout);

    // Session records: each LIVE session's durable worktree branch —
    // covers a session whose recorded checkout is already deleted.
    let mut meta_branches: Vec<(String, String)> = Vec::new();
    for (session_id, _) in group.session_roots.iter().take(MAX_META_READS) {
        if let Some(worktree) =
            crate::boot_readopt::session_meta_for(home, session_id).and_then(|meta| meta.worktree)
        {
            meta_branches.push((session_id.clone(), worktree.branch));
        }
    }
    let owners = branch_owners(&group.session_roots, &worktree_branches, &meta_branches);

    let list = run_gh(
        toplevel,
        &[
            "pr",
            "list",
            "--json",
            "number,title,headRefName,isDraft,state,mergeStateStatus,mergeable,autoMergeRequest,statusCheckRollup",
            "--limit",
            "50",
        ],
    )
    .await?;
    let mut observations = parse_pr_list(&list.stdout)?;

    let (owner, name) = slug.split_once('/')?;
    let queue_query = format!(
        "query{{repository(owner:\"{owner}\",name:\"{name}\"){{mergeQueue{{entries(first:50){{nodes{{pullRequest{{number}}}}}}}}}}}}",
    );
    let queue = run_gh(
        toplevel,
        &["api", "graphql", "-f", &format!("query={queue_query}")],
    )
    .await?;
    let queued = parse_queue_numbers(&queue.stdout)?;

    observations.retain(|pr| local_branches.contains(&pr.head_ref));
    for pr in &mut observations {
        pr.queued = queued.contains(&pr.number);
    }
    Some(RepoPoll {
        slug,
        observations,
        owners,
    })
}

// ---------------------------------------------------------------------------
// The standing task
// ---------------------------------------------------------------------------

/// ONE shepherd per daemon: the spawn edge refuses a second (the mode
/// branches call the shared gateway wiring once, but the pin is cheap
/// and the invariant is load-bearing — two shepherds double-wake).
static SHEPHERD_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Spawn the landing shepherd (daemon startup, beside the PR scanner).
/// Observe-and-wake only; bounded per tick: ≤3 `gh` + ≤3 `git`
/// subprocess calls per shepherded repository (each under the probe
/// timeout), ≤[`MAX_REPOS`] repositories, one agenda snapshot.
pub(crate) fn spawn_landing_shepherd(
    agenda: Arc<AgendaHandle>,
    settings_root: PathBuf,
    project_root: Option<PathBuf>,
    handover: Option<Arc<crate::handover::HandoverRuntime>>,
) -> Option<tokio::task::JoinHandle<()>> {
    if SHEPHERD_SPAWNED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(tokio::spawn(async move {
        let bus = agenda.bus().clone();
        let home = crate::platform::home_dir();
        let mut ledger = ShepherdLedger::default();
        let mut toplevel_cache: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
        let mut slug_cache: HashMap<PathBuf, String> = HashMap::new();
        let mut last_line = String::new();
        let mut transition = move |line: String| {
            if line != last_line {
                eprintln!("[landing-shepherd] {line}");
                last_line = line;
            }
        };
        loop {
            // Track HS2: a standing automation that wakes sessions and
            // parks daemon-attributed items runs on the scheduler-lease
            // holder only; secondaries idle and follow the lease.
            if let Some(runtime) = &handover {
                if !runtime.is_holder() {
                    tokio::time::sleep(IDLE_RECHECK).await;
                    continue;
                }
            }
            let config = crate::project::Project::from_root(settings_root.clone())
                .map(|proj| proj.config.landing_shepherd.clone())
                .unwrap_or_default();
            let enabled = shepherd_enabled(
                config.enabled,
                std::env::var("INTENDANT_LANDING_SHEPHERD").ok().as_deref(),
            );
            if !enabled {
                tokio::time::sleep(IDLE_RECHECK).await;
                continue;
            }
            let poll = poll_interval(
                config.poll_secs,
                std::env::var("INTENDANT_LANDING_SHEPHERD_POLL_MS")
                    .ok()
                    .as_deref(),
            );
            let stall_grace_ms = stall_ms(config.stall_secs);

            let session_roots: Vec<(String, PathBuf)> =
                crate::session_vitals::published_git_vitals_targets()
                    .map(|targets| targets.snapshot())
                    .unwrap_or_default();
            let groups =
                collect_repo_groups(project_root.as_deref(), &session_roots, &mut toplevel_cache)
                    .await;

            let mut live_slugs: BTreeSet<String> = BTreeSet::new();
            let mut woke = 0usize;
            let mut parked = 0usize;
            let mut skipped_repos = 0usize;
            for group in groups.values().take(MAX_REPOS) {
                let Some(poll_result) = poll_repo(group, &mut slug_cache, &home).await else {
                    skipped_repos += 1;
                    continue;
                };
                live_slugs.insert(poll_result.slug.clone());
                let now_ms = crate::coordination::now_ms();
                let alerts = ledger.fold(
                    &poll_result.slug,
                    &poll_result.observations,
                    now_ms,
                    stall_grace_ms,
                );
                if alerts.is_empty() {
                    continue;
                }
                // Seat liveness: the supervisor's published registry.
                // Never published (no supervisor in this shape) = no
                // wakeable seats — the park lane takes everything.
                // Published but contended = unknown; defer the whole
                // repo to the next tick rather than park a live seat's
                // PR.
                let live_ids = match crate::session_supervisor::published_live_session_registry() {
                    Some(registry) => match registry.live_wrapper_ids() {
                        Some(ids) => ids,
                        None => continue,
                    },
                    None => HashSet::new(),
                };
                let snapshot = agenda.snapshot();
                let already_parked = open_shepherd_prs(&snapshot, &poll_result.slug);
                for alert in &alerts {
                    let owner = owning_live_session(
                        &poll_result.owners,
                        &alert.observation.head_ref,
                        &live_ids,
                    );
                    let delivery = deliver_alert(
                        &agenda,
                        &bus,
                        &poll_result.slug,
                        alert,
                        owner.as_deref(),
                        &already_parked,
                    );
                    match delivery {
                        Delivery::Woke { session_id } => {
                            woke += 1;
                            eprintln!(
                                "[landing-shepherd] woke session {session_id}: PR #{} {}",
                                alert.observation.number,
                                alert.class.label()
                            );
                        }
                        Delivery::Parked { item_id } => {
                            parked += 1;
                            eprintln!(
                                "[landing-shepherd] parked needs-you item {item_id}: PR #{} {} \
                                 (no live owning session)",
                                alert.observation.number,
                                alert.class.label()
                            );
                        }
                        Delivery::AlreadyParked => {}
                        Delivery::ParkFailed { error } => {
                            eprintln!(
                                "[landing-shepherd] park failed for PR #{}: {error}",
                                alert.observation.number
                            );
                        }
                    }
                }
            }
            ledger.retain_slugs(&live_slugs);
            transition(match (live_slugs.len(), skipped_repos) {
                (0, 0) => "no shepherded repositories (no supervised checkouts)".to_string(),
                (0, skipped) => format!(
                    "gh unavailable or failing for {skipped} repo(s); shepherding idle"
                ),
                (repos, skipped) => format!(
                    "watching {repos} repo(s) ({skipped} skipped); last pass woke {woke} parked {parked}"
                ),
            });
            tokio::time::sleep(poll).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(number: u64) -> PrObservation {
        PrObservation {
            number,
            head_ref: format!("worktree-seat-{number}"),
            title: format!("change {number}"),
            draft: false,
            open: true,
            merge_state: "CLEAN".to_string(),
            mergeable: "MERGEABLE".to_string(),
            armed: false,
            queued: false,
            checks_failing: 0,
            checks_pending: 0,
        }
    }

    fn fold_one(ledger: &mut ShepherdLedger, pr: &PrObservation, now: u64) -> Vec<AlertClass> {
        ledger
            .fold("owner/repo", std::slice::from_ref(pr), now, 300_000)
            .into_iter()
            .map(|alert| alert.class)
            .collect()
    }

    // ── Observation parsing ──

    #[test]
    fn pr_list_json_parses_the_gh_shape() {
        // A trimmed real `gh pr list --json …` capture: one armed DIRTY
        // PR with a mixed rollup (CheckRun pending + failed, legacy
        // StatusContext), one clean draft with null auto-merge.
        let json = br#"[
          {"number": 761, "title": "fix: honest failures", "headRefName": "agent/fix-ci",
           "isDraft": false, "state": "OPEN", "mergeStateStatus": "DIRTY",
           "mergeable": "CONFLICTING",
           "autoMergeRequest": {"enabledAt": "2026-08-02T01:00:00Z"},
           "statusCheckRollup": [
             {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
             {"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": ""},
             {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"},
             {"__typename": "StatusContext", "state": "ERROR"}
           ]},
          {"number": 762, "title": "draft", "headRefName": "worktree-x",
           "isDraft": true, "state": "OPEN", "mergeStateStatus": "CLEAN",
           "mergeable": "MERGEABLE", "autoMergeRequest": null,
           "statusCheckRollup": []}
        ]"#;
        let prs = parse_pr_list(json).expect("parses");
        assert_eq!(prs.len(), 2);
        let armed = &prs[0];
        assert_eq!(armed.number, 761);
        assert_eq!(armed.head_ref, "agent/fix-ci");
        assert!(armed.armed && armed.open && !armed.draft);
        assert!(armed.conflicted());
        assert_eq!(
            armed.checks_failing, 2,
            "CheckRun FAILURE + StatusContext ERROR"
        );
        assert_eq!(armed.checks_pending, 1, "IN_PROGRESS");
        let draft = &prs[1];
        assert!(draft.draft && !draft.armed && !draft.conflicted());
        assert!(parse_pr_list(b"not json").is_none());
    }

    #[test]
    fn queue_numbers_parse_and_fail_closed() {
        let populated = br#"{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[
            {"position":1,"state":"AWAITING_CHECKS","pullRequest":{"number":760}},
            {"position":2,"state":"QUEUED","pullRequest":{"number":758}}]}}}}}"#;
        assert_eq!(
            parse_queue_numbers(populated).expect("parses"),
            BTreeSet::from([760, 758])
        );
        // No merge queue configured: an empty set, not a failure.
        let no_queue = br#"{"data":{"repository":{"mergeQueue":null}}}"#;
        assert_eq!(parse_queue_numbers(no_queue), Some(BTreeSet::new()));
        // GraphQL errors or shape trouble: None — ejection/stall must
        // never classify against an unknown queue truth.
        let errored = br#"{"errors":[{"message":"boom"}],"data":null}"#;
        assert!(parse_queue_numbers(errored).is_none());
        assert!(parse_queue_numbers(b"junk").is_none());
    }

    #[test]
    fn worktree_and_ref_listings_parse() {
        let porcelain = b"worktree /repo\nHEAD abc\nbranch refs/heads/main\n\nworktree /repo/.worktrees/seat\nHEAD def\nbranch refs/heads/worktree-seat\n\nworktree /repo/.worktrees/detached\nHEAD 123\ndetached\n";
        let rows = parse_worktree_branches(porcelain);
        assert_eq!(
            rows,
            vec![
                (PathBuf::from("/repo"), "main".to_string()),
                (
                    PathBuf::from("/repo/.worktrees/seat"),
                    "worktree-seat".to_string()
                ),
            ]
        );
        assert_eq!(
            parse_local_branches(b"main\nworktree-seat\n"),
            BTreeSet::from(["main".to_string(), "worktree-seat".to_string()])
        );
        assert_eq!(
            parse_name_with_owner(br#"{"nameWithOwner":"intendant-dev/Intendant"}"#),
            Some("intendant-dev/Intendant".to_string())
        );
        assert!(parse_name_with_owner(br#"{"nameWithOwner":"junk"}"#).is_none());
    }

    // ── The transition classifier, pinned case by case ──

    #[test]
    fn armed_dirty_fires_on_first_sight() {
        // The rig invariant: a DIRTY armed PR wakes within ONE poll —
        // no prior observation exists after a restart, and the class is
        // terminal, so the first fold must fire.
        let mut ledger = ShepherdLedger::default();
        let mut pr = obs(1);
        pr.armed = true;
        pr.merge_state = "DIRTY".to_string();
        assert_eq!(
            fold_one(&mut ledger, &pr, 1_000),
            vec![AlertClass::Conflict]
        );
    }

    #[test]
    fn ready_conflicting_fires_and_draft_never_does() {
        let mut ledger = ShepherdLedger::default();
        // Ready = open + non-draft; CONFLICTING via `mergeable` alone.
        let mut ready = obs(2);
        ready.mergeable = "CONFLICTING".to_string();
        assert_eq!(
            fold_one(&mut ledger, &ready, 1_000),
            vec![AlertClass::Conflict]
        );
        // A draft may be dirty forever — WIP is not a landing.
        let mut draft = obs(3);
        draft.draft = true;
        draft.merge_state = "DIRTY".to_string();
        draft.armed = true;
        assert!(fold_one(&mut ledger, &draft, 1_000).is_empty());
    }

    #[test]
    fn persisting_conflict_respects_cooldown_and_refires_on_new_edge() {
        let mut ledger = ShepherdLedger::default();
        let mut pr = obs(4);
        pr.armed = true;
        pr.merge_state = "DIRTY".to_string();
        assert_eq!(fold_one(&mut ledger, &pr, 1_000).len(), 1);
        // Persisting condition: quiet inside the cooldown…
        assert!(fold_one(&mut ledger, &pr, 2_000).is_empty());
        // …refires once the cooldown elapses (the heartbeat backstop)…
        assert_eq!(
            fold_one(&mut ledger, &pr, 1_000 + ALERT_COOLDOWN_MS).len(),
            1
        );
        // …clears when the condition clears…
        pr.merge_state = "CLEAN".to_string();
        assert!(fold_one(&mut ledger, &pr, 3_000 + ALERT_COOLDOWN_MS).is_empty());
        // …and a NEW conflict is a fresh edge that fires immediately.
        pr.merge_state = "DIRTY".to_string();
        assert_eq!(
            fold_one(&mut ledger, &pr, 4_000 + ALERT_COOLDOWN_MS).len(),
            1
        );
    }

    #[test]
    fn queue_ejection_fires_and_merge_exit_stays_silent() {
        let mut ledger = ShepherdLedger::default();
        let mut pr = obs(5);
        pr.queued = true;
        assert!(
            fold_one(&mut ledger, &pr, 1_000).is_empty(),
            "in-queue is healthy"
        );
        // Ejected: still open, no longer queued (armed state is noise
        // across this edge — ejection usually also disarms).
        pr.queued = false;
        assert_eq!(
            fold_one(&mut ledger, &pr, 2_000),
            vec![AlertClass::QueueEjected]
        );
        // The MERGED exit: the PR leaves the open set entirely — the
        // fold prunes it and never alerts.
        let mut ledger = ShepherdLedger::default();
        let mut pr = obs(6);
        pr.queued = true;
        assert!(fold_one(&mut ledger, &pr, 1_000).is_empty());
        assert!(ledger.fold("owner/repo", &[], 2_000, 300_000).is_empty());
        assert!(ledger.prev.is_empty(), "merged PR state pruned");
    }

    #[test]
    fn disarm_fires_outside_the_queue_only() {
        let mut ledger = ShepherdLedger::default();
        // Armed → unarmed with no queue on either side: a real disarm.
        let mut pr = obs(7);
        pr.armed = true;
        assert!(fold_one(&mut ledger, &pr, 1_000).is_empty());
        pr.armed = false;
        assert_eq!(
            fold_one(&mut ledger, &pr, 2_000),
            vec![AlertClass::Disarmed]
        );

        // Armed → queued (autoMergeRequest nulling in transit): noise.
        let mut ledger = ShepherdLedger::default();
        let mut pr = obs(8);
        pr.armed = true;
        assert!(fold_one(&mut ledger, &pr, 1_000).is_empty());
        pr.armed = false;
        pr.queued = true;
        assert!(fold_one(&mut ledger, &pr, 2_000).is_empty());

        // Ejection tick: the ejection is the diagnosis, not the disarm.
        pr.queued = false;
        assert_eq!(
            fold_one(&mut ledger, &pr, 3_000),
            vec![AlertClass::QueueEjected]
        );
    }

    #[test]
    fn armed_stall_needs_settled_checks_and_the_full_grace() {
        let mut ledger = ShepherdLedger::default();
        let mut pr = obs(9);
        pr.armed = true;
        // Pending checks: the clock never starts.
        pr.checks_pending = 2;
        assert!(fold_one(&mut ledger, &pr, 1_000).is_empty());
        // Checks settle (with a stuck-failure residue — the exact
        // 2026-07-17 class): clock starts, quiet until the grace.
        pr.checks_pending = 0;
        pr.checks_failing = 1;
        assert!(fold_one(&mut ledger, &pr, 10_000).is_empty());
        assert!(fold_one(&mut ledger, &pr, 10_000 + 299_000).is_empty());
        assert_eq!(
            fold_one(&mut ledger, &pr, 10_000 + 300_000),
            vec![AlertClass::ArmedStall]
        );
        // Entering the queue clears the stall condition entirely.
        pr.queued = true;
        assert!(fold_one(&mut ledger, &pr, 10_000 + 301_000).is_empty());
        assert!(ledger.stall_since.is_empty());
    }

    #[test]
    fn stall_clock_resets_when_checks_reopen() {
        let mut ledger = ShepherdLedger::default();
        let mut pr = obs(10);
        pr.armed = true;
        assert!(fold_one(&mut ledger, &pr, 0).is_empty());
        // New push: checks pending again — the clock must restart, not
        // resume.
        pr.checks_pending = 1;
        assert!(fold_one(&mut ledger, &pr, 200_000).is_empty());
        pr.checks_pending = 0;
        assert!(fold_one(&mut ledger, &pr, 250_000).is_empty());
        assert!(
            fold_one(&mut ledger, &pr, 250_000 + 299_000).is_empty(),
            "grace counts from the settle, not the first arm"
        );
        assert_eq!(fold_one(&mut ledger, &pr, 250_000 + 300_000).len(), 1);
    }

    #[test]
    fn conflict_owns_the_dirty_armed_case_not_the_stall() {
        // DIRTY + armed + settled would satisfy the stall shape too;
        // the conflict class owns it so the wake carries the reconcile
        // ritual, not the rerun recipe.
        let mut ledger = ShepherdLedger::default();
        let mut pr = obs(11);
        pr.armed = true;
        pr.merge_state = "DIRTY".to_string();
        let classes = fold_one(&mut ledger, &pr, 400_000);
        assert_eq!(classes, vec![AlertClass::Conflict]);
        assert!(
            ledger.stall_since.is_empty(),
            "no stall clock on a conflict"
        );
    }

    #[test]
    fn foreign_repo_state_prunes_when_sessions_leave() {
        let mut ledger = ShepherdLedger::default();
        let mut pr = obs(12);
        pr.armed = true;
        pr.merge_state = "DIRTY".to_string();
        assert_eq!(fold_one(&mut ledger, &pr, 1_000).len(), 1);
        ledger.retain_slugs(&BTreeSet::new());
        assert!(ledger.prev.is_empty() && ledger.active.is_empty());
    }

    // ── Ownership ──

    #[test]
    fn branch_owners_join_worktrees_and_session_records() {
        let session_roots = vec![
            (
                "s-inner".to_string(),
                PathBuf::from("/repo/.worktrees/seat/sub"),
            ),
            ("s-main".to_string(), PathBuf::from("/repo")),
            ("s-lost".to_string(), PathBuf::from("/elsewhere")),
        ];
        let worktrees = vec![
            (PathBuf::from("/repo"), "main".to_string()),
            (
                PathBuf::from("/repo/.worktrees/seat"),
                "worktree-seat".to_string(),
            ),
        ];
        // The durable session record covers a deleted checkout.
        let metas = vec![("s-lost".to_string(), "worktree-lost".to_string())];
        let owners = branch_owners(&session_roots, &worktrees, &metas);
        assert_eq!(owners["worktree-seat"], vec!["s-inner".to_string()]);
        assert_eq!(owners["main"], vec!["s-main".to_string()]);
        assert_eq!(owners["worktree-lost"], vec!["s-lost".to_string()]);

        let live: HashSet<String> = ["s-inner".to_string()].into();
        assert_eq!(
            owning_live_session(&owners, "worktree-seat", &live),
            Some("s-inner".to_string())
        );
        assert_eq!(owning_live_session(&owners, "worktree-lost", &live), None);
        assert_eq!(owning_live_session(&owners, "unknown-branch", &live), None);
    }

    // ── Wake / park fallback ──

    fn open_agenda(dir: &Path) -> AgendaHandle {
        let store = crate::agenda::AgendaStore::open(dir).expect("open store");
        AgendaHandle::new(store, EventBus::new(), dir)
    }

    fn conflict_alert(number: u64) -> PrAlert {
        let mut pr = obs(number);
        pr.armed = true;
        pr.merge_state = "DIRTY".to_string();
        PrAlert {
            class: AlertClass::Conflict,
            observation: pr,
        }
    }

    #[tokio::test]
    async fn live_owner_wakes_through_the_task_lane() {
        let tmp = tempfile::tempdir().unwrap();
        let agenda = open_agenda(tmp.path());
        let bus = EventBus::new();
        let mut intents = bus.subscribe_intents();
        let alert = conflict_alert(20);

        let delivery = deliver_alert(
            &agenda,
            &bus,
            "owner/repo",
            &alert,
            Some("s-owner"),
            &HashSet::new(),
        );
        assert_eq!(
            delivery,
            Delivery::Woke {
                session_id: "s-owner".to_string()
            }
        );
        // The wake rides the lossless intent lane as a targeted
        // StartTask — the existing task/nudge lane, nothing bespoke.
        let event = intents.try_recv().expect("intent queued");
        let AppEvent::ControlCommand(ControlMsg::StartTask {
            session_id, task, ..
        }) = event
        else {
            panic!("expected StartTask, got {event:?}");
        };
        assert_eq!(session_id.as_deref(), Some("s-owner"));
        assert!(task.contains("[landing-shepherd]"));
        assert!(task.contains("PR #20"));
        assert!(
            task.contains("merge, never rebase"),
            "ritual rides the wake"
        );
        assert!(
            task.contains("app-html-assembler"),
            "fragments ritual rides the wake"
        );
        assert!(
            task.contains("never merges, resolves, or re-arms"),
            "observe-and-wake stance stated"
        );
        // No agenda item for a woken seat.
        assert!(agenda.snapshot().is_empty());
    }

    #[tokio::test]
    async fn gone_owner_parks_one_deduped_needs_you_item() {
        let tmp = tempfile::tempdir().unwrap();
        let agenda = open_agenda(tmp.path());
        let bus = EventBus::new();
        let alert = conflict_alert(21);

        let delivery = deliver_alert(&agenda, &bus, "owner/repo", &alert, None, &HashSet::new());
        let Delivery::Parked { item_id } = delivery else {
            panic!("expected park, got {delivery:?}");
        };
        let snapshot = agenda.snapshot();
        let item = snapshot
            .iter()
            .find(|item| item.id == item_id)
            .expect("parked");
        assert_eq!(item.kind, AgendaKind::Task);
        assert!(item.title.starts_with("Landing needs you: PR #21"));
        assert!(item.tags.contains(&SHEPHERD_TAG.to_string()));
        assert!(item.tags.contains(&NEEDS_YOU_TAG.to_string()));
        assert_eq!(item.provenance.kind.as_deref(), Some("daemon"));
        assert_eq!(item.provenance.source.as_deref(), Some(SHEPHERD_SOURCE));
        assert!(item
            .refs
            .iter()
            .any(|r| r.locator == "https://github.com/owner/repo/pull/21"));
        assert!(item.body.contains("No live session owns branch"));
        assert!(item.body.contains("merge, never rebase"));

        // The fold is the durable dedup: a restarted shepherd
        // reconstructs the parked set and re-parks nothing.
        let already = open_shepherd_prs(&snapshot, "owner/repo");
        assert!(already.contains(&21));
        assert_eq!(
            deliver_alert(&agenda, &bus, "owner/repo", &alert, None, &already),
            Delivery::AlreadyParked
        );
        assert_eq!(agenda.snapshot().len(), 1, "still one item");
        // A different repo's same-number PR is a different key.
        assert!(open_shepherd_prs(&snapshot, "other/repo").is_empty());
    }

    #[tokio::test]
    async fn wake_beats_park_when_the_seat_is_alive() {
        // The fallback ORDER is the card's (d): park only when the
        // owning session is gone.
        let tmp = tempfile::tempdir().unwrap();
        let agenda = open_agenda(tmp.path());
        let bus = EventBus::new();
        let alert = conflict_alert(22);
        let owners: BTreeMap<String, Vec<String>> = BTreeMap::from([(
            alert.observation.head_ref.clone(),
            vec!["s-dead".to_string(), "s-live".to_string()],
        )]);
        // Two recorded owners, one alive: the live one is woken.
        let live: HashSet<String> = ["s-live".to_string()].into();
        let owner = owning_live_session(&owners, &alert.observation.head_ref, &live);
        assert_eq!(owner.as_deref(), Some("s-live"));
        // Nobody alive: the park lane takes it.
        let none_live: HashSet<String> = HashSet::new();
        assert_eq!(
            owning_live_session(&owners, &alert.observation.head_ref, &none_live),
            None
        );
        let delivery = deliver_alert(&agenda, &bus, "owner/repo", &alert, None, &HashSet::new());
        assert!(matches!(delivery, Delivery::Parked { .. }));
    }

    // ── Config edge ──

    #[test]
    fn env_kill_switch_outranks_config_both_ways() {
        assert!(shepherd_enabled(true, None));
        assert!(!shepherd_enabled(false, None));
        for off in ["0", "false", "off", "no", " OFF "] {
            assert!(!shepherd_enabled(true, Some(off)), "{off:?}");
        }
        assert!(shepherd_enabled(false, Some("1")));
        assert!(shepherd_enabled(false, Some("on")));
    }

    #[test]
    fn poll_and_stall_knobs_clamp() {
        assert_eq!(poll_interval(None, None), Duration::from_secs(60));
        assert_eq!(poll_interval(Some(30), None), Duration::from_secs(30));
        assert_eq!(
            poll_interval(Some(1), None),
            Duration::from_secs(5),
            "floor"
        );
        assert_eq!(
            poll_interval(Some(600), Some("2000")),
            Duration::from_millis(2000),
            "rig env override wins"
        );
        assert_eq!(
            poll_interval(None, Some("1")),
            Duration::from_millis(250),
            "rig floor"
        );
        assert_eq!(stall_ms(None), 300_000);
        assert_eq!(stall_ms(Some(10)), 30_000, "floor");
    }
}

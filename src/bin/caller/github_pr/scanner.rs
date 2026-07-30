//! The PR scanner: the coordination radar's zero-LLM sibling — a
//! deterministic daemon task that mirrors every watched pull request
//! onto the agenda as a **thin intent anchor** and lifecycles it.
//! **The agenda is the durable truth, GitHub is the live truth, scanner
//! state is a pure function of both** (Track PR ruling 6): the scanner
//! keeps no durable state of its own — at every pass it reconstructs
//! its known-anchor map from the agenda fold (its own daemon-attributed
//! items' PR url refs) and diffs it against the open-PR set, so a
//! restart converges with zero duplicates by construction. ETags and
//! the hub id are in-memory warmth; losing them costs one conditional
//! read, never a wrong op.
//!
//! Ops record intent lifecycle ONLY — parked, placed, annotated,
//! completed, reopened. PR *state* (checks, reviews, draft flips, title
//! edits, mergeability) is never an op and never stored here; it is
//! joined at render time by the next slice. The scanner completes what
//! it parks (the completing-what-you-resolved exception), reopens its
//! own completed anchors when a PR reopens, and never touches items it
//! didn't park — a **retired** anchor stays retired (an owner act
//! outranks the mirror; the scanner annotates once instead).

use std::collections::{BTreeMap, HashMap};

use super::client::{ApiError, Conditional, GithubAppClient, PrSummary, PullDetail};
use crate::agenda::{AgendaActor, AgendaCommand, AgendaHandle, AgendaItem, AgendaKind};
use crate::agenda::{AgendaRefSpec, AgendaStatus};

/// The scanner's self-described lane label — on every op it writes,
/// beside (never inside) the `daemon` actor attribution.
pub(crate) const SCANNER_SOURCE: &str = "github-pr-scanner";
/// The reserved tag keying the PRs hub (hubs have no name key — ULIDs
/// only — so a tag is the one durable, machine-safe find key).
pub(crate) const PRS_HUB_TAG: &str = "prs-hub";
/// Every anchor's tag. Nothing mutable ever lands in tags.
pub(crate) const PR_TAG: &str = "pr";
/// The retired-anchor note marker (also the once-only dedupe key).
const RETIRED_NOTE: &str =
    "PR reopened on GitHub; this anchor was retired by the owner — leaving it retired.";

/// One anchor as reconstructed from the agenda fold: a scanner-parked
/// item whose url ref names a watched PR.
#[derive(Debug, Clone)]
pub(crate) struct AnchorView {
    pub(crate) item_id: String,
    pub(crate) status: AgendaStatus,
    /// The retired-anchor note was already written (once, ever). The
    /// executor re-reads everything else (placement, terminal notes)
    /// from a fresh snapshot at act time — fresher beats cached.
    pub(crate) has_retired_note: bool,
}

/// What one pass decided for one repo — pure data out of [`plan_repo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScanAction {
    /// A PR with no anchor of any status: park + place.
    Park { number: u64 },
    /// An open anchor whose PR left the open set: annotate terminal
    /// state, unplace, complete (state-checked, re-entry safe).
    Complete { item_id: String, number: u64 },
    /// A completed anchor whose PR is open again: reopen + re-place.
    Reopen { item_id: String },
    /// A retired anchor whose PR is open again: one note, never a
    /// status change — the owner's act outranks the mirror.
    NoteRetiredReopened { item_id: String },
}

/// Parse a GitHub PR html url into `(owner/repo, number)` — the anchor
/// key. Anything else is not an anchor locator.
pub(crate) fn parse_pr_locator(url: &str) -> Option<(String, u64)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let mut segments = rest.split('/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    if segments.next()? != "pull" {
        return None;
    }
    let number: u64 = segments.next()?.split(['?', '#']).next()?.parse().ok()?;
    if owner.is_empty() || repo.is_empty() || number == 0 {
        return None;
    }
    Some((format!("{owner}/{repo}"), number))
}

/// Reconstruct the known-anchor map for `repo` from the agenda fold:
/// items the daemon parked with the scanner's source label, keyed by
/// their PR url ref. The fold is the scanner's only durable state.
pub(crate) fn anchors_for_repo(snapshot: &[AgendaItem], repo: &str) -> BTreeMap<u64, AnchorView> {
    let mut anchors: BTreeMap<u64, AnchorView> = BTreeMap::new();
    for item in snapshot {
        if item.provenance.kind.as_deref() != Some("daemon")
            || item.provenance.source.as_deref() != Some(SCANNER_SOURCE)
        {
            continue;
        }
        let Some((item_repo, number)) = item.refs.iter().find_map(|r| parse_pr_locator(&r.locator))
        else {
            continue;
        };
        if item_repo != repo {
            continue;
        }
        let view = AnchorView {
            item_id: item.id.clone(),
            status: item.status,
            has_retired_note: item.annotations.iter().any(|note| {
                note.source.as_deref() == Some(SCANNER_SOURCE) && note.text == RETIRED_NOTE
            }),
        };
        // First (oldest ULID) anchor wins a duplicate key; later ones
        // would only exist from a historical bug and are left alone.
        anchors.entry(number).or_insert(view);
    }
    anchors
}

/// The pure diff: known anchors × the live open set → actions. Membership
/// changes are the ONLY trigger — a draft flip, title edit, or CI change
/// alters nothing here, which is what keeps the diary byte-quiet between
/// lifecycle events.
pub(crate) fn plan_repo(
    anchors: &BTreeMap<u64, AnchorView>,
    open: &[PrSummary],
) -> Vec<ScanAction> {
    let mut actions = Vec::new();
    let open_numbers: std::collections::BTreeSet<u64> = open.iter().map(|pr| pr.number).collect();
    for pr in open {
        match anchors.get(&pr.number) {
            None => actions.push(ScanAction::Park { number: pr.number }),
            Some(anchor) => match anchor.status {
                AgendaStatus::Open => {}
                AgendaStatus::Done => actions.push(ScanAction::Reopen {
                    item_id: anchor.item_id.clone(),
                }),
                AgendaStatus::Retired => {
                    if !anchor.has_retired_note {
                        actions.push(ScanAction::NoteRetiredReopened {
                            item_id: anchor.item_id.clone(),
                        });
                    }
                }
            },
        }
    }
    for (number, anchor) in anchors {
        if anchor.status == AgendaStatus::Open && !open_numbers.contains(number) {
            actions.push(ScanAction::Complete {
                item_id: anchor.item_id.clone(),
                number: *number,
            });
        }
    }
    actions
}

fn daemon_actor() -> Option<AgendaActor> {
    Some(AgendaActor::daemon())
}

/// Find or create the PRs hub: the oldest open item tagged
/// [`PRS_HUB_TAG`] (ULID order — deterministic under accidental
/// duplicates, which are diagnosed, never auto-merged).
pub(crate) fn find_or_create_hub(
    agenda: &AgendaHandle,
    snapshot: &[AgendaItem],
) -> Result<String, String> {
    let mut hubs: Vec<&AgendaItem> = snapshot
        .iter()
        .filter(|item| {
            item.status == AgendaStatus::Open && item.tags.iter().any(|t| t == PRS_HUB_TAG)
        })
        .collect();
    hubs.sort_by(|a, b| a.id.cmp(&b.id));
    if hubs.len() > 1 {
        eprintln!(
            "[github-pr] {} items carry the {PRS_HUB_TAG} tag — using the oldest ({}); \
             reconcile the others by hand",
            hubs.len(),
            hubs[0].id
        );
    }
    if let Some(hub) = hubs.first() {
        return Ok(hub.id.clone());
    }
    let hub = agenda
        .apply(
            AgendaCommand::Add {
                kind: AgendaKind::Note,
                title: "PRs".to_string(),
                body: "Open pull requests across the watched repos — parked, filed, and \
                       completed by the daemon's GitHub scanner. GitHub stays the source \
                       of truth for PR state; anchors point, they never store."
                    .to_string(),
                tags: vec![PRS_HUB_TAG.to_string()],
                due_ms: None,
                source: Some(SCANNER_SOURCE.to_string()),
                refs: Vec::new(),
            },
            daemon_actor(),
        )
        .map_err(|error| format!("park PRs hub: {error}"))?;
    Ok(hub.id)
}

/// Anchor title: `Repo#N: <PR title>`, truncated to the intake cap.
fn anchor_title(repo: &str, pr: &PrSummary) -> String {
    let short = repo.split('/').next_back().unwrap_or(repo);
    let mut title = format!("{short}#{}: {}", pr.number, pr.title.trim());
    if title.chars().count() > 500 {
        title = title.chars().take(499).collect::<String>() + "…";
    }
    title
}

fn terminal_note(detail: &PullDetail) -> String {
    if detail.merged {
        let sha = detail
            .merge_commit_sha
            .as_deref()
            .map(|sha| format!(" ({})", &sha[..sha.len().min(9)]))
            .unwrap_or_default();
        let at = detail
            .merged_at
            .as_deref()
            .map(|t| format!(" at {t}"))
            .unwrap_or_default();
        format!("PR merged{sha}{at}")
    } else {
        let at = detail
            .closed_at
            .as_deref()
            .map(|t| format!(" at {t}"))
            .unwrap_or_default();
        format!("PR closed without merge{at}")
    }
}

/// One pass's tally — what actually changed (all zeros on a converged
/// re-scan: the acceptance criterion made observable).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ScanSummary {
    pub(crate) parked: usize,
    pub(crate) completed: usize,
    pub(crate) reopened: usize,
    pub(crate) noted_retired: usize,
    pub(crate) errors: Vec<(String, ApiError)>,
}

/// In-memory warmth only: conditional-read validators per repo. Losing
/// this (a restart) costs one full list read, never a wrong op.
#[derive(Default)]
pub(crate) struct ScannerState {
    etags: HashMap<String, String>,
}

/// One full pass over the watched repos. Hermetic by construction: the
/// caller supplies the agenda handle, the client (fixture-based in
/// tests), and the repo list — nothing here reads config, custody, or
/// the process environment.
pub(crate) async fn scan_once(
    agenda: &AgendaHandle,
    client: &GithubAppClient,
    repos: &[String],
    state: &mut ScannerState,
    tier1: &super::join::Tier1Cache,
) -> ScanSummary {
    let mut summary = ScanSummary::default();
    let mut hub_id: Option<String> = None;
    for repo in repos {
        let listed = match client
            .list_open_pulls(repo, state.etags.get(repo).map(String::as_str))
            .await
        {
            Ok(listed) => listed,
            Err(error) => {
                summary.errors.push((repo.clone(), error));
                continue;
            }
        };
        let open = match listed {
            // 304: GitHub's open set is unchanged since the validator —
            // membership cannot have changed, so there is nothing to do
            // and nothing to write. The tier-1 join's states are hereby
            // CONFIRMED current: refresh their stamps.
            Conditional::NotModified => {
                tier1.touch_repo(repo);
                continue;
            }
            Conditional::Fresh { value, etag } => {
                match etag {
                    Some(etag) => {
                        state.etags.insert(repo.clone(), etag);
                    }
                    None => {
                        state.etags.remove(repo);
                    }
                }
                value
            }
        };
        // Feed the tier-1 render join before planning: the list read
        // already paid for this state; rendering must never re-fetch it.
        tier1.update_repo(repo, &open);
        let snapshot = agenda.snapshot();
        let anchors = anchors_for_repo(&snapshot, repo);
        let actions = plan_repo(&anchors, &open);
        if actions.is_empty() {
            continue;
        }
        let by_number: HashMap<u64, &PrSummary> = open.iter().map(|pr| (pr.number, pr)).collect();
        for action in actions {
            match action {
                ScanAction::Park { number } => {
                    let Some(pr) = by_number.get(&number) else {
                        continue;
                    };
                    let hub = match &hub_id {
                        Some(id) => id.clone(),
                        None => match find_or_create_hub(agenda, &agenda.snapshot()) {
                            Ok(id) => {
                                hub_id = Some(id.clone());
                                id
                            }
                            Err(error) => {
                                summary
                                    .errors
                                    .push((repo.clone(), ApiError::Unreachable(error)));
                                continue;
                            }
                        },
                    };
                    let parked = agenda.apply(
                        AgendaCommand::Add {
                            kind: AgendaKind::Task,
                            title: anchor_title(repo, pr),
                            body: format!(
                                "by {} · {} ← {}",
                                pr.user.login, pr.base.branch, pr.head.branch
                            ),
                            tags: vec![PR_TAG.to_string()],
                            due_ms: None,
                            source: Some(SCANNER_SOURCE.to_string()),
                            refs: vec![AgendaRefSpec {
                                ref_type: crate::agenda::AgendaRefType::Url,
                                locator: pr.html_url.clone(),
                                must_read: false,
                                label: None,
                            }],
                        },
                        daemon_actor(),
                    );
                    match parked {
                        Ok(item) => {
                            let placed = agenda.apply(
                                AgendaCommand::Place {
                                    id: item.id.clone(),
                                    under: hub.clone(),
                                    source: Some(SCANNER_SOURCE.to_string()),
                                },
                                daemon_actor(),
                            );
                            if let Err(error) = placed {
                                eprintln!("[github-pr] place {}: {error}", item.id);
                            }
                            summary.parked += 1;
                        }
                        Err(error) => {
                            eprintln!("[github-pr] park {repo}#{number}: {error}");
                        }
                    }
                }
                ScanAction::Complete { item_id, number } => {
                    // Terminal state first: without it we cannot write an
                    // honest completion note. A transient failure defers
                    // the completion to the next pass — the anchor stays
                    // open one more cycle rather than closing blind. A
                    // detail that still claims "open" is a list/detail
                    // race: reconcile next pass.
                    let detail = match client.get_pull(repo, number).await {
                        Ok(detail) => detail,
                        Err(error) => {
                            summary.errors.push((repo.clone(), error));
                            continue;
                        }
                    };
                    if detail.state.as_deref() == Some("open") {
                        continue;
                    }
                    // State-checked executor: re-entry after a crash
                    // between these ops must not duplicate any of them.
                    let current = agenda.snapshot();
                    let Some(item) = current.iter().find(|i| i.id == item_id) else {
                        continue;
                    };
                    let has_note = item.annotations.iter().any(|note| {
                        note.source.as_deref() == Some(SCANNER_SOURCE)
                            && (note.text.starts_with("PR merged")
                                || note.text.starts_with("PR closed"))
                    });
                    if !has_note {
                        let _ = agenda.apply(
                            AgendaCommand::Annotate {
                                id: item_id.clone(),
                                text: terminal_note(&detail),
                                source: Some(SCANNER_SOURCE.to_string()),
                            },
                            daemon_actor(),
                        );
                    }
                    if let Some(placement) = item.part_of.as_ref() {
                        let _ = agenda.apply(
                            AgendaCommand::RemovePartOf {
                                id: item_id.clone(),
                                parent_id: placement.parent_id.clone(),
                                source: Some(SCANNER_SOURCE.to_string()),
                            },
                            daemon_actor(),
                        );
                    }
                    if item.status == AgendaStatus::Open {
                        match agenda.apply(
                            AgendaCommand::Complete {
                                id: item_id.clone(),
                                source: Some(SCANNER_SOURCE.to_string()),
                            },
                            daemon_actor(),
                        ) {
                            Ok(_) => summary.completed += 1,
                            Err(error) => {
                                eprintln!("[github-pr] complete {item_id}: {error}");
                            }
                        }
                    }
                }
                ScanAction::Reopen { item_id } => {
                    let reopened = agenda.apply(
                        AgendaCommand::Reopen {
                            id: item_id.clone(),
                            source: Some(SCANNER_SOURCE.to_string()),
                        },
                        daemon_actor(),
                    );
                    if reopened.is_ok() {
                        summary.reopened += 1;
                        let hub = match &hub_id {
                            Some(id) => Some(id.clone()),
                            None => match find_or_create_hub(agenda, &agenda.snapshot()) {
                                Ok(id) => {
                                    hub_id = Some(id.clone());
                                    Some(id)
                                }
                                Err(_) => None,
                            },
                        };
                        if let Some(hub) = hub {
                            let _ = agenda.apply(
                                AgendaCommand::Place {
                                    id: item_id.clone(),
                                    under: hub,
                                    source: Some(SCANNER_SOURCE.to_string()),
                                },
                                daemon_actor(),
                            );
                        }
                    }
                }
                ScanAction::NoteRetiredReopened { item_id } => {
                    let noted = agenda.apply(
                        AgendaCommand::Annotate {
                            id: item_id.clone(),
                            text: RETIRED_NOTE.to_string(),
                            source: Some(SCANNER_SOURCE.to_string()),
                        },
                        daemon_actor(),
                    );
                    if noted.is_ok() {
                        summary.noted_retired += 1;
                    }
                }
            }
        }
    }
    summary
}

/// Default cadence — parity with the coordination radar's gh cache
/// floor (`PR_CACHE_MIN_MS`); `[integrations.github] poll_minutes`
/// overrides, floor 1.
const DEFAULT_POLL_MINUTES: u64 = 5;
/// How often the idle loop re-checks whether the integration became
/// configured (blob existence + config — pure path math, no keystore).
const IDLE_RECHECK_S: u64 = 60;
/// Failure backoff ceiling.
const MAX_BACKOFF_S: u64 = 60 * 60;

/// The standing scanner loop. Natural on/off (Track PR ruling 5):
/// credentials sealed AND a non-empty watch list = it runs; anything
/// less = it sleeps, no errors. The keystore is touched only when the
/// client (re)builds — noticed via the sealed blob's mtime, never by a
/// per-poll unseal. One diagnostic line per status *transition*, never
/// per poll.
pub(crate) fn spawn_scanner(
    agenda: std::sync::Arc<AgendaHandle>,
    settings_root: std::path::PathBuf,
    handover: Option<std::sync::Arc<crate::handover::HandoverRuntime>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let runtime = super::status::global();
        let mut state = ScannerState::default();
        let mut client: Option<(std::time::SystemTime, std::sync::Arc<GithubAppClient>)> = None;
        let mut consecutive_failures: u32 = 0;
        let mut last_line = String::new();
        let mut transition = |line: String| {
            if line != last_line {
                eprintln!("[github-pr] {line}");
                last_line = line;
            }
        };
        loop {
            // Track HS2 (intake Q9): the scanner is a standing automation
            // — a daemon-attributed mirror writer on a poll loop — so it
            // runs on the scheduler-lease holder only; two daemons
            // scanning race anchor parks against slightly stale folds.
            // Secondaries idle at the same recheck cadence as the
            // keystore-free state and follow the lease as it moves.
            if let Some(runtime) = &handover {
                if !runtime.is_holder() {
                    tokio::time::sleep(std::time::Duration::from_secs(IDLE_RECHECK_S)).await;
                    continue;
                }
            }
            let config = crate::project::Project::from_root(settings_root.clone())
                .map(|proj| proj.config.integrations.github.clone())
                .unwrap_or_default();
            let blob_mtime = crate::key_custody::github_app_blob_mtime();
            if blob_mtime.is_none() || config.repos.is_empty() {
                client = None;
                super::join::publish_client(None);
                tokio::time::sleep(std::time::Duration::from_secs(IDLE_RECHECK_S)).await;
                continue;
            }
            let blob_mtime = blob_mtime.expect("checked above");
            let rebuild = match &client {
                Some((built_from, _)) => *built_from != blob_mtime,
                None => true,
            };
            if rebuild {
                // The one unseal site outside a configure gesture: the
                // client holds the parsed key for its lifetime; a
                // re-configure (new blob mtime) rebuilds it.
                let parsed = crate::key_custody::github_app_from_custody().and_then(|secret| {
                    super::credentials::GithubAppCredentials::from_sealed_bytes(secret.as_bytes())
                        .ok()
                });
                // A pending-install document (manifest ceremony sealed,
                // App not installed yet) is a named pause, not a
                // failure — and this unseal re-establishes the runtime's
                // pending cache after a daemon restart.
                if let Some(creds) = parsed.as_ref() {
                    if creds.installation_id.is_none() {
                        runtime.set_pending_install(creds.slug.clone().unwrap_or_default());
                        transition(
                            "installation pending — install the App on GitHub; integration paused"
                                .to_string(),
                        );
                        client = None;
                        super::join::publish_client(None);
                        tokio::time::sleep(std::time::Duration::from_secs(IDLE_RECHECK_S)).await;
                        continue;
                    }
                }
                let built =
                    parsed.and_then(|creds| GithubAppClient::new(runtime.api_base(), creds).ok());
                match built {
                    Some(fresh) => {
                        let fresh = std::sync::Arc::new(fresh);
                        super::join::publish_client(Some(fresh.clone()));
                        client = Some((blob_mtime, fresh));
                    }
                    None => {
                        // Deny/parse failure was audited by name inside
                        // the custody lane; the status surface says why.
                        runtime.record(super::status::CheckResult::Denied(
                            "custody or credential-document failure — see the custody trail"
                                .to_string(),
                        ));
                        transition("credentials unavailable; integration paused".to_string());
                        client = None;
                        super::join::publish_client(None);
                        tokio::time::sleep(std::time::Duration::from_secs(IDLE_RECHECK_S)).await;
                        continue;
                    }
                }
            }
            let Some((_, active)) = &client else {
                continue;
            };
            let summary = scan_once(
                &agenda,
                active,
                &config.repos,
                &mut state,
                super::join::tier1(),
            )
            .await;
            let poll_s = config
                .poll_minutes
                .unwrap_or(DEFAULT_POLL_MINUTES)
                .max(1)
                .saturating_mul(60);
            let mut sleep_s = poll_s;
            if summary.errors.is_empty() {
                consecutive_failures = 0;
                runtime.record(super::status::CheckResult::Valid);
                transition(format!(
                    "watching {} repo(s); last pass parked {} completed {} reopened {}",
                    config.repos.len(),
                    summary.parked,
                    summary.completed,
                    summary.reopened
                ));
            } else {
                consecutive_failures = consecutive_failures.saturating_add(1);
                // The most configuration-shaped error wins the status.
                let worst = summary
                    .errors
                    .iter()
                    .map(|(_, error)| error)
                    .max_by_key(|error| match error {
                        ApiError::Unreachable(_) => 0,
                        ApiError::RateLimited { .. } => 1,
                        ApiError::Denied(_) => 2,
                    })
                    .expect("non-empty");
                runtime.record_error(worst);
                transition(format!(
                    "pass had {} error(s): {} — backing off",
                    summary.errors.len(),
                    worst
                ));
                let backoff = poll_s.saturating_mul(1 << consecutive_failures.min(6));
                sleep_s = backoff.min(MAX_BACKOFF_S).max(poll_s);
                if let Some(retry) = summary.errors.iter().find_map(|(_, e)| match e {
                    ApiError::RateLimited {
                        retry_after_s: Some(s),
                        ..
                    } => Some(*s),
                    _ => None,
                }) {
                    sleep_s = sleep_s.max(retry);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(sleep_s)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agenda::AgendaStore;
    use crate::github_pr::client::test_fixture::{
        pull, spawn_fixture, test_credentials, token_route,
    };
    use std::collections::HashMap as StdHashMap;
    use std::sync::Arc;

    fn open_agenda(dir: &std::path::Path) -> Arc<AgendaHandle> {
        let bus = crate::event::EventBus::new();
        Arc::new(AgendaHandle::new(AgendaStore::open(dir).unwrap(), bus, dir))
    }

    fn log_bytes(dir: &std::path::Path) -> Vec<u8> {
        std::fs::read(dir.join("agenda.jsonl")).unwrap_or_default()
    }

    fn pulls_route(prs: serde_json::Value, etag: &str) -> (u16, Vec<(String, String)>, String) {
        (
            200,
            vec![("etag".to_string(), format!("\"{etag}\""))],
            prs.to_string(),
        )
    }

    #[test]
    fn pr_locators_parse_and_reject() {
        assert_eq!(
            parse_pr_locator("https://github.com/intendant-dev/Intendant/pull/591"),
            Some(("intendant-dev/Intendant".to_string(), 591))
        );
        for bad in [
            "https://github.com/o/r/issues/5",
            "https://example.com/o/r/pull/5",
            "https://github.com/o/r/pull/zero",
            "https://github.com/o/r/pull/0",
        ] {
            assert_eq!(parse_pr_locator(bad), None, "{bad}");
        }
    }

    #[test]
    fn plan_is_membership_only() {
        let anchors = BTreeMap::from([(
            1u64,
            AnchorView {
                item_id: "A".into(),
                status: AgendaStatus::Open,
                has_retired_note: false,
            },
        )]);
        // Same membership, different tier-1 state: nothing to do.
        let flipped: Vec<crate::github_pr::client::PrSummary> =
            serde_json::from_value(serde_json::json!([pull(1, "retitled entirely", true)]))
                .unwrap();
        assert!(plan_repo(&anchors, &flipped).is_empty());
        // A new number parks; a vanished one completes.
        let two: Vec<crate::github_pr::client::PrSummary> =
            serde_json::from_value(serde_json::json!([pull(2, "new", false)])).unwrap();
        assert_eq!(
            plan_repo(&anchors, &two),
            vec![
                ScanAction::Park { number: 2 },
                ScanAction::Complete {
                    item_id: "A".into(),
                    number: 1
                }
            ]
        );
    }

    #[tokio::test]
    async fn backfill_parks_placed_daemon_attributed_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let agenda = open_agenda(dir.path());
        let mut routes = StdHashMap::new();
        routes.insert(token_route().0, token_route().1);
        routes.insert(
            ("GET".to_string(), "/repos/o/r/pulls".to_string()),
            pulls_route(
                serde_json::json!([pull(1, "first", false), pull(2, "second", true)]),
                "e1",
            ),
        );
        let fixture = spawn_fixture(routes).await;
        let client = GithubAppClient::new(&fixture.base, test_credentials()).unwrap();
        let mut state = ScannerState::default();
        let tier1 = crate::github_pr::join::Tier1Cache::default();
        let summary = scan_once(&agenda, &client, &["o/r".to_string()], &mut state, &tier1).await;
        assert_eq!(summary.parked, 2);
        assert!(summary.errors.is_empty());

        let snapshot = agenda.snapshot();
        let hub = snapshot
            .iter()
            .find(|i| i.tags.iter().any(|t| t == PRS_HUB_TAG))
            .expect("hub exists");
        let anchors: Vec<_> = snapshot
            .iter()
            .filter(|i| i.tags.iter().any(|t| t == PR_TAG))
            .collect();
        assert_eq!(anchors.len(), 2);
        for anchor in anchors {
            assert_eq!(anchor.kind, AgendaKind::Task);
            assert_eq!(anchor.provenance.kind.as_deref(), Some("daemon"));
            assert_eq!(anchor.provenance.source.as_deref(), Some(SCANNER_SOURCE));
            assert_eq!(
                anchor.part_of.as_ref().map(|p| p.parent_id.as_str()),
                Some(hub.id.as_str())
            );
            assert!(anchor
                .refs
                .iter()
                .any(|r| parse_pr_locator(&r.locator).is_some()));
            assert!(anchor.title.contains("r#"));
        }
    }

    /// THE load-bearing acceptance (Track PR ruling 7 rider): once the
    /// mirror converges, re-scans write NOTHING — not on a 304, not on
    /// a fresh list whose membership is unchanged while every tier-1
    /// field flipped (the CI-flip sequence), and not from a fresh
    /// scanner instance (restart). The op log is compared byte for
    /// byte.
    #[tokio::test]
    async fn converged_rescans_and_state_churn_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let agenda = open_agenda(dir.path());
        let mut routes = StdHashMap::new();
        routes.insert(token_route().0, token_route().1);
        routes.insert(
            ("GET".to_string(), "/repos/o/r/pulls".to_string()),
            pulls_route(
                serde_json::json!([pull(1, "first", false), pull(2, "second", false)]),
                "e1",
            ),
        );
        let fixture = spawn_fixture(routes).await;
        let client = GithubAppClient::new(&fixture.base, test_credentials()).unwrap();
        let mut state = ScannerState::default();
        let tier1 = crate::github_pr::join::Tier1Cache::default();
        scan_once(&agenda, &client, &["o/r".to_string()], &mut state, &tier1).await;
        let converged = log_bytes(dir.path());
        assert!(!converged.is_empty());

        // Pass 2: 304 path (same validator).
        let summary = scan_once(&agenda, &client, &["o/r".to_string()], &mut state, &tier1).await;
        assert_eq!(summary, ScanSummary::default());
        assert_eq!(log_bytes(dir.path()), converged, "304 pass wrote ops");

        // Pass 3: the CI-flip sequence — same membership, every tier-1
        // field different (titles, drafts, branches), fresh ETag so the
        // list is actually re-served and re-planned.
        fixture.set_route(
            "GET",
            "/repos/o/r/pulls",
            pulls_route(
                serde_json::json!([
                    pull(1, "totally retitled", true),
                    pull(2, "also different now", true),
                ]),
                "e2-after-ci-flip",
            ),
        );
        let summary = scan_once(&agenda, &client, &["o/r".to_string()], &mut state, &tier1).await;
        assert_eq!(summary, ScanSummary::default());
        assert_eq!(
            log_bytes(dir.path()),
            converged,
            "state churn reached the diary"
        );

        // Pass 4: a fresh scanner instance (restart — no ETags, no hub
        // cache) reconstructs everything from the fold and converges.
        let mut fresh = ScannerState::default();
        let summary = scan_once(&agenda, &client, &["o/r".to_string()], &mut fresh, &tier1).await;
        assert_eq!(summary, ScanSummary::default());
        assert_eq!(log_bytes(dir.path()), converged, "restart re-parked");
    }

    #[tokio::test]
    async fn merge_completes_unplaces_and_reopen_resurrects() {
        let dir = tempfile::tempdir().unwrap();
        let agenda = open_agenda(dir.path());
        let mut routes = StdHashMap::new();
        routes.insert(token_route().0, token_route().1);
        routes.insert(
            ("GET".to_string(), "/repos/o/r/pulls".to_string()),
            pulls_route(
                serde_json::json!([pull(1, "first", false), pull(2, "second", false)]),
                "e1",
            ),
        );
        let fixture = spawn_fixture(routes).await;
        let client = GithubAppClient::new(&fixture.base, test_credentials()).unwrap();
        let mut state = ScannerState::default();
        let tier1 = crate::github_pr::join::Tier1Cache::default();
        scan_once(&agenda, &client, &["o/r".to_string()], &mut state, &tier1).await;

        // PR 2 merges: it leaves the open list; the detail serves the
        // terminal state.
        fixture.set_route(
            "GET",
            "/repos/o/r/pulls",
            pulls_route(serde_json::json!([pull(1, "first", false)]), "e2"),
        );
        fixture.set_route(
            "GET",
            "/repos/o/r/pulls/2",
            (
                200,
                Vec::new(),
                serde_json::json!({
                    "state": "closed",
                    "merged": true,
                    "merge_commit_sha": "abc123def456",
                    "merged_at": "2026-07-24T19:00:00Z",
                })
                .to_string(),
            ),
        );
        let summary = scan_once(&agenda, &client, &["o/r".to_string()], &mut state, &tier1).await;
        assert_eq!(summary.completed, 1);

        let snapshot = agenda.snapshot();
        let anchor = snapshot
            .iter()
            .find(|i| i.refs.iter().any(|r| r.locator.ends_with("/pull/2")))
            .expect("anchor exists");
        let anchor_id = anchor.id.clone();
        assert_eq!(anchor.status, AgendaStatus::Done);
        assert!(
            anchor.part_of.is_none(),
            "completion unplaces (hub = open shelf)"
        );
        assert!(anchor
            .annotations
            .iter()
            .any(|n| n.text.starts_with("PR merged (abc123def")
                && n.source.as_deref() == Some(SCANNER_SOURCE)));
        let items_after_merge = snapshot.len();

        // The PR reopens on GitHub: the SAME anchor resurrects — no
        // duplicate is ever parked against an any-status key.
        fixture.set_route(
            "GET",
            "/repos/o/r/pulls",
            pulls_route(
                serde_json::json!([pull(1, "first", false), pull(2, "second", false)]),
                "e3",
            ),
        );
        let summary = scan_once(&agenda, &client, &["o/r".to_string()], &mut state, &tier1).await;
        assert_eq!(summary.reopened, 1);
        assert_eq!(summary.parked, 0);
        let snapshot = agenda.snapshot();
        assert_eq!(snapshot.len(), items_after_merge, "no duplicate anchor");
        let anchor = snapshot.iter().find(|i| i.id == anchor_id).unwrap();
        assert_eq!(anchor.status, AgendaStatus::Open);
        assert!(anchor.part_of.is_some(), "reopen re-places under the hub");
    }

    #[tokio::test]
    async fn retired_stays_retired_with_exactly_one_note() {
        let dir = tempfile::tempdir().unwrap();
        let agenda = open_agenda(dir.path());
        let mut routes = StdHashMap::new();
        routes.insert(token_route().0, token_route().1);
        routes.insert(
            ("GET".to_string(), "/repos/o/r/pulls".to_string()),
            pulls_route(serde_json::json!([pull(7, "contested", false)]), "e1"),
        );
        let fixture = spawn_fixture(routes).await;
        let client = GithubAppClient::new(&fixture.base, test_credentials()).unwrap();
        let mut state = ScannerState::default();
        let tier1 = crate::github_pr::join::Tier1Cache::default();
        scan_once(&agenda, &client, &["o/r".to_string()], &mut state, &tier1).await;
        let anchor_id = agenda
            .snapshot()
            .iter()
            .find(|i| i.tags.iter().any(|t| t == PR_TAG))
            .unwrap()
            .id
            .clone();

        // The owner retires the anchor — an owner act the mirror never
        // overrides.
        agenda
            .apply(
                AgendaCommand::Retire {
                    id: anchor_id.clone(),
                    source: None,
                },
                Some(crate::agenda::AgendaActor {
                    principal: Some("principal:test:owner".into()),
                    session_id: None,
                    kind: Some("dashboard".into()),
                }),
            )
            .unwrap();

        // Two more passes with the PR still open: the anchor stays
        // retired and gains exactly one note, ever.
        for etag in ["e2", "e3"] {
            fixture.set_route(
                "GET",
                "/repos/o/r/pulls",
                pulls_route(serde_json::json!([pull(7, "contested", false)]), etag),
            );
            scan_once(&agenda, &client, &["o/r".to_string()], &mut state, &tier1).await;
        }
        let snapshot = agenda.snapshot();
        let anchor = snapshot.iter().find(|i| i.id == anchor_id).unwrap();
        assert_eq!(anchor.status, AgendaStatus::Retired);
        let notes = anchor
            .annotations
            .iter()
            .filter(|n| n.text == RETIRED_NOTE)
            .count();
        assert_eq!(notes, 1, "the retired note is once-ever");
    }
}

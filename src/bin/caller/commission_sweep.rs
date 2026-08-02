//! The unfinished-commission boot sweep: the readopt pass's
//! commission-lens complement.
//!
//! Boot auto-readopt resumes what died MID-WORK — the transport lens
//! (`running`/`interrupted` metas, pending limit parks, the dead boot's
//! fail-closed occurrences) — and idle sessions stay down by ruled
//! design. But "idle" and "finished" are different claims: a seat whose
//! commission is unfinished (occurrence started, never attested, never
//! deliberately terminaled) looks exactly like a finished one to the
//! mid-work lens, so a crash-boot used to strand paused commissioned
//! work silently (three live specimens, 2026-07-29/30). This sweep runs
//! ONCE per boot, inside the readopt pass after the ordinary mid-work
//! loop, and keys on UNFINISHED COMMISSIONS, never idleness — the
//! idle-stays-down law is untouched for everything without agenda debt.
//!
//! The classifier is the AO safe-to-stop conjunction read at boot (the
//! grid envelope's rule: safe-to-stop is fail-closed from durable
//! journal facts alone — process state can never talk the debt away).
//! NOT safe to stop AND not running = stranded: an open item's effect
//! whose `last_run` a crash left `unknown`, unattested, with no live
//! wrapper anywhere in its resume lineage. Everything derives from the
//! item store's fold, the occurrence journal, and the shared lineage
//! walker — the sweep keeps no bookkeeping of its own.
//!
//! Three lanes, nothing else:
//! - **Wake** — occurrences THIS boot's recovery fail-closed (the
//!   readopt seeds): the standard delivery-aware
//!   continue-where-you-left-off resume, through the readopt guard
//!   ladder under [`crate::boot_readopt::ResumeLens::OpenCommission`] —
//!   identical rungs (owner stop, staleness, live tip, admission CAS)
//!   except that a concluded/idle lineage tip resumes instead of
//!   staying down, because the commission — not the session status —
//!   is the question. Bounded by the agenda streak brake and the shared
//!   per-boot cap; the scheduler's readopt watch re-keys the occurrence
//!   onto the admitted successor exactly as for ordinary readopts, so
//!   the woken seat can still attest.
//! - **List** — what the sweep must not or cannot wake, parked in ONE
//!   needs-you agenda task (found or created by
//!   [`COMMISSION_SWEEP_TAG`], annotated once per boot) beside one
//!   attention notification: `failed` runs (a deliberate terminal —
//!   NEVER auto re-fired; the owner re-arms by re-approving the
//!   unchanged digest), suspended series (the brake), strandings older
//!   than this boot, spawnless fail-closes, and wakes that were
//!   refused or died inside the verify window. Never silence, never a
//!   re-fire.
//! - **Settled** (silent) — attested runs (the seat self-reported; even
//!   `blocked` is a delivered outcome, not a stranding), `completed`
//!   terminals, items no longer open, effects the planner will fire
//!   again (`next_fire_ms`: an armed standing series or a trigger
//!   walk's bounded regeneration carries its own continuity — waking
//!   beside it would race the planner), and lineages with a live
//!   wrapper or a resume already dispatched this boot.

use crate::agenda::{
    AgendaActor, AgendaCommand, AgendaHandle, AgendaItem, AgendaKind, AgendaStatus,
};
use crate::boot_readopt::{
    decide_candidate_with_lens, MidWorkClass, ReadoptCandidate, ReadoptDecision, ResumeLens,
};
use crate::event::{AppEvent, ControlMsg, EventBus};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The stable tag naming THE needs-you task (find-or-create key, the
/// PRs-hub pattern): one open item carries every boot's stranded list.
pub(crate) const COMMISSION_SWEEP_TAG: &str = "commission-sweep";

/// The sweep's self-described `source` label on its agenda writes.
const COMMISSION_SWEEP_SOURCE: &str = "commission-sweep";

const NEEDS_YOU_TITLE: &str = "Stranded commissions need attention";

const NEEDS_YOU_BODY: &str = "Commissioned sessions stranded across daemon crash-boots land \
     here when the boot sweep cannot wake them: failed runs (never auto re-fired), suspended \
     series (the failure-streak brake), strandings older than the latest boot, and wakes that \
     could not be dispatched. Re-approve an item's manifest to re-arm it (one click on the \
     unchanged digest), resume its session from the Sessions tab, or retire the item. Each \
     boot's sweep appends the current list below; nothing on it is ever re-fired \
     automatically.";

/// Listed entries named in full per annotation/notification; overflow is
/// counted, never dropped silently.
const LIST_DETAIL_CAP: usize = 16;

/// One stranded commission, as the classifier named it — derived
/// entirely from the item store's fold (`effect.last_run`), never
/// stored anywhere by the sweep.
#[derive(Debug, Clone)]
pub(crate) struct CommissionRef {
    pub(crate) occurrence_id: String,
    pub(crate) item_id: String,
    /// Item-authored text: render quoted, like every title.
    pub(crate) item_title: String,
    /// The write-back state that made it a candidate: `failed` |
    /// `unknown`.
    pub(crate) run_state: String,
    /// The journal lineage tip (`last_run.session_id`) — `None` for a
    /// spawnless fail-close (the dispatch died with the process).
    pub(crate) session_id: Option<String>,
}

/// The classifier's per-commission verdict. Settled commissions are not
/// represented — absence claims nothing.
#[derive(Debug)]
pub(crate) enum CommissionStanding {
    /// Stranded by this boot's classification with a resume lineage to
    /// wake: dispatch the standard continue-where-you-left-off resume.
    Wake(CommissionRef),
    /// Owner lane: listed in the needs-you task with the reason —
    /// never woken, never re-fired.
    List(CommissionRef, String),
    /// A `completed` transport terminal with NO attestation: the
    /// write-back records the transport ending, not arc completion (the
    /// 19:00 limit-wave class), so it may dress an interrupted mid-arc
    /// seat as done. Whether the arc concluded needs the session's own
    /// durable status — `plan_sweep` consults it and LISTS the
    /// interrupted ones (never a wake: by sweep time the stranding
    /// predates this boot, the stale-stranding law's lane); every other
    /// completed run stays settled.
    CompletedUnattested(CommissionRef),
}

/// The open-commission conjunction (the AO safe-to-stop rule read at
/// boot), pure over the decorated item fold. `fresh_fail_closes` is the
/// set of occurrence ids THIS boot's recovery fail-closed (the readopt
/// seeds): only those are wake-eligible — the wake resumes what this
/// crash stranded, while older strandings (whose 15-minute re-key watch
/// is long gone, so a woken seat could never attest) go to the owner
/// lane instead of looping a wake every boot.
pub(crate) fn classify_commissions(
    items: &[AgendaItem],
    fresh_fail_closes: &HashSet<String>,
) -> Vec<CommissionStanding> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut standings = Vec::new();
    for item in items {
        if item.status != AgendaStatus::Open {
            continue; // the commission was completed or retired — settled
        }
        for effect in &item.effects {
            let Some(run) = effect.last_run.as_ref() else {
                continue; // never fired — nothing started, nothing stranded
            };
            if !seen.insert(run.occurrence_id.clone()) {
                continue;
            }
            if run.attestation.is_some() {
                // The fired session self-reported (achieved, partial,
                // blocked, or abandoned alike): a delivered outcome,
                // not a silent stranding — idle-done stays down.
                continue;
            }
            if effect.next_fire_ms.is_some() {
                // The planner will fire this effect again (armed
                // standing series, trigger regeneration): machinery
                // already carries the continuity — waking the old seat
                // beside it would race the next fire.
                continue;
            }
            let cref = CommissionRef {
                occurrence_id: run.occurrence_id.clone(),
                item_id: item.id.clone(),
                item_title: item.title.clone(),
                run_state: run.state.clone(),
                session_id: run.session_id.clone(),
            };
            if run.state == "completed" {
                // The interrupted-mid-arc blindness (2026-08-01 owner
                // specimen): "completed" is a TRANSPORT ending, and
                // unattested it proves nothing about the arc — the
                // session's own status decides in plan_sweep.
                standings.push(CommissionStanding::CompletedUnattested(cref));
                continue;
            }
            if !matches!(run.state.as_str(), "failed" | "unknown") {
                // `started` = a live (or co-homed) run owns it;
                // `missed` = a settled terminal with its own lane.
                continue;
            }
            if run.state == "failed" {
                standings.push(CommissionStanding::List(
                    cref,
                    "the run failed — never auto re-fired; re-approve the manifest to re-arm"
                        .to_string(),
                ));
            } else if effect.suspended() {
                standings.push(CommissionStanding::List(
                    cref,
                    "standing series suspended after repeated failures — re-approve the \
                     unchanged digest to re-arm"
                        .to_string(),
                ));
            } else if !fresh_fail_closes.contains(&run.occurrence_id) {
                standings.push(CommissionStanding::List(
                    cref,
                    "stranded before this boot — resume the session by hand or re-approve \
                     the manifest"
                        .to_string(),
                ));
            } else if cref.session_id.is_none() {
                standings.push(CommissionStanding::List(
                    cref,
                    "crashed before its session dispatched — re-approve the manifest to \
                     reschedule"
                        .to_string(),
                ));
            } else {
                standings.push(CommissionStanding::Wake(cref));
            }
        }
    }
    standings
}

/// The sweep's dispatch plan: wire messages for the wake lane, reasons
/// for the owner lane. Wakes carry the ladder candidate session id so
/// the verify pass can walk its lineage.
#[derive(Debug, Default)]
pub(crate) struct SweepPlan {
    pub(crate) wakes: Vec<(CommissionRef, String, Box<ControlMsg>)>,
    pub(crate) listed: Vec<(CommissionRef, String)>,
}

/// Decide deliverability for each wake-shaped standing: resolve the
/// occurrence's resume lineage (journal `started_history` ∪ the
/// write-back tip, closed over the shared walker), drop candidates a
/// live wrapper or an already-dispatched resume covers, and run the
/// readopt guard ladder under the open-commission lens. Undispatchable
/// wakes fall to the owner lane — never silence.
pub(crate) fn plan_sweep(
    home: &Path,
    standings: Vec<CommissionStanding>,
    lineage_history: &HashMap<String, Vec<String>>,
    now_secs: u64,
    live: &HashSet<String>,
    dispatched_sessions: &HashSet<String>,
) -> SweepPlan {
    let mut plan = SweepPlan::default();
    for standing in standings {
        let (cref, session, wake) = match standing {
            CommissionStanding::List(cref, reason) => {
                plan.listed.push((cref, reason));
                continue;
            }
            CommissionStanding::Wake(cref) => match cref.session_id.clone() {
                Some(session) => (cref, session, true),
                // Unreachable by classification; fail toward the owner
                // lane rather than dropping it.
                None => {
                    plan.listed.push((
                        cref,
                        "wake undispatchable — no session recorded for the occurrence".to_string(),
                    ));
                    continue;
                }
            },
            CommissionStanding::CompletedUnattested(cref) => {
                let Some(session) = cref.session_id.clone() else {
                    continue; // no session ever ran — nothing mid-arc
                };
                if !session_ended_interrupted(home, &session) {
                    // The transport ending stands unchallenged: the
                    // session's own durable status shows no
                    // interrupted-mid-arc evidence — settled, today's
                    // behavior.
                    continue;
                }
                (cref, session, false)
            }
        };
        let mut seeds: Vec<String> = lineage_history
            .get(&cref.occurrence_id)
            .cloned()
            .unwrap_or_default();
        if !seeds.iter().any(|seed| seed == &session) {
            seeds.push(session.clone());
        }
        let seed_refs: Vec<&str> = seeds.iter().map(String::as_str).collect();
        let lineage =
            crate::session_supervisor::resume_lineage::resolve_resume_lineage(home, &seed_refs);
        let mut members: HashSet<&str> = lineage
            .wrapper_records
            .iter()
            .map(|record| record.intendant_session_id.as_str())
            .collect();
        members.extend(seeds.iter().map(String::as_str));
        if members.iter().any(|id| live.contains(*id)) {
            // Not stranded after all: a wrapper in the lineage is live
            // (a co-homed daemon's seat, a manual resume) — the running
            // session owns the outcome. Silent, like every live lineage.
            continue;
        }
        if members.iter().any(|id| dispatched_sessions.contains(*id)) {
            // The mid-work pass already dispatched this lineage's
            // resume; the commission rides that seat and the readopt
            // summary reports its outcome.
            continue;
        }
        if !wake {
            // The interrupted-mid-arc lane: a "completed"-dressed run
            // whose seat's durable status says interrupted, with no
            // live or already-resumed lineage — LISTED, never woken
            // (prior-boot by construction; the stale-stranding law).
            plan.listed.push((
                cref,
                "ended interrupted mid-task without attesting — the arc looks unconcluded; \
                 resume the session by hand or re-approve the manifest"
                    .to_string(),
            ));
            continue;
        }
        let mut candidate = ReadoptCandidate {
            session_id: session.clone(),
            class: MidWorkClass::Agenda,
            suspended: false,
            activity_secs: 0,
        };
        if let Some(dir) =
            crate::session_log::SessionLog::find_session_by_id_in_home(home, &session)
        {
            candidate.activity_secs = crate::boot_readopt::activity_mtime_secs(&dir);
        }
        match decide_candidate_with_lens(
            home,
            &candidate,
            now_secs,
            live,
            ResumeLens::OpenCommission,
        ) {
            ReadoptDecision::Readopt(resume) => plan.wakes.push((cref, session, resume)),
            ReadoptDecision::LeftDead(reason) => plan
                .listed
                .push((cref, format!("wake undispatchable — {reason}"))),
        }
    }
    plan
}

/// The per-boot annotation appended to the needs-you task: the CURRENT
/// stranded list, one line per commission. Pure text — data, never
/// instructions (bodies doctrine). Stable across identical boots so the
/// caller can skip consecutive duplicates.
pub(crate) fn stranded_annotation(listed: &[(CommissionRef, String)]) -> String {
    let mut lines = vec![format!(
        "Boot sweep: {} stranded commission(s) need you.",
        listed.len()
    )];
    for (cref, reason) in listed.iter().take(LIST_DETAIL_CAP) {
        lines.push(format!(
            "- {} \u{201c}{}\u{201d} (item {}, occurrence {}) — {}",
            cref.run_state,
            cref.item_title,
            short_id(&cref.item_id),
            short_id(&cref.occurrence_id),
            reason
        ));
    }
    if listed.len() > LIST_DETAIL_CAP {
        lines.push(format!("…and {} more.", listed.len() - LIST_DETAIL_CAP));
    }
    lines.join("\n")
}

/// Find or create THE needs-you task (oldest open item carrying the
/// tag — ULID order, the PRs-hub convention) and append this boot's
/// list, skipping an identical consecutive annotation so a
/// crash-looping daemon never stacks copies of the same list.
fn park_needs_you(
    handle: &AgendaHandle,
    listed: &[(CommissionRef, String)],
) -> Result<String, String> {
    let snapshot = handle.snapshot();
    let mut anchors: Vec<&AgendaItem> = snapshot
        .iter()
        .filter(|item| {
            item.status == AgendaStatus::Open
                && item.tags.iter().any(|tag| tag == COMMISSION_SWEEP_TAG)
        })
        .collect();
    anchors.sort_by(|a, b| a.id.cmp(&b.id));
    let text = stranded_annotation(listed);
    let item_id = match anchors.first() {
        Some(item) => {
            if item
                .annotations
                .last()
                .is_some_and(|note| note.text == text)
            {
                return Ok(item.id.clone());
            }
            item.id.clone()
        }
        None => {
            handle
                .apply(
                    AgendaCommand::Add {
                        kind: AgendaKind::Task,
                        title: NEEDS_YOU_TITLE.to_string(),
                        body: NEEDS_YOU_BODY.to_string(),
                        tags: vec![COMMISSION_SWEEP_TAG.to_string()],
                        due_ms: None,
                        source: Some(COMMISSION_SWEEP_SOURCE.to_string()),
                        refs: Vec::new(),
                    },
                    Some(AgendaActor::daemon()),
                )
                .map_err(|err| format!("park needs-you task: {err}"))?
                .id
        }
    };
    handle
        .apply(
            AgendaCommand::Annotate {
                id: item_id.clone(),
                text,
                source: Some(COMMISSION_SWEEP_SOURCE.to_string()),
            },
            Some(AgendaActor::daemon()),
        )
        .map_err(|err| format!("annotate needs-you task: {err}"))?;
    Ok(item_id)
}

/// One visible summary per boot (id keyed by boot so repeats never
/// stack), silence when the sweep found nothing. Only VERIFIED
/// continuations count as woken — the caller has already reclassified
/// dispatches that died into `listed`.
pub(crate) fn sweep_notification(
    boot_id: &str,
    woken: &[(CommissionRef, String)],
    listed: &[(CommissionRef, String)],
    needs_you_item: Option<&str>,
) -> Option<AppEvent> {
    if woken.is_empty() && listed.is_empty() {
        return None;
    }
    let mut title_parts = Vec::new();
    if !woken.is_empty() {
        title_parts.push(format!("{} session(s) woken", woken.len()));
    }
    if !listed.is_empty() {
        title_parts.push(format!("{} need(s) you", listed.len()));
    }
    let mut lines = Vec::new();
    if !woken.is_empty() {
        lines.push(format!(
            "Woken to continue: {}.",
            woken
                .iter()
                .take(LIST_DETAIL_CAP)
                .map(|(cref, session)| {
                    format!(
                        "\u{201c}{}\u{201d} ({})",
                        cref.item_title,
                        short_id(session)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !listed.is_empty() {
        lines.push(format!(
            "Needs you (never auto re-fired): {}.",
            listed
                .iter()
                .take(LIST_DETAIL_CAP)
                .map(|(cref, reason)| {
                    format!("\u{201c}{}\u{201d} — {}", cref.item_title, reason)
                })
                .collect::<Vec<_>>()
                .join("; ")
        ));
        match needs_you_item {
            Some(id) => lines.push(format!(
                "Full list on agenda task {} (\u{201c}{NEEDS_YOU_TITLE}\u{201d}).",
                short_id(id)
            )),
            None => lines.push(
                "The list could not be parked on the agenda — see the daemon log.".to_string(),
            ),
        }
    }
    Some(AppEvent::UserNotification {
        session_id: None,
        id: format!("commission-sweep-{boot_id}"),
        title: Some(format!("Commission sweep: {}", title_parts.join(", "))),
        text: lines.join(" "),
        urgency: crate::types::NotificationUrgency::Attention,
        ts: now_ms(),
    })
}

/// Park the owner lane and emit the boot summary — the sweep's entire
/// visible surface. A missing agenda handle degrades to the
/// notification alone (the log names the loss); parking failures never
/// swallow the notification.
pub(crate) fn report_sweep(
    bus: &EventBus,
    handle: Option<&AgendaHandle>,
    boot_id: &str,
    woken: &[(CommissionRef, String)],
    listed: &[(CommissionRef, String)],
) {
    if woken.is_empty() && listed.is_empty() {
        return;
    }
    let mut needs_you_item = None;
    if !listed.is_empty() {
        match handle {
            Some(handle) => match park_needs_you(handle, listed) {
                Ok(id) => needs_you_item = Some(id),
                Err(err) => {
                    eprintln!("[commission-sweep] {err} — the notification carries the list");
                }
            },
            None => {
                eprintln!("[commission-sweep] agenda handle unavailable — stranded list not parked")
            }
        }
    }
    if let Some(notification) =
        sweep_notification(boot_id, woken, listed, needs_you_item.as_deref())
    {
        bus.send(notification);
    }
}

/// Whether the session's own durable status says it ended interrupted —
/// the arc evidence a "completed" transport terminal cannot see. Read
/// through the shared meta reader; an unreadable meta claims nothing.
fn session_ended_interrupted(home: &Path, session_id: &str) -> bool {
    crate::boot_readopt::session_meta_for(home, session_id)
        .and_then(|meta| meta.status)
        .is_some_and(|status| status == "interrupted")
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_log::SessionLog;
    use serde_json::json;
    use std::path::PathBuf;

    fn logs_root(home: &Path) -> PathBuf {
        crate::platform::intendant_home_in(home).join("logs")
    }

    /// A wrapper log dir announcing its backend conversation(s) — the
    /// same eager-identity write live wrappers perform (meta +
    /// wrapper-index row), the boot_readopt test fixture.
    fn announce_ids(home: &Path, wrapper: &str, backend_ids: &[&str]) {
        let mut log = SessionLog::open(logs_root(home).join(wrapper)).unwrap();
        log.write_meta(None, None);
        for backend_id in backend_ids {
            log.session_identity(wrapper, "claude-code", backend_id);
        }
    }

    fn write_status(home: &Path, session: &str, status: &str) {
        let dir = logs_root(home).join(session);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session_meta.json"),
            serde_json::to_string_pretty(&json!({
                "session_id": session,
                "created_at": "2026-07-28T00:00:00",
                "status": status,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// A folded item exactly as the classifier sees it, built through
    /// the DTO's own serde — vocabulary drift breaks here, visibly.
    fn item(id: &str, status: &str, effect: serde_json::Value) -> AgendaItem {
        serde_json::from_value(json!({
            "id": id,
            "kind": "task",
            "title": format!("commission {id}"),
            "body": "",
            "tags": [],
            "provenance": {"created_ms": 1},
            "status": status,
            "updated_ms": 1,
            "effects": [effect],
        }))
        .expect("test item deserializes")
    }

    fn effect(
        occ: &str,
        state: &str,
        session: Option<&str>,
        run_extra: serde_json::Value,
        effect_extra: serde_json::Value,
    ) -> serde_json::Value {
        let mut run = json!({"occurrence_id": occ, "state": state, "at_ms": 1});
        if let Some(session) = session {
            run["session_id"] = session.into();
        }
        if let serde_json::Value::Object(extra) = run_extra {
            for (key, value) in extra {
                run[key] = value;
            }
        }
        let mut effect = json!({
            "effect_id": format!("ef-{occ}"),
            "manifest": {"goal": "g", "fire_at_ms": 1},
            "digest": "d",
            "proposed_ms": 1,
            "last_run": run,
        });
        if let serde_json::Value::Object(extra) = effect_extra {
            for (key, value) in extra {
                effect[key] = value;
            }
        }
        effect
    }

    fn wake_occurrences(standings: &[CommissionStanding]) -> Vec<String> {
        standings
            .iter()
            .filter_map(|standing| match standing {
                CommissionStanding::Wake(cref) => Some(cref.occurrence_id.clone()),
                CommissionStanding::List(..) => None,
            })
            .collect()
    }

    fn listed_occurrences(standings: &[CommissionStanding]) -> Vec<(String, String)> {
        standings
            .iter()
            .filter_map(|standing| match standing {
                CommissionStanding::List(cref, reason) => {
                    Some((cref.occurrence_id.clone(), reason.clone()))
                }
                CommissionStanding::Wake(_) | CommissionStanding::CompletedUnattested(_) => None,
            })
            .collect()
    }

    fn completed_unattested_occurrences(standings: &[CommissionStanding]) -> Vec<String> {
        standings
            .iter()
            .filter_map(|standing| match standing {
                CommissionStanding::CompletedUnattested(cref) => {
                    Some(cref.occurrence_id.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// The classifier is the open-commission conjunction, every leg
    /// pinned: only an OPEN item's UNATTESTED `unknown` run fail-closed
    /// by THIS boot with a recorded session wakes. Everything else is
    /// settled (absent) or listed.
    #[test]
    fn classifier_is_the_open_commission_conjunction() {
        let fresh: HashSet<String> = [
            "occ-wake",
            "occ-attested",
            "occ-completed",
            "occ-started",
            "occ-done-item",
            "occ-retired-item",
            "occ-armed",
            "occ-spawnless",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let items = vec![
            item(
                "i-wake",
                "open",
                effect("occ-wake", "unknown", Some("sess-1"), json!({}), json!({})),
            ),
            item(
                "i-completed",
                "open",
                effect(
                    "occ-completed",
                    "completed",
                    Some("sess-3"),
                    json!({}),
                    json!({}),
                ),
            ),
            item(
                "i-started",
                "open",
                effect(
                    "occ-started",
                    "started",
                    Some("sess-4"),
                    json!({}),
                    json!({}),
                ),
            ),
            item(
                "i-done",
                "done",
                effect(
                    "occ-done-item",
                    "unknown",
                    Some("sess-5"),
                    json!({}),
                    json!({}),
                ),
            ),
            item(
                "i-retired",
                "retired",
                effect(
                    "occ-retired-item",
                    "unknown",
                    Some("sess-6"),
                    json!({}),
                    json!({}),
                ),
            ),
            item(
                "i-armed",
                "open",
                effect(
                    "occ-armed",
                    "unknown",
                    Some("sess-7"),
                    json!({}),
                    // The planner will fire this effect again: machinery
                    // already carries the continuity.
                    json!({"next_fire_ms": 12345u64}),
                ),
            ),
            item(
                "i-spawnless",
                "open",
                effect("occ-spawnless", "unknown", None, json!({}), json!({})),
            ),
        ];
        let standings = classify_commissions(&items, &fresh);
        assert_eq!(
            wake_occurrences(&standings),
            vec!["occ-wake".to_string()],
            "exactly the conjunction wakes: open ∧ unattested ∧ unknown ∧ fresh ∧ session"
        );
        let listed = listed_occurrences(&standings);
        assert_eq!(
            listed.len(),
            1,
            "of the settled shapes only the spawnless fail-close is listed: {listed:?}"
        );
        assert!(
            listed[0].0 == "occ-spawnless" && listed[0].1.contains("before its session"),
            "the spawnless fail-close goes to the owner lane: {listed:?}"
        );
        assert_eq!(
            completed_unattested_occurrences(&standings),
            vec!["occ-completed".to_string()],
            "an unattested completed run on an OPEN item is never ignorable — \
             plan_sweep consults the seat's own status"
        );
    }

    /// The next-fire guard bounds the interrupted-mid-arc consult
    /// exactly as it bounds the wake lane: an armed standing series'
    /// completed occurrence stays silent — the planner carries the
    /// continuity.
    #[test]
    fn armed_series_completed_run_stays_settled() {
        let fresh = HashSet::new();
        let items = vec![item(
            "i-armed-completed",
            "open",
            effect(
                "occ-armed-completed",
                "completed",
                Some("sess-1"),
                json!({}),
                json!({"next_fire_ms": 12345u64}),
            ),
        )];
        let standings = classify_commissions(&items, &fresh);
        assert!(
            standings.is_empty(),
            "an armed series' completed run is settled: {standings:?}"
        );
    }

    /// The idle-done exclusion by name: an attested run — ANY outcome,
    /// `blocked` and `abandoned` included — is a delivered self-report,
    /// not a silent stranding, and the seat stays down.
    #[test]
    fn idle_done_seats_stay_down() {
        let fresh: HashSet<String> = ["occ-a", "occ-b", "occ-c"]
            .into_iter()
            .map(String::from)
            .collect();
        let items = vec![
            item(
                "i-achieved",
                "open",
                effect(
                    "occ-a",
                    "unknown",
                    Some("sess-1"),
                    json!({"attestation": {"outcome": "achieved", "at_ms": 2}}),
                    json!({}),
                ),
            ),
            item(
                "i-blocked",
                "open",
                effect(
                    "occ-b",
                    "failed",
                    Some("sess-2"),
                    json!({"attestation": {"outcome": "blocked", "at_ms": 2}}),
                    json!({}),
                ),
            ),
            // An attested completed run never reaches the
            // interrupted-mid-arc consult: the self-report settles it.
            item(
                "i-attested-completed",
                "open",
                effect(
                    "occ-c",
                    "completed",
                    Some("sess-3"),
                    json!({"attestation": {"outcome": "achieved", "at_ms": 2}}),
                    json!({}),
                ),
            ),
        ];
        let standings = classify_commissions(&items, &fresh);
        assert!(
            standings.is_empty(),
            "attested runs are settled — never woken, never listed: {:?}",
            listed_occurrences(&standings)
        );
    }

    /// FAILED occurrences are LISTED, never re-fired: a `failed`
    /// terminal is a deliberate outcome — the remedy is the owner's
    /// one-click re-approval, and the sweep never wakes or re-fires it,
    /// fresh or not.
    #[test]
    fn failed_occurrences_are_listed_never_woken() {
        let fresh: HashSet<String> = ["occ-f"].into_iter().map(String::from).collect();
        let items = vec![item(
            "i-failed",
            "open",
            effect("occ-f", "failed", Some("sess-1"), json!({}), json!({})),
        )];
        let standings = classify_commissions(&items, &fresh);
        assert!(
            wake_occurrences(&standings).is_empty(),
            "failed never wakes"
        );
        let listed = listed_occurrences(&standings);
        assert_eq!(listed.len(), 1);
        assert!(
            listed[0].1.contains("never auto re-fired") && listed[0].1.contains("re-approve"),
            "the reason names the re-approval lane: {}",
            listed[0].1
        );
    }

    /// The streak brake bounds the wake exactly as it bounds the
    /// mid-work pass: a suspended standing series is listed with the
    /// re-arm remedy, never stood back up.
    #[test]
    fn streak_brake_bounds_the_wake() {
        let fresh: HashSet<String> = ["occ-s"].into_iter().map(String::from).collect();
        let items = vec![item(
            "i-suspended",
            "open",
            effect(
                "occ-s",
                "unknown",
                Some("sess-1"),
                json!({}),
                json!({
                    "manifest": {"goal": "g", "fire_at_ms": 1, "recurrence": {"every_ms": 3600000u64}},
                    "consecutive_failures": 3,
                }),
            ),
        )];
        let standings = classify_commissions(&items, &fresh);
        assert!(
            wake_occurrences(&standings).is_empty(),
            "a suspended series never wakes"
        );
        let listed = listed_occurrences(&standings);
        assert_eq!(listed.len(), 1);
        assert!(
            listed[0].1.contains("suspended"),
            "the reason names the brake: {}",
            listed[0].1
        );
    }

    /// Strandings older than this boot's classification go to the owner
    /// lane: their re-key watch is long gone (a woken seat could never
    /// attest), so a wake would loop every boot instead of finishing.
    #[test]
    fn stale_boot_strandings_are_listed_not_woken() {
        let fresh = HashSet::new();
        let items = vec![item(
            "i-old",
            "open",
            effect("occ-old", "unknown", Some("sess-1"), json!({}), json!({})),
        )];
        let standings = classify_commissions(&items, &fresh);
        assert!(wake_occurrences(&standings).is_empty());
        let listed = listed_occurrences(&standings);
        assert_eq!(listed.len(), 1);
        assert!(
            listed[0].1.contains("before this boot"),
            "the reason names the era: {}",
            listed[0].1
        );
    }

    /// The needs-you fallback: a wake the guard ladder refuses (here: no
    /// recorded backend conversation) falls to the owner lane with the
    /// ladder's reason — never silence.
    #[test]
    fn undispatchable_wake_falls_to_the_needs_you_lane() {
        let home = tempfile::tempdir().unwrap();
        write_status(home.path(), "sess-1", "idle");
        let standings = vec![CommissionStanding::Wake(CommissionRef {
            occurrence_id: "occ-1".to_string(),
            item_id: "i-1".to_string(),
            item_title: "commission i-1".to_string(),
            run_state: "unknown".to_string(),
            session_id: Some("sess-1".to_string()),
        })];
        let plan = plan_sweep(
            home.path(),
            standings,
            &HashMap::new(),
            crate::session_activity::epoch_seconds(),
            &HashSet::new(),
            &HashSet::new(),
        );
        assert!(plan.wakes.is_empty());
        assert_eq!(plan.listed.len(), 1);
        assert!(
            plan.listed[0].1.contains("wake undispatchable")
                && plan.listed[0].1.contains("no external resume lineage"),
            "the ladder's refusal reaches the owner lane: {}",
            plan.listed[0].1
        );
    }

    /// The commissioning specimen: a commissioned seat killed while
    /// IDLE — concluded meta, conversation recorded — wakes with the
    /// standard continue-where-you-left-off resume on the automatic
    /// lane (the mid-work lens would have left it down).
    #[test]
    fn killed_idle_commission_seat_wakes() {
        let home = tempfile::tempdir().unwrap();
        announce_ids(home.path(), "sess-1", &["b1-conv"]);
        write_status(home.path(), "sess-1", "idle");
        let standings = vec![CommissionStanding::Wake(CommissionRef {
            occurrence_id: "occ-1".to_string(),
            item_id: "i-1".to_string(),
            item_title: "commission i-1".to_string(),
            run_state: "unknown".to_string(),
            session_id: Some("sess-1".to_string()),
        })];
        let plan = plan_sweep(
            home.path(),
            standings,
            &HashMap::new(),
            crate::session_activity::epoch_seconds(),
            &HashSet::new(),
            &HashSet::new(),
        );
        assert!(plan.listed.is_empty(), "nothing to list: {:?}", plan.listed);
        assert_eq!(plan.wakes.len(), 1);
        let (cref, candidate_session, resume) = &plan.wakes[0];
        assert_eq!(cref.occurrence_id, "occ-1");
        assert_eq!(candidate_session, "sess-1");
        match resume.as_ref() {
            ControlMsg::ResumeSession {
                source,
                session_id,
                task,
                fork,
                auto_attach,
                ..
            } => {
                assert_eq!(source, "claude-code");
                assert_eq!(session_id, "b1-conv");
                assert_eq!(
                    task.as_deref(),
                    Some(crate::boot_readopt::COMMISSION_CONTINUATION_TEXT),
                    "the nudge, never the original goal"
                );
                assert!(!fork);
                assert!(
                    auto_attach,
                    "the automatic resume lane — intent Resume, never Revive"
                );
            }
            other => panic!("expected ResumeSession, got {other:?}"),
        }
    }

    /// A live wrapper anywhere in the lineage — or a resume the
    /// mid-work pass already dispatched — settles the commission
    /// silently: the running seat owns the outcome.
    #[test]
    fn live_or_dispatched_lineages_are_settled() {
        let home = tempfile::tempdir().unwrap();
        announce_ids(home.path(), "sess-1", &["b1-conv"]);
        let standing = || {
            vec![CommissionStanding::Wake(CommissionRef {
                occurrence_id: "occ-1".to_string(),
                item_id: "i-1".to_string(),
                item_title: "commission i-1".to_string(),
                run_state: "unknown".to_string(),
                session_id: Some("sess-1".to_string()),
            })]
        };
        let live: HashSet<String> = ["sess-1".to_string()].into_iter().collect();
        let plan = plan_sweep(
            home.path(),
            standing(),
            &HashMap::new(),
            crate::session_activity::epoch_seconds(),
            &live,
            &HashSet::new(),
        );
        assert!(plan.wakes.is_empty() && plan.listed.is_empty());
        let dispatched: HashSet<String> = ["sess-1".to_string()].into_iter().collect();
        let plan = plan_sweep(
            home.path(),
            standing(),
            &HashMap::new(),
            crate::session_activity::epoch_seconds(),
            &HashSet::new(),
            &dispatched,
        );
        assert!(plan.wakes.is_empty() && plan.listed.is_empty());
    }

    /// The interrupted-mid-arc lane end to end: a "completed"-dressed
    /// unattested run whose seat's durable status says interrupted is
    /// LISTED with the arc-unconcluded reason — never woken; a seat
    /// whose status shows a clean end stays settled (today's behavior);
    /// a live lineage settles it silently (the running seat owns it).
    #[test]
    fn completed_unattested_interrupted_seat_is_listed() {
        let home = tempfile::tempdir().unwrap();
        write_status(home.path(), "sess-i", "interrupted");
        write_status(home.path(), "sess-done", "completed");
        let standing = |occ: &str, session: &str| {
            CommissionStanding::CompletedUnattested(CommissionRef {
                occurrence_id: occ.to_string(),
                item_id: "i-1".to_string(),
                item_title: format!("commission {occ}"),
                run_state: "completed".to_string(),
                session_id: Some(session.to_string()),
            })
        };
        let plan = plan_sweep(
            home.path(),
            vec![standing("occ-i", "sess-i"), standing("occ-done", "sess-done")],
            &HashMap::new(),
            crate::session_activity::epoch_seconds(),
            &HashSet::new(),
            &HashSet::new(),
        );
        assert!(plan.wakes.is_empty(), "the lane never wakes");
        assert_eq!(plan.listed.len(), 1, "only the interrupted seat lists");
        assert_eq!(plan.listed[0].0.occurrence_id, "occ-i");
        assert!(
            plan.listed[0].1.contains("interrupted mid-task")
                && plan.listed[0].1.contains("arc looks unconcluded"),
            "the reason names the class: {}",
            plan.listed[0].1
        );

        // A live wrapper in the lineage settles the consult silently.
        let live: HashSet<String> = ["sess-i".to_string()].into_iter().collect();
        let plan = plan_sweep(
            home.path(),
            vec![standing("occ-i", "sess-i")],
            &HashMap::new(),
            crate::session_activity::epoch_seconds(),
            &live,
            &HashSet::new(),
        );
        assert!(plan.wakes.is_empty() && plan.listed.is_empty());
    }

    #[test]
    fn stranded_annotation_names_and_caps() {
        let listed: Vec<(CommissionRef, String)> = (0..20)
            .map(|n| {
                (
                    CommissionRef {
                        occurrence_id: format!("occ-{n:02}"),
                        item_id: format!("item-{n:02}"),
                        item_title: format!("commission {n}"),
                        run_state: "unknown".to_string(),
                        session_id: None,
                    },
                    "a reason".to_string(),
                )
            })
            .collect();
        let text = stranded_annotation(&listed);
        assert!(text.starts_with("Boot sweep: 20 stranded commission(s)"));
        assert_eq!(
            text.lines().count(),
            1 + LIST_DETAIL_CAP + 1,
            "header + capped detail + overflow line"
        );
        assert!(text.ends_with("…and 4 more."));
        assert!(text.contains("\u{201c}commission 0\u{201d}"));
    }

    /// One boot-keyed attention notification; silence when the sweep
    /// found nothing.
    #[test]
    fn sweep_notification_is_boot_keyed_and_attention() {
        assert!(sweep_notification("boot-1", &[], &[], None).is_none());
        let woken = vec![(
            CommissionRef {
                occurrence_id: "occ-1".to_string(),
                item_id: "item-1".to_string(),
                item_title: "commission one".to_string(),
                run_state: "unknown".to_string(),
                session_id: Some("sess-1".to_string()),
            },
            "sess-1".to_string(),
        )];
        let listed = vec![(
            CommissionRef {
                occurrence_id: "occ-2".to_string(),
                item_id: "item-2".to_string(),
                item_title: "commission two".to_string(),
                run_state: "failed".to_string(),
                session_id: None,
            },
            "the run failed — never auto re-fired; re-approve the manifest to re-arm".to_string(),
        )];
        match sweep_notification("boot-1", &woken, &listed, Some("01ITEM")) {
            Some(AppEvent::UserNotification {
                id,
                title,
                text,
                urgency,
                ..
            }) => {
                assert_eq!(id, "commission-sweep-boot-1");
                let title = title.unwrap_or_default();
                assert!(
                    title.contains("1 session(s) woken") && title.contains("1 need(s) you"),
                    "both lanes counted: {title}"
                );
                assert!(text.contains("commission one") && text.contains("commission two"));
                assert!(matches!(
                    urgency,
                    crate::types::NotificationUrgency::Attention
                ));
            }
            other => panic!("expected a UserNotification, got {other:?}"),
        }
    }
}

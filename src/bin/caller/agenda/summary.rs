//! Track AS S4 — the summary projection and the serving-seam derived
//! flags (sealed ruling §4.4, R-AS3).
//!
//! One serving-seam DTO carries exactly what the dashboard's cards
//! render ungated (the render-completeness law): identity, chips,
//! instants, the who-line's session ids, answer text on answered
//! questions, UNCLEARED blocker criteria, the three edge id-lists,
//! slim refs (PR-chip/citation keying), slim effect state, and the
//! cross-item flags — `blocked` and `frontier` — that ctl, the SPA,
//! and the triage skill each re-derived until now. Bodies and
//! annotation threads stay full-item material (the inspector fetches
//! the item route). Everything here is derived at serve time from the
//! decorated fold — never stored, never folded, never an op.
//!
//! Derive, don't mirror: these predicates are THE implementation. The
//! SPA's and ctl's copies are deleted in S5/S7 once the served flags
//! reach them; the docs chapter and the triage mandate template point
//! here.

use super::types::{AgendaItem, AgendaKind, AgendaStatus};
use serde::Serialize;

/// The live serving window for CLOSED items (Track AS S6, owner-ratified
/// 2026-07-29 on question 01KYR8X7ZB): done/retired items stay in the
/// default dashboard feed for 14 days by `updated_ms`, then page from
/// the archive on demand. A FIXED daemon-side constant by explicit owner
/// decision — no settings knob. Open items are NEVER windowed out
/// (ruling Q1: the window mechanism is binding design; only closed
/// items age off the wire).
pub(crate) const AGENDA_LIVE_WINDOW_MS: u64 = 14 * 24 * 60 * 60 * 1000;

/// Which slice of the ledger a list read serves (Track AS S6). `All`
/// is the frozen bare default; `Live` is the dashboard's default feed;
/// `Archive` is the paged complement (closed items older than the
/// window), served at FULL grain (ruling R-AS2 — the lenses that page
/// it render answer text and bodies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgendaWindow {
    All,
    Live,
    Archive,
}

impl AgendaWindow {
    /// Parse the additive `window` parameter: absent/empty/`all` = the
    /// frozen full default; unknown values refuse by name.
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None => Ok(Self::All),
            Some("all") => Ok(Self::All),
            Some("live") => Ok(Self::Live),
            Some("archive") => Ok(Self::Archive),
            Some(other) => Err(format!("unknown window '{other}' (all, live, or archive)")),
        }
    }

    /// Does `item` belong to this window at `now_ms`? Open items are in
    /// `Live` unconditionally and never in `Archive`.
    pub(crate) fn admits(self, item: &AgendaItem, now_ms: u64) -> bool {
        match self {
            Self::All => true,
            Self::Live => {
                item.status == AgendaStatus::Open
                    || item.updated_ms >= now_ms.saturating_sub(AGENDA_LIVE_WINDOW_MS)
            }
            Self::Archive => {
                item.status != AgendaStatus::Open
                    && item.updated_ms < now_ms.saturating_sub(AGENDA_LIVE_WINDOW_MS)
            }
        }
    }
}

/// Archive paging (Track AS S6): the compound recency cursor + page
/// size for `window=archive`. Absent fields = first page, default size.
#[derive(Debug, Clone, Default)]
pub(crate) struct AgendaArchivePage {
    pub(crate) before: Option<u64>,
    pub(crate) before_id: Option<String>,
    pub(crate) limit: Option<u64>,
}

/// Apply the serving window (and, for `Archive`, the recency-ordered
/// page bound) to the SERVED set in place — one implementation for the
/// HTTP core and the MCP tool alike. Returns the next page's compound
/// cursor when the archive page filled. The fold and the cross-item
/// summary context are never touched here (ruling R-AS5).
pub(crate) fn apply_window(
    items: &mut Vec<AgendaItem>,
    window: AgendaWindow,
    page: Option<AgendaArchivePage>,
    now_ms: u64,
) -> Option<(u64, String)> {
    if window == AgendaWindow::All {
        return None;
    }
    items.retain(|item| window.admits(item, now_ms));
    if window != AgendaWindow::Archive {
        return None;
    }
    // Newest-closed first; (updated_ms, id) is the stable compound
    // cursor. Archive pages serve FULL items (ruling R-AS2) — callers
    // render answer text and bodies — so bytes stay bounded by the page.
    let page = page.unwrap_or_default();
    items.sort_by(|a, b| {
        b.updated_ms
            .cmp(&a.updated_ms)
            .then_with(|| b.id.cmp(&a.id))
    });
    if let Some(before) = page.before {
        let before_id = page.before_id.as_deref().unwrap_or("");
        items.retain(|item| {
            item.updated_ms < before
                || (item.updated_ms == before
                    && !before_id.is_empty()
                    && item.id.as_str() < before_id)
        });
    }
    let limit = page.limit.unwrap_or(50).clamp(1, 200) as usize;
    if items.len() > limit {
        items.truncate(limit);
        return items.last().map(|last| (last.updated_ms, last.id.clone()));
    }
    None
}

/// One item at summary grain. Field NAMES and sub-shapes mirror the
/// full DTO wherever a field is carried (`provenance.session_id`,
/// `relies_on[].target_id`, …) so lens code adopts summaries without
/// renames; fields the summary deliberately slims are renamed to make
/// the grain explicit (`annotations` → `annotations_count`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AgendaItemSummary {
    pub(crate) id: String,
    pub(crate) kind: AgendaKind,
    pub(crate) status: AgendaStatus,
    pub(crate) title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) due_ms: Option<u64>,
    pub(crate) updated_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) completed_ms: Option<u64>,
    pub(crate) provenance: SummaryProvenance,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) answer: Option<SummaryAnswer>,
    /// True when a rich-ask payload exists (the card's panel affordance);
    /// the questions themselves are full-item material.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ask: Option<SummaryAsk>,
    /// Presence of a live dismissal marker (rail skip/deny on a
    /// still-open question).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) dismissed: bool,
    /// UNCLEARED blockers only — the card's blocked line names the
    /// criterion; cleared history is full-item material.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) blockers: Vec<SummaryBlocker>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) relies_on: Vec<SummaryEdge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) part_of: Option<SummaryPlacement>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) relates_to: Vec<SummaryEdge>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) refs: Vec<SummaryRef>,
    pub(crate) annotations_count: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) effects: Vec<SummaryEffect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) watched_by: Option<super::types::AgendaWatchedBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) deferred_until: Option<u64>,
    /// Cross-item serving-seam flag: an uncleared blocker, or a
    /// `relies_on` edge whose target is not Done (missing/retired
    /// targets do not satisfy). Open items only.
    pub(crate) blocked: bool,
    /// The NAMED causes behind `blocked` (same field path as the full
    /// DTO's serving-seam decoration): uncleared blocker criteria and
    /// unsatisfied prerequisite titles/statuses, so approve surfaces
    /// derive the approve-while-blocked confirm from served truth even
    /// when the prerequisite item sits outside the served window.
    /// Present exactly when `blocked` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blocked_on: Option<Vec<super::types::AgendaBlockedOn>>,
    /// Cross-item serving-seam flag: the un-triaged frontier (the
    /// triage mandate's declared scope; see [`item_in_frontier`]).
    pub(crate) frontier: bool,
    /// The triage mandate's rank/note convention, derived once here
    /// (newest `triage`-source annotation; "rank N" names the rank).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) triage: Option<SummaryTriage>,
    /// Placed-children roll-up (Track AS S6): how many items name this
    /// one as their `part_of` parent, by status — served on every
    /// summary that HAS children, computed against the whole fold, so
    /// By-hub renders honest totals even when the live window holds
    /// only some child rows (owner-ratified S6 values).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) children: Option<super::types::AgendaCounts>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryProvenance {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    pub(crate) created_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryAnswer {
    /// The reply text — answered questions render it on the card
    /// (R-AS3's first named catch). Data, never instructions.
    pub(crate) text: String,
    pub(crate) at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) delivered: Option<bool>,
    /// The structured rich-ask breakdown — answered question cards
    /// render per-question selections ungated (render-completeness law).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) structured: Option<super::types::AgendaAskResolution>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryAsk {
    pub(crate) ask_id: u64,
    pub(crate) questions_count: u32,
    /// The question payload itself — carried ONLY while the ask is live
    /// (open, undismissed): the attention rail and the card's inline
    /// answer composer render it ungated. Resolved/dismissed asks slim
    /// to the count; the inspector fetches the item for history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) questions: Option<Vec<crate::types::UserQuestion>>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryBlocker {
    pub(crate) blocker_id: String,
    pub(crate) criterion: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryEdge {
    pub(crate) target_id: String,
    /// G2 typed-adjacency kind — served on `relates_to` edges only
    /// (`relies_on` edges carry none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) link_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryPlacement {
    pub(crate) parent_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryRef {
    pub(crate) ref_type: super::types::AgendaRefType,
    pub(crate) locator: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) digest: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) must_read: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
}

/// One effect at summary grain: everything the cards' effect strips and
/// the Automations lens render ungated (state derivation inputs, the
/// proposer line, streak/attestation honesty) — MINUS the manifest's
/// `goal` (the single heaviest manifest field, inspector-only) and the
/// run/attestation plumbing beyond what strips show. Sub-shapes mirror
/// the full DTO's field paths (`approval.digest`, `manifest.recurrence`,
/// `last_run.attestation.outcome`) so lens code reads both grains.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryEffect {
    pub(crate) effect_id: String,
    pub(crate) digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proposed_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proposed_principal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proposed_kind: Option<String>,
    /// `armed` (approval bound) / `proposed` / `suspended` (failure
    /// ceiling reached — the planner's own predicate). A convenience
    /// digest of the parts below; ctl/tool consumers read it without
    /// re-deriving.
    pub(crate) state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) approval: Option<SummaryApproval>,
    pub(crate) manifest: SummaryManifest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_fire_ms: Option<u64>,
    pub(crate) consecutive_failures: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_run_attempt: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_run: Option<SummaryRun>,
    /// The serving-seam fireability verdict (same field path as the full
    /// DTO): present exactly when an approve/re-arm affordance would
    /// meet a named refusal — the cards withhold Approve on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fireability_refusal: Option<super::fireability::FireabilityRefusalView>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryApproval {
    pub(crate) digest: String,
    pub(crate) at_ms: u64,
}

/// The digest-bound manifest minus `goal` (and minus nothing else that
/// strips render): fire instant, cadence, trigger, executor pins,
/// project pin. Field names mirror [`super::types::SessionManifest`].
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryManifest {
    pub(crate) fire_at_ms: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) orchestrate: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) interactive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) project_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_config: Option<Box<crate::event::AgentLaunchConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recurrence: Option<super::types::RecurrenceSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) trigger: Option<super::types::TriggerSpec>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryRun {
    pub(crate) state: String,
    pub(crate) at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    /// The fired session's self-report (Track AO), outcome + note only —
    /// the suspended strip renders "last self-report: blocked — …".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attestation: Option<SummaryAttestation>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryAttestation {
    pub(crate) outcome: super::types::AttestationOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SummaryTriage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) rank: Option<u32>,
    /// The newest triage note's text (the chip label). Data only.
    pub(crate) note: String,
}

/// Project the served subset at summary grain, with cross-item flags
/// computed against the FULL decorated fold (`all`) — a subset context
/// starves `blocked`'s dependency targets and `frontier`'s watermark
/// exactly like it starved `watched_by` (#649).
pub(crate) fn summarize(all: &[AgendaItem], served: &[AgendaItem]) -> Vec<AgendaItemSummary> {
    let watermark = triage_watermark(all);
    // Placed-children roll-ups (S6): one pass over the whole fold, so
    // every parent's totals are honest regardless of which children the
    // serving window carries.
    let mut child_counts: std::collections::HashMap<&str, super::types::AgendaCounts> =
        std::collections::HashMap::new();
    for item in all {
        if let Some(placement) = &item.part_of {
            let counts = child_counts
                .entry(placement.parent_id.as_str())
                .or_default();
            match item.status {
                AgendaStatus::Open => counts.open += 1,
                AgendaStatus::Done => counts.done += 1,
                AgendaStatus::Retired => counts.retired += 1,
            }
        }
    }
    served
        .iter()
        .map(|item| {
            let mut summary = summarize_one(all, item, watermark);
            summary.children = child_counts.get(item.id.as_str()).copied();
            summary
        })
        .collect()
}

fn summarize_one(all: &[AgendaItem], item: &AgendaItem, watermark: u64) -> AgendaItemSummary {
    let blocked_causes = blocked_on(all, item);
    AgendaItemSummary {
        id: item.id.clone(),
        kind: item.kind,
        status: item.status,
        title: item.title.clone(),
        tags: item.tags.clone(),
        due_ms: item.due_ms,
        updated_ms: item.updated_ms,
        completed_ms: item.completed_ms,
        provenance: SummaryProvenance {
            principal: item.provenance.principal.clone(),
            session_id: item.provenance.session_id.clone(),
            kind: item.provenance.kind.clone(),
            source: item.provenance.source.clone(),
            created_ms: item.provenance.created_ms,
        },
        answer: item.answer.as_ref().map(|answer| SummaryAnswer {
            text: answer.text.clone(),
            at_ms: answer.at_ms,
            session_id: answer.session_id.clone(),
            delivered: answer.delivered,
            structured: answer.structured.clone(),
        }),
        ask: item.ask.as_ref().map(|ask| SummaryAsk {
            ask_id: ask.ask_id,
            questions_count: ask.questions.len() as u32,
            questions: (item.status == AgendaStatus::Open && item.dismissed.is_none())
                .then(|| ask.questions.clone()),
        }),
        dismissed: item.dismissed.is_some(),
        blockers: item
            .blockers
            .iter()
            .filter(|blocker| blocker.cleared.is_none())
            .map(|blocker| SummaryBlocker {
                blocker_id: blocker.blocker_id.clone(),
                criterion: blocker.criterion.clone(),
            })
            .collect(),
        relies_on: item
            .relies_on
            .iter()
            .map(|edge| SummaryEdge {
                target_id: edge.target_id.clone(),
                link_kind: None,
            })
            .collect(),
        part_of: item.part_of.as_ref().map(|placement| SummaryPlacement {
            parent_id: placement.parent_id.clone(),
        }),
        relates_to: item
            .relates_to
            .iter()
            .map(|edge| SummaryEdge {
                target_id: edge.target_id.clone(),
                link_kind: edge.link_kind.clone(),
            })
            .collect(),
        refs: item
            .refs
            .iter()
            .map(|r| SummaryRef {
                ref_type: r.ref_type,
                locator: r.locator.clone(),
                digest: r.digest.clone(),
                must_read: r.must_read,
                label: r.label.clone(),
            })
            .collect(),
        annotations_count: item.annotations.len() as u32,
        effects: item
            .effects
            .iter()
            .map(|effect| SummaryEffect {
                effect_id: effect.effect_id.clone(),
                digest: effect.digest.clone(),
                proposed_session_id: effect.proposed_session_id.clone(),
                proposed_principal: effect.proposed_principal.clone(),
                proposed_kind: effect.proposed_kind.clone(),
                state: if effect.suspended() {
                    "suspended"
                } else if effect.approval.is_some() {
                    "armed"
                } else {
                    "proposed"
                },
                approval: effect.approval.as_ref().map(|approval| SummaryApproval {
                    digest: approval.digest.clone(),
                    at_ms: approval.at_ms,
                }),
                manifest: SummaryManifest {
                    fire_at_ms: effect.manifest.fire_at_ms,
                    orchestrate: effect.manifest.orchestrate,
                    interactive: effect.manifest.interactive,
                    project_root: effect.manifest.project_root.clone(),
                    agent_config: effect.manifest.agent_config.clone(),
                    recurrence: effect.manifest.recurrence,
                    trigger: effect.manifest.trigger.clone(),
                },
                next_fire_ms: effect.next_fire_ms,
                consecutive_failures: effect.consecutive_failures,
                last_run_attempt: effect.last_run_attempt,
                fireability_refusal: effect.fireability_refusal.clone(),
                last_run: effect.last_run.as_ref().map(|run| SummaryRun {
                    state: run.state.clone(),
                    at_ms: run.at_ms,
                    session_id: run.session_id.clone(),
                    attestation: run.attestation.as_ref().map(|att| SummaryAttestation {
                        outcome: att.outcome,
                        note: att.note.clone(),
                    }),
                }),
            })
            .collect(),
        watched_by: item.watched_by.clone(),
        deferred_until: item.deferred_until,
        blocked: !blocked_causes.is_empty(),
        blocked_on: (!blocked_causes.is_empty()).then_some(blocked_causes),
        frontier: item_in_frontier(item, watermark),
        triage: triage_info(item),
        children: None,
    }
}

/// The frontier watermark: the newest `triage:summary` item's park
/// instant; `0` when no summary was ever parked (every open item is
/// then in the frontier).
pub(crate) fn triage_watermark(all: &[AgendaItem]) -> u64 {
    all.iter()
        .filter(|item| summary_tagged(item))
        .map(|item| item.provenance.created_ms)
        .max()
        .unwrap_or(0)
}

fn summary_tagged(item: &AgendaItem) -> bool {
    item.tags.iter().any(|tag| tag == "triage:summary")
}

/// The un-triaged frontier (the triage mandate's declared scope): open
/// items newer than the newest `triage:summary` park, plus open items
/// lacking both a placement and a triage annotation. Summaries are
/// excluded by definition, and so are daemon-parked items that are
/// currently placed (the mirror-writer exemption — a PR anchor the
/// scanner parked and filed arrives already placed and described;
/// unfiling one re-admits it). Previously implemented three times
/// (ctl, the SPA, the mandate prose); this is now THE implementation.
pub(crate) fn item_in_frontier(item: &AgendaItem, triage_watermark: u64) -> bool {
    if item.status != AgendaStatus::Open || summary_tagged(item) {
        return false;
    }
    let placed = item.part_of.is_some();
    let daemon_parked = item.provenance.kind.as_deref() == Some("daemon");
    if daemon_parked && placed {
        return false;
    }
    if item.provenance.created_ms > triage_watermark {
        return true;
    }
    let triaged = item
        .annotations
        .iter()
        .any(|note| note.source.as_deref() == Some("triage"));
    !placed && !triaged
}

/// Blocked = open, and an uncleared blocker exists or a `relies_on`
/// target is not Done (a missing or retired target does not satisfy —
/// the same rule the trigger planner and the render twins use). The
/// bool IS "any named cause exists" — one live derivation,
/// [`blocked_on`]; this named twin survives only to PIN that identity
/// in tests (the `types::is_blocked` pattern).
#[cfg(test)]
pub(crate) fn item_is_blocked(all: &[AgendaItem], item: &AgendaItem) -> bool {
    !blocked_on(all, item).is_empty()
}

/// The NAMED causes keeping an open item blocked right now — the single
/// derivation behind the served `blocked` flag, the `blocked_on`
/// decoration on both serving grains, and therefore behind every
/// approve-while-blocked confirm/warning (advisory by doctrine: these
/// name, they never gate). Empty exactly when the item is not blocked.
/// Blocker entries name the uncleared criterion; dependency entries name
/// the target's live title and status (`open`, `retired` — which does
/// not satisfy, `missing` — absent from the fold, named by id).
pub(crate) fn blocked_on(
    all: &[AgendaItem],
    item: &AgendaItem,
) -> Vec<super::types::AgendaBlockedOn> {
    if item.status != AgendaStatus::Open {
        return Vec::new();
    }
    let mut causes = Vec::new();
    for blocker in item.blockers.iter().filter(|b| b.cleared.is_none()) {
        causes.push(super::types::AgendaBlockedOn {
            cause: "blocker".to_string(),
            title: blocker.criterion.clone(),
            target_id: None,
            target_status: None,
            blocker_id: Some(blocker.blocker_id.clone()),
        });
    }
    for edge in &item.relies_on {
        let target = all.iter().find(|candidate| candidate.id == edge.target_id);
        let (title, status) = match target {
            None => (edge.target_id.clone(), "missing"),
            Some(target) => match target.status {
                AgendaStatus::Done => continue,
                AgendaStatus::Retired => (target.title.clone(), "retired"),
                AgendaStatus::Open => (target.title.clone(), "open"),
            },
        };
        causes.push(super::types::AgendaBlockedOn {
            cause: "relies_on".to_string(),
            title,
            target_id: Some(edge.target_id.clone()),
            target_status: Some(status.to_string()),
            blocker_id: None,
        });
    }
    causes
}

/// The triage rank/note convention (the mandate's declared "rank N"
/// phrase in `triage`-source annotations): the newest ranked note wins;
/// an unranked one still marks the item triage-flagged.
fn triage_info(item: &AgendaItem) -> Option<SummaryTriage> {
    let notes: Vec<&super::types::AgendaAnnotation> = item
        .annotations
        .iter()
        .filter(|note| note.source.as_deref() == Some("triage"))
        .collect();
    let last = notes.last()?;
    for note in notes.iter().rev() {
        if let Some(rank) = parse_rank(&note.text) {
            return Some(SummaryTriage {
                rank: Some(rank),
                note: note.text.clone(),
            });
        }
    }
    Some(SummaryTriage {
        rank: None,
        note: last.text.clone(),
    })
}

/// First "rank N" phrase in a note (the SPA's `/rank (\d+)/` parse,
/// case-sensitive like the original).
fn parse_rank(text: &str) -> Option<u32> {
    let at = text.find("rank ")?;
    let digits: String = text[at + 5..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Server-side search (Track AS S4): the client search's exact reach —
/// digest-prefix resolution (≥8 hex chars against effect digests,
/// approval digests, and ref attach digests), then case-insensitive
/// substring over title, body, tags, and id. `q` arrives raw; matching
/// lowercases both sides.
pub(crate) fn matches_query(item: &AgendaItem, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    if q.len() >= 8 && q.len() <= 64 && q.chars().all(|c| c.is_ascii_hexdigit()) {
        let digest_hit = item
            .effects
            .iter()
            .flat_map(|effect| {
                std::iter::once(effect.digest.as_str())
                    .chain(effect.approval.as_ref().map(|a| a.digest.as_str()))
            })
            .chain(item.refs.iter().filter_map(|r| r.digest.as_deref()))
            .any(|digest| digest.to_lowercase().starts_with(&q));
        if digest_hit {
            return true;
        }
    }
    item.title.to_lowercase().contains(&q)
        || item.body.to_lowercase().contains(&q)
        || item.tags.iter().any(|tag| tag.to_lowercase().contains(&q))
        || item.id.to_lowercase().contains(&q)
}

#[cfg(test)]
mod tests {
    use super::super::store::{AgendaError, AgendaStore};
    use super::super::types::{AgendaActor, AgendaCommand, AgendaKind};
    use super::*;

    fn owner() -> Option<AgendaActor> {
        Some(AgendaActor {
            principal: Some("owner".into()),
            session_id: None,
            kind: Some("dashboard".into()),
        })
    }

    fn add(store: &mut AgendaStore, title: &str, at: u64) -> Result<AgendaItem, AgendaError> {
        store.apply_command(
            AgendaCommand::Add {
                kind: AgendaKind::Task,
                title: title.into(),
                body: format!("{title} body"),
                tags: Vec::new(),
                due_ms: None,
                source: None,
                refs: Vec::new(),
            },
            owner(),
            at,
        )
    }

    /// Track AS S4 parity pin (derive-don't-mirror discipline): every
    /// summary field is a pure derivation of the full decorated DTO —
    /// identity fields equal, slimmed shapes subset, and the served
    /// flags equal the predicates applied to the full items. The
    /// SPA/ctl/skill re-implementations are deleted only in S5/S7,
    /// AFTER these served flags exist (ruling conformance checklist).
    #[test]
    fn summary_fields_derive_from_the_full_dto() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        store.set_spawn_context(super::super::spawn_project::SessionSpawnContext {
            home: dir.path().to_path_buf(),
            default_project_root: Some(dir.path().to_path_buf()),
            default_agent: None,
        });
        let prereq = add(&mut store, "prerequisite", 1000).unwrap();
        let dependent = add(&mut store, "dependent", 2000).unwrap();
        store
            .apply_command(
                AgendaCommand::AddReliesOn {
                    id: dependent.id.clone(),
                    target_id: prereq.id.clone(),
                    source: None,
                },
                owner(),
                3000,
            )
            .unwrap();
        store
            .apply_command(
                AgendaCommand::ProposeEffect {
                    id: dependent.id.clone(),
                    goal: "goal".into(),
                    fire_at_ms: 4_102_444_800_000,
                    orchestrate: false,
                    interactive: None,
                    recurrence: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                    binding_refs: Vec::new(),
                    source: None,
                },
                owner(),
                4000,
            )
            .unwrap();
        store
            .apply_command(
                AgendaCommand::Annotate {
                    id: dependent.id.clone(),
                    text: "rank 2 — after the prerequisite".into(),
                    source: Some("triage".into()),
                },
                owner(),
                5000,
            )
            .unwrap();

        let all = store.snapshot();
        let summaries = summarize(&all, &all);
        assert_eq!(summaries.len(), all.len());
        let full = all.iter().find(|i| i.id == dependent.id).unwrap();
        let summary = summaries.iter().find(|s| s.id == dependent.id).unwrap();

        assert_eq!(summary.title, full.title);
        assert_eq!(summary.status, full.status);
        assert_eq!(summary.updated_ms, full.updated_ms);
        assert_eq!(summary.provenance.created_ms, full.provenance.created_ms);
        assert_eq!(summary.annotations_count, full.annotations.len() as u32);
        assert_eq!(summary.relies_on.len(), 1);
        assert_eq!(summary.relies_on[0].target_id, prereq.id);
        assert_eq!(summary.effects.len(), 1);
        assert_eq!(summary.effects[0].digest, full.effects[0].digest);
        assert_eq!(summary.effects[0].state, "proposed");
        assert_eq!(
            summary.blocked,
            item_is_blocked(&all, full),
            "the served flag IS the predicate"
        );
        assert!(summary.blocked, "open prerequisite blocks the dependent");
        let causes = summary
            .blocked_on
            .as_ref()
            .expect("a blocked summary carries its named causes");
        assert_eq!(causes.len(), 1);
        assert_eq!(causes[0].cause, "relies_on");
        assert_eq!(
            causes[0].title, "prerequisite",
            "the confirm derives the ACTUAL prerequisite title from serving"
        );
        assert_eq!(causes[0].target_id.as_deref(), Some(prereq.id.as_str()));
        assert_eq!(causes[0].target_status.as_deref(), Some("open"));
        assert_eq!(
            summary.frontier,
            item_in_frontier(full, triage_watermark(&all))
        );
        let triage = summary.triage.as_ref().expect("triage note derived");
        assert_eq!(triage.rank, Some(2));

        // Completing the prerequisite unblocks — cross-item recompute.
        store
            .apply_command(
                AgendaCommand::Complete {
                    id: prereq.id.clone(),
                    source: None,
                },
                owner(),
                6000,
            )
            .unwrap();
        let all = store.snapshot();
        let full = all.iter().find(|i| i.id == dependent.id).unwrap().clone();
        let summaries = summarize(&all, std::slice::from_ref(&full));
        assert!(!summaries[0].blocked, "done target satisfies the edge");

        // Answered questions carry the answer text (R-AS3's named catch);
        // bodies never ride the summary shape.
        let question = store
            .apply_command(
                AgendaCommand::Add {
                    kind: AgendaKind::Question,
                    title: "which color?".into(),
                    body: "a multi-KB body would live here".into(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                    refs: Vec::new(),
                },
                owner(),
                7000,
            )
            .unwrap();
        store
            .apply_command(
                AgendaCommand::Answer {
                    id: question.id.clone(),
                    text: "green".into(),
                    structured: None,
                    source: None,
                },
                owner(),
                8000,
            )
            .unwrap();
        let all = store.snapshot();
        let summaries = summarize(&all, &all);
        let q = summaries.iter().find(|s| s.id == question.id).unwrap();
        assert_eq!(q.answer.as_ref().map(|a| a.text.as_str()), Some("green"));
        let wire = serde_json::to_value(q).unwrap();
        assert!(
            wire.get("body").is_none(),
            "bodies stay excluded from the summary shape (Q3)"
        );

        // The effect strip's render inputs ride the summary — manifest
        // WITHOUT its goal (the heavy field stays inspector-only), the
        // approval sub-shape mirroring the full DTO's path, and the
        // state digest.
        let dep = summaries.iter().find(|s| s.id == dependent.id).unwrap();
        let dep_wire = serde_json::to_value(dep).unwrap();
        let eff = &dep_wire["effects"][0];
        assert_eq!(eff["manifest"]["fire_at_ms"], 4_102_444_800_000u64);
        assert!(
            eff["manifest"].get("goal").is_none(),
            "manifest goals stay excluded from the summary shape"
        );
        assert_eq!(eff["state"], "proposed");
    }

    /// Live asks carry their question payload on the summary (the rail
    /// and the card composer render them ungated); resolved asks slim to
    /// the count — history is full-item material.
    #[test]
    fn live_asks_ride_the_summary_resolved_asks_slim() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let parked = store
            .apply_command(
                AgendaCommand::ask(vec![crate::mcp::AskUserQuestionParams {
                    question: "ship it?".into(),
                    header: Some("Ship".into()),
                    options: Vec::new(),
                    pick_min: None,
                    pick_max: None,
                    free_text: None,
                    previews: Vec::new(),
                }]),
                owner(),
                1000,
            )
            .unwrap();
        let all = store.snapshot();
        let live = summarize(&all, &all);
        let ask = live[0].ask.as_ref().expect("ask on the summary");
        assert_eq!(ask.questions_count, 1);
        assert!(
            ask.questions.as_ref().is_some_and(|qs| qs.len() == 1),
            "open ask carries its questions"
        );

        store
            .apply_command(
                AgendaCommand::Answer {
                    id: parked.id.clone(),
                    text: "yes".into(),
                    structured: None,
                    source: None,
                },
                owner(),
                2000,
            )
            .unwrap();
        let all = store.snapshot();
        let resolved = summarize(&all, &all);
        let ask = resolved[0].ask.as_ref().expect("ask history marker");
        assert!(ask.questions.is_none(), "resolved ask slims to the count");
        assert_eq!(ask.questions_count, 1);
    }

    /// Track AS S6 pin (ruling R-AS5): the serving window is WIRE
    /// vocabulary only — the fold, `snapshot()`, and `serving_read`'s
    /// `all` context keep every item forever; only the served copy
    /// windows. Open items are NEVER windowed out however old; closed
    /// items age into the archive at the fixed 14-day constant.
    #[test]
    fn serving_window_never_filters_the_fold() {
        let dir = tempfile::tempdir().unwrap();
        let now = 100 * AGENDA_LIVE_WINDOW_MS;
        // Author history with explicit op instants through the store
        // (the handle's clock is the wall clock), then serve through a
        // fresh handle over the same log.
        let (old_open, old_done, fresh_done) = {
            let mut store = AgendaStore::open(dir.path()).unwrap();
            let old_open = add(&mut store, "ancient open", 1000).unwrap();
            let old_done = add(&mut store, "ancient done", 2000).unwrap();
            store
                .apply_command(
                    AgendaCommand::Complete {
                        id: old_done.id.clone(),
                        source: None,
                    },
                    owner(),
                    3000,
                )
                .unwrap();
            let fresh_done = add(&mut store, "fresh done", now - 1000).unwrap();
            store
                .apply_command(
                    AgendaCommand::Complete {
                        id: fresh_done.id.clone(),
                        source: None,
                    },
                    owner(),
                    now - 500,
                )
                .unwrap();
            (old_open, old_done, fresh_done)
        };
        let bus = crate::event::EventBus::new();
        let handle = super::super::AgendaHandle::new(
            AgendaStore::open(dir.path()).unwrap(),
            bus,
            dir.path(),
        );

        // The fold and both serving-context views keep everything.
        assert_eq!(handle.snapshot().len(), 3);
        assert_eq!(handle.serving_read(None).all.len(), 3);

        // Live: open always (however ancient) + fresh closed.
        let mut live = handle.serving_read(None).served;
        assert!(apply_window(&mut live, AgendaWindow::Live, None, now).is_none());
        let live_ids: Vec<&str> = live.iter().map(|i| i.id.as_str()).collect();
        assert!(
            live_ids.contains(&old_open.id.as_str()),
            "open never ages off"
        );
        assert!(live_ids.contains(&fresh_done.id.as_str()));
        assert!(!live_ids.contains(&old_done.id.as_str()));

        // Archive: exactly the aged closed complement.
        let mut archive = handle.serving_read(None).served;
        apply_window(&mut archive, AgendaWindow::Archive, None, now);
        assert_eq!(
            archive.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec![old_done.id.as_str()]
        );
    }

    /// The server search covers the client search's exact reach: id,
    /// title, body, tags, and ≥8-hex digest prefixes (effect digests).
    #[test]
    fn query_reach_matches_the_client_search() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        store.set_spawn_context(super::super::spawn_project::SessionSpawnContext {
            home: dir.path().to_path_buf(),
            default_project_root: Some(dir.path().to_path_buf()),
            default_agent: None,
        });
        let item = store
            .apply_command(
                AgendaCommand::Add {
                    kind: AgendaKind::Task,
                    title: "Deploy the relay".into(),
                    body: "needs the sealed manifest".into(),
                    tags: vec!["infra".into()],
                    due_ms: None,
                    source: None,
                    refs: Vec::new(),
                },
                owner(),
                1000,
            )
            .unwrap();
        store
            .apply_command(
                AgendaCommand::ProposeEffect {
                    id: item.id.clone(),
                    goal: "goal".into(),
                    fire_at_ms: 4_102_444_800_000,
                    orchestrate: false,
                    interactive: None,
                    recurrence: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                    binding_refs: Vec::new(),
                    source: None,
                },
                owner(),
                2000,
            )
            .unwrap();
        let all = store.snapshot();
        let full = &all[0];
        assert!(matches_query(full, "relay"), "title");
        assert!(matches_query(full, "SEALED"), "body, case-insensitive");
        assert!(matches_query(full, "infra"), "tags");
        assert!(matches_query(full, &full.id[..10].to_lowercase()), "id");
        let digest_prefix = full.effects[0].digest[..8].to_string();
        assert!(matches_query(full, &digest_prefix), "digest prefix");
        assert!(!matches_query(full, "nomatch-anywhere"));
    }

    fn bare(id: &str, title: &str, status: AgendaStatus) -> AgendaItem {
        AgendaItem {
            id: id.into(),
            kind: AgendaKind::Task,
            title: title.into(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            provenance: super::super::types::AgendaProvenance {
                principal: None,
                session_id: None,
                kind: None,
                source: None,
                created_ms: 1,
            },
            status,
            updated_ms: 1,
            completed_ms: None,
            answer: None,
            effects: Vec::new(),
            ask: None,
            dismissed: None,
            annotations: Vec::new(),
            blockers: Vec::new(),
            relies_on: Vec::new(),
            refs: Vec::new(),
            part_of: None,
            relates_to: Vec::new(),
            deferred_until: None,
            watched_by: None,
            blocked_on: None,
        }
    }

    fn dep(target_id: &str) -> super::super::types::AgendaDependency {
        super::super::types::AgendaDependency {
            target_id: target_id.into(),
            added_ms: 1,
            principal: None,
            session_id: None,
            kind: None,
            source: None,
        }
    }

    /// The approve-while-blocked serving contract (confirm derivation
    /// from served truth): `blocked_on` names every live cause —
    /// blocker criteria verbatim, unsatisfied prerequisites by live
    /// title with `open`/`retired`/`missing` status, Done targets
    /// dropped — is empty exactly when the item is unblocked or
    /// non-open, and the served `blocked` flag IS its non-emptiness.
    #[test]
    fn blocked_on_names_causes_across_lanes() {
        let prereq_open = bare("01AOPEN", "Ship the envelope", AgendaStatus::Open);
        let prereq_retired = bare("01BRETIRED", "Old plan", AgendaStatus::Retired);
        let prereq_done = bare("01CDONE", "Landed already", AgendaStatus::Done);
        let mut item = bare("01DDEPENDENT", "dependent", AgendaStatus::Open);
        item.relies_on = vec![
            dep("01AOPEN"),
            dep("01BRETIRED"),
            dep("01CDONE"),
            dep("01MISSING"),
        ];
        item.blockers = vec![super::super::types::AgendaBlocker {
            blocker_id: "bk-1".into(),
            criterion: "vendor API still 403s".into(),
            set_ms: 1,
            principal: None,
            session_id: None,
            kind: None,
            source: None,
            cleared: None,
        }];
        let all = vec![prereq_open, prereq_retired, prereq_done, item.clone()];

        let causes = blocked_on(&all, &item);
        assert_eq!(
            causes.len(),
            4,
            "blocker + open + retired + missing; Done drops"
        );
        assert_eq!(causes[0].cause, "blocker");
        assert_eq!(causes[0].title, "vendor API still 403s");
        assert_eq!(causes[0].blocker_id.as_deref(), Some("bk-1"));
        assert_eq!(causes[1].cause, "relies_on");
        assert_eq!(causes[1].title, "Ship the envelope");
        assert_eq!(causes[1].target_status.as_deref(), Some("open"));
        assert_eq!(causes[2].title, "Old plan");
        assert_eq!(causes[2].target_status.as_deref(), Some("retired"));
        assert_eq!(
            causes[3].title, "01MISSING",
            "a missing target is named by id"
        );
        assert_eq!(causes[3].target_status.as_deref(), Some("missing"));
        assert!(
            causes
                .iter()
                .all(|c| c.target_id.as_deref() != Some("01CDONE")),
            "a Done prerequisite is satisfied — never a cause"
        );
        assert_eq!(
            item_is_blocked(&all, &item),
            !blocked_on(&all, &item).is_empty(),
            "the served flag IS non-emptiness of the named causes"
        );

        let mut done_item = item.clone();
        done_item.status = AgendaStatus::Done;
        assert!(
            blocked_on(&all, &done_item).is_empty(),
            "non-open items never derive blocked causes"
        );
    }
}

//! Agenda vocabulary: item/op types, the wire command shape, and the fold
//! that derives item state from the op log. Pure data — no I/O here.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Caps enforced at command intake ([`validate`] helpers) — the log itself
/// is trusted (only the validated single-writer path appends).
pub(crate) const MAX_TITLE_CHARS: usize = 500;
pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024;
pub(crate) const MAX_TAGS: usize = 32;
pub(crate) const MAX_TAG_CHARS: usize = 100;
pub(crate) const MAX_SOURCE_CHARS: usize = 100;
/// F2 caps (steward-ruled 2026-07-20): annotations got an explicit intake
/// cap — the one otherwise-unbounded DTO surface (weekly housekeeping ≈
/// 52/item/year; 500 is a pathology rail, not a budget).
pub(crate) const MAX_ANNOTATIONS_PER_ITEM: usize = 500;
pub(crate) const MAX_UNCLEARED_BLOCKERS_PER_ITEM: usize = 32;
pub(crate) const MAX_RELIES_ON_PER_ITEM: usize = 32;
pub(crate) const MAX_CRITERION_CHARS: usize = 1000;
/// G2 caps (steward-ruled 2026-07-22). Adjacency follows the edges idiom;
/// the children cap is a pathology rail like annotations (project hubs
/// legitimately accrue hundreds of children — 32 would strangle the
/// primary intended use), and the depth cap keeps the tree a working
/// surface, not an ontology.
pub(crate) const MAX_RELATES_TO_PER_ITEM: usize = 32;
pub(crate) const MAX_PART_OF_DEPTH: usize = 16;
pub(crate) const MAX_CHILDREN_PER_HUB: usize = 500;
/// G1 caps (steward-ruled 2026-07-22). Refs follow the blockers/edges
/// idiom; locator bounds are per-type (paths, opaque ids, urls).
pub(crate) const MAX_REFS_PER_ITEM: usize = 32;
pub(crate) const MAX_REF_LABEL_CHARS: usize = 100;
pub(crate) const MAX_REF_FILE_LOCATOR_CHARS: usize = 1000;
pub(crate) const MAX_REF_ID_LOCATOR_CHARS: usize = 200;
pub(crate) const MAX_REF_URL_LOCATOR_CHARS: usize = 2000;
/// Largest file the intake digest (and the expand-time drift rehash)
/// will hash. Refs point at working artifacts, not archives.
pub(crate) const MAX_REF_FILE_HASH_BYTES: u64 = 64 * 1024 * 1024;

/// What an agenda entry is. Kinds and effects are orthogonal: no kind
/// implies any delivery or execution behavior. `Question` (slice A4) is a
/// durable, non-blocking ask — the counterpart of the ephemeral blocking
/// `ask_user` rail: an agent parks it, dies, and reads the owner's reply
/// in a later session via the `answer` op. Older builds reading a newer
/// log skip `question` lines by the usual forward-compat rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgendaKind {
    Note,
    Task,
    Question,
}

/// Fold-derived lifecycle state. Transitions are explicit ops — `Complete`
/// (open → done), `Reopen` (done|retired → open), `Retire` (any → retired) —
/// never implied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgendaStatus {
    Open,
    Done,
    Retired,
}

/// Who performed an op, as the daemon's gates attributed it. This is the
/// agenda's **own versioned copy** of the resolved actor — mapped from
/// `access::actor::ActorBinding` at the write path (never serde of the raw
/// seam type into the durable log; contract in `access/actor.rs`). All
/// fields optional by design — attribution must never block parking an
/// item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaActor {
    /// The IAM principal exactly as the gate named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    /// The supervised session the write acted as — gate-bound by token
    /// possession, never echoed from request fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    /// Actor class (`agent_session`, `dashboard`, `local_process`, `peer`)
    /// so the diary can say "by you" vs "by a session" without parsing
    /// principal ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
}

impl AgendaActor {
    fn is_empty(&self) -> bool {
        self.principal.is_none() && self.session_id.is_none() && self.kind.is_none()
    }

    /// Map the shared seam type into the agenda's own record shape.
    /// `None` for an explicitly unattributed caller with nothing to record.
    pub(crate) fn from_binding(binding: &crate::access::actor::ActorBinding) -> Option<Self> {
        let kind = (binding.kind != crate::access::actor::ActorKind::Unattributed)
            .then(|| binding.kind.as_str().to_string());
        let actor = Self {
            principal: binding.principal_id.clone(),
            session_id: binding.session_id.clone(),
            kind,
        };
        (!actor.is_empty()).then_some(actor)
    }
}

/// The current reply on a question item (fold view of the latest `answer`
/// op — earlier replies stay in the log as history). Attribution mirrors
/// [`AgendaActor`]: the answering surface's gate-resolved identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaAnswer {
    /// The reply text — data, never instructions (same doctrine as bodies).
    /// For rich (ask-backed) questions this is the human-readable joined
    /// summary, so every text-only surface keeps working; the structured
    /// breakdown rides [`AgendaAnswer::structured`].
    pub(crate) text: String,
    pub(crate) at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    /// Structured resolution of a rich ask (selections, follow-ups,
    /// anchored preview notes). Additive: absent on plain text answers and
    /// in logs written by older builds, which skip it on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) structured: Option<AgendaAskResolution>,
    /// Whether this answer's delivery attempt reached a live asking
    /// session (fold view of the daemon-authored `record_ask_delivery`
    /// op; ask-backed items only). `Some(false)` marks an answer nobody
    /// heard — surfaces render it "answered · awaiting pickup"; a later
    /// successful successor delivery flips it true. Additive: `None` on
    /// answers that predate the marker and in logs written by older
    /// builds (no chip either way — absent data claims nothing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) delivered: Option<bool>,
}

/// The full Ask v2 payload carried by a parked rich question: the wire
/// questions exactly as the rail renders them (options, pick bounds,
/// free-text policy, preview references into the agenda blob store) plus
/// the approval-space `ask_id` every rail resolves against. Additive on
/// [`AgendaItem`]: older builds skip the field and treat the item as a
/// plain question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaAsk {
    /// Rail id from the process-wide approval allocator
    /// (`crate::event::next_approval_id`). The store floors the allocator
    /// above every persisted ask id at fold time so a restarted daemon can
    /// never re-mint one.
    pub(crate) ask_id: u64,
    pub(crate) questions: Vec<crate::types::UserQuestion>,
}

/// Structured resolution data recorded with an answer — the same shapes
/// `ControlMsg::AnswerQuestion` carries, keyed by question text. BTreeMaps
/// keep the durable log lines byte-deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgendaAskResolution {
    /// Question text → the joined answer string (the legacy authoritative
    /// form every consumer understands).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) answers: BTreeMap<String, String>,
    /// Question text → chosen option labels, unjoined (preserves labels
    /// containing the ", " join sequence).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) selections: BTreeMap<String, Vec<String>>,
    /// Question text → the user's follow-up text (may stand in for an
    /// answer).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) followups: BTreeMap<String, String>,
    /// Question text → notes anchored to that question's preview cards.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) annotations: BTreeMap<String, Vec<crate::types::QuestionAnnotation>>,
}

impl AgendaAskResolution {
    pub(crate) fn is_empty(&self) -> bool {
        self.answers.is_empty()
            && self.selections.is_empty()
            && self.followups.is_empty()
            && self.annotations.is_empty()
    }
}

/// Dismissal marker on an open question (fold view of the latest `dismiss`
/// op). A dismissal clears the rails NOW but leaves the item OPEN — a
/// parked question survives dismissal; only an answer resolves it. Cleared
/// by `answer` and `reopen`; the log keeps every dismissal as history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaDismissal {
    /// The dismissing verb as the rail spoke it (`skip`, `deny`,
    /// `approve`, `approve_all`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) action: String,
    pub(crate) at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
}

/// In-manifest recurrence (G3-pre, ratified A5-rider amendment
/// 2026-07-22): a manifest may declare its own standing cadence, so ONE
/// approval covers the series — the ceremony matches the decision
/// ("housekeeping runs weekly until revoked" is one decision). Because
/// this lives INSIDE the manifest, the existing digest machinery does all
/// the work: any edit mints a new digest and voids the approval, and a
/// recurrence-less manifest serializes byte-identically to the legacy
/// shape, so every legacy digest is unchanged by construction. Cadence is
/// TIME only — event triggers are deliberately not vocabulary (G4,
/// deferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecurrenceSpec {
    /// Cadence interval in ms; intake floors it at
    /// [`RECURRENCE_MIN_EVERY_MS`] (a runaway sub-minute cadence is a
    /// session-spawn bomb).
    pub every_ms: u64,
    /// Expiry: no instants after this (epoch ms; must exceed
    /// `fire_at_ms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    /// Series length bound in INSTANTS (time-defined, replay-derivable):
    /// instants that pass unspent while the daemon is down still consume
    /// their indices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_occurrences: Option<u32>,
    /// Consecutive non-success (`failed`/`unknown`) outcomes that suspend
    /// the effect — surfaced, never silently re-fired. Default 3. The
    /// owner re-arms by re-approving the unchanged digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend_after_failures: Option<u32>,
}

/// The default failure-suspend threshold when a recurrence declares none.
pub(crate) const DEFAULT_SUSPEND_AFTER_FAILURES: u32 = 3;
/// Cadence floor (15 minutes), enforced at intake only.
pub(crate) const RECURRENCE_MIN_EVERY_MS: u64 = 15 * 60 * 1000;

impl RecurrenceSpec {
    pub(crate) fn suspend_threshold(&self) -> u32 {
        self.suspend_after_failures
            .unwrap_or(DEFAULT_SUSPEND_AFTER_FAILURES)
            .max(1)
    }
}

/// A scheduled-session manifest (slice A5): the complete statement of
/// what firing does — reviewed by the owner at approval time. Immutable
/// per revision: [`manifest_digest`] binds the approval, and any edit
/// mints a new digest that invalidates it. Fields are additive-only from
/// here (sandbox/budget knobs arrive as later optional fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionManifest {
    /// The task text the spawned session receives. Data under review —
    /// never instructions to whoever reads the agenda.
    pub(crate) goal: String,
    /// When to fire (epoch ms). One-shot.
    pub(crate) fire_at_ms: u64,
    /// Orchestrate vs direct execution (defaults to direct).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) orchestrate: bool,
    /// Additive: open the spawned session interactively — the goal is the
    /// opening user message and the session then waits for the owner,
    /// exactly like a session started from the composer. `false` (the
    /// legacy default) runs the goal as an autonomous supervised task.
    /// Absent-on-the-wire when false, so legacy manifest bytes — and the
    /// digests their approvals bind — are unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) interactive: bool,
    /// Additive: the project root the spawned session runs under. `None`
    /// (the legacy shape) resolves at fire time: the parking session's
    /// recorded project root, else the daemon default — and the spawn is
    /// refused with a named failure when neither exists, never launched
    /// project-less.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_root: Option<String>,
    /// Additive: the agent-launch configuration the spawned session runs
    /// with — exactly the CreateSession vocabulary (backend selection,
    /// model/effort/permission pins per backend). `None` (the legacy
    /// shape) inherits every field, so legacy manifest bytes — and the
    /// digests their approvals bind — are unchanged. Setting it revises
    /// the manifest and mints a new digest, exactly like any other
    /// manifest edit: the owner approves the config they reviewed. At
    /// fire time each field resolves explicit pin → daemon default →
    /// backend default, through the same launch path every session uses.
    /// Boxed for enum-size hygiene only — serde and the digest see the
    /// inner value verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent_config: Option<Box<crate::event::AgentLaunchConfig>>,
    /// Standing cadence (G3-pre). Additive: absent-on-the-wire when
    /// `None`, so legacy manifest bytes — and the digests their approvals
    /// bind — are unchanged. Living inside the manifest means the digest
    /// binds it: declaring or editing recurrence revises the manifest and
    /// voids any standing approval like any other edit. In a shared home,
    /// an older build re-serializes this manifest WITHOUT the field and
    /// derives a different digest, sees the approval as a mismatch, and
    /// never fires — recurrence degrades fail-closed, never as a mangled
    /// one-shot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) recurrence: Option<RecurrenceSpec>,
}

/// An owner's approval of one manifest revision. `digest` is the bound
/// revision — the approval is void for any other bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaApproval {
    pub(crate) digest: String,
    pub(crate) at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
}

/// The latest occurrence outcome recorded against an effect (fold view
/// of daemon-authored `record_occurrence` ops — full history in the log
/// and the occurrence journal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaRun {
    pub(crate) occurrence_id: String,
    /// `started` | `completed` | `failed` | `missed` | `unknown`.
    pub(crate) state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    pub(crate) at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<String>,
}

/// One owner-requested extra instant of an approved standing manifest
/// (G3-pre `request_occurrence` fold view): the "run now" gesture beside
/// a standing approval, recorded attributed like every act. The instant
/// (`at_ms`) was minted at intake and read from the op — replay never
/// consults a clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaRequestedRun {
    pub(crate) at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
}

/// How many requested-run entries the fold keeps in view (the log keeps
/// all; the journal is the execution truth either way).
pub(crate) const MAX_REQUESTED_RUNS_VIEW: usize = 8;

/// A scheduled-session effect on an item — a separate object referencing
/// the entry, per the ratified doctrine (never item fields). `effect_id`
/// is the stable lineage identity; the digest names one revision. v1
/// allows one session effect per item (the vocabulary supports more).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaEffect {
    pub(crate) effect_id: String,
    pub(crate) manifest: SessionManifest,
    pub(crate) digest: String,
    pub(crate) proposed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) proposed_principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) proposed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) proposed_kind: Option<String>,
    /// Owner approval of exactly `digest`; cleared by any re-propose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) approval: Option<AgendaApproval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_run: Option<AgendaRun>,
    /// Consecutive non-success (`failed`/`unknown`) occurrence outcomes
    /// since the last approval (G3-pre) — fold-derived from log order
    /// alone: `completed` resets it, `approve_effect` (the one-click
    /// re-arm) and `propose_effect` reset it, `missed`/`started` are
    /// neutral. Suspension = this counter reaching the manifest's
    /// threshold; derived, never stored beyond the fold product.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub(crate) consecutive_failures: u32,
    /// Owner-requested extra instants (G3-pre), newest-8 view; cleared by
    /// re-propose and revoke (a request exists only under an approval).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) requested: Vec<AgendaRequestedRun>,
    /// Display-only planner derivation: the next instant this effect
    /// would actually fire ([`super::reminders::effect_next_fire_ms`] —
    /// the real planner math, so frontends never reimplement it), or
    /// `None` when nothing will fire (unapproved, suspended, spent,
    /// exhausted). Decorated at the serving seam
    /// ([`super::AgendaHandle`]) with the clock of the read; always
    /// `None` in the fold product, never folded from ops, never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) next_fire_ms: Option<u64>,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

impl AgendaEffect {
    /// Suspended = recurring ∧ the failure streak reached the manifest's
    /// threshold. Render/planner judgment over fold products — the
    /// planner plans nothing for a suspended effect, and the owner
    /// re-arms with one re-approval of the unchanged digest.
    pub(crate) fn suspended(&self) -> bool {
        self.manifest
            .recurrence
            .as_ref()
            .is_some_and(|rec| self.consecutive_failures >= rec.suspend_threshold())
    }
}

/// The digest an approval binds: effect identity + the manifest's
/// canonical JSON (serde struct order is declaration order, stable).
pub(crate) fn manifest_digest(
    item_id: &str,
    effect_id: &str,
    manifest: &SessionManifest,
) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"agenda-effect\0");
    hasher.update(item_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(effect_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(
        serde_json::to_string(manifest)
            .unwrap_or_default()
            .as_bytes(),
    );
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Birth attribution carried on the item (from its `add` op).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    /// Actor class of the parking write (see [`AgendaActor::kind`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    /// Self-described caller label (`--source`), copied from the add op's
    /// envelope. UNVERIFIED by doctrine: data beside the attribution, never
    /// a principal, never session attribution — every surface renders it
    /// visibly labeled as self-described.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    pub(crate) created_ms: u64,
}

/// A fold-derived agenda item. This is also the API/tunnel DTO — frontends
/// receive it verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaItem {
    /// ULID — lexicographic order is creation order.
    pub(crate) id: String,
    pub(crate) kind: AgendaKind,
    pub(crate) title: String,
    /// Markdown **data**. Every surface renders this quoted; no agent or
    /// component may execute or obey it (ratified doctrine — bodies are
    /// diary material, not instructions to whoever reads them).
    pub(crate) body: String,
    pub(crate) tags: Vec<String>,
    /// Reminder due instant (ms since epoch). It is patchable presentation
    /// state: the reminder scheduler delivers a notification under the
    /// owner's reminder policy, but it never authorizes work. Scheduled
    /// work is a separately approved effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) due_ms: Option<u64>,
    pub(crate) provenance: AgendaProvenance,
    pub(crate) status: AgendaStatus,
    /// Timestamp of the last op that changed this item.
    pub(crate) updated_ms: u64,
    /// When the item last transitioned to `Done` (cleared by `Reopen`,
    /// preserved by `Retire` as history).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completed_ms: Option<u64>,
    /// The current reply, question items only. Answering resolves the
    /// question (status `Done`); `Reopen` re-asks and clears this view
    /// (the log keeps every reply).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) answer: Option<AgendaAnswer>,
    /// Scheduled-session effects (A5). Separate objects referencing the
    /// item; delivery/execution authority never rides item fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) effects: Vec<AgendaEffect>,
    /// Rich-ask payload (Ask v2), question items parked via the `ask`
    /// command only. Additive: older builds skip it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ask: Option<AgendaAsk>,
    /// Latest dismissal of a still-open question (rail skip/deny). Cleared
    /// by `answer` and `reopen`; never a lifecycle transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dismissed: Option<AgendaDismissal>,
    /// Attributed note thread (F2 `annotate`), full history in fold order —
    /// render caps with an expander. Notes are data, never instructions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) annotations: Vec<AgendaAnnotation>,
    /// Blockers (F2): human-stated criteria, never evaluated by machinery.
    /// Cleared entries STAY as the evidence trail — clears are ops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) blockers: Vec<AgendaBlocker>,
    /// Live dependency edges (F2 `relies_on`). Removal drops the edge from
    /// this view; the log keeps the full add/remove history. Satisfaction
    /// and the blocked chip are derived at RENDER time, never stored or
    /// wired.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) relies_on: Vec<AgendaDependency>,
    /// Live typed references (G1 `add_ref`). Removal drops the ref from
    /// this view; the log keeps history. File drift is derived at
    /// expand time against the recorded attach digest — never stored,
    /// never on list render.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) refs: Vec<AgendaRef>,
    /// The single live parent (G2). Re-parent is a remove+add op pair;
    /// child lists, roll-up counts, and the tree lens are derived at
    /// render from the ordinary snapshot — never stored, and placement
    /// never hides an item from the flat recent lens (anti-hiding rule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) part_of: Option<AgendaPlacement>,
    /// Stored adjacency edges (G2 `add_relates_to`), this item's side
    /// only — surfaces render the undirected union.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) relates_to: Vec<AgendaRelation>,
    /// Display-only planner derivation: when the owner's quiet hours
    /// would defer this item's pending reminder, the instant delivery
    /// would actually happen
    /// ([`super::reminders::reminder_deferred_until`] — the planner's
    /// own quiet-window math). `None` when nothing defers — including
    /// reminders disabled (nothing will deliver at all; absence claims
    /// nothing). Decorated at the serving seam ([`super::AgendaHandle`])
    /// with the clock of the read; always `None` in the fold product,
    /// never folded from ops, never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) deferred_until: Option<u64>,
}

/// One attributed note on an item (F2 `annotate` fold view). Attribution
/// mirrors [`AgendaActor`] + the envelope's self-described `source` label
/// (which supplements, never replaces, gate attribution — steward ruling
/// Q2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaAnnotation {
    /// The note — data, never instructions (bodies doctrine).
    pub(crate) text: String,
    pub(crate) at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
}

/// One blocker (F2 fold view): criterion text stated by whoever set it,
/// with set/cleared attribution. NO evaluation machinery exists — the
/// owner clears from the card; agents clear only under an explicit
/// mandate (conduct doctrine, not an IAM gate — steward ruling Q3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaBlocker {
    /// Intake-minted stable id (`bk-…`), recorded in the op — replay never
    /// mints.
    pub(crate) blocker_id: String,
    /// The blocking criterion — data describing the world, never a
    /// condition any component evaluates.
    pub(crate) criterion: String,
    pub(crate) set_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    /// Set by `clear_blocker` — the cleared entry remains rendered
    /// history (clears are ops, never deletions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cleared: Option<AgendaBlockerClear>,
}

/// The clearing act's attribution (F2 `clear_blocker` fold view).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaBlockerClear {
    pub(crate) at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
}

/// One live dependency edge (F2 `relies_on` fold view). Satisfaction is
/// derived at render time from the target's status — a completed target
/// satisfies; a retired target does NOT silently satisfy (renders a
/// "prerequisite retired — review" marker); cycles simply render every
/// member blocked. Nothing evaluates, nothing fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaDependency {
    /// The prerequisite item's id.
    pub(crate) target_id: String,
    pub(crate) added_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
}

/// The single live parent (G2 `add_part_of` fold view): pure
/// subordination for navigation and roll-ups. **A hub is just an item
/// with children** — no kind, no fields, no schema axis; projects are
/// hubs by convention. NO transitive semantics (pinned): placement never
/// propagates blocking, completion never cascades, and a hub completing
/// with open children gets a render-level flag, nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaPlacement {
    pub(crate) parent_id: String,
    pub(crate) added_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
}

/// One stored adjacency edge (G2 `add_relates_to` fold view): untyped,
/// purely navigational — nothing derives, evaluates, blocks, or fires
/// from adjacency, ever. Stored directed (the writer's item carries it),
/// rendered undirected and deduped by every surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaRelation {
    pub(crate) target_id: String,
    pub(crate) added_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
}

/// What a typed reference points at (G1). The discriminator mirrors the
/// kernel's Appendix A.5 `evref` spirit (scheme + locator + digest) so the
/// future D0-Agenda-Data migration maps refs mechanically: `file`→`file`,
/// `session`→`session-log`, `url`→`url`, `memory`→plane claim ref. A
/// ref type this build does not know fails the typed parse, so the whole
/// line degrades to preserved-skipped — future types are op-vocabulary
/// additions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgendaRefType {
    File,
    Memory,
    Session,
    Url,
}

impl AgendaRefType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AgendaRefType::File => "file",
            AgendaRefType::Memory => "memory",
            AgendaRefType::Session => "session",
            AgendaRefType::Url => "url",
        }
    }
}

/// One live typed reference (G1 `add_ref` fold view): a TYPED POINTER,
/// never content — no blobs, no copies, no uploads; the agenda points, it
/// does not store. Addressed by `(ref_type, locator)` — no minted id,
/// exactly as `relies_on` edges are addressed by `target_id`; changing
/// `must_read` or `label` is remove+add (history honest). Locators and
/// labels are data, never instructions to whoever reads them; a
/// `must_read` is a pointer the reading agent weighs, not a standing
/// order. Item-to-item links are NOT refs — that is G2's edge vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaRef {
    pub(crate) ref_type: AgendaRefType,
    /// `file`: absolute path; `memory`: claim id; `session`: conversation
    /// id; `url`: http(s) URL. Stored verbatim as intake validated it.
    pub(crate) locator: String,
    /// File refs only: full sha256 hex of the file as it stood at attach —
    /// intake-minted and recorded in the op (replay never hashes). The
    /// detail view re-hashes on expand to render drift honestly; nothing
    /// re-derives this durable fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) digest: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) must_read: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
    pub(crate) added_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
}

/// One ref as the `add` command's attach-time sugar carries it (G1: refs
/// ride the parking gesture). Validated exactly like `AddRef`; the daemon
/// appends the `add` op and then one `add_ref` per spec under the same
/// lock, all-or-nothing (any invalid spec refuses the whole park).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgendaRefSpec {
    pub ref_type: AgendaRefType,
    pub locator: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub must_read: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Field-level patch of presentation state (umbrella RFC §7.2: `Patch`
/// carries non-effectful presentation metadata only). Wire semantics follow
/// JSON merge-patch for `due_ms`: absent = keep, `null` = clear.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgendaPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tags: Option<Vec<String>>,
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<u64>")]
    pub(crate) due_ms: Option<Option<u64>>,
}

impl AgendaPatch {
    pub(crate) fn is_empty(&self) -> bool {
        self.title.is_none() && self.body.is_none() && self.tags.is_none() && self.due_ms.is_none()
    }
}

/// `Option<Option<T>>` as JSON merge-patch: field absent → outer `None`
/// (keep), field `null` → `Some(None)` (clear), value → `Some(Some(v))`.
/// Shared by [`AgendaPatch::due_ms`] and the reminder-policy patch.
pub(crate) mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(crate) fn serialize<T: Serialize, S: Serializer>(
        v: &Option<Option<T>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        // Outer `None` is skipped via `skip_serializing_if`; only the inner
        // option reaches the wire.
        match v {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<T>>, D::Error> {
        Ok(Some(Option::<T>::deserialize(d)?))
    }
}

/// A frontend intent, before the daemon has validated it or minted an id.
/// This is the wire shape ctl and the dashboard send; the daemon turns an
/// accepted command into an [`AgendaOp`] (the durable shape). Kept separate
/// on purpose: commands carry no ids the client could forge (`add` mints
/// server-side), and the durable log never depends on wire evolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgendaCommand {
    /// Park a new note or task on the agenda.
    Add {
        kind: AgendaKind,
        title: String,
        /// Markdown body — data, never instructions to whoever reads it.
        #[serde(default)]
        body: String,
        #[serde(default)]
        tags: Vec<String>,
        /// Reminder due instant (ms since epoch). It may deliver a
        /// policy-controlled notification but never authorizes work.
        #[serde(default)]
        due_ms: Option<u64>,
        /// Self-described caller label for unsupervised writers
        /// (`--source`). Explicitly UNVERIFIED: stored beside the
        /// gate-resolved actor, never as attribution.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        /// Refs attached at park time (G1 sugar): validated like `AddRef`,
        /// appended as `add_ref` ops after the `add` under one lock,
        /// all-or-nothing.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        refs: Vec<AgendaRefSpec>,
    },
    Patch {
        id: String,
        patch: AgendaPatch,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    Complete {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    Reopen {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    Retire {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Reply to an open question (question items only). Resolves it.
    /// `structured` (optional, additive) carries the rich-ask breakdown —
    /// selections, follow-ups, anchored preview notes — recorded alongside
    /// the joined `text` summary.
    Answer {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured: Option<AgendaAskResolution>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Park a rich multi-question ask (the Ask v2 payload — same
    /// vocabulary as `ask_user`'s `questions` form: options, pick bounds,
    /// free-text policy, inline preview sources) as a durable agenda
    /// question item. Returns immediately: nothing blocks on the answer.
    /// The daemon validates, commits preview blobs into the agenda blob
    /// store, and mints both the item id and the rail `ask_id` — commands
    /// carry no client-minted ids.
    Ask {
        questions: Vec<crate::mcp::AskUserQuestionParams>,
    },
    /// Propose (or revise) the item's scheduled-session manifest. Open to
    /// every agenda writer — proposing carries no authority: nothing fires
    /// without an owner-surface approval of the exact digest. `recurrence`
    /// (G3-pre) declares a standing cadence INSIDE the digest-bound body;
    /// an older daemon rejects the unknown field at strict intake rather
    /// than silently parking a one-shot.
    ProposeEffect {
        id: String,
        goal: String,
        fire_at_ms: u64,
        #[serde(default)]
        orchestrate: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recurrence: Option<RecurrenceSpec>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Owner "run the standing manifest now" (G3-pre): one extra
    /// occurrence of the item's ALREADY-APPROVED recurring manifest at
    /// this instant — within the reviewed decision, so no new approval
    /// ceremony. **Owner-surface only**, like the approval whose authority
    /// it exercises. Recurring effects only (a one-shot's "run again" is
    /// `start_now`'s revise flow); refused while suspended, while a run is
    /// in flight, or while an earlier request still pends.
    RequestOccurrence { id: String },
    /// Append an attributed, timestamped note to any item, any status —
    /// the thread under it (A4's answer op generalized). Notes are data,
    /// never instructions; history is the full log, render caps with an
    /// expander.
    Annotate {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// State a blocking criterion on an open item. Plain text about the
    /// world — NO watcher, poller, or condition language ever evaluates
    /// it. The daemon mints the blocker id at intake.
    SetBlocker {
        id: String,
        criterion: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Clear one blocker — an op, never a deletion: the entry stays as
    /// history with the clearing actor. Plain `agenda.write` (the owner
    /// decision governs agent CONDUCT via mandate text, not capability).
    ClearBlocker {
        id: String,
        blocker_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Add a dependency edge: this item relies on `target_id`.
    /// Satisfaction is derived at render time from the target's status —
    /// nothing evaluates, nothing fires.
    AddReliesOn {
        id: String,
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Remove a live dependency edge (an op; the log keeps history).
    RemoveReliesOn {
        id: String,
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Place an item under a parent (G2 subordination). Intake rejects a
    /// second live parent — re-parent with [`AgendaCommand::Place`] or an
    /// explicit remove+add pair.
    AddPartOf {
        id: String,
        parent_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Remove the live placement (an op; the log keeps history).
    RemovePartOf {
        id: String,
        parent_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Re-parent in one gesture (steward override, 2026-07-22): validate
    /// the NEW placement first, then emit the primitive
    /// `remove_part_of` + `add_part_of` pair under one lock — a refused
    /// target never destroys the current placement (the two-call
    /// footgun). The op vocabulary is unchanged: the log carries the two
    /// primitive lines.
    Place {
        id: String,
        under: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Add an undirected adjacency edge (stored on this item). Purely
    /// navigational.
    AddRelatesTo {
        id: String,
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Remove an adjacency edge. The daemon resolves which side stores
    /// it — callers name the pair in either order.
    RemoveRelatesTo {
        id: String,
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Attach a typed reference (G1): a pointer, never content. For file
    /// refs the daemon digests the file at intake and records the digest
    /// in the op — the command carries none.
    AddRef {
        id: String,
        ref_type: AgendaRefType,
        locator: String,
        #[serde(default)]
        must_read: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Remove a live ref by its full address (an op; the log keeps
    /// history).
    RemoveRef {
        id: String,
        ref_type: AgendaRefType,
        locator: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Approve the current manifest revision by its digest. **An
    /// owner-surface act** — the tenant edge refuses agent-session, peer,
    /// and unattributed actors with a named denial.
    ApproveEffect { id: String, digest: String },
    /// Withdraw the approval (owner-surface, like granting it).
    RevokeEffect { id: String },
    /// Owner "start session now" (F3): mint a manifest from the item —
    /// goal is the item's title + body quoted as data with its id — and
    /// approve it in the same atomic act, firing through the ordinary
    /// scheduled lane (occurrence journal + StartTask), never a bypass.
    /// **Owner-surface only**, like the approval it embeds: the gesture IS
    /// the owner act, and the appended approve op binds the digest of
    /// exactly the manifest minted under the same lock. Revises the item's
    /// single session effect if one exists (stable lineage, fresh digest,
    /// prior approval void — the standing re-propose semantics).
    ///
    /// The optional fields are the confirm sheet's reviewed parameters
    /// (additive — a bare `{op, id}` keeps working):
    /// - `goal`: replaces the default item-statement text. The daemon
    ///   still appends the mode coda (interactive framing, or the
    ///   goal-run follow-through/write-back instructions).
    /// - `project_root`: explicit project directory for the spawned
    ///   session. Absent: the parking session's recorded project root,
    ///   else the daemon default — refused with a named error when
    ///   neither exists (never a project-less spawn).
    /// - `interactive`: absent defaults to **true** (owner-ratified): the
    ///   session opens with the goal as its first user message and waits
    ///   for the owner. `false` is the autonomous goal run.
    /// - `agent_config`: the confirm sheet's reviewed launch pins (the
    ///   CreateSession vocabulary — backend/model/effort). Recorded on the
    ///   minted manifest and applied by the spawn's resolution chain
    ///   (explicit pin → daemon default → backend default); absent fields
    ///   inherit the daemon defaults honestly.
    StartNow {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_root: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interactive: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_config: Option<Box<crate::event::AgentLaunchConfig>>,
    },
}

impl AgendaItem {
    /// Every session id this item's attribution views reference (birth
    /// provenance, answer, effect proposals and runs, session-type refs) —
    /// the set a display surface resolves to conversations and names.
    /// Deduplication is the caller's concern.
    pub(crate) fn referenced_session_ids(&self) -> impl Iterator<Item = &str> {
        self.provenance
            .session_id
            .as_deref()
            .into_iter()
            .chain(self.answer.as_ref().and_then(|a| a.session_id.as_deref()))
            .chain(self.effects.iter().flat_map(|effect| {
                effect.proposed_session_id.as_deref().into_iter().chain(
                    effect
                        .last_run
                        .as_ref()
                        .and_then(|run| run.session_id.as_deref()),
                )
            }))
            .chain(self.refs.iter().filter_map(|r| {
                (r.ref_type == AgendaRefType::Session).then_some(r.locator.as_str())
            }))
    }
}

impl AgendaCommand {
    /// Detach the self-described `--source` label for envelope recording.
    /// The owner-surface verbs (approve/revoke) carry none by design.
    pub(crate) fn take_source(&mut self) -> Option<String> {
        match self {
            AgendaCommand::Add { source, .. }
            | AgendaCommand::Patch { source, .. }
            | AgendaCommand::Complete { source, .. }
            | AgendaCommand::Reopen { source, .. }
            | AgendaCommand::Retire { source, .. }
            | AgendaCommand::Answer { source, .. }
            | AgendaCommand::ProposeEffect { source, .. }
            | AgendaCommand::Annotate { source, .. }
            | AgendaCommand::SetBlocker { source, .. }
            | AgendaCommand::ClearBlocker { source, .. }
            | AgendaCommand::AddReliesOn { source, .. }
            | AgendaCommand::RemoveReliesOn { source, .. }
            | AgendaCommand::AddPartOf { source, .. }
            | AgendaCommand::RemovePartOf { source, .. }
            | AgendaCommand::Place { source, .. }
            | AgendaCommand::AddRelatesTo { source, .. }
            | AgendaCommand::RemoveRelatesTo { source, .. }
            | AgendaCommand::AddRef { source, .. }
            | AgendaCommand::RemoveRef { source, .. } => source.take(),
            AgendaCommand::Ask { .. }
            | AgendaCommand::ApproveEffect { .. }
            | AgendaCommand::RevokeEffect { .. }
            | AgendaCommand::RequestOccurrence { .. }
            | AgendaCommand::StartNow { .. } => None,
        }
    }
}

/// A durable op — the payload of one log line. Compatible with the
/// umbrella RFC §7.2 vocabulary (`Answer` rides the `question` kind per
/// §7.1; `propose_effect`/`approve_effect`/`revoke_effect` are the lean
/// projections of `ProposeEffect`/`ApproveEffectRevision`/
/// `RevokeEffectApproval`, and `record_occurrence` of
/// `RecordOccurrenceStarted`/`RecordOccurrenceResult`). `record_occurrence`
/// deliberately has **no command twin**: only the daemon's scheduler
/// authors it, so no external surface can forge run results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AgendaOp {
    Add {
        id: String,
        kind: AgendaKind,
        title: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        body: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        due_ms: Option<u64>,
        /// Rich-ask payload for `ask`-parked questions (additive: older
        /// builds skip the field and fold a plain question item).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ask: Option<AgendaAsk>,
    },
    Patch {
        id: String,
        patch: AgendaPatch,
    },
    Complete {
        id: String,
    },
    Reopen {
        id: String,
    },
    Retire {
        id: String,
    },
    Answer {
        id: String,
        text: String,
        /// Structured rich-ask resolution (additive; older builds skip it
        /// and fold the text alone).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured: Option<AgendaAskResolution>,
    },
    /// Dismissal marker on an open question (rail skip/deny): recorded as
    /// history, the item stays open. Older builds skip the whole line
    /// (unknown op vocabulary) — consistent, since dismissal changes no
    /// lifecycle state.
    Dismiss {
        id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        action: String,
    },
    /// Attributed note (F2). Attribution + `source` ride the envelope.
    Annotate {
        id: String,
        text: String,
    },
    /// Blocker set (F2): `blocker_id` was minted at intake and is recorded
    /// here — replay never mints.
    SetBlocker {
        id: String,
        blocker_id: String,
        criterion: String,
    },
    /// Blocker cleared (F2): an op, never a deletion.
    ClearBlocker {
        id: String,
        blocker_id: String,
    },
    /// Dependency edge added (F2).
    AddReliesOn {
        id: String,
        target_id: String,
    },
    /// Dependency edge removed (F2): drops the view edge; history stays.
    RemoveReliesOn {
        id: String,
        target_id: String,
    },
    /// Placement set (G2). The fold enforces single-live-parent by
    /// warn-ignoring a second add; cycle/depth/children strictness lives
    /// at intake, and a cycle in a foreign log is a render concern.
    AddPartOf {
        id: String,
        parent_id: String,
    },
    /// Placement removed (G2): clears the view; history stays.
    RemovePartOf {
        id: String,
        parent_id: String,
    },
    /// Adjacency edge added (G2), stored on `id`'s side.
    AddRelatesTo {
        id: String,
        target_id: String,
    },
    /// Adjacency edge removed (G2).
    RemoveRelatesTo {
        id: String,
        target_id: String,
    },
    /// Typed reference attached (G1). `digest` (file refs) was minted at
    /// intake and is recorded here — replay never hashes (§7 purity).
    AddRef {
        id: String,
        ref_type: AgendaRefType,
        locator: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        digest: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        must_read: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Typed reference removed (G1): drops the view ref; history stays.
    RemoveRef {
        id: String,
        ref_type: AgendaRefType,
        locator: String,
    },
    ProposeEffect {
        id: String,
        effect_id: String,
        manifest: SessionManifest,
    },
    ApproveEffect {
        id: String,
        effect_id: String,
        digest: String,
    },
    RevokeEffect {
        id: String,
        effect_id: String,
    },
    /// Owner-requested extra instant of an approved standing manifest
    /// (G3-pre). `digest` names the approved revision the request
    /// exercises; `at_ms` is the instant, minted at intake and read here
    /// on replay (never the clock). A request folded against a since-
    /// revised effect (digest mismatch) is warn-skipped — it pointed at
    /// bytes that no longer stand.
    RequestOccurrence {
        id: String,
        effect_id: String,
        digest: String,
        at_ms: u64,
    },
    /// Daemon-authored occurrence outcome (scheduler only — see the enum
    /// docs). Writes the run result back onto the item's effect.
    RecordOccurrence {
        id: String,
        effect_id: String,
        occurrence_id: String,
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Daemon-authored ask-delivery write-back (the session supervisor's
    /// delivery arm only — no command twin, like `record_occurrence`):
    /// whether the recorded answer reached a live asking session (or its
    /// resume-lineage successor). `session_id` is the receiving session
    /// when delivered — history for the log; the fold keeps only the
    /// boolean on [`AgendaAnswer::delivered`]. Older builds skip the whole
    /// line by the usual forward-compat rule.
    RecordAskDelivery {
        id: String,
        delivered: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
}

impl AgendaOp {
    /// The id of the item this op addresses.
    pub(crate) fn item_id(&self) -> &str {
        match self {
            AgendaOp::Add { id, .. }
            | AgendaOp::Patch { id, .. }
            | AgendaOp::Complete { id }
            | AgendaOp::Reopen { id }
            | AgendaOp::Retire { id }
            | AgendaOp::Answer { id, .. }
            | AgendaOp::Dismiss { id, .. }
            | AgendaOp::Annotate { id, .. }
            | AgendaOp::SetBlocker { id, .. }
            | AgendaOp::ClearBlocker { id, .. }
            | AgendaOp::AddReliesOn { id, .. }
            | AgendaOp::RemoveReliesOn { id, .. }
            | AgendaOp::AddPartOf { id, .. }
            | AgendaOp::RemovePartOf { id, .. }
            | AgendaOp::AddRelatesTo { id, .. }
            | AgendaOp::RemoveRelatesTo { id, .. }
            | AgendaOp::AddRef { id, .. }
            | AgendaOp::RemoveRef { id, .. }
            | AgendaOp::ProposeEffect { id, .. }
            | AgendaOp::ApproveEffect { id, .. }
            | AgendaOp::RevokeEffect { id, .. }
            | AgendaOp::RequestOccurrence { id, .. }
            | AgendaOp::RecordOccurrence { id, .. }
            | AgendaOp::RecordAskDelivery { id, .. } => id,
        }
    }
}

/// One log line: a versioned envelope around a durable op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgendaOpRecord {
    /// Line-format version; bump only on a breaking encoding change.
    pub(crate) v: u32,
    pub(crate) at_ms: u64,
    #[serde(default, skip_serializing_if = "actor_is_empty")]
    pub(crate) actor: Option<AgendaActor>,
    /// Self-described caller label (`--source`), recorded verbatim beside —
    /// never inside — the gate-resolved actor. UNVERIFIED by doctrine; the
    /// owner-surface verbs (approve/revoke) accept none. Older builds
    /// tolerate the field (serde ignores unknown envelope fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    pub(crate) op: AgendaOp,
}

fn actor_is_empty(actor: &Option<AgendaActor>) -> bool {
    actor.as_ref().is_none_or(AgendaActor::is_empty)
}

pub(crate) const AGENDA_LOG_VERSION: u32 = 1;

/// Item counts by status, for card badges and `ctl agenda list` summaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaCounts {
    pub(crate) open: u64,
    pub(crate) done: u64,
    pub(crate) retired: u64,
}

/// Fold one record into derived state. Tolerant by design: the fold only
/// ever *warns* (returned as `Some(reason)`) and keeps going — the log is
/// append-only history, and a line the current build cannot apply (crash
/// artifact, hand edit, op from a newer build) must never brick the ledger.
/// Strictness lives at command intake, before anything is appended.
pub(crate) fn apply_op(
    items: &mut BTreeMap<String, AgendaItem>,
    rec: &AgendaOpRecord,
) -> Option<String> {
    let at_ms = rec.at_ms;
    match &rec.op {
        AgendaOp::Add {
            id,
            kind,
            title,
            body,
            tags,
            due_ms,
            ask,
        } => {
            if items.contains_key(id) {
                return Some(format!("duplicate add for {id} ignored"));
            }
            let actor = rec.actor.clone().unwrap_or_default();
            items.insert(
                id.clone(),
                AgendaItem {
                    id: id.clone(),
                    kind: *kind,
                    title: title.clone(),
                    body: body.clone(),
                    tags: tags.clone(),
                    due_ms: *due_ms,
                    provenance: AgendaProvenance {
                        principal: actor.principal,
                        session_id: actor.session_id,
                        kind: actor.kind,
                        source: rec.source.clone(),
                        created_ms: at_ms,
                    },
                    status: AgendaStatus::Open,
                    updated_ms: at_ms,
                    completed_ms: None,
                    answer: None,
                    effects: Vec::new(),
                    ask: ask.clone(),
                    dismissed: None,
                    annotations: Vec::new(),
                    blockers: Vec::new(),
                    relies_on: Vec::new(),
                    refs: Vec::new(),
                    part_of: None,
                    relates_to: Vec::new(),
                    deferred_until: None,
                },
            );
            None
        }
        AgendaOp::Annotate { id, text } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("annotate for unknown {id} ignored"));
            };
            let actor = rec.actor.clone().unwrap_or_default();
            item.annotations.push(AgendaAnnotation {
                text: text.clone(),
                at_ms,
                principal: actor.principal,
                session_id: actor.session_id,
                kind: actor.kind,
                source: rec.source.clone(),
            });
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::SetBlocker {
            id,
            blocker_id,
            criterion,
        } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("set_blocker for unknown {id} ignored"));
            };
            if item.blockers.iter().any(|b| b.blocker_id == *blocker_id) {
                return Some(format!("duplicate blocker {blocker_id} on {id} ignored"));
            }
            let actor = rec.actor.clone().unwrap_or_default();
            item.blockers.push(AgendaBlocker {
                blocker_id: blocker_id.clone(),
                criterion: criterion.clone(),
                set_ms: at_ms,
                principal: actor.principal,
                session_id: actor.session_id,
                kind: actor.kind,
                source: rec.source.clone(),
                cleared: None,
            });
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::ClearBlocker { id, blocker_id } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("clear_blocker for unknown {id} ignored"));
            };
            let Some(blocker) = item
                .blockers
                .iter_mut()
                .find(|b| b.blocker_id == *blocker_id)
            else {
                return Some(format!(
                    "clear_blocker for unknown {blocker_id} on {id} ignored"
                ));
            };
            if blocker.cleared.is_some() {
                return Some(format!("blocker {blocker_id} on {id} already cleared"));
            }
            let actor = rec.actor.clone().unwrap_or_default();
            blocker.cleared = Some(AgendaBlockerClear {
                at_ms,
                principal: actor.principal,
                session_id: actor.session_id,
                kind: actor.kind,
                source: rec.source.clone(),
            });
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::AddReliesOn { id, target_id } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("add_relies_on for unknown {id} ignored"));
            };
            if target_id == id {
                return Some(format!("self-edge on {id} ignored"));
            }
            if item.relies_on.iter().any(|e| e.target_id == *target_id) {
                return Some(format!("duplicate edge {id}→{target_id} ignored"));
            }
            // A target missing from this fold (partial/foreign replay) is
            // tolerated — the render marks the edge "prerequisite missing".
            let actor = rec.actor.clone().unwrap_or_default();
            item.relies_on.push(AgendaDependency {
                target_id: target_id.clone(),
                added_ms: at_ms,
                principal: actor.principal,
                session_id: actor.session_id,
                kind: actor.kind,
                source: rec.source.clone(),
            });
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::RemoveReliesOn { id, target_id } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("remove_relies_on for unknown {id} ignored"));
            };
            let before = item.relies_on.len();
            item.relies_on.retain(|e| e.target_id != *target_id);
            if item.relies_on.len() == before {
                return Some(format!(
                    "remove_relies_on for absent edge {id}→{target_id} ignored"
                ));
            }
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::AddPartOf { id, parent_id } => {
            if parent_id == id {
                return Some(format!("self-placement on {id} ignored"));
            }
            let Some(item) = items.get_mut(id) else {
                return Some(format!("add_part_of for unknown {id} ignored"));
            };
            if let Some(placement) = &item.part_of {
                return Some(format!(
                    "add_part_of on {id} already placed under {} ignored",
                    placement.parent_id
                ));
            }
            // A parent missing from this fold (partial/foreign replay) is
            // tolerated — the render marks the placement "parent missing".
            let actor = rec.actor.clone().unwrap_or_default();
            item.part_of = Some(AgendaPlacement {
                parent_id: parent_id.clone(),
                added_ms: at_ms,
                principal: actor.principal,
                session_id: actor.session_id,
                kind: actor.kind,
                source: rec.source.clone(),
            });
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::RemovePartOf { id, parent_id } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("remove_part_of for unknown {id} ignored"));
            };
            match &item.part_of {
                Some(placement) if placement.parent_id == *parent_id => {
                    item.part_of = None;
                    item.updated_ms = at_ms;
                    None
                }
                _ => Some(format!(
                    "remove_part_of for absent placement {id}→{parent_id} ignored"
                )),
            }
        }
        AgendaOp::AddRelatesTo { id, target_id } => {
            if target_id == id {
                return Some(format!("self-relation on {id} ignored"));
            }
            let Some(item) = items.get_mut(id) else {
                return Some(format!("add_relates_to for unknown {id} ignored"));
            };
            if item.relates_to.iter().any(|e| e.target_id == *target_id) {
                return Some(format!("duplicate relation {id}↔{target_id} ignored"));
            }
            // Either-direction dedup is intake's job; the fold stores what
            // the log says (renders dedup the undirected union).
            let actor = rec.actor.clone().unwrap_or_default();
            item.relates_to.push(AgendaRelation {
                target_id: target_id.clone(),
                added_ms: at_ms,
                principal: actor.principal,
                session_id: actor.session_id,
                kind: actor.kind,
                source: rec.source.clone(),
            });
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::RemoveRelatesTo { id, target_id } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("remove_relates_to for unknown {id} ignored"));
            };
            let before = item.relates_to.len();
            item.relates_to.retain(|e| e.target_id != *target_id);
            if item.relates_to.len() == before {
                return Some(format!(
                    "remove_relates_to for absent relation {id}↔{target_id} ignored"
                ));
            }
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::AddRef {
            id,
            ref_type,
            locator,
            digest,
            must_read,
            label,
        } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("add_ref for unknown {id} ignored"));
            };
            if item
                .refs
                .iter()
                .any(|r| r.ref_type == *ref_type && r.locator == *locator)
            {
                return Some(format!(
                    "duplicate {} ref on {id} ignored",
                    ref_type.as_str()
                ));
            }
            let actor = rec.actor.clone().unwrap_or_default();
            item.refs.push(AgendaRef {
                ref_type: *ref_type,
                locator: locator.clone(),
                digest: digest.clone(),
                must_read: *must_read,
                label: label.clone(),
                added_ms: at_ms,
                principal: actor.principal,
                session_id: actor.session_id,
                kind: actor.kind,
                source: rec.source.clone(),
            });
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::RemoveRef {
            id,
            ref_type,
            locator,
        } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("remove_ref for unknown {id} ignored"));
            };
            let before = item.refs.len();
            item.refs
                .retain(|r| !(r.ref_type == *ref_type && r.locator == *locator));
            if item.refs.len() == before {
                return Some(format!(
                    "remove_ref for absent {} ref on {id} ignored",
                    ref_type.as_str()
                ));
            }
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::Patch { id, patch } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("patch for unknown {id} ignored"));
            };
            if let Some(title) = &patch.title {
                item.title = title.clone();
            }
            if let Some(body) = &patch.body {
                item.body = body.clone();
            }
            if let Some(tags) = &patch.tags {
                item.tags = tags.clone();
            }
            if let Some(due) = patch.due_ms {
                item.due_ms = due;
            }
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::Complete { id } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("complete for unknown {id} ignored"));
            };
            match item.status {
                AgendaStatus::Open => {
                    item.status = AgendaStatus::Done;
                    item.completed_ms = Some(at_ms);
                    item.updated_ms = at_ms;
                    None
                }
                AgendaStatus::Done => None,
                AgendaStatus::Retired => Some(format!("complete on retired {id} ignored")),
            }
        }
        AgendaOp::Reopen { id } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("reopen for unknown {id} ignored"));
            };
            match item.status {
                AgendaStatus::Done | AgendaStatus::Retired => {
                    item.status = AgendaStatus::Open;
                    item.completed_ms = None;
                    // Re-asking a question awaits a fresh reply; earlier
                    // replies (and dismissals) remain in the log as history.
                    item.answer = None;
                    item.dismissed = None;
                    item.updated_ms = at_ms;
                    None
                }
                AgendaStatus::Open => None,
            }
        }
        AgendaOp::Retire { id } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("retire for unknown {id} ignored"));
            };
            match item.status {
                AgendaStatus::Retired => None,
                // `completed_ms` survives retirement: it is history, and
                // history is the diary's raw material.
                AgendaStatus::Open | AgendaStatus::Done => {
                    item.status = AgendaStatus::Retired;
                    item.updated_ms = at_ms;
                    None
                }
            }
        }
        AgendaOp::ProposeEffect {
            id,
            effect_id,
            manifest,
        } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("propose_effect for unknown {id} ignored"));
            };
            let actor = rec.actor.clone().unwrap_or_default();
            let effect = AgendaEffect {
                effect_id: effect_id.clone(),
                digest: manifest_digest(id, effect_id, manifest),
                manifest: manifest.clone(),
                proposed_ms: at_ms,
                proposed_principal: actor.principal,
                proposed_session_id: actor.session_id,
                proposed_kind: actor.kind,
                // A new revision voids any standing approval — the owner
                // approved different bytes. The failure streak and any
                // pending requests belonged to those bytes too: reset.
                approval: None,
                last_run: None,
                consecutive_failures: 0,
                requested: Vec::new(),
                next_fire_ms: None,
            };
            match item.effects.iter_mut().find(|e| e.effect_id == *effect_id) {
                Some(existing) => *existing = effect,
                None => item.effects.push(effect),
            }
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::ApproveEffect {
            id,
            effect_id,
            digest,
        } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("approve_effect for unknown {id} ignored"));
            };
            let Some(effect) = item.effects.iter_mut().find(|e| e.effect_id == *effect_id) else {
                return Some(format!("approve_effect for unknown effect on {id} ignored"));
            };
            if effect.digest != *digest {
                return Some(format!(
                    "approve_effect digest mismatch on {id} ignored (manifest superseded)"
                ));
            }
            let actor = rec.actor.clone().unwrap_or_default();
            effect.approval = Some(AgendaApproval {
                digest: digest.clone(),
                at_ms,
                principal: actor.principal,
                kind: actor.kind,
            });
            // The approve op is the streak reset (G3-pre): re-approving an
            // unchanged digest is the one-click re-arm of a suspended
            // standing effect — no new vocabulary.
            effect.consecutive_failures = 0;
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::RevokeEffect { id, effect_id } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("revoke_effect for unknown {id} ignored"));
            };
            let Some(effect) = item.effects.iter_mut().find(|e| e.effect_id == *effect_id) else {
                return Some(format!("revoke_effect for unknown effect on {id} ignored"));
            };
            effect.approval = None;
            // A request exists only under an approval.
            effect.requested.clear();
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::RequestOccurrence {
            id,
            effect_id,
            digest,
            at_ms: instant,
        } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("request_occurrence for unknown {id} ignored"));
            };
            let Some(effect) = item.effects.iter_mut().find(|e| e.effect_id == *effect_id) else {
                return Some(format!(
                    "request_occurrence for unknown effect on {id} ignored"
                ));
            };
            if effect.digest != *digest {
                return Some(format!(
                    "request_occurrence digest mismatch on {id} ignored (manifest revised)"
                ));
            }
            let actor = rec.actor.clone().unwrap_or_default();
            effect.requested.push(AgendaRequestedRun {
                at_ms: *instant,
                principal: actor.principal,
                kind: actor.kind,
            });
            if effect.requested.len() > MAX_REQUESTED_RUNS_VIEW {
                let drop = effect.requested.len() - MAX_REQUESTED_RUNS_VIEW;
                effect.requested.drain(..drop);
            }
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::RecordOccurrence {
            id,
            effect_id,
            occurrence_id,
            state,
            session_id,
            note,
        } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("record_occurrence for unknown {id} ignored"));
            };
            let Some(effect) = item.effects.iter_mut().find(|e| e.effect_id == *effect_id) else {
                return Some(format!(
                    "record_occurrence for unknown effect on {id} ignored"
                ));
            };
            effect.last_run = Some(AgendaRun {
                occurrence_id: occurrence_id.clone(),
                state: state.clone(),
                session_id: session_id.clone(),
                at_ms,
                note: note.clone(),
            });
            // The failure streak (G3-pre), from log order alone: attempted
            // non-success counts, success resets, `missed` (daemon
            // downtime, not the mandate's fault) and `started` are
            // neutral.
            match state.as_str() {
                "failed" | "unknown" => {
                    effect.consecutive_failures = effect.consecutive_failures.saturating_add(1);
                }
                "completed" => effect.consecutive_failures = 0,
                _ => {}
            }
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::Answer {
            id,
            text,
            structured,
        } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("answer for unknown {id} ignored"));
            };
            if item.kind != AgendaKind::Question {
                return Some(format!("answer on non-question {id} ignored"));
            }
            match item.status {
                AgendaStatus::Open => {
                    let actor = rec.actor.clone().unwrap_or_default();
                    item.answer = Some(AgendaAnswer {
                        text: text.clone(),
                        at_ms,
                        principal: actor.principal,
                        session_id: actor.session_id,
                        kind: actor.kind,
                        structured: structured.clone(),
                        // Delivery is a later fact: the supervisor's
                        // delivery arm records it as its own op.
                        delivered: None,
                    });
                    // A reply resolves the question (an earlier dismissal
                    // is history the answer supersedes).
                    item.dismissed = None;
                    item.status = AgendaStatus::Done;
                    item.completed_ms = Some(at_ms);
                    item.updated_ms = at_ms;
                    None
                }
                AgendaStatus::Done | AgendaStatus::Retired => {
                    Some(format!("answer on resolved {id} ignored"))
                }
            }
        }
        AgendaOp::RecordAskDelivery { id, delivered, .. } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("record_ask_delivery for unknown {id} ignored"));
            };
            // The marker annotates the CURRENT answer. Reopen cleared the
            // answer view (the log keeps both as history), so a marker
            // arriving after a reopen has nothing to annotate.
            let Some(answer) = item.answer.as_mut() else {
                return Some(format!(
                    "record_ask_delivery on {id} without a current answer ignored"
                ));
            };
            answer.delivered = Some(*delivered);
            item.updated_ms = at_ms;
            None
        }
        AgendaOp::Dismiss { id, action } => {
            let Some(item) = items.get_mut(id) else {
                return Some(format!("dismiss for unknown {id} ignored"));
            };
            if item.kind != AgendaKind::Question {
                return Some(format!("dismiss on non-question {id} ignored"));
            }
            match item.status {
                AgendaStatus::Open => {
                    let actor = rec.actor.clone().unwrap_or_default();
                    item.dismissed = Some(AgendaDismissal {
                        action: action.clone(),
                        at_ms,
                        principal: actor.principal,
                        session_id: actor.session_id,
                        kind: actor.kind,
                    });
                    // Deliberately NOT a lifecycle transition: a parked
                    // question survives dismissal — that's the point.
                    item.updated_ms = at_ms;
                    None
                }
                AgendaStatus::Done | AgendaStatus::Retired => {
                    Some(format!("dismiss on resolved {id} ignored"))
                }
            }
        }
    }
}

/// Render-time judgment of one dependency edge: `(satisfied, review)`.
/// A completed target satisfies; a retired target does NOT silently
/// satisfy (review `"target_retired"`); a target absent from the fold —
/// foreign/partial replay — is review `"target_missing"`. Direct status
/// lookup only: cycles need no machinery (every member of a cycle has a
/// non-Done target and simply derives blocked), nothing walks, nothing
/// evaluates, nothing fires.
///
/// This is presentation, deliberately NOT a stored or wire field (the DTO
/// stays the pure fold product — the D0-Agenda-Data migration replays the
/// log verbatim). The dashboard and ctl derive the same judgment from the
/// serialized items like the overdue chip; this typed twin exists to PIN
/// the semantics in unit tests (the retire-review and cycle rules).
#[cfg(test)]
pub(crate) fn dependency_state(
    items: &BTreeMap<String, AgendaItem>,
    edge: &AgendaDependency,
) -> (bool, Option<&'static str>) {
    match items.get(&edge.target_id) {
        None => (false, Some("target_missing")),
        Some(target) => match target.status {
            AgendaStatus::Done => (true, None),
            AgendaStatus::Retired => (false, Some("target_retired")),
            AgendaStatus::Open => (false, None),
        },
    }
}

/// Render-time blocked judgment: an open item with any uncleared blocker
/// or any unsatisfied live dependency. Never stored, never on the wire,
/// never notifies — A3's time lane remains the only thing that fires.
#[cfg(test)]
pub(crate) fn is_blocked(items: &BTreeMap<String, AgendaItem>, item: &AgendaItem) -> bool {
    item.status == AgendaStatus::Open
        && (item.blockers.iter().any(|b| b.cleared.is_none())
            || item
                .relies_on
                .iter()
                .any(|edge| !dependency_state(items, edge).0))
}

pub(crate) fn counts(items: &BTreeMap<String, AgendaItem>) -> AgendaCounts {
    let mut c = AgendaCounts::default();
    for item in items.values() {
        match item.status {
            AgendaStatus::Open => c.open += 1,
            AgendaStatus::Done => c.done += 1,
            AgendaStatus::Retired => c.retired += 1,
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(at_ms: u64, op: AgendaOp) -> AgendaOpRecord {
        AgendaOpRecord {
            v: AGENDA_LOG_VERSION,
            at_ms,
            actor: None,
            source: None,
            op,
        }
    }

    fn add(id: &str, title: &str) -> AgendaOp {
        AgendaOp::Add {
            id: id.to_string(),
            kind: AgendaKind::Task,
            title: title.to_string(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            ask: None,
        }
    }

    /// The manifest's additive fields are absent-on-the-wire at their
    /// defaults: legacy manifest JSON round-trips byte-identically, so the
    /// digests existing approvals bind are unchanged — and setting either
    /// field mints a NEW digest, voiding any prior approval (the review
    /// contract: an approval covers exactly the bytes reviewed).
    #[test]
    fn manifest_additive_fields_round_trip_and_revise_the_digest() {
        let legacy_json = r#"{"goal":"run the sweep","fire_at_ms":1234}"#;
        let legacy: SessionManifest = serde_json::from_str(legacy_json).unwrap();
        assert!(!legacy.interactive);
        assert_eq!(legacy.project_root, None);
        assert_eq!(serde_json::to_string(&legacy).unwrap(), legacy_json);

        let base_digest = manifest_digest("item-1", "ef-1", &legacy);
        assert_eq!(
            manifest_digest("item-1", "ef-1", &legacy),
            base_digest,
            "digesting is deterministic"
        );

        let interactive = SessionManifest {
            interactive: true,
            ..legacy.clone()
        };
        let with_project = SessionManifest {
            project_root: Some("/work/project".into()),
            ..legacy.clone()
        };
        let with_config = SessionManifest {
            agent_config: Some(Box::new(crate::event::AgentLaunchConfig {
                agent: Some("claude-code".into()),
                claude_effort: Some("max".into()),
                ..Default::default()
            })),
            ..legacy.clone()
        };
        let interactive_digest = manifest_digest("item-1", "ef-1", &interactive);
        let project_digest = manifest_digest("item-1", "ef-1", &with_project);
        let config_digest = manifest_digest("item-1", "ef-1", &with_config);
        assert_ne!(interactive_digest, base_digest);
        assert_ne!(project_digest, base_digest);
        assert_ne!(interactive_digest, project_digest);
        assert_ne!(
            config_digest, base_digest,
            "setting the agent-config block revises the digest — the owner approves the config they reviewed"
        );

        // Full round-trip with the additive fields set preserves them.
        let full = SessionManifest {
            recurrence: None,
            goal: "g".into(),
            fire_at_ms: 9,
            orchestrate: true,
            interactive: true,
            project_root: Some("/work/project".into()),
            agent_config: Some(Box::new(crate::event::AgentLaunchConfig {
                agent: Some("claude-code".into()),
                claude_model: Some("haiku".into()),
                claude_effort: Some("xhigh".into()),
                ..Default::default()
            })),
        };
        let round: SessionManifest =
            serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        assert_eq!(round, full);
        // The block is a nested object on the manifest (never flattened —
        // manifest fields and config fields must not collide).
        let value = serde_json::to_value(&full).unwrap();
        assert_eq!(value["agent_config"]["claude_effort"], "xhigh");
    }

    #[test]
    fn fold_lifecycle_transitions() {
        let mut items = BTreeMap::new();
        assert!(apply_op(&mut items, &rec(1, add("a", "t"))).is_none());
        assert_eq!(items["a"].status, AgendaStatus::Open);

        assert!(apply_op(&mut items, &rec(2, AgendaOp::Complete { id: "a".into() })).is_none());
        assert_eq!(items["a"].status, AgendaStatus::Done);
        assert_eq!(items["a"].completed_ms, Some(2));

        assert!(apply_op(&mut items, &rec(3, AgendaOp::Reopen { id: "a".into() })).is_none());
        assert_eq!(items["a"].status, AgendaStatus::Open);
        assert_eq!(items["a"].completed_ms, None);

        assert!(apply_op(&mut items, &rec(4, AgendaOp::Complete { id: "a".into() })).is_none());
        assert!(apply_op(&mut items, &rec(5, AgendaOp::Retire { id: "a".into() })).is_none());
        assert_eq!(items["a"].status, AgendaStatus::Retired);
        // History survives retirement.
        assert_eq!(items["a"].completed_ms, Some(4));

        // Reopen resurrects a retired item (the one resurrection verb).
        assert!(apply_op(&mut items, &rec(6, AgendaOp::Reopen { id: "a".into() })).is_none());
        assert_eq!(items["a"].status, AgendaStatus::Open);
        assert_eq!(items["a"].completed_ms, None);
        assert_eq!(items["a"].updated_ms, 6);
    }

    #[test]
    fn fold_warns_and_survives_bad_ops() {
        let mut items = BTreeMap::new();
        assert!(apply_op(
            &mut items,
            &rec(1, AgendaOp::Complete { id: "nope".into() })
        )
        .is_some());
        assert!(apply_op(&mut items, &rec(2, add("a", "t"))).is_none());
        // Duplicate add: first birth wins.
        assert!(apply_op(&mut items, &rec(3, add("a", "other"))).is_some());
        assert_eq!(items["a"].title, "t");
        // Complete on retired warns and changes nothing.
        assert!(apply_op(&mut items, &rec(4, AgendaOp::Retire { id: "a".into() })).is_none());
        assert!(apply_op(&mut items, &rec(5, AgendaOp::Complete { id: "a".into() })).is_some());
        assert_eq!(items["a"].status, AgendaStatus::Retired);
        assert_eq!(items["a"].updated_ms, 4);
    }

    #[test]
    fn fold_patch_applies_presentation_fields() {
        let mut items = BTreeMap::new();
        apply_op(&mut items, &rec(1, add("a", "t")));
        let patch = AgendaPatch {
            title: Some("new title".into()),
            body: Some("body".into()),
            tags: Some(vec!["x".into()]),
            due_ms: Some(Some(99)),
        };
        assert!(apply_op(
            &mut items,
            &rec(
                2,
                AgendaOp::Patch {
                    id: "a".into(),
                    patch
                }
            )
        )
        .is_none());
        let item = &items["a"];
        assert_eq!(item.title, "new title");
        assert_eq!(item.body, "body");
        assert_eq!(item.tags, vec!["x".to_string()]);
        assert_eq!(item.due_ms, Some(99));
        // Clear due via the merge-patch null; keep everything else.
        let clear = AgendaPatch {
            due_ms: Some(None),
            ..AgendaPatch::default()
        };
        apply_op(
            &mut items,
            &rec(
                3,
                AgendaOp::Patch {
                    id: "a".into(),
                    patch: clear,
                },
            ),
        );
        assert_eq!(items["a"].due_ms, None);
        assert_eq!(items["a"].title, "new title");
    }

    /// Pins the wire format of the patch merge semantics: absent = keep,
    /// null = clear, value = set.
    #[test]
    fn patch_merge_semantics_on_the_wire() {
        let keep: AgendaPatch = serde_json::from_str(r#"{"title":"x"}"#).unwrap();
        assert_eq!(keep.due_ms, None);
        let clear: AgendaPatch = serde_json::from_str(r#"{"due_ms":null}"#).unwrap();
        assert_eq!(clear.due_ms, Some(None));
        let set: AgendaPatch = serde_json::from_str(r#"{"due_ms":1234}"#).unwrap();
        assert_eq!(set.due_ms, Some(Some(1234)));

        // Serialization round-trips each shape.
        assert_eq!(serde_json::to_string(&keep).unwrap(), r#"{"title":"x"}"#);
        assert_eq!(serde_json::to_string(&clear).unwrap(), r#"{"due_ms":null}"#);
        assert_eq!(serde_json::to_string(&set).unwrap(), r#"{"due_ms":1234}"#);
    }

    /// Pins the durable line format (v1). If this test changes, a migration
    /// story must exist — the log outlives builds.
    #[test]
    fn op_record_line_format_is_pinned() {
        let record = AgendaOpRecord {
            v: 1,
            at_ms: 42,
            actor: Some(AgendaActor {
                principal: Some("owner".into()),
                session_id: None,
                kind: None,
            }),
            source: None,
            op: AgendaOp::Add {
                id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                kind: AgendaKind::Note,
                title: "remember".into(),
                body: String::new(),
                tags: vec!["later".into()],
                due_ms: None,
                ask: None,
            },
        };
        let line = serde_json::to_string(&record).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"at_ms":42,"actor":{"principal":"owner"},"op":{"type":"add","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","kind":"note","title":"remember","tags":["later"]}}"#
        );
        let back: AgendaOpRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn command_wire_shapes_parse() {
        let add: AgendaCommand =
            serde_json::from_str(r#"{"op":"add","kind":"task","title":"do it"}"#).unwrap();
        assert!(matches!(add, AgendaCommand::Add { .. }));
        let complete: AgendaCommand =
            serde_json::from_str(r#"{"op":"complete","id":"01X"}"#).unwrap();
        assert!(matches!(complete, AgendaCommand::Complete { .. }));
        let patch: AgendaCommand =
            serde_json::from_str(r#"{"op":"patch","id":"01X","patch":{"due_ms":null}}"#).unwrap();
        match patch {
            AgendaCommand::Patch { patch, .. } => assert_eq!(patch.due_ms, Some(None)),
            other => panic!("unexpected {other:?}"),
        }
        // Unknown command fields are rejected (fail closed at intake) —
        // unknown *ops* in the log are tolerated instead (store tests).
        assert!(serde_json::from_str::<AgendaCommand>(
            r#"{"op":"add","title":"x","kind":"note","effect":"launch"}"#
        )
        .is_err());
    }

    /// The tenant-side mapping of the shared actor seam: unattributed
    /// callers record nothing; everyone else records principal/session/kind
    /// exactly as the gate resolved them.
    #[test]
    fn agenda_actor_maps_the_seam_faithfully() {
        use crate::access::actor::ActorBinding;
        assert_eq!(
            AgendaActor::from_binding(&ActorBinding::unattributed()),
            None
        );

        let agent = AgendaActor::from_binding(&ActorBinding::agent_session(
            Some("principal:agent-session:abc".into()),
            "sess-abc".into(),
        ))
        .unwrap();
        assert_eq!(
            agent.principal.as_deref(),
            Some("principal:agent-session:abc")
        );
        assert_eq!(agent.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(agent.kind.as_deref(), Some("agent_session"));

        // Trusted-local dashboard: no named principal, kind still recorded.
        let local = AgendaActor::from_binding(&ActorBinding::dashboard(None)).unwrap();
        assert_eq!(local.principal, None);
        assert_eq!(local.kind.as_deref(), Some("dashboard"));
    }

    /// A4: answering resolves; re-answer warns; reopen re-asks (clears
    /// the answer view, history stays in the log); answers carry the
    /// gate-resolved attribution; non-questions never accept answers.
    #[test]
    fn question_lifecycle_answer_resolves_and_reopen_reasks() {
        let mut items = BTreeMap::new();
        apply_op(
            &mut items,
            &rec(
                1,
                AgendaOp::Add {
                    id: "q".into(),
                    kind: AgendaKind::Question,
                    title: "Which DB for the cache?".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    ask: None,
                },
            ),
        );
        let mut answer = rec(
            2,
            AgendaOp::Answer {
                id: "q".into(),
                text: "sqlite is fine".into(),
                structured: None,
            },
        );
        answer.actor = Some(AgendaActor {
            principal: Some("principal:root:dashboard".into()),
            session_id: None,
            kind: Some("dashboard".into()),
        });
        assert!(apply_op(&mut items, &answer).is_none());
        let q = &items["q"];
        assert_eq!(q.status, AgendaStatus::Done);
        assert_eq!(q.completed_ms, Some(2));
        let reply = q.answer.as_ref().unwrap();
        assert_eq!(reply.text, "sqlite is fine");
        assert_eq!(reply.kind.as_deref(), Some("dashboard"));
        assert_eq!(reply.principal.as_deref(), Some("principal:root:dashboard"));

        // Re-answer on resolved warns and changes nothing.
        assert!(apply_op(
            &mut items,
            &rec(
                3,
                AgendaOp::Answer {
                    id: "q".into(),
                    text: "no, postgres".into(),
                    structured: None,
                }
            )
        )
        .is_some());
        assert_eq!(items["q"].answer.as_ref().unwrap().text, "sqlite is fine");

        // Reopen re-asks; a fresh answer lands.
        apply_op(&mut items, &rec(4, AgendaOp::Reopen { id: "q".into() }));
        assert_eq!(items["q"].status, AgendaStatus::Open);
        assert!(items["q"].answer.is_none());
        apply_op(
            &mut items,
            &rec(
                5,
                AgendaOp::Answer {
                    id: "q".into(),
                    text: "postgres after all".into(),
                    structured: None,
                },
            ),
        );
        assert_eq!(
            items["q"].answer.as_ref().unwrap().text,
            "postgres after all"
        );

        // Tasks never accept answers.
        apply_op(&mut items, &rec(6, add("t", "a task")));
        assert!(apply_op(
            &mut items,
            &rec(
                7,
                AgendaOp::Answer {
                    id: "t".into(),
                    text: "nope".into(),
                    structured: None,
                }
            )
        )
        .is_some());
        assert!(items["t"].answer.is_none());
        assert_eq!(items["t"].status, AgendaStatus::Open);
    }

    /// Pins the answer op's durable line format (additive to v1).
    #[test]
    fn answer_record_line_format_is_pinned() {
        let record = AgendaOpRecord {
            v: 1,
            at_ms: 7,
            actor: Some(AgendaActor {
                principal: Some("principal:root:dashboard".into()),
                session_id: None,
                kind: Some("dashboard".into()),
            }),
            source: None,
            op: AgendaOp::Answer {
                id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                text: "yes — ship it".into(),
                structured: None,
            },
        };
        let line = serde_json::to_string(&record).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"at_ms":7,"actor":{"principal":"principal:root:dashboard","kind":"dashboard"},"op":{"type":"answer","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","text":"yes — ship it"}}"#
        );
        let back: AgendaOpRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back, record);
    }

    /// The ask-delivery marker fold: a `record_ask_delivery` op annotates
    /// the CURRENT answer (`Some(false)` = recorded but unheard), a later
    /// op flips it (a successor delivery succeeding), and the marker never
    /// applies without an answer to annotate — unknown items, unanswered
    /// questions, and post-reopen items all warn-and-skip. Reopen clears
    /// the marker with the answer view it rides on.
    #[test]
    fn ask_delivery_marker_folds_flips_and_requires_an_answer() {
        let delivery = |at_ms: u64, delivered: bool, session: Option<&str>| {
            rec(
                at_ms,
                AgendaOp::RecordAskDelivery {
                    id: "q".into(),
                    delivered,
                    session_id: session.map(str::to_string),
                },
            )
        };
        let mut items = BTreeMap::new();
        // Unknown item: warn, nothing applied.
        assert!(apply_op(&mut items, &delivery(1, false, None)).is_some());

        apply_op(
            &mut items,
            &rec(
                2,
                AgendaOp::Add {
                    id: "q".into(),
                    kind: AgendaKind::Question,
                    title: "Which grid?".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    ask: None,
                },
            ),
        );
        // No answer yet: warn, nothing applied.
        assert!(apply_op(&mut items, &delivery(3, false, None)).is_some());

        apply_op(
            &mut items,
            &rec(
                4,
                AgendaOp::Answer {
                    id: "q".into(),
                    text: "A".into(),
                    structured: None,
                },
            ),
        );
        assert_eq!(
            items["q"].answer.as_ref().unwrap().delivered,
            None,
            "an answer claims nothing about delivery until the write-back"
        );
        assert!(apply_op(&mut items, &delivery(5, false, None)).is_none());
        assert_eq!(items["q"].answer.as_ref().unwrap().delivered, Some(false));
        assert_eq!(items["q"].updated_ms, 5);

        // A later successful successor delivery flips the marker.
        assert!(apply_op(&mut items, &delivery(6, true, Some("sess-successor"))).is_none());
        assert_eq!(items["q"].answer.as_ref().unwrap().delivered, Some(true));

        // Reopen clears the marker with the answer it annotates; a stale
        // marker arriving afterwards has nothing to annotate.
        apply_op(&mut items, &rec(7, AgendaOp::Reopen { id: "q".into() }));
        assert!(items["q"].answer.is_none());
        assert!(apply_op(&mut items, &delivery(8, true, None)).is_some());
    }

    /// Pins the ask-delivery op's durable line format (additive to v1;
    /// older builds skip the line, newer answers without the marker fold
    /// `delivered: None`).
    #[test]
    fn record_ask_delivery_line_format_is_pinned() {
        let record = AgendaOpRecord {
            v: 1,
            at_ms: 9,
            actor: None,
            source: None,
            op: AgendaOp::RecordAskDelivery {
                id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                delivered: true,
                session_id: Some("sess-successor".into()),
            },
        };
        let line = serde_json::to_string(&record).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"at_ms":9,"op":{"type":"record_ask_delivery","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","delivered":true,"session_id":"sess-successor"}}"#
        );
        let back: AgendaOpRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back, record);

        // The undelivered form omits the absent session (additive field).
        let undelivered = AgendaOpRecord {
            op: AgendaOp::RecordAskDelivery {
                id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                delivered: false,
                session_id: None,
            },
            ..record
        };
        assert_eq!(
            serde_json::to_string(&undelivered).unwrap(),
            r#"{"v":1,"at_ms":9,"op":{"type":"record_ask_delivery","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","delivered":false}}"#
        );
    }

    /// DTO forward-compat for the marker: an answer serialized by an older
    /// build (no `delivered` field) deserializes to `None`, and a `None`
    /// marker stays off the wire (the answered-item pin above carries no
    /// `delivered` key).
    #[test]
    fn answer_without_delivered_field_deserializes_to_none() {
        let answer: AgendaAnswer = serde_json::from_str(r#"{"text":"A","at_ms":2}"#).unwrap();
        assert_eq!(answer.delivered, None);
        assert!(!serde_json::to_string(&answer)
            .unwrap()
            .contains("delivered"));

        let marked = AgendaAnswer {
            delivered: Some(false),
            ..answer
        };
        let wire = serde_json::to_string(&marked).unwrap();
        assert!(wire.contains(r#""delivered":false"#), "{wire}");
        let back: AgendaAnswer = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.delivered, Some(false));
    }

    /// The dashboard's "answered · awaiting pickup" chip renders from
    /// exactly this DTO contract: a DONE, ask-backed item whose answer
    /// carries `delivered === false` (absent = pre-marker history = no
    /// chip). Pinned against the built SPA so a vocabulary change that
    /// forgets the frontend fails here instead of shipping as drift (the
    /// derive-don't-mirror parity pattern).
    #[test]
    fn awaiting_pickup_chip_condition_is_pinned_in_app_html() {
        let app = include_str!("../../../../static/app.html");
        for marker in [
            "item.status === 'done' && item.ask && item.answer",
            "item.answer.delivered === false",
            "answered · awaiting pickup",
            "agendaChipHtml('answered · awaiting pickup', 'sky'",
        ] {
            assert!(
                app.contains(marker),
                "app.html lost the awaiting-pickup chip marker {marker:?}"
            );
        }
    }

    /// Pins the `--source` envelope field (additive to v1): recorded
    /// verbatim beside the actor, folded into add provenance, absent from
    /// the wire when unset (the existing pin tests prove that half).
    #[test]
    fn source_label_line_format_is_pinned_and_folds_into_provenance() {
        let record = AgendaOpRecord {
            v: 1,
            at_ms: 42,
            actor: None,
            source: Some("deploy-hook".into()),
            op: AgendaOp::Add {
                id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                kind: AgendaKind::Task,
                title: "rotate certs".into(),
                body: String::new(),
                tags: Vec::new(),
                due_ms: None,
                ask: None,
            },
        };
        let line = serde_json::to_string(&record).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"at_ms":42,"source":"deploy-hook","op":{"type":"add","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","kind":"task","title":"rotate certs"}}"#
        );
        let back: AgendaOpRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back, record);

        let mut items = BTreeMap::new();
        assert!(apply_op(&mut items, &record).is_none());
        let provenance = &items["01ARZ3NDEKTSV4RRFFQ69G5FAV"].provenance;
        assert_eq!(provenance.source.as_deref(), Some("deploy-hook"));
        // The label is data beside the attribution, never attribution:
        // principal/session/kind stay exactly what the gate resolved (here,
        // nothing).
        assert_eq!(provenance.principal, None);
        assert_eq!(provenance.session_id, None);
        assert_eq!(provenance.kind, None);
    }

    #[test]
    fn question_command_wire_shapes_parse() {
        let ask: AgendaCommand =
            serde_json::from_str(r#"{"op":"add","kind":"question","title":"deploy now?"}"#)
                .unwrap();
        assert!(matches!(
            ask,
            AgendaCommand::Add {
                kind: AgendaKind::Question,
                ..
            }
        ));
        let answer: AgendaCommand =
            serde_json::from_str(r#"{"op":"answer","id":"01X","text":"yes"}"#).unwrap();
        assert!(matches!(answer, AgendaCommand::Answer { .. }));
    }

    /// F2 fold: annotations thread (any status), blockers with
    /// clear-is-an-op history, edges with add/remove, and full tolerance
    /// for out-of-order or foreign lines.
    #[test]
    fn f2_fold_annotations_blockers_edges() {
        let mut items = BTreeMap::new();
        apply_op(&mut items, &rec(1, add("a", "dependent")));
        apply_op(&mut items, &rec(2, add("b", "prerequisite")));

        // Annotations: attributed thread, allowed on done items too.
        let mut note = rec(
            3,
            AgendaOp::Annotate {
                id: "a".into(),
                text: "evidence: still waiting on the API".into(),
            },
        );
        note.actor = Some(AgendaActor {
            principal: None,
            session_id: Some("sess-1".into()),
            kind: Some("agent_session".into()),
        });
        note.source = Some("housekeeper".into());
        assert!(apply_op(&mut items, &note).is_none());
        apply_op(&mut items, &rec(4, AgendaOp::Complete { id: "b".into() }));
        assert!(apply_op(
            &mut items,
            &rec(
                5,
                AgendaOp::Annotate {
                    id: "b".into(),
                    text: "post-completion note".into()
                }
            )
        )
        .is_none());
        let a = &items["a"];
        assert_eq!(a.annotations.len(), 1);
        assert_eq!(a.annotations[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(a.annotations[0].source.as_deref(), Some("housekeeper"));
        assert_eq!(items["b"].annotations.len(), 1);
        // Unknown item: warn, keep going.
        assert!(apply_op(
            &mut items,
            &rec(
                6,
                AgendaOp::Annotate {
                    id: "nope".into(),
                    text: "x".into()
                }
            )
        )
        .is_some());

        // Blockers: set, duplicate-id warn, clear-is-an-op (entry stays),
        // double-clear warn.
        assert!(apply_op(
            &mut items,
            &rec(
                7,
                AgendaOp::SetBlocker {
                    id: "a".into(),
                    blocker_id: "bk-000000000001".into(),
                    criterion: "gpt-live-1 available on the API".into(),
                }
            )
        )
        .is_none());
        assert!(apply_op(
            &mut items,
            &rec(
                8,
                AgendaOp::SetBlocker {
                    id: "a".into(),
                    blocker_id: "bk-000000000001".into(),
                    criterion: "duplicate".into(),
                }
            )
        )
        .is_some());
        let mut clear = rec(
            9,
            AgendaOp::ClearBlocker {
                id: "a".into(),
                blocker_id: "bk-000000000001".into(),
            },
        );
        clear.actor = Some(AgendaActor {
            principal: Some("principal:root:dashboard".into()),
            session_id: None,
            kind: Some("dashboard".into()),
        });
        assert!(apply_op(&mut items, &clear).is_none());
        let blocker = &items["a"].blockers[0];
        assert_eq!(blocker.criterion, "gpt-live-1 available on the API");
        let cleared = blocker.cleared.as_ref().expect("clear recorded");
        assert_eq!(cleared.kind.as_deref(), Some("dashboard"));
        assert!(apply_op(&mut items, &clear).is_some(), "double clear warns");
        assert_eq!(items["a"].blockers.len(), 1, "history is never deleted");

        // Edges: add, self-edge warn, duplicate warn, remove drops the
        // view (log history is the caller's record).
        assert!(apply_op(
            &mut items,
            &rec(
                10,
                AgendaOp::AddReliesOn {
                    id: "a".into(),
                    target_id: "b".into()
                }
            )
        )
        .is_none());
        assert!(apply_op(
            &mut items,
            &rec(
                11,
                AgendaOp::AddReliesOn {
                    id: "a".into(),
                    target_id: "a".into()
                }
            )
        )
        .is_some());
        assert!(apply_op(
            &mut items,
            &rec(
                12,
                AgendaOp::AddReliesOn {
                    id: "a".into(),
                    target_id: "b".into()
                }
            )
        )
        .is_some());
        assert_eq!(items["a"].relies_on.len(), 1);
        // Dangling target tolerated (foreign/partial log).
        assert!(apply_op(
            &mut items,
            &rec(
                13,
                AgendaOp::AddReliesOn {
                    id: "a".into(),
                    target_id: "01GONE".into()
                }
            )
        )
        .is_none());
        assert!(apply_op(
            &mut items,
            &rec(
                14,
                AgendaOp::RemoveReliesOn {
                    id: "a".into(),
                    target_id: "01GONE".into()
                }
            )
        )
        .is_none());
        assert!(
            apply_op(
                &mut items,
                &rec(
                    15,
                    AgendaOp::RemoveReliesOn {
                        id: "a".into(),
                        target_id: "01GONE".into()
                    }
                )
            )
            .is_some(),
            "removing an absent edge warns"
        );
        assert_eq!(items["a"].relies_on.len(), 1);
        assert_eq!(items["a"].relies_on[0].target_id, "b");
    }

    /// Pins the five F2 durable line formats (additive to v1) and their
    /// round-trips — the migration story is "these exact bytes replay".
    #[test]
    fn f2_op_line_formats_are_pinned() {
        let annotate = AgendaOpRecord {
            v: 1,
            at_ms: 20,
            actor: Some(AgendaActor {
                principal: None,
                session_id: Some("sess-9".into()),
                kind: Some("agent_session".into()),
            }),
            source: None,
            op: AgendaOp::Annotate {
                id: "01X".into(),
                text: "note".into(),
            },
        };
        let line = serde_json::to_string(&annotate).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"at_ms":20,"actor":{"session_id":"sess-9","kind":"agent_session"},"op":{"type":"annotate","id":"01X","text":"note"}}"#
        );
        assert_eq!(
            serde_json::from_str::<AgendaOpRecord>(&line).unwrap(),
            annotate
        );

        for (record, expected) in [
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 21,
                    actor: None,
                    source: Some("deploy-hook".into()),
                    op: AgendaOp::SetBlocker {
                        id: "01X".into(),
                        blocker_id: "bk-0a1b2c3d4e5f".into(),
                        criterion: "api access granted".into(),
                    },
                },
                r#"{"v":1,"at_ms":21,"source":"deploy-hook","op":{"type":"set_blocker","id":"01X","blocker_id":"bk-0a1b2c3d4e5f","criterion":"api access granted"}}"#,
            ),
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 22,
                    actor: None,
                    source: None,
                    op: AgendaOp::ClearBlocker {
                        id: "01X".into(),
                        blocker_id: "bk-0a1b2c3d4e5f".into(),
                    },
                },
                r#"{"v":1,"at_ms":22,"op":{"type":"clear_blocker","id":"01X","blocker_id":"bk-0a1b2c3d4e5f"}}"#,
            ),
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 23,
                    actor: None,
                    source: None,
                    op: AgendaOp::AddReliesOn {
                        id: "01X".into(),
                        target_id: "01Y".into(),
                    },
                },
                r#"{"v":1,"at_ms":23,"op":{"type":"add_relies_on","id":"01X","target_id":"01Y"}}"#,
            ),
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 24,
                    actor: None,
                    source: None,
                    op: AgendaOp::RemoveReliesOn {
                        id: "01X".into(),
                        target_id: "01Y".into(),
                    },
                },
                r#"{"v":1,"at_ms":24,"op":{"type":"remove_relies_on","id":"01X","target_id":"01Y"}}"#,
            ),
        ] {
            let line = serde_json::to_string(&record).unwrap();
            assert_eq!(line, expected);
            assert_eq!(
                serde_json::from_str::<AgendaOpRecord>(&line).unwrap(),
                record
            );
        }
    }

    #[test]
    fn f2_command_wire_shapes_parse() {
        for (json, ok) in [
            (r#"{"op":"annotate","id":"01X","text":"n"}"#, true),
            (
                r#"{"op":"annotate","id":"01X","text":"n","source":"hook"}"#,
                true,
            ),
            (r#"{"op":"set_blocker","id":"01X","criterion":"c"}"#, true),
            (
                r#"{"op":"clear_blocker","id":"01X","blocker_id":"bk-1"}"#,
                true,
            ),
            (
                r#"{"op":"add_relies_on","id":"01X","target_id":"01Y"}"#,
                true,
            ),
            (
                r#"{"op":"remove_relies_on","id":"01X","target_id":"01Y"}"#,
                true,
            ),
            // Clients never mint blocker ids on set.
            (
                r#"{"op":"set_blocker","id":"01X","criterion":"c","blocker_id":"bk-forged"}"#,
                false,
            ),
        ] {
            assert_eq!(
                serde_json::from_str::<AgendaCommand>(json).is_ok(),
                ok,
                "{json}"
            );
        }
    }

    /// The ruled derivation semantics: done satisfies; RETIRED does not
    /// silently satisfy (review marker); missing target marks for review;
    /// cycles derive blocked on every member without any walk; clearing
    /// the last blocker unblocks; everything is pure recomputation.
    #[test]
    fn derived_blocked_retire_review_and_cycle_tolerance() {
        let mut items = BTreeMap::new();
        apply_op(&mut items, &rec(1, add("a", "dependent")));
        apply_op(&mut items, &rec(2, add("b", "prerequisite")));
        apply_op(
            &mut items,
            &rec(
                3,
                AgendaOp::AddReliesOn {
                    id: "a".into(),
                    target_id: "b".into(),
                },
            ),
        );
        assert!(is_blocked(&items, &items["a"]), "open target blocks");
        assert!(!is_blocked(&items, &items["b"]));

        // Completion satisfies by pure recomputation — no event fired.
        apply_op(&mut items, &rec(4, AgendaOp::Complete { id: "b".into() }));
        assert_eq!(
            dependency_state(&items, &items["a"].relies_on[0]),
            (true, None)
        );
        assert!(!is_blocked(&items, &items["a"]));

        // Retire does NOT silently satisfy: review marker + blocked again.
        apply_op(&mut items, &rec(5, AgendaOp::Reopen { id: "b".into() }));
        apply_op(&mut items, &rec(6, AgendaOp::Retire { id: "b".into() }));
        assert_eq!(
            dependency_state(&items, &items["a"].relies_on[0]),
            (false, Some("target_retired"))
        );
        assert!(is_blocked(&items, &items["a"]));

        // Dangling edge (foreign/partial log): review, blocked, no error.
        apply_op(
            &mut items,
            &rec(
                7,
                AgendaOp::AddReliesOn {
                    id: "b".into(),
                    target_id: "01GONE".into(),
                },
            ),
        );
        apply_op(&mut items, &rec(8, AgendaOp::Reopen { id: "b".into() }));
        assert_eq!(
            dependency_state(&items, &items["b"].relies_on[0]),
            (false, Some("target_missing"))
        );

        // Cycle: c→d, d→c — both derive blocked, nothing recurses.
        apply_op(&mut items, &rec(9, add("c", "one")));
        apply_op(&mut items, &rec(10, add("d", "other")));
        apply_op(
            &mut items,
            &rec(
                11,
                AgendaOp::AddReliesOn {
                    id: "c".into(),
                    target_id: "d".into(),
                },
            ),
        );
        apply_op(
            &mut items,
            &rec(
                12,
                AgendaOp::AddReliesOn {
                    id: "d".into(),
                    target_id: "c".into(),
                },
            ),
        );
        assert!(is_blocked(&items, &items["c"]));
        assert!(is_blocked(&items, &items["d"]));

        // Blockers: uncleared blocks, clearing the last one unblocks.
        apply_op(&mut items, &rec(13, add("e", "solo")));
        apply_op(
            &mut items,
            &rec(
                14,
                AgendaOp::SetBlocker {
                    id: "e".into(),
                    blocker_id: "bk-1".into(),
                    criterion: "wait".into(),
                },
            ),
        );
        assert!(is_blocked(&items, &items["e"]));
        apply_op(
            &mut items,
            &rec(
                15,
                AgendaOp::ClearBlocker {
                    id: "e".into(),
                    blocker_id: "bk-1".into(),
                },
            ),
        );
        assert!(!is_blocked(&items, &items["e"]));
        // A done item is never "blocked" regardless of blockers.
        apply_op(
            &mut items,
            &rec(
                16,
                AgendaOp::SetBlocker {
                    id: "e".into(),
                    blocker_id: "bk-2".into(),
                    criterion: "wait more".into(),
                },
            ),
        );
        apply_op(&mut items, &rec(17, AgendaOp::Complete { id: "e".into() }));
        assert!(!is_blocked(&items, &items["e"]));
    }

    #[test]
    fn counts_by_status() {
        let mut items = BTreeMap::new();
        apply_op(&mut items, &rec(1, add("a", "a")));
        apply_op(&mut items, &rec(2, add("b", "b")));
        apply_op(&mut items, &rec(3, add("c", "c")));
        apply_op(&mut items, &rec(4, AgendaOp::Complete { id: "b".into() }));
        apply_op(&mut items, &rec(5, AgendaOp::Retire { id: "c".into() }));
        assert_eq!(
            counts(&items),
            AgendaCounts {
                open: 1,
                done: 1,
                retired: 1
            }
        );
    }

    fn rich_ask() -> AgendaAsk {
        AgendaAsk {
            ask_id: 17_592_186_044_423, // (1 << 44) + 7
            questions: vec![crate::types::UserQuestion {
                question: "Which grid?".into(),
                header: "Grid".into(),
                options: vec![crate::types::UserQuestionOption {
                    label: "A".into(),
                    description: String::new(),
                }],
                multi_select: false,
                pick_min: Some(1),
                pick_max: Some(1),
                free_text: None,
                previews: vec![crate::types::QuestionPreview {
                    label: "A".into(),
                    source: crate::types::QuestionPreviewSource::Html {
                        upload_id: "blob-1".into(),
                        url: "/api/agenda/blobs/01ITEM/blob-1/raw".into(),
                    },
                }],
            }],
        }
    }

    fn ask_add(id: &str) -> AgendaOp {
        AgendaOp::Add {
            id: id.to_string(),
            kind: AgendaKind::Question,
            title: "Which grid?".into(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            ask: Some(rich_ask()),
        }
    }

    /// Slice 1: an ask-add folds the payload onto the item; a structured
    /// answer resolves it and records the breakdown; dismissal marks but
    /// never transitions; reopen clears both views (log keeps history).
    #[test]
    fn ask_fold_lifecycle_answer_dismiss_reopen() {
        let mut items = BTreeMap::new();
        assert!(apply_op(&mut items, &rec(1, ask_add("q"))).is_none());
        let item = &items["q"];
        assert_eq!(item.kind, AgendaKind::Question);
        assert_eq!(item.ask.as_ref().unwrap().ask_id, rich_ask().ask_id);
        assert_eq!(item.ask.as_ref().unwrap().questions.len(), 1);

        // Dismissal (rail skip): marker recorded, item stays OPEN.
        assert!(apply_op(
            &mut items,
            &rec(
                2,
                AgendaOp::Dismiss {
                    id: "q".into(),
                    action: "skip".into()
                }
            )
        )
        .is_none());
        assert_eq!(items["q"].status, AgendaStatus::Open);
        assert_eq!(items["q"].dismissed.as_ref().unwrap().action, "skip");
        assert_eq!(items["q"].dismissed.as_ref().unwrap().at_ms, 2);

        // A structured answer resolves it and clears the dismissal view.
        let structured = AgendaAskResolution {
            answers: BTreeMap::from([("Which grid?".to_string(), "A".to_string())]),
            selections: BTreeMap::from([("Which grid?".to_string(), vec!["A".to_string()])]),
            followups: BTreeMap::from([(
                "Which grid?".to_string(),
                "can B keep the sidebar?".to_string(),
            )]),
            annotations: BTreeMap::from([(
                "Which grid?".to_string(),
                vec![crate::types::QuestionAnnotation {
                    preview: "A".into(),
                    note: "rails too faint".into(),
                }],
            )]),
        };
        assert!(apply_op(
            &mut items,
            &rec(
                3,
                AgendaOp::Answer {
                    id: "q".into(),
                    text: "A".into(),
                    structured: Some(structured.clone()),
                }
            )
        )
        .is_none());
        let answered = &items["q"];
        assert_eq!(answered.status, AgendaStatus::Done);
        assert!(answered.dismissed.is_none());
        let reply = answered.answer.as_ref().unwrap();
        assert_eq!(reply.text, "A");
        assert_eq!(reply.structured.as_ref().unwrap(), &structured);
        // The archive keeps the ask payload (previews stay visible).
        assert!(answered.ask.is_some());

        // Dismiss on a resolved question warns and changes nothing.
        assert!(apply_op(
            &mut items,
            &rec(
                4,
                AgendaOp::Dismiss {
                    id: "q".into(),
                    action: "deny".into()
                }
            )
        )
        .is_some());

        // Reopen re-asks: answer + dismissal views clear, ask stays.
        apply_op(&mut items, &rec(5, AgendaOp::Reopen { id: "q".into() }));
        let reopened = &items["q"];
        assert_eq!(reopened.status, AgendaStatus::Open);
        assert!(reopened.answer.is_none());
        assert!(reopened.dismissed.is_none());
        assert!(reopened.ask.is_some());

        // Dismiss never lands on non-questions.
        apply_op(&mut items, &rec(6, add("t", "task")));
        assert!(apply_op(
            &mut items,
            &rec(
                7,
                AgendaOp::Dismiss {
                    id: "t".into(),
                    action: "skip".into()
                }
            )
        )
        .is_some());
        assert!(items["t"].dismissed.is_none());
    }

    /// Pins the ask-add durable line (additive to v1) and its round-trip.
    #[test]
    fn ask_add_record_line_format_is_pinned() {
        let record = AgendaOpRecord {
            v: 1,
            at_ms: 11,
            actor: Some(AgendaActor {
                principal: None,
                session_id: Some("sess-park".into()),
                kind: Some("agent_session".into()),
            }),
            source: None,
            op: ask_add("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        };
        let line = serde_json::to_string(&record).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"at_ms":11,"actor":{"session_id":"sess-park","kind":"agent_session"},"op":{"type":"add","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","kind":"question","title":"Which grid?","ask":{"ask_id":17592186044423,"questions":[{"question":"Which grid?","header":"Grid","options":[{"label":"A"}],"multi_select":false,"pick_min":1,"pick_max":1,"previews":[{"label":"A","kind":"html","upload_id":"blob-1","url":"/api/agenda/blobs/01ITEM/blob-1/raw"}]}]}}}"#
        );
        let back: AgendaOpRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back, record);
    }

    /// Pins the dismiss line and the structured-answer line (additive).
    #[test]
    fn dismiss_and_structured_answer_line_formats_are_pinned() {
        let dismiss = AgendaOpRecord {
            v: 1,
            at_ms: 12,
            actor: None,
            source: None,
            op: AgendaOp::Dismiss {
                id: "01X".into(),
                action: "skip".into(),
            },
        };
        let line = serde_json::to_string(&dismiss).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"at_ms":12,"op":{"type":"dismiss","id":"01X","action":"skip"}}"#
        );
        assert_eq!(
            serde_json::from_str::<AgendaOpRecord>(&line).unwrap(),
            dismiss
        );

        let answer = AgendaOpRecord {
            v: 1,
            at_ms: 13,
            actor: None,
            source: None,
            op: AgendaOp::Answer {
                id: "01X".into(),
                text: "A".into(),
                structured: Some(AgendaAskResolution {
                    answers: BTreeMap::from([("Q?".to_string(), "A".to_string())]),
                    selections: BTreeMap::from([("Q?".to_string(), vec!["A".to_string()])]),
                    followups: BTreeMap::new(),
                    annotations: BTreeMap::new(),
                }),
            },
        };
        let line = serde_json::to_string(&answer).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"at_ms":13,"op":{"type":"answer","id":"01X","text":"A","structured":{"answers":{"Q?":"A"},"selections":{"Q?":["A"]}}}}"#
        );
        assert_eq!(
            serde_json::from_str::<AgendaOpRecord>(&line).unwrap(),
            answer
        );
    }

    /// Forward compatibility both ways: an old add line (no `ask`) folds
    /// into an ask-less item; an old item JSON (no `ask`/`dismissed`)
    /// deserializes; and a new item round-trips through JSON intact.
    #[test]
    fn ask_fields_are_additive_on_wire_and_log() {
        // Old log line, current build.
        let old_line =
            r#"{"v":1,"at_ms":1,"op":{"type":"add","id":"q","kind":"question","title":"old?"}}"#;
        let record: AgendaOpRecord = serde_json::from_str(old_line).unwrap();
        let mut items = BTreeMap::new();
        assert!(apply_op(&mut items, &record).is_none());
        assert!(items["q"].ask.is_none());
        assert!(items["q"].dismissed.is_none());

        // Old item DTO, current build.
        let old_item = r#"{"id":"q","kind":"question","title":"old?","body":"","tags":[],"provenance":{"created_ms":1},"status":"open","updated_ms":1}"#;
        let item: AgendaItem = serde_json::from_str(old_item).unwrap();
        assert!(item.ask.is_none());

        // New item round-trip.
        apply_op(&mut items, &rec(2, ask_add("rich")));
        let json = serde_json::to_string(&items["rich"]).unwrap();
        let back: AgendaItem = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, &items["rich"]);
        // Plain (non-ask) items serialize without the new keys at all.
        let plain = serde_json::to_string(&items["q"]).unwrap();
        assert!(!plain.contains("\"ask\""));
        assert!(!plain.contains("\"dismissed\""));
    }

    /// The park command's wire shape parses, and old daemons' strictness
    /// story holds: `deny_unknown_fields` still rejects unknown command
    /// fields while the new optional `structured` on answer is accepted.
    #[test]
    fn ask_command_wire_shape_parses() {
        let cmd: AgendaCommand = serde_json::from_str(
            r#"{"op":"ask","questions":[{"question":"Which grid?","options":[{"label":"A"},{"label":"B"}],"pick_min":1,"pick_max":1}]}"#,
        )
        .unwrap();
        match cmd {
            AgendaCommand::Ask { questions } => {
                assert_eq!(questions.len(), 1);
                assert_eq!(questions[0].question, "Which grid?");
                assert_eq!(questions[0].options.len(), 2);
            }
            other => panic!("unexpected {other:?}"),
        }
        let answer: AgendaCommand = serde_json::from_str(
            r#"{"op":"answer","id":"01X","text":"A","structured":{"answers":{"Q?":"A"}}}"#,
        )
        .unwrap();
        match answer {
            AgendaCommand::Answer { structured, .. } => {
                assert_eq!(
                    structured.unwrap().answers.get("Q?").map(String::as_str),
                    Some("A")
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            serde_json::from_str::<AgendaCommand>(r#"{"op":"ask","questions":[],"wait":30}"#)
                .is_err(),
            "unknown command fields stay rejected at intake"
        );
    }

    /// G1 fold: refs attach with envelope attribution, duplicates and
    /// unknown items warn-ignore, removal drops the view ref only.
    #[test]
    fn g1_fold_refs_add_remove_and_tolerance() {
        let mut items = BTreeMap::new();
        apply_op(
            &mut items,
            &rec(
                1,
                AgendaOp::Add {
                    id: "01X".into(),
                    kind: AgendaKind::Task,
                    title: "host".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    ask: None,
                },
            ),
        );
        let add_ref = AgendaOpRecord {
            v: 1,
            at_ms: 2,
            actor: Some(AgendaActor {
                principal: None,
                session_id: Some("sess-7".into()),
                kind: Some("agent_session".into()),
            }),
            source: Some("track-g".into()),
            op: AgendaOp::AddRef {
                id: "01X".into(),
                ref_type: AgendaRefType::File,
                locator: "/work/brief.md".into(),
                digest: Some("ab".repeat(32)),
                must_read: true,
                label: Some("brief".into()),
            },
        };
        assert!(apply_op(&mut items, &add_ref).is_none());
        let item = &items["01X"];
        assert_eq!(item.refs.len(), 1);
        assert_eq!(item.refs[0].session_id.as_deref(), Some("sess-7"));
        assert_eq!(item.refs[0].source.as_deref(), Some("track-g"));
        assert!(item.refs[0].must_read);
        assert_eq!(item.updated_ms, 2);

        // Duplicate address warns and keeps the first; a different type
        // with the same locator is a different address.
        assert!(apply_op(&mut items, &add_ref).is_some());
        assert!(apply_op(
            &mut items,
            &rec(
                3,
                AgendaOp::AddRef {
                    id: "01X".into(),
                    ref_type: AgendaRefType::Url,
                    locator: "/work/brief.md".into(),
                    digest: None,
                    must_read: false,
                    label: None,
                },
            ),
        )
        .is_none());
        assert_eq!(items["01X"].refs.len(), 2);

        // Unknown item and absent ref warn-ignore (foreign log tolerance).
        assert!(apply_op(
            &mut items,
            &rec(
                4,
                AgendaOp::AddRef {
                    id: "01GONE".into(),
                    ref_type: AgendaRefType::Memory,
                    locator: "mem-1".into(),
                    digest: None,
                    must_read: false,
                    label: None,
                },
            ),
        )
        .is_some());
        assert!(apply_op(
            &mut items,
            &rec(
                5,
                AgendaOp::RemoveRef {
                    id: "01X".into(),
                    ref_type: AgendaRefType::Memory,
                    locator: "never-attached".into(),
                },
            ),
        )
        .is_some());

        // Removal drops exactly the addressed ref from the view.
        assert!(apply_op(
            &mut items,
            &rec(
                6,
                AgendaOp::RemoveRef {
                    id: "01X".into(),
                    ref_type: AgendaRefType::File,
                    locator: "/work/brief.md".into(),
                },
            ),
        )
        .is_none());
        let item = &items["01X"];
        assert_eq!(item.refs.len(), 1);
        assert_eq!(item.refs[0].ref_type, AgendaRefType::Url);
    }

    /// G1 op-line bytes are pinned (the F2 pin's sibling): what these ops
    /// serialize to is what every future build must keep folding.
    #[test]
    fn g1_op_line_formats_are_pinned() {
        for (record, expected) in [
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 30,
                    actor: None,
                    source: None,
                    op: AgendaOp::AddRef {
                        id: "01X".into(),
                        ref_type: AgendaRefType::File,
                        locator: "/work/brief.md".into(),
                        digest: Some("0a1b".into()),
                        must_read: true,
                        label: Some("brief".into()),
                    },
                },
                r#"{"v":1,"at_ms":30,"op":{"type":"add_ref","id":"01X","ref_type":"file","locator":"/work/brief.md","digest":"0a1b","must_read":true,"label":"brief"}}"#,
            ),
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 31,
                    actor: None,
                    source: None,
                    op: AgendaOp::AddRef {
                        id: "01X".into(),
                        ref_type: AgendaRefType::Url,
                        locator: "https://example.com/pr/7".into(),
                        digest: None,
                        must_read: false,
                        label: None,
                    },
                },
                r#"{"v":1,"at_ms":31,"op":{"type":"add_ref","id":"01X","ref_type":"url","locator":"https://example.com/pr/7"}}"#,
            ),
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 32,
                    actor: None,
                    source: None,
                    op: AgendaOp::RemoveRef {
                        id: "01X".into(),
                        ref_type: AgendaRefType::Session,
                        locator: "sess-1".into(),
                    },
                },
                r#"{"v":1,"at_ms":32,"op":{"type":"remove_ref","id":"01X","ref_type":"session","locator":"sess-1"}}"#,
            ),
        ] {
            let line = serde_json::to_string(&record).unwrap();
            assert_eq!(line, expected);
            assert_eq!(
                serde_json::from_str::<AgendaOpRecord>(&line).unwrap(),
                record
            );
        }
    }

    /// G1 wire commands parse; clients never mint digests; pre-G1 item
    /// JSON (no `refs`) still deserializes (additive DTO).
    #[test]
    fn g1_command_wire_shapes_parse() {
        for (json, ok) in [
            (
                r#"{"op":"add_ref","id":"01X","ref_type":"url","locator":"https://x.dev/pr/1"}"#,
                true,
            ),
            (
                r#"{"op":"add_ref","id":"01X","ref_type":"file","locator":"/a/b.md","must_read":true,"label":"brief","source":"hook"}"#,
                true,
            ),
            (
                r#"{"op":"remove_ref","id":"01X","ref_type":"file","locator":"/a/b.md"}"#,
                true,
            ),
            (
                r#"{"op":"add","kind":"task","title":"t","refs":[{"ref_type":"url","locator":"https://x.dev","must_read":true}]}"#,
                true,
            ),
            // Clients never mint digests — on the command or the spec.
            (
                r#"{"op":"add_ref","id":"01X","ref_type":"file","locator":"/a/b.md","digest":"forged"}"#,
                false,
            ),
            (
                r#"{"op":"add","kind":"task","title":"t","refs":[{"ref_type":"file","locator":"/a","digest":"forged"}]}"#,
                false,
            ),
            // Unknown ref types are rejected at intake (and degrade to a
            // preserved-skipped line when folded from a foreign log).
            (
                r#"{"op":"add_ref","id":"01X","ref_type":"sigil","locator":"x"}"#,
                false,
            ),
        ] {
            assert_eq!(
                serde_json::from_str::<AgendaCommand>(json).is_ok(),
                ok,
                "{json}"
            );
        }
        let legacy_item = r#"{"id":"01X","kind":"task","title":"t","body":"","tags":[],"provenance":{"created_ms":1},"status":"open","updated_ms":1}"#;
        assert!(serde_json::from_str::<AgendaItem>(legacy_item)
            .unwrap()
            .refs
            .is_empty());
    }

    fn add_item(items: &mut BTreeMap<String, AgendaItem>, at: u64, id: &str) {
        apply_op(
            items,
            &rec(
                at,
                AgendaOp::Add {
                    id: id.into(),
                    kind: AgendaKind::Task,
                    title: format!("item {id}"),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    ask: None,
                },
            ),
        );
    }

    /// G2 fold: single live parent (second add warn-ignores), re-parent
    /// as the remove+add pair, adjacency add/remove with tolerance.
    #[test]
    fn g2_fold_placement_and_relations() {
        let mut items = BTreeMap::new();
        for id in ["01HUB", "01HUB2", "01CHILD", "01PEER"] {
            add_item(&mut items, 1, id);
        }
        assert!(apply_op(
            &mut items,
            &rec(
                2,
                AgendaOp::AddPartOf {
                    id: "01CHILD".into(),
                    parent_id: "01HUB".into(),
                },
            ),
        )
        .is_none());
        let placed = &items["01CHILD"].part_of;
        assert_eq!(placed.as_ref().unwrap().parent_id, "01HUB");

        // Single live parent: a second add is fold-ignored with a warning.
        assert!(apply_op(
            &mut items,
            &rec(
                3,
                AgendaOp::AddPartOf {
                    id: "01CHILD".into(),
                    parent_id: "01HUB2".into(),
                },
            ),
        )
        .is_some());
        assert_eq!(
            items["01CHILD"].part_of.as_ref().unwrap().parent_id,
            "01HUB"
        );

        // Re-parent = remove + add (the primitive pair Place emits).
        assert!(apply_op(
            &mut items,
            &rec(
                4,
                AgendaOp::RemovePartOf {
                    id: "01CHILD".into(),
                    parent_id: "01HUB".into(),
                },
            ),
        )
        .is_none());
        assert!(items["01CHILD"].part_of.is_none());
        assert!(apply_op(
            &mut items,
            &rec(
                5,
                AgendaOp::AddPartOf {
                    id: "01CHILD".into(),
                    parent_id: "01HUB2".into(),
                },
            ),
        )
        .is_none());
        assert_eq!(
            items["01CHILD"].part_of.as_ref().unwrap().parent_id,
            "01HUB2"
        );
        // Mismatched remove warns and changes nothing.
        assert!(apply_op(
            &mut items,
            &rec(
                6,
                AgendaOp::RemovePartOf {
                    id: "01CHILD".into(),
                    parent_id: "01HUB".into(),
                },
            ),
        )
        .is_some());
        assert!(items["01CHILD"].part_of.is_some());

        // Adjacency: add, duplicate warn, self warn, remove.
        assert!(apply_op(
            &mut items,
            &rec(
                7,
                AgendaOp::AddRelatesTo {
                    id: "01CHILD".into(),
                    target_id: "01PEER".into(),
                },
            ),
        )
        .is_none());
        assert!(apply_op(
            &mut items,
            &rec(
                8,
                AgendaOp::AddRelatesTo {
                    id: "01CHILD".into(),
                    target_id: "01PEER".into(),
                },
            ),
        )
        .is_some());
        assert!(apply_op(
            &mut items,
            &rec(
                9,
                AgendaOp::AddRelatesTo {
                    id: "01PEER".into(),
                    target_id: "01PEER".into(),
                },
            ),
        )
        .is_some());
        assert_eq!(items["01CHILD"].relates_to.len(), 1);
        assert!(apply_op(
            &mut items,
            &rec(
                10,
                AgendaOp::RemoveRelatesTo {
                    id: "01CHILD".into(),
                    target_id: "01PEER".into(),
                },
            ),
        )
        .is_none());
        assert!(items["01CHILD"].relates_to.is_empty());
    }

    /// The ruled no-transitive pin: placement NEVER propagates blocking —
    /// a blocked child renders its hub unblocked — and a hub may complete
    /// over open children (render-level flag only, no cascade).
    #[test]
    fn g2_no_transitive_semantics() {
        let mut items = BTreeMap::new();
        for id in ["01HUB", "01CHILD"] {
            add_item(&mut items, 1, id);
        }
        apply_op(
            &mut items,
            &rec(
                2,
                AgendaOp::AddPartOf {
                    id: "01CHILD".into(),
                    parent_id: "01HUB".into(),
                },
            ),
        );
        apply_op(
            &mut items,
            &rec(
                3,
                AgendaOp::SetBlocker {
                    id: "01CHILD".into(),
                    blocker_id: "bk-child".into(),
                    criterion: "vendor access".into(),
                },
            ),
        );
        assert!(is_blocked(&items, &items["01CHILD"]));
        assert!(
            !is_blocked(&items, &items["01HUB"]),
            "a blocked child must never render its hub blocked (part_of \
             propagates nothing)"
        );

        // Hub completes while the child stays open: no cascade either way.
        apply_op(
            &mut items,
            &rec(4, AgendaOp::Complete { id: "01HUB".into() }),
        );
        assert_eq!(items["01HUB"].status, AgendaStatus::Done);
        assert_eq!(items["01CHILD"].status, AgendaStatus::Open);
    }

    /// G2 op-line bytes are pinned.
    #[test]
    fn g2_op_line_formats_are_pinned() {
        for (record, expected) in [
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 40,
                    actor: None,
                    source: None,
                    op: AgendaOp::AddPartOf {
                        id: "01X".into(),
                        parent_id: "01H".into(),
                    },
                },
                r#"{"v":1,"at_ms":40,"op":{"type":"add_part_of","id":"01X","parent_id":"01H"}}"#,
            ),
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 41,
                    actor: None,
                    source: None,
                    op: AgendaOp::RemovePartOf {
                        id: "01X".into(),
                        parent_id: "01H".into(),
                    },
                },
                r#"{"v":1,"at_ms":41,"op":{"type":"remove_part_of","id":"01X","parent_id":"01H"}}"#,
            ),
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 42,
                    actor: None,
                    source: None,
                    op: AgendaOp::AddRelatesTo {
                        id: "01X".into(),
                        target_id: "01Y".into(),
                    },
                },
                r#"{"v":1,"at_ms":42,"op":{"type":"add_relates_to","id":"01X","target_id":"01Y"}}"#,
            ),
            (
                AgendaOpRecord {
                    v: 1,
                    at_ms: 43,
                    actor: None,
                    source: None,
                    op: AgendaOp::RemoveRelatesTo {
                        id: "01X".into(),
                        target_id: "01Y".into(),
                    },
                },
                r#"{"v":1,"at_ms":43,"op":{"type":"remove_relates_to","id":"01X","target_id":"01Y"}}"#,
            ),
        ] {
            let line = serde_json::to_string(&record).unwrap();
            assert_eq!(line, expected);
            assert_eq!(
                serde_json::from_str::<AgendaOpRecord>(&line).unwrap(),
                record
            );
        }
    }

    /// G3-pre op-line bytes are pinned: the recurrence-bearing manifest
    /// as `propose_effect` carries it, and the `request_occurrence` line
    /// whose `at_ms` replay reads (never the clock).
    #[test]
    fn g3pre_op_line_formats_are_pinned() {
        let manifest = SessionManifest {
            goal: "standing".into(),
            fire_at_ms: 1000,
            orchestrate: false,
            interactive: false,
            project_root: None,
            agent_config: None,
            recurrence: Some(RecurrenceSpec {
                every_ms: 3_600_000,
                until_ms: None,
                max_occurrences: Some(4),
                suspend_after_failures: None,
            }),
        };
        let propose = AgendaOpRecord {
            v: 1,
            at_ms: 50,
            actor: None,
            source: None,
            op: AgendaOp::ProposeEffect {
                id: "01X".into(),
                effect_id: "ef-1".into(),
                manifest,
            },
        };
        let line = serde_json::to_string(&propose).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"at_ms":50,"op":{"type":"propose_effect","id":"01X","effect_id":"ef-1","manifest":{"goal":"standing","fire_at_ms":1000,"recurrence":{"every_ms":3600000,"max_occurrences":4}}}}"#
        );
        assert_eq!(
            serde_json::from_str::<AgendaOpRecord>(&line).unwrap(),
            propose
        );

        let request = AgendaOpRecord {
            v: 1,
            at_ms: 51,
            actor: Some(AgendaActor {
                principal: Some("owner".into()),
                session_id: None,
                kind: Some("dashboard".into()),
            }),
            source: None,
            op: AgendaOp::RequestOccurrence {
                id: "01X".into(),
                effect_id: "ef-1".into(),
                digest: "0a1b".into(),
                at_ms: 999,
            },
        };
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"at_ms":51,"actor":{"principal":"owner","kind":"dashboard"},"op":{"type":"request_occurrence","id":"01X","effect_id":"ef-1","digest":"0a1b","at_ms":999}}"#
        );
        assert_eq!(
            serde_json::from_str::<AgendaOpRecord>(&line).unwrap(),
            request
        );

        // A stale request against a revised manifest is fold-skipped.
        let mut items = BTreeMap::new();
        add_item(&mut items, 1, "01X");
        apply_op(
            &mut items,
            &rec(
                2,
                AgendaOp::ProposeEffect {
                    id: "01X".into(),
                    effect_id: "ef-1".into(),
                    manifest: SessionManifest {
                        goal: "v2".into(),
                        fire_at_ms: 2000,
                        orchestrate: false,
                        interactive: false,
                        project_root: None,
                        agent_config: None,
                        recurrence: None,
                    },
                },
            ),
        );
        assert!(apply_op(
            &mut items,
            &rec(
                3,
                AgendaOp::RequestOccurrence {
                    id: "01X".into(),
                    effect_id: "ef-1".into(),
                    digest: "stale".into(),
                    at_ms: 3,
                },
            ),
        )
        .is_some());
        assert!(items["01X"].effects[0].requested.is_empty());
    }

    /// G2 wire commands parse (Place included); unknown fields rejected.
    #[test]
    fn g2_command_wire_shapes_parse() {
        for (json, ok) in [
            (r#"{"op":"add_part_of","id":"01X","parent_id":"01H"}"#, true),
            (
                r#"{"op":"remove_part_of","id":"01X","parent_id":"01H"}"#,
                true,
            ),
            (r#"{"op":"place","id":"01X","under":"01H"}"#, true),
            (
                r#"{"op":"place","id":"01X","under":"01H","source":"hook"}"#,
                true,
            ),
            (
                r#"{"op":"add_relates_to","id":"01X","target_id":"01Y"}"#,
                true,
            ),
            (
                r#"{"op":"remove_relates_to","id":"01X","target_id":"01Y"}"#,
                true,
            ),
            (
                r#"{"op":"place","id":"01X","under":"01H","force":true}"#,
                false,
            ),
        ] {
            assert_eq!(
                serde_json::from_str::<AgendaCommand>(json).is_ok(),
                ok,
                "{json}"
            );
        }
    }
}

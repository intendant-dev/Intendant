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
    /// without an owner-surface approval of the exact digest.
    ProposeEffect {
        id: String,
        goal: String,
        fire_at_ms: u64,
        #[serde(default)]
        orchestrate: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
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
    StartNow { id: String },
}

impl AgendaItem {
    /// Every session id this item's attribution views reference (birth
    /// provenance, answer, effect proposals and runs) — the set a display
    /// surface resolves to conversations and names. Deduplication is the
    /// caller's concern.
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
            | AgendaCommand::RemoveReliesOn { source, .. } => source.take(),
            AgendaCommand::Ask { .. }
            | AgendaCommand::ApproveEffect { .. }
            | AgendaCommand::RevokeEffect { .. }
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
            | AgendaOp::ProposeEffect { id, .. }
            | AgendaOp::ApproveEffect { id, .. }
            | AgendaOp::RevokeEffect { id, .. }
            | AgendaOp::RecordOccurrence { id, .. } => id,
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
                // approved different bytes.
                approval: None,
                last_run: None,
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
}

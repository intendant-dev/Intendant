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
    /// Reply to an open question (question items only). Resolves it.
    /// `structured` (optional, additive) carries the rich-ask breakdown —
    /// selections, follow-ups, anchored preview notes — recorded alongside
    /// the joined `text` summary.
    Answer {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured: Option<AgendaAskResolution>,
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
    },
    /// Approve the current manifest revision by its digest. **An
    /// owner-surface act** — the tenant edge refuses agent-session, peer,
    /// and unattributed actors with a named denial.
    ApproveEffect {
        id: String,
        digest: String,
    },
    /// Withdraw the approval (owner-surface, like granting it).
    RevokeEffect {
        id: String,
    },
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
                        created_ms: at_ms,
                    },
                    status: AgendaStatus::Open,
                    updated_ms: at_ms,
                    completed_ms: None,
                    answer: None,
                    effects: Vec::new(),
                    ask: ask.clone(),
                    dismissed: None,
                },
            );
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

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
    pub(crate) text: String,
    pub(crate) at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
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
    /// Display-only due instant (ms since epoch). Presentation state: it is
    /// patchable and fires nothing. A time that *delivers* arrives in a
    /// later slice as a separate approved effect, never this field.
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
        /// Display-only due instant (ms since epoch); fires nothing.
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
    Answer {
        id: String,
        text: String,
    },
}

/// A durable op — the payload of one log line. Compatible with the
/// umbrella RFC §7.2 vocabulary (`Answer` rides the `question` kind per
/// §7.1); effect/occurrence/journal operations are reserved there and
/// intentionally absent in v1.
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
            | AgendaOp::Answer { id, .. } => id,
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
                    // replies remain in the log as history.
                    item.answer = None;
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
        AgendaOp::Answer { id, text } => {
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
                    });
                    // A reply resolves the question.
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
                },
            ),
        );
        let mut answer = rec(
            2,
            AgendaOp::Answer {
                id: "q".into(),
                text: "sqlite is fine".into(),
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
                    text: "no, postgres".into()
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
                    text: "nope".into()
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
}

//! The Memory service: propose / search / read over a local plane,
//! with reducer-derived status and effective durability on every
//! view.

use std::collections::BTreeMap;

#[cfg(test)]
use owner_plane_core::cbor;
use owner_plane_core::shapes::envelope::ActorKind;
use owner_plane_core::shapes::memory::{BasicVerdict, Mclaim, Mjudge};
use owner_plane_core::shapes::{Class, Kind, ToValue, Verb};

use crate::access::actor::{ActorBinding, ActorKind as GateActorKind};

use super::plane::EphemeralPlane;
use super::types::{
    hex32, ClaimProvenance, ClaimView, JudgeArgs, JudgmentView, MemoryError, ProposeArgs,
    SearchArgs, MAX_REASON_CHARS,
};

/// Search results are hard-capped regardless of the caller's ask
/// (§6.5: bounded retrieval — no whole-store reads through this API).
const SEARCH_LIMIT_CEILING: usize = 50;

/// What the service records about a claim it minted: the plaintext it
/// authored (the mint side holds the plaintext; ops carry it in the
/// signed body) keyed by the accepted op hash. Status is NEVER stored
/// here — it is derived from the fold at read time.
struct ClaimRecord {
    kind: Kind,
    statement: String,
    sensitivity: Class,
    session: Option<String>,
    project: Option<String>,
    model: Option<String>,
    labels: Vec<String>,
    created_ms: u64,
    proposed_by: ClaimProvenance,
}

fn parse_vocab<T: Copy>(
    what: &'static str,
    got: &str,
    all: &'static [T],
    as_str: fn(T) -> &'static str,
) -> Result<T, MemoryError> {
    all.iter()
        .copied()
        .find(|v| as_str(*v) == got)
        .ok_or_else(|| MemoryError::Vocabulary {
            what,
            got: got.to_string(),
            allowed: all
                .iter()
                .map(|v| as_str(*v))
                .collect::<Vec<_>>()
                .join(", "),
        })
}

/// Does `p` — validated lowercase hex — prefix-match `key`'s 64-char
/// lowercase hex form? Compared nibble-wise, so the hex string is
/// never materialized per candidate.
fn hex_prefix_matches(key: &[u8; 32], p: &str) -> bool {
    if p.len() > 64 {
        return false;
    }
    p.bytes().enumerate().all(|(i, c)| {
        let nibble = if i % 2 == 0 {
            key[i / 2] >> 4
        } else {
            key[i / 2] & 0x0f
        };
        c == b"0123456789abcdef"[nibble as usize]
    })
}

/// What the service records about a judgment it minted or recovered,
/// keyed alongside claims by the accepted op hash. Whether a judgment
/// COUNTS is never stored — the claim's derived status is the fold's
/// answer; the record exists so history renders every §11.2-recorded
/// judgment ("recorded and surfaced", counting or not).
struct JudgmentRecord {
    id: [u8; 32],
    verdict: String,
    target: [u8; 32],
    replacement: Option<[u8; 32]>,
    reason: Option<String>,
    at_ms: u64,
    judged_by: ClaimProvenance,
    policy: String,
}

pub(crate) struct MemoryService {
    plane: EphemeralPlane,
    claims: BTreeMap<[u8; 32], ClaimRecord>,
    /// Judgment history in append order (the plane's item order —
    /// rebuilt in the same order on reopen, so live and recovered
    /// views agree; ruling R2).
    judgments: Vec<JudgmentRecord>,
    /// P1.8: the durable custody store, when this daemon runs the
    /// durable plane (macOS — multi-platform custody stays full Gate
    /// B, so other OSes run ephemeral and say so). `None` = ephemeral.
    store: Option<super::store::DurableStore>,
}

impl MemoryService {
    /// Bootstrap an in-memory plane and an empty claim registry.
    pub(crate) fn new() -> Result<MemoryService, MemoryError> {
        Ok(MemoryService {
            plane: EphemeralPlane::bootstrap()?,
            claims: BTreeMap::new(),
            judgments: Vec::new(),
            store: None,
        })
    }

    /// P1.8: open (or create) the DURABLE plane at `dir`. Reopen
    /// recovers the op set through the stamped walker + fold and
    /// rebuilds the claim registry from the recovered op bodies.
    pub(crate) fn new_durable(
        dir: &std::path::Path,
    ) -> Result<MemoryService, super::store::StoreError> {
        use super::store::{DurableStore, RecoveredStore};
        if dir.join("custody.v1.json").exists() {
            let RecoveredStore {
                store,
                resume,
                items,
            } = DurableStore::open(dir)?;
            let plane = EphemeralPlane::resume(&resume, items)?;
            let claims = Self::rebuild_claims(&plane)?;
            let judgments = Self::rebuild_judgments(&plane);
            Ok(MemoryService {
                plane,
                claims,
                judgments,
                store: Some(store),
            })
        } else {
            let (plane, custody) = EphemeralPlane::bootstrap_with_custody()?;
            let store = DurableStore::create_from_ceremony(dir, &plane, custody)?;
            Ok(MemoryService {
                plane,
                claims: BTreeMap::new(),
                judgments: Vec::new(),
                store: Some(store),
            })
        }
    }

    /// The honest per-mode durability label every view carries.
    pub(crate) fn durability_label(&self) -> &'static str {
        if self.store.is_some() {
            "durable"
        } else {
            "ephemeral"
        }
    }

    /// Rebuild the claim registry from recovered ops: decode each held
    /// `m.claim` body (reducer-side Node walk — write-with-core /
    /// read-with-reducer, the differential spirit) and re-derive
    /// provenance from the op envelope. Envelope truth is what
    /// survives a restart: agent-session/peer actors keep their
    /// gate-named principals verbatim; `human`/`daemon` actors carry
    /// the O8 device identity, so they surface as `dashboard` /
    /// `unattributed` WITHOUT a principal (documented collapse — the
    /// live-path exit criterion is unaffected).
    fn rebuild_claims(
        plane: &EphemeralPlane,
    ) -> Result<BTreeMap<[u8; 32], ClaimRecord>, MemoryError> {
        use owner_plane_reducer::envelope::parse_op;
        let mut claims = BTreeMap::new();
        for raw in plane.held_items().values() {
            let Ok(op) = parse_op(raw) else { continue };
            if op.header.operation_type != Mclaim::OP_TYPE {
                continue;
            }
            let body = op.body.clone();
            let text = |k: &str| body.get(k).and_then(|v| v.as_text().map(|t| t.to_string()));
            let prov_node = body.get("provenance");
            let prov_text = |k: &str| {
                prov_node
                    .as_ref()
                    .and_then(|p| p.get(k))
                    .and_then(|v| v.as_text().map(|t| t.to_string()))
            };
            let kind_str = text("kind").unwrap_or_default();
            let sens_str = text("sensitivity").unwrap_or_default();
            let kind = parse_vocab("kind", &kind_str, Kind::ALL, Kind::as_str)?;
            let sensitivity = parse_vocab("sensitivity", &sens_str, Class::ALL, Class::as_str)?;
            let labels = body
                .get("labels")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|n| n.as_text().map(|t| t.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let proposed_by = match op.header.actor_kind {
                // The envelope carries the PRINCIPAL verbatim; the
                // gate-bound session is not re-attestable across a
                // restart (the body's `session` is the writer's context
                // claim, not attribution), so recovered provenance
                // collapses `session` to None — documented, and the
                // live-path exit criterion is unaffected.
                "agent-session" => ClaimProvenance {
                    v: 1,
                    actor: "agent_session".into(),
                    principal: Some(op.header.actor_id.to_string()),
                    session: None,
                },
                "peer" => ClaimProvenance {
                    v: 1,
                    actor: "peer".into(),
                    principal: Some(op.header.actor_id.to_string()),
                    session: None,
                },
                "human" => ClaimProvenance {
                    v: 1,
                    actor: "dashboard".into(),
                    principal: None,
                    session: None,
                },
                _ => ClaimProvenance {
                    v: 1,
                    actor: "unattributed".into(),
                    principal: None,
                    session: None,
                },
            };
            claims.insert(
                op.op_hash(),
                ClaimRecord {
                    kind,
                    statement: text("statement").unwrap_or_default(),
                    sensitivity,
                    session: prov_text("session"),
                    project: prov_text("project"),
                    model: prov_text("model"),
                    labels,
                    created_ms: op.header.created_ms,
                    proposed_by,
                },
            );
        }
        Ok(claims)
    }

    /// Rebuild the judgment history from recovered ops, in the log's
    /// append order. Provenance is envelope truth in the DURABLE
    /// identity vocabulary (ruling R2): `human` → `owner` (the O4
    /// shape the judgment seal mints — a dashboard-vs-ctl surface
    /// distinction cannot survive restart, so the live path records
    /// the same collapse), attested/`agent-session` → `session`,
    /// `peer` → `peer`, anything else (a bare non-human writer, whose
    /// judgments are D-201-inert but §11.2 "recorded and surfaced")
    /// → `unattributed`.
    fn rebuild_judgments(plane: &EphemeralPlane) -> Vec<JudgmentRecord> {
        use owner_plane_reducer::envelope::parse_op;
        let mut judgments = Vec::new();
        for raw in plane.held_items().values() {
            let Ok(op) = parse_op(raw) else { continue };
            if op.header.operation_type != Mjudge::OP_TYPE {
                continue;
            }
            let body = op.body.clone();
            let text = |k: &str| body.get(k).and_then(|v| v.as_text().map(|t| t.to_string()));
            let bytes32 = |k: &str| body.get(k).and_then(|v| v.bytes_n::<32>());
            let Some(target) = bytes32("target") else {
                continue;
            };
            let judged_by = match op.header.actor_kind {
                "human" => ClaimProvenance {
                    v: 1,
                    actor: "owner".into(),
                    principal: None,
                    session: None,
                },
                "agent-session" => ClaimProvenance {
                    v: 1,
                    actor: "session".into(),
                    principal: Some(op.header.actor_id.to_string()),
                    session: None,
                },
                "peer" => ClaimProvenance {
                    v: 1,
                    actor: "peer".into(),
                    principal: Some(op.header.actor_id.to_string()),
                    session: None,
                },
                _ => ClaimProvenance {
                    v: 1,
                    actor: "unattributed".into(),
                    principal: None,
                    session: None,
                },
            };
            judgments.push(JudgmentRecord {
                id: op.op_hash(),
                verdict: text("verdict").unwrap_or_default(),
                target,
                replacement: bytes32("replacement"),
                reason: text("reason"),
                at_ms: op.header.created_ms,
                judged_by,
                policy: body
                    .get("policy")
                    .and_then(|p| p.get("id"))
                    .and_then(|v| v.as_text().map(|t| t.to_string()))
                    .unwrap_or_default(),
            });
        }
        judgments
    }

    /// The plane id (hex) — logged at wiring so an operator can tell
    /// one plane incarnation from the next (especially in ephemeral
    /// mode, where a restart mints a new plane).
    pub(crate) fn plane_id_hex(&self) -> String {
        hex32(&self.plane.plane_id)
    }

    /// The tenant-edge write authorization (seam ruling Q4: rings are
    /// an authorization decision made HERE, from the gate-resolved
    /// actor kind on top of the pre-dispatch IAM gate — never a field
    /// of the seam type). Owner surfaces — the dashboard and the
    /// owner's own local processes — may reach every write verb the
    /// service exposes; supervised agent sessions, federated peers,
    /// and unattributed callers are ring-2 writers: they AUTHOR
    /// candidates (`propose`) and nothing else. Judgment, pin, and
    /// curation verbs stay owner-side, and the denial is a named
    /// outcome (§C.2 discipline), never a silent downgrade.
    fn authorize_write(actor: &ActorBinding, verb: Verb) -> Result<(), MemoryError> {
        let owner_surface = matches!(
            actor.kind,
            GateActorKind::Dashboard | GateActorKind::LocalProcess
        );
        if owner_surface || verb == Verb::Propose {
            return Ok(());
        }
        Err(MemoryError::NotPermitted {
            verb: verb.as_str(),
            actor: actor.kind.as_str(),
        })
    }

    /// Map the gate-resolved actor onto the kernel envelope's closed
    /// actor vocabulary. O8 constrains `human`/`daemon`/`browser`/
    /// `service` actor ids to the writer device id (`tenant_op` fills
    /// it for `None`); `agent-session`/`peer` ids are free-form and
    /// carry the gate-named IAM principal verbatim. Ops seal
    /// UNATTESTED in this build: an unattested non-human actor has no
    /// §11.4 class, so ring-2 judgments would be fold-inert even if
    /// one got past [`Self::authorize_write`] — attestation (the
    /// session path to status influence) is a deliberate later
    /// decision that lands with the judgment surfaces it affects.
    fn envelope_actor(actor: &ActorBinding) -> (ActorKind, Option<String>) {
        match actor.kind {
            // The human at an authenticated dashboard surface.
            GateActorKind::Dashboard => (ActorKind::Human, None),
            // The owner's box-local software, and internal dispatch
            // that states no actor: the daemon device itself.
            GateActorKind::LocalProcess | GateActorKind::Unattributed => (ActorKind::Daemon, None),
            GateActorKind::AgentSession => (
                ActorKind::AgentSession,
                actor.principal_id.clone().or_else(|| {
                    // Principal-less bindings cannot arise from the
                    // gates (`from_principal` always names one); keep
                    // the session identity rather than fabricating.
                    actor.session_id.clone()
                }),
            ),
            GateActorKind::Peer => (ActorKind::Peer, actor.principal_id.clone()),
        }
    }

    /// The single choke point every write verb seals through:
    /// authorize at the tenant edge, map the actor onto the envelope,
    /// then mint + admit. Future write surfaces (judgments, pins,
    /// curation) MUST route through here — never `plane.tenant_op`
    /// directly — so the ring rules cannot be bypassed.
    fn seal_write(
        &mut self,
        actor: &ActorBinding,
        verb: Verb,
        op_type: &str,
        body: owner_plane_core::cbor::Value,
    ) -> Result<[u8; 32], MemoryError> {
        Self::authorize_write(actor, verb)?;
        let (actor_kind, actor_id) = Self::envelope_actor(actor);
        self.plane.tenant_op(actor_kind, actor_id, op_type, body)
    }

    /// The judgment-class choke (ruling R1, 2026-07-20) — the ONLY
    /// path that seals an `m.judge` op, and the ONLY place the HUMAN
    /// envelope actor is minted. Structure IS the enforcement
    /// (condition 1): the ring check runs first, so a ring-2 caller
    /// takes the named `actor-not-permitted` denial and can never
    /// reach the human seal below (condition 2's test pins it); the
    /// verb is fixed at `judge.full` here, never caller input. An
    /// owner-surface invocation — dashboard or the owner's local
    /// shell — IS the owner-at-the-box posture D-47 names, so the op
    /// carries O4 human evidence: `kind: "human"`, id = the writer
    /// device (O8, filled by `tenant_op`), `attested_by` ABSENT.
    /// Authoring ops keep [`Self::envelope_actor`]'s mapping — the
    /// narrowness is the point (judgments express authority the edge
    /// verified; proposals carry diary provenance and need none).
    /// Even bypassed, the kernel's D-201 keeps a bare-daemon
    /// judgment fold-inert — belt and suspenders.
    fn seal_judgment(
        &mut self,
        actor: &ActorBinding,
        body: &Mjudge,
    ) -> Result<[u8; 32], MemoryError> {
        Self::authorize_write(actor, Verb::JudgeFull)?;
        self.plane
            .tenant_op(ActorKind::Human, None, Mjudge::OP_TYPE, body.to_value())
    }

    /// P1.8 durability lockstep (§6.2 L1) shared by every sealing
    /// path: the write ACKs only after its sealed item is flushed; on
    /// a store failure the in-memory admission is RETRACTED (nothing
    /// unpersisted is ever exposed) and the named outcome surfaces.
    fn persist_admitted(&mut self, op_hash: &[u8; 32]) -> Result<(), MemoryError> {
        if let Some(store) = &mut self.store {
            let op_bytes = self
                .plane
                .held_items()
                .values()
                .last()
                .expect("the op just admitted is held")
                .clone();
            if let Err(e) = store.append_sealed_op(&op_bytes) {
                self.plane.retract_unpersisted(op_hash)?;
                return Err(MemoryError::InvalidArg(e.to_string()));
            }
        }
        Ok(())
    }

    /// Author a claim (`propose` — the candidate lane; `assert` is a
    /// separate verb this build does not expose). The claim enters as
    /// a `candidate` and only judgments move its derived status.
    /// `actor` is the gate-resolved binding the dispatch edge carried
    /// in — attribution never comes from `args`.
    pub(crate) fn propose(
        &mut self,
        args: ProposeArgs,
        actor: &ActorBinding,
    ) -> Result<ClaimView, MemoryError> {
        if args.statement.trim().is_empty() {
            return Err(MemoryError::InvalidArg(
                "statement must be non-empty".into(),
            ));
        }
        let kind = parse_vocab("kind", &args.kind, Kind::ALL, Kind::as_str)?;
        let sensitivity = parse_vocab("sensitivity", &args.sensitivity, Class::ALL, Class::as_str)?;
        let created_ms = now_ms();
        // Session CONTEXT is the writer's statement; when unstated it
        // defaults from the gate-bound session (never from dispatch
        // parameters or query echoes — those may carry unbound ids).
        let session = args.session.clone().or_else(|| actor.session_id.clone());
        let proposed_by = ClaimProvenance::from_binding(actor);
        let body = Mclaim {
            kind,
            statement: args.statement.clone(),
            sensitivity,
            observed_at_ms: Some(created_ms),
            valid_from_ms: None,
            valid_until_ms: None,
            expires_at_ms: None,
            session: session.clone(),
            project: args.project.clone(),
            model: args.model.clone(),
            evidence: vec![],
            supersedes: None,
            labels: if args.labels.is_empty() {
                None
            } else {
                Some(args.labels.clone())
            },
        };
        let op_hash = self.seal_write(actor, Verb::Propose, Mclaim::OP_TYPE, body.to_value())?;
        self.persist_admitted(&op_hash)?;
        self.claims.insert(
            op_hash,
            ClaimRecord {
                kind,
                statement: args.statement,
                sensitivity,
                session,
                project: args.project,
                model: args.model,
                labels: args.labels,
                created_ms,
                proposed_by,
            },
        );
        Ok(self.view(&op_hash, &self.claims[&op_hash]))
    }

    /// Bounded lexical search. Candidates are excluded unless opted
    /// into, and every result carries its derived status (§6.5).
    pub(crate) fn search(&self, args: &SearchArgs) -> Vec<ClaimView> {
        let limit = args.limit.clamp(1, SEARCH_LIMIT_CEILING);
        let needle = args.query.to_lowercase();
        let mut out = Vec::new();
        for (op_hash, rec) in &self.claims {
            if !needle.is_empty() {
                let hay = format!(
                    "{} {} {}",
                    rec.statement.to_lowercase(),
                    rec.labels.join(" ").to_lowercase(),
                    rec.kind.as_str()
                );
                if !hay.contains(&needle) {
                    continue;
                }
            }
            // Derive the status BEFORE building the view: excluded
            // candidates never pay for the view's clones.
            let status = self.claim_status_of(op_hash);
            if status == "candidate" && !args.include_candidates {
                continue;
            }
            out.push(self.view_with_status(op_hash, rec, status));
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    /// Read one claim by id prefix (≥ 8 hex chars of the op hash).
    pub(crate) fn read(&self, id_prefix: &str) -> Result<ClaimView, MemoryError> {
        let op_hash = self.resolve(id_prefix)?;
        Ok(self.view(&op_hash, &self.claims[&op_hash]))
    }

    /// Resolve a claim id prefix (≥ 8 hex chars) to its op hash —
    /// shared by read and every judgment target/replacement lookup.
    fn resolve(&self, id_prefix: &str) -> Result<[u8; 32], MemoryError> {
        let p = id_prefix.to_lowercase();
        if p.len() < 8 || !p.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(MemoryError::InvalidArg(
                "id prefix must be at least 8 hex characters".into(),
            ));
        }
        let matches: Vec<&[u8; 32]> = self
            .claims
            .keys()
            .filter(|hash| hex_prefix_matches(hash, &p))
            .collect();
        match matches.len() {
            0 => Err(MemoryError::NotFound(id_prefix.into())),
            1 => Ok(*matches[0]),
            n => Err(MemoryError::Ambiguous(id_prefix.into(), n)),
        }
    }

    /// Judge a claim (owner curation): seal one attributed `m.judge`
    /// op through the R1 choke and return the target's refreshed view
    /// — derived status visibly moved iff the fold counted it. The
    /// polref is stamped from the target space's binding, never
    /// caller input (§11.3; a mismatch would pend `policy-missing`).
    pub(crate) fn judge(
        &mut self,
        args: JudgeArgs,
        actor: &ActorBinding,
    ) -> Result<ClaimView, MemoryError> {
        if let Some(reason) = &args.reason {
            let n = reason.chars().count();
            if n > MAX_REASON_CHARS {
                return Err(MemoryError::InvalidArg(format!(
                    "reason is {n} chars (cap {MAX_REASON_CHARS}) — shorten it; \
                     the full rationale can live in a claim or evidence"
                )));
            }
        }
        let target = self.resolve(&args.id)?;
        let policy = self.plane.home_status_policy();
        let policy_id = policy.id.clone();
        let body = match args.verdict.as_str() {
            v @ ("accept" | "dispute" | "retire") => {
                if args.replacement.is_some() {
                    return Err(MemoryError::InvalidArg(format!(
                        "replacement only applies to supersede (verdict: {v})"
                    )));
                }
                let verdict = match v {
                    "accept" => BasicVerdict::Accept,
                    "dispute" => BasicVerdict::Dispute,
                    _ => BasicVerdict::Retire,
                };
                Mjudge::Basic {
                    verdict,
                    target,
                    policy,
                    reason: args.reason.clone(),
                    evidence: None,
                }
            }
            "supersede" => {
                let Some(replacement_prefix) = args.replacement.as_deref() else {
                    return Err(MemoryError::InvalidArg(
                        "supersede requires replacement (the superseding claim's id)".into(),
                    ));
                };
                let replacement = self.resolve(replacement_prefix)?;
                if replacement == target {
                    return Err(MemoryError::InvalidArg(
                        "a claim cannot supersede itself".into(),
                    ));
                }
                Mjudge::Supersede {
                    target,
                    replacement,
                    policy,
                    reason: args.reason.clone(),
                }
            }
            // The kernel vocabulary this build deliberately does not
            // mint: retract (author/agent-lane machinery, surfaced
            // read-only in v1), raise_class / declassify (fail-closed
            // classification arms, §C.2). Named rejection, never a
            // coerced or silently dropped verdict.
            other => {
                return Err(MemoryError::Vocabulary {
                    what: "verdict",
                    got: other.to_string(),
                    allowed: "accept, dispute, retire, supersede".into(),
                })
            }
        };
        let op_hash = self.seal_judgment(actor, &body)?;
        self.persist_admitted(&op_hash)?;
        self.judgments.push(JudgmentRecord {
            id: op_hash,
            verdict: args.verdict.clone(),
            target,
            replacement: match &body {
                Mjudge::Supersede { replacement, .. } => Some(*replacement),
                _ => None,
            },
            reason: args.reason,
            // The sealed op's own HLC millisecond — exactly what a
            // restart recovers from the header (ruling R2: live and
            // rebuilt views agree).
            at_ms: self.plane.last_hlc_ms(),
            // The durable identity the envelope carries: the owner
            // (R2 — rebuild maps `human` → `owner` identically).
            judged_by: ClaimProvenance {
                v: 1,
                actor: "owner".into(),
                principal: None,
                session: None,
            },
            policy: policy_id,
        });
        Ok(self.view(&target, &self.claims[&target]))
    }

    /// Judgment history for one claim, oldest first (append order).
    fn judgments_for(&self, claim: &[u8; 32]) -> Vec<JudgmentView> {
        self.judgments
            .iter()
            .filter(|j| j.target == *claim)
            .map(|j| JudgmentView {
                id: hex32(&j.id),
                verdict: j.verdict.clone(),
                target: hex32(&j.target),
                replacement: j.replacement.as_ref().map(hex32),
                reason: j.reason.clone(),
                at_ms: j.at_ms,
                judged_by: j.judged_by.clone(),
                policy: j.policy.clone(),
            })
            .collect()
    }

    /// The reducer-derived status a view carries ("pending" until the
    /// fold speaks) — derived at read time, never stored.
    fn claim_status_of(&self, op_hash: &[u8; 32]) -> &'static str {
        self.plane.claim_status(op_hash).unwrap_or("pending")
    }

    fn view(&self, op_hash: &[u8; 32], rec: &ClaimRecord) -> ClaimView {
        let mut view = self.view_with_status(op_hash, rec, self.claim_status_of(op_hash));
        view.judgments = self.judgments_for(op_hash);
        view
    }

    fn view_with_status(
        &self,
        op_hash: &[u8; 32],
        rec: &ClaimRecord,
        status: &'static str,
    ) -> ClaimView {
        ClaimView {
            id: hex32(op_hash),
            kind: rec.kind.as_str().into(),
            statement: rec.statement.clone(),
            sensitivity: rec.sensitivity.as_str().into(),
            status: status.into(),
            session: rec.session.clone(),
            project: rec.project.clone(),
            model: rec.model.clone(),
            labels: rec.labels.clone(),
            created_ms: rec.created_ms,
            proposed_by: rec.proposed_by.clone(),
            durability: self.durability_label().into(),
            // Lean by construction (search results); [`Self::view`]
            // attaches history for single-claim views.
            judgments: Vec::new(),
        }
    }

    /// Test seam for the space-denial exit test: a claim aimed at the
    /// AUDIT space, which the ordinary writer grant does not cover.
    #[cfg(test)]
    fn tenant_op_for_test_in_audit_space(&mut self) -> Result<[u8; 32], MemoryError> {
        let body = Mclaim {
            kind: Kind::Observation,
            statement: "wrong space".into(),
            sensitivity: Class::Private,
            observed_at_ms: Some(1),
            valid_from_ms: None,
            valid_until_ms: None,
            expires_at_ms: None,
            session: None,
            project: None,
            model: None,
            evidence: vec![],
            supersedes: None,
            labels: None,
        };
        self.plane.tenant_op_in_space_for_test(
            self.plane.audit_space,
            Mclaim::OP_TYPE,
            body.to_value(),
        )
    }

    /// Test seam: seal an arbitrary Memory-tenant op on the plane as
    /// the bare daemon actor, BYPASSING the tenant-edge authorization
    /// (the D-201 inert-judgment and §C.2 named-outcome tests exercise
    /// kernel semantics directly, without a public verb existing yet).
    #[cfg(test)]
    pub(crate) fn tenant_op_for_test(
        &mut self,
        op_type: &str,
        body: cbor::Value,
    ) -> Result<[u8; 32], MemoryError> {
        self.plane.tenant_op(ActorKind::Daemon, None, op_type, body)
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use owner_plane_core::shapes::memory::{BasicVerdict, Mjudge};
    use owner_plane_core::shapes::Polref;

    fn propose_args(statement: &str) -> ProposeArgs {
        ProposeArgs {
            kind: "observation".into(),
            statement: statement.into(),
            sensitivity: "private".into(),
            session: Some("test-session".into()),
            project: None,
            model: None,
            labels: vec!["memory-p1".into()],
        }
    }

    /// Internal-dispatch posture: no actor stated, explicitly
    /// unattributed (the fail-closed default the dispatch edge uses).
    fn no_actor() -> ActorBinding {
        ActorBinding::unattributed()
    }

    fn agent_actor() -> ActorBinding {
        ActorBinding::agent_session(
            Some("principal:agent-session:sess-1".into()),
            "sess-1".into(),
        )
    }

    /// The genesis ceremony must ADMIT under the stamped reader.
    #[test]
    fn bootstrap_genesis_admits() {
        let svc = MemoryService::new().expect("bootstrap admits");
        assert_eq!(svc.plane.held_ops(), 1, "genesis is held");
    }

    /// The no-alloc prefix comparator must agree with the hex-string
    /// formulation it replaced (`hex32(key).starts_with(p)`),
    /// including odd-length prefixes and the over-length edge.
    #[test]
    fn hex_prefix_matches_agrees_with_hex_expansion() {
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let full = hex32(&key);
        for len in [8, 9, 12, 63, 64] {
            assert_eq!(
                hex_prefix_matches(&key, &full[..len]),
                full.starts_with(&full[..len]),
                "len {len}"
            );
            assert!(hex_prefix_matches(&key, &full[..len]), "len {len}");
        }
        let mut wrong = full[..12].to_string();
        wrong.pop();
        wrong.push(if full.as_bytes()[11] == b'0' {
            '1'
        } else {
            '0'
        });
        assert!(!hex_prefix_matches(&key, &wrong), "mismatched last nibble");
        let over = format!("{full}0");
        assert!(
            !hex_prefix_matches(&key, &over),
            "a 65-char prefix can never match a 64-char id"
        );
    }

    /// propose → candidate → read round-trip, with the reducer-derived
    /// status and the honest durability label on the view.
    #[test]
    fn propose_then_read_roundtrip() {
        let mut svc = MemoryService::new().unwrap();
        let view = svc
            .propose(propose_args("the deploy runs at 06:00 UTC"), &no_actor())
            .unwrap();
        assert_eq!(view.status, "candidate");
        assert_eq!(view.durability, "ephemeral");
        let back = svc.read(&view.id[..12]).unwrap();
        assert_eq!(back.statement, "the deploy runs at 06:00 UTC");
        assert_eq!(back.kind, "observation");
        assert_eq!(back.session.as_deref(), Some("test-session"));
    }

    /// §6.5: candidates are excluded from retrieval by default;
    /// callers opt in, results stay bounded and status-labeled.
    #[test]
    fn search_excludes_candidates_by_default() {
        let mut svc = MemoryService::new().unwrap();
        for i in 0..5 {
            svc.propose(
                propose_args(&format!("observation number {i}")),
                &no_actor(),
            )
            .unwrap();
        }
        let default_results = svc.search(&SearchArgs {
            query: "observation".into(),
            ..SearchArgs::default()
        });
        assert!(default_results.is_empty(), "candidates hidden by default");
        let opted = svc.search(&SearchArgs {
            query: "observation".into(),
            limit: 3,
            include_candidates: true,
        });
        assert_eq!(opted.len(), 3, "bounded by the caller's limit");
        assert!(opted.iter().all(|v| v.status == "candidate"));
    }

    /// Unknown vocabulary rejects with the allowed set — never a
    /// defaulted or downgraded value.
    #[test]
    fn unknown_kind_rejects() {
        let mut svc = MemoryService::new().unwrap();
        let mut args = propose_args("x");
        args.kind = "fact".into();
        let err = svc.propose(args, &no_actor()).unwrap_err();
        assert!(matches!(err, MemoryError::Vocabulary { what: "kind", .. }));
    }

    /// D-201 (the ruled D2): a bare non-human unattested writer's
    /// judgment is recordable where authoring verbs admit it and INERT
    /// in the status fold — the daemon's self-retract does not move
    /// the claim off `candidate`.
    #[test]
    fn bare_daemon_retract_is_recorded_but_inert() {
        let mut svc = MemoryService::new().unwrap();
        let view = svc
            .propose(propose_args("retractable observation"), &no_actor())
            .unwrap();
        let target: [u8; 32] = {
            let mut b = [0u8; 32];
            for (i, chunk) in view.id.as_bytes().chunks(2).enumerate().take(32) {
                b[i] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
            }
            b
        };
        let judge = Mjudge::Basic {
            verdict: BasicVerdict::Retract,
            target,
            policy: Polref {
                id: "workflow-v1".into(),
                version: 1,
                hash: owner_plane_core::scenario::workflow_v1().hash(),
            },
            reason: Some("test retract".into()),
            evidence: None,
        };
        svc.tenant_op_for_test(Mjudge::OP_TYPE, judge.to_value())
            .expect("judgment admits (authoring verbs) even though it counts nowhere");
        let after = svc.read(&view.id[..12]).unwrap();
        assert_eq!(
            after.status, "candidate",
            "bare-daemon judgment must be inert in the status fold (D-201)"
        );
    }

    /// D-203 §C.2: a rejected operation surfaces the reducer's NAMED
    /// outcome/disposition verbatim — here the closed §7.1/§11.1
    /// registry rejecting an unknown operation type as `op-unknown` —
    /// and the failed op exerts no precedence (D-112): the writer
    /// chain position stays reusable and the next propose admits.
    #[test]
    fn rejected_op_surfaces_named_outcome() {
        let mut svc = MemoryService::new().unwrap();
        svc.propose(propose_args("first claim"), &no_actor())
            .unwrap();
        let err = svc
            .tenant_op_for_test("m.bogus", cbor::map(vec![]))
            .unwrap_err();
        match err {
            MemoryError::Rejected { outcome, .. } => assert_eq!(
                outcome, "op-unknown",
                "the reducer's named outcome must surface verbatim"
            ),
            other => panic!("expected a named kernel rejection, got {other:?}"),
        }
        svc.propose(propose_args("after the rejection"), &no_actor())
            .expect("a rejected op must not consume the chain position");
    }

    /// The tenant-edge attribution mapping: a gate-bound agent session
    /// lands in the claim's own versioned provenance fields — principal
    /// verbatim, session from token possession — and the sealed op
    /// ADMITS under the stamped reducer with the `agent-session`
    /// envelope actor (free-form id lane, O8).
    #[test]
    fn agent_session_propose_records_the_gate_actor() {
        let mut svc = MemoryService::new().unwrap();
        let mut args = propose_args("attributed observation");
        args.session = None;
        let view = svc.propose(args, &agent_actor()).unwrap();
        assert_eq!(view.status, "candidate", "sealed op admits");
        assert_eq!(view.proposed_by.v, 1);
        assert_eq!(view.proposed_by.actor, "agent_session");
        assert_eq!(
            view.proposed_by.principal.as_deref(),
            Some("principal:agent-session:sess-1"),
            "the IAM principal rides verbatim (exit criterion)"
        );
        assert_eq!(view.proposed_by.session.as_deref(), Some("sess-1"));
        // Unstated session CONTEXT defaults from the gate-bound
        // session, so the claim reads honestly in session views.
        assert_eq!(view.session.as_deref(), Some("sess-1"));
        // An unattributed write stays explicitly unattributed.
        let view = svc
            .propose(propose_args("internal observation"), &no_actor())
            .unwrap();
        assert_eq!(view.proposed_by.actor, "unattributed");
        assert_eq!(view.proposed_by.principal, None);
        assert_eq!(view.proposed_by.session, None);
    }

    /// A writer-stated session is a context CLAIM and survives as
    /// stated; attribution comes from the gate binding regardless —
    /// the two must never be conflated (that conflation is the
    /// forgeable-attribution hole the seam closes).
    #[test]
    fn caller_stated_session_stays_a_context_claim() {
        let mut svc = MemoryService::new().unwrap();
        let mut args = propose_args("context-stated observation");
        args.session = Some("stated-context".into());
        let view = svc.propose(args, &agent_actor()).unwrap();
        assert_eq!(view.session.as_deref(), Some("stated-context"));
        assert_eq!(
            view.proposed_by.session.as_deref(),
            Some("sess-1"),
            "attribution ignores the stated context"
        );
    }

    /// Owner-surface mappings must ADMIT under the stamped reducer:
    /// O8 pins `human`/`daemon` actor ids to the writer device id, so
    /// a wrong mapping would reject as `body-invariant` here.
    #[test]
    fn owner_surface_and_peer_proposals_admit() {
        let mut svc = MemoryService::new().unwrap();
        let dashboard = ActorBinding::dashboard(Some("principal:root-session:test".into()));
        let view = svc
            .propose(propose_args("owner observation"), &dashboard)
            .unwrap();
        assert_eq!(view.status, "candidate");
        assert_eq!(view.proposed_by.actor, "dashboard");
        assert_eq!(
            view.proposed_by.principal.as_deref(),
            Some("principal:root-session:test")
        );

        let local = ActorBinding::local_process(Some("principal:local-process:loopback".into()));
        let view = svc
            .propose(propose_args("shell observation"), &local)
            .unwrap();
        assert_eq!(view.proposed_by.actor, "local_process");

        let peer = ActorBinding::peer(Some("principal:peer:fingerprint".into()));
        let view = svc
            .propose(propose_args("peer observation"), &peer)
            .unwrap();
        assert_eq!(view.proposed_by.actor, "peer");
    }

    /// Ring-2 propose-only (the seam ruling's tenant-edge decision):
    /// supervised agent sessions, peers, and unattributed callers may
    /// author candidates and NOTHING else — every other write verb
    /// denies with the named `actor-not-permitted` outcome BEFORE any
    /// kernel contact; owner surfaces pass the edge for every verb.
    #[test]
    fn ring2_actors_are_propose_only() {
        for actor in [
            agent_actor(),
            ActorBinding::peer(Some("principal:peer:fingerprint".into())),
            no_actor(),
        ] {
            assert!(MemoryService::authorize_write(&actor, Verb::Propose).is_ok());
            for verb in [
                Verb::Assert,
                Verb::JudgeSafe,
                Verb::JudgeFull,
                Verb::PinSafe,
                Verb::PinFull,
                Verb::CurateInstruction,
            ] {
                match MemoryService::authorize_write(&actor, verb) {
                    Err(MemoryError::NotPermitted { verb: v, actor: a }) => {
                        assert_eq!(v, verb.as_str());
                        assert_eq!(a, actor.kind.as_str());
                    }
                    other => panic!("expected actor-not-permitted, got {other:?}"),
                }
            }
        }
        for actor in [
            ActorBinding::dashboard(None),
            ActorBinding::local_process(None),
        ] {
            for verb in [Verb::Propose, Verb::JudgeSafe, Verb::CurateInstruction] {
                assert!(MemoryService::authorize_write(&actor, verb).is_ok());
            }
        }

        // Through the choke point: the denial happens at the edge —
        // no op is minted, the plane never sees it.
        let mut svc = MemoryService::new().unwrap();
        let held_before = svc.plane.held_ops();
        let err = svc
            .seal_write(
                &agent_actor(),
                Verb::JudgeSafe,
                "m.judge",
                cbor::map(vec![]),
            )
            .unwrap_err();
        assert!(
            matches!(err, MemoryError::NotPermitted { .. }),
            "expected the named edge denial, got {err:?}"
        );
        assert_eq!(
            svc.plane.held_ops(),
            held_before,
            "a denied write must never reach the kernel"
        );
    }

    /// P1.8 exit battery — durable round-trip through the SERVICE with
    /// recovered provenance rules: agent-session principals survive a
    /// restart verbatim (the envelope carries them); owner-surface
    /// claims collapse to the envelope's O8 device identity
    /// (dashboard, no principal) — the documented reverse map. The
    /// mode label flips honestly and the chain keeps accepting.
    #[test]
    fn durable_service_roundtrip_recovers_claims_and_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plane");
        let agent_hash;
        {
            let mut svc = MemoryService::new_durable(&dir).unwrap();
            assert_eq!(svc.durability_label(), "durable");
            let view = svc
                .propose(propose_args("agent durable claim"), &agent_actor())
                .unwrap();
            assert_eq!(view.durability, "durable");
            agent_hash = view.id.clone();
            svc.propose(
                propose_args("owner durable claim"),
                &ActorBinding::dashboard(Some("principal:root:dashboard".into())),
            )
            .unwrap();
        }
        let mut svc = MemoryService::new_durable(&dir).unwrap();
        let all = svc.search(&SearchArgs {
            query: String::new(),
            limit: 50,
            include_candidates: true,
        });
        assert_eq!(all.len(), 2, "both claims survive the restart");
        let agent = all.iter().find(|c| c.id == agent_hash).unwrap();
        assert_eq!(
            agent.proposed_by.principal.as_deref(),
            Some("principal:agent-session:sess-1"),
            "agent principals survive restarts verbatim (envelope truth)"
        );
        assert_eq!(
            agent.proposed_by.session, None,
            "gate sessions are not re-attestable across restarts (documented collapse)"
        );
        assert_eq!(
            agent.session.as_deref(),
            Some("test-session"),
            "the writer's session CONTEXT claim survives as stated"
        );
        let owner = all.iter().find(|c| c.id != agent_hash).unwrap();
        assert_eq!(owner.proposed_by.actor, "dashboard");
        assert_eq!(
            owner.proposed_by.principal, None,
            "owner-surface principals collapse to the O8 device identity across restarts (documented)"
        );
        svc.propose(propose_args("post-restart claim"), &agent_actor())
            .expect("the recovered chain keeps accepting");
    }

    /// Ruling R2 binding test: judge live, reopen the plane — the
    /// views are IDENTICAL. Derived status re-folds to the same
    /// answer, and the judgment history (verdict, target,
    /// replacement, reason, timestamp, durable-identity provenance,
    /// policy) rebuilds byte-equal from the recovered envelopes; a
    /// post-restart judgment keeps counting on the recovered chain.
    #[test]
    fn durable_judgments_survive_restart_with_identical_views() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plane");
        let owner = ActorBinding::dashboard(Some("principal:root:dashboard".into()));
        let (old_live, new_live);
        {
            let mut svc = MemoryService::new_durable(&dir).unwrap();
            let old = svc.propose(propose_args("the old fact"), &owner).unwrap();
            let new = svc.propose(propose_args("the new fact"), &owner).unwrap();
            svc.judge(judge_args("accept", &new.id), &owner).unwrap();
            old_live = svc
                .judge(
                    JudgeArgs {
                        replacement: Some(new.id.clone()),
                        reason: Some("superseded by the corrected fact".into()),
                        ..judge_args("supersede", &old.id)
                    },
                    &owner,
                )
                .unwrap();
            assert_eq!(old_live.status, "superseded");
            new_live = svc.read(&new.id[..12]).unwrap();
        }
        let mut svc = MemoryService::new_durable(&dir).unwrap();
        let old_back = svc.read(&old_live.id[..12]).unwrap();
        let new_back = svc.read(&new_live.id[..12]).unwrap();
        assert_eq!(old_back.status, "superseded", "status re-folds identically");
        assert_eq!(
            old_back.judgments, old_live.judgments,
            "judgment history is identical across the restart (R2)"
        );
        assert_eq!(new_back.judgments, new_live.judgments);
        assert_eq!(old_back.judgments.len(), 1);
        assert_eq!(old_back.judgments[0].judged_by.actor, "owner");
        assert_eq!(
            old_back.judgments[0].reason.as_deref(),
            Some("superseded by the corrected fact")
        );
        // The recovered chain keeps judging: retire the replacement
        // and the old claim REVIVES (replacement loss, §11.2 rule 2 —
        // revival is automatic and surfaced).
        let retired = svc.judge(judge_args("retire", &new_back.id), &owner).unwrap();
        assert_eq!(retired.status, "retired");
        assert_eq!(
            svc.read(&old_back.id[..12]).unwrap().status,
            "candidate",
            "supersession released by the replacement's retirement (D-21 revival)"
        );
    }

    /// P1.8 exit battery — zone/space denial: an op aimed at a space
    /// the writer grant does not cover rejects with the kernel's named
    /// scope outcome. (The service only ever writes the home space;
    /// this pins what happens if that invariant ever breaks.)
    #[test]
    fn ops_outside_the_granted_space_reject_with_named_scope_outcome() {
        let mut svc = MemoryService::new().unwrap();
        let err = svc
            .tenant_op_for_test_in_audit_space()
            .expect_err("audit space is not in the writer grant");
        match err {
            MemoryError::Rejected { outcome, .. } => {
                assert!(
                    outcome.starts_with("scope-"),
                    "expected a named scope rejection, got {outcome}"
                );
            }
            other => panic!("expected a named kernel rejection, got {other:?}"),
        }
    }

    /// P1.8 exit battery — conflicting claims COEXIST: contradictory
    /// statements are both represented and surfaced, never silently
    /// overwritten or deduplicated (§5.4: conflicts are data).
    #[test]
    fn conflicting_claims_coexist_and_both_surface() {
        let mut svc = MemoryService::new().unwrap();
        svc.propose(propose_args("the deploy runs at 06:00 UTC"), &no_actor())
            .unwrap();
        svc.propose(propose_args("the deploy runs at 18:00 UTC"), &no_actor())
            .unwrap();
        let hits = svc.search(&SearchArgs {
            query: "the deploy runs".into(),
            limit: 10,
            include_candidates: true,
        });
        assert_eq!(hits.len(), 2, "both contradictory claims surface");
    }

    /// The Memory Explorer's kind/sensitivity selects are an unavoidable
    /// static mirror of the kernel's closed vocabularies (the SPA can't
    /// import `Kind::ALL`), so per the derive-don't-mirror convention a
    /// daemon-side parity test pins the option values to the source —
    /// a vocabulary change that forgets the fragment fails here instead
    /// of shipping as drift.
    #[test]
    fn explorer_vocab_selects_mirror_the_kernel() {
        let app = include_str!("../../../../static/app.html");
        let options = |select_id: &str| -> Vec<String> {
            let start = app
                .find(&format!("<select id=\"{select_id}\""))
                .unwrap_or_else(|| panic!("{select_id} select not found in static/app.html"));
            let block = &app[start..start + app[start..].find("</select>").unwrap()];
            block
                .match_indices("value=\"")
                .map(|(at, _)| {
                    let rest = &block[at + "value=\"".len()..];
                    rest[..rest.find('"').unwrap()].to_string()
                })
                .collect()
        };
        let kernel = |all: &[&str]| all.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            options("memory-add-kind"),
            kernel(&Kind::ALL.iter().map(|k| k.as_str()).collect::<Vec<_>>()),
            "memory-add-kind options drifted from Kind::ALL"
        );
        assert_eq!(
            options("memory-add-sensitivity"),
            kernel(&Class::ALL.iter().map(|c| c.as_str()).collect::<Vec<_>>()),
            "memory-add-sensitivity options drifted from Class::ALL"
        );
    }

    fn judge_args(verdict: &str, id: &str) -> JudgeArgs {
        JudgeArgs {
            verdict: verdict.into(),
            id: id.into(),
            reason: None,
            replacement: None,
        }
    }

    /// Owner judgments visibly move derived status for every minted
    /// verdict — accept, dispute, retire — and the judgment history
    /// rides the returned view (who judged what, when).
    #[test]
    fn owner_judgments_move_status_per_verdict() {
        let mut svc = MemoryService::new().unwrap();
        let owner = ActorBinding::dashboard(Some("principal:browser-cert:test".into()));

        let a = svc.propose(propose_args("accepted claim"), &owner).unwrap();
        let view = svc.judge(judge_args("accept", &a.id), &owner).unwrap();
        assert_eq!(view.status, "accepted", "accept counts (owner class)");
        assert_eq!(view.judgments.len(), 1);
        assert_eq!(view.judgments[0].verdict, "accept");
        assert_eq!(view.judgments[0].policy, "workflow-v1");

        let d = svc.propose(propose_args("disputed claim"), &owner).unwrap();
        let view = svc
            .judge(
                JudgeArgs {
                    reason: Some("disputed: authorship-in-fact, content unverified".into()),
                    ..judge_args("dispute", &d.id)
                },
                &owner,
            )
            .unwrap();
        assert_eq!(view.status, "disputed");
        assert_eq!(
            view.judgments[0].reason.as_deref(),
            Some("disputed: authorship-in-fact, content unverified"),
            "the rationale rides the sealed op and the view"
        );

        let r = svc.propose(propose_args("retired claim"), &owner).unwrap();
        let view = svc.judge(judge_args("retire", &r.id), &owner).unwrap();
        assert_eq!(view.status, "retired");
    }

    /// §11.2 rule 2 honesty (ruling R4): supersession holds only
    /// while the replacement's derived status is `accepted` — a
    /// candidate replacement leaves the target unmoved (the judgment
    /// is recorded, never fake atomicity), and accepting the
    /// replacement later flips the target to `superseded` with no
    /// further op.
    #[test]
    fn supersede_holds_only_while_replacement_accepted() {
        let mut svc = MemoryService::new().unwrap();
        let owner = ActorBinding::dashboard(None);
        let old = svc.propose(propose_args("the old fact"), &owner).unwrap();
        let new = svc.propose(propose_args("the new fact"), &owner).unwrap();

        let view = svc
            .judge(
                JudgeArgs {
                    replacement: Some(new.id.clone()),
                    ..judge_args("supersede", &old.id)
                },
                &owner,
            )
            .unwrap();
        assert_eq!(
            view.status, "candidate",
            "candidate replacement holds no supersession (recorded, surfaced, honest)"
        );
        assert_eq!(view.judgments.len(), 1, "the judgment IS recorded");
        assert_eq!(view.judgments[0].replacement.as_deref(), Some(&new.id[..]));

        svc.judge(judge_args("accept", &new.id), &owner).unwrap();
        let after = svc.read(&old.id[..12]).unwrap();
        assert_eq!(
            after.status, "superseded",
            "accepting the replacement completes the supersession via the fold alone"
        );
    }

    /// Ruling R1: the owner's LOCAL SHELL is an owner surface — a
    /// `local_process` judgment seals the O4 human shape (kind
    /// `human`, attested_by absent) and COUNTS, exactly like the
    /// dashboard. This is the mapping J0 found missing (bare-daemon
    /// ctl judgments would have been recorded-but-inert).
    #[test]
    fn local_process_judgments_seal_human_and_count() {
        let mut svc = MemoryService::new().unwrap();
        let shell = ActorBinding::local_process(Some("principal:loopback:test".into()));
        let claim = svc.propose(propose_args("judged from ctl"), &shell).unwrap();
        let view = svc.judge(judge_args("accept", &claim.id), &shell).unwrap();
        assert_eq!(view.status, "accepted", "ctl judgment moves status");
        assert_eq!(
            svc.plane.tail_op_actor(),
            Some(("human".into(), false)),
            "the sealed envelope carries O4 human evidence: kind human, UNATTESTED"
        );
        assert_eq!(view.judgments[0].judged_by.actor, "owner", "R2 identity");
        assert_eq!(view.judgments[0].judged_by.principal, None);
    }

    /// Ruling R1 condition 2: ring-2 actors take the named
    /// `actor-not-permitted` denial BEFORE any seal — they can never
    /// obtain the human envelope actor (and the kernel's D-201 keeps
    /// even a hypothetical bypass fold-inert). Nothing is sealed,
    /// nothing recorded.
    #[test]
    fn ring2_judgments_denied_before_any_seal() {
        let mut svc = MemoryService::new().unwrap();
        let owner = ActorBinding::dashboard(None);
        let claim = svc.propose(propose_args("target"), &owner).unwrap();
        let held_before = svc.plane.held_ops();

        for ring2 in [
            ActorBinding::agent_session(
                Some("principal:agent-session:sess-9".into()),
                "sess-9".into(),
            ),
            ActorBinding::peer(Some("principal:peer:remote".into())),
            ActorBinding::unattributed(),
        ] {
            let err = svc
                .judge(judge_args("accept", &claim.id), &ring2)
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    MemoryError::NotPermitted {
                        verb: "judge.full",
                        ..
                    }
                ),
                "named tenant-edge denial, got {err:?}"
            );
        }
        assert_eq!(
            svc.plane.held_ops(),
            held_before,
            "denied judgments seal NOTHING — the human seal is unreachable for ring-2"
        );
        assert!(
            svc.judgments.is_empty(),
            "no judgment record exists after denials"
        );
    }

    /// Unminted kernel vocabulary rejects with the allowed set:
    /// retract (author-lane machinery, read-only surfaced in v1),
    /// the fail-closed classification arms, and unknown words alike.
    #[test]
    fn unminted_verdicts_reject_with_the_allowed_set() {
        let mut svc = MemoryService::new().unwrap();
        let owner = ActorBinding::dashboard(None);
        let claim = svc.propose(propose_args("target"), &owner).unwrap();
        for verdict in ["retract", "raise_class", "declassify", "erase", "approve"] {
            let err = svc.judge(judge_args(verdict, &claim.id), &owner).unwrap_err();
            match err {
                MemoryError::Vocabulary { what, got, allowed } => {
                    assert_eq!(what, "verdict");
                    assert_eq!(got, verdict);
                    assert_eq!(allowed, "accept, dispute, retire, supersede");
                }
                other => panic!("expected vocabulary rejection for {verdict}, got {other:?}"),
            }
        }
    }

    /// Ruling R3: the 2000-char `reason` intake cap rejects loudly —
    /// never truncates — and names the cap.
    #[test]
    fn reason_cap_rejects_loudly() {
        let mut svc = MemoryService::new().unwrap();
        let owner = ActorBinding::dashboard(None);
        let claim = svc.propose(propose_args("target"), &owner).unwrap();
        let err = svc
            .judge(
                JudgeArgs {
                    reason: Some("x".repeat(MAX_REASON_CHARS + 1)),
                    ..judge_args("accept", &claim.id)
                },
                &owner,
            )
            .unwrap_err();
        assert!(
            matches!(&err, MemoryError::InvalidArg(m) if m.contains("2000")),
            "cap named in the rejection, got {err:?}"
        );
        let ok = svc
            .judge(
                JudgeArgs {
                    reason: Some("y".repeat(MAX_REASON_CHARS)),
                    ..judge_args("accept", &claim.id)
                },
                &owner,
            )
            .unwrap();
        assert_eq!(ok.status, "accepted", "at-cap reason seals fine");
    }

    /// Supersede argument shape: replacement is required for
    /// supersede, refused elsewhere, and self-supersession is refused
    /// at construction (the kernel would derive `disputed` from the
    /// cycle — refusing is the honest §C.2 construction-side named
    /// outcome).
    #[test]
    fn supersede_argument_shapes_are_enforced() {
        let mut svc = MemoryService::new().unwrap();
        let owner = ActorBinding::dashboard(None);
        let a = svc.propose(propose_args("claim a"), &owner).unwrap();
        let b = svc.propose(propose_args("claim b"), &owner).unwrap();

        let err = svc.judge(judge_args("supersede", &a.id), &owner).unwrap_err();
        assert!(matches!(&err, MemoryError::InvalidArg(m) if m.contains("replacement")));

        let err = svc
            .judge(
                JudgeArgs {
                    replacement: Some(b.id.clone()),
                    ..judge_args("accept", &a.id)
                },
                &owner,
            )
            .unwrap_err();
        assert!(matches!(&err, MemoryError::InvalidArg(m) if m.contains("supersede")));

        let err = svc
            .judge(
                JudgeArgs {
                    replacement: Some(a.id.clone()),
                    ..judge_args("supersede", &a.id)
                },
                &owner,
            )
            .unwrap_err();
        assert!(matches!(&err, MemoryError::InvalidArg(m) if m.contains("itself")));
    }

    /// POST-RULING FINDING #1 boundary proof: the stamped reducer
    /// fail-closes `m.pin` (registry-known, mechanism undispatched) —
    /// the named kernel boundary surfaces verbatim and nothing is
    /// admitted. When a future kernel slice lifts the row, this test
    /// fails loudly and the pin surface becomes buildable.
    #[test]
    fn pin_ops_are_fail_closed_at_the_stamped_kernel_boundary() {
        use owner_plane_core::shapes::memory::Mpin;
        let mut svc = MemoryService::new().unwrap();
        let owner = ActorBinding::dashboard(None);
        let claim = svc.propose(propose_args("pin target"), &owner).unwrap();
        svc.judge(judge_args("accept", &claim.id), &owner).unwrap();
        let target: [u8; 32] = {
            let mut b = [0u8; 32];
            for (i, chunk) in claim.id.as_bytes().chunks(2).enumerate().take(32) {
                b[i] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
            }
            b
        };
        let accept_judgment = svc.judgments.last().map(|j| j.id).unwrap();
        let pin = Mpin {
            target,
            dest_space: [0u8; 16],
            dest_role: "context".into(),
            expiry_ms: None,
            token_budget: None,
            provenance_floor: None,
            accepted_under_judgment: accept_judgment,
            accepted_under_policy: super::super::plane::workflow_polref(),
        };
        let err = svc
            .tenant_op_for_test(Mpin::OP_TYPE, pin.to_value())
            .unwrap_err();
        assert!(
            matches!(&err, MemoryError::Unimplemented(m) if m.contains("m.pin")),
            "the stamped kernel names the fail-closed boundary, got {err:?}"
        );
    }
}

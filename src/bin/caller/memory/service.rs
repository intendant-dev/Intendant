//! The Memory service: propose / search / read over the ephemeral
//! plane, with the reducer-derived status on every view.

use std::collections::BTreeMap;

#[cfg(test)]
use owner_plane_core::cbor;
use owner_plane_core::shapes::envelope::ActorKind;
use owner_plane_core::shapes::memory::Mclaim;
use owner_plane_core::shapes::{Class, Kind, ToValue, Verb};

use crate::access::actor::{ActorBinding, ActorKind as GateActorKind};

use super::plane::EphemeralPlane;
use super::types::{hex32, ClaimProvenance, ClaimView, MemoryError, ProposeArgs, SearchArgs};

/// Search results are hard-capped regardless of the caller's ask
/// (§6.5: bounded retrieval — no whole-store reads through this API).
const SEARCH_LIMIT_CEILING: usize = 50;

/// What the service records about a claim it minted: the plaintext it
/// authored (the mint side holds the plaintext; ops carry it in the
/// signed body) keyed by the accepted op hash. Status is NEVER stored
/// here — it is derived from the fold at read time.
struct ClaimRecord {
    op_hash: [u8; 32],
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

pub(crate) struct MemoryService {
    plane: EphemeralPlane,
    claims: BTreeMap<[u8; 32], ClaimRecord>,
    /// P1.8: the durable custody store, when this daemon runs the
    /// durable plane (macOS — multi-platform custody stays full Gate
    /// B, so other OSes run ephemeral and say so). `None` = ephemeral.
    store: Option<super::store::DurableStore>,
}

impl MemoryService {
    /// Bootstrap an ephemeral plane and an empty claim registry.
    pub(crate) fn new() -> Result<MemoryService, MemoryError> {
        Ok(MemoryService {
            plane: EphemeralPlane::bootstrap()?,
            claims: BTreeMap::new(),
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
            Ok(MemoryService {
                plane,
                claims,
                store: Some(store),
            })
        } else {
            let (plane, custody) = EphemeralPlane::bootstrap_with_custody()?;
            let store = DurableStore::create_from_ceremony(dir, &plane, custody)?;
            Ok(MemoryService {
                plane,
                claims: BTreeMap::new(),
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
                    op_hash: op.op_hash(),
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

    /// The plane id (hex) — logged at wiring so an operator can tell
    /// one ephemeral plane incarnation from the next across restarts.
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
        // P1.8 durability: §6.2 L1 — the claim ACKs only after its
        // sealed item is flushed. On a store failure the in-memory
        // admission is RETRACTED (nothing unpersisted is ever exposed)
        // and the named outcome surfaces verbatim.
        if let Some(store) = &mut self.store {
            let op_bytes = self
                .plane
                .held_items()
                .values()
                .last()
                .expect("the op just admitted is held")
                .clone();
            if let Err(e) = store.append_sealed_op(&op_bytes) {
                self.plane.retract_unpersisted(&op_hash)?;
                return Err(MemoryError::InvalidArg(e.to_string()));
            }
        }
        self.claims.insert(
            op_hash,
            ClaimRecord {
                op_hash,
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
        Ok(self.view(&self.claims[&op_hash]))
    }

    /// Bounded lexical search. Candidates are excluded unless opted
    /// into, and every result carries its derived status (§6.5).
    pub(crate) fn search(&self, args: &SearchArgs) -> Vec<ClaimView> {
        let limit = args.limit.clamp(1, SEARCH_LIMIT_CEILING);
        let needle = args.query.to_lowercase();
        let mut out = Vec::new();
        for rec in self.claims.values() {
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
            let view = self.view(rec);
            if view.status == "candidate" && !args.include_candidates {
                continue;
            }
            out.push(view);
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    /// Read one claim by id prefix (≥ 8 hex chars of the op hash).
    pub(crate) fn read(&self, id_prefix: &str) -> Result<ClaimView, MemoryError> {
        let p = id_prefix.to_lowercase();
        if p.len() < 8 || !p.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(MemoryError::InvalidArg(
                "id prefix must be at least 8 hex characters".into(),
            ));
        }
        let matches: Vec<&ClaimRecord> = self
            .claims
            .values()
            .filter(|r| hex32(&r.op_hash).starts_with(&p))
            .collect();
        match matches.len() {
            0 => Err(MemoryError::NotFound(id_prefix.into())),
            1 => Ok(self.view(matches[0])),
            n => Err(MemoryError::Ambiguous(id_prefix.into(), n)),
        }
    }

    fn view(&self, rec: &ClaimRecord) -> ClaimView {
        ClaimView {
            id: hex32(&rec.op_hash),
            kind: rec.kind.as_str().into(),
            statement: rec.statement.clone(),
            sensitivity: rec.sensitivity.as_str().into(),
            status: self
                .plane
                .claim_status(&rec.op_hash)
                .unwrap_or("pending")
                .into(),
            session: rec.session.clone(),
            project: rec.project.clone(),
            model: rec.model.clone(),
            labels: rec.labels.clone(),
            created_ms: rec.created_ms,
            proposed_by: rec.proposed_by.clone(),
            durability: self.durability_label().into(),
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
}

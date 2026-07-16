//! The Memory service: propose / search / read over the ephemeral
//! plane, with the reducer-derived status on every view.

use std::collections::BTreeMap;

#[cfg(test)]
use owner_plane_core::cbor;
use owner_plane_core::shapes::envelope::ActorKind;
use owner_plane_core::shapes::memory::Mclaim;
use owner_plane_core::shapes::{Class, Kind, ToValue};

use super::plane::EphemeralPlane;
use super::types::{hex32, ClaimView, MemoryError, ProposeArgs, SearchArgs};

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
}

impl MemoryService {
    /// Bootstrap an ephemeral plane and an empty claim registry.
    pub(crate) fn new() -> Result<MemoryService, MemoryError> {
        Ok(MemoryService {
            plane: EphemeralPlane::bootstrap()?,
            claims: BTreeMap::new(),
        })
    }

    /// The plane id (hex) — logged at wiring so an operator can tell
    /// one ephemeral plane incarnation from the next across restarts.
    pub(crate) fn plane_id_hex(&self) -> String {
        hex32(&self.plane.plane_id)
    }

    /// Author a claim (`propose` — the candidate lane; `assert` is a
    /// separate verb this build does not expose). The claim enters as
    /// a `candidate` and only judgments move its derived status.
    pub(crate) fn propose(&mut self, args: ProposeArgs) -> Result<ClaimView, MemoryError> {
        if args.statement.trim().is_empty() {
            return Err(MemoryError::InvalidArg(
                "statement must be non-empty".into(),
            ));
        }
        let kind = parse_vocab("kind", &args.kind, Kind::ALL, Kind::as_str)?;
        let sensitivity = parse_vocab("sensitivity", &args.sensitivity, Class::ALL, Class::as_str)?;
        let created_ms = now_ms();
        let body = Mclaim {
            kind,
            statement: args.statement.clone(),
            sensitivity,
            observed_at_ms: Some(created_ms),
            valid_from_ms: None,
            valid_until_ms: None,
            expires_at_ms: None,
            session: args.session.clone(),
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
        let op_hash =
            self.plane
                .tenant_op(ActorKind::Daemon, None, Mclaim::OP_TYPE, body.to_value())?;
        self.claims.insert(
            op_hash,
            ClaimRecord {
                op_hash,
                kind,
                statement: args.statement,
                sensitivity,
                session: args.session,
                project: args.project,
                model: args.model,
                labels: args.labels,
                created_ms,
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
            durability: "ephemeral",
        }
    }

    /// Test seam: seal an arbitrary Memory-tenant op on the plane
    /// (the D-201 inert-judgment and §C.2 named-outcome tests mint
    /// through this without a public verb existing yet).
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
            .propose(propose_args("the deploy runs at 06:00 UTC"))
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
            svc.propose(propose_args(&format!("observation number {i}")))
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
        let err = svc.propose(args).unwrap_err();
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
            .propose(propose_args("retractable observation"))
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
        svc.propose(propose_args("first claim")).unwrap();
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
        svc.propose(propose_args("after the rejection"))
            .expect("a rejected op must not consume the chain position");
    }
}

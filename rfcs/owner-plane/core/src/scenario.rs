//! Scenario prelude — the Appendix B pinned objects every fixture
//! shares.
//!
//! B.2/B.3 are the program's first true cross-validation targets: the
//! spec pins their canonical byte LENGTHS and `H_policy` hashes, so
//! the tests here re-derive both from the typed shapes through the
//! reference encoder — a mismatch is a failing executable trace
//! (D-200 grade), never something to paper over.

use crate::shapes::memory::{
    ActorClass, KindsSel, Policy, Relation, Rule, SpaceClassesSel, Verdictname,
};
use crate::shapes::{Bytes16, DeadlineFallback, Kind, Spaceclass, Strictness, Zonepolicy};

/// B.1 — the genesis ZonePolicy template instantiated for a zone:
/// the solo posture (D-28). No `time_witnesses`, no
/// `grant_epoch_slack`, no `connect_service_key`.
pub fn genesis_zone_policy(zone_id: Bytes16) -> Zonepolicy {
    Zonepolicy {
        zone_id,
        strictness: Strictness::Strict,
        deadline_fallback: DeadlineFallback::Budgets,
        require_cert_deadlines: false,
        grant_epoch_slack: None,
        time_witnesses: None,
        connect_service_key: None,
    }
}

fn rule(
    verdict: Verdictname,
    kinds: KindsSel,
    space_classes: SpaceClassesSel,
    actor_classes: &[ActorClass],
    relation: Relation,
) -> Rule {
    Rule {
        verdict,
        kinds,
        space_classes,
        actor_classes: actor_classes.to_vec(),
        relation,
    }
}

/// B.2 — the `workflow-v1` status policy, transcribed from the
/// pinned literal (rule order irrelevant: `policy.rules` is an E7
/// set; the literal's `actor_classes` member order is already
/// canonical and is preserved verbatim — E9 writer order).
pub fn workflow_v1() -> Policy {
    use ActorClass as A;
    use KindsSel as K;
    use Relation as R;
    use SpaceClassesSel as S;
    use Verdictname as V;
    let ep_obs = || K::Kinds(vec![Kind::Episode, Kind::Observation]);
    let personal_workflow = || S::Classes(vec![Spaceclass::Personal, Spaceclass::Workflow]);
    let workflow = || S::Classes(vec![Spaceclass::Workflow]);
    Policy {
        policy_id: "workflow-v1".into(),
        version: 1,
        rules: vec![
            rule(V::Accept, K::Wildcard, S::Wildcard, &[A::Owner], R::Any),
            rule(V::Retire, K::Wildcard, S::Wildcard, &[A::Owner], R::Any),
            rule(
                V::Dispute,
                K::Wildcard,
                S::Wildcard,
                &[A::Owner, A::SafeHuman],
                R::Any,
            ),
            rule(V::Retract, K::Wildcard, S::Wildcard, &[A::Owner], R::Any),
            rule(
                V::Retract,
                K::Wildcard,
                S::Wildcard,
                &[A::Peer, A::Session, A::External, A::SafeHuman],
                R::Author,
            ),
            rule(V::Supersede, K::Wildcard, S::Wildcard, &[A::Owner], R::Any),
            rule(
                V::Supersede,
                K::Wildcard,
                workflow(),
                &[A::Session],
                R::Author,
            ),
            rule(V::Declassify, K::Wildcard, S::Wildcard, &[A::Owner], R::Any),
            rule(
                V::RaiseClass,
                K::Wildcard,
                S::Wildcard,
                &[A::Owner, A::Session, A::SafeHuman],
                R::Any,
            ),
            rule(
                V::Accept,
                ep_obs(),
                personal_workflow(),
                &[A::SafeHuman],
                R::Any,
            ),
            rule(V::Accept, ep_obs(), workflow(), &[A::Session], R::SelfP),
            rule(
                V::Retire,
                ep_obs(),
                personal_workflow(),
                &[A::SafeHuman],
                R::Any,
            ),
        ],
    }
}

/// B.3 — the `owner-v1` status policy: every verdict, all kinds, all
/// space classes, owner only, relation any.
pub fn owner_v1() -> Policy {
    use ActorClass as A;
    use KindsSel as K;
    use Relation as R;
    use SpaceClassesSel as S;
    use Verdictname as V;
    let all = [
        V::Accept,
        V::Retire,
        V::Dispute,
        V::Retract,
        V::Supersede,
        V::Declassify,
        V::RaiseClass,
    ];
    Policy {
        policy_id: "owner-v1".into(),
        version: 1,
        rules: all
            .iter()
            .map(|v| rule(*v, K::Wildcard, S::Wildcard, &[A::Owner], R::Any))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cbor;
    use crate::shapes::{assert_pins, ToValue};
    use crate::vector::hex;

    const SPEC_PINS: &[&str] = &[
        // B.1 — the template's fixed fields.
        r#"`{ v: 1, zone_id: <genesis zone>, strictness: "strict",
deadline_fallback: "budgets",
require_cert_deadlines: false }` (no `time_witnesses` — a solo plane
has none; Connect time arrives only via explicit policy) — the
solo posture (D-28)."#,
        // B.2 — length + hash + the full literal.
        "**B.2 `workflow-v1`** — the literal canonical object (rules in
canonical set order; array members canonically sorted). Deterministic
CBOR encoding: 1133 bytes;",
        r#"H_policy(workflow-v1) =
  219b9baced57e8fdc06f56119e25dd02403cc4076cec04448e4957d0fa91dd1c"#,
        r#"{ v: 1, policy_id: "workflow-v1", version: 1, rules: [
  { verdict: accept, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: retire, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: dispute, kinds: "*", space_classes: "*",
    actor_classes: [owner, safe-human], relation: any }
  { verdict: retract, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: retract, kinds: "*", space_classes: "*",
    actor_classes: [peer, session, external, safe-human], relation: author }
  { verdict: supersede, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: supersede, kinds: "*", space_classes: [workflow],
    actor_classes: [session], relation: author }
  { verdict: declassify, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: raise_class, kinds: "*", space_classes: "*",
    actor_classes: [owner, session, safe-human], relation: any }
  { verdict: accept, kinds: [episode, observation], space_classes: [personal, workflow],
    actor_classes: [safe-human], relation: any }
  { verdict: accept, kinds: [episode, observation], space_classes: [workflow],
    actor_classes: [session], relation: self }
  { verdict: retire, kinds: [episode, observation], space_classes: [personal, workflow],
    actor_classes: [safe-human], relation: any }
] }"#,
        // B.3 — length + hash + the literal.
        "**B.3 `owner-v1`** — every verdict, all kinds, all space classes,
owner only, relation any. Canonical encoding: 571 bytes;",
        r#"H_policy(owner-v1) =
  d7d5559a6c3462426cb63eed84b05c569f2571da0cc6b5009edc79910f1e4486"#,
        r#"{ v: 1, policy_id: "owner-v1", version: 1, rules: [
  { verdict: accept, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: retire, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: dispute, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: retract, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: supersede, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: declassify, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
  { verdict: raise_class, kinds: "*", space_classes: "*",
    actor_classes: [owner], relation: any }
] }"#,
    ];

    #[test]
    fn spec_pins_are_verbatim() {
        assert_pins(SPEC_PINS);
    }

    /// The B.2 KAT: exact canonical length and pinned H_policy.
    #[test]
    fn workflow_v1_pinned_bytes() {
        let p = workflow_v1();
        let enc = cbor::encode(&p.to_value()).unwrap();
        assert_eq!(enc.len(), 1133, "workflow-v1 canonical length");
        assert_eq!(
            hex(&p.hash()),
            "219b9baced57e8fdc06f56119e25dd02403cc4076cec04448e4957d0fa91dd1c"
        );
    }

    /// The B.3 KAT: exact canonical length and pinned H_policy.
    #[test]
    fn owner_v1_pinned_bytes() {
        let p = owner_v1();
        let enc = cbor::encode(&p.to_value()).unwrap();
        assert_eq!(enc.len(), 571, "owner-v1 canonical length");
        assert_eq!(
            hex(&p.hash()),
            "d7d5559a6c3462426cb63eed84b05c569f2571da0cc6b5009edc79910f1e4486"
        );
    }

    #[test]
    fn genesis_zone_policy_shape() {
        use crate::shapes::map_keys;
        let zp = genesis_zone_policy([0x5A; 16]);
        // The B.1 solo posture: exactly the four fixed fields + v +
        // zone_id, none of the optional members.
        assert_eq!(
            map_keys(&zp.to_value()),
            [
                "v",
                "zone_id",
                "strictness",
                "deadline_fallback",
                "require_cert_deadlines"
            ]
        );
    }
}

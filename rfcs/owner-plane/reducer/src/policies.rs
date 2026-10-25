//! The B.2/B.3 built-in status policies (§11.3, Appendix B) — the
//! reducer's OWN transcription of the spec literals, hash-pinned:
//! encoding each policy through the reducer's canonical writer must
//! reproduce the spec's pinned `H_policy` values (B.2 `workflow-v1`
//! 1133 B / 219b9bac…, B.3 `owner-v1` 571 B / d7d5559a…) — the
//! program's deepest cross-implementation byte target after the
//! scenario KATs.
//!
//! The §11.2 counting relation: a judgment counts toward status iff
//! SOME rule matches all five of (verdict, target.kind, target's
//! space_class, actor_class(judgment), relation(judgment, target)).

use crate::domains;
use crate::kat::{encode, Enc};

/// One transcribed rule (all vocab as wire strings).
pub struct Rule {
    pub verdict: &'static str,
    /// `None` = the `"*"` wildcard.
    pub kinds: Option<&'static [&'static str]>,
    pub space_classes: Option<&'static [&'static str]>,
    pub actor_classes: &'static [&'static str],
    pub relation: &'static str,
}

const EP_OBS: &[&str] = &["episode", "observation"];
const PERSONAL_WORKFLOW: &[&str] = &["personal", "workflow"];
const WORKFLOW: &[&str] = &["workflow"];

/// B.2 `workflow-v1`, transcribed from the Appendix B literal.
pub const WORKFLOW_V1: &[Rule] = &[
    Rule {
        verdict: "accept",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "retire",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "dispute",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner", "safe-human"],
        relation: "any",
    },
    Rule {
        verdict: "retract",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "retract",
        kinds: None,
        space_classes: None,
        actor_classes: &["peer", "session", "external", "safe-human"],
        relation: "author",
    },
    Rule {
        verdict: "supersede",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "supersede",
        kinds: None,
        space_classes: Some(WORKFLOW),
        actor_classes: &["session"],
        relation: "author",
    },
    Rule {
        verdict: "declassify",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "raise_class",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner", "session", "safe-human"],
        relation: "any",
    },
    Rule {
        verdict: "accept",
        kinds: Some(EP_OBS),
        space_classes: Some(PERSONAL_WORKFLOW),
        actor_classes: &["safe-human"],
        relation: "any",
    },
    Rule {
        verdict: "accept",
        kinds: Some(EP_OBS),
        space_classes: Some(WORKFLOW),
        actor_classes: &["session"],
        relation: "self",
    },
    Rule {
        verdict: "retire",
        kinds: Some(EP_OBS),
        space_classes: Some(PERSONAL_WORKFLOW),
        actor_classes: &["safe-human"],
        relation: "any",
    },
];

/// B.3 `owner-v1`: every verdict, owner only, relation any.
pub const OWNER_V1: &[Rule] = &[
    Rule {
        verdict: "accept",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "retire",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "dispute",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "retract",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "supersede",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "declassify",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
    Rule {
        verdict: "raise_class",
        kinds: None,
        space_classes: None,
        actor_classes: &["owner"],
        relation: "any",
    },
];

/// The rules table for a bound policy id.
pub fn rules_for(policy_id: &str) -> Option<&'static [Rule]> {
    match policy_id {
        "workflow-v1" => Some(WORKFLOW_V1),
        "owner-v1" => Some(OWNER_V1),
        _ => None,
    }
}

fn sel(items: Option<&'static [&'static str]>) -> Enc {
    match items {
        None => Enc::T("*"),
        Some(xs) => Enc::A(xs.iter().map(|x| Enc::T(x)).collect()),
    }
}

fn rule_enc(r: &Rule) -> Enc {
    Enc::M(vec![
        ("verdict", Enc::T(r.verdict)),
        ("kinds", sel(r.kinds)),
        ("space_classes", sel(r.space_classes)),
        (
            "actor_classes",
            Enc::A(r.actor_classes.iter().map(|a| Enc::T(a)).collect()),
        ),
        ("relation", Enc::T(r.relation)),
    ])
}

/// Canonical policy bytes: `{v, policy_id, version, rules}` with the
/// rules array in sorted-set order (E7 default key). Hand-assembled
/// in the canonical key order — by ENCODED key bytes:
/// `v` (0x61…) < `rules` (0x65…) < `version` (0x67…) <
/// `policy_id` (0x69…).
pub fn policy_bytes(policy_id: &'static str, rules: &[Rule]) -> Vec<u8> {
    let mut encoded: Vec<Vec<u8>> = rules.iter().map(|r| encode(&rule_enc(r))).collect();
    encoded.sort();
    let mut out = vec![0xa4];
    out.extend_from_slice(&encode(&Enc::T("v")));
    out.extend_from_slice(&encode(&Enc::U(1)));
    out.extend_from_slice(&encode(&Enc::T("rules")));
    out.push(0x80 | rules.len() as u8);
    for r in encoded {
        out.extend_from_slice(&r);
    }
    out.extend_from_slice(&encode(&Enc::T("version")));
    out.extend_from_slice(&encode(&Enc::U(1)));
    out.extend_from_slice(&encode(&Enc::T("policy_id")));
    out.extend_from_slice(&encode(&Enc::T(policy_id)));
    out
}

/// `H_policy` of a bound policy id.
pub fn policy_hash(policy_id: &str) -> Option<[u8; 32]> {
    let (id, rules): (&'static str, &[Rule]) = match policy_id {
        "workflow-v1" => ("workflow-v1", WORKFLOW_V1),
        "owner-v1" => ("owner-v1", OWNER_V1),
        _ => return None,
    };
    Some(domains::h("policy", &policy_bytes(id, rules)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The spec-pinned byte lengths and H_policy values — the
    /// reducer's own writer reproduces both literals exactly.
    #[test]
    fn b2_b3_pins() {
        let b2 = policy_bytes("workflow-v1", WORKFLOW_V1);
        assert_eq!(b2.len(), 1133, "B.2 canonical length");
        assert_eq!(
            hex(&domains::h("policy", &b2)),
            "219b9baced57e8fdc06f56119e25dd02403cc4076cec04448e4957d0fa91dd1c"
        );
        let b3 = policy_bytes("owner-v1", OWNER_V1);
        assert_eq!(b3.len(), 571, "B.3 canonical length");
        assert_eq!(
            hex(&domains::h("policy", &b3)),
            "d7d5559a6c3462426cb63eed84b05c569f2571da0cc6b5009edc79910f1e4486"
        );
    }

    /// The sorted rules array is duplicate-free (E7 set).
    #[test]
    fn rules_are_sets() {
        for rules in [WORKFLOW_V1, OWNER_V1] {
            let mut enc: Vec<Vec<u8>> = rules.iter().map(|r| encode(&rule_enc(r))).collect();
            enc.sort();
            let n = enc.len();
            enc.dedup();
            assert_eq!(n, enc.len());
        }
    }
}

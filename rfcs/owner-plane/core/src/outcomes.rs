//! §10.4 closed outcome enums + the §10.5 disposition map.
//!
//! E10: every parse/validation FAILURE maps to one member of the
//! closed outcome enums and its disposition — these vocabularies
//! classify failures; an admitted operation carries no pair (the
//! companion's per-item rows encode admission as the ABSENT pair).
//!
//! §13.1 keeps `outcome`/`disposition` plain strings in the schemas
//! so there is no duplicated enum to drift; the cross-validation the
//! spec assigns to the harness is implemented here once and used at
//! mint time too (`vector::check`), pinned to the spec bytes below.

/// Every §10.4 outcome, in the spec's group order.
pub const OUTCOMES: &[&str] = &[
    // parse
    "malformed",
    "oversized",
    "depth",
    "non-canonical",
    "unknown-version",
    // cert
    "no-cert",
    "cert-revoked",
    "cert-superseded",
    "class-excluded",
    "key-malformed",
    // authz
    "proof-arm",
    "no-grant",
    "scope-tenant",
    "scope-zone",
    "scope-space",
    "scope-op",
    "scope-kind",
    "class-ceiling",
    "provenance-ceiling",
    "hosted-ceiling",
    "no-flow",
    "sig-invalid",
    // chain
    "fork",
    "gen-first-op",
    "lineage-gen",
    "cutoff",
    "capability-epoch",
    "epoch-unopened",
    "budget",
    "duplicate",
    "request-fork",
    "ctrl-fork",
    "recovery-competition",
    "ref-unresolved",
    "import-collision",
    // time
    "deadline-unreceipted",
    "lease-missing",
    "lease-stale",
    "issuer-fork",
    "issuer-gap",
    // body
    "body-hash",
    "op-unknown",
    "body-invariant",
    "causal-missing",
    "policy-missing",
    "source-erased",
    // storage
    "log-corrupt",
    "lock-denied",
    "storage-io",
    "wrapper-mismatch",
    "aead-fail",
    "storage-orphaned",
    // edge
    "no-session",
    "session-ended",
    "no-token",
    "token-scope",
    "token-revoked",
    "quota",
    "audit-unavailable",
];

/// The nine §10.5 dispositions, in row order.
pub const DISPOSITIONS: &[&str] = &[
    "reject-permanent",
    "pending-dependency",
    "quarantine-reproposal",
    "duplicate-idempotent",
    "freeze-control",
    "freeze-writer",
    "storage-quarantine",
    "storage-freeze",
    "edge-deny",
];

/// The §10.5 map: which dispositions an outcome may carry. Starred
/// outcomes (`unknown-version`, `no-cert`, `cert-revoked`,
/// `op-unknown`) are edge-deny live and reject-permanent in the fold;
/// `cert-superseded` splits by context (proven-incompatible =
/// reject-permanent, awaiting renewal chain = pending-dependency,
/// D-187). Everything else has exactly one lifecycle. Unknown
/// outcomes map to the empty slice.
pub fn dispositions_for(outcome: &str) -> &'static [&'static str] {
    match outcome {
        // parse
        "malformed" | "oversized" | "depth" | "non-canonical" => &["reject-permanent"],
        "unknown-version" => &["edge-deny", "reject-permanent"],
        // cert
        "no-cert" | "cert-revoked" => &["edge-deny", "reject-permanent"],
        "cert-superseded" => &["reject-permanent", "pending-dependency"],
        "class-excluded" | "key-malformed" => &["reject-permanent"],
        // authz
        "proof-arm" | "no-grant" | "scope-tenant" | "scope-zone" | "scope-space" | "scope-op"
        | "scope-kind" | "class-ceiling" | "provenance-ceiling" | "hosted-ceiling" | "no-flow"
        | "sig-invalid" => &["reject-permanent"],
        // chain
        "fork" => &["freeze-writer"],
        "gen-first-op" | "request-fork" => &["reject-permanent"],
        "lineage-gen" | "cutoff" | "capability-epoch" | "budget" | "import-collision" => {
            &["quarantine-reproposal"]
        }
        "epoch-unopened" | "ref-unresolved" => &["pending-dependency"],
        "duplicate" => &["duplicate-idempotent"],
        "ctrl-fork" | "recovery-competition" => &["freeze-control"],
        // time
        "deadline-unreceipted" | "lease-missing" | "issuer-gap" => &["pending-dependency"],
        "lease-stale" => &["quarantine-reproposal"],
        "issuer-fork" => &["freeze-writer"],
        // body
        "body-hash" | "body-invariant" | "source-erased" => &["reject-permanent"],
        "op-unknown" => &["edge-deny", "reject-permanent"],
        "causal-missing" | "policy-missing" => &["pending-dependency"],
        // storage
        "log-corrupt" | "wrapper-mismatch" | "aead-fail" | "storage-orphaned" => {
            &["storage-quarantine"]
        }
        "storage-io" => &["storage-freeze"],
        "lock-denied" => &["edge-deny"],
        // edge
        "no-session" | "session-ended" | "no-token" | "token-scope" | "token-revoked" | "quota"
        | "audit-unavailable" => &["edge-deny"],
        _ => &[],
    }
}

/// `(outcome, disposition)` is a legal §10.4×§10.5 pair.
pub fn valid_pair(outcome: &str, disposition: &str) -> bool {
    dispositions_for(outcome).contains(&disposition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapes::assert_pins;
    use std::collections::BTreeSet;

    /// Verbatim spec substrings: the whole §10.4 block, then the
    /// nine §10.5 rows (heads and/or outcome cells) and the starred
    /// footnote.
    const SPEC_PINS: &[&str] = &[
        // §10.4 — the complete closed enum block.
        r#"parse:   malformed, oversized, depth, non-canonical, unknown-version
cert:    no-cert, cert-revoked, cert-superseded,
         class-excluded, key-malformed
authz:   proof-arm, no-grant, scope-tenant, scope-zone, scope-space,
         scope-op, scope-kind, class-ceiling, provenance-ceiling,
         hosted-ceiling, no-flow, sig-invalid
chain:   fork, gen-first-op, lineage-gen, cutoff, capability-epoch,
         epoch-unopened, budget, duplicate, request-fork, ctrl-fork,
         recovery-competition, ref-unresolved, import-collision
time:    deadline-unreceipted, lease-missing, lease-stale,
         issuer-fork, issuer-gap
body:    body-hash, op-unknown, body-invariant, causal-missing,
         policy-missing, source-erased
storage: log-corrupt, lock-denied, storage-io, wrapper-mismatch,
         aead-fail, storage-orphaned
edge:    no-session, session-ended, no-token, token-scope,
         token-revoked, quota, audit-unavailable"#,
        // §10.5 — reject-permanent (the outcome cell's stable prefix).
        "| malformed, oversized, depth, non-canonical, sig-invalid, body-hash, body-invariant, key-malformed, proof-arm, no-grant, scope-*, class-*, provenance-ceiling, hosted-ceiling, no-flow, gen-first-op, request-fork (surfaced as fork evidence), source-erased",
        // §10.5 — pending-dependency (through the ref-unresolved entry).
        "| causal-missing, cert-superseded (awaiting renewal chain — the PROVEN-incompatible context is reject-permanent, D-187), policy-missing, deadline-unreceipted, lease-missing (§9.1), epoch-unopened (awaiting the epoch-opening control op — D-78), issuer-gap (awaiting the missing chain link — D-87), ref-unresolved (awaiting",
        // §10.5 — quarantine-reproposal (the full outcome cell).
        "| capability-epoch, cutoff, lease-stale, budget, lineage-gen, import-collision (D-196) |",
        // §10.5 — the four short rows, whole.
        "| **duplicate-idempotent** | duplicate (byte-identical) |",
        "| **freeze-control** | ctrl-fork (C2), recovery-competition (same-repoch, §7.4) |",
        "| **freeze-writer** | fork (tenant writer), issuer-fork (that issuer's feed) |",
        "| **storage-freeze** (local writer freezes; retry after remedy — transient, no rebuild) | storage-io |",
        // §10.5 — storage-quarantine (head + the cell's tail).
        "| **storage-quarantine** (local read-only + rebuild) | log-corrupt (incl. journal invariant violations:",
        "wrapper-mismatch, aead-fail, storage-orphaned (activation the surviving control state does not carry — D-104) |",
        // §10.5 — edge-deny (the full row).
        "| **edge-deny** (live request only; nothing replicated) | unknown-version*, no-cert*, cert-revoked*, op-unknown*, no-session, session-ended, no-token, token-scope, token-revoked, quota, lock-denied, audit-unavailable |",
        // The star footnote.
        "(*when raised at the edge; the same outcome inside the fold is
reject-permanent.)",
        // E10 — the failure framing the absent-pair convention rests on.
        "- **E10 (outcomes).** Every parse/validation failure maps to one member
  of the closed outcome enums (§10.4) **and** its disposition (§10.5).",
    ];

    #[test]
    fn spec_pins_are_verbatim() {
        assert_pins(SPEC_PINS);
    }

    #[test]
    fn inventory_closed_and_distinct() {
        assert_eq!(OUTCOMES.len(), 59);
        assert_eq!(OUTCOMES.iter().collect::<BTreeSet<_>>().len(), 59);
        assert_eq!(DISPOSITIONS.len(), 9);
        assert_eq!(DISPOSITIONS.iter().collect::<BTreeSet<_>>().len(), 9);
        for o in OUTCOMES {
            assert!(
                o.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
                "outcome not kebab: {o}"
            );
        }
    }

    #[test]
    fn map_is_total_and_within_vocabulary() {
        let mut used: BTreeSet<&str> = BTreeSet::new();
        for o in OUTCOMES {
            let ds = dispositions_for(o);
            assert!(!ds.is_empty(), "outcome without a disposition: {o}");
            for d in ds {
                assert!(DISPOSITIONS.contains(d), "unknown disposition: {d}");
                used.insert(d);
            }
        }
        // Every disposition is some outcome's lifecycle.
        assert_eq!(used.len(), DISPOSITIONS.len());
        // Unknown outcomes map to nothing.
        assert!(dispositions_for("accepted").is_empty());
        assert!(dispositions_for("").is_empty());
    }

    #[test]
    fn dual_lifecycle_outcomes_exact() {
        // The starred set + the D-187 context split, nothing else.
        let dual: Vec<&str> = OUTCOMES
            .iter()
            .copied()
            .filter(|o| dispositions_for(o).len() > 1)
            .collect();
        assert_eq!(
            dual,
            [
                "unknown-version",
                "no-cert",
                "cert-revoked",
                "cert-superseded",
                "op-unknown"
            ]
        );
        for o in ["unknown-version", "no-cert", "cert-revoked", "op-unknown"] {
            assert_eq!(dispositions_for(o), ["edge-deny", "reject-permanent"]);
        }
        assert_eq!(
            dispositions_for("cert-superseded"),
            ["reject-permanent", "pending-dependency"]
        );
    }

    #[test]
    fn pair_validation() {
        assert!(valid_pair("malformed", "reject-permanent"));
        assert!(valid_pair("ref-unresolved", "pending-dependency"));
        assert!(valid_pair("cutoff", "quarantine-reproposal"));
        assert!(valid_pair("duplicate", "duplicate-idempotent"));
        assert!(valid_pair("ctrl-fork", "freeze-control"));
        assert!(valid_pair("fork", "freeze-writer"));
        assert!(valid_pair("log-corrupt", "storage-quarantine"));
        assert!(valid_pair("storage-io", "storage-freeze"));
        assert!(valid_pair("quota", "edge-deny"));
        assert!(valid_pair("no-cert", "edge-deny"));
        assert!(valid_pair("no-cert", "reject-permanent"));

        assert!(!valid_pair("malformed", "pending-dependency"));
        assert!(!valid_pair("fork", "freeze-control"));
        assert!(!valid_pair("duplicate", "reject-permanent"));
        assert!(!valid_pair("accepted", "reject-permanent"));
        assert!(!valid_pair("malformed", "accepted"));
    }
}

//! §10.4 outcomes × §10.5 dispositions — the reducer's own table,
//! organized as the flat pair relation (a deliberately different
//! representation from the reference core's match-based map; both
//! pin to the same spec text, neither to each other).

/// Every legal `(outcome, disposition)` pair. Starred outcomes
/// (`unknown-version`, `no-cert`, `cert-revoked`, `op-unknown`) are
/// edge-deny live and reject-permanent in the fold; `cert-superseded`
/// splits by context (D-187).
pub const PAIRS: &[(&str, &str)] = &[
    // parse
    ("malformed", "reject-permanent"),
    ("oversized", "reject-permanent"),
    ("depth", "reject-permanent"),
    ("non-canonical", "reject-permanent"),
    ("unknown-version", "edge-deny"),
    ("unknown-version", "reject-permanent"),
    // cert
    ("no-cert", "edge-deny"),
    ("no-cert", "reject-permanent"),
    ("cert-revoked", "edge-deny"),
    ("cert-revoked", "reject-permanent"),
    ("cert-superseded", "reject-permanent"),
    ("cert-superseded", "pending-dependency"),
    ("class-excluded", "reject-permanent"),
    ("key-malformed", "reject-permanent"),
    // authz
    ("proof-arm", "reject-permanent"),
    ("no-grant", "reject-permanent"),
    ("scope-tenant", "reject-permanent"),
    ("scope-zone", "reject-permanent"),
    ("scope-space", "reject-permanent"),
    ("scope-op", "reject-permanent"),
    ("scope-kind", "reject-permanent"),
    ("class-ceiling", "reject-permanent"),
    ("provenance-ceiling", "reject-permanent"),
    ("hosted-ceiling", "reject-permanent"),
    ("no-flow", "reject-permanent"),
    ("sig-invalid", "reject-permanent"),
    // chain
    ("fork", "freeze-writer"),
    ("gen-first-op", "reject-permanent"),
    ("lineage-gen", "quarantine-reproposal"),
    ("cutoff", "quarantine-reproposal"),
    ("capability-epoch", "quarantine-reproposal"),
    ("epoch-unopened", "pending-dependency"),
    ("budget", "quarantine-reproposal"),
    ("duplicate", "duplicate-idempotent"),
    ("request-fork", "reject-permanent"),
    ("ctrl-fork", "freeze-control"),
    ("recovery-competition", "freeze-control"),
    ("ref-unresolved", "pending-dependency"),
    ("import-collision", "quarantine-reproposal"),
    // time
    ("deadline-unreceipted", "pending-dependency"),
    ("lease-missing", "pending-dependency"),
    ("lease-stale", "quarantine-reproposal"),
    ("issuer-fork", "freeze-writer"),
    ("issuer-gap", "pending-dependency"),
    // body
    ("body-hash", "reject-permanent"),
    ("op-unknown", "edge-deny"),
    ("op-unknown", "reject-permanent"),
    ("body-invariant", "reject-permanent"),
    ("causal-missing", "pending-dependency"),
    ("policy-missing", "pending-dependency"),
    ("source-erased", "reject-permanent"),
    // storage
    ("log-corrupt", "storage-quarantine"),
    ("lock-denied", "edge-deny"),
    ("storage-io", "storage-freeze"),
    ("wrapper-mismatch", "storage-quarantine"),
    ("aead-fail", "storage-quarantine"),
    ("storage-orphaned", "storage-quarantine"),
    // edge
    ("no-session", "edge-deny"),
    ("session-ended", "edge-deny"),
    ("no-token", "edge-deny"),
    ("token-scope", "edge-deny"),
    ("token-revoked", "edge-deny"),
    ("quota", "edge-deny"),
    ("audit-unavailable", "edge-deny"),
];

pub fn valid_pair(outcome: &str, disposition: &str) -> bool {
    PAIRS.contains(&(outcome, disposition))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn table_shape() {
        // 59 distinct outcomes across the pairs; 9 dispositions.
        let outs: BTreeSet<&str> = PAIRS.iter().map(|(o, _)| *o).collect();
        assert_eq!(outs.len(), 59);
        let disps: BTreeSet<&str> = PAIRS.iter().map(|(_, d)| *d).collect();
        assert_eq!(disps.len(), 9);
        // No duplicate pairs.
        assert_eq!(PAIRS.iter().collect::<BTreeSet<_>>().len(), PAIRS.len());
        // Exactly five dual-lifecycle outcomes.
        let dual: Vec<&str> = outs
            .iter()
            .copied()
            .filter(|o| PAIRS.iter().filter(|(po, _)| po == o).count() > 1)
            .collect();
        assert_eq!(
            dual,
            [
                "cert-revoked",
                "cert-superseded",
                "no-cert",
                "op-unknown",
                "unknown-version"
            ]
        );
    }

    /// Pin the §10.4 enum block verbatim (the reducer reads the spec
    /// directly — pinning against the SPEC is not sharing core code).
    #[test]
    fn spec_block_pinned() {
        let spec = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("owner-plane-d0a-spec.md"),
        )
        .unwrap();
        let block = r#"parse:   malformed, oversized, depth, non-canonical, unknown-version
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
         token-revoked, quota, audit-unavailable"#;
        assert!(spec.contains(block), "§10.4 block drifted");
        // Every name in the block appears in the table and vice versa.
        let block_names: BTreeSet<String> = block
            .split(&[':', ',', '\n', ' '][..])
            .filter(|w| !w.is_empty())
            .filter(|w| {
                ![
                    "parse", "cert", "authz", "chain", "time", "body", "storage", "edge",
                ]
                .contains(w)
            })
            .map(|w| w.to_string())
            .collect();
        let table_names: BTreeSet<String> = PAIRS.iter().map(|(o, _)| o.to_string()).collect();
        assert_eq!(block_names, table_names);
    }
}

//! §13.2 surface matrix — the family → Gate-A required-surface sets,
//! pinned to the spec table and enforced over every minted vector.
//!
//! The rule: a vector's `surfaces` array is a NON-EMPTY subset of
//! its family's R-set — a fixture never claims a lane its family
//! does not require, and full-set coverage is the norm. Two
//! deliberate subsets exist: family-3 sign-then-verify cases exclude
//! `browser` (WebCrypto cannot inject signing randomness — the
//! companion's in-schema guard), and family-14's
//! `offline-confirmation` documentation fixture runs on `core`
//! alone.

/// The §13.2 "Family × required surfaces" R-columns; `storage
/// per-OS` expands to the three `storage-*` names.
pub fn family_surfaces(family: u8) -> &'static [&'static str] {
    match family {
        1 | 2 => &["core", "browser"],
        3 | 4 => &["native-crypto", "browser"],
        5 => &["core", "native-crypto", "browser"],
        6 | 7 | 9 | 10 | 11 | 12 => &["core"],
        8 => &["native-crypto", "browser"],
        13 => &[
            "browser",
            "storage-macos",
            "storage-linux",
            "storage-windows",
        ],
        14 => &["core", "storage-macos", "storage-linux", "storage-windows"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The §13.2 table rows, verbatim — a spec edit to the matrix
    /// fails this suite before it ships as drift.
    const MATRIX_PINS: &[&str] = &[
        "| 1 encoding/caps | R | — | R | — |",
        "| 2 domains/key-ids | R | — | R | — |",
        "| 3 signatures | — | R | R | — |",
        "| 4 HPKE | — | R | R | — |",
        "| 5 item crypto | R | R | R | — |",
        "| 6 frontier | R | — | — | — |",
        "| 7 control fold | R | — | — | — |",
        "| 8 recovery derivation | — | R | R | — |",
        "| 9 time/lease | R | — | — | — |",
        "| 10 lineage/budget | R | — | — | — |",
        "| 11 Memory fold | R | — | — | — |",
        "| 12 IAM | R | — | — | — |",
        "| 13 storage | — | — | R (IndexedDB Txn subset) | R |",
        "| 14 migration/projection | R | — | — | R |",
    ];

    #[test]
    fn matrix_rows_pinned_to_spec() {
        let spec = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("owner-plane-d0a-spec.md"),
        )
        .unwrap();
        for pin in MATRIX_PINS {
            assert!(spec.contains(pin), "matrix row drifted: {pin}");
        }
    }

    /// Every builder-minted vector's surfaces = a non-empty subset of
    /// its family's R-set.
    #[test]
    fn every_vector_within_its_family_surfaces() {
        let mut all = crate::tranche::tranche();
        all.extend(crate::corpus::corpus());
        all.extend(crate::corpus_fold::corpus_fold());
        all.extend(crate::corpus_recovery::corpus_recovery());
        all.extend(crate::corpus_edge::corpus_edge());
        all.extend(crate::corpus_migration::corpus_migration());
        all.extend(crate::corpus_status::corpus_status());
        all.extend(crate::corpus_erase::corpus_erase());
        all.extend(crate::corpus_time::corpus_time());
        all.extend(crate::corpus_ctrl::corpus_ctrl());
        all.extend(crate::corpus_budget::corpus_budget());
        all.extend(crate::corpus_audit::corpus_audit());
        assert_eq!(all.len(), 148, "the full vector inventory");
        for v in &all {
            let allowed = family_surfaces(v.family);
            assert!(!v.surfaces.is_empty(), "{}: empty surfaces", v.name);
            for s in &v.surfaces {
                assert!(
                    allowed.contains(&s.as_str()),
                    "{} (family {}): surface {s:?} outside the §13.2 R-set {allowed:?}",
                    v.name,
                    v.family
                );
            }
        }
    }
}

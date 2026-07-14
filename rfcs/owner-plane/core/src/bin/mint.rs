//! Mint the tranche + corpus vector files into `../vectors/`
//! (relative to the crate — i.e. `rfcs/owner-plane/vectors/`). Every emitted vector is
//! checked against the container rules and the companion vocabulary
//! before it is written; the `tranche::tests` drift gate then pins
//! the committed bytes to the builders.

use owner_plane_core::{corpus, corpus_edge, corpus_fold, corpus_recovery, tranche, vector};

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let dir = root.join("vectors");
    std::fs::create_dir_all(&dir).expect("create vectors dir");
    let companion: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json"))
            .expect("companion readable"),
    )
    .expect("companion parses");

    let mut all = tranche::tranche();
    all.extend(corpus::corpus());
    all.extend(corpus_fold::corpus_fold());
    all.extend(corpus_recovery::corpus_recovery());
    all.extend(corpus_edge::corpus_edge());
    for v in all {
        vector::check(&v.to_json(), &companion)
            .unwrap_or_else(|e| panic!("{} fails mint-time check: {e}", v.name));
        let path = dir.join(format!("f{:02}-{}.json", v.family, v.name));
        std::fs::write(&path, v.to_file_string()).expect("write vector");
        println!("minted {}", path.display());
    }
}

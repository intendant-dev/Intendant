//! Run the differential harness over the committed tranche and print
//! per-vector status. Exit nonzero if any STRUCTURAL layer fails
//! (semantics report as unimplemented while the engine is built).

use owner_plane_reducer::harness::{plane_root, run_all, SemStatus};

fn main() {
    let reports = match run_all(&plane_root().join("vectors")) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("harness setup failed: {e}");
            std::process::exit(2);
        }
    };
    let mut structural_failures = 0;
    for r in &reports {
        let s = |x: &Result<(), String>| if x.is_ok() { "ok" } else { "FAIL" };
        let sem = match &r.semantics {
            SemStatus::Unimplemented(why) => format!("unimplemented ({why})"),
            SemStatus::Pass => "PASS".to_string(),
            SemStatus::Fail(e) => format!("FAIL: {e}"),
        };
        println!(
            "f{:02} {:56} container={} companion={} pairs={} decode={} semantics={}",
            r.family,
            r.file,
            s(&r.container_ok),
            s(&r.companion_ok),
            s(&r.pairs_ok),
            s(&r.decode_ok),
            sem
        );
        if !r.structural_ok() {
            structural_failures += 1;
            for (label, res) in [
                ("container", &r.container_ok),
                ("companion", &r.companion_ok),
                ("pairs", &r.pairs_ok),
                ("decode", &r.decode_ok),
            ] {
                if let Err(e) = res {
                    eprintln!("  {label}: {e}");
                }
            }
        }
    }
    if structural_failures > 0 {
        eprintln!("{structural_failures} vector(s) failed structural layers");
        std::process::exit(1);
    }
}

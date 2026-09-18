//! Run the differential harness over the committed vectors and print
//! per-vector status. The gate is STRICT: any structural failure,
//! any semantic FAIL, and any Unimplemented committed vector exits
//! nonzero — a committed corpus is green or the gate is red.

use owner_plane_reducer::harness::{all_green, plane_root, run_all, SemStatus};

const USAGE: &str = "usage: harness [--help] [VECTORS_DIR]

Runs the D0-A differential harness over every committed vector
(default: ../vectors relative to the crate). Exits 0 only when every
vector passes all structural layers AND semantics; any FAIL or
Unimplemented committed vector exits 1 (setup errors exit 2).";

fn main() {
    let mut args = std::env::args().skip(1);
    let mut dir = plane_root().join("vectors");
    if let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown flag {other}\n{USAGE}");
                std::process::exit(2);
            }
            path => dir = path.into(),
        }
    }
    if args.next().is_some() {
        eprintln!("too many arguments\n{USAGE}");
        std::process::exit(2);
    }
    let reports = match run_all(&dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("harness setup failed: {e}");
            std::process::exit(2);
        }
    };
    // Review R8.1: an empty corpus is a setup error, never a green
    // gate — all_green([]) is vacuously true and a mistyped
    // directory would otherwise exit 0 silently.
    if reports.is_empty() {
        eprintln!("harness setup failed: no vectors in {}", dir.display());
        std::process::exit(2);
    }
    let mut structural_failures = 0;
    for r in &reports {
        let s = |x: &Result<(), String>| if x.is_ok() { "ok" } else { "FAIL" };
        let sem = match &r.semantics {
            SemStatus::Unimplemented(why) => format!("unimplemented ({why})"),
            SemStatus::Pass => "PASS".to_string(),
            SemStatus::Fail(e) => format!("FAIL: {e}"),
        };
        println!(
            "f{:02} {:56} container={} companion={} pairs={} decode={} convergence={} semantics={}",
            r.family,
            r.file,
            s(&r.container_ok),
            s(&r.companion_ok),
            s(&r.pairs_ok),
            s(&r.decode_ok),
            s(&r.convergence_ok),
            sem
        );
        if !r.structural_ok() {
            structural_failures += 1;
            for (label, res) in [
                ("container", &r.container_ok),
                ("companion", &r.companion_ok),
                ("pairs", &r.pairs_ok),
                ("decode", &r.decode_ok),
                ("convergence", &r.convergence_ok),
            ] {
                if let Err(e) = res {
                    eprintln!("  {label}: {e}");
                }
            }
        }
    }
    if structural_failures > 0 {
        eprintln!("{structural_failures} vector(s) failed structural layers");
    }
    if !all_green(&reports) {
        eprintln!(
            "GATE RED: structural failures, semantic FAILs, or Unimplemented vectors present"
        );
        std::process::exit(1);
    }
}

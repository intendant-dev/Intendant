//! Build script for the intendant binary.
//!
//! Checks whether the compiled WASM files in `static/wasm-web/` are older than
//! the Rust source in `crates/presence-web/src/`. If stale, attempts to rebuild
//! via `wasm-pack build`. If `wasm-pack` is not installed, emits a warning.

use std::path::Path;

fn main() {
    // Re-run if any presence-web source file changes.
    println!("cargo:rerun-if-changed=crates/presence-web/src/");
    println!("cargo:rerun-if-changed=static/wasm-web/presence_web_bg.wasm");

    let wasm_bin = Path::new("static/wasm-web/presence_web_bg.wasm");
    let src_dir = Path::new("crates/presence-web/src");

    if !wasm_bin.exists() || !src_dir.exists() {
        return; // Nothing to check
    }

    let wasm_modified = wasm_bin
        .metadata()
        .and_then(|m| m.modified())
        .ok();

    let src_modified = newest_in_dir(src_dir);

    let stale = match (wasm_modified, src_modified) {
        (Some(w), Some(s)) => s > w,
        _ => false,
    };

    if !stale {
        return;
    }

    println!("cargo:warning=WASM is stale: presence-web source is newer than static/wasm-web/presence_web_bg.wasm");
    println!("cargo:warning=Rebuild WASM: cd crates/presence-web && wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web");
}

/// Find the newest modification time among all files in a directory (recursive).
fn newest_in_dir(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(t) = newest_in_dir(&path) {
                    newest = Some(newest.map_or(t, |n: std::time::SystemTime| n.max(t)));
                }
            } else if let Ok(meta) = path.metadata() {
                if let Ok(modified) = meta.modified() {
                    newest = Some(newest.map_or(modified, |n: std::time::SystemTime| n.max(modified)));
                }
            }
        }
    }
    newest
}

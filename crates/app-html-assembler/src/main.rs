//! CLI entry point for the app.html assembler — the CI regen gate.
//!
//! From the repo root (or with the root as the sole argument):
//!
//! ```text
//! cargo run -p app-html-assembler --locked
//! git diff --exit-code static/app.html
//! ```
//!
//! The first command rewrites `static/app.html` from the fragments exactly
//! as `build.rs` does; the diff then fails if the committed artifact did not
//! match the committed fragments (hand-edit, forgotten regeneration, or
//! manifest drift).

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    match app_html_assembler::assemble(&root) {
        Ok(app_html_assembler::Outcome::NoFragments) => {
            println!(
                "no fragments under {} — nothing to assemble",
                app_html_assembler::FRAGMENT_DIR
            );
            ExitCode::SUCCESS
        }
        Ok(app_html_assembler::Outcome::Unchanged { fragments, bytes }) => {
            println!(
                "{} up to date ({fragments} fragments, {bytes} bytes)",
                app_html_assembler::OUTPUT
            );
            ExitCode::SUCCESS
        }
        Ok(app_html_assembler::Outcome::Written { fragments, bytes }) => {
            println!(
                "{} regenerated ({fragments} fragments, {bytes} bytes)",
                app_html_assembler::OUTPUT
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("app.html assembly failed: {err}");
            ExitCode::FAILURE
        }
    }
}

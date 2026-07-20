//! CLI discovery descriptor (F1.5): lets an UNSUPERVISED agent on this
//! machine find a controller binary to `ctl` with. App installs keep
//! `intendant` off PATH (the bundle binary is even named `intendant-bin`),
//! and `$INTENDANT` is injected only under supervision — so authorized
//! loopback callers could reach nothing. The daemon fixes that at boot by
//! recording where its own controller lives.
//!
//! Shape (stated choice: a plain-text path file plus a JSON sidecar — the
//! primary file stays readable from a shell one-liner without jq):
//!
//! - `<state root>/cli-path` — one line, the absolute controller path.
//! - `<state root>/cli-path.meta.json` — debug context: port, pid,
//!   version, write time. **No secrets, ever** — this file is world-open
//!   discovery data, and tokens/keys must never ride it.
//!
//! Written on **daemon boot only** (a gateway-serving controller start) —
//! never by ctl or other transient invocations — atomically
//! (temp + rename). Multi-daemon homes get last-booted-wins by design:
//! the descriptor resolves a *CLI binary*, and any controller binary can
//! ctl any daemon at the standard endpoints; the sidecar records the
//! writing daemon so misroutes are debuggable.

use std::path::Path;

pub(crate) const CLI_PATH_FILE: &str = "cli-path";
pub(crate) const CLI_META_FILE: &str = "cli-path.meta.json";

/// Write/refresh the descriptor under `state_root`. Failure is non-fatal
/// at the call site (discovery degrades to PATH / bare `intendant`).
pub(crate) fn write_boot_descriptor(state_root: &Path, port: u16) -> std::io::Result<()> {
    let controller = std::env::current_exe()?;
    std::fs::create_dir_all(state_root)?;
    let meta = serde_json::json!({
        "controller": controller.to_string_lossy(),
        "port": port,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "wrote_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });
    write_atomic(
        &state_root.join(CLI_PATH_FILE),
        format!("{}\n", controller.to_string_lossy()).as_bytes(),
    )?;
    write_atomic(
        &state_root.join(CLI_META_FILE),
        format!("{meta:#}\n").as_bytes(),
    )
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_writes_path_file_and_secretless_meta() {
        let dir = tempfile::tempdir().unwrap();
        write_boot_descriptor(dir.path(), 8765).unwrap();

        let path_line = std::fs::read_to_string(dir.path().join(CLI_PATH_FILE)).unwrap();
        let controller = std::env::current_exe().unwrap();
        assert_eq!(path_line.trim(), controller.to_string_lossy());
        assert!(
            path_line.ends_with('\n'),
            "shell one-liner friendliness: newline-terminated single line"
        );

        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(CLI_META_FILE)).unwrap())
                .unwrap();
        assert_eq!(meta["port"], 8765);
        assert_eq!(meta["pid"], std::process::id());
        assert_eq!(meta["controller"], controller.to_string_lossy().as_ref());
        // The no-secrets contract: exactly the declared fields, nothing else.
        let mut keys: Vec<&str> = meta
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["controller", "pid", "port", "version", "wrote_at_ms"],
            "descriptor meta grows only by deliberate review — never tokens"
        );

        // Refresh replaces atomically (no partial state, no stale tmp).
        write_boot_descriptor(dir.path(), 9000).unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(CLI_META_FILE)).unwrap())
                .unwrap();
        assert_eq!(meta["port"], 9000);
        assert!(!dir.path().join("cli-path.tmp").exists());
    }

    /// The F1.5 pin: every repo skill that shells to the Intendant CLI
    /// carries the canonical resolver preamble ($INTENDANT → PATH →
    /// descriptor → bare fallback), so a future skill cannot regress to
    /// the `${INTENDANT:-intendant}` shape that unsupervised agents can
    /// never resolve on app installs.
    #[test]
    fn repo_skills_that_invoke_the_cli_carry_the_canonical_resolver() {
        let skills_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
        let canonical = "INTENDANT=\"${INTENDANT:-$(command -v intendant || cat \"${INTENDANT_HOME:-$HOME/.intendant}/cli-path\" 2>/dev/null || echo intendant)}\"";
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&skills_root).expect("repo skills dir") {
            let skill = entry.unwrap().path().join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&skill) else {
                continue;
            };
            // CLI-invocation shapes only: the quoted-var call and the
            // legacy bare-fallback expansion. Plain `$INTENDANT_HOME`
            // state-root reads (log-search) are not CLI invocations.
            let invokes_cli = text.contains("\"$INTENDANT\"") || text.contains("${INTENDANT:-");
            if !invokes_cli {
                continue;
            }
            checked += 1;
            assert!(
                text.contains(canonical),
                "{} shells to the Intendant CLI without the canonical resolver preamble:\n{canonical}",
                skill.display()
            );
            assert!(
                !text.contains("${INTENDANT:-intendant}\" ctl"),
                "{} still uses the bare-fallback invocation shape",
                skill.display()
            );
        }
        assert!(
            checked >= 3,
            "expected the CLI-invoking skills to be found (layout moved?)"
        );
    }
}

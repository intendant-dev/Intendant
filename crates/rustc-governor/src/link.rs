//! Heavyweight-link classification: which governed invocations must also
//! serialize through the machine-global link slots (`permits.rs`).
//!
//! The measured failure this gate answers (2026-07-15, 24GB host with a
//! fixed 16GB guest): the three-permit ordinary ceiling held exactly —
//! no bypass, no leak — yet two concurrent FINAL LINKS (a debug
//! `intendant` link peaks ~2GiB linker RSS) drove the host compressor to
//! 10–11GB with sustained swap in both directions, and a third pushed
//! swap-out to ~124MiB/s. The causal probe was clean: two links = severe
//! churn, one link = recovering, zero = idle. Counting rustcs is not
//! enough when every slot holds a link; the gate serializes the links
//! themselves and leaves ordinary compiles alone.
//!
//! An invocation is a heavyweight link iff it EMITS A LINK ARTIFACT and
//! TARGETS A FINAL BINARY:
//!
//! - emits link: no `--emit` flag at all (rustc's effective default is
//!   to link), or any `--emit` list contains the `link` kind —
//!   `--emit=kinds`, split `--emit kinds`, and `kind=path` items all
//!   count;
//! - final binary: bare `--test` (the cargo unit-test shape, which
//!   carries no `--crate-type`), or `bin` among the `--crate-type`
//!   values (equals / space / comma-list / repeated forms all count).
//!
//! Deliberately NOT heavyweight, each pinned by a test:
//!
//! - **Build scripts.** cargo compiles every `build.rs` as
//!   `--crate-name build_script_<stem> --crate-type bin
//!   --emit=dep-info,link` — dozens of trivial KB-scale links per cold
//!   build that must not queue on the slot. The `build_script_` prefix
//!   is a cargo-wide convention, not a project name list (a machine-wide
//!   governor must not be brittle against one repo's crate names).
//! - **Library artifacts**: `lib`/`rlib`, `staticlib`, `dylib`,
//!   `proc-macro` — their `link` emit writes an archive/library, not a
//!   final binary working set.
//! - **`cdylib`**: a scoped POLICY decision for the current workload
//!   (the wasm-pack crates' cdylib links are small), NOT a claim that
//!   all cdylib links are — revisit with soak data if a heavy cdylib
//!   appears.
//!
//! Blanket bin/`--test` gating (no crate-name allowlist beyond the
//! build-script convention) is deliberate for the first soak: some
//! harmless small test links will serialize, and reliability comes
//! first — the gated log lines (crate, waits, runtime; `govlog.rs`) are
//! the data a future allowlist or weighting would be justified by.
//!
//! Known edges, accepted: bare `rustc main.rs` (no explicit crate-type)
//! DEFAULTS to a bin link inside rustc but carries no marker this
//! classifier accepts — cargo always passes `--crate-type`/`--test`,
//! hand-run one-offs are small, and never-surprise beats completeness.
//! `@argfile` indirection is not expanded (same limitation as probe.rs);
//! neither cargo nor sccache uses it for rustc. Probe-only invocations
//! never reach this classifier: main.rs runs the probe fast path first
//! (cargo's startup probe carries `--crate-type bin` and would otherwise
//! resemble a bin invocation).

/// What `classify` learned about a compile invocation.
pub(crate) struct Classified {
    /// Must the invocation hold a link slot (in addition to its ordinary
    /// permit)?
    pub(crate) heavy: bool,
    /// The `--crate-name` value, for the governed log lines (`None` on
    /// direct rustc invocations that carry none).
    pub(crate) crate_name: Option<String>,
}

pub(crate) fn classify(args: &[String]) -> Classified {
    let mut saw_test = false;
    let mut bin_crate_type = false;
    let mut saw_emit = false;
    let mut emit_link = false;
    let mut crate_name: Option<String> = None;
    for (i, arg) in args.iter().enumerate() {
        match arg.as_str() {
            "--test" => saw_test = true,
            "--crate-type" => {
                if let Some(list) = args.get(i + 1) {
                    bin_crate_type |= crate_type_list_has_bin(list);
                }
            }
            "--emit" => {
                saw_emit = true;
                if let Some(list) = args.get(i + 1) {
                    emit_link |= emit_list_has_link(list);
                }
            }
            "--crate-name" => {
                if let Some(name) = args.get(i + 1) {
                    crate_name = Some(name.clone());
                }
            }
            other => {
                if let Some(list) = other.strip_prefix("--crate-type=") {
                    bin_crate_type |= crate_type_list_has_bin(list);
                } else if let Some(list) = other.strip_prefix("--emit=") {
                    saw_emit = true;
                    emit_link |= emit_list_has_link(list);
                } else if let Some(name) = other.strip_prefix("--crate-name=") {
                    crate_name = Some(name.to_string());
                }
            }
        }
    }
    let emits_link = !saw_emit || emit_link;
    let final_binary = saw_test || bin_crate_type;
    let build_script = crate_name
        .as_deref()
        .is_some_and(|name| name.starts_with("build_script_"));
    Classified {
        heavy: emits_link && final_binary && !build_script,
        crate_name,
    }
}

/// `--crate-type` accepts comma lists (`bin,rlib`); any `bin` counts.
fn crate_type_list_has_bin(list: &str) -> bool {
    list.split(',').any(|t| t.trim() == "bin")
}

/// Each `--emit` item is `kind` or `kind=path`.
fn emit_list_has_link(list: &str) -> bool {
    list.split(',')
        .any(|item| item.split('=').next().unwrap_or("").trim() == "link")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn heavy(list: &[&str]) -> bool {
        classify(&args(list)).heavy
    }

    /// The production bin-link shape, verbatim off a cargo -v line
    /// (trimmed): this is the invocation the gate exists for.
    #[test]
    fn cargo_bin_link_is_heavy() {
        assert!(heavy(&[
            "--crate-name",
            "intendant",
            "--edition",
            "2021",
            "src/bin/caller/main.rs",
            "--crate-type",
            "bin",
            "--emit=dep-info,link",
            "-C",
            "debuginfo=line-tables-only",
            "--out-dir",
            "/w/target/debug/deps",
        ]));
    }

    /// The observed cargo unit-test shape: bare `--test`, NO --crate-type.
    #[test]
    fn cargo_test_binary_is_heavy() {
        assert!(heavy(&[
            "--crate-name",
            "intendant",
            "--edition",
            "2021",
            "src/bin/caller/main.rs",
            "--emit=dep-info,link",
            "--test",
            "--out-dir",
            "/w/target/debug/deps",
        ]));
    }

    #[test]
    fn emit_forms_all_count() {
        // Split --emit.
        assert!(heavy(&["--crate-type", "bin", "--emit", "dep-info,link"]));
        // kind=path items.
        assert!(heavy(&[
            "--crate-type",
            "bin",
            "--emit=link=/out/intendant"
        ]));
        assert!(heavy(&[
            "--crate-type",
            "bin",
            "--emit",
            "dep-info,link=/out/x",
        ]));
        // No --emit at all: rustc's effective default is to link.
        assert!(heavy(&["--crate-name", "x", "--crate-type", "bin", "x.rs"]));
        // --emit present but linkless: nothing to gate.
        assert!(!heavy(&["--crate-type", "bin", "--emit=metadata"]));
        assert!(!heavy(&["--crate-type", "bin", "--emit", "dep-info"]));
    }

    #[test]
    fn crate_type_forms_all_count() {
        assert!(heavy(&["--crate-type=bin", "--emit=link"]));
        assert!(heavy(&["--crate-type", "bin,rlib", "--emit=link"]));
        assert!(heavy(&["--crate-type=rlib,bin", "--emit=link"]));
        // Repeated flags accumulate.
        assert!(heavy(&[
            "--crate-type",
            "rlib",
            "--crate-type",
            "bin",
            "--emit=link",
        ]));
        assert!(!heavy(&["--crate-type", "rlib,cdylib", "--emit=link"]));
    }

    /// Build scripts are bin links by shape and exempt by convention —
    /// a cold build compiles dozens of these trivial KB-scale links.
    #[test]
    fn build_scripts_are_exempt() {
        assert!(!heavy(&[
            "--crate-name",
            "build_script_build",
            "--edition",
            "2021",
            "build.rs",
            "--crate-type",
            "bin",
            "--emit=dep-info,link",
        ]));
        // `build = "src/main.rs"` style names too: the prefix is the
        // cargo convention, not the one common filename.
        assert!(!heavy(&[
            "--crate-name=build_script_main",
            "--crate-type",
            "bin",
            "--emit=dep-info,link",
        ]));
        // A --test build never carries the build-script name; the
        // exemption must not leak past the prefix.
        assert!(heavy(&["--crate-name", "build_scripts", "--test"]));
    }

    /// Library artifacts: pinned non-heavy, one per crate type. cdylib is
    /// a scoped policy decision (current wasm workload), not a general
    /// smallness claim.
    #[test]
    fn library_crate_types_are_not_heavy() {
        for ct in ["lib", "rlib", "staticlib", "dylib", "cdylib", "proc-macro"] {
            assert!(
                !heavy(&[
                    "--crate-name",
                    "x",
                    "--crate-type",
                    ct,
                    "--emit=dep-info,link"
                ]),
                "--crate-type {ct} must not classify heavyweight"
            );
        }
        // The wasm-pack shape specifically.
        assert!(!heavy(&[
            "--crate-name",
            "presence_web",
            "--crate-type",
            "cdylib",
            "--emit=dep-info,link",
            "--target",
            "wasm32-unknown-unknown",
        ]));
        // The cargo rlib shape sccache caches (tests/sccache_chain.rs).
        assert!(!heavy(&[
            "--crate-name",
            "prime",
            "--crate-type",
            "lib",
            "--emit=dep-info,link",
            "--out-dir",
            "/tmp/out",
        ]));
    }

    #[test]
    fn crate_name_is_extracted_for_logging() {
        let c = classify(&args(&["--crate-name", "intendant", "--test"]));
        assert_eq!(c.crate_name.as_deref(), Some("intendant"));
        let c = classify(&args(&["--crate-name=serde", "--crate-type", "rlib"]));
        assert_eq!(c.crate_name.as_deref(), Some("serde"));
        assert!(classify(&args(&["main.rs"])).crate_name.is_none());
    }

    /// No explicit marker, no gating: bare invocations (rustc would
    /// default them to bin) and empty argv stay ordinary — cargo always
    /// passes --crate-type or --test.
    #[test]
    fn unmarked_invocations_are_ordinary() {
        assert!(!heavy(&[]));
        assert!(!heavy(&["main.rs"]));
        assert!(!heavy(&["--crate-name", "x", "main.rs", "-o", "x"]));
    }
}

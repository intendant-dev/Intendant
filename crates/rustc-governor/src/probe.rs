//! Probe fast path: classify invocations that bypass the permit pool AND
//! the `wrap_with` chain — a probe execs the real compiler (argv[1])
//! directly, so it neither waits on permits nor depends on a healthy
//! sccache server.
//!
//! cargo probes the compiler at every startup (`rustc -vV`, and the target
//! probe `rustc - --crate-name ___ --print=file-names --print=cfg …`);
//! none of those may queue behind a full permit pool or cargo startup
//! wedges whenever the box is busy. (sccache's own compiler-identification
//! probes no longer pass through the governor at all: sccache sits behind
//! it in the chain and probes argv[1] itself.) An invocation is probe-only
//! iff it *cannot* compile anything:
//!
//! - it asks for the version (`-vV`, `-V`, `--version`), rustc prints and
//!   exits regardless of other flags; or
//! - it carries a `--print` request AND no codegen request — no `-o` /
//!   `--out-dir`, and no `--emit` kind other than `dep-info` (the design
//!   names link/metadata/obj as codegen; treating every non-`dep-info`
//!   kind — asm, llvm-ir, … — as codegen is the same rule extended in the
//!   conservative direction: never bypass something that does real work).
//!
//! A real compilation that merely carries `--print` (e.g.
//! `--print native-static-libs --emit link`) is NOT probe-only.
//!
//! Known limitation, accepted: `@argfile` indirection is not expanded.
//! Without an explicit `--print`/version flag on the command line there is
//! no bypass at all, so an argfile can only ever cause a probe to be
//! governed (harmless), never a compile to slip past the pool — unless a
//! caller mixes an explicit `--print` with codegen flags hidden in an
//! argfile, which neither cargo nor sccache does.

/// `--emit` kinds that do not make the invocation a compile.
const NON_CODEGEN_EMIT_KINDS: &[&str] = &["dep-info"];

pub fn is_probe_only(args: &[String]) -> bool {
    let mut saw_print = false;
    let mut saw_codegen = false;
    for (i, arg) in args.iter().enumerate() {
        match arg.as_str() {
            "-vV" | "-V" | "--version" => return true,
            "--print" => saw_print = true,
            "-o" | "--out-dir" => saw_codegen = true,
            "--emit" => {
                if let Some(list) = args.get(i + 1) {
                    if emit_has_codegen(list) {
                        saw_codegen = true;
                    }
                }
            }
            other => {
                if other.starts_with("--print=") {
                    saw_print = true;
                } else if let Some(list) = other.strip_prefix("--emit=") {
                    if emit_has_codegen(list) {
                        saw_codegen = true;
                    }
                } else if other.starts_with("--out-dir=") {
                    saw_codegen = true;
                } else if other.starts_with("-o") && !other.starts_with("--") {
                    // rustc's only short flag spelled `-o…` is -o itself
                    // (attached-value form, `-ofile`); -O is a different
                    // flag and case-sensitive.
                    saw_codegen = true;
                }
            }
        }
    }
    saw_print && !saw_codegen
}

fn emit_has_codegen(list: &str) -> bool {
    list.split(',').any(|item| {
        // Each item is `kind` or `kind=path`.
        let kind = item.split('=').next().unwrap_or(item).trim();
        !kind.is_empty() && !NON_CODEGEN_EMIT_KINDS.contains(&kind)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn version_probes_bypass() {
        assert!(is_probe_only(&args(&["-vV"])));
        assert!(is_probe_only(&args(&["--version"])));
        assert!(is_probe_only(&args(&["--version", "--verbose"])));
        assert!(is_probe_only(&args(&["-V"])));
    }

    #[test]
    fn cargo_target_info_probe_bypasses() {
        // The shape cargo runs on startup: stdin input, several --print
        // requests, crate-type flags, no codegen.
        assert!(is_probe_only(&args(&[
            "-",
            "--crate-name",
            "___",
            "--print=file-names",
            "--crate-type",
            "bin",
            "--crate-type",
            "rlib",
            "--print=sysroot",
            "--print=split-debuginfo",
            "--print=crate-name",
            "--print=cfg",
        ])));
        assert!(is_probe_only(&args(&["--print", "cfg"])));
        // dep-info emission alone is not codegen.
        assert!(is_probe_only(&args(&["--print=cfg", "--emit=dep-info"])));
    }

    #[test]
    fn compiles_carrying_print_are_governed() {
        assert!(!is_probe_only(&args(&[
            "--print",
            "native-static-libs",
            "--emit",
            "link",
            "src/main.rs",
        ])));
        assert!(!is_probe_only(&args(&[
            "--print=native-static-libs",
            "--emit=dep-info,link",
            "src/main.rs",
        ])));
        assert!(!is_probe_only(&args(&["--print=cfg", "--emit=metadata"])));
        assert!(!is_probe_only(&args(&["--print=sysroot", "-o", "out"])));
        assert!(!is_probe_only(&args(&["--print=sysroot", "-oout"])));
        assert!(!is_probe_only(&args(&["--print=cfg", "--out-dir", "d"])));
        assert!(!is_probe_only(&args(&["--print=cfg", "--out-dir=d"])));
        // Conservative extension: exotic emit kinds count as codegen too.
        assert!(!is_probe_only(&args(&["--print=cfg", "--emit=asm"])));
    }

    #[test]
    fn ordinary_compiles_are_governed() {
        assert!(!is_probe_only(&args(&[])));
        assert!(!is_probe_only(&args(&[
            "--crate-name",
            "foo",
            "--emit=dep-info,metadata,link",
            "-o",
            "foo.o",
            "src/lib.rs",
        ])));
        // No --print and no version flag: governed even without codegen
        // flags.
        assert!(!is_probe_only(&args(&[
            "--crate-name",
            "foo",
            "src/lib.rs"
        ])));
    }
}

//! The heavyweight-link phase shim.
//!
//! Cargo invokes the governor as a rustc wrapper. For a governed bin/test
//! invocation, the outer governor rewrites rustc's `-C linker=...` to point
//! back at this same executable and passes the real linker plus log context
//! in private environment variables. Rustc performs parsing, compilation,
//! and codegen under its ordinary compile permit; only when it invokes the
//! linker does this mode acquire the machine-wide link slot.
//!
//! Heavy final-artifact invocations are deliberately sent straight to
//! rustc rather than through `wrap_with`: sccache documents/observably
//! treats these link-producing bin/test shapes as non-cacheable, and a
//! long-lived sccache server is not a sound carrier for invocation-private
//! linker environment. Cacheable library invocations keep the normal
//! sccache chain.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Instant;

use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::process::CommandExt as _;

use crate::{config, govlog, permits};

const MODE_ENV: &str = "INTENDANT_GOVERNOR_LINKER_MODE_V1";
const REAL_LINKER_ENV: &str = "INTENDANT_GOVERNOR_REAL_LINKER_V1";
const CLASS_ENV: &str = "INTENDANT_GOVERNOR_LINK_CLASS_V1";
const CRATE_ENV: &str = "INTENDANT_GOVERNOR_LINK_CRATE_V1";
const PERMIT_ENV: &str = "INTENDANT_GOVERNOR_LINK_PERMIT_V1";
const PERMIT_WAIT_ENV: &str = "INTENDANT_GOVERNOR_LINK_PERMIT_WAIT_MS_V1";

const PRIVATE_ENVS: &[&str] = &[
    MODE_ENV,
    REAL_LINKER_ENV,
    CLASS_ENV,
    CRATE_ENV,
    PERMIT_ENV,
    PERMIT_WAIT_ENV,
];

pub(crate) fn is_linker_mode() -> bool {
    std::env::var_os(MODE_ENV).is_some_and(|value| value == "1")
}

pub(crate) struct PreparedRustc {
    pub(crate) args: Vec<OsString>,
    real_linker: OsString,
}

/// Preserve an explicitly configured linker, or the native Unix default
/// (`cc`) when rustc has no explicit `--target` or linker-flavor override.
/// Otherwise rustc's target/configuration may select `rust-lld`, `wasm-ld`,
/// `link.exe`, or another driver that cannot be recovered after overriding
/// `-C linker`; the caller keeps the safe whole-rustc gate for that
/// uncommon shape.
pub(crate) fn prepare_rustc(args: &[OsString]) -> Result<PreparedRustc, &'static str> {
    let mut rewritten = Vec::with_capacity(args.len() + 2);
    let mut real_linker: Option<OsString> = None;
    let mut explicit_target = false;
    let mut explicit_linker_flavor = false;
    let mut i = 0;
    while i < args.len() {
        let bytes = args[i].as_os_str().as_bytes();
        if bytes == b"-C" || bytes == b"--codegen" {
            if let Some(next) = args.get(i + 1) {
                let option = next.as_os_str().as_bytes();
                if let Some(linker) = option.strip_prefix(b"linker=") {
                    real_linker = Some(OsString::from_vec(linker.to_vec()));
                    i += 2;
                    continue;
                }
                explicit_linker_flavor |= option.starts_with(b"linker-flavor=");
            }
        } else if let Some(linker) = bytes
            .strip_prefix(b"-Clinker=")
            .or_else(|| bytes.strip_prefix(b"--codegen=linker="))
        {
            real_linker = Some(OsString::from_vec(linker.to_vec()));
            i += 1;
            continue;
        } else if bytes.starts_with(b"-Clinker-flavor=")
            || bytes.starts_with(b"--codegen=linker-flavor=")
        {
            explicit_linker_flavor = true;
        } else if bytes == b"--target" || bytes.starts_with(b"--target=") {
            explicit_target = true;
        }
        rewritten.push(args[i].clone());
        i += 1;
    }

    let real_linker = match real_linker {
        Some(linker) if !linker.is_empty() => linker,
        Some(_) => return Err("empty explicit linker"),
        None if explicit_target || explicit_linker_flavor => {
            return Err("configured default linker is unknown")
        }
        None => OsString::from("cc"),
    };
    let governor = std::env::current_exe().map_err(|_| "cannot resolve the governor executable")?;
    if paths_name_same(&real_linker, &governor) {
        return Err("the configured real linker points at rustc-governor");
    }
    rewritten.push(OsString::from("-C"));
    let mut linker_arg = OsString::from("linker=");
    linker_arg.push(&governor);
    rewritten.push(linker_arg);
    Ok(PreparedRustc {
        args: rewritten,
        real_linker,
    })
}

fn paths_name_same(candidate: &OsStr, governor: &Path) -> bool {
    let candidate_path = Path::new(candidate);
    if std::fs::canonicalize(candidate_path)
        .ok()
        .is_some_and(|path| std::fs::canonicalize(governor).ok().as_ref() == Some(&path))
    {
        return true;
    }
    candidate_path == governor
}

pub(crate) struct CompileContext<'a> {
    pub(crate) config_path: &'a Path,
    pub(crate) class: &'a str,
    pub(crate) crate_name: Option<&'a str>,
    pub(crate) permit_name: &'a str,
    pub(crate) permit_wait_ms: u64,
}

pub(crate) fn spawn_rustc(
    real_rustc: &Path,
    prepared: &PreparedRustc,
    context: CompileContext<'_>,
) -> std::io::Result<Child> {
    let mut command = Command::new(real_rustc);
    command
        .args(&prepared.args)
        .env(MODE_ENV, "1")
        .env(REAL_LINKER_ENV, &prepared.real_linker)
        .env("INTENDANT_GOVERNOR_CONFIG", context.config_path)
        .env(CLASS_ENV, context.class)
        .env(CRATE_ENV, context.crate_name.unwrap_or("-"))
        .env(PERMIT_ENV, context.permit_name)
        .env(PERMIT_WAIT_ENV, context.permit_wait_ms.to_string());
    command.spawn()
}

fn private_context() -> (String, Option<String>, String, u64) {
    let class = std::env::var(CLASS_ENV).unwrap_or_else(|_| "-".to_string());
    let crate_name = std::env::var(CRATE_ENV).ok().filter(|name| name != "-");
    let permit = std::env::var(PERMIT_ENV).unwrap_or_else(|_| "-".to_string());
    let permit_wait_ms = std::env::var(PERMIT_WAIT_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    (class, crate_name, permit, permit_wait_ms)
}

fn real_linker_command(real_linker: &OsStr, args: &[OsString]) -> Command {
    let mut command = Command::new(real_linker);
    command.args(args);
    for key in PRIVATE_ENVS {
        command.env_remove(key);
    }
    command
}

fn exec_real_linker(real_linker: &OsStr, args: &[OsString]) -> ! {
    let err = real_linker_command(real_linker, args).exec();
    eprintln!(
        "rustc-governor: failed to exec real linker {}: {err}",
        Path::new(real_linker).display()
    );
    std::process::exit(127);
}

/// Entry point when rustc invokes this binary as its linker.
pub(crate) fn run() -> ! {
    let Some(real_linker) = std::env::var_os(REAL_LINKER_ENV) else {
        eprintln!("rustc-governor: linker mode is missing its real-linker handoff");
        std::process::exit(127);
    };
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let governor = std::env::current_exe().ok();
    if governor
        .as_deref()
        .is_some_and(|path| paths_name_same(&real_linker, path))
    {
        eprintln!("rustc-governor: refusing recursive linker-shim execution");
        std::process::exit(127);
    }

    let config_path = config::config_path();
    let Some(cfg) = config::load(&config_path) else {
        exec_real_linker(&real_linker, &args);
    };
    if !cfg.enabled || std::env::var_os("INTENDANT_GOVERNOR").is_some_and(|value| value == "off") {
        exec_real_linker(&real_linker, &args);
    }

    let (class, crate_name, permit, permit_wait_ms) = private_context();
    let link_gate = permits::acquire_link_slot(&cfg, &config_path, crate_name.as_deref());
    let disposition = match &link_gate {
        permits::LinkGate::Held(slot) => govlog::LinkDisposition::Gated {
            slot: &slot.name,
            link_wait_ms: slot.wait_ms,
            queue: slot.queue.as_str(),
            scope: "linker",
        },
        permits::LinkGate::Off => govlog::LinkDisposition::Off,
        permits::LinkGate::Degraded => govlog::LinkDisposition::Degraded,
        permits::LinkGate::FailOpen => exec_real_linker(&real_linker, &args),
    };
    govlog::log_link(
        &cfg.permit_dir,
        &class,
        crate_name.as_deref(),
        &disposition,
        &permit,
        permit_wait_ms,
    );

    let started = Instant::now();
    let child = match real_linker_command(&real_linker, &args).spawn() {
        Ok(child) => child,
        Err(err) => {
            drop(link_gate);
            eprintln!(
                "rustc-governor: failed to run real linker {}: {err}",
                PathBuf::from(&real_linker).display()
            );
            std::process::exit(127);
        }
    };
    let status = crate::wait_for_child(child);
    // Release the high-RSS slot immediately when the linker is reaped,
    // before best-effort telemetry I/O.
    drop(link_gate);
    govlog::log_link_done(
        &cfg.permit_dir,
        crate_name.as_deref(),
        started.elapsed().as_millis() as u64,
        "linker",
    );
    match status {
        Ok(status) => crate::exit_like_child(status),
        Err(err) => {
            eprintln!("rustc-governor: failed to wait for the real linker: {err}");
            std::process::exit(127);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn strings(values: &[OsString]) -> Vec<String> {
        values
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn split_linker_is_preserved_and_replaced_by_the_shim() {
        let prepared = prepare_rustc(&args(&[
            "--crate-name",
            "x",
            "-C",
            "linker=/opt/toolchain/cc",
            "-C",
            "opt-level=3",
        ]))
        .unwrap();
        assert_eq!(prepared.real_linker, OsString::from("/opt/toolchain/cc"));
        let rewritten = strings(&prepared.args);
        assert!(!rewritten
            .iter()
            .any(|arg| arg == "linker=/opt/toolchain/cc"));
        assert!(rewritten.iter().any(|arg| arg == "opt-level=3"));
        assert!(rewritten.last().unwrap().starts_with("linker="));
    }

    #[test]
    fn compact_linkers_use_the_last_rustc_value() {
        let prepared = prepare_rustc(&args(&[
            "-Clinker=first",
            "--codegen=linker=second",
            "--test",
        ]))
        .unwrap();
        assert_eq!(prepared.real_linker, OsString::from("second"));
        assert_eq!(
            strings(&prepared.args)
                .iter()
                .filter(|arg| arg.contains("linker="))
                .count(),
            1
        );
    }

    #[test]
    fn native_default_is_cc_but_unknown_target_defaults_are_not_guessed() {
        assert_eq!(
            prepare_rustc(&args(&["--test"])).unwrap().real_linker,
            OsString::from("cc")
        );
        assert_eq!(
            prepare_rustc(&args(&["--test", "--target", "wasm32-wasip2"]))
                .err()
                .unwrap(),
            "configured default linker is unknown"
        );
        assert_eq!(
            prepare_rustc(&args(&["--target=x86_64-unknown-linux-gnu", "--test"]))
                .err()
                .unwrap(),
            "configured default linker is unknown"
        );
        assert_eq!(
            prepare_rustc(&args(&["--test", "--codegen", "linker-flavor=ld.lld"]))
                .err()
                .unwrap(),
            "configured default linker is unknown"
        );
    }

    #[test]
    fn long_codegen_linker_form_is_preserved_and_rewritten() {
        let prepared =
            prepare_rustc(&args(&["--test", "--codegen", "linker=/opt/cross/clang"])).unwrap();
        assert_eq!(prepared.real_linker, OsString::from("/opt/cross/clang"));
        assert_eq!(
            strings(&prepared.args)
                .iter()
                .filter(|arg| arg.contains("linker="))
                .count(),
            1
        );
    }
}

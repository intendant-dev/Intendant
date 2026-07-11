//! rustc-governor — the machine-wide rustc concurrency governor.
//!
//! Wired as cargo's `[build] rustc` while `rustc-wrapper` stays sccache, so
//! the chain is: cargo → sccache client → sccache server → THIS BINARY →
//! exec(2) of the real rustc. sccache treats the governor as the compiler:
//! cache hits never execute it (hits never wait); on a miss / non-cacheable
//! invocation the sccache server spawns it, it acquires a machine-wide
//! compile permit (flock(2) files shared by every account on the box), and
//! then replaces itself with the real rustc via exec — exit status and
//! signal disposition are inherited by construction, and the permit's flock
//! rides the FD_CLOEXEC-cleared fd until that rustc exits, however it exits.
//!
//! Doctrine, in order:
//!   1. **Fail open.** Missing/unparseable config, `enabled = false`, the
//!      secondary `INTENDANT_GOVERNOR=off` env override, an unusable permit
//!      dir, or zero configured permits ⇒ skip permits entirely and exec the
//!      real rustc. A governor must never break a build. The config file is
//!      re-read by every invocation (and once per poll tick by in-flight
//!      waiters), which is what makes it the live kill switch — no listener
//!      restarts.
//!   2. **Probe fast path.** `-vV` / `-V` / `--version`, or a `--print`
//!      request with no codegen, bypasses permits (cargo and sccache issue
//!      these constantly; they must stay snappy under a full pool). See
//!      `probe.rs` for the exact classification.
//!   3. **Class detection.** The euid's username, matched against the
//!      config's `ci_users`, splits invocations into `ci` and `local`.
//!   4. **Permits.** Own-class reservation first, then borrow the other
//!      class's spare permits iff that class has no registered waiters,
//!      else register demand and poll — see `permits.rs` for the demand-gate
//!      protocol. Nothing is ever killed or signalled; borrowed permits
//!      return naturally when their holder exits.
//!
//! Config: `/usr/local/etc/intendant-governor.toml`
//! (`INTENDANT_GOVERNOR_CONFIG` overrides — how the acceptance tests point
//! the binary at hermetic tempdir rigs). Installer:
//! `scripts/ci/install-governor-macos.sh`. Operator doc:
//! `scripts/ci/README.md`, "Governor" section.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

// `config` is the one cross-platform module (the portable fallback below
// still honors `real_rustc`); the governor proper — flock permits, exec —
// is Unix-only, so the non-unix build deliberately leaves parts of the
// config unread.
#[cfg_attr(not(unix), allow(dead_code))]
mod config;
#[cfg(unix)]
mod flock;
#[cfg(unix)]
mod govlog;
#[cfg(unix)]
mod permits;
#[cfg(unix)]
mod probe;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let config_path = config::config_path();
    let cfg = config::load(&config_path);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let real = resolve_real_rustc(cfg.as_ref(), home.as_deref());
    guard_against_self_exec(&real);
    #[cfg(unix)]
    run_unix(args, cfg, &config_path, &real);
    #[cfg(not(unix))]
    run_portable(args, &real);
}

/// Resolution order: `real_rustc` from config; else `$HOME/.cargo/bin/rustc`
/// when present (the rustup proxy — preserves rust-toolchain.toml and
/// `+toolchain` resolution exactly as if cargo had invoked rustc itself);
/// else `rustc` from PATH.
fn resolve_real_rustc(cfg: Option<&config::Config>, home: Option<&Path>) -> PathBuf {
    if let Some(explicit) = cfg.and_then(|c| c.real_rustc.clone()) {
        return explicit;
    }
    if let Some(home) = home {
        let rustup_proxy = home.join(".cargo").join("bin").join("rustc");
        if rustup_proxy.is_file() {
            return rustup_proxy;
        }
    }
    PathBuf::from("rustc")
}

/// A misconfigured chain (`real_rustc` pointing back at the governor, or the
/// governor installed on PATH under the name `rustc`) would exec(2) itself
/// in a tight loop — a fork bomb wearing a compiler's clothes. Best-effort
/// (a bare `rustc` only canonicalizes when it names a cwd-relative file),
/// and two syscalls per invocation buy the guard.
fn guard_against_self_exec(real: &Path) {
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    let target = std::fs::canonicalize(real).ok();
    if me.is_some() && me == target {
        eprintln!(
            "rustc-governor: refusing to exec itself (the resolved real rustc is the governor \
             binary); fix the real_rustc / [build] rustc wiring"
        );
        std::process::exit(127);
    }
}

#[cfg(unix)]
fn run_unix(
    args: Vec<OsString>,
    cfg: Option<config::Config>,
    config_path: &Path,
    real: &Path,
) -> ! {
    use std::os::unix::process::CommandExt as _;

    let permit = governed_permit(&args, cfg, config_path);
    // exec(2): on success this process IS the real rustc — argv[1..] and the
    // environment pass through untouched, exit status and signal disposition
    // propagate by construction, and the permit's flock (if any) rides the
    // FD_CLOEXEC-cleared fd until that rustc exits. Any exit — clean, panic,
    // SIGKILL — releases the permit in the kernel.
    let err = Command::new(real).args(&args).exec();
    // Reachable only when exec failed (real rustc missing / not executable).
    drop(permit);
    eprintln!("rustc-governor: failed to exec {}: {err}", real.display());
    std::process::exit(127);
}

/// Decide whether this invocation is governed, and if so acquire its permit.
/// `None` always means "run ungoverned" — bypass and fail-open share one
/// path, because both must behave identically: exec the real rustc.
#[cfg(unix)]
fn governed_permit(
    args: &[OsString],
    cfg: Option<config::Config>,
    config_path: &Path,
) -> Option<permits::AcquiredPermit> {
    // Secondary, per-invocation kill switch (the config file is the primary
    // one). Any value other than "off" is ignored.
    if std::env::var_os("INTENDANT_GOVERNOR").is_some_and(|v| v == "off") {
        return None;
    }
    // Missing/unparseable config ⇒ fail open; `enabled = false` ⇒ the live
    // kill switch.
    let cfg = cfg?;
    if !cfg.enabled {
        return None;
    }
    // Probe fast path. Classification only ever inspects well-known ASCII
    // flags, so lossy UTF-8 conversion is fine — the exec passes the
    // original OsStrings through untouched.
    let lossy: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if probe::is_probe_only(&lossy) {
        return None;
    }
    let class = permits::classify(permits::current_username().as_deref(), &cfg);
    let permit = permits::acquire(&cfg, class, config_path)?;
    govlog::log_governed(
        &cfg.permit_dir,
        class.as_str(),
        &permit.name,
        permit.wait_ms,
    );
    // Rust's std opens every file O_CLOEXEC; the permit must survive the
    // exec (the flock IS the permit). If clearing fails, proceed anyway:
    // losing the permit at exec oversubscribes the box by one compile,
    // while failing the compile would break the build — and a governor
    // must never break a build.
    let _ = flock::clear_cloexec(&permit.file);
    Some(permit)
}

/// The governor is a Unix (macOS / Linux) tool — flock(2) permit pools and
/// exec(2) semantics don't map onto Windows, and it is never deployed
/// there. This fallback keeps the workspace building on every first-class
/// platform (repo policy): degrade gracefully by running the real compiler
/// ungoverned and propagating its exit code.
#[cfg(not(unix))]
fn run_portable(args: Vec<OsString>, real: &Path) -> ! {
    match Command::new(real).args(&args).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => {
            eprintln!("rustc-governor: failed to run {}: {err}", real.display());
            std::process::exit(127);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_config_real_rustc() {
        let cfg = config::Config {
            real_rustc: Some(PathBuf::from("/opt/toolchain/bin/rustc")),
            ..config::Config::default()
        };
        assert_eq!(
            resolve_real_rustc(Some(&cfg), None),
            PathBuf::from("/opt/toolchain/bin/rustc")
        );
    }

    #[test]
    fn resolve_falls_back_to_home_rustup_proxy_then_path() {
        let home = tempfile::tempdir().unwrap();
        // No ~/.cargo/bin/rustc yet: PATH fallback.
        assert_eq!(
            resolve_real_rustc(None, Some(home.path())),
            PathBuf::from("rustc")
        );
        // With the rustup proxy present, it wins.
        let bin = home.path().join(".cargo").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let proxy = bin.join("rustc");
        std::fs::write(&proxy, b"#!/bin/sh\n").unwrap();
        assert_eq!(resolve_real_rustc(None, Some(home.path())), proxy);
        // Config beats the proxy.
        let cfg = config::Config {
            real_rustc: Some(PathBuf::from("/x/rustc")),
            ..config::Config::default()
        };
        assert_eq!(
            resolve_real_rustc(Some(&cfg), Some(home.path())),
            PathBuf::from("/x/rustc")
        );
    }
}

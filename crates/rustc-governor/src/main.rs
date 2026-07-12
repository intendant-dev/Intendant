//! rustc-governor — the machine-wide rustc concurrency governor.
//!
//! Wired as cargo's `[build] rustc-wrapper` (RUSTC_WRAPPER), so cargo
//! invokes it as `rustc-governor <real-rustc> <args…>` and the chain is:
//!
//! ```text
//! cargo → THIS BINARY → [flock(2) permit] → exec(2) of
//!   `wrap_with <real-rustc> <args…>` (the blocking sccache client)
//!   → sccache server: hits answered from cache, misses compiled
//!     server-side
//! ```
//!
//! The permit is held by the exec'd sccache *client*, which blocks until
//! the server answers: at most N outstanding clients ⇒ at most N
//! server-side compiles, so the machine-wide ceiling holds transitively —
//! no matter which rustc binary the server resolves and runs. Exit status
//! and signal disposition are inherited by construction, and the permit's
//! flock rides the FD_CLOEXEC-cleared fd until the exec'd chain exits,
//! however it exits. A cache hit occupies its permit only for the client
//! round-trip (~tens of ms); hits queueing head-of-line behind
//! miss-saturated permits is an accepted trade (ceiling correctness over
//! warm-path latency).
//!
//! Why wrapper-side: the previous wiring (`[build] rustc = governor`,
//! `rustc-wrapper = sccache`) made sccache treat the governor as the
//! compiler — and was silently BYPASSED for cacheable misses. sccache
//! identifies rustup proxies by probing the compiler with `+stable -vV`;
//! the governor's probe fast path passed that through to the rustup
//! proxy, so sccache classified the governor AS a proxy, resolved the
//! underlying toolchain rustc, and had its server invoke that binary
//! directly — ungoverned. Wrapper-side, nothing reaches sccache without a
//! permit, so no compiler-identification cleverness can route around the
//! pool. Post-mortem: `scripts/ci/README.md`, "Governor" section.
//!
//! Doctrine, in order:
//!   1. **Fail open.** Missing/unparseable config, `enabled = false`, the
//!      secondary `INTENDANT_GOVERNOR=off` env override, an unusable permit
//!      dir, or zero configured permits ⇒ skip permits but still exec the
//!      `wrap_with` chain (the real compiler directly when `wrap_with` is
//!      unset or won't exec): a disabled governor behaves exactly like a
//!      plain sccache rustc-wrapper — caching is never dropped, and a
//!      governor must never break a build. The config file is re-read by
//!      every invocation (and once per poll tick by in-flight waiters),
//!      which is what makes it the live kill switch — no listener
//!      restarts.
//!   2. **Probe fast path.** `-vV` / `-V` / `--version`, or a `--print`
//!      request with no codegen, execs the real compiler (argv[1])
//!      directly — bypassing permits AND sccache. cargo issues these at
//!      every startup; they must stay snappy under a full pool, and they
//!      must not depend on a healthy sccache server (a real incident
//!      class). Classification runs over argv[2..]; see `probe.rs`.
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

// `config` and `probe` are cross-platform (the portable fallback below
// honors `wrap_with` and the probe fast path); the governor proper —
// flock permits, exec — is Unix-only, so the non-unix build deliberately
// leaves the permit-sizing parts of the config unread.
#[cfg_attr(not(unix), allow(dead_code))]
mod config;
#[cfg(unix)]
mod flock;
#[cfg(unix)]
mod govlog;
#[cfg(unix)]
mod permits;
mod probe;

fn main() {
    let mut argv = std::env::args_os().skip(1);
    // cargo's RUSTC_WRAPPER contract: argv[1] is the real compiler.
    let Some(real) = argv.next().map(PathBuf::from) else {
        eprintln!(
            "rustc-governor: no compiler to run (argv[1] is missing). This binary is a \
             cargo RUSTC_WRAPPER: wire it as `[build] rustc-wrapper = \"…/rustc-governor\"` \
             and cargo invokes it as `rustc-governor <real-rustc> <args…>`"
        );
        std::process::exit(127);
    };
    let args: Vec<OsString> = argv.collect();
    guard_against_self_exec(&real);

    // Probe fast path — before permits, before sccache, before even the
    // config read: probes exec the real compiler directly in every
    // governor state. Classification only ever inspects well-known ASCII
    // flags, so lossy UTF-8 conversion is fine — the execs below pass the
    // original OsStrings through untouched.
    let lossy: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if probe::is_probe_only(&lossy) {
        run_compiler_direct(&real, &args);
    }

    let config_path = config::config_path();
    let cfg = config::load(&config_path);
    let wrap = usable_wrap_with(cfg.as_ref());
    #[cfg(unix)]
    run_unix(&real, &args, wrap, cfg, &config_path);
    #[cfg(not(unix))]
    run_portable(&real, &args, wrap);
}

/// The `wrap_with` chain target for this invocation, iff usable: present
/// in a parsed config, non-empty (the parser normalizes `""` to unset),
/// and not this very binary — a `wrap_with` pointing back at the governor
/// would exec(2) an identical invocation forever. That misconfiguration
/// fails OPEN (warn and run the compiler directly, uncached) rather than
/// hard-erroring like the argv[1] guard below: it is config-file state,
/// and config-file problems must never break a build.
fn usable_wrap_with(cfg: Option<&config::Config>) -> Option<PathBuf> {
    let wrap = cfg?.wrap_with.clone()?;
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    if me.is_some() && me == std::fs::canonicalize(&wrap).ok() {
        eprintln!(
            "rustc-governor: wrap_with points at the governor itself; ignoring it and \
             running the compiler directly (uncached) — fix wrap_with in the governor config"
        );
        return None;
    }
    Some(wrap)
}

/// cargo hands the wrapper the real compiler as argv[1]; if that path is
/// the governor itself, the account's cargo config still carries the
/// legacy `[build] rustc = …rustc-governor` line alongside the new
/// `rustc-wrapper` wiring — exec'ing onward would loop (directly, or
/// laundered once through sccache). Loud exit 127 (cargo surfaces wrapper
/// failures immediately) instead of a fork bomb wearing a compiler's
/// clothes. Best-effort (a bare `rustc` only canonicalizes when it names
/// a cwd-relative file), and two syscalls per invocation buy the guard.
fn guard_against_self_exec(real: &Path) {
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    let target = std::fs::canonicalize(real).ok();
    if me.is_some() && me == target {
        eprintln!(
            "rustc-governor: refusing to exec itself (argv[1] — the compiler cargo passed — \
             is the governor binary); remove the legacy `[build] rustc = …rustc-governor` \
             line: the governor is wired via `rustc-wrapper` now"
        );
        std::process::exit(127);
    }
}

/// Exec (spawn on non-unix) the real compiler with argv passed through
/// untouched. Shared by the probe fast path — probes bypass permits and
/// sccache both — and the last-resort fallback when the wrap chain won't
/// exec.
fn run_compiler_direct(real: &Path, args: &[OsString]) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let err = Command::new(real).args(args).exec();
        eprintln!("rustc-governor: failed to exec {}: {err}", real.display());
        std::process::exit(127);
    }
    #[cfg(not(unix))]
    {
        match Command::new(real).args(args).status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(err) => {
                eprintln!("rustc-governor: failed to run {}: {err}", real.display());
                std::process::exit(127);
            }
        }
    }
}

#[cfg(unix)]
fn run_unix(
    real: &Path,
    args: &[OsString],
    wrap: Option<PathBuf>,
    cfg: Option<config::Config>,
    config_path: &Path,
) -> ! {
    use std::os::unix::process::CommandExt as _;

    let permit = governed_permit(cfg, config_path);
    // exec(2): on success this process IS the governed chain — the sccache
    // client when wrap_with is configured, else the real compiler. argv
    // and the environment pass through untouched, exit status and signal
    // disposition propagate by construction, and the permit's flock (if
    // any) rides the FD_CLOEXEC-cleared fd until the exec'd chain exits.
    // Any exit — clean, panic, SIGKILL — releases the permit in the
    // kernel.
    if let Some(wrap) = &wrap {
        let err = Command::new(wrap).arg(real).args(args).exec();
        // Reachable only when the wrap exec failed (missing / not
        // executable): fail open to the direct compiler — the permit is
        // still held, the build must not break; caching is what's lost.
        eprintln!(
            "rustc-governor: failed to exec wrap_with {}: {err}; running {} directly (uncached)",
            wrap.display(),
            real.display()
        );
    }
    let err = Command::new(real).args(args).exec();
    // Reachable only when exec failed (real compiler missing / not
    // executable).
    drop(permit);
    eprintln!("rustc-governor: failed to exec {}: {err}", real.display());
    std::process::exit(127);
}

/// Decide whether this invocation is governed, and if so acquire its
/// permit. `None` always means "run ungoverned" — every fail-open path
/// funnels here, and the caller execs the same `wrap_with` chain either
/// way: a disabled governor must be indistinguishable from a plain
/// sccache rustc-wrapper.
#[cfg(unix)]
fn governed_permit(
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
/// platform (repo policy): degrade gracefully with the same chain shape,
/// minus permits — spawn `wrap_with <real> <args…>` (the real compiler
/// directly when `wrap_with` is unset or won't run) and propagate the
/// exit code.
#[cfg(not(unix))]
fn run_portable(real: &Path, args: &[OsString], wrap: Option<PathBuf>) -> ! {
    if let Some(wrap) = &wrap {
        match Command::new(wrap).arg(real).args(args).status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(err) => eprintln!(
                "rustc-governor: failed to run wrap_with {}: {err}; running {} directly (uncached)",
                wrap.display(),
                real.display()
            ),
        }
    }
    run_compiler_direct(real, args);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_with_requires_a_parsed_config_key() {
        assert_eq!(usable_wrap_with(None), None);
        // A config without the key: the governor works without sccache.
        assert_eq!(usable_wrap_with(Some(&config::Config::default())), None);
    }

    #[test]
    fn wrap_with_passes_through_a_configured_path() {
        let cfg = config::Config {
            wrap_with: Some(PathBuf::from("/opt/homebrew/bin/sccache")),
            ..config::Config::default()
        };
        assert_eq!(
            usable_wrap_with(Some(&cfg)),
            Some(PathBuf::from("/opt/homebrew/bin/sccache"))
        );
    }

    #[test]
    fn wrap_with_pointing_at_this_binary_is_refused() {
        // In unit tests current_exe is the test binary — good enough to
        // prove the canonicalized self-comparison refuses the exec loop.
        let cfg = config::Config {
            wrap_with: Some(std::env::current_exe().unwrap()),
            ..config::Config::default()
        };
        assert_eq!(usable_wrap_with(Some(&cfg)), None);
    }
}

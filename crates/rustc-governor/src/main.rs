//! rustc-governor — the machine-wide rustc concurrency governor.
//!
//! Wired as cargo's `[build] rustc-wrapper` (RUSTC_WRAPPER), so cargo
//! invokes it as `rustc-governor <real-rustc> <args…>` and the chain is:
//!
//! ```text
//! cargo → THIS BINARY (acquires + HOLDS the flock(2) permit, waits) →
//!   spawn of `wrap_with <real-rustc> <args…>` (the blocking sccache
//!   client) → sccache server: hits answered from cache, misses
//!   compiled server-side
//! ```
//!
//! The permit is held by the governor itself, which stays alive as the
//! spawned chain's parent. The sccache *client* blocks until the server
//! answers: at most N outstanding clients ⇒ at most N server-side
//! compiles, so the machine-wide ceiling holds transitively — no matter
//! which rustc binary the server resolves and runs. The permit fd keeps
//! FD_CLOEXEC set, so no child — crucially, not even the sccache server
//! the client daemonizes when none is running — can inherit it: flock
//! belongs to the open file description, and a long-lived inheritor
//! keeps the permit held long after the compile exits (the 2026-07-12
//! production leak; post-mortem in `run_governed`'s doc and
//! `scripts/ci/README.md`). The child's exit code is propagated, its
//! signal death is re-raised so cargo observes the same disposition,
//! and TERM/INT/HUP are forwarded to the child while the governor
//! waits. A cache hit occupies its permit only for the client
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
use std::process::{Child, Command};
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};

// `config` and `probe` are cross-platform (the portable fallback below
// honors `wrap_with` and the probe fast path); the governor proper —
// flock permits, the permit-holding parent — is Unix-only, so the
// non-unix build deliberately leaves the permit-sizing parts of the
// config unread.
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
/// sccache both — and, on unix, the fail-open fallback when the wrap
/// chain won't exec.
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
    match governed_permit(cfg, config_path) {
        // Fail-open: no permit is held, so there is no fd whose lifetime
        // must outlast the chain — exec(2) keeps the zero-overhead shape
        // (this process image simply BECOMES the chain, and a disabled
        // governor stays indistinguishable from a plain sccache
        // rustc-wrapper).
        None => exec_wrap_chain(real, args, wrap.as_deref()),
        // Governed: the permit is parent-held — see `run_governed`.
        Some(permit) => run_governed(real, args, wrap.as_deref(), permit),
    }
}

/// Exec `wrap_with <real> <args…>`, falling back to the real compiler
/// when the wrap chain won't exec. Fail-open (permitless) invocations
/// only: on the governed path the governor must outlive the child to
/// keep holding the permit, so it never execs there.
#[cfg(unix)]
fn exec_wrap_chain(real: &Path, args: &[OsString], wrap: Option<&Path>) -> ! {
    use std::os::unix::process::CommandExt as _;
    if let Some(wrap) = wrap {
        let err = Command::new(wrap).arg(real).args(args).exec();
        // Reachable only when the wrap exec failed (missing / not
        // executable): fail open to the direct compiler — the build must
        // not break; caching is what's lost.
        eprintln!(
            "rustc-governor: failed to exec wrap_with {}: {err}; running {} directly (uncached)",
            wrap.display(),
            real.display()
        );
    }
    run_compiler_direct(real, args);
}

/// The governed path: the governor stays alive as the permit-owning
/// parent — spawn the chain, forward TERM/INT/HUP to it, wait, and exit
/// the way the child exited.
///
/// This shape (vs. exec(2)'ing the chain over a FD_CLOEXEC-cleared
/// permit fd, the original design) is the fix for the 2026-07-12
/// production leak: when no sccache server is running, the exec'd
/// client daemonizes one, and that long-lived server (ppid 1) inherited
/// the cleared fd — flock belongs to the open file description, so the
/// permit stayed held for the server's whole lifetime and a 3-permit
/// pool silently ran as 2 for hours. Parent-held, the fd never leaves
/// this process.
///
/// Crash semantics (accepted — same spirit as the exec design's
/// any-exit-releases story): SIGKILL on the governor releases the
/// permit in the kernel the instant the process dies (its fds close),
/// and the child orphans and finishes its current compile momentarily
/// ungoverned.
#[cfg(unix)]
fn run_governed(
    real: &Path,
    args: &[OsString],
    wrap: Option<&Path>,
    permit: permits::AcquiredPermit,
) -> ! {
    let Some(mut child) = spawn_chain(real, args, wrap) else {
        // Neither the wrap chain nor the compiler would spawn (messages
        // already printed by spawn_chain).
        drop(permit);
        std::process::exit(127);
    };
    // Between spawn and handler install, TERM/INT/HUP still hits the
    // default disposition: the governor dies, the kernel releases the
    // permit, the child orphans — the documented crash semantics, for a
    // few-instruction window.
    CHILD_PID.store(child.id() as i32, Ordering::Release);
    install_signal_forwarders();
    let status = child.wait();
    // The child is reaped: its pid is free for reuse, so the forwarders
    // must stop aiming at it before this process does anything else.
    CHILD_PID.store(0, Ordering::Release);
    // The permit was held for the child's whole run (the fd kept
    // O_CLOEXEC, so the child never saw it); releasing it now is
    // release-on-exit made explicit.
    drop(permit);
    match status {
        Ok(status) => exit_like_child(status),
        Err(err) => {
            eprintln!("rustc-governor: failed to wait for the governed chain: {err}");
            std::process::exit(127);
        }
    }
}

/// Spawn `wrap_with <real> <args…>` — the real compiler directly when
/// `wrap_with` is unset or won't spawn — with argv, environment, and
/// stdio all inherited untouched. `None` only when nothing would spawn
/// (both failures already reported on stderr).
fn spawn_chain(real: &Path, args: &[OsString], wrap: Option<&Path>) -> Option<Child> {
    if let Some(wrap) = wrap {
        match Command::new(wrap).arg(real).args(args).spawn() {
            Ok(child) => return Some(child),
            // Fail open to the direct compiler — the build must not
            // break; caching is what's lost.
            Err(err) => eprintln!(
                "rustc-governor: failed to run wrap_with {}: {err}; running {} directly (uncached)",
                wrap.display(),
                real.display()
            ),
        }
    }
    match Command::new(real).args(args).spawn() {
        Ok(child) => Some(child),
        Err(err) => {
            eprintln!("rustc-governor: failed to run {}: {err}", real.display());
            None
        }
    }
}

/// Pid of the governed child while the governor waits on it; 0 outside
/// that window. Read by the signal forwarders (pid_t is i32 on every
/// supported unix target).
#[cfg(unix)]
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// Async-signal-safe forwarder installed for SIGTERM/SIGINT/SIGHUP
/// while the governor waits: relay the same signal to the child and
/// return — the child's exit (not the handler) drives the governor's
/// exit, which then re-raises. Chosen over the self-pipe pattern
/// because the handler's entire job is one kill(2) — which is on the
/// async-signal-safe list — plus one atomic load; a pipe would add
/// machinery only to move that same kill out of the handler.
#[cfg(unix)]
extern "C" fn forward_to_child(sig: libc::c_int) {
    let pid = CHILD_PID.load(Ordering::Acquire);
    if pid > 0 {
        // SAFETY: kill(2) is async-signal-safe and touches no caller
        // memory. `pid` is the child this process spawned: until
        // run_governed's wait reaps it the pid cannot be recycled (a
        // dead child stays a zombie), and CHILD_PID is zeroed
        // immediately after that wait returns, so the residual window
        // in which a stale pid could be signalled is a few
        // instructions.
        unsafe {
            libc::kill(pid as libc::pid_t, sig);
        }
    }
}

/// Install `forward_to_child` for the forwarded signals. SA_RESTART so
/// the wait in `run_governed` resumes instead of surfacing EINTR (std
/// retries EINTR anyway; the flag just keeps the interruption out of
/// the hot path).
#[cfg(unix)]
fn install_signal_forwarders() {
    let handler: extern "C" fn(libc::c_int) = forward_to_child;
    // SAFETY: sigaction is a plain C struct for which all-zero bytes are
    // a valid value; every field consulted below is set explicitly.
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = handler as libc::sighandler_t;
    sa.sa_flags = libc::SA_RESTART;
    // SAFETY: `sa.sa_mask` is a live local; sigemptyset only writes it.
    unsafe { libc::sigemptyset(&mut sa.sa_mask) };
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: `sa` is fully initialized and the handler it installs
        // is async-signal-safe (see forward_to_child). A failed install
        // is tolerated: the signal keeps its default disposition, which
        // is the documented crash semantics.
        let _ = unsafe { libc::sigaction(sig, &sa, std::ptr::null_mut()) };
    }
}

/// Exit the way the governed child exited: its code when it exited;
/// its signal re-raised on ourselves when it died by one — after
/// restoring the default disposition — so cargo observes the same
/// signal death the exec design produced by construction. exit(128+N)
/// is the belt-and-braces fallback if the re-raise somehow returns.
#[cfg(unix)]
fn exit_like_child(status: std::process::ExitStatus) -> ! {
    use std::os::unix::process::ExitStatusExt as _;
    if let Some(sig) = status.signal() {
        // SAFETY: sigaction is a plain C struct for which all-zero bytes
        // are a valid value; SIG_DFL needs no other fields set.
        let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
        sa.sa_sigaction = libc::SIG_DFL;
        // SAFETY: `sa.sa_mask` is a live local; sigemptyset only writes
        // it.
        unsafe { libc::sigemptyset(&mut sa.sa_mask) };
        // SAFETY: restoring SIG_DFL is always sound; failure (e.g.
        // SIGKILL, which cannot be caught or reset) is tolerated because
        // raise(2) below delivers such signals regardless.
        let _ = unsafe { libc::sigaction(sig, &sa, std::ptr::null_mut()) };
        // SAFETY: sigset_t is a plain C type for which all-zero bytes
        // are a valid starting value; it is emptied and extended with
        // `sig` before use, and both calls only write `set`.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, sig);
        }
        // SAFETY: unblocking `sig` in our own mask; `set` is initialized
        // and this thread owns its signal mask (defensive — nothing here
        // blocks signals).
        let _ = unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) };
        // SAFETY: raise(2) sends `sig` to this thread; under the default
        // disposition of a fatal signal it does not return.
        let _ = unsafe { libc::raise(sig) };
        // The re-raise returned (the disposition could not be restored):
        // encode the signal the way shells do.
        std::process::exit(128 + sig);
    }
    std::process::exit(status.code().unwrap_or(1));
}

/// Decide whether this invocation is governed, and if so acquire its
/// permit. `None` always means "run ungoverned" — every fail-open path
/// funnels here, and the caller execs the same `wrap_with` chain,
/// permitless: a disabled governor must be indistinguishable from a
/// plain sccache rustc-wrapper. On `Some`, the permit fd deliberately
/// keeps std's FD_CLOEXEC: the caller stays alive as the permit-holding
/// parent, and the fd must be invisible to every child — a leaked fd in
/// a long-lived child (in production: the sccache server the client
/// daemonizes on demand) keeps the flock held long after the compile.
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
    Some(permit)
}

/// The governor is a Unix (macOS / Linux) tool — flock(2) permit pools
/// don't map onto Windows, and it is never deployed there. This fallback
/// keeps the workspace building on every first-class platform (repo
/// policy): degrade gracefully with the same chain shape, minus permits —
/// structurally the unix governed parent (spawn the chain, wait, exit
/// like the child) without the permit or the signal forwarding.
#[cfg(not(unix))]
fn run_portable(real: &Path, args: &[OsString], wrap: Option<PathBuf>) -> ! {
    let Some(mut child) = spawn_chain(real, args, wrap.as_deref()) else {
        std::process::exit(127);
    };
    match child.wait() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => {
            eprintln!("rustc-governor: failed to wait for the governed chain: {err}");
            std::process::exit(127);
        }
    }
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

//! The machine-wide compile-permit pool: flock(2) files with class
//! reservations and a demand gate.
//!
//! Layout inside `permit_dir` (names are minted by
//! `scripts/ci/install-governor-macos.sh` in production — non-root accounts
//! cannot create files in the root-owned dir — and created on the fly in
//! user-writable dirs such as test rigs; interlock: change the naming here
//! and in the installer together):
//!
//! - `permit-local-<i>`, i < local_reserved  — the interactive reservation
//! - `permit-ci-<i>`,    i < ci_reserved     — the CI reservation
//! - `demand-local`, `demand-ci`             — one demand file per class
//! - `link-<i>`,         i < link_slots      — the heavyweight-link slots
//!   (machine-global, classless: host memory doesn't care whose link it
//!   is, so there is no reservation split and no demand gate)
//! - `link-waiter-<i>`,  i < link_queue_slots — writable ticket files for
//!   the bounded, crash-safe FIFO in front of the link slots
//!
//! Protocol:
//! - Holding a permit = holding LOCK_EX on its file. The governor holds
//!   the lock itself for the governed chain's whole run — it stays alive
//!   as the chain's parent, and the fd keeps std's O_CLOEXEC so no child
//!   ever inherits it — and the kernel releases it when the governor
//!   exits, however it exits: crash release is structural.
//! - A waiter holds LOCK_SH on its OWN class's demand file for the whole
//!   wait, and releases it the moment it holds a permit.
//! - Borrowing: before touching a foreign-class permit, probe that class's
//!   demand file with LOCK_EX|LOCK_NB. Success (released immediately) means
//!   no waiters ⇒ the class's spare capacity may be borrowed; failure means
//!   the class has waiters ⇒ its reservation is honored. A failed probe can
//!   also mean another borrower's probe won the same instant — a transient,
//!   conservative false "has waiters" (we skip borrowing for one poll tick).
//! - A fresh arrival runs the same probe against its OWN class's demand
//!   file before its first grab: registered waiters mean it joins the
//!   queue — register, then poll sleeping-first — instead of racing the
//!   sleepers for a freed permit. Without it, a cargo's back-to-back
//!   compile stream regrabs the permit within milliseconds of each
//!   release and can starve a parked waiter for minutes.
//! - Waiting is a 100ms LOCK_EX|LOCK_NB poll over every eligible permit —
//!   never a blocking flock on a permit: blocking flock has no timeout, and
//!   parking inside the kernel on a foreign permit would bypass the demand
//!   gate for the rest of the wait.
//! - Nothing is ever killed or signalled; borrowed permits return naturally
//!   when their holder exits.
//!
//! The link slot is acquired by the linker shim, after rustc has completed
//! compilation and codegen. The outer governor continues to hold that
//! rustc's ordinary permit while the linker waits and runs. This cannot
//! form a cycle: every heavyweight takes resources in the same
//! permit-then-link order, and the process holding a link slot needs no
//! further governor resource before it can finish. It also means a queued
//! link may occupy a compile permit, but only for the duration of the
//! actual links ahead of it — not for another crate's entire compile and
//! codegen phase.
//!
//! FIFO is a fixed pool of root-minted, world-writable ticket files. A
//! waiter exclusively flocks one file, writes its monotonic arrival tuple,
//! and keeps the flock until it acquires a link slot. Scanners treat
//! unlocked files as free/stale, so SIGKILL releases a ticket structurally
//! with no cleanup daemon. The first `usable link slots` active tickets may
//! compete for slots. If no ticket file is usable (old install, bad
//! permissions), serialization remains active but ordering degrades to the
//! old unordered poll; `link_queue_slots = 0` explicitly chooses that mode.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{self, Config};
use crate::flock;

pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(100);
const LINK_WAIT_NOTICE: Duration = Duration::from_secs(5);
const COMPILE_WAIT_NOTICE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Local,
    Ci,
}

impl Class {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Class::Local => "local",
            Class::Ci => "ci",
        }
    }

    fn other(self) -> Class {
        match self {
            Class::Local => Class::Ci,
            Class::Ci => Class::Local,
        }
    }
}

/// Everything not listed in `ci_users` — including an unresolvable
/// username — is local (the design: CI is the explicit allowlist).
pub(crate) fn classify(username: Option<&str>, cfg: &Config) -> Class {
    match username {
        Some(user) if cfg.ci_users.iter().any(|ci| ci == user) => Class::Ci,
        _ => Class::Local,
    }
}

/// Username of the effective uid via getpwuid_r — deliberately not
/// `$USER`/`$LOGNAME`, which go stale under sudo/setuid and are trivially
/// spoofed into the wrong class.
pub(crate) fn current_username() -> Option<String> {
    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let mut buf = vec![0_u8; 256];
    loop {
        // SAFETY: `passwd` is a plain C struct for which all-zero bytes are
        // a valid (if meaningless) value; getpwuid_r fully initializes it
        // on success.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer references a live local; `buf` outlives the
        // call and its true length is passed alongside it.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            if buf.len() >= 64 * 1024 {
                return None;
            }
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        // SAFETY: on success `result` points at `pwd`, whose pw_name is a
        // NUL-terminated string inside `buf`; both are still live.
        let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
        return Some(name.to_string_lossy().into_owned());
    }
}

pub(crate) struct AcquiredPermit {
    /// Keeping this `File` open IS the permit — held for RAII alone,
    /// never read: the governor holds it, parent-side, for the governed
    /// chain's whole run — the fd keeps std's O_CLOEXEC so no child (the
    /// sccache client, or any server it daemonizes) can inherit it — and
    /// the kernel releases the flock when the `File` closes (drop, or
    /// any governor exit).
    pub(crate) _file: File,
    pub(crate) name: String,
    pub(crate) wait_ms: u64,
}

/// A held heavyweight-link slot. Exactly the `AcquiredPermit` story:
/// keeping the `File` open IS the slot, parent-held with O_CLOEXEC intact,
/// kernel-released on any exit.
pub(crate) struct AcquiredLinkSlot {
    pub(crate) _file: File,
    pub(crate) name: String,
    pub(crate) wait_ms: u64,
    pub(crate) queue: LinkQueueKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkQueueKind {
    Fifo,
    Disabled,
    Degraded,
}

impl LinkQueueKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LinkQueueKind::Fifo => "fifo",
            LinkQueueKind::Disabled => "disabled",
            LinkQueueKind::Degraded => "degraded",
        }
    }
}

/// Outcome of gating a heavyweight link.
pub(crate) enum LinkGate {
    /// Slot held: the link is serialized.
    Held(AcquiredLinkSlot),
    /// `link_slots = 0`: the gate is configured off on this box.
    Off,
    /// No slot file was usable (config grown past the installer-minted
    /// files in a root-owned dir, or the dir denies creation). Link
    /// gating degrades — the caller logs it and proceeds — but ordinary
    /// governance NEVER rides with it: only the global kill-switch paths
    /// drop the whole governor.
    Degraded,
    /// The config vanished or disabled mid-wait: the whole invocation
    /// fails open (the caller drops nothing — no slot was acquired).
    FailOpen,
}

fn permit_name(class: Class, i: u32) -> String {
    format!("permit-{}-{i}", class.as_str())
}

fn demand_name(class: Class) -> String {
    format!("demand-{}", class.as_str())
}

fn link_slot_name(i: u32) -> String {
    format!("link-{i}")
}

fn link_waiter_name(i: u32) -> String {
    format!("link-waiter-{i}")
}

/// Open (or, where the directory allows it, create) a lock file. flock(2)
/// only needs an open fd — read-only is enough against the root-owned 0644
/// files of a production install; the create fallback serves tempdir rigs
/// and any user-writable permit dir. Concurrent creators share the inode
/// (O_CREAT without O_EXCL), so their locks contend correctly.
fn open_lock_file(path: &Path) -> Option<File> {
    match OpenOptions::new().read(true).open(path) {
        Ok(f) => Some(f),
        Err(e) if e.kind() == io::ErrorKind::NotFound => OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .ok(),
        Err(_) => None,
    }
}

/// Queue tickets carry a small arrival record, so unlike ordinary lock
/// files they must be writable by every governed account. Production
/// assets are pre-created 0666 by the installer; creation is only a
/// convenience for user-writable test/diagnostic rigs.
fn open_queue_file(path: &Path) -> Option<File> {
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => Some(f),
        Err(e) if e.kind() == io::ErrorKind::NotFound => OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .ok(),
        Err(_) => None,
    }
}

/// Open whichever of the class's permit files are usable; the ones that
/// aren't (e.g. counts grown in config without the installer re-run that
/// mints the new root-owned files) are simply not part of the pool.
fn open_permits(dir: &Path, class: Class, count: u32) -> Vec<(String, File)> {
    (0..count)
        .filter_map(|i| {
            let name = permit_name(class, i);
            open_lock_file(&dir.join(&name)).map(|f| (name, f))
        })
        .collect()
}

/// First lockable permit, moved out of the pool.
fn try_take(pool: &mut Vec<(String, File)>) -> Option<(String, File)> {
    let idx = pool
        .iter()
        .position(|(_, f)| flock::try_lock_exclusive(f))?;
    Some(pool.swap_remove(idx))
}

/// Probe a class's demand gate. Success ⇒ no waiters are registered on
/// that class right now — a foreign borrower may take its spare permits,
/// and a fresh own-class arrival may grab ahead of the (empty) queue. A
/// failed probe can also mean another probe won the same instant — a
/// transient, conservative false "has waiters" costing one poll tick.
fn no_registered_waiters(demand: &File) -> bool {
    if flock::try_lock_exclusive(demand) {
        flock::unlock(demand);
        true
    } else {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TicketOrder {
    monotonic_ns: u128,
    pid: u32,
    index: u32,
}

struct QueueTicket {
    file: File,
    order: TicketOrder,
}

impl Drop for QueueTicket {
    fn drop(&mut self) {
        // Stale bytes are harmless (an unlocked file is inactive), but
        // truncating keeps diagnostics human-readable on graceful exits.
        let _ = self.file.set_len(0);
        flock::unlock(&self.file);
    }
}

enum QueueClaim {
    Claimed(QueueTicket),
    Full,
    Unusable,
}

enum QueueState {
    Fifo(QueueTicket),
    Disabled,
    Degraded,
}

impl QueueState {
    fn kind(&self) -> LinkQueueKind {
        match self {
            QueueState::Fifo(_) => LinkQueueKind::Fifo,
            QueueState::Disabled => LinkQueueKind::Disabled,
            QueueState::Degraded => LinkQueueKind::Degraded,
        }
    }
}

fn monotonic_ns() -> u128 {
    // CLOCK_MONOTONIC is machine-wide on both supported Unix families, so
    // independently spawned waiters can order their arrivals without a
    // mutable counter file. Fall back to wall time only if the OS call
    // unexpectedly fails.
    // SAFETY: `ts` is a live, writable timespec and clock_gettime writes
    // exactly that value; CLOCK_MONOTONIC needs no other precondition.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc == 0 {
        (ts.tv_sec.max(0) as u128) * 1_000_000_000 + ts.tv_nsec.max(0) as u128
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }
}

fn try_claim_queue_ticket(dir: &Path, count: u32) -> QueueClaim {
    let mut occupied = false;
    for index in 0..count {
        let Some(mut file) = open_queue_file(&dir.join(link_waiter_name(index))) else {
            continue;
        };
        if !flock::try_lock_exclusive(&file) {
            occupied = true;
            continue;
        }
        let order = TicketOrder {
            monotonic_ns: monotonic_ns(),
            pid: std::process::id(),
            index,
        };
        let record = format!("{} {} {}\n", order.monotonic_ns, order.pid, order.index);
        let wrote = file.set_len(0).is_ok()
            && file.seek(SeekFrom::Start(0)).is_ok()
            && file.write_all(record.as_bytes()).is_ok();
        if wrote {
            return QueueClaim::Claimed(QueueTicket { file, order });
        }
        flock::unlock(&file);
    }
    if occupied {
        QueueClaim::Full
    } else {
        QueueClaim::Unusable
    }
}

fn parse_ticket_record(text: &str, index: u32) -> TicketOrder {
    let mut fields = text.split_whitespace();
    let parsed = (|| {
        Some(TicketOrder {
            monotonic_ns: fields.next()?.parse().ok()?,
            pid: fields.next()?.parse().ok()?,
            index: fields.next()?.parse().ok()?,
        })
    })();
    // An active owner may be observed in the few instructions between its
    // flock and record write. Treat an empty/partial active record as the
    // oldest waiter, conservatively preventing a newcomer from overtaking.
    parsed.unwrap_or(TicketOrder {
        monotonic_ns: 0,
        pid: 0,
        index,
    })
}

fn active_ticket_rank(dir: &Path, count: u32, mine: TicketOrder) -> usize {
    let mut active = Vec::new();
    for index in 0..count {
        let Some(mut file) = open_queue_file(&dir.join(link_waiter_name(index))) else {
            continue;
        };
        if flock::try_lock_exclusive(&file) {
            // Free or crash-stale: it is not an active queue member.
            flock::unlock(&file);
            continue;
        }
        let mut record = String::new();
        let _ = file.seek(SeekFrom::Start(0));
        let _ = file.read_to_string(&mut record);
        active.push(parse_ticket_record(&record, index));
    }
    active.sort_unstable();
    active
        .iter()
        .position(|ticket| *ticket == mine)
        // Never let a missing/partially observed own record jump to the
        // front. A normal claimant writes before this scan, so MAX is only
        // a conservative one-tick response to a race (or external file
        // removal/corruption).
        .unwrap_or(usize::MAX)
}

fn live_config_enabled(config_path: &Path) -> bool {
    matches!(config::load(config_path), Some(live) if live.enabled)
}

/// Crate names come from argv: bound and sanitize them before the
/// one-line stderr diagnostics (`-` when absent or unusable).
fn stderr_crate(crate_name: Option<&str>) -> String {
    let name: String = crate_name
        .unwrap_or("-")
        .chars()
        .filter(|c| c.is_ascii_graphic())
        .take(64)
        .collect();
    if name.is_empty() {
        "-".to_string()
    } else {
        name
    }
}

fn maybe_report_link_wait(
    start: Instant,
    reported: &mut bool,
    crate_name: Option<&str>,
    queue: &str,
) {
    if !*reported && start.elapsed() >= LINK_WAIT_NOTICE {
        eprintln!(
            "rustc-governor: {} waiting for the machine-wide linker slot ({:.1}s, queue={queue})",
            stderr_crate(crate_name),
            start.elapsed().as_secs_f64(),
        );
        *reported = true;
    }
}

/// One concise stderr line after ten seconds parked in the compile-permit
/// queue. The threshold sits above the link notice's five: multi-second
/// permit waits are routine under concurrent agent builds, and the notice
/// exists for the starved outlier — a build that looks hung should
/// explain itself.
fn maybe_report_compile_wait(
    start: Instant,
    reported: &mut bool,
    crate_name: Option<&str>,
    class: Class,
) {
    if !*reported && start.elapsed() >= COMPILE_WAIT_NOTICE {
        eprintln!(
            "rustc-governor: {} waiting for a machine-wide compile permit ({:.1}s, class={})",
            stderr_crate(crate_name),
            start.elapsed().as_secs_f64(),
            class.as_str(),
        );
        *reported = true;
    }
}

/// Acquire one machine-global link slot for the actual linker process,
/// waiting as long as it takes. The outer governor already holds this
/// rustc invocation's ordinary permit; see the module-level ordering
/// proof. No class demand gate applies: link slots are machine-global.
/// The live kill switch is honored mid-wait exactly like the permit poll.
pub(crate) fn acquire_link_slot(
    cfg: &Config,
    config_path: &Path,
    crate_name: Option<&str>,
) -> LinkGate {
    if cfg.link_slots == 0 {
        return LinkGate::Off;
    }
    let dir = &cfg.permit_dir;
    let _ = fs::create_dir_all(dir);
    let mut slots: Vec<(String, File)> = (0..cfg.link_slots)
        .filter_map(|i| {
            let name = link_slot_name(i);
            open_lock_file(&dir.join(&name)).map(|f| (name, f))
        })
        .collect();
    if slots.is_empty() {
        return LinkGate::Degraded;
    }
    let start = Instant::now();
    let mut reported = false;
    let queue = if cfg.link_queue_slots == 0 {
        QueueState::Disabled
    } else {
        loop {
            match try_claim_queue_ticket(dir, cfg.link_queue_slots) {
                QueueClaim::Claimed(ticket) => break QueueState::Fifo(ticket),
                QueueClaim::Unusable => break QueueState::Degraded,
                QueueClaim::Full => {
                    if !live_config_enabled(config_path) {
                        return LinkGate::FailOpen;
                    }
                    maybe_report_link_wait(start, &mut reported, crate_name, "full");
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
    };
    loop {
        let may_take = match &queue {
            QueueState::Fifo(ticket) => {
                active_ticket_rank(dir, cfg.link_queue_slots, ticket.order) < slots.len()
            }
            QueueState::Disabled | QueueState::Degraded => true,
        };
        if may_take {
            if let Some((name, file)) = try_take(&mut slots) {
                let queue_kind = queue.kind();
                drop(queue);
                return LinkGate::Held(AcquiredLinkSlot {
                    _file: file,
                    name,
                    wait_ms: start.elapsed().as_millis() as u64,
                    queue: queue_kind,
                });
            }
        }
        if !live_config_enabled(config_path) {
            return LinkGate::FailOpen;
        }
        maybe_report_link_wait(start, &mut reported, crate_name, queue.kind().as_str());
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Acquire one machine-wide compile permit for `class`, waiting as long as
/// it takes. `None` always means FAIL OPEN (run ungoverned): zero configured
/// permits, an unusable permit dir, an unregisterable wait, or the kill
/// switch flipping mid-wait — never "denied". `crate_name` feeds the
/// long-wait stderr notice only.
pub(crate) fn acquire(
    cfg: &Config,
    class: Class,
    config_path: &Path,
    crate_name: Option<&str>,
) -> Option<AcquiredPermit> {
    if cfg.local_reserved == 0 && cfg.ci_reserved == 0 {
        return None;
    }
    let dir = &cfg.permit_dir;
    // Production dirs come from the installer; tempdir rigs (and any
    // user-writable permit_dir) are created on the fly. Failure is fine —
    // open_lock_file below decides whether anything is actually usable.
    let _ = fs::create_dir_all(dir);

    let (own_count, foreign_count) = match class {
        Class::Local => (cfg.local_reserved, cfg.ci_reserved),
        Class::Ci => (cfg.ci_reserved, cfg.local_reserved),
    };
    let mut own = open_permits(dir, class, own_count);
    let mut foreign = open_permits(dir, class.other(), foreign_count);
    if own.is_empty() && foreign.is_empty() {
        // Nothing lockable at all: fail open.
        return None;
    }

    let start = Instant::now();
    let acquired = |start: Instant, (name, file): (String, File)| {
        Some(AcquiredPermit {
            _file: file,
            name,
            wait_ms: start.elapsed().as_millis() as u64,
        })
    };

    // A registered queue outranks a fresh arrival: probe the own-class
    // demand gate — the same probe foreign borrowers use — and skip the
    // immediate grabs when waiters are parked on it. Without this, a
    // cargo's compile stream regrabbing the permit within milliseconds of
    // each release starves sleeping pollers (a waiter measured 194s parked
    // behind 20 overtakes, 2026-07-21). A failed probe can also be a
    // colliding probe: transient, conservative, costs one poll tick. An
    // unopenable own demand file cannot be probed — grab-first then,
    // matching the fail-open registration below.
    let own_demand = open_lock_file(&dir.join(demand_name(class)));
    let join_queue = own_demand
        .as_ref()
        .is_some_and(|gate| !no_registered_waiters(gate));

    // The other class's gate, opened up front: the immediate borrow and
    // the poll loop both consult it. An unopenable foreign demand file
    // means the gate cannot be probed: never borrow blind.
    let foreign_demand = open_lock_file(&dir.join(demand_name(class.other())));

    if !join_queue {
        // Own-class reservation first.
        if let Some(p) = try_take(&mut own) {
            return acquired(start, p);
        }
        // Then the other class's spare capacity, through its demand gate.
        if !foreign.is_empty() {
            if let Some(gate) = &foreign_demand {
                if no_registered_waiters(gate) {
                    if let Some(p) = try_take(&mut foreign) {
                        return acquired(start, p);
                    }
                }
            }
        }
    }

    // Nothing free (or the class has a queue this arrival must join):
    // register demand — LOCK_SH held for the entire wait so foreign
    // borrowers keep their hands off this class's reservation — then poll,
    // sleeping BEFORE each round: an immediate first retry would hand a
    // just-freed permit to the newest arrival ahead of every sleeping
    // waiter, reopening the overtake the demand probe above closes. If
    // demand can't be registered, fail open rather than wait unregistered:
    // borrowers couldn't see us, and we could starve behind them
    // indefinitely.
    let own_demand = own_demand?;
    if !flock::lock_shared_blocking(&own_demand) {
        return None;
    }
    let mut reported = false;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        if let Some(p) = try_take(&mut own) {
            flock::unlock(&own_demand);
            return acquired(start, p);
        }
        if !foreign.is_empty() {
            if let Some(gate) = &foreign_demand {
                if no_registered_waiters(gate) {
                    if let Some(p) = try_take(&mut foreign) {
                        flock::unlock(&own_demand);
                        return acquired(start, p);
                    }
                }
            }
        }
        // Live kill switch for in-flight waiters too, not just new
        // invocations: a flipped `enabled = false` (or a deleted config)
        // unwedges every governed process within one poll tick, fail-open.
        // Only `enabled` is honored mid-wait; counts/paths stay as loaded.
        match config::load(config_path) {
            Some(live) if live.enabled => {}
            _ => {
                flock::unlock(&own_demand);
                return None;
            }
        }
        maybe_report_compile_wait(start, &mut reported, crate_name, class);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rig(local: u32, ci: u32) -> (tempfile::TempDir, Config, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let permit_dir = tmp.path().join("permits");
        let cfg = Config {
            enabled: true,
            permit_dir: permit_dir.clone(),
            local_reserved: local,
            ci_reserved: ci,
            link_slots: 1,
            link_queue_slots: 8,
            ci_users: vec!["_intendant-ci".into(), "ci".into()],
            wrap_with: None,
        };
        let config_path = tmp.path().join("governor.toml");
        std::fs::write(
            &config_path,
            format!(
                "enabled = true\npermit_dir = \"{}\"\nlocal_reserved = {local}\nci_reserved = {ci}\n",
                permit_dir.display()
            ),
        )
        .unwrap();
        (tmp, cfg, config_path)
    }

    #[test]
    fn classify_matches_ci_users_and_defaults_local() {
        let cfg = Config::default();
        assert_eq!(classify(Some("_intendant-ci"), &cfg), Class::Ci);
        assert_eq!(classify(Some("ci"), &cfg), Class::Ci);
        assert_eq!(classify(Some("somebody"), &cfg), Class::Local);
        assert_eq!(classify(None, &cfg), Class::Local);
    }

    #[test]
    fn current_username_resolves() {
        let name = current_username().expect("euid must resolve to a user");
        assert!(!name.is_empty());
    }

    #[test]
    fn acquire_prefers_own_class_then_borrows_idle_foreign() {
        let (_tmp, cfg, path) = rig(1, 1);
        let a = acquire(&cfg, Class::Local, &path, None).unwrap();
        assert_eq!(a.name, "permit-local-0");
        // Own class exhausted, no CI demand registered: borrow.
        let b = acquire(&cfg, Class::Local, &path, None).unwrap();
        assert_eq!(b.name, "permit-ci-0");
        drop(a);
        // Own reservation is free again and preferred.
        let c = acquire(&cfg, Class::Local, &path, None).unwrap();
        assert_eq!(c.name, "permit-local-0");
    }

    #[test]
    fn zero_permits_fails_open() {
        let (_tmp, cfg, path) = rig(0, 0);
        assert!(acquire(&cfg, Class::Local, &path, None).is_none());
    }

    #[test]
    fn link_gate_off_and_degraded_never_block() {
        let (_tmp, mut cfg, path) = rig(1, 0);
        cfg.link_slots = 0;
        assert!(matches!(
            acquire_link_slot(&cfg, &path, None),
            LinkGate::Off
        ));
        // An unusable permit dir (here: a plain file where the dir should
        // be) means no slot file can be opened or created: Degraded, so
        // the caller keeps ordinary governance instead of failing open.
        cfg.link_slots = 1;
        cfg.permit_dir = path.clone();
        assert!(matches!(
            acquire_link_slot(&cfg, &path, None),
            LinkGate::Degraded
        ));
    }

    #[test]
    fn link_slots_serialize_and_release() {
        let (_tmp, cfg, path) = rig(2, 0);
        let a = match acquire_link_slot(&cfg, &path, Some("first")) {
            LinkGate::Held(slot) => slot,
            _ => panic!("first heavyweight must take the slot"),
        };
        assert_eq!(a.name, "link-0");
        // The single slot is held: a second taker polls. Prove it waits
        // by watching a thread not finish, then release and join.
        let (cfg2, path2) = (cfg.clone(), path.clone());
        let waiter = std::thread::spawn(move || acquire_link_slot(&cfg2, &path2, Some("second")));
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !waiter.is_finished(),
            "second heavyweight must queue on the held link slot"
        );
        drop(a);
        match waiter.join().unwrap() {
            LinkGate::Held(slot) => assert_eq!(slot.name, "link-0"),
            _ => panic!("waiter must acquire after release"),
        }
    }

    #[test]
    fn multiple_link_slots_admit_that_many() {
        let (_tmp, mut cfg, path) = rig(2, 0);
        cfg.link_slots = 2;
        let a = acquire_link_slot(&cfg, &path, Some("a"));
        let b = acquire_link_slot(&cfg, &path, Some("b"));
        let (a, b) = match (a, b) {
            (LinkGate::Held(a), LinkGate::Held(b)) => (a, b),
            _ => panic!("two slots must admit two links"),
        };
        assert_ne!(a.name, b.name);
    }

    #[test]
    fn waiter_respects_foreign_demand_then_takes_own_release() {
        let (_tmp, cfg, path) = rig(1, 1);
        let held = acquire(&cfg, Class::Local, &path, None).unwrap();
        assert_eq!(held.name, "permit-local-0");
        // Register CI demand so Local may not borrow the idle CI permit.
        let demand_ci = open_lock_file(&cfg.permit_dir.join("demand-ci")).unwrap();
        assert!(flock::lock_shared_blocking(&demand_ci));

        let (cfg2, path2) = (cfg.clone(), path.clone());
        let waiter = std::thread::spawn(move || acquire(&cfg2, Class::Local, &path2, None));
        // Generous settle time; the waiter must still be polling (the CI
        // permit is free but demand-gated).
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !waiter.is_finished(),
            "local waiter must not borrow a contested CI permit"
        );

        drop(held);
        let got = waiter
            .join()
            .unwrap()
            .expect("waiter must acquire after release");
        assert_eq!(got.name, "permit-local-0");
        flock::unlock(&demand_ci);
    }

    /// The 2026-07-21 starvation shape: the permit is FREE but the class
    /// has a registered waiter — the pre-fix governor handed the permit
    /// to the newcomer instantly (wait_ms=0) while the waiter slept out
    /// its poll tick. A newcomer must join the queue instead: it
    /// registers and sleeps at least one tick before its first take.
    #[test]
    fn newcomer_joins_a_registered_queue_instead_of_overtaking() {
        let (_tmp, cfg, path) = rig(1, 0);
        std::fs::create_dir_all(&cfg.permit_dir).unwrap();
        let waiter = open_lock_file(&cfg.permit_dir.join("demand-local")).unwrap();
        assert!(flock::lock_shared_blocking(&waiter));
        let got = acquire(&cfg, Class::Local, &path, Some("newcomer")).unwrap();
        assert_eq!(got.name, "permit-local-0");
        assert!(
            got.wait_ms >= POLL_INTERVAL.as_millis() as u64,
            "a newcomer must queue behind registered demand, not overtake \
             (waited {}ms)",
            got.wait_ms
        );
        flock::unlock(&waiter);
    }

    /// The queue probe must not tax the uncontended path: no registered
    /// demand ⇒ a free permit is still taken immediately, no poll tick.
    #[test]
    fn empty_queue_keeps_the_grab_first_fast_path() {
        let (_tmp, cfg, path) = rig(1, 0);
        let got = acquire(&cfg, Class::Local, &path, None).unwrap();
        assert_eq!(got.name, "permit-local-0");
        assert!(
            got.wait_ms < POLL_INTERVAL.as_millis() as u64,
            "no registered demand: the free permit is taken immediately \
             (waited {}ms)",
            got.wait_ms
        );
    }
}

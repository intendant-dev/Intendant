//! Live registry of backend-native scheduled wakeups, per session — the
//! wake-source twin of [`crate::background_tasks`] for the respawn-orphan
//! class.
//!
//! Claude Code's harness serves the model a `ScheduleWakeup` tool (the
//! self-pacing loop timer: "wake me in N seconds with this prompt"). That
//! timer lives INSIDE the backend process, so every backend respawn class
//! (credential reload, rate-limit restart, service-recovery restart,
//! daemon restart) kills it while the session's idle state survives — the
//! 2026-08-01 specimen idled forever after a credential reload because
//! nothing knew its 22:17 wakeup had died at 22:05. The adapter records
//! only what its own wire proves: a main-thread `ScheduleWakeup` tool_use
//! arms (or, with `stop: true`, clears) the session's one pending record.
//!
//! Ownership handoff is the point of the registry. While the arming
//! process lives, the harness owns the fire and the record is bookkeeping
//! (the supervising loop retires it when the deadline passes with the
//! process alive — the harness had its chance, and any turn it woke is
//! ordinary observed activity). When a respawn seam confirms the process
//! died with the record still pending, [`take_over_at_respawn`] flips it
//! to wrapper-owned: the supervising loop's deadline arm then delivers
//! the wake itself at the original due time (immediately when the due
//! time already passed while the backend was down). Unlike background
//! tasks — commands that are never re-run automatically — re-arming a
//! timer is safe: delivering the recorded wake prompt executes nothing.
//!
//! Keys are the backend-native session id (the id stamped on every wire
//! line, stable across resumes), exactly like the background-task
//! registry. The durable half lives on `SessionMeta::native_wakeup`
//! (stamped by the drains and the supervision seams), which is how the
//! boot pass tells a wakeup the daemon restart killed from silence.
//!
//! Core operations live on [`Registry`] (tests drive local instances —
//! the process global is shared); the `pub(crate)` free functions are the
//! global's transport edge.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Ceiling for a retained wake prompt. Prompts past it are truncated with
/// [`PROMPT_TRUNCATION_SUFFIX`] — the wake still delivers, honestly
/// marked, and the durable marker stays one bounded meta field instead of
/// an unbounded transcript copy.
pub(crate) const PROMPT_RETAINED_CHARS: usize = 4000;

/// Appended to a prompt cut at [`PROMPT_RETAINED_CHARS`].
pub(crate) const PROMPT_TRUNCATION_SUFFIX: &str = " …[truncated by Intendant]";

/// The harness clamps `delaySeconds` to this range before arming; mirror
/// it so a wire value outside the range reads as what the harness will
/// actually do, not as junk to discard.
const MIN_DELAY_SECONDS: u64 = 60;
const MAX_DELAY_SECONDS: u64 = 3600;

/// Sessions retained in the registry. One pending record per session;
/// eviction removes the least-recently-touched record when the cap is
/// hit. A record normally leaves by consumption (fired, stopped,
/// delivered, or surfaced at exit) — the cap only bounds sessions whose
/// wrapper vanished without reaching any seam.
const SESSIONS_RETAINED: usize = 128;

/// One pending native scheduled wakeup, from the backend's own tool_use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeWakeupRecord {
    /// Epoch seconds the arm was observed on the wire.
    pub(crate) armed_at_epoch: u64,
    /// Epoch seconds the wake is due (`armed_at` + the clamped delay).
    pub(crate) fire_at_epoch: u64,
    /// The wake prompt the model asked to receive, bounded at
    /// [`PROMPT_RETAINED_CHARS`].
    pub(crate) prompt: String,
    /// The model's stated reason for the chosen delay, when present.
    pub(crate) reason: Option<String>,
    /// The arming tool_use id (correlation/debug only).
    pub(crate) tool_use_id: String,
    /// `Some(cause)` once a respawn seam confirmed the arming process
    /// died and the supervising wrapper took delivery over; the named
    /// cause is the restart class (e.g. "the credential-reload
    /// restart"). `None` = the harness still owns the fire.
    pub(crate) rearmed_cause: Option<String>,
}

/// Clamp a wire `delaySeconds` to the harness's documented arming range.
pub(crate) fn clamp_delay_seconds(delay: u64) -> u64 {
    delay.clamp(MIN_DELAY_SECONDS, MAX_DELAY_SECONDS)
}

/// Bound a wake prompt for retention (registry and durable marker).
pub(crate) fn bounded_prompt(prompt: &str) -> String {
    if prompt.chars().count() <= PROMPT_RETAINED_CHARS {
        return prompt.to_string();
    }
    let mut bounded: String = prompt.chars().take(PROMPT_RETAINED_CHARS).collect();
    bounded.push_str(PROMPT_TRUNCATION_SUFFIX);
    bounded
}

struct SessionWakeup {
    record: NativeWakeupRecord,
    /// Monotonic touch counter value at last update (eviction order).
    touched: u64,
}

/// The wakeup store proper. All mutation and lookup semantics live here;
/// the module's free functions apply them to the process global.
pub(crate) struct Registry {
    sessions: HashMap<String, SessionWakeup>,
    /// Monotonic counter backing `SessionWakeup::touched`.
    clock: u64,
}

impl Registry {
    pub(crate) fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            clock: 0,
        }
    }

    fn next_clock(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn evict_if_needed(&mut self) {
        while self.sessions.len() > SESSIONS_RETAINED {
            let Some(oldest) = self
                .sessions
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(id, _)| id.clone())
            else {
                return;
            };
            self.sessions.remove(&oldest);
        }
    }

    /// Arm (or replace — the harness keeps one timer) the session's
    /// pending record.
    pub(crate) fn record_armed(&mut self, session_id: &str, record: NativeWakeupRecord) {
        let touched = self.next_clock();
        self.sessions
            .insert(session_id.to_string(), SessionWakeup { record, touched });
        self.evict_if_needed();
    }

    /// `stop: true` — the model ended its loop; nothing pends.
    pub(crate) fn record_stopped(&mut self, session_id: &str) -> bool {
        self.sessions.remove(session_id).is_some()
    }

    /// The session's pending record, if any.
    pub(crate) fn pending_for(&self, session_id: &str) -> Option<NativeWakeupRecord> {
        self.sessions
            .get(session_id)
            .map(|entry| entry.record.clone())
    }

    /// Remove and return the pending record (delivered, consumed by the
    /// live harness at its deadline, or surfaced at session end).
    pub(crate) fn consume(&mut self, session_id: &str) -> Option<NativeWakeupRecord> {
        self.sessions.remove(session_id).map(|entry| entry.record)
    }

    /// A respawn seam confirmed the arming process died with this record
    /// still pending: flip it wrapper-owned under the named cause and
    /// return the flipped record so the seam can announce the re-arm and
    /// stamp the durable marker. Idempotent — an already wrapper-owned
    /// record keeps its first, most specific cause and returns `None`
    /// (the seam that flipped it already announced).
    pub(crate) fn take_over_at_respawn(
        &mut self,
        session_id: &str,
        cause: &str,
    ) -> Option<NativeWakeupRecord> {
        let touched = self.next_clock();
        let entry = self.sessions.get_mut(session_id)?;
        if entry.record.rearmed_cause.is_some() {
            return None;
        }
        entry.record.rearmed_cause = Some(cause.to_string());
        entry.touched = touched;
        Some(entry.record.clone())
    }

    /// The backend rotated its native session id on a LIVE reader (the
    /// arming process continues; only the key changed): move the record
    /// so the timer keeps its observer. A record already under the new
    /// id (should not happen — one process, one timer) is replaced.
    pub(crate) fn migrate(&mut self, old_session_id: &str, new_session_id: &str) {
        if old_session_id == new_session_id {
            return;
        }
        if let Some(entry) = self.sessions.remove(old_session_id) {
            self.sessions.insert(new_session_id.to_string(), entry);
        }
    }
}

fn global() -> &'static Mutex<Registry> {
    static GLOBAL: OnceLock<Mutex<Registry>> = OnceLock::new();
    GLOBAL.get_or_init(|| Mutex::new(Registry::new()))
}

fn with_global<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut registry = global().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut registry)
}

pub(crate) fn record_armed(session_id: &str, record: NativeWakeupRecord) {
    with_global(|r| r.record_armed(session_id, record));
}

pub(crate) fn record_stopped(session_id: &str) -> bool {
    with_global(|r| r.record_stopped(session_id))
}

pub(crate) fn pending_for(session_id: &str) -> Option<NativeWakeupRecord> {
    with_global(|r| r.pending_for(session_id))
}

pub(crate) fn consume(session_id: &str) -> Option<NativeWakeupRecord> {
    with_global(|r| r.consume(session_id))
}

pub(crate) fn take_over_at_respawn(session_id: &str, cause: &str) -> Option<NativeWakeupRecord> {
    with_global(|r| r.take_over_at_respawn(session_id, cause))
}

pub(crate) fn migrate(old_session_id: &str, new_session_id: &str) {
    with_global(|r| r.migrate(old_session_id, new_session_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fire_at: u64) -> NativeWakeupRecord {
        NativeWakeupRecord {
            armed_at_epoch: fire_at.saturating_sub(600),
            fire_at_epoch: fire_at,
            prompt: "<<autonomous-loop-dynamic>>".to_string(),
            reason: Some("watching CI".to_string()),
            tool_use_id: "toolu_01test".to_string(),
            rearmed_cause: None,
        }
    }

    #[test]
    fn arm_replace_stop_consume_lifecycle() {
        let mut reg = Registry::new();
        assert!(reg.pending_for("s1").is_none());
        reg.record_armed("s1", record(1000));
        assert_eq!(reg.pending_for("s1").unwrap().fire_at_epoch, 1000);
        // The harness keeps one timer: a new arm replaces the record.
        reg.record_armed("s1", record(2000));
        assert_eq!(reg.pending_for("s1").unwrap().fire_at_epoch, 2000);
        assert!(reg.record_stopped("s1"));
        assert!(!reg.record_stopped("s1"));
        assert!(reg.pending_for("s1").is_none());
        reg.record_armed("s1", record(3000));
        assert_eq!(reg.consume("s1").unwrap().fire_at_epoch, 3000);
        assert!(reg.pending_for("s1").is_none());
    }

    #[test]
    fn take_over_flips_once_and_first_cause_stands() {
        let mut reg = Registry::new();
        assert!(reg.take_over_at_respawn("s1", "the daemon restart").is_none());
        reg.record_armed("s1", record(1000));
        let flipped = reg
            .take_over_at_respawn("s1", "the credential-reload restart")
            .expect("pending record flips");
        assert_eq!(
            flipped.rearmed_cause.as_deref(),
            Some("the credential-reload restart")
        );
        // Second seam finds it already wrapper-owned: no re-announce, the
        // first, most specific cause stands.
        assert!(reg.take_over_at_respawn("s1", "the rate-limit restart").is_none());
        assert_eq!(
            reg.pending_for("s1").unwrap().rearmed_cause.as_deref(),
            Some("the credential-reload restart")
        );
    }

    #[test]
    fn migrate_follows_a_live_reader_id_rotation() {
        let mut reg = Registry::new();
        reg.record_armed("old", record(1000));
        reg.migrate("old", "new");
        assert!(reg.pending_for("old").is_none());
        assert_eq!(reg.pending_for("new").unwrap().fire_at_epoch, 1000);
        // Same-id migration is a no-op, never a drop.
        reg.migrate("new", "new");
        assert!(reg.pending_for("new").is_some());
    }

    #[test]
    fn eviction_bounds_abandoned_sessions() {
        let mut reg = Registry::new();
        for i in 0..(SESSIONS_RETAINED + 8) {
            reg.record_armed(&format!("s{i}"), record(1000 + i as u64));
        }
        assert!(reg.sessions.len() <= SESSIONS_RETAINED);
        // Oldest-touched left first.
        assert!(reg.pending_for("s0").is_none());
        assert!(reg
            .pending_for(&format!("s{}", SESSIONS_RETAINED + 7))
            .is_some());
    }

    #[test]
    fn delay_clamp_and_prompt_bound() {
        assert_eq!(clamp_delay_seconds(5), 60);
        assert_eq!(clamp_delay_seconds(780), 780);
        assert_eq!(clamp_delay_seconds(90000), 3600);
        let short = bounded_prompt("wake");
        assert_eq!(short, "wake");
        let long: String = "x".repeat(PROMPT_RETAINED_CHARS + 10);
        let bounded = bounded_prompt(&long);
        assert!(bounded.ends_with(PROMPT_TRUNCATION_SUFFIX));
        assert_eq!(
            bounded.chars().count(),
            PROMPT_RETAINED_CHARS + PROMPT_TRUNCATION_SUFFIX.chars().count()
        );
    }
}

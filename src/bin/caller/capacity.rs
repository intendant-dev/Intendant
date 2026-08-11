//! Capacity staging: box-wide memory-headroom watermarks and the bounded
//! resident-session admission budget (the capacity program's first slice).
//!
//! The daemon historically had no resource awareness — it held 23 resident
//! max-effort sessions on a 16 GiB guest through a ~100-minute thrash the
//! kernel never relieved (2026-07-29; macOS pressure oscillated warn/normal
//! and never reached critical while the compressor swelled to ~40% of RAM).
//! This module stages backpressure BEFORE that freeze point:
//!
//! - **defer** — new session admissions queue or refuse honestly; resident
//!   work continues untouched.
//! - **park** — additionally, the longest-idle resident sessions are marked
//!   parked with visible chips: an honest census of what is holding memory,
//!   and a promise that the daemon will not auto-wake them while pressure
//!   holds. Parks never kill, and anything user-initiated still serves
//!   (delivery unparks). Real reclaim (hibernating idle wrapper/backend
//!   processes) is a later slice of the program.
//!
//! Invariants: fail-open — a missing or broken probe reads as "no signal"
//! and must never brick admissions (the count bound needs no probe and
//! still applies); every deferral, refusal, queue position, and park is
//! visible — the freeze was silent starvation, and silence is the defect;
//! stages worsen immediately and ease only after a sustained dwell, so a
//! flapping signal cannot flap the backpressure.

use std::sync::Arc;
use std::time::{Duration, Instant};

use intendant_platform::memory::MemorySample;
use serde::{Deserialize, Serialize};

/// Poll cadence for the capacity monitor (same beat as the vitals
/// producer; the probe is a handful of in-process syscalls).
pub(crate) const CAPACITY_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// A stage eases only after the lower raw stage holds for this long;
/// worsening applies immediately. Any sample at or above the current
/// stage restarts the dwell.
pub(crate) const EASE_DWELL: Duration = Duration::from_secs(30);

/// A resident root session is park-eligible once idle this long.
pub(crate) const PARK_IDLE_MIN: Duration = Duration::from_secs(600);

/// Bound on the deferred-admission queue: beyond it, admissions refuse
/// outright (an unbounded queue under sustained pressure is its own
/// leak). Sized generously above any observed burst (the 07-30 wave was
/// eight).
pub(crate) const ADMISSION_QUEUE_CAP: usize = 32;

/// Resident-session bound when the box's memory size is unknown.
const PROBELESS_DEFAULT_MAX_RESIDENT: usize = 32;

/// Clamp range for the RAM-derived resident bound (one resident session
/// per GiB of physical memory, within reason: the freeze box would get
/// 16 where 23 starved it; big hosts stop at 64).
const DERIVED_BOUND_MIN: usize = 8;
const DERIVED_BOUND_MAX: usize = 64;

/// Cap on parked-session ids carried on the wire view (the count is
/// always exact).
const VIEW_PARKED_IDS_CAP: usize = 32;

/// Cap on queued-admission rows carried on the wire view (the count is
/// always exact).
const VIEW_QUEUE_ROWS_CAP: usize = 16;

// Watermarks. Enter thresholds only — easing hysteresis is the dwell in
// [`StageTracker`], not separate exit thresholds. Park thresholds are
// strictly tighter than defer thresholds per signal (the staging-order
// invariant, pinned by test below).
//
// macOS: the kernel pressure level alone under-fires at the top (it
// never reached critical during the 07-29 freeze) and over-fires at the
// bottom (warn is a chronic state on healthy-busy Macs — a perfectly
// workable 24 GB box sat in warn while admissions refused on it,
// 2026-08-11). The compressor fraction is the loud magnitude signal
// (1.4 GiB healthy-busy → 5.3–6.6 GiB of a 16 GiB guest collapsing). So
// pressure-warn NEVER stages alone: it defers only when corroborated by
// a second signal ([`macos_warn_corroboration`]) — the compressor past
// [`MACOS_WARN_COMPRESSOR_CORROBORATION`], or the available fraction
// already under its own defer watermark — while critical still parks on
// its own. "Available" reads healthy on a thrashing Mac (reclaimable
// file pages), so the available fraction is a universal backstop only.
const AVAILABLE_FRAC_DEFER: f64 = 0.10;
const AVAILABLE_FRAC_PARK: f64 = 0.05;
const MACOS_PRESSURE_DEFER: u32 = 2; // memorystatus warn
const MACOS_PRESSURE_PARK: u32 = 4; // memorystatus critical
const COMPRESSOR_FRAC_DEFER: f64 = 0.25;
const COMPRESSOR_FRAC_PARK: f64 = 0.35;
/// Corroboration floor for macOS pressure-warn: warn alone is routine on
/// a healthy-busy Mac, so it only defers when the compressor — the 07-29
/// freeze's loud discriminator (~9% of RAM healthy-busy vs 33–41%
/// collapsing) — is already elevated past this floor. 0.15 sits well
/// above the healthy-busy compressor band and well under
/// [`COMPRESSOR_FRAC_DEFER`]: warn plus a swelling-but-not-yet-loud
/// compressor is the earliest honest two-signal distress, without letting
/// chronic warn re-become a solo trigger.
const MACOS_WARN_COMPRESSOR_CORROBORATION: f64 = 0.15;
const PSI_SOME_AVG10_DEFER: f64 = 10.0;
const PSI_SOME_AVG10_PARK: f64 = 40.0; // the fleet watchdog's pause line
const PSI_FULL_AVG10_PARK: f64 = 5.0;
const WINDOWS_LOAD_DEFER: u32 = 90;
const WINDOWS_LOAD_PARK: u32 = 95;

/// Daemon-wide capacity stage, ordered by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapacityStage {
    Normal,
    Defer,
    Park,
}

impl CapacityStage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CapacityStage::Normal => "normal",
            CapacityStage::Defer => "defer",
            CapacityStage::Park => "park",
        }
    }
}

/// The second signal that lets a macOS pressure-warn reading stage at
/// all: the compressor already past the corroboration floor (named with
/// its reading), or the available fraction already under its own defer
/// watermark. `None` = uncorroborated — warn alone stays [`CapacityStage::Normal`].
fn macos_warn_corroboration(sample: &MemorySample) -> Option<String> {
    if let Some(frac) = sample.compressor_frac {
        if frac >= MACOS_WARN_COMPRESSOR_CORROBORATION {
            return Some(format!("compressor {:.0}%", frac * 100.0));
        }
    }
    if let Some(frac) = sample.available_frac() {
        if frac < AVAILABLE_FRAC_DEFER {
            return Some(format!("available {:.0}%", frac * 100.0));
        }
    }
    None
}

/// Raw (pre-hysteresis) stage for one sample, plus the honest
/// human-readable list of the signal(s) firing — one entry per firing
/// signal, each naming its reading. Policy and its explanation are one
/// pass over the sample so they cannot drift apart. Absent signals
/// contribute nothing — policy applies only to what the host actually
/// reports; a signal below every watermark contributes no reason.
pub(crate) fn stage_and_reasons(sample: &MemorySample) -> (CapacityStage, Vec<String>) {
    let mut stage = CapacityStage::Normal;
    let mut reasons: Vec<String> = Vec::new();
    let mut fire = |to: CapacityStage, reason: String| {
        if to > stage {
            stage = to;
        }
        reasons.push(reason);
    };
    if let Some(frac) = sample.available_frac() {
        if frac < AVAILABLE_FRAC_PARK {
            fire(
                CapacityStage::Park,
                format!("available memory {:.0}%", frac * 100.0),
            );
        } else if frac < AVAILABLE_FRAC_DEFER {
            fire(
                CapacityStage::Defer,
                format!("available memory {:.0}%", frac * 100.0),
            );
        }
    }
    if let Some(level) = sample.os_pressure_level {
        if level >= MACOS_PRESSURE_PARK {
            fire(CapacityStage::Park, "memory pressure critical".to_string());
        } else if level >= MACOS_PRESSURE_DEFER {
            // Warn alone never stages (chronic on healthy-busy Macs) —
            // only the corroborated pair defers.
            if let Some(corroboration) = macos_warn_corroboration(sample) {
                fire(
                    CapacityStage::Defer,
                    format!("memory pressure warn + {corroboration}"),
                );
            }
        }
    }
    if let Some(frac) = sample.compressor_frac {
        if frac >= COMPRESSOR_FRAC_PARK {
            fire(
                CapacityStage::Park,
                format!("compressor {:.0}% of RAM", frac * 100.0),
            );
        } else if frac >= COMPRESSOR_FRAC_DEFER {
            fire(
                CapacityStage::Defer,
                format!("compressor {:.0}% of RAM", frac * 100.0),
            );
        }
    }
    if let Some(some) = sample.psi_some_avg10 {
        if some >= PSI_SOME_AVG10_PARK {
            fire(
                CapacityStage::Park,
                format!("memory stall {some:.0}% (PSI some)"),
            );
        } else if some >= PSI_SOME_AVG10_DEFER {
            fire(
                CapacityStage::Defer,
                format!("memory stall {some:.0}% (PSI some)"),
            );
        }
    }
    if let Some(full) = sample.psi_full_avg10 {
        if full >= PSI_FULL_AVG10_PARK {
            fire(
                CapacityStage::Park,
                format!("full memory stall {full:.0}% (PSI full)"),
            );
        }
    }
    if let Some(load) = sample.load_percent {
        if load >= WINDOWS_LOAD_PARK {
            fire(CapacityStage::Park, format!("memory load {load}%"));
        } else if load >= WINDOWS_LOAD_DEFER {
            fire(CapacityStage::Defer, format!("memory load {load}%"));
        }
    }
    (stage, reasons)
}

/// Raw stage only (the historical name; the staging-order pin and the
/// truth-table tests key on it). Same single pass as
/// [`stage_and_reasons`], which is what production consumes.
#[cfg(test)]
pub(crate) fn raw_stage(sample: &MemorySample) -> CapacityStage {
    stage_and_reasons(sample).0
}

/// Owner knob for staging aggressiveness (`[capacity] staging` /
/// `INTENDANT_CAPACITY_STAGING`): one coarse ceiling on what the
/// pressure signals may stage — deliberately not per-threshold tuning.
/// The resident-session bound, the census, and the view stay in force in
/// every mode; `[capacity] enabled = false` remains the full off switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum StagingMode {
    /// Full staging: defer and park per the watermarks (the default).
    #[default]
    Normal,
    /// Pressure signals cap at defer: admissions queue, nothing parks.
    DeferOnly,
    /// Pressure staging off: only the resident-session bound gates.
    Off,
}

impl StagingMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            StagingMode::Normal => "normal",
            StagingMode::DeferOnly => "defer-only",
            StagingMode::Off => "off",
        }
    }
}

/// Resolve the staging mode: explicit config key, then the
/// `INTENDANT_CAPACITY_STAGING` env override (the resolver is pure over
/// the values, like [`resolve_max_resident`]; the edge reads the
/// environment once), then the default. Unrecognized values fall through
/// toward the default — a typo must never brick or blind the daemon.
pub(crate) fn resolve_staging(config: Option<&str>, env: Option<&str>) -> StagingMode {
    fn parse(value: &str) -> Option<StagingMode> {
        match value.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(StagingMode::Normal),
            "defer-only" | "defer_only" => Some(StagingMode::DeferOnly),
            "off" => Some(StagingMode::Off),
            _ => None,
        }
    }
    config
        .and_then(parse)
        .or_else(|| env.and_then(parse))
        .unwrap_or_default()
}

/// Stage hysteresis: worsening applies immediately; easing requires the
/// lower raw stage to hold for [`EASE_DWELL`]. A sample at or above the
/// current stage discards any pending ease.
#[derive(Debug)]
pub(crate) struct StageTracker {
    current: CapacityStage,
    /// Pending ease candidate: the sustained lower stage and when its
    /// current run began.
    easing: Option<(CapacityStage, Instant)>,
}

impl Default for StageTracker {
    fn default() -> Self {
        StageTracker {
            current: CapacityStage::Normal,
            easing: None,
        }
    }
}

impl StageTracker {
    /// Fold one raw stage observation; returns the (possibly unchanged)
    /// effective stage.
    pub(crate) fn observe(&mut self, raw: CapacityStage, now: Instant) -> CapacityStage {
        if raw > self.current {
            self.current = raw;
            self.easing = None;
        } else if raw < self.current {
            match self.easing {
                Some((candidate, since)) if candidate == raw => {
                    if now.duration_since(since) >= EASE_DWELL {
                        self.current = raw;
                        self.easing = None;
                    }
                }
                // A different lower stage restarts the dwell at the new
                // level (park → oscillating defer/normal never eases past
                // what was actually sustained).
                _ => self.easing = Some((raw, now)),
            }
        } else {
            self.easing = None;
        }
        self.current
    }
}

/// Resolve the resident-session bound: explicit config, then the
/// `INTENDANT_CAPACITY_MAX_RESIDENT` env override (resolver is pure over
/// the value; the edge reads the environment once), then one session per
/// GiB of physical memory clamped to [`DERIVED_BOUND_MIN`]..=[`DERIVED_BOUND_MAX`],
/// then the probe-less default.
pub(crate) fn resolve_max_resident(
    config: Option<usize>,
    env: Option<&str>,
    total_bytes: Option<u64>,
) -> usize {
    if let Some(bound) = config {
        return bound.max(1);
    }
    if let Some(bound) = env
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<usize>().ok())
    {
        return bound.max(1);
    }
    match total_bytes {
        Some(total) => {
            let gib = (total / (1024 * 1024 * 1024)) as usize;
            gib.clamp(DERIVED_BOUND_MIN, DERIVED_BOUND_MAX)
        }
        None => PROBELESS_DEFAULT_MAX_RESIDENT,
    }
}

/// One queued admission on the wire: its honest position and age.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityQueueRow {
    pub position: usize,
    /// What kind of create is waiting (`create_session` / `start_task`).
    pub kind: String,
    pub enqueued_ms: u64,
}

/// The serialized daemon-wide capacity view: the wire truth for the
/// dashboard chip, `get_status`/`ctl status`, and the agenda scheduler's
/// level check. One struct, derived everywhere it surfaces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityView {
    pub stage: CapacityStage,
    /// The honest, human-readable signal(s) behind the stage, derived at
    /// sample time (e.g. `"memory pressure warn + compressor 18%"`).
    /// Empty at a quiet normal; carries the easing note while the dwell
    /// holds a stage past its signals. Frontends render these strings —
    /// no client-side threshold knowledge.
    #[serde(default)]
    pub reasons: Vec<String>,
    /// The staging mode in force (`normal` / `defer-only` / `off`) — so
    /// a capped or disabled staging is diagnosable from the view.
    #[serde(default = "default_view_staging")]
    pub staging: String,
    /// False when the platform probe is unavailable or failing — rendered
    /// honestly (the stage then stays `normal` by fail-open, not by
    /// measurement).
    pub probe_ok: bool,
    /// Resident-session bound in force.
    pub bound: usize,
    /// Last observed resident-session count.
    pub resident: usize,
    /// Deferred admissions currently queued.
    pub queued: usize,
    /// The queue's honest rows (capped at [`VIEW_QUEUE_ROWS_CAP`]; the
    /// `queued` count is exact).
    pub queue: Vec<CapacityQueueRow>,
    /// Parked session ids (capped at [`VIEW_PARKED_IDS_CAP`]); the count
    /// is exact.
    pub parked: Vec<String>,
    pub parked_count: usize,
    /// True when new admissions defer (stage ≥ defer, or residents at the
    /// bound) — the single signal upstream holders consult.
    pub admissions_deferred: bool,
    /// Epoch ms when the current stage was entered.
    pub stage_since_ms: u64,
    /// Last good sample, for the honest numbers behind the stage.
    pub sample: Option<MemorySample>,
}

impl CapacityView {
    fn recompute_derived(&mut self) {
        self.admissions_deferred =
            self.stage >= CapacityStage::Defer || self.resident >= self.bound;
    }
}

fn default_view_staging() -> String {
    StagingMode::Normal.as_str().to_string()
}

/// Outcome of an admission check at the gate.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AdmissionCheck {
    Admit,
    /// Admissions are deferred; the caller queues or refuses per lane.
    Gate {
        stage: CapacityStage,
        /// The view's signal reasons at the gate, for refusals that say
        /// why (empty when only the count bound gates).
        reasons: Vec<String>,
        bound: usize,
        resident: usize,
        queued: usize,
    },
}

/// Shared capacity state: the tracker, the published view, and the
/// admission math. Owned by the session supervisor's monitor; read by the
/// gates, the agenda scheduler, `get_status`, and the dashboard event
/// lane. Constructed only when `[capacity] enabled` (the default) — its
/// absence is complete fail-open to pre-slice behavior.
pub(crate) struct CapacityController {
    inner: std::sync::Mutex<Inner>,
    staging: StagingMode,
}

struct Inner {
    tracker: StageTracker,
    view: CapacityView,
}

impl CapacityController {
    /// Test convenience: the default staging mode. Production construction
    /// goes through [`controller_from_config`], which resolves the mode.
    #[cfg(test)]
    pub(crate) fn new(max_resident: usize) -> Arc<Self> {
        Self::with_staging(max_resident, StagingMode::Normal)
    }

    pub(crate) fn with_staging(max_resident: usize, staging: StagingMode) -> Arc<Self> {
        let view = CapacityView {
            stage: CapacityStage::Normal,
            reasons: Vec::new(),
            staging: staging.as_str().to_string(),
            probe_ok: false,
            bound: max_resident.max(1),
            resident: 0,
            queued: 0,
            queue: Vec::new(),
            parked: Vec::new(),
            parked_count: 0,
            admissions_deferred: false,
            stage_since_ms: epoch_ms(),
            sample: None,
        };
        Arc::new(CapacityController {
            inner: std::sync::Mutex::new(Inner {
                tracker: StageTracker::default(),
                view,
            }),
            staging,
        })
    }

    pub(crate) fn view(&self) -> CapacityView {
        self.inner.lock().expect("capacity lock").view.clone()
    }

    /// Fold one probe observation (`None` = probe unavailable this tick)
    /// plus the current census; returns the new view when anything
    /// user-visible changed (the caller broadcasts it).
    pub(crate) fn observe(
        &self,
        sample: Option<MemorySample>,
        now: Instant,
    ) -> Option<CapacityView> {
        let mut inner = self.inner.lock().expect("capacity lock");
        let previous = inner.view.clone();
        // Fail-open on a missing sample: no signal is not distress. The
        // stage decays to normal through the same dwell (a mid-pressure
        // probe outage releases backpressure rather than pinning it).
        let (mut raw, mut reasons) = match &sample {
            Some(sample) => stage_and_reasons(sample),
            None => (CapacityStage::Normal, Vec::new()),
        };
        // The owner's staging ceiling caps what the signals may stage;
        // the readings themselves stay honestly listed.
        match self.staging {
            StagingMode::Normal => {}
            StagingMode::DeferOnly => {
                if raw > CapacityStage::Defer {
                    raw = CapacityStage::Defer;
                    reasons.push("park capped at defer (staging = \"defer-only\")".to_string());
                }
            }
            StagingMode::Off => {
                raw = CapacityStage::Normal;
                reasons.clear();
            }
        }
        let stage = inner.tracker.observe(raw, now);
        if stage > raw {
            // The dwell is holding a stage its signals no longer call
            // for — say so instead of showing a stage with no reason.
            reasons.push(format!(
                "easing — {} releases after {}s of sustained lower readings",
                stage.as_str(),
                EASE_DWELL.as_secs()
            ));
        }
        if stage != inner.view.stage {
            inner.view.stage_since_ms = epoch_ms();
        }
        inner.view.stage = stage;
        inner.view.reasons = reasons;
        inner.view.probe_ok = sample.is_some();
        inner.view.sample = sample;
        inner.view.recompute_derived();
        (inner.view != previous).then(|| inner.view.clone())
    }

    /// Update the resident/queue/park census; returns the new view when
    /// it changed (the caller broadcasts it).
    pub(crate) fn update_census(
        &self,
        resident: usize,
        queue: Vec<CapacityQueueRow>,
        parked: &[String],
    ) -> Option<CapacityView> {
        let mut inner = self.inner.lock().expect("capacity lock");
        let previous = inner.view.clone();
        inner.view.resident = resident;
        inner.view.queued = queue.len();
        inner.view.queue = queue.into_iter().take(VIEW_QUEUE_ROWS_CAP).collect();
        inner.view.parked_count = parked.len();
        inner.view.parked = parked.iter().take(VIEW_PARKED_IDS_CAP).cloned().collect();
        inner.view.recompute_derived();
        (inner.view != previous).then(|| inner.view.clone())
    }

    /// The admission decision for one create attempt, given the live
    /// resident count at the gate.
    pub(crate) fn admission_check(&self, resident_now: usize) -> AdmissionCheck {
        let inner = self.inner.lock().expect("capacity lock");
        let stage = inner.view.stage;
        let bound = inner.view.bound;
        if stage >= CapacityStage::Defer || resident_now >= bound {
            AdmissionCheck::Gate {
                stage,
                reasons: inner.view.reasons.clone(),
                bound,
                resident: resident_now,
                queued: inner.view.queued,
            }
        } else {
            AdmissionCheck::Admit
        }
    }
}

/// Build the daemon's capacity controller from its `[capacity]` config:
/// `None` when disabled — complete fail-open, no probe, no bound. The
/// env override and the box's memory size are read once, here, so the
/// resolver stays pure ([`resolve_max_resident`]).
pub(crate) fn controller_from_config(
    config: &crate::project::CapacityConfig,
) -> Option<Arc<CapacityController>> {
    if !config.enabled {
        return None;
    }
    let env = std::env::var("INTENDANT_CAPACITY_MAX_RESIDENT").ok();
    let staging_env = std::env::var("INTENDANT_CAPACITY_STAGING").ok();
    let total = intendant_platform::memory::sample_memory().map(|s| s.total_bytes);
    Some(CapacityController::with_staging(
        resolve_max_resident(config.max_resident_sessions, env.as_deref(), total),
        resolve_staging(config.staging.as_deref(), staging_env.as_deref()),
    ))
}

/// Stable machine-checkable prefix for capacity refusals (surfaces and
/// tests key on it, like `daemon_draining:`).
pub(crate) const CAPACITY_REFUSAL_PREFIX: &str = "capacity_deferred:";

/// Parenthetical rendering of the firing signals for refusal/queue copy:
/// empty stays empty (count-bound gating at a quiet normal has nothing
/// to name beyond the bound itself, which the copy already names).
fn reasons_parenthetical(reasons: &[String]) -> String {
    if reasons.is_empty() {
        String::new()
    } else {
        format!(" ({})", reasons.join(", "))
    }
}

/// The honest refusal for a lane that cannot queue: names what is
/// refused, the stage and the signal(s) behind it, the bound and count,
/// and the queue depth.
pub(crate) fn refusal_text(
    what: &str,
    stage: CapacityStage,
    reasons: &[String],
    resident: usize,
    bound: usize,
    queued: usize,
) -> String {
    format!(
        "{CAPACITY_REFUSAL_PREFIX} {what} deferred — capacity stage is \
         {stage}{why} with {resident} of {bound} resident sessions and {queued} \
         admission(s) queued; retry when headroom returns (watch the \
         capacity state in status)",
        stage = stage.as_str(),
        why = reasons_parenthetical(reasons),
    )
}

/// The honest queue notice for a queued admission: names the position,
/// the stage and its signal(s), and the bound.
pub(crate) fn queued_text(
    what: &str,
    position: usize,
    stage: CapacityStage,
    reasons: &[String],
    resident: usize,
    bound: usize,
) -> String {
    format!(
        "capacity: {what} queued at position {position} — stage {stage}{why}, \
         {resident} of {bound} resident sessions; it fires when headroom \
         returns",
        stage = stage.as_str(),
        why = reasons_parenthetical(reasons),
    )
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Published controller: the daemon-wide read handle for surfaces outside
// the supervisor (the agenda scheduler's level check, the MCP start-task
// honesty twin, get_status). Same shape as the published live-session
// registry: last publisher wins, absence means fail-open.
// ---------------------------------------------------------------------

static PUBLISHED: std::sync::RwLock<Option<Arc<CapacityController>>> = std::sync::RwLock::new(None);

pub(crate) fn publish_capacity_controller(controller: Arc<CapacityController>) {
    *PUBLISHED.write().expect("capacity publish lock") = Some(controller);
}

pub(crate) fn published_capacity_controller() -> Option<Arc<CapacityController>> {
    PUBLISHED.read().expect("capacity publish lock").clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MemorySample {
        MemorySample {
            total_bytes: 16 * 1024 * 1024 * 1024,
            available_bytes: Some(8 * 1024 * 1024 * 1024),
            os_pressure_level: None,
            compressor_frac: None,
            psi_some_avg10: None,
            psi_full_avg10: None,
            load_percent: None,
        }
    }

    fn frac(total: u64, frac: f64) -> u64 {
        (total as f64 * frac) as u64
    }

    // The staging-order invariant: every park watermark sits strictly
    // inside its defer watermark, so worsening pressure always crosses
    // defer before park — admissions defer BEFORE any resident work is
    // parked. (The macOS pressure case carries its corroborating
    // compressor: warn alone never stages at all — see the corroboration
    // truth table below.)
    #[test]
    fn staging_order_defer_engages_before_park_per_signal() {
        let total = 16u64 * 1024 * 1024 * 1024;
        // (defer-triggering sample, park-triggering sample) per signal.
        let cases: Vec<(MemorySample, MemorySample)> = vec![
            (
                MemorySample {
                    available_bytes: Some(frac(total, 0.08)),
                    ..sample()
                },
                MemorySample {
                    available_bytes: Some(frac(total, 0.04)),
                    ..sample()
                },
            ),
            (
                MemorySample {
                    os_pressure_level: Some(2),
                    compressor_frac: Some(0.18),
                    ..sample()
                },
                MemorySample {
                    os_pressure_level: Some(4),
                    ..sample()
                },
            ),
            (
                MemorySample {
                    compressor_frac: Some(0.26),
                    ..sample()
                },
                MemorySample {
                    compressor_frac: Some(0.36),
                    ..sample()
                },
            ),
            (
                MemorySample {
                    psi_some_avg10: Some(11.0),
                    ..sample()
                },
                MemorySample {
                    psi_some_avg10: Some(41.0),
                    ..sample()
                },
            ),
            (
                MemorySample {
                    load_percent: Some(91),
                    ..sample()
                },
                MemorySample {
                    load_percent: Some(96),
                    ..sample()
                },
            ),
        ];
        for (defer_sample, park_sample) in cases {
            assert_eq!(raw_stage(&defer_sample), CapacityStage::Defer);
            assert_eq!(raw_stage(&park_sample), CapacityStage::Park);
        }
        // PSI full is a park-only confirmation signal; it cannot fire
        // without PSI some (a superset by definition) already deferring.
        let psi_full = MemorySample {
            psi_some_avg10: Some(12.0),
            psi_full_avg10: Some(6.0),
            ..sample()
        };
        assert_eq!(raw_stage(&psi_full), CapacityStage::Park);
        // A healthy sample stages nothing.
        assert_eq!(raw_stage(&sample()), CapacityStage::Normal);
    }

    // The corroboration truth table: macOS pressure-warn is chronic on
    // healthy-busy Macs (the 2026-08-11 complaint: a workable 24 GB box
    // refused new work on warn alone), so warn by itself NEVER stages —
    // it defers only with a second signal (compressor past the
    // corroboration floor, or available already under its defer
    // watermark) — while critical still parks on its own and the
    // standalone compressor watermarks are untouched.
    #[test]
    fn macos_pressure_warn_defers_only_when_corroborated() {
        let total = 16u64 * 1024 * 1024 * 1024;
        let warn = |compressor: Option<f64>, available: Option<f64>| MemorySample {
            os_pressure_level: Some(2),
            compressor_frac: compressor,
            available_bytes: available.map(|f| frac(total, f)),
            ..sample()
        };
        // Warn alone (healthy available, no compressor reading): Normal.
        assert_eq!(raw_stage(&warn(None, Some(0.50))), CapacityStage::Normal);
        assert_eq!(raw_stage(&warn(None, None)), CapacityStage::Normal);
        // Warn + compressor under the corroboration floor: still Normal.
        assert_eq!(
            raw_stage(&warn(Some(0.14), Some(0.50))),
            CapacityStage::Normal
        );
        // Warn + compressor at/above the floor (but under its own defer
        // watermark): the corroborated pair defers.
        assert_eq!(
            raw_stage(&warn(Some(0.15), Some(0.50))),
            CapacityStage::Defer
        );
        assert_eq!(
            raw_stage(&warn(Some(0.18), Some(0.50))),
            CapacityStage::Defer
        );
        // Warn + available already under its defer watermark: Defer.
        assert_eq!(raw_stage(&warn(None, Some(0.08))), CapacityStage::Defer);
        // The corroborating readings alone (no pressure signal) stage
        // nothing: the corroboration floor is not a new solo trigger.
        assert_eq!(
            raw_stage(&MemorySample {
                compressor_frac: Some(0.18),
                ..sample()
            }),
            CapacityStage::Normal
        );
        // Level 3 sits in the warn band (>= warn, < critical): same rule.
        assert_eq!(
            raw_stage(&MemorySample {
                os_pressure_level: Some(3),
                ..sample()
            }),
            CapacityStage::Normal
        );
        assert_eq!(
            raw_stage(&MemorySample {
                os_pressure_level: Some(3),
                compressor_frac: Some(0.20),
                ..sample()
            }),
            CapacityStage::Defer
        );
        // Critical needs no corroboration: Park on its own, unchanged.
        assert_eq!(
            raw_stage(&MemorySample {
                os_pressure_level: Some(4),
                ..sample()
            }),
            CapacityStage::Park
        );
        // The standalone compressor watermarks are untouched: defer at
        // 0.25, park at 0.35, with or without the warn reading.
        assert_eq!(
            raw_stage(&MemorySample {
                compressor_frac: Some(0.26),
                ..sample()
            }),
            CapacityStage::Defer
        );
        assert_eq!(
            raw_stage(&warn(Some(0.36), Some(0.50))),
            CapacityStage::Park
        );
        // Warn + park-level low available: available's own park wins the
        // fold (warn corroboration never caps a worse signal).
        assert_eq!(raw_stage(&warn(None, Some(0.04))), CapacityStage::Park);
    }

    // Reasons are the same pass as the stage: every firing signal is
    // named with its reading, non-firing signals contribute nothing.
    #[test]
    fn reasons_name_each_firing_signal_with_its_reading() {
        let total = 16u64 * 1024 * 1024 * 1024;
        // A quiet sample: no stage, no reasons.
        assert!(stage_and_reasons(&sample()).1.is_empty());
        // Uncorroborated warn: no stage, no reasons (nothing fired).
        assert!(stage_and_reasons(&MemorySample {
            os_pressure_level: Some(2),
            ..sample()
        })
        .1
        .is_empty());
        // The corroborated pair names both halves in one reason.
        let (stage, reasons) = stage_and_reasons(&MemorySample {
            os_pressure_level: Some(2),
            compressor_frac: Some(0.18),
            ..sample()
        });
        assert_eq!(stage, CapacityStage::Defer);
        assert_eq!(reasons, vec!["memory pressure warn + compressor 18%"]);
        // Warn corroborated by low available names the available reading.
        let (_, reasons) = stage_and_reasons(&MemorySample {
            os_pressure_level: Some(2),
            available_bytes: Some(frac(total, 0.08)),
            ..sample()
        });
        assert_eq!(
            reasons,
            vec![
                "available memory 8%".to_string(),
                "memory pressure warn + available 8%".to_string(),
            ]
        );
        // Critical, standalone compressor, PSI, and Windows load each
        // carry their own reading.
        let (stage, reasons) = stage_and_reasons(&MemorySample {
            os_pressure_level: Some(4),
            compressor_frac: Some(0.36),
            ..sample()
        });
        assert_eq!(stage, CapacityStage::Park);
        assert_eq!(
            reasons,
            vec![
                "memory pressure critical".to_string(),
                "compressor 36% of RAM".to_string(),
            ]
        );
        let (_, reasons) = stage_and_reasons(&MemorySample {
            psi_some_avg10: Some(15.0),
            ..sample()
        });
        assert_eq!(reasons, vec!["memory stall 15% (PSI some)"]);
        let (stage, reasons) = stage_and_reasons(&MemorySample {
            psi_some_avg10: Some(12.0),
            psi_full_avg10: Some(6.0),
            ..sample()
        });
        assert_eq!(stage, CapacityStage::Park);
        assert_eq!(
            reasons,
            vec![
                "memory stall 12% (PSI some)".to_string(),
                "full memory stall 6% (PSI full)".to_string(),
            ]
        );
        let (_, reasons) = stage_and_reasons(&MemorySample {
            load_percent: Some(92),
            ..sample()
        });
        assert_eq!(reasons, vec!["memory load 92%"]);
    }

    // The view's reasons follow the latest sample, and the ease dwell
    // holding a stage past its signals says so instead of showing a
    // stage with no reason.
    #[test]
    fn view_reasons_follow_the_sample_and_name_the_easing_dwell() {
        let controller = CapacityController::new(8);
        let t0 = Instant::now();
        let pressured = MemorySample {
            os_pressure_level: Some(2),
            compressor_frac: Some(0.18),
            ..sample()
        };
        let view = controller.observe(Some(pressured), t0).expect("changed");
        assert_eq!(view.stage, CapacityStage::Defer);
        assert_eq!(view.reasons, vec!["memory pressure warn + compressor 18%"]);
        // Signals clear inside the dwell: the stage holds and the
        // reasons carry the honest easing note.
        let view = controller
            .observe(Some(sample()), t0 + Duration::from_secs(5))
            .expect("changed");
        assert_eq!(view.stage, CapacityStage::Defer, "inside the dwell");
        assert_eq!(
            view.reasons,
            vec![format!(
                "easing — defer releases after {}s of sustained lower readings",
                EASE_DWELL.as_secs()
            )]
        );
        // Past the dwell: normal, nothing to explain.
        let view = controller
            .observe(Some(sample()), t0 + Duration::from_secs(5) + EASE_DWELL)
            .expect("changed");
        assert_eq!(view.stage, CapacityStage::Normal);
        assert!(view.reasons.is_empty());
    }

    // The staging knob: config wins over env, env fills when the key is
    // unset, unrecognized values fall through toward the default at
    // every level — a typo never bricks or blinds the daemon.
    #[test]
    fn resolve_staging_precedence_and_lenient_fallback() {
        assert_eq!(resolve_staging(None, None), StagingMode::Normal);
        assert_eq!(resolve_staging(Some("normal"), None), StagingMode::Normal);
        assert_eq!(
            resolve_staging(Some("defer-only"), None),
            StagingMode::DeferOnly
        );
        assert_eq!(
            resolve_staging(Some("defer_only"), None),
            StagingMode::DeferOnly
        );
        assert_eq!(resolve_staging(Some("off"), None), StagingMode::Off);
        assert_eq!(resolve_staging(Some(" OFF "), None), StagingMode::Off);
        // Config wins over env.
        assert_eq!(
            resolve_staging(Some("defer-only"), Some("off")),
            StagingMode::DeferOnly
        );
        // Env fills when the key is unset or invalid.
        assert_eq!(resolve_staging(None, Some("off")), StagingMode::Off);
        assert_eq!(resolve_staging(Some("nope"), Some("off")), StagingMode::Off);
        // Garbage everywhere reads as the default.
        assert_eq!(resolve_staging(Some("nope"), Some("also-nope")), StagingMode::Normal);
        assert_eq!(resolve_staging(None, Some("")), StagingMode::Normal);
    }

    // `defer-only`: pressure signals cap at defer — admissions still
    // gate, nothing can reach the park stage — and the view says the cap
    // fired.
    #[test]
    fn defer_only_staging_caps_park_at_defer() {
        let controller = CapacityController::with_staging(8, StagingMode::DeferOnly);
        let critical = MemorySample {
            os_pressure_level: Some(4),
            ..sample()
        };
        let view = controller
            .observe(Some(critical), Instant::now())
            .expect("changed");
        assert_eq!(view.stage, CapacityStage::Defer);
        assert_eq!(view.staging, "defer-only");
        assert_eq!(
            view.reasons,
            vec![
                "memory pressure critical".to_string(),
                "park capped at defer (staging = \"defer-only\")".to_string(),
            ]
        );
        match controller.admission_check(0) {
            AdmissionCheck::Gate { stage, .. } => assert_eq!(stage, CapacityStage::Defer),
            other => panic!("defer-only still gates admissions: {other:?}"),
        }
    }

    // `off`: pressure staging disabled — the worst sample stages
    // nothing — while the resident-session bound still gates (the full
    // off switch remains `[capacity] enabled = false`).
    #[test]
    fn off_staging_ignores_pressure_but_keeps_the_count_bound() {
        let controller = CapacityController::with_staging(2, StagingMode::Off);
        let critical = MemorySample {
            os_pressure_level: Some(4),
            compressor_frac: Some(0.40),
            ..sample()
        };
        let view = controller
            .observe(Some(critical), Instant::now())
            .expect("probe_ok flips");
        assert_eq!(view.stage, CapacityStage::Normal);
        assert_eq!(view.staging, "off");
        assert!(view.reasons.is_empty());
        assert!(view.probe_ok, "the probe still reports honestly");
        assert_eq!(controller.admission_check(1), AdmissionCheck::Admit);
        match controller.admission_check(2) {
            AdmissionCheck::Gate { stage, reasons, .. } => {
                assert_eq!(stage, CapacityStage::Normal);
                assert!(reasons.is_empty());
            }
            other => panic!("the count bound still gates under off: {other:?}"),
        }
    }

    #[test]
    fn tracker_worsens_immediately_and_walks_the_stages_in_order() {
        let mut tracker = StageTracker::default();
        let t0 = Instant::now();
        assert_eq!(
            tracker.observe(CapacityStage::Normal, t0),
            CapacityStage::Normal
        );
        assert_eq!(
            tracker.observe(CapacityStage::Defer, t0),
            CapacityStage::Defer
        );
        assert_eq!(
            tracker.observe(CapacityStage::Park, t0),
            CapacityStage::Park
        );
    }

    #[test]
    fn tracker_eases_only_after_the_dwell() {
        let mut tracker = StageTracker::default();
        let t0 = Instant::now();
        tracker.observe(CapacityStage::Park, t0);
        assert_eq!(
            tracker.observe(CapacityStage::Normal, t0),
            CapacityStage::Park
        );
        assert_eq!(
            tracker.observe(
                CapacityStage::Normal,
                t0 + EASE_DWELL - Duration::from_secs(1)
            ),
            CapacityStage::Park,
            "an ease inside the dwell must not apply"
        );
        assert_eq!(
            tracker.observe(CapacityStage::Normal, t0 + EASE_DWELL),
            CapacityStage::Normal
        );
    }

    #[test]
    fn tracker_flapping_restarts_the_dwell_and_holds_the_worse_stage() {
        let mut tracker = StageTracker::default();
        let t0 = Instant::now();
        tracker.observe(CapacityStage::Park, t0);
        tracker.observe(CapacityStage::Normal, t0 + Duration::from_secs(5));
        // A defer sample restarts the candidate at defer.
        tracker.observe(CapacityStage::Defer, t0 + Duration::from_secs(10));
        assert_eq!(
            tracker.observe(CapacityStage::Defer, t0 + Duration::from_secs(35)),
            CapacityStage::Park,
            "25s of sustained defer is inside the dwell"
        );
        // Sustained defer past the dwell eases park → defer, not → normal.
        assert_eq!(
            tracker.observe(CapacityStage::Defer, t0 + Duration::from_secs(41)),
            CapacityStage::Defer
        );
        // A re-worsening discards the pending ease outright.
        tracker.observe(CapacityStage::Park, t0 + Duration::from_secs(50));
        tracker.observe(CapacityStage::Normal, t0 + Duration::from_secs(55));
        tracker.observe(CapacityStage::Park, t0 + Duration::from_secs(60));
        assert_eq!(
            tracker.observe(CapacityStage::Normal, t0 + Duration::from_secs(89)),
            CapacityStage::Park,
            "the earlier normal run must not count after re-worsening"
        );
    }

    // Fail-open: probe absence is not distress — the stage returns to
    // normal (through the same dwell), the view says so honestly, and
    // the count bound keeps working without any probe.
    #[test]
    fn probe_unavailable_fails_open_and_keeps_the_count_bound() {
        let controller = CapacityController::new(2);
        let t0 = Instant::now();
        assert!(
            controller.observe(None, t0).is_none(),
            "probe-less normal is the initial view — no change to publish"
        );
        let view = controller.view();
        assert_eq!(view.stage, CapacityStage::Normal);
        assert!(!view.probe_ok);
        assert!(view.sample.is_none());
        assert_eq!(controller.admission_check(0), AdmissionCheck::Admit);
        assert_eq!(controller.admission_check(1), AdmissionCheck::Admit);
        match controller.admission_check(2) {
            AdmissionCheck::Gate {
                stage,
                bound,
                resident,
                ..
            } => {
                assert_eq!(stage, CapacityStage::Normal);
                assert_eq!(bound, 2);
                assert_eq!(resident, 2);
            }
            other => panic!("count bound must gate probe-less: {other:?}"),
        }
    }

    #[test]
    fn probe_outage_mid_pressure_releases_backpressure_after_the_dwell() {
        let controller = CapacityController::new(8);
        let t0 = Instant::now();
        let pressured = MemorySample {
            os_pressure_level: Some(4),
            ..sample()
        };
        let view = controller.observe(Some(pressured), t0).expect("changed");
        assert_eq!(view.stage, CapacityStage::Park);
        // Probe dies. The stage must not pin at park forever.
        let view = controller
            .observe(None, t0 + Duration::from_secs(5))
            .expect("probe_ok flips");
        assert_eq!(view.stage, CapacityStage::Park, "inside the dwell");
        assert!(!view.probe_ok);
        let view = controller
            .observe(None, t0 + Duration::from_secs(5) + EASE_DWELL)
            .expect("stage eases");
        assert_eq!(view.stage, CapacityStage::Normal);
        assert!(matches!(
            controller.admission_check(0),
            AdmissionCheck::Admit
        ));
    }

    #[test]
    fn defer_stage_gates_even_under_the_bound() {
        let controller = CapacityController::new(64);
        let pressured = MemorySample {
            psi_some_avg10: Some(15.0),
            ..sample()
        };
        controller.observe(Some(pressured), Instant::now());
        match controller.admission_check(1) {
            AdmissionCheck::Gate { stage, reasons, .. } => {
                assert_eq!(stage, CapacityStage::Defer);
                assert_eq!(
                    reasons,
                    vec!["memory stall 15% (PSI some)"],
                    "the gate carries the firing signal for the refusal"
                );
            }
            other => panic!("defer stage must gate admissions: {other:?}"),
        }
    }

    #[test]
    fn resolve_max_resident_precedence_and_clamps() {
        let gib = 1024 * 1024 * 1024u64;
        // Config wins over everything.
        assert_eq!(resolve_max_resident(Some(3), Some("9"), Some(64 * gib)), 3);
        assert_eq!(resolve_max_resident(Some(0), None, None), 1, "floor of 1");
        // Env wins over derivation; garbage env falls through.
        assert_eq!(resolve_max_resident(None, Some("9"), Some(64 * gib)), 9);
        assert_eq!(resolve_max_resident(None, Some("nope"), Some(64 * gib)), 64);
        // RAM-derived: one per GiB, clamped.
        assert_eq!(resolve_max_resident(None, None, Some(16 * gib)), 16);
        assert_eq!(
            resolve_max_resident(None, None, Some(2 * gib)),
            DERIVED_BOUND_MIN
        );
        assert_eq!(
            resolve_max_resident(None, None, Some(256 * gib)),
            DERIVED_BOUND_MAX
        );
        // Probe-less default.
        assert_eq!(
            resolve_max_resident(None, None, None),
            PROBELESS_DEFAULT_MAX_RESIDENT
        );
    }

    // Refusal/queue honesty: the messages name the bound, the count, the
    // queue, the firing signal(s), and carry the stable prefix.
    #[test]
    fn refusal_and_queue_texts_name_the_facts() {
        let reasons = vec!["memory pressure warn + compressor 18%".to_string()];
        let refusal = refusal_text("fork", CapacityStage::Defer, &reasons, 16, 16, 3);
        assert!(refusal.starts_with(CAPACITY_REFUSAL_PREFIX));
        assert!(refusal.contains("16 of 16"));
        assert!(refusal.contains("3 admission(s) queued"));
        assert!(refusal.contains("stage is defer (memory pressure warn + compressor 18%)"));
        let queued = queued_text("session create", 4, CapacityStage::Park, &reasons, 16, 16);
        assert!(queued.contains("position 4"));
        assert!(queued.contains("16 of 16"));
        assert!(queued.contains("stage park (memory pressure warn + compressor 18%)"));
        assert!(queued.contains("fires when headroom returns"));
        // Count-bound gating at a quiet normal has no signal to name:
        // no empty parenthetical noise.
        let refusal = refusal_text("fork", CapacityStage::Normal, &[], 16, 16, 0);
        assert!(refusal.contains("stage is normal with 16 of 16"));
        assert!(!refusal.contains("()"));
        let queued = queued_text("session create", 1, CapacityStage::Normal, &[], 16, 16);
        assert!(queued.contains("stage normal, 16 of 16"));
        assert!(!queued.contains("()"));
    }

    #[test]
    fn census_updates_publish_only_on_change() {
        let controller = CapacityController::new(4);
        let parked = vec!["s-1".to_string()];
        let row = || {
            vec![CapacityQueueRow {
                position: 1,
                kind: "create_session".to_string(),
                enqueued_ms: 7,
            }]
        };
        let view = controller
            .update_census(2, row(), &parked)
            .expect("changed");
        assert_eq!(view.resident, 2);
        assert_eq!(view.queued, 1);
        assert_eq!(view.queue, row());
        assert_eq!(view.parked_count, 1);
        assert!(!view.admissions_deferred);
        assert!(
            controller.update_census(2, row(), &parked).is_none(),
            "no change, no publish"
        );
        let view = controller
            .update_census(4, row(), &parked)
            .expect("at bound");
        assert!(
            view.admissions_deferred,
            "at-bound census defers admissions"
        );
    }
}

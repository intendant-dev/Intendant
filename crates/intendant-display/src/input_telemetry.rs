//! Lightweight display-input telemetry.
//!
//! Enabled only when `INTENDANT_DISPLAY_INPUT_TELEMETRY=1` (also accepts
//! `true`, `yes`, or `on`). The hot-path calls are no-ops when disabled.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ENABLE_ENV: &str = "INTENDANT_DISPLAY_INPUT_TELEMETRY";
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) fn record_data_channel_input(label: &str, bytes: usize) {
    let Some(telemetry) = telemetry() else {
        return;
    };
    match label {
        "control" => {
            telemetry.dc_control_events.fetch_add(1, Ordering::Relaxed);
            telemetry
                .dc_control_bytes
                .fetch_add(bytes as u64, Ordering::Relaxed);
        }
        "pointer" => {
            telemetry.dc_pointer_events.fetch_add(1, Ordering::Relaxed);
            telemetry
                .dc_pointer_bytes
                .fetch_add(bytes as u64, Ordering::Relaxed);
        }
        _ => {
            telemetry.dc_unknown_events.fetch_add(1, Ordering::Relaxed);
        }
    }
    telemetry.maybe_report();
}

pub(crate) fn record_input_parse_error() {
    let Some(telemetry) = telemetry() else {
        return;
    };
    telemetry.dc_parse_errors.fetch_add(1, Ordering::Relaxed);
    telemetry.maybe_report();
}

pub(crate) fn record_authority_drop(kind: &'static str) {
    let Some(telemetry) = telemetry() else {
        return;
    };
    telemetry.authority_drops.fetch_add(1, Ordering::Relaxed);
    telemetry.increment_kind(kind);
    telemetry.maybe_report();
}

pub(crate) fn record_queue_coalesced() {
    let Some(telemetry) = telemetry() else {
        return;
    };
    telemetry.queue_coalesced.fetch_add(1, Ordering::Relaxed);
    telemetry.maybe_report();
}

pub(crate) fn record_queue_dropped_continuous() {
    let Some(telemetry) = telemetry() else {
        return;
    };
    telemetry
        .queue_dropped_continuous
        .fetch_add(1, Ordering::Relaxed);
    telemetry.maybe_report();
}

pub(crate) fn record_queue_dropped_discrete() {
    let Some(telemetry) = telemetry() else {
        return;
    };
    telemetry
        .queue_dropped_discrete
        .fetch_add(1, Ordering::Relaxed);
    telemetry.maybe_report();
}

pub(crate) fn record_inject_started(kind: &'static str) {
    let Some(telemetry) = telemetry() else {
        return;
    };
    telemetry.inject_started.fetch_add(1, Ordering::Relaxed);
    telemetry.increment_kind(kind);
    telemetry.maybe_report();
}

pub(crate) fn record_inject_completed(elapsed: Duration) {
    let Some(telemetry) = telemetry() else {
        return;
    };
    telemetry.inject_ok.fetch_add(1, Ordering::Relaxed);
    let micros = duration_micros(elapsed);
    telemetry.inject_us_sum.fetch_add(micros, Ordering::Relaxed);
    fetch_max(&telemetry.inject_us_max, micros);
    telemetry.maybe_report();
}

pub(crate) fn record_inject_failed(elapsed: Duration) {
    let Some(telemetry) = telemetry() else {
        return;
    };
    telemetry.inject_err.fetch_add(1, Ordering::Relaxed);
    let micros = duration_micros(elapsed);
    telemetry.inject_us_sum.fetch_add(micros, Ordering::Relaxed);
    fetch_max(&telemetry.inject_us_max, micros);
    telemetry.maybe_report();
}

#[cfg(target_os = "macos")]
pub(crate) fn record_macos_cgevent_post(elapsed: Duration) {
    let Some(telemetry) = telemetry() else {
        return;
    };
    telemetry.macos_posts.fetch_add(1, Ordering::Relaxed);
    let micros = duration_micros(elapsed);
    telemetry
        .macos_post_us_sum
        .fetch_add(micros, Ordering::Relaxed);
    fetch_max(&telemetry.macos_post_us_max, micros);
    telemetry.maybe_report();
}

fn telemetry() -> Option<&'static InputTelemetry> {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    static TELEMETRY: OnceLock<InputTelemetry> = OnceLock::new();

    if !*ENABLED.get_or_init(input_telemetry_enabled) {
        return None;
    }
    Some(TELEMETRY.get_or_init(InputTelemetry::new))
}

fn input_telemetry_enabled() -> bool {
    std::env::var(ENABLE_ENV)
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

struct InputTelemetry {
    last_report: Mutex<Instant>,
    dc_control_events: AtomicU64,
    dc_control_bytes: AtomicU64,
    dc_pointer_events: AtomicU64,
    dc_pointer_bytes: AtomicU64,
    dc_unknown_events: AtomicU64,
    dc_parse_errors: AtomicU64,
    authority_drops: AtomicU64,
    queue_coalesced: AtomicU64,
    queue_dropped_continuous: AtomicU64,
    queue_dropped_discrete: AtomicU64,
    inject_started: AtomicU64,
    inject_ok: AtomicU64,
    inject_err: AtomicU64,
    inject_us_sum: AtomicU64,
    inject_us_max: AtomicU64,
    macos_posts: AtomicU64,
    macos_post_us_sum: AtomicU64,
    macos_post_us_max: AtomicU64,
    kind_kd: AtomicU64,
    kind_ku: AtomicU64,
    kind_md: AtomicU64,
    kind_mu: AtomicU64,
    kind_mm: AtomicU64,
    kind_sc: AtomicU64,
}

impl InputTelemetry {
    fn new() -> Self {
        Self {
            last_report: Mutex::new(Instant::now()),
            dc_control_events: AtomicU64::new(0),
            dc_control_bytes: AtomicU64::new(0),
            dc_pointer_events: AtomicU64::new(0),
            dc_pointer_bytes: AtomicU64::new(0),
            dc_unknown_events: AtomicU64::new(0),
            dc_parse_errors: AtomicU64::new(0),
            authority_drops: AtomicU64::new(0),
            queue_coalesced: AtomicU64::new(0),
            queue_dropped_continuous: AtomicU64::new(0),
            queue_dropped_discrete: AtomicU64::new(0),
            inject_started: AtomicU64::new(0),
            inject_ok: AtomicU64::new(0),
            inject_err: AtomicU64::new(0),
            inject_us_sum: AtomicU64::new(0),
            inject_us_max: AtomicU64::new(0),
            macos_posts: AtomicU64::new(0),
            macos_post_us_sum: AtomicU64::new(0),
            macos_post_us_max: AtomicU64::new(0),
            kind_kd: AtomicU64::new(0),
            kind_ku: AtomicU64::new(0),
            kind_md: AtomicU64::new(0),
            kind_mu: AtomicU64::new(0),
            kind_mm: AtomicU64::new(0),
            kind_sc: AtomicU64::new(0),
        }
    }

    fn increment_kind(&self, kind: &'static str) {
        match kind {
            "kd" => self.kind_kd.fetch_add(1, Ordering::Relaxed),
            "ku" => self.kind_ku.fetch_add(1, Ordering::Relaxed),
            "md" => self.kind_md.fetch_add(1, Ordering::Relaxed),
            "mu" => self.kind_mu.fetch_add(1, Ordering::Relaxed),
            "mm" => self.kind_mm.fetch_add(1, Ordering::Relaxed),
            "sc" => self.kind_sc.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
    }

    fn maybe_report(&self) {
        let now = Instant::now();
        let mut last = self.last_report.lock().unwrap_or_else(|e| e.into_inner());
        if now.duration_since(*last) < REPORT_INTERVAL {
            return;
        }
        *last = now;
        drop(last);

        let snapshot = self.take_snapshot();
        if snapshot.is_empty() {
            return;
        }

        eprintln!(
            "[display/input-telemetry] dc=control:{}/{}B pointer:{}/{}B unknown:{} parse_err:{} \
             auth_drop:{} queue=coalesced:{} drop_cont:{} drop_disc:{} \
             inject=start:{} ok:{} err:{} avg_us:{} max_us:{} \
             events=kd:{} ku:{} md:{} mu:{} mm:{} sc:{} \
             macos_post=n:{} avg_us:{} max_us:{}",
            snapshot.dc_control_events,
            snapshot.dc_control_bytes,
            snapshot.dc_pointer_events,
            snapshot.dc_pointer_bytes,
            snapshot.dc_unknown_events,
            snapshot.dc_parse_errors,
            snapshot.authority_drops,
            snapshot.queue_coalesced,
            snapshot.queue_dropped_continuous,
            snapshot.queue_dropped_discrete,
            snapshot.inject_started,
            snapshot.inject_ok,
            snapshot.inject_err,
            avg(
                snapshot.inject_us_sum,
                snapshot.inject_ok + snapshot.inject_err
            ),
            snapshot.inject_us_max,
            snapshot.kind_kd,
            snapshot.kind_ku,
            snapshot.kind_md,
            snapshot.kind_mu,
            snapshot.kind_mm,
            snapshot.kind_sc,
            snapshot.macos_posts,
            avg(snapshot.macos_post_us_sum, snapshot.macos_posts),
            snapshot.macos_post_us_max,
        );
    }

    fn take_snapshot(&self) -> InputTelemetrySnapshot {
        InputTelemetrySnapshot {
            dc_control_events: take(&self.dc_control_events),
            dc_control_bytes: take(&self.dc_control_bytes),
            dc_pointer_events: take(&self.dc_pointer_events),
            dc_pointer_bytes: take(&self.dc_pointer_bytes),
            dc_unknown_events: take(&self.dc_unknown_events),
            dc_parse_errors: take(&self.dc_parse_errors),
            authority_drops: take(&self.authority_drops),
            queue_coalesced: take(&self.queue_coalesced),
            queue_dropped_continuous: take(&self.queue_dropped_continuous),
            queue_dropped_discrete: take(&self.queue_dropped_discrete),
            inject_started: take(&self.inject_started),
            inject_ok: take(&self.inject_ok),
            inject_err: take(&self.inject_err),
            inject_us_sum: take(&self.inject_us_sum),
            inject_us_max: take(&self.inject_us_max),
            macos_posts: take(&self.macos_posts),
            macos_post_us_sum: take(&self.macos_post_us_sum),
            macos_post_us_max: take(&self.macos_post_us_max),
            kind_kd: take(&self.kind_kd),
            kind_ku: take(&self.kind_ku),
            kind_md: take(&self.kind_md),
            kind_mu: take(&self.kind_mu),
            kind_mm: take(&self.kind_mm),
            kind_sc: take(&self.kind_sc),
        }
    }
}

struct InputTelemetrySnapshot {
    dc_control_events: u64,
    dc_control_bytes: u64,
    dc_pointer_events: u64,
    dc_pointer_bytes: u64,
    dc_unknown_events: u64,
    dc_parse_errors: u64,
    authority_drops: u64,
    queue_coalesced: u64,
    queue_dropped_continuous: u64,
    queue_dropped_discrete: u64,
    inject_started: u64,
    inject_ok: u64,
    inject_err: u64,
    inject_us_sum: u64,
    inject_us_max: u64,
    macos_posts: u64,
    macos_post_us_sum: u64,
    macos_post_us_max: u64,
    kind_kd: u64,
    kind_ku: u64,
    kind_md: u64,
    kind_mu: u64,
    kind_mm: u64,
    kind_sc: u64,
}

impl InputTelemetrySnapshot {
    fn is_empty(&self) -> bool {
        self.dc_control_events
            + self.dc_pointer_events
            + self.dc_unknown_events
            + self.dc_parse_errors
            + self.authority_drops
            + self.queue_coalesced
            + self.queue_dropped_continuous
            + self.queue_dropped_discrete
            + self.inject_started
            + self.inject_ok
            + self.inject_err
            + self.macos_posts
            == 0
    }
}

fn take(value: &AtomicU64) -> u64 {
    value.swap(0, Ordering::Relaxed)
}

fn avg(sum: u64, count: u64) -> u64 {
    sum.checked_div(count).unwrap_or(0)
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn fetch_max(value: &AtomicU64, candidate: u64) {
    let mut current = value.load(Ordering::Relaxed);
    while candidate > current {
        match value.compare_exchange_weak(current, candidate, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

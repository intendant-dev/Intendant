//! Backpressure policy for supersedable tile-delta frames.
//!
//! D-4c keeps this deliberately event-driven. The rtc 0.9 wrapper
//! exposes buffered-amount high/low events, but not a public
//! `buffered_amount()` getter. The WebRTC driver sets the SCTP stream
//! thresholds and feeds those events into [`TileDeltaBackpressure`].
//! While throttled, only tile deltas are dropped; control and snapshot
//! channels keep their existing reliable behavior.

/// Pause sending tile deltas once the SCTP stream buffers 256 KiB.
pub const TILE_DELTAS_HIGH_WATERMARK_BYTES: usize = 256 * 1024;

/// Resume sending tile deltas once the SCTP stream drains to 64 KiB.
pub const TILE_DELTAS_LOW_WATERMARK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileDeltaSendDecision {
    Send,
    Drop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileDeltaBackpressureConfig {
    pub high_watermark_bytes: usize,
    pub low_watermark_bytes: usize,
}

impl Default for TileDeltaBackpressureConfig {
    fn default() -> Self {
        Self {
            high_watermark_bytes: TILE_DELTAS_HIGH_WATERMARK_BYTES,
            low_watermark_bytes: TILE_DELTAS_LOW_WATERMARK_BYTES,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TileDeltaBackpressureStats {
    pub sent_frames: u64,
    pub sent_bytes: u64,
    pub dropped_frames: u64,
    pub dropped_bytes: u64,
    pub high_events: u64,
    pub low_events: u64,
}

#[derive(Clone, Debug)]
pub struct TileDeltaBackpressure {
    config: TileDeltaBackpressureConfig,
    throttled: bool,
    stats: TileDeltaBackpressureStats,
}

impl TileDeltaBackpressure {
    pub fn new() -> Self {
        Self::with_config(TileDeltaBackpressureConfig::default())
    }

    pub fn with_config(config: TileDeltaBackpressureConfig) -> Self {
        Self {
            config: normalize_config(config),
            throttled: false,
            stats: TileDeltaBackpressureStats::default(),
        }
    }

    pub fn config(&self) -> TileDeltaBackpressureConfig {
        self.config
    }

    pub fn is_throttled(&self) -> bool {
        self.throttled
    }

    pub fn stats(&self) -> &TileDeltaBackpressureStats {
        &self.stats
    }

    pub fn reset(&mut self) {
        self.throttled = false;
    }

    /// Record that the SCTP stream crossed the high watermark.
    ///
    /// Returns true when this call changed the throttled state, which
    /// lets callers emit one transition log line without spamming.
    pub fn on_buffered_amount_high(&mut self) -> bool {
        self.stats.high_events = self.stats.high_events.saturating_add(1);
        let changed = !self.throttled;
        self.throttled = true;
        changed
    }

    /// Record that the SCTP stream drained below the low watermark.
    ///
    /// Returns true when this call changed the throttled state.
    pub fn on_buffered_amount_low(&mut self) -> bool {
        self.stats.low_events = self.stats.low_events.saturating_add(1);
        let changed = self.throttled;
        self.throttled = false;
        changed
    }

    pub fn decide_delta(&mut self, bytes: usize) -> TileDeltaSendDecision {
        if self.throttled {
            self.record_drop(bytes);
            TileDeltaSendDecision::Drop
        } else {
            TileDeltaSendDecision::Send
        }
    }

    pub fn record_delta_sent(&mut self, bytes: usize) {
        self.stats.sent_frames = self.stats.sent_frames.saturating_add(1);
        self.stats.sent_bytes = self.stats.sent_bytes.saturating_add(bytes as u64);
    }

    fn record_drop(&mut self, bytes: usize) {
        self.stats.dropped_frames = self.stats.dropped_frames.saturating_add(1);
        self.stats.dropped_bytes = self.stats.dropped_bytes.saturating_add(bytes as u64);
    }
}

impl Default for TileDeltaBackpressure {
    fn default() -> Self {
        Self::new()
    }
}

fn normalize_config(config: TileDeltaBackpressureConfig) -> TileDeltaBackpressureConfig {
    let high = config.high_watermark_bytes.max(1);
    let low = config.low_watermark_bytes.min(high);
    TileDeltaBackpressureConfig {
        high_watermark_bytes: high,
        low_watermark_bytes: low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_config() -> TileDeltaBackpressureConfig {
        TileDeltaBackpressureConfig {
            high_watermark_bytes: 100,
            low_watermark_bytes: 25,
        }
    }

    #[test]
    fn default_watermarks_match_design() {
        let policy = TileDeltaBackpressure::new();
        assert_eq!(
            policy.config().high_watermark_bytes,
            TILE_DELTAS_HIGH_WATERMARK_BYTES
        );
        assert_eq!(
            policy.config().low_watermark_bytes,
            TILE_DELTAS_LOW_WATERMARK_BYTES
        );
    }

    #[test]
    fn sends_until_high_event_arrives() {
        let mut policy = TileDeltaBackpressure::with_config(small_config());
        assert_eq!(policy.decide_delta(10), TileDeltaSendDecision::Send);
        policy.record_delta_sent(10);
        assert_eq!(policy.decide_delta(20), TileDeltaSendDecision::Send);
        policy.record_delta_sent(20);
        assert_eq!(policy.stats().sent_frames, 2);
        assert_eq!(policy.stats().sent_bytes, 30);
        assert_eq!(policy.stats().dropped_frames, 0);
    }

    #[test]
    fn high_event_enters_throttled_mode_once() {
        let mut policy = TileDeltaBackpressure::with_config(small_config());
        assert!(policy.on_buffered_amount_high());
        assert!(policy.is_throttled());
        assert!(!policy.on_buffered_amount_high());
        assert_eq!(policy.stats().high_events, 2);
    }

    #[test]
    fn throttled_mode_drops_supersedable_deltas() {
        let mut policy = TileDeltaBackpressure::with_config(small_config());
        policy.on_buffered_amount_high();
        assert_eq!(policy.decide_delta(64), TileDeltaSendDecision::Drop);
        assert_eq!(policy.decide_delta(32), TileDeltaSendDecision::Drop);
        assert_eq!(policy.stats().sent_frames, 0);
        assert_eq!(policy.stats().dropped_frames, 2);
        assert_eq!(policy.stats().dropped_bytes, 96);
    }

    #[test]
    fn low_event_resumes_delta_sends_once() {
        let mut policy = TileDeltaBackpressure::with_config(small_config());
        policy.on_buffered_amount_high();
        assert!(policy.on_buffered_amount_low());
        assert!(!policy.is_throttled());
        assert!(!policy.on_buffered_amount_low());
        assert_eq!(policy.decide_delta(8), TileDeltaSendDecision::Send);
        policy.record_delta_sent(8);
        assert_eq!(policy.stats().low_events, 2);
        assert_eq!(policy.stats().sent_frames, 1);
    }

    #[test]
    fn reset_clears_throttle_but_keeps_counters() {
        let mut policy = TileDeltaBackpressure::with_config(small_config());
        policy.on_buffered_amount_high();
        assert_eq!(policy.decide_delta(7), TileDeltaSendDecision::Drop);
        policy.reset();
        assert!(!policy.is_throttled());
        assert_eq!(policy.decide_delta(5), TileDeltaSendDecision::Send);
        policy.record_delta_sent(5);
        assert_eq!(policy.stats().dropped_bytes, 7);
        assert_eq!(policy.stats().sent_bytes, 5);
    }

    #[test]
    fn config_normalization_keeps_low_at_or_below_high() {
        let policy = TileDeltaBackpressure::with_config(TileDeltaBackpressureConfig {
            high_watermark_bytes: 10,
            low_watermark_bytes: 99,
        });
        assert_eq!(policy.config().high_watermark_bytes, 10);
        assert_eq!(policy.config().low_watermark_bytes, 10);
    }
}

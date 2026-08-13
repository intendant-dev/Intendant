//! Voice input: hold-to-talk capture through the dashboard's existing
//! server-side transcription lane, previewed for deliberate confirm.
//!
//! The talk pill is a THIRD hold semantic, kept visually and mechanically
//! distinct from the other two: a quick pinch selects, the 900 ms
//! confirm-hold approves — and the talk hold RECORDS. Pinch-and-hold the
//! pill and the hold is the recording window; release stops it. It never
//! fires on a timer and never cancels on aim drift (your hand wanders
//! while you speak); releasing the pinch is the only stop, so the mic can
//! never stay hot behind your back.
//!
//! The capture path is the dashboard's existing one end to end: the JS
//! glue (`ui2-xr.js` voice section) streams mic PCM over the page's
//! `user_audio` lane into the daemon's Whisper transcription
//! (`transcription.rs`, `[transcription] enabled = true`), and the
//! transcript comes back on the broadcast `user_transcript` event. That
//! lane only logs — nothing daemon-side injects it into any conversation
//! — so capture needs no presence pipeline changes. The wasm side here
//! is a pure state machine: Idle → Listening (press) → Transcribing
//! (release) → Result (transcript) → consumed or discarded.
//!
//! The result is NEVER auto-sent. It lands on a preview strip with
//! use/discard pills; "use" emits `{type:'text_commit', field_id, text}`
//! through the same action router every other XR act uses, and the JS
//! glue routes it through the dashboard's existing focus + composer
//! submit path. (`field_id` is `composer:<sessionId>` — the reconcile
//! point with the ray-keyboard seat's TextEntry contract: when its
//! facade lands, the strip's commit collapses into that buffer's commit
//! path and this shape must keep matching.)
//!
//! Honesty: transcription unavailable — config off, mic denied, hosted
//! Connect lane, dead event stream — renders as a visible status line on
//! the pill, pushed by the JS glue which owns the truth. Never a silent
//! no-op. Mic permission is requested on the FIRST talk press, never at
//! session entry.

use serde::Deserialize;
use wasm_bindgen::JsValue;

use crate::atlas::TextMeasure;
use crate::kit::{self, HitKind, HitTarget, PanelInstance, SceneBatches, TextAlign, TextRun};
use crate::math::Panel;
use crate::Inner;

/// Releases shorter than this are treated as an accidental pinch, not a
/// recording: the capture cancels and the pill teaches the gesture.
pub(crate) const MIN_TALK_MS: f64 = 300.0;
/// Backstop for a glue that never resolves a release (page-side crash):
/// the pill must not stick at "transcribing…" forever. The JS glue's own
/// no-speech timeout (~9 s) normally fires long before this.
pub(crate) const TRANSCRIBE_TIMEOUT_MS: f64 = 20_000.0;
/// Pulse period for the listening ring (ms).
const PULSE_MS: f64 = 1400.0;

/// Quick-release hint (the accidental-pinch teaching line).
const HINT_HOLD: &str = "hold to talk — keep pinching while you speak";
/// Result-strip hint when no session is focused to send to.
const HINT_NO_TARGET: &str = "select a session to send to";
/// Transcribe-backstop line.
const NOTE_TIMEOUT: &str = "transcription timed out — try again";

/// Talk-lane phase. `Listening`/`Transcribing` carry their start time
/// (the frame/`performance.now()` clock) for the min-hold check, the
/// pulse, and the transcribe backstop.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) enum TalkPhase {
    #[default]
    Idle,
    Listening {
        since_ms: f64,
    },
    Transcribing {
        since_ms: f64,
    },
    Result {
        text: String,
    },
}

/// Commands the state machine hands back for the JS capture lane. Emitted
/// through the ordinary action router as `{type:'voice_talk', phase}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VoiceCmd {
    Start,
    Stop,
    Cancel,
}

impl VoiceCmd {
    pub(crate) fn payload(self) -> serde_json::Value {
        let phase = match self {
            VoiceCmd::Start => "start",
            VoiceCmd::Stop => "stop",
            VoiceCmd::Cancel => "cancel",
        };
        serde_json::json!({ "type": "voice_talk", "phase": phase })
    }
}

/// Standing availability pushed by the JS glue (which owns the truth:
/// daemon config, transport posture, mic permission). Field names follow
/// the wire convention (camelCase); unknown fields are ignored.
#[derive(Clone, Deserialize, Debug)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct VoiceAvailability {
    pub(crate) available: bool,
    /// The honest reason while unavailable — rendered verbatim under the
    /// pill.
    pub(crate) detail: String,
}

impl Default for VoiceAvailability {
    fn default() -> Self {
        Self {
            available: true,
            detail: String::new(),
        }
    }
}

/// Facade-owned voice state living in [`Inner`]. Host-constructible and
/// pure — every transition below is exercised by inline tests.
#[derive(Default)]
pub(crate) struct VoiceDock {
    pub(crate) phase: TalkPhase,
    /// `None` until the JS glue reports; `Some(status)` after.
    pub(crate) availability: Option<VoiceAvailability>,
    /// Transient line from the last failed/cancelled capture ("no speech
    /// recognized…", the quick-release hint). Cleared on the next press.
    pub(crate) note: String,
    /// Malformed `voiceStatus` pushes, dropped and counted.
    pub(crate) parse_errors: u64,
}

impl VoiceDock {
    pub(crate) fn phase_name(&self) -> &'static str {
        match self.phase {
            TalkPhase::Idle => "idle",
            TalkPhase::Listening { .. } => "listening",
            TalkPhase::Transcribing { .. } => "transcribing",
            TalkPhase::Result { .. } => "result",
        }
    }

    pub(crate) fn result_text(&self) -> Option<&str> {
        match &self.phase {
            TalkPhase::Result { text } => Some(text.as_str()),
            _ => None,
        }
    }
}

// ---- transitions (pure) --------------------------------------------------

/// Talk-pill pinch begins. Idle and Result both start a fresh capture
/// (pressing talk with a pending result deliberately re-records over
/// it). Already listening/transcribing: no-op.
pub(crate) fn on_press(dock: &mut VoiceDock, now_ms: f64) -> Option<VoiceCmd> {
    match dock.phase {
        TalkPhase::Idle | TalkPhase::Result { .. } => {
            dock.phase = TalkPhase::Listening { since_ms: now_ms };
            dock.note.clear();
            Some(VoiceCmd::Start)
        }
        _ => None,
    }
}

/// Talk-pill pinch ends. A hold under [`MIN_TALK_MS`] reads as an
/// accidental pinch and cancels with the teaching hint; `deliberate`
/// (activation-by-name — automation/accessibility) skips that check
/// because a named activation is already the deliberate act.
pub(crate) fn on_release(dock: &mut VoiceDock, now_ms: f64, deliberate: bool) -> Option<VoiceCmd> {
    match dock.phase {
        TalkPhase::Listening { since_ms } => {
            if !deliberate && now_ms - since_ms < MIN_TALK_MS {
                dock.phase = TalkPhase::Idle;
                dock.note = HINT_HOLD.to_string();
                Some(VoiceCmd::Cancel)
            } else {
                dock.phase = TalkPhase::Transcribing { since_ms: now_ms };
                Some(VoiceCmd::Stop)
            }
        }
        _ => None,
    }
}

/// A transcript arrived from the JS lane. Accepted while listening or
/// transcribing (a fast daemon can beat the release bookkeeping) and as
/// a replacement for an unconsumed result. Empty text folds to a failed
/// capture rather than an empty preview.
pub(crate) fn apply_result(dock: &mut VoiceDock, text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        apply_failed(dock, "no speech recognized — try again");
        return false;
    }
    match dock.phase {
        TalkPhase::Idle => false,
        _ => {
            dock.phase = TalkPhase::Result {
                text: text.to_string(),
            };
            true
        }
    }
}

/// The capture attempt ended without a transcript (mic denied, no
/// speech, lane down). Back to idle with the reason rendered under the
/// pill until the next press.
pub(crate) fn apply_failed(dock: &mut VoiceDock, message: &str) {
    dock.phase = TalkPhase::Idle;
    dock.note = if message.trim().is_empty() {
        "voice capture failed".to_string()
    } else {
        message.trim().to_string()
    };
}

/// Standing availability from the JS glue; independent of phase.
pub(crate) fn apply_availability(dock: &mut VoiceDock, availability: VoiceAvailability) {
    dock.availability = Some(availability);
}

/// Per-frame backstop: a transcribe wait the glue never resolves folds
/// back to idle with an honest line. Returns true when state changed.
pub(crate) fn tick(dock: &mut VoiceDock, now_ms: f64) -> bool {
    if let TalkPhase::Transcribing { since_ms } = dock.phase {
        if now_ms - since_ms > TRANSCRIBE_TIMEOUT_MS {
            dock.phase = TalkPhase::Idle;
            dock.note = NOTE_TIMEOUT.to_string();
            return true;
        }
    }
    false
}

/// Discard the pending result (the strip's discard pill).
pub(crate) fn on_discard(dock: &mut VoiceDock) -> bool {
    if matches!(dock.phase, TalkPhase::Result { .. }) {
        dock.phase = TalkPhase::Idle;
        true
    } else {
        false
    }
}

/// The commit payload for the strip's use pill: the pending transcript
/// aimed at the focused session's composer. `None` without a result or a
/// focused session (the strip renders the missing-target hint instead of
/// a use pill). The shape is the text-entry contract:
/// `{type:'text_commit', field_id: 'composer:<sessionId>', text}`.
pub(crate) fn commit_payload(inner: &Inner) -> Option<serde_json::Value> {
    let text = inner.voice.result_text()?.to_string();
    let sid = selected_session(inner)?;
    Some(serde_json::json!({
        "type": "text_commit",
        "field_id": format!("composer:{sid}"),
        "text": text,
    }))
}

/// The XR-local selection, filtered to real session cards (agenda-rail
/// selections share `selected_id` but are not sendable targets).
pub(crate) fn selected_session(inner: &Inner) -> Option<String> {
    let sid = inner.selected_id.as_deref()?;
    let model = inner.model.as_ref()?;
    model
        .agents
        .iter()
        .find(|a| a.id == sid)
        .map(|a| a.id.clone())
}

// ---- facade entry points -------------------------------------------------

/// Ingest a `voiceStatus` push. Parse failures keep the previous state
/// and count — the seam must never take the session down.
pub(crate) fn apply_status_js(inner: &mut Inner, status: JsValue) {
    match serde_wasm_bindgen::from_value::<VoiceAvailability>(status) {
        Ok(availability) => {
            apply_availability(&mut inner.voice, availability);
            inner.ui_dirty = true;
        }
        Err(_) => inner.voice.parse_errors += 1,
    }
}

// ---- scene build ---------------------------------------------------------

/// Pure snapshot of dock state for one scene build.
#[derive(Clone, Debug, Default)]
pub(crate) struct DockView {
    pub(crate) phase: TalkPhase,
    pub(crate) availability: Option<VoiceAvailability>,
    pub(crate) note: String,
}

impl VoiceDock {
    pub(crate) fn dock_view(&self) -> DockView {
        DockView {
            phase: self.phase.clone(),
            availability: self.availability.clone(),
            note: self.note.clone(),
        }
    }
}

fn dim(c: [f32; 4], f: f32) -> [f32; 4] {
    [c[0], c[1], c[2], c[3] * f]
}

/// Co-planarity lifts, matching `ui.rs` (panel < decor < text).
const LIFT_DECOR: f32 = 0.0018;
const LIFT_TEXT: f32 = 0.0036;

fn side_basis(
    az: f32,
    dist: f32,
    y: f32,
) -> (crate::math::Vec3, crate::math::Vec3, crate::math::Vec3) {
    use crate::math::v3;
    (
        v3(dist * az.sin(), y, -dist * az.cos()),
        v3(az.cos(), 0.0, az.sin()),
        v3(0.0, 1.0, 0.0),
    )
}

fn lift(
    p: crate::math::Vec3,
    right: crate::math::Vec3,
    up: crate::math::Vec3,
    amount: f32,
    floor_y: f32,
) -> crate::math::Vec3 {
    use crate::math::v3;
    let n = right.cross(up).normalize();
    let lifted = p + n.scale(amount);
    v3(lifted.x, lifted.y + floor_y, lifted.z)
}

fn at_floor(p: crate::math::Vec3, floor_y: f32) -> crate::math::Vec3 {
    crate::math::v3(p.x, p.y + floor_y, p.z)
}

/// Append the voice affordances to a built scene: the talk pill always,
/// the honest status line whenever there is one, the preview strip while
/// a result is pending. Called from the frame loop's scene rebuild after
/// `terminal::build_pane`, into the same batches.
pub(crate) fn build_dock(
    dock: &DockView,
    selected_session: Option<&str>,
    hover_id: Option<&str>,
    time_ms: f64,
    floor_y: f32,
    measure: &dyn TextMeasure,
    out: &mut SceneBatches,
) {
    let (center, right, up) =
        side_basis(kit::VOICE_PILL_AZ, kit::VOICE_PILL_DIST, kit::VOICE_PILL_Y);
    let unavailable = dock.availability.as_ref().is_some_and(|a| !a.available);

    // Phase → the pill's accent + label. The label is rendered state, not
    // a tooltip: a headset has no hover text.
    let (accent, label): ([f32; 4], &str) = match &dock.phase {
        TalkPhase::Idle => (kit::LINE_2, "talk"),
        TalkPhase::Listening { .. } => (kit::GREEN, "listening…"),
        TalkPhase::Transcribing { .. } => (kit::AMBER, "transcribing…"),
        TalkPhase::Result { .. } => (kit::IRIS, "ready"),
    };
    let text_h = 0.021;
    let dot_r = 0.006;
    let dot_gap = 0.012;
    let text_w = measure.measure(label, text_h);
    let pill_hw = ((text_w + dot_r * 2.0 + dot_gap) / 2.0 + 0.024).max(0.055);
    let pill_hh = 0.021;
    let is_hover = hover_id == Some("voice:talk");
    let active = !matches!(dock.phase, TalkPhase::Idle);

    out.panels.push(PanelInstance {
        center: at_floor(center, floor_y),
        right,
        up,
        half_w: pill_hw,
        half_h: pill_hh,
        radius: pill_hh,
        fill: if active {
            dim(accent, 0.16)
        } else if is_hover {
            dim(kit::IRIS, 0.30)
        } else {
            kit::SURFACE
        },
        border: if active {
            accent
        } else if is_hover {
            kit::IRIS
        } else if unavailable {
            dim(kit::AMBER, 0.7)
        } else {
            kit::LINE_2
        },
        border_w: if active || is_hover { 0.0035 } else { 0.0025 },
    });

    // Listening/transcribing pulse: a ring band around the pill breathing
    // on a fixed period — recording is a state you can SEE from across
    // the room, not an easily missed border tint.
    if let TalkPhase::Listening { .. } | TalkPhase::Transcribing { .. } = dock.phase {
        let period = (time_ms % PULSE_MS) / PULSE_MS;
        let wave = (period * std::f64::consts::TAU).sin() as f32 * 0.5 + 0.5;
        let grow = 0.006 + 0.006 * wave;
        out.panels.push(PanelInstance {
            center: lift(center, right, up, -LIFT_DECOR, floor_y),
            right,
            up,
            half_w: pill_hw + grow,
            half_h: pill_hh + grow,
            radius: pill_hh + grow,
            fill: [0.0; 4],
            border: dim(accent, 0.25 + 0.45 * wave),
            border_w: 0.0035,
        });
    }

    // Mic dot: state accent while active, availability while idle.
    let content_hw = (dot_r * 2.0 + dot_gap + text_w) / 2.0;
    out.panels.push(PanelInstance {
        center: lift(
            center + right.scale(-content_hw + dot_r),
            right,
            up,
            LIFT_DECOR,
            floor_y,
        ),
        right,
        up,
        half_w: dot_r,
        half_h: dot_r,
        radius: dot_r,
        fill: if active {
            accent
        } else if unavailable {
            kit::AMBER
        } else {
            kit::TEXT_3
        },
        border: [0.0; 4],
        border_w: 0.0,
    });
    out.texts.push(TextRun {
        origin: lift(
            center + right.scale(-content_hw + dot_r * 2.0 + dot_gap) - up.scale(0.008),
            right,
            up,
            LIFT_TEXT,
            floor_y,
        ),
        right,
        up,
        height: text_h,
        color: if active { accent } else { kit::TEXT_2 },
        align: TextAlign::Left,
        max_width: pill_hw * 2.0,
        text: label.to_string(),
    });
    out.hits.push(HitTarget {
        id: "voice:talk".to_string(),
        kind: HitKind::VoiceTalk,
        agent_id: String::new(),
        panel: Panel {
            center: at_floor(center, floor_y),
            right,
            up,
            half_w: pill_hw,
            half_h: pill_hh,
        },
    });

    // The honesty line under the pill: the unavailability reason wins
    // (standing state), else the last capture note (transient until the
    // next press). Rendered text — never a silent no-op.
    let status_line = dock
        .availability
        .as_ref()
        .filter(|a| !a.available)
        .map(|a| {
            if a.detail.trim().is_empty() {
                "voice input unavailable".to_string()
            } else {
                a.detail.trim().to_string()
            }
        })
        .or_else(|| {
            if dock.note.trim().is_empty() {
                None
            } else {
                Some(dock.note.trim().to_string())
            }
        });
    if let Some(line) = status_line {
        out.texts.push(TextRun {
            origin: lift(
                center - up.scale(pill_hh + 0.030),
                right,
                up,
                LIFT_TEXT,
                floor_y,
            ),
            right,
            up,
            height: 0.019,
            color: if unavailable { kit::AMBER } else { kit::TEXT_2 },
            align: TextAlign::Center,
            max_width: 0.52,
            text: line,
        });
    }

    if let TalkPhase::Result { text } = &dock.phase {
        build_strip(text, selected_session, hover_id, floor_y, measure, out);
    }
}

/// The preview strip: captured transcript + use/discard pills. The
/// deliberate-confirm surface — text is reviewed here and only a pinch
/// on "use" sends it anywhere.
fn build_strip(
    text: &str,
    selected_session: Option<&str>,
    hover_id: Option<&str>,
    floor_y: f32,
    measure: &dyn TextMeasure,
    out: &mut SceneBatches,
) {
    use crate::math::v3;
    let center = v3(0.0, kit::VOICE_STRIP_Y, -kit::VOICE_STRIP_DIST);
    let right = v3(1.0, 0.0, 0.0);
    let up = v3(0.0, 1.0, 0.0);
    let hw = kit::VOICE_STRIP_HALF_W;
    let hh = kit::VOICE_STRIP_HALF_H;
    let pad = 0.020;

    out.panels.push(PanelInstance {
        center: at_floor(center, floor_y),
        right,
        up,
        half_w: hw,
        half_h: hh,
        radius: 0.016,
        fill: kit::SURFACE_2,
        border: kit::IRIS,
        border_w: 0.0025,
    });
    // Caption: what this surface is (review, then decide).
    out.texts.push(TextRun {
        origin: lift(
            center + right.scale(-hw + pad) + up.scale(hh - 0.017),
            right,
            up,
            LIFT_TEXT,
            floor_y,
        ),
        right,
        up,
        height: 0.014,
        color: kit::TEXT_3,
        align: TextAlign::Left,
        max_width: hw * 2.0 - pad * 2.0,
        text: "voice transcript — review, then use".to_string(),
    });
    // The transcript itself.
    out.texts.push(TextRun {
        origin: lift(
            center + right.scale(-hw + pad) + up.scale(0.004),
            right,
            up,
            LIFT_TEXT,
            floor_y,
        ),
        right,
        up,
        height: 0.020,
        color: kit::TEXT,
        align: TextAlign::Left,
        max_width: hw * 2.0 - pad * 2.0,
        text: text.to_string(),
    });

    // Bottom row: use/discard pills right-aligned; the missing-target
    // hint replaces the use pill when nothing is focused.
    let pill_y = -hh + 0.024;
    let pill_hh = 0.017;
    let label_h = 0.018;
    let mut pen = hw - pad;
    let pill = |label: &str,
                id: &str,
                kind: HitKind,
                color: [f32; 4],
                pen: &mut f32,
                out: &mut SceneBatches| {
        let pill_hw = (measure.measure(label, label_h) / 2.0 + 0.018).max(0.040);
        let x = *pen - pill_hw;
        *pen = x - pill_hw - 0.018;
        let pcenter = center + right.scale(x) + up.scale(pill_y);
        let is_hover = hover_id == Some(id);
        out.panels.push(PanelInstance {
            center: lift(pcenter, right, up, LIFT_DECOR, floor_y),
            right,
            up,
            half_w: pill_hw,
            half_h: pill_hh,
            radius: pill_hh,
            fill: if is_hover {
                dim(color, 0.30)
            } else {
                kit::SURFACE
            },
            border: color,
            border_w: if is_hover { 0.0035 } else { 0.0025 },
        });
        out.texts.push(TextRun {
            origin: lift(
                pcenter - up.scale(0.007),
                right,
                up,
                LIFT_TEXT + LIFT_DECOR,
                floor_y,
            ),
            right,
            up,
            height: label_h,
            color,
            align: TextAlign::Center,
            max_width: pill_hw * 2.0 - 0.008,
            text: label.to_string(),
        });
        out.hits.push(HitTarget {
            id: id.to_string(),
            kind,
            agent_id: String::new(),
            panel: Panel {
                center: at_floor(pcenter, floor_y),
                right,
                up,
                half_w: pill_hw,
                half_h: pill_hh,
            },
        });
    };

    pill(
        "discard",
        "voice:discard",
        HitKind::VoiceDiscard,
        kit::TEXT_2,
        &mut pen,
        out,
    );
    if selected_session.is_some() {
        pill(
            "use",
            "voice:use",
            HitKind::VoiceUse,
            kit::GREEN,
            &mut pen,
            out,
        );
    } else {
        out.texts.push(TextRun {
            origin: lift(
                center + right.scale(-hw + pad) + up.scale(pill_y - 0.006),
                right,
                up,
                LIFT_TEXT,
                floor_y,
            ),
            right,
            up,
            height: 0.016,
            color: kit::AMBER,
            align: TextAlign::Left,
            max_width: hw * 2.0 - pad * 2.0 - 0.12,
            text: HINT_NO_TARGET.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::ApproxMeasure;

    fn ids(out: &SceneBatches) -> Vec<String> {
        out.hits.iter().map(|h| h.id.clone()).collect()
    }

    #[test]
    fn press_starts_listening_and_clears_the_note() {
        let mut dock = VoiceDock {
            note: "old note".into(),
            ..Default::default()
        };
        assert_eq!(on_press(&mut dock, 1000.0), Some(VoiceCmd::Start));
        assert_eq!(dock.phase_name(), "listening");
        assert!(dock.note.is_empty());
        // A second press while listening is a no-op.
        assert_eq!(on_press(&mut dock, 1100.0), None);
        assert_eq!(dock.phase_name(), "listening");
    }

    #[test]
    fn quick_release_cancels_with_the_teaching_hint() {
        let mut dock = VoiceDock::default();
        on_press(&mut dock, 1000.0);
        assert_eq!(
            on_release(&mut dock, 1000.0 + MIN_TALK_MS - 1.0, false),
            Some(VoiceCmd::Cancel)
        );
        assert_eq!(dock.phase_name(), "idle");
        assert_eq!(dock.note, HINT_HOLD);
    }

    #[test]
    fn held_release_moves_to_transcribing() {
        let mut dock = VoiceDock::default();
        on_press(&mut dock, 1000.0);
        assert_eq!(
            on_release(&mut dock, 1000.0 + MIN_TALK_MS + 50.0, false),
            Some(VoiceCmd::Stop)
        );
        assert_eq!(dock.phase_name(), "transcribing");
        // Release with no live hold is a no-op.
        assert_eq!(on_release(&mut dock, 9000.0, false), None);
    }

    #[test]
    fn deliberate_release_skips_the_min_hold() {
        let mut dock = VoiceDock::default();
        on_press(&mut dock, 1000.0);
        assert_eq!(on_release(&mut dock, 1001.0, true), Some(VoiceCmd::Stop));
        assert_eq!(dock.phase_name(), "transcribing");
    }

    #[test]
    fn result_lands_and_is_consumed_or_discarded() {
        let mut dock = VoiceDock::default();
        on_press(&mut dock, 0.0);
        on_release(&mut dock, 1000.0, false);
        assert!(apply_result(&mut dock, "  open the logs  "));
        assert_eq!(dock.result_text(), Some("open the logs"));
        assert!(on_discard(&mut dock));
        assert_eq!(dock.phase_name(), "idle");
        assert!(!on_discard(&mut dock));
    }

    #[test]
    fn result_can_land_while_still_listening() {
        // A fast daemon chunk can beat the release bookkeeping; the
        // machine accepts it rather than dropping speech on the floor.
        let mut dock = VoiceDock::default();
        on_press(&mut dock, 0.0);
        assert!(apply_result(&mut dock, "quick words"));
        assert_eq!(dock.phase_name(), "result");
        // The now-stale release is a no-op against a landed result.
        assert_eq!(on_release(&mut dock, 5000.0, false), None);
        assert_eq!(dock.result_text(), Some("quick words"));
    }

    #[test]
    fn pressing_talk_over_a_result_rerecords() {
        let mut dock = VoiceDock::default();
        on_press(&mut dock, 0.0);
        on_release(&mut dock, 1000.0, false);
        apply_result(&mut dock, "first take");
        assert_eq!(on_press(&mut dock, 2000.0), Some(VoiceCmd::Start));
        assert_eq!(dock.phase_name(), "listening");
        assert_eq!(dock.result_text(), None);
    }

    #[test]
    fn empty_result_folds_to_a_failed_capture() {
        let mut dock = VoiceDock::default();
        on_press(&mut dock, 0.0);
        on_release(&mut dock, 1000.0, false);
        assert!(!apply_result(&mut dock, "   "));
        assert_eq!(dock.phase_name(), "idle");
        assert!(dock.note.contains("no speech"));
    }

    #[test]
    fn ignored_result_when_idle() {
        let mut dock = VoiceDock::default();
        assert!(!apply_result(&mut dock, "stray transcript"));
        assert_eq!(dock.phase_name(), "idle");
    }

    #[test]
    fn failed_capture_lands_the_reason() {
        let mut dock = VoiceDock::default();
        on_press(&mut dock, 0.0);
        apply_failed(&mut dock, "mic denied: NotAllowedError");
        assert_eq!(dock.phase_name(), "idle");
        assert_eq!(dock.note, "mic denied: NotAllowedError");
        apply_failed(&mut dock, "   ");
        assert_eq!(dock.note, "voice capture failed");
    }

    #[test]
    fn transcribe_backstop_times_out() {
        let mut dock = VoiceDock::default();
        on_press(&mut dock, 0.0);
        on_release(&mut dock, 1000.0, false);
        assert!(!tick(&mut dock, 1000.0 + TRANSCRIBE_TIMEOUT_MS - 1.0));
        assert_eq!(dock.phase_name(), "transcribing");
        assert!(tick(&mut dock, 1000.0 + TRANSCRIBE_TIMEOUT_MS + 1.0));
        assert_eq!(dock.phase_name(), "idle");
        assert_eq!(dock.note, NOTE_TIMEOUT);
        // Idle ticks are inert.
        assert!(!tick(&mut dock, 99_999_999.0));
    }

    #[test]
    fn commit_payload_needs_a_result_and_a_real_session() {
        let mut inner = Inner::new();
        // No result → no payload.
        assert!(commit_payload(&inner).is_none());

        on_press(&mut inner.voice, 0.0);
        on_release(&mut inner.voice, 1000.0, false);
        apply_result(&mut inner.voice, "ship the fix");
        // Result but no selection → still none.
        assert!(commit_payload(&inner).is_none());

        let snap: crate::model::XrSnapshot = serde_json::from_value(serde_json::json!({
            "hosts": [{ "id": "local", "name": "local", "connected": true }],
            "agents": [{ "id": "sess-1", "hostId": "local", "status": "running" }],
        }))
        .unwrap();
        inner.model = Some(snap);
        // Agenda-rail selections are not sendable targets.
        inner.selected_id = Some("agenda:item-1".into());
        assert!(commit_payload(&inner).is_none());

        inner.selected_id = Some("sess-1".into());
        let payload = commit_payload(&inner).expect("payload");
        assert_eq!(payload["type"], "text_commit");
        assert_eq!(payload["field_id"], "composer:sess-1");
        assert_eq!(payload["text"], "ship the fix");
    }

    #[test]
    fn build_dock_always_offers_the_talk_pill() {
        let dock = DockView::default();
        let mut out = SceneBatches::default();
        build_dock(&dock, None, None, 0.0, 0.0, &ApproxMeasure, &mut out);
        assert!(ids(&out).contains(&"voice:talk".to_string()));
        assert!(!ids(&out).contains(&"voice:use".to_string()));
        assert!(out.texts.iter().any(|t| t.text == "talk"));
    }

    #[test]
    fn build_dock_renders_the_unavailability_line() {
        let dock = DockView {
            availability: Some(VoiceAvailability {
                available: false,
                detail: "transcription is off on this daemon".into(),
            }),
            ..Default::default()
        };
        let mut out = SceneBatches::default();
        build_dock(&dock, None, None, 0.0, 0.0, &ApproxMeasure, &mut out);
        assert!(out
            .texts
            .iter()
            .any(|t| t.text == "transcription is off on this daemon"));
        // Blank detail still renders an honest generic line.
        let dock = DockView {
            availability: Some(VoiceAvailability {
                available: false,
                detail: "  ".into(),
            }),
            ..Default::default()
        };
        let mut out = SceneBatches::default();
        build_dock(&dock, None, None, 0.0, 0.0, &ApproxMeasure, &mut out);
        assert!(out
            .texts
            .iter()
            .any(|t| t.text == "voice input unavailable"));
    }

    #[test]
    fn build_dock_renders_the_capture_note() {
        let dock = DockView {
            note: HINT_HOLD.into(),
            ..Default::default()
        };
        let mut out = SceneBatches::default();
        build_dock(&dock, None, None, 0.0, 0.0, &ApproxMeasure, &mut out);
        assert!(out.texts.iter().any(|t| t.text == HINT_HOLD));
    }

    #[test]
    fn result_strip_offers_use_only_with_a_target() {
        let dock = DockView {
            phase: TalkPhase::Result {
                text: "open the build logs".into(),
            },
            ..Default::default()
        };
        let mut out = SceneBatches::default();
        build_dock(
            &dock,
            Some("sess-1"),
            None,
            0.0,
            0.0,
            &ApproxMeasure,
            &mut out,
        );
        let with_target = ids(&out);
        assert!(with_target.contains(&"voice:use".to_string()));
        assert!(with_target.contains(&"voice:discard".to_string()));
        assert!(out.texts.iter().any(|t| t.text == "open the build logs"));

        let mut out = SceneBatches::default();
        build_dock(&dock, None, None, 0.0, 0.0, &ApproxMeasure, &mut out);
        let without = ids(&out);
        assert!(!without.contains(&"voice:use".to_string()));
        assert!(without.contains(&"voice:discard".to_string()));
        assert!(out.texts.iter().any(|t| t.text == HINT_NO_TARGET));
    }

    #[test]
    fn listening_pulse_adds_the_ring() {
        let dock = DockView {
            phase: TalkPhase::Listening { since_ms: 0.0 },
            ..Default::default()
        };
        let mut base = SceneBatches::default();
        build_dock(
            &DockView::default(),
            None,
            None,
            0.0,
            0.0,
            &ApproxMeasure,
            &mut base,
        );
        let mut live = SceneBatches::default();
        build_dock(&dock, None, None, 350.0, 0.0, &ApproxMeasure, &mut live);
        assert!(live.panels.len() > base.panels.len(), "pulse ring present");
        assert!(live.texts.iter().any(|t| t.text == "listening…"));
    }

    #[test]
    fn availability_parse_shape() {
        let a: VoiceAvailability =
            serde_json::from_value(serde_json::json!({ "available": false, "detail": "why" }))
                .unwrap();
        assert!(!a.available);
        assert_eq!(a.detail, "why");
        // Defaults: available with no detail.
        let d: VoiceAvailability = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(d.available);
    }
}

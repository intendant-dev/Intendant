//! Voice input: hold-to-talk capture through the dashboard's existing
//! server-side transcription lane, delivered into the in-scene text
//! entry for deliberate confirm.
//!
//! The talk pill is a THIRD hold semantic, kept visually and mechanically
//! distinct from the other two: a quick pinch selects, the 900 ms
//! confirm-hold approves — and the talk hold RECORDS. Pinch-and-hold the
//! pill and the hold is the recording window; release stops it. It never
//! fires on a timer and never cancels on aim drift (your hand wanders
//! while you speak); releasing the pinch is the only stop, so the mic can
//! never stay hot.
//!
//! The capture path is the dashboard's existing one end to end: the JS
//! glue (`ui2-xr.js` voice section) streams mic PCM over the page's
//! `user_audio` lane into the daemon's Whisper transcription
//! (`transcription.rs`, `[transcription] enabled = true`), and the
//! transcript comes back on the broadcast `user_transcript` event. That
//! lane only logs — nothing daemon-side injects it into any conversation
//! — so capture needs no presence pipeline changes. The wasm side here
//! is a pure state machine: Idle → Listening (press) → Transcribing
//! (release) → delivered.
//!
//! Delivery binds into the text-entry substrate (`keyboard.rs`) — the
//! transcript is NEVER auto-sent. With the board open, the utterance
//! appends at the cursor (dictate into the draft you were typing); with
//! it closed, the board opens bound to the focused session's steer field
//! carrying the transcript as its draft. Review, edits, and the commit
//! all go through the keyboard's own grammar — enter emits
//! `{type:'text_commit', field_id, text}` exactly as a typed draft
//! would, and cancel discards. Voice adds a capture lane, not a second
//! send path.
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
/// No open entry and no focused session: nowhere to put the words.
const HINT_NO_TARGET: &str = "select a session to dictate to";
/// Transcribe-backstop line.
const NOTE_TIMEOUT: &str = "transcription timed out — try again";

/// Talk-lane phase. `Listening`/`Transcribing` carry their start time
/// (the frame/`performance.now()` clock) for the min-hold check, the
/// pulse, and the transcribe backstop. A delivered transcript folds
/// back to `Idle` — the text-entry board carrying the draft IS the
/// result state.
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
        }
    }
}

// ---- transitions ---------------------------------------------------------

/// Talk-pill pinch begins. Only idle starts a capture; already
/// listening/transcribing is a no-op.
pub(crate) fn on_press(dock: &mut VoiceDock, now_ms: f64) -> Option<VoiceCmd> {
    match dock.phase {
        TalkPhase::Idle => {
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

/// A transcript arrived from the JS lane: deliver it into the text-entry
/// substrate. Board open → append at the cursor (a joining space rides
/// in when the cursor doesn't already sit after whitespace); board
/// closed → open it bound to the focused session's steer field with the
/// transcript as the draft; no focused session → the honest no-target
/// note. Empty text folds to a failed capture. The capture phase folds
/// to idle in every branch — the board carrying the draft is the result
/// state, and its enter/cancel are the confirm grammar.
pub(crate) fn apply_result(inner: &mut Inner, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        apply_failed(&mut inner.voice, "no speech recognized — try again");
        inner.ui_dirty = true;
        return;
    }
    inner.voice.phase = TalkPhase::Idle;
    inner.voice.note.clear();

    if !inner.text_entry.open {
        let Some((sid, label)) = selected_session(inner) else {
            inner.voice.note = HINT_NO_TARGET.to_string();
            inner.ui_dirty = true;
            return;
        };
        crate::keyboard::open_entry(inner, format!("steer:{sid}"), format!("steer · {label}"));
    }
    insert_transcript(inner, text);
    inner.ui_dirty = true;
}

/// Append the utterance at the entry cursor, joining with a space when
/// the cursor sits directly after non-whitespace. Refused inserts (the
/// entry's buffer cap) surface as an honest truncation note.
fn insert_transcript(inner: &mut Inner, text: &str) {
    let e = &mut inner.text_entry;
    let needs_space = e.cursor > 0
        && e.buffer
            .chars()
            .nth(e.cursor - 1)
            .is_some_and(|c| !c.is_whitespace());
    let mut truncated = false;
    if needs_space && !e.insert_char(' ') {
        truncated = true;
    }
    if !truncated {
        for c in text.chars() {
            if !e.insert_char(c) {
                truncated = true;
                break;
            }
        }
    }
    if truncated {
        inner.voice.note = "transcript truncated — the draft is full".to_string();
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

/// The dictation target when no entry is open: the XR-local selection
/// filtered to real session cards (agenda-rail selections share
/// `selected_id` but are not sendable), with its card label for the
/// board header.
fn selected_session(inner: &Inner) -> Option<(String, String)> {
    let sid = inner.selected_id.as_deref()?;
    let model = inner.model.as_ref()?;
    model
        .agents
        .iter()
        .find(|a| a.id == sid)
        .map(|a| (a.id.clone(), a.label()))
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
/// the honest status line whenever there is one. Called from the frame
/// loop's scene rebuild after `terminal::build_pane`, into the same
/// batches. (The captured transcript renders in the text entry, not
/// here.)
pub(crate) fn build_dock(
    dock: &DockView,
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
    if active {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::ApproxMeasure;

    fn ids(out: &SceneBatches) -> Vec<String> {
        out.hits.iter().map(|h| h.id.clone()).collect()
    }

    fn inner_with_session() -> Inner {
        let mut inner = Inner::new();
        let snap: crate::model::XrSnapshot = serde_json::from_value(serde_json::json!({
            "hosts": [{ "id": "local", "name": "local", "connected": true }],
            "agents": [{ "id": "sess-1", "hostId": "local", "status": "running" }],
        }))
        .unwrap();
        inner.model = Some(snap);
        inner.selected_id = Some("sess-1".into());
        inner
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
    fn transcript_opens_the_steer_entry_for_the_focused_session() {
        let mut inner = inner_with_session();
        on_press(&mut inner.voice, 0.0);
        on_release(&mut inner.voice, 1000.0, false);
        apply_result(&mut inner, "  open the logs  ");
        assert_eq!(inner.voice.phase_name(), "idle");
        assert!(inner.text_entry.open);
        assert_eq!(inner.text_entry.field_id, "steer:sess-1");
        assert!(inner.text_entry.label.starts_with("steer · "));
        assert_eq!(inner.text_entry.buffer, "open the logs");
        assert_eq!(inner.text_entry.cursor, inner.text_entry.char_len());
        assert!(inner.voice.note.is_empty());
    }

    #[test]
    fn transcript_appends_into_an_open_draft_at_the_cursor() {
        let mut inner = inner_with_session();
        crate::keyboard::open_entry(&mut inner, "steer:sess-1".into(), "steer · s".into());
        for c in "check ci".chars() {
            inner.text_entry.insert_char(c);
        }
        on_press(&mut inner.voice, 0.0);
        on_release(&mut inner.voice, 1000.0, false);
        apply_result(&mut inner, "and the queue");
        assert_eq!(inner.text_entry.buffer, "check ci and the queue");
        // Cursor after whitespace: no doubled joining space.
        inner.text_entry.insert_char(' ');
        apply_result(&mut inner, "now");
        assert_eq!(inner.text_entry.buffer, "check ci and the queue now");
    }

    #[test]
    fn transcript_with_no_entry_and_no_session_lands_the_no_target_note() {
        let mut inner = Inner::new();
        on_press(&mut inner.voice, 0.0);
        on_release(&mut inner.voice, 1000.0, false);
        apply_result(&mut inner, "stranded words");
        assert_eq!(inner.voice.phase_name(), "idle");
        assert!(!inner.text_entry.open);
        assert_eq!(inner.voice.note, HINT_NO_TARGET);
        // Agenda-rail selections are not sendable targets either.
        let mut inner = inner_with_session();
        inner.selected_id = Some("agenda:item-1".into());
        apply_result(&mut inner, "stranded words");
        assert!(!inner.text_entry.open);
        assert_eq!(inner.voice.note, HINT_NO_TARGET);
    }

    #[test]
    fn transcript_can_land_while_still_listening() {
        // A fast daemon chunk can beat the release bookkeeping; the
        // machine accepts it rather than dropping speech on the floor.
        let mut inner = inner_with_session();
        on_press(&mut inner.voice, 0.0);
        apply_result(&mut inner, "quick words");
        assert_eq!(inner.voice.phase_name(), "idle");
        assert_eq!(inner.text_entry.buffer, "quick words");
        // The now-stale release is a no-op after delivery.
        assert_eq!(on_release(&mut inner.voice, 5000.0, false), None);
    }

    #[test]
    fn empty_transcript_folds_to_a_failed_capture() {
        let mut inner = inner_with_session();
        on_press(&mut inner.voice, 0.0);
        on_release(&mut inner.voice, 1000.0, false);
        apply_result(&mut inner, "   ");
        assert_eq!(inner.voice.phase_name(), "idle");
        assert!(inner.voice.note.contains("no speech"));
        assert!(!inner.text_entry.open);
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
    fn delivered_transcript_commits_through_the_keyboard() {
        // The whole loop the merge was designed for: dictated text rides
        // the SAME commit path a typed draft rides — keyboard enter, the
        // text_commit shape, delivery bookkeeping — no voice send path.
        let mut inner = inner_with_session();
        on_press(&mut inner.voice, 0.0);
        on_release(&mut inner.voice, 1000.0, false);
        apply_result(&mut inner, "ship the fix");
        assert!(crate::keyboard::commit(&mut inner));
        assert!(!inner.text_entry.open);
        let (field, state) = inner.text_entry.status.clone().expect("delivery state");
        assert_eq!(field, "steer:sess-1");
        assert_eq!(state, crate::keyboard::DeliveryState::Sending);
    }

    #[test]
    fn build_dock_always_offers_the_talk_pill() {
        let dock = DockView::default();
        let mut out = SceneBatches::default();
        build_dock(&dock, None, 0.0, 0.0, &ApproxMeasure, &mut out);
        assert!(ids(&out).contains(&"voice:talk".to_string()));
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
        build_dock(&dock, None, 0.0, 0.0, &ApproxMeasure, &mut out);
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
        build_dock(&dock, None, 0.0, 0.0, &ApproxMeasure, &mut out);
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
        build_dock(&dock, None, 0.0, 0.0, &ApproxMeasure, &mut out);
        assert!(out.texts.iter().any(|t| t.text == HINT_HOLD));
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
            0.0,
            0.0,
            &ApproxMeasure,
            &mut base,
        );
        let mut live = SceneBatches::default();
        build_dock(&dock, None, 350.0, 0.0, &ApproxMeasure, &mut live);
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

//! In-scene text entry: the focused-field model + the ray-typed keyboard.
//!
//! This is the XR surface's missing primitive — a way to ENTER text. The
//! substrate is deliberately generic: any affordance can open an entry
//! bound to a *field id* (the first consumer is the workbench's steer
//! pill, field `steer:<agent_id>`), the operator types on a rendered
//! QWERTY board with quick pinches, and committing emits the dashboard's
//! action vocabulary — `{type:'text_commit', field_id, text}` — through
//! the same registered action router every other XR action uses. The
//! dashboard side resolves the field and routes the text through its
//! EXISTING composer path (`ui2-xr.js`); the wasm never grows a send
//! path of its own, and delivery state comes back through
//! `textEntryResult` so the scene can say "sent" (or the error) honestly
//! instead of pretending.
//!
//! Interaction grammar: keystrokes are LIGHT acts — hover + quick pinch,
//! resolved on release exactly like cards and the terminal pills. The
//! 900 ms deliberate-confirm hold stays approvals-only; a keyboard that
//! made you hold every key would be unusable, and a keystroke is
//! trivially reversible (backspace) where an approval is not.
//!
//! The board lives in a FIXED near-field slot — front-low, below the
//! workbench, tilted up toward the operator's gaze like a drafting
//! table. Panel arrangement/movement is a sibling concern and this slot
//! deliberately does not participate in it.

use crate::atlas::TextMeasure;
use crate::kit::{self, HitKind, HitTarget, PanelInstance, SceneBatches, TextAlign, TextRun};
use crate::math::{v3, Panel, Vec3};
use crate::Inner;

/// Buffer cap: keeps a pathological activate() loop from growing quads
/// without bound. Far beyond anything ray-typed in practice.
const BUFFER_CAP: usize = 2000;
/// Failure detail cap for the workbench status line.
const DETAIL_CAP: usize = 80;

/// Co-planarity lifts, matching `ui.rs` (panel < decor < text).
const LIFT_DECOR: f32 = 0.0018;
const LIFT_TEXT: f32 = 0.0036;

/// Inner padding between the plate edge and the key grid / preview.
const PLATE_PAD: f32 = 0.022;
/// Preview strip (buffer + cursor) height and its gap above the keys.
const PREVIEW_H: f32 = 0.048;
const PREVIEW_GAP: f32 = 0.012;

// ---- state ---------------------------------------------------------------

/// Delivery verdict for the last committed field, reported back by the
/// dashboard router (`textEntryResult`). Rendered beside the field's
/// affordance so the operator sees what actually happened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum DeliveryState {
    /// Committed and handed to the router; no verdict yet.
    Sending,
    Sent,
    Failed(String),
}

/// Facade-owned text-entry state living in [`Inner`]. Pure data —
/// host-constructible and host-testable.
#[derive(Default)]
pub(crate) struct TextEntry {
    /// Keyboard summoned and bound to `field_id`.
    pub(crate) open: bool,
    /// Target field ("steer:<agent_id>" for the workbench consumer).
    pub(crate) field_id: String,
    /// Human label rendered above the board ("steer · claude-code 0123…").
    pub(crate) label: String,
    pub(crate) buffer: String,
    /// Cursor as a char index into `buffer` (0..=chars).
    pub(crate) cursor: usize,
    /// One-shot shift: applies to the next character, then clears.
    pub(crate) shift: bool,
    /// Last commit's delivery verdict, keyed by field id. Survives the
    /// board closing so the workbench can render it.
    pub(crate) status: Option<(String, DeliveryState)>,
}

impl TextEntry {
    /// Byte offset of char index `i` (len when past the end).
    fn byte_at(&self, i: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(i)
            .map(|(b, _)| b)
            .unwrap_or(self.buffer.len())
    }

    pub(crate) fn char_len(&self) -> usize {
        self.buffer.chars().count()
    }

    /// Insert at the cursor; false when the cap refuses it.
    pub(crate) fn insert_char(&mut self, c: char) -> bool {
        if self.char_len() >= BUFFER_CAP {
            return false;
        }
        let at = self.byte_at(self.cursor);
        self.buffer.insert(at, c);
        self.cursor += 1;
        true
    }

    /// Delete the char before the cursor; false at the left edge.
    pub(crate) fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = self.byte_at(self.cursor - 1);
        let end = self.byte_at(self.cursor);
        self.buffer.replace_range(start..end, "");
        self.cursor -= 1;
        true
    }

    pub(crate) fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    pub(crate) fn move_right(&mut self) -> bool {
        if self.cursor >= self.char_len() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// Cheap clone of what the scene build needs (pure data).
    pub(crate) fn view(&self) -> EntryView {
        EntryView {
            open: self.open,
            field_id: self.field_id.clone(),
            label: self.label.clone(),
            buffer: self.buffer.clone(),
            cursor: self.cursor,
            shift: self.shift,
            status: self.status.clone(),
        }
    }
}

/// Snapshot of entry state for one scene build. Pure and host-testable.
#[derive(Clone, Default, Debug)]
pub(crate) struct EntryView {
    pub(crate) open: bool,
    pub(crate) field_id: String,
    pub(crate) label: String,
    pub(crate) buffer: String,
    pub(crate) cursor: usize,
    pub(crate) shift: bool,
    pub(crate) status: Option<(String, DeliveryState)>,
}

// ---- facade entry points -------------------------------------------------

/// Open (or re-open) the entry bound to a field. Composing anew clears
/// the previous buffer and supersedes the last delivery verdict.
pub(crate) fn open_entry(inner: &mut Inner, field_id: String, label: String) {
    let e = &mut inner.text_entry;
    e.open = true;
    e.field_id = field_id;
    e.label = label;
    e.buffer.clear();
    e.cursor = 0;
    e.shift = false;
    e.status = None;
    inner.ui_dirty = true;
}

/// Close without committing. The buffer does not survive — a canceled
/// draft must not silently reappear later.
pub(crate) fn cancel_entry(inner: &mut Inner) {
    let e = &mut inner.text_entry;
    if !e.open {
        return;
    }
    e.open = false;
    e.buffer.clear();
    e.cursor = 0;
    e.shift = false;
    inner.ui_dirty = true;
}

/// Delivery verdict from the dashboard router for a committed field.
pub(crate) fn apply_result(inner: &mut Inner, field_id: &str, ok: bool, detail: &str) {
    let state = if ok {
        DeliveryState::Sent
    } else {
        let d = detail.trim();
        let d = if d.is_empty() { "send failed" } else { d };
        DeliveryState::Failed(d.chars().take(DETAIL_CAP).collect())
    };
    inner.text_entry.status = Some((field_id.to_string(), state));
    inner.ui_dirty = true;
}

/// Resolve a steer-pill activation: toggle the entry bound to that
/// pill's field, labeling the board with the agent's own card label.
pub(crate) fn open_for_steer(inner: &mut Inner, hit: &HitTarget) -> bool {
    if inner.text_entry.open && inner.text_entry.field_id == hit.id {
        cancel_entry(inner);
        return true;
    }
    let label = inner
        .model
        .as_ref()
        .and_then(|m| m.agents.iter().find(|a| a.id == hit.agent_id))
        .map(|a| a.label())
        .unwrap_or_else(|| hit.agent_id.clone());
    open_entry(inner, hit.id.clone(), format!("steer · {label}"));
    true
}

/// Resolve a key activation by hit-target id (`key:<token>`). Returns
/// true when the keystroke had an effect.
pub(crate) fn handle_key(inner: &mut Inner, target_id: &str) -> bool {
    let Some(token) = target_id.strip_prefix("key:") else {
        return false;
    };
    if !inner.text_entry.open {
        return false;
    }
    let changed = match token {
        "enter" => return commit(inner),
        "cancel" => {
            cancel_entry(inner);
            return true;
        }
        "shift" => {
            inner.text_entry.shift = !inner.text_entry.shift;
            true
        }
        "backspace" => inner.text_entry.backspace(),
        "left" => inner.text_entry.move_left(),
        "right" => inner.text_entry.move_right(),
        "space" => insert_shifted(&mut inner.text_entry, ' '),
        _ => {
            let mut chars = token.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => insert_shifted(&mut inner.text_entry, c),
                _ => false,
            }
        }
    };
    if changed {
        // Keystroke → ui_dirty: the scene rebuild in the same frame is
        // what makes typing feel attached to the hand.
        inner.ui_dirty = true;
    }
    changed
}

/// Insert applying (and consuming) the one-shot shift.
fn insert_shifted(entry: &mut TextEntry, c: char) -> bool {
    let c = if entry.shift { shifted_char(c) } else { c };
    let inserted = entry.insert_char(c);
    if inserted && entry.shift {
        entry.shift = false;
    }
    inserted
}

/// Commit the buffer: emit `{type:'text_commit', field_id, text}`
/// through the registered action router, close the board, and mark the
/// field's delivery as in flight. Empty (or whitespace-only) buffers
/// refuse — enter on nothing must not fire an empty send.
pub(crate) fn commit(inner: &mut Inner) -> bool {
    if !inner.text_entry.open {
        return false;
    }
    let text = inner.text_entry.buffer.trim().to_string();
    if text.is_empty() {
        return false;
    }
    let field = inner.text_entry.field_id.clone();
    let payload = serde_json::json!({
        "type": "text_commit",
        "field_id": field,
        "text": text,
    });
    crate::input::emit_action(inner, &payload);
    let e = &mut inner.text_entry;
    e.open = false;
    e.buffer.clear();
    e.cursor = 0;
    e.shift = false;
    e.status = Some((field, DeliveryState::Sending));
    inner.ui_dirty = true;
    true
}

/// The status line a field's affordance renders: text + accent for the
/// last delivery verdict on `field`, if any.
pub(crate) fn status_line_for(
    status: &Option<(String, DeliveryState)>,
    field: &str,
) -> Option<(String, [f32; 4])> {
    let (f, state) = status.as_ref()?;
    if f != field {
        return None;
    }
    Some(match state {
        DeliveryState::Sending => ("sending…".to_string(), kit::TEXT_2),
        DeliveryState::Sent => ("sent".to_string(), kit::GREEN),
        DeliveryState::Failed(detail) => (detail.clone(), kit::RED),
    })
}

// ---- layout --------------------------------------------------------------

/// QWERTY rows as (token, width-units). Tokens are the `key:<token>`
/// activation vocabulary: single ASCII chars plus the named specials.
const ROWS: &[&[(&str, f32)]] = &[
    &[
        ("1", 1.0),
        ("2", 1.0),
        ("3", 1.0),
        ("4", 1.0),
        ("5", 1.0),
        ("6", 1.0),
        ("7", 1.0),
        ("8", 1.0),
        ("9", 1.0),
        ("0", 1.0),
        ("-", 1.0),
        ("=", 1.0),
    ],
    &[
        ("q", 1.0),
        ("w", 1.0),
        ("e", 1.0),
        ("r", 1.0),
        ("t", 1.0),
        ("y", 1.0),
        ("u", 1.0),
        ("i", 1.0),
        ("o", 1.0),
        ("p", 1.0),
    ],
    &[
        ("a", 1.0),
        ("s", 1.0),
        ("d", 1.0),
        ("f", 1.0),
        ("g", 1.0),
        ("h", 1.0),
        ("j", 1.0),
        ("k", 1.0),
        ("l", 1.0),
        (";", 1.0),
        ("'", 1.0),
    ],
    &[
        ("shift", 1.5),
        ("z", 1.0),
        ("x", 1.0),
        ("c", 1.0),
        ("v", 1.0),
        ("b", 1.0),
        ("n", 1.0),
        ("m", 1.0),
        (",", 1.0),
        (".", 1.0),
        ("/", 1.0),
        ("backspace", 1.5),
    ],
    &[
        ("cancel", 1.5),
        ("left", 1.0),
        ("space", 4.0),
        ("right", 1.0),
        ("enter", 2.0),
    ],
];

/// US-layout shift map over the board's base characters. Everything maps
/// inside the atlas's ASCII range — nothing shifted can render '?'.
pub(crate) fn shifted_char(c: char) -> char {
    match c {
        'a'..='z' => c.to_ascii_uppercase(),
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        other => other,
    }
}

/// One laid-out key: plate-relative center + half sizes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeyGeom {
    pub(crate) token: &'static str,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) half_w: f32,
    pub(crate) half_h: f32,
}

/// Plate half sizes for the full board (keys + preview + padding).
pub(crate) fn plate_half_size() -> (f32, f32) {
    let widest = ROWS
        .iter()
        .map(|row| row.iter().map(|(_, u)| u).sum::<f32>())
        .fold(0.0f32, f32::max);
    let half_w = widest * kit::KEY_PITCH / 2.0 + PLATE_PAD;
    let grid_h = ROWS.len() as f32 * kit::KEY_PITCH;
    let half_h = (grid_h + PREVIEW_H + PREVIEW_GAP + 2.0 * PLATE_PAD) / 2.0;
    (half_w, half_h)
}

/// The key grid, centered per row, top row under the preview strip.
pub(crate) fn layout_keys() -> Vec<KeyGeom> {
    let (_, plate_hh) = plate_half_size();
    let grid_top = plate_hh - PLATE_PAD - PREVIEW_H - PREVIEW_GAP;
    let mut out = Vec::new();
    for (r, row) in ROWS.iter().enumerate() {
        let total: f32 = row.iter().map(|(_, u)| u).sum::<f32>() * kit::KEY_PITCH;
        let y = grid_top - (r as f32 + 0.5) * kit::KEY_PITCH;
        let mut pen = -total / 2.0;
        for (token, units) in row.iter() {
            let w = units * kit::KEY_PITCH;
            out.push(KeyGeom {
                token,
                x: pen + w / 2.0,
                y,
                half_w: (w - kit::KEY_GAP) / 2.0,
                half_h: (kit::KEY_PITCH - kit::KEY_GAP) / 2.0,
            });
            pen += w;
        }
    }
    out
}

/// The cap a key renders: what pressing it would produce (shift-aware),
/// or the special's name.
fn key_label(token: &str, shift: bool) -> String {
    match token {
        "enter" => "send".to_string(),
        "cancel" => "cancel".to_string(),
        "space" => "space".to_string(),
        "backspace" => "back".to_string(),
        "shift" => "shift".to_string(),
        "left" => "<".to_string(),
        "right" => ">".to_string(),
        c => {
            let c = c.chars().next().unwrap_or('?');
            (if shift { shifted_char(c) } else { c }).to_string()
        }
    }
}

/// The preview string: buffer with the cursor bar, head-trimmed (with a
/// leading ellipsis) until the bar fits inside `max_w` — the cursor must
/// always be visible; the run's own max_width ellipsizes any long tail.
pub(crate) fn preview_text(
    buffer: &str,
    cursor: usize,
    max_w: f32,
    height: f32,
    measure: &dyn TextMeasure,
) -> String {
    let mut chars: Vec<char> = Vec::with_capacity(buffer.len() + 1);
    let mut bar = 0usize;
    for (i, c) in buffer.chars().enumerate() {
        if i == cursor {
            bar = chars.len();
            chars.push('|');
        }
        chars.push(c);
    }
    if cursor >= buffer.chars().count() {
        bar = chars.len();
        chars.push('|');
    }
    let mut trimmed = false;
    loop {
        let head: String = std::iter::once('…')
            .filter(|_| trimmed)
            .chain(chars[..=bar].iter().copied())
            .collect();
        if measure.measure(&head, height) <= max_w || bar == 0 {
            break;
        }
        chars.remove(0);
        bar -= 1;
        trimmed = true;
    }
    let body: String = chars.into_iter().collect();
    if trimmed {
        format!("…{body}")
    } else {
        body
    }
}

// ---- scene build ---------------------------------------------------------

fn dim(c: [f32; 4], f: f32) -> [f32; 4] {
    [c[0], c[1], c[2], c[3] * f]
}

fn tint(c: [f32; 4], alpha: f32) -> [f32; 4] {
    [c[0], c[1], c[2], alpha]
}

fn lift(p: Vec3, right: Vec3, up: Vec3, amount: f32, floor_y: f32) -> Vec3 {
    let n = right.cross(up).normalize();
    let lifted = p + n.scale(amount);
    v3(lifted.x, lifted.y + floor_y, lifted.z)
}

fn at_floor(p: Vec3, floor_y: f32) -> Vec3 {
    v3(p.x, p.y + floor_y, p.z)
}

/// The fixed keyboard slot's basis: front-low, tilted back so the plate
/// faces the operator's downward gaze (normal points up-and-toward the
/// eyes).
fn keyboard_basis() -> (Vec3, Vec3, Vec3) {
    let t = kit::KEYBOARD_TILT;
    (
        v3(0.0, kit::KEYBOARD_Y, -kit::KEYBOARD_DIST),
        v3(1.0, 0.0, 0.0),
        v3(0.0, t.cos(), -t.sin()),
    )
}

/// Append the text-entry board to a built scene (no-op while closed).
/// Called from the scene rebuild alongside the other overlay builders,
/// into the same batches.
pub(crate) fn build_keyboard(
    entry: &EntryView,
    hover_id: Option<&str>,
    floor_y: f32,
    measure: &dyn TextMeasure,
    out: &mut SceneBatches,
) {
    if !entry.open {
        return;
    }
    let (center, right, up) = keyboard_basis();
    let (hw, hh) = plate_half_size();

    // Backplate.
    out.panels.push(PanelInstance {
        center: at_floor(center, floor_y),
        right,
        up,
        half_w: hw,
        half_h: hh,
        radius: 0.024,
        fill: kit::SURFACE_2,
        border: kit::LINE_2,
        border_w: 0.003,
    });

    // Field label above the plate's top edge.
    out.texts.push(TextRun {
        origin: lift(
            center + right.scale(-hw + 0.006) + up.scale(hh + 0.026),
            right,
            up,
            LIFT_TEXT,
            floor_y,
        ),
        right,
        up,
        height: 0.022,
        color: kit::TEXT_2,
        align: TextAlign::Left,
        max_width: hw * 2.0 - 0.012,
        text: if entry.label.is_empty() {
            "text entry".to_string()
        } else {
            entry.label.clone()
        },
    });

    // Preview strip: the buffer with a visible cursor bar.
    let strip_y = hh - PLATE_PAD - PREVIEW_H / 2.0;
    let strip_hw = hw - PLATE_PAD;
    out.panels.push(PanelInstance {
        center: lift(center + up.scale(strip_y), right, up, LIFT_DECOR, floor_y),
        right,
        up,
        half_w: strip_hw,
        half_h: PREVIEW_H / 2.0,
        radius: 0.010,
        fill: kit::SURFACE,
        border: kit::LINE,
        border_w: 0.0016,
    });
    let text_h = 0.024;
    let text_w_cap = strip_hw * 2.0 - 0.024;
    if entry.buffer.is_empty() {
        // Placeholder + the bar, quiet: the field is armed and empty.
        out.texts.push(TextRun {
            origin: lift(
                center + right.scale(-strip_hw + 0.012) + up.scale(strip_y - 0.008),
                right,
                up,
                LIFT_TEXT + LIFT_DECOR,
                floor_y,
            ),
            right,
            up,
            height: text_h,
            color: kit::TEXT_3,
            align: TextAlign::Left,
            max_width: text_w_cap,
            text: "| message…".to_string(),
        });
    } else {
        out.texts.push(TextRun {
            origin: lift(
                center + right.scale(-strip_hw + 0.012) + up.scale(strip_y - 0.008),
                right,
                up,
                LIFT_TEXT + LIFT_DECOR,
                floor_y,
            ),
            right,
            up,
            height: text_h,
            color: kit::TEXT,
            align: TextAlign::Left,
            max_width: text_w_cap,
            text: preview_text(&entry.buffer, entry.cursor, text_w_cap, text_h, measure),
        });
    }

    // Keys.
    for key in layout_keys() {
        let kcenter = center + right.scale(key.x) + up.scale(key.y);
        let id = format!("key:{}", key.token);
        let is_hover = hover_id == Some(id.as_str());
        let shift_armed = key.token == "shift" && entry.shift;
        // Accents: send wears iris (the composer's action color), shift
        // shows its armed state, everything else is a quiet key cap.
        let accent = match key.token {
            "enter" => kit::IRIS,
            "shift" if shift_armed => kit::IRIS_2,
            _ => kit::TEXT_2,
        };
        let (fill, border, border_w) = if is_hover {
            (tint(kit::IRIS, 0.30), kit::IRIS, 0.0030)
        } else if shift_armed {
            (tint(kit::IRIS, 0.18), kit::IRIS, 0.0026)
        } else if key.token == "enter" {
            (tint(kit::IRIS, 0.10), tint(kit::IRIS, 0.45), 0.0022)
        } else {
            (kit::SURFACE_3, kit::LINE_2, 0.0018)
        };
        out.panels.push(PanelInstance {
            center: lift(kcenter, right, up, LIFT_DECOR, floor_y),
            right,
            up,
            half_w: key.half_w,
            half_h: key.half_h,
            radius: 0.008,
            fill,
            border,
            border_w,
        });
        let label = key_label(key.token, entry.shift);
        let label_h = if label.chars().count() > 1 {
            0.014
        } else {
            0.018
        };
        out.texts.push(TextRun {
            origin: lift(
                kcenter - up.scale(label_h * 0.38),
                right,
                up,
                LIFT_TEXT + LIFT_DECOR,
                floor_y,
            ),
            right,
            up,
            height: label_h,
            color: if is_hover {
                kit::TEXT
            } else if key.token == "enter" || shift_armed {
                accent
            } else {
                dim(kit::TEXT, 0.92)
            },
            align: TextAlign::Center,
            max_width: key.half_w * 2.0 - 0.006,
            text: label,
        });
        out.hits.push(HitTarget {
            id,
            kind: HitKind::Key,
            agent_id: String::new(),
            panel: Panel {
                center: at_floor(kcenter, floor_y),
                right,
                up,
                half_w: key.half_w,
                half_h: key.half_h,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::ApproxMeasure;
    use crate::model::XrSnapshot;

    // ---- layout ----------------------------------------------------------

    #[test]
    fn layout_covers_the_typing_vocabulary() {
        let keys = layout_keys();
        let tokens: Vec<&str> = keys.iter().map(|k| k.token).collect();
        for c in 'a'..='z' {
            let s = c.to_string();
            assert!(
                tokens.iter().any(|t| **t == *s),
                "letter {c} missing from the board"
            );
        }
        for d in '0'..='9' {
            let s = d.to_string();
            assert!(tokens.iter().any(|t| **t == *s), "digit {d} missing");
        }
        for special in [
            "enter",
            "cancel",
            "space",
            "backspace",
            "shift",
            "left",
            "right",
        ] {
            assert!(tokens.contains(&special), "{special} missing");
        }
        // Every token is unique — activation by name must be unambiguous.
        let mut sorted = tokens.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), tokens.len(), "duplicate key tokens");
    }

    #[test]
    fn keys_are_readable_at_arms_length_and_never_overlap() {
        let keys = layout_keys();
        let (plate_hw, plate_hh) = plate_half_size();
        for k in &keys {
            // The legibility floor: every key at least 35 mm in both axes.
            assert!(k.half_w * 2.0 >= 0.035, "{} too narrow", k.token);
            assert!(k.half_h * 2.0 >= 0.035, "{} too short", k.token);
            // Inside the plate.
            assert!(
                k.x.abs() + k.half_w <= plate_hw,
                "{} outside plate",
                k.token
            );
            assert!(
                k.y.abs() + k.half_h <= plate_hh,
                "{} outside plate",
                k.token
            );
        }
        // No overlaps: same-row keys are disjoint on x, rows disjoint on y.
        for a in &keys {
            for b in &keys {
                if std::ptr::eq(a, b) {
                    continue;
                }
                let dx = (a.x - b.x).abs();
                let dy = (a.y - b.y).abs();
                assert!(
                    dx + 1e-6 >= a.half_w + b.half_w || dy + 1e-6 >= a.half_h + b.half_h,
                    "{} overlaps {}",
                    a.token,
                    b.token
                );
            }
        }
    }

    #[test]
    fn shift_map_stays_inside_the_ascii_atlas() {
        for row in ROWS {
            for (token, _) in row.iter() {
                let mut chars = token.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    let s = shifted_char(c);
                    assert!(
                        (' '..='~').contains(&s),
                        "shifted {c} -> {s} escapes the atlas"
                    );
                }
            }
        }
        assert_eq!(shifted_char('a'), 'A');
        assert_eq!(shifted_char('1'), '!');
        assert_eq!(shifted_char('/'), '?');
    }

    // ---- buffer editing --------------------------------------------------

    #[test]
    fn buffer_edits_track_the_cursor() {
        let mut e = TextEntry::default();
        assert!(e.insert_char('h'));
        assert!(e.insert_char('i'));
        assert_eq!(e.buffer, "hi");
        assert_eq!(e.cursor, 2);
        assert!(e.move_left());
        assert!(e.insert_char('e'));
        assert_eq!(e.buffer, "hei");
        assert!(e.backspace());
        assert_eq!(e.buffer, "hi");
        assert_eq!(e.cursor, 1);
        assert!(e.move_right());
        assert_eq!(e.cursor, 2);
        assert!(!e.move_right(), "right edge refuses");
        assert!(e.move_left());
        assert!(e.move_left());
        assert_eq!(e.cursor, 0);
        assert!(!e.move_left(), "left edge refuses");
        assert!(!e.backspace(), "backspace at the edge refuses");
    }

    #[test]
    fn buffer_cap_refuses_growth() {
        let mut e = TextEntry {
            buffer: "x".repeat(BUFFER_CAP),
            cursor: BUFFER_CAP,
            ..Default::default()
        };
        assert!(!e.insert_char('y'));
        assert_eq!(e.char_len(), BUFFER_CAP);
    }

    // ---- key handling / state machine ------------------------------------

    fn open_inner() -> Inner {
        let mut inner = Inner::new();
        open_entry(&mut inner, "steer:a1".into(), "steer · card".into());
        inner
    }

    #[test]
    fn keys_type_shift_and_edit() {
        let mut inner = open_inner();
        assert!(handle_key(&mut inner, "key:shift"));
        assert!(inner.text_entry.shift);
        assert!(handle_key(&mut inner, "key:h"));
        assert!(!inner.text_entry.shift, "shift is one-shot");
        assert!(handle_key(&mut inner, "key:i"));
        assert_eq!(inner.text_entry.buffer, "Hi");
        assert!(handle_key(&mut inner, "key:space"));
        assert!(handle_key(&mut inner, "key:shift"));
        assert!(handle_key(&mut inner, "key:1"));
        assert_eq!(inner.text_entry.buffer, "Hi !");
        assert!(handle_key(&mut inner, "key:backspace"));
        assert!(handle_key(&mut inner, "key:left"));
        assert_eq!(inner.text_entry.cursor, 2);
        assert!(handle_key(&mut inner, "key:right"));
        // Unknown and multi-char tokens refuse; closed board refuses all.
        assert!(!handle_key(&mut inner, "key:nosuch"));
        assert!(!handle_key(&mut inner, "notakey"));
        cancel_entry(&mut inner);
        assert!(!handle_key(&mut inner, "key:h"));
    }

    #[test]
    fn commit_requires_text_and_reports_sending() {
        let mut inner = open_inner();
        // Empty and whitespace-only refuse and keep the board open.
        assert!(!handle_key(&mut inner, "key:enter"));
        assert!(handle_key(&mut inner, "key:space"));
        assert!(!handle_key(&mut inner, "key:enter"));
        assert!(inner.text_entry.open);
        assert!(handle_key(&mut inner, "key:h"));
        assert!(handle_key(&mut inner, "key:i"));
        // No action router on the host — emit falls through silently; the
        // state machine is what this asserts.
        assert!(handle_key(&mut inner, "key:enter"));
        assert!(!inner.text_entry.open, "commit closes the board");
        assert!(inner.text_entry.buffer.is_empty());
        assert_eq!(
            inner.text_entry.status,
            Some(("steer:a1".to_string(), DeliveryState::Sending))
        );
        // Router verdicts land on the field.
        apply_result(&mut inner, "steer:a1", true, "");
        assert_eq!(
            inner.text_entry.status,
            Some(("steer:a1".to_string(), DeliveryState::Sent))
        );
        apply_result(
            &mut inner,
            "steer:a1",
            false,
            "  no session behind this card  ",
        );
        assert_eq!(
            inner.text_entry.status,
            Some((
                "steer:a1".to_string(),
                DeliveryState::Failed("no session behind this card".into())
            ))
        );
        apply_result(&mut inner, "steer:a1", false, "");
        assert_eq!(
            inner.text_entry.status,
            Some((
                "steer:a1".to_string(),
                DeliveryState::Failed("send failed".into())
            ))
        );
    }

    #[test]
    fn cancel_clears_the_draft_and_reopen_supersedes_the_verdict() {
        let mut inner = open_inner();
        assert!(handle_key(&mut inner, "key:h"));
        assert!(handle_key(&mut inner, "key:cancel"));
        assert!(!inner.text_entry.open);
        assert!(inner.text_entry.buffer.is_empty(), "draft does not survive");
        inner.text_entry.status = Some(("steer:a1".into(), DeliveryState::Sent));
        open_entry(&mut inner, "steer:a1".into(), "steer".into());
        assert!(
            inner.text_entry.status.is_none(),
            "composing anew clears the verdict"
        );
    }

    #[test]
    fn status_line_matches_only_its_field() {
        let sent = Some(("steer:a1".to_string(), DeliveryState::Sent));
        assert_eq!(
            status_line_for(&sent, "steer:a1"),
            Some(("sent".to_string(), kit::GREEN))
        );
        assert!(status_line_for(&sent, "steer:a2").is_none());
        let failed = Some((
            "steer:a1".to_string(),
            DeliveryState::Failed("peer not connected".into()),
        ));
        assert_eq!(
            status_line_for(&failed, "steer:a1"),
            Some(("peer not connected".to_string(), kit::RED))
        );
        let sending = Some(("steer:a1".to_string(), DeliveryState::Sending));
        assert_eq!(
            status_line_for(&sending, "steer:a1"),
            Some(("sending…".to_string(), kit::TEXT_2))
        );
        assert!(status_line_for(&None, "steer:a1").is_none());
    }

    // ---- steer-pill dispatch (the real activation path) ------------------

    fn steer_hit() -> HitTarget {
        HitTarget {
            id: "steer:a1".to_string(),
            kind: HitKind::SteerOpen,
            agent_id: "a1".to_string(),
            panel: Panel {
                center: v3(0.0, 1.0, -1.0),
                right: v3(1.0, 0.0, 0.0),
                up: v3(0.0, 1.0, 0.0),
                half_w: 0.05,
                half_h: 0.02,
            },
        }
    }

    #[test]
    fn steer_pill_toggles_and_labels_from_the_card() {
        let mut inner = Inner::new();
        let snap: XrSnapshot = serde_json::from_value(serde_json::json!({
            "agents": [{"id": "a1", "hostId": "local", "sessionId": "0123456789abcdef",
                        "source": "claude-code"}]
        }))
        .unwrap();
        inner.model = Some(snap);
        assert!(open_for_steer(&mut inner, &steer_hit()));
        assert!(inner.text_entry.open);
        assert_eq!(inner.text_entry.field_id, "steer:a1");
        assert_eq!(inner.text_entry.label, "steer · claude-code 0123456789");
        // Same pill again: toggle closed.
        assert!(open_for_steer(&mut inner, &steer_hit()));
        assert!(!inner.text_entry.open);
    }

    // ---- preview ---------------------------------------------------------

    #[test]
    fn preview_shows_the_cursor_and_keeps_it_visible() {
        let m = ApproxMeasure;
        assert_eq!(preview_text("hi", 2, 10.0, 0.024, &m), "hi|");
        assert_eq!(preview_text("hi", 1, 10.0, 0.024, &m), "h|i");
        assert_eq!(preview_text("hi", 0, 10.0, 0.024, &m), "|hi");
        // A long buffer head-trims so the bar stays inside the strip.
        let long = "x".repeat(200);
        let shown = preview_text(&long, 200, 0.5, 0.024, &m);
        assert!(shown.starts_with('…'));
        assert!(shown.ends_with('|'));
        assert!(
            m.measure(&shown, 0.024) <= 0.5 + 0.024,
            "bar fits the strip"
        );
    }

    // ---- scene build -----------------------------------------------------

    #[test]
    fn closed_entry_builds_nothing() {
        let mut out = SceneBatches::default();
        build_keyboard(&EntryView::default(), None, 0.0, &ApproxMeasure, &mut out);
        assert!(out.panels.is_empty() && out.texts.is_empty() && out.hits.is_empty());
    }

    #[test]
    fn open_entry_builds_the_board() {
        let mut inner = open_inner();
        assert!(handle_key(&mut inner, "key:h"));
        let view = inner.text_entry.view();
        let mut out = SceneBatches::default();
        build_keyboard(&view, Some("key:h"), 0.0, &ApproxMeasure, &mut out);
        // Every key is a hit target; the probe's vocabulary is present.
        let key_count = layout_keys().len();
        assert_eq!(out.hits.len(), key_count);
        for id in ["key:h", "key:enter", "key:cancel", "key:space", "key:shift"] {
            assert!(out.hits.iter().any(|h| h.id == id), "{id} missing");
        }
        assert!(out.hits.iter().all(|h| h.kind == HitKind::Key));
        // Plate + preview + one panel per key.
        assert_eq!(out.panels.len(), 2 + key_count);
        // Label + preview + one cap per key.
        assert!(out.texts.iter().any(|t| t.text == "steer · card"));
        assert!(out.texts.iter().any(|t| t.text == "h|"));
        // The board sits in the fixed near-field slot, below the bench.
        let plate = &out.panels[0];
        assert!(plate.center.y < kit::WORKBENCH_Y);
        assert!(
            plate.center.z > -kit::WORKBENCH_DIST,
            "nearer than the bench"
        );
        // Keys face the operator: normal points up-and-toward +z.
        let n = plate.right.cross(plate.up);
        assert!(n.y > 0.0 && n.z > 0.0, "tilted toward the gaze");
    }

    #[test]
    fn shift_state_changes_the_caps() {
        let mut inner = open_inner();
        assert!(handle_key(&mut inner, "key:shift"));
        let mut out = SceneBatches::default();
        build_keyboard(
            &inner.text_entry.view(),
            None,
            0.0,
            &ApproxMeasure,
            &mut out,
        );
        assert!(
            out.texts.iter().any(|t| t.text == "H"),
            "shifted caps render"
        );
        assert!(
            out.texts.iter().any(|t| t.text == "!"),
            "shifted digits render"
        );
        assert!(!out.texts.iter().any(|t| t.text == "h"));
    }

    #[test]
    fn floor_offset_shifts_the_board() {
        let inner = open_inner();
        let view = inner.text_entry.view();
        let mut level = SceneBatches::default();
        build_keyboard(&view, None, 0.0, &ApproxMeasure, &mut level);
        let mut sunk = SceneBatches::default();
        build_keyboard(&view, None, -1.5, &ApproxMeasure, &mut sunk);
        let a = level.panels[0].center;
        let b = sunk.panels[0].center;
        assert!((a.y - b.y - 1.5).abs() < 1e-5);
    }
}

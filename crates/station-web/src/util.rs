//! Colors, palette, and small pure formatting/hashing helpers.

use std::f32::consts::PI;

#[derive(Clone, Copy)]
pub(crate) struct Color {
    pub(crate) r: f32,
    pub(crate) g: f32,
    pub(crate) b: f32,
    pub(crate) a: f32,
}

impl Color {
    pub(crate) const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
    }

    pub(crate) fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }
}

impl From<Color> for [f32; 4] {
    fn from(value: Color) -> Self {
        [value.r, value.g, value.b, value.a]
    }
}

// The Iris design-system palette — the ui-v2 dark-theme values from
// static/app/16-styles-v2-tokens.css. The Station scene is dark by design
// even under the light app theme (ui2-station.css pins its chrome the same
// way), so the canvas carries the dark block only. The Catppuccin-era
// vocabulary collapsed along the tokens file's alias layer: blue+sapphire
// land on iris, yellow+peach on amber, mauve on violet, lavender on
// iris-2, teal on sky, red on rose, and the text/subtext/overlay ladder
// becomes text/text-2/text-3.
pub(crate) const C_TEXT: Color = Color::rgb(234, 236, 242); // --text
pub(crate) const C_TEXT2: Color = Color::rgb(167, 174, 190); // --text-2
pub(crate) const C_SURFACE2: Color = Color::rgb(26, 30, 40); // --surface-2
pub(crate) const C_TEXT3: Color = Color::rgb(126, 136, 150); // --text-3
pub(crate) const C_IRIS: Color = Color::rgb(126, 140, 250); // --iris
pub(crate) const C_IRIS2: Color = Color::rgb(166, 174, 255); // --iris-2
pub(crate) const C_SKY: Color = Color::rgb(93, 169, 230); // --sky
pub(crate) const C_GREEN: Color = Color::rgb(88, 192, 140); // --green
pub(crate) const C_AMBER: Color = Color::rgb(228, 168, 91); // --amber
pub(crate) const C_ROSE: Color = Color::rgb(236, 106, 133); // --rose
pub(crate) const C_VIOLET: Color = Color::rgb(155, 124, 242); // --violet

/// Parse a `#rrggbb` CSS color into a scene `Color` (alpha 1.0), so
/// world-pane consumers reuse the CSS palette the focus content carries
/// instead of maintaining a mirrored `Color` table. Anything unparsable
/// falls back to the body text color.
pub(crate) fn css_color(css: &str) -> Color {
    let hex = css.strip_prefix('#').unwrap_or(css);
    if hex.len() == 6 {
        if let Ok(v) = u32::from_str_radix(hex, 16) {
            return Color::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
        }
    }
    C_TEXT
}

pub(crate) const C_TEXT_CSS: &str = "#eaecf2";
pub(crate) const C_TEXT2_CSS: &str = "#a7aebe";
pub(crate) const C_TEXT3_CSS: &str = "#7e8896";
pub(crate) const C_IRIS_CSS: &str = "#7e8cfa";
pub(crate) const C_IRIS2_CSS: &str = "#a6aeff";
pub(crate) const C_SKY_CSS: &str = "#5da9e6";
pub(crate) const C_GREEN_CSS: &str = "#58c08c";
pub(crate) const C_AMBER_CSS: &str = "#e4a85b";
pub(crate) const C_ROSE_CSS: &str = "#ec6a85";
pub(crate) const C_VIOLET_CSS: &str = "#9b7cf2";

pub(crate) fn role_color(role: &str) -> Color {
    match role {
        "orchestrator" => C_IRIS,
        // Same hue as the "subagent" relationship below: a sub-agent node
        // and the edge that owns it must read as one thing, and the v2
        // Activity grid already fixed that hue at green (ui2-grid.css).
        "sub-agent" => C_GREEN,
        "direct" => C_SKY,
        "session" => C_IRIS,
        "external" => C_AMBER,
        _ => C_SKY,
    }
}

/// Edge tint for a parent/child session relationship. Falls back to the
/// child's role color for unknown/absent kinds so pre-Phase-B nodes keep
/// their look. The kind hues are pinned to the v2 Activity grid's
/// relationship vocabulary (ui2-grid.css: fork=iris, subagent=green,
/// side=violet) so the canvas and the DOM grid tell the same story;
/// fission branches stay in the fork family as the brighter iris-2.
pub(crate) fn relationship_color(kind: &str, role: &str) -> Color {
    match kind {
        "subagent" => C_GREEN,
        "fork" => C_IRIS,
        "side" => C_VIOLET,
        "fission-branch" => C_IRIS2,
        _ => role_color(role),
    }
}

pub(crate) fn phase_color(phase: &str) -> Color {
    match phase {
        "thinking" => C_IRIS2,
        "running" => C_SKY,
        "waiting" => C_AMBER,
        "done" => C_GREEN,
        _ => C_TEXT3,
    }
}

pub(crate) fn phase_color_css(phase: &str) -> &'static str {
    match phase {
        "thinking" => C_IRIS2_CSS,
        "running" => C_SKY_CSS,
        "waiting" => C_AMBER_CSS,
        "done" => C_GREEN_CSS,
        _ => C_TEXT3_CSS,
    }
}

/// Session-goal status tint (the shared goal vocabulary from
/// `normalize_goal_status`, plus the dashboard's kebab-case synonyms).
pub(crate) fn goal_status_color(status: &str) -> Color {
    match status {
        "active" => C_GREEN,
        "paused" | "budgetLimited" | "budget-limited" => C_AMBER,
        "completed" | "complete" => C_IRIS,
        _ => C_IRIS2,
    }
}

pub(crate) fn goal_status_color_css(status: &str) -> &'static str {
    match status {
        "active" => C_GREEN_CSS,
        "paused" | "budgetLimited" | "budget-limited" => C_AMBER_CSS,
        "completed" | "complete" => C_IRIS_CSS,
        _ => C_IRIS2_CSS,
    }
}

pub(crate) fn level_color(level: &str) -> Color {
    match level {
        "error" => C_ROSE,
        "warn" => C_AMBER,
        "model" => C_IRIS,
        "agent" => C_SKY,
        // Violet, not the relationship green: presence already owns green
        // in this log-lane set, and a lane tint is not a relationship badge.
        "subagent" => C_VIOLET,
        "presence" => C_GREEN,
        _ => C_TEXT3,
    }
}

pub(crate) fn level_color_css(level: &str) -> &'static str {
    match level {
        "error" => C_ROSE_CSS,
        "warn" => C_AMBER_CSS,
        "model" => C_IRIS_CSS,
        "agent" => C_SKY_CSS,
        "subagent" => C_VIOLET_CSS,
        "presence" => C_GREEN_CSS,
        _ => C_TEXT3_CSS,
    }
}

/// Detail-row tone (the dashboard's snapshot `tone` strings) to an accent
/// color for the focus-panel row label.
pub(crate) fn tone_color_css(tone: &str) -> &'static str {
    match tone {
        "ok" => C_GREEN_CSS,
        "red" => C_ROSE_CSS,
        "warning" => C_AMBER_CSS,
        "context" => C_IRIS_CSS,
        "managed" => C_VIOLET_CSS,
        "peer" => C_AMBER_CSS,
        "session" => C_SKY_CSS,
        "changes" => C_IRIS_CSS,
        _ => C_TEXT3_CSS,
    }
}

/// Attention-item level to its alert color (`blocked` is the hard stop).
pub(crate) fn attention_level_color_css(level: &str) -> &'static str {
    match level {
        "blocked" => C_ROSE_CSS,
        "warn" => C_AMBER_CSS,
        "ready" => C_GREEN_CSS,
        _ => C_TEXT3_CSS,
    }
}

pub(crate) fn css_rgba(color: [f32; 4]) -> String {
    format!(
        "rgba({:.0},{:.0},{:.0},{:.3})",
        color[0] * 255.0,
        color[1] * 255.0,
        color[2] * 255.0,
        color[3]
    )
}

/// Parse a `#rrggbb` CSS color into a [`Color`]; the glass chrome uses this
/// to derive alpha/glow variants from the same palette constants the flat
/// HUD text uses, so accents stay in one place.
pub(crate) fn hex_color(css: &str) -> Option<Color> {
    let hex = css.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(hex.get(range)?, 16).ok();
    Some(Color::rgb(channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

pub(crate) fn percent(value: f32, max: f32) -> f32 {
    if max <= 0.0 {
        0.0
    } else {
        (value / max).clamp(0.0, 1.0)
    }
}

pub(crate) fn pct_label(pct: f32) -> String {
    format!("{:.0}%", pct.clamp(0.0, 1.0) * 100.0)
}

pub(crate) fn nonempty(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn pressure_color(pct: f32) -> &'static str {
    if pct >= 0.9 {
        C_ROSE_CSS
    } else if pct >= 0.72 {
        C_AMBER_CSS
    } else if pct >= 0.5 {
        C_IRIS_CSS
    } else {
        C_GREEN_CSS
    }
}

/// Compact human number for HUD figures: 850, 12.5k, 1.2m.
pub(crate) fn fmt_compact(value: f32) -> String {
    let abs = value.abs();
    if abs >= 10_000_000.0 {
        format!("{:.0}m", value / 1_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("{:.1}m", value / 1_000_000.0)
    } else if abs >= 10_000.0 {
        format!("{:.0}k", value / 1_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else {
        format!("{}", value.round() as i64)
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

pub(crate) fn stable_angle(s: &str) -> f32 {
    stable_unit(s) * PI * 2.0
}

pub(crate) fn stable_unit(s: &str) -> f32 {
    let mut h = 2166136261u32;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h as f32 / u32::MAX as f32).clamp(0.0, 1.0)
}

pub(crate) fn lcg(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

pub(crate) fn unit(seed: u32) -> f32 {
    seed as f32 / u32::MAX as f32
}

pub(crate) fn station_enable_webgpu() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|document| document.url().ok())
        .is_none_or(|url| !url.contains("station_gpu=canvas") && !url.contains("station_gpu=off"))
}

/// World-space panes (Phase C) are opt-IN while the program is in
/// flight: `?station_panes=on` (or `=1`) enables them; absence or any
/// other value keeps the scene wireframe-only. Flips to opt-out when the
/// pane presentation graduates.
pub(crate) fn station_enable_panes() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|document| document.url().ok())
        .is_some_and(|url| url.contains("station_panes=on") || url.contains("station_panes=1"))
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn now_ms() -> f64 {
    thread_local! {
        static PERFORMANCE: Option<web_sys::Performance> =
            web_sys::window().and_then(|w| w.performance());
    }
    PERFORMANCE.with(|p| p.as_ref().map_or(0.0, |p| p.now()))
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn now_ms() -> f64 {
    0.0
}

/// Wall-clock unix seconds (`Date.now()`), for countdowns anchored to
/// server-stamped epochs (the cache-TTL vitals row). Native tests get 0 —
/// callers treat 0 as "no clock" and skip live countdowns.
#[cfg(target_arch = "wasm32")]
pub(crate) fn epoch_seconds_now() -> f64 {
    js_sys::Date::now() / 1000.0
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn epoch_seconds_now() -> f64 {
    0.0
}

/// `m:ss` for a cache-TTL countdown.
pub(crate) fn fmt_countdown(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_color_round_trips_the_palette_and_falls_back() {
        for (css, color) in [
            (C_TEXT_CSS, C_TEXT),
            (C_ROSE_CSS, C_ROSE),
            (C_IRIS_CSS, C_IRIS),
        ] {
            let parsed = css_color(css);
            assert!((parsed.r - color.r).abs() < 1e-6, "{css} r");
            assert!((parsed.g - color.g).abs() < 1e-6, "{css} g");
            assert!((parsed.b - color.b).abs() < 1e-6, "{css} b");
            assert!((parsed.a - 1.0).abs() < 1e-6);
        }
        let fallback = css_color("rgba(1,2,3,0.5)");
        assert!((fallback.r - C_TEXT.r).abs() < 1e-6);
    }

    #[test]
    fn fmt_compact_scales_units() {
        assert_eq!(fmt_compact(0.0), "0");
        assert_eq!(fmt_compact(850.0), "850");
        assert_eq!(fmt_compact(12_600.0), "13k");
        assert_eq!(fmt_compact(1_500.0), "1.5k");
        assert_eq!(fmt_compact(1_200_000.0), "1.2m");
        assert_eq!(fmt_compact(25_000_000.0), "25m");
    }

    #[test]
    fn truncate_passes_short_strings_through() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("", 4), "");
    }

    #[test]
    fn truncate_cuts_on_chars_not_bytes() {
        assert_eq!(truncate("hello", 3), "hel…");
        // Multi-byte characters count as one.
        assert_eq!(truncate("héllo wörld", 5), "héllo…");
    }

    #[test]
    fn nonempty_trims_and_falls_back() {
        assert_eq!(nonempty("  value  ", "fb"), "value");
        assert_eq!(nonempty("   ", "fb"), "fb");
        assert_eq!(nonempty("", "fb"), "fb");
    }

    #[test]
    fn percent_clamps_and_handles_empty_window() {
        assert_eq!(percent(50.0, 200.0), 0.25);
        assert_eq!(percent(500.0, 200.0), 1.0);
        assert_eq!(percent(-1.0, 200.0), 0.0);
        assert_eq!(percent(10.0, 0.0), 0.0);
        assert_eq!(percent(10.0, -5.0), 0.0);
    }

    #[test]
    fn pct_label_rounds_and_clamps() {
        assert_eq!(pct_label(0.0), "0%");
        assert_eq!(pct_label(0.254), "25%");
        assert_eq!(pct_label(1.7), "100%");
    }

    #[test]
    fn pressure_color_thresholds() {
        assert_eq!(pressure_color(0.1), C_GREEN_CSS);
        assert_eq!(pressure_color(0.5), C_IRIS_CSS);
        assert_eq!(pressure_color(0.72), C_AMBER_CSS);
        assert_eq!(pressure_color(0.9), C_ROSE_CSS);
    }

    #[test]
    fn stable_unit_is_deterministic_and_in_range() {
        for id in ["", "agent-1", "host:alpha", "x"] {
            let a = stable_unit(id);
            assert_eq!(a, stable_unit(id));
            assert!((0.0..=1.0).contains(&a), "{id} -> {a}");
        }
        assert_ne!(stable_unit("agent-1"), stable_unit("agent-2"));
        assert_eq!(stable_angle("a"), stable_unit("a") * PI * 2.0);
    }

    #[test]
    fn lcg_and_unit_are_deterministic() {
        let s1 = lcg(1);
        assert_eq!(s1, lcg(1));
        assert!((0.0..=1.0).contains(&unit(s1)));
    }

    #[test]
    fn css_rgba_formats_components() {
        assert_eq!(css_rgba([1.0, 0.0, 0.5, 0.25]), "rgba(255,0,128,0.250)");
    }

    #[test]
    fn hex_color_parses_palette_and_rejects_garbage() {
        let iris = hex_color(C_IRIS_CSS).expect("palette constant parses");
        let reference: [f32; 4] = C_IRIS.into();
        let parsed: [f32; 4] = iris.into();
        assert_eq!(parsed, reference);
        assert!(hex_color("#fff").is_none());
        assert!(hex_color("7e8cfa").is_none());
        assert!(hex_color("#7e8cfg").is_none());
        assert!(hex_color("").is_none());
    }

    #[test]
    fn color_with_alpha_keeps_rgb() {
        let c = C_IRIS.with_alpha(0.5);
        let arr: [f32; 4] = c.into();
        assert_eq!(arr[3], 0.5);
        assert_eq!(arr[0], C_IRIS.r);
    }

    #[test]
    fn semantic_color_maps_cover_known_keys() {
        assert_eq!(level_color_css("error"), C_ROSE_CSS);
        assert_eq!(level_color_css("warn"), C_AMBER_CSS);
        assert_eq!(level_color_css("unknown"), C_TEXT3_CSS);
        let err: [f32; 4] = level_color("error").into();
        let rose: [f32; 4] = C_ROSE.into();
        assert_eq!(err, rose);
        let orch: [f32; 4] = role_color("orchestrator").into();
        let iris: [f32; 4] = C_IRIS.into();
        assert_eq!(orch, iris);
        let think: [f32; 4] = phase_color("thinking").into();
        let iris2: [f32; 4] = C_IRIS2.into();
        assert_eq!(think, iris2);
    }

    #[test]
    fn goal_status_colors_agree_between_scene_and_css() {
        // The scene ring (Color) and the HUD rows (CSS) derive from the
        // same mapping; hex_color(css) must match the Color for every
        // status in the shared goal vocabulary plus the fallback.
        for status in [
            "active",
            "paused",
            "budgetLimited",
            "budget-limited",
            "completed",
            "complete",
            "blocked",
            "",
        ] {
            let scene: [f32; 4] = goal_status_color(status).into();
            let css: [f32; 4] = hex_color(goal_status_color_css(status))
                .expect("goal css colors are #rrggbb")
                .into();
            assert_eq!(
                &scene[..3],
                &css[..3],
                "goal color mismatch for status {status:?}"
            );
        }
    }

    #[test]
    fn relationship_colors_key_edge_kinds_and_fall_back_to_role() {
        // The kind hues are a cross-surface contract: the v2 Activity
        // grid colors relationship chips/wires fork=iris, subagent=green,
        // side=violet (ui2-grid.css), and the canvas must tell the same
        // story. The literal hexes pin the ui2 token values so a palette
        // edit that drifts from the DOM grid fails here instead of
        // shipping two vocabularies.
        for (kind, css) in [
            ("fork", "#7e8cfa"),
            ("subagent", "#58c08c"),
            ("side", "#9b7cf2"),
        ] {
            let edge: [f32; 4] = relationship_color(kind, "session").into();
            let pinned: [f32; 4] = hex_color(css).expect("pinned grid hue parses").into();
            assert_eq!(edge, pinned, "grid-contract hue for {kind:?}");
        }
        // Fission branches stay in the fork family as the brighter iris-2.
        let fission: [f32; 4] = relationship_color("fission-branch", "session").into();
        let iris2: [f32; 4] = C_IRIS2.into();
        assert_eq!(fission, iris2);
        // A sub-agent node matches its edge: same green, one vocabulary.
        let sub_role: [f32; 4] = role_color("sub-agent").into();
        let green: [f32; 4] = C_GREEN.into();
        assert_eq!(sub_role, green);
        // Unknown kinds keep the node's role color, so pre-Phase-B nodes
        // (which never set a kind) render exactly as before.
        let unknown: [f32; 4] = relationship_color("", "orchestrator").into();
        let iris: [f32; 4] = C_IRIS.into();
        assert_eq!(unknown, iris);
        let session: [f32; 4] = role_color("session").into();
        assert_eq!(session, iris);
    }
}

//! Batch observation policy for computer use.
//!
//! `execute_cu_actions` historically force-captured a full screenshot after
//! every non-capture batch — encoded, decoded, annotated, re-encoded, and
//! rewritten even when the caller only consumed a path. This module makes the
//! trailing observation a *policy*: pixels (screenshot), an accessibility
//! element tree (a few hundred tokens instead of ~1.5k image tokens), an
//! automatic choice between them, or nothing. It also owns the opt-in click
//! markers, drawn on the raw frame before the single PNG encode.
//!
//! Marker policy (CU-06): model-facing images are CLEAN by default — baked-in
//! crosshairs obscured the very controls being verified, and the dashboard
//! already renders live action overlays. `annotate: true` opts in, and the
//! disk artifact always carries the same bytes as the model payload: its one
//! remaining reader is the managed-Codex `view_image` path (the Activity-tab
//! disk substitution died with the Gemini CLI backend), which wants exactly
//! what the model would have seen inline.

use crate::computer_use::{
    format_screen_elements, read_screen_elements, CuAction, DisplayTarget, ScreenElements,
    UiElement,
};
use serde::{Deserialize, Serialize};

// ── Observe policy ───────────────────────────────────────────────────────────

/// What the automatic post-batch observation should be.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ObserveMode {
    /// Attach the post-action screenshot (the historical behavior).
    ///
    /// The default: the MCP tool description and the native `peer cu`
    /// guidance both promise a post-action screenshot, so a quieter default
    /// would silently break callers that verify from pixels. Token-sensitive
    /// callers (managed Codex) opt into `auto`/`ax` explicitly.
    #[default]
    Pixels,
    /// Attach the frontmost element tree instead of pixels.
    Ax,
    /// Element tree when the frontmost tree is usable, pixels as fallback.
    Auto,
    /// Per-action results only — for callers chaining batches that will
    /// observe separately.
    None,
}

impl ObserveMode {
    /// Wire/reason label.
    pub fn label(self) -> &'static str {
        match self {
            ObserveMode::Pixels => "pixels",
            ObserveMode::Ax => "ax",
            ObserveMode::Auto => "auto",
            ObserveMode::None => "none",
        }
    }
}

/// Batch-level execution options beyond the action list.
#[derive(Debug, Clone, Copy, Default)]
pub struct CuExecOptions {
    /// Trailing-observation policy.
    pub observe: ObserveMode,
    /// Draw click markers on captured screenshots (model payload and disk
    /// artifact alike — they are the same bytes). Default false: clean
    /// pixels; the live dashboard already overlays actions in real time.
    pub annotate: bool,
}

// ── Observation result ───────────────────────────────────────────────────────

/// What the batch's final observation actually is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CuObservationKind {
    /// A screenshot rides the results (trailing auto-capture or an explicit
    /// screenshot/zoom action).
    Pixels,
    /// An element tree rides [`CuObservation::ax_text`].
    Ax,
    /// Results only.
    None,
}

impl CuObservationKind {
    pub fn label(self) -> &'static str {
        match self {
            CuObservationKind::Pixels => "pixels",
            CuObservationKind::Ax => "ax",
            CuObservationKind::None => "none",
        }
    }
}

/// The chosen observation for a batch: which kind it carries and why —
/// callers surface both so a fallback (`ax sparse → pixels`) is never silent.
#[derive(Debug, Clone)]
pub struct CuObservation {
    pub kind: CuObservationKind,
    /// Stable, human-readable choice trace, e.g. `observe=pixels`,
    /// `auto: ax usable (42 nodes)`, `auto: ax sparse (2 nodes) → pixels`.
    pub reason: String,
    /// Formatted element tree when `kind == Ax`.
    pub ax_text: Option<String>,
}

impl CuObservation {
    /// One-line description for tool output: `ax (auto: ax usable (42 nodes))`.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.kind.label(), self.reason)
    }
}

/// Lightweight per-batch measurements (see the `[cu]` diagnostics line).
#[derive(Debug, Clone, Copy, Default)]
pub struct CuBatchMetrics {
    /// Capture + single PNG encode duration of the trailing observation
    /// screenshot, when one was taken.
    pub capture_ms: Option<u64>,
    /// AX element-tree walk duration, when one ran.
    pub ax_ms: Option<u64>,
    /// Approximate bytes of the observation attached to the result
    /// (PNG bytes for pixels, UTF-8 text bytes for ax).
    pub observation_bytes: usize,
}

/// Everything `execute_actions` produces for a batch: per-action results in
/// order (possibly plus one trailing auto-screenshot result), the observation
/// decision, and measurements.
#[derive(Debug)]
pub struct CuBatchOutcome {
    pub results: Vec<crate::computer_use::CuActionResult>,
    pub observation: CuObservation,
    pub metrics: CuBatchMetrics,
}

impl CuBatchOutcome {
    /// The most recent screenshot in the batch, if any.
    pub fn last_screenshot(&self) -> Option<&crate::computer_use::ScreenshotData> {
        self.results
            .iter()
            .rev()
            .find_map(|r| r.screenshot.as_ref())
    }

    /// Compact one-line measurement summary for session logs/diagnostics:
    /// `observation=ax (auto: ax usable (9 nodes)) ax_ms=12 obs_bytes=743`.
    pub fn metrics_line(&self) -> String {
        let mut line = format!("observation={}", self.observation.describe());
        if let Some(ms) = self.metrics.capture_ms {
            line.push_str(&format!(" capture_ms={ms}"));
        }
        if let Some(ms) = self.metrics.ax_ms {
            line.push_str(&format!(" ax_ms={ms}"));
        }
        line.push_str(&format!(" obs_bytes={}", self.metrics.observation_bytes));
        line
    }
}

// ── Observation planning ─────────────────────────────────────────────────────

/// Minimum element count for an `auto` AX observation to count as usable.
/// A bare window frame with an unlabeled group or two grounds nothing —
/// real UIs expose dozens of nodes; below this floor `auto` falls back to
/// pixels and says so.
pub const AX_AUTO_MIN_NODES: usize = 5;

/// Total node count of an element tree.
pub(crate) fn element_count(element: &UiElement) -> usize {
    1 + element.children.iter().map(element_count).sum::<usize>()
}

fn snapshot_node_count(snapshot: &ScreenElements) -> usize {
    snapshot.root.as_ref().map(element_count).unwrap_or(0)
}

/// The resolved plan for a batch's trailing observation. `Pixels` means the
/// executor should capture; `ExplicitCapture` means the batch's own final
/// screenshot/zoom action already is the observation.
#[derive(Debug)]
pub(crate) enum ObservationPlan {
    ExplicitCapture,
    None {
        reason: String,
    },
    Ax {
        text: String,
        reason: String,
        walk_ms: u64,
    },
    Pixels {
        reason: String,
    },
}

/// Decide the trailing observation for a batch. Runs the AX walk when the
/// mode calls for one; never captures pixels itself.
pub(crate) async fn plan_observation(
    actions: &[CuAction],
    observe: ObserveMode,
    target: DisplayTarget,
) -> ObservationPlan {
    if actions.is_empty() {
        return ObservationPlan::None {
            reason: "no actions".to_string(),
        };
    }
    if actions
        .last()
        .is_some_and(|a| matches!(a, CuAction::Screenshot | CuAction::Zoom { .. }))
    {
        // The batch's own capture is the observation — including under
        // `auto`/`ax`: an explicit screenshot/zoom action always yields
        // its pixels.
        return ObservationPlan::ExplicitCapture;
    }
    match observe {
        ObserveMode::None => ObservationPlan::None {
            reason: "observe=none".to_string(),
        },
        ObserveMode::Pixels => ObservationPlan::Pixels {
            reason: "observe=pixels".to_string(),
        },
        ObserveMode::Ax => match ax_walk(target).await {
            Ok((text, nodes, walk_ms)) => ObservationPlan::Ax {
                text,
                reason: format!("observe=ax ({nodes} nodes)"),
                walk_ms,
            },
            Err(e) => ObservationPlan::None {
                reason: format!("observe=ax failed: {e}"),
            },
        },
        ObserveMode::Auto => {
            if !target.is_user_session() {
                // Element trees are user-session-only on every platform;
                // skip the doomed walk instead of paying for its error.
                return ObservationPlan::Pixels {
                    reason: "auto: ax unavailable on virtual displays → pixels".to_string(),
                };
            }
            match ax_walk(target).await {
                Ok((text, nodes, walk_ms)) if nodes >= AX_AUTO_MIN_NODES => ObservationPlan::Ax {
                    text,
                    reason: format!("auto: ax usable ({nodes} nodes)"),
                    walk_ms,
                },
                Ok((_, nodes, _)) => ObservationPlan::Pixels {
                    reason: format!("auto: ax sparse ({nodes} nodes) → pixels"),
                },
                Err(e) => ObservationPlan::Pixels {
                    reason: format!("auto: ax error ({e}) → pixels"),
                },
            }
        }
    }
}

/// Read + format the frontmost element tree, timing the walk.
async fn ax_walk(target: DisplayTarget) -> Result<(String, usize, u64), String> {
    let start = std::time::Instant::now();
    let snapshot = read_screen_elements(target, false).await?;
    let walk_ms = start.elapsed().as_millis() as u64;
    let nodes = snapshot_node_count(&snapshot);
    Ok((format_screen_elements(&snapshot), nodes, walk_ms))
}

// ── Synthetic element tree (mock display rigs) ──────────────────────────────

/// Deterministic element tree served while the synthetic display backend is
/// armed (`INTENDANT_MOCK_DISPLAY=synthetic` under `PROVIDER=mock`), so AX
/// observation flows are exercisable in CI without touching a native
/// accessibility API (macOS AX / AT-SPI / UIA) — the same charter as the
/// synthetic capture backend. Node count is deliberately above
/// [`AX_AUTO_MIN_NODES`] so `observe=auto` deterministically picks `ax`.
pub(crate) fn synthetic_screen_elements() -> ScreenElements {
    let button = |label: &str, x: i32| UiElement {
        role: "button".to_string(),
        label: Some(label.to_string()),
        value: None,
        frame: (x, 640, 120, 40),
        focused: false,
        enabled: true,
        children: Vec::new(),
    };
    ScreenElements {
        app: "Synthetic Desktop".to_string(),
        pid: 0,
        window_title: Some("Synthetic Display".to_string()),
        root: Some(UiElement {
            role: "window".to_string(),
            label: Some("Synthetic Display".to_string()),
            value: None,
            frame: (0, 0, 1280, 720),
            focused: false,
            enabled: true,
            children: vec![
                UiElement {
                    role: "textfield".to_string(),
                    label: Some("Input".to_string()),
                    value: Some(String::new()),
                    frame: (40, 40, 400, 32),
                    focused: true,
                    enabled: true,
                    children: Vec::new(),
                },
                button("OK", 40),
                button("Cancel", 180),
                UiElement {
                    role: "statictext".to_string(),
                    label: Some("Synthetic test card".to_string()),
                    value: None,
                    frame: (40, 100, 300, 20),
                    focused: false,
                    enabled: true,
                    children: Vec::new(),
                },
            ],
        }),
        other_windows: Vec::new(),
        truncated: None,
    }
}

// ── Click markers (opt-in) ───────────────────────────────────────────────────

/// The click-family coordinates of a batch, in execution order — where the
/// batch *aimed*, drawn regardless of per-action outcome so a missed click
/// is still visible.
pub(crate) fn click_points(actions: &[CuAction]) -> Vec<(i32, i32)> {
    actions
        .iter()
        .filter_map(|a| match a {
            CuAction::Click { x, y, .. }
            | CuAction::DoubleClick { x, y, .. }
            | CuAction::TripleClick { x, y, .. }
            | CuAction::MouseDown { x, y, .. } => Some((*x, *y)),
            _ => None,
        })
        .collect()
}

/// Draw crosshair + circle markers at `clicks` on a raw RGBA frame — red for
/// in-bounds points, yellow (plus a top-edge warning bar) for out-of-bounds
/// ones. Operates pre-encode so annotation costs zero extra PNG round-trips.
pub(crate) fn draw_click_markers(img: &mut image::RgbaImage, clicks: &[(i32, i32)]) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    if w == 0 || h == 0 {
        return;
    }
    let red = image::Rgba([255u8, 0, 0, 255]);
    let yellow = image::Rgba([255u8, 255, 0, 255]);
    let arm = 20i32;
    let thickness = 3i32;

    for (cx, cy) in clicks {
        // Clamp to image bounds; use yellow for out-of-bounds clicks.
        let oob = *cx < 0 || *cx >= w || *cy < 0 || *cy >= h;
        let color = if oob { yellow } else { red };
        let dx = (*cx).max(0).min(w - 1);
        let dy = (*cy).max(0).min(h - 1);

        // Crosshair at the (clamped) position.
        for offset in -arm..=arm {
            for t in -thickness..=thickness {
                let hx = dx + offset;
                let hy = dy + t;
                if hx >= 0 && hx < w && hy >= 0 && hy < h {
                    img.put_pixel(hx as u32, hy as u32, color);
                }
                let vx = dx + t;
                let vy = dy + offset;
                if vx >= 0 && vx < w && vy >= 0 && vy < h {
                    img.put_pixel(vx as u32, vy as u32, color);
                }
            }
        }
        // Circle (radius 12).
        let r = 12i32;
        for angle in 0..360 {
            let rad = (angle as f64) * std::f64::consts::PI / 180.0;
            let px = dx + (r as f64 * rad.cos()) as i32;
            let py = dy + (r as f64 * rad.sin()) as i32;
            for t in 0..=2 {
                let px2 = px + t;
                let py2 = py + t;
                if px2 >= 0 && px2 < w && py2 >= 0 && py2 < h {
                    img.put_pixel(px2 as u32, py2 as u32, color);
                }
            }
        }
        // Out-of-bounds warning bar along the top edge.
        if oob {
            for bx in 0..80i32 {
                for by in 0..6i32 {
                    if bx < w && by < h {
                        img.put_pixel(bx as u32, by as u32, yellow);
                    }
                }
            }
        }
    }
}

/// Encode a raw RGBA image as PNG bytes — the single encode of the capture
/// pipeline.
pub(crate) fn encode_rgba_png(img: &image::RgbaImage) -> Result<Vec<u8>, String> {
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode: {e}"))?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_mode_parses_wire_labels_and_defaults_to_pixels() {
        assert_eq!(ObserveMode::default(), ObserveMode::Pixels);
        for (wire, mode) in [
            ("\"pixels\"", ObserveMode::Pixels),
            ("\"ax\"", ObserveMode::Ax),
            ("\"auto\"", ObserveMode::Auto),
            ("\"none\"", ObserveMode::None),
        ] {
            let parsed: ObserveMode = serde_json::from_str(wire).unwrap();
            assert_eq!(parsed, mode);
            assert_eq!(format!("\"{}\"", mode.label()), wire);
        }
        assert!(serde_json::from_str::<ObserveMode>("\"screenshots\"").is_err());
    }

    fn leaf(role: &str) -> UiElement {
        UiElement {
            role: role.to_string(),
            label: None,
            value: None,
            frame: (0, 0, 10, 10),
            focused: false,
            enabled: true,
            children: Vec::new(),
        }
    }

    #[test]
    fn element_count_walks_the_whole_tree() {
        let mut root = leaf("window");
        let mut group = leaf("group");
        group.children.push(leaf("button"));
        group.children.push(leaf("button"));
        root.children.push(group);
        assert_eq!(element_count(&root), 4);
    }

    #[test]
    fn synthetic_tree_is_deterministic_and_above_the_auto_floor() {
        let a = synthetic_screen_elements();
        let b = synthetic_screen_elements();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "synthetic tree must be byte-deterministic"
        );
        assert!(snapshot_node_count(&a) >= AX_AUTO_MIN_NODES);
        let text = format_screen_elements(&a);
        assert!(text.contains("Synthetic Desktop"), "{text}");
        assert!(text.contains("button \"OK\""), "{text}");
    }

    #[tokio::test]
    async fn plan_prefers_explicit_captures_and_honors_none() {
        let click = CuAction::Click {
            x: 1,
            y: 2,
            button: Default::default(),
        };
        let plan = plan_observation(
            &[click.clone(), CuAction::Screenshot],
            ObserveMode::Auto,
            DisplayTarget::UserSession,
        )
        .await;
        assert!(matches!(plan, ObservationPlan::ExplicitCapture), "{plan:?}");

        let plan = plan_observation(
            &[click.clone()],
            ObserveMode::None,
            DisplayTarget::UserSession,
        )
        .await;
        assert!(
            matches!(&plan, ObservationPlan::None { reason } if reason == "observe=none"),
            "{plan:?}"
        );

        let plan = plan_observation(&[], ObserveMode::Pixels, DisplayTarget::UserSession).await;
        assert!(
            matches!(&plan, ObservationPlan::None { reason } if reason == "no actions"),
            "{plan:?}"
        );

        // Virtual targets cannot serve element trees on any platform: auto
        // must fall to pixels without attempting the walk.
        let plan = plan_observation(
            &[click],
            ObserveMode::Auto,
            DisplayTarget::Virtual { id: 99 },
        )
        .await;
        assert!(
            matches!(&plan, ObservationPlan::Pixels { reason }
                     if reason.contains("ax unavailable on virtual displays")),
            "{plan:?}"
        );
    }

    #[test]
    fn click_points_collects_the_click_family_only() {
        let actions = vec![
            CuAction::Click {
                x: 10,
                y: 20,
                button: Default::default(),
            },
            CuAction::Type {
                text: "hi".to_string(),
            },
            CuAction::MouseDown {
                x: 30,
                y: 40,
                button: Default::default(),
            },
            CuAction::Screenshot,
        ];
        assert_eq!(click_points(&actions), vec![(10, 20), (30, 40)]);
    }

    #[test]
    fn markers_paint_in_bounds_red_and_out_of_bounds_yellow() {
        let mut img = image::RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 255]));
        draw_click_markers(&mut img, &[(50, 50)]);
        assert_eq!(img.get_pixel(50, 50), &image::Rgba([255, 0, 0, 255]));

        let mut img = image::RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 255]));
        draw_click_markers(&mut img, &[(500, 500)]);
        // Clamped marker paints yellow at the clamped corner and the warning
        // bar paints the top edge.
        assert_eq!(img.get_pixel(99, 99), &image::Rgba([255, 255, 0, 255]));
        assert_eq!(img.get_pixel(0, 0), &image::Rgba([255, 255, 0, 255]));
    }

    #[test]
    fn clean_by_default_markers_change_nothing_when_absent() {
        let base = image::RgbaImage::from_pixel(32, 32, image::Rgba([7, 7, 7, 255]));
        let mut img = base.clone();
        draw_click_markers(&mut img, &[]);
        assert_eq!(base.as_raw(), img.as_raw());
        let png = encode_rgba_png(&img).unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }
}

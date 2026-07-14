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
    /// Bounded UI-quiescence wait, anchored at the last input action, run
    /// before the batch's observation. `None` = off (the freshness floor
    /// still applies to captures).
    pub settle: Option<SettleRequest>,
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
    /// Settle wait duration, when a settle ran.
    pub settle_ms: Option<u64>,
}

/// Everything `execute_actions` produces for a batch: per-action results in
/// order (possibly plus one trailing auto-screenshot result), the observation
/// decision, the settle report (when requested), and measurements.
#[derive(Debug)]
pub struct CuBatchOutcome {
    pub results: Vec<crate::computer_use::CuActionResult>,
    pub observation: CuObservation,
    pub settle: Option<SettleReport>,
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
        if let Some(settle) = &self.settle {
            line.push_str(&format!(" settle=[{}]", settle.describe()));
        }
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

// ── Settle: bounded UI quiescence ────────────────────────────────────────────

/// Quiet window: the UI counts as settled once no display content change has
/// been observed for this long. Matches the scale of the freshness floor
/// (`FRESH_FRAME_TIMEOUT`) and sits between caret-blink half-periods
/// (~500 ms), so a blinking cursor still finds a gap.
pub const SETTLE_QUIET_WINDOW_MS: u64 = 300;
/// Cap when the caller enables settle without a budget (`settle: true`).
pub const SETTLE_DEFAULT_CAP_MS: u64 = 2_000;
/// Hard ceiling for a caller-supplied cap.
pub const SETTLE_MAX_CAP_MS: u64 = 5_000;

/// Wire form of the `settle` parameter: `true`/`false`, or a cap in
/// milliseconds. Bounded quiescence instead of model-authored sleeps.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum SettleParam {
    /// `true` = settle with the default cap; `false` = off.
    Enabled(bool),
    /// Cap in milliseconds (clamped to [`SETTLE_MAX_CAP_MS`]; `0` = off).
    CapMs(u64),
}

impl SettleParam {
    /// Resolve the wire form to a request, or `None` when disabled.
    pub fn resolve(self) -> Option<SettleRequest> {
        let cap_ms = match self {
            SettleParam::Enabled(false) => return None,
            SettleParam::CapMs(0) => return None,
            SettleParam::Enabled(true) => SETTLE_DEFAULT_CAP_MS,
            SettleParam::CapMs(ms) => ms.clamp(SETTLE_QUIET_WINDOW_MS, SETTLE_MAX_CAP_MS),
        };
        Some(SettleRequest {
            quiet: std::time::Duration::from_millis(SETTLE_QUIET_WINDOW_MS),
            cap: std::time::Duration::from_millis(cap_ms),
        })
    }
}

/// A resolved settle request: wait for `quiet` of no display change, give up
/// (and say so) at `cap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleRequest {
    pub quiet: std::time::Duration,
    pub cap: std::time::Duration,
}

/// How a settle wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleOutcome {
    /// No display change for the quiet window — the UI is at rest.
    Settled,
    /// The display was still changing when the cap elapsed.
    StillLoading,
    /// No usable damage signal on this path (no live capture session, the
    /// synthetic backend's free-running test card, or a capture stream that
    /// ended mid-wait): a fixed minimal wait ran instead, honestly labeled.
    FixedWait,
}

/// The settle result reported alongside the batch: outcome + elapsed, plus
/// the reason when the damage signal was unavailable.
#[derive(Debug, Clone)]
pub struct SettleReport {
    pub outcome: SettleOutcome,
    pub elapsed_ms: u64,
    pub quiet_ms: u64,
    pub cap_ms: u64,
    pub note: Option<String>,
}

impl SettleReport {
    /// One-line description for tool output.
    pub fn describe(&self) -> String {
        match self.outcome {
            SettleOutcome::Settled => format!(
                "settled after {}ms (no display change for {}ms)",
                self.elapsed_ms, self.quiet_ms
            ),
            SettleOutcome::StillLoading => format!(
                "still_loading after {}ms (display still changing at the {}ms cap)",
                self.elapsed_ms, self.cap_ms
            ),
            SettleOutcome::FixedWait => format!(
                "fixed {}ms wait ({})",
                self.elapsed_ms,
                self.note.as_deref().unwrap_or("no damage signal")
            ),
        }
    }
}

/// Fixed minimal wait for paths without a usable damage signal: sleep the
/// quiet window and report it as exactly that.
pub(crate) async fn settle_fixed_wait(req: SettleRequest, note: &str) -> SettleReport {
    tokio::time::sleep(req.quiet).await;
    SettleReport {
        outcome: SettleOutcome::FixedWait,
        elapsed_ms: req.quiet.as_millis() as u64,
        quiet_ms: req.quiet.as_millis() as u64,
        cap_ms: req.cap.as_millis() as u64,
        note: Some(note.to_string()),
    }
}

/// Settle against a live capture session's frame stream.
///
/// The synthetic test-card backend free-runs a changing counter strip, so it
/// has no quiescence semantics — it degrades to the fixed wait (also what
/// keeps the e2e suite deterministic).
pub(crate) async fn settle_via_session(
    session: &crate::display::DisplaySession,
    baseline: std::time::Instant,
    req: SettleRequest,
) -> SettleReport {
    if crate::display::synthetic::armed() {
        return settle_fixed_wait(req, "synthetic display has no damage signal").await;
    }
    let frames = session.subscribe_frames();
    let latest = session.latest_frame().await;
    quiesce_frames(frames, latest, baseline, req).await
}

/// FNV-1a 64 over a frame's pixel bytes: the content fingerprint behind
/// damage detection on polling backends (X11 re-delivers unchanged frames at
/// the capture rate, so frame *arrival* is not change).
fn frame_fingerprint(frame: &crate::display::Frame) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in &frame.data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Whether a received frame represents display change. Platform-native dirty
/// rects win when present (ScreenCaptureKit); otherwise the frame's content
/// fingerprint is compared against the previous one.
fn frame_is_damage(frame: &crate::display::Frame, fingerprint: &mut Option<u64>) -> bool {
    if let Some(rects) = &frame.dirty_rects {
        return !rects.is_empty();
    }
    let fp = frame_fingerprint(frame);
    let changed = *fingerprint != Some(fp);
    *fingerprint = Some(fp);
    changed
}

/// Core quiescence wait over a frame stream: returns once no damage has been
/// observed for `req.quiet` (counted from `baseline`, the completion of the
/// last input action), giving up at `req.cap`.
///
/// `latest` (the newest frame at subscribe time) seeds the content
/// fingerprint so a polling backend's next unchanged re-delivery does not
/// read as damage; damage timing itself starts at `baseline` — a change that
/// landed before the subscription is already reflected in `latest`, and the
/// quiet window then measures stability since the input.
pub(crate) async fn quiesce_frames(
    mut frames: tokio::sync::broadcast::Receiver<std::sync::Arc<crate::display::Frame>>,
    latest: Option<std::sync::Arc<crate::display::Frame>>,
    baseline: std::time::Instant,
    req: SettleRequest,
) -> SettleReport {
    use tokio::sync::broadcast::error::RecvError;

    let started = std::time::Instant::now();
    let mut fingerprint: Option<u64> = latest.as_ref().map(|f| frame_fingerprint(f));
    let mut last_damage = baseline;
    let cap_deadline = started + req.cap;
    let report = |outcome: SettleOutcome, note: Option<String>| SettleReport {
        outcome,
        elapsed_ms: started.elapsed().as_millis() as u64,
        quiet_ms: req.quiet.as_millis() as u64,
        cap_ms: req.cap.as_millis() as u64,
        note,
    };

    loop {
        let now = std::time::Instant::now();
        if now.duration_since(last_damage) >= req.quiet {
            return report(SettleOutcome::Settled, None);
        }
        if now >= cap_deadline {
            return report(SettleOutcome::StillLoading, None);
        }
        let sleep_until = (last_damage + req.quiet).min(cap_deadline);
        tokio::select! {
            recv = frames.recv() => match recv {
                Ok(frame) => {
                    if frame_is_damage(&frame, &mut fingerprint) {
                        last_damage = frame.timestamp.max(last_damage);
                    }
                }
                // Falling behind the ring means frames are flooding in —
                // that IS activity; count it as damage now.
                Err(RecvError::Lagged(_)) => last_damage = std::time::Instant::now(),
                Err(RecvError::Closed) => {
                    return report(
                        SettleOutcome::FixedWait,
                        Some("capture stream ended during settle".to_string()),
                    );
                }
            },
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(sleep_until)) => {}
        }
    }
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

    // ── Settle ──────────────────────────────────────────────────────────

    #[test]
    fn settle_param_resolves_bool_number_and_clamps() {
        assert!(SettleParam::Enabled(false).resolve().is_none());
        assert!(SettleParam::CapMs(0).resolve().is_none());
        let default = SettleParam::Enabled(true).resolve().unwrap();
        assert_eq!(default.cap.as_millis() as u64, SETTLE_DEFAULT_CAP_MS);
        assert_eq!(default.quiet.as_millis() as u64, SETTLE_QUIET_WINDOW_MS);
        let clamped_up = SettleParam::CapMs(10).resolve().unwrap();
        assert_eq!(clamped_up.cap.as_millis() as u64, SETTLE_QUIET_WINDOW_MS);
        let clamped_down = SettleParam::CapMs(60_000).resolve().unwrap();
        assert_eq!(clamped_down.cap.as_millis() as u64, SETTLE_MAX_CAP_MS);
        let exact = SettleParam::CapMs(1234).resolve().unwrap();
        assert_eq!(exact.cap.as_millis() as u64, 1234);
        // Wire forms: bool and number both parse.
        assert!(matches!(
            serde_json::from_str::<SettleParam>("true").unwrap(),
            SettleParam::Enabled(true)
        ));
        assert!(matches!(
            serde_json::from_str::<SettleParam>("2500").unwrap(),
            SettleParam::CapMs(2500)
        ));
    }

    fn test_frame(
        fill: u8,
        dirty_rects: Option<Vec<crate::display::capture::damage::Rect>>,
    ) -> std::sync::Arc<crate::display::Frame> {
        std::sync::Arc::new(crate::display::Frame {
            data: vec![fill; 2 * 2 * 4],
            format: crate::display::FrameFormat::Bgra,
            width: 2,
            height: 2,
            stride: 8,
            timestamp: std::time::Instant::now(),
            dirty_rects,
        })
    }

    fn small_request() -> SettleRequest {
        SettleRequest {
            quiet: std::time::Duration::from_millis(80),
            cap: std::time::Duration::from_millis(400),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quiesce_settles_when_nothing_changes() {
        let (_tx, rx) = tokio::sync::broadcast::channel(16);
        let report = quiesce_frames(rx, None, std::time::Instant::now(), small_request()).await;
        assert_eq!(report.outcome, SettleOutcome::Settled, "{report:?}");
        assert!(
            report.elapsed_ms < 400,
            "static content must settle inside the cap: {report:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quiesce_reports_still_loading_while_content_keeps_changing() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let feeder = tokio::spawn(async move {
            // Changing content every 25 ms, well past the 400 ms cap.
            for i in 0..28u8 {
                let _ = tx.send(test_frame(i, None));
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });
        let report = quiesce_frames(rx, None, std::time::Instant::now(), small_request()).await;
        feeder.abort();
        assert_eq!(report.outcome, SettleOutcome::StillLoading, "{report:?}");
        assert!(
            report.elapsed_ms >= 350,
            "still_loading must run to the cap: {report:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quiesce_ignores_polled_redeliveries_of_unchanged_content() {
        // The X11 backend re-delivers unchanged frames at the capture rate:
        // arrival is not damage. Seed the fingerprint with the same content
        // the feeder repeats.
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let feeder = tokio::spawn(async move {
            for _ in 0..28u8 {
                let _ = tx.send(test_frame(42, None));
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });
        let report = quiesce_frames(
            rx,
            Some(test_frame(42, None)),
            std::time::Instant::now(),
            small_request(),
        )
        .await;
        feeder.abort();
        assert_eq!(report.outcome, SettleOutcome::Settled, "{report:?}");
        assert!(
            report.elapsed_ms < 400,
            "unchanged polling must settle inside the cap: {report:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quiesce_trusts_empty_native_dirty_rects() {
        // Event-driven backends may deliver frames whose dirty rects are
        // empty — explicitly not damage, even though the bytes differ.
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let feeder = tokio::spawn(async move {
            for i in 0..28u8 {
                let _ = tx.send(test_frame(i, Some(Vec::new())));
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });
        let report = quiesce_frames(rx, None, std::time::Instant::now(), small_request()).await;
        feeder.abort();
        assert_eq!(report.outcome, SettleOutcome::Settled, "{report:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quiesce_reports_fixed_wait_when_the_stream_ends() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        drop(tx);
        let report = quiesce_frames(rx, None, std::time::Instant::now(), small_request()).await;
        assert_eq!(report.outcome, SettleOutcome::FixedWait, "{report:?}");
        assert!(
            report
                .note
                .as_deref()
                .unwrap_or_default()
                .contains("capture stream ended"),
            "{report:?}"
        );
    }

    #[tokio::test]
    async fn settle_fixed_wait_sleeps_the_quiet_window_and_says_why() {
        let report = settle_fixed_wait(small_request(), "no live capture session").await;
        assert_eq!(report.outcome, SettleOutcome::FixedWait);
        assert_eq!(report.elapsed_ms, 80);
        assert!(report.describe().contains("no live capture session"));
    }
}

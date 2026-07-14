//! Per-layer Computer Use readiness probes.
//!
//! Intendant authority (the user-display grant) and OS-level capability
//! (TCC permissions, portal sessions, display presence) are independent
//! layers: `display request` can truthfully answer `already_granted`
//! while macOS Screen Recording still blocks every screenshot and
//! Accessibility blocks every element read. This module probes each
//! layer separately and names the missing ones, so operators see actual
//! CU readiness instead of just Intendant authority.
//!
//! The five layers, in report order:
//!
//! 1. `intendant_display_authority` — the user-display grant / caller trust.
//! 2. `screen_capture_permission` — macOS Screen Recording (TCC) preflight;
//!    Linux X11 socket / Wayland portal session; Windows capture session.
//! 3. `accessibility_permission` — macOS Accessibility (TCC); Linux AT-SPI
//!    bus reachability; Windows UI Automation availability.
//! 4. `target_display` — the requested display actually exists / has a
//!    live capture session.
//! 5. `input_backend` — the injection path for the target (CGEvent, XTest,
//!    portal remote desktop, SendInput).
//!
//! Probes are cheap, read-only (they never pop permission prompts), and
//! deliberately **never cached** — TCC and portal state can change (or be
//! revoked) at any moment, so every call re-reads live state. A probe
//! that cannot determine its layer reports `unknown`, and unknown counts
//! as NOT ready (fail closed) rather than optimistically green.
//!
//! Platform calls stay behind `#[cfg]` guards and use only safe wrappers:
//! `core_graphics::access` (Screen Recording preflight), `crate::ax`
//! (the documented AX island), the `atspi` crate, and
//! `crate::windows_uia` (the documented UIA island). No new `unsafe`
//! lives here.

use serde::Serialize;

use crate::computer_use::DisplayTarget;

/// Stable layer identifiers (the `layer` field of every report row).
pub(crate) const LAYER_AUTHORITY: &str = "intendant_display_authority";
pub(crate) const LAYER_CAPTURE: &str = "screen_capture_permission";
pub(crate) const LAYER_ACCESSIBILITY: &str = "accessibility_permission";
pub(crate) const LAYER_DISPLAY: &str = "target_display";
pub(crate) const LAYER_INPUT: &str = "input_backend";

/// Outcome of one layer probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LayerStatus {
    /// The layer was positively verified.
    Ready,
    /// The layer was positively verified as missing/blocked.
    Blocked,
    /// The probe could not determine the layer's state. Treated as not
    /// ready (fail closed).
    Unknown,
}

/// One probed layer: what was checked, what came back, how to fix it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReadinessLayer {
    pub layer: &'static str,
    pub status: LayerStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// A full per-layer readiness report for one display target.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CuReadiness {
    pub target: String,
    /// True only when every layer probed `ready`. `unknown` layers keep
    /// this false — a readiness report must never overstate capability.
    pub ready: bool,
    /// One-line human summary naming the non-ready layers.
    pub summary: String,
    pub layers: Vec<ReadinessLayer>,
}

impl CuReadiness {
    /// Compact JSON of the readiness gap: overall flag plus only the
    /// non-ready layers. Used to enrich other tools' results
    /// (`request_user_display`) without repeating the green layers.
    pub(crate) fn gap_json(&self) -> serde_json::Value {
        let not_ready: Vec<serde_json::Value> = self
            .layers
            .iter()
            .filter(|layer| layer.status != LayerStatus::Ready)
            .map(|layer| serde_json::to_value(layer).unwrap_or_default())
            .collect();
        serde_json::json!({
            "ready": self.ready,
            "summary": self.summary,
            "not_ready_layers": not_ready,
        })
    }
}

/// Internal probe result before it is bound to a layer id.
#[derive(Debug, Clone)]
pub(crate) struct Probe {
    status: LayerStatus,
    detail: String,
    fix: Option<String>,
}

impl Probe {
    pub(crate) fn ready(detail: impl Into<String>) -> Self {
        Probe {
            status: LayerStatus::Ready,
            detail: detail.into(),
            fix: None,
        }
    }

    pub(crate) fn blocked(detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Probe {
            status: LayerStatus::Blocked,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    pub(crate) fn unknown(detail: impl Into<String>) -> Self {
        Probe {
            status: LayerStatus::Unknown,
            detail: detail.into(),
            fix: None,
        }
    }

    fn into_layer(self, layer: &'static str) -> ReadinessLayer {
        ReadinessLayer {
            layer,
            status: self.status,
            detail: self.detail,
            fix: self.fix,
        }
    }
}

/// The four OS-level probe results (everything except Intendant authority).
#[derive(Debug, Clone)]
pub(crate) struct OsProbes {
    pub capture: Probe,
    pub accessibility: Probe,
    pub display: Probe,
    pub input: Probe,
}

// ── Pure assembly (unit-tested; probes injected) ────────────────────────────

/// Layer 1: Intendant display authority for the target. Pure.
pub(crate) fn authority_probe(
    target_is_user_session: bool,
    user_session_allowed: bool,
    user_display_granted: bool,
) -> Probe {
    if !target_is_user_session {
        return Probe::ready("agent-owned virtual display — no user-display grant required");
    }
    if user_display_granted {
        return Probe::ready("user-display grant held");
    }
    if user_session_allowed {
        return Probe::ready(
            "owner surface — user-session access allowed without an explicit grant",
        );
    }
    Probe::blocked(
        "the user-display grant is not held and this caller is not an owner surface",
        "ask for it with request_user_display (or `intendant ctl display request`); \
         only the user's click can grant it",
    )
}

/// Bind the probes to their layers and compute the overall verdict. Pure.
pub(crate) fn assemble_readiness(target: String, authority: Probe, os: OsProbes) -> CuReadiness {
    let layers = vec![
        authority.into_layer(LAYER_AUTHORITY),
        os.capture.into_layer(LAYER_CAPTURE),
        os.accessibility.into_layer(LAYER_ACCESSIBILITY),
        os.display.into_layer(LAYER_DISPLAY),
        os.input.into_layer(LAYER_INPUT),
    ];
    let ready = layers
        .iter()
        .all(|layer| layer.status == LayerStatus::Ready);
    let summary = readiness_summary(&target, &layers);
    CuReadiness {
        target,
        ready,
        summary,
        layers,
    }
}

/// One-line summary naming the non-ready layers. Pure.
fn readiness_summary(target: &str, layers: &[ReadinessLayer]) -> String {
    let blocked: Vec<&str> = layers
        .iter()
        .filter(|l| l.status == LayerStatus::Blocked)
        .map(|l| l.layer)
        .collect();
    let unknown: Vec<&str> = layers
        .iter()
        .filter(|l| l.status == LayerStatus::Unknown)
        .map(|l| l.layer)
        .collect();
    if blocked.is_empty() && unknown.is_empty() {
        return format!("READY: all layers verified for {target}");
    }
    let mut parts = Vec::new();
    if !blocked.is_empty() {
        parts.push(format!("blocked: {}", blocked.join(", ")));
    }
    if !unknown.is_empty() {
        parts.push(format!(
            "unknown (treated as not ready): {}",
            unknown.join(", ")
        ));
    }
    format!("NOT READY for {target} — {}", parts.join("; "))
}

/// CU-04 pure core: enrich a capture failure with the likely-missing
/// permission, the affected binary, and the settings destination — only
/// when the preflight positively indicates the permission is missing.
/// `preflight_granted: None` (probe unavailable) passes the raw error
/// through unchanged: never blame a permission the probe didn't confirm.
pub(crate) fn capture_failure_with_permission_hint(
    raw: &str,
    preflight_granted: Option<bool>,
    binary: Option<&str>,
) -> String {
    match preflight_granted {
        Some(false) => {
            let binary_note = binary
                .map(|b| format!(" ({b})"))
                .unwrap_or_default();
            format!(
                "{raw} — the Screen Recording (TCC) permission is missing for this \
                 process{binary_note}. Grant it in System Settings → Privacy & Security → \
                 Screen Recording (add or re-toggle the entry), then relaunch Intendant — \
                 grants are only re-read at launch, and a rebuilt/re-signed binary silently \
                 invalidates a prior grant even while the toggle still shows ON."
            )
        }
        _ => raw.to_string(),
    }
}

/// The label used when naming the affected process in permission guidance.
fn process_binary_label() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "the Intendant binary".to_string())
}

// ── Transport edge: live probes (cheap, uncached, read-only) ────────────────

/// Probe all five layers for `target`. Never cached; safe to call on every
/// status/request/error path.
pub(crate) async fn probe_readiness(
    target: DisplayTarget,
    user_session_allowed: bool,
    user_display_granted: bool,
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) -> CuReadiness {
    let authority = authority_probe(
        target.is_user_session(),
        user_session_allowed,
        user_display_granted,
    );
    let os = os_probes(target, session_registry).await;
    assemble_readiness(target.to_string(), authority, os)
}

/// Convenience for enriching user-display flows (`request_user_display`):
/// the OS-layer readiness of `user_session` with authority taken as held.
pub(crate) async fn probe_user_session_os_readiness(
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) -> CuReadiness {
    probe_readiness(DisplayTarget::UserSession, true, true, session_registry).await
}

/// CU-04 edge: enrich a failed capture's error message when the platform
/// preflight indicates the capture permission is missing. Pass-through on
/// platforms without a capture-permission concept and on non-permission
/// failures. Read-only: never pops the OS permission prompt.
pub(crate) fn enrich_capture_failure(raw: String) -> String {
    #[cfg(target_os = "macos")]
    {
        let granted = core_graphics::access::ScreenCaptureAccess.preflight();
        capture_failure_with_permission_hint(
            &raw,
            Some(granted),
            Some(process_binary_label().as_str()),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        raw
    }
}

/// Resolution of the live, agent-visible capture session for `target`
/// (private user views deliberately read as absent, matching the CU
/// screenshot path's lens).
async fn live_session_resolution(
    session_registry: &Option<crate::display::SharedSessionRegistry>,
    target: &DisplayTarget,
) -> Option<(u32, u32)> {
    let registry = session_registry.as_ref()?;
    let display_id = match target {
        DisplayTarget::UserSession => 0,
        DisplayTarget::Virtual { id } => *id,
    };
    let session = registry.read().await.get(display_id)?;
    Some(session.resolution())
}

/// OS-level probes for the target, dispatched per platform. Every branch
/// degrades gracefully — unsupported combinations report `blocked` or
/// `unknown` with a reason, never a panic or a silent green.
async fn os_probes(
    target: DisplayTarget,
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) -> OsProbes {
    let session_resolution = live_session_resolution(session_registry, &target).await;
    #[cfg(target_os = "macos")]
    {
        macos_probes(target, session_resolution)
    }
    #[cfg(target_os = "linux")]
    {
        linux_probes(target, session_resolution).await
    }
    #[cfg(windows)]
    {
        windows_probes(target, session_resolution).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (target, session_resolution);
        let unsupported = || Probe::unknown("readiness probing is not implemented on this platform");
        OsProbes {
            capture: unsupported(),
            accessibility: unsupported(),
            display: unsupported(),
            input: unsupported(),
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_probes(target: DisplayTarget, session_resolution: Option<(u32, u32)>) -> OsProbes {
    let binary = process_binary_label();
    let relaunch_fix = |pane: &str| {
        format!(
            "System Settings → Privacy & Security → {pane}: add or re-toggle {binary}, \
             then relaunch Intendant — grants are only re-read at launch, and a \
             rebuilt/re-signed binary silently invalidates a prior grant even while \
             the toggle still shows ON"
        )
    };

    // Screen Recording: CGPreflightScreenCaptureAccess via the safe
    // core-graphics wrapper. Read-only (the request/prompt variant is
    // deliberately not called from a probe).
    let capture = if core_graphics::access::ScreenCaptureAccess.preflight() {
        Probe::ready("Screen Recording (TCC) permission granted")
    } else {
        Probe::blocked(
            format!(
                "Screen Recording (TCC) permission is not granted to this process \
                 ({binary}) — screenshots and display capture will fail"
            ),
            relaunch_fix("Screen Recording"),
        )
    };

    let ax_trusted = crate::ax::is_trusted();
    let accessibility = if ax_trusted {
        Probe::ready("Accessibility (TCC) permission granted")
    } else {
        Probe::blocked(
            format!(
                "Accessibility (TCC) permission is not granted to this process \
                 ({binary}) — read_screen element trees and input injection will fail"
            ),
            relaunch_fix("Accessibility"),
        )
    };

    let display = match target {
        DisplayTarget::Virtual { id } => Probe::blocked(
            format!(":{id} cannot exist on macOS — virtual displays are Xvfb/Linux"),
            "target the user session instead (display_target=\"user_session\")",
        ),
        DisplayTarget::UserSession => match session_resolution {
            Some((w, h)) => Probe::ready(format!("live capture session ({w}x{h})")),
            None => match crate::platform::main_display_pixel_size() {
                Some((w, h)) => Probe::ready(format!("main display present ({w}x{h} physical)")),
                None => Probe::unknown(
                    "could not read the main display size — the daemon may be running \
                     outside the GUI login session",
                ),
            },
        },
    };

    let input = if ax_trusted {
        Probe::ready("CGEvent input injection available (rides the Accessibility permission)")
    } else {
        Probe::blocked(
            "CGEvent input injection is blocked without the Accessibility (TCC) permission",
            relaunch_fix("Accessibility"),
        )
    };

    OsProbes {
        capture,
        accessibility,
        display,
        input,
    }
}

/// Bound on the AT-SPI reachability probe: a readiness check must answer
/// fast even when the session bus is wedged.
#[cfg(target_os = "linux")]
const ATSPI_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(target_os = "linux")]
async fn linux_probes(target: DisplayTarget, session_resolution: Option<(u32, u32)>) -> OsProbes {
    // Effective capture backend mirrors execute_actions: the user session
    // rides the Wayland portal when WAYLAND_DISPLAY is set; virtual
    // displays are always X11 (Xvfb), even on a Wayland host.
    let portal = std::env::var("WAYLAND_DISPLAY").is_ok() && target.is_user_session();

    let accessibility = atspi_probe().await;

    if portal {
        let portal_fix = format!(
            "re-grant with grant_user_display (or `intendant ctl display grant-user`) and \
             approve the screen-sharing portal dialog on the physical display with \
             Allow Remote Interaction enabled. {}",
            crate::linux_display_env::diagnostic_summary()
        );
        let (capture, display, input) = match session_resolution {
            Some((w, h)) => (
                Probe::ready(format!("live Wayland portal capture session ({w}x{h})")),
                Probe::ready(format!(
                    "the portal capture session is the display handle on Wayland ({w}x{h})"
                )),
                Probe::ready(
                    "portal remote-desktop session live — Remote Interaction was verified \
                     when the share was granted",
                ),
            ),
            None => (
                Probe::blocked(
                    "no live Wayland portal capture session for the user display",
                    portal_fix.clone(),
                ),
                Probe::blocked(
                    "display presence unknown without a portal session (Wayland exposes \
                     the user display only through the portal)",
                    portal_fix.clone(),
                ),
                Probe::blocked(
                    "portal input injection needs a live remote-desktop session",
                    portal_fix,
                ),
            ),
        };
        return OsProbes {
            capture,
            accessibility,
            display,
            input,
        };
    }

    // X11 path (user session on X11, or any virtual/Xvfb display).
    let display_env = target.display_env_string();
    let socket = x11_socket_path(&display_env);
    let socket_up = socket
        .as_ref()
        .map(|path| path.exists())
        .unwrap_or(false);
    let (capture, display, input) = if socket_up {
        let session_note = session_resolution
            .map(|(w, h)| format!("; live capture session ({w}x{h})"))
            .unwrap_or_default();
        (
            Probe::ready(format!(
                "X11 display {display_env} socket present{session_note}"
            )),
            Probe::ready(format!("X11 display {display_env} socket present")),
            Probe::ready("XTest injection over the X11 connection".to_string()),
        )
    } else {
        let fix = match target {
            DisplayTarget::Virtual { id } => format!(
                "start the virtual display first: `Xvfb :{id} -screen 0 1920x1080x24 &`"
            ),
            DisplayTarget::UserSession => format!(
                "the daemon may lack the GUI session environment. {}",
                crate::linux_display_env::diagnostic_summary()
            ),
        };
        let detail = |what: &str| {
            format!(
                "{what} unavailable: no X socket for display {display_env}{}",
                socket
                    .as_ref()
                    .map(|p| format!(" ({})", p.display()))
                    .unwrap_or_default()
            )
        };
        (
            Probe::blocked(detail("X11 capture"), fix.clone()),
            Probe::blocked(detail("X11 display"), fix.clone()),
            Probe::blocked(detail("XTest input injection"), fix),
        )
    };
    OsProbes {
        capture,
        accessibility,
        display,
        input,
    }
}

/// AT-SPI session accessibility bus reachability, bounded so a wedged bus
/// cannot stall a readiness report. Skips the connect attempt entirely
/// when no session-bus environment exists (fast, deterministic).
#[cfg(target_os = "linux")]
async fn atspi_probe() -> Probe {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
        && std::env::var_os("XDG_RUNTIME_DIR").is_none()
    {
        return Probe::blocked(
            "no session bus environment (DBUS_SESSION_BUS_ADDRESS / XDG_RUNTIME_DIR \
             unset) — the AT-SPI accessibility bus is unreachable",
            format!(
                "run inside the desktop session or adopt its environment. {}",
                crate::linux_display_env::diagnostic_summary()
            ),
        );
    }
    match tokio::time::timeout(
        ATSPI_PROBE_TIMEOUT,
        atspi::connection::AccessibilityConnection::new(),
    )
    .await
    {
        Ok(Ok(_conn)) => Probe::ready("AT-SPI accessibility bus reachable"),
        Ok(Err(e)) => Probe::blocked(
            format!("AT-SPI accessibility bus unreachable: {e}"),
            "read_screen needs a desktop session with at-spi2-core (Debian: \
             `apt install at-spi2-core`; GNOME/KDE ship it by default)",
        ),
        Err(_) => Probe::unknown(format!(
            "AT-SPI accessibility bus probe timed out after {}s — treat as not ready",
            ATSPI_PROBE_TIMEOUT.as_secs()
        )),
    }
}

#[cfg(windows)]
async fn windows_probes(target: DisplayTarget, session_resolution: Option<(u32, u32)>) -> OsProbes {
    // UIA availability: COM apartment + client instantiation, probed on a
    // blocking thread (COM must not run on the async worker).
    let accessibility =
        match tokio::task::spawn_blocking(crate::windows_uia::probe_available).await {
            Ok(Ok(())) => Probe::ready("UI Automation (UIA) client available"),
            Ok(Err(e)) => Probe::blocked(
                format!("UI Automation (UIA) client unavailable: {e}"),
                "read_screen element trees need UIA (COM); check that the daemon runs in \
                 an interactive desktop session",
            ),
            Err(e) => Probe::unknown(format!("UIA probe task failed: {e} — treat as not ready")),
        };

    if let DisplayTarget::Virtual { id } = target {
        let blocked = || {
            Probe::blocked(
                format!(":{id} cannot exist on Windows — virtual displays are Xvfb/Linux"),
                "target the desktop with display_target=\"user_session\" instead",
            )
        };
        return OsProbes {
            capture: blocked(),
            accessibility,
            display: blocked(),
            input: blocked(),
        };
    }

    // Windows screen capture has no user-facing permission gate; readiness
    // is the registered desktop capture session (DXGI + SendInput ride it).
    let session_fix = "the desktop display normally auto-registers at daemon startup; \
                       re-request it with grant_user_display (or `intendant ctl display \
                       grant-user`) and retry";
    let (capture, display, input) = match session_resolution {
        Some((w, h)) => (
            Probe::ready(format!("desktop capture session live ({w}x{h}, DXGI)")),
            Probe::ready(format!("desktop capture session live ({w}x{h})")),
            Probe::ready("SendInput injection rides the active capture session"),
        ),
        None => (
            Probe::blocked("no desktop capture session registered", session_fix),
            Probe::blocked(
                "display presence unknown without the desktop capture session",
                session_fix,
            ),
            Probe::blocked(
                "SendInput injection needs the desktop capture session",
                session_fix,
            ),
        ),
    };
    OsProbes {
        capture,
        accessibility,
        display,
        input,
    }
}

/// `":0"`-style display string → the X socket path that backs it.
/// Returns `None` for non-local display strings.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn x11_socket_path(display: &str) -> Option<std::path::PathBuf> {
    let rest = display.strip_prefix(':')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(format!("/tmp/.X11-unix/X{digits}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_os_probes() -> OsProbes {
        OsProbes {
            capture: Probe::ready("capture ok"),
            accessibility: Probe::ready("ax ok"),
            display: Probe::ready("display ok"),
            input: Probe::ready("input ok"),
        }
    }

    #[test]
    fn authority_probe_covers_grant_owner_and_denied() {
        let granted = authority_probe(true, true, true);
        assert_eq!(granted.status, LayerStatus::Ready);
        assert!(granted.detail.contains("grant held"));

        let owner = authority_probe(true, true, false);
        assert_eq!(owner.status, LayerStatus::Ready);
        assert!(owner.detail.contains("owner surface"));

        let denied = authority_probe(true, false, false);
        assert_eq!(denied.status, LayerStatus::Blocked);
        assert!(denied.fix.as_deref().unwrap().contains("request_user_display"));

        let virtual_target = authority_probe(false, false, false);
        assert_eq!(virtual_target.status, LayerStatus::Ready);
        assert!(virtual_target.detail.contains("no user-display grant required"));
    }

    #[test]
    fn assemble_readiness_all_ready() {
        let report = assemble_readiness(
            "user_session".to_string(),
            Probe::ready("authority ok"),
            ready_os_probes(),
        );
        assert!(report.ready);
        assert!(report.summary.starts_with("READY"));
        assert_eq!(report.layers.len(), 5);
        assert_eq!(report.layers[0].layer, LAYER_AUTHORITY);
        assert_eq!(report.layers[1].layer, LAYER_CAPTURE);
        assert_eq!(report.layers[2].layer, LAYER_ACCESSIBILITY);
        assert_eq!(report.layers[3].layer, LAYER_DISPLAY);
        assert_eq!(report.layers[4].layer, LAYER_INPUT);
    }

    #[test]
    fn assemble_readiness_names_blocked_layers_and_fails_closed_on_unknown() {
        let mut os = ready_os_probes();
        os.capture = Probe::blocked("no TCC", "grant it");
        os.display = Probe::unknown("could not read display size");
        let report = assemble_readiness(
            "user_session".to_string(),
            Probe::ready("authority ok"),
            os,
        );
        assert!(!report.ready, "blocked/unknown layers must fail closed");
        assert!(report.summary.contains("NOT READY"));
        assert!(report.summary.contains(LAYER_CAPTURE));
        assert!(report.summary.contains(LAYER_DISPLAY));
        assert!(report.summary.contains("treated as not ready"));

        // Unknown-only reports are also not ready (fail closed).
        let mut os = ready_os_probes();
        os.accessibility = Probe::unknown("probe timed out");
        let report =
            assemble_readiness("user_session".to_string(), Probe::ready("ok"), os);
        assert!(!report.ready);
    }

    #[test]
    fn gap_json_lists_only_non_ready_layers() {
        let mut os = ready_os_probes();
        os.capture = Probe::blocked("no TCC", "grant it in System Settings");
        let report = assemble_readiness(
            "user_session".to_string(),
            Probe::ready("authority ok"),
            os,
        );
        let gap = report.gap_json();
        assert_eq!(gap["ready"], serde_json::json!(false));
        let rows = gap["not_ready_layers"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["layer"], LAYER_CAPTURE);
        assert_eq!(rows[0]["status"], "blocked");
        assert!(rows[0]["fix"].as_str().unwrap().contains("System Settings"));
    }

    #[test]
    fn capture_failure_hint_only_fires_on_confirmed_denial() {
        let raw = "screencapture failed: could not create image from display";
        let enriched =
            capture_failure_with_permission_hint(raw, Some(false), Some("/opt/intendant"));
        assert!(enriched.starts_with(raw), "raw error must be preserved");
        assert!(enriched.contains("Screen Recording"));
        assert!(enriched.contains("/opt/intendant"));
        assert!(enriched.contains("System Settings → Privacy & Security → Screen Recording"));
        assert!(enriched.contains("relaunch"));

        // Permission verified present, or probe unavailable: pass through
        // unchanged — never blame a permission the probe didn't confirm.
        assert_eq!(
            capture_failure_with_permission_hint(raw, Some(true), Some("/opt/intendant")),
            raw
        );
        assert_eq!(capture_failure_with_permission_hint(raw, None, None), raw);
    }

    #[test]
    fn x11_socket_path_parses_local_displays_only() {
        assert_eq!(
            x11_socket_path(":0"),
            Some(std::path::PathBuf::from("/tmp/.X11-unix/X0"))
        );
        assert_eq!(
            x11_socket_path(":99"),
            Some(std::path::PathBuf::from("/tmp/.X11-unix/X99"))
        );
        // Screen suffixes parse to the display number.
        assert_eq!(
            x11_socket_path(":1.0"),
            Some(std::path::PathBuf::from("/tmp/.X11-unix/X1"))
        );
        assert_eq!(x11_socket_path("remote:0"), None);
        assert_eq!(x11_socket_path(""), None);
        assert_eq!(x11_socket_path(":"), None);
    }

    #[test]
    fn readiness_serializes_with_stable_field_names() {
        let report = assemble_readiness(
            "user_session".to_string(),
            Probe::ready("authority ok"),
            ready_os_probes(),
        );
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["target"], "user_session");
        assert_eq!(value["ready"], serde_json::json!(true));
        assert!(value["summary"].as_str().unwrap().starts_with("READY"));
        let layers = value["layers"].as_array().unwrap();
        assert_eq!(layers.len(), 5);
        assert_eq!(layers[0]["status"], "ready");
        assert!(
            layers[0].get("fix").is_none(),
            "ready layers must not carry a fix field"
        );
    }
}

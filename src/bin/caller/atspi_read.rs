//! Linux AT-SPI2 element-tree reader (read-only observation).
//!
//! Fills the portable [`ScreenElements`] shape from the session's
//! accessibility bus — works on both X11 and Wayland, since AT-SPI is a
//! session-level D-Bus service independent of the display server. Honors the
//! same depth/node caps as the macOS AX reader.
//!
//! Privacy posture (see the read_screen docs): from the operator's own
//! session, one bounded walk per explicit call — no background polling, no
//! tree caches. Labels and readable values are capped; password fields are
//! skipped. Enabling `org.a11y.Status.IsEnabled` (the platform's mechanism for
//! making toolkits expose trees) happens on demand at first use, never at
//! daemon startup.
//!
//! Layout follows the keymap precedent: pure role/field mapping stays ungated
//! so its unit tests run on every host; the D-Bus walk is Linux-only (the
//! `atspi` crate is a Linux-target dependency).

use crate::computer_use::UiElement;

/// Cap for label/value text carried per element.
const TEXT_CAP: usize = 80;

fn normalized_role_key(atspi_role_name: &str) -> String {
    atspi_role_name
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .collect::<String>()
        .to_lowercase()
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn role_is_password_text(atspi_role_name: &str) -> bool {
    normalized_role_key(atspi_role_name) == "passwordtext"
}

fn truncate(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let cut: String = text.chars().take(cap).collect();
    format!("{cut}...")
}

fn clean_text(text: Option<String>) -> Option<String> {
    text.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| truncate(&s, TEXT_CAP))
}

/// Map an AT-SPI role name to the lowercase role vocabulary shared with the
/// macOS AX and Windows UIA readers (`format_screen_elements` keys its
/// collapse rule on `group`/`generic`/`unknown`).
///
/// AT-SPI role names arrive as lowercase space-separated words
/// (`"push button"`, `"page tab list"`); normalization strips separators so
/// PascalCase spellings match too. Unmatched roles pass through normalized —
/// AT-SPI's vocabulary is already descriptive.
fn role_to_shared(atspi_role_name: &str) -> String {
    let normalized = normalized_role_key(atspi_role_name);
    let mapped = match normalized.as_str() {
        "pushbutton" | "togglebutton" => "button",
        "checkbox" => "checkbox",
        "radiobutton" => "radiobutton",
        "entry" | "passwordtext" | "autocomplete" | "editbar" => "textfield",
        "text" | "label" | "paragraph" | "blockquote" | "caption" | "static" => "text",
        "heading" => "heading",
        "link" => "link",
        "frame" | "window" => "window",
        "dialog" | "filechooser" | "colorchooser" | "alert" => "dialog",
        "panel" | "filler" | "section" | "scrollpane" | "viewport" | "splitpane" | "glasspane"
        | "rootpane" | "layeredpane" | "internalframe" | "desktopframe" | "form" | "grouping"
        | "embedded" | "canvas" => "group",
        "menubar" => "menubar",
        "menu" => "menu",
        "menuitem" | "checkmenuitem" | "radiomenuitem" | "tearoffmenuitem" => "menuitem",
        "pagetablist" => "tabgroup",
        "pagetab" => "tab",
        "list" | "listbox" => "list",
        "listitem" => "listitem",
        "table" => "table",
        "tablerow" => "row",
        "tablecell" => "cell",
        "columnheader" | "rowheader" | "tablecolumnheader" | "tablerowheader" => "headeritem",
        "toolbar" => "toolbar",
        "statusbar" => "statusbar",
        "titlebar" => "titlebar",
        "combobox" => "combobox",
        "slider" => "slider",
        "spinbutton" => "spinner",
        "progressbar" => "progressbar",
        "scrollbar" => "scrollbar",
        "tree" | "treetable" => "tree",
        "treeitem" => "treeitem",
        "image" | "icon" => "image",
        "separator" => "separator",
        "tooltip" => "tooltip",
        "documentframe"
        | "documentweb"
        | "documenttext"
        | "documentemail"
        | "documentpresentation"
        | "documentspreadsheet" => "document",
        "application" => "application",
        "unknown" | "invalid" | "extended" | "redundantobject" => "unknown",
        _ => "",
    };
    if mapped.is_empty() {
        if normalized.is_empty() {
            "unknown".to_string()
        } else {
            normalized
        }
    } else {
        mapped.to_string()
    }
}

/// Assemble a [`UiElement`] from raw AT-SPI fields.
///
/// `extents` is the Component interface's (x, y, width, height) in screen
/// coordinates — width/height arrive as `i32` and clamp to zero when a
/// broken toolkit reports negatives. Empty/whitespace names normalize to
/// `None` so the structural-collapse rule can fire.
// Consumed by the cfg(linux) AT-SPI walk; allow until that lands everywhere.
#[allow(dead_code)]
fn make_element(
    atspi_role_name: &str,
    name: Option<String>,
    value: Option<String>,
    extents: (i32, i32, i32, i32),
    focused: bool,
    enabled: bool,
    children: Vec<UiElement>,
) -> UiElement {
    let (x, y, w, h) = extents;
    UiElement {
        role: role_to_shared(atspi_role_name),
        label: clean_text(name),
        value: clean_text(value),
        frame: (x, y, w.max(0) as u32, h.max(0) as u32),
        focused,
        enabled,
        children,
    }
}

/// Hard wall-clock bound on one whole read: a hung app's accessibility
/// interface must not stall the tool (macOS bounds per-element at 1s; here
/// one deadline covers the bounded walk).
#[cfg(target_os = "linux")]
const READ_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Read the active window's element tree from the session accessibility bus.
///
/// One bounded walk per explicit call — no background polling, no caching.
/// The first call flips `org.a11y.Status.IsEnabled` (the platform mechanism
/// that makes toolkits expose trees); apps already running may need a restart
/// to honor it, which the result's hint text explains.
#[cfg(target_os = "linux")]
pub async fn read_frontmost(
    max_depth: usize,
    max_nodes: usize,
) -> Result<crate::computer_use::ScreenElements, String> {
    tokio::time::timeout(READ_DEADLINE, walk::read_frontmost(max_depth, max_nodes))
        .await
        .map_err(|_| {
            format!(
                "AT-SPI read timed out after {}s — an application's accessibility \
                 interface may be hung; retry, or use take_screenshot instead",
                READ_DEADLINE.as_secs()
            )
        })?
}

#[cfg(target_os = "linux")]
mod walk {
    use super::{make_element, role_is_password_text, UiElement, TEXT_CAP};
    use crate::computer_use::ScreenElements;
    use atspi::connection::AccessibilityConnection;
    use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
    use atspi::proxy::component::ComponentProxy;
    use atspi::proxy::text::TextProxy;
    use atspi::proxy::value::ValueProxy;
    use atspi::{CoordType, State};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Caps on the application/toplevel scan (before the element walk).
    const MAX_APPS: usize = 128;
    const MAX_WINDOWS_PER_APP: usize = 32;
    const MAX_OTHER_WINDOWS: usize = 10;
    const MAX_ACTIVE_CANDIDATES: usize = 8;

    /// Enable session accessibility once per process, on demand. Toolkits
    /// consult `org.a11y.Status.IsEnabled` at startup; without it GTK still
    /// registers but Qt and some browsers expose nothing.
    async fn ensure_session_accessibility() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static DONE: AtomicBool = AtomicBool::new(false);
        if DONE.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = atspi::connection::set_session_accessibility(true).await {
            eprintln!("[read_screen] could not enable org.a11y.Status.IsEnabled: {e}");
        }
    }

    pub(super) async fn read_frontmost(
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<ScreenElements, String> {
        ensure_session_accessibility().await;

        let conn = AccessibilityConnection::new().await.map_err(|e| {
            format!(
                "no session accessibility bus: {e} — AT-SPI needs a desktop session \
                 with at-spi2-core (Debian: `apt install at-spi2-core`, present on \
                 GNOME/KDE by default)"
            )
        })?;

        let registry_root = conn
            .root_accessible_on_registry()
            .await
            .map_err(|e| format!("accessibility registry root: {e}"))?;
        let apps = registry_root
            .get_children()
            .await
            .map_err(|e| format!("list accessible applications: {e}"))?;

        // Scan applications for toplevels carrying the Active state; collect
        // other visible toplevels for orientation. Multiple toplevels can
        // claim Active — window managers (e.g. xfwm4) park a dummy Active
        // frame offscreen — so geometry picks the plausible one afterwards.
        let mut active_candidates = Vec::new();
        let mut other_windows: Vec<String> = Vec::new();
        for app_ref in apps.into_iter().take(MAX_APPS) {
            let Ok(app_proxy) = app_ref
                .clone()
                .into_accessible_proxy(conn.connection())
                .await
            else {
                continue;
            };
            let app_name = app_proxy.name().await.unwrap_or_default();
            let Ok(windows) = app_proxy.get_children().await else {
                continue;
            };
            for win_ref in windows.into_iter().take(MAX_WINDOWS_PER_APP) {
                let Ok(win) = win_ref.into_accessible_proxy(conn.connection()).await else {
                    continue;
                };
                let Ok(states) = win.get_state().await else {
                    continue;
                };
                if !states.contains(State::Showing) {
                    continue;
                }
                let title = win.name().await.unwrap_or_default();
                if states.contains(State::Active) && active_candidates.len() < MAX_ACTIVE_CANDIDATES
                {
                    active_candidates.push((app_name.clone(), win, title));
                } else if !title.trim().is_empty() && other_windows.len() < MAX_OTHER_WINDOWS {
                    other_windows.push(format!("{title} ({app_name})"));
                }
            }
        }

        // Choose the Active claimant with plausible on-screen geometry —
        // largest sane area wins; without a sane one, the first claimant
        // stands (never regress to "no active window" when something claimed
        // it). Demoted claimants join other_windows.
        let mut chosen = None;
        let mut chosen_area: i64 = -1;
        let mut demoted: Vec<String> = Vec::new();
        for (app_name, win, title) in active_candidates {
            let (x, y, w, h) = component_extents(&conn, &win).await.unwrap_or((0, 0, 0, 0));
            let sane = w >= 16 && h >= 16 && x.saturating_add(w) > 0 && y.saturating_add(h) > 0;
            let area = if sane { w as i64 * h as i64 } else { -1 };
            if chosen.is_none() || area > chosen_area {
                if let Some((old_app, _, old_title)) = chosen.replace((app_name, win, title)) {
                    if !old_title.trim().is_empty() {
                        demoted.push(format!("{old_title} ({old_app})"));
                    }
                }
                chosen_area = area;
            } else if !title.trim().is_empty() {
                demoted.push(format!("{title} ({app_name})"));
            }
        }
        for demoted_title in demoted {
            if other_windows.len() < MAX_OTHER_WINDOWS {
                other_windows.push(demoted_title);
            }
        }

        let Some((app, window, title)) = chosen else {
            return Ok(ScreenElements {
                app: "(no active window)".to_string(),
                pid: 0,
                window_title: None,
                root: None,
                other_windows,
                truncated: Some(
                    "no toplevel reports the Active state — focus the target window; \
                     apps started before accessibility was enabled may need a restart \
                     (Qt: QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1)"
                        .to_string(),
                ),
            });
        };

        let pid = pid_of(&conn, window.inner().destination().as_str()).await;
        let budget = AtomicUsize::new(max_nodes);
        let truncated = AtomicBool::new(false);
        let root = walk_element(&conn, window, max_depth, &budget, &truncated).await;
        let truncated = truncated.load(Ordering::SeqCst);

        let mut truncated_msg = truncated
            .then(|| format!("element tree truncated at {max_nodes} nodes / depth {max_depth}"));
        if let Some(root_el) = &root {
            if root_el.children.is_empty() && truncated_msg.is_none() {
                truncated_msg = Some(
                    "the active window exposes no child elements — if the app predates \
                     accessibility enablement, restart it (Qt: \
                     QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1)"
                        .to_string(),
                );
            }
        }

        Ok(ScreenElements {
            app,
            pid,
            window_title: (!title.trim().is_empty()).then_some(title),
            root,
            other_windows,
            truncated: truncated_msg,
        })
    }

    /// Resolve the pid behind a unique bus name on the accessibility bus
    /// (AT-SPI's Accessible interface does not expose pids directly).
    async fn pid_of(conn: &AccessibilityConnection, bus_name: &str) -> i32 {
        let Ok(dbus) = ashpd::zbus::fdo::DBusProxy::new(conn.connection()).await else {
            return 0;
        };
        let Ok(name) = ashpd::zbus::names::BusName::try_from(bus_name.to_string()) else {
            return 0;
        };
        dbus.get_connection_unix_process_id(name)
            .await
            .map(|pid| pid as i32)
            .unwrap_or(0)
    }

    /// Depth-first bounded walk. Proxies borrow the connection (`'a`); the
    /// budget/truncated flags are atomics so the recursive boxed future stays
    /// `Send` without `&mut` reborrow gymnastics.
    fn walk_element<'a>(
        conn: &'a AccessibilityConnection,
        el: AccessibleProxy<'a>,
        depth_left: usize,
        budget: &'a AtomicUsize,
        truncated: &'a AtomicBool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<UiElement>> + Send + 'a>> {
        Box::pin(async move {
            if budget
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
                .is_err()
            {
                truncated.store(true, Ordering::SeqCst);
                return None;
            }

            let role = el.get_role_name().await.unwrap_or_default();
            let name = el.name().await.ok();
            let states = el.get_state().await.ok();
            let (focused, enabled, showing) = match &states {
                Some(s) => (
                    s.contains(State::Focused),
                    s.contains(State::Enabled) || s.contains(State::Sensitive),
                    s.contains(State::Showing),
                ),
                None => (false, true, true),
            };
            if !showing {
                return None;
            }

            let value = element_value(conn, &el, &role).await;
            let extents = component_extents(conn, &el).await.unwrap_or((0, 0, 0, 0));

            let mut children = Vec::new();
            if depth_left > 0 {
                if let Ok(child_refs) = el.get_children().await {
                    for child_ref in child_refs {
                        if budget.load(Ordering::SeqCst) == 0 {
                            truncated.store(true, Ordering::SeqCst);
                            break;
                        }
                        let Ok(child) = child_ref.into_accessible_proxy(conn.connection()).await
                        else {
                            continue;
                        };
                        if let Some(child_el) =
                            walk_element(conn, child, depth_left - 1, budget, truncated).await
                        {
                            children.push(child_el);
                        }
                    }
                }
            } else if el.child_count().await.unwrap_or(0) > 0 {
                truncated.store(true, Ordering::SeqCst);
            }

            Some(make_element(
                &role, name, value, extents, focused, enabled, children,
            ))
        })
    }

    /// Screen-space extents via the Component interface; `None` when the
    /// element doesn't implement it (pure structural nodes).
    async fn component_extents(
        conn: &AccessibilityConnection,
        el: &AccessibleProxy<'_>,
    ) -> Option<(i32, i32, i32, i32)> {
        let component = ComponentProxy::builder(conn.connection())
            .destination(el.inner().destination().to_owned())
            .ok()?
            .path(el.inner().path().to_owned())
            .ok()?
            .build()
            .await
            .ok()?;
        component.get_extents(CoordType::Screen).await.ok()
    }

    async fn element_value(
        conn: &AccessibilityConnection,
        el: &AccessibleProxy<'_>,
        role: &str,
    ) -> Option<String> {
        if role_is_password_text(role) {
            return None;
        }
        if let Some(text) = text_contents(conn, el).await {
            return Some(text);
        }
        if let Some(text) = value_text(conn, el).await {
            return Some(text);
        }
        numeric_value(conn, el).await
    }

    async fn text_contents(
        conn: &AccessibilityConnection,
        el: &AccessibleProxy<'_>,
    ) -> Option<String> {
        let text = TextProxy::builder(conn.connection())
            .destination(el.inner().destination().to_owned())
            .ok()?
            .path(el.inner().path().to_owned())
            .ok()?
            .build()
            .await
            .ok()?;
        let character_count = text.character_count().await.ok()?;
        if character_count <= 0 {
            return None;
        }
        text.get_text(0, character_count.min(TEXT_CAP as i32 + 1))
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    async fn value_text(
        conn: &AccessibilityConnection,
        el: &AccessibleProxy<'_>,
    ) -> Option<String> {
        let value = ValueProxy::builder(conn.connection())
            .destination(el.inner().destination().to_owned())
            .ok()?
            .path(el.inner().path().to_owned())
            .ok()?
            .build()
            .await
            .ok()?;
        value
            .text()
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    async fn numeric_value(
        conn: &AccessibilityConnection,
        el: &AccessibleProxy<'_>,
    ) -> Option<String> {
        let value = ValueProxy::builder(conn.connection())
            .destination(el.inner().destination().to_owned())
            .ok()?
            .path(el.inner().path().to_owned())
            .ok()?
            .build()
            .await
            .ok()?;
        let number = value.current_value().await.ok()?;
        if !number.is_finite() {
            return None;
        }
        let formatted = if number.fract() == 0.0 {
            format!("{number:.0}")
        } else {
            format!("{number:.3}")
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string()
        };
        Some(formatted).filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_map_to_shared_vocabulary() {
        // AT-SPI's native lowercase-with-spaces spelling.
        assert_eq!(role_to_shared("push button"), "button");
        assert_eq!(role_to_shared("page tab list"), "tabgroup");
        assert_eq!(role_to_shared("entry"), "textfield");
        assert_eq!(role_to_shared("frame"), "window");
        // PascalCase spellings normalize identically.
        assert_eq!(role_to_shared("PushButton"), "button");
        assert_eq!(role_to_shared("ScrollPane"), "group");
        // Structural roles land on the collapse vocabulary.
        assert_eq!(role_to_shared("filler"), "group");
        assert_eq!(role_to_shared("invalid"), "unknown");
        // Unmatched roles pass through normalized rather than degrading.
        assert_eq!(role_to_shared("notification"), "notification");
        assert_eq!(role_to_shared(""), "unknown");
        assert!(role_is_password_text("password text"));
        assert!(role_is_password_text("PasswordText"));
    }

    /// Live test — needs a session accessibility bus and a focused window.
    /// Run on the Linux boxes:
    /// `cargo test --bin intendant atspi_read -- --ignored --nocapture`
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore]
    async fn live_read_frontmost() {
        let elements = super::read_frontmost(12, 400).await.expect("AT-SPI read");
        println!("{}", crate::computer_use::format_screen_elements(&elements));
    }

    #[test]
    fn make_element_normalizes_fields() {
        let el = make_element(
            "push button",
            Some("Open".to_string()),
            None,
            (5, 10, 80, -3),
            false,
            true,
            Vec::new(),
        );
        assert_eq!(el.role, "button");
        assert_eq!(el.label.as_deref(), Some("Open"));
        assert_eq!(el.frame, (5, 10, 80, 0), "negative height clamps to zero");
        assert!(el.enabled);
    }

    #[test]
    fn make_element_cleans_value_text() {
        let el = make_element(
            "entry",
            Some(" Query ".to_string()),
            Some(" value ".to_string()),
            (0, 0, 10, 10),
            true,
            true,
            Vec::new(),
        );
        assert_eq!(el.role, "textfield");
        assert_eq!(el.label.as_deref(), Some("Query"));
        assert_eq!(el.value.as_deref(), Some("value"));
    }
}

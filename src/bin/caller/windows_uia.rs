//! Windows UI Automation element-tree reader (read-only observation).
//!
//! Maps UIA element properties (Name / ControlType / BoundingRectangle /
//! focus / enabled) into the portable [`ScreenElements`] shape used by
//! `read_screen`, honoring the same depth/node caps as the macOS AX reader
//! in `ax.rs`. Strictly read-only: no input injection, no background tasks —
//! one bounded walk per call.
//!
//! Layout follows the keymap precedent: the pure control-type/field mapping
//! stays ungated so its unit tests run on every host; only the actual UIA
//! (COM) walk is `#[cfg(windows)]`.

use crate::computer_use::{ScreenElements, UiElement};

/// Map a UIA `ControlType` ID to the lowercase role vocabulary shared with
/// the macOS AX reader (`format_screen_elements` keys its collapse rule on
/// `group`/`generic`/`unknown`). IDs from `UIAutomationClient.h`.
// Consumed by the cfg(windows) UIA walk; allow until that lands.
#[allow(dead_code)]
fn control_type_role(control_type_id: i32) -> &'static str {
    match control_type_id {
        50000 => "button",      // Button
        50001 => "calendar",    // Calendar
        50002 => "checkbox",    // CheckBox
        50003 => "combobox",    // ComboBox
        50004 => "textfield",   // Edit
        50005 => "link",        // Hyperlink
        50006 => "image",       // Image
        50007 => "listitem",    // ListItem
        50008 => "list",        // List
        50009 => "menu",        // Menu
        50010 => "menubar",     // MenuBar
        50011 => "menuitem",    // MenuItem
        50012 => "progressbar", // ProgressBar
        50013 => "radiobutton", // RadioButton
        50014 => "scrollbar",   // ScrollBar
        50015 => "slider",      // Slider
        50016 => "spinner",     // Spinner
        50017 => "statusbar",   // StatusBar
        50018 => "tabgroup",    // Tab (the container)
        50019 => "tab",         // TabItem
        50020 => "text",        // Text
        50021 => "toolbar",     // ToolBar
        50022 => "tooltip",     // ToolTip
        50023 => "tree",        // Tree
        50024 => "treeitem",    // TreeItem
        50025 => "generic",     // Custom
        50026 => "group",       // Group
        50027 => "thumb",       // Thumb
        50028 => "table",       // DataGrid
        50029 => "dataitem",    // DataItem
        50030 => "document",    // Document
        50031 => "button",      // SplitButton
        50032 => "window",      // Window
        50033 => "group",       // Pane (structural; lets unlabeled collapse)
        50034 => "header",      // Header
        50035 => "headeritem",  // HeaderItem
        50036 => "table",       // Table
        50037 => "titlebar",    // TitleBar
        50038 => "separator",   // Separator
        50039 => "group",       // SemanticZoom
        50040 => "toolbar",     // AppBar
        _ => "unknown",
    }
}

/// Assemble a [`UiElement`] from raw UIA-shaped fields.
///
/// `rect` is the UIA `BoundingRectangle` as (left, top, right, bottom) in
/// screen pixels; degenerate rectangles clamp to zero size (the formatter
/// drops zero-size childless leaves). Empty/whitespace names and values
/// normalize to `None` so the structural-collapse rule can fire.
// Consumed by the cfg(windows) UIA walk; allow until that lands.
#[allow(dead_code)]
fn map_element(
    control_type_id: i32,
    name: Option<String>,
    value: Option<String>,
    rect: (i32, i32, i32, i32),
    focused: bool,
    enabled: bool,
    children: Vec<UiElement>,
) -> UiElement {
    let (left, top, right, bottom) = rect;
    let width = (right.saturating_sub(left)).max(0) as u32;
    let height = (bottom.saturating_sub(top)).max(0) as u32;
    UiElement {
        role: control_type_role(control_type_id).to_string(),
        label: name.filter(|s| !s.trim().is_empty()),
        value: value.filter(|s| !s.trim().is_empty()),
        frame: (left, top, width, height),
        focused,
        enabled,
        children,
    }
}

/// Read the frontmost window's element tree via UI Automation.
///
/// Blocking (COM cross-process calls) — callers wrap it in `spawn_blocking`.
// The caller is the cfg(windows) arm of read_screen_elements.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn read_frontmost(max_depth: usize, max_nodes: usize) -> Result<ScreenElements, String> {
    let _ = (max_depth, max_nodes);
    Err(
        "element-tree observation via UI Automation is not implemented yet — \
         use take_screenshot instead"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_element_normalizes_fields() {
        let el = map_element(
            50000,
            Some("Save".to_string()),
            Some("   ".to_string()),
            (10, 20, 110, 60),
            true,
            true,
            Vec::new(),
        );
        assert_eq!(el.role, "button");
        assert_eq!(el.label.as_deref(), Some("Save"));
        assert_eq!(el.value, None, "whitespace value normalizes to None");
        assert_eq!(el.frame, (10, 20, 100, 40));
        assert!(el.focused);
        assert!(el.enabled);

        // Unknown control type + degenerate rect clamp.
        let el = map_element(1234, None, None, (50, 50, 40, 45), false, false, Vec::new());
        assert_eq!(el.role, "unknown");
        assert_eq!(el.label, None);
        assert_eq!(el.frame, (50, 50, 0, 0));

        // Pane maps into the structural-collapse vocabulary.
        assert_eq!(control_type_role(50033), "group");
    }
}

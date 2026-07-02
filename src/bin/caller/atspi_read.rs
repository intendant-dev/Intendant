//! Linux AT-SPI2 element-tree reader (read-only observation).
//!
//! Fills the portable [`ScreenElements`] shape from the session's
//! accessibility bus — works on both X11 and Wayland, since AT-SPI is a
//! session-level D-Bus service independent of the display server. Honors the
//! same depth/node caps as the macOS AX reader.
//!
//! Privacy posture (see the read_screen docs): UI metadata only, from the
//! operator's own session, one bounded walk per explicit call — no background
//! polling, no tree caches. Enabling `org.a11y.Status.IsEnabled` (the
//! platform's mechanism for making toolkits expose trees) happens on demand
//! at first use, never at daemon startup.
//!
//! Layout follows the keymap precedent: pure role/field mapping stays ungated
//! so its unit tests run on every host; the D-Bus walk is Linux-only (the
//! `atspi` crate is a Linux-target dependency).

use crate::computer_use::{ScreenElements, UiElement};

/// Map an AT-SPI role name to the lowercase role vocabulary shared with the
/// macOS AX and Windows UIA readers (`format_screen_elements` keys its
/// collapse rule on `group`/`generic`/`unknown`).
///
/// AT-SPI role names arrive as lowercase space-separated words
/// (`"push button"`, `"page tab list"`); normalization strips separators so
/// PascalCase spellings match too. Unmatched roles pass through normalized —
/// AT-SPI's vocabulary is already descriptive.
fn role_to_shared(atspi_role_name: &str) -> String {
    let normalized: String = atspi_role_name
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .collect::<String>()
        .to_lowercase();
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
        "panel" | "filler" | "section" | "scrollpane" | "viewport" | "splitpane"
        | "glasspane" | "rootpane" | "layeredpane" | "internalframe" | "desktopframe"
        | "form" | "grouping" | "embedded" | "canvas" => "group",
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
        "documentframe" | "documentweb" | "documenttext" | "documentemail"
        | "documentpresentation" | "documentspreadsheet" => "document",
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
        label: name.filter(|s| !s.trim().is_empty()),
        value: value.filter(|s| !s.trim().is_empty()),
        frame: (x, y, w.max(0) as u32, h.max(0) as u32),
        focused,
        enabled,
        children,
    }
}

/// Read the active window's element tree from the session accessibility bus.
// The caller is the cfg(linux) arm of read_screen_elements.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub async fn read_frontmost(max_depth: usize, max_nodes: usize) -> Result<ScreenElements, String> {
    let _ = (max_depth, max_nodes);
    Err(
        "element-tree observation via AT-SPI is not implemented yet — \
         use take_screenshot instead"
            .to_string(),
    )
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
}

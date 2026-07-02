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

#![cfg_attr(not(any(windows, test)), allow(dead_code))]

use crate::computer_use::{ScreenElements, UiElement};

/// Cap for label/value text carried per element.
const TEXT_CAP: usize = 80;
/// How many "other visible windows" summaries to include.
#[cfg(windows)]
const OTHER_WINDOWS_CAP: usize = 8;

/// Map a UIA `ControlType` ID to the lowercase role vocabulary shared with
/// the macOS AX reader (`format_screen_elements` keys its collapse rule on
/// `group`/`generic`/`unknown`). IDs from `UIAutomationClient.h`.
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

/// Assemble a [`UiElement`] from raw UIA-shaped fields.
///
/// `rect` is the UIA `BoundingRectangle` as (left, top, right, bottom) in
/// screen pixels; degenerate rectangles clamp to zero size (the formatter
/// drops zero-size childless leaves). Empty/whitespace names and values
/// normalize to `None` so the structural-collapse rule can fire.
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
        label: clean_text(name),
        value: clean_text(value),
        frame: (left, top, width, height),
        focused,
        enabled,
        children,
    }
}

#[cfg(windows)]
mod imp {
    use super::*;

    use windows::Win32::Foundation::{BOOL, HWND, RECT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTreeWalker,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};

    struct ComApartment;

    impl ComApartment {
        fn init() -> Result<Self, String> {
            // SAFETY: Initializes COM for this blocking worker thread; the
            // returned guard pairs it with CoUninitialize after all UIA COM
            // interface values declared later in read_frontmost have dropped.
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr.is_err() {
                return Err(format!("CoInitializeEx failed: {hr:?}"));
            }
            Ok(Self)
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            // SAFETY: Paired with the successful CoInitializeEx in init().
            unsafe { CoUninitialize() };
        }
    }

    pub fn read_frontmost(max_depth: usize, max_nodes: usize) -> Result<ScreenElements, String> {
        let _com = ComApartment::init()?;

        // SAFETY: CoCreateInstance is called after COM initialization on this
        // thread; CUIAutomation is the documented in-proc/local UIA client
        // class, and windows-rs owns/releases the returned interface.
        let automation: IUIAutomation = unsafe {
            CoCreateInstance(
                &CUIAutomation,
                None,
                CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER,
            )
        }
        .map_err(|e| format!("create UIAutomation client: {e}"))?;

        // SAFETY: GetForegroundWindow has no preconditions and returns null
        // when no foreground window exists.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            return Err("no foreground window found".to_string());
        }

        // SAFETY: hwnd was returned by GetForegroundWindow; UIA validates the
        // handle and returns an error if the window is not accessible.
        let root = unsafe { automation.ElementFromHandle(hwnd) }
            .map_err(|e| format!("UIA element from foreground window: {e}"))?;
        // SAFETY: ControlViewWalker is a read-only UIA helper object owned by
        // the automation client.
        let walker = unsafe { automation.ControlViewWalker() }
            .map_err(|e| format!("create UIA control-view walker: {e}"))?;

        let mut budget = max_nodes;
        let mut depth_capped = false;
        let root_element = walk(&root, &walker, 0, max_depth, &mut budget, &mut depth_capped);

        let pid = current_process_id(&root);
        let window_title = current_name(&root).or_else(|| window_text(hwnd));
        let app = window_title
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("pid {pid}"));
        let other_windows = other_windows(&automation, &root);

        let mut notes: Vec<String> = Vec::new();
        if budget == 0 {
            notes.push(format!(
                "element cap ({max_nodes}) reached — remaining content omitted"
            ));
        }
        if depth_capped {
            notes.push(format!(
                "depth cap ({max_depth}) reached — deeper nesting omitted"
            ));
        }

        Ok(ScreenElements {
            app,
            pid,
            window_title,
            root: Some(root_element),
            other_windows,
            truncated: if notes.is_empty() {
                None
            } else {
                Some(notes.join("; "))
            },
        })
    }

    fn walk(
        element: &IUIAutomationElement,
        walker: &IUIAutomationTreeWalker,
        depth: usize,
        max_depth: usize,
        budget: &mut usize,
        depth_capped: &mut bool,
    ) -> UiElement {
        *budget = budget.saturating_sub(1);

        let mut children = Vec::new();
        if depth + 1 > max_depth {
            *depth_capped = true;
        } else if *budget > 0 {
            children = child_elements(element, walker, depth + 1, max_depth, budget, depth_capped);
        }

        map_element(
            current_control_type(element),
            current_name(element),
            None,
            current_rect(element),
            current_focused(element),
            current_enabled(element),
            children,
        )
    }

    fn child_elements(
        element: &IUIAutomationElement,
        walker: &IUIAutomationTreeWalker,
        depth: usize,
        max_depth: usize,
        budget: &mut usize,
        depth_capped: &mut bool,
    ) -> Vec<UiElement> {
        let mut out = Vec::new();
        // SAFETY: UIA tree-walker calls are read-only COM calls against a live
        // element on the same initialized COM thread. Errors/null children are
        // treated as an empty child list.
        let mut child = unsafe { walker.GetFirstChildElement(element) }.ok();
        while let Some(current) = child {
            if *budget == 0 {
                break;
            }
            out.push(walk(
                &current,
                walker,
                depth,
                max_depth,
                budget,
                depth_capped,
            ));
            // SAFETY: Same as above; sibling traversal stays within the
            // control-view tree rooted at the foreground window.
            child = unsafe { walker.GetNextSiblingElement(&current) }.ok();
        }
        out
    }

    fn other_windows(automation: &IUIAutomation, front: &IUIAutomationElement) -> Vec<String> {
        // SAFETY: Read-only UIA root/control traversal. If any call fails,
        // the "other windows" summary is omitted rather than failing the main
        // foreground-window read.
        let root = match unsafe { automation.GetRootElement() } {
            Ok(root) => root,
            Err(_) => return Vec::new(),
        };
        // SAFETY: Read-only helper object from UIA.
        let walker = match unsafe { automation.ControlViewWalker() } {
            Ok(walker) => walker,
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::new();
        // SAFETY: Control-view traversal over the desktop root.
        let mut child = unsafe { walker.GetFirstChildElement(&root) }.ok();
        while let Some(current) = child {
            if out.len() >= OTHER_WINDOWS_CAP {
                break;
            }
            // SAFETY: CompareElements only compares identity of two UIA
            // element references.
            let is_front = unsafe { automation.CompareElements(front, &current) }
                .map(bool_from)
                .unwrap_or(false);
            if !is_front {
                let label = current_name(&current)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("pid {}", current_process_id(&current)));
                let rect = current_rect(&current);
                let (left, top, right, bottom) = rect;
                out.push(format!(
                    "{label} ({},{}, {}x{})",
                    left,
                    top,
                    (right.saturating_sub(left)).max(0),
                    (bottom.saturating_sub(top)).max(0)
                ));
            }
            // SAFETY: Read-only sibling traversal over the desktop root.
            child = unsafe { walker.GetNextSiblingElement(&current) }.ok();
        }
        out
    }

    fn current_name(element: &IUIAutomationElement) -> Option<String> {
        // SAFETY: Read-only UIA property getter. windows-rs owns the returned
        // BSTR and frees it on drop.
        unsafe { element.CurrentName() }
            .ok()
            .and_then(|s| String::try_from(s).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn current_process_id(element: &IUIAutomationElement) -> i32 {
        // SAFETY: Read-only UIA property getter.
        unsafe { element.CurrentProcessId() }.unwrap_or(0)
    }

    fn current_control_type(element: &IUIAutomationElement) -> i32 {
        // SAFETY: Read-only UIA property getter.
        unsafe { element.CurrentControlType() }
            .map(|control_type| control_type.0)
            .unwrap_or(0)
    }

    fn current_rect(element: &IUIAutomationElement) -> (i32, i32, i32, i32) {
        // SAFETY: Read-only UIA property getter returning a plain RECT.
        unsafe { element.CurrentBoundingRectangle() }
            .map(rect_tuple)
            .unwrap_or((0, 0, 0, 0))
    }

    fn current_focused(element: &IUIAutomationElement) -> bool {
        // SAFETY: Read-only UIA property getter.
        unsafe { element.CurrentHasKeyboardFocus() }
            .map(bool_from)
            .unwrap_or(false)
    }

    fn current_enabled(element: &IUIAutomationElement) -> bool {
        // SAFETY: Read-only UIA property getter.
        unsafe { element.CurrentIsEnabled() }
            .map(bool_from)
            .unwrap_or(true)
    }

    fn rect_tuple(rect: RECT) -> (i32, i32, i32, i32) {
        (rect.left, rect.top, rect.right, rect.bottom)
    }

    fn bool_from(value: BOOL) -> bool {
        value.as_bool()
    }

    fn window_text(hwnd: HWND) -> Option<String> {
        let mut buf = [0u16; 512];
        // SAFETY: hwnd came from GetForegroundWindow and buf is a valid,
        // writable UTF-16 buffer. GetWindowTextW writes at most buf.len()
        // code units including the terminator.
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        (len > 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Read the frontmost window's element tree via UI Automation.
///
/// Blocking (COM cross-process calls) — callers wrap it in `spawn_blocking`.
// The caller is the cfg(windows) arm of read_screen_elements.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn read_frontmost(max_depth: usize, max_nodes: usize) -> Result<ScreenElements, String> {
    #[cfg(windows)]
    {
        imp::read_frontmost(max_depth, max_nodes)
    }
    #[cfg(not(windows))]
    {
        let _ = (max_depth, max_nodes);
        Err(
            "element-tree observation via UI Automation is only available on Windows — \
             use take_screenshot instead"
                .to_string(),
        )
    }
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

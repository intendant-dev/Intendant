//! macOS Accessibility (AX) element-tree observation.
//!
//! Reads the frontmost application's focused-window UI tree through the
//! `AXUIElement` C API and normalizes it into the portable
//! [`crate::computer_use::UiElement`] shape. This is the textual grounding
//! layer for computer use: roles, labels, values, and logical-point frames
//! instead of pixels. Reading requires the same Accessibility (TCC)
//! permission that input injection already needs.
//!
//! ## Unsafe policy
//!
//! This module is the deliberate, documented exception to the repo's
//! no-`unsafe`-on-Unix-paths rule (see CLAUDE.md): the AX API has no safe
//! wrapper crate that doesn't drag in a duplicate `core-graphics`/legacy
//! `objc` stack, so raw `accessibility-sys` bindings are wrapped here.
//! Every `unsafe` block is as small as the FFI call it wraps, carries a
//! `// SAFETY:` comment, and object lifetimes are RAII-managed through
//! `core-foundation`'s `TCFType` wrappers (release-on-drop). Do not add
//! `unsafe` outside this module.

use std::ffi::c_void;

use accessibility_sys::{
    kAXChildrenAttribute, kAXDescriptionAttribute, kAXEnabledAttribute, kAXErrorSuccess,
    kAXFocusedAttribute, kAXFocusedWindowAttribute, kAXPositionAttribute, kAXRoleAttribute,
    kAXSizeAttribute, kAXTitleAttribute, kAXValueAttribute, kAXValueTypeCGPoint,
    kAXValueTypeCGSize, kAXWindowsAttribute, AXIsProcessTrusted, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementGetTypeID, AXUIElementRef,
    AXUIElementSetMessagingTimeout, AXValueGetTypeID, AXValueGetValue, AXValueRef,
};
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFGetTypeID, CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation::{declare_TCFType, impl_TCFType};
use core_graphics::geometry::{CGPoint, CGSize};
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionOnScreenOnly,
};

use crate::computer_use::{ScreenElements, UiElement};

declare_TCFType!(AXUIElement, AXUIElementRef);
impl_TCFType!(AXUIElement, AXUIElementRef, AXUIElementGetTypeID);

declare_TCFType!(AXValue, AXValueRef);
impl_TCFType!(AXValue, AXValueRef, AXValueGetTypeID);

/// Cap for label/value text carried per element.
const TEXT_CAP: usize = 80;
/// How many "other visible windows" summaries to include.
const OTHER_WINDOWS_CAP: usize = 8;
/// Per-attribute IPC timeout so one unresponsive app cannot hang the read.
const MESSAGING_TIMEOUT_SECS: f32 = 1.0;

/// Whether this process holds the Accessibility (TCC) permission.
pub fn is_trusted() -> bool {
    // SAFETY: argument-less query with no preconditions.
    unsafe { AXIsProcessTrusted() }
}

/// Read the frontmost application's focused-window element tree.
pub fn read_frontmost(max_depth: usize, max_nodes: usize) -> Result<ScreenElements, String> {
    if !is_trusted() {
        return Err(
            "reading UI elements requires the Accessibility permission — grant Intendant \
             access in System Settings → Privacy & Security → Accessibility and retry"
                .to_string(),
        );
    }

    let windows = on_screen_windows();
    let front = windows
        .first()
        .ok_or_else(|| "no on-screen application windows found".to_string())?;
    let pid = front.pid;
    let app = front.owner.clone();
    let other_windows = windows
        .iter()
        .skip(1)
        .take(OTHER_WINDOWS_CAP)
        .map(WindowInfo::summary)
        .collect::<Vec<_>>();

    // SAFETY: AXUIElementCreateApplication follows the Create rule and accepts
    // any pid; the wrapper takes ownership and releases on drop.
    let app_element =
        unsafe { AXUIElement::wrap_under_create_rule(AXUIElementCreateApplication(pid)) };
    // SAFETY: app_element is a valid AXUIElement for the duration of the call.
    unsafe {
        AXUIElementSetMessagingTimeout(app_element.as_concrete_TypeRef(), MESSAGING_TIMEOUT_SECS)
    };

    let window = focused_window(&app_element);
    let window_title = window
        .as_ref()
        .and_then(|w| attr_string(w, kAXTitleAttribute));

    let mut budget = max_nodes;
    let mut depth_capped = false;
    let root = window
        .as_ref()
        .map(|w| walk(w, 0, max_depth, &mut budget, &mut depth_capped));

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
        root,
        other_windows,
        truncated: if notes.is_empty() {
            None
        } else {
            Some(notes.join("; "))
        },
    })
}

/// The frontmost app's focused window, falling back to its first window.
fn focused_window(app_element: &AXUIElement) -> Option<AXUIElement> {
    if let Some(window) =
        copy_attr(app_element, kAXFocusedWindowAttribute).and_then(|cf| cf.downcast_into())
    {
        return Some(window);
    }
    let windows = copy_attr(app_element, kAXWindowsAttribute)?;
    cf_as_element_array(&windows)?.into_iter().next()
}

/// Convert a CFType known to hold a CFArray of AXUIElements into owned
/// wrappers. Items whose dynamic type is not AXUIElement are skipped.
fn cf_as_element_array(cf: &CFType) -> Option<Vec<AXUIElement>> {
    if !cf.instance_of::<CFArray>() {
        return None;
    }
    // SAFETY: dynamic type verified as CFArray above; the get-rule wrap
    // retains, giving `array` independent ownership.
    let array: CFArray = unsafe { CFArray::wrap_under_get_rule(cf.as_CFTypeRef() as CFArrayRef) };
    let mut elements = Vec::with_capacity(array.len() as usize);
    for item in array.iter() {
        let ptr = *item;
        if ptr.is_null() {
            continue;
        }
        // SAFETY: CFGetTypeID accepts any live CF object (the array retains
        // its items for the iteration); the wrap only happens when the
        // dynamic type matches AXUIElement, and wrap_under_get_rule retains
        // for independent ownership.
        unsafe {
            if CFGetTypeID(ptr) == AXUIElementGetTypeID() {
                elements.push(AXUIElement::wrap_under_get_rule(ptr as AXUIElementRef));
            }
        }
    }
    Some(elements)
}

/// Type-checked view of a CFType as a string-keyed dictionary (the shape of
/// CGWindowList entries and their `kCGWindowBounds` values).
fn cf_as_string_dict(cf: &CFType) -> Option<CFDictionary<CFString, CFType>> {
    if !cf.instance_of::<CFDictionary>() {
        return None;
    }
    // SAFETY: dynamic type verified as CFDictionary above; the K/V type
    // params are a reading convention for these documented string-keyed
    // dictionaries, and the get-rule wrap retains for independent ownership.
    Some(unsafe { CFDictionary::wrap_under_get_rule(cf.as_CFTypeRef() as CFDictionaryRef) })
}

/// Depth-first walk normalizing AX attributes into `UiElement`s.
fn walk(
    element: &AXUIElement,
    depth: usize,
    max_depth: usize,
    budget: &mut usize,
    depth_capped: &mut bool,
) -> UiElement {
    *budget = budget.saturating_sub(1);

    let role = attr_string(element, kAXRoleAttribute)
        .map(|r| normalize_role(&r))
        .unwrap_or_else(|| "unknown".to_string());
    let label = attr_string(element, kAXTitleAttribute)
        .filter(|s| !s.is_empty())
        .or_else(|| attr_string(element, kAXDescriptionAttribute).filter(|s| !s.is_empty()))
        .map(|s| truncate(&s, TEXT_CAP));
    let value = attr_value_string(element).map(|s| truncate(&s, TEXT_CAP));
    let focused = attr_bool(element, kAXFocusedAttribute).unwrap_or(false);
    let enabled = attr_bool(element, kAXEnabledAttribute).unwrap_or(true);
    let frame = element_frame(element).unwrap_or((0, 0, 0, 0));

    let mut children = Vec::new();
    if depth + 1 > max_depth {
        *depth_capped = true;
    } else if *budget > 0 {
        if let Some(child_elements) = child_elements(element) {
            for child in child_elements {
                if *budget == 0 {
                    break;
                }
                children.push(walk(&child, depth + 1, max_depth, budget, depth_capped));
            }
        }
    }

    UiElement {
        role,
        label,
        value,
        frame,
        focused,
        enabled,
        children,
    }
}

/// `AXButton` → `button`; unknown shapes are lowercased as-is.
fn normalize_role(role: &str) -> String {
    role.strip_prefix("AX").unwrap_or(role).to_ascii_lowercase()
}

fn truncate(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let cut: String = text.chars().take(cap).collect();
    format!("{cut}…")
}

/// Copy one AX attribute, taking ownership of the returned object.
fn copy_attr(element: &AXUIElement, attribute: &str) -> Option<CFType> {
    let key = CFString::new(attribute);
    let mut out: *const c_void = std::ptr::null();
    // SAFETY: element and key are valid CF objects for the duration of the
    // call; `out` is written only on success and then owned per the Copy rule
    // by the wrapping CFType, which releases on drop.
    let err = unsafe {
        AXUIElementCopyAttributeValue(
            element.as_concrete_TypeRef(),
            key.as_concrete_TypeRef(),
            &mut out,
        )
    };
    if err != kAXErrorSuccess || out.is_null() {
        return None;
    }
    // SAFETY: non-null result from a successful Copy-rule call.
    Some(unsafe { CFType::wrap_under_create_rule(out) })
}

fn attr_string(element: &AXUIElement, attribute: &str) -> Option<String> {
    copy_attr(element, attribute)
        .and_then(|cf| cf.downcast_into::<CFString>())
        .map(|s| s.to_string())
}

fn attr_bool(element: &AXUIElement, attribute: &str) -> Option<bool> {
    copy_attr(element, attribute)
        .and_then(|cf| cf.downcast_into::<CFBoolean>())
        .map(Into::into)
}

/// Stringify `AXValue` payloads worth showing (text, numbers, booleans).
fn attr_value_string(element: &AXUIElement) -> Option<String> {
    let cf = copy_attr(element, kAXValueAttribute)?;
    if let Some(s) = cf.downcast::<CFString>() {
        let s = s.to_string();
        return (!s.is_empty()).then_some(s);
    }
    if let Some(n) = cf.downcast::<CFNumber>() {
        if let Some(i) = n.to_i64() {
            return Some(i.to_string());
        }
        if let Some(f) = n.to_f64() {
            return Some(format!("{f:.2}"));
        }
    }
    if let Some(b) = cf.downcast::<CFBoolean>() {
        return Some(bool::from(b).to_string());
    }
    None
}

fn child_elements(element: &AXUIElement) -> Option<Vec<AXUIElement>> {
    cf_as_element_array(&copy_attr(element, kAXChildrenAttribute)?)
}

/// Element frame in logical points, from the AXPosition/AXSize `AXValue`s.
fn element_frame(element: &AXUIElement) -> Option<(i32, i32, u32, u32)> {
    let position: AXValue = copy_attr(element, kAXPositionAttribute)?.downcast_into()?;
    let size: AXValue = copy_attr(element, kAXSizeAttribute)?.downcast_into()?;

    let mut point = CGPoint::new(0.0, 0.0);
    // SAFETY: valuePtr points at a CGPoint and kAXValueTypeCGPoint requests
    // exactly that layout; AXValueGetValue writes it only when returning true.
    let ok = unsafe {
        AXValueGetValue(
            position.as_concrete_TypeRef(),
            kAXValueTypeCGPoint,
            &mut point as *mut CGPoint as *mut c_void,
        )
    };
    if !ok {
        return None;
    }
    let mut cg_size = CGSize::new(0.0, 0.0);
    // SAFETY: valuePtr points at a CGSize and kAXValueTypeCGSize requests
    // exactly that layout; AXValueGetValue writes it only when returning true.
    let ok = unsafe {
        AXValueGetValue(
            size.as_concrete_TypeRef(),
            kAXValueTypeCGSize,
            &mut cg_size as *mut CGSize as *mut c_void,
        )
    };
    if !ok {
        return None;
    }
    Some((
        point.x.round() as i32,
        point.y.round() as i32,
        cg_size.width.max(0.0).round() as u32,
        cg_size.height.max(0.0).round() as u32,
    ))
}

// ── Window enumeration (CGWindowList) ────────────────────────────────────────

struct WindowInfo {
    pid: i32,
    owner: String,
    title: Option<String>,
    bounds: Option<(i32, i32, u32, u32)>,
}

impl WindowInfo {
    fn summary(&self) -> String {
        let title = self
            .title
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(|t| format!(" — \"{t}\""))
            .unwrap_or_default();
        let bounds = self
            .bounds
            .map(|(x, y, w, h)| format!(" ({x},{y} {w}x{h})"))
            .unwrap_or_default();
        format!("{}{}{}", self.owner, title, bounds)
    }
}

/// On-screen, layer-0 (normal) windows, front-to-back.
fn on_screen_windows() -> Vec<WindowInfo> {
    let Some(list) = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
        return Vec::new();
    };

    let mut windows = Vec::new();
    for item in list.iter() {
        let ptr = *item;
        if ptr.is_null() {
            continue;
        }
        // SAFETY: the array retains its items for the iteration; the get-rule
        // wrap retains again for independent ownership. The dictionary view is
        // type-checked by cf_as_string_dict below.
        let cf = unsafe { CFType::wrap_under_get_rule(ptr) };
        let Some(dict) = cf_as_string_dict(&cf) else {
            continue;
        };
        let layer = dict_i64(&dict, "kCGWindowLayer").unwrap_or(-1);
        if layer != 0 {
            continue;
        }
        let Some(pid) = dict_i64(&dict, "kCGWindowOwnerPID") else {
            continue;
        };
        let owner = dict_string(&dict, "kCGWindowOwnerName").unwrap_or_else(|| "unknown".into());
        let title = dict_string(&dict, "kCGWindowName");
        let bounds = dict
            .find(CFString::new("kCGWindowBounds"))
            .and_then(|cf| cf_as_string_dict(&cf))
            .and_then(|b| {
                Some((
                    dict_i64(&b, "X")? as i32,
                    dict_i64(&b, "Y")? as i32,
                    dict_i64(&b, "Width")?.max(0) as u32,
                    dict_i64(&b, "Height")?.max(0) as u32,
                ))
            });
        windows.push(WindowInfo {
            pid: pid as i32,
            owner,
            title,
            bounds,
        });
    }
    windows
}

fn dict_string(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
    dict.find(CFString::new(key))
        .and_then(|cf| cf.downcast::<CFString>())
        .map(|s| s.to_string())
}

fn dict_i64(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<i64> {
    dict.find(CFString::new(key))
        .and_then(|cf| cf.downcast::<CFNumber>())
        .and_then(|n| n.to_i64().or_else(|| n.to_f64().map(|f| f.round() as i64)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live probe against the real GUI session. Requires the Accessibility
    /// permission for the invoking process tree; both outcomes are printed.
    /// Run manually:
    /// `cargo test --bin intendant -- ax::tests::live_read_frontmost --ignored --nocapture`
    #[test]
    #[ignore = "requires a GUI session and the Accessibility (TCC) permission"]
    fn live_read_frontmost() {
        match read_frontmost(
            crate::computer_use::ELEMENT_TREE_MAX_DEPTH,
            crate::computer_use::ELEMENT_TREE_MAX_NODES,
        ) {
            Ok(snapshot) => {
                let text = crate::computer_use::format_screen_elements(&snapshot);
                println!("{text}");
                assert!(!snapshot.app.is_empty());
            }
            Err(e) => println!("read_frontmost error (expected without TCC): {e}"),
        }
    }
}

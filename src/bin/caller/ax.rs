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
//! This module is the deliberate, documented AX exception to the repo's
//! default safe-Rust Unix policy (see CLAUDE.md): the AX API has no safe
//! wrapper crate that doesn't drag in a duplicate `core-graphics`/legacy
//! `objc` stack, so raw `accessibility-sys` bindings are wrapped here.
//! Every `unsafe` block is as small as the FFI call it wraps, carries a
//! `// SAFETY:` comment, and object lifetimes are RAII-managed through
//! `core-foundation`'s `TCFType` wrappers (release-on-drop). Do not add AX
//! `unsafe` outside this module; other Unix `unsafe` sites are limited to the
//! documented platform probes/signals in `platform.rs` and the Vortex shared
//! memory bridge in `live_audio.rs`.

use std::ffi::c_void;

use accessibility_sys::{
    kAXChildrenAttribute, kAXDescriptionAttribute, kAXEnabledAttribute,
    kAXErrorAttributeUnsupported, kAXErrorNoValue, kAXErrorSuccess, kAXFocusedAttribute,
    kAXFocusedUIElementAttribute, kAXFocusedWindowAttribute, kAXPositionAttribute,
    kAXRoleAttribute, kAXSizeAttribute, kAXTitleAttribute, kAXValueAttribute, kAXValueTypeCGPoint,
    kAXValueTypeCGSize, kAXWindowsAttribute, AXIsProcessTrusted, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementCreateSystemWide, AXUIElementGetTypeID,
    AXUIElementRef, AXUIElementSetMessagingTimeout, AXValueGetTypeID, AXValueGetValue, AXValueRef,
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

/// The focused element's role and readable value, for bounded `type`
/// read-back verification.
pub struct FocusedElementText {
    /// Normalized role (`AXTextField` → `textfield`), when readable.
    pub role: Option<String>,
    /// The element's `AXValue` rendered as text — `None` when the value is
    /// missing or not a string/number/bool.
    pub value: Option<String>,
}

/// Read the system-wide focused UI element's role and value.
///
/// Bounded like every AX read in this module (per-element messaging
/// timeout, two attribute copies). The attribute copies are synchronous IPC
/// into the focused app — call from `spawn_blocking`.
pub fn focused_element_text() -> Result<FocusedElementText, String> {
    if !is_trusted() {
        return Err(
            "reading UI elements requires the Accessibility permission — grant Intendant \
             access in System Settings → Privacy & Security → Accessibility and retry"
                .to_string(),
        );
    }
    // SAFETY: AXUIElementCreateSystemWide follows the Create rule and has no
    // preconditions; the wrapper takes ownership and releases on drop.
    let system = unsafe { AXUIElement::wrap_under_create_rule(AXUIElementCreateSystemWide()) };
    // SAFETY: system is a valid AXUIElement for the duration of the call.
    unsafe { AXUIElementSetMessagingTimeout(system.as_concrete_TypeRef(), MESSAGING_TIMEOUT_SECS) };
    let focused: AXUIElement = copy_attr(&system, kAXFocusedUIElementAttribute)
        .and_then(|cf| cf.downcast_into())
        .ok_or_else(|| "no focused UI element".to_string())?;
    // SAFETY: focused is a valid AXUIElement for the duration of the call.
    unsafe {
        AXUIElementSetMessagingTimeout(focused.as_concrete_TypeRef(), MESSAGING_TIMEOUT_SECS)
    };
    Ok(FocusedElementText {
        role: attr_string(&focused, kAXRoleAttribute).map(|r| normalize_role(&r)),
        value: attr_value_string(&focused),
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
    // Labels/values are carried in full here; the display cap is applied
    // once, centrally, by computer_use::cap_screen_elements_texts (so the
    // read_screen `full_values` opt-out can serve the uncapped text).
    let label = attr_string(element, kAXTitleAttribute)
        .filter(|s| !s.is_empty())
        .or_else(|| attr_string(element, kAXDescriptionAttribute).filter(|s| !s.is_empty()));
    let value = attr_value_string(element);
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

// Explicit owned-monitor placement. These wrappers are original, narrowly
// scoped AX/CG observation and position/size setters; no raw input path.
use crate::macos_monitor::placement::{
    self, Bounds, ListedWindow, Native, Observation, WindowIdentity,
};
use std::time::Instant;

pub(crate) struct PlacementNative;
pub(crate) struct RetainedWindow {
    element: AXUIElement,
    identity: WindowIdentity,
    // Make thread confinement explicit independently of the CF wrapper traits.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
#[derive(PartialEq)]
pub(crate) struct PlacementFocus {
    element: AXUIElement,
    pid: i32,
    birth: (u64, u32),
}

fn placement_permissions(deadline: Instant) -> Result<(), String> {
    placement::time_left(deadline)?;
    if !is_trusted() || !core_graphics::access::ScreenCaptureAccess.preflight() {
        return Err("window binding/placement requires existing Accessibility and Screen Recording permissions; no permission prompt requested".into());
    }
    Ok(())
}
fn placement_timeout(element: &AXUIElement, deadline: Instant) -> Result<(), String> {
    placement::time_left(deadline)?;
    // SAFETY: live retained object, positive bounded per-IPC timeout. No default
    // system-wide timeout is changed, and no AX object crosses a thread.
    let status = unsafe { AXUIElementSetMessagingTimeout(element.as_concrete_TypeRef(), 0.05) };
    if status != kAXErrorSuccess {
        return Err("cannot bound AX messaging timeout".into());
    }
    Ok(())
}
fn placement_app(pid: i32, deadline: Instant) -> Result<AXUIElement, String> {
    placement_permissions(deadline)?;
    if pid <= 0 {
        return Err("invalid PID".into());
    }
    // SAFETY: Create-rule API accepts positive PID. Null checked before wrapping.
    let raw = unsafe { AXUIElementCreateApplication(pid) };
    if raw.is_null() {
        return Err("AX application unavailable".into());
    }
    // SAFETY: non-null Create-rule object, released by wrapper.
    let app = unsafe { AXUIElement::wrap_under_create_rule(raw) };
    placement_timeout(&app, deadline)?;
    Ok(app)
}
fn placement_window_id(element: &AXUIElement, deadline: Instant) -> Result<u32, String> {
    placement_timeout(element, deadline)?;
    type GetWindow = unsafe extern "C" fn(AXUIElementRef, *mut u32) -> i32;
    // SAFETY: constant C symbol; ApplicationServices remains linked for lifetime
    // of the helper. Missing SPI is a refusal, never a title/order heuristic.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"_AXUIElementGetWindow".as_ptr()) };
    if symbol.is_null() {
        return Err("exact AX window mapping SPI unavailable".into());
    }
    // SAFETY: known SPI ABI AXError(AXUIElementRef, CGWindowID*).
    let get: GetWindow = unsafe { std::mem::transmute(symbol) };
    let mut id = 0;
    // SAFETY: retained live element and writable u32 output.
    let status = unsafe { get(element.as_concrete_TypeRef(), &mut id) };
    if status != kAXErrorSuccess || id == 0 {
        return Err("cannot map AX window exactly".into());
    }
    Ok(id)
}
fn placement_pid(element: &AXUIElement, deadline: Instant) -> Result<i32, String> {
    placement_timeout(element, deadline)?;
    let mut pid = 0;
    // SAFETY: retained AX object and writable pid_t output.
    let status =
        unsafe { accessibility_sys::AXUIElementGetPid(element.as_concrete_TypeRef(), &mut pid) };
    if status != kAXErrorSuccess || pid <= 0 {
        return Err("AX process identity unavailable".into());
    }
    Ok(pid)
}
fn placement_generation(identity: WindowIdentity) -> Result<(), String> {
    identity.validate()?;
    if crate::platform::macos_process_birth(identity.pid)
        != Some((identity.start_seconds, identity.start_micros))
    {
        return Err("window process exited or PID generation changed".into());
    }
    Ok(())
}
fn placement_roots(pid: i32, deadline: Instant) -> Result<Vec<AXUIElement>, String> {
    let app = placement_app(pid, deadline)?;
    let key = CFString::new(kAXWindowsAttribute);
    let mut raw = std::ptr::null();
    // SAFETY: retained app/key and writable array output; request at most cap+1
    // objects, so an app with too many windows fails without an unbounded copy.
    let status = unsafe {
        accessibility_sys::AXUIElementCopyAttributeValues(
            app.as_concrete_TypeRef(),
            key.as_concrete_TypeRef(),
            0,
            (placement::MAX_CANDIDATES + 1) as _,
            &mut raw,
        )
    };
    if raw.is_null() {
        return Err("application does not expose bounded AX windows".into());
    }
    // SAFETY: Copy-rule output is retained even if a malformed provider also
    // returned an error. Wrapper releases it on all following paths.
    let array: CFArray = unsafe { CFArray::wrap_under_create_rule(raw) };
    if status != kAXErrorSuccess || array.len() as usize > placement::MAX_CANDIDATES {
        return Err("AX window enumeration unavailable or exceeds capacity".into());
    }
    let mut windows = Vec::with_capacity(array.len() as usize);
    for item in array.iter() {
        placement::time_left(deadline)?;
        let ptr = *item;
        if ptr.is_null() {
            return Err("null AX window".into());
        }
        // SAFETY: array retains each live CF item throughout the loop.
        if unsafe { CFGetTypeID(ptr) } != AXUIElement::type_id() {
            return Err("invalid AX window type".into());
        }
        // SAFETY: dynamic AX type checked, get-rule wrapper retains independently.
        let window = unsafe { AXUIElement::wrap_under_get_rule(ptr as AXUIElementRef) };
        placement_timeout(&window, deadline)?;
        windows.push(window);
    }
    Ok(windows)
}
fn placement_exact(identity: WindowIdentity, deadline: Instant) -> Result<AXUIElement, String> {
    placement_generation(identity)?;
    let mut found = None;
    for window in placement_roots(identity.pid, deadline)? {
        if placement_window_id(&window, deadline)? == identity.window_id {
            if found.is_some() {
                return Err("ambiguous AX window mapping".into());
            }
            found = Some(window);
        }
    }
    placement_generation(identity)?;
    found.ok_or_else(|| "exact window is destroyed or unavailable".into())
}
fn placement_cg(identity: WindowIdentity, deadline: Instant) -> Result<Bounds, String> {
    placement_permissions(deadline)?;
    placement_generation(identity)?;
    let list = copy_window_info(
        core_graphics::window::kCGWindowListOptionIncludingWindow,
        identity.window_id,
    )
    .ok_or("CG window readback unavailable")?;
    if list.len() != 1 {
        return Err("CG window absent or ambiguous".into());
    }
    let ptr = *list.get(0).ok_or("CG window missing")?;
    if ptr.is_null() {
        return Err("CG window missing".into());
    }
    // SAFETY: list retains the CF item, get-rule wrapper retains independently.
    let cf = unsafe { CFType::wrap_under_get_rule(ptr) };
    let dict = cf_as_string_dict(&cf).ok_or("CG window metadata invalid")?;
    if dict_i64(&dict, "kCGWindowNumber") != Some(i64::from(identity.window_id))
        || dict_i64(&dict, "kCGWindowOwnerPID") != Some(i64::from(identity.pid))
        || dict_i64(&dict, "kCGWindowLayer") != Some(0)
        || !dict
            .find(CFString::new("kCGWindowIsOnscreen"))
            .and_then(|v| v.downcast::<CFBoolean>())
            .is_some_and(bool::from)
    {
        return Err("CG window identity/visibility changed".into());
    }
    let b = dict
        .find(CFString::new("kCGWindowBounds"))
        .and_then(|v| cf_as_string_dict(&v))
        .ok_or("CG bounds unavailable")?;
    let number = |key: &str| {
        b.find(CFString::new(key))
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_f64())
            .ok_or_else(|| "CG bounds invalid".to_string())
    };
    let bounds = Bounds {
        x: number("X")?,
        y: number("Y")?,
        width: number("Width")?,
        height: number("Height")?,
    };
    bounds.validate()?;
    placement_generation(identity)?;
    Ok(bounds)
}
fn placement_ax(element: &AXUIElement, deadline: Instant) -> Result<Bounds, String> {
    placement_timeout(element, deadline)?;
    let position: AXValue = copy_attr(element, kAXPositionAttribute)
        .and_then(|v| v.downcast_into())
        .ok_or("AX position unavailable")?;
    placement::time_left(deadline)?;
    let size: AXValue = copy_attr(element, kAXSizeAttribute)
        .and_then(|v| v.downcast_into())
        .ok_or("AX size unavailable")?;
    let mut point = CGPoint::new(0.0, 0.0);
    let mut size_out = CGSize::new(0.0, 0.0);
    // SAFETY: retained AXValue and writable CGPoint matching requested type.
    let p = unsafe {
        AXValueGetValue(
            position.as_concrete_TypeRef(),
            kAXValueTypeCGPoint,
            (&mut point as *mut CGPoint).cast(),
        )
    };
    // SAFETY: retained AXValue and writable CGSize matching requested type.
    let s = unsafe {
        AXValueGetValue(
            size.as_concrete_TypeRef(),
            kAXValueTypeCGSize,
            (&mut size_out as *mut CGSize).cast(),
        )
    };
    if !p || !s {
        return Err("AX geometry type mismatch".into());
    }
    let bounds = Bounds {
        x: point.x,
        y: point.y,
        width: size_out.width,
        height: size_out.height,
    };
    bounds.validate()?;
    Ok(bounds)
}
fn placement_settable(
    window: &AXUIElement,
    attribute: &str,
    deadline: Instant,
) -> Result<(), String> {
    placement_timeout(window, deadline)?;
    let key = CFString::new(attribute);
    let mut settable = 0;
    // SAFETY: retained window/key and writable C Boolean output.
    let status = unsafe {
        accessibility_sys::AXUIElementIsAttributeSettable(
            window.as_concrete_TypeRef(),
            key.as_concrete_TypeRef(),
            &mut settable,
        )
    };
    if status != kAXErrorSuccess || settable == 0 {
        return Err(format!("{attribute} is not settable"));
    }
    Ok(())
}
fn placement_set(
    retained: &RetainedWindow,
    target: Bounds,
    size: bool,
    deadline: Instant,
) -> Result<(), String> {
    placement_permissions(deadline)?;
    let window = &retained.element;
    placement_timeout(window, deadline)?;
    let attribute = if size {
        kAXSizeAttribute
    } else {
        kAXPositionAttribute
    };
    placement_settable(window, attribute, deadline)?;
    let point = CGPoint::new(target.x, target.y);
    let dimensions = CGSize::new(target.width, target.height);
    let (kind, ptr) = if size {
        (kAXValueTypeCGSize, (&dimensions as *const CGSize).cast())
    } else {
        (kAXValueTypeCGPoint, (&point as *const CGPoint).cast())
    };
    // SAFETY: ptr references the stack struct corresponding to kind; AXValueCreate copies it.
    let raw = unsafe { accessibility_sys::AXValueCreate(kind, ptr) };
    if raw.is_null() {
        return Err("AX geometry value allocation failed".into());
    }
    // SAFETY: non-null Create-rule AXValue, released on all exits.
    let value = unsafe { AXValue::wrap_under_create_rule(raw) };
    let key = CFString::new(attribute);
    placement::time_left(deadline)?;
    placement_generation(retained.identity)?;
    if placement_window_id(window, deadline)? != retained.identity.window_id
        || placement_pid(window, deadline)? != retained.identity.pid
    {
        return Err("retained window identity changed immediately before write".into());
    }
    // SAFETY: retained exact AX window, key and correctly typed AXValue are live.
    // These are the only mutation verbs in the placement implementation.
    let status = unsafe {
        accessibility_sys::AXUIElementSetAttributeValue(
            window.as_concrete_TypeRef(),
            key.as_concrete_TypeRef(),
            value.as_CFTypeRef(),
        )
    };
    if status != kAXErrorSuccess {
        return Err(format!(
            "AX {attribute} write refused or unconfirmed ({status})"
        ));
    }
    Ok(())
}
impl Native for PlacementNative {
    type Window = RetainedWindow;
    type Focus = PlacementFocus;
    fn candidates(
        &mut self,
        pid: i32,
        deadline: Instant,
    ) -> Result<Vec<ListedWindow<RetainedWindow>>, String> {
        placement_permissions(deadline)?;
        let (start_seconds, start_micros) =
            crate::platform::macos_process_birth(pid).ok_or("process generation unavailable")?;
        let mut candidates: Vec<ListedWindow<RetainedWindow>> = Vec::new();
        for element in placement_roots(pid, deadline)? {
            let identity = WindowIdentity {
                pid,
                start_seconds,
                start_micros,
                window_id: placement_window_id(&element, deadline)?,
            };
            if candidates.iter().any(|c| c.identity == identity) {
                return Err("ambiguous AX window mapping".into());
            }
            let bounds = placement_cg(identity, deadline)?;
            if !placement_ax(&element, deadline)?.close(bounds) {
                return Err("candidate AX/CG geometry mismatch".into());
            }
            candidates.push(ListedWindow {
                identity,
                bounds,
                window: RetainedWindow {
                    element,
                    identity,
                    _thread: std::marker::PhantomData,
                },
            });
        }
        placement::time_left(deadline)?;
        if crate::platform::macos_process_birth(pid) != Some((start_seconds, start_micros)) {
            return Err("PID generation changed while listing".into());
        }
        Ok(candidates)
    }
    fn observe(
        &mut self,
        window: &RetainedWindow,
        identity: WindowIdentity,
        deadline: Instant,
    ) -> Result<Observation, String> {
        let exact = placement_exact(identity, deadline)?;
        // CFEqual tests remote AX object identity, not CGWindowID, title or frame.
        if exact != window.element
            || placement_pid(&window.element, deadline)? != identity.pid
            || placement_window_id(&window.element, deadline)? != identity.window_id
        {
            return Err("retained AX window destroyed/replaced; refusing reused CGWindowID".into());
        }
        placement_timeout(&window.element, deadline)?;
        let minimized = attr_bool(&window.element, "AXMinimized");
        placement::time_left(deadline)?;
        let fullscreen = attr_bool(&window.element, "AXFullScreen");
        if minimized != Some(false) || fullscreen != Some(false) {
            return Err("window minimized/fullscreen state unavailable or unsupported".into());
        }
        // Observation does not require writable geometry. Each actual setter
        // checks its own capability immediately before mutation.
        let ax = placement_ax(&window.element, deadline)?;
        let cg = placement_cg(identity, deadline)?;
        placement_generation(identity)?;
        placement::time_left(deadline)?;
        Ok(Observation { ax, cg })
    }
    fn focus(&mut self, deadline: Instant) -> Result<PlacementFocus, String> {
        placement_permissions(deadline)?;
        // SAFETY: argument-free Create-rule API; null checked before wrapping.
        let raw = unsafe { AXUIElementCreateSystemWide() };
        if raw.is_null() {
            return Err("focus observation unavailable".into());
        }
        // SAFETY: non-null Create-rule result released on drop.
        let system = unsafe { AXUIElement::wrap_under_create_rule(raw) };
        placement_timeout(&system, deadline)?;
        let element: AXUIElement = copy_attr(&system, kAXFocusedUIElementAttribute)
            .and_then(|v| v.downcast_into())
            .ok_or("focus observation unavailable; placement refused")?;
        placement_timeout(&element, deadline)?;
        let pid = placement_pid(&element, deadline)?;
        let birth = crate::platform::macos_process_birth(pid)
            .ok_or("focus process identity unavailable")?;
        Ok(PlacementFocus {
            element,
            pid,
            birth,
        })
    }
    fn position(
        &mut self,
        window: &RetainedWindow,
        target: Bounds,
        deadline: Instant,
    ) -> Result<(), String> {
        placement_set(window, target, false, deadline)
    }
    fn size(
        &mut self,
        window: &RetainedWindow,
        target: Bounds,
        deadline: Instant,
    ) -> Result<(), String> {
        placement_set(window, target, true, deadline)
    }
}

// Semantic controls use private retained AX references, on the SAME helper main
// thread as monitors/windows. None of these wrappers sends global input.
use crate::macos_monitor::controls::{self, Metadata, Safety};
#[derive(Clone)]
pub(crate) struct RetainedElement {
    element: AXUIElement,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
fn control_attr(e: &AXUIElement, key: &str, deadline: Instant) -> Result<CFType, String> {
    placement_permissions(deadline)?;
    placement_timeout(e, deadline)?;
    copy_attr(e, key).ok_or_else(|| format!("required AX attribute {key} unavailable"))
}
fn control_string(value: CFType, cap: usize) -> Result<String, String> {
    let string = value
        .downcast_into::<CFString>()
        .ok_or("AX string type required")?;
    // Bound before materializing Rust UTF-8, then enforce the UTF-8 byte cap.
    // SAFETY: retained, dynamically checked CFString.
    let length =
        unsafe { core_foundation::string::CFStringGetLength(string.as_concrete_TypeRef()) };
    if length < 0 || length as usize > cap {
        return Err("AX string limit exceeded".into());
    }
    let text = string.to_string();
    if text.len() > cap {
        return Err("AX UTF-8 string limit exceeded".into());
    }
    Ok(text)
}
// Optional does not mean errors are ignored: only documented absence without
// an accompanying value is admitted. In particular, failed IPC is never false.
fn control_optional_result(
    status: accessibility_sys::AXError,
    value: Option<CFType>,
) -> Result<Option<CFType>, String> {
    if (status == kAXErrorAttributeUnsupported || status == kAXErrorNoValue) && value.is_none() {
        return Ok(None);
    }
    if status != kAXErrorSuccess || value.is_none() {
        return Err("optional AX metadata unavailable or contradictory".into());
    }
    Ok(value)
}
fn control_optional_attr(
    element: &AXUIElement,
    attribute: &str,
    deadline: Instant,
) -> Result<Option<CFType>, String> {
    placement_permissions(deadline)?;
    placement_timeout(element, deadline)?;
    let key = CFString::new(attribute);
    let mut raw = std::ptr::null();
    // SAFETY: retained object/key and writable Copy-rule output. Every returned
    // object is released even on failure, and typed only by the decoder below.
    let status = unsafe {
        accessibility_sys::AXUIElementCopyAttributeValue(
            element.as_concrete_TypeRef(),
            key.as_concrete_TypeRef(),
            &mut raw,
        )
    };
    let value = if raw.is_null() {
        None
    } else {
        // SAFETY: non-null Copy-rule CF object, dynamically checked by caller.
        Some(unsafe { CFType::wrap_under_create_rule(raw) })
    };
    placement::time_left(deadline)?;
    control_optional_result(status, value)
        .map_err(|_| format!("AX {attribute} read failed; metadata is not known"))
}
fn control_sensitive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("secure") || name.contains("password")
}
fn control_optional_subrole(value: Option<CFType>) -> Result<Option<String>, String> {
    value.map(|v| control_string(v, 64)).transpose()
}
fn control_optional_protected(value: Option<CFType>) -> Result<bool, String> {
    match value {
        None => Ok(false),
        Some(value) => value
            .downcast_into::<CFBoolean>()
            .map(bool::from)
            .ok_or_else(|| "AX protected-content metadata must be a Boolean".into()),
    }
}
fn control_safety(e: &AXUIElement, deadline: Instant) -> Result<Safety, String> {
    let role = control_string(control_attr(e, kAXRoleAttribute, deadline)?, 64)?;
    if role.is_empty() {
        return Err("empty AX role".into());
    }
    if control_sensitive_name(&role) {
        return Ok(Safety { role, secure: true });
    }
    // AXSubrole is optional in Apple's SDK. An absent specialization is not a
    // transport failure. Explicit secure subroles still stop before all content.
    let subrole = control_optional_subrole(control_optional_attr(e, "AXSubrole", deadline)?)?;
    if subrole.as_deref().is_some_and(control_sensitive_name) {
        return Ok(Safety { role, secure: true });
    }
    // Also respect protected content on containers, before labels/children/value.
    // This is reported app metadata, not a security sandbox or a promise that
    // arbitrary app-supplied text contains no sensitive information.
    let secure = control_optional_protected(control_optional_attr(
        e,
        "AXContainsProtectedContent",
        deadline,
    )?)?;
    Ok(Safety { role, secure })
}
// Only documented absence is an empty optional label. An IPC/type error must
// not make a changed button title look identical to a previous empty snapshot.
fn control_label_result(
    status: accessibility_sys::AXError,
    value: Option<CFType>,
) -> Result<String, String> {
    if (status == kAXErrorAttributeUnsupported || status == kAXErrorNoValue) && value.is_none() {
        return Ok(String::new());
    }
    if status != kAXErrorSuccess {
        return Err("AX title read failed; refresh after the application responds".into());
    }
    control_string(
        value.ok_or("AX title missing from successful reply")?,
        controls::MAX_LABEL,
    )
}
fn control_label(e: &AXUIElement, deadline: Instant) -> Result<String, String> {
    placement_timeout(e, deadline)?;
    let key = CFString::new(kAXTitleAttribute);
    let mut raw = std::ptr::null();
    // SAFETY: retained element/key and writable Copy-rule output. All non-null
    // output is wrapped and released, including on native error.
    let status = unsafe {
        accessibility_sys::AXUIElementCopyAttributeValue(
            e.as_concrete_TypeRef(),
            key.as_concrete_TypeRef(),
            &mut raw,
        )
    };
    let value = if raw.is_null() {
        None
    } else {
        // SAFETY: Copy-rule output is independently retained and type-checked below.
        Some(unsafe { CFType::wrap_under_create_rule(raw) })
    };
    placement::time_left(deadline)?;
    control_label_result(status, value)
}
fn control_enabled(e: &AXUIElement, deadline: Instant) -> Result<bool, String> {
    control_attr(e, kAXEnabledAttribute, deadline)?
        .downcast_into::<CFBoolean>()
        .map(bool::from)
        .ok_or_else(|| "AX enabled state unavailable".into())
}
fn control_settable(e: &AXUIElement, deadline: Instant) -> Result<bool, String> {
    placement_timeout(e, deadline)?;
    let key = CFString::new(kAXValueAttribute);
    let mut value = 0;
    // SAFETY: retained object/key and writable C Boolean output.
    let status = unsafe {
        accessibility_sys::AXUIElementIsAttributeSettable(
            e.as_concrete_TypeRef(),
            key.as_concrete_TypeRef(),
            &mut value,
        )
    };
    if status != kAXErrorSuccess {
        return Err("AX value mutability unavailable".into());
    }
    Ok(value != 0)
}
// AXPress may include an AppKit button animation. Keep mutation IPC bounded
// separately from the short read probes, without exceeding the operation budget.
fn control_action_timeout_seconds(remaining: std::time::Duration) -> Result<f32, String> {
    let seconds = remaining.as_secs_f32().min(0.5);
    if seconds <= 0.0 {
        return Err("element action deadline exceeded before dispatch".into());
    }
    Ok(seconds)
}
fn control_action_timeout(e: &AXUIElement, deadline: Instant) -> Result<(), String> {
    placement::time_left(deadline)?;
    let seconds =
        control_action_timeout_seconds(deadline.saturating_duration_since(Instant::now()))?;
    // SAFETY: exact retained element; strictly positive timeout capped at 500 ms
    // and the remaining operation budget. This does not set a system-wide default.
    let status = unsafe { AXUIElementSetMessagingTimeout(e.as_concrete_TypeRef(), seconds) };
    if status != kAXErrorSuccess {
        return Err("cannot bound element action messaging timeout".into());
    }
    Ok(())
}
fn control_press_supported(e: &AXUIElement, deadline: Instant) -> Result<bool, String> {
    placement_timeout(e, deadline)?;
    let mut raw = std::ptr::null();
    // SAFETY: retained AX object and writable Copy-rule array output.
    let status =
        unsafe { accessibility_sys::AXUIElementCopyActionNames(e.as_concrete_TypeRef(), &mut raw) };
    if raw.is_null() {
        return Err("AX action names unavailable".into());
    }
    // SAFETY: Copy-rule array retained and released on every following exit.
    let actions: CFArray = unsafe { CFArray::wrap_under_create_rule(raw) };
    if status != kAXErrorSuccess || actions.len() > 32 {
        return Err("AX action names unavailable/excessive".into());
    }
    let mut press = false;
    for item in actions.iter() {
        placement::time_left(deadline)?;
        let ptr = *item;
        if ptr.is_null() {
            return Err("null AX action name".into());
        }
        // SAFETY: the array retains this CF item, get-rule wrapper retains it.
        let value = unsafe { CFType::wrap_under_get_rule(ptr) };
        press |= control_string(value, 64)? == "AXPress";
    }
    Ok(press)
}
fn control_writable(e: &AXUIElement, press: bool, deadline: Instant) -> Result<(), String> {
    placement_permissions(deadline)?;
    let safety = control_safety(e, deadline)?;
    if safety.secure || !control_enabled(e, deadline)? {
        return Err("secure or disabled control refused".into());
    }
    let supported = if press {
        matches!(
            safety.role.as_str(),
            "AXButton" | "AXCheckBox" | "AXRadioButton"
        ) && control_press_supported(e, deadline)?
    } else {
        matches!(safety.role.as_str(), "AXTextField" | "AXTextArea")
            && control_settable(e, deadline)?
    };
    if !supported {
        return Err("control role/operation unsupported".into());
    }
    placement::time_left(deadline)
}

fn control_child_count(status: accessibility_sys::AXError, count: isize) -> Result<usize, String> {
    if status == kAXErrorAttributeUnsupported || status == kAXErrorNoValue {
        Ok(0)
    } else if status == kAXErrorSuccess && (0..=controls::MAX_CHILDREN as isize).contains(&count) {
        Ok(count as usize)
    } else {
        Err("AX children count unavailable or overflow".into())
    }
}

fn control_children_result(
    status: accessibility_sys::AXError,
    value: Option<CFType>,
    deadline: Instant,
) -> Result<Vec<AXUIElement>, String> {
    placement::time_left(deadline)?;
    if (status == kAXErrorAttributeUnsupported || status == kAXErrorNoValue) && value.is_none() {
        return Ok(vec![]);
    }
    if status != kAXErrorSuccess {
        return Err("bounded AX children unavailable".into());
    }
    let array = value
        .and_then(|v| v.downcast_into::<CFArray>())
        .ok_or("AX children array required")?;
    if array.len() as usize > controls::MAX_CHILDREN {
        return Err("AX children overflow".into());
    }
    let mut children = Vec::new();
    for item in array.iter() {
        placement::time_left(deadline)?;
        let ptr = *item;
        if ptr.is_null() {
            return Err("null AX child".into());
        }
        // SAFETY: the array retains this CF item throughout the loop.
        let value = unsafe { CFType::wrap_under_get_rule(ptr) };
        children.push(
            value
                .downcast_into::<AXUIElement>()
                .ok_or("non-element AX child")?,
        );
    }
    Ok(children)
}

fn control_child_membership<E>(
    path: &[E],
    mut children: impl FnMut(&E) -> Result<Vec<E>, String>,
    equal: impl Fn(&E, &E) -> bool,
) -> Result<(), String> {
    for edge in path.windows(2) {
        let current = children(&edge[0])?;
        if current.len() > controls::MAX_CHILDREN
            || !current.iter().any(|child| equal(child, &edge[1]))
        {
            return Err("retained AX child membership changed or excessive".into());
        }
    }
    Ok(())
}

impl controls::Native for PlacementNative {
    type Element = RetainedElement;
    fn root(&mut self, window: &RetainedWindow) -> Result<RetainedElement, String> {
        Ok(RetainedElement {
            element: window.element.clone(),
            _thread: std::marker::PhantomData,
        })
    }
    fn equal(&self, a: &RetainedElement, b: &RetainedElement) -> bool {
        a.element == b.element
    }
    fn safety(&mut self, e: &RetainedElement, deadline: Instant) -> Result<Safety, String> {
        control_safety(&e.element, deadline)
    }
    fn children(
        &mut self,
        e: &RetainedElement,
        deadline: Instant,
    ) -> Result<Vec<RetainedElement>, String> {
        placement_timeout(&e.element, deadline)?;
        let key = CFString::new(kAXChildrenAttribute);
        let mut count = -1;
        // SAFETY: retained object/key and writable CFIndex output.
        let status = unsafe {
            accessibility_sys::AXUIElementGetAttributeValueCount(
                e.element.as_concrete_TypeRef(),
                key.as_concrete_TypeRef(),
                &mut count,
            )
        };
        placement::time_left(deadline)?;
        if control_child_count(status, count)? == 0 {
            // Do not request index zero from an empty array: the SDK permits
            // kAXErrorIllegalArgument for an out-of-range copy.
            return Ok(vec![]);
        }
        placement_timeout(&e.element, deadline)?;
        let mut raw = std::ptr::null();
        // SAFETY: retained object/key, writable Copy-rule output, bounded cap+1.
        let status = unsafe {
            accessibility_sys::AXUIElementCopyAttributeValues(
                e.element.as_concrete_TypeRef(),
                key.as_concrete_TypeRef(),
                0,
                (controls::MAX_CHILDREN + 1) as _,
                &mut raw,
            )
        };
        let value = if raw.is_null() {
            None
        } else {
            // SAFETY: Copy-rule CF result; release even on error and dynamically
            // check for an array before using any array-specific operations.
            Some(unsafe { CFType::wrap_under_create_rule(raw as _) })
        };
        control_children_result(status, value, deadline)?
            .into_iter()
            .map(|element| {
                placement_timeout(&element, deadline)?;
                Ok(RetainedElement {
                    element,
                    _thread: std::marker::PhantomData,
                })
            })
            .collect()
    }
    fn metadata(
        &mut self,
        e: &RetainedElement,
        role: &str,
        deadline: Instant,
    ) -> Result<Metadata, String> {
        let safety = control_safety(&e.element, deadline)?;
        if safety.secure || safety.role != role {
            return Err("control safety changed".into());
        }
        let enabled = control_enabled(&e.element, deadline)?;
        // Disabled controls are omitted without even reading their label.
        if !enabled {
            return Ok(Metadata {
                label: String::new(),
                bounds: placement_ax(&e.element, deadline)?,
                enabled,
                press: false,
                value_settable: false,
            });
        }
        let text = matches!(role, "AXTextField" | "AXTextArea");
        let press = !text && control_press_supported(&e.element, deadline)?;
        let value_settable = text && control_settable(&e.element, deadline)?;
        // AXTitle only: no AXDescription/document text/value fallback. Missing
        // title is an empty label; excessive present titles refuse the snapshot.
        let label = control_label(&e.element, deadline)?;
        Ok(Metadata {
            label,
            bounds: placement_ax(&e.element, deadline)?,
            enabled,
            press,
            value_settable,
        })
    }
    fn member(
        &mut self,
        window: &RetainedWindow,
        path: &[RetainedElement],
        deadline: Instant,
    ) -> Result<(), String> {
        if path.is_empty()
            || path.len() > controls::MAX_DEPTH + 1
            || path[0].element != window.element
        {
            return Err("invalid retained AX ancestry".into());
        }
        placement_generation(window.identity)?;
        for (index, e) in path.iter().enumerate() {
            let safety = control_safety(&e.element, deadline)?;
            if safety.secure || placement_pid(&e.element, deadline)? != window.identity.pid {
                return Err("secure/foreign AX ancestry".into());
            }
            if index > 0 {
                let parent: AXUIElement = control_attr(&e.element, "AXParent", deadline)?
                    .downcast_into()
                    .ok_or("AX parent unavailable")?;
                if parent != path[index - 1].element {
                    return Err(format!(
                        "retained AX parent changed at depth {index} for {}",
                        safety.role
                    ));
                }
                let owner: AXUIElement = control_attr(&e.element, "AXWindow", deadline)?
                    .downcast_into()
                    .ok_or("AX window unavailable")?;
                if owner != window.element {
                    return Err("element left retained AX window".into());
                }
            }
        }
        // Back-pointers alone may survive detachment. Check every parent still
        // exposes its exact retained child; never replace any path object.
        control_child_membership(
            path,
            |parent| self.children(parent, deadline),
            |a, b| a.element == b.element,
        )?;
        placement::time_left(deadline)
    }
    fn press(&mut self, e: &RetainedElement, deadline: Instant) -> Result<(), String> {
        control_writable(&e.element, true, deadline)?;
        let action = CFString::new("AXPress");
        control_action_timeout(&e.element, deadline)?;
        // SAFETY: exact retained live element and action; allowed role/action
        // checked immediately above. No global input or focus mutation.
        let status = unsafe {
            accessibility_sys::AXUIElementPerformAction(
                e.element.as_concrete_TypeRef(),
                action.as_concrete_TypeRef(),
            )
        };
        if status != kAXErrorSuccess {
            return Err(format!("AXPress refused or unconfirmed ({status})"));
        }
        Ok(())
    }
    fn set_value(
        &mut self,
        e: &RetainedElement,
        text: &str,
        deadline: Instant,
    ) -> Result<(), String> {
        if text.len() > controls::MAX_TEXT {
            return Err("text limit exceeded".into());
        }
        control_writable(&e.element, false, deadline)?;
        let key = CFString::new(kAXValueAttribute);
        let value = CFString::new(text);
        placement::time_left(deadline)?;
        control_action_timeout(&e.element, deadline)?;
        // SAFETY: exact retained live element, key and bounded string. Only
        // nonsecure enabled editable text roles with settable AXValue reach here.
        let status = unsafe {
            accessibility_sys::AXUIElementSetAttributeValue(
                e.element.as_concrete_TypeRef(),
                key.as_concrete_TypeRef(),
                value.as_CFTypeRef(),
            )
        };
        if status != kAXErrorSuccess {
            return Err(format!("AXValue write refused or unconfirmed ({status})"));
        }
        Ok(())
    }
    fn value_matches(
        &mut self,
        e: &RetainedElement,
        text: &str,
        deadline: Instant,
    ) -> Result<bool, String> {
        control_writable(&e.element, false, deadline)?; // secure check BEFORE value
        let value = control_string(
            control_attr(&e.element, kAXValueAttribute, deadline)?,
            controls::MAX_TEXT,
        )?;
        placement::time_left(deadline)?;
        Ok(value == text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_control_metadata_distinguishes_absence_from_failure() {
        for status in [kAXErrorAttributeUnsupported, kAXErrorNoValue] {
            assert!(control_optional_result(status, None).unwrap().is_none());
            assert!(
                control_optional_result(status, Some(CFString::new("AXUnknown").as_CFType()))
                    .is_err()
            );
        }
        for status in [
            accessibility_sys::kAXErrorCannotComplete,
            accessibility_sys::kAXErrorInvalidUIElement,
            accessibility_sys::kAXErrorIllegalArgument,
        ] {
            assert!(control_optional_result(status, None).is_err());
            assert!(
                control_optional_result(status, Some(CFBoolean::false_value().as_CFType()))
                    .is_err()
            );
        }
        assert!(control_optional_result(kAXErrorSuccess, None).is_err());
        let value = CFString::new("AXUnknown").as_CFType();
        assert_eq!(
            control_optional_result(kAXErrorSuccess, Some(value.clone())).unwrap(),
            Some(value)
        );
    }

    #[test]
    fn optional_control_subroles_remain_typed_and_bounded() {
        assert!(control_optional_subrole(None).unwrap().is_none());
        for name in ["", "AXUnknown", "AXSearchField"] {
            assert_eq!(
                control_optional_subrole(Some(CFString::new(name).as_CFType()))
                    .unwrap()
                    .as_deref(),
                Some(name)
            );
        }
        assert!(control_optional_subrole(Some(CFBoolean::false_value().as_CFType())).is_err());
        assert!(
            control_optional_subrole(Some(CFString::new(&"a".repeat(65)).as_CFType())).is_err()
        );
        assert!(
            control_optional_subrole(Some(CFString::new(&"π".repeat(33)).as_CFType())).is_err()
        );
        for name in ["AXSecureTextField", "axpassword", "AXContainerSecure"] {
            assert!(control_sensitive_name(name));
        }
        assert!(!control_sensitive_name("AXTextField"));
        assert!(!control_sensitive_name("AXButton"));
    }

    #[test]
    fn optional_protected_content_requires_actual_boolean() {
        assert!(!control_optional_protected(None).unwrap());
        assert!(!control_optional_protected(Some(CFBoolean::false_value().as_CFType())).unwrap());
        assert!(control_optional_protected(Some(CFBoolean::true_value().as_CFType())).unwrap());
        for value in [
            CFString::new("false").as_CFType(),
            CFString::new("").as_CFType(),
            CFNumber::from(0i32).as_CFType(),
        ] {
            assert!(control_optional_protected(Some(value)).is_err());
        }
    }

    #[test]
    fn optional_control_label_distinguishes_absence_from_ipc_or_type_failure() {
        use accessibility_sys::{kAXErrorCannotComplete, kAXErrorInvalidUIElement};
        for status in [kAXErrorAttributeUnsupported, kAXErrorNoValue] {
            assert_eq!(control_label_result(status, None).unwrap(), "");
        }
        for status in [
            kAXErrorCannotComplete,
            kAXErrorInvalidUIElement,
            kAXErrorSuccess,
        ] {
            assert!(control_label_result(status, None).is_err());
        }
        let label = CFString::new("Preview").as_CFType();
        assert_eq!(
            control_label_result(kAXErrorSuccess, Some(label.clone())).unwrap(),
            "Preview"
        );
        assert!(control_label_result(kAXErrorCannotComplete, Some(label)).is_err());
        assert!(
            control_label_result(kAXErrorSuccess, Some(CFBoolean::true_value().as_CFType()))
                .is_err()
        );
    }

    #[test]
    fn semantic_mutation_timeout_is_positive_capped_and_respects_remaining_budget() {
        use std::time::Duration;
        assert!(control_action_timeout_seconds(Duration::ZERO).is_err());
        assert_eq!(
            control_action_timeout_seconds(Duration::from_secs(4)).unwrap(),
            0.5
        );
        assert_eq!(
            control_action_timeout_seconds(Duration::from_millis(500)).unwrap(),
            0.5
        );
        let short = control_action_timeout_seconds(Duration::from_millis(10)).unwrap();
        assert!((short - 0.01).abs() < f32::EPSILON);
        assert!(control_action_timeout_seconds(Duration::from_nanos(1)).unwrap() > 0.0);
    }

    #[test]
    fn control_children_accept_only_documented_absence_or_bounded_counts() {
        use accessibility_sys::{
            kAXErrorCannotComplete, kAXErrorFailure, kAXErrorIllegalArgument,
            kAXErrorInvalidUIElement, kAXErrorNotImplemented,
        };
        for status in [kAXErrorAttributeUnsupported, kAXErrorNoValue] {
            assert_eq!(control_child_count(status, -1).unwrap(), 0);
        }
        assert_eq!(control_child_count(kAXErrorSuccess, 0).unwrap(), 0);
        assert_eq!(
            control_child_count(kAXErrorSuccess, controls::MAX_CHILDREN as isize).unwrap(),
            controls::MAX_CHILDREN
        );
        for count in [-1, controls::MAX_CHILDREN as isize + 1] {
            assert!(control_child_count(kAXErrorSuccess, count).is_err());
        }
        for status in [
            kAXErrorCannotComplete,
            kAXErrorFailure,
            kAXErrorIllegalArgument,
            kAXErrorInvalidUIElement,
            kAXErrorNotImplemented,
        ] {
            // A zero-initialized output is never evidence of a leaf on failure.
            assert!(control_child_count(status, 0).is_err());
            assert!(control_children_result(
                status,
                None,
                Instant::now() + std::time::Duration::from_secs(4)
            )
            .is_err());
        }
    }

    #[test]
    fn control_children_leaf_results_do_not_mask_malformed_arrays_or_limits() {
        let deadline = Instant::now() + std::time::Duration::from_secs(4);
        let empty = || CFArray::<CFString>::from_CFTypes(&[]).as_CFType();
        for status in [kAXErrorAttributeUnsupported, kAXErrorNoValue] {
            assert!(control_children_result(status, None, deadline)
                .unwrap()
                .is_empty());
            assert!(control_children_result(status, Some(empty()), deadline).is_err());
        }
        assert!(
            control_children_result(kAXErrorSuccess, Some(empty()), deadline)
                .unwrap()
                .is_empty()
        );
        assert!(control_children_result(kAXErrorSuccess, None, deadline).is_err());
        assert!(control_children_result(
            kAXErrorSuccess,
            Some(CFString::new("not an array").as_CFType()),
            deadline,
        )
        .is_err());
        let non_element = CFArray::from_CFTypes(&[CFString::new("not an AX element")]);
        assert!(
            control_children_result(kAXErrorSuccess, Some(non_element.as_CFType()), deadline)
                .err()
                .unwrap()
                .contains("non-element")
        );
        let overflow =
            CFArray::from_CFTypes(&vec![CFString::new("child"); controls::MAX_CHILDREN + 1]);
        assert!(
            control_children_result(kAXErrorSuccess, Some(overflow.as_CFType()), deadline)
                .err()
                .unwrap()
                .contains("overflow")
        );
        assert!(control_children_result(
            kAXErrorSuccess,
            Some(empty()),
            Instant::now() - std::time::Duration::from_secs(1),
        )
        .is_err());
    }

    #[test]
    fn control_membership_rechecks_each_retained_edge_without_substitution() {
        // This is the edge validator called by PlacementNative::member after
        // AXParent/AXWindow/security checks, with current child reads injected.
        let path = [10, 20, 30];
        let mut visited = Vec::new();
        control_child_membership(
            &path,
            |parent| {
                visited.push(*parent);
                Ok(vec![parent + 10])
            },
            |a, b| a == b,
        )
        .unwrap();
        assert_eq!(visited, [10, 20]);
        for parent in [10, 20] {
            for failure in ["detached", "replacement", "overflow", "ipc"] {
                let result = control_child_membership(
                    &path,
                    |current| {
                        if *current != parent {
                            return Ok(vec![current + 10]);
                        }
                        match failure {
                            "detached" => Ok(vec![]),
                            "replacement" => Ok(vec![99]),
                            "overflow" => Ok(vec![current + 10; controls::MAX_CHILDREN + 1]),
                            _ => Err("AX messaging failed".into()),
                        }
                    },
                    |a, b| a == b,
                );
                assert!(result.is_err(), "{parent}: {failure}");
            }
        }
    }

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

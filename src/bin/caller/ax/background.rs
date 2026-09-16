//! Exact-window AX operations in the existing AX unsafe island.
//! Never activate or raise an application/window; never guess window identity.
use super::*;
use crate::background_cu::{Bounds, WindowTarget};
use serde::Serialize;

#[derive(Serialize)]
pub struct WindowSummary {
    pub target: String,
    pub pid: i32,
    pub window_id: u32,
    pub app: String,
    pub title: Option<String>,
    pub bounds: Bounds,
}
pub fn windows() -> Vec<WindowSummary> {
    window_list(core_graphics::window::kCGWindowListOptionAll)
        .into_iter()
        .filter_map(|w| {
            let (seconds, micros) = crate::platform::macos_process_birth(w.pid)?;
            let (x, y, width, height) = w.bounds?;
            if w.window_id == 0 || width == 0 || height == 0 || width > 16384 || height > 16384 {
                return None;
            }
            let t = WindowTarget {
                pid: w.pid,
                window_id: w.window_id,
                birth_seconds: seconds,
                birth_micros: micros,
            };
            Some(WindowSummary {
                target: t.selector(),
                pid: w.pid,
                window_id: w.window_id,
                app: w.owner,
                title: w.title,
                bounds: Bounds {
                    x,
                    y,
                    width,
                    height,
                },
            })
        })
        .take(256)
        .collect()
}
pub fn validate(t: WindowTarget) -> Result<WindowSummary, String> {
    if crate::platform::macos_process_birth(t.pid) != Some((t.birth_seconds, t.birth_micros)) {
        return Err("target exited or PID reused; rediscover with cu windows".into());
    }
    windows()
        .into_iter()
        .find(|w| w.target == t.selector())
        .ok_or_else(|| "selected window unavailable; no substitute selected".into())
}

/// Runtime-resolved exact AX/CGWindowID mapping SPI. Fail closed when absent;
/// do not substitute a window matched by title, bounds or enumeration order.
fn window_id(e: &AXUIElement) -> Result<u32, String> {
    type GetWindow = unsafe extern "C" fn(AXUIElementRef, *mut u32) -> i32;
    // SAFETY: constant NUL-terminated symbol. ApplicationServices is already
    // linked by accessibility-sys and remains loaded for this process lifetime.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"_AXUIElementGetWindow".as_ptr()) };
    if symbol.is_null() {
        return Err("exact AX window mapping unavailable; background input disabled".into());
    }
    // SAFETY: this symbol's ABI is AXError(AXUIElementRef, CGWindowID*).
    let get: GetWindow = unsafe { std::mem::transmute(symbol) };
    let mut id = 0;
    // SAFETY: retained live element and writable u32 storage.
    let err = unsafe { get(e.as_concrete_TypeRef(), &mut id) };
    if err != kAXErrorSuccess || id == 0 {
        return Err(format!("cannot map AX window exactly ({err})"));
    }
    Ok(id)
}
fn application(pid: i32) -> Result<AXUIElement, String> {
    if !is_trusted() {
        return Err(
            "background AX/input requires Intendant's macOS Accessibility permission".into(),
        );
    }
    // SAFETY: Create-rule API accepts PID; wrapper owns and releases result.
    let app = unsafe { AXUIElement::wrap_under_create_rule(AXUIElementCreateApplication(pid)) };
    // SAFETY: live retained AX object; bound IPC to unresponsive applications.
    unsafe { AXUIElementSetMessagingTimeout(app.as_concrete_TypeRef(), 0.2) };
    Ok(app)
}
fn exact_window(t: WindowTarget, focused: bool) -> Result<AXUIElement, String> {
    let app = application(t.pid)?;
    if focused {
        let w: AXUIElement = copy_attr(&app, kAXFocusedWindowAttribute)
            .and_then(|v| v.downcast_into())
            .ok_or("target app has no AX-focused window; no activation attempted")?;
        if window_id(&w)? != t.window_id {
            return Err(
                "another window is focused inside target app; background input refused".into(),
            );
        }
        return Ok(w);
    }
    let roots = copy_attr(&app, kAXWindowsAttribute)
        .and_then(|v| cf_as_element_array(&v))
        .ok_or("target app does not expose AX windows")?;
    for w in roots.into_iter().take(128) {
        if window_id(&w).ok() == Some(t.window_id) {
            return Ok(w);
        }
    }
    Err("selected window not exposed by AX; no other window selected".into())
}
pub fn assert_background(t: WindowTarget, b: Bounds, focused: bool) -> Result<(), String> {
    if validate(t)?.bounds != b {
        return Err("window moved/resized; capture again before input".into());
    }
    // SAFETY: Create-rule, no arguments; wrapper owns result.
    let system = unsafe { AXUIElement::wrap_under_create_rule(AXUIElementCreateSystemWide()) };
    let front: AXUIElement = copy_attr(&system, "AXFocusedApplication")
        .and_then(|v| v.downcast_into())
        .ok_or("cannot determine foreground app; background input refused")?;
    let mut pid = 0;
    // SAFETY: retained live AX element and writable PID storage.
    let err =
        unsafe { accessibility_sys::AXUIElementGetPid(front.as_concrete_TypeRef(), &mut pid) };
    if err != kAXErrorSuccess || pid <= 0 {
        return Err("cannot identify foreground app; input refused".into());
    }
    if pid == t.pid {
        return Err(
            "target is the user's foreground app; background input paused to avoid collision"
                .into(),
        );
    }
    exact_window(t, focused)?;
    Ok(())
}
pub fn read(t: WindowTarget, full: bool) -> Result<ScreenElements, String> {
    let info = validate(t)?;
    let root = exact_window(t, false)?;
    let mut budget = crate::computer_use::ELEMENT_TREE_MAX_NODES;
    let mut capped = false;
    let mut tree = walk(
        &root,
        0,
        crate::computer_use::ELEMENT_TREE_MAX_DEPTH,
        &mut budget,
        &mut capped,
    );
    fn local(n: &mut UiElement, x: i32, y: i32) {
        n.frame.0 = n.frame.0.saturating_sub(x);
        n.frame.1 = n.frame.1.saturating_sub(y);
        for c in &mut n.children {
            local(c, x, y);
        }
    }
    local(&mut tree, info.bounds.x, info.bounds.y);
    let mut s = ScreenElements {
        app: info.app,
        pid: t.pid,
        window_title: info.title,
        root: Some(tree),
        other_windows: Vec::new(),
        truncated: (budget == 0 || capped)
            .then(|| "bounded window AX tree; deeper content omitted".into()),
    };
    if !full {
        crate::computer_use::cap_screen_elements_texts(&mut s);
    }
    if validate(t)?.bounds != info.bounds {
        return Err("window changed during AX read; retry".into());
    }
    Ok(s)
}
fn supports_press(e: &AXUIElement) -> bool {
    let mut names: CFArrayRef = std::ptr::null();
    // SAFETY: retained live element and output storage. Successful Copy
    // transfers one retain, subsequently released by the wrapper.
    let err = unsafe {
        accessibility_sys::AXUIElementCopyActionNames(e.as_concrete_TypeRef(), &mut names)
    };
    if err != kAXErrorSuccess || names.is_null() {
        return false;
    }
    // SAFETY: successful CopyActionNames returns an array of CFStrings.
    let names: CFArray<CFString> = unsafe { CFArray::wrap_under_create_rule(names) };
    names.iter().any(|n| n.to_string() == "AXPress")
}
/// False means no semantic action exists. A failed/uncertain AX request is an
/// error, never a signal to retry via a second synthetic click.
pub fn press(t: WindowTarget, b: Bounds, x: i32, y: i32) -> Result<bool, String> {
    assert_background(t, b, false)?;
    let root = exact_window(t, false)?;
    fn find(
        e: AXUIElement,
        x: i64,
        y: i64,
        budget: &mut usize,
        depth: usize,
    ) -> Option<AXUIElement> {
        if *budget == 0 || depth > 12 {
            return None;
        }
        *budget -= 1;
        let (left, top, width, height) = element_frame(&e)?;
        if x < i64::from(left)
            || y < i64::from(top)
            || x >= i64::from(left) + i64::from(width)
            || y >= i64::from(top) + i64::from(height)
        {
            return None;
        }
        if let Some(children) = child_elements(&e) {
            for c in children.into_iter().rev() {
                if let Some(found) = find(c, x, y, budget, depth + 1) {
                    return Some(found);
                }
            }
        }
        (attr_bool(&e, kAXEnabledAttribute).unwrap_or(true) && supports_press(&e)).then_some(e)
    }
    let Some(e) = find(
        root,
        i64::from(b.x) + i64::from(x),
        i64::from(b.y) + i64::from(y),
        &mut 400,
        0,
    ) else {
        return Ok(false);
    };
    assert_background(t, b, false)?;
    let action = CFString::new("AXPress");
    // SAFETY: retained live element and action; bounded AX IPC.
    let err = unsafe {
        accessibility_sys::AXUIElementPerformAction(
            e.as_concrete_TypeRef(),
            action.as_concrete_TypeRef(),
        )
    };
    if err != kAXErrorSuccess {
        return Err(format!(
            "AXPress returned {err}; delivery uncertain, so no click retry attempted"
        ));
    }
    Ok(true)
}

//! Public process-local CGEvents plus exact-window AX grounding. No HID path.
use super::*;
use crate::ax::background as ax;
use crate::computer_use::{MouseButton, ScrollDirection};
use core_graphics::event::{CGEvent, CGEventType, CGMouseButton, EventField, ScrollEventUnit};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

type Gate = tokio::sync::Mutex<()>;
static GATES: OnceLock<Mutex<HashMap<i32, Weak<Gate>>>> = OnceLock::new();
fn gate(pid: i32) -> Arc<Gate> {
    let mut gates = GATES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    gates.retain(|_, weak| weak.strong_count() > 0);
    if let Some(existing) = gates.get(&pid).and_then(Weak::upgrade) {
        return existing;
    }
    let gate = Arc::new(Gate::new(()));
    gates.insert(pid, Arc::downgrade(&gate));
    gate
}

pub async fn run(spec: String, request: Request) -> Result<Reply, String> {
    // A dropped HTTP future must not drop the per-process fence while blocking
    // AX/input work is running. The owned worker keeps it through cleanup.
    tokio::spawn(async move {
        if let Request::List = request {
            if spec != LIST_TARGET { return Err("window discovery requires macos_windows".into()); }
            let list = tokio::task::spawn_blocking(ax::windows).await.map_err(|e| e.to_string())?;
            return Ok(Reply { text: serde_json::json!({"mode":"background", "experimental":true,
                "coordinate_space":"window-local logical points", "windows":list,
                "limitations":["no HID or foreground fallback", "raw events require the selected AX-focused window", "app event acceptance must be verified", "Chromium/canvas compatibility is not guaranteed"]}).to_string(), ..Reply::default() });
        }
        let target = WindowTarget::parse(&spec)?;
        let lock = gate(target.pid);
        let _guard = tokio::time::timeout(Duration::from_secs(5), lock.lock_owned()).await
            .map_err(|_| "target process is busy; retry after its current batch completes")?;
        let summary = tokio::task::spawn_blocking(move || ax::validate(target)).await.map_err(|e| e.to_string())??;
        let bounds = summary.bounds;
        match request {
            Request::List => unreachable!(),
            Request::Read { full_values, json } => {
                let snapshot = tokio::task::spawn_blocking(move || ax::read(target, full_values)).await.map_err(|e| e.to_string())??;
                let text = if json { serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())? }
                    else { crate::computer_use::format_screen_elements(&snapshot) };
                Ok(Reply { text, ..Reply::default() })
            }
            Request::Capture => Ok(Reply { text: serde_json::json!({"mode":"background", "target":spec,
                "width":bounds.width, "height":bounds.height, "coordinate_space":"window-local logical points"}).to_string(),
                png: Some(capture(target, bounds).await?), failed: false }),
            Request::Actions { actions, normalized, observe } => {
                validate_actions(&actions)?;
                for action in &actions { preflight(action, bounds, normalized)?; }
                let deadline = Instant::now() + Duration::from_secs(30);
                let mut reply = Reply::default();
                let mut rows = Vec::new();
                for (index, action) in actions.into_iter().enumerate() {
                    if Instant::now() >= deadline { rows.push("batch deadline reached; remaining actions not executed".into()); reply.failed = true; break; }
                    let result: Result<String, String> = if matches!(action, CuAction::Screenshot) {
                        match capture(target, bounds).await {
                            Ok(png) => { reply.png = Some(png); Ok("ok: selected-window screenshot captured".into()) },
                            Err(e) => Err(e),
                        }
                    } else {
                        // Earlier pixels are not evidence of a subsequent action.
                        reply.png = None;
                        tokio::task::spawn_blocking(move || input(target, bounds, &action, normalized, deadline))
                            .await.map_err(|e| e.to_string())?
                    };
                    match result {
                        Ok(detail) => rows.push(format!("action[{index}] {detail}")),
                        Err(error) => { rows.push(format!("action[{index}] failed: {error}; remaining actions not executed")); reply.failed = true; break; }
                    }
                }
                if reply.png.is_none() && !matches!(observe, ObserveMode::None) {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let mut need_pixels = matches!(observe, ObserveMode::Pixels);
                    if matches!(observe, ObserveMode::Ax | ObserveMode::Auto) {
                        match tokio::task::spawn_blocking(move || ax::read(target, false)).await.map_err(|e| e.to_string())? {
                            Ok(tree) if tree.root.is_some() => rows.push(format!("observation: window AX\n{}", crate::computer_use::format_screen_elements(&tree))),
                            Ok(_) | Err(_) if matches!(observe, ObserveMode::Auto) => need_pixels = true,
                            Ok(_) => { reply.failed = true; rows.push("observation failed: selected-window AX tree unavailable".into()); },
                            Err(e) => { reply.failed = true; rows.push(format!("observation failed: {e}")); },
                        }
                    }
                    if need_pixels {
                        match capture(target, bounds).await {
                            Ok(png) => { reply.png = Some(png); rows.push("observation: selected-window pixels".into()); },
                            Err(e) => { reply.failed = true; rows.push(format!("observation failed: {e}")); },
                        }
                    }
                }
                rows.push(format!("background target: {spec}; dispatched input is unverified; no foreground/HID/clipboard fallback"));
                reply.text = rows.join("\n");
                Ok(reply)
            }
        }
    }).await.map_err(|e| format!("background worker failed: {e}"))?
}

async fn capture(target: WindowTarget, bounds: Bounds) -> Result<Vec<u8>, String> {
    let current = tokio::task::spawn_blocking(move || ax::validate(target))
        .await
        .map_err(|e| e.to_string())??;
    if current.bounds != bounds {
        return Err("window moved/resized; use a fresh capture request".into());
    }
    let png = crate::display::macos::background::capture_window_png(
        target.window_id,
        bounds.width,
        bounds.height,
    )
    .await
    .map_err(|e| e.to_string())?;
    let current = tokio::task::spawn_blocking(move || ax::validate(target))
        .await
        .map_err(|e| e.to_string())??;
    if current.bounds != bounds {
        return Err("window changed during capture; frame discarded".into());
    }
    Ok(png)
}

fn preflight(action: &CuAction, bounds: Bounds, normalized: bool) -> Result<(), String> {
    match action {
        CuAction::Click { x, y, .. }
        | CuAction::DoubleClick { x, y, .. }
        | CuAction::TripleClick { x, y, .. }
        | CuAction::MoveMouse { x, y }
        | CuAction::Scroll { x, y, .. } => {
            bounds.point(*x, *y, normalized)?;
        }
        CuAction::Drag {
            start_x,
            start_y,
            end_x,
            end_y,
        } => {
            bounds.point(*start_x, *start_y, normalized)?;
            bounds.point(*end_x, *end_y, normalized)?;
        }
        CuAction::Key { key } => {
            crate::computer_use::macos_input::parse_key(key)?;
        }
        _ => {}
    }
    Ok(())
}
fn source() -> Result<CGEventSource, String> {
    CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|_| "cannot create private event source".into())
}
fn mouse(
    target: WindowTarget,
    bounds: Bounds,
    kind: CGEventType,
    point: (i32, i32),
    button: CGMouseButton,
    clicks: i64,
) -> Result<(), String> {
    let event = CGEvent::new_mouse_event(
        source()?,
        kind,
        CGPoint::new(
            f64::from(bounds.x) + f64::from(point.0),
            f64::from(bounds.y) + f64::from(point.1),
        ),
        button,
    )
    .map_err(|_| "cannot create process-local mouse event")?;
    event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, clicks);
    event.set_integer_value_field(
        EventField::MOUSE_EVENT_WINDOW_UNDER_MOUSE_POINTER,
        i64::from(target.window_id),
    );
    event.set_integer_value_field(
        EventField::MOUSE_EVENT_WINDOW_UNDER_MOUSE_POINTER_THAT_CAN_HANDLE_THIS_EVENT,
        i64::from(target.window_id),
    );
    event.post_to_pid(target.pid);
    Ok(())
}
fn input(
    target: WindowTarget,
    bounds: Bounds,
    action: &CuAction,
    normalized: bool,
    deadline: Instant,
) -> Result<String, String> {
    if let CuAction::Wait { ms } = action {
        std::thread::sleep(Duration::from_millis(*ms));
        return Ok("ok: wait completed".into());
    }
    let point = |x, y| bounds.point(x, y, normalized);
    if let CuAction::Click {
        x,
        y,
        button: MouseButton::Left,
    } = action
    {
        let p = point(*x, *y)?;
        if ax::press(target, bounds, p.0, p.1)? {
            return Ok(
                "injected: AXPress dispatched to selected-window element (effect unverified)"
                    .into(),
            );
        }
    }
    // Raw events address a PROCESS, not a guaranteed window. Require an exact
    // match to its internally focused window; never activate or switch it.
    ax::assert_background(target, bounds, true)?;
    match action {
        CuAction::Click { x, y, button }
        | CuAction::DoubleClick { x, y, button }
        | CuAction::TripleClick { x, y, button } => {
            let (down, up, button) = match button {
                MouseButton::Left => (
                    CGEventType::LeftMouseDown,
                    CGEventType::LeftMouseUp,
                    CGMouseButton::Left,
                ),
                MouseButton::Right => (
                    CGEventType::RightMouseDown,
                    CGEventType::RightMouseUp,
                    CGMouseButton::Right,
                ),
                MouseButton::Middle => (
                    CGEventType::OtherMouseDown,
                    CGEventType::OtherMouseUp,
                    CGMouseButton::Center,
                ),
            };
            let clicks = match action {
                CuAction::DoubleClick { .. } => 2,
                CuAction::TripleClick { .. } => 3,
                _ => 1,
            };
            for state in 1..=clicks {
                ax::assert_background(target, bounds, true)?;
                mouse(target, bounds, down, point(*x, *y)?, button, state)?;
                mouse(target, bounds, up, point(*x, *y)?, button, state)?;
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        CuAction::MoveMouse { x, y } => mouse(
            target,
            bounds,
            CGEventType::MouseMoved,
            point(*x, *y)?,
            CGMouseButton::Left,
            0,
        )?,
        CuAction::Key { key } => {
            let (code, flags) = crate::computer_use::macos_input::parse_key(key)?;
            let down = CGEvent::new_keyboard_event(source()?, code, true)
                .map_err(|_| "cannot create key-down")?;
            let up = CGEvent::new_keyboard_event(source()?, code, false)
                .map_err(|_| "cannot create key-up")?;
            down.set_flags(flags);
            up.set_flags(flags);
            down.post_to_pid(target.pid);
            up.post_to_pid(target.pid);
        }
        CuAction::Type { text } => {
            for ch in text.chars() {
                if Instant::now() >= deadline {
                    return Err("typing deadline reached; prefix may have been delivered".into());
                }
                ax::assert_background(target, bounds, true)?;
                let code = match ch {
                    '\n' | '\r' => 36,
                    '\t' => 48,
                    _ => 0,
                };
                let down = CGEvent::new_keyboard_event(source()?, code, true)
                    .map_err(|_| "cannot create text key-down")?;
                let up = CGEvent::new_keyboard_event(source()?, code, false)
                    .map_err(|_| "cannot create text key-up")?;
                if code == 0 {
                    let mut units = [0u16; 2];
                    let encoded = ch.encode_utf16(&mut units);
                    down.set_string_from_utf16_unchecked(encoded);
                    up.set_string_from_utf16_unchecked(encoded);
                }
                down.post_to_pid(target.pid);
                up.post_to_pid(target.pid);
                std::thread::sleep(Duration::from_millis(8));
            }
        }
        CuAction::Scroll {
            x,
            y,
            direction,
            amount,
        } => {
            let (vertical, horizontal) = match direction {
                ScrollDirection::Up => (*amount, 0),
                ScrollDirection::Down => (-*amount, 0),
                ScrollDirection::Left => (0, *amount),
                ScrollDirection::Right => (0, -*amount),
            };
            let event = CGEvent::new_scroll_event(
                source()?,
                ScrollEventUnit::LINE,
                2,
                vertical,
                horizontal,
                0,
            )
            .map_err(|_| "cannot create scroll")?;
            let p = point(*x, *y)?;
            event.set_location(CGPoint::new(
                f64::from(bounds.x) + f64::from(p.0),
                f64::from(bounds.y) + f64::from(p.1),
            ));
            event.post_to_pid(target.pid);
        }
        CuAction::Drag {
            start_x,
            start_y,
            end_x,
            end_y,
        } => {
            let start = point(*start_x, *start_y)?;
            let end = point(*end_x, *end_y)?;
            mouse(
                target,
                bounds,
                CGEventType::LeftMouseDown,
                start,
                CGMouseButton::Left,
                1,
            )?;
            let moved = mouse(
                target,
                bounds,
                CGEventType::LeftMouseDragged,
                end,
                CGMouseButton::Left,
                1,
            );
            let released = mouse(
                target,
                bounds,
                CGEventType::LeftMouseUp,
                end,
                CGMouseButton::Left,
                1,
            );
            moved?;
            released?;
        }
        _ => return Err("unsupported background action; no foreground fallback".into()),
    }
    Ok("injected: process-local CGEvent dispatched (app acceptance/effect unverified)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn per_process_gates_share_only_with_the_same_process() {
        let a = gate(42);
        let b = gate(42);
        let c = gate(43);
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
    }
    #[test]
    fn out_of_window_actions_fail_in_preflight_without_native_calls() {
        let b = Bounds {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(preflight(&CuAction::MoveMouse { x: 100, y: 2 }, b, false).is_err());
        assert!(preflight(&CuAction::MoveMouse { x: 1001, y: 2 }, b, true).is_err());
    }
}

//! Shared window vocabulary and main-thread transaction engine. No native calls
//! here: the helper injects AX and retained-monitor geometry implementations.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub(crate) const MAX_BINDINGS: usize = 16;
pub(crate) const MAX_CANDIDATES: usize = 16;
/// Logical points per edge/size component. Containment itself has NO tolerance.
pub(crate) const TOLERANCE: f64 = 1.0;
pub(super) const BUDGET: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WindowIdentity {
    pub pid: i32,
    pub start_seconds: u64,
    pub start_micros: u32,
    pub window_id: u32,
}
impl WindowIdentity {
    pub(crate) fn validate(self) -> Result<(), String> {
        if self.pid <= 0
            || self.start_seconds == 0
            || self.start_micros >= 1_000_000
            || self.window_id == 0
        {
            return Err("invalid explicit window identity".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
impl Bounds {
    pub(crate) fn validate(self) -> Result<(), String> {
        if ![self.x, self.y, self.width, self.height]
            .iter()
            .all(|n| n.is_finite())
            || self.x.abs() > 1_000_000.0
            || self.y.abs() > 1_000_000.0
            || self.width <= 0.0
            || self.height <= 0.0
            || self.width > 16384.0
            || self.height > 16384.0
        {
            return Err("invalid or excessive window geometry".into());
        }
        Ok(())
    }
    pub(crate) fn contains(self, other: Self) -> bool {
        self.validate().is_ok()
            && other.validate().is_ok()
            && other.x >= self.x
            && other.y >= self.y
            && other.x + other.width <= self.x + self.width
            && other.y + other.height <= self.y + self.height
    }
    pub(crate) fn close(self, other: Self) -> bool {
        self.validate().is_ok()
            && other.validate().is_ok()
            && [
                (self.x, other.x),
                (self.y, other.y),
                (self.width, other.width),
                (self.height, other.height),
            ]
            .iter()
            .all(|(a, b)| (a - b).abs() <= TOLERANCE)
    }
    fn target(self, local: Self) -> Result<Self, String> {
        local.validate()?;
        let target = Self {
            x: self.x + local.x,
            y: self.y + local.y,
            ..local
        };
        if !self.contains(target) {
            return Err("entire requested rectangle must fit the owned monitor".into());
        }
        Ok(target)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Candidate {
    pub candidate: String,
    pub identity: WindowIdentity,
    pub bounds: Bounds,
}
pub(crate) fn validate_candidate(candidate: &str) -> Result<(), String> {
    if !candidate
        .strip_prefix("macos_candidate:")
        .is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err("invalid opaque window candidate token".into());
    }
    Ok(())
}
/// The exact AX object acquired during enumeration, never re-resolved by ID.
pub(crate) struct ListedWindow<W> {
    pub identity: WindowIdentity,
    pub bounds: Bounds,
    pub window: W,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Observation {
    pub ax: Bounds,
    pub cg: Bounds,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlacementStatus {
    Verified,
    Partial,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlacementResult {
    pub status: PlacementStatus,
    pub requested_global: Bounds,
    pub before: Observation,
    pub after: Option<Observation>,
    pub writes_attempted: u8,
    /// None means focus could not be observed after the attempted write.
    pub focus_interference: Option<bool>,
    pub detail: Option<String>,
}
impl PlacementResult {
    pub(crate) fn verified(&self) -> bool {
        matches!(self.status, PlacementStatus::Verified)
    }
    pub(super) fn valid_reply(&self) -> bool {
        self.requested_global.validate().is_ok()
            && self.before.ax.validate().is_ok()
            && self.before.cg.validate().is_ok()
            && self.writes_attempted <= 2
            && self.detail.as_ref().is_none_or(|s| s.len() <= 2048)
            && self
                .after
                .is_none_or(|o| o.ax.validate().is_ok() && o.cg.validate().is_ok())
            && (!self.verified()
                || ((self.writes_attempted != 0
                    || (self.before.ax.close(self.requested_global)
                        && self.before.cg.close(self.requested_global)))
                    && self.focus_interference == Some(false)
                    && self.detail.is_none()
                    && self.after.is_some_and(|o| {
                        o.ax.close(self.requested_global)
                            && o.cg.close(self.requested_global)
                            && o.ax.close(o.cg)
                    })))
    }
}

pub(crate) fn time_left(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err("window operation deadline exceeded".into())
    } else {
        Ok(())
    }
}

/// AX objects deliberately have no Send/Sync requirement. All methods execute
/// in the same private helper main thread, including Drop on every error path.
pub(crate) trait Native {
    type Window;
    type Focus: PartialEq;
    /// Read-only native readiness for exact background pointer input. Never prompt.
    fn pointer_ready(&mut self, _window: &Self::Window, _deadline: Instant) -> Result<(), String> {
        Err("native bound-window pointer input unavailable".into())
    }
    /// Construct both events without posting; default platforms fail closed.
    fn pointer_pair(
        &mut self,
        _window: &Self::Window,
        _point: crate::macos_monitor::pointer::Point,
        _global: crate::macos_monitor::pointer::Point,
        _deadline: Instant,
    ) -> Result<Box<dyn crate::macos_monitor::pointer::Pair>, String> {
        Err("native bound-window pointer input unavailable".into())
    }
    /// Readback-only settling never repeats a setter or changes focus.
    /// Fakes override this pacing hook; native work retains the operation budget.
    fn pause_readback(&mut self, deadline: Instant) -> Result<(), String> {
        time_left(deadline)?;
        std::thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
        time_left(deadline)
    }
    fn candidates(
        &mut self,
        pid: i32,
        deadline: Instant,
    ) -> Result<Vec<ListedWindow<Self::Window>>, String>;
    fn observe(
        &mut self,
        window: &Self::Window,
        identity: WindowIdentity,
        deadline: Instant,
    ) -> Result<Observation, String>;
    fn focus(&mut self, deadline: Instant) -> Result<Self::Focus, String>;
    fn position(
        &mut self,
        window: &Self::Window,
        target: Bounds,
        deadline: Instant,
    ) -> Result<(), String>;
    fn size(
        &mut self,
        window: &Self::Window,
        target: Bounds,
        deadline: Instant,
    ) -> Result<(), String>;
}
pub(super) struct Binding<W> {
    pub(super) monitor: u32,
    pub(super) identity: WindowIdentity,
    pub(super) geometry: Bounds,
    pub(super) window: W,
}
pub(super) struct Windows<N: super::controls::Native> {
    pub(super) native: N,
    pub(super) elements: super::controls::Inventory<N::Element, N::Focus>,
    pub(super) pointers: super::pointer::Inventory<N::Focus>,
    next: u32,
    pub(super) bindings: BTreeMap<u32, Binding<N::Window>>,
    candidates: BTreeMap<String, ListedWindow<N::Window>>,
}
impl<N: super::controls::Native> Windows<N> {
    pub(super) fn new(native: N) -> Self {
        Self {
            native,
            elements: Default::default(),
            pointers: Default::default(),
            next: 1,
            bindings: BTreeMap::new(),
            candidates: BTreeMap::new(),
        }
    }
    pub(super) fn candidates(&mut self, pid: i32) -> Result<Vec<Candidate>, String> {
        // One replaceable inventory across all PIDs/callers. A failed refresh
        // also invalidates the previous inventory; bindings remain independent.
        self.candidates.clear();
        if pid <= 0 {
            return Err("candidate listing requires a positive explicit PID".into());
        }
        let candidates = self.native.candidates(pid, Instant::now() + BUDGET)?;
        if candidates.len() > MAX_CANDIDATES {
            return Err("candidate capacity exceeded".into());
        }
        let mut result = Vec::new();
        for listed in candidates {
            let candidate = format!("macos_candidate:{}", uuid::Uuid::new_v4().simple());
            result.push(Candidate {
                candidate: candidate.clone(),
                identity: listed.identity,
                bounds: listed.bounds,
            });
            self.candidates.insert(candidate, listed);
        }
        Ok(result)
    }
    pub(super) fn bind(
        &mut self,
        monitor: u32,
        identity: WindowIdentity,
        candidate: &str,
        mut geometry: impl FnMut(u32) -> Result<Bounds, String>,
    ) -> Result<u32, String> {
        identity.validate()?;
        validate_candidate(candidate)?;
        let listed = self
            .candidates
            .get(candidate)
            .ok_or("stale or foreign window candidate; list again")?;
        if listed.identity != identity {
            return Err("window candidate identity mismatch".into());
        }
        if self.bindings.len() >= MAX_BINDINGS {
            return Err("window binding capacity reached; explicitly unbind".into());
        }
        if self.bindings.values().any(|b| b.identity == identity) {
            return Err("window is already bound; explicitly unbind first".into());
        }
        let deadline = Instant::now() + BUDGET;
        let bounds = geometry(monitor)?;
        bounds.validate()?;
        // Once selected, consume the token even if validation later fails.
        // Transfer the listed object; never retain a replacement found by ID.
        let window = self
            .candidates
            .remove(candidate)
            .expect("candidate checked")
            .window;
        let observation = self.native.observe(&window, identity, deadline)?;
        if !observation.ax.close(observation.cg) || geometry(monitor)? != bounds {
            return Err("window or monitor geometry mismatch during binding".into());
        }
        time_left(deadline)?;
        let id = self.next;
        self.next = id.checked_add(1).ok_or("binding generations exhausted")?;
        self.bindings.insert(
            id,
            Binding {
                monitor,
                identity,
                geometry: bounds,
                window,
            },
        );
        Ok(id)
    }
    pub(super) fn unbind(&mut self, id: u32) -> Result<(), String> {
        self.elements.invalidate(id);
        self.pointers.invalidate(id);
        self.bindings
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| "stale window binding".into())
    }
    pub(super) fn destroy_monitor(&mut self, monitor: u32) {
        for (&id, b) in &self.bindings {
            if b.monitor == monitor {
                self.elements.invalidate(id);
                self.pointers.invalidate(id);
            }
        }
        self.bindings.retain(|_, b| b.monitor != monitor);
    }
    pub(super) fn place(
        &mut self,
        id: u32,
        local: Bounds,
        mut geometry: impl FnMut(u32) -> Result<Bounds, String>,
    ) -> Result<PlacementResult, String> {
        self.elements.invalidate(id);
        self.pointers.invalidate(id);
        let b = self.bindings.get(&id).ok_or("stale window binding")?;
        let deadline = Instant::now() + BUDGET;
        let target = b.geometry.target(local)?;
        let check_geometry =
            |geometry: &mut dyn FnMut(u32) -> Result<Bounds, String>| -> Result<(), String> {
                time_left(deadline)?;
                if geometry(b.monitor)? != b.geometry {
                    return Err(
                        "owned monitor moved, resized or disappeared; rebind explicitly".into(),
                    );
                }
                Ok(())
            };
        check_geometry(&mut geometry)?;
        let focus = self.native.focus(deadline)?;
        let before = self.native.observe(&b.window, b.identity, deadline)?;
        if !before.ax.close(before.cg) {
            return Err("AX/CG geometry mismatch before placement".into());
        }
        let mut result = PlacementResult {
            status: PlacementStatus::Partial,
            requested_global: target,
            before,
            after: None,
            writes_attempted: 0,
            focus_interference: None,
            detail: None,
        };
        // Position then size, at most once each. No retries, rollback, activation or focus
        // restoration. Revalidate exact identity and monitor on BOTH sides of
        // EACH write, even when an AX setter returns an error (it may have acted).
        for size in [false, true] {
            result.focus_interference = None;
            let preflight = (|| {
                let current = self.native.observe(&b.window, b.identity, deadline)?;
                let previous = result.after.unwrap_or(before);
                result.after = Some(current);
                if !current.ax.close(current.cg) {
                    return Err("AX/CG geometry mismatch before write".into());
                }
                if !current.ax.close(previous.ax) || !current.cg.close(previous.cg) {
                    return Err(
                        "window geometry changed between placement checks; stopped without retry"
                            .into(),
                    );
                }
                if self.native.focus(deadline)? != focus {
                    result.focus_interference = Some(true);
                    return Err("focus changed; placement stopped without restoring focus".into());
                }
                check_geometry(&mut geometry)
            })();
            if let Err(error) = preflight {
                if result.writes_attempted == 0 {
                    return Err(error);
                }
                result.detail = Some(error);
                return Ok(result);
            }
            // Skip only when BOTH observations match the component exactly.
            // No-op stages retain the same identity/geometry/focus postchecks.
            let current = result.after.expect("preflight observed");
            let matches = [current.ax, current.cg].iter().all(|bounds| {
                if size {
                    bounds.width == target.width && bounds.height == target.height
                } else {
                    bounds.x == target.x && bounds.y == target.y
                }
            });
            result.focus_interference = None;
            let write = if matches {
                Ok(())
            } else {
                result.writes_attempted += 1;
                if size {
                    self.native.size(&b.window, target, deadline)
                } else {
                    self.native.position(&b.window, target, deadline)
                }
            };
            let post = (|| {
                check_geometry(&mut geometry)?;
                result.after = Some(self.native.observe(&b.window, b.identity, deadline)?);
                if self.native.focus(deadline)? != focus {
                    result.focus_interference = Some(true);
                    return Err("focus changed during placement; no restoration attempted".into());
                }
                result.focus_interference = Some(false);
                check_geometry(&mut geometry)
            })();
            if let Err(error) = write.and(post) {
                result.detail = Some(error);
                return Ok(result);
            }
        }
        // AX may acknowledge a resize before WindowServer publishes its CG
        // geometry. Observe for <=250 ms / 20 polls within the original budget,
        // never repeat either write and never restore focus or old geometry.
        let settle_deadline = deadline.min(Instant::now() + Duration::from_millis(250));
        for attempt in 0..=20 {
            let after = result.after.expect("both placement stages observed");
            if after.ax.close(target)
                && after.cg.close(target)
                && after.ax.close(after.cg)
                && b.geometry.contains(after.ax)
                && b.geometry.contains(after.cg)
            {
                result.status = PlacementStatus::Verified;
                return Ok(result);
            }
            if attempt == 20 {
                break;
            }
            result.focus_interference = None;
            let observed = (|| {
                self.native.pause_readback(settle_deadline)?;
                check_geometry(&mut geometry)?;
                result.after = Some(
                    self.native
                        .observe(&b.window, b.identity, settle_deadline)?,
                );
                if self.native.focus(settle_deadline)? != focus {
                    result.focus_interference = Some(true);
                    return Err("focus changed during readback; no restoration attempted".into());
                }
                result.focus_interference = Some(false);
                check_geometry(&mut geometry)
            })();
            if let Err(error) = observed {
                result.detail = Some(error);
                return Ok(result);
            }
        }
        result.detail = Some(
            "AX/CG readback did not verify the entire requested rectangle inside the owned monitor"
                .into(),
        );
        Ok(result)
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) struct UnsupportedNative;
#[cfg(not(target_os = "macos"))]
impl Native for UnsupportedNative {
    type Window = ();
    type Focus = ();
    fn candidates(&mut self, _: i32, _: Instant) -> Result<Vec<ListedWindow<()>>, String> {
        Err("macOS window placement requires macOS".into())
    }
    fn observe(&mut self, _: &(), _: WindowIdentity, _: Instant) -> Result<Observation, String> {
        Err("macOS window placement requires macOS".into())
    }
    fn focus(&mut self, _: Instant) -> Result<(), String> {
        Err("macOS window placement requires macOS".into())
    }
    fn position(&mut self, _: &(), _: Bounds, _: Instant) -> Result<(), String> {
        Err("macOS window placement requires macOS".into())
    }
    fn size(&mut self, _: &(), _: Bounds, _: Instant) -> Result<(), String> {
        Err("macOS window placement requires macOS".into())
    }
}

#[cfg(not(target_os = "macos"))]
impl super::controls::Native for UnsupportedNative {
    type Element = ();
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListMacosWindowsParams {
    /// Explicit application PID. At most 16 candidate windows, no title heuristics.
    pub pid: i32,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindMacosWindowParams {
    /// Exact opaque macos_virtual selector of an existing daemon-owned monitor.
    pub display_target: String,
    /// Full candidate identity, including process start generation, supplied explicitly.
    pub identity: WindowIdentity,
    /// Opaque token from the latest list. Refresh (even failed) invalidates all
    /// unbound candidates across PIDs/callers; a selected bind consumes its token.
    pub candidate: String,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaceMacosWindowParams {
    /// Exact opaque binding returned by bind_macos_window.
    pub binding: String,
    /// Monitor-local logical points, top-left origin. Entire rectangle must fit.
    pub bounds: Bounds,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnbindMacosWindowParams {
    pub binding: String,
}

#[cfg(test)]
pub(in crate::macos_monitor) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    pub(in crate::macos_monitor) fn identity() -> WindowIdentity {
        WindowIdentity {
            pid: 123,
            start_seconds: 456,
            start_micros: 7,
            window_id: 8,
        }
    }
    pub(in crate::macos_monitor) fn monitor() -> Bounds {
        Bounds {
            x: -800.0,
            y: -200.0,
            width: 800.0,
            height: 600.0,
        }
    }
    fn local() -> Bounds {
        Bounds {
            x: 20.0,
            y: 30.0,
            width: 300.0,
            height: 200.0,
        }
    }
    #[test]
    fn zero_write_placement_still_checks_focus_and_permissions() {
        for lose_permission in [false, true] {
            let f = Fake::default();
            f.0.borrow_mut().bounds = monitor().target(local()).unwrap();
            let mut w = Windows::new(f.clone());
            let c = w.candidates(identity().pid).unwrap().remove(0);
            let binding = w
                .bind(1, identity(), &c.candidate, |_| Ok(monitor()))
                .unwrap();
            let mut probes = 0;
            let result = w
                .place(binding, local(), |_| {
                    probes += 1;
                    if probes == 2 {
                        if lose_permission {
                            f.0.borrow_mut().permission = false;
                        } else {
                            f.0.borrow_mut().focus += 1;
                        }
                    }
                    Ok(monitor())
                })
                .unwrap();
            assert!(!result.verified(), "{result:?}");
            assert!(result.valid_reply(), "{result:?}");
            assert_eq!(result.writes_attempted, 0);
            assert_eq!(f.0.borrow().writes, 0);
            if !lose_permission {
                assert_eq!(result.focus_interference, Some(true));
            }
        }
    }

    #[test]
    fn zero_write_verified_reply_requires_matching_initial_geometry() {
        let f = Fake::default();
        f.0.borrow_mut().bounds = monitor().target(local()).unwrap();
        let mut w = Windows::new(f);
        let c = w.candidates(identity().pid).unwrap().remove(0);
        let binding = w
            .bind(1, identity(), &c.candidate, |_| Ok(monitor()))
            .unwrap();
        let mut result = w.place(binding, local(), |_| Ok(monitor())).unwrap();
        assert!(result.valid_reply());
        result.before.cg.width += 10.0;
        assert!(
            !result.valid_reply(),
            "no-op cannot claim unexplained geometry changes"
        );
        result.before = result.after.unwrap();
        result.writes_attempted = 3;
        assert!(!result.valid_reply());
    }

    #[test]
    fn matching_placement_components_are_not_written_again() {
        for (position_matches, size_matches, writes) in [
            (false, false, 2),
            (false, true, 1),
            (true, false, 1),
            (true, true, 0),
        ] {
            let f = Fake::default();
            let target = monitor().target(local()).unwrap();
            {
                let mut m = f.0.borrow_mut();
                if position_matches {
                    m.bounds.x = target.x;
                    m.bounds.y = target.y;
                }
                if size_matches {
                    m.bounds.width = target.width;
                    m.bounds.height = target.height;
                    m.fail_size = true;
                }
            }
            let mut windows = Windows::new(f.clone());
            let candidate = windows.candidates(identity().pid).unwrap().remove(0);
            let binding = windows
                .bind(1, identity(), &candidate.candidate, |_| Ok(monitor()))
                .unwrap();
            let result = windows.place(binding, local(), |_| Ok(monitor())).unwrap();
            assert!(result.verified(), "{result:?}");
            assert!(result.valid_reply(), "{result:?}");
            assert_eq!(result.writes_attempted, writes);
            assert_eq!(f.0.borrow().writes, writes as usize);
        }
    }

    #[derive(Clone)]
    pub(in crate::macos_monitor) struct Fake(pub(in crate::macos_monitor) Rc<RefCell<Model>>);
    pub(in crate::macos_monitor) struct Model {
        pub(in crate::macos_monitor) retained: usize,
        pub(in crate::macos_monitor) writes: usize,
        birth: u64,
        object: u64,
        live: bool,
        permission: bool,
        spi: bool,
        ambiguous: bool,
        bounds: Bounds,
        focus: u32,
        focus_after_write: bool,
        replace_after_write: bool,
        ignore_size: bool,
        lag_after_size: u8,
        readback_pauses: usize,
        focus_during_readback: bool,
        replace_during_readback: bool,
        fail_size: bool,
        cg_delta: f64,
        list_count: usize,
        observations: usize,
        change_on_second_preflight: bool,
        disagree_on_second_preflight: bool,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self(Rc::new(RefCell::new(Model {
                retained: 0,
                writes: 0,
                birth: 456,
                object: 1,
                live: true,
                permission: true,
                spi: true,
                ambiguous: false,
                bounds: Bounds {
                    x: 100.0,
                    y: 100.0,
                    width: 400.0,
                    height: 300.0,
                },
                focus: 1,
                focus_after_write: false,
                replace_after_write: false,
                ignore_size: false,
                lag_after_size: 0,
                readback_pauses: 0,
                focus_during_readback: false,
                replace_during_readback: false,
                fail_size: false,
                cg_delta: 0.0,
                list_count: 1,
                observations: 0,
                change_on_second_preflight: false,
                disagree_on_second_preflight: false,
            })))
        }
    }
    pub(in crate::macos_monitor) struct Exact {
        object: u64,
        state: Rc<RefCell<Model>>,
    }
    impl Drop for Exact {
        fn drop(&mut self) {
            self.state.borrow_mut().retained -= 1;
        }
    }
    impl Fake {
        fn check(&self, identity: WindowIdentity, deadline: Instant) -> Result<(), String> {
            time_left(deadline)?;
            let m = self.0.borrow();
            if !m.permission {
                return Err("permission missing".into());
            }
            if !m.spi {
                return Err("SPI missing".into());
            }
            if m.ambiguous {
                return Err("ambiguous window".into());
            }
            if !m.live || identity.start_seconds != m.birth {
                return Err("stale PID/window".into());
            }
            Ok(())
        }
    }
    impl super::super::controls::Native for Fake {
        type Element = ();
    }
    impl Native for Fake {
        type Window = Exact;
        type Focus = u32;
        fn pause_readback(&mut self, deadline: Instant) -> Result<(), String> {
            time_left(deadline)?;
            let mut m = self.0.borrow_mut();
            m.readback_pauses += 1;
            m.lag_after_size = m.lag_after_size.saturating_sub(1);
            if m.focus_during_readback {
                m.focus += 1;
            }
            if m.replace_during_readback {
                m.object += 1;
            }
            Ok(())
        }
        fn candidates(
            &mut self,
            pid: i32,
            deadline: Instant,
        ) -> Result<Vec<ListedWindow<Exact>>, String> {
            self.check(identity(), deadline)?;
            let mut m = self.0.borrow_mut();
            let mut result = Vec::new();
            for n in 0..m.list_count {
                let mut identity = identity();
                identity.pid = pid;
                identity.window_id += n as u32;
                m.retained += 1;
                result.push(ListedWindow {
                    identity,
                    bounds: m.bounds,
                    window: Exact {
                        object: m.object,
                        state: self.0.clone(),
                    },
                });
            }
            Ok(result)
        }
        fn observe(
            &mut self,
            window: &Exact,
            identity: WindowIdentity,
            deadline: Instant,
        ) -> Result<Observation, String> {
            self.check(identity, deadline)?;
            let mut m = self.0.borrow_mut();
            m.observations += 1;
            // Bind is observation 1; before/preflight/post-position are 2..4.
            if m.observations == 5 {
                if m.change_on_second_preflight {
                    m.bounds.width += 10.0;
                }
                if m.disagree_on_second_preflight {
                    m.cg_delta = 10.0;
                }
            }
            if window.object != m.object {
                return Err("same-PID CGWindowID replacement".into());
            }
            Ok(Observation {
                ax: m.bounds,
                cg: Bounds {
                    x: m.bounds.x
                        + if m.writes == 2 && m.lag_after_size > 0 {
                            8.0
                        } else {
                            m.cg_delta
                        },
                    ..m.bounds
                },
            })
        }
        fn focus(&mut self, deadline: Instant) -> Result<u32, String> {
            time_left(deadline)?;
            Ok(self.0.borrow().focus)
        }
        fn position(&mut self, _: &Exact, target: Bounds, deadline: Instant) -> Result<(), String> {
            time_left(deadline)?;
            let mut m = self.0.borrow_mut();
            m.writes += 1;
            m.bounds.x = target.x;
            m.bounds.y = target.y;
            if m.focus_after_write {
                m.focus += 1;
            }
            if m.replace_after_write {
                m.object += 1;
            }
            Ok(())
        }
        fn size(&mut self, _: &Exact, target: Bounds, deadline: Instant) -> Result<(), String> {
            time_left(deadline)?;
            let mut m = self.0.borrow_mut();
            m.writes += 1;
            if !m.ignore_size {
                m.bounds.width = target.width;
                m.bounds.height = target.height;
            }
            if m.fail_size {
                return Err("setter error after possible application".into());
            }
            Ok(())
        }
    }
    fn bound() -> (Windows<Fake>, Fake, u32) {
        let fake = Fake::default();
        let mut windows = Windows::new(fake.clone());
        let candidate = windows.candidates(123).unwrap().remove(0).candidate;
        let id = windows
            .bind(1, identity(), &candidate, |_| Ok(monitor()))
            .unwrap();
        (windows, fake, id)
    }
    #[test]
    fn listed_object_cannot_be_replaced_between_list_and_bind() {
        let fake = Fake::default();
        let mut windows = Windows::new(fake.clone());
        let listed = windows.candidates(123).unwrap().remove(0);
        assert_eq!(fake.0.borrow().retained, 1);
        fake.0.borrow_mut().object += 1; // same PID birth and CGWindowID
        assert!(windows
            .bind(1, listed.identity, &listed.candidate, |_| Ok(monitor()))
            .unwrap_err()
            .contains("replacement"));
        assert_eq!(fake.0.borrow().retained, 0);
        assert_eq!(fake.0.borrow().writes, 0);
        let fresh = windows.candidates(123).unwrap().remove(0);
        assert_ne!(fresh.candidate, listed.candidate);
        windows
            .bind(1, fresh.identity, &fresh.candidate, |_| Ok(monitor()))
            .unwrap();
    }
    #[test]
    fn inventory_is_bounded_replaced_and_identity_checked() {
        let fake = Fake::default();
        let mut windows = Windows::new(fake.clone());
        fake.0.borrow_mut().list_count = MAX_CANDIDATES;
        let old = windows.candidates(123).unwrap().remove(0);
        let current = windows.candidates(124).unwrap().remove(0);
        assert_eq!(fake.0.borrow().retained, MAX_CANDIDATES);
        assert!(windows
            .bind(1, old.identity, &old.candidate, |_| Ok(monitor()))
            .is_err());
        let mut wrong = current.identity;
        wrong.window_id += 1;
        assert!(windows
            .bind(1, wrong, &current.candidate, |_| Ok(monitor()))
            .is_err());
        assert!(windows
            .bind(
                1,
                current.identity,
                "macos_candidate:ffffffffffffffffffffffffffffffff",
                |_| Ok(monitor())
            )
            .is_err());
        let id = windows
            .bind(1, current.identity, &current.candidate, |_| Ok(monitor()))
            .unwrap();
        windows.unbind(id).unwrap();
        assert!(windows
            .bind(1, current.identity, &current.candidate, |_| Ok(monitor()))
            .is_err());
        fake.0.borrow_mut().permission = false;
        assert!(windows.candidates(123).is_err());
        assert_eq!(fake.0.borrow().retained, 0);
        fake.0.borrow_mut().permission = true;
        fake.0.borrow_mut().list_count = MAX_CANDIDATES + 1;
        assert!(windows.candidates(123).is_err());
        assert_eq!(fake.0.borrow().retained, 0);
        fake.0.borrow_mut().list_count = MAX_CANDIDATES;
        windows.candidates(123).unwrap();
        drop(windows);
        assert_eq!(fake.0.borrow().retained, 0);
        assert_eq!(fake.0.borrow().writes, 0);
    }
    #[test]
    fn second_write_refusal_preserves_latest_observation_even_when_ax_cg_disagree() {
        for disagree in [false, true] {
            let (mut w, fake, id) = bound();
            fake.0.borrow_mut().change_on_second_preflight = !disagree;
            fake.0.borrow_mut().disagree_on_second_preflight = disagree;
            let result = w.place(id, local(), |_| Ok(monitor())).unwrap();
            assert!(!result.verified());
            assert_eq!(result.writes_attempted, 1);
            let after = result.after.unwrap();
            let model = fake.0.borrow();
            assert_eq!(after.ax, model.bounds);
            assert_eq!(after.cg.x, model.bounds.x + model.cg_delta);
            assert_eq!(model.writes, 1);
            assert!(result.detail.unwrap().contains(if disagree {
                "AX/CG"
            } else {
                "changed between"
            }));
        }
    }
    #[test]
    fn delayed_cg_readback_settles_without_repeating_writes() {
        let (mut windows, fake, id) = bound();
        fake.0.borrow_mut().lag_after_size = 2;
        let result = windows.place(id, local(), |_| Ok(monitor())).unwrap();
        assert!(result.verified());
        assert_eq!(result.writes_attempted, 2);
        assert_eq!(fake.0.borrow().writes, 2);
        assert_eq!(fake.0.borrow().readback_pauses, 2);
    }

    #[test]
    fn readback_wait_is_bounded_and_preserves_focus_and_identity_refusals() {
        for failure in 0..3 {
            let (mut windows, fake, id) = bound();
            {
                let mut m = fake.0.borrow_mut();
                m.lag_after_size = 100;
                m.focus_during_readback = failure == 1;
                m.replace_during_readback = failure == 2;
            }
            let result = windows.place(id, local(), |_| Ok(monitor())).unwrap();
            assert!(!result.verified());
            assert_eq!(result.writes_attempted, 2);
            assert_eq!(fake.0.borrow().writes, 2);
            assert!(fake.0.borrow().readback_pauses <= 20);
            assert!(result.detail.is_some());
            if failure == 1 {
                assert_eq!(result.focus_interference, Some(true));
            }
            if failure == 0 {
                assert_eq!(fake.0.borrow().readback_pauses, 20);
            }
        }
    }

    #[test]
    fn verified_negative_origin_and_strict_containment() {
        let (mut w, fake, id) = bound();
        let r = w.place(id, local(), |_| Ok(monitor())).unwrap();
        assert!(r.verified());
        assert_eq!(r.requested_global.x, -780.0);
        assert_eq!(r.requested_global.y, -170.0);
        assert_eq!(fake.0.borrow().writes, 2);
        for b in [
            Bounds { x: -0.1, ..local() },
            Bounds { y: -0.1, ..local() },
            Bounds {
                x: 501.0,
                ..local()
            },
            Bounds {
                height: 601.0,
                ..local()
            },
            Bounds {
                x: f64::NAN,
                ..local()
            },
            Bounds {
                width: 0.0,
                ..local()
            },
            Bounds {
                width: f64::INFINITY,
                ..local()
            },
        ] {
            assert!(w.place(id, b, |_| Ok(monitor())).is_err());
        }
        assert_eq!(fake.0.borrow().writes, 2);
    }
    #[test]
    fn permissions_spi_and_ambiguity_fail_without_retention_or_writes() {
        for change in [0, 1, 2] {
            let fake = Fake::default();
            let mut w = Windows::new(fake.clone());
            let candidate = w.candidates(123).unwrap().remove(0).candidate;
            {
                let mut m = fake.0.borrow_mut();
                match change {
                    0 => m.permission = false,
                    1 => m.spi = false,
                    _ => m.ambiguous = true,
                }
            }
            assert!(w
                .bind(1, identity(), &candidate, |_| Ok(monitor()))
                .is_err());
            assert!(w.candidates(123).is_err());
            assert_eq!(fake.0.borrow().retained, 0);
            assert_eq!(fake.0.borrow().writes, 0);
        }
    }
    #[test]
    fn dead_reused_pid_replaced_window_and_revoked_permission_refuse() {
        for change in 0..5 {
            let (mut w, fake, id) = bound();
            {
                let mut m = fake.0.borrow_mut();
                match change {
                    0 => m.live = false,
                    1 => m.birth += 1,
                    2 => m.object += 1,
                    3 => m.permission = false,
                    _ => m.ambiguous = true,
                }
            }
            assert!(w.place(id, local(), |_| Ok(monitor())).is_err());
            assert_eq!(fake.0.borrow().writes, 0);
            w.unbind(id).unwrap();
            assert_eq!(fake.0.borrow().retained, 0);
        }
    }
    #[test]
    fn stale_monitor_and_changed_geometry_refuse_and_drop_failed_bind() {
        let (mut w, fake, id) = bound();
        assert!(w
            .place(id, local(), |_| Err("destroyed monitor".into()))
            .is_err());
        assert!(w
            .place(id, local(), |_| Ok(Bounds {
                x: -900.0,
                ..monitor()
            }))
            .is_err());
        assert_eq!(fake.0.borrow().writes, 0);
        fake.0.borrow_mut().list_count = 2;
        let candidates = w.candidates(123).unwrap();
        let mut calls = 0;
        let mut other = identity();
        other.window_id += 1;
        assert!(w
            .bind(1, other, &candidates[1].candidate, |_| {
                calls += 1;
                Ok(Bounds {
                    x: -800.0 + f64::from(calls),
                    ..monitor()
                })
            })
            .is_err());
        assert_eq!(fake.0.borrow().retained, 2);
        fake.0.borrow_mut().permission = false;
        assert!(w.candidates(123).is_err()); // releases the remaining inventory
        w.destroy_monitor(1);
        assert_eq!(fake.0.borrow().retained, 0);
        assert!(w.place(id, local(), |_| Ok(monitor())).is_err());
    }
    #[test]
    fn dispatch_success_is_not_verified_readback_and_setter_errors_are_partial() {
        for mode in 0..5 {
            let (mut w, fake, id) = bound();
            {
                let mut m = fake.0.borrow_mut();
                match mode {
                    0 => m.ignore_size = true,
                    1 => m.fail_size = true,
                    2 => m.focus_after_write = true,
                    3 => m.replace_after_write = true,
                    _ => (),
                }
            }
            let mut calls = 0;
            let r = w
                .place(id, local(), |_| {
                    calls += 1;
                    if mode == 4 && calls >= 3 {
                        Err("monitor disappeared after write".into())
                    } else {
                        Ok(monitor())
                    }
                })
                .unwrap();
            assert!(!r.verified());
            assert!(r.detail.is_some());
            assert!(r.writes_attempted >= 1);
            if mode == 2 {
                assert_eq!(r.focus_interference, Some(true));
                assert_eq!(r.writes_attempted, 1);
            }
            if mode == 4 {
                assert_eq!(r.focus_interference, None);
            }
            assert_eq!(fake.0.borrow().writes, r.writes_attempted as usize);
        }
    }
    #[test]
    fn tolerance_never_expands_monitor_containment() {
        let (mut w, fake, id) = bound();
        fake.0.borrow_mut().cg_delta = 0.5;
        assert!(w.place(id, local(), |_| Ok(monitor())).unwrap().verified());
        let edge = Bounds {
            x: 500.0,
            ..local()
        };
        assert!(!w.place(id, edge, |_| Ok(monitor())).unwrap().verified());
        fake.0.borrow_mut().cg_delta = 1.01;
        assert!(w.place(id, local(), |_| Ok(monitor())).is_err());
    }
    #[test]
    fn retention_capacity_duplicate_unbind_monitor_destroy_and_eof_are_bounded() {
        let fake = Fake::default();
        let mut w = Windows::new(fake.clone());
        fake.0.borrow_mut().list_count = MAX_CANDIDATES;
        let candidates = w.candidates(123).unwrap();
        let mut ids = Vec::new();
        for (n, candidate) in candidates.iter().enumerate().take(MAX_BINDINGS) {
            let mut i = identity();
            i.window_id += n as u32;
            ids.push(
                w.bind(
                    if n % 2 == 0 { 1 } else { 2 },
                    i,
                    &candidate.candidate,
                    |_| Ok(monitor()),
                )
                .unwrap(),
            );
        }
        let refreshed = w.candidates(123).unwrap();
        assert!(w
            .bind(1, refreshed[0].identity, &refreshed[0].candidate, |_| Ok(
                monitor()
            ))
            .unwrap_err()
            .contains("capacity"));
        assert_eq!(fake.0.borrow().retained, MAX_BINDINGS + MAX_CANDIDATES);
        w.unbind(ids[0]).unwrap();
        assert!(w.unbind(ids[0]).is_err());
        assert!(w
            .bind(1, refreshed[1].identity, &refreshed[1].candidate, |_| Ok(
                monitor()
            ))
            .unwrap_err()
            .contains("already bound"));
        fake.0.borrow_mut().permission = false;
        assert!(w.candidates(123).is_err());
        w.destroy_monitor(1);
        assert_eq!(fake.0.borrow().retained, MAX_BINDINGS / 2);
        drop(w);
        assert_eq!(fake.0.borrow().retained, 0);
        assert_eq!(fake.0.borrow().writes, 0);
        assert!(time_left(Instant::now() - Duration::from_millis(1)).is_err());
    }
}

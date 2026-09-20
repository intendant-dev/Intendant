//! One-use window-local pointer plans. No native calls or ambient authority here.
//! A posted pair is never a verified application effect. The helper serializes
//! the pair and retains its source; cancellation never retries or rolls it back.
use super::placement::{self, Binding, Bounds, Observation};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub(crate) const TOKEN_TTL_MS: u64 = 10_000;
const SOURCE_HOLD: Duration = Duration::from_secs(4);
const MAX_SOURCES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Point {
    pub x: f64,
    pub y: f64,
}
impl Point {
    pub(crate) fn validate(self) -> Result<(), String> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || self.x < 0.0
            || self.y < 0.0
            || self.x >= 16384.0
            || self.y >= 16384.0
        {
            return Err("invalid window-local logical point".into());
        }
        Ok(())
    }
    fn global(self, bounds: Bounds) -> Result<Self, String> {
        self.validate()?;
        bounds.validate()?;
        if self.x >= bounds.width || self.y >= bounds.height {
            return Err("point must be strictly inside the retained window".into());
        }
        Ok(Self {
            x: bounds.x + self.x,
            y: bounds.y + self.y,
        })
    }
    fn inside(self, bounds: Bounds) -> bool {
        bounds.validate().is_ok()
            && self.x.is_finite()
            && self.y.is_finite()
            && self.x >= bounds.x
            && self.y >= bounds.y
            && self.x < bounds.x + bounds.width
            && self.y < bounds.y + bounds.height
    }
}

pub(crate) fn validate_token(token: &str) -> Result<(), String> {
    if !token.strip_prefix("macos_pointer:").is_some_and(|v| {
        v.len() == 32
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err("invalid opaque pointer preparation token".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Prepared {
    pub token: String,
    pub point: Point,
    pub global: Point,
    pub window: Observation,
    pub expires_in_ms: u64,
}
impl Prepared {
    pub(super) fn valid_reply(&self) -> bool {
        validate_token(&self.token).is_ok()
            && self.expires_in_ms == TOKEN_TTL_MS
            && self.window.ax.close(self.window.cg)
            && self
                .point
                .global(self.window.cg)
                .is_ok_and(|p| p == self.global)
            && self.global.inside(self.window.ax)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClickStatus {
    Dispatched,
    Partial,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClickResult {
    pub status: ClickStatus,
    pub action_attempted: bool,
    pub posting_calls: u8,
    pub effects_unconfirmed: bool,
    pub effect_verified: bool,
    pub point: Point,
    pub global: Point,
    pub before: Observation,
    pub after: Option<Observation>,
    pub focus_interference: Option<bool>,
    pub detail: Option<String>,
}
impl ClickResult {
    pub(crate) fn successful(&self) -> bool {
        self.status == ClickStatus::Dispatched
    }
    pub(super) fn valid_reply(&self) -> bool {
        self.posting_calls <= 2
            && !self.effect_verified
            && self.action_attempted == (self.posting_calls > 0)
            && self.effects_unconfirmed == self.action_attempted
            && self.before.ax.close(self.before.cg)
            && self
                .point
                .global(self.before.cg)
                .is_ok_and(|p| p == self.global)
            && self.global.inside(self.before.ax)
            && self.detail.as_ref().is_none_or(|s| s.len() <= 2048)
            && self
                .after
                .is_none_or(|o| o.ax.validate().is_ok() && o.cg.validate().is_ok())
            && (!self.successful()
                || (self.posting_calls == 2
                    && self.detail.is_none()
                    && self.focus_interference == Some(false)
                    && self.after.is_some_and(|o| same(o, self.before))))
    }
}

/// A fully constructed pair, confined to the helper thread. post() is one-shot;
/// its implementation must not perform a fallible operation between the halves.
/// Drop releases its native source/events, never sends corrective input.
pub(crate) trait Pair {
    fn post(&mut self) -> Posting;
}
pub(crate) struct Posting {
    pub calls: u8,
    pub detail: Option<String>,
}
#[cfg(any(target_os = "macos", test))]
impl Posting {
    pub(crate) fn from_native(calls: u8, failed: bool) -> Self {
        let detail = if calls == 0 {
            Some("native pointer preflight refused; zero posting calls".into())
        } else if failed {
            Some("native pointer posting failed; attempted calls retained; do not retry".into())
        } else if calls != 2 {
            Some("native pointer pair partially posted; do not retry".into())
        } else {
            None
        };
        Self { calls, detail }
    }
}

struct Snapshot<F> {
    binding: u32,
    prepared: Prepared,
    focus: F,
    created: Instant,
}
pub(super) struct Inventory<F> {
    pending: Option<Snapshot<F>>,
    // Source lifetime is bounded in count. Idle helpers retain sources until the
    // next operation/EOF; expiration is not evidence of delivery or application.
    sources: Vec<(Instant, Box<dyn Pair>)>,
}
impl<F> Default for Inventory<F> {
    fn default() -> Self {
        Self {
            pending: None,
            sources: Vec::new(),
        }
    }
}
impl<F> Inventory<F> {
    pub(super) fn clear(&mut self) {
        self.pending = None;
    }
    pub(super) fn invalidate(&mut self, id: u32) {
        if self.pending.as_ref().is_some_and(|p| p.binding == id) {
            self.clear();
        }
    }
    fn capacity(&mut self) -> Result<(), String> {
        self.sources.retain(|(t, _)| t.elapsed() < SOURCE_HOLD);
        if self.sources.len() >= MAX_SOURCES {
            return Err("pointer source retention busy; no events dispatched".into());
        }
        Ok(())
    }
}
fn same(a: Observation, b: Observation) -> bool {
    a.ax == b.ax && a.cg == b.cg
}
fn check<N: placement::Native>(
    native: &mut N,
    b: &Binding<N::Window>,
    geometry: &mut impl FnMut(u32) -> Result<Bounds, String>,
    deadline: Instant,
) -> Result<Observation, String> {
    placement::time_left(deadline)?;
    if geometry(b.monitor)? != b.geometry {
        return Err("owned monitor geometry changed; rebind explicitly".into());
    }
    let observed = native.observe(&b.window, b.identity, deadline)?;
    if !observed.ax.close(observed.cg)
        || !b.geometry.contains(observed.ax)
        || !b.geometry.contains(observed.cg)
    {
        return Err("entire retained window must remain inside its unchanged owned monitor".into());
    }
    native.pointer_ready(&b.window, deadline)?;
    placement::time_left(deadline)?;
    Ok(observed)
}
impl<N: super::controls::Native> placement::Windows<N> {
    pub(super) fn prepare_click(
        &mut self,
        id: u32,
        point: Point,
        mut geometry: impl FnMut(u32) -> Result<Bounds, String>,
    ) -> Result<Prepared, String> {
        self.pointers.clear();
        self.elements.clear();
        self.pointers.capacity()?;
        point.validate()?;
        let b = self.bindings.get(&id).ok_or("stale window binding")?;
        let deadline = Instant::now() + placement::BUDGET;
        let before = check(&mut self.native, b, &mut geometry, deadline)?;
        let global = point.global(before.cg)?;
        if !global.inside(before.ax) || !global.inside(b.geometry) {
            return Err("pointer not contained in exact AX/CG window and monitor".into());
        }
        let focus = self.native.focus(deadline)?;
        let after = check(&mut self.native, b, &mut geometry, deadline)?;
        if !same(before, after) || self.native.focus(deadline)? != focus {
            return Err("window or observed focus changed during pointer preparation".into());
        }
        placement::time_left(deadline)?;
        let prepared = Prepared {
            token: format!("macos_pointer:{}", uuid::Uuid::new_v4().simple()),
            point,
            global,
            window: before,
            expires_in_ms: TOKEN_TTL_MS,
        };
        self.pointers.pending = Some(Snapshot {
            binding: id,
            prepared: prepared.clone(),
            focus,
            created: Instant::now(),
        });
        Ok(prepared)
    }
    pub(super) fn click(
        &mut self,
        id: u32,
        token: &str,
        mut geometry: impl FnMut(u32) -> Result<Bounds, String>,
    ) -> Result<ClickResult, String> {
        // Consume before validation: stale/foreign/expired attempts cannot leave
        // a usable token. No raw coordinates or substitute window at dispatch.
        let pending = self.pointers.pending.take();
        self.elements.clear();
        validate_token(token)?;
        let s = pending.ok_or("stale or consumed pointer preparation")?;
        if s.binding != id || s.prepared.token != token {
            return Err("pointer token belongs to a different preparation/binding".into());
        }
        if s.created.elapsed() >= Duration::from_millis(TOKEN_TTL_MS) {
            return Err("pointer preparation expired; prepare again explicitly".into());
        }
        self.pointers.capacity()?;
        let b = self.bindings.get(&id).ok_or("stale window binding")?;
        let deadline = Instant::now() + placement::BUDGET;
        let before = check(&mut self.native, b, &mut geometry, deadline)?;
        if !same(before, s.prepared.window) || self.native.focus(deadline)? != s.focus {
            return Err("prepared pointer window or focus changed; no dispatch".into());
        }
        let mut pair =
            self.native
                .pointer_pair(&b.window, s.prepared.point, s.prepared.global, deadline)?;
        // Construction may call native APIs; revalidate the retained identity,
        // geometry, native authority and focus again immediately before posting.
        let final_before = check(&mut self.native, b, &mut geometry, deadline)?;
        if !same(before, final_before) || self.native.focus(deadline)? != s.focus {
            return Err("pointer state changed during construction; no dispatch".into());
        }
        if s.created.elapsed() >= Duration::from_millis(TOKEN_TTL_MS) {
            return Err("pointer preparation expired before dispatch".into());
        }
        placement::time_left(deadline)?;
        let posting = pair.post();
        // Retain even partial/uncertain pairs. Never retry or synthesize an up on
        // another target. The concrete pair performs its two calls adjacently.
        self.pointers.sources.push((Instant::now(), pair));
        let mut result = ClickResult {
            status: ClickStatus::Partial,
            action_attempted: posting.calls > 0,
            posting_calls: posting.calls,
            effects_unconfirmed: posting.calls > 0,
            effect_verified: false,
            point: s.prepared.point,
            global: s.prepared.global,
            before,
            after: None,
            focus_interference: None,
            detail: posting.detail,
        };
        // Observations are independent of dispatch readiness. A foreground
        // transition, newly held human button or revoked permission must not hide
        // already available geometry/focus after a possible native effect.
        let mut errors = Vec::new();
        match self.native.observe(&b.window, b.identity, deadline) {
            Ok(after) => {
                result.after = Some(after);
                if !after.ax.close(after.cg)
                    || !b.geometry.contains(after.ax)
                    || !b.geometry.contains(after.cg)
                    || !same(before, after)
                {
                    errors.push("window changed during pointer dispatch".to_string());
                }
            }
            Err(error) => errors.push(error),
        }
        match self.native.focus(deadline) {
            Ok(focus) => {
                let changed = focus != s.focus;
                result.focus_interference = Some(changed);
                if changed {
                    errors.push("observed focus changed during pointer dispatch".into());
                }
            }
            Err(error) => errors.push(error),
        }
        match geometry(b.monitor) {
            Ok(bounds) if bounds == b.geometry => {}
            Ok(_) => errors.push("owned monitor geometry changed during pointer dispatch".into()),
            Err(error) => errors.push(error),
        }
        if let Err(error) = self.native.pointer_ready(&b.window, deadline) {
            errors.push(error);
        }
        if let Err(error) = placement::time_left(deadline) {
            errors.push(error);
        }
        if !errors.is_empty() {
            let error = errors.join("; ");
            result.detail = Some(match result.detail.take() {
                Some(previous) => format!("{previous}; postcheck: {error}"),
                None => error,
            });
        }
        if result.posting_calls != 2 && result.detail.is_none() {
            result.detail = Some("pointer pair was not fully posted; no retry attempted".into());
        }
        if result.posting_calls == 2 && result.detail.is_none() {
            result.status = ClickStatus::Dispatched;
        }
        Ok(result)
    }
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareMacosWindowClickParams {
    /// Exact binding returned by bind_macos_window, not a PID or display ID.
    pub binding: String,
    /// Window-local logical points, top-left origin (not normalized/pixel scale).
    pub point: Point,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClickMacosWindowParams {
    pub binding: String,
    /// One-use token from prepare_macos_window_click; expires after 10 seconds.
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::super::placement::{
        tests::{identity, monitor},
        ListedWindow, Native, WindowIdentity, Windows,
    };
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    struct State {
        bounds: Bounds,
        permitted: bool,
        focus: u8,
        object: u64,
        calls: u8,
        posts: usize,
        construction_change: bool,
        post_change: bool,
        post_failed: bool,
        readiness_fails_after_post: bool,
        move_after_post: bool,
        dropped: usize,
    }
    #[derive(Clone)]
    struct Fake(Rc<RefCell<State>>);
    struct FakePair(Fake);
    impl Drop for FakePair {
        fn drop(&mut self) {
            self.0 .0.borrow_mut().dropped += 1;
        }
    }
    impl Pair for FakePair {
        fn post(&mut self) -> Posting {
            let mut s = self.0 .0.borrow_mut();
            s.posts += 1;
            if s.post_change {
                s.focus += 1;
            }
            if s.move_after_post {
                s.bounds.x += 1.0;
            }
            Posting::from_native(s.calls, s.post_failed)
        }
    }
    impl Native for Fake {
        type Window = u64;
        type Focus = u8;
        fn candidates(&mut self, _: i32, _: Instant) -> Result<Vec<ListedWindow<u64>>, String> {
            Ok(vec![ListedWindow {
                identity: identity(),
                bounds: self.0.borrow().bounds,
                window: 1,
            }])
        }
        fn observe(
            &mut self,
            w: &u64,
            _: WindowIdentity,
            _: Instant,
        ) -> Result<Observation, String> {
            let s = self.0.borrow();
            if !s.permitted || *w != s.object {
                return Err("identity/permission changed".into());
            }
            Ok(Observation {
                ax: s.bounds,
                cg: s.bounds,
            })
        }
        fn focus(&mut self, _: Instant) -> Result<u8, String> {
            Ok(self.0.borrow().focus)
        }
        fn position(&mut self, _: &u64, _: Bounds, _: Instant) -> Result<(), String> {
            Ok(())
        }
        fn size(&mut self, _: &u64, _: Bounds, _: Instant) -> Result<(), String> {
            Ok(())
        }
        fn pointer_ready(&mut self, _: &u64, _: Instant) -> Result<(), String> {
            let s = self.0.borrow();
            if s.permitted && !(s.posts > 0 && s.readiness_fails_after_post) {
                Ok(())
            } else {
                Err("native readiness unavailable".into())
            }
        }
        fn pointer_pair(
            &mut self,
            _: &u64,
            _: Point,
            _: Point,
            _: Instant,
        ) -> Result<Box<dyn Pair>, String> {
            if self.0.borrow().construction_change {
                self.0.borrow_mut().focus += 1;
            }
            Ok(Box::new(FakePair(self.clone())))
        }
    }
    impl super::super::controls::Native for Fake {
        type Element = ();
    }
    fn rig() -> (Windows<Fake>, Fake, u32) {
        let f = Fake(Rc::new(RefCell::new(State {
            bounds: Bounds {
                x: -750.,
                y: -150.,
                width: 300.,
                height: 200.,
            },
            permitted: true,
            focus: 1,
            object: 1,
            calls: 2,
            posts: 0,
            construction_change: false,
            post_change: false,
            post_failed: false,
            readiness_fails_after_post: false,
            move_after_post: false,
            dropped: 0,
        })));
        let mut w = Windows::new(f.clone());
        let c = w.candidates(123).unwrap().remove(0);
        let id = w
            .bind(1, identity(), &c.candidate, |_| Ok(monitor()))
            .unwrap();
        (w, f, id)
    }
    fn prepare(w: &mut Windows<Fake>, id: u32) -> Prepared {
        w.prepare_click(id, Point { x: 37.25, y: 123.5 }, |_| Ok(monitor()))
            .unwrap()
    }
    #[test]
    fn exact_one_use_click_is_dispatched_not_effect_verified() {
        let (mut w, f, id) = rig();
        let p = prepare(&mut w, id);
        assert!(p.valid_reply());
        assert_eq!(
            p.global,
            Point {
                x: -712.75,
                y: -26.5
            }
        );
        let r = w.click(id, &p.token, |_| Ok(monitor())).unwrap();
        assert!(r.successful() && r.valid_reply());
        assert!(r.effects_unconfirmed && !r.effect_verified);
        assert_eq!(r.posting_calls, 2);
        assert!(w.click(id, &p.token, |_| Ok(monitor())).is_err());
        assert_eq!(f.0.borrow().posts, 1);
        assert_eq!(f.0.borrow().dropped, 0);
        drop(w);
        assert_eq!(f.0.borrow().dropped, 1);
    }
    #[test]
    fn refresh_foreign_and_expired_tokens_cannot_dispatch() {
        for mode in 0..5 {
            let (mut w, f, id) = rig();
            let p = prepare(&mut w, id);
            match mode {
                0 => {
                    prepare(&mut w, id);
                }
                1 => {
                    w.pointers.pending.as_mut().unwrap().created =
                        Instant::now() - Duration::from_secs(11);
                }
                2 => {
                    w.pointers.pending.as_mut().unwrap().binding = id + 1;
                }
                3 => {
                    w.pointers
                        .pending
                        .as_mut()
                        .unwrap()
                        .prepared
                        .token
                        .push('a');
                }
                _ => {
                    w.pointers.clear();
                }
            }
            assert!(w.click(id, &p.token, |_| Ok(monitor())).is_err());
            assert_eq!(f.0.borrow().posts, 0);
            assert!(w.pointers.pending.is_none());
        }
    }
    #[test]
    fn pointer_geometry_is_finite_half_open_and_never_clamped() {
        for point in [
            Point { x: -1., y: 0. },
            Point { x: f64::NAN, y: 0. },
            Point {
                x: 0.,
                y: f64::INFINITY,
            },
            Point { x: 300., y: 10. },
            Point { x: 10., y: 200. },
        ] {
            let (mut w, f, id) = rig();
            assert!(w.prepare_click(id, point, |_| Ok(monitor())).is_err());
            assert_eq!(f.0.borrow().posts, 0);
        }
    }
    #[test]
    fn native_state_changes_consume_before_any_post() {
        for mode in 0..5 {
            let (mut w, f, id) = rig();
            let p = prepare(&mut w, id);
            {
                let mut s = f.0.borrow_mut();
                match mode {
                    0 => s.focus += 1,
                    1 => s.bounds.x += 0.25,
                    2 => s.object += 1,
                    3 => s.permitted = false,
                    _ => s.construction_change = true,
                }
            }
            assert!(w.click(id, &p.token, |_| Ok(monitor())).is_err());
            assert!(w.pointers.pending.is_none());
            assert_eq!(f.0.borrow().posts, 0);
        }
    }
    #[test]
    fn moved_or_destroyed_monitor_never_falls_back() {
        let (mut w, f, id) = rig();
        let p = prepare(&mut w, id);
        assert!(w
            .click(id, &p.token, |_| {
                let mut m = monitor();
                m.x += 1.;
                Ok(m)
            })
            .is_err());
        let p = prepare(&mut w, id);
        w.destroy_monitor(1);
        assert!(w.click(id, &p.token, |_| Ok(monitor())).is_err());
        assert_eq!(f.0.borrow().posts, 0);
    }
    #[test]
    fn unbind_placement_and_semantic_refresh_invalidate_pointer_preparations() {
        for mode in 0..3 {
            let (mut w, f, id) = rig();
            let p = prepare(&mut w, id);
            match mode {
                0 => {
                    w.unbind(id).unwrap();
                }
                1 => {
                    let _ = w.place(
                        id,
                        Bounds {
                            x: 50.,
                            y: 50.,
                            width: 300.,
                            height: 200.,
                        },
                        |_| Ok(monitor()),
                    );
                }
                _ => {
                    let _ = w.read_elements(id, |_| Ok(monitor()));
                }
            }
            assert!(w.click(id, &p.token, |_| Ok(monitor())).is_err());
            assert_eq!(f.0.borrow().posts, 0);
        }
    }
    #[test]
    fn partial_pairs_and_post_focus_changes_preserve_evidence_without_retry() {
        for calls in [0, 1, 2] {
            for changed in [false, true] {
                let (mut w, f, id) = rig();
                let p = prepare(&mut w, id);
                {
                    let mut s = f.0.borrow_mut();
                    s.calls = calls;
                    s.post_change = changed;
                }
                let r = w.click(id, &p.token, |_| Ok(monitor())).unwrap();
                assert!(r.valid_reply(), "{r:?}");
                assert_eq!(r.successful(), calls == 2 && !changed);
                assert_eq!(r.posting_calls, calls);
                assert_eq!(r.action_attempted, calls > 0);
                assert!(!r.effect_verified);
                assert_eq!(f.0.borrow().posts, 1);
                assert!(w.click(id, &p.token, |_| Ok(monitor())).is_err());
            }
        }
    }
    #[test]
    fn sources_have_a_bounded_retention_lifetime_without_dropping_live_pairs() {
        let (mut w, f, id) = rig();
        for _ in 0..MAX_SOURCES {
            let p = prepare(&mut w, id);
            w.click(id, &p.token, |_| Ok(monitor())).unwrap();
        }
        assert!(w
            .prepare_click(id, Point { x: 1., y: 1. }, |_| Ok(monitor()))
            .is_err());
        assert_eq!(f.0.borrow().dropped, 0);
        for (t, _) in &mut w.pointers.sources {
            *t = Instant::now() - SOURCE_HOLD;
        }
        prepare(&mut w, id);
        assert_eq!(f.0.borrow().dropped, MAX_SOURCES);
    }
    #[test]
    fn strict_params_and_reply_cannot_claim_verified_effects() {
        for v in [
            serde_json::json!({"binding":"x","point":{"x":1,"y":2},"activate":true}),
            serde_json::json!({"binding":"x","point":{"x":1,"y":2,"scale":2}}),
        ] {
            assert!(serde_json::from_value::<PrepareMacosWindowClickParams>(v).is_err());
        }
        for v in [
            serde_json::json!({"binding":"x","token":"x","x":2}),
            serde_json::json!({"token":"x"}),
        ] {
            assert!(serde_json::from_value::<ClickMacosWindowParams>(v).is_err());
        }
        let (mut w, _, id) = rig();
        let p = prepare(&mut w, id);
        let mut r = w.click(id, &p.token, |_| Ok(monitor())).unwrap();
        r.effect_verified = true;
        assert!(!r.valid_reply());
        r.effect_verified = false;
        r.posting_calls = 1;
        assert!(!r.valid_reply());
    }
    #[test]
    fn native_second_call_failure_cannot_be_reported_dispatched() {
        for calls in [0, 1, 2] {
            for failed in [false, true] {
                let p = Posting::from_native(calls, failed);
                assert_eq!(p.detail.is_none(), calls == 2 && !failed);
            }
        }
        let (mut w, f, id) = rig();
        let p = prepare(&mut w, id);
        f.0.borrow_mut().post_failed = true;
        let r = w.click(id, &p.token, |_| Ok(monitor())).unwrap();
        assert_eq!(r.posting_calls, 2);
        assert!(r.action_attempted && r.effects_unconfirmed);
        assert!(!r.successful());
        assert!(r.valid_reply());
        assert!(r.detail.unwrap().contains("posting failed"));
        assert_eq!(f.0.borrow().posts, 1);
    }
    #[test]
    fn post_readiness_failure_preserves_independent_geometry_and_focus() {
        for changed in [false, true] {
            for moved in [false, true] {
                let (mut w, f, id) = rig();
                let p = prepare(&mut w, id);
                {
                    let mut s = f.0.borrow_mut();
                    s.readiness_fails_after_post = true;
                    s.post_change = changed;
                    s.move_after_post = moved;
                }
                let r = w.click(id, &p.token, |_| Ok(monitor())).unwrap();
                assert!(!r.successful());
                assert!(r.valid_reply());
                assert_eq!(r.focus_interference, Some(changed));
                assert_eq!(r.after.unwrap().cg, f.0.borrow().bounds);
                assert!(r.detail.unwrap().contains("readiness"));
                assert_eq!(r.posting_calls, 2);
            }
        }
    }
}

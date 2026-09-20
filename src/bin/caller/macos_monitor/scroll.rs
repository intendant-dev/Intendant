//! Scroll-only wire vocabulary. Transactions, focus and source retention live in
//! pointer.rs; these separate strict receipts preserve the existing click schema.
use super::{
    placement::Observation,
    pointer::{ClickResult, ClickStatus, Point, Prepared},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub(crate) fn validate_delta(delta_y: i32) -> Result<(), String> {
    // Range comparison deliberately avoids abs()/negation overflow at i32::MIN.
    if delta_y == 0 || !(-600..=600).contains(&delta_y) {
        return Err("delta_y must be a nonzero signed integer in -600..=600 logical pixel scroll request units (positive down)".into());
    }
    Ok(())
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareMacosWindowScrollParams {
    /// Exact retained owned-monitor window binding, never a PID or display ID.
    pub binding: String,
    /// Window-local logical points, top-left origin.
    pub point: Point,
    /// Signed logical pixel scroll request units: positive down; nonzero, abs <= 600.
    #[schemars(range(min = -600, max = 600), extend("not" = {"const": 0}))]
    pub delta_y: i32,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScrollMacosWindowParams {
    pub binding: String,
    /// One-use token from prepare_macos_window_scroll; expires after 10 seconds.
    pub token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedScroll {
    pub delta_y: i32,
    pub token: String,
    pub point: Point,
    pub global: Point,
    pub window: Observation,
    pub expires_in_ms: u64,
}
impl PreparedScroll {
    pub(super) fn new(p: Prepared, delta_y: i32) -> Self {
        Self {
            delta_y,
            token: p.token,
            point: p.point,
            global: p.global,
            window: p.window,
            expires_in_ms: p.expires_in_ms,
        }
    }
    pub(super) fn valid_reply(&self) -> bool {
        validate_delta(self.delta_y).is_ok()
            && Prepared {
                token: self.token.clone(),
                point: self.point,
                global: self.global,
                window: self.window,
                expires_in_ms: self.expires_in_ms,
            }
            .valid_reply()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScrollResult {
    pub delta_y: i32,
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
impl ScrollResult {
    pub(super) fn new(r: ClickResult, delta_y: i32) -> Self {
        Self {
            delta_y,
            status: r.status,
            action_attempted: r.action_attempted,
            posting_calls: r.posting_calls,
            effects_unconfirmed: r.effects_unconfirmed,
            effect_verified: r.effect_verified,
            point: r.point,
            global: r.global,
            before: r.before,
            after: r.after,
            focus_interference: r.focus_interference,
            detail: r.detail,
        }
    }
    pub(crate) fn successful(&self) -> bool {
        self.status == ClickStatus::Dispatched
    }
    pub(super) fn valid_reply(&self) -> bool {
        validate_delta(self.delta_y).is_ok()
            && ClickResult {
                status: self.status,
                action_attempted: self.action_attempted,
                posting_calls: self.posting_calls,
                effects_unconfirmed: self.effects_unconfirmed,
                effect_verified: self.effect_verified,
                point: self.point,
                global: self.global,
                before: self.before,
                after: self.after,
                focus_interference: self.focus_interference,
                detail: self.detail.clone(),
            }
            .valid_for(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_scroll_params_reject_types_overflow_and_unbounded_values() {
        for delta in [
            json!(1.0),
            json!("1"),
            json!(true),
            json!(null),
            json!([]),
            json!(2147483648_i64),
            json!(-2147483649_i64),
            json!(u64::MAX),
        ] {
            assert!(
                serde_json::from_value::<PrepareMacosWindowScrollParams>(json!({
                    "binding":"b","point":{"x":1,"y":2},"delta_y":delta
                }))
                .is_err()
            );
        }
        for delta_y in [i32::MIN, -601, 0, 601, i32::MAX] {
            let p: PrepareMacosWindowScrollParams = serde_json::from_value(json!({
                "binding":"b","point":{"x":1,"y":2},"delta_y":delta_y
            }))
            .unwrap();
            assert!(validate_delta(p.delta_y).is_err());
        }
        for delta_y in [-600, -1, 1, 600] {
            assert!(validate_delta(delta_y).is_ok());
        }
        for input in [
            json!({"binding":"b","point":{"x":1,"y":2}}),
            json!({"binding":"b","point":{"x":1,"y":2},"delta_y":1,"delta_x":0}),
            json!({"binding":"b","point":{"x":1,"y":2,"scale":1},"delta_y":1}),
            json!({"binding":"b","point":{"x":1,"y":2},"delta_y":1,"momentum":false}),
        ] {
            assert!(serde_json::from_value::<PrepareMacosWindowScrollParams>(input).is_err());
        }
        for field in [
            "point",
            "delta_y",
            "delta_x",
            "pid",
            "window_id",
            "activate",
            "retry",
        ] {
            let mut input = json!({"binding":"b","token":"t"});
            input[field] = json!(1);
            assert!(serde_json::from_value::<ScrollMacosWindowParams>(input).is_err());
        }
    }

    fn dispatched() -> ScrollResult {
        let bounds = super::super::placement::tests::monitor();
        ScrollResult::new(
            ClickResult {
                status: ClickStatus::Dispatched,
                action_attempted: true,
                posting_calls: 1,
                effects_unconfirmed: true,
                effect_verified: false,
                point: Point { x: 37.25, y: 123.5 },
                global: Point {
                    x: bounds.x + 37.25,
                    y: bounds.y + 123.5,
                },
                before: Observation {
                    ax: bounds,
                    cg: bounds,
                },
                after: Some(Observation {
                    ax: bounds,
                    cg: bounds,
                }),
                focus_interference: Some(false),
                detail: None,
            },
            73,
        )
    }
    #[test]
    fn scroll_receipts_reject_lying_success_counts_and_unknown_fields() {
        let good = dispatched();
        assert!(good.valid_reply());
        for patch in [
            json!({"posting_calls":0}),
            json!({"posting_calls":2}),
            json!({"posting_calls":255,"status":"partial"}),
            json!({"effect_verified":true}),
            json!({"action_attempted":false}),
            json!({"effects_unconfirmed":false}),
            json!({"after":null}),
            json!({"focus_interference":null}),
            json!({"focus_interference":true}),
            json!({"detail":"native exception"}),
            json!({"delta_y":0}),
            json!({"delta_y":i32::MIN}),
            json!({"global":{"x":0,"y":0}}),
        ] {
            let mut wire = serde_json::to_value(&good).unwrap();
            wire.as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            assert!(
                !serde_json::from_value::<ScrollResult>(wire)
                    .unwrap()
                    .valid_reply(),
                "{patch}"
            );
        }
        let mut moved = good.clone();
        moved.after.as_mut().unwrap().cg.x += 0.25;
        assert!(!moved.valid_reply());
        for calls in [0, 1] {
            let mut partial = good.clone();
            partial.status = ClickStatus::Partial;
            partial.posting_calls = calls;
            partial.action_attempted = calls > 0;
            partial.effects_unconfirmed = calls > 0;
            partial.detail = Some("native exception; no retry".into());
            assert!(partial.valid_reply());
        }
        let mut wire = serde_json::to_value(good).unwrap();
        wire["unexpected"] = true.into();
        assert!(serde_json::from_value::<ScrollResult>(wire).is_err());
    }
}

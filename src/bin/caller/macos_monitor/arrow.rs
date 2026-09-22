//! Single-use, exact-receiver ArrowRight. No text/chords or implicit focus click.
use super::{
    controls,
    keyboard::{KeyboardTarget, ReceiverSnapshot},
    placement::{self, Bounds, Observation},
    pointer::ClickStatus,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
pub(crate) const TTL_MS: u64 = 10_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Key {
    ArrowRight,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receiver {
    pub role: String,
    pub bounds: Bounds,
    pub enabled: bool,
}
impl Receiver {
    fn from_target(t: &KeyboardTarget) -> Result<Self, String> {
        let r = Self {
            role: t.role.clone(),
            bounds: t.bounds,
            enabled: t.enabled,
        };
        if !r.valid() {
            return Err(
                "ArrowRight requires an enabled nonprotected AXTextField or AXTextArea receiver"
                    .into(),
            );
        }
        Ok(r)
    }
    fn valid(&self) -> bool {
        self.enabled
            && matches!(self.role.as_str(), "AXTextField" | "AXTextArea")
            && self.bounds.validate().is_ok()
    }
}
pub(crate) fn validate_token(t: &str) -> Result<(), String> {
    if !t.strip_prefix("macos_key:").is_some_and(|v| {
        v.len() == 32
            && v.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    }) {
        return Err("invalid opaque ArrowRight token".into());
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Prepared {
    pub token: String,
    pub key: Key,
    pub receiver: Receiver,
    pub window: Observation,
    pub expires_in_ms: u64,
}
impl Prepared {
    pub(super) fn valid_reply(&self) -> bool {
        validate_token(&self.token).is_ok()
            && self.expires_in_ms == TTL_MS
            && self.receiver.valid()
            && self.window.ax.close(self.window.cg)
            && self.window.ax.contains(self.receiver.bounds)
            && self.window.cg.contains(self.receiver.bounds)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArrowResult {
    pub key: Key,
    pub status: ClickStatus,
    pub posting_calls: u8,
    pub action_attempted: bool,
    pub effects_unconfirmed: bool,
    pub effect_verified: bool,
    pub receiver: Receiver,
    pub after_receiver: Option<Receiver>,
    pub receiver_unchanged: Option<bool>,
    pub before: Observation,
    pub after: Option<Observation>,
    pub focus_interference: Option<bool>,
    pub detail: Option<String>,
}
impl ArrowResult {
    pub(crate) fn successful(&self) -> bool {
        self.status == ClickStatus::Dispatched
    }
    pub(super) fn valid_reply(&self) -> bool {
        self.posting_calls <= 2
            && !self.effect_verified
            && self.action_attempted == (self.posting_calls > 0)
            && self.effects_unconfirmed == self.action_attempted
            && self.receiver.valid()
            && self.before.ax.close(self.before.cg)
            && self.before.ax.contains(self.receiver.bounds)
            && self.before.cg.contains(self.receiver.bounds)
            && self
                .after
                .is_none_or(|o| o.ax.validate().is_ok() && o.cg.validate().is_ok())
            && self.after_receiver.as_ref().is_none_or(Receiver::valid)
            && self
                .detail
                .as_ref()
                .is_none_or(|s| !s.is_empty() && s.len() <= 2048)
            && (self.receiver_unchanged != Some(true)
                || self.after_receiver.as_ref() == Some(&self.receiver))
            && (!self.successful()
                || (self.posting_calls == 2
                    && self.detail.is_none()
                    && self.focus_interference == Some(false)
                    && self.receiver_unchanged == Some(true)
                    && self
                        .after
                        .is_some_and(|o| o.ax == self.before.ax && o.cg == self.before.cg)))
    }
}
struct Pending<E, F> {
    binding: u32,
    prepared: Prepared,
    snapshot: ReceiverSnapshot<E, F>,
    created: Instant,
}
pub(super) struct Inventory<E, F>(Option<Pending<E, F>>);
impl<E, F> Default for Inventory<E, F> {
    fn default() -> Self {
        Self(None)
    }
}
impl<E, F> Inventory<E, F> {
    #[cfg(test)]
    pub(super) fn expire_for_test(&mut self) {
        self.0.as_mut().unwrap().created = Instant::now() - Duration::from_millis(TTL_MS);
    }
    pub(super) fn clear(&mut self) {
        self.0 = None;
    }
    pub(super) fn invalidate(&mut self, id: u32) {
        if self.0.as_ref().is_some_and(|s| s.binding == id) {
            self.clear();
        }
    }
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareMacosWindowArrowrightParams {
    pub binding: String,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PressMacosWindowArrowrightParams {
    pub binding: String,
    pub token: String,
}

impl<N: controls::Native> placement::Windows<N> {
    pub(super) fn prepare_arrow(
        &mut self,
        id: u32,
        mut geometry: impl FnMut(u32) -> Result<Bounds, String>,
    ) -> Result<Prepared, String> {
        self.arrows.clear();
        self.pointers.clear();
        self.elements.clear();
        self.pointers.capacity()?;
        let deadline = Instant::now() + placement::BUDGET;
        let b = self.bindings.get(&id).ok_or("stale window binding")?;
        self.native.arrow_ready(&b.window, deadline)?;
        let snapshot = self.keyboard_snapshot(id, &mut geometry, deadline, None)?;
        let receiver = Receiver::from_target(&snapshot.target)?;
        let b = self.bindings.get(&id).ok_or("stale window binding")?;
        self.native.arrow_ready(&b.window, deadline)?;
        placement::time_left(deadline)?;
        let prepared = Prepared {
            token: format!("macos_key:{}", uuid::Uuid::new_v4().simple()),
            key: Key::ArrowRight,
            receiver,
            window: snapshot.window,
            expires_in_ms: TTL_MS,
        };
        if !prepared.valid_reply() {
            return Err("invalid ArrowRight preparation".into());
        }
        self.arrows.0 = Some(Pending {
            binding: id,
            prepared: prepared.clone(),
            snapshot,
            created: Instant::now(),
        });
        Ok(prepared)
    }
    pub(super) fn press_arrow(
        &mut self,
        id: u32,
        token: &str,
        mut geometry: impl FnMut(u32) -> Result<Bounds, String>,
    ) -> Result<ArrowResult, String> {
        // Consume before validation. Ingress refusals cannot mutate helper state.
        let pending = self.arrows.0.take();
        self.elements.clear();
        self.pointers.clear();
        validate_token(token)?;
        let pending = pending.ok_or("stale or consumed ArrowRight preparation")?;
        if pending.binding != id || pending.prepared.token != token {
            return Err("ArrowRight token belongs to a different binding/preparation".into());
        }
        if pending.created.elapsed() >= Duration::from_millis(TTL_MS) {
            return Err("ArrowRight preparation expired".into());
        }
        self.pointers.capacity()?;
        let deadline = Instant::now() + placement::BUDGET;
        self.keyboard_snapshot(id, &mut geometry, deadline, Some(&pending.snapshot))?;
        let b = self.bindings.get(&id).ok_or("stale window binding")?;
        self.native.arrow_ready(&b.window, deadline)?;
        let mut pair = self.native.arrow_pair(&b.window, deadline)?;
        // Construction does not post. Revalidate the exact receiver/path again.
        self.keyboard_snapshot(id, &mut geometry, deadline, Some(&pending.snapshot))?;
        let b = self.bindings.get(&id).ok_or("stale window binding")?;
        self.native.arrow_ready(&b.window, deadline)?;
        if pending.created.elapsed() >= Duration::from_millis(TTL_MS) {
            return Err("ArrowRight preparation expired before dispatch".into());
        }
        placement::time_left(deadline)?;
        let posting = pair.post();
        self.pointers.hold(pair); // hold partial/uncertain sources; no corrective input
        let mut result = ArrowResult {
            key: Key::ArrowRight,
            status: ClickStatus::Partial,
            posting_calls: posting.calls,
            action_attempted: posting.calls > 0,
            effects_unconfirmed: posting.calls > 0,
            effect_verified: false,
            receiver: pending.prepared.receiver,
            after_receiver: None,
            receiver_unchanged: None,
            before: pending.snapshot.window,
            after: None,
            focus_interference: None,
            detail: posting.detail,
        };
        let mut errors = Vec::new();
        // Preserve independently available evidence after possible native effects.
        match self.native.observe(&b.window, b.identity, deadline) {
            Ok(o) => {
                result.after = Some(o);
                if o.ax != result.before.ax || o.cg != result.before.cg {
                    errors.push("window changed during ArrowRight".to_string());
                }
            }
            Err(e) => errors.push(e),
        }
        match self.native.keyboard_human_focus(deadline) {
            Ok(f) => {
                let changed = f != pending.snapshot.focus;
                result.focus_interference = Some(changed);
                if changed {
                    errors.push("human focus changed during ArrowRight".into());
                }
            }
            Err(e) => errors.push(e),
        }
        if let Err(e) = self.native.arrow_ready(&b.window, deadline) {
            errors.push(e);
        }
        match self.keyboard_snapshot(id, &mut geometry, deadline, Some(&pending.snapshot)) {
            Ok(s) => match Receiver::from_target(&s.target) {
                Ok(receiver) => {
                    result.after_receiver = Some(receiver);
                    result.receiver_unchanged = Some(true);
                }
                Err(error) => errors.push(error),
            },
            Err(e) => errors.push(e),
        }
        if let Err(e) = placement::time_left(deadline) {
            errors.push(e);
        }
        if posting.calls != 2 && result.detail.is_none() {
            errors.push("ArrowRight pair was not fully posted; no retry attempted".into());
        }
        if let Some(e) = result.detail.take() {
            errors.insert(0, e);
        }
        if !errors.is_empty() {
            result.detail = Some(errors.join(";").chars().take(512).collect());
        }
        if result.posting_calls == 2 && result.detail.is_none() {
            result.status = ClickStatus::Dispatched;
        }
        Ok(result)
    }
}

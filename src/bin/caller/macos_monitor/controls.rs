//! Bounded semantic controls on the exact retained window. This engine has no
//! native dependencies; AX objects and their ancestry stay on the helper thread.
use super::placement::{self, Bounds, Observation, WindowIdentity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Instant;

pub(crate) const MAX_NODES: usize = 128;
// Chromium 153 fixture reaches depth 13 with only 51 nodes. Keep all other
// budgets unchanged; depth remains finite and exact ancestry remains retained.
pub(crate) const MAX_DEPTH: usize = 16;
pub(crate) const MAX_CHILDREN: usize = 32;
pub(crate) const MAX_CONTROLS: usize = 16;
pub(crate) const MAX_LABEL: usize = 128;
pub(crate) const MAX_TEXT: usize = 1024;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ElementAction {
    // Struct variant makes deny_unknown_fields reject extra press arguments.
    Press {},
    SetValue { text: String },
}
impl ElementAction {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if matches!(self, Self::SetValue { text } if text.len() > MAX_TEXT) {
            return Err("text exceeds 1024 UTF-8 bytes".into());
        }
        Ok(())
    }
    fn operation(&self) -> SupportedOperation {
        match self {
            Self::Press {} => SupportedOperation::Press,
            Self::SetValue { .. } => SupportedOperation::SetValue,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SupportedOperation {
    Press,
    SetValue,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadMacosWindowElementsParams {
    pub binding: String,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActMacosWindowElementParams {
    pub binding: String,
    pub element: String,
    pub action: ElementAction,
}

pub(crate) fn validate_token(token: &str) -> Result<(), String> {
    if !token.strip_prefix("macos_element:").is_some_and(|s| {
        s.len() == 32
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err("invalid opaque window element token".into());
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Control {
    pub element: String,
    pub role: String,
    pub label: String,
    pub bounds: Bounds,
    pub operations: Vec<SupportedOperation>,
}
impl Control {
    pub(super) fn valid_reply(&self) -> bool {
        validate_token(&self.element).is_ok()
            && self.label.len() <= MAX_LABEL
            && self.bounds.validate().is_ok()
            && match self.role.as_str() {
                "AXButton" | "AXCheckBox" | "AXRadioButton" => {
                    self.operations == [SupportedOperation::Press]
                }
                "AXTextField" | "AXTextArea" => self.operations == [SupportedOperation::SetValue],
                _ => false,
            }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionStatus {
    Verified,
    Dispatched,
    Partial,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionResult {
    pub status: ActionStatus,
    pub operation: SupportedOperation,
    pub action_attempted: bool,
    pub before: Bounds,
    pub after: Option<Bounds>,
    pub value_matches: Option<bool>,
    pub focus_interference: Option<bool>,
    pub effects_unconfirmed: bool,
    pub detail: Option<String>,
}
impl ActionResult {
    pub(crate) fn successful(&self) -> bool {
        matches!(
            self.status,
            ActionStatus::Verified | ActionStatus::Dispatched
        )
    }
    pub(super) fn valid_reply(&self) -> bool {
        self.before.validate().is_ok()
            && self.after.is_none_or(|b| b.validate().is_ok())
            && self.detail.as_ref().is_none_or(|s| s.len() <= 2048)
            && match self.status {
                ActionStatus::Verified => {
                    self.action_attempted
                        && self.operation == SupportedOperation::SetValue
                        && self.value_matches == Some(true)
                        && self.focus_interference == Some(false)
                        && !self.effects_unconfirmed
                        && self.detail.is_none()
                        && self.after == Some(self.before)
                }
                ActionStatus::Dispatched => {
                    self.action_attempted
                        && self.operation == SupportedOperation::Press
                        && self.value_matches.is_none()
                        && self.focus_interference == Some(false)
                        && self.effects_unconfirmed
                        && self.detail.is_none()
                        && self.after == Some(self.before)
                }
                ActionStatus::Partial => self.effects_unconfirmed == self.action_attempted,
            }
    }
}

/// Safety is read BEFORE labels, children or values, even on a later readback.
pub(crate) struct Safety {
    pub role: String,
    pub secure: bool,
}
#[derive(Clone)]
pub(crate) struct Metadata {
    pub label: String,
    pub bounds: Bounds,
    pub enabled: bool,
    pub press: bool,
    pub value_settable: bool,
}
/// Implementations may not resolve replacements by title, identifier or path.
/// No Send/Sync bound: Elements can be !Send AX wrappers retained on main.
pub(crate) trait Native: placement::Native {
    type Element: Clone;
    fn root(&mut self, _window: &Self::Window) -> Result<Self::Element, String> {
        Err("semantic AX controls unavailable".into())
    }
    fn equal(&self, _a: &Self::Element, _b: &Self::Element) -> bool {
        false
    }
    fn safety(&mut self, _element: &Self::Element, _deadline: Instant) -> Result<Safety, String> {
        Err("semantic AX controls unavailable".into())
    }
    /// Request cap+1 children; overflow is an error, never silently truncate.
    fn children(
        &mut self,
        _element: &Self::Element,
        _deadline: Instant,
    ) -> Result<Vec<Self::Element>, String> {
        Err("semantic AX controls unavailable".into())
    }
    fn metadata(
        &mut self,
        _element: &Self::Element,
        _role: &str,
        _deadline: Instant,
    ) -> Result<Metadata, String> {
        Err("semantic AX controls unavailable".into())
    }
    /// Recheck AXWindow, each exact AXParent link and reciprocal bounded current
    /// AXChildren membership, including the window root. Never select replacements.
    fn member(
        &mut self,
        _window: &Self::Window,
        _path: &[Self::Element],
        _deadline: Instant,
    ) -> Result<(), String> {
        Err("semantic AX controls unavailable".into())
    }
    fn press(&mut self, _element: &Self::Element, _deadline: Instant) -> Result<(), String> {
        Err("semantic AX controls unavailable".into())
    }
    fn set_value(
        &mut self,
        _element: &Self::Element,
        _text: &str,
        _deadline: Instant,
    ) -> Result<(), String> {
        Err("semantic AX controls unavailable".into())
    }
    /// Exact bounded comparison only; the native value never leaves the adapter.
    fn value_matches(
        &mut self,
        _element: &Self::Element,
        _text: &str,
        _deadline: Instant,
    ) -> Result<bool, String> {
        Err("semantic AX controls unavailable".into())
    }
}
struct Retained<E> {
    control: Control,
    path: Vec<E>,
}
struct Snapshot<E, F> {
    binding: u32,
    window: Observation,
    focus: F,
    controls: Vec<Retained<E>>,
}
pub(super) struct Inventory<E, F>(Option<Snapshot<E, F>>);
impl<E, F> Default for Inventory<E, F> {
    fn default() -> Self {
        Self(None)
    }
}
impl<E, F> Inventory<E, F> {
    pub(super) fn clear(&mut self) {
        self.0 = None;
    }
    pub(super) fn invalidate(&mut self, binding: u32) {
        if self.0.as_ref().is_some_and(|s| s.binding == binding) {
            self.clear();
        }
    }
}

fn operations(s: &Safety, m: &Metadata) -> Vec<SupportedOperation> {
    if s.secure || !m.enabled {
        return vec![];
    }
    match s.role.as_str() {
        "AXButton" | "AXCheckBox" | "AXRadioButton" if m.press => vec![SupportedOperation::Press],
        "AXTextField" | "AXTextArea" if m.value_settable => vec![SupportedOperation::SetValue],
        _ => vec![],
    }
}
fn check_window<N: Native>(
    native: &mut N,
    window: &N::Window,
    identity: WindowIdentity,
    monitor: Bounds,
    geometry: &mut impl FnMut() -> Result<Bounds, String>,
    deadline: Instant,
) -> Result<Observation, String> {
    placement::time_left(deadline)?;
    if geometry()? != monitor {
        return Err("owned monitor geometry changed; rebind explicitly".into());
    }
    let o = native.observe(window, identity, deadline)?;
    if !o.ax.close(o.cg) || !monitor.contains(o.ax) || !monitor.contains(o.cg) {
        return Err("entire AX/CG window rectangle must lie inside retained owned monitor".into());
    }
    placement::time_left(deadline)?;
    Ok(o)
}
fn same_window(a: Observation, b: Observation) -> Result<(), String> {
    if a.ax != b.ax || a.cg != b.cg {
        return Err("window moved since element snapshot".into());
    }
    Ok(())
}
/// DFS rooted ONLY at the retained window. Paths retain exact ancestors; this
/// also bounds retained references to 16 * (depth + 1), not a second inventory.
fn walk<N: Native>(
    native: &mut N,
    window: &N::Window,
    bounds: Observation,
    deadline: Instant,
) -> Result<Vec<Retained<N::Element>>, String> {
    let mut stack = vec![vec![native.root(window)?]];
    let mut visited = Vec::new();
    let mut controls = Vec::new();
    while let Some(path) = stack.pop() {
        placement::time_left(deadline)?;
        let element = path.last().expect("rooted path");
        if visited.len() == MAX_NODES || visited.iter().any(|e| native.equal(e, element)) {
            return Err("AX traversal node limit or cycle".into());
        }
        visited.push(element.clone());
        let safety = native.safety(element, deadline)?;
        if safety.secure {
            continue;
        } // Omit the whole secure subtree, including labels.
          // Only known actionable roles need metadata; do not read labels of
          // document/group/static-text nodes, nor any AXValue during discovery.
        if path.len() > 1
            && matches!(
                safety.role.as_str(),
                "AXButton" | "AXCheckBox" | "AXRadioButton" | "AXTextField" | "AXTextArea"
            )
        {
            let m = native.metadata(element, &safety.role, deadline)?;
            let ops = operations(&safety, &m);
            if !ops.is_empty() {
                if controls.len() == MAX_CONTROLS {
                    return Err("actionable control capacity exceeded".into());
                }
                if m.label.len() > MAX_LABEL
                    || !bounds.ax.contains(m.bounds)
                    || !bounds.cg.contains(m.bounds)
                {
                    return Err("control label or containment limit".into());
                }
                native.member(window, &path, deadline)?;
                controls.push(Retained {
                    control: Control {
                        element: format!("macos_element:{}", uuid::Uuid::new_v4().simple()),
                        role: safety.role,
                        label: m.label,
                        bounds: m.bounds,
                        operations: ops,
                    },
                    path: path.clone(),
                });
            }
        }
        let children = native.children(element, deadline)?;
        if children.len() > MAX_CHILDREN
            || (path.len() > MAX_DEPTH && !children.is_empty())
            || visited.len() + stack.len() + children.len() > MAX_NODES
        {
            return Err("AX traversal children/depth/node limit exceeded".into());
        }
        for child in children.into_iter().rev() {
            let mut next = path.clone();
            next.push(child);
            stack.push(next);
        }
    }
    Ok(controls)
}

impl<E: Clone, F: PartialEq> Inventory<E, F> {
    #[allow(clippy::too_many_arguments)] // Explicit retained identity and geometry, no ambient context.
    pub(super) fn read<N: Native<Element = E, Focus = F>>(
        &mut self,
        native: &mut N,
        binding: u32,
        window: &N::Window,
        identity: WindowIdentity,
        monitor: Bounds,
        mut geometry: impl FnMut() -> Result<Bounds, String>,
    ) -> Result<Vec<Control>, String> {
        self.clear(); // Failure is a refresh too, across ALL bindings.
        let deadline = Instant::now() + placement::BUDGET;
        let before = check_window(native, window, identity, monitor, &mut geometry, deadline)?;
        let focus = native.focus(deadline)?;
        let controls = walk(native, window, before, deadline)?;
        same_window(
            before,
            check_window(native, window, identity, monitor, &mut geometry, deadline)?,
        )?;
        if native.focus(deadline)? != focus {
            return Err("focus changed during element snapshot".into());
        }
        placement::time_left(deadline)?;
        let public: Vec<_> = controls.iter().map(|c| c.control.clone()).collect();
        // Check the actual worst-sequence reply envelope BEFORE publishing any
        // tokens. The unchanged 16 KiB wire limit rejects, never truncates.
        super::protocol::encode(&super::protocol::Reply {
            seq: u64::MAX,
            result: super::protocol::Outcome::WindowElements {
                controls: public.clone(),
            },
        })?;
        placement::time_left(deadline)?;
        self.0 = Some(Snapshot {
            binding,
            window: before,
            focus,
            controls,
        });
        Ok(public)
    }
    #[allow(clippy::too_many_arguments)] // Both selectors plus explicit retained identity and geometry.
    pub(super) fn act<N: Native<Element = E, Focus = F>>(
        &mut self,
        native: &mut N,
        binding: u32,
        token: &str,
        action: &ElementAction,
        window: &N::Window,
        identity: WindowIdentity,
        monitor: Bounds,
        mut geometry: impl FnMut() -> Result<Bounds, String>,
    ) -> Result<ActionResult, String> {
        // Even a failed/foreign action refresh replaces the inventory. Take it
        // BEFORE any native attempt; no receipt/delivery path can restore it.
        let snapshot = self.0.take().ok_or("stale element inventory; read again")?;
        action.validate()?;
        validate_token(token)?;
        if snapshot.binding != binding {
            return Err("foreign element binding".into());
        }
        let retained = snapshot
            .controls
            .iter()
            .find(|c| c.control.element == token)
            .ok_or("stale or foreign element token")?;
        let deadline = Instant::now() + placement::BUDGET;
        let recheck = |native: &mut N,
                       geometry: &mut _,
                       focus_interference: &mut Option<bool>,
                       after: &mut Option<Bounds>|
         -> Result<Bounds, String> {
            *focus_interference = None;
            let current = check_window(native, window, identity, monitor, geometry, deadline)?;
            same_window(snapshot.window, current)?;
            // Validate the retained chain by exact parent/window identity and
            // reciprocal bounded children, without selecting a replacement.
            native.member(window, &retained.path, deadline)?;
            let element = retained.path.last().expect("retained element");
            let safety = native.safety(element, deadline)?;
            if safety.secure || safety.role != retained.control.role {
                return Err("element secure state or role changed".into());
            }
            let metadata = native.metadata(element, &safety.role, deadline)?;
            *after = Some(metadata.bounds);
            if metadata.label != retained.control.label {
                return Err("element label changed; read a fresh inventory".into());
            }
            if metadata.bounds != retained.control.bounds
                || !current.ax.contains(metadata.bounds)
                || !current.cg.contains(metadata.bounds)
                || !monitor.contains(metadata.bounds)
                || !operations(&safety, &metadata).contains(&action.operation())
                || !retained.control.operations.contains(&action.operation())
            {
                return Err("element state or bounds changed".into());
            }
            same_window(
                snapshot.window,
                check_window(native, window, identity, monitor, geometry, deadline)?,
            )?;
            let changed = native.focus(deadline)? != snapshot.focus;
            *focus_interference = Some(changed);
            if changed {
                return Err("focus changed; no restoration attempted".into());
            }
            placement::time_left(deadline)?;
            Ok(metadata.bounds)
        };
        let element = retained.path.last().expect("retained element");
        let mut result = ActionResult {
            status: ActionStatus::Partial,
            operation: action.operation(),
            action_attempted: false,
            before: retained.control.bounds,
            after: None,
            value_matches: None,
            focus_interference: None,
            effects_unconfirmed: false,
            detail: None,
        };
        if let Err(error) = recheck(
            native,
            &mut geometry,
            &mut result.focus_interference,
            &mut result.after,
        ) {
            result.detail = Some(error);
            return Ok(result);
        }
        result.action_attempted = true;
        result.effects_unconfirmed = true;
        result.focus_interference = None;
        result.after = None;
        // A native call may apply before returning an error. Attempt exactly once.
        let write = match action {
            ElementAction::Press {} => native.press(element, deadline),
            ElementAction::SetValue { text } => native.set_value(element, text, deadline),
        };
        let post = (|| {
            recheck(
                native,
                &mut geometry,
                &mut result.focus_interference,
                &mut result.after,
            )?;
            if let ElementAction::SetValue { text } = action {
                let readback = native.value_matches(element, text, deadline);
                if let Ok(matches) = &readback {
                    result.value_matches = Some(*matches);
                }
                // Recheck even after failed readback, preserving any available
                // focus/geometry evidence without reading or writing again.
                let rechecked = recheck(
                    native,
                    &mut geometry,
                    &mut result.focus_interference,
                    &mut result.after,
                );
                readback?;
                rechecked?;
                if result.value_matches != Some(true) {
                    return Err("exact bounded value readback mismatch".into());
                }
            }
            Ok(())
        })();
        result.detail = match (write, post) {
            (Err(write), Err(post)) => Some(format!("{write}; postcheck: {post}")),
            (Err(error), _) | (_, Err(error)) => Some(error),
            (Ok(()), Ok(())) => None,
        };
        if result.detail.is_some() {
            return Ok(result);
        }
        result.status = match action {
            ElementAction::Press {} => ActionStatus::Dispatched,
            ElementAction::SetValue { .. } => ActionStatus::Verified,
        };
        result.effects_unconfirmed = matches!(action, ElementAction::Press {});
        Ok(result)
    }
}

impl<N: Native> placement::Windows<N> {
    pub(super) fn read_elements(
        &mut self,
        id: u32,
        mut geometry: impl FnMut(u32) -> Result<Bounds, String>,
    ) -> Result<Vec<Control>, String> {
        self.elements.clear();
        let b = self.bindings.get(&id).ok_or("stale window binding")?;
        self.elements.read(
            &mut self.native,
            id,
            &b.window,
            b.identity,
            b.geometry,
            || geometry(b.monitor),
        )
    }
    pub(super) fn act_element(
        &mut self,
        id: u32,
        token: &str,
        action: &ElementAction,
        mut geometry: impl FnMut(u32) -> Result<Bounds, String>,
    ) -> Result<ActionResult, String> {
        let Some(b) = self.bindings.get(&id) else {
            self.elements.clear();
            return Err("stale window binding".into());
        };
        self.elements.act(
            &mut self.native,
            id,
            token,
            action,
            &b.window,
            b.identity,
            b.geometry,
            || geometry(b.monitor),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::placement::{
        tests::{identity, monitor},
        ListedWindow, Windows,
    };
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

    #[derive(Clone)]
    struct Node {
        parent: usize,
        children: Vec<usize>,
        role: String,
        secure: bool,
        meta: Metadata,
        value: String,
    }
    impl Node {
        fn new(role: &str) -> Self {
            Self {
                parent: 0,
                children: vec![],
                role: role.into(),
                secure: false,
                meta: Metadata {
                    label: "fixture".into(),
                    bounds: Bounds {
                        x: -760.0,
                        y: -160.0,
                        width: 40.0,
                        height: 20.0,
                    },
                    enabled: true,
                    press: role == "AXButton",
                    value_settable: role == "AXTextField",
                },
                value: String::new(),
            }
        }
    }
    struct Model {
        nodes: BTreeMap<usize, Node>,
        window: Bounds,
        object: u64,
        birth: u64,
        permission: bool,
        focus: u8,
        writes: usize,
        reads: usize,
        forbidden_reads: usize,
        fail_attr: bool,
        fail_write: bool,
        after_write: &'static str,
        timeout: bool,
    }
    #[derive(Clone)]
    struct Fake(Rc<RefCell<Model>>);
    impl Default for Fake {
        fn default() -> Self {
            let mut root = Node::new("AXWindow");
            root.children = vec![1, 2];
            Self(Rc::new(RefCell::new(Model {
                nodes: [
                    (0, root),
                    (1, Node::new("AXTextField")),
                    (2, Node::new("AXButton")),
                ]
                .into(),
                window: monitor(),
                object: 1,
                birth: identity().start_seconds,
                permission: true,
                focus: 1,
                writes: 0,
                reads: 0,
                forbidden_reads: 0,
                fail_attr: false,
                fail_write: false,
                after_write: "",
                timeout: false,
            })))
        }
    }
    impl Fake {
        fn check(&self, deadline: Instant) -> Result<(), String> {
            placement::time_left(deadline)?;
            let m = self.0.borrow();
            if !m.permission || m.timeout || m.fail_attr {
                return Err("required state unavailable".into());
            }
            Ok(())
        }
        fn write(&self, e: usize, text: Option<&str>, deadline: Instant) -> Result<(), String> {
            self.check(deadline)?;
            let mut m = self.0.borrow_mut();
            m.writes += 1;
            if let Some(text) = text {
                m.nodes.get_mut(&e).unwrap().value = text.into();
            }
            match m.after_write {
                "focus" => m.focus += 1,
                "permission" => m.permission = false,
                "timeout" => m.timeout = true,
                "secure" => m.nodes.get_mut(&e).unwrap().secure = true,
                "bounds" => m.nodes.get_mut(&e).unwrap().meta.bounds.x += 1.0,
                "label" => m.nodes.get_mut(&e).unwrap().meta.label = "changed".into(),
                "value" => m.nodes.get_mut(&e).unwrap().value = "different".into(),
                "oversize" => m.nodes.get_mut(&e).unwrap().value = "x".repeat(MAX_TEXT + 1),
                _ => {}
            }
            if m.fail_write {
                return Err("setter may have acted".into());
            }
            Ok(())
        }
    }
    impl placement::Native for Fake {
        type Window = u64;
        type Focus = u8;
        fn candidates(
            &mut self,
            _: i32,
            deadline: Instant,
        ) -> Result<Vec<ListedWindow<u64>>, String> {
            self.check(deadline)?;
            Ok(vec![ListedWindow {
                identity: identity(),
                bounds: self.0.borrow().window,
                window: self.0.borrow().object,
            }])
        }
        fn observe(
            &mut self,
            w: &u64,
            id: WindowIdentity,
            deadline: Instant,
        ) -> Result<Observation, String> {
            self.check(deadline)?;
            let m = self.0.borrow();
            if *w != m.object || id.start_seconds != m.birth {
                return Err("window/process replaced".into());
            }
            Ok(Observation {
                ax: m.window,
                cg: m.window,
            })
        }
        fn focus(&mut self, deadline: Instant) -> Result<u8, String> {
            self.check(deadline)?;
            Ok(self.0.borrow().focus)
        }
        fn position(&mut self, _: &u64, _: Bounds, _: Instant) -> Result<(), String> {
            Ok(())
        }
        fn size(&mut self, _: &u64, _: Bounds, _: Instant) -> Result<(), String> {
            Ok(())
        }
    }
    impl Native for Fake {
        type Element = usize;
        fn root(&mut self, _: &u64) -> Result<usize, String> {
            Ok(0)
        }
        fn equal(&self, a: &usize, b: &usize) -> bool {
            a == b
        }
        fn safety(&mut self, e: &usize, d: Instant) -> Result<Safety, String> {
            self.check(d)?;
            let m = self.0.borrow();
            let n = m.nodes.get(e).ok_or("element removed")?;
            Ok(Safety {
                role: n.role.clone(),
                secure: n.secure,
            })
        }
        fn children(&mut self, e: &usize, d: Instant) -> Result<Vec<usize>, String> {
            self.check(d)?;
            let mut m = self.0.borrow_mut();
            if m.nodes[e].secure {
                m.forbidden_reads += 1;
                return Err("secure subtree traversed".into());
            }
            Ok(m.nodes[e].children.clone())
        }
        fn metadata(&mut self, e: &usize, _: &str, d: Instant) -> Result<Metadata, String> {
            self.check(d)?;
            let mut m = self.0.borrow_mut();
            if m.nodes[e].secure {
                m.forbidden_reads += 1;
                return Err("secure metadata read".into());
            }
            Ok(m.nodes[e].meta.clone())
        }
        fn member(&mut self, _: &u64, path: &[usize], d: Instant) -> Result<(), String> {
            self.check(d)?;
            let m = self.0.borrow();
            for pair in path.windows(2) {
                let p = m.nodes.get(&pair[0]).ok_or("parent gone")?;
                let c = m.nodes.get(&pair[1]).ok_or("child gone")?;
                if p.secure || c.secure || c.parent != pair[0] || !p.children.contains(&pair[1]) {
                    return Err("ancestry changed".into());
                }
            }
            Ok(())
        }
        fn press(&mut self, e: &usize, d: Instant) -> Result<(), String> {
            self.write(*e, None, d)
        }
        fn set_value(&mut self, e: &usize, text: &str, d: Instant) -> Result<(), String> {
            self.write(*e, Some(text), d)
        }
        fn value_matches(&mut self, e: &usize, text: &str, d: Instant) -> Result<bool, String> {
            self.check(d)?;
            let mut m = self.0.borrow_mut();
            if m.nodes[e].secure {
                m.forbidden_reads += 1;
                return Err("secure value read".into());
            }
            m.reads += 1;
            if m.nodes[e].value.len() > MAX_TEXT {
                return Err("value too large".into());
            }
            Ok(m.nodes[e].value == text)
        }
    }
    fn rig() -> (Windows<Fake>, Fake, u32) {
        let fake = Fake::default();
        let mut w = Windows::new(fake.clone());
        let candidate = w.candidates(identity().pid).unwrap().remove(0);
        let binding = w
            .bind(1, identity(), &candidate.candidate, |_| Ok(monitor()))
            .unwrap();
        (w, fake, binding)
    }
    fn read(w: &mut Windows<Fake>, b: u32) -> Vec<Control> {
        w.read_elements(b, |_| Ok(monitor())).unwrap()
    }
    fn set() -> ElementAction {
        ElementAction::SetValue {
            text: "disposable π 😀".into(),
        }
    }
    fn act(
        w: &mut Windows<Fake>,
        b: u32,
        token: &str,
        action: &ElementAction,
    ) -> Result<ActionResult, String> {
        w.act_element(b, token, action, |_| Ok(monitor()))
    }
    fn refused(result: Result<ActionResult, String>) -> bool {
        match result {
            Err(_) => true,
            Ok(r) => !r.action_attempted && !r.successful() && !r.effects_unconfirmed,
        }
    }
    #[test]
    fn exact_text_verification_and_press_dispatch_are_one_use_and_content_free() {
        let (mut w, f, b) = rig();
        let controls = read(&mut w, b);
        assert_eq!(controls.len(), 2);
        assert_eq!(f.0.borrow().reads, 0);
        let result = act(&mut w, b, &controls[0].element, &set()).unwrap();
        assert_eq!(result.status, ActionStatus::Verified);
        assert_eq!(result.value_matches, Some(true));
        assert!(result.valid_reply());
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("disposable"));
        assert!(refused(act(&mut w, b, &controls[0].element, &set())));
        // An action invalidates the whole snapshot, not just the selected token.
        assert!(refused(act(
            &mut w,
            b,
            &controls[1].element,
            &ElementAction::Press {}
        )));
        let controls = read(&mut w, b);
        let result = act(&mut w, b, &controls[1].element, &ElementAction::Press {}).unwrap();
        assert_eq!(result.status, ActionStatus::Dispatched);
        assert!(result.effects_unconfirmed);
        assert!(result.valid_reply());
        assert!(refused(act(
            &mut w,
            b,
            &controls[1].element,
            &ElementAction::Press {}
        )));
        assert_eq!(f.0.borrow().writes, 2);
    }
    #[test]
    fn refresh_failure_foreign_token_and_binding_consume_inventory() {
        for kind in [
            "refresh",
            "failed_refresh",
            "foreign_token",
            "foreign_binding",
        ] {
            let (mut w, f, b) = rig();
            let controls = read(&mut w, b);
            match kind {
                "refresh" => {
                    read(&mut w, b);
                }
                "failed_refresh" => {
                    f.0.borrow_mut().permission = false;
                    assert!(w.read_elements(b, |_| Ok(monitor())).is_err());
                    f.0.borrow_mut().permission = true;
                }
                "foreign_token" => {
                    assert!(act(
                        &mut w,
                        b,
                        "macos_element:00000000000000000000000000000000",
                        &set()
                    )
                    .is_err());
                }
                _ => {
                    assert!(refused(act(&mut w, b + 1, &controls[0].element, &set())));
                }
            }
            assert!(
                refused(act(&mut w, b, &controls[0].element, &set())),
                "{kind}"
            );
            assert_eq!(f.0.borrow().writes, 0);
        }
    }
    #[test]
    fn revalidate_identity_ancestry_secure_disabled_permissions_focus_and_geometry() {
        for kind in [
            "pid",
            "window",
            "replace",
            "parent",
            "detach",
            "secure_parent",
            "secure",
            "disabled",
            "role",
            "settable",
            "off_monitor",
            "bounds",
            "permission",
            "focus",
            "missing",
        ] {
            let (mut w, f, b) = rig();
            let controls = read(&mut w, b);
            {
                let mut m = f.0.borrow_mut();
                match kind {
                    "pid" => m.birth += 1,
                    "window" => m.object += 1,
                    "replace" => {
                        let n = m.nodes.remove(&1).unwrap();
                        m.nodes.insert(3, n);
                        m.nodes.get_mut(&0).unwrap().children = vec![3, 2];
                    }
                    "parent" => m.nodes.get_mut(&1).unwrap().parent = 2,
                    "detach" => m.nodes.get_mut(&0).unwrap().children = vec![2],
                    "secure_parent" => m.nodes.get_mut(&0).unwrap().secure = true,
                    "secure" => m.nodes.get_mut(&1).unwrap().secure = true,
                    "disabled" => m.nodes.get_mut(&1).unwrap().meta.enabled = false,
                    "role" => m.nodes.get_mut(&1).unwrap().role = "AXButton".into(),
                    "settable" => m.nodes.get_mut(&1).unwrap().meta.value_settable = false,
                    "off_monitor" => m.window.x -= 0.01,
                    "bounds" => m.nodes.get_mut(&1).unwrap().meta.bounds.x += 0.01,
                    "permission" => m.permission = false,
                    "focus" => m.focus += 1,
                    _ => m.fail_attr = true,
                }
            }
            assert!(
                refused(act(&mut w, b, &controls[0].element, &set())),
                "{kind}"
            );
            assert_eq!(f.0.borrow().writes, 0);
            assert_eq!(f.0.borrow().forbidden_reads, 0);
            assert!(refused(act(&mut w, b, &controls[0].element, &set())));
        }
    }
    #[test]
    fn discovery_omits_secure_subtrees_disabled_and_unknown_controls_without_values() {
        let (mut w, f, b) = rig();
        {
            let mut m = f.0.borrow_mut();
            m.nodes.get_mut(&1).unwrap().secure = true;
            m.nodes.get_mut(&1).unwrap().children = vec![999];
            m.nodes.get_mut(&2).unwrap().meta.enabled = false;
        }
        assert!(read(&mut w, b).is_empty());
        assert_eq!(f.0.borrow().forbidden_reads, 0);
        assert_eq!(f.0.borrow().reads, 0);
        f.0.borrow_mut().nodes.get_mut(&2).unwrap().role = "AXLink".into();
        assert!(read(&mut w, b).is_empty());
    }
    #[test]
    fn changed_bounded_labels_require_fresh_inventory_before_press_or_set_value() {
        for action in [ElementAction::Press {}, set()] {
            for (before, after) in [
                ("Preview".to_string(), "Delete".to_string()),
                (String::new(), "Preview".to_string()),
                ("Preview".to_string(), String::new()),
                ("é".repeat(MAX_LABEL / 2), "è".repeat(MAX_LABEL / 2)),
            ] {
                let (mut w, f, b) = rig();
                let (node, index) = if matches!(action, ElementAction::Press {}) {
                    (2, 1)
                } else {
                    (1, 0)
                };
                f.0.borrow_mut().nodes.get_mut(&node).unwrap().meta.label = before;
                let snapshot = read(&mut w, b);
                f.0.borrow_mut().nodes.get_mut(&node).unwrap().meta.label = after.clone();
                let result = act(&mut w, b, &snapshot[index].element, &action).unwrap();
                assert_eq!(result.status, ActionStatus::Partial);
                assert!(!result.action_attempted && !result.effects_unconfirmed);
                assert!(result.detail.unwrap().contains("fresh inventory"));
                assert_eq!(f.0.borrow().writes, 0);
                assert_eq!(f.0.borrow().reads, 0);
                assert!(refused(act(&mut w, b, &snapshot[index].element, &action)));
                let fresh = read(&mut w, b);
                assert_eq!(fresh[index].label, after);
                assert!(act(&mut w, b, &fresh[index].element, &action)
                    .unwrap()
                    .successful());
                assert_eq!(f.0.borrow().writes, 1);
            }
        }
    }
    #[test]
    fn monitor_geometry_change_and_place_unbind_destroy_invalidate() {
        for kind in ["geometry", "place", "unbind", "destroy"] {
            let (mut w, f, b) = rig();
            let c = read(&mut w, b);
            match kind {
                "geometry" => {
                    let mut moved = monitor();
                    moved.x -= 1.0;
                    assert!(refused(
                        w.act_element(b, &c[0].element, &set(), |_| Ok(moved))
                    ));
                }
                "place" => {
                    let _ = w.place(
                        b,
                        Bounds {
                            x: -1.0,
                            y: 0.0,
                            width: 10.0,
                            height: 10.0,
                        },
                        |_| Ok(monitor()),
                    );
                }
                "unbind" => w.unbind(b).unwrap(),
                _ => w.destroy_monitor(1),
            }
            assert!(refused(act(&mut w, b, &c[0].element, &set())));
            assert_eq!(f.0.borrow().writes, 0);
        }
    }
    #[test]
    fn action_errors_keep_attempt_and_available_evidence_without_duplicate_writes() {
        for mode in [
            "focus",
            "permission",
            "timeout",
            "secure",
            "bounds",
            "value",
            "label",
            "oversize",
            "setter",
        ] {
            let (mut w, f, b) = rig();
            let c = read(&mut w, b);
            {
                let mut m = f.0.borrow_mut();
                m.after_write = mode;
                m.fail_write = mode == "setter";
            }
            let result = act(&mut w, b, &c[0].element, &set()).unwrap();
            assert_eq!(result.status, ActionStatus::Partial, "{mode}");
            assert!(result.action_attempted && result.effects_unconfirmed);
            if mode == "focus" {
                assert_eq!(result.focus_interference, Some(true));
            }
            if mode == "bounds" {
                assert!(result.after.is_some());
            }
            assert!(result.valid_reply());
            assert!(refused(act(&mut w, b, &c[0].element, &set())));
            assert_eq!(f.0.borrow().writes, 1);
            assert_eq!(f.0.borrow().forbidden_reads, 0);
        }
    }
    #[test]
    fn chromium_depth_and_exact_boundary_keep_retained_ancestry_guards() {
        for depth in [13, MAX_DEPTH, MAX_DEPTH + 1] {
            let (mut w, f, b) = rig();
            {
                let mut m = f.0.borrow_mut();
                m.nodes.clear();
                for id in 0..=depth {
                    let role = if id == 0 {
                        "AXWindow"
                    } else if id == depth {
                        "AXButton"
                    } else {
                        "AXGroup"
                    };
                    let mut node = Node::new(role);
                    node.parent = id.saturating_sub(1);
                    if id < depth {
                        node.children = vec![id + 1];
                    }
                    m.nodes.insert(id, node);
                }
            }
            let result = w.read_elements(b, |_| Ok(monitor()));
            if depth > MAX_DEPTH {
                assert!(result.is_err());
                assert_eq!(f.0.borrow().writes, 0);
                continue;
            }
            let c = result.unwrap();
            assert_eq!(c.len(), 1);
            let result = act(&mut w, b, &c[0].element, &ElementAction::Press {}).unwrap();
            assert_eq!(result.status, ActionStatus::Dispatched);
            assert_eq!(f.0.borrow().writes, 1);
            let c = read(&mut w, b);
            f.0.borrow_mut().nodes.get_mut(&(depth / 2)).unwrap().secure = true;
            assert!(refused(act(
                &mut w,
                b,
                &c[0].element,
                &ElementAction::Press {}
            )));
            assert_eq!(
                f.0.borrow().writes,
                1,
                "newly protected ancestor forbids another write"
            );
        }
    }

    #[test]
    fn traversal_label_text_and_wire_limits_refuse_without_partial_inventory() {
        for mode in ["children", "depth", "nodes", "controls", "label", "cycle"] {
            let (mut w, f, b) = rig();
            let old = read(&mut w, b);
            {
                let mut m = f.0.borrow_mut();
                match mode {
                    "children" => m.nodes.get_mut(&0).unwrap().children = vec![1; MAX_CHILDREN + 1],
                    "controls" => {
                        for id in 3..=18 {
                            m.nodes.insert(id, Node::new("AXButton"));
                            m.nodes.get_mut(&0).unwrap().children.push(id);
                        }
                    }
                    "label" => m.nodes.get_mut(&1).unwrap().meta.label = "x".repeat(MAX_LABEL + 1),
                    "cycle" => m.nodes.get_mut(&0).unwrap().children.push(0),
                    "depth" => {
                        m.nodes.get_mut(&0).unwrap().children = vec![1];
                        for id in 1..=MAX_DEPTH + 1 {
                            let mut n = Node::new("AXGroup");
                            n.parent = id - 1;
                            if id <= MAX_DEPTH {
                                n.children = vec![id + 1];
                            }
                            m.nodes.insert(id, n);
                        }
                    }
                    _ => {
                        m.nodes.get_mut(&0).unwrap().children = (3..=10).collect();
                        for id in 3..=10 {
                            let mut n = Node::new("AXGroup");
                            n.children = (id * 100..id * 100 + 32).collect();
                            for &child in &n.children {
                                m.nodes.insert(child, Node::new("AXGroup"));
                            }
                            m.nodes.insert(id, n);
                        }
                    }
                }
            }
            assert!(w.read_elements(b, |_| Ok(monitor())).is_err(), "{mode}");
            assert!(refused(act(&mut w, b, &old[0].element, &set())));
            assert_eq!(f.0.borrow().writes, 0);
        }
        assert!(ElementAction::SetValue {
            text: "é".repeat(512)
        }
        .validate()
        .is_ok());
        assert!(ElementAction::SetValue {
            text: "é".repeat(513)
        }
        .validate()
        .is_err());
        let (mut w, f, b) = rig();
        let c = read(&mut w, b);
        assert!(act(
            &mut w,
            b,
            &c[0].element,
            &ElementAction::SetValue {
                text: "x".repeat(MAX_TEXT + 1)
            }
        )
        .is_err());
        assert_eq!(f.0.borrow().writes, 0);
        let mut controls = c;
        controls[0].label = "\u{0001}".repeat(super::super::protocol::MAX_LINE);
        assert!(
            super::super::protocol::encode(&super::super::protocol::Reply {
                seq: 1,
                result: super::super::protocol::Outcome::WindowElements { controls }
            })
            .is_err()
        );
    }
    #[test]
    fn only_known_enabled_roles_advertising_exact_operation_are_admitted() {
        let (mut w, f, b) = rig();
        let c = read(&mut w, b);
        assert!(refused(act(
            &mut w,
            b,
            &c[0].element,
            &ElementAction::Press {}
        )));
        let c = read(&mut w, b);
        f.0.borrow_mut().nodes.get_mut(&2).unwrap().meta.press = false;
        assert!(refused(act(
            &mut w,
            b,
            &c[1].element,
            &ElementAction::Press {}
        )));
        assert_eq!(f.0.borrow().writes, 0);
        for role in ["AXButton", "AXCheckBox", "AXRadioButton"] {
            let mut m = f.0.borrow_mut();
            let n = m.nodes.get_mut(&2).unwrap();
            n.role = role.into();
            n.meta.press = true;
            drop(m);
            let c = read(&mut w, b);
            assert_eq!(
                act(&mut w, b, &c[1].element, &ElementAction::Press {})
                    .unwrap()
                    .status,
                ActionStatus::Dispatched
            );
        }
    }
    #[test]
    fn preflight_focus_failure_preserves_evidence_and_never_dispatches() {
        let (mut w, f, b) = rig();
        let c = read(&mut w, b);
        f.0.borrow_mut().focus += 1;
        let result = act(&mut w, b, &c[0].element, &set()).unwrap();
        assert_eq!(result.focus_interference, Some(true));
        assert!(!result.action_attempted && !result.effects_unconfirmed);
        assert!(result.after.is_some() && result.valid_reply());
        assert_eq!(f.0.borrow().writes, 0);
    }
    #[test]
    fn a_second_binding_replaces_the_single_inventory() {
        let f = Fake::default();
        let mut native = f.clone();
        let mut inventory = Inventory::default();
        let first = inventory
            .read(&mut native, 1, &1, identity(), monitor(), || Ok(monitor()))
            .unwrap();
        let second = inventory
            .read(&mut native, 2, &1, identity(), monitor(), || Ok(monitor()))
            .unwrap();
        assert_ne!(first[0].element, second[0].element);
        assert!(inventory
            .act(
                &mut native,
                1,
                &first[0].element,
                &set(),
                &1,
                identity(),
                monitor(),
                || Ok(monitor())
            )
            .is_err());
        assert!(inventory
            .act(
                &mut native,
                2,
                &second[0].element,
                &set(),
                &1,
                identity(),
                monitor(),
                || Ok(monitor())
            )
            .is_err());
        assert_eq!(f.0.borrow().writes, 0);
    }

    #[test]
    fn nonsecure_editable_text_area_accepts_empty_and_exact_multibyte_limit() {
        let (mut w, f, b) = rig();
        f.0.borrow_mut().nodes.get_mut(&1).unwrap().role = "AXTextArea".into();
        for text in [String::new(), "é".repeat(512)] {
            let c = read(&mut w, b);
            let result = act(&mut w, b, &c[0].element, &ElementAction::SetValue { text }).unwrap();
            assert!(result.valid_reply());
            assert_eq!(result.status, ActionStatus::Verified);
        }
        assert_eq!(f.0.borrow().writes, 2);
    }
    #[test]
    fn sixteen_controls_are_the_single_retention_limit() {
        let (mut w, f, b) = rig();
        {
            let mut m = f.0.borrow_mut();
            for id in 3..=16 {
                m.nodes.insert(id, Node::new("AXButton"));
                m.nodes.get_mut(&0).unwrap().children.push(id);
            }
        }
        assert_eq!(read(&mut w, b).len(), MAX_CONTROLS);
        assert_eq!(w.elements.0.as_ref().unwrap().controls.len(), MAX_CONTROLS);
        {
            let mut m = f.0.borrow_mut();
            m.nodes.insert(17, Node::new("AXButton"));
            m.nodes.get_mut(&0).unwrap().children.push(17);
        }
        assert!(w.read_elements(b, |_| Ok(monitor())).is_err());
        assert!(w.elements.0.is_none());
    }
}

//! Bounded read-only observation of an exact bound window's application-local
//! AX keyboard receiver. This is neither a keyboard capability nor a retained
//! authority: the helper retains no receiver object after producing the report.

use super::controls::{self, KeyboardMetadata, Native, Safety};
#[cfg(test)]
use super::placement::WindowIdentity;
use super::placement::{self, Bounds, Observation};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadMacosWindowKeyboardTargetParams {
    /// Exact retained owned-monitor window binding, never a PID, window ID or
    /// receiver token.
    pub binding: String,
}

/// Deliberately small public observation. In particular it never contains an
/// AX label, AXValue, native pointer, document/app metadata or a reusable
/// receiver/authority token.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyboardTarget {
    pub role: String,
    pub bounds: Bounds,
    pub enabled: bool,
    pub keyboard_dispatch_supported: bool,
}

impl KeyboardTarget {
    pub(super) fn valid_reply(&self) -> bool {
        !self.role.is_empty()
            && self.role != "AXWindow"
            && self.role.len() <= 64
            && self.bounds.validate().is_ok()
            && !self.keyboard_dispatch_supported
    }
}

fn locate<N: Native>(
    native: &mut N,
    window: &N::Window,
    receiver: &N::Element,
    deadline: Instant,
) -> Result<Vec<N::Element>, String> {
    let mut stack = vec![vec![native.root(window)?]];
    let mut visited = Vec::new();
    while let Some(path) = stack.pop() {
        placement::time_left(deadline)?;
        let depth = path.len() - 1;
        if depth > controls::MAX_DEPTH {
            return Err("AX keyboard receiver traversal depth limit exceeded".into());
        }
        let element = path.last().expect("rooted AX path");
        if visited.len() == controls::MAX_NODES
            || visited.iter().any(|seen| native.equal(seen, element))
        {
            return Err("AX keyboard receiver traversal node limit or cycle".into());
        }
        visited.push(element.clone());
        let safety = native.safety(element, deadline)?;
        if safety.secure {
            if native.equal(element, receiver) {
                return Err("focused keyboard receiver is protected".into());
            }
            continue;
        }
        if native.equal(element, receiver) {
            native.member(window, &path, deadline)?;
            return Ok(path);
        }
        let children = native.children(element, deadline).map_err(|e| {
            format!(
                "AX keyboard receiver traversal depth {}: {e}",
                path.len() - 1
            )
        })?;
        if children.len() > controls::MAX_CHILDREN
            || (depth >= controls::MAX_DEPTH && !children.is_empty())
            || visited.len() + stack.len() + children.len() > controls::MAX_NODES
        {
            return Err("AX keyboard receiver traversal children/depth/node limit exceeded".into());
        }
        for child in children.into_iter().rev() {
            let mut next = path.clone();
            next.push(child);
            stack.push(next);
        }
    }
    Err("application-local focused receiver is absent from retained window".into())
}

fn current_target<N: Native>(
    native: &mut N,
    window: &N::Window,
    path: &[N::Element],
    receiver: &N::Element,
    deadline: Instant,
) -> Result<(Safety, KeyboardMetadata), String> {
    if path.len() <= 1 {
        return Err("application-local focus is the retained window, not a receiver".into());
    }
    let element = path.last().ok_or("empty retained keyboard receiver path")?;
    if !native.equal(element, receiver) {
        return Err("focused keyboard receiver identity changed during traversal".into());
    }
    native.member(window, path, deadline)?;
    let safety = native.safety(element, deadline)?;
    if safety.secure {
        return Err("focused keyboard receiver is protected".into());
    }
    let metadata = native.keyboard_metadata(element, &safety.role, deadline)?;
    placement::time_left(deadline)?;
    Ok((safety, metadata))
}

fn same_metadata(before: &KeyboardMetadata, after: &KeyboardMetadata) -> Result<(), String> {
    if before.bounds != after.bounds || before.enabled != after.enabled {
        return Err(
            "keyboard receiver geometry or enabled state changed during observation".into(),
        );
    }
    Ok(())
}

fn target_bounds(
    metadata: &KeyboardMetadata,
    window: Observation,
    monitor: Bounds,
) -> Result<(), String> {
    if !window.ax.contains(metadata.bounds)
        || !window.cg.contains(metadata.bounds)
        || !monitor.contains(metadata.bounds)
    {
        return Err("keyboard receiver geometry is outside retained window or monitor".into());
    }
    Ok(())
}

impl<N: Native> placement::Windows<N> {
    /// Reads current state without clearing semantic or pointer inventories and
    /// without retaining a receiver after return.
    pub(super) fn read_keyboard_target(
        &mut self,
        id: u32,
        mut geometry: impl FnMut(u32) -> Result<Bounds, String>,
    ) -> Result<KeyboardTarget, String> {
        let binding = self.bindings.get(&id).ok_or("stale window binding")?;
        let deadline = Instant::now() + placement::BUDGET;
        let before = controls::check_window(
            &mut self.native,
            &binding.window,
            binding.identity,
            binding.geometry,
            &mut || geometry(binding.monitor),
            deadline,
        )?;
        // This records the human global focused object only as an unchanged
        // before/after witness. It is never used to choose the receiver.
        let human_before = self.native.keyboard_human_focus(deadline)?;
        let receiver = self
            .native
            .keyboard_focused(&binding.window, binding.identity, deadline)?;
        let path = locate(&mut self.native, &binding.window, &receiver, deadline)?;
        let (safety, metadata) = current_target(
            &mut self.native,
            &binding.window,
            &path,
            &receiver,
            deadline,
        )?;
        target_bounds(&metadata, before, binding.geometry)?;

        // Required rechecks after native metadata reads. A stale retained AX
        // object, bridge edge, monitor/window geometry or protected transition
        // never turns into a nearby replacement receiver.
        self.native.member(&binding.window, &path, deadline)?;
        let after_metadata = self
            .native
            .safety(path.last().expect("receiver path"), deadline)?;
        if after_metadata.secure || after_metadata.role != safety.role {
            return Err(
                "keyboard receiver protected state or role changed during observation".into(),
            );
        }
        let metadata_after = self.native.keyboard_metadata(
            path.last().expect("receiver path"),
            &safety.role,
            deadline,
        )?;
        same_metadata(&metadata, &metadata_after)?;
        target_bounds(&metadata_after, before, binding.geometry)?;
        controls::same_window(
            before,
            controls::check_window(
                &mut self.native,
                &binding.window,
                binding.identity,
                binding.geometry,
                &mut || geometry(binding.monitor),
                deadline,
            )?,
        )?;

        let receiver_after =
            self.native
                .keyboard_focused(&binding.window, binding.identity, deadline)?;
        if !self.native.equal(&receiver, &receiver_after) {
            return Err("application-local focused receiver changed during observation".into());
        }
        self.native.member(&binding.window, &path, deadline)?;
        let final_safety = self
            .native
            .safety(path.last().expect("receiver path"), deadline)?;
        if final_safety.secure || final_safety.role != safety.role {
            return Err(
                "keyboard receiver protected state or role changed during observation".into(),
            );
        }
        let metadata_final = self.native.keyboard_metadata(
            path.last().expect("receiver path"),
            &safety.role,
            deadline,
        )?;
        same_metadata(&metadata, &metadata_final)?;
        target_bounds(&metadata_final, before, binding.geometry)?;
        // Close protected-membership checks after the last metadata read.
        self.native.member(&binding.window, &path, deadline)?;
        let checked = self
            .native
            .safety(path.last().expect("receiver path"), deadline)?;
        if checked.secure || checked.role != safety.role {
            return Err("keyboard receiver safety changed after final metadata".into());
        }
        controls::same_window(
            before,
            controls::check_window(
                &mut self.native,
                &binding.window,
                binding.identity,
                binding.geometry,
                &mut || geometry(binding.monitor),
                deadline,
            )?,
        )?;
        if self.native.keyboard_human_focus(deadline)? != human_before {
            return Err(
                "human global focused object changed during keyboard receiver observation".into(),
            );
        }
        // The global-focus witness itself is a native read. Close the window
        // witness after it, so neither observation can race a geometry change
        // into a successful report.
        controls::same_window(
            before,
            controls::check_window(
                &mut self.native,
                &binding.window,
                binding.identity,
                binding.geometry,
                &mut || geometry(binding.monitor),
                deadline,
            )?,
        )?;
        // Validate receiver metadata again after the final focus/window reads.
        // These are bounded observations, never an atomic or lasting focus lock.
        let (latest_safety, latest_metadata) = current_target(
            &mut self.native,
            &binding.window,
            &path,
            &receiver,
            deadline,
        )?;
        if latest_safety.role != safety.role {
            return Err("keyboard receiver role changed at final checkpoint".into());
        }
        same_metadata(&metadata, &latest_metadata)?;
        target_bounds(&latest_metadata, before, binding.geometry)?;
        self.native.member(&binding.window, &path, deadline)?;
        let result = KeyboardTarget {
            role: safety.role,
            bounds: metadata.bounds,
            enabled: metadata.enabled,
            keyboard_dispatch_supported: false,
        };
        if !result.valid_reply() {
            return Err("invalid keyboard receiver observation".into());
        }
        // The private protocol must reject rather than truncate a malformed or
        // oversized report before anything reaches its stdout transport.
        super::protocol::encode(&super::protocol::Reply {
            seq: u64::MAX,
            result: super::protocol::Outcome::KeyboardTarget {
                target: result.clone(),
            },
        })?;
        placement::time_left(deadline)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macos_monitor::controls::{ElementAction, Metadata};
    use crate::macos_monitor::placement::{
        tests::{identity, monitor},
        ListedWindow, Native as PlacementNative, Windows,
    };
    use crate::macos_monitor::pointer::{Pair, Point, Posting};
    use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

    #[derive(Clone)]
    struct Node {
        parent: usize,
        children: Vec<usize>,
        role: String,
        secure: bool,
        enabled: bool,
        bounds: Bounds,
        value: String,
    }

    #[derive(Clone, Copy)]
    enum Change {
        None,
        ReplaceWindow,
        Detach,
        Protect,
        Role,
        Receiver,
        MissingReceiver,
        ForeignReceiver,
        AfterHumanFocusGeometry,
        ReceiverGeometry,
        ReceiverEnabled,
        WindowGeometry,
        MonitorGeometry,
        HumanFocus,
    }

    struct Model {
        nodes: BTreeMap<usize, Node>,
        window: Bounds,
        monitor: Bounds,
        object: u64,
        birth: u64,
        receiver: Option<usize>,
        second_receiver: Option<usize>,
        foreign: bool,
        human_focus: u64,
        focused_reads: usize,
        semantic_metadata_reads: usize,
        value_reads: usize,
        writes: usize,
        postings: usize,
        protected_child_reads: usize,
        change: Change,
    }

    #[derive(Clone)]
    struct Fake(Rc<RefCell<Model>>);

    impl Default for Fake {
        fn default() -> Self {
            let receiver_bounds = Bounds {
                x: -760.0,
                y: -160.0,
                width: 120.0,
                height: 28.0,
            };
            let mut nodes = BTreeMap::new();
            nodes.insert(
                0,
                Node {
                    parent: 0,
                    children: vec![1],
                    role: "AXWindow".into(),
                    secure: false,
                    enabled: true,
                    bounds: monitor(),
                    value: String::new(),
                },
            );
            nodes.insert(
                1,
                Node {
                    parent: 0,
                    children: vec![],
                    role: "AXTextField".into(),
                    secure: false,
                    enabled: true,
                    bounds: receiver_bounds,
                    value: String::new(),
                },
            );
            nodes.insert(
                2,
                Node {
                    parent: 0,
                    children: vec![],
                    role: "AXTextField".into(),
                    secure: false,
                    enabled: true,
                    bounds: receiver_bounds,
                    value: String::new(),
                },
            );
            Self(Rc::new(RefCell::new(Model {
                nodes,
                window: monitor(),
                monitor: monitor(),
                object: 1,
                birth: identity().start_seconds,
                receiver: Some(1),
                second_receiver: None,
                foreign: false,
                human_focus: 7,
                focused_reads: 0,
                semantic_metadata_reads: 0,
                value_reads: 0,
                writes: 0,
                postings: 0,
                protected_child_reads: 0,
                change: Change::None,
            })))
        }
    }

    impl Fake {
        fn check(&self, window: u64, id: WindowIdentity, deadline: Instant) -> Result<(), String> {
            placement::time_left(deadline)?;
            let m = self.0.borrow();
            if window != m.object || id != identity() || id.start_seconds != m.birth {
                return Err("retained process/window changed".into());
            }
            Ok(())
        }

        fn apply_change(m: &mut Model) {
            match m.change {
                Change::None | Change::AfterHumanFocusGeometry => {}
                Change::ReplaceWindow => m.object += 1,
                Change::Detach => m.nodes.get_mut(&0).unwrap().children.clear(),
                Change::Protect => m.nodes.get_mut(&1).unwrap().secure = true,
                Change::Role => m.nodes.get_mut(&1).unwrap().role = "AXButton".into(),
                Change::Receiver => m.second_receiver = Some(2),
                Change::MissingReceiver => m.receiver = None,
                Change::ForeignReceiver => m.foreign = true,
                Change::ReceiverGeometry => m.nodes.get_mut(&1).unwrap().bounds.x += 1.0,
                Change::ReceiverEnabled => m.nodes.get_mut(&1).unwrap().enabled = false,
                Change::WindowGeometry => m.window.x += 2.0,
                Change::MonitorGeometry => m.monitor.x += 2.0,
                Change::HumanFocus => m.human_focus += 1,
            }
        }
    }

    struct FakePair(Fake);
    impl Pair for FakePair {
        fn post(&mut self) -> Posting {
            self.0 .0.borrow_mut().postings += 1;
            Posting::from_native(2, false)
        }
    }

    impl PlacementNative for Fake {
        type Window = u64;
        type Focus = u64;

        fn pointer_ready(&mut self, window: &u64, deadline: Instant) -> Result<(), String> {
            self.check(*window, identity(), deadline)
        }

        fn pointer_pair(
            &mut self,
            window: &u64,
            _point: Point,
            _global: Point,
            deadline: Instant,
        ) -> Result<Box<dyn Pair>, String> {
            self.check(*window, identity(), deadline)?;
            Ok(Box::new(FakePair(self.clone())))
        }

        fn candidates(
            &mut self,
            pid: i32,
            deadline: Instant,
        ) -> Result<Vec<ListedWindow<u64>>, String> {
            placement::time_left(deadline)?;
            if pid != identity().pid {
                return Err("wrong candidate PID".into());
            }
            let m = self.0.borrow();
            Ok(vec![ListedWindow {
                identity: identity(),
                bounds: m.window,
                window: m.object,
            }])
        }

        fn observe(
            &mut self,
            window: &u64,
            id: WindowIdentity,
            deadline: Instant,
        ) -> Result<Observation, String> {
            self.check(*window, id, deadline)?;
            let m = self.0.borrow();
            Ok(Observation {
                ax: m.window,
                cg: m.window,
            })
        }

        fn focus(&mut self, deadline: Instant) -> Result<u64, String> {
            placement::time_left(deadline)?;
            let mut m = self.0.borrow_mut();
            if matches!(m.change, Change::AfterHumanFocusGeometry) && m.focused_reads >= 2 {
                m.nodes.get_mut(&1).unwrap().bounds.x += 1.0;
            }
            Ok(m.human_focus)
        }

        fn position(&mut self, _: &u64, _: Bounds, _: Instant) -> Result<(), String> {
            Err("read-only keyboard fixture must not position windows".into())
        }

        fn size(&mut self, _: &u64, _: Bounds, _: Instant) -> Result<(), String> {
            Err("read-only keyboard fixture must not resize windows".into())
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

        fn safety(&mut self, element: &usize, deadline: Instant) -> Result<Safety, String> {
            placement::time_left(deadline)?;
            let m = self.0.borrow();
            let node = m.nodes.get(element).ok_or("retained AX node disappeared")?;
            Ok(Safety {
                role: node.role.clone(),
                secure: node.secure,
            })
        }

        fn children(&mut self, element: &usize, deadline: Instant) -> Result<Vec<usize>, String> {
            placement::time_left(deadline)?;
            let mut m = self.0.borrow_mut();
            let (secure, children) = {
                let node = m.nodes.get(element).ok_or("retained AX node disappeared")?;
                (node.secure, node.children.clone())
            };
            if secure {
                m.protected_child_reads += 1;
                return Err("protected subtree traversed".into());
            }
            Ok(children)
        }

        fn metadata(
            &mut self,
            element: &usize,
            _role: &str,
            deadline: Instant,
        ) -> Result<Metadata, String> {
            placement::time_left(deadline)?;
            let mut m = self.0.borrow_mut();
            m.semantic_metadata_reads += 1;
            let node = m.nodes.get(element).ok_or("retained AX node disappeared")?;
            Ok(Metadata {
                label: "fixture label".into(),
                bounds: node.bounds,
                enabled: node.enabled,
                press: node.role == "AXButton",
                value_settable: node.role == "AXTextField",
            })
        }

        fn member(
            &mut self,
            window: &u64,
            path: &[usize],
            deadline: Instant,
        ) -> Result<(), String> {
            self.check(*window, identity(), deadline)?;
            let m = self.0.borrow();
            if path.first() != Some(&0) {
                return Err("receiver ancestry is not rooted at retained window".into());
            }
            for edge in path.windows(2) {
                let parent = m
                    .nodes
                    .get(&edge[0])
                    .ok_or("retained ancestor disappeared")?;
                let child = m
                    .nodes
                    .get(&edge[1])
                    .ok_or("retained receiver disappeared")?;
                if parent.secure
                    || child.secure
                    || child.parent != edge[0]
                    || !parent.children.contains(&edge[1])
                {
                    return Err("retained receiver ancestry changed".into());
                }
            }
            Ok(())
        }

        fn press(&mut self, _: &usize, deadline: Instant) -> Result<(), String> {
            placement::time_left(deadline)?;
            self.0.borrow_mut().writes += 1;
            Ok(())
        }

        fn set_value(
            &mut self,
            element: &usize,
            text: &str,
            deadline: Instant,
        ) -> Result<(), String> {
            placement::time_left(deadline)?;
            let mut m = self.0.borrow_mut();
            m.writes += 1;
            m.nodes
                .get_mut(element)
                .ok_or("retained receiver disappeared")?
                .value = text.into();
            Ok(())
        }

        fn value_matches(
            &mut self,
            element: &usize,
            text: &str,
            deadline: Instant,
        ) -> Result<bool, String> {
            placement::time_left(deadline)?;
            let mut m = self.0.borrow_mut();
            m.value_reads += 1;
            Ok(m.nodes
                .get(element)
                .ok_or("retained receiver disappeared")?
                .value
                == text)
        }

        fn keyboard_focused(
            &mut self,
            window: &u64,
            id: WindowIdentity,
            deadline: Instant,
        ) -> Result<usize, String> {
            self.check(*window, id, deadline)?;
            let mut m = self.0.borrow_mut();
            m.focused_reads += 1;
            if m.foreign {
                return Err("application-local focus belongs to a foreign target".into());
            }
            if m.focused_reads >= 2 {
                if let Some(receiver) = m.second_receiver {
                    return Ok(receiver);
                }
            }
            m.receiver
                .ok_or_else(|| "application-local focus is absent".to_string())
        }

        fn keyboard_metadata(
            &mut self,
            element: &usize,
            role: &str,
            deadline: Instant,
        ) -> Result<KeyboardMetadata, String> {
            placement::time_left(deadline)?;
            let mut m = self.0.borrow_mut();
            let (secure, actual_role, bounds, enabled) = {
                let node = m
                    .nodes
                    .get(element)
                    .ok_or("retained receiver disappeared")?;
                (node.secure, node.role.clone(), node.bounds, node.enabled)
            };
            if secure || actual_role != role {
                return Err("protected or contradictory keyboard metadata".into());
            }
            let result = KeyboardMetadata { bounds, enabled };
            Self::apply_change(&mut m);
            Ok(result)
        }
    }

    fn rig() -> (Windows<Fake>, Fake, u32) {
        let fake = Fake::default();
        let mut windows = Windows::new(fake.clone());
        let candidate = windows.candidates(identity().pid).unwrap().remove(0);
        let binding = windows
            .bind(1, identity(), &candidate.candidate, |_| Ok(monitor()))
            .unwrap();
        (windows, fake, binding)
    }

    fn inspect(
        windows: &mut Windows<Fake>,
        binding: u32,
        fake: &Fake,
    ) -> Result<KeyboardTarget, String> {
        windows.read_keyboard_target(binding, |_| Ok(fake.0.borrow().monitor))
    }

    #[test]
    fn stable_receiver_report_is_bounded_and_never_reads_labels_or_values() {
        let (mut windows, fake, binding) = rig();
        let report = inspect(&mut windows, binding, &fake).unwrap();
        assert_eq!(report.role, "AXTextField");
        assert_eq!(report.bounds, fake.0.borrow().nodes[&1].bounds);
        assert!(report.enabled);
        assert!(!report.keyboard_dispatch_supported);
        assert!(report.valid_reply());
        let json = serde_json::to_value(&report).unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(object.len(), 4);
        for forbidden in ["label", "value", "text", "element", "token", "pointer"] {
            assert!(!object.contains_key(forbidden));
        }
        let m = fake.0.borrow();
        assert_eq!(m.focused_reads, 2);
        assert_eq!(m.semantic_metadata_reads, 0);
        assert_eq!(m.value_reads, 0);
        assert_eq!(m.writes, 0);
        assert_eq!(m.postings, 0);
        assert_eq!(m.protected_child_reads, 0);
    }

    #[test]
    fn no_focus_foreign_replaced_detached_and_protected_receivers_refuse() {
        for kind in ["no_focus", "foreign", "replaced", "detached", "protected"] {
            let (mut windows, fake, binding) = rig();
            {
                let mut m = fake.0.borrow_mut();
                match kind {
                    "no_focus" => m.receiver = None,
                    "foreign" => m.foreign = true,
                    "replaced" => m.change = Change::ReplaceWindow,
                    "detached" => m.change = Change::Detach,
                    _ => m.nodes.get_mut(&1).unwrap().secure = true,
                }
            }
            assert!(inspect(&mut windows, binding, &fake).is_err(), "{kind}");
            let m = fake.0.borrow();
            assert_eq!(m.writes, 0, "{kind}");
            assert_eq!(m.postings, 0, "{kind}");
            assert_eq!(m.protected_child_reads, 0, "{kind}");
        }
    }

    #[test]
    fn every_changed_snapshot_witness_refuses() {
        for change in [
            Change::Protect,
            Change::Role,
            Change::Receiver,
            Change::MissingReceiver,
            Change::ForeignReceiver,
            Change::WindowGeometry,
            Change::MonitorGeometry,
            Change::HumanFocus,
        ] {
            let (mut windows, fake, binding) = rig();
            fake.0.borrow_mut().change = change;
            assert!(inspect(&mut windows, binding, &fake).is_err());
            assert_eq!(fake.0.borrow().writes, 0);
        }
    }

    #[test]
    fn containment_stale_binding_and_all_traversal_budgets_refuse() {
        let (mut windows, fake, binding) = rig();
        fake.0.borrow_mut().nodes.get_mut(&1).unwrap().bounds.x = -801.0;
        assert!(inspect(&mut windows, binding, &fake).is_err());

        let (mut windows, fake, binding) = rig();
        windows.unbind(binding).unwrap();
        assert!(inspect(&mut windows, binding, &fake).is_err());

        for mode in ["children", "depth", "nodes"] {
            let (mut windows, fake, binding) = rig();
            let mut m = fake.0.borrow_mut();
            m.receiver = Some(999);
            match mode {
                "children" => {
                    m.nodes.get_mut(&0).unwrap().children = vec![1; controls::MAX_CHILDREN + 1]
                }
                "depth" => {
                    m.nodes.clear();
                    for id in 0..=controls::MAX_DEPTH + 1 {
                        m.nodes.insert(
                            id,
                            Node {
                                parent: id.saturating_sub(1),
                                children: (id < controls::MAX_DEPTH + 1)
                                    .then_some(id + 1)
                                    .into_iter()
                                    .collect(),
                                role: if id == 0 { "AXWindow" } else { "AXGroup" }.into(),
                                secure: false,
                                enabled: true,
                                bounds: monitor(),
                                value: String::new(),
                            },
                        );
                    }
                }
                _ => {
                    m.nodes.get_mut(&0).unwrap().children = (3..=34).collect();
                    for id in 3..=34 {
                        let children: Vec<_> =
                            (id * 100..id * 100 + controls::MAX_CHILDREN).collect();
                        for child in &children {
                            m.nodes.insert(
                                *child,
                                Node {
                                    parent: id,
                                    children: vec![],
                                    role: "AXGroup".into(),
                                    secure: false,
                                    enabled: true,
                                    bounds: monitor(),
                                    value: String::new(),
                                },
                            );
                        }
                        m.nodes.insert(
                            id,
                            Node {
                                parent: 0,
                                children,
                                role: "AXGroup".into(),
                                secure: false,
                                enabled: true,
                                bounds: monitor(),
                                value: String::new(),
                            },
                        );
                    }
                }
            }
            drop(m);
            assert!(inspect(&mut windows, binding, &fake).is_err(), "{mode}");
        }

        let mut fake = Fake::default();
        assert!(locate(&mut fake, &1, &1, Instant::now()).is_err());
    }

    #[test]
    fn inspection_preserves_existing_semantic_and_pointer_preparations() {
        let (mut windows, fake, binding) = rig();
        let controls = windows
            .read_elements(binding, |_| Ok(fake.0.borrow().monitor))
            .unwrap();
        let metadata_reads = fake.0.borrow().semantic_metadata_reads;
        inspect(&mut windows, binding, &fake).unwrap();
        assert_eq!(fake.0.borrow().semantic_metadata_reads, metadata_reads);
        assert!(windows
            .act_element(
                binding,
                &controls[0].element,
                &ElementAction::SetValue {
                    text: "fixture".into()
                },
                |_| Ok(fake.0.borrow().monitor),
            )
            .unwrap()
            .successful());

        let (mut windows, fake, binding) = rig();
        let prepared = windows
            .prepare_click(binding, Point { x: 10.0, y: 10.0 }, |_| {
                Ok(fake.0.borrow().monitor)
            })
            .unwrap();
        inspect(&mut windows, binding, &fake).unwrap();
        assert!(windows
            .click(binding, &prepared.token, |_| Ok(fake.0.borrow().monitor))
            .unwrap()
            .successful());
        assert_eq!(fake.0.borrow().postings, 1);
    }

    #[test]
    fn malformed_public_reports_are_never_truthful() {
        let mut report = KeyboardTarget {
            role: "AXTextField".into(),
            bounds: Bounds {
                x: -760.0,
                y: -160.0,
                width: 120.0,
                height: 28.0,
            },
            enabled: true,
            keyboard_dispatch_supported: false,
        };
        assert!(report.valid_reply());
        report.keyboard_dispatch_supported = true;
        assert!(!report.valid_reply());
        report.keyboard_dispatch_supported = false;
        report.role = String::new();
        assert!(!report.valid_reply());
        report.role = "AXTextField".into();
        report.bounds.width = 0.0;
        assert!(!report.valid_reply());
    }
    #[test]
    fn receiver_metadata_changes_inside_stable_window_refuse() {
        for change in [
            Change::ReceiverGeometry,
            Change::ReceiverEnabled,
            Change::AfterHumanFocusGeometry,
        ] {
            let (mut windows, fake, binding) = rig();
            fake.0.borrow_mut().change = change;
            assert!(
                inspect(&mut windows, binding, &fake).is_err(),
                "changed receiver metadata must refuse"
            );
            assert_eq!(fake.0.borrow().postings, 0);
        }
    }
    #[test]
    fn root_window_is_not_a_keyboard_receiver() {
        let (mut windows, fake, binding) = rig();
        fake.0.borrow_mut().receiver = Some(0);
        assert!(inspect(&mut windows, binding, &fake).is_err());
    }

    #[test]
    fn failed_inspection_preserves_semantic_and_pointer_preparations() {
        let (mut windows, fake, binding) = rig();
        let controls = windows
            .read_elements(binding, |_| Ok(fake.0.borrow().monitor))
            .unwrap();
        fake.0.borrow_mut().receiver = None;
        assert!(inspect(&mut windows, binding, &fake).is_err());
        fake.0.borrow_mut().receiver = Some(1);
        assert!(windows
            .act_element(
                binding,
                &controls[0].element,
                &ElementAction::SetValue {
                    text: "fixture".into()
                },
                |_| Ok(fake.0.borrow().monitor)
            )
            .unwrap()
            .successful());
        let (mut windows, fake, binding) = rig();
        let prepared = windows
            .prepare_click(binding, Point { x: 10.0, y: 10.0 }, |_| {
                Ok(fake.0.borrow().monitor)
            })
            .unwrap();
        fake.0.borrow_mut().receiver = None;
        assert!(inspect(&mut windows, binding, &fake).is_err());
        fake.0.borrow_mut().receiver = Some(1);
        assert!(windows
            .click(binding, &prepared.token, |_| Ok(fake.0.borrow().monitor))
            .unwrap()
            .successful());
        assert_eq!(fake.0.borrow().postings, 1);
    }
}

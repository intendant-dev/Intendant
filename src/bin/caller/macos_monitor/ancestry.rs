//! Exact-object projection for one documented native shape observed in the
//! disposable Chromium fixture: Group exposes WebArea, whose actual parent is
//! an omitted ScrollArea under that same Group. Never repair arbitrary ancestry.
use super::{controls, placement};
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Instant;

#[derive(Clone, PartialEq)]
pub(crate) struct Node<E> {
    pub(crate) element: E,
    // A bridge is equal only when BOTH the intermediate and the originally
    // exposed child are equal. Comparing the intermediate alone allows a stale
    // child to survive a change to the outer group's exposed child list.
    pub(crate) forwarded_child: Option<E>,
    _thread: PhantomData<Rc<()>>,
}
impl<E> Node<E> {
    pub(crate) fn plain(element: E) -> Self {
        Self {
            element,
            forwarded_child: None,
            _thread: PhantomData,
        }
    }
    fn bridge(element: E, child: E) -> Self {
        Self {
            element,
            forwarded_child: Some(child),
            _thread: PhantomData,
        }
    }
}

pub(crate) trait Graph {
    type Element: Clone + PartialEq;
    fn safety(&mut self, e: &Self::Element) -> Result<controls::Safety, String>;
    fn parent(&mut self, e: &Self::Element) -> Result<Self::Element, String>;
    fn window(&mut self, e: &Self::Element) -> Result<Self::Element, String>;
    fn pid(&mut self, e: &Self::Element) -> Result<i32, String>;
    fn children(&mut self, e: &Self::Element) -> Result<Vec<Self::Element>, String>;
}

fn bounded_children<G: Graph>(g: &mut G, e: &G::Element) -> Result<Vec<G::Element>, String> {
    let children = g.children(e)?;
    if children.len() > controls::MAX_CHILDREN {
        return Err("AX projected children limit exceeded".into());
    }
    Ok(children)
}

/// Project only an exact, fully checked bridge. The caller walks the returned
/// ScrollArea as a normal node, so its security state is checked before any
/// descendant's label or value. The original downward edge remains a witness.
pub(crate) fn children<G: Graph>(
    g: &mut G,
    node: &Node<G::Element>,
    deadline: Instant,
) -> Result<Vec<Node<G::Element>>, String> {
    placement::time_left(deadline)?;
    let safety = g
        .safety(&node.element)
        .map_err(|e| format!("AX child-list parent safety: {e}"))?;
    if safety.secure {
        return Ok(vec![]);
    }
    if let Some(child) = &node.forwarded_child {
        // Restrict traversal to the exact originally exposed child. Never
        // discover additional content through an omitted intermediate node.
        if safety.role != "AXScrollArea" {
            return Err("AX bridge role changed".into());
        }
        let child_safety = g
            .safety(child)
            .map_err(|e| format!("AX retained forwarded-child safety: {e}"))?;
        if child_safety.secure {
            return Ok(vec![]);
        }
        if child_safety.role != "AXWebArea"
            || g.parent(child)? != node.element
            || g.pid(child)? != g.pid(&node.element)?
            || g.window(child)? != g.window(&node.element)?
            || !bounded_children(g, &node.element)?.contains(child)
        {
            return Err("retained AX bridge child changed".into());
        }
        placement::time_left(deadline)?;
        return Ok(vec![Node::plain(child.clone())]);
    }
    let raw = bounded_children(g, &node.element)?;
    let mut projected = Vec::with_capacity(raw.len());
    for child in raw {
        placement::time_left(deadline)?;
        let child_safety = g
            .safety(&child)
            .map_err(|e| format!("AX exposed child safety under {}: {e}", safety.role))?;
        if child_safety.secure || child_safety.role != "AXWebArea" {
            projected.push(Node::plain(child));
            continue;
        }
        let parent = g.parent(&child)?;
        if parent == node.element {
            projected.push(Node::plain(child));
            continue;
        }
        let parent_safety = g
            .safety(&parent)
            .map_err(|e| format!("AX WebArea intermediate safety: {e}"))?;
        // No role-only substitution: every relationship and PID/window must
        // agree with the exact child returned by this exact group's AXChildren.
        if safety.role != "AXGroup"
            || parent_safety.role != "AXScrollArea"
            || parent == child
            || g.parent(&parent)? != node.element
            || g.pid(&parent)? != g.pid(&node.element)?
            || g.pid(&child)? != g.pid(&node.element)?
            || g.window(&parent)? != g.window(&node.element)?
            || g.window(&child)? != g.window(&node.element)?
        {
            return Err("unsupported or foreign AX parent bridge".into());
        }
        // Protected bridges remain visible only as an omitted subtree; do not
        // read their children to validate a relationship which cannot be used.
        if !parent_safety.secure && !bounded_children(g, &parent)?.contains(&child) {
            return Err("AX parent bridge no longer exposes its exact child".into());
        }
        projected.push(Node::bridge(parent, child));
    }
    placement::time_left(deadline)?;
    Ok(projected)
}

/// Validate only the retained edge during membership rechecks. The native caller
/// separately revalidates every path node's safety/PID/window and exact AXParent.
/// Never rescan unrelated siblings' metadata for each ancestor of each control.
pub(crate) fn membership_children<G: Graph>(
    g: &mut G,
    parent: &Node<G::Element>,
    expected: &Node<G::Element>,
    deadline: Instant,
) -> Result<Vec<Node<G::Element>>, String> {
    placement::time_left(deadline)?;
    if parent.forwarded_child.is_some() {
        // A retained bridge has exactly one permitted child; this also checks
        // its current role, PID/window, security and exact-parent relationships.
        return children(g, parent, deadline);
    }
    let raw = bounded_children(g, &parent.element)?;
    let Some(exposed) = &expected.forwarded_child else {
        placement::time_left(deadline)?;
        return Ok(raw.into_iter().map(Node::plain).collect());
    };
    if !raw.contains(exposed) {
        return Ok(vec![]);
    }
    let outer = g.safety(&parent.element)?;
    let inner = g.safety(&expected.element)?;
    let child = g.safety(exposed)?;
    if outer.secure
        || inner.secure
        || child.secure
        || outer.role != "AXGroup"
        || inner.role != "AXScrollArea"
        || child.role != "AXWebArea"
        || g.parent(exposed)? != expected.element
        || g.parent(&expected.element)? != parent.element
        || g.pid(exposed)? != g.pid(&parent.element)?
        || g.pid(&expected.element)? != g.pid(&parent.element)?
        || g.window(exposed)? != g.window(&parent.element)?
        || g.window(&expected.element)? != g.window(&parent.element)?
        || !bounded_children(g, &expected.element)?.contains(exposed)
    {
        return Err("retained AX projected edge changed".into());
    }
    placement::time_left(deadline)?;
    Ok(vec![expected.clone()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    #[derive(Clone)]
    struct Item {
        role: &'static str,
        parent: u8,
        children: Vec<u8>,
        secure: bool,
        pid: i32,
        window: u8,
    }
    struct Fake {
        nodes: BTreeMap<u8, Item>,
        children_read: Vec<u8>,
        failed: Option<u8>,
    }
    impl Fake {
        fn item(&self, e: &u8) -> Result<&Item, String> {
            if self.failed == Some(*e) {
                return Err("metadata unavailable".into());
            }
            self.nodes.get(e).ok_or("element unavailable".into())
        }
    }
    impl Graph for Fake {
        type Element = u8;
        fn safety(&mut self, e: &u8) -> Result<controls::Safety, String> {
            let i = self.item(e)?;
            Ok(controls::Safety {
                role: i.role.into(),
                secure: i.secure,
            })
        }
        fn parent(&mut self, e: &u8) -> Result<u8, String> {
            Ok(self.item(e)?.parent)
        }
        fn window(&mut self, e: &u8) -> Result<u8, String> {
            Ok(self.item(e)?.window)
        }
        fn pid(&mut self, e: &u8) -> Result<i32, String> {
            Ok(self.item(e)?.pid)
        }
        fn children(&mut self, e: &u8) -> Result<Vec<u8>, String> {
            self.children_read.push(*e);
            let i = self.item(e)?;
            assert!(!i.secure, "protected subtree was traversed");
            Ok(i.children.clone())
        }
    }
    fn rig() -> Fake {
        let mut nodes = BTreeMap::new();
        for (id, role, parent, children) in [
            (0, "AXWindow", 0, vec![1]),
            (1, "AXGroup", 0, vec![3]),
            (2, "AXScrollArea", 1, vec![3]),
            (3, "AXWebArea", 2, vec![4]),
            (4, "AXTextField", 3, vec![]),
        ] {
            nodes.insert(
                id,
                Item {
                    role,
                    parent,
                    children,
                    secure: false,
                    pid: 42,
                    window: 0,
                },
            );
        }
        Fake {
            nodes,
            children_read: vec![],
            failed: None,
        }
    }
    fn read(g: &mut Fake, n: &Node<u8>) -> Result<Vec<Node<u8>>, String> {
        children(g, n, Instant::now() + placement::BUDGET)
    }
    #[test]
    fn projection_retains_exact_intermediate_and_original_exposed_child() {
        let mut g = rig();
        let outer = Node::plain(1);
        let first = read(&mut g, &outer).unwrap().remove(0);
        assert_eq!(first.element, 2);
        assert_eq!(first.forwarded_child, Some(3));
        let web = read(&mut g, &first).unwrap().remove(0);
        assert_eq!(web.element, 3);
        assert!(web.forwarded_child.is_none());
        assert!(read(&mut g, &outer).unwrap().contains(&first));
        // Replacing the exposed WebArea must invalidate the witness even if
        // the old child is still listed by the same intermediate ScrollArea.
        let replacement = g.nodes[&3].clone();
        g.nodes.insert(5, replacement);
        g.nodes.get_mut(&1).unwrap().children = vec![5];
        g.nodes.get_mut(&2).unwrap().children.push(5);
        assert!(!read(&mut g, &outer).unwrap().contains(&first));
    }
    #[test]
    fn no_projection_for_ordinary_parent_child_or_other_roles() {
        let mut g = rig();
        g.nodes.get_mut(&3).unwrap().parent = 1;
        let n = read(&mut g, &Node::plain(1)).unwrap().remove(0);
        assert_eq!(n.element, 3);
        assert!(n.forwarded_child.is_none());
        g.nodes.get_mut(&3).unwrap().role = "AXGroup";
        g.nodes.get_mut(&3).unwrap().parent = 2;
        let n = read(&mut g, &Node::plain(1)).unwrap().remove(0);
        assert_eq!(n.element, 3);
        assert!(n.forwarded_child.is_none());
    }
    #[test]
    fn bridge_refuses_foreign_window_pid_reparenting_detachment_and_bad_roles() {
        for change in 0..9 {
            let mut g = rig();
            match change {
                0 => g.nodes.get_mut(&2).unwrap().pid = 99,
                1 => g.nodes.get_mut(&3).unwrap().pid = 99,
                2 => g.nodes.get_mut(&2).unwrap().window = 99,
                3 => g.nodes.get_mut(&3).unwrap().window = 99,
                4 => g.nodes.get_mut(&2).unwrap().parent = 0,
                5 => g.nodes.get_mut(&2).unwrap().children.clear(),
                6 => g.nodes.get_mut(&2).unwrap().role = "AXGroup",
                7 => g.nodes.get_mut(&1).unwrap().role = "AXToolbar",
                _ => g.failed = Some(2),
            }
            assert!(read(&mut g, &Node::plain(1)).is_err(), "change {change}");
        }
    }
    #[test]
    fn captured_bridge_refuses_replacement_or_detached_child() {
        for change in 0..5 {
            let mut g = rig();
            let n = read(&mut g, &Node::plain(1)).unwrap().remove(0);
            match change {
                0 => g.nodes.get_mut(&2).unwrap().children.clear(),
                1 => g.nodes.get_mut(&3).unwrap().parent = 1,
                2 => g.nodes.get_mut(&3).unwrap().pid = 99,
                3 => g.nodes.get_mut(&3).unwrap().window = 99,
                _ => g.nodes.get_mut(&3).unwrap().role = "AXGroup",
            }
            assert!(read(&mut g, &n).is_err());
        }
        let mut g = rig();
        let n = read(&mut g, &Node::plain(1)).unwrap().remove(0);
        let replacement = g.nodes[&2].clone();
        g.nodes.insert(5, replacement);
        g.nodes.get_mut(&3).unwrap().parent = 5;
        assert!(!read(&mut g, &Node::plain(1)).unwrap().contains(&n));
    }
    #[test]
    fn protected_bridge_is_checked_before_children_and_can_never_retain_contents() {
        let mut g = rig();
        g.nodes.get_mut(&2).unwrap().secure = true;
        let n = read(&mut g, &Node::plain(1)).unwrap().remove(0);
        assert!(!g.children_read.contains(&2));
        assert!(read(&mut g, &n).unwrap().is_empty());
        assert!(!g.children_read.contains(&2));
        g.nodes.get_mut(&2).unwrap().secure = false;
        g.nodes.get_mut(&3).unwrap().secure = true;
        assert!(read(&mut g, &n).unwrap().is_empty());
    }
    #[test]
    fn targeted_membership_preserves_witness_and_ignores_unrelated_metadata() {
        let mut g = rig();
        let outer = Node::plain(1);
        let retained = read(&mut g, &outer).unwrap().remove(0);
        // Metadata of unrelated siblings is not necessary to prove this edge.
        g.nodes.get_mut(&1).unwrap().children.push(99);
        g.failed = Some(99);
        let deadline = Instant::now() + placement::BUDGET;
        assert!(membership_children(&mut g, &outer, &retained, deadline)
            .unwrap()
            .contains(&retained));
        g.nodes.get_mut(&1).unwrap().children = vec![99];
        assert!(membership_children(&mut g, &outer, &retained, deadline)
            .unwrap()
            .is_empty());
        g.nodes.get_mut(&1).unwrap().children = vec![3];
        g.nodes.get_mut(&2).unwrap().secure = true;
        assert!(membership_children(&mut g, &outer, &retained, deadline).is_err());
    }

    #[test]
    fn raw_and_bridge_children_keep_existing_caps_and_deadlines() {
        let mut g = rig();
        g.nodes.get_mut(&1).unwrap().children = vec![3; controls::MAX_CHILDREN + 1];
        assert!(read(&mut g, &Node::plain(1)).is_err());
        let mut g = rig();
        g.nodes.get_mut(&2).unwrap().children = vec![3; controls::MAX_CHILDREN + 1];
        assert!(read(&mut g, &Node::plain(1)).is_err());
        assert!(children(&mut rig(), &Node::plain(1), Instant::now()).is_err());
    }
}

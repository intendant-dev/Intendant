//! Collision-free `DisplaySession` peer identifiers.
//!
//! Every display signaling lane inserts into the same per-session peer map,
//! so its internal `u64` keys must be disjoint even when the lanes allocate
//! independently. The top two bits identify the lane; the remaining 62 bits
//! carry either a monotonic local counter or the deterministic federated
//! connection/session hash used by offer/ICE/close and WS teardown.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

const NAMESPACE_SHIFT: u32 = 62;
const PAYLOAD_MASK: u64 = (1_u64 << NAMESPACE_SHIFT) - 1;
const LEGACY_WS_NAMESPACE: u64 = 0_u64 << NAMESPACE_SHIFT;
const DASHBOARD_CONTROL_NAMESPACE: u64 = 1_u64 << NAMESPACE_SHIFT;
const FEDERATED_NAMESPACE: u64 = 2_u64 << NAMESPACE_SHIFT;

/// One counter is shared by the two locally allocated lanes. Namespacing is
/// the structural collision fence; the shared payload sequence also makes it
/// impossible for a future accidental tag removal to recreate the original
/// deterministic first-tab/first-control collision.
static NEXT_LOCAL_PEER_PAYLOAD: AtomicU64 = AtomicU64::new(1);

fn allocate_in_namespace(counter: &AtomicU64, namespace: u64) -> Option<crate::display::PeerId> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |payload| {
            if (1..=PAYLOAD_MASK).contains(&payload) {
                Some(payload + 1)
            } else {
                None
            }
        })
        .ok()
        .map(|payload| namespace | payload)
}

pub(crate) fn allocate_legacy_ws_display_peer_id() -> Option<crate::display::PeerId> {
    allocate_in_namespace(&NEXT_LOCAL_PEER_PAYLOAD, LEGACY_WS_NAMESPACE)
}

pub(crate) fn allocate_dashboard_control_display_peer_id() -> Option<crate::display::PeerId> {
    allocate_in_namespace(&NEXT_LOCAL_PEER_PAYLOAD, DASHBOARD_CONTROL_NAMESPACE)
}

/// Stable mapping for the federated lane. The transport connection id and
/// browser-supplied session id together name one peer: the session id is only
/// unique inside its authenticated federation connection and must not let a
/// second connection replace or tear down the first connection's peer. The
/// same pair resolves identically for offer, ICE, explicit close, and
/// transport teardown, while never aliasing either locally allocated lane.
pub(crate) fn peer_id_for_federated_session(
    federation_connection_id: &str,
    session_id: &str,
) -> crate::display::PeerId {
    let mut hasher = DefaultHasher::new();
    (federation_connection_id, session_id).hash(&mut hasher);
    FEDERATED_NAMESPACE | (hasher.finish() & PAYLOAD_MASK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_peer_id_lanes_are_disjoint_and_federated_is_stable() {
        let ws = allocate_legacy_ws_display_peer_id().expect("WS peer id");
        let control =
            allocate_dashboard_control_display_peer_id().expect("dashboard-control peer id");
        let federated = peer_id_for_federated_session("connection-a", "session-a");

        assert_eq!(ws >> NAMESPACE_SHIFT, 0);
        assert_eq!(control >> NAMESPACE_SHIFT, 1);
        assert_eq!(federated >> NAMESPACE_SHIFT, 2);
        assert_ne!(ws, control);
        assert_ne!(ws, federated);
        assert_ne!(control, federated);
        assert_eq!(
            federated,
            peer_id_for_federated_session("connection-a", "session-a"),
            "federated offer and teardown must derive the same key"
        );
        assert_ne!(
            federated,
            peer_id_for_federated_session("connection-b", "session-a"),
            "a browser session id is scoped to its federation connection"
        );
    }

    #[test]
    fn allocator_exhaustion_never_wraps_or_crosses_namespaces() {
        let counter = AtomicU64::new(PAYLOAD_MASK);
        assert_eq!(
            allocate_in_namespace(&counter, DASHBOARD_CONTROL_NAMESPACE),
            Some(DASHBOARD_CONTROL_NAMESPACE | PAYLOAD_MASK)
        );
        assert_eq!(counter.load(Ordering::Relaxed), PAYLOAD_MASK + 1);
        assert_eq!(
            allocate_in_namespace(&counter, DASHBOARD_CONTROL_NAMESPACE),
            None
        );
        assert_eq!(
            allocate_in_namespace(&counter, DASHBOARD_CONTROL_NAMESPACE),
            None
        );

        let corrupted = AtomicU64::new(u64::MAX);
        assert_eq!(allocate_in_namespace(&corrupted, LEGACY_WS_NAMESPACE), None);
        assert_eq!(corrupted.load(Ordering::Relaxed), u64::MAX);
    }
}

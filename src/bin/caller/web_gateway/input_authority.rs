//! Display input authority: the holder state machine, grant/release
//! application for local, dashboard-control, and federated websockets,
//! federated authority subscribers, and bootstrap snapshots.

use super::*;

/// Identity of who currently holds input authority for one display.
///
/// Three provenance kinds, with explicit identity per kind so the
/// arbitration / gate / cleanup paths can match on the source of the
/// hold without resorting to string-shape inference:
///
/// - **`LocalWs`**: holder is a WebSocket connection on this gateway.
///   Carries the WS connection id (identity) plus the connection's
///   `direct_tx` for the local-only `display_input_authority_revoked`
///   confirmation that fires when this holder is preempted by another
///   grant. Federated holders do NOT get a direct revoke — federated
///   state always flows through the personalized authority-state
///   broadcast on each federated WebRtcPeer's `display_input_authority`
///   data channel.
///
/// - **`FederatedWebRtc`**: holder is a federated `PeerDisplayConnection`
///   on a peer primary. Identified by `(federation_connection_id,
///   session_id)`. `federation_connection_id` is the gateway-WS
///   `connection_id` of the federation transport (one per primary's
///   federation client); `session_id` distinguishes multiple
///   `PeerDisplayConnection` tabs from the same primary. Field name
///   spelled out so it's not confused with the local-browser
///   `LocalWs::connection_id`.
///
///   The design doc originally specified `peer_id: PeerId`, but the
///   stable federation `PeerId` isn't carried in the
///   `ControlMsg::WebRtcSignal` wire format — it's implicit in which
///   `/ws` connection delivered the message. F-1.3b uses the federation
///   WS `connection_id` as the holder identity instead; it's
///   authenticated by the federation WS connection, unique per
///   primary's federation transport, and already covered by WS-close
///   cleanup. `connection_id` changes across federation WS reconnect
///   (a stable `PeerId` would survive); WS-close cleanup releases any
///   held authority on each disconnect, so the trade-off is a UX
///   nicety, not correctness. See
///   `docs/design-federated-input-authority.md` for the full note.
///
/// - **`DashboardControl`**: holder is one daemon-scoped dashboard
///   control DataChannel session. It has no WS `direct_tx`; state
///   changes are personalized and pushed through the control tunnel's
///   event stream.
///
/// The map is `HashMap<u32, DisplayInputHolder>` — no `Option`, no
/// wrapper struct. Entry absence = unclaimed; that's the pre-phase-5
/// backwards-compat state where every connection's input flowed
/// through (now: only the holder's input flows through; everyone
/// else's is dropped at the gate, federated input is dropped
/// unconditionally until F-2 lights up the federated input gate).
#[derive(Clone, Debug)]
pub(crate) enum DisplayInputHolder {
    LocalWs {
        connection_id: String,
        /// Outbound channel for sending this WS connection's
        /// `display_input_authority_revoked` confirmation when a
        /// later grant preempts this holder. Local-only — the
        /// federated path uses the personalized authority-state
        /// broadcast for the same notification.
        direct_tx: mpsc::UnboundedSender<String>,
    },
    FederatedWebRtc {
        federation_connection_id: String,
        session_id: String,
    },
    DashboardControl {
        session_id: String,
    },
}

impl DisplayInputHolder {
    /// True iff this holder is `LocalWs` with the given `connection_id`.
    /// Used by local gate / personalization sites; deliberately returns
    /// false for `FederatedWebRtc` rather than panicking, so a future
    /// caller that mistakenly passes a connection id from the federated
    /// side gets a silent drop rather than mis-authorization.
    pub(crate) fn matches_local_ws(&self, connection_id: &str) -> bool {
        match self {
            Self::LocalWs {
                connection_id: c, ..
            } => c == connection_id,
            Self::FederatedWebRtc { .. } | Self::DashboardControl { .. } => false,
        }
    }

    /// True iff this holder is `FederatedWebRtc` with the given
    /// `(federation_connection_id, session_id)` pair. Used by the
    /// federated input gate (in F-2) and the federated close-cleanup
    /// path.
    pub(crate) fn matches_federated(&self, federation_connection_id: &str, session_id: &str) -> bool {
        match self {
            Self::FederatedWebRtc {
                federation_connection_id: c,
                session_id: s,
            } => c == federation_connection_id && s == session_id,
            Self::LocalWs { .. } | Self::DashboardControl { .. } => false,
        }
    }

    /// True iff this holder is a daemon-scoped dashboard control
    /// session with the given `session_id`.
    pub(crate) fn matches_dashboard_control(&self, session_id: &str) -> bool {
        match self {
            Self::DashboardControl { session_id: s } => s == session_id,
            Self::LocalWs { .. } | Self::FederatedWebRtc { .. } => false,
        }
    }

    /// True iff `self` and `other` identify the same holder
    /// (provenance + identity). Used by release / preempt sites where
    /// we need to compare the requesting holder against the current
    /// one without unwrapping the variant manually. Deliberately
    /// ignores `direct_tx` (which isn't equality-comparable and isn't
    /// part of identity — it's a notification handle that can change
    /// if the same WS connection rebuilds its outbound queue).
    ///
    /// Distinct from a `PartialEq` impl on purpose: spelled-out method
    /// at call sites makes intent explicit and prevents accidental
    /// equality-comparison pitfalls in collections / `.contains()` /
    /// pattern guards.
    ///
    /// Production callers don't need this yet — every F-1 / F-2
    /// release-or-preempt site already knows which provenance kind it's
    /// matching against and uses `matches_local_ws` /
    /// `matches_federated` directly. The method is pinned by unit
    /// tests as the documented identity-equality contract for future
    /// arbitration work (e.g. F-2's per-primary multi-operator
    /// scoping, where the comparison is against an opaque
    /// `DisplayInputHolder` snapshot).
    #[allow(dead_code)]
    pub(crate) fn same_identity(&self, other: &DisplayInputHolder) -> bool {
        match (self, other) {
            (
                Self::LocalWs {
                    connection_id: a, ..
                },
                Self::LocalWs {
                    connection_id: b, ..
                },
            ) => a == b,
            (
                Self::FederatedWebRtc {
                    federation_connection_id: ca,
                    session_id: sa,
                },
                Self::FederatedWebRtc {
                    federation_connection_id: cb,
                    session_id: sb,
                },
            ) => ca == cb && sa == sb,
            (
                Self::DashboardControl { session_id: a },
                Self::DashboardControl { session_id: b },
            ) => a == b,
            _ => false,
        }
    }
}

/// Phase 5a.1: dedicated internal broadcast event for display input
/// authority transitions.
///
/// Carries the holder's *server-internal* identity (or `None` for
/// unclaimed) so each WS outbound task can personalize this for its
/// own browser as `you | other | unclaimed` without ever shipping
/// holder IDs to browsers.  Personalization happens in the
/// per-connection outbound select arm where `connection_id_outbound`
/// is in scope.
///
/// Distinct from [`AppEvent`] on purpose: the generic outbound
/// broadcast carries already-serialized JSON strings, which would leak
/// holder IDs if we routed authority through it.  A dedicated typed
/// channel keeps the holder identity inside the gateway and forces
/// every per-connection consumer to compute its own personalized state.
#[derive(Clone, Debug)]
pub(crate) struct DisplayInputAuthorityChange {
    pub(crate) display_id: u32,
    pub(crate) holder: Option<DisplayInputHolder>,
}

/// Build the per-peer "may this connection inject input now?" closure
/// for the local `/ws` display-offer path (Phase 5a.1).
///
/// Returns a closure that consults the live authority map every time
/// it's called, so a grant or release elsewhere takes effect on the
/// very next data-channel input event without needing to reconstruct
/// the closure or rebuild the peer connection.
///
/// Semantics:
/// - `auth.get(display_id) == Some(entry)` and
///   `entry.matches_local_ws(this_id)` → `true`
///   (this WS connection holds authority)
/// - `auth.get(display_id) == Some(entry)` and
///   `!entry.matches_local_ws(this_id)` → `false`
///   (someone else — local or, once the variant lands, federated —
///   holds it; silent drop)
/// - `auth.get(display_id) == None`
///   → `true` (unclaimed = pre-phase-5 default; any connection can
///   input)
///
/// The federated path does NOT call this; it has its own deny-by-
/// default authorizer that becomes a `FederatedWebRtc` registry
/// lookup in F-1's later commits.
pub(crate) fn build_local_ws_input_authorizer(
    display_id: u32,
    connection_id: String,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(move || {
        let auth = authority.read().unwrap_or_else(|e| e.into_inner());
        match auth.get(&display_id) {
            Some(entry) => entry.matches_local_ws(&connection_id),
            None => true,
        }
    })
}

/// Capacity of the [`DisplayInputAuthorityChange`] broadcast channel.
///
/// Sized to comfortably absorb a burst of grants/releases across a few
/// dozen connected browsers — typically 1-3 events per user action,
/// fanned out across all WS connections.  64 is plenty of headroom and
/// cheap; lagged subscribers fall back to a fresh personalized snapshot
/// path (see the `Lagged` arm in the outbound select).
pub(crate) const AUTHORITY_CHANGE_CAPACITY: usize = 64;

/// F-2: federated path's input-authorization closure. Returns `true`
/// iff the current holder for `display_id` is `FederatedWebRtc` matching
/// THIS peer's `(federation_connection_id, session_id)`. Anything else
/// — no holder, a `LocalWs` holder, a `FederatedWebRtc` with a different
/// session id (e.g. another tab from the same primary), or a different
/// connection — returns `false` and the federated input handler drops
/// the event silently.
///
/// Symmetric in shape to [`build_local_ws_input_authorizer`], but with
/// strict deny-by-default for the unclaimed case: local 5c treats `None`
/// as "anyone may input" for pre-phase-5 backwards compatibility, while
/// the federated path has no such legacy and treats `None` as "nobody
/// holds this — drop everything." A federated browser only sends input
/// when its chip is `'you'` (UX-side guard); receiving input here under
/// any other condition is a protocol bug or a stale post-release race
/// and silent drop is correct.
///
/// The closure is the entire boundary: `display/mod.rs` invokes it per
/// event and never sees the registry, the holder identity, or the
/// connection/session IDs. F-2's gate flip is the single semantic change
/// from F-1's `Arc::new(|| false)` deny-everything stub.
pub(crate) fn build_federated_input_authorizer(
    display_id: u32,
    federation_connection_id: String,
    session_id: String,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(move || {
        let auth = authority.read().unwrap_or_else(|e| e.into_inner());
        match auth.get(&display_id) {
            Some(entry) => entry.matches_federated(&federation_connection_id, &session_id),
            None => false,
        }
    })
}

/// Apply a `RequestDisplayInputAuthority`.  Inserts the new holder,
/// returns the prior holder if any, sends `display_input_authority_revoked`
/// to the prior holder (if displaced), and emits the personalized
/// authority change for fan-out.  Caller is responsible for the
/// `display_input_authority_granted` confirm to `requester_direct_tx`
/// and the bus log message — both stay at the call site to keep the
/// helper's surface narrow (no logging dependency, no second send to
/// the same channel).
///
/// Lock discipline: the `authority` write guard is dropped before any
/// `direct_tx.send` or `authority_change_tx.send` call.
pub(crate) fn apply_grant_input_authority(
    display_id: u32,
    requester_connection_id: String,
    requester_direct_tx: mpsc::UnboundedSender<String>,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Option<DisplayInputHolder> {
    let new_holder = DisplayInputHolder::LocalWs {
        connection_id: requester_connection_id.clone(),
        direct_tx: requester_direct_tx,
    };
    // Clone for the broadcast — broadcast recipients personalize
    // from holder identity (the channel-clone in LocalWs is unused
    // downstream but cheap because mpsc::UnboundedSender is
    // Arc-backed).
    let broadcast_holder = new_holder.clone();
    let prior = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        map.insert(display_id, new_holder)
    };
    // Only `LocalWs` prior holders get the direct revoke confirmation
    // — `direct_tx` is local-only by design (see `DisplayInputHolder`
    // doc). A `FederatedWebRtc` prior holder learns of the preempt
    // through the personalized authority-state broadcast on its own
    // `display_input_authority` data channel.
    if let Some(DisplayInputHolder::LocalWs {
        connection_id: prior_id,
        direct_tx: prior_tx,
    }) = prior.as_ref()
    {
        if prior_id != &requester_connection_id {
            let notify = serde_json::json!({
                "t": "display_input_authority_revoked",
                "display_id": display_id,
                "reason": "another connection requested control",
            })
            .to_string();
            let _ = prior_tx.send(notify);
        }
    }
    let _ = authority_change_tx.send(DisplayInputAuthorityChange {
        display_id,
        holder: Some(broadcast_holder),
    });
    prior
}

/// Apply a `ReleaseDisplayInputAuthority`.  No-op if the calling
/// connection isn't the holder (prevents A from unclaiming B's slot).
/// Returns `true` iff the slot was actually released.  Emits the
/// personalized authority change with `None` only when the release
/// took effect — a no-op release does not flip anyone's UI state.
///
/// Lock discipline: matches [`apply_grant_input_authority`].
pub(crate) fn apply_release_input_authority(
    display_id: u32,
    releaser_connection_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> bool {
    let removed = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        match map.get(&display_id) {
            Some(entry) if entry.matches_local_ws(releaser_connection_id) => {
                map.remove(&display_id);
                true
            }
            _ => false,
        }
    };
    if removed {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id,
            holder: None,
        });
    }
    removed
}

/// F-1.3b: federated grant. Constructs a `FederatedWebRtc` holder
/// from `(federation_connection_id, session_id)`, inserts it into
/// the registry, returns the prior holder if any, and emits the
/// personalized authority change for fan-out.
///
/// Mirrors [`apply_grant_input_authority`] for the local path but
/// is provenance-distinct: federated holders carry no `direct_tx`
/// (federated state always flows through the personalized
/// authority-state broadcast on the federated WebRtcPeer's
/// `display_input_authority` data channel — see the F-1 design
/// note in `DisplayInputHolder`).
///
/// Prior holder revocation:
/// - If prior is `LocalWs`, send the existing
///   `display_input_authority_revoked` notification on the prior
///   holder's `direct_tx`. Same protocol as a local→local handover.
/// - If prior is `FederatedWebRtc` with a different identity, no
///   direct revoke — the broadcast-driven personalized state
///   `"other"` reaches that prior federated holder via its own
///   authority data channel and updates its chip.
///
/// Lock discipline: matches [`apply_grant_input_authority`].
pub(crate) fn apply_grant_input_authority_federated(
    display_id: u32,
    federation_connection_id: String,
    session_id: String,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Option<DisplayInputHolder> {
    let new_holder = DisplayInputHolder::FederatedWebRtc {
        federation_connection_id: federation_connection_id.clone(),
        session_id: session_id.clone(),
    };
    let broadcast_holder = new_holder.clone();
    let prior = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        map.insert(display_id, new_holder)
    };
    // Prior LocalWs holder gets the legacy direct revoke; prior
    // FederatedWebRtc gets nothing here because the personalized
    // broadcast below carries `"other"` to it on its own data channel.
    if let Some(DisplayInputHolder::LocalWs {
        direct_tx: prior_tx,
        ..
    }) = prior.as_ref()
    {
        let notify = serde_json::json!({
            "t": "display_input_authority_revoked",
            "display_id": display_id,
            "reason": "another connection requested control",
        })
        .to_string();
        let _ = prior_tx.send(notify);
    }
    let _ = authority_change_tx.send(DisplayInputAuthorityChange {
        display_id,
        holder: Some(broadcast_holder),
    });
    prior
}

/// F-1.3b: federated release. No-op if the calling
/// `(federation_connection_id, session_id)` doesn't match the
/// current holder (prevents one federated session from unclaiming
/// another's slot — even from the same primary, distinct
/// `PeerDisplayConnection` tabs have distinct `session_id`s).
/// Returns `true` iff the slot was actually released.
///
/// Lock discipline: matches [`apply_grant_input_authority_federated`].
pub(crate) fn apply_release_input_authority_federated(
    display_id: u32,
    federation_connection_id: &str,
    session_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> bool {
    let removed = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        match map.get(&display_id) {
            Some(entry) if entry.matches_federated(federation_connection_id, session_id) => {
                map.remove(&display_id);
                true
            }
            _ => false,
        }
    };
    if removed {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id,
            holder: None,
        });
    }
    removed
}

pub(crate) fn dashboard_control_authority_state_frame(
    session_id: &str,
    display_id: u32,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> serde_json::Value {
    let state = {
        let auth = authority.read().unwrap_or_else(|e| e.into_inner());
        match auth.get(&display_id) {
            Some(entry) if entry.matches_dashboard_control(session_id) => "you",
            Some(_) => "other",
            None => "unclaimed",
        }
    };
    serde_json::json!({
        "t": "display_input_authority_state",
        "display_id": display_id,
        "state": state,
    })
}

pub(crate) fn dashboard_control_authority_snapshot_frames(
    session_id: &str,
    display_ids: &[u32],
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> Vec<serde_json::Value> {
    display_ids
        .iter()
        .map(|display_id| {
            dashboard_control_authority_state_frame(session_id, *display_id, authority)
        })
        .collect()
}

pub(crate) fn apply_grant_input_authority_dashboard_control(
    display_id: u32,
    session_id: String,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Option<DisplayInputHolder> {
    let new_holder = DisplayInputHolder::DashboardControl {
        session_id: session_id.clone(),
    };
    let broadcast_holder = new_holder.clone();
    let prior = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        map.insert(display_id, new_holder)
    };
    if let Some(DisplayInputHolder::LocalWs {
        direct_tx: prior_tx,
        ..
    }) = prior.as_ref()
    {
        let notify = serde_json::json!({
            "t": "display_input_authority_revoked",
            "display_id": display_id,
            "reason": "another connection requested control",
        })
        .to_string();
        let _ = prior_tx.send(notify);
    }
    let _ = authority_change_tx.send(DisplayInputAuthorityChange {
        display_id,
        holder: Some(broadcast_holder),
    });
    prior
}

pub(crate) fn apply_release_input_authority_dashboard_control(
    display_id: u32,
    session_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> bool {
    let removed = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        match map.get(&display_id) {
            Some(entry) if entry.matches_dashboard_control(session_id) => {
                map.remove(&display_id);
                true
            }
            _ => false,
        }
    };
    if removed {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id,
            holder: None,
        });
    }
    removed
}

pub(crate) fn apply_dashboard_control_close_input_authority(
    session_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Vec<u32> {
    let released: Vec<u32> = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        map.retain(|did, entry| {
            if entry.matches_dashboard_control(session_id) {
                out.push(*did);
                false
            } else {
                true
            }
        });
        out
    };
    for did in &released {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id: *did,
            holder: None,
        });
    }
    released
}

pub(crate) fn dashboard_control_input_authorized(
    session_id: &str,
    display_id: u32,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> bool {
    let auth = authority.read().unwrap_or_else(|e| e.into_inner());
    match auth.get(&display_id) {
        Some(entry) => entry.matches_dashboard_control(session_id),
        None => true,
    }
}

/// F-1.3b: federated WS-close cleanup. Releases every
/// `FederatedWebRtc` entry whose `federation_connection_id` matches
/// the dropping federation transport, regardless of `session_id`
/// (the WS drop kills every `PeerDisplayConnection` session multiplexed
/// over that primary's federation transport). Emits one `None`-holder
/// authority change per affected display so other viewers' chips
/// flip back to `unclaimed`.
///
/// Distinct from [`apply_ws_close_input_authority`] which targets
/// `LocalWs` entries: a single `connection_id` is either acting as
/// a local browser or a federation transport but not both, so the
/// two cleanup paths address disjoint registry entries. Both fire
/// from the same WS-close hook (the gateway calls them in sequence).
///
/// Lock discipline: matches [`apply_grant_input_authority_federated`].
pub(crate) fn apply_federated_ws_close_input_authority(
    federation_connection_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Vec<u32> {
    let released: Vec<u32> = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        map.retain(|did, entry| match entry {
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: c,
                ..
            } if c == federation_connection_id => {
                out.push(*did);
                false
            }
            _ => true,
        });
        out
    };
    for did in &released {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id: *did,
            holder: None,
        });
    }
    released
}

// ---------------------------------------------------------------------------
// F-1.3b3: federated authority subscriber registry + helpers
//
// The federated counterpart to local 5c's per-WS subscriber model.
// Local 5c has no shared subscriber registry — each WS outbound task
// subscribes to `authority_change_tx` directly and personalizes for
// its own `connection_id`. Federated needs a registry because the
// send target is `WebRtcPeer::send_authority_state`, not a WS
// `direct_tx`: the gateway must hold an `Arc<WebRtcPeer>` to push to,
// and that handle isn't available until `handle_offer` returns and
// the peer is stored in the session.
//
// One entry per `(federation_connection_id, session_id, display_id)` —
// uniquely identifies one federated `PeerDisplayConnection`'s
// subscription to one display's authority state. Each entry owns a
// fanout task + a `CancellationToken` for clean teardown on the two
// distinct cleanup edges:
//
// 1. `WebRtcSignal::Close` / `DisplaySession::remove_peer(peer_id)`:
//    unregister this exact `(federation_connection_id, session_id,
//    display_id)` entry. Identity-matched authority release runs
//    alongside via `apply_release_input_authority_federated`.
// 2. Federation WS close: unregister all entries for that
//    `federation_connection_id`. Bulk authority release runs
//    alongside via `apply_federated_ws_close_input_authority`.
// ---------------------------------------------------------------------------

/// One federated authority subscriber. Holds the cancellation token
/// that terminates the per-subscriber fanout task on cleanup.
///
/// The `Arc<WebRtcPeer>` push target lives entirely inside the
/// fanout task spawned by [`register_federated_authority_subscriber`];
/// the registry doesn't carry a second copy because nothing reads
/// it back. The Drop chain is: cleanup edge calls `shutdown.cancel()`
/// → fanout task exits → its captured peer Arc drops → reference
/// count to the `WebRtcPeer` decrements. Any peer-teardown work that
/// the registry needs (e.g. tearing down WebRtcPeers on federation
/// WS-close) lives separately at the gateway level via
/// [`peer_id_for_federated_session`] + `DisplaySession::remove_peer`,
/// not by holding a duplicate Arc here.
pub(crate) struct FederatedAuthoritySubscriber {
    shutdown: tokio_util::sync::CancellationToken,
}

/// Stable mapping from a federated `session_id` (the
/// browser-supplied per-`PeerDisplayConnection` id round-tripped in
/// `ControlMsg::WebRtcSignal`) to the [`crate::display::PeerId`]
/// (`u64`) used as the `WebRtcPeer` key inside `DisplaySession`.
///
/// Used in two places that must agree exactly:
/// 1. [`handle_federated_webrtc_signal`] — derives the key on
///    Offer/IceCandidate/Close so subsequent signals route to the
///    same peer.
/// 2. WS-close cleanup — derives the key from each `(session_id,
///    display_id)` returned by
///    [`unregister_all_federated_subscribers_for_connection`] so
///    the federation WS-close can call `DisplaySession::remove_peer`
///    on every WebRtcPeer owned by the dropping connection.
///
/// A divergence between the two callers would leak peers (cleanup
/// would target a different key than was inserted on Offer), which
/// is exactly the bug fixed by extracting this helper.
pub(crate) fn peer_id_for_federated_session(session_id: &str) -> crate::display::PeerId {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    session_id.hash(&mut h);
    h.finish()
}

/// Gateway-side registry of federated authority subscribers, keyed by
/// `(federation_connection_id, session_id, display_id)`. Owned by the
/// gateway listener task; cloned per-WS for the inbound handler so
/// every per-connection branch can register/unregister without
/// passing the registry through every helper signature.
pub(crate) type FederatedAuthoritySubscribers =
    Arc<StdRwLock<HashMap<(String, String, u32), FederatedAuthoritySubscriber>>>;

/// Compute the personalized authority state for one federated
/// subscriber from a `Option<&DisplayInputHolder>`. Returns `You` if
/// the holder is a `FederatedWebRtc` matching this subscriber's
/// `(federation_connection_id, session_id)`, `Other` if any other
/// holder exists, `Unclaimed` if no one holds. Mirrors the local 5c
/// outbound personalization logic at the per-WS subscriber loop.
pub(crate) fn personalize_authority_for_federated(
    holder: Option<&DisplayInputHolder>,
    federation_connection_id: &str,
    session_id: &str,
) -> crate::display::webrtc::DisplayInputAuthorityState {
    use crate::display::webrtc::DisplayInputAuthorityState;
    match holder {
        Some(h) if h.matches_federated(federation_connection_id, session_id) => {
            DisplayInputAuthorityState::You
        }
        Some(_) => DisplayInputAuthorityState::Other,
        None => DisplayInputAuthorityState::Unclaimed,
    }
}

/// Build the federated authority data-channel handler closure.
///
/// The handler is invoked by the WebRTC driver on every parsed
/// [`crate::display::webrtc::AuthorityChannelMessage`] received on the
/// `display_input_authority` channel. Identity is captured at
/// construction time, so messages from this peer always apply
/// authority changes against this peer's
/// `(federation_connection_id, session_id)` — there's no way for one
/// federated session to act on behalf of another, even from the same
/// primary.
///
/// Display-ID mismatches are silently dropped: the federated peer's
/// `PeerDisplayConnection` is bound to one display, so a request for
/// any other display is a protocol bug on the browser side rather
/// than a recoverable condition. Authority gating still applies on
/// the input-injection path (F-2's job), so a misdirected message
/// here can't bypass anything.
pub(crate) fn build_federated_authority_handler(
    display_id: u32,
    federation_connection_id: String,
    session_id: String,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: broadcast::Sender<DisplayInputAuthorityChange>,
    allow_input_authority: bool,
) -> crate::display::webrtc::AuthorityChannelHandler {
    use crate::display::webrtc::AuthorityChannelMessage;
    Arc::new(move |msg| match msg {
        AuthorityChannelMessage::Request {
            display_id: req_did,
        } if req_did == display_id && allow_input_authority => {
            apply_grant_input_authority_federated(
                display_id,
                federation_connection_id.clone(),
                session_id.clone(),
                &authority,
                &authority_change_tx,
            );
        }
        AuthorityChannelMessage::Request { .. } if !allow_input_authority => {
            // Read-only peer profile: view signaling is allowed, input authority
            // requests are ignored. The input-injection authorizer below is also
            // deny-all, so a malformed client cannot bypass this by sending input
            // without first becoming the holder.
        }
        AuthorityChannelMessage::Release {
            display_id: req_did,
        } if req_did == display_id => {
            apply_release_input_authority_federated(
                display_id,
                &federation_connection_id,
                &session_id,
                &authority,
                &authority_change_tx,
            );
        }
        AuthorityChannelMessage::Request { .. } | AuthorityChannelMessage::Release { .. } => {
            // Display-ID mismatch — drop silently. See doc comment.
        }
    })
}

/// Register a federated authority subscriber and start its fanout
/// task. Called from the federated `Offer` arm after a successful
/// `DisplaySession::handle_offer` and `get_peer` lookup.
///
/// Behavior, in order:
/// 1. Subscribe to `authority_change_tx` FIRST. Doing this before
///    the snapshot read closes the race where a holder change
///    arrives between the registry read and the subscribe — without
///    this ordering, that change would land on neither the snapshot
///    nor the fanout, and the chip would end up stale until the
///    next change.
/// 2. Compute the initial personalized snapshot from the current
///    registry state and send it via `peer.send_authority_state`.
///    F-1.2's pending-authority queue absorbs the case where the
///    `display_input_authority` data channel hasn't opened yet on
///    the federated browser side — the queued state flushes on
///    `OnDataChannel(OnOpen)` so the chip cannot start stuck on
///    `unknown`.
/// 3. Spawn the fanout task with the rx from step 1. It
///    personalizes each inbound change for this subscriber's
///    identity and pushes via `peer.send_authority_state`. Lagged
///    subscribers re-snapshot from the registry — same recovery
///    pattern as the local 5c lagged path so a momentary catch-up
///    cannot leave the chip on stale state.
/// 4. Insert the entry into `subscribers` keyed by
///    `(federation_connection_id, session_id, display_id)` so
///    cleanup edges can reach it.
///
/// Snapshot-vs-change ordering across the wire is FIFO via
/// `WebRtcPeer::send_authority_state`'s underlying `Command`
/// channel. If a change races the initial snapshot, both land on
/// the channel in the order they were enqueued; the more recent
/// one wins on the browser side, so the chip ends up correct
/// regardless of which arrives last.
pub(crate) fn register_federated_authority_subscriber(
    federation_connection_id: String,
    session_id: String,
    display_id: u32,
    peer: Arc<crate::display::webrtc::WebRtcPeer>,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: broadcast::Sender<DisplayInputAuthorityChange>,
    subscribers: FederatedAuthoritySubscribers,
) {
    // 1. Subscribe BEFORE snapshot — closes the race window where a
    //    change between snapshot read and subscribe lands on neither
    //    path.
    let mut auth_rx = authority_change_tx.subscribe();

    // 2. Initial snapshot. F-1.2's queue handles "channel not open yet."
    let initial_state = {
        let map = authority.read().unwrap_or_else(|e| e.into_inner());
        personalize_authority_for_federated(
            map.get(&display_id),
            &federation_connection_id,
            &session_id,
        )
    };
    let peer_for_initial = Arc::clone(&peer);
    tokio::spawn(async move {
        let _ = peer_for_initial
            .send_authority_state(display_id, initial_state)
            .await;
    });

    // 3. Fanout task.
    let shutdown = tokio_util::sync::CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task_authority = Arc::clone(&authority);
    let task_fcid = federation_connection_id.clone();
    let task_sid = session_id.clone();
    let task_did = display_id;
    let task_peer = peer; // moved — registry doesn't keep a copy.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = task_shutdown.cancelled() => break,
                msg = auth_rx.recv() => match msg {
                    Ok(change) if change.display_id == task_did => {
                        let state = personalize_authority_for_federated(
                            change.holder.as_ref(),
                            &task_fcid,
                            &task_sid,
                        );
                        let _ = task_peer
                            .send_authority_state(task_did, state)
                            .await;
                    }
                    Ok(_) => {} // change for a different display
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Re-snapshot from registry — same recovery
                        // pattern as the local 5c lagged subscriber so
                        // the chip is never left stuck on stale state.
                        let state = {
                            let map = task_authority
                                .read()
                                .unwrap_or_else(|e| e.into_inner());
                            personalize_authority_for_federated(
                                map.get(&task_did),
                                &task_fcid,
                                &task_sid,
                            )
                        };
                        let _ = task_peer
                            .send_authority_state(task_did, state)
                            .await;
                    }
                }
            }
        }
    });

    // 4. Insert into the registry. Replace-on-collision: a duplicate
    //    `(fcid, sid, did)` would mean a renegotiated peer for the
    //    same identity; cancel the prior shutdown to terminate its
    //    fanout task before the new entry takes over.
    if let Some(prior) = subscribers
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            (federation_connection_id, session_id, display_id),
            FederatedAuthoritySubscriber { shutdown },
        )
    {
        prior.shutdown.cancel();
    }
}

/// Unregister one federated authority subscriber by exact identity.
/// Called from the federated `Close` arm. Cancels the fanout task
/// and removes the entry. Returns `true` if an entry was removed.
///
/// Does NOT release authority — that's
/// `apply_release_input_authority_federated`'s responsibility, called
/// alongside this function. Splitting the two keeps each helper
/// single-purpose: this one manages subscriber lifecycle, the other
/// manages the holder map.
pub(crate) fn unregister_federated_authority_subscriber(
    federation_connection_id: &str,
    session_id: &str,
    display_id: u32,
    subscribers: &FederatedAuthoritySubscribers,
) -> bool {
    if let Some(sub) = subscribers
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(
            federation_connection_id.to_string(),
            session_id.to_string(),
            display_id,
        ))
    {
        sub.shutdown.cancel();
        true
    } else {
        false
    }
}

/// Tear down every federated `WebRtcPeer` listed in `released`.
/// Called from the federation WS-close cleanup hook AFTER
/// [`unregister_all_federated_subscribers_for_connection`] returns
/// the surviving entries' `(session_id, display_id)` pairs. Without
/// this, the WebRTC data channels on those peers would stay alive
/// past the federation WS drop and could keep dispatching
/// `display_input_authority_request` frames against the registry —
/// the authority handler closure captures the
/// `federation_connection_id` at construction time, so a request
/// arriving after the WS-close would re-grant the (already-released)
/// authority under a now-defunct identity.
///
/// Tearing the peers down here is the structural fix: the federation
/// WS identity is the only thing tying these peers to a real user;
/// once it's gone the peers must go too. `DisplaySession::remove_peer`
/// closes the underlying WebRTC peer connection cleanly, which causes
/// every data channel on it to close and the driver task to exit —
/// no further authority frames can be processed.
///
/// Returns the count of peers actually removed. Missing displays
/// (display session torn down between Offer and WS-close) and
/// missing peers (already removed by an earlier `WebRtcSignal::Close`
/// for the same session) both fall through silently as no-ops on
/// `remove_peer`.
pub(crate) async fn close_federated_peers_for_sessions(
    released: &[(String, u32)],
    session_registry: Option<&Arc<tokio::sync::RwLock<crate::display::SessionRegistry>>>,
) -> usize {
    if released.is_empty() {
        return 0;
    }
    let Some(sr) = session_registry else {
        return 0;
    };
    // Snapshot Arcs out of the read guard first so per-peer awaits
    // (remove_peer's `peer.close()` chain) don't hold the registry
    // lock — same lock-discipline rationale as the local
    // `display_ice` handler that fixed the original 5-20s mDNS
    // starvation. The registry's RwLock is read-only here so a
    // concurrent display deactivate isn't blocked by us either way,
    // but keeping the pattern consistent prevents future regressions
    // if the lock semantics change.
    let mut targets: Vec<(Arc<crate::display::DisplaySession>, crate::display::PeerId)> =
        Vec::with_capacity(released.len());
    {
        let reg = sr.read().await;
        for (sid, did) in released {
            // `get_any`: teardown must reach every session that could
            // have accepted this connection's peers.
            if let Some(session) = reg.get_any(*did) {
                targets.push((session, peer_id_for_federated_session(sid)));
            }
        }
    }
    let count = targets.len();
    for (session, pid) in targets {
        session.remove_peer(pid).await;
    }
    count
}

/// Unregister every federated authority subscriber for a dropping
/// federation transport. Called from the WS-close cleanup hook
/// alongside [`apply_federated_ws_close_input_authority`]. Returns
/// the `(session_id, display_id)` pairs that were unregistered, for
/// caller logging and for the post-step
/// [`close_federated_peers_for_sessions`] which actually tears down
/// the WebRtcPeers.
pub(crate) fn unregister_all_federated_subscribers_for_connection(
    federation_connection_id: &str,
    subscribers: &FederatedAuthoritySubscribers,
) -> Vec<(String, u32)> {
    let mut released = Vec::new();
    let mut map = subscribers.write().unwrap_or_else(|e| e.into_inner());
    map.retain(|(fcid, sid, did), sub| {
        if fcid == federation_connection_id {
            released.push((sid.clone(), *did));
            sub.shutdown.cancel();
            false
        } else {
            true
        }
    });
    released
}

/// Apply WS-close cleanup for a dropping connection.  Removes every
/// authority entry held by `connection_id` and emits one `None`-holder
/// authority change per affected display so observers move from
/// `you/other` back to `unclaimed`.  Returns the list of released
/// display ids for caller logging / tests.
///
/// Lock discipline: matches [`apply_grant_input_authority`].
pub(crate) fn apply_ws_close_input_authority(
    connection_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Vec<u32> {
    let released: Vec<u32> = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        map.retain(|did, entry| {
            if entry.matches_local_ws(connection_id) {
                out.push(*did);
                false
            } else {
                true
            }
        });
        out
    };
    for did in &released {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id: *did,
            holder: None,
        });
    }
    released
}

/// Phase 5a.1 / 5c.2: build the personalized
/// `display_input_authority_state` snapshot a freshly-connecting browser
/// needs to bootstrap its chip from `unknown` to the authoritative state.
///
/// One entry per active display id, with `state` resolved against this
/// connection's id:
/// - `"you"` if `connection_id` currently holds the slot;
/// - `"other"` if some other connection holds it;
/// - `"unclaimed"` if no one holds it.
///
/// Holder connection ids never leave the daemon — the caller serializes
/// only the resolved `&'static str` into the `display_input_authority_state`
/// frame.
///
/// The frames built from this snapshot must be sent to `direct_tx`
/// **after** the `log_replay` block: replayed historical `display_ready` /
/// `user_display_revoked` events re-trigger `addDisplaySlot` /
/// `removeDisplaySlot` on the browser, which destroys the bootstrap slot
/// and creates a fresh one whose chip starts at `unknown`. Sending the
/// authority snapshot after replay guarantees it lands on the *final*
/// slot, so a late-connecting browser never gets stranded at `unknown`
/// for a display that already exists. See the
/// `bootstrap_authority_snapshots_*` tests for the regression coverage.
pub(crate) fn compute_bootstrap_authority_snapshots(
    active_display_ids: impl IntoIterator<Item = u32>,
    authority: &HashMap<u32, DisplayInputHolder>,
    connection_id: &str,
) -> Vec<(u32, &'static str)> {
    active_display_ids
        .into_iter()
        .map(|did| {
            let state = match authority.get(&did) {
                Some(entry) if entry.matches_local_ws(connection_id) => "you",
                Some(_) => "other",
                None => "unclaimed",
            };
            (did, state)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_gateway::tests::{seed_holder};

    // ---------------------------------------------------------------
    // Phase 5a.1: input-authority closure semantics + emission tests
    // ---------------------------------------------------------------

    /// Build an empty `display_input_authority` map of the production
    /// shape, for the helper-shape tests below.
    fn empty_authority_map() -> Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>> {
        Arc::new(StdRwLock::new(HashMap::new()))
    }

    /// Closure semantics: unclaimed map → authorized.  Matches the
    /// pre-phase-5 backwards-compat default; without this, the gate
    /// would block input on a fresh display that no one has claimed
    /// yet (regression hazard).
    #[test]
    fn local_ws_authorizer_returns_true_when_unclaimed() {
        let map = empty_authority_map();
        let authz = build_local_ws_input_authorizer(0, "conn-A".to_string(), map);
        assert!(authz(), "unclaimed display should authorize any connection");
    }

    /// Closure semantics: holder asks → authorized.  The on-going
    /// holder's input keeps flowing without re-asking.
    #[test]
    fn local_ws_authorizer_returns_true_for_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let authz = build_local_ws_input_authorizer(0, "conn-A".to_string(), map);
        assert!(authz(), "holder must remain authorized");
    }

    /// Closure semantics: non-holder asks → denied.  This is the
    /// silent-drop case — the closure returns false; the gate in
    /// `display/mod.rs::gated_input_handler` then drops the event.
    #[test]
    fn local_ws_authorizer_returns_false_for_non_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let authz = build_local_ws_input_authorizer(0, "conn-B".to_string(), map);
        assert!(
            !authz(),
            "non-holder must be denied even though display is claimed"
        );
    }

    /// Closure re-evaluates on every call — the gate must observe
    /// live grant/release transitions for a long-lived `WebRtcPeer`.
    /// Captured-snapshot semantics would freeze the gate at the value
    /// at construction time, breaking the take-control flow mid-session.
    #[test]
    fn local_ws_authorizer_re_evaluates_on_each_call() {
        let map = empty_authority_map();
        let authz = build_local_ws_input_authorizer(0, "conn-A".to_string(), Arc::clone(&map));
        assert!(authz(), "starts unclaimed → authorized");
        seed_holder(&map, 0, "conn-B");
        assert!(!authz(), "after seeding conn-B as holder → denied");
        // Replace holder with self.
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::LocalWs {
                connection_id: "conn-A".to_string(),
                direct_tx: mpsc::unbounded_channel().0,
            },
        );
        assert!(authz(), "after taking holder → re-authorized");
        // Release.
        map.write().unwrap_or_else(|e| e.into_inner()).remove(&0);
        assert!(authz(), "after release back to unclaimed → authorized");
    }

    /// `apply_grant_input_authority` emits a personalized authority
    /// change carrying `Some(holder)`.  The change flows through the
    /// broadcast channel; per-connection outbound tasks resolve the
    /// holder against their own id (via `matches_local_ws`) to produce
    /// `you|other|unclaimed` for browsers — the authoritative state
    /// the dashboard chip binds against.
    #[test]
    fn apply_grant_emits_authority_change_with_holder() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let (direct_tx, _direct_rx) = mpsc::unbounded_channel::<String>();
        let prior = apply_grant_input_authority(7, "conn-A".to_string(), direct_tx, &map, &auth_tx);
        assert!(prior.is_none(), "no prior holder on first grant");
        let change = auth_rx.try_recv().expect("authority change emitted");
        assert_eq!(change.display_id, 7);
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_local_ws("conn-A"))
                .unwrap_or(false),
            "broadcast holder must identify conn-A as the LocalWs holder"
        );
        // And the map records the new holder.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&7).unwrap().matches_local_ws("conn-A"),
            "registry entry must identify conn-A as LocalWs holder"
        );
    }

    /// A second grant from a different connection must auto-revoke
    /// the prior holder (matches Zoom's "granting auto-revokes prior"
    /// UX).  The prior holder receives a `display_input_authority_revoked`
    /// notification on its own direct_tx; the personalized change
    /// emits with the new holder's id.
    #[test]
    fn apply_grant_auto_revokes_prior_holder() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let (direct_tx_a, mut direct_rx_a) = mpsc::unbounded_channel::<String>();
        let (direct_tx_b, _direct_rx_b) = mpsc::unbounded_channel::<String>();

        // First grant to A.
        apply_grant_input_authority(7, "conn-A".to_string(), direct_tx_a.clone(), &map, &auth_tx);
        // Drain the first authority change.
        let _ = auth_rx.try_recv().expect("first grant emitted");

        // Second grant to B → A is auto-revoked.
        let prior =
            apply_grant_input_authority(7, "conn-B".to_string(), direct_tx_b, &map, &auth_tx);
        let prior_entry = prior.expect("prior holder returned");
        assert!(
            prior_entry.matches_local_ws("conn-A"),
            "prior holder must be conn-A"
        );

        // Authority change shows new holder.
        let change = auth_rx.try_recv().expect("second grant emitted");
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_local_ws("conn-B"))
                .unwrap_or(false),
            "broadcast holder must identify conn-B"
        );

        // A receives a revoked notification on its direct_tx.
        let notify = direct_rx_a
            .try_recv()
            .expect("prior holder gets display_input_authority_revoked");
        assert!(notify.contains("display_input_authority_revoked"));
        assert!(notify.contains("\"display_id\":7"));
    }

    /// `apply_release_input_authority` emits a `None`-holder change
    /// only when the release actually took effect (caller is the
    /// current holder).  No-op release does not emit.
    #[test]
    fn apply_release_emits_authority_change_with_none() {
        let map = empty_authority_map();
        seed_holder(&map, 7, "conn-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let removed = apply_release_input_authority(7, "conn-A", &map, &auth_tx);
        assert!(removed, "holder's release should succeed");
        let change = auth_rx.try_recv().expect("authority change emitted");
        assert_eq!(change.display_id, 7);
        assert!(change.holder.is_none(), "release emits None holder");
        assert!(map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&7)
            .is_none());
    }

    #[test]
    fn dashboard_control_authority_grant_release_and_cleanup() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);

        apply_grant_input_authority_dashboard_control(
            11,
            "control-session-A".to_string(),
            &map,
            &auth_tx,
        );
        let grant = auth_rx.try_recv().expect("dashboard grant emitted");
        assert_eq!(grant.display_id, 11);
        assert!(
            grant
                .holder
                .as_ref()
                .map(|holder| holder.matches_dashboard_control("control-session-A"))
                .unwrap_or(false),
            "broadcast holder must identify the dashboard-control session"
        );
        assert!(dashboard_control_input_authorized(
            "control-session-A",
            11,
            &map
        ));
        assert!(!dashboard_control_input_authorized(
            "control-session-B",
            11,
            &map
        ));
        assert_eq!(
            dashboard_control_authority_state_frame("control-session-A", 11, &map)["state"],
            "you"
        );
        assert_eq!(
            dashboard_control_authority_state_frame("control-session-B", 11, &map)["state"],
            "other"
        );

        let released = apply_release_input_authority_dashboard_control(
            11,
            "control-session-B",
            &map,
            &auth_tx,
        );
        assert!(!released, "non-holder release should be a no-op");
        assert!(auth_rx.try_recv().is_err(), "no-op release should not emit");

        apply_grant_input_authority_dashboard_control(
            12,
            "control-session-A".to_string(),
            &map,
            &auth_tx,
        );
        let _ = auth_rx.try_recv().expect("second dashboard grant emitted");
        let mut cleaned =
            apply_dashboard_control_close_input_authority("control-session-A", &map, &auth_tx);
        cleaned.sort_unstable();
        assert_eq!(cleaned, vec![11, 12]);
        let cleanup_a = auth_rx.try_recv().expect("first cleanup emitted");
        let cleanup_b = auth_rx.try_recv().expect("second cleanup emitted");
        assert!(cleanup_a.holder.is_none());
        assert!(cleanup_b.holder.is_none());
        assert!(dashboard_control_input_authorized(
            "control-session-B",
            11,
            &map
        ));
    }

    /// Release attempted by a non-holder is a no-op — prevents A from
    /// unclaiming B's slot.  No authority change is emitted.
    #[test]
    fn apply_release_is_noop_for_non_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 7, "conn-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let removed = apply_release_input_authority(7, "conn-B", &map, &auth_tx);
        assert!(!removed, "non-holder cannot unclaim");
        // No change emitted.
        assert!(
            auth_rx.try_recv().is_err(),
            "no authority change for no-op release"
        );
        // Original holder still in map.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&7).unwrap().matches_local_ws("conn-A"),
            "original holder conn-A must still be in registry after no-op release"
        );
    }

    /// WS-close cleanup releases every entry held by the dropping
    /// connection and emits one `None`-holder change per affected
    /// display.  Without this fan-out, browsers in `other` state
    /// after the dropping connection had taken control would stay
    /// stuck on stale UI.
    #[test]
    fn apply_ws_close_emits_authority_change_with_none_for_each_held_display() {
        let map = empty_authority_map();
        seed_holder(&map, 1, "conn-A");
        seed_holder(&map, 2, "conn-A");
        seed_holder(&map, 3, "conn-B");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let released = apply_ws_close_input_authority("conn-A", &map, &auth_tx);
        // A's two holdings released; B untouched.
        let mut released_sorted = released.clone();
        released_sorted.sort();
        assert_eq!(released_sorted, vec![1, 2]);
        assert!(map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&1)
            .is_none());
        assert!(map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&2)
            .is_none());
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&3).unwrap().matches_local_ws("conn-B"),
            "other connections' holdings preserved",
        );
        drop(map_guard);
        // One change emitted per released display, both with None.
        let mut events: Vec<DisplayInputAuthorityChange> = Vec::new();
        while let Ok(change) = auth_rx.try_recv() {
            events.push(change);
        }
        assert_eq!(events.len(), 2);
        for change in &events {
            assert!(change.holder.is_none());
            assert!(change.display_id == 1 || change.display_id == 2);
        }
    }

    /// WS-close for a connection that holds no slots → no events,
    /// empty release list.  Common case (non-controller dropping out).
    #[test]
    fn apply_ws_close_is_noop_when_no_slots_held() {
        let map = empty_authority_map();
        seed_holder(&map, 1, "conn-other");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let released = apply_ws_close_input_authority("conn-A", &map, &auth_tx);
        assert!(released.is_empty(), "no slots held → no releases");
        assert!(auth_rx.try_recv().is_err(), "no authority changes emitted");
        // Other holder untouched.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&1).unwrap().matches_local_ws("conn-other"),
            "other holder untouched after no-op close",
        );
    }

    // ===================================================================
    // F-1.3b: federated authority registry helpers
    // ===================================================================

    /// Seed a `FederatedWebRtc` holder directly into the map for tests
    /// that need to set up cross-provenance scenarios. Mirrors
    /// `seed_holder` for `LocalWs`.
    fn seed_federated_holder(
        map: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
        display_id: u32,
        federation_connection_id: &str,
        session_id: &str,
    ) {
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            display_id,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: federation_connection_id.to_string(),
                session_id: session_id.to_string(),
            },
        );
    }

    /// `matches_federated`: same `(federation_connection_id, session_id)`
    /// pair matches; mismatched connection or mismatched session does
    /// not. Pins the F-1 identity rule that one federation tab can't
    /// pose as another (even from the same primary).
    #[test]
    fn matches_federated_identity_check() {
        let h = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-conn-1".to_string(),
            session_id: "sess-A".to_string(),
        };
        assert!(h.matches_federated("fed-conn-1", "sess-A"));
        assert!(
            !h.matches_federated("fed-conn-1", "sess-B"),
            "same connection + different session must not match"
        );
        assert!(
            !h.matches_federated("fed-conn-2", "sess-A"),
            "different connection + same session must not match"
        );
        assert!(
            !h.matches_federated("fed-conn-2", "sess-B"),
            "fully-different identity must not match"
        );
    }

    /// `matches_federated` returns false for a `LocalWs` holder
    /// regardless of inputs. Cross-provenance equality is impossible.
    #[test]
    fn matches_federated_false_for_local_ws() {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let h = DisplayInputHolder::LocalWs {
            connection_id: "conn-A".to_string(),
            direct_tx: tx,
        };
        assert!(!h.matches_federated("conn-A", "sess-A"));
    }

    /// `matches_local_ws` returns false for a `FederatedWebRtc`
    /// holder regardless of inputs. Symmetric with the test above.
    #[test]
    fn matches_local_ws_false_for_federated() {
        let h = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-conn-1".to_string(),
            session_id: "sess-A".to_string(),
        };
        assert!(!h.matches_local_ws("fed-conn-1"));
    }

    /// `same_identity` distinguishes provenance even when string
    /// values collide. A `LocalWs { connection_id: "x" }` is NOT
    /// `same_identity` as `FederatedWebRtc { federation_connection_id:
    /// "x", session_id: "x" }` even though all the strings happen to
    /// match.
    #[test]
    fn same_identity_does_not_cross_provenance() {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let local = DisplayInputHolder::LocalWs {
            connection_id: "x".to_string(),
            direct_tx: tx,
        };
        let federated = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "x".to_string(),
            session_id: "x".to_string(),
        };
        assert!(!local.same_identity(&federated));
        assert!(!federated.same_identity(&local));
    }

    /// `apply_grant_input_authority_federated` first call inserts a
    /// `FederatedWebRtc` holder, returns no prior, emits the change
    /// with the new holder.
    #[test]
    fn apply_grant_federated_first_grant_no_prior() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let prior = apply_grant_input_authority_federated(
            7,
            "fed-conn-1".to_string(),
            "sess-A".to_string(),
            &map,
            &auth_tx,
        );
        assert!(prior.is_none(), "no prior on first grant");
        let change = auth_rx.try_recv().expect("change emitted");
        assert_eq!(change.display_id, 7);
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_federated("fed-conn-1", "sess-A"))
                .unwrap_or(false),
            "broadcast holder must be FederatedWebRtc(fed-conn-1, sess-A)"
        );
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard
                .get(&7)
                .unwrap()
                .matches_federated("fed-conn-1", "sess-A"),
            "registry must record the federated holder"
        );
    }

    /// **Cross-provenance handover**: a federated grant takes from a
    /// local holder. The local holder's `direct_tx` receives the
    /// `display_input_authority_revoked` notification (legacy local
    /// protocol); the broadcast change carries the new federated
    /// holder so other viewers personalize to "other".
    #[test]
    fn apply_grant_federated_takes_from_local_holder() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let (local_tx, mut local_rx) = mpsc::unbounded_channel::<String>();
        // Local holder.
        apply_grant_input_authority(7, "conn-LOCAL".to_string(), local_tx, &map, &auth_tx);
        let _ = auth_rx.try_recv().expect("local grant change");

        // Federated takes.
        let prior = apply_grant_input_authority_federated(
            7,
            "fed-conn-1".to_string(),
            "sess-A".to_string(),
            &map,
            &auth_tx,
        );
        let prior_entry = prior.expect("prior holder returned");
        assert!(
            prior_entry.matches_local_ws("conn-LOCAL"),
            "prior holder must be the local one"
        );

        // Local holder gets the legacy direct revoke.
        let revoke = local_rx
            .try_recv()
            .expect("local prior holder must receive direct revoke");
        assert!(revoke.contains("display_input_authority_revoked"));
        assert!(revoke.contains("\"display_id\":7"));

        // Broadcast carries the new federated holder.
        let change = auth_rx.try_recv().expect("broadcast change after handover");
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_federated("fed-conn-1", "sess-A"))
                .unwrap_or(false),
            "broadcast holder after handover must be the federated one"
        );
    }

    /// **Cross-provenance handover (other direction)**: a local grant
    /// takes from a federated holder. The federated holder gets NO
    /// direct revoke (federated state always flows through the
    /// personalized broadcast — see `DisplayInputHolder` doc).
    #[test]
    fn apply_grant_local_takes_from_federated_holder_no_direct_revoke() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Federated holder.
        apply_grant_input_authority_federated(
            7,
            "fed-conn-1".to_string(),
            "sess-A".to_string(),
            &map,
            &auth_tx,
        );
        let _ = auth_rx.try_recv().expect("federated grant change");

        // Local takes.
        let (local_tx, _local_rx) = mpsc::unbounded_channel::<String>();
        let prior =
            apply_grant_input_authority(7, "conn-LOCAL".to_string(), local_tx, &map, &auth_tx);
        let prior_entry = prior.expect("prior holder returned");
        assert!(
            prior_entry.matches_federated("fed-conn-1", "sess-A"),
            "prior holder must be the federated one"
        );

        // Federated holder is informed via the broadcast (handler
        // would compute "other" for this federated subscriber). The
        // direct-revoke path is not used for federated prior holders.
        let change = auth_rx.try_recv().expect("broadcast change after handover");
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_local_ws("conn-LOCAL"))
                .unwrap_or(false),
            "broadcast holder after handover must be the local one"
        );
    }

    /// Federated release succeeds only when the calling
    /// `(federation_connection_id, session_id)` matches the current
    /// holder. A different session on the same federation connection
    /// cannot unclaim.
    #[test]
    fn apply_release_federated_only_on_matching_identity() {
        let map = empty_authority_map();
        seed_federated_holder(&map, 7, "fed-conn-1", "sess-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);

        // Wrong session — no-op.
        let removed =
            apply_release_input_authority_federated(7, "fed-conn-1", "sess-B", &map, &auth_tx);
        assert!(!removed, "wrong session must not unclaim");
        assert!(auth_rx.try_recv().is_err(), "no change for no-op release");

        // Wrong connection — no-op.
        let removed =
            apply_release_input_authority_federated(7, "fed-conn-2", "sess-A", &map, &auth_tx);
        assert!(!removed, "wrong connection must not unclaim");
        assert!(auth_rx.try_recv().is_err(), "no change for no-op release");

        // Original holder still in map.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard
                .get(&7)
                .unwrap()
                .matches_federated("fed-conn-1", "sess-A"),
            "original federated holder still in registry"
        );
        drop(map_guard);

        // Correct identity — releases.
        let removed =
            apply_release_input_authority_federated(7, "fed-conn-1", "sess-A", &map, &auth_tx);
        assert!(removed, "matching identity must release");
        let change = auth_rx.try_recv().expect("change emitted on release");
        assert!(change.holder.is_none(), "release emits None");
        assert!(
            map.read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&7)
                .is_none(),
            "registry empty after release"
        );
    }

    /// Federated release is also no-op against a `LocalWs` holder —
    /// federated session can't unclaim a local one even if the IDs
    /// happen to collide.
    #[test]
    fn apply_release_federated_noop_on_local_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 7, "conn-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let removed =
            apply_release_input_authority_federated(7, "conn-A", "sess-X", &map, &auth_tx);
        assert!(
            !removed,
            "federated release must not unclaim a LocalWs holder"
        );
        assert!(auth_rx.try_recv().is_err(), "no change emitted");
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&7).unwrap().matches_local_ws("conn-A"),
            "local holder still in registry"
        );
    }

    /// Federated WS-close releases ALL `FederatedWebRtc` entries with
    /// matching `federation_connection_id`, regardless of `session_id`
    /// (the WS drop kills every session multiplexed over that primary's
    /// federation transport). Other federation connections' entries
    /// AND any local entries are untouched.
    #[test]
    fn apply_federated_ws_close_releases_all_sessions_on_dropping_connection() {
        let map = empty_authority_map();
        seed_federated_holder(&map, 1, "fed-conn-1", "sess-A");
        seed_federated_holder(&map, 2, "fed-conn-1", "sess-B");
        seed_federated_holder(&map, 3, "fed-conn-2", "sess-C");
        seed_holder(&map, 4, "conn-LOCAL");

        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(16);
        let released = apply_federated_ws_close_input_authority("fed-conn-1", &map, &auth_tx);
        let mut released_sorted = released.clone();
        released_sorted.sort();
        assert_eq!(
            released_sorted,
            vec![1, 2],
            "both sessions on fed-conn-1 must be released"
        );

        // Other federation connection's entry untouched.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard
                .get(&3)
                .unwrap()
                .matches_federated("fed-conn-2", "sess-C"),
            "other federation connection's entry untouched"
        );
        // Local entry untouched.
        assert!(
            map_guard.get(&4).unwrap().matches_local_ws("conn-LOCAL"),
            "local holder untouched"
        );
        drop(map_guard);

        // One change emitted per affected display, all with None.
        let mut events = Vec::new();
        while let Ok(change) = auth_rx.try_recv() {
            events.push(change);
        }
        assert_eq!(events.len(), 2);
        for change in &events {
            assert!(change.holder.is_none());
            assert!(change.display_id == 1 || change.display_id == 2);
        }
    }

    /// Federated WS-close with no matching entries → empty list, no
    /// events. Local entries with the same `connection_id` value are
    /// not touched (the function is provenance-scoped).
    #[test]
    fn apply_federated_ws_close_is_noop_with_no_matching_entries() {
        let map = empty_authority_map();
        seed_holder(&map, 1, "fed-conn-1");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let released = apply_federated_ws_close_input_authority("fed-conn-1", &map, &auth_tx);
        assert!(
            released.is_empty(),
            "no FederatedWebRtc entries with this connection — no releases"
        );
        assert!(auth_rx.try_recv().is_err(), "no change emitted");
        // Local entry with the same connection_id (rare but possible
        // if a single connection_id value is reused across phases) is
        // untouched by the federated cleanup.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&1).unwrap().matches_local_ws("fed-conn-1"),
            "LocalWs entry with same id value untouched by federated cleanup"
        );
    }

    /// F-2: positive — an authority entry of `FederatedWebRtc` matching
    /// this closure's `(federation_connection_id, session_id)`
    /// authorizes input. Mirrors the local 5c
    /// `local_ws_authorizer_returns_true_for_holder` shape.
    #[test]
    fn federated_input_authorizer_returns_true_for_matching_holder() {
        let map = empty_authority_map();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-A".to_string(),
            },
        );
        let authz = build_federated_input_authorizer(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
        );
        assert!(authz(), "matching identity must authorize input");
    }

    /// F-2: negative — unclaimed (`None`) is strict deny on the
    /// federated path. Different from local 5c (which treats `None`
    /// as "anyone may input" for backwards compat); federated has no
    /// such legacy.
    #[test]
    fn federated_input_authorizer_returns_false_when_no_holder() {
        let map = empty_authority_map();
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "unclaimed display must drop federated input — different \
             from local 5c's pre-phase-5 default-allow"
        );
    }

    /// F-2: negative — a `LocalWs` holder denies federated input.
    /// Mixed cross-provenance hold: local browser drives input; the
    /// federated browser's events are dropped at the gate.
    #[test]
    fn federated_input_authorizer_returns_false_when_local_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "local-conn-A");
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "LocalWs holder must drop federated input even though the \
             registry is non-empty"
        );
    }

    /// F-2: negative — same `federation_connection_id`, different
    /// `session_id`. Two tabs from the same primary; only one holds.
    /// The non-holding tab's events drop.
    #[test]
    fn federated_input_authorizer_returns_false_when_wrong_session() {
        let map = empty_authority_map();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-OTHER".to_string(),
            },
        );
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "same connection + different session must deny — distinct \
             tabs from the same primary don't share input authority"
        );
    }

    /// F-2: negative — different `federation_connection_id` (different
    /// primary). The federated holder belongs to a different primary's
    /// transport; this primary's federated browser must not be able to
    /// drive input on behalf of the other primary's session.
    #[test]
    fn federated_input_authorizer_returns_false_when_wrong_connection() {
        let map = empty_authority_map();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-OTHER".to_string(),
                session_id: "sess-A".to_string(),
            },
        );
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "different federation_connection_id must deny even when \
             session_id matches — distinct primaries are distinct \
             security boundaries"
        );
    }

    // ---------------------------------------------------------------
    // F-1.3b3: federated authority handler + subscriber registry
    // ---------------------------------------------------------------

    /// Test helper: build a stub `WebRtcPeer` via the existing
    /// `new_for_test` constructor. Send-authority-state calls against
    /// the returned peer will fail (its command_rx is dropped) but
    /// the registry-level tests below only inspect the subscriber
    /// map, never await on delivery.
    fn make_test_peer(peer_id: u64) -> Arc<crate::display::webrtc::WebRtcPeer> {
        use crate::display::encode::pool::SimulcastRid;
        use crate::display::webrtc::WebRtcPeer;
        Arc::new(WebRtcPeer::new_for_test(
            peer_id,
            vec![SimulcastRid::full()],
        ))
    }

    /// Build an empty subscriber registry of the production shape.
    fn empty_subscribers() -> FederatedAuthoritySubscribers {
        Arc::new(StdRwLock::new(HashMap::new()))
    }

    /// `personalize_authority_for_federated` returns `You` when the
    /// holder's identity matches this subscriber's
    /// `(federation_connection_id, session_id)`. Mirrors the local
    /// 5c outbound personalization at the per-WS subscriber loop.
    #[test]
    fn personalize_authority_for_federated_returns_you_on_match() {
        let holder = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-1".to_string(),
            session_id: "sess-A".to_string(),
        };
        let state = personalize_authority_for_federated(Some(&holder), "fed-1", "sess-A");
        assert_eq!(
            state,
            crate::display::webrtc::DisplayInputAuthorityState::You
        );
    }

    /// `personalize_authority_for_federated` returns `Other` when
    /// any holder exists that isn't this subscriber's identity. The
    /// "wrong session, same connection" case (two tabs from one
    /// primary) also resolves to `Other` — distinct session IDs
    /// don't collapse.
    #[test]
    fn personalize_authority_for_federated_returns_other_when_someone_else_holds() {
        let other_federated = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-1".to_string(),
            session_id: "sess-B".to_string(),
        };
        assert_eq!(
            personalize_authority_for_federated(Some(&other_federated), "fed-1", "sess-A"),
            crate::display::webrtc::DisplayInputAuthorityState::Other,
            "same connection, different session must be 'other'",
        );
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let local = DisplayInputHolder::LocalWs {
            connection_id: "local-conn".to_string(),
            direct_tx: tx,
        };
        assert_eq!(
            personalize_authority_for_federated(Some(&local), "fed-1", "sess-A"),
            crate::display::webrtc::DisplayInputAuthorityState::Other,
            "LocalWs holder must surface as 'other' to a federated subscriber",
        );
    }

    /// `personalize_authority_for_federated` returns `Unclaimed` when
    /// no holder is in the registry. Map absence is the canonical
    /// "no one holds" signal — no `Option` in the value type.
    #[test]
    fn personalize_authority_for_federated_returns_unclaimed_when_no_holder() {
        let state = personalize_authority_for_federated(None, "fed-1", "sess-A");
        assert_eq!(
            state,
            crate::display::webrtc::DisplayInputAuthorityState::Unclaimed
        );
    }

    /// The handler closure built by `build_federated_authority_handler`
    /// dispatches a `Request` to `apply_grant_input_authority_federated`,
    /// resulting in a holder bound to this peer's identity in the
    /// registry. Pins that the handler closure carries the right
    /// identity and that the dispatch shape is correct.
    #[test]
    fn build_federated_authority_handler_dispatches_request_to_grant() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
            true,
        );

        handler(AuthorityChannelMessage::Request { display_id: 0 });

        let guard = map.read().unwrap_or_else(|e| e.into_inner());
        match guard.get(&0) {
            Some(DisplayInputHolder::FederatedWebRtc {
                federation_connection_id,
                session_id,
            }) => {
                assert_eq!(federation_connection_id, "fed-1");
                assert_eq!(session_id, "sess-A");
            }
            other => panic!("expected FederatedWebRtc holder, got {other:?}"),
        }
    }

    /// `Release` against a holder of this same identity removes the
    /// entry from the registry. Pins the wire→registry round-trip.
    #[test]
    fn build_federated_authority_handler_dispatches_release_to_apply_release() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Seed a federated holder with the identity the handler was
        // built for.
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-A".to_string(),
            },
        );

        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
            true,
        );
        handler(AuthorityChannelMessage::Release { display_id: 0 });

        assert!(
            map.read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&0)
                .is_none(),
            "release with matching identity must remove the holder"
        );
    }

    /// `Release` on a holder of a DIFFERENT identity is a silent
    /// no-op — the F-1.3b1 helper enforces identity matching at the
    /// registry layer, and the handler can't bypass it. Two tabs from
    /// the same primary can't unclaim each other.
    #[test]
    fn build_federated_authority_handler_release_noop_on_wrong_identity() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Seed with a holder of a DIFFERENT session id.
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-OTHER".to_string(),
            },
        );

        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
            true,
        );
        handler(AuthorityChannelMessage::Release { display_id: 0 });

        let guard = map.read().unwrap_or_else(|e| e.into_inner());
        match guard.get(&0) {
            Some(DisplayInputHolder::FederatedWebRtc { session_id, .. }) => {
                assert_eq!(
                    session_id, "sess-OTHER",
                    "wrong-identity release must not remove the slot"
                );
            }
            other => panic!("expected slot to remain held by sess-OTHER, got {other:?}"),
        }
    }

    /// Display-ID mismatches drop silently. The federated peer's
    /// `PeerDisplayConnection` is bound to one display; a `Request`
    /// targeting any other display must not mutate the registry.
    #[test]
    fn build_federated_authority_handler_ignores_display_id_mismatch() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
            true,
        );

        handler(AuthorityChannelMessage::Request { display_id: 99 });
        handler(AuthorityChannelMessage::Release { display_id: 99 });

        assert!(
            map.read().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "display-id mismatch must not mutate the registry"
        );
    }

    #[test]
    fn build_federated_authority_handler_denies_request_when_profile_read_only() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
            false,
        );

        handler(AuthorityChannelMessage::Request { display_id: 0 });

        assert!(
            map.read().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "read-only profile must not grant federated input authority"
        );
    }

    /// `unregister_federated_authority_subscriber` removes the entry
    /// when the identity tuple matches and returns true. Cancellation
    /// of the spawned fanout task is a side effect of the cancel call
    /// on the stored token; not directly observable in this test, but
    /// the broadcast channel close on test exit reaps any orphaned
    /// task cleanly.
    #[tokio::test]
    async fn unregister_federated_authority_subscriber_removes_matching() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "register must insert one entry"
        );

        let removed = unregister_federated_authority_subscriber("fed-1", "sess-A", 0, &subscribers);

        assert!(removed, "matching unregister returns true");
        assert!(
            subscribers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "registry must be empty after unregister"
        );
    }

    /// `unregister_federated_authority_subscriber` returns false (and
    /// leaves the registry untouched) when no entry matches.
    #[tokio::test]
    async fn unregister_federated_authority_subscriber_noop_on_miss() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );

        let removed =
            unregister_federated_authority_subscriber("fed-1", "sess-OTHER", 0, &subscribers);
        assert!(!removed, "non-matching unregister returns false");
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "registry must be unchanged after non-matching unregister"
        );
    }

    /// Federation WS-close cleanup releases every subscriber whose
    /// `federation_connection_id` matches the dropping connection,
    /// regardless of `session_id` or `display_id`. Counterpart to
    /// `apply_federated_ws_close_input_authority`.
    #[tokio::test]
    async fn unregister_all_federated_subscribers_for_connection_releases_matching() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Three subscribers: two on fed-1 (different sessions, same
        // display), one on fed-2 (the survivor).
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-B".to_string(),
            0,
            make_test_peer(2),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        register_federated_authority_subscriber(
            "fed-2".to_string(),
            "sess-C".to_string(),
            0,
            make_test_peer(3),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            3
        );

        let released = unregister_all_federated_subscribers_for_connection("fed-1", &subscribers);

        assert_eq!(released.len(), 2, "two fed-1 entries released");
        let remaining: Vec<(String, String, u32)> = subscribers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            remaining,
            vec![("fed-2".to_string(), "sess-C".to_string(), 0)],
            "only fed-2 entry must remain"
        );
    }

    /// `unregister_all_federated_subscribers_for_connection` returns
    /// an empty vec and leaves the registry untouched when no entries
    /// match the dropping connection.
    #[tokio::test]
    async fn unregister_all_federated_subscribers_for_connection_noop_on_no_match() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-2".to_string(),
            "sess-C".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );

        let released = unregister_all_federated_subscribers_for_connection("fed-1", &subscribers);
        assert!(
            released.is_empty(),
            "no matching entries → empty release list"
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "registry unchanged"
        );
    }

    /// `register_federated_authority_subscriber` replaces an existing
    /// entry with the same `(fcid, sid, did)` key (renegotiated peer
    /// for the same identity). Map size stays at 1; the prior entry's
    /// shutdown token fires via the in-helper cancel path.
    #[tokio::test]
    async fn register_federated_authority_subscriber_replaces_on_collision() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(2),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "duplicate-key registration must replace, not append"
        );
    }

    // ---------------------------------------------------------------
    // F-1.3b3 fix #2: WS-close peer teardown — peer_id helper
    // determinism + close-helper edge cases. The actual
    // session.remove_peer side effect requires a real
    // DisplaySession (which needs a real backend) and is exercised
    // by the F-3 smoke; these unit tests pin the contract that
    // must hold for the smoke to be meaningful: the same session_id
    // hashes to the same PeerId on both the Offer (insert) and the
    // WS-close (cleanup) sides.
    // ---------------------------------------------------------------


    /// Distinct `session_id`s map to distinct `PeerId`s in
    /// practice. (`u64` hash collisions are theoretically possible
    /// but vanishingly unlikely between any two real session ids
    /// generated by the browser.) Without this property, two
    /// federated tabs from one primary would alias to the same
    /// `WebRtcPeer` slot — cleanup of one tab would tear down the
    /// other.
    #[test]
    fn peer_id_for_federated_session_distinct_for_distinct_sessions() {
        let a = peer_id_for_federated_session("sess-A");
        let b = peer_id_for_federated_session("sess-B");
        assert_ne!(
            a, b,
            "distinct session ids should produce distinct peer ids"
        );
    }

    /// `close_federated_peers_for_sessions` short-circuits to 0 on
    /// empty release input — covers the "WS-close fired but the
    /// connection had no federated subscribers" no-op path
    /// (typical: the connection was a local browser, not a
    /// federation transport).
    #[tokio::test]
    async fn close_federated_peers_for_sessions_noop_on_empty_release() {
        let reg = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let count = close_federated_peers_for_sessions(&[], Some(&reg)).await;
        assert_eq!(count, 0, "empty release must short-circuit");
    }

    /// `close_federated_peers_for_sessions` short-circuits on a
    /// `None` session_registry — the daemon may run without one
    /// (e.g. presence-disabled startup), and the WS-close path
    /// must not panic in that mode.
    #[tokio::test]
    async fn close_federated_peers_for_sessions_noop_on_no_registry() {
        let count = close_federated_peers_for_sessions(&[("sess-A".to_string(), 0)], None).await;
        assert_eq!(count, 0, "missing registry must short-circuit");
    }

    /// `close_federated_peers_for_sessions` returns 0 (and runs no
    /// `remove_peer` calls) when the listed displays aren't in the
    /// registry — covers the race where a display session gets
    /// deactivated between Offer-time and WS-close.
    #[tokio::test]
    async fn close_federated_peers_for_sessions_noop_when_displays_missing() {
        let reg = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let count = close_federated_peers_for_sessions(
            &[("sess-A".to_string(), 0), ("sess-B".to_string(), 1)],
            Some(&reg),
        )
        .await;
        assert_eq!(
            count, 0,
            "missing displays in the registry must fall through silently",
        );
    }

    // ---------------------------------------------------------------
    // Phase 5c.2: bootstrap snapshot regression — late-second browser
    // joining a daemon that already has an active display must end up
    // with its chip resolved to `you`/`other`/`unclaimed`, never stuck
    // at `unknown`.  The snapshot computation is the per-connection
    // personalization pass (the holder-id never reaches the wire).
    // ---------------------------------------------------------------

    /// Active display, no holder → `unclaimed` for the connecting browser.
    /// Covers the "fresh display granted before browser B connects, no one
    /// has clicked Take Control yet" case.
    #[test]
    fn bootstrap_authority_snapshots_unclaimed_when_no_holder() {
        let map = empty_authority_map();
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32], &auth, "conn-B");
        assert_eq!(snaps, vec![(0, "unclaimed")]);
    }

    /// Active display, browser A holds → connecting browser B sees `other`.
    /// This is the exact regression that left B's chip at `unknown`
    /// before slice 5c.2 — the bootstrap was sent but landed on the
    /// wrong slot, so this test pins the snapshot resolution.
    #[test]
    fn bootstrap_authority_snapshots_other_for_late_second_browser() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32], &auth, "conn-B");
        assert_eq!(
            snaps,
            vec![(0, "other")],
            "browser B (different connection_id) must see `other` while A holds",
        );
    }

    /// Active display, this connection IS the holder → `you`.
    /// Covers a holder browser refresh: same `connection_id` (or
    /// equivalent) reconnecting must see `you` so the chip stays
    /// consistent with the server-side gate.
    #[test]
    fn bootstrap_authority_snapshots_you_when_self_is_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32], &auth, "conn-A");
        assert_eq!(snaps, vec![(0, "you")]);
    }

    /// Multiple active displays, mixed holders → per-display
    /// personalization is independent.  The connecting browser sees
    /// `you` for its own holdings and `other`/`unclaimed` for the rest.
    /// Locks in that the snapshot iterates per display, not per holder.
    #[test]
    fn bootstrap_authority_snapshots_resolve_per_display_independently() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A"); // you
        seed_holder(&map, 1, "conn-B"); // other
                                        // display 2 unclaimed
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32, 1, 2], &auth, "conn-A");
        assert_eq!(
            snaps,
            vec![(0, "you"), (1, "other"), (2, "unclaimed")],
            "each display's state resolves independently against this connection",
        );
    }

    /// Empty session registry → no snapshots, no frames to send.
    /// Matches the "browser connects to a daemon with no granted
    /// display" path; bootstrap loop is a no-op.
    #[test]
    fn bootstrap_authority_snapshots_empty_when_no_active_displays() {
        let map = empty_authority_map();
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([] as [u32; 0], &auth, "conn-A");
        assert!(snaps.is_empty());
    }
}

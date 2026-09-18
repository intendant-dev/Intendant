//! Non-starting reads of the owned broker. Public descriptions contain only
//! logical generation handles; private helper/native state is never serialized.

use super::*;
use serde::Serialize;

#[derive(Clone)]
pub(crate) enum Inspection {
    Inventory,
    Status { selector: String },
}

impl Inspection {
    pub(super) async fn check(&self, authority: &Authority) -> Result<(), String> {
        if matches!(self, Self::Inventory) && !authority.owner_surface {
            return Err("macOS monitor inventory requires an owner surface; a user-display grant does not authorize daemon-wide handle enumeration".into());
        }
        authority.check().await?;
        if let Self::Status { selector } = self {
            if !exact_selector(selector) {
                return Err(UNSUPPORTED.into());
            }
        }
        Ok(())
    }
}

/// Syntax admission only. Ownership is still exact equality in State::resolve;
/// no normalization, raw ID, owner substitution or generation alias is allowed.
pub(crate) fn exact_selector(selector: &str) -> bool {
    let mut parts = selector.split(':');
    let (Some("macos_virtual"), Some(owner), Some(generation), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    owner.len() == 32
        && owner
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && generation
            .parse::<u32>()
            .ok()
            .is_some_and(|id| reserved_id(id) && id.to_string() == generation)
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum BrokerState {
    NotStarted,
    Live,
    Retired,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Capabilities {
    backend: &'static str,
    lifecycle_ready: bool,
    capture_ready: bool,
    capture_status: &'static str,
    input_supported: bool,
    streaming_supported: bool,
    cursor_overlay: bool,
    isolation: &'static str,
    ready: bool,
}

impl Capabilities {
    pub(crate) fn new(lifecycle_ready: bool, capture_ready: bool) -> Self {
        Self {
            backend: "macos_virtual",
            lifecycle_ready,
            capture_ready,
            capture_status: if capture_ready {
                "verified for this screenshot only"
            } else {
                "unverified; take_screenshot validates an exact first frame and Screen Recording permission"
            },
            input_supported: false,
            streaming_supported: false,
            cursor_overlay: false,
            isolation: "shared_windowserver",
            ready: false, // No input backend, even after a verified screenshot.
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MonitorDescription {
    pub display_id: u32,
    pub display_target: String,
    pub capture_generation: String,
    pub width: u32,
    pub height: u32,
    #[serde(flatten)]
    capabilities: Capabilities,
}

impl Monitor {
    pub(crate) fn description(&self) -> MonitorDescription {
        MonitorDescription {
            display_id: self.display_id,
            display_target: self.selector.clone(),
            capture_generation: self.selector.clone(),
            width: self.width,
            height: self.height,
            capabilities: Capabilities::new(true, false),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Snapshot {
    broker_state: BrokerState,
    monitors: Vec<MonitorDescription>,
}

impl Snapshot {
    pub(super) fn empty(broker_state: BrokerState) -> Self {
        Self {
            broker_state,
            monitors: Vec::new(),
        }
    }

    pub(crate) fn status(&self) -> serde_json::Value {
        let monitor = self.monitors.first();
        let lifecycle_status = match (self.broker_state, monitor.is_some()) {
            (BrokerState::Live, true) => "verified",
            (BrokerState::Live | BrokerState::NotStarted, false) => "unknown",
            (BrokerState::Retired, _) => "retired",
            _ => "unavailable",
        };
        let mut status = serde_json::json!(Capabilities::new(monitor.is_some(), false));
        status["broker_state"] = serde_json::json!(self.broker_state);
        status["lifecycle_status"] = lifecycle_status.into();
        status["monitor"] = serde_json::json!(monitor);
        status
    }
}

impl Broker {
    /// Never initializes the actor's OnceLock, runtime, helper or native owner.
    /// A pending receipt holds this bounded queue until commit/rollback finishes.
    pub(crate) async fn inspect(
        &self,
        inspection: Inspection,
        authority: Authority,
    ) -> Result<Snapshot, String> {
        inspection.check(&authority).await?;
        let snapshot = match self.sender.get() {
            None => Snapshot::empty(BrokerState::NotStarted),
            Some(Err(_)) => Snapshot::empty(BrokerState::Retired),
            Some(Ok(sender)) => {
                let (reply, receive) = oneshot::channel();
                let request = Request {
                    action: Action::Inspect(inspection.clone()),
                    authority: authority.clone(),
                    reply,
                    element_dispatch: None,
                };
                match sender.try_send(request) {
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        Snapshot::empty(BrokerState::Unavailable)
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        Snapshot::empty(BrokerState::Retired)
                    }
                    Ok(()) => {
                        match tokio::time::timeout(std::time::Duration::from_secs(20), receive)
                            .await
                        {
                            Err(_) => Snapshot::empty(BrokerState::Unavailable),
                            Ok(Err(_)) => Snapshot::empty(BrokerState::Retired),
                            Ok(Ok(result)) => {
                                let receipt = result?;
                                let Value::Inspected(snapshot) = &receipt.value else {
                                    return Err("unexpected monitor inspection result".into());
                                };
                                let snapshot = snapshot.clone();
                                // No await between receipt delivery and commit.
                                return Ok(if receipt.commit() {
                                    snapshot
                                } else {
                                    Snapshot::empty(BrokerState::Unavailable)
                                });
                            }
                        }
                    }
                }
            }
        };
        inspection.check(&authority).await?;
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(crate) fn not_started(&self) -> bool {
        self.sender.get().is_none()
    }
}

pub(super) async fn inspect(
    state: &State,
    child: &mut impl Driver,
    inspection: &Inspection,
) -> Result<Snapshot, String> {
    child.live()?;
    let monitors = match inspection {
        Inspection::Inventory => state.monitors.values().cloned().collect::<Vec<_>>(),
        Inspection::Status { selector } => state.resolve(selector).ok().into_iter().collect(),
    };
    for monitor in &monitors {
        verify(child, monitor).await?;
    }
    child.live()?;
    Ok(Snapshot {
        broker_state: BrokerState::Live,
        monitors: monitors.iter().map(Monitor::description).collect(),
    })
}

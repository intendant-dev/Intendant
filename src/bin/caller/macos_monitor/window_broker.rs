//! Public binding generations never expose private helper handles/native IDs.
use super::placement::{Bounds, Candidate, PlacementResult, WindowIdentity, MAX_BINDINGS};
use super::*;
use serde::Serialize;

pub(crate) enum WindowAction {
    List {
        pid: i32,
    },
    Bind {
        selector: String,
        identity: WindowIdentity,
        candidate: String,
    },
    Place {
        binding: String,
        bounds: Bounds,
    },
    Unbind {
        binding: String,
    },
}
impl WindowAction {
    pub(super) fn validate(&self) -> Result<(), String> {
        match self {
            Self::List { pid } if *pid <= 0 => {
                Err("candidate listing requires a positive explicit PID".into())
            }
            Self::Bind {
                selector,
                identity,
                candidate,
            } => {
                if !exact_selector(selector) {
                    return Err("binding requires an exact owned monitor selector".into());
                }
                placement::validate_candidate(candidate)?;
                identity.validate()
            }
            Self::Place { binding, bounds } => {
                valid_binding(binding)?;
                bounds.validate()
            }
            Self::Unbind { binding } => valid_binding(binding),
            _ => Ok(()),
        }
    }
}
fn valid_binding(binding: &str) -> Result<(), String> {
    if binding.len() > 128 || !binding.starts_with("macos_window:") {
        return Err("invalid window binding generation".into());
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct WindowBinding {
    pub binding: String,
    pub display_target: String,
    pub identity: WindowIdentity,
    #[serde(skip)]
    pub(super) helper_binding: u32,
}
pub(crate) enum WindowValue {
    Candidates(Vec<Candidate>),
    Bound(WindowBinding),
    Placed(PlacementResult),
    PlacementUnconfirmed {
        result: PlacementResult,
        error: String,
    },
    Unbound,
}

pub(super) async fn execute_window(
    state: &mut State,
    child: &mut impl Driver,
    request: &mut Request,
) -> Result<Value, Failure> {
    let Action::Window(action) = &request.action else {
        unreachable!()
    };
    action.validate().map_err(Failure::Request)?;
    let op = match action {
        WindowAction::List { pid } => Operation::ListWindows { pid: *pid },
        WindowAction::Bind {
            selector,
            identity,
            candidate,
        } => {
            if state.bindings.len() >= MAX_BINDINGS {
                return Err(Failure::Request("window binding capacity reached".into()));
            }
            let monitor = state.resolve(selector).map_err(Failure::Request)?;
            verify(child, &monitor).await?;
            Operation::BindWindow {
                handle: monitor.helper_handle,
                identity: *identity,
                candidate: candidate.clone(),
            }
        }
        WindowAction::Place { binding, bounds } => {
            let bound = state
                .bindings
                .get(binding)
                .ok_or_else(|| Failure::Request("stale or foreign window binding".into()))?;
            let monitor = state
                .resolve(&bound.display_target)
                .map_err(Failure::Request)?;
            verify(child, &monitor).await?;
            Operation::PlaceWindow {
                binding: bound.helper_binding,
                bounds: *bounds,
            }
        }
        WindowAction::Unbind { binding } => {
            let bound = state
                .bindings
                .get(binding)
                .ok_or_else(|| Failure::Request("stale or foreign window binding".into()))?;
            Operation::UnbindWindow {
                binding: bound.helper_binding,
            }
        }
    };
    request
        .action
        .check(&request.authority)
        .await
        .map_err(Failure::Request)?;
    if request.reply.is_closed() {
        return Err(Failure::Request(
            "window request cancelled before dispatch".into(),
        ));
    }
    let outcome = child.exchange(op).await.map_err(|e| {
        Failure::Retire(if matches!(action, WindowAction::Place { .. }) {
            placement_unconfirmed(&e)
        } else {
            format!("window operation unconfirmed; {e}")
        })
    })?;
    let value = match (action, outcome) {
        (
            _,
            Outcome::Error {
                message,
                fatal: false,
            },
        ) => return Err(Failure::Request(message)),
        (WindowAction::List { pid }, Outcome::Windows { candidates })
            if candidates.len() <= placement::MAX_CANDIDATES
                && candidates.iter().all(|c| {
                    placement::validate_candidate(&c.candidate).is_ok()
                        && c.identity.pid == *pid
                        && c.identity.validate().is_ok()
                        && c.bounds.validate().is_ok()
                }) =>
        {
            WindowValue::Candidates(candidates)
        }
        (
            WindowAction::Bind {
                selector, identity, ..
            },
            Outcome::BoundWindow { binding },
        ) if binding > state.last_binding => {
            state.last_binding = binding;
            let bound = WindowBinding {
                binding: format!("macos_window:{}:{binding}", state.owner),
                display_target: selector.clone(),
                identity: *identity,
                helper_binding: binding,
            };
            state.bindings.insert(bound.binding.clone(), bound.clone());
            WindowValue::Bound(bound)
        }
        (WindowAction::Place { .. }, Outcome::PlacedWindow { result }) if result.valid_reply() => {
            WindowValue::Placed(result)
        }
        (WindowAction::Unbind { binding }, Outcome::UnboundWindow { binding: helper })
            if state
                .bindings
                .get(binding)
                .is_some_and(|b| b.helper_binding == helper) =>
        {
            state.bindings.remove(binding);
            WindowValue::Unbound
        }
        _ => {
            let error = "unexpected window helper response; effects unconfirmed";
            return Err(Failure::Retire(
                if matches!(action, WindowAction::Place { .. }) {
                    placement_unconfirmed(error)
                } else {
                    error.into()
                },
            ));
        }
    };
    Ok(Value::Window(value))
}

pub(super) async fn rollback_binding(
    state: &mut State,
    child: &mut impl Driver,
    binding: &str,
) -> Result<(), String> {
    let b = state
        .bindings
        .remove(binding)
        .ok_or("binding rollback lost ownership")?;
    match child
        .exchange(Operation::UnbindWindow {
            binding: b.helper_binding,
        })
        .await?
    {
        Outcome::UnboundWindow { binding } if binding == b.helper_binding => Ok(()),
        _ => Err("window binding rollback unconfirmed; cleanup required".into()),
    }
}

//! Public binding generations never expose private helper handles/native IDs.
use super::controls::{ActionResult, Control, ElementAction};
use super::keyboard::KeyboardTarget;
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
    ReadElements {
        binding: String,
    },
    ReadKeyboardTarget {
        binding: String,
    },
    PrepareArrow {
        binding: String,
    },
    PressArrow {
        binding: String,
        token: String,
    },
    ActElement {
        binding: String,
        element: String,
        action: ElementAction,
    },
    PrepareClick {
        binding: String,
        point: pointer::Point,
    },
    Click {
        binding: String,
        token: String,
    },
    PrepareScroll {
        binding: String,
        point: pointer::Point,
        delta_y: i32,
    },
    Scroll {
        binding: String,
        token: String,
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
            Self::PrepareClick { binding, point } => {
                valid_binding(binding)?;
                point.validate()
            }
            Self::PrepareScroll {
                binding,
                point,
                delta_y,
            } => {
                valid_binding(binding)?;
                point.validate()?;
                scroll::validate_delta(*delta_y)
            }
            Self::PressArrow { binding, token } => {
                valid_binding(binding)?;
                arrow::validate_token(token)
            }
            Self::Click { binding, token } | Self::Scroll { binding, token } => {
                valid_binding(binding)?;
                pointer::validate_token(token)
            }
            Self::Place { binding, bounds } => {
                valid_binding(binding)?;
                bounds.validate()
            }
            Self::Unbind { binding }
            | Self::ReadElements { binding }
            | Self::ReadKeyboardTarget { binding }
            | Self::PrepareArrow { binding } => valid_binding(binding),
            Self::ActElement {
                binding,
                element,
                action,
            } => {
                valid_binding(binding)?;
                controls::validate_token(element)?;
                action.validate()
            }
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
    Elements(Vec<Control>),
    KeyboardTarget(KeyboardTarget),
    PreparedArrow(arrow::Prepared),
    Arrowed(Box<arrow::ArrowResult>),
    ArrowUnconfirmed {
        result: Box<arrow::ArrowResult>,
        error: String,
    },
    Acted(ActionResult),
    ActionUnconfirmed {
        result: ActionResult,
        error: String,
    },
    PreparedPointer(pointer::Prepared),
    Clicked(pointer::ClickResult),
    ClickUnconfirmed {
        result: pointer::ClickResult,
        error: String,
    },
    PreparedScroll(scroll::PreparedScroll),
    Scrolled(scroll::ScrollResult),
    ScrollUnconfirmed {
        result: scroll::ScrollResult,
        error: String,
    },
    Unbound,
}

fn begin_window_dispatch(
    progress: Option<&std::sync::atomic::AtomicBool>,
    cancelled: impl FnOnce() -> bool,
) -> bool {
    // Publish possible dispatch BEFORE the final cancellation check. A caller
    // that closes its receiver and then observes false must prevent dispatch;
    // once true is visible, a lost reply conservatively means uncertain effects.
    if let Some(progress) = progress {
        progress.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    !cancelled()
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
        WindowAction::ReadElements { binding }
        | WindowAction::ReadKeyboardTarget { binding }
        | WindowAction::PrepareArrow { binding }
        | WindowAction::PressArrow { binding, .. }
        | WindowAction::ActElement { binding, .. }
        | WindowAction::PrepareClick { binding, .. }
        | WindowAction::Click { binding, .. }
        | WindowAction::PrepareScroll { binding, .. }
        | WindowAction::Scroll { binding, .. } => {
            let bound = state
                .bindings
                .get(binding)
                .ok_or_else(|| Failure::Request("stale or foreign window binding".into()))?;
            // The helper checks its retained monitor geometry within the same
            // snapshot/action budget. Do not dispatch a separate native resolve
            // ahead of consuming the element inventory.
            match action {
                WindowAction::PrepareArrow { .. } => Operation::PrepareArrow {
                    binding: bound.helper_binding,
                },
                WindowAction::PressArrow { token, .. } => Operation::PressArrow {
                    binding: bound.helper_binding,
                    token: token.clone(),
                },
                WindowAction::PrepareClick { point, .. } => Operation::PreparePointer {
                    binding: bound.helper_binding,
                    point: *point,
                },
                WindowAction::Click { token, .. } => Operation::ClickPointer {
                    binding: bound.helper_binding,
                    token: token.clone(),
                },
                WindowAction::PrepareScroll { point, delta_y, .. } => Operation::PrepareScroll {
                    binding: bound.helper_binding,
                    point: *point,
                    delta_y: *delta_y,
                },
                WindowAction::Scroll { token, .. } => Operation::ScrollPointer {
                    binding: bound.helper_binding,
                    token: token.clone(),
                },
                WindowAction::ReadElements { .. } => Operation::ReadWindowElements {
                    binding: bound.helper_binding,
                },
                WindowAction::ReadKeyboardTarget { .. } => Operation::ReadKeyboardTarget {
                    binding: bound.helper_binding,
                },
                WindowAction::ActElement {
                    element, action, ..
                } => Operation::ActWindowElement {
                    binding: bound.helper_binding,
                    element: element.clone(),
                    action: action.clone(),
                },
                _ => unreachable!(),
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
    if !begin_window_dispatch(request.element_dispatch.as_deref(), || {
        request.reply.is_closed()
    }) {
        return Err(Failure::Request(
            "window request cancelled before dispatch".into(),
        ));
    }
    let outcome = child.exchange(op).await.map_err(|e| {
        Failure::Retire(if matches!(action, WindowAction::Place { .. }) {
            placement_unconfirmed(&e)
        } else if matches!(action, WindowAction::PressArrow { .. }) {
            key_unconfirmed(&e)
        } else if matches!(
            action,
            WindowAction::Click { .. } | WindowAction::Scroll { .. }
        ) {
            pointer_unconfirmed(&e)
        } else if matches!(action, WindowAction::ActElement { .. }) {
            element_unconfirmed(&e)
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
        (WindowAction::ReadElements { .. }, Outcome::WindowElements { controls })
            if controls.len() <= controls::MAX_CONTROLS
                && controls.iter().all(Control::valid_reply)
                && controls
                    .iter()
                    .enumerate()
                    .all(|(i, c)| controls[..i].iter().all(|p| p.element != c.element)) =>
        {
            WindowValue::Elements(controls)
        }
        (WindowAction::PrepareArrow { .. }, Outcome::PreparedArrow { prepared })
            if prepared.valid_reply() =>
        {
            WindowValue::PreparedArrow(prepared)
        }
        (WindowAction::PressArrow { .. }, Outcome::PressedArrow { result })
            if result.valid_reply() =>
        {
            WindowValue::Arrowed(Box::new(result))
        }
        (WindowAction::ReadKeyboardTarget { .. }, Outcome::KeyboardTarget { target })
            if target.valid_reply() =>
        {
            WindowValue::KeyboardTarget(target)
        }
        (WindowAction::ActElement { action, .. }, Outcome::ActedWindowElement { result })
            if result.valid_reply()
                && matches!(
                    (action, result.operation),
                    (ElementAction::Press {}, controls::SupportedOperation::Press)
                        | (
                            ElementAction::SetValue { .. },
                            controls::SupportedOperation::SetValue
                        )
                ) =>
        {
            WindowValue::Acted(result)
        }
        (WindowAction::PrepareClick { point, .. }, Outcome::PreparedPointer { prepared })
            if prepared.valid_reply() && prepared.point == *point =>
        {
            WindowValue::PreparedPointer(prepared)
        }
        (WindowAction::Click { .. }, Outcome::ClickedPointer { result })
            if result.valid_reply() =>
        {
            WindowValue::Clicked(result)
        }
        (
            WindowAction::PrepareScroll { point, delta_y, .. },
            Outcome::PreparedScroll { prepared },
        ) if prepared.valid_reply() && prepared.point == *point && prepared.delta_y == *delta_y => {
            WindowValue::PreparedScroll(prepared)
        }
        (WindowAction::Scroll { .. }, Outcome::ScrolledPointer { result })
            if result.valid_reply() =>
        {
            WindowValue::Scrolled(result)
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
                } else if matches!(action, WindowAction::PressArrow { .. }) {
                    key_unconfirmed(error)
                } else if matches!(
                    action,
                    WindowAction::Click { .. } | WindowAction::Scroll { .. }
                ) {
                    pointer_unconfirmed(error)
                } else if matches!(action, WindowAction::ActElement { .. }) {
                    element_unconfirmed(error)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn cancellation_before_publication_cannot_dispatch_after_reporting_no_attempt() {
        let progress = AtomicBool::new(false);
        let (reply, receive) = oneshot::channel();
        let error = await_element_reply(receive, &progress, Duration::ZERO)
            .await
            .err()
            .unwrap();
        assert!(!error.starts_with(ELEMENT_UNCONFIRMED));
        assert!(!progress.load(Ordering::SeqCst));
        assert!(!begin_window_dispatch(Some(&progress), || reply.is_closed()));
    }

    #[tokio::test]
    async fn cancellation_at_or_after_final_check_always_reports_possible_dispatch() {
        for at_check in [true, false] {
            let progress = AtomicBool::new(false);
            let (reply, mut receive) = oneshot::channel();
            let may_dispatch = begin_window_dispatch(Some(&progress), || {
                // Deterministically stop at the former race boundary: the
                // publication must already be visible when cancellation checks.
                assert!(progress.load(Ordering::SeqCst));
                if at_check {
                    receive.close();
                }
                reply.is_closed()
            });
            assert_eq!(may_dispatch, !at_check);
            let error = await_element_reply(receive, &progress, Duration::ZERO)
                .await
                .err()
                .unwrap();
            assert!(error.starts_with(ELEMENT_UNCONFIRMED));
        }
    }
}

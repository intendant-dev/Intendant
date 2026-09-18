//! Daemon-scoped macOS monitor broker. These monitors share WindowServer with
//! the owner; ownership is never a display grant. No public display registry,
//! streaming, browser workspaces, input or primary-display fallback. Explicit
//! owner-only AX window binding/placement uses the same serialized helper.

#[cfg(any(target_os = "macos", test))]
pub(crate) mod ancestry;
mod capture;
pub(crate) mod controls;
mod helper;
mod inspection;
pub(crate) mod placement;
mod process;
mod protocol;
mod smoke;
mod window_broker;
pub(crate) use window_broker::{WindowAction, WindowValue};

use crate::autonomy::SharedAutonomy;
pub(crate) use inspection::{exact_selector, Capabilities, Inspection, Snapshot};
use protocol::{Operation, Outcome};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::sync::{mpsc, oneshot};

const QUEUE_SIZE: usize = 8;
// Below the macOS window range (0x40000000); never native/helper IDs.
pub(crate) const DISPLAY_ID_MIN: u32 = 0x2000_0000;
pub(crate) const DISPLAY_ID_MAX: u32 = 0x3fff_ffff;
pub(crate) const UNSUPPORTED: &str = "generic macos_virtual display APIs support only exact take_screenshot, display_readiness and destroy_virtual_display; input, AX trees, browser workspaces, shared views and streaming are unavailable. Explicit owner-only app placement uses bind_macos_window/place_macos_window";

/// Deliberately broad reservation: malformed/case/whitespace variants must
/// never fall through an old parser to :99 or the user's primary display.
pub(crate) fn reserved(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    if [
        "macos_virtual",
        "macos_window",
        "macos_candidate",
        "macos_element",
    ]
    .iter()
    .any(|prefix| value.contains(prefix))
    {
        return true;
    }
    let mut digits = value.as_str();
    while let Some(rest) = digits
        .strip_prefix("display_")
        .or_else(|| digits.strip_prefix(':'))
    {
        digits = rest.trim();
    }
    digits.parse::<u32>().ok().is_some_and(reserved_id)
}

pub(crate) fn reserved_id(id: u32) -> bool {
    (DISPLAY_ID_MIN..=DISPLAY_ID_MAX).contains(&id)
}

pub(crate) fn reject_unsupported(target: Option<&str>, id: Option<u32>) -> Result<(), String> {
    if target.is_some_and(reserved) || id.is_some_and(reserved_id) {
        Err(UNSUPPORTED.into())
    } else {
        Ok(())
    }
}

pub(crate) fn intercept_helper() -> Option<Result<(), String>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args
        .first()
        .is_some_and(|s| s == "--private-macos-monitor-smoke-v1")
    {
        return Some(
            if args.len() == 2 && args[1] == "--accept-shared-session-hotplug" {
                smoke::run()
            } else {
                Err("native smoke requires --accept-shared-session-hotplug; it creates only test-owned monitors but may rearrange login-session windows".into())
            },
        );
    }
    if !args.iter().any(|s| s == protocol::HELPER_ARG) {
        return None;
    }
    Some(if args.len() == 1 {
        helper::run()
    } else {
        Err("private monitor helper takes no other arguments".into())
    })
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), String> {
    use intendant_platform::cgvirtual::{MAX_DIMENSION, MIN_DIMENSION};
    if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&width)
        || !(MIN_DIMENSION..=MAX_DIMENSION).contains(&height)
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
    {
        return Err(format!(
            "macOS monitor dimensions must each be even and {MIN_DIMENSION}..={MAX_DIMENSION}"
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Authority {
    pub owner_surface: bool,
    pub autonomy: SharedAutonomy,
}

impl Authority {
    pub(crate) async fn check(&self) -> Result<(), String> {
        if self.owner_surface || self.autonomy.read().await.user_display_granted {
            Ok(())
        } else {
            Err("macOS monitors share the user's WindowServer session; an owner surface or an existing explicit user-display grant is required".into())
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Monitor {
    pub display_id: u32,
    pub selector: String,
    pub width: u32,
    pub height: u32,
    native_id: u32,
    helper_handle: u32,
}

pub(crate) struct Screenshot {
    pub path: PathBuf,
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub(crate) enum Value {
    Created(Monitor),
    Destroyed,
    Captured(Screenshot),
    Inspected(Snapshot),
    Window(WindowValue),
}

/// The actor holds serialization until the frontend has synchronously built
/// its response. Dropping an uncommitted create receipt rolls creation back,
/// including cancellation *after* the actor has sent the result.
/// Commit means local response construction, not transport delivery or a
/// network/client acknowledgment. Callers must retain committed handles.
pub(crate) struct Receipt {
    pub value: Value,
    commit: oneshot::Sender<()>,
}

impl Receipt {
    #[cfg(test)]
    pub(crate) fn fixture(value: Value) -> (Self, oneshot::Receiver<()>) {
        let (commit, committed) = oneshot::channel();
        (Self { value, commit }, committed)
    }
    /// False means the bounded receipt expired; never publish its handles or
    /// artifact as successful. Placement may already have applied partially.
    pub(crate) fn commit(self) -> bool {
        self.commit.send(()).is_ok()
    }
}

/// Closing linearizes expiry against send. A send that won before close may
/// already be buffered even though timeout last polled the receiver as pending.
fn close_and_drain<T>(receive: &mut oneshot::Receiver<T>) -> Option<T> {
    receive.close();
    receive.try_recv().ok()
}

async fn await_commit(mut committed: oneshot::Receiver<()>) -> bool {
    match tokio::time::timeout(std::time::Duration::from_secs(2), &mut committed).await {
        Ok(result) => result.is_ok(),
        Err(_) => close_and_drain(&mut committed).is_some(),
    }
}

pub(crate) const ELEMENT_UNCONFIRMED: &str = "element action effects unconfirmed";
fn element_unconfirmed(error: &str) -> String {
    format!("{ELEMENT_UNCONFIRMED}; action may already have applied or still be in progress; do not replay; {error}")
}

pub(crate) const PLACEMENT_UNCONFIRMED: &str = "placement effects unconfirmed";
fn placement_unconfirmed(error: &str) -> String {
    format!("{PLACEMENT_UNCONFIRMED}; window movement/resize may already have applied and may still be in progress; {error}")
}

async fn await_reply(
    mut receive: oneshot::Receiver<Result<Receipt, String>>,
    placement: bool,
    budget: std::time::Duration,
) -> Result<Receipt, String> {
    let error = match tokio::time::timeout(budget, &mut receive).await {
        Ok(Ok(result)) => return result,
        Ok(Err(_)) => "macOS monitor broker retired before delivery",
        Err(_) => {
            // Preserve any completed result buffered at the deadline boundary.
            if let Some(result) = close_and_drain(&mut receive) {
                return result;
            }
            "monitor request deadline exceeded; broker retains cleanup ownership"
        }
    };
    Err(if placement {
        placement_unconfirmed(error)
    } else {
        error.into()
    })
}

async fn await_element_reply(
    receive: oneshot::Receiver<Result<Receipt, String>>,
    dispatched: &std::sync::atomic::AtomicBool,
    budget: std::time::Duration,
) -> Result<Receipt, String> {
    await_reply(receive, false, budget).await.map_err(|e| {
        if dispatched.load(std::sync::atomic::Ordering::SeqCst)
            && (e.starts_with("monitor request deadline")
                || e.starts_with("macOS monitor broker retired"))
        {
            element_unconfirmed(&e)
        } else {
            e
        }
    })
}

pub(crate) enum Action {
    Create { width: u32, height: u32 },
    Destroy { display_id: u32, selector: String },
    Capture { selector: String, path: PathBuf },
    Inspect(Inspection),
    Window(WindowAction),
}

impl Action {
    async fn check(&self, authority: &Authority) -> Result<(), String> {
        match self {
            Self::Inspect(inspection) => inspection.check(authority).await,
            Self::Window(action) => {
                if !authority.owner_surface {
                    return Err("macOS window operations require an owner surface; user-display grants do not authorize application window movement or enumeration".into());
                }
                action.validate()
            }
            _ => authority.check().await,
        }
    }
}

struct Request {
    action: Action,
    authority: Authority,
    reply: oneshot::Sender<Result<Receipt, String>>,
    element_dispatch: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// Allocated with the EventBus, shared across MCP transports/sessions. Lazy
/// start is one-shot, including failure. No GUI probes in ordinary startup.
#[derive(Default)]
pub(crate) struct Broker {
    sender: OnceLock<Result<mpsc::Sender<Request>, String>>,
}

impl Broker {
    pub(crate) async fn request(
        &self,
        action: Action,
        authority: Authority,
    ) -> Result<Receipt, String> {
        action.check(&authority).await?;
        // Inspection must use the non-starting entry point, even internally.
        if matches!(action, Action::Inspect(_)) {
            return Err("use non-starting monitor inspection".into());
        }
        if let Action::Create { width, height } = action {
            validate_dimensions(width, height)?;
        }
        if matches!(action, Action::Window(_)) && self.sender.get().is_none() {
            return Err("no owned macOS monitor generation".into());
        }
        if !cfg!(target_os = "macos") {
            return Err("macOS owned monitors require macOS".into());
        }
        let sender = self
            .sender
            .get_or_init(|| {
                let (tx, rx) = mpsc::channel(QUEUE_SIZE);
                // A dedicated thread owns the runtime/actor. Request cancellation
                // never aborts it, including inside synchronous SCK start/stop.
                // At most one worker and one helper exist per daemon broker.
                std::thread::Builder::new()
                    .name("macos-monitor-broker".into())
                    .spawn(move || {
                        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            runtime.block_on(run(rx));
                        }
                    })
                    .map_err(|e| e.to_string())?;
                Ok(tx)
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let placement = matches!(action, Action::Window(WindowAction::Place { .. }));
        let element_dispatch = matches!(action, Action::Window(WindowAction::ActElement { .. }))
            .then(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let (reply, receive) = oneshot::channel();
        sender
            .try_send(Request {
                action,
                authority,
                reply,
                element_dispatch: element_dispatch.clone(),
            })
            .map_err(|_| "macOS monitor broker unavailable or busy")?;
        // This deadline only cancels the caller's receipt. The worker retains
        // child/backend ownership through stop/rollback; it is never aborted.
        if let Some(progress) = element_dispatch {
            await_element_reply(receive, &progress, std::time::Duration::from_secs(20)).await
        } else {
            await_reply(receive, placement, std::time::Duration::from_secs(20)).await
        }
    }
}

struct State {
    owner: String,
    last_handle: u32,
    next_display_id: u32,
    monitors: BTreeMap<u32, Monitor>,
    bindings: BTreeMap<String, window_broker::WindowBinding>,
    last_binding: u32,
}

impl State {
    fn new() -> Self {
        Self {
            owner: uuid::Uuid::new_v4().simple().to_string(),
            last_handle: 0,
            next_display_id: DISPLAY_ID_MIN,
            monitors: BTreeMap::new(),
            bindings: BTreeMap::new(),
            last_binding: 0,
        }
    }

    fn selector(&self, id: u32) -> String {
        format!("macos_virtual:{}:{id}", self.owner)
    }

    fn resolve(&self, selector: &str) -> Result<Monitor, String> {
        // Equality with an owned entry is the entire parser. No native-ID,
        // numeric alias, prefix match or normalized malformed form is admitted.
        self.monitors
            .values()
            .find(|m| m.selector == selector)
            .cloned()
            .ok_or_else(|| "stale, foreign or malformed macos_virtual generation".into())
    }
}

#[async_trait::async_trait]
trait Driver: Send {
    fn live(&mut self) -> Result<(), String>;
    async fn exchange(&mut self, operation: Operation) -> Result<Outcome, String>;
    async fn capture(
        &mut self,
        monitor: &Monitor,
        path: &std::path::Path,
        reply: &mut oneshot::Sender<Result<Receipt, String>>,
    ) -> Result<Screenshot, Failure>;
    async fn close(&mut self) -> Result<(), String>;
}

#[async_trait::async_trait]
impl Driver for process::Process {
    fn live(&mut self) -> Result<(), String> {
        self.live()
    }
    async fn exchange(&mut self, operation: Operation) -> Result<Outcome, String> {
        self.exchange(operation).await
    }
    async fn capture(
        &mut self,
        monitor: &Monitor,
        path: &std::path::Path,
        reply: &mut oneshot::Sender<Result<Receipt, String>>,
    ) -> Result<Screenshot, Failure> {
        capture::capture(monitor, path, reply).await
    }
    async fn close(&mut self) -> Result<(), String> {
        self.close().await
    }
}

async fn run(requests: mpsc::Receiver<Request>) {
    let _ = run_with_factory(requests, process::Process::spawn).await;
}

async fn run_with_factory<D, F, Fut>(
    mut requests: mpsc::Receiver<Request>,
    factory: F,
) -> Result<(), String>
where
    D: Driver,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<D, String>>,
{
    let mut factory = Some(factory);
    let mut state = State::new();
    let mut process: Option<D> = None;
    let mut retired: Option<String> = None;
    while let Some(mut request) = requests.recv().await {
        if request.reply.is_closed() {
            continue;
        }
        if let Err(e) = request.action.check(&request.authority).await {
            let _ = request.reply.send(Err(e));
            continue;
        }
        if let Action::Inspect(inspection) = &request.action {
            // Serialized behind every outstanding receipt and cleanup. No
            // factory access: reads cannot start or replace the owned helper.
            let mut snapshot = if retired.is_some() {
                Snapshot::empty(inspection::BrokerState::Retired)
            } else if let Some(child) = process.as_mut() {
                match inspection::inspect(&state, child, inspection).await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        retired = Some(error);
                        let _ = child.close().await;
                        state.monitors.clear();
                        state.bindings.clear();
                        process = None;
                        Snapshot::empty(inspection::BrokerState::Retired)
                    }
                }
            } else {
                Snapshot::empty(inspection::BrokerState::NotStarted)
            };
            if let Err(error) = request.action.check(&request.authority).await {
                let _ = request.reply.send(Err(error));
            } else {
                // Match lifecycle delivery's last synchronous liveness check
                // after the authority await. Failure never releases handles.
                if let Some(child) = process.as_mut() {
                    if let Err(error) = child.live() {
                        retired = Some(error);
                        let _ = child.close().await;
                        state.monitors.clear();
                        state.bindings.clear();
                        process = None;
                        snapshot = Snapshot::empty(inspection::BrokerState::Retired);
                        if let Err(error) = request.action.check(&request.authority).await {
                            let _ = request.reply.send(Err(error));
                            continue;
                        }
                    }
                }
                let (commit, committed) = oneshot::channel();
                let _ = request.reply.send(Ok(Receipt {
                    value: Value::Inspected(snapshot),
                    commit,
                }));
                let _ = await_commit(committed).await;
            }
            continue;
        }
        if let Action::Create { width, height } = request.action {
            if let Err(error) = validate_dimensions(width, height) {
                let _ = request.reply.send(Err(error));
                continue;
            }
        }
        if let Some(reason) = &retired {
            let _ = request.reply.send(Err(reason.clone()));
            continue;
        }
        if process.is_none() {
            // Only creation may start a helper. Foreign selectors cannot even
            // probe the native API, much less cause ID adoption.
            if !matches!(request.action, Action::Create { .. }) {
                let _ = request
                    .reply
                    .send(Err("no owned macOS monitor generation".into()));
                continue;
            }
            match factory.take().expect("helper starts at most once")().await {
                Ok(child) => process = Some(child),
                Err(e) => {
                    retired = Some(e.clone());
                    let _ = request.reply.send(Err(e));
                    continue;
                }
            }
        }
        let child = process.as_mut().expect("owned helper");
        if request.reply.is_closed() {
            continue;
        }
        if let Err(e) = request.action.check(&request.authority).await {
            let _ = request.reply.send(Err(e));
            continue;
        }
        let result = execute(&mut state, child, &mut request).await;
        match result {
            Ok(value) => {
                let created = match &value {
                    Value::Created(m) => Some(m.display_id),
                    _ => None,
                };
                let bound = match &value {
                    Value::Window(WindowValue::Bound(b)) => Some(b.binding.clone()),
                    _ => None,
                };
                let artifact = match &value {
                    Value::Captured(s) => Some(s.path.clone()),
                    _ => None,
                };
                let (commit, committed) = oneshot::channel();
                // A failed send drops the receipt and therefore the commit
                // sender. A dropped frontend future does exactly the same.
                let authorized = request.action.check(&request.authority).await;
                let live = child.live(); // last synchronous check before result delivery
                if let Err(error) = &live {
                    retired = Some(error.clone());
                }
                if let Err(error) = authorized.and(live) {
                    let preserved = match value {
                        Value::Window(WindowValue::Placed(result)) => {
                            Some(WindowValue::PlacementUnconfirmed {
                                result,
                                error: error.clone(),
                            })
                        }
                        Value::Window(WindowValue::Acted(result)) => {
                            Some(WindowValue::ActionUnconfirmed {
                                result,
                                error: error.clone(),
                            })
                        }
                        _ => None,
                    };
                    if let Some(value) = preserved {
                        let _ = request.reply.send(Ok(Receipt {
                            value: Value::Window(value),
                            commit,
                        }));
                    } else {
                        drop(commit);
                        let _ = request.reply.send(Err(error));
                    }
                } else {
                    let _ = request.reply.send(Ok(Receipt { value, commit }));
                }
                if !await_commit(committed).await {
                    if let Some(binding) = bound.filter(|_| retired.is_none()) {
                        if let Err(e) =
                            window_broker::rollback_binding(&mut state, child, &binding).await
                        {
                            retired = Some(e);
                        }
                    }
                    if let Some(path) = artifact {
                        let _ = std::fs::remove_file(path);
                    }
                    if let Some(id) = created.filter(|_| retired.is_none()) {
                        if let Err(e) = destroy(&mut state, child, id).await {
                            retired = Some(e);
                        }
                    }
                }
            }
            Err(Failure::Request(e)) => {
                let _ = request.reply.send(Err(e));
            }
            Err(Failure::Retire(e)) => {
                retired = Some(e.clone());
                let _ = request.reply.send(Err(e));
            }
        }
        if retired.is_some() {
            // Keep the terminal latch even after exact child reaping succeeds:
            // process exit cannot prove private WindowServer cleanup succeeded.
            let _ = child.close().await;
            state.monitors.clear();
            state.bindings.clear();
            process = None;
        }
    }
    if let Some(mut child) = process {
        child.close().await
    } else if let Some(reason) = retired {
        Err(reason)
    } else {
        Ok(())
    }
}

#[derive(Debug)]
enum Failure {
    Request(String),
    Retire(String),
}
impl From<String> for Failure {
    fn from(value: String) -> Self {
        Self::Retire(value)
    }
}

async fn verify(child: &mut impl Driver, monitor: &Monitor) -> Result<(), String> {
    match child
        .exchange(Operation::Resolve {
            handle: monitor.helper_handle,
        })
        .await?
    {
        Outcome::Monitor {
            handle,
            native_id,
            width,
            height,
        } if handle == monitor.helper_handle
            && native_id == monitor.native_id
            && width == monitor.width
            && height == monitor.height =>
        {
            Ok(())
        }
        _ => Err("helper generation/geometry mismatch; broker retired".into()),
    }
}

async fn destroy(state: &mut State, child: &mut impl Driver, id: u32) -> Result<(), String> {
    let monitor = state
        .monitors
        .remove(&id)
        .ok_or("no owned monitor generation")?;
    state
        .bindings
        .retain(|_, b| b.display_target != monitor.selector);
    // Invalidate the public generation before native teardown; the private
    // pipe only ever receives the helper's retained-object handle.
    match child
        .exchange(Operation::Destroy {
            handle: monitor.helper_handle,
        })
        .await?
    {
        Outcome::Destroyed { handle } if handle == monitor.helper_handle => Ok(()),
        _ => Err("helper teardown unconfirmed; broker retired".into()),
    }
}

async fn execute(
    state: &mut State,
    child: &mut impl Driver,
    request: &mut Request,
) -> Result<Value, Failure> {
    match &request.action {
        Action::Create { width, height } => {
            validate_dimensions(*width, *height).map_err(Failure::Request)?;
            if !reserved_id(state.next_display_id) {
                return Err(Failure::Request(
                    "macOS monitor logical IDs exhausted".into(),
                ));
            }
            if state.monitors.len() >= intendant_platform::cgvirtual::MAX_DISPLAYS {
                return Err(Failure::Request("macOS monitor capacity is two".into()));
            }
            match child
                .exchange(Operation::Create {
                    width: *width,
                    height: *height,
                })
                .await?
            {
                Outcome::Monitor {
                    handle,
                    native_id,
                    width: w,
                    height: h,
                } if handle > state.last_handle
                    && native_id != 0
                    && w == *width
                    && h == *height
                    && !state.monitors.values().any(|m| m.native_id == native_id) =>
                {
                    let display_id = state.next_display_id;
                    let monitor = Monitor {
                        display_id,
                        selector: state.selector(display_id),
                        width: w,
                        height: h,
                        native_id,
                        helper_handle: handle,
                    };
                    state.last_handle = handle;
                    state.next_display_id += 1;
                    state.monitors.insert(display_id, monitor.clone());
                    Ok(Value::Created(monitor))
                }
                _ => Err(Failure::Retire("invalid helper creation result".into())),
            }
        }
        Action::Destroy {
            display_id,
            selector,
        } => {
            let monitor = state.resolve(selector).map_err(Failure::Request)?;
            if *display_id != monitor.display_id {
                return Err(Failure::Request("monitor id/generation mismatch".into()));
            }
            verify(child, &monitor).await?;
            request.authority.check().await.map_err(Failure::Request)?;
            destroy(state, child, *display_id).await?;
            Ok(Value::Destroyed)
        }
        Action::Capture { selector, path } => {
            let monitor = state.resolve(selector).map_err(Failure::Request)?;
            verify(child, &monitor).await?;
            request.authority.check().await.map_err(Failure::Request)?;
            if request.reply.is_closed() {
                return Err(Failure::Request("capture cancelled".into()));
            }
            let image = child.capture(&monitor, path, &mut request.reply).await?;
            // Even a successful frame cannot escape a dead/replaced helper or
            // generation. Destroy remains serialized until receipt release.
            if let Err(error) = verify(child, &monitor).await {
                let _ = std::fs::remove_file(&image.path);
                return Err(Failure::Retire(error));
            }
            Ok(Value::Captured(image))
        }
        Action::Window(_) => window_broker::execute_window(state, child, request).await,
        Action::Inspect(_) => unreachable!("inspection is handled without starting a helper"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn receipt_commit_before_timeout_close_is_drained_and_never_rolled_back() {
        let (receipt, mut committed) = Receipt::fixture(Value::Destroyed);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            std::future::Future::poll(std::pin::Pin::new(&mut committed), &mut cx).is_pending()
        );
        // Exact old race: pending receiver, successful send, then timer branch.
        assert!(receipt.commit());
        assert_eq!(close_and_drain(&mut committed), Some(()));
    }

    #[test]
    fn receipt_timeout_close_before_commit_refuses_publication() {
        let (receipt, mut committed) = Receipt::fixture(Value::Destroyed);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            std::future::Future::poll(std::pin::Pin::new(&mut committed), &mut cx).is_pending()
        );
        assert_eq!(close_and_drain(&mut committed), None);
        assert!(!receipt.commit());
    }

    #[test]
    fn buffered_reply_at_deadline_keeps_the_complete_placement_receipt() {
        let bounds = placement::tests::monitor();
        let result = placement::PlacementResult {
            status: placement::PlacementStatus::Verified,
            requested_global: bounds,
            before: placement::Observation {
                ax: bounds,
                cg: bounds,
            },
            after: Some(placement::Observation {
                ax: bounds,
                cg: bounds,
            }),
            writes_attempted: 2,
            focus_interference: Some(false),
            detail: None,
        };
        let expected = serde_json::to_value(&result).unwrap();
        let (receipt, _committed) = Receipt::fixture(Value::Window(WindowValue::Placed(result)));
        let (reply, mut receive) = oneshot::channel();
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(std::pin::Pin::new(&mut receive), &mut cx).is_pending());
        assert!(reply.send(receipt).is_ok());
        let receipt = close_and_drain(&mut receive).expect("buffered result must survive expiry");
        let Value::Window(WindowValue::Placed(result)) = &receipt.value else {
            panic!()
        };
        assert_eq!(serde_json::to_value(result).unwrap(), expected);
        assert!(receipt.commit());
    }

    fn authority(owner_surface: bool) -> Authority {
        Authority {
            owner_surface,
            autonomy: Arc::new(tokio::sync::RwLock::new(Default::default())),
        }
    }

    #[derive(Default)]
    struct Fake {
        log: Arc<Mutex<Vec<&'static str>>>,
        destroyed: Arc<Mutex<Vec<u32>>>,
        resolved: Arc<Mutex<Vec<u32>>>,
        fail_resolve: Arc<std::sync::atomic::AtomicBool>,
        resolve_started: Option<oneshot::Sender<()>>,
        resolve_release: Option<oneshot::Receiver<()>>,
        next: u32,
        next_binding: u32,
        element_token: Option<(u32, String)>,
        die_after_place: bool,
        verified_place: bool,
        window_started: Option<oneshot::Sender<()>>,
        window_release: Option<oneshot::Receiver<()>>,
        place_started: Option<oneshot::Sender<()>>,
        place_release: Option<oneshot::Receiver<()>>,
        fail_destroy: bool,
        dead: bool,
        die_after_capture: bool,
        mismatch_after_capture: bool,
        captured: bool,
        successful_capture: bool,
        created: Option<oneshot::Sender<()>>,
        create_release: Option<oneshot::Receiver<()>>,
        capture_started: Option<oneshot::Sender<()>>,
        stop_started: Option<oneshot::Sender<()>>,
        stop_release: Option<oneshot::Receiver<()>>,
    }

    #[async_trait::async_trait]
    impl Driver for Fake {
        fn live(&mut self) -> Result<(), String> {
            if self.dead {
                Err("helper died".into())
            } else {
                Ok(())
            }
        }
        async fn exchange(&mut self, op: Operation) -> Result<Outcome, String> {
            self.live()?;
            match op {
                Operation::Create { width, height } => {
                    self.log.lock().unwrap().push("create");
                    if let Some(started) = self.created.take() {
                        let _ = started.send(());
                    }
                    if let Some(release) = self.create_release.take() {
                        let _ = release.await;
                    }
                    self.next += 1;
                    Ok(Outcome::Monitor {
                        handle: self.next,
                        native_id: 42 + self.next,
                        width,
                        height,
                    })
                }
                Operation::Resolve { handle } => {
                    self.resolved.lock().unwrap().push(handle);
                    if let Some(started) = self.resolve_started.take() {
                        let _ = started.send(());
                    }
                    if let Some(release) = self.resolve_release.take() {
                        let _ = release.await;
                    }
                    if self.fail_resolve.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(
                            "private native/helper/path diagnostic must not escape inspection"
                                .into(),
                        );
                    }
                    Ok(Outcome::Monitor {
                        handle,
                        native_id: 42
                            + handle
                            + u32::from(self.captured && self.mismatch_after_capture),
                        width: 640,
                        height: 480,
                    })
                }
                Operation::ListWindows { .. } => Ok(Outcome::Windows { candidates: vec![] }),
                Operation::BindWindow { .. } => {
                    self.log.lock().unwrap().push("bind");
                    if let Some(started) = self.window_started.take() {
                        let _ = started.send(());
                    }
                    if let Some(release) = self.window_release.take() {
                        let _ = release.await;
                    }
                    self.next_binding += 1;
                    Ok(Outcome::BoundWindow {
                        binding: self.next_binding,
                    })
                }
                Operation::ReadWindowElements { binding } => {
                    self.log.lock().unwrap().push("elements");
                    let token = format!("macos_element:{}", uuid::Uuid::new_v4().simple());
                    self.element_token = Some((binding, token.clone()));
                    Ok(Outcome::WindowElements {
                        controls: vec![controls::Control {
                            element: token,
                            role: "AXButton".into(),
                            label: "fixture".into(),
                            bounds: placement::tests::monitor(),
                            operations: vec![controls::SupportedOperation::Press],
                        }],
                    })
                }
                Operation::ActWindowElement {
                    binding, element, ..
                } => {
                    if self.element_token.take() != Some((binding, element)) {
                        return Ok(Outcome::Error {
                            message: "stale element token".into(),
                            fatal: false,
                        });
                    }
                    self.log.lock().unwrap().push("element_action");
                    if let Some(started) = self.place_started.take() {
                        let _ = started.send(());
                    }
                    if let Some(release) = self.place_release.take() {
                        let _ = release.await;
                    }
                    self.dead = self.die_after_place;
                    Ok(Outcome::ActedWindowElement {
                        result: controls::ActionResult {
                            status: controls::ActionStatus::Dispatched,
                            operation: controls::SupportedOperation::Press,
                            action_attempted: true,
                            before: placement::tests::monitor(),
                            after: Some(placement::tests::monitor()),
                            value_matches: None,
                            focus_interference: Some(false),
                            effects_unconfirmed: true,
                            detail: None,
                        },
                    })
                }
                Operation::UnbindWindow { binding } => {
                    self.element_token = None;
                    self.log.lock().unwrap().push("unbind");
                    Ok(Outcome::UnboundWindow { binding })
                }
                Operation::PlaceWindow { bounds, .. } => {
                    self.element_token = None;
                    self.log.lock().unwrap().push("place");
                    if let Some(started) = self.place_started.take() {
                        let _ = started.send(());
                    }
                    if let Some(release) = self.place_release.take() {
                        let _ = release.await;
                    }
                    self.dead = self.die_after_place;
                    Ok(Outcome::PlacedWindow {
                        result: placement::PlacementResult {
                            status: if self.verified_place {
                                placement::PlacementStatus::Verified
                            } else {
                                placement::PlacementStatus::Partial
                            },
                            requested_global: bounds,
                            before: placement::Observation {
                                ax: bounds,
                                cg: bounds,
                            },
                            after: Some(placement::Observation {
                                ax: bounds,
                                cg: bounds,
                            }),
                            writes_attempted: if self.verified_place { 2 } else { 1 },
                            focus_interference: Some(!self.verified_place),
                            detail: if self.verified_place {
                                None
                            } else {
                                Some("fixture focus interference".into())
                            },
                        },
                    })
                }
                Operation::Destroy { handle } => {
                    self.element_token = None;
                    self.log.lock().unwrap().push("destroy");
                    self.destroyed.lock().unwrap().push(handle);
                    if self.fail_destroy {
                        Err("uncertain cleanup".into())
                    } else {
                        Ok(Outcome::Destroyed { handle })
                    }
                }
            }
        }
        async fn capture(
            &mut self,
            _: &Monitor,
            path: &std::path::Path,
            reply: &mut oneshot::Sender<Result<Receipt, String>>,
        ) -> Result<Screenshot, Failure> {
            if self.successful_capture || self.die_after_capture || self.mismatch_after_capture {
                self.captured = true;
                self.dead = self.die_after_capture;
                std::fs::write(path, b"fixture image").unwrap();
                return Ok(Screenshot {
                    path: path.to_path_buf(),
                    png: b"fixture image".to_vec(),
                    width: 640,
                    height: 480,
                });
            }
            self.log.lock().unwrap().push("capture_start");
            if let Some(started) = self.capture_started.take() {
                let _ = started.send(());
            }
            reply.closed().await;
            self.log.lock().unwrap().push("capture_stop");
            if let Some(started) = self.stop_started.take() {
                let _ = started.send(());
            }
            if let Some(release) = self.stop_release.take() {
                let _ = release.await;
            }
            self.log.lock().unwrap().push("capture_stopped");
            Err(Failure::Request("cancelled".into()))
        }
        async fn close(&mut self) -> Result<(), String> {
            self.log.lock().unwrap().push("close");
            Ok(())
        }
    }

    fn runner(
        fake: Fake,
    ) -> (
        mpsc::Sender<Request>,
        tokio::task::JoinHandle<Result<(), String>>,
    ) {
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        let task = tokio::spawn(run_with_factory(rx, || async { Ok(fake) }));
        (tx, task)
    }

    fn send(
        tx: &mpsc::Sender<Request>,
        action: Action,
        authority: Authority,
    ) -> oneshot::Receiver<Result<Receipt, String>> {
        let (reply, receive) = oneshot::channel();
        assert!(tx
            .try_send(Request {
                action,
                authority,
                reply,
                element_dispatch: None,
            })
            .is_ok());
        receive
    }

    fn create(tx: &mpsc::Sender<Request>) -> oneshot::Receiver<Result<Receipt, String>> {
        send(
            tx,
            Action::Create {
                width: 640,
                height: 480,
            },
            authority(true),
        )
    }

    async fn committed_monitor(tx: &mpsc::Sender<Request>) -> Monitor {
        let receipt = create(tx).await.unwrap().unwrap();
        let Value::Created(monitor) = &receipt.value else {
            panic!("create result")
        };
        let monitor = monitor.clone();
        receipt.commit();
        monitor
    }

    async fn inspected(
        tx: &mpsc::Sender<Request>,
        inspection: Inspection,
        authority: Authority,
    ) -> Snapshot {
        let receipt = send(tx, Action::Inspect(inspection), authority)
            .await
            .unwrap()
            .unwrap();
        let Value::Inspected(snapshot) = &receipt.value else {
            panic!("inspection result")
        };
        let snapshot = snapshot.clone();
        receipt.commit();
        snapshot
    }

    fn status(selector: &str) -> Inspection {
        Inspection::Status {
            selector: selector.into(),
        }
    }

    #[tokio::test]
    async fn inspection_before_create_never_allocates_actor_or_starts_helper() {
        let broker = Broker::default();
        let selector = format!("macos_virtual:{}:{DISPLAY_ID_MIN}", "a".repeat(32));
        for inspection in [Inspection::Inventory, status(&selector)] {
            let snapshot = broker.inspect(inspection, authority(true)).await.unwrap();
            assert_eq!(
                serde_json::json!(snapshot),
                serde_json::json!({"broker_state":"not_started", "monitors":[]})
            );
            assert_eq!(snapshot.status()["lifecycle_status"], "unknown");
            assert!(!snapshot.status()["ready"].as_bool().unwrap());
            assert!(broker.not_started());
        }
        let scoped = authority(false);
        assert!(broker
            .inspect(status(&selector), scoped.clone())
            .await
            .is_err());
        scoped.autonomy.write().await.user_display_granted = true;
        assert!(broker
            .inspect(status(&selector), scoped.clone())
            .await
            .is_ok());
        assert!(broker
            .inspect(Inspection::Inventory, scoped)
            .await
            .unwrap_err()
            .contains("owner surface"));
        assert!(broker.not_started());

        // The actor also refuses to invoke its factory if inspected directly.
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        let worker = tokio::spawn(run_with_factory(rx, || async {
            Err::<Fake, String>("inspection must not start a helper".into())
        }));
        for inspection in [Inspection::Inventory, status(&selector)] {
            let snapshot = inspected(&tx, inspection, authority(true)).await;
            assert_eq!(serde_json::json!(snapshot)["broker_state"], "not_started");
        }
        drop(tx);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn committed_response_loss_is_recoverable_and_destroy_uses_exact_recovered_handle() {
        let fake = Fake {
            next: 700,
            ..Default::default()
        };
        let destroyed = fake.destroyed.clone();
        let resolved = fake.resolved.clone();
        let log = fake.log.clone();
        let (tx, worker) = runner(fake);
        let broker = Broker::default();
        assert!(broker.sender.set(Ok(tx.clone())).is_ok());
        let original = committed_monitor(&tx).await;
        let inventory = serde_json::json!(broker
            .inspect(Inspection::Inventory, authority(true))
            .await
            .unwrap());
        let recovered = &inventory["monitors"][0];
        assert_eq!(inventory["monitors"].as_array().unwrap().len(), 1);
        assert_eq!(*recovered, serde_json::json!(original.description()));
        assert_eq!(recovered["width"], 640);
        assert_eq!(recovered["height"], 480);
        let scoped = authority(false);
        scoped.autonomy.write().await.user_display_granted = true;
        let readiness = broker
            .inspect(status(&original.selector), scoped)
            .await
            .unwrap()
            .status();
        assert_eq!(readiness["lifecycle_status"], "verified");
        assert_eq!(readiness["lifecycle_ready"], true);
        for field in [
            "ready",
            "capture_ready",
            "input_supported",
            "streaming_supported",
            "cursor_overlay",
        ] {
            assert_eq!(readiness[field], false, "{field}");
        }
        assert!(readiness["capture_status"]
            .as_str()
            .unwrap()
            .starts_with("unverified"));
        assert_eq!(&*log.lock().unwrap(), &["create"]); // no capture/input/create during reads
        assert_eq!(&*resolved.lock().unwrap(), &[701, 701]);
        // A whitelist pins every serialized field, including nested descriptions.
        let mut keys: Vec<_> = recovered
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut expected = vec![
            "display_id",
            "display_target",
            "capture_generation",
            "width",
            "height",
            "backend",
            "lifecycle_ready",
            "capture_ready",
            "capture_status",
            "input_supported",
            "streaming_supported",
            "cursor_overlay",
            "isolation",
            "ready",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
        let encoded = inventory.to_string();
        for private in ["native_id", "helper_handle", "pid", "path"] {
            assert!(!encoded.contains(private), "{private}");
        }
        send(
            &tx,
            Action::Destroy {
                display_id: recovered["display_id"].as_u64().unwrap() as u32,
                selector: recovered["capture_generation"].as_str().unwrap().into(),
            },
            authority(true),
        )
        .await
        .unwrap()
        .unwrap()
        .commit();
        assert_eq!(&*destroyed.lock().unwrap(), &[701]);
        let inventory = inspected(&tx, Inspection::Inventory, authority(true)).await;
        assert_eq!(
            serde_json::json!(inventory)["monitors"],
            serde_json::json!([])
        );
        let stale = inspected(&tx, status(&original.selector), authority(true))
            .await
            .status();
        assert_eq!(stale["broker_state"], "live");
        assert_eq!(stale["lifecycle_status"], "unknown");
        assert!(stale["monitor"].is_null());
        drop(broker);
        drop(tx);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn pending_create_receipt_blocks_inventory_and_rollback_excludes_generation() {
        let (tx, worker) = runner(Fake::default());
        let receipt = create(&tx).await.unwrap().unwrap();
        let mut pending = send(&tx, Action::Inspect(Inspection::Inventory), authority(true));
        tokio::task::yield_now().await;
        assert!(matches!(
            pending.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        drop(receipt);
        let inventory = pending.await.unwrap().unwrap();
        let Value::Inspected(snapshot) = &inventory.value else {
            panic!("inventory")
        };
        assert_eq!(
            serde_json::json!(snapshot)["monitors"],
            serde_json::json!([])
        );
        inventory.commit();
        drop(tx);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn inspection_failure_retires_without_handles_or_respawn() {
        let fake = Fake::default();
        let fail = fake.fail_resolve.clone();
        let log = fake.log.clone();
        let (tx, worker) = runner(fake);
        let monitor = committed_monitor(&tx).await;
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        for inspection in [
            Inspection::Inventory,
            status(&monitor.selector),
            Inspection::Inventory,
        ] {
            let snapshot = inspected(&tx, inspection, authority(true)).await;
            assert_eq!(
                serde_json::json!(snapshot),
                serde_json::json!({"broker_state":"retired", "monitors":[]})
            );
            let status = snapshot.status();
            assert_eq!(status["lifecycle_status"], "retired");
            assert_eq!(status["lifecycle_ready"], false);
            assert_eq!(status["capture_ready"], false);
            assert_eq!(status["ready"], false);
            assert!(status["monitor"].is_null());
        }
        assert!(create(&tx).await.unwrap().is_err());
        assert_eq!(&*log.lock().unwrap(), &["create", "close"]);
        drop(tx);
        assert!(worker.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn unknown_owner_and_generation_never_resolve_helper_or_fall_back() {
        let fake = Fake::default();
        let resolved = fake.resolved.clone();
        let (tx, worker) = runner(fake);
        let monitor = committed_monitor(&tx).await;
        let other_owner = State::new();
        let owner = monitor.selector.split(':').nth(1).unwrap();
        for selector in [
            other_owner.selector(monitor.display_id),
            format!("macos_virtual:{owner}:{}", monitor.display_id + 1),
        ] {
            let status = inspected(&tx, status(&selector), authority(true))
                .await
                .status();
            assert_eq!(status["broker_state"], "live");
            assert_eq!(status["lifecycle_status"], "unknown");
            assert_eq!(status["lifecycle_ready"], false);
            assert!(status["monitor"].is_null());
        }
        for selector in [
            monitor.display_id.to_string(),
            format!("display_{}", monitor.display_id),
            format!(" {}", monitor.selector),
            monitor.selector.to_uppercase(),
            format!("macos_virtual:{owner}:0{}", monitor.display_id),
        ] {
            assert!(!exact_selector(&selector));
            assert!(
                send(&tx, Action::Inspect(status(&selector)), authority(true))
                    .await
                    .unwrap()
                    .is_err()
            );
        }
        assert!(resolved.lock().unwrap().is_empty());
        drop(tx);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn busy_closed_and_failed_brokers_return_no_recovery_handles() {
        let broker = Broker::default();
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        assert!(broker.sender.set(Ok(tx.clone())).is_ok());
        let mut pending = Vec::new();
        for _ in 0..QUEUE_SIZE {
            pending.push(send(
                &tx,
                Action::Inspect(Inspection::Inventory),
                authority(true),
            ));
        }
        let snapshot = broker
            .inspect(Inspection::Inventory, authority(true))
            .await
            .unwrap();
        assert_eq!(
            serde_json::json!(snapshot),
            serde_json::json!({"broker_state":"unavailable", "monitors":[]})
        );
        assert_eq!(snapshot.status()["ready"], false);
        drop(rx);
        let snapshot = broker
            .inspect(Inspection::Inventory, authority(true))
            .await
            .unwrap();
        assert_eq!(
            serde_json::json!(snapshot),
            serde_json::json!({"broker_state":"retired", "monitors":[]})
        );
        let failed = Broker::default();
        assert!(failed
            .sender
            .set(Err("private startup path".into()))
            .is_ok());
        assert_eq!(
            serde_json::json!(failed
                .inspect(Inspection::Inventory, authority(true))
                .await
                .unwrap()),
            serde_json::json!({"broker_state":"retired", "monitors":[]})
        );
    }

    #[tokio::test]
    async fn inspection_rechecks_owner_grant_at_dequeue_and_after_resolve() {
        let (started, starting) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let fake = Fake {
            resolve_started: Some(started),
            resolve_release: Some(released),
            ..Default::default()
        };
        let resolved = fake.resolved.clone();
        let (tx, worker) = runner(fake);
        let receipt = create(&tx).await.unwrap().unwrap();
        let Value::Created(monitor) = &receipt.value else {
            panic!("create")
        };
        let selector = monitor.selector.clone();
        let scoped = authority(false);
        scoped.autonomy.write().await.user_display_granted = true;
        let inventory = send(&tx, Action::Inspect(Inspection::Inventory), scoped.clone());
        let queued = send(&tx, Action::Inspect(status(&selector)), scoped.clone());
        scoped.autonomy.write().await.user_display_granted = false;
        receipt.commit();
        assert!(inventory.await.unwrap().is_err());
        assert!(queued.await.unwrap().is_err());
        assert!(resolved.lock().unwrap().is_empty());
        scoped.autonomy.write().await.user_display_granted = true;
        assert!(
            send(&tx, Action::Inspect(Inspection::Inventory), scoped.clone())
                .await
                .unwrap()
                .is_err()
        );
        let pending = send(&tx, Action::Inspect(status(&selector)), scoped.clone());
        starting.await.unwrap();
        scoped.autonomy.write().await.user_display_granted = false;
        release.send(()).unwrap();
        assert!(pending.await.unwrap().is_err());
        assert_eq!(
            inspected(&tx, status(&selector), authority(true))
                .await
                .status()["lifecycle_ready"],
            true
        );
        assert!(!scoped.autonomy.read().await.user_display_granted);
        drop(tx);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn invalid_dimensions_never_start_helper_or_publish_receipt() {
        let broker = Broker::default();
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let factory_started = started.clone();
        let worker = tokio::spawn(run_with_factory(rx, || async move {
            factory_started.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(Fake::default())
        }));
        for (width, height) in [
            (0, 480),
            (63, 480),
            (65, 480),
            (640, 481),
            (4098, 480),
            (640, 4097),
        ] {
            assert!(broker
                .request(Action::Create { width, height }, authority(true))
                .await
                .is_err());
            assert!(broker.sender.get().is_none());
            assert!(send(&tx, Action::Create { width, height }, authority(true))
                .await
                .unwrap()
                .is_err());
        }
        for size in [64, 640, 4096] {
            assert!(validate_dimensions(size, size).is_ok());
        }
        drop(tx);
        worker.await.unwrap().unwrap();
        assert!(!started.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn public_ids_never_alias_private_handles_and_stale_generations_do_not_destroy() {
        let fake = Fake {
            next: 100,
            ..Default::default()
        };
        let destroyed = fake.destroyed.clone();
        let (tx, worker) = runner(fake);
        let monitor = committed_monitor(&tx).await;
        assert_eq!(monitor.display_id, DISPLAY_ID_MIN);
        assert_eq!(monitor.helper_handle, 101);
        for (id, selector) in [
            (monitor.helper_handle, monitor.selector.clone()),
            (monitor.native_id, monitor.selector.clone()),
            (monitor.display_id, monitor.display_id.to_string()),
        ] {
            assert!(send(
                &tx,
                Action::Destroy {
                    display_id: id,
                    selector
                },
                authority(true)
            )
            .await
            .unwrap()
            .is_err());
        }
        assert!(destroyed.lock().unwrap().is_empty());
        send(
            &tx,
            Action::Destroy {
                display_id: monitor.display_id,
                selector: monitor.selector.clone(),
            },
            authority(true),
        )
        .await
        .unwrap()
        .unwrap()
        .commit();
        let replacement = committed_monitor(&tx).await;
        assert_ne!(replacement.selector, monitor.selector);
        assert!(send(
            &tx,
            Action::Destroy {
                display_id: replacement.display_id,
                selector: monitor.selector
            },
            authority(true)
        )
        .await
        .unwrap()
        .is_err());
        assert_eq!(&*destroyed.lock().unwrap(), &[101]);
        drop(tx);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn dropped_capture_receipt_removes_artifact_but_preserves_monitor() {
        let fake = Fake {
            successful_capture: true,
            ..Default::default()
        };
        let destroyed = fake.destroyed.clone();
        let (tx, worker) = runner(fake);
        let monitor = committed_monitor(&tx).await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cancelled.png");
        let image = send(
            &tx,
            Action::Capture {
                selector: monitor.selector.clone(),
                path: path.clone(),
            },
            authority(true),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(path.exists());
        drop(image);
        let readiness = inspected(&tx, status(&monitor.selector), authority(true))
            .await
            .status();
        assert_eq!(readiness["lifecycle_ready"], true);
        assert_eq!(readiness["capture_ready"], false); // A prior screenshot is not current readiness.
        assert_eq!(readiness["ready"], false);
        // The next actor response is a barrier after receipt rollback.
        let _ = committed_monitor(&tx).await;
        assert!(!path.exists());
        assert!(destroyed.lock().unwrap().is_empty());
        send(
            &tx,
            Action::Destroy {
                display_id: monitor.display_id,
                selector: monitor.selector,
            },
            authority(true),
        )
        .await
        .unwrap()
        .unwrap()
        .commit();
        assert_eq!(&*destroyed.lock().unwrap(), &[1]);
        drop(tx);
        worker.await.unwrap().unwrap();
    }

    #[test]
    fn reserved_aliases_leave_physical_linux_and_window_ids_alone() {
        for id in [DISPLAY_ID_MIN, DISPLAY_ID_MIN + 7, DISPLAY_ID_MAX] {
            for value in [
                id.to_string(),
                format!(":{id}"),
                format!("display_{id}"),
                format!(" DISPLAY_00{id} "),
                format!("::+{id}"),
            ] {
                assert!(reserved(&value), "{value}");
            }
            assert!(reject_unsupported(Some("user_session"), Some(id)).is_err());
        }
        for id in [
            0,
            1,
            42,
            99,
            DISPLAY_ID_MIN - 1,
            DISPLAY_ID_MAX + 1,
            0x4000_002a,
        ] {
            assert!(!reserved_id(id));
            assert!(!reserved(&format!("display_{id}")));
        }
        for value in ["user_session", "user", "primary", ":0", ":99"] {
            assert!(!reserved(value));
        }
    }

    #[tokio::test]
    async fn cancelled_create_rolls_back_before_and_after_result_delivery() {
        for in_flight in [true, false] {
            let (started, starting) = oneshot::channel();
            let (release, released) = oneshot::channel();
            let fake = Fake {
                next: 40, // private handles deliberately differ from public IDs
                created: Some(started),
                create_release: Some(released),
                ..Default::default()
            };
            let log = fake.log.clone();
            let destroyed = fake.destroyed.clone();
            let (tx, task) = runner(fake);
            let result = create(&tx);
            starting.await.unwrap();
            if in_flight {
                drop(result);
                release.send(()).unwrap();
            } else {
                release.send(()).unwrap();
                drop(result.await.unwrap().unwrap());
            }
            assert_eq!(
                serde_json::json!(inspected(&tx, Inspection::Inventory, authority(true)).await)
                    ["monitors"],
                serde_json::json!([])
            );
            let next = committed_monitor(&tx).await;
            assert_eq!(next.display_id, DISPLAY_ID_MIN + 1);
            assert_eq!(next.helper_handle, 42);
            assert_eq!(&*destroyed.lock().unwrap(), &[41]);
            assert_eq!(&*log.lock().unwrap(), &["create", "destroy", "create"]);
            drop(tx);
            task.await.unwrap().unwrap();
            assert_eq!(log.lock().unwrap().last(), Some(&"close"));
        }
    }

    #[tokio::test]
    async fn cancelled_capture_finishes_stop_before_queued_destroy() {
        let (capture_started, capturing) = oneshot::channel();
        let (stop_started, stopping) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let fake = Fake {
            capture_started: Some(capture_started),
            stop_started: Some(stop_started),
            stop_release: Some(released),
            ..Default::default()
        };
        let log = fake.log.clone();
        let (tx, task) = runner(fake);
        let monitor = committed_monitor(&tx).await;
        let temp = tempfile::tempdir().unwrap();
        let result = send(
            &tx,
            Action::Capture {
                selector: monitor.selector.clone(),
                path: temp.path().join("unused.png"),
            },
            authority(true),
        );
        capturing.await.unwrap();
        let mut destroy_result = send(
            &tx,
            Action::Destroy {
                display_id: monitor.display_id,
                selector: monitor.selector,
            },
            authority(true),
        );
        drop(result);
        stopping.await.unwrap();
        assert!(matches!(
            destroy_result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(!log.lock().unwrap().contains(&"destroy"));
        release.send(()).unwrap();
        destroy_result.await.unwrap().unwrap().commit();
        drop(tx);
        task.await.unwrap().unwrap();
        assert_eq!(
            &*log.lock().unwrap(),
            &[
                "create",
                "capture_start",
                "capture_stop",
                "capture_stopped",
                "destroy",
                "close"
            ]
        );
    }

    #[tokio::test]
    async fn uncertain_cleanup_permanently_retires_and_never_respawns() {
        let fake = Fake {
            fail_destroy: true,
            ..Default::default()
        };
        let log = fake.log.clone();
        let (tx, task) = runner(fake);
        drop(create(&tx).await.unwrap().unwrap()); // rollback fails
        assert!(create(&tx).await.unwrap().is_err());
        assert_eq!(
            serde_json::json!(inspected(&tx, Inspection::Inventory, authority(true)).await),
            serde_json::json!({"broker_state":"retired", "monitors":[]})
        );
        assert_eq!(&*log.lock().unwrap(), &["create", "destroy", "close"]);
        drop(tx);
        assert!(task.await.unwrap().is_err());
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|v| **v == "create")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn shared_session_authority_is_independent_and_rechecked_when_dequeued() {
        let fake = Fake::default();
        let log = fake.log.clone();
        let (tx, task) = runner(fake);
        let scoped = authority(false);
        let request = || Action::Create {
            width: 640,
            height: 480,
        };
        assert!(send(&tx, request(), scoped.clone()).await.unwrap().is_err());
        assert!(log.lock().unwrap().is_empty());
        scoped.autonomy.write().await.user_display_granted = true;
        let first = send(&tx, request(), scoped.clone()).await.unwrap().unwrap();
        let queued = send(&tx, request(), scoped.clone()); // held behind receipt
        scoped.autonomy.write().await.user_display_granted = false;
        first.commit();
        assert!(queued.await.unwrap().is_err());
        assert_eq!(&*log.lock().unwrap(), &["create"]);
        // Owner-surface authorization doesn't mutate the standing grant.
        let _ = committed_monitor(&tx).await;
        assert!(!scoped.autonomy.read().await.user_display_granted);
        assert!(create(&tx).await.unwrap().is_err()); // capacity two
        drop(tx);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn post_capture_helper_death_or_generation_change_discards_image_and_retires() {
        for death in [true, false] {
            let fake = Fake {
                die_after_capture: death,
                mismatch_after_capture: !death,
                ..Default::default()
            };
            let (tx, task) = runner(fake);
            let monitor = committed_monitor(&tx).await;
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("discard.png");
            let result = send(
                &tx,
                Action::Capture {
                    selector: monitor.selector,
                    path: path.clone(),
                },
                authority(true),
            )
            .await
            .unwrap();
            assert!(result.is_err());
            assert!(!path.exists());
            assert!(create(&tx).await.unwrap().is_err());
            drop(tx);
            assert!(task.await.unwrap().is_err());
        }
    }
    #[test]
    fn generation_binds_owner_even_when_native_id_is_reused() {
        let mut a = State::new();
        let mut b = State::new();
        let first = DISPLAY_ID_MIN;
        let second = first + 1;
        for state in [&mut a, &mut b] {
            state.monitors.insert(
                first,
                Monitor {
                    display_id: first,
                    helper_handle: 1,
                    selector: state.selector(first),
                    native_id: 42,
                    width: 640,
                    height: 480,
                },
            );
        }
        let old = a.selector(first);
        assert!(b.resolve(&old).is_err());
        a.monitors.remove(&first);
        a.monitors.insert(
            second,
            Monitor {
                display_id: second,
                helper_handle: 2,
                selector: a.selector(second),
                native_id: 42,
                width: 640,
                height: 480,
            },
        );
        assert!(a.resolve(&old).is_err());
        for invalid in [
            "macos_virtual",
            " MACOS_VIRTUAL:42 ",
            "macos_virtual:42",
            "42",
            "display_42",
        ] {
            assert!(a.resolve(invalid).is_err());
        }
        assert!(a.resolve(&a.selector(second)).is_ok());
    }
    fn bind_action(monitor: &Monitor) -> Action {
        Action::Window(WindowAction::Bind {
            selector: monitor.selector.clone(),
            identity: placement::tests::identity(),
            candidate: "macos_candidate:00000000000000000000000000000001".into(),
        })
    }
    #[tokio::test]
    async fn window_owner_gates_ignore_user_display_grants_without_starting_native_helper() {
        for grant in [false, true] {
            let a = authority(false);
            a.autonomy.write().await.user_display_granted = grant;
            let b = Broker::default();
            for action in [
                WindowAction::ReadElements {
                    binding: "macos_window:fixture:1".into(),
                },
                WindowAction::ActElement {
                    binding: "macos_window:fixture:1".into(),
                    element: "macos_element:00000000000000000000000000000001".into(),
                    action: controls::ElementAction::Press {},
                },
                WindowAction::List { pid: 123 },
                WindowAction::Bind {
                    selector: format!("macos_virtual:{}:{}", "a".repeat(32), DISPLAY_ID_MIN),
                    identity: placement::tests::identity(),
                    candidate: "macos_candidate:00000000000000000000000000000001".into(),
                },
                WindowAction::Place {
                    binding: "macos_window:fixture:1".into(),
                    bounds: placement::tests::monitor(),
                },
                WindowAction::Unbind {
                    binding: "macos_window:fixture:1".into(),
                },
            ] {
                assert!(b
                    .request(Action::Window(action), a.clone())
                    .await
                    .err()
                    .unwrap()
                    .contains("owner surface"));
                assert!(b.not_started());
            }
        }
    }
    #[tokio::test]
    async fn bind_cancellation_rolls_back_and_never_abandons_monitor_cleanup() {
        for after_receipt in [false, true] {
            let (started, begin) = oneshot::channel();
            let (release, wait) = oneshot::channel();
            let fake = Fake {
                window_started: Some(started),
                window_release: Some(wait),
                ..Default::default()
            };
            let log = fake.log.clone();
            let (tx, worker) = runner(fake);
            let monitor = committed_monitor(&tx).await;
            let receive = send(&tx, bind_action(&monitor), authority(true));
            begin.await.unwrap();
            if after_receipt {
                release.send(()).unwrap();
                drop(receive.await.unwrap().unwrap());
            } else {
                drop(receive);
                release.send(()).unwrap();
            }
            let receipt = send(
                &tx,
                Action::Destroy {
                    display_id: monitor.display_id,
                    selector: monitor.selector,
                },
                authority(true),
            )
            .await
            .unwrap()
            .unwrap();
            receipt.commit();
            drop(tx);
            worker.await.unwrap().unwrap();
            assert_eq!(
                *log.lock().unwrap(),
                vec!["create", "bind", "unbind", "destroy", "close"]
            );
        }
    }
    #[tokio::test]
    async fn queued_cancelled_binding_has_no_native_effect_and_scoped_dequeue_refuses() {
        let fake = Fake::default();
        let log = fake.log.clone();
        let (tx, worker) = runner(fake);
        let creation = create(&tx).await.unwrap().unwrap();
        let Value::Created(monitor) = &creation.value else {
            panic!()
        };
        let monitor = monitor.clone();
        drop(send(&tx, bind_action(&monitor), authority(true)));
        let a = authority(false);
        a.autonomy.write().await.user_display_granted = true;
        let refused = send(&tx, bind_action(&monitor), a);
        creation.commit();
        assert!(refused
            .await
            .unwrap()
            .err()
            .unwrap()
            .contains("owner surface"));
        drop(tx);
        worker.await.unwrap().unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["create", "close"]);
    }
    #[tokio::test]
    async fn public_binding_rollback_stale_monitor_and_partial_result_are_exact() {
        let fake = Fake::default();
        let (tx, worker) = runner(fake);
        let m = committed_monitor(&tx).await;
        let receipt = send(&tx, bind_action(&m), authority(true))
            .await
            .unwrap()
            .unwrap();
        let Value::Window(WindowValue::Bound(b)) = &receipt.value else {
            panic!()
        };
        let b = b.clone();
        let public = serde_json::to_value(&b).unwrap();
        assert!(public.get("helper_binding").is_none());
        receipt.commit();
        let receipt = send(
            &tx,
            Action::Window(WindowAction::Place {
                binding: b.binding.clone(),
                bounds: placement::tests::monitor(),
            }),
            authority(true),
        )
        .await
        .unwrap()
        .unwrap();
        let Value::Window(WindowValue::Placed(result)) = &receipt.value else {
            panic!()
        };
        assert!(!result.verified());
        receipt.commit();
        let receipt = send(
            &tx,
            Action::Destroy {
                display_id: m.display_id,
                selector: m.selector,
            },
            authority(true),
        )
        .await
        .unwrap()
        .unwrap();
        receipt.commit();
        assert!(send(
            &tx,
            Action::Window(WindowAction::Unbind { binding: b.binding }),
            authority(true)
        )
        .await
        .unwrap()
        .is_err());
        drop(tx);
        worker.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn late_liveness_failure_preserves_partial_and_verified_placement() {
        for verified in [false, true] {
            let (tx, worker) = runner(Fake {
                die_after_place: true,
                verified_place: verified,
                ..Default::default()
            });
            let m = committed_monitor(&tx).await;
            let receipt = send(&tx, bind_action(&m), authority(true))
                .await
                .unwrap()
                .unwrap();
            let Value::Window(WindowValue::Bound(b)) = &receipt.value else {
                panic!()
            };
            let binding = b.binding.clone();
            assert!(receipt.commit());
            let receipt = send(
                &tx,
                Action::Window(WindowAction::Place {
                    binding,
                    bounds: placement::tests::monitor(),
                }),
                authority(true),
            )
            .await
            .unwrap()
            .unwrap();
            let Value::Window(WindowValue::PlacementUnconfirmed { result, error }) = &receipt.value
            else {
                panic!("placement result discarded")
            };
            assert_eq!(result.verified(), verified);
            assert!(result.after.is_some());
            assert!(error.contains("helper died"));
            assert!(receipt.commit());
            drop(tx);
            assert!(worker.await.unwrap().is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn dispatched_placement_receipt_deadline_reports_unconfirmed_effects() {
        let (started, begin) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let fake = Fake {
            place_started: Some(started),
            place_release: Some(wait),
            ..Default::default()
        };
        let log = fake.log.clone();
        let (tx, worker) = runner(fake);
        let m = committed_monitor(&tx).await;
        let receipt = send(&tx, bind_action(&m), authority(true))
            .await
            .unwrap()
            .unwrap();
        let Value::Window(WindowValue::Bound(b)) = &receipt.value else {
            panic!()
        };
        let binding = b.binding.clone();
        assert!(receipt.commit());
        let receive = send(
            &tx,
            Action::Window(WindowAction::Place {
                binding,
                bounds: placement::tests::monitor(),
            }),
            authority(true),
        );
        begin.await.unwrap();
        let error = await_reply(receive, true, std::time::Duration::from_millis(1))
            .await
            .err()
            .unwrap();
        assert!(error.starts_with(PLACEMENT_UNCONFIRMED));
        assert!(error.contains("may still be in progress"));
        release.send(()).unwrap();
        drop(tx);
        worker.await.unwrap().unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            vec!["create", "bind", "place", "close"]
        );
    }

    #[tokio::test]
    async fn lost_placement_delivery_reports_unconfirmed_effects() {
        let (reply, receive) = oneshot::channel();
        drop(reply);
        let error = await_reply(receive, true, std::time::Duration::from_secs(1))
            .await
            .err()
            .unwrap();
        assert!(error.starts_with(PLACEMENT_UNCONFIRMED));
    }

    #[tokio::test]
    async fn placement_cancellation_finishes_serialized_attempt_without_hidden_rollback() {
        let (started, begin) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let fake = Fake {
            place_started: Some(started),
            place_release: Some(wait),
            ..Default::default()
        };
        let log = fake.log.clone();
        let (tx, worker) = runner(fake);
        let monitor = committed_monitor(&tx).await;
        let receipt = send(&tx, bind_action(&monitor), authority(true))
            .await
            .unwrap()
            .unwrap();
        let Value::Window(WindowValue::Bound(b)) = &receipt.value else {
            panic!()
        };
        let binding = b.binding.clone();
        receipt.commit();
        let receive = send(
            &tx,
            Action::Window(WindowAction::Place {
                binding: binding.clone(),
                bounds: placement::tests::monitor(),
            }),
            authority(true),
        );
        begin.await.unwrap();
        drop(receive);
        let cleanup = send(
            &tx,
            Action::Window(WindowAction::Unbind { binding }),
            authority(true),
        );
        assert_eq!(*log.lock().unwrap(), vec!["create", "bind", "place"]);
        release.send(()).unwrap();
        cleanup.await.unwrap().unwrap().commit();
        drop(tx);
        worker.await.unwrap().unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            vec!["create", "bind", "place", "unbind", "close"]
        );
    }
    #[test]
    fn expired_receipt_cannot_acknowledge_a_rolled_back_binding() {
        let (commit, receive) = oneshot::channel();
        drop(receive);
        assert!(!Receipt {
            value: Value::Window(WindowValue::Unbound),
            commit
        }
        .commit());
    }
    async fn bound_fixture(tx: &mpsc::Sender<Request>) -> String {
        let monitor = committed_monitor(tx).await;
        let receipt = send(tx, bind_action(&monitor), authority(true))
            .await
            .unwrap()
            .unwrap();
        let Value::Window(WindowValue::Bound(b)) = &receipt.value else {
            panic!("binding")
        };
        let binding = b.binding.clone();
        assert!(receipt.commit());
        binding
    }
    async fn elements_fixture(tx: &mpsc::Sender<Request>, binding: &str) -> (Receipt, String) {
        let receipt = send(
            tx,
            Action::Window(WindowAction::ReadElements {
                binding: binding.into(),
            }),
            authority(true),
        )
        .await
        .unwrap()
        .unwrap();
        let Value::Window(WindowValue::Elements(c)) = &receipt.value else {
            panic!("elements")
        };
        let token = c[0].element.clone();
        (receipt, token)
    }
    fn press_fixture(binding: &str, element: &str) -> Action {
        Action::Window(WindowAction::ActElement {
            binding: binding.into(),
            element: element.into(),
            action: controls::ElementAction::Press {},
        })
    }
    #[tokio::test]
    async fn element_reads_and_actions_never_start_a_helper_or_broker() {
        let broker = Broker::default();
        for action in [
            Action::Window(WindowAction::ReadElements {
                binding: "macos_window:fixture:1".into(),
            }),
            press_fixture(
                "macos_window:fixture:1",
                "macos_element:00000000000000000000000000000001",
            ),
        ] {
            assert!(broker.request(action, authority(true)).await.is_err());
            assert!(broker.not_started());
        }
    }
    #[tokio::test]
    async fn element_snapshot_replacement_and_cross_binding_tokens_do_not_dispatch_effects() {
        let fake = Fake::default();
        let log = fake.log.clone();
        let (tx, worker) = runner(fake);
        let binding = bound_fixture(&tx).await;
        let (receipt, old) = elements_fixture(&tx, &binding).await;
        assert!(receipt.commit());
        let (receipt, new) = elements_fixture(&tx, &binding).await;
        assert!(receipt.commit());
        assert_ne!(old, new);
        assert!(send(&tx, press_fixture(&binding, &old), authority(true))
            .await
            .unwrap()
            .is_err());
        // The foreign-token attempt itself consumed the fresh snapshot.
        assert!(send(&tx, press_fixture(&binding, &new), authority(true))
            .await
            .unwrap()
            .is_err());
        let (receipt, new) = elements_fixture(&tx, &binding).await;
        assert!(receipt.commit());
        assert!(send(
            &tx,
            press_fixture("macos_window:foreign:1", &new),
            authority(true)
        )
        .await
        .unwrap()
        .is_err());
        assert!(send(&tx, press_fixture(&binding, &new), authority(true))
            .await
            .unwrap()
            .unwrap()
            .commit());
        assert!(send(&tx, press_fixture(&binding, &new), authority(true))
            .await
            .unwrap()
            .is_err());
        drop(tx);
        worker.await.unwrap().unwrap();
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|&&x| x == "element_action")
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn cancellation_before_element_dispatch_is_no_effect_and_after_is_uncertain_once() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (started, begin) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let fake = Fake {
            place_started: Some(started),
            place_release: Some(wait),
            ..Default::default()
        };
        let log = fake.log.clone();
        let (tx, worker) = runner(fake);
        let binding = bound_fixture(&tx).await;
        let (receipt, token) = elements_fixture(&tx, &binding).await;
        // Hold the read receipt so the action is definitely queued, then close.
        let progress = Arc::new(AtomicBool::new(false));
        let (reply, receive) = oneshot::channel();
        assert!(tx
            .send(Request {
                action: press_fixture(&binding, &token),
                authority: authority(true),
                reply,
                element_dispatch: Some(progress.clone()),
            })
            .await
            .is_ok());
        let error = await_element_reply(receive, &progress, std::time::Duration::from_millis(1))
            .await
            .err()
            .unwrap();
        assert!(!error.starts_with(ELEMENT_UNCONFIRMED));
        assert!(!progress.load(Ordering::SeqCst));
        assert!(receipt.commit());
        let (reply, receive) = oneshot::channel();
        assert!(tx
            .send(Request {
                action: press_fixture(&binding, &token),
                authority: authority(true),
                reply,
                element_dispatch: Some(progress.clone()),
            })
            .await
            .is_ok());
        begin.await.unwrap();
        let error = await_element_reply(receive, &progress, std::time::Duration::from_millis(1))
            .await
            .err()
            .unwrap();
        assert!(error.starts_with(ELEMENT_UNCONFIRMED));
        release.send(()).unwrap();
        // Serialized replay cannot duplicate the dispatched effect.
        assert!(send(&tx, press_fixture(&binding, &token), authority(true))
            .await
            .unwrap()
            .is_err());
        drop(tx);
        worker.await.unwrap().unwrap();
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|&&x| x == "element_action")
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn action_receipt_expiry_and_final_liveness_preserve_evidence_and_never_rollback() {
        for dead in [false, true] {
            let fake = Fake {
                die_after_place: dead,
                ..Default::default()
            };
            let log = fake.log.clone();
            let (tx, worker) = runner(fake);
            let binding = bound_fixture(&tx).await;
            let (receipt, token) = elements_fixture(&tx, &binding).await;
            assert!(receipt.commit());
            let receipt = send(&tx, press_fixture(&binding, &token), authority(true))
                .await
                .unwrap()
                .unwrap();
            match &receipt.value {
                Value::Window(WindowValue::Acted(r)) if !dead => {
                    assert!(r.action_attempted && r.successful())
                }
                Value::Window(WindowValue::ActionUnconfirmed { result, error }) if dead => {
                    assert!(result.action_attempted && result.successful());
                    assert!(error.contains("died"));
                }
                _ => panic!("lost evidence"),
            }
            // Exercise the actor's actual receipt expiry, not a synthetic error.
            let replay = send(&tx, press_fixture(&binding, &token), authority(true));
            assert!(replay.await.unwrap().is_err());
            assert!(!receipt.commit());
            drop(tx);
            assert_eq!(worker.await.unwrap().is_err(), dead);
            assert_eq!(
                log.lock()
                    .unwrap()
                    .iter()
                    .filter(|&&x| x == "element_action")
                    .count(),
                1
            );
            assert!(!log.lock().unwrap().contains(&"unbind"));
        }
    }
}

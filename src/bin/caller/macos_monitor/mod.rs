//! Daemon-scoped macOS monitor broker. These monitors share WindowServer with
//! the owner; ownership is never a display grant. No public display registry,
//! streaming, browser placement, input or primary-display fallback.

mod capture;
mod helper;
mod process;
mod protocol;
mod smoke;

use crate::autonomy::SharedAutonomy;
use protocol::{Operation, Outcome};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::sync::{mpsc, oneshot};

const QUEUE_SIZE: usize = 8;
// Below the macOS window range (0x40000000); never native/helper IDs.
pub(crate) const DISPLAY_ID_MIN: u32 = 0x2000_0000;
pub(crate) const DISPLAY_ID_MAX: u32 = 0x3fff_ffff;
pub(crate) const UNSUPPORTED: &str = "macos_virtual selectors support only exact read-only take_screenshot and destroy_virtual_display; input, AX, browser placement, shared views and streaming are unavailable";

/// Deliberately broad reservation: malformed/case/whitespace variants must
/// never fall through an old parser to :99 or the user's primary display.
pub(crate) fn reserved(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    if value.contains("macos_virtual") {
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
    pub(crate) fn commit(self) {
        let _ = self.commit.send(());
    }
}

pub(crate) enum Action {
    Create { width: u32, height: u32 },
    Destroy { display_id: u32, selector: String },
    Capture { selector: String, path: PathBuf },
}

struct Request {
    action: Action,
    authority: Authority,
    reply: oneshot::Sender<Result<Receipt, String>>,
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
        authority.check().await?;
        if let Action::Create { width, height } = action {
            validate_dimensions(width, height)?;
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
        let (reply, receive) = oneshot::channel();
        sender
            .try_send(Request {
                action,
                authority,
                reply,
            })
            .map_err(|_| "macOS monitor broker unavailable or busy")?;
        // This deadline only cancels the caller's receipt. The worker retains
        // child/backend ownership through stop/rollback; it is never aborted.
        tokio::time::timeout(std::time::Duration::from_secs(20), receive)
            .await
            .map_err(|_| "monitor request deadline exceeded; broker retains cleanup ownership")?
            .map_err(|_| "macOS monitor broker retired")?
    }
}

struct State {
    owner: String,
    last_handle: u32,
    next_display_id: u32,
    monitors: BTreeMap<u32, Monitor>,
}

impl State {
    fn new() -> Self {
        Self {
            owner: uuid::Uuid::new_v4().simple().to_string(),
            last_handle: 0,
            next_display_id: DISPLAY_ID_MIN,
            monitors: BTreeMap::new(),
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
    let mut process = None;
    let mut retired: Option<String> = None;
    while let Some(mut request) = requests.recv().await {
        if request.reply.is_closed() {
            continue;
        }
        if let Err(e) = request.authority.check().await {
            let _ = request.reply.send(Err(e));
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
        if let Err(e) = request.authority.check().await {
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
                let artifact = match &value {
                    Value::Captured(s) => Some(s.path.clone()),
                    _ => None,
                };
                let (commit, committed) = oneshot::channel();
                // A failed send drops the receipt and therefore the commit
                // sender. A dropped frontend future does exactly the same.
                let authorized = request.authority.check().await;
                let live = child.live(); // last synchronous check before result delivery
                if let Err(error) = &live {
                    retired = Some(error.clone());
                }
                if let Err(error) = authorized.and(live) {
                    drop(commit);
                    let _ = request.reply.send(Err(error));
                } else {
                    let _ = request.reply.send(Ok(Receipt { value, commit }));
                }
                if committed.await.is_err() {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

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
        next: u32,
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
                Operation::Resolve { handle } => Ok(Outcome::Monitor {
                    handle,
                    native_id: 42
                        + handle
                        + u32::from(self.captured && self.mismatch_after_capture),
                    width: 640,
                    height: 480,
                }),
                Operation::Destroy { handle } => {
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
                reply
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
}

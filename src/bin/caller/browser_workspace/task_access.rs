//! Session-owned browser resources. A permit is minted only by the daemon's
//! task service, never parsed from a tool argument and never an owner role.
//! The public task API chooses no session, native handle, profile or endpoint.
use super::*;
use crate::access::actor::{ActorBinding, ActorKind};
use crate::macos_monitor::{Action as MonitorAction, Inspection, WindowAction};
use crate::peer::access_policy::PeerOperation;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex as StdMutex,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[cfg(any(target_os = "macos", test))]
mod extension_ui;
#[cfg(target_os = "macos")]
mod headless_extension;
mod lifecycle;
pub(crate) use lifecycle::{execute, Request, Response};

const MAX_TASK_BROWSERS: usize = 2;
const WIDTH: u32 = 1024;
const HEIGHT: u32 = 768;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionKey {
    principal: String,
    session: String,
    epoch: String,
    incarnation: u64,
}

#[async_trait::async_trait]
trait SessionProbe: Send + Sync {
    async fn resolve(
        &self,
        actor: &ActorBinding,
        epoch: &str,
        input: bool,
    ) -> Result<SessionKey, String>;
}

struct DaemonProbe {
    cert_dir: PathBuf,
    configuration_conflict: Arc<AtomicBool>,
}
#[async_trait::async_trait]
impl SessionProbe for DaemonProbe {
    async fn resolve(
        &self,
        actor: &ActorBinding,
        epoch: &str,
        input: bool,
    ) -> Result<SessionKey, String> {
        if self.configuration_conflict.load(Ordering::SeqCst) {
            return Err("task browser gateway policy context changed".into());
        }
        let (session, principal) = actor_identity(actor, epoch)?;
        let live = crate::session_supervisor::published_live_session_registry()
            .ok_or("task browser needs a live supervised session")?;
        let incarnation = live
            .live_incarnation(session)
            .await
            .ok_or("task browser session ended or is unavailable")?;
        if crate::web_gateway::supervised_mcp_registration_epoch(session).as_deref() != Some(epoch)
        {
            return Err("task browser session credential was replaced".into());
        }
        // Resolve the SAME local IAM rule as ingress, at use time. An expired
        // or revoked binding must not fall back to a transport-default role.
        let access =
            crate::web_gateway::mcp_agent_session_context(&self.cert_dir, session, "http", true)
                .map_err(|(_, error)| error)?;
        if access.principal.id != principal {
            return Err("task browser IAM principal changed".into());
        }
        for op in [PeerOperation::RuntimeControl, PeerOperation::DisplayView]
            .into_iter()
            .chain(input.then_some(PeerOperation::DisplayInput))
        {
            if !access.decision(op).allowed {
                return Err(format!(
                    "task browser requires current {} permission",
                    crate::access::iam::operation_permission_id(op)
                ));
            }
        }
        Ok(SessionKey {
            principal: principal.into(),
            session: session.into(),
            epoch: epoch.into(),
            incarnation,
        })
    }
}

fn actor_identity<'a>(actor: &'a ActorBinding, epoch: &str) -> Result<(&'a str, &'a str), String> {
    if actor.kind != ActorKind::AgentSession || epoch.is_empty() {
        return Err("task browser requires the authenticated supervised-session credential".into());
    }
    let session = actor
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("task browser requires a gate-bound session")?;
    let principal = actor
        .principal_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("task browser requires a gate-bound IAM principal")?;
    Ok((session, principal))
}

/// Allocated per EventBus, not process-global. Gateway supplies its actual IAM
/// directory; tests inject a probe and never consult the machine's real store.
#[derive(Default)]
pub(crate) struct Registry {
    probe: OnceLock<Arc<dyn SessionProbe>>,
    configured_path: StdMutex<Option<PathBuf>>,
    configuration_conflict: Arc<AtomicBool>,
    entries: Mutex<BTreeMap<String, Arc<Allocation>>>,
}
impl Registry {
    pub(crate) fn configure(&self, cert_dir: PathBuf) {
        let Ok(mut configured) = self.configured_path.lock() else {
            self.configuration_conflict.store(true, Ordering::SeqCst);
            return;
        };
        if configured.as_ref().is_some_and(|old| old != &cert_dir) {
            self.configuration_conflict.store(true, Ordering::SeqCst);
            return;
        }
        *configured = Some(cert_dir.clone());
        let _ = self.probe.set(Arc::new(DaemonProbe {
            cert_dir,
            configuration_conflict: self.configuration_conflict.clone(),
        }));
    }
    async fn identity(
        &self,
        actor: &ActorBinding,
        epoch: Option<&str>,
        input: bool,
    ) -> Result<(SessionKey, Arc<dyn SessionProbe>), String> {
        if self.configuration_conflict.load(Ordering::SeqCst) {
            return Err("task browser gateway policy context is inconsistent".into());
        }
        let epoch = epoch.ok_or("task browser requires a registered session credential")?;
        actor_identity(actor, epoch)?;
        let probe = self
            .probe
            .get()
            .ok_or("task browser gateway is not configured")?
            .clone();
        let key = probe.resolve(actor, epoch, input).await?;
        Ok((key, probe))
    }
    async fn existing(&self, key: &SessionKey) -> Result<Arc<Allocation>, String> {
        let entry = self
            .entries
            .lock()
            .await
            .get(&key.session)
            .cloned()
            .ok_or("no task browser; call open first")?;
        if entry.key != *key {
            return Err("task browser belongs to a replaced session; cleanup is pending".into());
        }
        Ok(entry)
    }
    pub(crate) async fn invalidate_workspace(&self, id: &str) {
        for entry in self.entries.lock().await.values() {
            if entry
                .resources
                .lock()
                .is_ok_and(|r| r.workspace.as_deref() == Some(id))
            {
                entry.revoked.store(true, Ordering::SeqCst);
            }
        }
    }
    async fn forget(&self, entry: &Arc<Allocation>) {
        let mut entries = self.entries.lock().await;
        if entries
            .get(&entry.key.session)
            .is_some_and(|e| Arc::ptr_eq(e, entry))
        {
            entries.remove(&entry.key.session);
        }
    }
}

#[derive(Clone, Default)]
struct Resources {
    monitor: Option<crate::macos_monitor::Monitor>,
    workspace: Option<String>,
    profile: Option<PathBuf>,
    // Pinned by the trusted reservation; cleanup ownership begins only after
    // create_fresh_macos_profile succeeds and record_profile acknowledges it.
    expected_profile: Option<PathBuf>,
    stage: &'static str,
    process: Option<(u32, (u64, u32))>,
    binding: Option<String>,
    page: Option<String>,
    window: Option<crate::macos_monitor::placement::Observation>,
    ready: bool,
    requested_extension: Option<super::task_extension::TaskExtension>,
    extension: Option<BrowserWorkspaceExtension>,
    #[cfg(target_os = "macos")]
    headless: Option<Arc<headless_extension::Workspace>>,
}
struct Allocation {
    key: SessionKey,
    actor: ActorBinding,
    probe: Arc<dyn SessionProbe>,
    autonomy: crate::autonomy::SharedAutonomy,
    resources: StdMutex<Resources>,
    lane: Arc<Mutex<()>>,
    revoked: AtomicBool,
    quarantined: AtomicBool,
    requests: StdMutex<std::collections::BTreeSet<String>>,
}
impl Allocation {
    fn new(
        key: SessionKey,
        actor: ActorBinding,
        probe: Arc<dyn SessionProbe>,
        autonomy: crate::autonomy::SharedAutonomy,
    ) -> Self {
        Self {
            key,
            actor,
            probe,
            autonomy,
            resources: StdMutex::new(Resources::default()),
            lane: Arc::new(Mutex::new(())),
            revoked: AtomicBool::new(false),
            quarantined: AtomicBool::new(false),
            requests: StdMutex::new(Default::default()),
        }
    }
    async fn check(&self, input: bool) -> Result<(), String> {
        if self.revoked.load(Ordering::SeqCst) {
            return Err("task browser has been stopped or revoked".into());
        }
        let key = self
            .probe
            .resolve(&self.actor, &self.key.epoch, input)
            .await?;
        if key != self.key || self.revoked.load(Ordering::SeqCst) {
            return Err("task browser session identity changed".into());
        }
        if input && self.quarantined.load(Ordering::SeqCst) {
            return Err(
                "task browser input is uncertain; inspect and close before further input".into(),
            );
        }
        Ok(())
    }
    async fn admit(self: &Arc<Self>, input: bool) -> Result<OwnedMutexGuard<()>, String> {
        self.check(input).await?;
        let guard = self
            .lane
            .clone()
            .try_lock_owned()
            .map_err(|_| "task browser is busy; no action queued")?;
        // A cloned handle is not sufficient after revocation/reassignment.
        self.check(input).await?;
        Ok(guard)
    }
    fn check_workspace_id(&self, id: &str) -> Result<(), String> {
        let resources = self
            .resources
            .lock()
            .map_err(|_| "task resource tracking unavailable")?;
        if resources.workspace.as_deref() != Some(id) {
            return Err("task workspace generation changed".into());
        }
        Ok(())
    }
    fn claim(&self, id: &str) -> Result<(), String> {
        let uuid = uuid::Uuid::parse_str(id).map_err(|_| "request_id must be a canonical UUID")?;
        if uuid.to_string() != id {
            return Err("request_id must be a canonical UUID".into());
        }
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| "request ledger unavailable")?;
        if requests.contains(id) {
            return Err("duplicate request; input will not be replayed".into());
        }
        if requests.len() >= 8192 {
            return Err("task browser request ledger is full".into());
        }
        requests.insert(id.into());
        Ok(())
    }
    fn authority(self: &Arc<Self>, mode: Mode) -> crate::macos_monitor::Authority {
        crate::macos_monitor::Authority {
            owner_surface: false,
            autonomy: self.autonomy.clone(),
            task: Some(Permit {
                allocation: self.clone(),
                mode,
                cancelled: None,
            }),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Provision,
    Observe,
    Input,
    Cleanup,
}

/// Sealed exact-resource authority. No Deserialize, no public constructor or
/// mutable fields. Only the task service can create it after authenticated IAM
/// admission. Cleanup can retire only resources that this allocation created.
#[derive(Clone)]
pub(crate) struct Permit {
    allocation: Arc<Allocation>,
    mode: Mode,
    cancelled: Option<Arc<AtomicBool>>,
}
impl Permit {
    pub(crate) async fn check(&self) -> Result<(), String> {
        if self.mode == Mode::Cleanup {
            return Ok(());
        }
        if self
            .cancelled
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst))
        {
            return Err("task browser request cancelled before dispatch".into());
        }
        self.allocation
            .check(matches!(self.mode, Mode::Provision | Mode::Input))
            .await?;
        if self
            .cancelled
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst))
        {
            return Err("task browser request cancelled before dispatch".into());
        }
        Ok(())
    }
    pub(crate) fn cleanup(&self) -> Self {
        Self {
            allocation: self.allocation.clone(),
            mode: Mode::Cleanup,
            cancelled: None,
        }
    }
    pub(crate) fn allows(&self, action: &MonitorAction) -> bool {
        let Ok(r) = self.allocation.resources.lock() else {
            return false;
        };
        let selector_is = |s: &str| r.monitor.as_ref().is_some_and(|m| m.selector == s);
        let binding_is = |s: &str| r.binding.as_deref() == Some(s);
        match action {
            MonitorAction::Create { width, height } => {
                self.mode == Mode::Provision
                    && r.monitor.is_none()
                    && *width == WIDTH
                    && *height == HEIGHT
            }
            MonitorAction::Inspect(Inspection::Status { selector }) => selector_is(selector),
            MonitorAction::Capture { selector, path } => {
                matches!(self.mode, Mode::Observe | Mode::Input)
                    && r.ready
                    && path.is_none()
                    && selector_is(selector)
            }
            MonitorAction::Destroy {
                display_id,
                selector,
            } => {
                self.mode == Mode::Cleanup
                    && r.monitor
                        .as_ref()
                        .is_some_and(|m| m.display_id == *display_id && m.selector == *selector)
            }
            MonitorAction::Window(window) => match window {
                WindowAction::List { pid } => {
                    self.mode == Mode::Provision
                        && !r.ready
                        && r.process.is_some_and(|(p, _)| p == *pid as u32)
                }
                WindowAction::Bind {
                    selector, identity, ..
                } => {
                    self.mode == Mode::Provision
                        && !r.ready
                        && r.binding.is_none()
                        && selector_is(selector)
                        && r.process
                            == Some((
                                identity.pid as u32,
                                (identity.start_seconds, identity.start_micros),
                            ))
                }
                WindowAction::Place { binding, .. } => {
                    self.mode == Mode::Provision && !r.ready && binding_is(binding)
                }
                WindowAction::Unbind { binding } => {
                    self.mode == Mode::Cleanup && binding_is(binding)
                }
                #[cfg(any(target_os = "macos", test))]
                WindowAction::ValidatePageWindow { binding } => {
                    matches!(self.mode, Mode::Observe | Mode::Input)
                        && r.ready
                        && binding_is(binding)
                }
                WindowAction::PrepareClick { binding, .. }
                | WindowAction::Click { binding, .. }
                | WindowAction::PrepareScroll { binding, .. }
                | WindowAction::Scroll { binding, .. } => {
                    self.mode == Mode::Input && r.ready && binding_is(binding)
                }
                _ => false,
            },
            _ => false,
        }
    }
    pub(crate) fn creates_on(&self, selector: &str) -> bool {
        self.mode == Mode::Provision
            && self.allocation.resources.lock().is_ok_and(|r| {
                !r.ready && r.monitor.as_ref().is_some_and(|m| m.selector == selector)
            })
    }
    pub(crate) fn cleans_workspace(&self, id: &str, selector: Option<&str>) -> bool {
        self.mode == Mode::Cleanup
            && self.allocation.resources.lock().is_ok_and(|r| {
                r.workspace.as_deref() == Some(id)
                    && r.monitor
                        .as_ref()
                        .is_some_and(|m| Some(m.selector.as_str()) == selector)
            })
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn allows_page_input(&self, workspace: &BrowserWorkspace) -> bool {
        self.mode == Mode::Input && self.uses_workspace(workspace)
    }
    pub(crate) fn uses_workspace(&self, workspace: &BrowserWorkspace) -> bool {
        matches!(self.mode, Mode::Observe | Mode::Input)
            && self.allocation.resources.lock().is_ok_and(|r| {
                r.ready
                    && (workspace.status == BrowserWorkspaceStatus::Ready
                        || (self.mode == Mode::Observe
                            && workspace.status == BrowserWorkspaceStatus::Error))
                    && r.workspace.as_deref() == Some(workspace.id.as_str())
                    && r.monitor.as_ref().is_some_and(|m| {
                        workspace.display_target.as_deref() == Some(m.selector.as_str())
                    })
                    && r.binding.is_some()
                    && workspace.macos_window_binding == r.binding
                    && r.page.is_some()
                    && workspace.active_target_id == r.page
                    && r.process
                        .is_some_and(|(pid, _)| workspace.process_id == Some(pid))
            })
    }
    pub(super) fn record_workspace(&self, workspace: &BrowserWorkspace) -> Result<(), String> {
        let mut r = self
            .allocation
            .resources
            .lock()
            .map_err(|_| "resource tracking unavailable")?;
        if self.mode != Mode::Provision
            || r.ready
            || r.monitor
                .as_ref()
                .is_none_or(|m| workspace.display_target.as_deref() != Some(m.selector.as_str()))
            || r.workspace.is_some()
        {
            return Err("unexpected task browser reservation".into());
        }
        let profile = workspace
            .profile_dir
            .as_deref()
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or("task reservation has no absolute managed profile")?;
        r.workspace = Some(workspace.id.clone());
        r.expected_profile = Some(profile);
        r.stage = "workspace_reserved";
        Ok(())
    }
    pub(super) fn record_profile(&self, profile: &Path) -> Result<(), String> {
        let mut r = self
            .allocation
            .resources
            .lock()
            .map_err(|_| "resource tracking unavailable")?;
        if self.mode != Mode::Provision
            || r.ready
            || r.profile.is_some()
            || !profile.is_absolute()
            || r.workspace.is_none()
            || r.expected_profile.as_deref() != Some(profile)
        {
            return Err("unexpected task-owned profile".into());
        }
        r.profile = Some(profile.to_path_buf());
        r.stage = "profile_created";
        Ok(())
    }
    #[cfg(target_os = "macos")]
    pub(super) fn record_placement(
        &self,
        result: &crate::macos_monitor::placement::PlacementResult,
    ) -> Result<(), String> {
        let mut r = self
            .allocation
            .resources
            .lock()
            .map_err(|_| "resource tracking unavailable")?;
        if self.mode != Mode::Provision || r.ready || !result.verified() || r.window.is_some() {
            return Err("unexpected task browser placement".into());
        }
        r.stage = "window_placed";
        r.window = Some(
            result
                .after
                .ok_or("task browser placement observation missing")?,
        );
        Ok(())
    }
    pub(crate) fn checks_window(
        &self,
        window: crate::macos_monitor::placement::Observation,
    ) -> bool {
        self.allocation.resources.lock().is_ok_and(|r| {
            r.window
                .is_some_and(|expected| expected.ax == window.ax && expected.cg == window.cg)
        })
    }
    #[cfg(target_os = "macos")]
    pub(super) fn record_process(&self, pid: u32, birth: (u64, u32)) -> Result<(), String> {
        let mut r = self
            .allocation
            .resources
            .lock()
            .map_err(|_| "resource tracking unavailable")?;
        if self.mode != Mode::Provision || r.ready || r.process.is_some() || r.workspace.is_none() {
            return Err("unexpected task browser process".into());
        }
        r.process = Some((pid, birth));
        r.stage = "native_process_identified";
        Ok(())
    }
    #[cfg(target_os = "macos")]
    pub(super) fn record_binding(&self, binding: &str) -> Result<(), String> {
        let mut r = self
            .allocation
            .resources
            .lock()
            .map_err(|_| "resource tracking unavailable")?;
        if self.mode != Mode::Provision || r.ready || r.binding.is_some() || r.process.is_none() {
            return Err("unexpected task browser binding".into());
        }
        r.binding = Some(binding.into());
        r.stage = "window_bound";
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(super) struct Probe {
        current: StdMutex<SessionKey>,
        alive: AtomicBool,
        input: AtomicBool,
    }
    #[async_trait::async_trait]
    impl SessionProbe for Probe {
        async fn resolve(
            &self,
            actor: &ActorBinding,
            epoch: &str,
            input: bool,
        ) -> Result<SessionKey, String> {
            let (session, principal) = actor_identity(actor, epoch)?;
            let key = self.current.lock().unwrap().clone();
            if !self.alive.load(Ordering::SeqCst)
                || (input && !self.input.load(Ordering::SeqCst))
                || key.session != session
                || key.principal != principal
                || key.epoch != epoch
            {
                return Err("test current identity/permission denied".into());
            }
            Ok(key)
        }
    }
    pub(super) fn fixture() -> (Arc<Allocation>, Arc<Probe>) {
        let key = SessionKey {
            principal: "principal:test-session".into(),
            session: "test-session".into(),
            epoch: "test-epoch".into(),
            incarnation: 7,
        };
        let actor = ActorBinding::agent_session(Some(key.principal.clone()), key.session.clone());
        let probe = Arc::new(Probe {
            current: StdMutex::new(key.clone()),
            alive: AtomicBool::new(true),
            input: AtomicBool::new(true),
        });
        let autonomy = crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default());
        let entry = Arc::new(Allocation::new(key, actor, probe.clone(), autonomy));
        {
            let mut r = entry.resources.lock().unwrap();
            r.monitor = Some(crate::macos_monitor::Monitor::task_fixture(
                crate::macos_monitor::DISPLAY_ID_MIN,
                WIDTH,
                HEIGHT,
            ));
            r.workspace = Some("bw-task".into());
            r.binding = Some("macos_window:task:1".into());
            r.page = Some("page-task".into());
            r.process = Some((123, (1000, 7)));
            r.ready = true;
            let bounds = crate::macos_monitor::placement::Bounds {
                x: 40.0,
                y: 40.0,
                width: 720.0,
                height: 530.0,
            };
            r.window = Some(crate::macos_monitor::placement::Observation {
                ax: bounds,
                cg: bounds,
            });
        }
        (entry, probe)
    }
    fn selector(entry: &Allocation) -> String {
        entry
            .resources
            .lock()
            .unwrap()
            .monitor
            .as_ref()
            .unwrap()
            .selector
            .clone()
    }

    #[tokio::test]
    async fn task_permit_is_not_owner_or_a_global_display_grant() {
        let (entry, _) = fixture();
        let authority = entry.authority(Mode::Input);
        assert!(!authority.owner_surface);
        assert!(!authority.autonomy.read().await.user_display_granted);
        assert!(authority.check().await.is_ok());
        let p = authority.task.unwrap();
        for forbidden in [
            MonitorAction::Create {
                width: WIDTH,
                height: HEIGHT,
            },
            MonitorAction::Inspect(Inspection::Inventory),
            MonitorAction::Capture {
                selector: "display_0".into(),
                path: None,
            },
            MonitorAction::Capture {
                selector: selector(&entry),
                path: Some(PathBuf::from("/tmp/not-allowed.png")),
            },
            MonitorAction::Window(WindowAction::List { pid: 123 }),
            MonitorAction::Window(WindowAction::Unbind {
                binding: "macos_window:task:1".into(),
            }),
            MonitorAction::Window(WindowAction::PrepareArrow {
                binding: "macos_window:task:1".into(),
            }),
        ] {
            assert!(!p.allows(&forbidden));
        }
        assert!(p.allows(&MonitorAction::Capture {
            selector: selector(&entry),
            path: None
        }));
        assert!(p.allows(&MonitorAction::Window(WindowAction::PrepareClick {
            binding: "macos_window:task:1".into(),
            point: crate::macos_monitor::pointer::Point { x: 10.0, y: 20.0 },
        })));
        assert!(
            !p.allows(&MonitorAction::Window(WindowAction::PrepareClick {
                binding: "macos_window:foreign:1".into(),
                point: crate::macos_monitor::pointer::Point { x: 10.0, y: 20.0 },
            }))
        );
    }

    #[tokio::test]
    async fn observation_permit_cannot_inject_even_into_its_own_window() {
        let (entry, _) = fixture();
        let p = entry.authority(Mode::Observe).task.unwrap();
        assert!(
            !p.allows(&MonitorAction::Window(WindowAction::PrepareClick {
                binding: "macos_window:task:1".into(),
                point: crate::macos_monitor::pointer::Point { x: 10.0, y: 20.0 },
            }))
        );
        assert!(
            p.allows(&MonitorAction::Window(WindowAction::ValidatePageWindow {
                binding: "macos_window:task:1".into()
            }))
        );
    }

    #[tokio::test]
    async fn current_identity_and_input_permission_are_rechecked_on_every_operation() {
        let (entry, probe) = fixture();
        let p = entry.authority(Mode::Input).task.unwrap();
        assert!(p.check().await.is_ok());
        probe.input.store(false, Ordering::SeqCst);
        assert!(p.check().await.is_err());
        assert!(entry.authority(Mode::Observe).check().await.is_ok());
        probe.input.store(true, Ordering::SeqCst);
        probe.current.lock().unwrap().incarnation += 1;
        assert!(p.check().await.is_err());
        probe.current.lock().unwrap().incarnation -= 1;
        probe.current.lock().unwrap().epoch = "replacement".into();
        assert!(p.check().await.is_err());
    }

    #[tokio::test]
    async fn cloned_permit_is_invalid_after_revoke_but_cleanup_stays_exact() {
        let (entry, _) = fixture();
        let p = entry.authority(Mode::Input).task.unwrap();
        let cloned = p.clone();
        entry.revoked.store(true, Ordering::SeqCst);
        assert!(cloned.check().await.is_err());
        let cleanup = p.cleanup();
        assert!(cleanup.check().await.is_ok());
        assert!(cleanup.cleans_workspace("bw-task", Some(&selector(&entry))));
        assert!(!cleanup.cleans_workspace("bw-foreign", Some(&selector(&entry))));
        assert!(!cleanup.allows(&MonitorAction::Capture {
            selector: selector(&entry),
            path: None
        }));
        assert!(!cleanup.allows(&MonitorAction::Destroy {
            display_id: crate::macos_monitor::DISPLAY_ID_MIN + 1,
            selector: selector(&entry)
        }));
    }

    #[tokio::test]
    async fn revoke_waits_for_admitted_operation_and_blocks_every_later_admission() {
        let (entry, _) = fixture();
        let guard = entry.admit(true).await.unwrap();
        entry.revoked.store(true, Ordering::SeqCst);
        assert!(entry.admit(true).await.is_err());
        let close = entry.lane.clone().lock_owned();
        tokio::pin!(close);
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut close)
            .await
            .is_err());
        drop(guard);
        let _closed = close.await;
        assert!(entry.admit(true).await.is_err());
    }

    #[tokio::test]
    async fn busy_lane_refuses_instead_of_queueing_stale_input() {
        let (entry, _) = fixture();
        let _guard = entry.admit(true).await.unwrap();
        assert!(entry.admit(true).await.unwrap_err().contains("busy"));
    }

    #[test]
    fn requests_never_replay_or_evict_old_ids() {
        let (entry, _) = fixture();
        let first = uuid::Uuid::new_v4().to_string();
        entry.claim(&first).unwrap();
        assert!(entry.claim(&first).unwrap_err().contains("duplicate"));
        assert!(entry.claim("not-a-uuid").is_err());
        let mut set = entry.requests.lock().unwrap();
        for n in 1..8192 {
            set.insert(uuid::Uuid::from_u128(n).to_string());
        }
        drop(set);
        assert!(entry
            .claim(&uuid::Uuid::new_v4().to_string())
            .unwrap_err()
            .contains("full"));
        assert!(entry.claim(&first).unwrap_err().contains("duplicate"));
    }

    #[tokio::test]
    async fn request_cancellation_does_not_grant_cleanup_or_replay_input() {
        let (entry, _) = fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut p = entry.authority(Mode::Input).task.unwrap();
        p.cancelled = Some(cancel.clone());
        assert!(p.check().await.is_ok());
        cancel.store(true, Ordering::SeqCst);
        assert!(p.check().await.is_err());
        assert!(p.cleanup().check().await.is_ok());
        assert!(!p
            .cleanup()
            .allows(&MonitorAction::Window(WindowAction::Click {
                binding: "macos_window:task:1".into(),
                token: "some-token".into()
            })));
    }

    #[test]
    fn tool_shape_cannot_forge_session_endpoints_or_owner_authority() {
        for request in [
            json!({"op":"open","url":"https://example.test","session_id":"foreign"}),
            json!({"op":"open","url":"https://example.test","owner_surface":true}),
            json!({"op":"screenshot","display_target":"display_0"}),
            json!({"op":"keyboard","request_id":uuid::Uuid::new_v4().to_string(),"action":{"type":"key","key":"Cmd+Q"}}),
            json!({"op":"open","url":"https://example.test","profile_dir":"/Users/personal"}),
            json!({"op":"screenshot","path":"/tmp/frame.png"}),
        ] {
            assert!(serde_json::from_value::<Request>(request).is_err());
        }
    }

    #[tokio::test]
    async fn actual_raw_and_facade_dispatch_refuse_unregistered_owner_or_forged_session() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::mcp::tests::test_state_with_log_dir(root.path().to_path_buf());
        let bus = EventBus::new();
        let (_home, server) = crate::mcp::tests::test_server(state, bus.clone());
        for (tool, args) in [
            ("task_browser", json!({"op":"open","url":"about:blank"})),
            ("act", json!({"argv":["browser","task-open","about:blank"]})),
            (
                "inspect",
                json!({"argv":["browser","task-screenshot","bw-fixture"]}),
            ),
        ] {
            let response = server.call_tool_by_name(tool, args).await.unwrap();
            assert_eq!(response.is_error, Some(true));
            assert!(bus.macos_monitors.not_started());
            assert!(bus
                .macos_monitors
                .task_browsers
                .entries
                .lock()
                .await
                .is_empty());
        }
    }

    #[test]
    fn shared_api_is_advertised_to_supervised_agents_and_classified_as_control() {
        use crate::mcp::*;
        assert!(tool_allowed_for_profile(
            "task_browser",
            false,
            Some("codex")
        ));
        assert_eq!(
            mcp_tool_operation("task_browser"),
            PeerOperation::RuntimeControl
        );
        let args = json!({"argv":["browser","task-open","about:blank"]});
        assert_eq!(facade_resolved_tool("act", &args), Some("task_browser"));
        assert!(facade_resolved_tool("inspect", &args).is_none());
    }
    #[tokio::test]
    async fn authenticated_raw_and_facade_status_share_only_their_task_record() {
        use crate::mcp::{ToolCaller, ToolCallerTrust};
        let root = tempfile::tempdir().unwrap();
        let id = format!("task-status-{}", uuid::Uuid::new_v4());
        let log = Arc::new(StdMutex::new(
            crate::session_log::SessionLog::open(root.path().join("session")).unwrap(),
        ));
        crate::web_gateway::register_supervised_mcp_session(&id, &log);
        let token = crate::web_gateway::session_scoped_mcp_token(
            crate::web_gateway::loopback_mcp_auth_token(),
            &id,
        );
        let header = format!("POST /mcp?session_id={id} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\r\n");
        let context =
            crate::web_gateway::session_only_mcp_access_context(root.path(), &header).unwrap();
        let caller = ToolCaller::from_gate(&context.principal, Some(id.clone()));
        assert_eq!(caller.trust, ToolCallerTrust::Scoped);
        let key = SessionKey {
            principal: context.principal.id.clone(),
            session: id.clone(),
            epoch: caller.session_credential_epoch.clone().unwrap(),
            incarnation: 7,
        };
        let probe = Arc::new(Probe {
            current: StdMutex::new(key.clone()),
            alive: AtomicBool::new(true),
            input: AtomicBool::new(true),
        });
        let state = crate::mcp::tests::test_state_with_log_dir(root.path().join("logs"));
        let bus = EventBus::new();
        let entry = Arc::new(Allocation::new(
            key,
            caller.actor.clone(),
            probe.clone(),
            state.read().await.autonomy.clone(),
        ));
        let (sample, _) = fixture();
        *entry.resources.lock().unwrap() = sample.resources.lock().unwrap().clone();
        assert!(bus.macos_monitors.task_browsers.probe.set(probe).is_ok());
        bus.macos_monitors
            .task_browsers
            .entries
            .lock()
            .await
            .insert(id.clone(), entry);
        let (_home, server) = crate::mcp::tests::test_server(state, bus.clone());
        for (tool, args) in [
            ("task_browser", json!({"op":"status"})),
            ("inspect", json!({"argv":["browser","task-status"]})),
        ] {
            let result = server
                .call_tool_by_name_as_caller(tool, args, Some(&id), None, caller.clone())
                .await
                .unwrap();
            assert_ne!(result.is_error, Some(true));
            let value = serde_json::to_value(&result).unwrap();
            let text = value["content"][0]["text"].as_str().unwrap();
            let status: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(status["workspace_id"], "bw-task");
            for forbidden in [
                "cdp_",
                "profile_dir",
                "process_id",
                "macos_window:",
                token.as_str(),
            ] {
                assert!(!text.contains(forbidden));
            }
            assert!(bus.macos_monitors.not_started());
        }
        // A replacement registration may not attach the old bearer to the new task.
        let replacement = Arc::new(StdMutex::new(
            crate::session_log::SessionLog::open(root.path().join("replacement")).unwrap(),
        ));
        crate::web_gateway::register_supervised_mcp_session(&id, &replacement);
        let result = server
            .call_tool_by_name_as_caller(
                "task_browser",
                json!({"op":"status"}),
                Some(&id),
                None,
                caller,
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(bus.macos_monitors.not_started());
    }

    #[tokio::test]
    async fn conflicting_gateway_policy_roots_refuse_instead_of_using_first_policy() {
        let registry = Registry::default();
        let root = tempfile::tempdir().unwrap();
        registry.configure(root.path().join("one"));
        registry.configure(root.path().join("two"));
        let actor = ActorBinding::agent_session(Some("principal".into()), "session".into());
        assert!(registry
            .identity(&actor, Some("epoch"), false)
            .await
            .is_err());
    }
    #[test]
    fn production_profile_layout_is_pinned_before_fresh_creation_acknowledgement() {
        let root = tempfile::tempdir().unwrap();
        let (entry, _) = fixture();
        let monitor = entry.resources.lock().unwrap().monitor.clone();
        *entry.resources.lock().unwrap() = Resources {
            monitor,
            ..Resources::default()
        };
        let mut workspace = super::super::tests::sample_workspace("bw-profile-layout");
        let path = root.path().join(&workspace.id).join("profile");
        workspace.profile_dir = Some(path.display().to_string());
        workspace.display_target = Some(selector(&entry));
        let permit = entry.authority(Mode::Provision).task.unwrap();
        permit.record_workspace(&workspace).unwrap();
        assert!(entry.resources.lock().unwrap().profile.is_none());
        assert!(permit
            .record_profile(&root.path().join(&workspace.id))
            .is_err());
        assert!(permit.record_profile(&path).is_ok());
        assert_eq!(
            entry.resources.lock().unwrap().profile.as_ref(),
            Some(&path)
        );
        assert!(permit.record_profile(&path).is_err());
    }

    #[test]
    fn delayed_request_cannot_retarget_reopened_workspace() {
        let (entry, _) = fixture();
        assert!(entry.check_workspace_id("bw-task").is_ok());
        assert!(entry.check_workspace_id("bw-foreign").is_err());
        entry.resources.lock().unwrap().workspace = Some("bw-reopened".into());
        assert!(entry.check_workspace_id("bw-task").is_err());
        assert!(entry.check_workspace_id("bw-reopened").is_ok());
    }
}

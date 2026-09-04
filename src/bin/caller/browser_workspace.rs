use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::process::Child;
use tokio::sync::RwLock;

use crate::event::{AppEvent, EventBus};

pub type SharedBrowserWorkspaceRegistry = Arc<RwLock<BrowserWorkspaceRegistry>>;

static GLOBAL_BROWSER_WORKSPACES: OnceLock<SharedBrowserWorkspaceRegistry> = OnceLock::new();

const CDP_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const CDP_LIVENESS_TIMEOUT: Duration = Duration::from_secs(2);
const BROWSER_EXECUTABLE_ENV: &str = "INTENDANT_BROWSER_WORKSPACE_EXECUTABLE";
const LEGACY_BROWSER_EXECUTABLE_ENV: &str = "INTENDANT_BROWSER_EXECUTABLE";
// macOS-only system-browser escape hatch; other platforms' discovery path never consults it.
#[cfg(target_os = "macos")]
const ALLOW_SYSTEM_BROWSER_ENV: &str = "INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER";
#[cfg(target_os = "macos")]
const LEGACY_ALLOW_SYSTEM_BROWSER_ENV: &str = "INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_CHROME";
const CHROME_FOR_TESTING_DOWNLOADS_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";
const BROWSER_EXTENSION_ARCHIVE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const BROWSER_EXTENSION_MAX_FILES: usize = 4_096;
const BROWSER_EXTENSION_MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
const BROWSER_EXTENSION_MAX_ENTRY_BYTES: u64 = 96 * 1024 * 1024;
const BROWSER_EXTENSION_MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
const APPROVED_BROWSER_EXTENSION_SERVICE_WORKER: &str = "sw.js";
// Rabby Wallet v0.94.6, published by RabbyHub at
// https://github.com/RabbyHub/Rabby/releases/tag/v0.94.6. Loading an
// arbitrary caller-selected extension would turn an ordinary browser-workspace
// action into an unbounded code-execution/network capability, so this door is
// deliberately a compiled one-artifact allowlist.
const APPROVED_BROWSER_EXTENSION_SHA256: &str =
    "daf7819d7371a67ef447c788e899b1df628f95e380a460c6e5dd3b86bbe09e4f";
const APPROVED_BROWSER_EXTENSION_BYTE_LENGTH: u64 = 16_216_742;
const APPROVED_BROWSER_EXTENSION_MANIFEST_VERSION: u32 = 3;
const APPROVED_BROWSER_EXTENSION_VERSION: &str = "0.94.6";

pub fn global_registry() -> SharedBrowserWorkspaceRegistry {
    GLOBAL_BROWSER_WORKSPACES
        .get_or_init(|| Arc::new(RwLock::new(BrowserWorkspaceRegistry::default())))
        .clone()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserWorkspaceProvider {
    Auto,
    Cdp,
    SystemCdp,
    Playwright,
    AgentBrowser,
    Stream,
}

impl BrowserWorkspaceProvider {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("auto")
            .to_ascii_lowercase()
            .as_str()
        {
            "cdp" | "chrome" | "chromium" => Self::Cdp,
            "system_cdp" | "system-cdp" | "system_chrome" | "system-chrome" => Self::SystemCdp,
            "playwright" => Self::Playwright,
            "agent_browser" | "agent-browser" | "agentbrowser" => Self::AgentBrowser,
            "stream" | "streamed" | "remote_stream" | "remote-stream" => Self::Stream,
            _ => Self::Auto,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cdp => "cdp",
            Self::SystemCdp => "system_cdp",
            Self::Playwright => "playwright",
            Self::AgentBrowser => "agent_browser",
            Self::Stream => "stream",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserWorkspaceStatus {
    Starting,
    Ready,
    Closed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserWorkspacePreviewMode {
    Semantic,
    Screenshot,
    Stream,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserWorkspacePlacement {
    /// "local" or "peer". Kept stringly on the wire so older clients can
    /// forward unknown future placement kinds.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
}

impl BrowserWorkspacePlacement {
    pub fn local() -> Self {
        Self {
            kind: "local".to_string(),
            peer_id: None,
        }
    }

    pub fn peer(peer_id: String) -> Self {
        Self {
            kind: "peer".to_string(),
            peer_id: Some(peer_id),
        }
    }

    pub fn is_local(&self) -> bool {
        self.kind.eq_ignore_ascii_case("local") && self.peer_id.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserWorkspaceLease {
    pub holder_id: String,
    pub holder_kind: String,
    pub acquired_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserWorkspace {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub provider: BrowserWorkspaceProvider,
    pub requested_provider: BrowserWorkspaceProvider,
    pub placement: BrowserWorkspacePlacement,
    pub status: BrowserWorkspaceStatus,
    pub preview_mode: BrowserWorkspacePreviewMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    /// Canonical Intendant virtual-display target (for example,
    /// `display_99`) when this workspace is explicitly display-bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<BrowserWorkspaceExtension>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_executable_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debugging_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_http_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_ws_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<BrowserWorkspaceLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserWorkspaceExtension {
    pub archive_sha256: String,
    pub archive_byte_length: u64,
    pub manifest_version: u32,
    pub version: String,
    pub load_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserProviderStatus {
    pub provider: BrowserWorkspaceProvider,
    pub available: bool,
    pub executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedBrowserStatus {
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub install_root: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ManagedBrowserInstallOptions {
    pub channel: String,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedBrowserInstallResult {
    pub installed: bool,
    pub channel: String,
    pub version: String,
    pub platform: String,
    pub executable: String,
    pub source: String,
    pub install_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloaded_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBrowserWorkspaceRequest {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub peer_id: Option<String>,
    #[serde(default)]
    pub owner_session_id: Option<String>,
    /// Explicit daemon-created virtual display (`display_99`, `:99`, or
    /// `99`). User-session, session-local, and foreign X servers are rejected.
    #[serde(default)]
    pub display_target: Option<String>,
    #[serde(default)]
    pub profile_dir: Option<String>,
    /// Optional immutable extension archive. These five fields are an
    /// all-or-none tuple and are accepted only by the managed CDP provider.
    #[serde(default)]
    pub extension_archive_path: Option<String>,
    #[serde(default)]
    pub extension_archive_sha256: Option<String>,
    #[serde(default)]
    pub extension_archive_byte_length: Option<u64>,
    #[serde(default)]
    pub extension_manifest_version: Option<u32>,
    #[serde(default)]
    pub extension_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserExtensionArchiveSpec {
    archive_path: PathBuf,
    archive_sha256: String,
    archive_byte_length: u64,
    manifest_version: u32,
    version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireBrowserWorkspaceRequest {
    pub workspace_id: String,
    pub holder_id: String,
    #[serde(default)]
    pub holder_kind: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseBrowserWorkspaceRequest {
    pub workspace_id: String,
    #[serde(default)]
    pub holder_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug)]
pub enum BrowserWorkspaceError {
    NotFound(String),
    LeaseHeld {
        workspace_id: String,
        holder_id: String,
    },
    Unsupported(String),
    Io(String),
    Launch(String),
}

impl fmt::Display for BrowserWorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "browser workspace '{id}' not found"),
            Self::LeaseHeld {
                workspace_id,
                holder_id,
            } => write!(
                f,
                "browser workspace '{workspace_id}' is already leased by '{holder_id}'"
            ),
            Self::Unsupported(msg) | Self::Io(msg) | Self::Launch(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for BrowserWorkspaceError {}

#[derive(Default)]
pub struct BrowserWorkspaceRegistry {
    workspaces: BTreeMap<String, BrowserWorkspace>,
    children: HashMap<String, Child>,
}

struct RetiredBrowserWorkspace {
    workspace: BrowserWorkspace,
    process_id: Option<u32>,
    child: Option<Child>,
}

struct StartingReservationGuard {
    workspace_id: String,
    bus: EventBus,
    armed: bool,
}

#[derive(Debug)]
struct ExtensionFilesystemGuard {
    profile_dir: PathBuf,
    extension_root: PathBuf,
    armed: bool,
}

impl ExtensionFilesystemGuard {
    fn new(profile_dir: PathBuf, extension_root: PathBuf) -> Self {
        Self {
            profile_dir,
            extension_root,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ExtensionFilesystemGuard {
    fn drop(&mut self) {
        if self.armed {
            cleanup_extension_workspace_paths(&self.profile_dir, &self.extension_root);
        }
    }
}

impl StartingReservationGuard {
    fn new(workspace_id: String, bus: EventBus) -> Self {
        Self {
            workspace_id,
            bus,
            armed: true,
        }
    }

    async fn cleanup(&mut self, message: &str) {
        remove_failed_reservation(&self.workspace_id, message, &self.bus).await;
        self.armed = false;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartingReservationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let workspace_id = self.workspace_id.clone();
        let bus = self.bus.clone();
        runtime.spawn(async move {
            remove_failed_reservation(
                &workspace_id,
                "browser workspace creation was cancelled before it became ready",
                &bus,
            )
            .await;
        });
    }
}

impl BrowserWorkspaceRegistry {
    pub fn list(&self) -> Vec<BrowserWorkspace> {
        self.workspaces.values().cloned().collect()
    }

    fn insert(&mut self, workspace: BrowserWorkspace, child: Option<Child>) {
        if let Some(child) = child {
            self.children.insert(workspace.id.clone(), child);
        }
        self.workspaces.insert(workspace.id.clone(), workspace);
    }

    fn remove(&mut self, id: &str) -> Option<(BrowserWorkspace, Option<Child>)> {
        let workspace = self.workspaces.remove(id)?;
        let child = self.children.remove(id);
        Some((workspace, child))
    }

    fn reconcile_display_bindings(&mut self) -> Vec<RetiredBrowserWorkspace> {
        self.reconcile_display_bindings_with(
            crate::virtual_display::process_owns_browser_bindable_display,
        )
    }

    fn reconcile_display_bindings_with(
        &mut self,
        display_is_live: impl Fn(u32) -> bool,
    ) -> Vec<RetiredBrowserWorkspace> {
        let stale: Vec<(String, String)> = self
            .workspaces
            .values()
            .filter(|workspace| {
                matches!(
                    workspace.status,
                    BrowserWorkspaceStatus::Starting | BrowserWorkspaceStatus::Ready
                )
            })
            .filter_map(|workspace| {
                let target = workspace.display_target.as_deref()?;
                let binding = parse_browser_display_binding(target).ok()?;
                (!display_is_live(binding.display_id))
                    .then(|| (workspace.id.clone(), binding.canonical))
            })
            .collect();

        self.retire_display_bindings(stale, |display_target| {
            format!("bound virtual display {display_target} is no longer live; browser stopped")
        })
    }

    fn retire_workspaces_for_display(
        &mut self,
        display_id: u32,
        reason: &str,
    ) -> Vec<RetiredBrowserWorkspace> {
        let stale: Vec<(String, String)> = self
            .workspaces
            .values()
            .filter(|workspace| {
                matches!(
                    workspace.status,
                    BrowserWorkspaceStatus::Starting | BrowserWorkspaceStatus::Ready
                )
            })
            .filter_map(|workspace| {
                let binding =
                    parse_browser_display_binding(workspace.display_target.as_deref()?).ok()?;
                (binding.display_id == display_id)
                    .then(|| (workspace.id.clone(), binding.canonical))
            })
            .collect();

        self.retire_display_bindings(stale, |display_target| {
            format!("bound virtual display {display_target} was retired: {reason}")
        })
    }

    fn retire_display_bindings(
        &mut self,
        stale: Vec<(String, String)>,
        message: impl Fn(&str) -> String,
    ) -> Vec<RetiredBrowserWorkspace> {
        stale
            .into_iter()
            .filter_map(|(workspace_id, display_target)| {
                let workspace = self.workspaces.get_mut(&workspace_id)?;
                workspace.status = BrowserWorkspaceStatus::Error;
                workspace.lease = None;
                workspace.message = Some(message(&display_target));
                workspace.updated_at = now_string();
                workspace.debugging_port = None;
                workspace.cdp_http_url = None;
                workspace.cdp_ws_url = None;
                workspace.active_target_id = None;
                let process_id = workspace.process_id.take();
                let child = self.children.remove(&workspace_id);
                Some(RetiredBrowserWorkspace {
                    workspace: workspace.clone(),
                    process_id,
                    child,
                })
            })
            .collect()
    }

    fn acquire(
        &mut self,
        request: AcquireBrowserWorkspaceRequest,
    ) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
        let workspace = self
            .workspaces
            .get_mut(&request.workspace_id)
            .ok_or_else(|| BrowserWorkspaceError::NotFound(request.workspace_id.clone()))?;
        if workspace.status != BrowserWorkspaceStatus::Ready {
            return Err(BrowserWorkspaceError::Unsupported(format!(
                "browser workspace '{}' is not ready (status: {:?})",
                request.workspace_id, workspace.status
            )));
        }
        if let Some(lease) = workspace.lease.as_ref() {
            if lease.holder_id != request.holder_id && !request.force {
                return Err(BrowserWorkspaceError::LeaseHeld {
                    workspace_id: request.workspace_id,
                    holder_id: lease.holder_id.clone(),
                });
            }
        }
        workspace.lease = Some(BrowserWorkspaceLease {
            holder_id: request.holder_id,
            holder_kind: request
                .holder_kind
                .unwrap_or_else(|| "agent".to_string())
                .trim()
                .to_string(),
            acquired_at: now_string(),
            note: request.note,
        });
        workspace.updated_at = now_string();
        Ok(workspace.clone())
    }

    fn release(
        &mut self,
        request: ReleaseBrowserWorkspaceRequest,
    ) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
        let workspace = self
            .workspaces
            .get_mut(&request.workspace_id)
            .ok_or_else(|| BrowserWorkspaceError::NotFound(request.workspace_id.clone()))?;
        if let (Some(expected), Some(lease)) =
            (request.holder_id.as_deref(), workspace.lease.as_ref())
        {
            if !expected.trim().is_empty() && lease.holder_id != expected {
                return Err(BrowserWorkspaceError::LeaseHeld {
                    workspace_id: request.workspace_id,
                    holder_id: lease.holder_id.clone(),
                });
            }
        }
        workspace.lease = None;
        if let Some(note) = request.note.filter(|s| !s.trim().is_empty()) {
            workspace.message = Some(note);
        }
        workspace.updated_at = now_string();
        Ok(workspace.clone())
    }
}

pub async fn provider_statuses() -> Vec<BrowserProviderStatus> {
    let cdp_exe = resolve_chromium_executable(false, false);
    let system_cdp_exe = resolve_chromium_executable(true, false);
    let playwright_exe = find_executable("playwright").or_else(|| find_executable("npx"));
    let agent_browser_exe = find_executable("agent-browser");
    vec![
        match cdp_exe {
            Ok(exe) => BrowserProviderStatus {
                provider: BrowserWorkspaceProvider::Cdp,
                available: true,
                executable: Some(exe.path.display().to_string()),
                source: Some(exe.source),
                message:
                    "Local managed Chromium-family browser through the Chrome DevTools Protocol."
                        .to_string(),
            },
            Err(err) => BrowserProviderStatus {
                provider: BrowserWorkspaceProvider::Cdp,
                available: false,
                executable: None,
                source: None,
                message: err.to_string(),
            },
        },
        match system_cdp_exe {
            Ok(exe) => BrowserProviderStatus {
                provider: BrowserWorkspaceProvider::SystemCdp,
                available: true,
                executable: Some(exe.path.display().to_string()),
                source: Some(exe.source),
                message:
                    "Explicit opt-in CDP provider for the user's installed Chrome/Chromium browser."
                        .to_string(),
            },
            Err(err) => BrowserProviderStatus {
                provider: BrowserWorkspaceProvider::SystemCdp,
                available: false,
                executable: None,
                source: None,
                message: err.to_string(),
            },
        },
        BrowserProviderStatus {
            provider: BrowserWorkspaceProvider::Playwright,
            available: playwright_exe.is_some(),
            source: playwright_exe.as_ref().map(|_| "PATH".to_string()),
            executable: playwright_exe.map(|p| p.display().to_string()),
            message: "Provider contract reserved for the Playwright sidecar.".to_string(),
        },
        BrowserProviderStatus {
            provider: BrowserWorkspaceProvider::AgentBrowser,
            available: agent_browser_exe.is_some(),
            source: agent_browser_exe.as_ref().map(|_| "PATH".to_string()),
            executable: agent_browser_exe.map(|p| p.display().to_string()),
            message: "Provider contract reserved for Vercel Agent Browser integration.".to_string(),
        },
        BrowserProviderStatus {
            provider: BrowserWorkspaceProvider::Stream,
            available: true,
            executable: None,
            source: None,
            message:
                "Fallback to Intendant display streaming for remote or non-browser workspaces."
                    .to_string(),
        },
    ]
}

pub fn managed_chromium_status() -> ManagedBrowserStatus {
    match find_managed_chromium_executable() {
        Some(path) => ManagedBrowserStatus {
            installed: true,
            executable: Some(path.display().to_string()),
            source: Some("managed-cache".to_string()),
            install_root: managed_browser_install_root().display().to_string(),
            message: "managed Chromium-family browser is available".to_string(),
        },
        None => ManagedBrowserStatus {
            installed: false,
            executable: None,
            source: None,
            install_root: managed_browser_install_root().display().to_string(),
            message: format!(
                "no managed Chromium executable found; run `intendant setup browsers` or set {BROWSER_EXECUTABLE_ENV}"
            ),
        },
    }
}

pub async fn ensure_managed_chromium(
    options: ManagedBrowserInstallOptions,
) -> Result<ManagedBrowserInstallResult, String> {
    if !options.force {
        if let Some(path) = find_managed_chromium_executable() {
            return Ok(ManagedBrowserInstallResult {
                installed: false,
                channel: normalize_cft_channel(&options.channel)?.to_string(),
                version: "existing".to_string(),
                platform: cft_platform()?.to_string(),
                executable: path.display().to_string(),
                source: "managed-cache".to_string(),
                install_dir: path
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .display()
                    .to_string(),
                download_url: None,
                downloaded_bytes: None,
            });
        }
    }

    let channel = normalize_cft_channel(&options.channel)?;
    let platform = cft_platform()?;
    let manifest = fetch_cft_manifest().await?;
    let channel_info = manifest
        .channels
        .get(channel)
        .ok_or_else(|| format!("Chrome for Testing manifest has no {channel} channel"))?;
    let download = channel_info
        .downloads
        .chrome
        .iter()
        .find(|entry| entry.platform == platform)
        .ok_or_else(|| {
            format!(
                "Chrome for Testing channel {channel} has no chrome download for platform {platform}"
            )
        })?;

    let install_root = managed_browser_install_root();
    let install_dir = install_root
        .join("chrome-for-testing")
        .join(channel.to_ascii_lowercase())
        .join(platform)
        .join(&channel_info.version);
    if install_dir.exists() && !options.force {
        if let Some(path) =
            find_executable_under(&install_dir, managed_browser_executable_names(), 8)
        {
            return Ok(ManagedBrowserInstallResult {
                installed: false,
                channel: channel.to_string(),
                version: channel_info.version.clone(),
                platform: platform.to_string(),
                executable: path.display().to_string(),
                source: "managed-cache".to_string(),
                install_dir: install_dir.display().to_string(),
                download_url: None,
                downloaded_bytes: None,
            });
        }
    }

    let downloaded_bytes =
        download_and_extract_cft(&download.url, &install_root, &install_dir, options.force).await?;
    let executable = find_executable_under(&install_dir, managed_browser_executable_names(), 8)
        .ok_or_else(|| {
            format!(
                "Chrome for Testing extracted to {}, but no browser executable was found",
                install_dir.display()
            )
        })?;

    Ok(ManagedBrowserInstallResult {
        installed: true,
        channel: channel.to_string(),
        version: channel_info.version.clone(),
        platform: platform.to_string(),
        executable: executable.display().to_string(),
        source: "chrome-for-testing".to_string(),
        install_dir: install_dir.display().to_string(),
        download_url: Some(download.url.clone()),
        downloaded_bytes: Some(downloaded_bytes),
    })
}

pub async fn create_workspace(
    request: CreateBrowserWorkspaceRequest,
    bus: &EventBus,
) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
    let requested_provider = BrowserWorkspaceProvider::parse(request.provider.as_deref());
    let placement = match request
        .peer_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(peer_id) => BrowserWorkspacePlacement::peer(peer_id.to_string()),
        None => BrowserWorkspacePlacement::local(),
    };
    if !placement.is_local() {
        return Err(BrowserWorkspaceError::Unsupported(
            "remote peer browser workspace placement is modeled but not wired to the federation transport yet"
                .to_string(),
        ));
    }

    let provider = match requested_provider {
        BrowserWorkspaceProvider::Auto => BrowserWorkspaceProvider::Cdp,
        BrowserWorkspaceProvider::Cdp => BrowserWorkspaceProvider::Cdp,
        BrowserWorkspaceProvider::SystemCdp => BrowserWorkspaceProvider::SystemCdp,
        BrowserWorkspaceProvider::Playwright => {
            return Err(BrowserWorkspaceError::Unsupported(
                "Playwright browser workspaces need the sidecar driver; use provider=cdp for the first executable backend"
                    .to_string(),
            ));
        }
        BrowserWorkspaceProvider::AgentBrowser => {
            return Err(BrowserWorkspaceError::Unsupported(
                "Agent Browser workspaces need the Agent Browser provider adapter; use provider=cdp for the first executable backend"
                    .to_string(),
            ));
        }
        BrowserWorkspaceProvider::Stream => {
            return Err(BrowserWorkspaceError::Unsupported(
                "stream workspaces are represented by the existing display/shared-view path; create a display stream instead"
                    .to_string(),
            ));
        }
    };
    let extension_spec = parse_extension_archive_spec(&request)?;
    if extension_spec.is_some() && provider != BrowserWorkspaceProvider::Cdp {
        return Err(BrowserWorkspaceError::Unsupported(
            "browser extension archives are accepted only by provider=cdp; system_cdp and sidecar providers are refused"
                .to_string(),
        ));
    }

    let id = format!("bw-{}", uuid::Uuid::new_v4().simple());
    let created_at = now_string();
    let display_binding = request
        .display_target
        .as_deref()
        .map(parse_browser_display_binding)
        .transpose()?;
    let bound_display_id = display_binding.as_ref().map(|binding| binding.display_id);
    let bound_display_target = display_binding
        .as_ref()
        .map(|binding| binding.canonical.clone());
    let _display_access = match bound_display_id {
        Some(display_id) => {
            Some(crate::computer_use::acquire_virtual_display_shared(display_id).await)
        }
        None => None,
    };
    let profile_dir = request
        .profile_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| default_profile_dir(&id));
    if extension_spec.is_some()
        && (request
            .profile_dir
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
            || !profile_dir.is_absolute())
    {
        return Err(BrowserWorkspaceError::Unsupported(
            "browser extension launch requires an explicit absolute fresh profile_dir".to_string(),
        ));
    }
    let extension_root = extension_spec
        .as_ref()
        .map(|_| browser_extension_workspace_root(&id));
    let mut workspace = BrowserWorkspace {
        label: request
            .label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Browser workspace")
            .to_string(),
        url: request
            .url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        provider,
        requested_provider,
        placement,
        status: BrowserWorkspaceStatus::Starting,
        preview_mode: BrowserWorkspacePreviewMode::Semantic,
        owner_session_id: request
            .owner_session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        display_target: display_binding.map(|binding| binding.canonical),
        profile_dir: Some(profile_dir.display().to_string()),
        extension: None,
        browser_executable: None,
        browser_executable_source: None,
        process_id: None,
        debugging_port: None,
        cdp_http_url: None,
        cdp_ws_url: None,
        active_target_id: None,
        lease: None,
        message: Some("starting local CDP browser".to_string()),
        created_at: created_at.clone(),
        updated_at: created_at,
        id,
    };

    // Publish the Starting reservation before filesystem work or browser
    // launch. Display teardown can now retire this exact binding instead of
    // racing past a workspace that exists only on this task's stack.
    {
        let registry = global_registry();
        let mut registry = registry.write().await;
        if let Some(display_id) = bound_display_id {
            if !crate::virtual_display::process_owns_browser_bindable_display(display_id) {
                return Err(BrowserWorkspaceError::Unsupported(format!(
                    "browser workspace display {} is not a live daemon-created virtual display",
                    bound_display_target.as_deref().unwrap_or("unknown")
                )));
            }
        }
        registry.insert(workspace.clone(), None);
    }
    // Async cancellation can happen at every await below. The guard removes
    // the unpublished Starting row (and any child committed just before the
    // cancellation) so request abortion cannot leave a ghost workspace.
    let mut reservation = StartingReservationGuard::new(workspace.id.clone(), bus.clone());
    let mut extension_filesystem = match extension_root.as_ref() {
        Some(extension_root) => {
            match create_extension_workspace_filesystem(&profile_dir, extension_root) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    reservation.cleanup(&error.to_string()).await;
                    return Err(error);
                }
            }
        }
        None => {
            if let Err(error) = fs::create_dir_all(&profile_dir) {
                let message = format!(
                    "failed to create browser workspace profile {}: {error}",
                    profile_dir.display()
                );
                reservation.cleanup(&message).await;
                return Err(BrowserWorkspaceError::Io(message));
            }
            None
        }
    };

    if let Some(spec) = extension_spec.as_ref() {
        let extension_root = extension_root.as_ref().expect("presence checked");
        match prepare_browser_extension(spec, extension_root) {
            Ok(extension) => workspace.extension = Some(extension),
            Err(error) => {
                let message = error.to_string();
                reservation.cleanup(&message).await;
                return Err(error);
            }
        }
    }

    let (child, cdp) = match launch_cdp_browser(&workspace, &profile_dir).await {
        Ok(launched) => launched,
        Err(error) => {
            reservation.cleanup(&error.to_string()).await;
            return Err(error);
        }
    };
    if let (Some(extension), Some(runtime_id)) = (
        workspace.extension.as_mut(),
        cdp.extension_runtime_id.clone(),
    ) {
        extension.runtime_id = Some(runtime_id);
    }
    workspace.browser_executable = Some(cdp.executable.path.display().to_string());
    workspace.browser_executable_source = Some(cdp.executable.source);
    workspace.process_id = cdp.process_id;
    workspace.debugging_port = Some(cdp.port);
    workspace.cdp_http_url = Some(format!("http://127.0.0.1:{}", cdp.port));
    workspace.cdp_ws_url = cdp.web_socket_debugger_url;
    workspace.active_target_id = cdp.target_id;
    workspace.status = BrowserWorkspaceStatus::Ready;
    workspace.message = Some("ready".to_string());
    workspace.updated_at = now_string();

    let mut child = Some(child);
    let commit = {
        let registry = global_registry();
        let mut registry = registry.write().await;
        let display_is_live = bound_display_id.is_none_or(|display_id| {
            crate::virtual_display::process_owns_browser_bindable_display(display_id)
        });
        let current = registry.workspaces.get_mut(&workspace.id);
        match current {
            None => Err("browser workspace was closed during launch".to_string()),
            Some(current) if current.status != BrowserWorkspaceStatus::Starting => Err(current
                .message
                .clone()
                .unwrap_or_else(|| "browser workspace was retired during launch".to_string())),
            Some(current) if !display_is_live => {
                let message = "bound virtual display was retired during browser launch".to_string();
                current.status = BrowserWorkspaceStatus::Error;
                current.lease = None;
                current.message = Some(message.clone());
                current.updated_at = now_string();
                Err(message)
            }
            Some(current) => {
                if let Some(launched_child) = child.take() {
                    *current = workspace.clone();
                    let committed = current.clone();
                    registry
                        .children
                        .insert(workspace.id.clone(), launched_child);
                    // Publish while the registry write guard still serializes
                    // this Ready transition with display retirement. If the
                    // display is reaped next, its Error event must follow this
                    // creation event; it can never be overtaken by a stale
                    // Ready clone returned to the caller.
                    bus.send(AppEvent::BrowserWorkspaceChanged {
                        kind: "created".to_string(),
                        workspace_id: Some(committed.id.clone()),
                        workspace: Some(committed.clone()),
                        message: None,
                    });
                    Ok(committed)
                } else {
                    Err("browser workspace launch child was unavailable".to_string())
                }
            }
        }
    };
    match commit {
        Ok(committed) => {
            if let Some(filesystem) = extension_filesystem.as_mut() {
                filesystem.disarm();
            }
            reservation.disarm();
            Ok(committed)
        }
        Err(message) => {
            terminate_workspace_process(cdp.process_id, child);
            reservation.cleanup(&message).await;
            Err(BrowserWorkspaceError::Launch(message))
        }
    }
}

pub async fn list_workspaces(bus: &EventBus) -> Vec<BrowserWorkspace> {
    let (workspaces, retired) = {
        let registry = global_registry();
        let mut registry = registry.write().await;
        let retired = registry.reconcile_display_bindings();
        publish_retirements_locked(bus, &retired);
        (registry.list(), retired)
    };
    terminate_retired_processes(retired);
    workspaces
}

/// Prove that the exact local workspace record still names a live child and
/// an extant page target. Stored `Ready` metadata alone is not liveness: a
/// browser may exit or its selected tab may close between bounded proof
/// actions. The registry/child check brackets the CDP probe so a concurrent
/// close or replacement cannot be mistaken for the requested workspace.
pub(crate) async fn verify_live_workspace(workspace: &BrowserWorkspace) -> Result<(), String> {
    verify_registered_child(workspace).await?;

    let pid = workspace
        .process_id
        .ok_or_else(|| "browser workspace has no process id".to_string())?;
    let port = workspace
        .debugging_port
        .ok_or_else(|| "browser workspace has no debugging port".to_string())?;
    let target_id = workspace
        .active_target_id
        .as_deref()
        .ok_or_else(|| "browser workspace has no active target id".to_string())?;
    let expected_http = format!("http://127.0.0.1:{port}");
    if workspace.cdp_http_url.as_deref() != Some(expected_http.as_str()) {
        return Err("browser workspace CDP URL is not its exact loopback port".to_string());
    }
    if !crate::platform::process_alive(pid) {
        return Err("browser workspace process is no longer live".to_string());
    }

    let list_url = format!("{expected_http}/json/list");
    let response = reqwest::Client::new()
        .get(&list_url)
        .timeout(CDP_LIVENESS_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("browser workspace CDP liveness probe failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("browser workspace CDP liveness probe failed: {error}"))?;
    let targets: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("browser workspace CDP target list was invalid: {error}"))?;
    let target = exact_page_target(&targets, target_id)
        .ok_or_else(|| "browser workspace active page target is no longer live".to_string())?;
    let target_ws = target
        .get("webSocketDebuggerUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "browser workspace active page target has no debugger URL".to_string())?;
    if workspace.cdp_ws_url.as_deref() != Some(target_ws) {
        return Err("browser workspace active page debugger URL changed".to_string());
    }

    verify_registered_child(workspace).await?;
    if !crate::platform::process_alive(pid) {
        return Err("browser workspace process exited during its CDP liveness probe".to_string());
    }
    Ok(())
}

async fn verify_registered_child(workspace: &BrowserWorkspace) -> Result<(), String> {
    let registry = global_registry();
    let mut registry = registry.write().await;
    let current = registry
        .workspaces
        .get(&workspace.id)
        .ok_or_else(|| "browser workspace disappeared from the live registry".to_string())?;
    if current != workspace {
        return Err("browser workspace binding changed during the bounded proof task".to_string());
    }
    let child = registry
        .children
        .get_mut(&workspace.id)
        .ok_or_else(|| "browser workspace has no daemon-owned child process".to_string())?;
    match child.try_wait() {
        Ok(None) => Ok(()),
        Ok(Some(status)) => Err(format!("browser workspace child exited with {status}")),
        Err(error) => Err(format!("cannot inspect browser workspace child: {error}")),
    }
}

fn exact_page_target<'a>(
    targets: &'a serde_json::Value,
    target_id: &str,
) -> Option<&'a serde_json::Value> {
    targets.as_array()?.iter().find(|target| {
        target.get("type").and_then(serde_json::Value::as_str) == Some("page")
            && target.get("id").and_then(serde_json::Value::as_str) == Some(target_id)
    })
}

pub async fn close_display_binding(
    display_id: u32,
    reason: &str,
    bus: &EventBus,
) -> Vec<BrowserWorkspace> {
    let retired = {
        let registry = global_registry();
        let mut registry = registry.write().await;
        // Creation reserves its Starting row under this same registry lock
        // after checking the bindable set. Removing the display here before
        // the scan therefore closes the post-scan insertion race.
        crate::virtual_display::unregister_browser_bindable_display(display_id);
        let retired = registry.retire_workspaces_for_display(display_id, reason);
        // Publish each retirement before releasing the registry lock. A
        // concurrent explicit close must therefore publish its newer Closed
        // state after this event, never before a stale Error clone.
        for retired in &retired {
            publish_workspace_event(bus, "display_retired", &retired.workspace);
        }
        retired
    };
    retired
        .into_iter()
        .map(|retired| {
            terminate_workspace_process(retired.process_id, retired.child);
            cleanup_extension_workspace(&retired.workspace);
            retired.workspace
        })
        .collect()
}

async fn remove_failed_reservation(id: &str, message: &str, bus: &EventBus) {
    let removed = {
        let registry = global_registry();
        let mut registry = registry.write().await;
        let removed = registry.remove(id);
        if let Some((workspace, _)) = removed.as_ref() {
            // A display teardown may already have published this reservation
            // as Error. Serialize a terminal event with removal so the
            // dashboard cannot retain that transient row as a ghost.
            let mut closed = workspace.clone();
            closed.status = BrowserWorkspaceStatus::Closed;
            closed.lease = None;
            closed.message = Some(message.to_string());
            closed.updated_at = now_string();
            bus.send(AppEvent::BrowserWorkspaceChanged {
                kind: "closed".to_string(),
                workspace_id: Some(closed.id.clone()),
                message: closed.message.clone(),
                workspace: Some(closed),
            });
        }
        removed
    };
    if let Some((workspace, child)) = removed {
        terminate_workspace_process(workspace.process_id, child);
        cleanup_extension_workspace(&workspace);
    }
}

pub async fn close_workspace(
    id: &str,
    reason: Option<String>,
) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
    let _display_access = acquire_workspace_display_access(id).await?;
    let (mut workspace, child) = global_registry()
        .write()
        .await
        .remove(id)
        .ok_or_else(|| BrowserWorkspaceError::NotFound(id.to_string()))?;
    workspace.status = BrowserWorkspaceStatus::Closed;
    workspace.lease = None;
    workspace.message = reason.or_else(|| Some("closed".to_string()));
    workspace.updated_at = now_string();
    terminate_workspace_process(workspace.process_id, child);
    cleanup_extension_workspace(&workspace);
    Ok(workspace)
}

pub async fn acquire_workspace(
    request: AcquireBrowserWorkspaceRequest,
    bus: &EventBus,
) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
    let _display_access = acquire_workspace_display_access(&request.workspace_id).await?;
    let (result, retired) = {
        let registry = global_registry();
        let mut registry = registry.write().await;
        let retired = registry.reconcile_display_bindings();
        publish_retirements_locked(bus, &retired);
        let result = registry.acquire(request);
        if let Ok(workspace) = result.as_ref() {
            publish_workspace_event(bus, "lease_acquired", workspace);
        }
        (result, retired)
    };
    terminate_retired_processes(retired);
    result
}

pub async fn release_workspace(
    request: ReleaseBrowserWorkspaceRequest,
    bus: &EventBus,
) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
    let _display_access = acquire_workspace_display_access(&request.workspace_id).await?;
    let (result, retired) = {
        let registry = global_registry();
        let mut registry = registry.write().await;
        let retired = registry.reconcile_display_bindings();
        publish_retirements_locked(bus, &retired);
        let result = registry.release(request);
        if let Ok(workspace) = result.as_ref() {
            publish_workspace_event(bus, "lease_released", workspace);
        }
        (result, retired)
    };
    terminate_retired_processes(retired);
    result
}

async fn acquire_workspace_display_access(
    workspace_id: &str,
) -> Result<Option<tokio::sync::OwnedRwLockReadGuard<()>>, BrowserWorkspaceError> {
    let display_target = global_registry()
        .read()
        .await
        .workspaces
        .get(workspace_id)
        .and_then(|workspace| workspace.display_target.clone());
    let Some(display_target) = display_target else {
        return Ok(None);
    };
    let binding = parse_browser_display_binding(&display_target)?;
    Ok(Some(
        crate::computer_use::acquire_virtual_display_shared(binding.display_id).await,
    ))
}

fn publish_retirements_locked(bus: &EventBus, retired: &[RetiredBrowserWorkspace]) {
    for retired in retired {
        publish_workspace_event(bus, "display_retired", &retired.workspace);
    }
}

fn terminate_retired_processes(retired: Vec<RetiredBrowserWorkspace>) {
    for retired in retired {
        terminate_workspace_process(retired.process_id, retired.child);
        cleanup_extension_workspace(&retired.workspace);
    }
}

fn publish_workspace_event(bus: &EventBus, kind: &str, workspace: &BrowserWorkspace) {
    bus.send(AppEvent::BrowserWorkspaceChanged {
        kind: kind.to_string(),
        workspace_id: Some(workspace.id.clone()),
        message: workspace.message.clone(),
        workspace: Some(workspace.clone()),
    });
}

fn terminate_workspace_process(process_id: Option<u32>, mut child: Option<Child>) {
    if let Some(pid) = process_id {
        let targets = crate::platform::terminate_process_tree_now(pid);
        let still_alive: Vec<u32> = targets
            .into_iter()
            .filter(|target| crate::platform::process_alive(*target))
            .collect();
        if !still_alive.is_empty() {
            eprintln!(
                "[browser-workspace] failed to terminate workspace process tree rooted at pid {}: still alive {:?}",
                pid, still_alive
            );
        }
    }
    if let Some(child) = child.as_mut() {
        let _ = child.start_kill();
    }
}

struct CdpLaunch {
    executable: ChromiumExecutable,
    process_id: Option<u32>,
    port: u16,
    web_socket_debugger_url: Option<String>,
    target_id: Option<String>,
    extension_runtime_id: Option<String>,
}

fn browser_extension_workspace_root(workspace_id: &str) -> PathBuf {
    crate::platform::intendant_home()
        .join("browser-extension-workspaces")
        .join(workspace_id)
}

fn create_extension_workspace_filesystem(
    profile_dir: &Path,
    extension_root: &Path,
) -> Result<ExtensionFilesystemGuard, BrowserWorkspaceError> {
    let profile_parent = profile_dir.parent().ok_or_else(|| {
        BrowserWorkspaceError::Unsupported(
            "browser extension profile_dir must have an absolute parent".to_string(),
        )
    })?;
    fs::create_dir_all(profile_parent).map_err(|error| {
        BrowserWorkspaceError::Io(format!(
            "failed to create browser profile parent {}: {error}",
            profile_parent.display()
        ))
    })?;
    fs::create_dir(profile_dir).map_err(|error| {
        let context = if error.kind() == std::io::ErrorKind::AlreadyExists {
            "browser extension launch requires a fresh profile_dir"
        } else {
            "failed to atomically reserve browser extension profile_dir"
        };
        BrowserWorkspaceError::Io(format!("{context} {}: {error}", profile_dir.display()))
    })?;

    let extension_parent = extension_root.parent().ok_or_else(|| {
        let _ = fs::remove_dir(profile_dir);
        BrowserWorkspaceError::Io(
            "daemon extension workspace path had no parent directory".to_string(),
        )
    })?;
    if let Err(error) = fs::create_dir_all(extension_parent) {
        let _ = fs::remove_dir(profile_dir);
        return Err(BrowserWorkspaceError::Io(format!(
            "failed to create daemon extension workspace parent {}: {error}",
            extension_parent.display()
        )));
    }
    if let Err(error) = fs::create_dir(extension_root) {
        let _ = fs::remove_dir(profile_dir);
        return Err(BrowserWorkspaceError::Io(format!(
            "failed to atomically reserve daemon extension workspace {}: {error}",
            extension_root.display()
        )));
    }

    let guard =
        ExtensionFilesystemGuard::new(profile_dir.to_path_buf(), extension_root.to_path_buf());
    set_private_directory(profile_dir)?;
    set_private_directory(extension_root)?;
    Ok(guard)
}

fn set_private_directory(path: &Path) -> Result<(), BrowserWorkspaceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            BrowserWorkspaceError::Io(format!(
                "failed to make browser workspace directory private {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

fn parse_extension_archive_spec(
    request: &CreateBrowserWorkspaceRequest,
) -> Result<Option<BrowserExtensionArchiveSpec>, BrowserWorkspaceError> {
    let presence = [
        request.extension_archive_path.is_some(),
        request.extension_archive_sha256.is_some(),
        request.extension_archive_byte_length.is_some(),
        request.extension_manifest_version.is_some(),
        request.extension_version.is_some(),
    ];
    if presence.iter().all(|present| !present) {
        return Ok(None);
    }
    if !presence.iter().all(|present| *present) {
        return Err(BrowserWorkspaceError::Unsupported(
            "extension_archive_path, extension_archive_sha256, extension_archive_byte_length, extension_manifest_version, and extension_version are an all-or-none tuple"
                .to_string(),
        ));
    }

    let archive_path = PathBuf::from(
        request
            .extension_archive_path
            .as_deref()
            .expect("presence checked"),
    );
    if !archive_path.is_absolute() {
        return Err(BrowserWorkspaceError::Unsupported(
            "extension_archive_path must be absolute".to_string(),
        ));
    }
    let archive_sha256 = request
        .extension_archive_sha256
        .as_deref()
        .expect("presence checked")
        .trim();
    if archive_sha256.len() != 64
        || !archive_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BrowserWorkspaceError::Unsupported(
            "extension_archive_sha256 must be 64 lowercase hexadecimal characters".to_string(),
        ));
    }
    let archive_byte_length = request
        .extension_archive_byte_length
        .expect("presence checked");
    if archive_byte_length == 0 || archive_byte_length > BROWSER_EXTENSION_ARCHIVE_MAX_BYTES {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "extension archive byte length must be between 1 and {BROWSER_EXTENSION_ARCHIVE_MAX_BYTES}"
        )));
    }
    let manifest_version = request
        .extension_manifest_version
        .expect("presence checked");
    if manifest_version == 0 {
        return Err(BrowserWorkspaceError::Unsupported(
            "extension_manifest_version must be positive".to_string(),
        ));
    }
    let version = request
        .extension_version
        .as_deref()
        .expect("presence checked")
        .trim();
    if version.is_empty() || version.len() > 128 || version.chars().any(char::is_control) {
        return Err(BrowserWorkspaceError::Unsupported(
            "extension_version must be a non-empty printable value of at most 128 characters"
                .to_string(),
        ));
    }
    if archive_sha256 != APPROVED_BROWSER_EXTENSION_SHA256
        || archive_byte_length != APPROVED_BROWSER_EXTENSION_BYTE_LENGTH
        || manifest_version != APPROVED_BROWSER_EXTENSION_MANIFEST_VERSION
        || version != APPROVED_BROWSER_EXTENSION_VERSION
    {
        return Err(BrowserWorkspaceError::Unsupported(
            "extension archive identity is outside Intendant's compiled browser-extension allowlist"
                .to_string(),
        ));
    }

    Ok(Some(BrowserExtensionArchiveSpec {
        archive_path,
        archive_sha256: archive_sha256.to_string(),
        archive_byte_length,
        manifest_version,
        version: version.to_string(),
    }))
}

fn prepare_browser_extension(
    spec: &BrowserExtensionArchiveSpec,
    extension_root: &Path,
) -> Result<BrowserWorkspaceExtension, BrowserWorkspaceError> {
    let source_metadata = fs::symlink_metadata(&spec.archive_path).map_err(|error| {
        BrowserWorkspaceError::Io(format!(
            "failed to inspect extension archive {}: {error}",
            spec.archive_path.display()
        ))
    })?;
    let source_is_link_like = intendant_platform::platform::path_leaf_is_symlink_or_reparse(
        &spec.archive_path,
    )
    .map_err(|error| {
        BrowserWorkspaceError::Io(format!(
            "failed to inspect extension archive leaf {}: {error}",
            spec.archive_path.display()
        ))
    })?;
    if source_is_link_like || !source_metadata.is_file() {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "extension archive must be a regular non-symlink, non-reparse file: {}",
            spec.archive_path.display()
        )));
    }

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let archive_file = options.open(&spec.archive_path).map_err(|error| {
        BrowserWorkspaceError::Io(format!(
            "failed to open extension archive {} without following links: {error}",
            spec.archive_path.display()
        ))
    })?;
    let opened_metadata = archive_file.metadata().map_err(|error| {
        BrowserWorkspaceError::Io(format!(
            "failed to inspect opened extension archive {}: {error}",
            spec.archive_path.display()
        ))
    })?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if opened_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(BrowserWorkspaceError::Unsupported(format!(
                "opened extension archive is a Windows reparse point: {}",
                spec.archive_path.display()
            )));
        }
    }
    if !opened_metadata.is_file() || opened_metadata.len() != spec.archive_byte_length {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "extension archive length mismatch: expected {}, opened {}",
            spec.archive_byte_length,
            opened_metadata.len()
        )));
    }

    let capacity = usize::try_from(spec.archive_byte_length).map_err(|_| {
        BrowserWorkspaceError::Unsupported(
            "extension archive length does not fit this platform".to_string(),
        )
    })?;
    let mut archive_bytes = Vec::with_capacity(capacity);
    let mut bounded_reader = archive_file.take(spec.archive_byte_length.saturating_add(1));
    bounded_reader
        .read_to_end(&mut archive_bytes)
        .map_err(|error| {
            BrowserWorkspaceError::Io(format!(
                "failed to snapshot extension archive {}: {error}",
                spec.archive_path.display()
            ))
        })?;
    let hashed_bytes = u64::try_from(archive_bytes.len()).map_err(|_| {
        BrowserWorkspaceError::Unsupported(
            "extension archive length does not fit the receipt".to_string(),
        )
    })?;
    if hashed_bytes != spec.archive_byte_length {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "extension archive changed while snapshotting: expected {} bytes, read {hashed_bytes}",
            spec.archive_byte_length
        )));
    }
    let actual_sha256 = Sha256::digest(&archive_bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual_sha256 != spec.archive_sha256 {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "extension archive sha256 mismatch: expected {}, opened {actual_sha256}",
            spec.archive_sha256
        )));
    }
    let load_path = extension_root.join("extension");
    fs::create_dir(&load_path).map_err(|error| {
        BrowserWorkspaceError::Io(format!(
            "failed to create isolated extension directory {}: {error}",
            load_path.display()
        ))
    })?;
    extract_browser_extension_archive(Cursor::new(archive_bytes), &load_path)?;
    let manifest_path = load_path.join("manifest.json");
    let manifest_metadata = fs::symlink_metadata(&manifest_path).map_err(|error| {
        BrowserWorkspaceError::Unsupported(format!(
            "extension archive has no root manifest.json: {error}"
        ))
    })?;
    if manifest_metadata.file_type().is_symlink()
        || !manifest_metadata.is_file()
        || manifest_metadata.len() > BROWSER_EXTENSION_MANIFEST_MAX_BYTES
    {
        return Err(BrowserWorkspaceError::Unsupported(
            "extension root manifest.json must be a bounded regular non-symlink file".to_string(),
        ));
    }
    let manifest_bytes = fs::read(&manifest_path).map_err(|error| {
        BrowserWorkspaceError::Io(format!(
            "failed to read extension manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        BrowserWorkspaceError::Unsupported(format!(
            "extension manifest.json is not valid JSON: {error}"
        ))
    })?;
    if manifest
        .get("manifest_version")
        .and_then(serde_json::Value::as_u64)
        != Some(u64::from(spec.manifest_version))
        || manifest.get("version").and_then(serde_json::Value::as_str)
            != Some(spec.version.as_str())
        || manifest
            .pointer("/background/service_worker")
            .and_then(serde_json::Value::as_str)
            != Some(APPROVED_BROWSER_EXTENSION_SERVICE_WORKER)
    {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "extension manifest identity mismatch: expected manifest_version {}, version {}, and service worker {}",
            spec.manifest_version, spec.version, APPROVED_BROWSER_EXTENSION_SERVICE_WORKER
        )));
    }
    let service_worker = load_path.join(APPROVED_BROWSER_EXTENSION_SERVICE_WORKER);
    let worker_metadata = fs::symlink_metadata(&service_worker).map_err(|error| {
        BrowserWorkspaceError::Unsupported(format!(
            "extension archive has no expected service worker {}: {error}",
            service_worker.display()
        ))
    })?;
    if worker_metadata.file_type().is_symlink() || !worker_metadata.is_file() {
        return Err(BrowserWorkspaceError::Unsupported(
            "extension service worker must be a regular non-symlink file".to_string(),
        ));
    }
    protect_extension_tree(extension_root)?;

    Ok(BrowserWorkspaceExtension {
        archive_sha256: actual_sha256,
        archive_byte_length: hashed_bytes,
        manifest_version: spec.manifest_version,
        version: spec.version.clone(),
        load_path: load_path.display().to_string(),
        runtime_id: None,
    })
}

fn extract_browser_extension_archive<R: Read + Seek>(
    archive_file: R,
    destination: &Path,
) -> Result<(), BrowserWorkspaceError> {
    let mut archive = zip::ZipArchive::new(archive_file).map_err(|error| {
        BrowserWorkspaceError::Unsupported(format!(
            "failed to parse extension zip archive: {error}"
        ))
    })?;
    if archive.is_empty() || archive.len() > BROWSER_EXTENSION_MAX_FILES {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "extension archive entry count must be between 1 and {BROWSER_EXTENSION_MAX_FILES}"
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut seen_folded = std::collections::BTreeSet::new();
    let mut total_unpacked = 0_u64;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            BrowserWorkspaceError::Unsupported(format!(
                "failed to read extension archive entry {index}: {error}"
            ))
        })?;
        let enclosed = entry.enclosed_name().ok_or_else(|| {
            BrowserWorkspaceError::Unsupported(format!(
                "extension archive entry {index} has an unsafe path"
            ))
        })?;
        let relative = enclosed.to_path_buf();
        let relative_text = relative.to_str().ok_or_else(|| {
            BrowserWorkspaceError::Unsupported(format!(
                "extension archive entry {index} path is not UTF-8"
            ))
        })?;
        if relative_text.is_empty()
            || relative_text.len() > 512
            || relative_text.contains('\\')
            || !seen.insert(relative_text.to_string())
            || !seen_folded.insert(relative_text.to_ascii_lowercase())
        {
            return Err(BrowserWorkspaceError::Unsupported(format!(
                "extension archive entry {index} has an empty, overlong, backslash, duplicate, or case-colliding path"
            )));
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170_000;
            let allowed_kind = kind == 0
                || (entry.is_dir() && kind == 0o040_000)
                || (!entry.is_dir() && kind == 0o100_000);
            if !allowed_kind {
                return Err(BrowserWorkspaceError::Unsupported(format!(
                    "extension archive entry {relative_text} is not a regular file or directory"
                )));
            }
        }
        if entry.size() > BROWSER_EXTENSION_MAX_ENTRY_BYTES {
            return Err(BrowserWorkspaceError::Unsupported(format!(
                "extension archive entry {relative_text} exceeds the per-entry unpacked limit"
            )));
        }
        total_unpacked = total_unpacked.checked_add(entry.size()).ok_or_else(|| {
            BrowserWorkspaceError::Unsupported(
                "extension archive unpacked byte count overflowed".to_string(),
            )
        })?;
        if total_unpacked > BROWSER_EXTENSION_MAX_UNPACKED_BYTES {
            return Err(BrowserWorkspaceError::Unsupported(format!(
                "extension archive exceeds the {BROWSER_EXTENSION_MAX_UNPACKED_BYTES}-byte unpacked limit"
            )));
        }

        let output_path = destination.join(&relative);
        if entry.is_dir() {
            fs::create_dir(&output_path)
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .map_err(|error| {
                    BrowserWorkspaceError::Io(format!(
                        "failed to create extension directory {}: {error}",
                        output_path.display()
                    ))
                })?;
            continue;
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                BrowserWorkspaceError::Io(format!(
                    "failed to create extension parent {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)
            .map_err(|error| {
                BrowserWorkspaceError::Io(format!(
                    "failed to create extension file {}: {error}",
                    output_path.display()
                ))
            })?;
        let copied = std::io::copy(
            &mut entry.by_ref().take(BROWSER_EXTENSION_MAX_ENTRY_BYTES + 1),
            &mut output,
        )
        .map_err(|error| {
            BrowserWorkspaceError::Io(format!(
                "failed to extract extension file {}: {error}",
                output_path.display()
            ))
        })?;
        output.flush().map_err(|error| {
            BrowserWorkspaceError::Io(format!(
                "failed to flush extension file {}: {error}",
                output_path.display()
            ))
        })?;
        if copied != entry.size() {
            return Err(BrowserWorkspaceError::Unsupported(format!(
                "extension archive entry {relative_text} declared {} bytes but yielded {copied}",
                entry.size()
            )));
        }
    }
    Ok(())
}

fn protect_extension_tree(path: &Path) -> Result<(), BrowserWorkspaceError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        BrowserWorkspaceError::Io(format!(
            "failed to inspect materialized extension path {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "materialized extension path must not be a symlink: {}",
            path.display()
        )));
    }
    if metadata.is_dir() {
        let mut entries = fs::read_dir(path)
            .map_err(|error| {
                BrowserWorkspaceError::Io(format!(
                    "failed to enumerate materialized extension directory {}: {error}",
                    path.display()
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                BrowserWorkspaceError::Io(format!(
                    "failed to enumerate materialized extension directory {}: {error}",
                    path.display()
                ))
            })?;
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            protect_extension_tree(&entry.path())?;
        }
    } else if !metadata.is_file() {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "materialized extension contains a non-file entry: {}",
            path.display()
        )));
    }

    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(if metadata.is_dir() { 0o500 } else { 0o400 });
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions).map_err(|error| {
        BrowserWorkspaceError::Io(format!(
            "failed to protect materialized extension path {}: {error}",
            path.display()
        ))
    })
}

fn make_extension_tree_writable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(if metadata.is_dir() { 0o700 } else { 0o600 });
    }
    #[cfg(not(unix))]
    permissions.set_readonly(false);
    let _ = fs::set_permissions(path, permissions);
    if metadata.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_extension_tree_writable(&entry.path());
            }
        }
    }
}

fn cleanup_extension_workspace_paths(profile_dir: &Path, extension_root: &Path) {
    make_extension_tree_writable(extension_root);
    for path in [extension_root, profile_dir] {
        if let Err(error) = fs::remove_dir_all(path) {
            if path.exists() {
                eprintln!(
                    "warning: failed to remove browser extension workspace path {}: {error}",
                    path.display()
                );
            }
        }
    }
}

fn cleanup_extension_workspace(workspace: &BrowserWorkspace) {
    let Some(extension) = workspace.extension.as_ref() else {
        return;
    };
    let Some(profile_dir) = workspace.profile_dir.as_deref().map(Path::new) else {
        return;
    };
    let load_path = Path::new(&extension.load_path);
    let Some(extension_root) = load_path.parent() else {
        return;
    };
    cleanup_extension_workspace_paths(profile_dir, extension_root);
}

async fn launch_cdp_browser(
    workspace: &BrowserWorkspace,
    profile_dir: &Path,
) -> Result<(Child, CdpLaunch), BrowserWorkspaceError> {
    let extension_required = workspace.extension.is_some();
    let executable = resolve_chromium_executable(
        matches!(workspace.provider, BrowserWorkspaceProvider::SystemCdp),
        extension_required,
    )?;
    let display_binding = workspace
        .display_target
        .as_deref()
        .map(parse_browser_display_binding)
        .transpose()?;
    if let Some(binding) = display_binding.as_ref() {
        if !crate::virtual_display::process_owns_browser_bindable_display(binding.display_id) {
            return Err(BrowserWorkspaceError::Launch(format!(
                "browser workspace display {} left the daemon-created lifecycle before browser launch",
                binding.canonical
            )));
        }
    }
    let port = reserve_local_port().await?;
    let mut command = tokio::process::Command::new(&executable.path);
    // If the async create request is cancelled while CDP readiness is being
    // awaited, dropping its future must also terminate the spawned browser.
    command.kill_on_drop(true);
    #[cfg(target_os = "linux")]
    if let Some(binding) = display_binding.as_ref() {
        let authorization = crate::vision::virtual_display_x11_authorization(binding.display_id)
            .ok_or_else(|| {
                BrowserWorkspaceError::Launch(format!(
                    "browser workspace display {} has no live private X11 authorization",
                    binding.canonical
                ))
            })?;
        // A bound workspace must use only its leased X11 display. Ambient
        // Wayland/Xauthority state belongs to the daemon's login session and
        // must not redirect or authorize this isolated browser child.
        command
            .env("DISPLAY", format!(":{}", binding.display_id))
            .env("XDG_SESSION_TYPE", "x11")
            .env_remove("WAYLAND_DISPLAY")
            .env("XAUTHORITY", authorization.xauthority_path())
            .arg("--ozone-platform=x11");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg("--disable-breakpad")
        .arg("--disable-client-side-phishing-detection")
        .arg("--disable-component-update")
        .arg("--disable-default-apps")
        .arg("--disable-domain-reliability");
    for flag in browser_extension_launch_flags(workspace.extension.as_ref()) {
        command.arg(flag);
    }
    command
        .arg("--disable-features=AutofillServerCommunication,CertificateTransparencyComponentUpdater,MediaRouter,OptimizationHints,OptimizationGuideModelDownloading,Translate")
        .arg("--disable-popup-blocking")
        .arg("--disable-sync")
        .arg("--metrics-recording-only")
        .arg("--password-store=basic");
    #[cfg(target_os = "macos")]
    command.arg("--use-mock-keychain");
    if let Some(url) = workspace.url.as_ref() {
        command.arg(url);
    } else {
        command.arg("about:blank");
    }
    let child = command.spawn().map_err(|e| {
        BrowserWorkspaceError::Launch(format!(
            "failed to launch {}: {e}",
            executable.path.display()
        ))
    })?;
    let process_id = child.id();
    match wait_for_cdp_target(port, extension_required).await {
        Ok((ws, target_id, extension_runtime_id)) => Ok((
            child,
            CdpLaunch {
                executable,
                process_id,
                port,
                web_socket_debugger_url: ws,
                target_id,
                extension_runtime_id,
            },
        )),
        Err(err) => {
            if let Some(pid) = process_id {
                let _ = crate::platform::terminate_process_tree_now(pid);
            }
            Err(err)
        }
    }
}

fn browser_extension_launch_flags(extension: Option<&BrowserWorkspaceExtension>) -> Vec<String> {
    match extension {
        Some(extension) => vec![
            format!("--disable-extensions-except={}", extension.load_path),
            format!("--load-extension={}", extension.load_path),
        ],
        None => vec!["--disable-extensions".to_string()],
    }
}

async fn reserve_local_port() -> Result<u16, BrowserWorkspaceError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| BrowserWorkspaceError::Io(format!("failed to reserve CDP port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| BrowserWorkspaceError::Io(format!("failed to read CDP port: {e}")))?
        .port();
    drop(listener);
    Ok(port)
}

async fn wait_for_cdp_target(
    port: u16,
    extension_required: bool,
) -> Result<(Option<String>, Option<String>, Option<String>), BrowserWorkspaceError> {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + CDP_STARTUP_TIMEOUT;
    let list_url = format!("http://127.0.0.1:{port}/json/list");
    loop {
        match client.get(&list_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let targets: serde_json::Value = resp.json().await.map_err(|e| {
                    BrowserWorkspaceError::Launch(format!(
                        "failed to parse CDP target list from {list_url}: {e}"
                    ))
                })?;
                if let Some((ws, id)) = first_page_target(&targets) {
                    let extension_runtime_id = active_extension_runtime_id(&targets);
                    if !extension_required || extension_runtime_id.is_some() {
                        return Ok((ws, id, extension_runtime_id));
                    }
                }
            }
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let expected = if extension_required {
                format!(
                    "page target and approved extension service worker {}",
                    APPROVED_BROWSER_EXTENSION_SERVICE_WORKER
                )
            } else {
                "page target".to_string()
            };
            return Err(BrowserWorkspaceError::Launch(format!(
                "timed out waiting for CDP {expected} at {list_url}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

fn active_extension_runtime_id(value: &serde_json::Value) -> Option<String> {
    value.as_array()?.iter().find_map(|target| {
        if target.get("type").and_then(serde_json::Value::as_str) != Some("service_worker") {
            return None;
        }
        let url = target.get("url").and_then(serde_json::Value::as_str)?;
        let remainder = url.strip_prefix("chrome-extension://")?;
        let (runtime_id, worker_path) = remainder.split_once('/')?;
        if worker_path != APPROVED_BROWSER_EXTENSION_SERVICE_WORKER
            || runtime_id.len() != 32
            || !runtime_id.bytes().all(|byte| (b'a'..=b'p').contains(&byte))
        {
            return None;
        }
        Some(runtime_id.to_string())
    })
}

fn first_page_target(value: &serde_json::Value) -> Option<(Option<String>, Option<String>)> {
    let targets = value.as_array()?;
    targets
        .iter()
        .find(|target| target.get("type").and_then(|v| v.as_str()) == Some("page"))
        .map(|target| {
            (
                target
                    .get("webSocketDebuggerUrl")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                target
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            )
        })
}

#[derive(Debug, Clone)]
struct ChromiumExecutable {
    path: PathBuf,
    source: String,
}

fn resolve_chromium_executable(
    allow_system_for_request: bool,
    require_extension_compatible: bool,
) -> Result<ChromiumExecutable, BrowserWorkspaceError> {
    if require_extension_compatible {
        return find_intendant_managed_chromium_executable()
            .map(|path| ChromiumExecutable {
                path,
                source: "intendant-managed-cache".to_string(),
            })
            .ok_or_else(|| {
                BrowserWorkspaceError::Launch(
                    "browser extension launch requires Intendant-managed Chrome for Testing; run `intendant setup browsers`"
                        .to_string(),
                )
            });
    }
    if let Some((env_name, path)) = configured_browser_executable() {
        if is_regular_file(&path) {
            return Ok(ChromiumExecutable {
                path,
                source: format!("env:{env_name}"),
            });
        }
        return Err(BrowserWorkspaceError::Launch(format!(
            "{env_name} points to a missing or non-file browser executable: {}",
            path.display()
        )));
    }

    if !allow_system_for_request {
        if let Some(path) = find_managed_chromium_executable() {
            return Ok(ChromiumExecutable {
                path,
                source: "managed-cache".to_string(),
            });
        }
    }

    #[cfg(target_os = "macos")]
    {
        if allow_system_for_request || allow_system_browser() {
            return find_system_chromium_executable()
                .map(|path| ChromiumExecutable {
                    path,
                    source: if allow_system_for_request {
                        "system-browser-provider".to_string()
                    } else {
                        "system-browser-env-opt-in".to_string()
                    },
                })
                .ok_or_else(|| {
                    let message = if allow_system_for_request {
                        "no system Chrome/Chromium executable found for provider=system_cdp"
                    } else {
                        "no managed Chromium or opted-in system Chrome/Chromium executable found for CDP browser workspace"
                    };
                    BrowserWorkspaceError::Launch(message.to_string())
                });
        }
        Err(BrowserWorkspaceError::Launch(format!(
            "no managed Chromium executable found for CDP browser workspace; install Playwright/Chrome-for-Testing Chromium, set {BROWSER_EXECUTABLE_ENV}, or set {ALLOW_SYSTEM_BROWSER_ENV}=1 to explicitly allow launching the system browser"
        )))
    }

    #[cfg(not(target_os = "macos"))]
    {
        find_system_chromium_executable()
            .map(|path| ChromiumExecutable {
                path,
                source: "system-browser".to_string(),
            })
            .ok_or_else(|| {
                BrowserWorkspaceError::Launch(
                    "no Chrome/Chromium executable found for CDP browser workspace".to_string(),
                )
            })
    }
}

fn configured_browser_executable() -> Option<(&'static str, PathBuf)> {
    for env_name in [BROWSER_EXECUTABLE_ENV, LEGACY_BROWSER_EXECUTABLE_ENV] {
        if let Ok(raw) = std::env::var(env_name) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some((env_name, PathBuf::from(trimmed)));
            }
        }
    }
    None
}

// macOS-only system-browser escape hatch; other platforms' discovery path never consults it.
#[cfg(target_os = "macos")]
fn allow_system_browser() -> bool {
    env_truthy(ALLOW_SYSTEM_BROWSER_ENV) || env_truthy(LEGACY_ALLOW_SYSTEM_BROWSER_ENV)
}

#[cfg(target_os = "macos")]
fn env_truthy(env_name: &str) -> bool {
    std::env::var(env_name)
        .ok()
        .map(|value| env_value_truthy(&value))
        .unwrap_or(false)
}

// Also compiled under `test`: the truthy-vocabulary unit test pins it on every platform.
#[cfg(any(target_os = "macos", test))]
fn env_value_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Debug, Deserialize)]
struct CftManifest {
    channels: BTreeMap<String, CftChannel>,
}

#[derive(Debug, Deserialize)]
struct CftChannel {
    version: String,
    downloads: CftDownloads,
}

#[derive(Debug, Deserialize)]
struct CftDownloads {
    chrome: Vec<CftDownload>,
}

#[derive(Debug, Deserialize)]
struct CftDownload {
    platform: String,
    url: String,
}

async fn fetch_cft_manifest() -> Result<CftManifest, String> {
    let response = reqwest::Client::new()
        .get(CHROME_FOR_TESTING_DOWNLOADS_URL)
        .send()
        .await
        .map_err(|e| format!("failed to fetch Chrome for Testing manifest: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Chrome for Testing manifest request failed: {e}"))?;
    response
        .json::<CftManifest>()
        .await
        .map_err(|e| format!("failed to parse Chrome for Testing manifest: {e}"))
}

async fn download_and_extract_cft(
    url: &str,
    install_root: &Path,
    install_dir: &Path,
    force: bool,
) -> Result<u64, String> {
    fs::create_dir_all(install_root).map_err(|e| {
        format!(
            "failed to create managed browser root {}: {e}",
            install_root.display()
        )
    })?;
    if let Some(parent) = install_dir.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create managed browser directory {}: {e}",
                parent.display()
            )
        })?;
    }

    let staging_dir = install_root.join(format!(".download-{}", uuid::Uuid::new_v4().simple()));
    let extract_dir = staging_dir.join("extract");
    let zip_path = staging_dir.join("chrome-for-testing.zip");
    fs::create_dir_all(&extract_dir).map_err(|e| {
        format!(
            "failed to create managed browser staging directory {}: {e}",
            extract_dir.display()
        )
    })?;

    let result = async {
        let bytes = download_to_file(url, &zip_path).await?;
        extract_zip(&zip_path, &extract_dir)?;
        if install_dir.exists() {
            if force {
                fs::remove_dir_all(install_dir).map_err(|e| {
                    format!(
                        "failed to replace existing managed browser directory {}: {e}",
                        install_dir.display()
                    )
                })?;
            } else {
                return Err(format!(
                    "managed browser directory already exists: {}",
                    install_dir.display()
                ));
            }
        }
        fs::rename(&extract_dir, install_dir).map_err(|e| {
            format!(
                "failed to install managed browser into {}: {e}",
                install_dir.display()
            )
        })?;
        Ok(bytes)
    }
    .await;

    let cleanup = fs::remove_dir_all(&staging_dir);
    if let Err(err) = cleanup {
        if staging_dir.exists() {
            eprintln!(
                "warning: failed to remove managed browser staging directory {}: {err}",
                staging_dir.display()
            );
        }
    }
    result
}

async fn download_to_file(url: &str, path: &Path) -> Result<u64, String> {
    let mut response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("failed to download Chrome for Testing from {url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Chrome for Testing download failed for {url}: {e}"))?;
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| format!("failed to create download file {}: {e}", path.display()))?;
    let mut written = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("failed while downloading Chrome for Testing: {e}"))?
    {
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .map_err(|e| format!("failed to write download file {}: {e}", path.display()))?;
        written += chunk.len() as u64;
    }
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|e| format!("failed to flush download file {}: {e}", path.display()))?;
    Ok(written)
}

fn extract_zip(zip_path: &Path, destination: &Path) -> Result<(), String> {
    let file = fs::File::open(zip_path)
        .map_err(|e| format!("failed to open {}: {e}", zip_path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("failed to read zip archive: {e}"))?;
    archive
        .extract_unwrapped_root_dir(destination, zip::read::root_dir_common_filter)
        .map_err(|e| {
            format!(
                "failed to extract Chrome for Testing into {}: {e}",
                destination.display()
            )
        })
}

fn normalize_cft_channel(raw: &str) -> Result<&'static str, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "stable" => Ok("Stable"),
        "beta" => Ok("Beta"),
        "dev" => Ok("Dev"),
        "canary" => Ok("Canary"),
        other => Err(format!(
            "unsupported Chrome for Testing channel '{other}'; expected stable, beta, dev, or canary"
        )),
    }
}

fn cft_platform() -> Result<&'static str, String> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Ok("linux64")
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok("mac-arm64")
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Ok("mac-x64")
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Ok("win64")
    }
    #[cfg(all(target_os = "windows", target_arch = "x86"))]
    {
        Ok("win32")
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "x86")
    )))]
    {
        Err(format!(
            "Chrome for Testing does not publish a managed browser for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    }
}

fn find_managed_chromium_executable() -> Option<PathBuf> {
    for root in managed_browser_roots() {
        if let Some(path) = find_executable_under(&root, managed_browser_executable_names(), 8) {
            return Some(path);
        }
    }
    None
}

fn find_intendant_managed_chromium_executable() -> Option<PathBuf> {
    find_executable_under(
        &managed_browser_install_root(),
        managed_browser_executable_names(),
        8,
    )
}

fn managed_browser_install_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("intendant")
        .join("browser-workspaces")
}

fn managed_browser_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(cache_dir) = dirs::cache_dir() {
        roots.push(cache_dir.join("ms-playwright"));
        roots.push(cache_dir.join("puppeteer"));
        roots.push(cache_dir.join("chrome-for-testing"));
        roots.push(cache_dir.join("intendant").join("browser-workspaces"));
    }
    if let Some(data_dir) = dirs::data_dir() {
        roots.push(data_dir.join("intendant").join("browser-workspaces"));
        roots.push(data_dir.join("intendant").join("browsers"));
    }
    roots
}

fn managed_browser_executable_names() -> &'static [&'static str] {
    #[cfg(target_os = "windows")]
    {
        &["chrome.exe", "msedge.exe", "chromium.exe"]
    }
    #[cfg(target_os = "macos")]
    {
        &["Google Chrome for Testing", "Chromium", "chrome"]
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        &["chrome", "chromium", "chromium-browser", "google-chrome"]
    }
}

fn find_executable_under(root: &Path, names: &[&str], max_depth: usize) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    let mut entries = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if max_depth > 0 {
                if let Some(found) = find_executable_under(&path, names, max_depth - 1) {
                    return Some(found);
                }
            }
            continue;
        }
        if let Some(file_name) = path.file_name().and_then(|name| name.to_str()) {
            if names.contains(&file_name) && is_regular_file(&path) {
                return Some(path);
            }
        }
    }
    None
}

fn find_system_chromium_executable() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        for path in [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        ] {
            let p = PathBuf::from(path);
            if is_regular_file(&p) {
                return Some(p);
            }
        }
    }
    for name in [
        "google-chrome",
        "chrome",
        "chromium",
        "chromium-browser",
        "msedge",
        "brave-browser",
    ] {
        if let Some(path) = find_executable(name) {
            return Some(path);
        }
    }
    None
}

fn is_regular_file(path: &Path) -> bool {
    path.metadata().map(|m| m.is_file()).unwrap_or(false)
}

fn find_executable(name: &str) -> Option<PathBuf> {
    which::which(name).ok()
}

fn default_profile_dir(id: &str) -> PathBuf {
    let base = dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("intendant")
        .join("browser-workspaces");
    base.join(id).join("profile")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserDisplayBinding {
    canonical: String,
    display_id: u32,
}

fn parse_browser_display_binding(
    raw: &str,
) -> Result<BrowserDisplayBinding, BrowserWorkspaceError> {
    let value = raw.trim();
    let digits = value
        .strip_prefix("display_")
        .or_else(|| value.strip_prefix(':'))
        .unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "invalid browser workspace display target '{value}'; expected display_N, :N, or N"
        )));
    }
    let display_id = digits.parse::<u32>().map_err(|_| {
        BrowserWorkspaceError::Unsupported(format!(
            "invalid browser workspace display target '{value}'"
        ))
    })?;
    if display_id == 0 {
        return Err(BrowserWorkspaceError::Unsupported(
            "browser workspaces cannot bind to the user's session display".to_string(),
        ));
    }
    if !crate::vision::managed_virtual_display_id(display_id) {
        return Err(BrowserWorkspaceError::Unsupported(format!(
            "browser workspace display :{display_id} is outside Intendant's managed virtual-display range"
        )));
    }
    Ok(BrowserDisplayBinding {
        canonical: format!("display_{display_id}"),
        display_id,
    })
}

fn now_string() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_workspace(id: &str) -> BrowserWorkspace {
        BrowserWorkspace {
            id: id.to_string(),
            label: "Test".to_string(),
            url: Some("http://localhost:8765".to_string()),
            provider: BrowserWorkspaceProvider::Cdp,
            requested_provider: BrowserWorkspaceProvider::Auto,
            placement: BrowserWorkspacePlacement::local(),
            status: BrowserWorkspaceStatus::Ready,
            preview_mode: BrowserWorkspacePreviewMode::Semantic,
            owner_session_id: Some("session-1".to_string()),
            display_target: None,
            profile_dir: None,
            extension: None,
            browser_executable: None,
            browser_executable_source: None,
            process_id: None,
            debugging_port: None,
            cdp_http_url: None,
            cdp_ws_url: None,
            active_target_id: None,
            lease: None,
            message: None,
            created_at: "2026-05-31T00:00:00.000Z".to_string(),
            updated_at: "2026-05-31T00:00:00.000Z".to_string(),
        }
    }

    fn sample_create_request() -> CreateBrowserWorkspaceRequest {
        CreateBrowserWorkspaceRequest {
            url: Some("https://example.com".to_string()),
            label: Some("proof".to_string()),
            provider: Some("cdp".to_string()),
            peer_id: None,
            owner_session_id: Some("attempt-1".to_string()),
            display_target: None,
            profile_dir: None,
            extension_archive_path: None,
            extension_archive_sha256: None,
            extension_archive_byte_length: None,
            extension_manifest_version: None,
            extension_version: None,
        }
    }

    fn write_test_extension_archive(path: &Path, case_collision: bool) -> (String, u64) {
        let file = fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o100_644);
        archive.start_file("manifest.json", options).unwrap();
        archive
            .write_all(
                br#"{"manifest_version":3,"version":"0.94.6","name":"Test","background":{"service_worker":"sw.js"}}"#,
            )
            .unwrap();
        archive.start_file("sw.js", options).unwrap();
        archive.write_all(b"self.test = true;\n").unwrap();
        if case_collision {
            archive.start_file("SW.JS", options).unwrap();
            archive.write_all(b"self.other = true;\n").unwrap();
        }
        archive.finish().unwrap();
        let bytes = fs::read(path).unwrap();
        let sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        (sha256, u64::try_from(bytes.len()).unwrap())
    }

    #[test]
    fn extension_archive_tuple_is_all_or_none_and_strict() {
        assert!(parse_extension_archive_spec(&sample_create_request())
            .unwrap()
            .is_none());

        let archive_path = std::env::temp_dir().join("extension.zip");
        let mut partial = sample_create_request();
        partial.extension_archive_path = Some(archive_path.to_string_lossy().into_owned());
        assert!(parse_extension_archive_spec(&partial)
            .unwrap_err()
            .to_string()
            .contains("all-or-none"));

        partial.extension_archive_sha256 = Some(APPROVED_BROWSER_EXTENSION_SHA256.to_string());
        partial.extension_archive_byte_length = Some(APPROVED_BROWSER_EXTENSION_BYTE_LENGTH);
        partial.extension_manifest_version = Some(APPROVED_BROWSER_EXTENSION_MANIFEST_VERSION);
        partial.extension_version = Some(APPROVED_BROWSER_EXTENSION_VERSION.to_string());
        let parsed = parse_extension_archive_spec(&partial).unwrap().unwrap();
        assert_eq!(parsed.archive_path, archive_path);
        assert_eq!(parsed.archive_sha256, APPROVED_BROWSER_EXTENSION_SHA256);
        assert_eq!(
            parsed.archive_byte_length,
            APPROVED_BROWSER_EXTENSION_BYTE_LENGTH
        );
        assert_eq!(
            parsed.manifest_version,
            APPROVED_BROWSER_EXTENSION_MANIFEST_VERSION
        );
        assert_eq!(parsed.version, APPROVED_BROWSER_EXTENSION_VERSION);

        partial.extension_archive_sha256 = Some("A".repeat(64));
        assert!(parse_extension_archive_spec(&partial).is_err());
        partial.extension_archive_sha256 = Some("a".repeat(64));
        assert!(parse_extension_archive_spec(&partial)
            .unwrap_err()
            .to_string()
            .contains("compiled browser-extension allowlist"));
    }

    #[test]
    fn pinned_extension_archive_is_verified_and_safely_materialized() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("extension.zip");
        let (sha256, byte_length) = write_test_extension_archive(&archive_path, false);
        let profile = temp.path().join("profiles").join("attempt-1");
        let extension_root = temp.path().join("state").join("extension-1");
        let _guard = create_extension_workspace_filesystem(&profile, &extension_root).unwrap();
        let spec = BrowserExtensionArchiveSpec {
            archive_path,
            archive_sha256: sha256.clone(),
            archive_byte_length: byte_length,
            manifest_version: 3,
            version: "0.94.6".to_string(),
        };

        let prepared = prepare_browser_extension(&spec, &extension_root).unwrap();
        assert_eq!(prepared.archive_sha256, sha256);
        assert_eq!(prepared.archive_byte_length, byte_length);
        assert_eq!(prepared.manifest_version, 3);
        assert_eq!(prepared.version, "0.94.6");
        assert!(prepared.runtime_id.is_none());
        assert!(Path::new(&prepared.load_path).starts_with(&extension_root));
        assert!(!Path::new(&prepared.load_path).starts_with(&profile));
        assert_eq!(
            fs::read_to_string(Path::new(&prepared.load_path).join("sw.js")).unwrap(),
            "self.test = true;\n"
        );
    }

    #[test]
    fn extension_archive_rejects_digest_drift_and_case_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("extension.zip");
        let (sha256, byte_length) = write_test_extension_archive(&archive_path, true);
        let wrong_root = temp.path().join("wrong-root");
        fs::create_dir(&wrong_root).unwrap();
        let wrong = BrowserExtensionArchiveSpec {
            archive_path: archive_path.clone(),
            archive_sha256: "0".repeat(64),
            archive_byte_length: byte_length,
            manifest_version: 3,
            version: "0.94.6".to_string(),
        };
        assert!(prepare_browser_extension(&wrong, &wrong_root)
            .unwrap_err()
            .to_string()
            .contains("sha256 mismatch"));

        let collision_root = temp.path().join("collision-root");
        fs::create_dir(&collision_root).unwrap();
        let collision = BrowserExtensionArchiveSpec {
            archive_path,
            archive_sha256: sha256,
            archive_byte_length: byte_length,
            manifest_version: 3,
            version: "0.94.6".to_string(),
        };
        assert!(prepare_browser_extension(&collision, &collision_root)
            .unwrap_err()
            .to_string()
            .contains("case-colliding"));
    }

    #[cfg(unix)]
    #[test]
    fn extension_archive_source_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("extension.zip");
        let (sha256, byte_length) = write_test_extension_archive(&archive_path, false);
        let linked_path = temp.path().join("linked.zip");
        symlink(&archive_path, &linked_path).unwrap();
        let extension_root = temp.path().join("extension-root");
        fs::create_dir(&extension_root).unwrap();
        let spec = BrowserExtensionArchiveSpec {
            archive_path: linked_path,
            archive_sha256: sha256,
            archive_byte_length: byte_length,
            manifest_version: 3,
            version: "0.94.6".to_string(),
        };

        assert!(prepare_browser_extension(&spec, &extension_root)
            .unwrap_err()
            .to_string()
            .contains("regular non-symlink"));
    }

    #[test]
    fn extension_launch_flags_are_exact_and_default_is_fail_closed() {
        assert_eq!(
            browser_extension_launch_flags(None),
            vec!["--disable-extensions"]
        );
        let extension = BrowserWorkspaceExtension {
            archive_sha256: "a".repeat(64),
            archive_byte_length: 123,
            manifest_version: 3,
            version: "0.94.6".to_string(),
            load_path: "/private/profile/intendant-extension".to_string(),
            runtime_id: None,
        };
        assert_eq!(
            browser_extension_launch_flags(Some(&extension)),
            vec![
                "--disable-extensions-except=/private/profile/intendant-extension",
                "--load-extension=/private/profile/intendant-extension"
            ]
        );
    }

    #[test]
    fn extension_profile_and_storage_are_reserved_and_cleaned_as_one_lifetime() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profiles").join("attempt-1");
        let extension_root = temp.path().join("state").join("extension-1");
        let guard = create_extension_workspace_filesystem(&profile, &extension_root).unwrap();
        assert!(profile.is_dir());
        assert!(extension_root.is_dir());

        let duplicate = create_extension_workspace_filesystem(
            &profile,
            &temp.path().join("state").join("extension-2"),
        )
        .unwrap_err();
        assert!(duplicate.to_string().contains("fresh profile_dir"));

        drop(guard);
        assert!(!profile.exists());
        assert!(!extension_root.exists());
    }

    #[tokio::test]
    async fn cancelled_starting_reservation_is_removed_and_closed() {
        let id = format!("bw-cancel-test-{}", uuid::Uuid::new_v4().simple());
        let mut workspace = sample_workspace(&id);
        workspace.status = BrowserWorkspaceStatus::Starting;
        global_registry().write().await.insert(workspace, None);

        let bus = EventBus::new();
        let mut events = bus.subscribe();
        drop(StartingReservationGuard::new(id.clone(), bus));

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if !global_registry().read().await.workspaces.contains_key(&id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled reservation cleanup");

        match events.recv().await.expect("closed cancellation event") {
            AppEvent::BrowserWorkspaceChanged {
                kind,
                workspace_id,
                workspace: Some(workspace),
                ..
            } => {
                assert_eq!(kind, "closed");
                assert_eq!(workspace_id.as_deref(), Some(id.as_str()));
                assert_eq!(workspace.status, BrowserWorkspaceStatus::Closed);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn acquire_publishes_serialized_lease_state() {
        let id = format!("bw-lease-test-{}", uuid::Uuid::new_v4().simple());
        global_registry()
            .write()
            .await
            .insert(sample_workspace(&id), None);
        let bus = EventBus::new();
        let mut events = bus.subscribe();

        let acquired = acquire_workspace(
            AcquireBrowserWorkspaceRequest {
                workspace_id: id.clone(),
                holder_id: "audit-agent".to_string(),
                holder_kind: Some("agent".to_string()),
                note: None,
                force: false,
            },
            &bus,
        )
        .await
        .unwrap();
        assert_eq!(
            acquired
                .lease
                .as_ref()
                .map(|lease| lease.holder_id.as_str()),
            Some("audit-agent")
        );
        match events.recv().await.expect("lease event") {
            AppEvent::BrowserWorkspaceChanged {
                kind,
                workspace_id,
                workspace: Some(workspace),
                ..
            } => {
                assert_eq!(kind, "lease_acquired");
                assert_eq!(workspace_id.as_deref(), Some(id.as_str()));
                assert_eq!(workspace, acquired);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        global_registry().write().await.remove(&id);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn explicit_browser_display_binding_is_strict_and_canonical() {
        for raw in ["display_99", ":99", "99", " display_099 "] {
            assert_eq!(
                parse_browser_display_binding(raw).unwrap(),
                BrowserDisplayBinding {
                    canonical: "display_99".to_string(),
                    display_id: 99,
                }
            );
        }
        for raw in [
            "",
            "user_session",
            "display_0",
            ":0",
            "0",
            "display_-1",
            "display_98",
            "display_200",
            "display_99.0",
        ] {
            assert!(
                parse_browser_display_binding(raw).is_err(),
                "unsafe display target unexpectedly accepted: {raw}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dead_bound_display_marks_workspace_error_and_revokes_lease() {
        let mut registry = BrowserWorkspaceRegistry::default();
        let mut workspace = sample_workspace("bw-dead-display");
        workspace.display_target = Some("display_99".to_string());
        workspace.process_id = Some(42);
        workspace.debugging_port = Some(9222);
        workspace.cdp_http_url = Some("http://127.0.0.1:9222".to_string());
        workspace.cdp_ws_url = Some("ws://127.0.0.1:9222/devtools/browser/test".to_string());
        workspace.active_target_id = Some("page-1".to_string());
        workspace.lease = Some(BrowserWorkspaceLease {
            holder_id: "agent-a".to_string(),
            holder_kind: "agent".to_string(),
            acquired_at: "2026-05-31T00:00:00.000Z".to_string(),
            note: None,
        });
        registry.insert(workspace, None);

        let stale = registry.reconcile_display_bindings_with(|_| false);

        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].process_id, Some(42));
        let workspace = registry.workspaces.get("bw-dead-display").unwrap();
        assert_eq!(workspace.status, BrowserWorkspaceStatus::Error);
        assert!(workspace.lease.is_none());
        assert!(workspace.process_id.is_none());
        assert!(workspace.debugging_port.is_none());
        assert!(workspace.cdp_http_url.is_none());
        assert!(workspace.cdp_ws_url.is_none());
        assert!(workspace.active_target_id.is_none());
        assert!(workspace
            .message
            .as_deref()
            .is_some_and(|message| message.contains("display_99 is no longer live")));

        let acquire = registry.acquire(AcquireBrowserWorkspaceRequest {
            workspace_id: "bw-dead-display".to_string(),
            holder_id: "agent-b".to_string(),
            holder_kind: Some("agent".to_string()),
            note: None,
            force: false,
        });
        assert!(matches!(
            acquire,
            Err(BrowserWorkspaceError::Unsupported(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_bound_display_remains_ready_and_leasable() {
        let mut registry = BrowserWorkspaceRegistry::default();
        let mut workspace = sample_workspace("bw-live-display");
        workspace.display_target = Some("display_99".to_string());
        registry.insert(workspace, None);

        let stale = registry.reconcile_display_bindings_with(|display_id| display_id == 99);

        assert!(stale.is_empty());
        assert_eq!(
            registry.workspaces.get("bw-live-display").unwrap().status,
            BrowserWorkspaceStatus::Ready
        );
        assert!(registry
            .acquire(AcquireBrowserWorkspaceRequest {
                workspace_id: "bw-live-display".to_string(),
                holder_id: "agent-a".to_string(),
                holder_kind: Some("agent".to_string()),
                note: None,
                force: false,
            })
            .is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn display_retirement_targets_only_workspaces_bound_to_that_display() {
        let mut registry = BrowserWorkspaceRegistry::default();
        let mut retired = sample_workspace("bw-retired");
        retired.status = BrowserWorkspaceStatus::Starting;
        retired.display_target = Some("display_99".to_string());
        retired.lease = Some(BrowserWorkspaceLease {
            holder_id: "agent-a".to_string(),
            holder_kind: "agent".to_string(),
            acquired_at: "2026-05-31T00:00:00.000Z".to_string(),
            note: None,
        });
        let mut survivor = sample_workspace("bw-survivor");
        survivor.display_target = Some("display_100".to_string());
        registry.insert(retired, None);
        registry.insert(survivor, None);

        let retired = registry.retire_workspaces_for_display(99, "tile closed");

        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].workspace.id, "bw-retired");
        assert_eq!(retired[0].workspace.status, BrowserWorkspaceStatus::Error);
        assert!(retired[0].workspace.lease.is_none());
        assert_eq!(
            retired[0].workspace.message.as_deref(),
            Some("bound virtual display display_99 was retired: tile closed")
        );
        assert_eq!(
            registry.workspaces.get("bw-survivor").unwrap().status,
            BrowserWorkspaceStatus::Ready
        );
    }

    #[test]
    fn reconciled_display_retirement_publishes_the_error_state() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let mut workspace = sample_workspace("bw-reconciled");
        workspace.status = BrowserWorkspaceStatus::Error;
        workspace.message = Some("bound display exited".to_string());

        let retired = vec![RetiredBrowserWorkspace {
            workspace: workspace.clone(),
            process_id: None,
            child: None,
        }];
        publish_retirements_locked(&bus, &retired);
        terminate_retired_processes(retired);

        match events.try_recv().expect("display retirement event") {
            AppEvent::BrowserWorkspaceChanged {
                kind,
                workspace_id,
                message,
                workspace: Some(published),
            } => {
                assert_eq!(kind, "display_retired");
                assert_eq!(workspace_id.as_deref(), Some("bw-reconciled"));
                assert_eq!(message.as_deref(), Some("bound display exited"));
                assert_eq!(published, workspace);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn explicit_browser_display_binding_is_unavailable_without_managed_xvfb() {
        assert!(parse_browser_display_binding("display_99").is_err());
    }

    #[test]
    fn lease_blocks_second_holder_without_force() {
        let mut registry = BrowserWorkspaceRegistry::default();
        registry.insert(sample_workspace("bw-test"), None);
        let first = registry
            .acquire(AcquireBrowserWorkspaceRequest {
                workspace_id: "bw-test".to_string(),
                holder_id: "agent-a".to_string(),
                holder_kind: Some("agent".to_string()),
                note: None,
                force: false,
            })
            .unwrap();
        assert_eq!(first.lease.unwrap().holder_id, "agent-a");

        let second = registry.acquire(AcquireBrowserWorkspaceRequest {
            workspace_id: "bw-test".to_string(),
            holder_id: "agent-b".to_string(),
            holder_kind: Some("agent".to_string()),
            note: None,
            force: false,
        });
        assert!(matches!(
            second,
            Err(BrowserWorkspaceError::LeaseHeld { .. })
        ));
    }

    #[test]
    fn force_acquire_replaces_holder() {
        let mut registry = BrowserWorkspaceRegistry::default();
        registry.insert(sample_workspace("bw-test"), None);
        registry
            .acquire(AcquireBrowserWorkspaceRequest {
                workspace_id: "bw-test".to_string(),
                holder_id: "agent-a".to_string(),
                holder_kind: Some("agent".to_string()),
                note: None,
                force: false,
            })
            .unwrap();
        let forced = registry
            .acquire(AcquireBrowserWorkspaceRequest {
                workspace_id: "bw-test".to_string(),
                holder_id: "agent-b".to_string(),
                holder_kind: Some("agent".to_string()),
                note: Some("takeover".to_string()),
                force: true,
            })
            .unwrap();
        assert_eq!(forced.lease.unwrap().holder_id, "agent-b");
    }

    #[test]
    fn cdp_target_parser_prefers_page() {
        let targets = serde_json::json!([
            {"type":"service_worker","id":"worker"},
            {"type":"page","id":"page-1","webSocketDebuggerUrl":"ws://127.0.0.1/devtools/page/page-1"}
        ]);
        let (ws, id) = first_page_target(&targets).unwrap();
        assert_eq!(id.as_deref(), Some("page-1"));
        assert_eq!(ws.as_deref(), Some("ws://127.0.0.1/devtools/page/page-1"));
    }

    #[test]
    fn cdp_liveness_parser_requires_the_exact_page_target() {
        let targets = serde_json::json!([
            {"type":"page","id":"replacement","webSocketDebuggerUrl":"ws://127.0.0.1/devtools/page/replacement"},
            {"type":"service_worker","id":"page-1","webSocketDebuggerUrl":"ws://127.0.0.1/devtools/worker/page-1"},
            {"type":"page","id":"page-1","webSocketDebuggerUrl":"ws://127.0.0.1/devtools/page/page-1"}
        ]);
        let target = exact_page_target(&targets, "page-1").unwrap();
        assert_eq!(
            target
                .get("webSocketDebuggerUrl")
                .and_then(serde_json::Value::as_str),
            Some("ws://127.0.0.1/devtools/page/page-1")
        );
        assert!(exact_page_target(&targets, "closed-page").is_none());
    }

    #[test]
    fn extension_readiness_requires_the_exact_service_worker_shape() {
        let valid_id = "abcdefghijklmnopabcdefghijklmnop";
        let targets = serde_json::json!([
            {
                "type": "service_worker",
                "url": format!("chrome-extension://{valid_id}/sw.js")
            }
        ]);
        assert_eq!(
            active_extension_runtime_id(&targets).as_deref(),
            Some(valid_id)
        );

        for url in [
            format!("chrome-extension://{valid_id}/other.js"),
            "chrome-extension://zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz/sw.js".to_string(),
            format!("https://{valid_id}/sw.js"),
        ] {
            let targets = serde_json::json!([{"type":"service_worker","url":url}]);
            assert!(active_extension_runtime_id(&targets).is_none());
        }
    }

    #[test]
    fn parses_explicit_system_cdp_provider() {
        assert_eq!(
            BrowserWorkspaceProvider::parse(Some("system_cdp")),
            BrowserWorkspaceProvider::SystemCdp
        );
        assert_eq!(
            BrowserWorkspaceProvider::parse(Some("system-chrome")),
            BrowserWorkspaceProvider::SystemCdp
        );
    }

    #[test]
    fn truthy_env_parser_is_strict() {
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(env_value_truthy(value), "{value:?} should be truthy");
        }
        for value in ["", "0", "false", "off", "system"] {
            assert!(!env_value_truthy(value), "{value:?} should not be truthy");
        }
    }

    #[test]
    fn cft_channel_parser_accepts_known_channels() {
        assert_eq!(normalize_cft_channel(""), Ok("Stable"));
        assert_eq!(normalize_cft_channel("stable"), Ok("Stable"));
        assert_eq!(normalize_cft_channel("BETA"), Ok("Beta"));
        assert_eq!(normalize_cft_channel(" dev "), Ok("Dev"));
        assert_eq!(normalize_cft_channel("canary"), Ok("Canary"));
        assert!(normalize_cft_channel("nightly").is_err());
    }

    #[test]
    fn cft_platform_is_supported_on_tier_one_targets() {
        assert!(cft_platform().is_ok());
    }

    #[test]
    fn finds_deep_managed_browser_executable() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp
            .path()
            .join("chrome")
            .join("mac_arm-123")
            .join("chrome-mac-arm64")
            .join("Google Chrome for Testing.app")
            .join("Contents")
            .join("MacOS")
            .join("Google Chrome for Testing");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "").unwrap();

        let found = find_executable_under(temp.path(), &["Google Chrome for Testing"], 6)
            .expect("managed browser executable should be found");
        assert_eq!(found, executable);
    }
}

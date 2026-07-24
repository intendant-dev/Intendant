//! Codex Cloud task provider and ephemeral worker-lease tracking.
//!
//! A Codex Cloud container is not a durable Intendant peer. Its provider task
//! can finish while processes remain reachable briefly, and the container can
//! later be reclaimed without a peer-level disconnect event. This module keeps
//! provider lifecycle state above peer transport state and delegates Cloud API
//! access to the user's already-authenticated `codex cloud` CLI.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

const STORE_VERSION: u32 = 1;
const SETUP_SCRIPT: &str = include_str!("../../../scripts/codex-cloud/setup.sh");
const MAINTENANCE_SCRIPT: &str = include_str!("../../../scripts/codex-cloud/maintenance.sh");
const WORKER_SCRIPT: &str = include_str!("../../../scripts/codex-cloud/run-worker.sh");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderLeaseState {
    Queued,
    Running,
    Finished,
    Failed,
    Cancelled,
    Unknown,
}

impl ProviderLeaseState {
    fn from_codex_status(status: &str) -> Self {
        match status.trim().to_ascii_lowercase().as_str() {
            "queued" | "pending" | "starting" => Self::Queued,
            "running" | "in_progress" | "active" => Self::Running,
            "ready" | "completed" | "complete" | "succeeded" => Self::Finished,
            "error" | "failed" => Self::Failed,
            "cancelled" | "canceled" => Self::Cancelled,
            _ => Self::Unknown,
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentState {
    NotRequested,
    Awaiting,
    Connected,
    Disconnected,
    Expired,
}

impl Default for AttachmentState {
    fn default() -> Self {
        Self::NotRequested
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerLease {
    pub task_id: String,
    pub task_url: Option<String>,
    pub title: String,
    pub environment_id: Option<String>,
    pub environment_label: Option<String>,
    pub provider_status: String,
    pub provider_state: ProviderLeaseState,
    #[serde(default)]
    pub attachment_state: AttachmentState,
    pub provider_updated_at: Option<String>,
    pub last_observed_unix_ms: u64,
}

#[derive(Debug, Clone)]
pub struct SubmitTaskRequest {
    pub environment: String,
    pub branch: Option<String>,
    pub attempts: u16,
    pub title: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubmitTaskResult {
    pub task_id: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub lease: Option<WorkerLease>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LeaseStore {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    leases: BTreeMap<String, WorkerLease>,
}

impl Default for LeaseStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            leases: BTreeMap::new(),
        }
    }
}

fn store_version() -> u32 {
    STORE_VERSION
}

#[derive(Debug, Deserialize)]
struct CloudListResponse {
    #[serde(default)]
    tasks: Vec<CloudTask>,
    #[allow(dead_code)]
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CloudTask {
    id: String,
    url: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: String,
    updated_at: Option<String>,
    environment_id: Option<String>,
    environment_label: Option<String>,
}

#[derive(Debug)]
struct CommandOutput {
    stdout: String,
    stderr: String,
}

pub async fn run(args: Vec<String>) -> Result<(), String> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(());
    }

    match args[0].as_str() {
        "doctor" => run_doctor(&args[1..]).await,
        "exec" | "submit" => run_exec(&args[1..]).await,
        "list" | "refresh" => run_list(&args[1..]).await,
        "status" => run_status(&args[1..]).await,
        "diff" => run_passthrough("diff", &args[1..]).await,
        "bootstrap" => run_bootstrap(&args[1..]),
        "attachment" => run_attachment(&args[1..]),
        other => Err(format!(
            "unknown codex-cloud command '{other}'. Run `intendant codex-cloud --help`."
        )),
    }
}

async fn run_doctor(args: &[String]) -> Result<(), String> {
    reject_args(args, "doctor")?;
    let codex = codex_command();
    let version = run_codex(&codex, &["--version".into()]).await?;
    let list = run_codex(
        &codex,
        &[
            "cloud".into(),
            "list".into(),
            "--json".into(),
            "--limit".into(),
            "1".into(),
        ],
    )
    .await?;
    parse_cloud_list(&list.stdout)?;
    println!("Codex Cloud provider is ready");
    println!("  {}", version.stdout.trim());
    println!("  auth: task listing succeeded");
    println!("  state: {}", state_path().display());
    Ok(())
}

async fn run_exec(args: &[String]) -> Result<(), String> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        print_exec_help();
        return Ok(());
    }

    let parsed = parse_exec_args(args)?;
    let result = submit_task(SubmitTaskRequest {
        environment: parsed.environment,
        branch: parsed.branch,
        attempts: parsed.attempts,
        title: parsed.title,
        prompt: parsed.query,
    })
    .await?;
    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
        if !result.stdout.ends_with('\n') {
            println!();
        }
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }

    if let Some(task_id) = result.task_id {
        eprintln!("[intendant] tracking worker lease {task_id}");
    } else {
        eprintln!(
            "[intendant] task submitted, but this Codex CLI output did not include a task id; run `intendant codex-cloud list` to synchronize it"
        );
    }
    Ok(())
}

pub async fn submit_task(request: SubmitTaskRequest) -> Result<SubmitTaskResult, String> {
    if request.environment.trim().is_empty() {
        return Err("Codex Cloud environment id cannot be empty".to_string());
    }
    if request.prompt.trim().is_empty() {
        return Err("Codex Cloud task prompt cannot be empty".to_string());
    }
    if request.attempts == 0 {
        return Err("Codex Cloud attempts must be positive".to_string());
    }

    let mut cloud_args = vec![
        "cloud".to_string(),
        "exec".to_string(),
        "--env".to_string(),
        request.environment.clone(),
        "--attempts".to_string(),
        request.attempts.to_string(),
    ];
    if let Some(branch) = request
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
    {
        cloud_args.push("--branch".to_string());
        cloud_args.push(branch.to_string());
    }
    cloud_args.push(request.prompt);

    let output = run_codex(&codex_command(), &cloud_args).await?;
    let task_id = extract_task_id(&format!("{}\n{}", output.stdout, output.stderr));
    let lease = if let Some(task_id) = task_id.as_deref() {
        let mut store = load_store()?;
        let lease = store
            .leases
            .entry(task_id.to_string())
            .or_insert_with(|| WorkerLease {
                task_id: task_id.to_string(),
                task_url: Some(format!("https://chatgpt.com/codex/tasks/{task_id}")),
                title: request
                    .title
                    .unwrap_or_else(|| "Codex Cloud task".to_string()),
                environment_id: Some(request.environment),
                environment_label: None,
                provider_status: "submitted".to_string(),
                provider_state: ProviderLeaseState::Queued,
                attachment_state: AttachmentState::NotRequested,
                provider_updated_at: None,
                last_observed_unix_ms: now_unix_ms(),
            })
            .clone();
        save_store(&store)?;
        Some(lease)
    } else {
        None
    };

    Ok(SubmitTaskResult {
        task_id,
        stdout: output.stdout,
        stderr: output.stderr,
        lease,
    })
}

async fn run_list(args: &[String]) -> Result<(), String> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        print_list_help();
        return Ok(());
    }
    let options = parse_list_args(args)?;
    let observed = refresh_leases(
        options.environment.as_deref(),
        options.limit,
        options.cursor.as_deref(),
    )
    .await?;
    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&observed)
                .map_err(|e| format!("serialize worker leases: {e}"))?
        );
    } else if observed.is_empty() {
        println!("No Codex Cloud tasks found.");
    } else {
        println!(
            "{:<38}  {:<10}  {:<13}  {}",
            "TASK", "PROVIDER", "ATTACHMENT", "TITLE"
        );
        for lease in &observed {
            println!(
                "{:<38}  {:<10}  {:<13}  {}",
                lease.task_id,
                lease.provider_status,
                attachment_label(&lease.attachment_state),
                lease.title
            );
        }
    }
    Ok(())
}

pub async fn refresh_leases(
    environment: Option<&str>,
    limit: u8,
    cursor: Option<&str>,
) -> Result<Vec<WorkerLease>, String> {
    if !(1..=20).contains(&limit) {
        return Err("Codex Cloud list limit must be from 1 to 20".to_string());
    }
    let mut cloud_args = vec![
        "cloud".to_string(),
        "list".to_string(),
        "--json".to_string(),
        "--limit".to_string(),
        limit.to_string(),
    ];
    if let Some(environment) = environment.map(str::trim).filter(|value| !value.is_empty()) {
        cloud_args.push("--env".to_string());
        cloud_args.push(environment.to_string());
    }
    if let Some(cursor) = cursor.map(str::trim).filter(|value| !value.is_empty()) {
        cloud_args.push("--cursor".to_string());
        cloud_args.push(cursor.to_string());
    }

    let output = run_codex(&codex_command(), &cloud_args).await?;
    let response = parse_cloud_list(&output.stdout)?;
    let mut store = load_store()?;
    sync_store(&mut store, &response.tasks);
    save_store(&store)?;
    Ok(response
        .tasks
        .iter()
        .filter_map(|task| store.leases.get(&task.id).cloned())
        .collect())
}

async fn run_status(args: &[String]) -> Result<(), String> {
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        println!("Usage: intendant codex-cloud status <TASK_ID> [--json]");
        return Ok(());
    }
    let task_id = args[0].clone();
    let json = match args.get(1).map(String::as_str) {
        None => false,
        Some("--json") => true,
        Some(other) => return Err(format!("unknown status flag {other}")),
    };
    if args.len() > 2 {
        return Err("status accepts one task id and optional --json".to_string());
    }

    // The current upstream `codex cloud status` has no JSON mode. Refresh the
    // structured list first, then fall back to its human-readable status when
    // a task is older than the list window.
    let refresh = run_codex(
        &codex_command(),
        &[
            "cloud".into(),
            "list".into(),
            "--json".into(),
            "--limit".into(),
            "20".into(),
        ],
    )
    .await?;
    let response = parse_cloud_list(&refresh.stdout)?;
    let mut store = load_store()?;
    sync_store(&mut store, &response.tasks);
    save_store(&store)?;

    if response.tasks.iter().any(|task| task.id == task_id) {
        let lease = &store.leases[&task_id];
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(lease)
                    .map_err(|e| format!("serialize worker lease: {e}"))?
            );
        } else {
            print_lease(lease);
        }
        return Ok(());
    }

    if json {
        return Err(
            "task was not in the newest 20 Cloud tasks; the upstream `codex cloud status` command has no JSON mode"
                .to_string(),
        );
    }
    let output = run_codex(
        &codex_command(),
        &["cloud".into(), "status".into(), task_id],
    )
    .await?;
    print!("{}", output.stdout);
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
    Ok(())
}

async fn run_passthrough(command: &str, args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err(format!("{command} requires a Codex Cloud task id"));
    }
    let mut cloud_args = vec!["cloud".to_string(), command.to_string()];
    cloud_args.extend(args.iter().cloned());
    let working_dir = codex_working_dir()?;
    let mut child = crate::platform::spawn_command(&codex_command());
    let status = child
        .args(cloud_args)
        .current_dir(working_dir.path())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|e| format!("run Codex CLI: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Codex CLI exited with {status}"))
    }
}

fn run_attachment(args: &[String]) -> Result<(), String> {
    if args.len() != 2
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        println!(
            "Usage: intendant codex-cloud attachment <TASK_ID> <awaiting|connected|disconnected|expired|none>"
        );
        return Ok(());
    }
    let state = match args[1].as_str() {
        "awaiting" => AttachmentState::Awaiting,
        "connected" => AttachmentState::Connected,
        "disconnected" => AttachmentState::Disconnected,
        "expired" => AttachmentState::Expired,
        "none" | "not-requested" => AttachmentState::NotRequested,
        other => return Err(format!("unknown attachment state {other:?}")),
    };
    let mut store = load_store()?;
    let label = {
        let lease = store.leases.get_mut(&args[0]).ok_or_else(|| {
            format!(
                "unknown worker lease {}; run `intendant codex-cloud list`",
                args[0]
            )
        })?;
        lease.attachment_state = state;
        lease.last_observed_unix_ms = now_unix_ms();
        attachment_label(&lease.attachment_state)
    };
    save_store(&store)?;
    println!("{} attachment={label}", args[0]);
    Ok(())
}

fn run_bootstrap(args: &[String]) -> Result<(), String> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        print_bootstrap_help();
        return Ok(());
    }
    let options = parse_bootstrap_args(args)?;
    if let Some(which) = options.print.as_deref() {
        let content = match which {
            "setup" => SETUP_SCRIPT,
            "maintenance" => MAINTENANCE_SCRIPT,
            "worker" => WORKER_SCRIPT,
            other => {
                return Err(format!(
                    "unknown bootstrap script {other:?}; expected setup, maintenance, or worker"
                ))
            }
        };
        print!("{content}");
        return Ok(());
    }

    let output = options
        .output
        .unwrap_or_else(|| PathBuf::from("intendant-codex-cloud"));
    let targets = [
        ("setup.sh", SETUP_SCRIPT.as_bytes(), true),
        ("maintenance.sh", MAINTENANCE_SCRIPT.as_bytes(), true),
        ("run-worker.sh", WORKER_SCRIPT.as_bytes(), true),
        ("README.md", bootstrap_readme().as_bytes(), false),
    ];
    if !options.force {
        if let Some(existing) = targets
            .iter()
            .map(|(name, _, _)| output.join(name))
            .find(|path| path.exists())
        {
            return Err(format!(
                "{} already exists (use --force to replace the generated bundle)",
                existing.display()
            ));
        }
    }
    std::fs::create_dir_all(&output)
        .map_err(|e| format!("create bootstrap directory {}: {e}", output.display()))?;
    for (name, content, executable) in targets {
        write_bundle_file(&output.join(name), content, options.force, executable)?;
    }
    println!("Wrote Codex Cloud bootstrap bundle to {}", output.display());
    println!("  setup script:       {}/setup.sh", output.display());
    println!("  maintenance script: {}/maintenance.sh", output.display());
    println!("  worker launcher:    {}/run-worker.sh", output.display());
    Ok(())
}

#[derive(Debug)]
struct ExecArgs {
    environment: String,
    branch: Option<String>,
    attempts: u16,
    title: Option<String>,
    query: String,
}

fn parse_exec_args(args: &[String]) -> Result<ExecArgs, String> {
    let mut environment = None;
    let mut branch = None;
    let mut attempts = 1u16;
    let mut title = None;
    let mut query = Vec::new();
    let mut positional = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if positional {
            query.push(arg.clone());
            i += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional = true,
            "--env" => {
                i += 1;
                environment = Some(required_value(args, i, "--env")?);
            }
            "--branch" => {
                i += 1;
                branch = Some(required_value(args, i, "--branch")?);
            }
            "--attempts" => {
                i += 1;
                attempts = required_value(args, i, "--attempts")?
                    .parse()
                    .map_err(|_| "--attempts must be a positive integer".to_string())?;
                if attempts == 0 {
                    return Err("--attempts must be a positive integer".to_string());
                }
            }
            "--title" => {
                i += 1;
                title = Some(required_value(args, i, "--title")?);
            }
            other if other.starts_with('-') => return Err(format!("unknown exec flag {other}")),
            _ => {
                positional = true;
                query.push(arg.clone());
            }
        }
        i += 1;
    }
    let environment = environment.ok_or_else(|| "exec requires --env <ENV_ID>".to_string())?;
    let query = query.join(" ").trim().to_string();
    if query.is_empty() {
        return Err("exec requires a task prompt".to_string());
    }
    Ok(ExecArgs {
        environment,
        branch,
        attempts,
        title,
        query,
    })
}

#[derive(Debug)]
struct ListArgs {
    environment: Option<String>,
    limit: u8,
    cursor: Option<String>,
    json: bool,
}

fn parse_list_args(args: &[String]) -> Result<ListArgs, String> {
    let mut options = ListArgs {
        environment: None,
        limit: 20,
        cursor: None,
        json: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--env" => {
                i += 1;
                options.environment = Some(required_value(args, i, "--env")?);
            }
            "--limit" => {
                i += 1;
                options.limit = required_value(args, i, "--limit")?
                    .parse()
                    .map_err(|_| "--limit must be an integer from 1 to 20".to_string())?;
                if !(1..=20).contains(&options.limit) {
                    return Err("--limit must be an integer from 1 to 20".to_string());
                }
            }
            "--cursor" => {
                i += 1;
                options.cursor = Some(required_value(args, i, "--cursor")?);
            }
            "--json" => options.json = true,
            other => return Err(format!("unknown list flag {other}")),
        }
        i += 1;
    }
    Ok(options)
}

#[derive(Debug, Default)]
struct BootstrapArgs {
    output: Option<PathBuf>,
    print: Option<String>,
    force: bool,
}

fn parse_bootstrap_args(args: &[String]) -> Result<BootstrapArgs, String> {
    let mut options = BootstrapArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--output" => {
                i += 1;
                options.output = Some(PathBuf::from(required_value(args, i, "--output")?));
            }
            "--print" => {
                i += 1;
                options.print = Some(required_value(args, i, "--print")?);
            }
            "--force" => options.force = true,
            other => return Err(format!("unknown bootstrap flag {other}")),
        }
        i += 1;
    }
    if options.output.is_some() && options.print.is_some() {
        return Err("bootstrap accepts either --output or --print, not both".to_string());
    }
    if options.force && options.print.is_some() {
        return Err("--force only applies with --output".to_string());
    }
    Ok(options)
}

fn required_value(args: &[String], index: usize, flag: &str) -> Result<String, String> {
    args.get(index)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_cloud_list(stdout: &str) -> Result<CloudListResponse, String> {
    serde_json::from_str(stdout).map_err(|e| format!("parse `codex cloud list --json`: {e}"))
}

fn sync_store(store: &mut LeaseStore, tasks: &[CloudTask]) {
    let observed = now_unix_ms();
    for task in tasks {
        let provider_state = ProviderLeaseState::from_codex_status(&task.status);
        let existing_attachment = store
            .leases
            .get(&task.id)
            .map(|lease| lease.attachment_state.clone())
            .unwrap_or_default();
        let attachment_state = if provider_state.is_terminal()
            && existing_attachment == AttachmentState::Disconnected
        {
            AttachmentState::Expired
        } else {
            existing_attachment
        };
        store.leases.insert(
            task.id.clone(),
            WorkerLease {
                task_id: task.id.clone(),
                task_url: task.url.clone(),
                title: task.title.clone(),
                environment_id: task.environment_id.clone(),
                environment_label: task.environment_label.clone(),
                provider_status: task.status.clone(),
                provider_state,
                attachment_state,
                provider_updated_at: task.updated_at.clone(),
                last_observed_unix_ms: observed,
            },
        );
    }
}

fn load_store() -> Result<LeaseStore, String> {
    let path = state_path();
    match std::fs::read(&path) {
        Ok(bytes) => {
            let store: LeaseStore = serde_json::from_slice(&bytes)
                .map_err(|e| format!("parse worker lease store {}: {e}", path.display()))?;
            if store.version != STORE_VERSION {
                return Err(format!(
                    "unsupported worker lease store version {} in {}",
                    store.version,
                    path.display()
                ));
            }
            Ok(store)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LeaseStore {
            version: STORE_VERSION,
            leases: BTreeMap::new(),
        }),
        Err(e) => Err(format!("read worker lease store {}: {e}", path.display())),
    }
}

fn save_store(store: &LeaseStore) -> Result<(), String> {
    let path = state_path();
    let bytes = serde_json::to_vec_pretty(store)
        .map_err(|e| format!("serialize worker lease store: {e}"))?;
    crate::file_watcher::atomic_write(&path, &bytes)
        .map_err(|e| format!("write worker lease store {}: {e}", path.display()))
}

fn state_path() -> PathBuf {
    if let Some(path) = std::env::var_os("INTENDANT_CODEX_CLOUD_STATE") {
        return PathBuf::from(path);
    }
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("intendant")
        .join("codex-cloud")
        .join("leases.json")
}

fn codex_command() -> String {
    std::env::var("INTENDANT_CODEX_COMMAND")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

async fn run_codex(program: &str, args: &[String]) -> Result<CommandOutput, String> {
    let working_dir = codex_working_dir()?;
    let output = crate::platform::spawn_command(program)
        .args(args)
        .current_dir(working_dir.path())
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("run {program:?}: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        Ok(CommandOutput { stdout, stderr })
    } else {
        let detail = [stderr.trim(), stdout.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        Err(if detail.is_empty() {
            format!("{program:?} exited with {}", output.status)
        } else {
            format!("{program:?} exited with {}: {detail}", output.status)
        })
    }
}

fn codex_working_dir() -> Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix("intendant-codex-cloud-")
        .tempdir()
        .map_err(|e| format!("create isolated Codex CLI working directory: {e}"))
}

fn extract_task_id(output: &str) -> Option<String> {
    output
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .find(|part| {
            part.starts_with("task_")
                && part.len() > "task_".len()
                && part["task_".len()..]
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
        .map(ToOwned::to_owned)
}

fn print_lease(lease: &WorkerLease) {
    println!("{}  {}", lease.task_id, lease.title);
    println!(
        "  provider: {} ({:?})",
        lease.provider_status, lease.provider_state
    );
    println!(
        "  attachment: {}",
        attachment_label(&lease.attachment_state)
    );
    if let Some(environment) = lease
        .environment_label
        .as_deref()
        .or(lease.environment_id.as_deref())
    {
        println!("  environment: {environment}");
    }
    if let Some(url) = lease.task_url.as_deref() {
        println!("  url: {url}");
    }
    if lease.provider_state.is_terminal()
        && matches!(
            lease.attachment_state,
            AttachmentState::Connected | AttachmentState::Awaiting
        )
    {
        println!(
            "  note: provider task is terminal; any live attachment is provisional until independently checked"
        );
    }
}

fn attachment_label(state: &AttachmentState) -> &'static str {
    match state {
        AttachmentState::NotRequested => "none",
        AttachmentState::Awaiting => "awaiting",
        AttachmentState::Connected => "connected",
        AttachmentState::Disconnected => "disconnected",
        AttachmentState::Expired => "expired",
    }
}

fn reject_args(args: &[String], command: &str) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("{command} does not accept arguments"))
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn write_bundle_file(
    path: &Path,
    content: impl AsRef<[u8]>,
    force: bool,
    executable: bool,
) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o755);
    }
    let mut file = options.open(path).map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::AlreadyExists {
            " (use --force to replace the generated bundle)"
        } else {
            ""
        };
        format!("write {}: {e}{hint}", path.display())
    })?;
    file.write_all(content.as_ref())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("mark {} executable: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

fn bootstrap_readme() -> &'static str {
    "# Intendant Codex Cloud bootstrap bundle\n\n\
Paste `setup.sh` and `maintenance.sh` into the matching Codex Cloud environment settings.\n\
Both scripts are idempotent. They install or refresh the Intendant binary but deliberately do\n\
not start a daemon or tunnel: setup and maintenance shells end before the agent phase, and a\n\
cached environment must never retain task identity or enrollment credentials.\n\n\
At task time, start `~/.local/libexec/intendant-cloud/run-worker.sh -- <command> [args...]` in a\n\
foreground/background terminal owned by that task. Pass only a one-time, short-lived enrollment\n\
credential. The launcher creates a fresh per-task XDG identity root and never reuses cached\n\
peer identity. The controller should expire the attachment when the provider task ends or the\n\
connection drops.\n\n\
If `INTENDANT_CLOUD_BINARY_URL` is set, `INTENDANT_CLOUD_BINARY_SHA256` is mandatory. Otherwise\n\
the scripts build the checked-out Intendant repository with Cargo. The Codex environment's agent\n\
internet allowlist must include every exact relay/download domain used by the worker.\n"
}

fn print_help() {
    println!(
        "Usage:\n  intendant codex-cloud <command> [options]\n\nCommands:\n  doctor       Verify the local Codex CLI and Cloud authentication\n  exec         Submit a task and create a provider-owned worker lease\n  list         Refresh and list Cloud tasks/leases\n  status       Show one tracked lease\n  diff         Show a task diff through the Codex CLI\n  attachment   Record the independent live-attachment state\n  bootstrap    Generate setup, maintenance, and task-time worker scripts\n\nCodex Cloud containers are ephemeral worker leases, not permanent peers."
    );
}

fn print_exec_help() {
    println!(
        "Usage:\n  intendant codex-cloud exec --env ENV_ID [--branch BRANCH] [--attempts N] [--title TITLE] -- PROMPT"
    );
}

fn print_list_help() {
    println!(
        "Usage:\n  intendant codex-cloud list [--env ENV_ID] [--limit 1..20] [--cursor CURSOR] [--json]"
    );
}

fn print_bootstrap_help() {
    println!(
        "Usage:\n  intendant codex-cloud bootstrap [--output DIR] [--force]\n  intendant codex-cloud bootstrap --print setup|maintenance|worker"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_provider_states_without_conflating_attachment() {
        assert_eq!(
            ProviderLeaseState::from_codex_status("in_progress"),
            ProviderLeaseState::Running
        );
        assert_eq!(
            ProviderLeaseState::from_codex_status("READY"),
            ProviderLeaseState::Finished
        );
        assert_eq!(
            ProviderLeaseState::from_codex_status("something-new"),
            ProviderLeaseState::Unknown
        );
    }

    #[test]
    fn parses_real_cloud_list_shape() {
        let response = parse_cloud_list(
            r#"{
              "tasks": [{
                "id": "task_e_123",
                "url": "https://chatgpt.com/codex/tasks/task_e_123",
                "title": "Build it",
                "status": "ready",
                "updated_at": "2026-07-24T08:42:37Z",
                "environment_id": null,
                "environment_label": "owner/repo",
                "summary": {"files_changed": 0},
                "is_review": false,
                "attempt_total": 1
              }],
              "cursor": "opaque"
            }"#,
        )
        .unwrap();
        assert_eq!(response.tasks.len(), 1);
        assert_eq!(response.tasks[0].id, "task_e_123");
        assert_eq!(
            response.tasks[0].environment_label.as_deref(),
            Some("owner/repo")
        );
    }

    #[test]
    fn sync_preserves_independent_attachment_state() {
        let mut store = LeaseStore {
            version: STORE_VERSION,
            leases: BTreeMap::from([(
                "task_e_123".into(),
                WorkerLease {
                    task_id: "task_e_123".into(),
                    task_url: None,
                    title: "old".into(),
                    environment_id: None,
                    environment_label: None,
                    provider_status: "running".into(),
                    provider_state: ProviderLeaseState::Running,
                    attachment_state: AttachmentState::Connected,
                    provider_updated_at: None,
                    last_observed_unix_ms: 1,
                },
            )]),
        };
        sync_store(
            &mut store,
            &[CloudTask {
                id: "task_e_123".into(),
                url: None,
                title: "new".into(),
                status: "ready".into(),
                updated_at: None,
                environment_id: None,
                environment_label: None,
            }],
        );
        let lease = &store.leases["task_e_123"];
        assert_eq!(lease.provider_state, ProviderLeaseState::Finished);
        assert_eq!(lease.attachment_state, AttachmentState::Connected);
    }

    #[test]
    fn disconnected_terminal_attachment_expires() {
        let mut store = LeaseStore {
            version: STORE_VERSION,
            leases: BTreeMap::from([(
                "task_e_123".into(),
                WorkerLease {
                    task_id: "task_e_123".into(),
                    task_url: None,
                    title: String::new(),
                    environment_id: None,
                    environment_label: None,
                    provider_status: "running".into(),
                    provider_state: ProviderLeaseState::Running,
                    attachment_state: AttachmentState::Disconnected,
                    provider_updated_at: None,
                    last_observed_unix_ms: 1,
                },
            )]),
        };
        sync_store(
            &mut store,
            &[CloudTask {
                id: "task_e_123".into(),
                url: None,
                title: String::new(),
                status: "error".into(),
                updated_at: None,
                environment_id: None,
                environment_label: None,
            }],
        );
        assert_eq!(
            store.leases["task_e_123"].attachment_state,
            AttachmentState::Expired
        );
    }

    #[test]
    fn extracts_task_id_from_cli_output() {
        assert_eq!(
            extract_task_id("Submitted task task_e_example123\n"),
            Some("task_e_example123".into())
        );
        assert_eq!(extract_task_id("no identifier here"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn provider_cli_runs_in_disposable_working_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("fake-codex");
        std::fs::write(
            &command,
            "#!/bin/sh\npwd\nprintf 'sensitive provider log' > error.log\n",
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();

        let output = run_codex(command.to_str().unwrap(), &[]).await.unwrap();
        let provider_cwd = PathBuf::from(output.stdout.trim());
        assert_ne!(provider_cwd, std::env::current_dir().unwrap());
        assert!(!provider_cwd.exists());
    }

    #[test]
    fn parses_exec_without_shell_joining_flags() {
        let args = strings(&[
            "--env",
            "env_123",
            "--branch",
            "feature/cloud",
            "--attempts",
            "2",
            "--",
            "fix",
            "the build",
        ]);
        let parsed = parse_exec_args(&args).unwrap();
        assert_eq!(parsed.environment, "env_123");
        assert_eq!(parsed.branch.as_deref(), Some("feature/cloud"));
        assert_eq!(parsed.attempts, 2);
        assert_eq!(parsed.query, "fix the build");
    }

    #[test]
    fn bootstrap_scripts_do_not_persist_credentials() {
        assert!(!SETUP_SCRIPT.contains("AUTH_KEY"));
        assert!(!SETUP_SCRIPT.contains("ENROLL_TOKEN"));
        assert!(!MAINTENANCE_SCRIPT.contains("AUTH_KEY"));
        assert!(!MAINTENANCE_SCRIPT.contains("ENROLL_TOKEN"));
        assert!(WORKER_SCRIPT.contains("mktemp -d"));
        assert!(WORKER_SCRIPT.contains("exec \"$@\""));
    }

    #[test]
    fn bootstrap_preflights_existing_bundle_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("bundle");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("maintenance.sh"), "keep me").unwrap();

        let error = run_bootstrap(&strings(&["--output", output.to_str().unwrap()])).unwrap_err();
        assert!(error.contains("already exists"));
        assert!(!output.join("setup.sh").exists());
        assert_eq!(
            std::fs::read_to_string(output.join("maintenance.sh")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn bootstrap_writes_complete_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("bundle");
        run_bootstrap(&strings(&["--output", output.to_str().unwrap()])).unwrap();

        for name in ["setup.sh", "maintenance.sh", "run-worker.sh", "README.md"] {
            assert!(output.join(name).is_file(), "missing {name}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(output.join("run-worker.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }
}

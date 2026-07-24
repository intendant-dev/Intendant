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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentState {
    #[default]
    NotRequested,
    Awaiting,
    Connected,
    Disconnected,
    Expired,
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
    /// When the attachment last entered `Connected` (ms since epoch). Drives
    /// the staleness TTL: a broker re-asserts liveness by recording
    /// `connected` again, which restarts this clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_at_unix_ms: Option<u64>,
    /// Last refresh that saw the task actively running (ms since epoch).
    /// With `last_terminal_at_unix_ms` this drives the warmth heuristic:
    /// follow-up turns driven from the task's web UI surface here as
    /// terminal → running → terminal flaps between refreshes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_running_at_unix_ms: Option<u64>,
    /// Last observed live → terminal edge — a completed turn (ms since
    /// epoch). A task first seen already terminal has no known edge time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_terminal_at_unix_ms: Option<u64>,
    /// Completed turns observed by refreshes (terminal edges). Follow-ups
    /// in the same task increment this; first-sight-terminal history does
    /// not.
    #[serde(default)]
    pub turns_observed: u32,
    /// Submitted by `codex-cloud probe`: the task's diff carries a worker
    /// fingerprint that refresh collects automatically once terminal.
    #[serde(default)]
    pub is_probe: bool,
    /// Worker identity parsed from the newest collected fingerprint
    /// (probe tasks, or any pulled diff that happened to carry one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerFingerprint>,
    pub provider_updated_at: Option<String>,
    pub last_observed_unix_ms: u64,
}

/// A worker identity fingerprint parsed from a task diff (see
/// `codex-cloud probe`). Every field is best-effort — the in-container
/// agent authored the file, so parsing is defensive and absence is normal.
/// Two fingerprints with matching `hostname` + `boot_id` + `pid1_start`
/// are the same booted worker; a mismatch across turns of one task is
/// direct evidence of a cold replacement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerFingerprint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid1_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rustc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_kb: Option<u64>,
    /// When this fingerprint was collected locally (ms since epoch).
    #[serde(default)]
    pub collected_at_unix_ms: u64,
}

/// The warm-worker heuristic distilled from the 2026-07-24 runtime
/// findings: an actively running turn holds its worker; a warm same-task
/// worker was measured surviving ~8 minutes between turns with its cargo
/// target tree intact (an identical build ran 68x faster); nothing proves
/// allocation beyond that, and the documented 12-hour window belongs to
/// the *setup cache*, not the worker. These are conservative labels, not
/// guarantees — `Unknown` is the honest default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Warmth {
    LikelyWarm,
    Unknown,
    ColdLikely,
}

/// Measured warm continuity was ~8 minutes; claim warmth just past it.
const WARM_WINDOW_MS: u64 = 10 * 60 * 1000;
/// Beyond the documented setup-cache window nothing warm can remain.
const SETUP_CACHE_WINDOW_MS: u64 = 12 * 60 * 60 * 1000;

pub fn lease_warmth(lease: &WorkerLease, now_ms: u64) -> Warmth {
    if matches!(lease.provider_state, ProviderLeaseState::Running) {
        return Warmth::LikelyWarm;
    }
    // Queued tasks have no worker yet; fall through to history (none →
    // Unknown).
    let last_activity = lease
        .last_terminal_at_unix_ms
        .max(lease.last_running_at_unix_ms);
    match last_activity {
        None => Warmth::Unknown,
        Some(at) => {
            let age = now_ms.saturating_sub(at);
            if age <= WARM_WINDOW_MS {
                Warmth::LikelyWarm
            } else if age <= SETUP_CACHE_WINDOW_MS {
                Warmth::Unknown
            } else {
                Warmth::ColdLikely
            }
        }
    }
}

pub fn warmth_label(warmth: Warmth) -> &'static str {
    match warmth {
        Warmth::LikelyWarm => "warm",
        Warmth::Unknown => "unknown",
        Warmth::ColdLikely => "cold",
    }
}

#[derive(Debug, Clone)]
pub struct SubmitTaskRequest {
    pub environment: String,
    pub branch: Option<String>,
    pub attempts: u16,
    pub title: Option<String>,
    pub prompt: String,
    /// Marks the lease as a `codex-cloud probe` submission so refresh
    /// collects the fingerprint from its diff once terminal.
    pub probe: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubmitTaskResult {
    pub task_id: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub lease: Option<WorkerLease>,
}

/// One provider-side lifecycle edge observed during a refresh: a tracked
/// lease left the live states for a terminal one. A task first seen already
/// terminal is history, not an edge, and is never reported.
#[derive(Debug, Clone, Serialize)]
pub struct TerminalTransition {
    pub task_id: String,
    pub title: String,
    pub task_url: Option<String>,
    pub provider_status: String,
    pub provider_state: ProviderLeaseState,
}

/// What one provider refresh produced. `workers` mirrors the provider's
/// current list window; `tracked_active` are store-only leases with a live
/// attachment (awaiting/connected) that fell out of that window — the point
/// of the attachment split is that liveness outlives the window, so they
/// stay visible here until they expire or are pruned.
#[derive(Debug, Clone, Serialize)]
pub struct RefreshOutcome {
    pub workers: Vec<WorkerLease>,
    pub tracked_active: Vec<WorkerLease>,
    pub cursor: Option<String>,
    pub transitions: Vec<TerminalTransition>,
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
        "pull" => run_pull(&args[1..]).await,
        "probe" => run_probe(&args[1..]).await,
        "followup" | "follow-up" | "message" => run_followup(&args[1..]).await,
        "bootstrap" => run_bootstrap(&args[1..]),
        "attachment" => run_attachment(&args[1..]),
        "prune" => run_prune(&args[1..]),
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
    let result = submit_task(
        &state_path(),
        SubmitTaskRequest {
            environment: parsed.environment,
            branch: parsed.branch,
            attempts: parsed.attempts,
            title: parsed.title,
            prompt: parsed.query,
            probe: false,
        },
    )
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

pub async fn submit_task(
    store_path: &Path,
    request: SubmitTaskRequest,
) -> Result<SubmitTaskResult, String> {
    submit_task_with(&codex_command(), store_path, request).await
}

async fn submit_task_with(
    codex: &str,
    store_path: &Path,
    request: SubmitTaskRequest,
) -> Result<SubmitTaskResult, String> {
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

    let output = run_codex(codex, &cloud_args).await?;
    let task_id = extract_task_id(&format!("{}\n{}", output.stdout, output.stderr));
    let lease = if let Some(task_id) = task_id.as_deref() {
        let _lock = StoreLock::acquire(store_path)?;
        let mut store = load_store(store_path)?;
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
                attached_at_unix_ms: None,
                last_running_at_unix_ms: None,
                last_terminal_at_unix_ms: None,
                turns_observed: 0,
                is_probe: request.probe,
                worker: None,
                provider_updated_at: None,
                last_observed_unix_ms: now_unix_ms(),
            })
            .clone();
        save_store(store_path, &store)?;
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
    let outcome = refresh_leases(
        &state_path(),
        options.environment.as_deref(),
        options.limit,
        options.cursor.as_deref(),
    )
    .await?;
    announce_transitions(&outcome.transitions).await;
    if options.json {
        let payload = serde_json::json!({
            "workers": leases_json(&outcome.workers),
            "tracked_active": leases_json(&outcome.tracked_active),
            "cursor": outcome.cursor,
            "transitions": outcome.transitions,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .map_err(|e| format!("serialize worker leases: {e}"))?
        );
        return Ok(());
    }
    if outcome.workers.is_empty() && outcome.tracked_active.is_empty() {
        println!("No Codex Cloud tasks found.");
        return Ok(());
    }
    if !outcome.workers.is_empty() {
        print_lease_table(&outcome.workers);
    }
    if !outcome.tracked_active.is_empty() {
        println!("\nTracked outside the provider window (live attachment):");
        print_lease_table(&outcome.tracked_active);
    }
    if let Some(cursor) = outcome.cursor.as_deref() {
        println!("\nNext page: intendant codex-cloud list --cursor {cursor}");
    }
    Ok(())
}

fn print_lease_table(leases: &[WorkerLease]) {
    let now = now_unix_ms();
    println!(
        "{:<38}  {:<10}  {:<13}  {:<8}  TITLE",
        "TASK", "PROVIDER", "ATTACHMENT", "WARMTH"
    );
    for lease in leases {
        println!(
            "{:<38}  {:<10}  {:<13}  {:<8}  {}",
            lease.task_id,
            lease.provider_status,
            attachment_label(&lease.attachment_state),
            warmth_label(lease_warmth(lease, now)),
            lease.title
        );
    }
}

/// Serialize leases with the derived warmth attached — computed on the
/// daemon side so no frontend re-implements the heuristic.
pub fn leases_json(leases: &[WorkerLease]) -> Vec<serde_json::Value> {
    let now = now_unix_ms();
    leases
        .iter()
        .map(|lease| {
            let mut value = serde_json::to_value(lease).unwrap_or(serde_json::Value::Null);
            if let Some(map) = value.as_object_mut() {
                map.insert(
                    "warmth".to_string(),
                    serde_json::Value::String(warmth_label(lease_warmth(lease, now)).to_string()),
                );
            }
            value
        })
        .collect()
}

fn transition_title(transition: &TerminalTransition) -> String {
    if transition.title.trim().is_empty() {
        "untitled Codex Cloud task".to_string()
    } else {
        transition.title.clone()
    }
}

/// Human notice + best-effort agenda parking for observed terminal
/// transitions. Parking rides the local daemon's lane when one is up;
/// without a daemon the printed notice is the whole delivery. The store
/// lock already guarantees each edge is observed once, so whoever observes
/// it parks it.
async fn announce_transitions(transitions: &[TerminalTransition]) {
    for transition in transitions {
        eprintln!(
            "[intendant] task {} is now {} — {}",
            transition.task_id,
            transition.provider_status,
            transition_title(transition)
        );
        let (title, body) = agenda_note_for(transition);
        if crate::ctl::park_agenda_note(&title, &body, &["codex-cloud"], "codex-cloud")
            .await
            .is_ok()
        {
            eprintln!("[intendant]   parked on the daemon agenda");
        }
    }
}

/// The agenda note describing one terminal transition — shared between the
/// CLI lane (via ctl) and the daemon lanes (MCP tool, dashboard route).
pub(crate) fn agenda_note_for(transition: &TerminalTransition) -> (String, String) {
    let title = format!(
        "Codex Cloud task {}: {}",
        transition.provider_status,
        transition_title(transition)
    );
    let mut body = format!(
        "Task `{}` reached provider state `{}`.\n",
        transition.task_id, transition.provider_status
    );
    if let Some(url) = transition.task_url.as_deref() {
        body.push('\n');
        body.push_str(url);
        body.push('\n');
    }
    body.push_str(&format!(
        "\nPull the result locally:\n\n    intendant codex-cloud pull {}\n",
        transition.task_id
    ));
    (title, body)
}

/// Refresh the provider's list window into the lease store and report what
/// changed. Read-only toward the provider; the local store is updated (that
/// is the point of a refresh).
pub async fn refresh_leases(
    store_path: &Path,
    environment: Option<&str>,
    limit: u8,
    cursor: Option<&str>,
) -> Result<RefreshOutcome, String> {
    refresh_leases_with(
        &codex_command(),
        store_path,
        environment,
        limit,
        cursor,
        attach_ttl_ms(),
    )
    .await
}

async fn refresh_leases_with(
    codex: &str,
    store_path: &Path,
    environment: Option<&str>,
    limit: u8,
    cursor: Option<&str>,
    attach_ttl_ms: u64,
) -> Result<RefreshOutcome, String> {
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

    // The provider call stays outside the lock; only the read-modify-write
    // of the store is the critical section. Holding the lock across
    // load → sync → save also makes each terminal transition observable by
    // exactly one refresher (the loser reloads post-terminal state).
    let output = run_codex(codex, &cloud_args).await?;
    let response = parse_cloud_list(&output.stdout)?;
    let (transitions, probe_pending) = {
        let _lock = StoreLock::acquire(store_path)?;
        let mut store = load_store(store_path)?;
        let transitions = sync_store(&mut store, &response.tasks, now_unix_ms(), attach_ttl_ms);
        save_store(store_path, &store)?;
        // Probe fingerprints ride the diff; collect (outside the lock) for
        // window tasks that reached terminal without one.
        let probe_pending: Vec<String> = response
            .tasks
            .iter()
            .filter_map(|task| store.leases.get(&task.id))
            .filter(|lease| {
                lease.is_probe && lease.provider_state.is_terminal() && lease.worker.is_none()
            })
            .map(|lease| lease.task_id.clone())
            .collect();
        (transitions, probe_pending)
    };
    for task_id in &probe_pending {
        collect_probe_fingerprint(codex, store_path, task_id).await;
    }
    let store = load_store(store_path)?;
    let workers = response
        .tasks
        .iter()
        .filter_map(|task| store.leases.get(&task.id).cloned())
        .collect();
    let mut tracked_active: Vec<WorkerLease> = store
        .leases
        .values()
        .filter(|lease| !response.tasks.iter().any(|task| task.id == lease.task_id))
        .filter(|lease| {
            matches!(
                lease.attachment_state,
                AttachmentState::Awaiting | AttachmentState::Connected
            )
        })
        .cloned()
        .collect();
    tracked_active.sort_by_key(|lease| std::cmp::Reverse(lease.last_observed_unix_ms));
    Ok(RefreshOutcome {
        workers,
        tracked_active,
        cursor: response.cursor,
        transitions,
    })
}

/// Read the tracked leases without touching the provider (dashboard first
/// paint, scripting). Newest-observed first.
pub fn cached_leases(store_path: &Path) -> Result<Vec<WorkerLease>, String> {
    let store = load_store(store_path)?;
    let mut leases: Vec<WorkerLease> = store.leases.into_values().collect();
    leases.sort_by_key(|lease| std::cmp::Reverse(lease.last_observed_unix_ms));
    Ok(leases)
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
    // structured list first; a tracked lease outside that window is served
    // from the store (its attachment state is ours alone), and only an
    // entirely unknown task falls back to the upstream human-readable status.
    let store_path = state_path();
    let outcome = refresh_leases(&store_path, None, 20, None).await?;
    announce_transitions(&outcome.transitions).await;
    let lease = match outcome
        .workers
        .iter()
        .find(|lease| lease.task_id == task_id)
    {
        Some(lease) => Some(lease.clone()),
        None => cached_leases(&store_path)?
            .into_iter()
            .find(|lease| lease.task_id == task_id)
            .inspect(|_| {
                eprintln!(
                    "[intendant] task is outside the newest provider window; provider fields may be stale"
                );
            }),
    };
    if let Some(lease) = lease {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&lease)
                    .map_err(|e| format!("serialize worker lease: {e}"))?
            );
        } else {
            print_lease(&lease);
        }
        return Ok(());
    }

    if json {
        return Err(
            "task is not tracked and was not in the newest 20 Cloud tasks; the upstream `codex cloud status` command has no JSON mode"
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

/// The canned probe prompt: the in-container agent reports the worker's
/// runtime identity as one added file, so the fingerprint travels in the
/// diff — the only channel the CLI surface reliably exposes. Systematizes
/// the 2026-07-24 runtime-findings methodology (worker isolation, cache
/// materialization, cold-replacement detection).
const PROBE_PROMPT: &str = "Create exactly one new file at ._intendant-probe/fingerprint.json and change nothing else. Its content must be a single line of minified JSON with exactly these keys: \"intendant_probe\": 1, \"hostname\": the output of `hostname`, \"boot_id\": the contents of /proc/sys/kernel/random/boot_id, \"pid1_start\": field 22 of /proc/1/stat as a string, \"unix_ms\": the output of `date +%s%3N` as a number, \"git_rev\": the output of `git rev-parse HEAD`, \"rustc\": the output of `rustc --version` (or null if unavailable), \"cpus\": the output of `nproc` as a number, \"mem_kb\": the MemTotal number from /proc/meminfo. Do not run builds or tests, do not modify any other file, do not create branches, and finish immediately after writing the file.";

async fn run_probe(args: &[String]) -> Result<(), String> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        println!("Usage: intendant codex-cloud probe --env ENV_ID [--title TITLE]");
        println!(
            "Submits a canned diagnostic task whose diff carries the worker's runtime fingerprint (hostname, boot id, PID 1 start, toolchain). Refresh collects it automatically once the task finishes; matching fingerprints identify one booted worker, mismatches across turns prove a cold replacement."
        );
        return Ok(());
    }
    let mut environment: Option<String> = None;
    let mut title: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--env" => {
                i += 1;
                environment = Some(required_value(args, i, "--env")?);
            }
            "--title" => {
                i += 1;
                title = Some(required_value(args, i, "--title")?);
            }
            other => return Err(format!("unknown probe flag {other}")),
        }
        i += 1;
    }
    let environment = environment.ok_or_else(|| "probe requires --env <ENV_ID>".to_string())?;
    let result = submit_task(
        &state_path(),
        SubmitTaskRequest {
            environment,
            branch: None,
            attempts: 1,
            title: Some(title.unwrap_or_else(|| "Intendant worker probe".to_string())),
            prompt: PROBE_PROMPT.to_string(),
            probe: true,
        },
    )
    .await?;
    match result.task_id {
        Some(task_id) => {
            println!("Probe submitted as {task_id}.");
            println!(
                "Run `intendant codex-cloud list` after it finishes; the worker fingerprint is collected from the diff automatically."
            );
        }
        None => println!(
            "Probe submitted, but no task id was visible in the CLI output; run `intendant codex-cloud list` to synchronize."
        ),
    }
    Ok(())
}

/// Find the probe fingerprint in a unified diff: an added line whose JSON
/// object carries `"intendant_probe"`. Defensive — the in-container agent
/// authored the file, so any shape drift yields `None`, never an error.
pub(crate) fn parse_probe_fingerprint(diff_text: &str) -> Option<WorkerFingerprint> {
    for line in diff_text.lines() {
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        let trimmed = added.trim();
        if !trimmed.starts_with('{') || !trimmed.contains("\"intendant_probe\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if value.get("intendant_probe").is_none() {
            continue;
        }
        let text = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let num = |key: &str| value.get(key).and_then(serde_json::Value::as_u64);
        return Some(WorkerFingerprint {
            hostname: text("hostname"),
            boot_id: text("boot_id"),
            pid1_start: text("pid1_start").or_else(|| num("pid1_start").map(|n| n.to_string())),
            unix_ms: num("unix_ms"),
            git_rev: text("git_rev"),
            rustc: text("rustc"),
            cpus: num("cpus"),
            mem_kb: num("mem_kb"),
            collected_at_unix_ms: now_unix_ms(),
        });
    }
    None
}

/// Best-effort: fetch a probe task's diff and record its fingerprint.
/// Failures leave the lease unfingerprinted for the next refresh to retry
/// (bounded: only tasks still in the provider window are attempted).
async fn collect_probe_fingerprint(codex: &str, store_path: &Path, task_id: &str) {
    let Ok(diff) = run_codex(
        codex,
        &["cloud".to_string(), "diff".to_string(), task_id.to_string()],
    )
    .await
    else {
        return;
    };
    let Some(fingerprint) = parse_probe_fingerprint(&diff.stdout) else {
        return;
    };
    record_worker_fingerprint(store_path, task_id, fingerprint);
}

/// Attach a fingerprint to a tracked lease (probe collection, or an
/// opportunistic parse from a pulled diff). Silent best-effort.
fn record_worker_fingerprint(store_path: &Path, task_id: &str, fingerprint: WorkerFingerprint) {
    let Ok(_lock) = StoreLock::acquire(store_path) else {
        return;
    };
    let Ok(mut store) = load_store(store_path) else {
        return;
    };
    if let Some(lease) = store.leases.get_mut(task_id) {
        lease.worker = Some(fingerprint);
        let _ = save_store(store_path, &store);
    }
}

// ── Follow-ups over the provider's private web backend ─────────────────
//
// The product supports follow-up turns on a Cloud task, but the public
// Codex CLI has no verb for them (exec/status/list/apply/diff only;
// upstream issue #24777 is an unimplemented proposal). The 2026-07-24
// reverse-engineering validation proved the web UI's own backend accepts
// a browser-free follow-up POST authenticated by the Codex CLI's stored
// ChatGPT login. This lane rides that: deliberately narrow, serialized
// per task, and fail-closed — the endpoint and schemas are private, so
// 404/409/422 or any unrecognized shape is a compatibility break to
// surface, never retry around. Prefer the official command the moment
// upstream ships one. The two credential values (bearer token, account
// id) never reach stdout, errors, logs, or receipts.

const DEFAULT_WHAM_BACKEND: &str = "https://chatgpt.com/backend-api";

fn wham_backend() -> String {
    std::env::var("INTENDANT_CODEX_CLOUD_BACKEND")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_WHAM_BACKEND.to_string())
}

fn wham_user_agent() -> String {
    format!("intendant/{}", env!("CARGO_PKG_VERSION"))
}

/// The Codex CLI's stored ChatGPT login, read from its own `auth.json`.
/// Both fields are credentials: deliberately not Serialize, and Debug is
/// hand-written to redact them.
pub(crate) struct CodexAuth {
    access_token: String,
    account_id: String,
}

impl std::fmt::Debug for CodexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAuth")
            .field("access_token", &"<redacted>")
            .field("account_id", &"<redacted>")
            .finish()
    }
}

/// Upstream's own state-home convention (`CODEX_HOME`, default `~/.codex`).
fn codex_home() -> PathBuf {
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".codex")
}

fn load_codex_auth(codex_home: &Path) -> Result<CodexAuth, String> {
    let path = codex_home.join("auth.json");
    let bytes = std::fs::read(&path).map_err(|e| {
        format!(
            "read Codex CLI login {}: {e}; follow-ups reuse the `codex login` ChatGPT session",
            path.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse Codex CLI login {}: {e}", path.display()))?;
    let tokens = value
        .get("tokens")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            format!(
                "{} has no ChatGPT login tokens (API-key-only auth cannot drive Cloud follow-ups); run `codex login`",
                path.display()
            )
        })?;
    let access_token = tokens
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| format!("{} has no access token; run `codex login`", path.display()))?
        .to_string();
    let account_id = tokens
        .get("account_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            tokens
                .get("id_token")
                .and_then(serde_json::Value::as_str)
                .and_then(chatgpt_account_id_from_id_token)
        })
        .ok_or_else(|| {
            format!(
                "{} carries no ChatGPT account id; re-run `codex login` with a current Codex CLI",
                path.display()
            )
        })?;
    Ok(CodexAuth {
        access_token,
        account_id,
    })
}

/// Older `auth.json` files carry the account id only inside the OpenID
/// id_token's `https://api.openai.com/auth` claim. The JWT is our own
/// local login file read for a header value — decoded, never verified.
fn chatgpt_account_id_from_id_token(id_token: &str) -> Option<String> {
    use base64::Engine as _;
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim())
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
}

/// Observed wham id shapes: `task_e_…` tasks, `task_e_…~assttrn_e_…`
/// assistant turns, `…~usrtrn_e_…` user turns. The follow-up POST demands
/// the latest *assistant* turn id — the validation run got HTTP 404 for
/// the predecessor user-turn id and HTTP 200 for the assistant turn.
fn is_assistant_turn_id(turn_id: &str) -> bool {
    turn_id.contains("~assttrn")
}

/// The exact recovered wire shape. `run_environment_in_qa_mode` is pinned
/// to `false` — the only value the validation exercised.
fn followup_body(task_id: &str, turn_id: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "follow_up": {
            "task_id": task_id,
            "turn_id": turn_id,
            "run_environment_in_qa_mode": false,
        },
        "input_items": [{
            "type": "message",
            "role": "user",
            "content": [{ "content_type": "text", "text": prompt }],
        }],
    })
}

/// Best-effort `current_turn_id` from the private task-detail response,
/// checked at the two shapes observed (top level, nested `task`). Absence
/// is a compatibility break the caller surfaces.
fn current_turn_id(detail: &serde_json::Value) -> Option<String> {
    [
        detail.get("current_turn_id"),
        detail
            .get("task")
            .and_then(|task| task.get("current_turn_id")),
    ]
    .into_iter()
    .find_map(|candidate| {
        candidate
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn collect_strings<'v>(value: &'v serde_json::Value, out: &mut Vec<&'v str>) {
    match value {
        serde_json::Value::String(text) => out.push(text),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_strings(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                collect_strings(item, out);
            }
        }
        _ => {}
    }
}

/// HTTP 200 alone is not success: the response must still reference the
/// task (linkage), else the private schema changed under us. Fresh turn
/// ids found in the response become the receipt (best-effort — their
/// absence is tolerated, a broken linkage is not).
fn validate_followup_response(
    task_id: &str,
    parent_turn_id: &str,
    body: &serde_json::Value,
) -> Result<Vec<String>, String> {
    let mut strings = Vec::new();
    collect_strings(body, &mut strings);
    if !strings.iter().any(|text| text.contains(task_id)) {
        return Err(format!(
            "the follow-up POST returned HTTP 200 but the response no longer references task {task_id} — treat this as a private-schema compatibility break and verify the task in the web UI"
        ));
    }
    let mut new_turn_ids = Vec::new();
    for text in strings {
        let looks_like_turn =
            text.starts_with("task_") && (text.contains("~assttrn") || text.contains("~usrtrn"));
        if looks_like_turn && text != parent_turn_id && !new_turn_ids.iter().any(|id| id == text) {
            new_turn_ids.push(text.to_string());
        }
    }
    Ok(new_turn_ids)
}

/// Error framing for the private backend: 401/403 is login freshness with
/// a local fix; 404/409/422 on known-good input is how a private schema
/// announces a compatibility break — surfaced, never retried around.
fn wham_error(status: u16, url: &str, body: &str) -> String {
    let snippet: String = body.trim().chars().take(200).collect();
    let detail = if snippet.is_empty() {
        String::new()
    } else {
        format!(": {snippet}")
    };
    match status {
        401 | 403 => format!(
            "the Codex login was not accepted by {url} (HTTP {status}){detail}; refresh it with `codex login` (or any `codex cloud` command), then retry"
        ),
        404 | 409 | 422 => format!(
            "{url} answered HTTP {status}{detail} — on a valid idle task this means the private follow-up endpoint changed shape (compatibility break); use the web UI and check whether upstream shipped an official follow-up command"
        ),
        _ => format!("{url} answered HTTP {status}{detail}"),
    }
}

async fn wham_get_json(
    client: &reqwest::Client,
    url: &str,
    auth: &CodexAuth,
) -> Result<serde_json::Value, String> {
    let response = client
        .get(url)
        .bearer_auth(&auth.access_token)
        .header("chatgpt-account-id", &auth.account_id)
        .header(reqwest::header::USER_AGENT, wham_user_agent())
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(wham_error(status.as_u16(), url, &text));
    }
    serde_json::from_str(&text).map_err(|e| format!("parse {url} response: {e}"))
}

/// Receipt for an accepted follow-up. The credentials that obtained it are
/// deliberately absent.
#[derive(Debug, Serialize)]
pub struct FollowupReceipt {
    pub task_id: String,
    /// The assistant turn the follow-up chained onto.
    pub parent_turn_id: String,
    /// Fresh turn ids referenced by the response (best-effort receipt).
    pub new_turn_ids: Vec<String>,
    pub task_url: String,
    /// Terminal transitions observed by the pre-send idle refresh; callers
    /// announce/park them like any refresh's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub transitions: Vec<TerminalTransition>,
}

/// A follow-up may only target an idle task: the provider runs one turn at
/// a time and the posture is to reject rather than pile on. Provider truth
/// comes from the public CLI — a fresh list refresh for window tasks, the
/// upstream status verb's human line for a task outside the window.
async fn ensure_task_idle(
    codex: &str,
    store_path: &Path,
    task_id: &str,
    attach_ttl_ms: u64,
) -> Result<Vec<TerminalTransition>, String> {
    let outcome = refresh_leases_with(codex, store_path, None, 20, None, attach_ttl_ms).await?;
    if let Some(lease) = outcome
        .workers
        .iter()
        .find(|lease| lease.task_id == task_id)
    {
        return if lease.provider_state.is_terminal() {
            Ok(outcome.transitions)
        } else {
            Err(format!(
                "task {task_id} still has an active turn (provider says {}); follow-ups are serialized per task — wait for it to finish, then retry",
                lease.provider_status
            ))
        };
    }
    let status = run_codex(
        codex,
        &["cloud".into(), "status".into(), task_id.to_string()],
    )
    .await?;
    let text = format!("{}\n{}", status.stdout, status.stderr).to_ascii_lowercase();
    let mut tokens = text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_');
    if tokens.any(|token| {
        matches!(
            token,
            "ready"
                | "completed"
                | "complete"
                | "succeeded"
                | "error"
                | "failed"
                | "cancelled"
                | "canceled"
        )
    }) {
        Ok(outcome.transitions)
    } else {
        Err(format!(
            "could not establish that task {task_id} is idle from `codex cloud status` (it is outside the newest list window); retry once the task reports READY"
        ))
    }
}

fn followup_lock_path(store_path: &Path, task_id: &str) -> PathBuf {
    let mut name = store_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".followup-{task_id}.lock"));
    store_path.with_file_name(name)
}

/// Edge resolver: Codex login, backend URL, codex command, and TTL come
/// from the environment; everything below takes them as parameters.
pub async fn follow_up_task(
    store_path: &Path,
    task_id: &str,
    prompt: &str,
) -> Result<FollowupReceipt, String> {
    let auth = load_codex_auth(&codex_home())?;
    follow_up_task_with(
        &codex_command(),
        &wham_backend(),
        &auth,
        store_path,
        task_id,
        prompt,
        attach_ttl_ms(),
    )
    .await
}

async fn follow_up_task_with(
    codex: &str,
    backend: &str,
    auth: &CodexAuth,
    store_path: &Path,
    task_id: &str,
    prompt: &str,
    attach_ttl_ms: u64,
) -> Result<FollowupReceipt, String> {
    let task_id = task_id.trim();
    if !(task_id.starts_with("task_")
        && task_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'))
    {
        return Err(format!(
            "'{task_id}' does not look like a Codex Cloud task id (task_…)"
        ));
    }
    if prompt.trim().is_empty() {
        return Err("follow-up prompt cannot be empty".to_string());
    }

    // One in-flight follow-up per task, machine-wide (blocking sidecar
    // lock; the OS releases it if the holder dies). The task id is
    // filename-safe — the alphabet was just validated.
    let _followup_lock = StoreLock::acquire_path(&followup_lock_path(store_path, task_id))?;

    let transitions = ensure_task_idle(codex, store_path, task_id, attach_ttl_ms).await?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    // Resolve the latest assistant turn immediately before sending: stale
    // turn ids and user-turn ids both 404 on the follow-up POST.
    let detail_url = format!("{backend}/wham/tasks/{task_id}");
    let detail = wham_get_json(&client, &detail_url, auth).await?;
    let turn_id = current_turn_id(&detail).ok_or_else(|| {
        format!(
            "the task detail from {detail_url} carries no current_turn_id — the private follow-up schema changed (compatibility break); use the web UI"
        )
    })?;
    if !is_assistant_turn_id(&turn_id) {
        return Err(format!(
            "the task's current turn ({turn_id}) is not an assistant turn; the task is likely still processing — wait for READY, then retry"
        ));
    }

    let post_url = format!("{backend}/wham/tasks");
    let response = client
        .post(&post_url)
        .bearer_auth(&auth.access_token)
        .header("chatgpt-account-id", &auth.account_id)
        .header(reqwest::header::USER_AGENT, wham_user_agent())
        .json(&followup_body(task_id, &turn_id, prompt))
        .send()
        .await
        .map_err(|e| format!("POST {post_url}: {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(wham_error(status.as_u16(), &post_url, &text));
    }
    let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        format!(
            "the follow-up POST returned HTTP {status} with a non-JSON body ({e}) — compatibility break; verify the task in the web UI"
        )
    })?;
    let new_turn_ids = validate_followup_response(task_id, &turn_id, &body)?;

    // The provider accepted a new turn: record the running edge now so
    // warmth stays honest and the next refresh's terminal edge counts the
    // turn even if no refresh happens to catch it mid-run.
    {
        let _lock = StoreLock::acquire(store_path)?;
        let mut store = load_store(store_path)?;
        if let Some(lease) = store.leases.get_mut(task_id) {
            lease.provider_status = "running".to_string();
            lease.provider_state = ProviderLeaseState::Running;
            lease.last_running_at_unix_ms = Some(now_unix_ms());
            lease.last_observed_unix_ms = now_unix_ms();
            save_store(store_path, &store)?;
        }
    }

    Ok(FollowupReceipt {
        task_id: task_id.to_string(),
        parent_turn_id: turn_id,
        new_turn_ids,
        task_url: format!("https://chatgpt.com/codex/tasks/{task_id}"),
        transitions,
    })
}

async fn run_followup(args: &[String]) -> Result<(), String> {
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        print_followup_help();
        return Ok(());
    }
    let mut task_id: Option<String> = None;
    let mut message: Option<String> = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-m" | "--message" => {
                i += 1;
                message = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--message requires a prompt argument")?,
                );
            }
            "--json" => json = true,
            other if task_id.is_none() && !other.starts_with('-') => {
                task_id = Some(other.to_string());
            }
            other => return Err(format!("unknown followup argument {other}")),
        }
        i += 1;
    }
    let task_id = task_id.ok_or("followup requires a Codex Cloud task id")?;
    let prompt = match message {
        Some(message) => message,
        None => {
            use std::io::IsTerminal as _;
            if std::io::stdin().is_terminal() {
                eprintln!("[intendant] reading the follow-up prompt from stdin — end with Ctrl-D");
            }
            tokio::task::spawn_blocking(|| std::io::read_to_string(std::io::stdin()))
                .await
                .map_err(|e| format!("read stdin: {e}"))?
                .map_err(|e| format!("read stdin: {e}"))?
        }
    };

    let receipt = follow_up_task(&state_path(), &task_id, &prompt).await?;
    announce_transitions(&receipt.transitions).await;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&receipt)
                .map_err(|e| format!("serialize follow-up receipt: {e}"))?
        );
    } else {
        println!("follow-up accepted: {}", receipt.task_id);
        println!("  parent turn: {}", receipt.parent_turn_id);
        if !receipt.new_turn_ids.is_empty() {
            println!("  new turns: {}", receipt.new_turn_ids.join(", "));
        }
        println!("  url: {}", receipt.task_url);
        println!("  watch: intendant codex-cloud status {}", receipt.task_id);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct PullOutcome {
    pub task_id: String,
    pub branch: String,
    pub worktree: PathBuf,
    /// Paths left with conflict markers by the three-way apply. Empty means
    /// the diff applied cleanly.
    pub conflicts: Vec<String>,
    /// Worker fingerprint opportunistically parsed from the pulled diff
    /// (present when the task wrote one — probe tasks always do).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerFingerprint>,
}

async fn run_pull(args: &[String]) -> Result<(), String> {
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        println!(
            "Usage:\n  intendant codex-cloud pull <TASK_ID> [--attempt N] [--branch NAME] [--dir PATH] [--repo PATH] [--json]"
        );
        println!(
            "Fetches the task's diff through the Codex CLI (in a disposable directory) and applies it onto a fresh branch in a new git worktree — the upstream CLI never runs inside your repository."
        );
        return Ok(());
    }
    let task_id = args[0].clone();
    let mut attempt: Option<u16> = None;
    let mut branch: Option<String> = None;
    let mut dir: Option<PathBuf> = None;
    let mut repo: Option<PathBuf> = None;
    let mut json = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--attempt" => {
                i += 1;
                attempt = Some(
                    required_value(args, i, "--attempt")?
                        .parse()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| "--attempt must be a positive integer".to_string())?,
                );
            }
            "--branch" => {
                i += 1;
                branch = Some(required_value(args, i, "--branch")?);
            }
            "--dir" => {
                i += 1;
                dir = Some(PathBuf::from(required_value(args, i, "--dir")?));
            }
            "--repo" => {
                i += 1;
                repo = Some(PathBuf::from(required_value(args, i, "--repo")?));
            }
            "--json" => json = true,
            other => return Err(format!("unknown pull flag {other}")),
        }
        i += 1;
    }
    let outcome = pull_task(
        &codex_command(),
        repo.as_deref().unwrap_or_else(|| Path::new(".")),
        &task_id,
        attempt,
        branch.as_deref(),
        dir.as_deref(),
    )
    .await?;
    if let Some(worker) = outcome.worker.clone() {
        record_worker_fingerprint(&state_path(), &outcome.task_id, worker);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome)
                .map_err(|e| format!("serialize pull outcome: {e}"))?
        );
        return Ok(());
    }
    println!(
        "Pulled {} onto branch {} in {}",
        outcome.task_id,
        outcome.branch,
        outcome.worktree.display()
    );
    if outcome.conflicts.is_empty() {
        println!("The diff applied cleanly (uncommitted). Next:");
        println!("  cd {}", outcome.worktree.display());
        println!("  git status && git diff   # review");
        println!("  git add -A && git commit # land it your usual way");
    } else {
        println!("The three-way apply left conflicts to resolve:");
        for path in &outcome.conflicts {
            println!("  {path}");
        }
        println!("  cd {}", outcome.worktree.display());
    }
    Ok(())
}

/// Fetch a Cloud task's diff and apply it onto a fresh branch in a new git
/// worktree. The Codex CLI runs in its disposable directory as always (its
/// `error.log` habit is why it is never run inside a repository); only our
/// own `git` touches the checkout. Nothing is committed — the result is a
/// reviewable worktree.
async fn pull_task(
    codex: &str,
    repo_hint: &Path,
    task_id: &str,
    attempt: Option<u16>,
    branch_override: Option<&str>,
    dir_override: Option<&Path>,
) -> Result<PullOutcome, String> {
    if task_id.trim().is_empty() || task_id.starts_with('-') {
        return Err("pull requires a Codex Cloud task id".to_string());
    }
    let top = run_git(repo_hint, &["rev-parse", "--show-toplevel"]).await?;
    let repo_root = PathBuf::from(top.stdout.trim());
    if repo_root.as_os_str().is_empty() {
        return Err(format!(
            "{} is not inside a git repository",
            repo_hint.display()
        ));
    }

    let mut diff_args = vec!["cloud".to_string(), "diff".to_string(), task_id.to_string()];
    if let Some(attempt) = attempt {
        diff_args.push("--attempt".to_string());
        diff_args.push(attempt.to_string());
    }
    let diff = run_codex(codex, &diff_args).await?;
    if diff.stdout.trim().is_empty() {
        return Err(format!(
            "task {task_id} produced an empty diff (attempt {})",
            attempt.unwrap_or(1)
        ));
    }
    let worker = parse_probe_fingerprint(&diff.stdout);

    let branch = branch_override
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("codex-cloud/{task_id}"));
    if run_git(
        &repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
    .is_ok()
    {
        return Err(format!(
            "branch {branch} already exists; pass --branch for a fresh name"
        ));
    }

    let worktree_dir = dir_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo_root.join(".intendant").join("worktrees").join(&branch));
    if worktree_dir.exists() {
        return Err(format!(
            "{} already exists; pass --dir for a fresh location",
            worktree_dir.display()
        ));
    }
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create worktree parent {}: {e}", parent.display()))?;
    }
    let worktree_str = worktree_dir
        .to_str()
        .ok_or_else(|| "worktree path is not valid UTF-8".to_string())?
        .to_string();
    run_git(
        &repo_root,
        &["worktree", "add", "-b", &branch, &worktree_str, "HEAD"],
    )
    .await?;

    let patch = tempfile::Builder::new()
        .prefix("intendant-codex-cloud-pull-")
        .suffix(".patch")
        .tempfile()
        .map_err(|e| format!("stage patch file: {e}"))?;
    std::fs::write(patch.path(), diff.stdout.as_bytes())
        .map_err(|e| format!("write patch file: {e}"))?;
    let patch_str = patch
        .path()
        .to_str()
        .ok_or_else(|| "patch path is not valid UTF-8".to_string())?
        .to_string();

    let applied = run_git(
        &worktree_dir,
        &["apply", "--3way", "--whitespace=nowarn", &patch_str],
    )
    .await;
    let conflicts = match applied {
        Ok(_) => Vec::new(),
        Err(apply_error) => {
            let unmerged = run_git(&worktree_dir, &["diff", "--name-only", "--diff-filter=U"])
                .await
                .map(|out| {
                    out.stdout
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if unmerged.is_empty() {
                // Nothing applied at all: remove the worktree and branch so a
                // failed pull leaves no residue.
                let _ = run_git(
                    &repo_root,
                    &["worktree", "remove", "--force", &worktree_str],
                )
                .await;
                let _ = run_git(&repo_root, &["branch", "-D", &branch]).await;
                return Err(format!("apply the task diff: {apply_error}"));
            }
            unmerged
        }
    };

    Ok(PullOutcome {
        task_id: task_id.to_string(),
        branch,
        worktree: worktree_dir,
        conflicts,
        worker,
    })
}

/// Run `git` with captured output. Unlike the provider CLI, git runs where
/// the work is — in the repository or the new worktree.
async fn run_git(cwd: &Path, args: &[&str]) -> Result<CommandOutput, String> {
    let output = crate::platform::spawn_command("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("run git: {e}"))?;
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
            format!("git {} exited with {}", args.join(" "), output.status)
        } else {
            format!(
                "git {} exited with {}: {detail}",
                args.join(" "),
                output.status
            )
        })
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
    let store_path = state_path();
    let _lock = StoreLock::acquire(&store_path)?;
    let mut store = load_store(&store_path)?;
    let label = {
        let lease = store.leases.get_mut(&args[0]).ok_or_else(|| {
            format!(
                "unknown worker lease {}; run `intendant codex-cloud list`",
                args[0]
            )
        })?;
        lease.attachment_state = state;
        // `connected` (re-)starts the staleness clock; `none` resets the
        // whole attachment record. Other states keep the history.
        match lease.attachment_state {
            AttachmentState::Connected => lease.attached_at_unix_ms = Some(now_unix_ms()),
            AttachmentState::NotRequested => lease.attached_at_unix_ms = None,
            _ => {}
        }
        lease.last_observed_unix_ms = now_unix_ms();
        attachment_label(&lease.attachment_state)
    };
    save_store(&store_path, &store)?;
    println!("{} attachment={label}", args[0]);
    Ok(())
}

fn run_prune(args: &[String]) -> Result<(), String> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        println!("Usage: intendant codex-cloud prune [--days N] [--all] [--json]");
        println!("Drops terminal leases with no live attachment: older than N days (default 7), or every one with --all.");
        return Ok(());
    }
    let mut days: Option<u64> = None;
    let mut all = false;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => {
                i += 1;
                days = Some(
                    required_value(args, i, "--days")?
                        .parse()
                        .map_err(|_| "--days must be a non-negative integer".to_string())?,
                );
            }
            "--all" => all = true,
            "--json" => json = true,
            other => return Err(format!("unknown prune flag {other}")),
        }
        i += 1;
    }
    if all && days.is_some() {
        return Err("pass --days or --all, not both".to_string());
    }
    let older_than_ms = if all {
        None
    } else {
        Some(days.unwrap_or(7).saturating_mul(24 * 60 * 60 * 1000))
    };
    let outcome = prune_leases(&state_path(), older_than_ms)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome)
                .map_err(|e| format!("serialize prune outcome: {e}"))?
        );
    } else {
        println!(
            "Pruned {} lease(s); {} kept.",
            outcome.removed.len(),
            outcome.kept
        );
        for task_id in &outcome.removed {
            println!("  removed {task_id}");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct PruneOutcome {
    pub removed: Vec<String>,
    pub kept: usize,
}

/// Drop terminal leases with no live attachment. `older_than_ms: None`
/// removes them regardless of age. Non-terminal provider states and
/// awaiting/connected attachments are never pruned — disconnect or expire
/// them first.
pub fn prune_leases(store_path: &Path, older_than_ms: Option<u64>) -> Result<PruneOutcome, String> {
    let now = now_unix_ms();
    let _lock = StoreLock::acquire(store_path)?;
    let mut store = load_store(store_path)?;
    let mut removed = Vec::new();
    store.leases.retain(|task_id, lease| {
        let prunable = lease.provider_state.is_terminal()
            && !matches!(
                lease.attachment_state,
                AttachmentState::Awaiting | AttachmentState::Connected
            )
            && older_than_ms
                .is_none_or(|cutoff| now.saturating_sub(lease.last_observed_unix_ms) > cutoff);
        if prunable {
            removed.push(task_id.clone());
        }
        !prunable
    });
    if !removed.is_empty() {
        save_store(store_path, &store)?;
    }
    Ok(PruneOutcome {
        removed,
        kept: store.leases.len(),
    })
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

/// Fold one provider list window into the store and age every attachment.
/// Returns the live → terminal edges this sync observed; callers hold the
/// store lock across load → sync → save, so each edge is reported by
/// exactly one refresher.
fn sync_store(
    store: &mut LeaseStore,
    tasks: &[CloudTask],
    now_ms: u64,
    attach_ttl_ms: u64,
) -> Vec<TerminalTransition> {
    let mut transitions = Vec::new();
    for task in tasks {
        let provider_state = ProviderLeaseState::from_codex_status(&task.status);
        let existing = store.leases.get(&task.id).cloned();

        let terminal_edge = provider_state.is_terminal()
            && existing
                .as_ref()
                .is_some_and(|lease| !lease.provider_state.is_terminal());
        let mut last_running_at = existing.as_ref().and_then(|l| l.last_running_at_unix_ms);
        let mut last_terminal_at = existing.as_ref().and_then(|l| l.last_terminal_at_unix_ms);
        let mut turns_observed = existing.as_ref().map(|l| l.turns_observed).unwrap_or(0);
        if matches!(provider_state, ProviderLeaseState::Running) {
            last_running_at = Some(now_ms);
        }
        if terminal_edge {
            last_terminal_at = Some(now_ms);
            turns_observed += 1;
        }

        if terminal_edge {
            transitions.push(TerminalTransition {
                task_id: task.id.clone(),
                title: if task.title.trim().is_empty() {
                    existing
                        .as_ref()
                        .map(|lease| lease.title.clone())
                        .unwrap_or_default()
                } else {
                    task.title.clone()
                },
                task_url: task
                    .url
                    .clone()
                    .or_else(|| existing.as_ref().and_then(|lease| lease.task_url.clone())),
                provider_status: task.status.clone(),
                provider_state: provider_state.clone(),
            });
        }

        // Provider fields refresh wholesale, but an empty/null provider value
        // never erases a locally-known one: the real list shape returns
        // `environment_id: null`, and titles can arrive empty while the
        // submit-time title is still the best label we have.
        let keep = |incoming: Option<String>, current: fn(&WorkerLease) -> Option<String>| {
            incoming.or_else(|| existing.as_ref().and_then(current))
        };
        let title = if task.title.trim().is_empty() {
            existing
                .as_ref()
                .map(|lease| lease.title.clone())
                .unwrap_or_default()
        } else {
            task.title.clone()
        };

        let existing_attachment = existing
            .as_ref()
            .map(|lease| lease.attachment_state.clone())
            .unwrap_or_default();
        // A pre-TTL store can hold `connected` without a timestamp; its
        // staleness clock starts at this sync instead of expiring it on
        // sight.
        let attached_at = match existing_attachment {
            AttachmentState::Connected => existing
                .as_ref()
                .and_then(|lease| lease.attached_at_unix_ms)
                .or(Some(now_ms)),
            _ => existing
                .as_ref()
                .and_then(|lease| lease.attached_at_unix_ms),
        };
        let attachment_state = next_attachment_state(
            existing_attachment,
            attached_at,
            provider_state.is_terminal(),
            now_ms,
            attach_ttl_ms,
        );

        store.leases.insert(
            task.id.clone(),
            WorkerLease {
                task_id: task.id.clone(),
                task_url: keep(task.url.clone(), |lease| lease.task_url.clone()),
                title,
                environment_id: keep(task.environment_id.clone(), |lease| {
                    lease.environment_id.clone()
                }),
                environment_label: keep(task.environment_label.clone(), |lease| {
                    lease.environment_label.clone()
                }),
                provider_status: task.status.clone(),
                provider_state,
                attachment_state,
                attached_at_unix_ms: attached_at,
                last_running_at_unix_ms: last_running_at,
                last_terminal_at_unix_ms: last_terminal_at,
                turns_observed,
                is_probe: existing.as_ref().is_some_and(|lease| lease.is_probe),
                worker: existing.as_ref().and_then(|lease| lease.worker.clone()),
                provider_updated_at: keep(task.updated_at.clone(), |lease| {
                    lease.provider_updated_at.clone()
                }),
                last_observed_unix_ms: now_ms,
            },
        );
    }

    // Store-only leases age too: a live attachment on a task that fell out
    // of the provider window is exactly the state most at risk of rotting
    // as `connected` forever.
    for lease in store.leases.values_mut() {
        if tasks.iter().any(|task| task.id == lease.task_id) {
            continue;
        }
        if lease.attachment_state == AttachmentState::Connected
            && lease.attached_at_unix_ms.is_none()
        {
            lease.attached_at_unix_ms = Some(now_ms);
        }
        lease.attachment_state = next_attachment_state(
            lease.attachment_state.clone(),
            lease.attached_at_unix_ms,
            lease.provider_state.is_terminal(),
            now_ms,
            attach_ttl_ms,
        );
    }

    transitions
}

/// The attachment lifecycle rules applied on every refresh:
/// - `awaiting`/`disconnected` on a terminal task → `expired` (the broker is
///   gone or will never arrive; nothing connects to a reclaimed container).
/// - `connected` past the staleness TTL → `expired` unless re-asserted
///   (`attachment <id> connected` restarts the clock).
/// - `connected` within the TTL is kept even on a terminal task:
///   reachability is checked independently of provider state.
fn next_attachment_state(
    current: AttachmentState,
    attached_at_unix_ms: Option<u64>,
    provider_terminal: bool,
    now_ms: u64,
    attach_ttl_ms: u64,
) -> AttachmentState {
    match current {
        AttachmentState::Awaiting | AttachmentState::Disconnected if provider_terminal => {
            AttachmentState::Expired
        }
        AttachmentState::Connected => {
            let stale = attached_at_unix_ms
                .is_none_or(|attached| now_ms.saturating_sub(attached) > attach_ttl_ms);
            if stale {
                AttachmentState::Expired
            } else {
                AttachmentState::Connected
            }
        }
        other => other,
    }
}

const DEFAULT_ATTACH_TTL_S: u64 = 3600;

/// How long a `connected` attachment stays credible without re-assertion.
fn attach_ttl_ms() -> u64 {
    std::env::var("INTENDANT_CODEX_CLOUD_ATTACH_TTL_S")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ATTACH_TTL_S)
        .saturating_mul(1000)
}

fn load_store(path: &Path) -> Result<LeaseStore, String> {
    match std::fs::read(path) {
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

fn save_store(path: &Path, store: &LeaseStore) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(store)
        .map_err(|e| format!("serialize worker lease store: {e}"))?;
    crate::file_watcher::atomic_write(path, &bytes)
        .map_err(|e| format!("write worker lease store {}: {e}", path.display()))
}

/// Advisory cross-process lock over the store's read-modify-write windows:
/// the daemon's MCP tools, the dashboard route, and any number of CLI
/// invocations share one file, and `atomic_write` alone cannot stop a stale
/// loader from clobbering a concurrent update. Locks a sidecar
/// `<store>.lock` — never the store itself, whose inode `atomic_write`
/// replaces (and whose reads Windows' LockFileEx would block). The OS
/// releases the lock if the holder dies.
struct StoreLock {
    file: std::fs::File,
}

impl StoreLock {
    fn acquire(store_path: &Path) -> Result<Self, String> {
        Self::acquire_path(&store_lock_path(store_path))
    }

    /// Lock an arbitrary sidecar path with the same semantics (the
    /// per-task follow-up serialization lock rides here too).
    fn acquire_path(lock_path: &Path) -> Result<Self, String> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create lease store directory {}: {e}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)
            .map_err(|e| format!("open lease store lock {}: {e}", lock_path.display()))?;
        file.lock()
            .map_err(|e| format!("lock lease store {}: {e}", lock_path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn store_lock_path(store_path: &Path) -> PathBuf {
    let mut name = store_path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    store_path.with_file_name(name)
}

/// The edge resolver for the lease store location. Everything below the
/// CLI/MCP/gateway edges takes the store path as a parameter; tests thread
/// tempdirs and never touch this.
pub(crate) fn state_path() -> PathBuf {
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

/// First `task_…` token in the combined CLI output. A heuristic: the split
/// already guarantees the token alphabet, and if the output ever mentions
/// several task ids the first one wins — every observed submit format
/// prints the new task's id first.
fn extract_task_id(output: &str) -> Option<String> {
    output
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .find(|part| part.len() > "task_".len() && part.starts_with("task_"))
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
    println!(
        "  warmth: {}",
        warmth_label(lease_warmth(lease, now_unix_ms()))
    );
    if lease.turns_observed > 0 {
        println!("  turns observed: {}", lease.turns_observed);
    }
    if let Some(worker) = &lease.worker {
        let boot = worker
            .boot_id
            .as_deref()
            .map(|id| format!(" (boot {})", &id[..id.len().min(8)]))
            .unwrap_or_default();
        println!(
            "  worker: {}{boot}",
            worker.hostname.as_deref().unwrap_or("unknown-host")
        );
    }
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
        "Usage:\n  intendant codex-cloud <command> [options]\n\nCommands:\n  doctor       Verify the local Codex CLI and Cloud authentication\n  exec         Submit a task and create a provider-owned worker lease\n  list         Refresh and list Cloud tasks/leases (window + live tracked)\n  status       Show one tracked lease\n  diff         Show a task diff through the Codex CLI\n  pull         Apply a task's diff onto a fresh branch in a new worktree\n  probe        Submit a diagnostic task that fingerprints its worker\n  followup     Send a follow-up turn into an existing task (private backend)\n  attachment   Record the independent live-attachment state\n  prune        Drop terminal leases with no live attachment\n  bootstrap    Generate setup, maintenance, and task-time worker scripts\n\nCodex Cloud containers are ephemeral worker leases, not permanent peers."
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

fn print_followup_help() {
    println!(
        "Usage:\n  intendant codex-cloud followup TASK_ID [-m PROMPT] [--json]\n\nSends a follow-up turn into an existing Codex Cloud task — the warm lever:\na follow-up reuses the task's worker and its incremental build state when\nit lands inside the warmth window (identical rebuild measured 68x faster).\nWithout -m the prompt is read from stdin, keeping sensitive instructions\nout of shell history.\n\nThis rides the provider's private web backend using the Codex CLI's own\nChatGPT login (never printed); the upstream CLI has no follow-up verb yet\n(issue #24777). Tasks with an active turn are refused, and any 404/409/422\nor schema change is reported as a compatibility break — prefer the\nofficial command once upstream ships one."
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

    const TEST_TTL_MS: u64 = 60_000;

    fn lease(task_id: &str) -> WorkerLease {
        WorkerLease {
            task_id: task_id.into(),
            task_url: None,
            title: "old".into(),
            environment_id: None,
            environment_label: None,
            provider_status: "running".into(),
            provider_state: ProviderLeaseState::Running,
            attachment_state: AttachmentState::NotRequested,
            attached_at_unix_ms: None,
            last_running_at_unix_ms: None,
            last_terminal_at_unix_ms: None,
            turns_observed: 0,
            is_probe: false,
            worker: None,
            provider_updated_at: None,
            last_observed_unix_ms: 1,
        }
    }

    fn store_with(leases: Vec<WorkerLease>) -> LeaseStore {
        LeaseStore {
            version: STORE_VERSION,
            leases: leases
                .into_iter()
                .map(|lease| (lease.task_id.clone(), lease))
                .collect(),
        }
    }

    fn task(id: &str, status: &str) -> CloudTask {
        CloudTask {
            id: id.into(),
            url: None,
            title: String::new(),
            status: status.into(),
            updated_at: None,
            environment_id: None,
            environment_label: None,
        }
    }

    #[test]
    fn sync_preserves_independent_attachment_state() {
        let mut connected = lease("task_e_123");
        connected.attachment_state = AttachmentState::Connected;
        let mut store = store_with(vec![connected]);
        let mut ready = task("task_e_123", "ready");
        ready.title = "new".into();
        sync_store(&mut store, &[ready], 1_000, TEST_TTL_MS);
        let lease = &store.leases["task_e_123"];
        assert_eq!(lease.provider_state, ProviderLeaseState::Finished);
        assert_eq!(lease.attachment_state, AttachmentState::Connected);
        assert_eq!(lease.title, "new");
        // A pre-TTL `connected` without a timestamp starts its clock at this
        // sync instead of expiring on sight.
        assert_eq!(lease.attached_at_unix_ms, Some(1_000));
    }

    #[test]
    fn disconnected_terminal_attachment_expires() {
        let mut disconnected = lease("task_e_123");
        disconnected.attachment_state = AttachmentState::Disconnected;
        let mut store = store_with(vec![disconnected]);
        sync_store(
            &mut store,
            &[task("task_e_123", "error")],
            1_000,
            TEST_TTL_MS,
        );
        assert_eq!(
            store.leases["task_e_123"].attachment_state,
            AttachmentState::Expired
        );
    }

    #[test]
    fn awaiting_on_terminal_task_expires() {
        let mut awaiting = lease("task_e_123");
        awaiting.attachment_state = AttachmentState::Awaiting;
        let mut store = store_with(vec![awaiting]);
        sync_store(
            &mut store,
            &[task("task_e_123", "cancelled")],
            1_000,
            TEST_TTL_MS,
        );
        assert_eq!(
            store.leases["task_e_123"].attachment_state,
            AttachmentState::Expired
        );
    }

    #[test]
    fn connected_attachment_expires_past_ttl_unless_reasserted() {
        let mut connected = lease("task_e_123");
        connected.attachment_state = AttachmentState::Connected;
        connected.attached_at_unix_ms = Some(1_000);
        let mut store = store_with(vec![connected]);

        // Within the TTL: survives, even though the task is terminal.
        sync_store(
            &mut store,
            &[task("task_e_123", "ready")],
            1_000 + TEST_TTL_MS,
            TEST_TTL_MS,
        );
        assert_eq!(
            store.leases["task_e_123"].attachment_state,
            AttachmentState::Connected
        );

        // Past the TTL without re-assertion: expired.
        sync_store(
            &mut store,
            &[task("task_e_123", "ready")],
            1_000 + TEST_TTL_MS + 1,
            TEST_TTL_MS,
        );
        assert_eq!(
            store.leases["task_e_123"].attachment_state,
            AttachmentState::Expired
        );
    }

    #[test]
    fn store_only_connected_lease_ages_out_too() {
        let mut connected = lease("task_e_old");
        connected.attachment_state = AttachmentState::Connected;
        connected.attached_at_unix_ms = Some(1_000);
        let mut store = store_with(vec![connected]);
        // The provider window no longer contains the task at all.
        sync_store(
            &mut store,
            &[task("task_e_new", "running")],
            1_000 + TEST_TTL_MS + 1,
            TEST_TTL_MS,
        );
        assert_eq!(
            store.leases["task_e_old"].attachment_state,
            AttachmentState::Expired
        );
    }

    #[test]
    fn terminal_transition_reported_exactly_once() {
        let mut store = store_with(vec![lease("task_e_123")]);
        let first = sync_store(
            &mut store,
            &[task("task_e_123", "ready")],
            1_000,
            TEST_TTL_MS,
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].task_id, "task_e_123");
        assert_eq!(first[0].provider_state, ProviderLeaseState::Finished);
        // The stored lease title backfills the transition when the provider
        // sends an empty one.
        assert_eq!(first[0].title, "old");

        let second = sync_store(
            &mut store,
            &[task("task_e_123", "ready")],
            2_000,
            TEST_TTL_MS,
        );
        assert!(second.is_empty(), "terminal → terminal is not an edge");
    }

    #[test]
    fn turn_tracking_counts_terminal_edges_and_follow_up_flaps() {
        let mut store = store_with(vec![lease("task_e_123")]);
        // First completed turn.
        sync_store(
            &mut store,
            &[task("task_e_123", "ready")],
            1_000,
            TEST_TTL_MS,
        );
        {
            let lease = &store.leases["task_e_123"];
            assert_eq!(lease.turns_observed, 1);
            assert_eq!(lease.last_terminal_at_unix_ms, Some(1_000));
        }
        // A follow-up driven from the task's web UI: terminal → running →
        // terminal shows up across refreshes as two more syncs.
        sync_store(
            &mut store,
            &[task("task_e_123", "running")],
            2_000,
            TEST_TTL_MS,
        );
        {
            let lease = &store.leases["task_e_123"];
            assert_eq!(lease.last_running_at_unix_ms, Some(2_000));
            assert_eq!(lease.turns_observed, 1, "running is not a completed turn");
        }
        let transitions = sync_store(
            &mut store,
            &[task("task_e_123", "ready")],
            3_000,
            TEST_TTL_MS,
        );
        assert_eq!(transitions.len(), 1, "each follow-up completion is an edge");
        let lease = &store.leases["task_e_123"];
        assert_eq!(lease.turns_observed, 2);
        assert_eq!(lease.last_terminal_at_unix_ms, Some(3_000));
    }

    #[test]
    fn warmth_follows_the_measured_windows() {
        let mut running = lease("task_e_live");
        running.provider_state = ProviderLeaseState::Running;
        assert_eq!(lease_warmth(&running, 0), Warmth::LikelyWarm);

        let mut queued = lease("task_e_queued");
        queued.provider_state = ProviderLeaseState::Queued;
        assert_eq!(
            lease_warmth(&queued, 0),
            Warmth::Unknown,
            "queued tasks have no worker yet"
        );

        let mut done = lease("task_e_done");
        done.provider_state = ProviderLeaseState::Finished;
        done.provider_status = "ready".into();
        done.last_terminal_at_unix_ms = Some(1_000);
        assert_eq!(
            lease_warmth(&done, 1_000 + WARM_WINDOW_MS),
            Warmth::LikelyWarm
        );
        assert_eq!(
            lease_warmth(&done, 1_000 + WARM_WINDOW_MS + 1),
            Warmth::Unknown
        );
        assert_eq!(
            lease_warmth(&done, 1_000 + SETUP_CACHE_WINDOW_MS + 1),
            Warmth::ColdLikely
        );

        // First seen already terminal: no known edge time, honest Unknown.
        let mut history = lease("task_e_hist");
        history.provider_state = ProviderLeaseState::Finished;
        assert_eq!(lease_warmth(&history, u64::MAX), Warmth::Unknown);
    }

    #[test]
    fn parses_probe_fingerprint_from_a_diff() {
        let diff = concat!(
            "diff --git a/._intendant-probe/fingerprint.json b/._intendant-probe/fingerprint.json\n",
            "new file mode 100644\n",
            "index 0000000..1111111\n",
            "--- /dev/null\n",
            "+++ b/._intendant-probe/fingerprint.json\n",
            "@@ -0,0 +1 @@\n",
            "+{\"intendant_probe\":1,\"hostname\":\"44c6850d6d8a\",\"boot_id\":\"cbebf3bc-b510-4853-a484-fd08e8fa1c93\",\"pid1_start\":\"68\",\"unix_ms\":1753380000123,\"git_rev\":\"17309c10\",\"rustc\":\"rustc 1.96.1\",\"cpus\":8,\"mem_kb\":16000000}\n",
        );
        let fingerprint = parse_probe_fingerprint(diff).expect("fingerprint parses");
        assert_eq!(fingerprint.hostname.as_deref(), Some("44c6850d6d8a"));
        assert_eq!(
            fingerprint.boot_id.as_deref(),
            Some("cbebf3bc-b510-4853-a484-fd08e8fa1c93")
        );
        assert_eq!(fingerprint.pid1_start.as_deref(), Some("68"));
        assert_eq!(fingerprint.cpus, Some(8));
        assert!(parse_probe_fingerprint("+not json at all").is_none());
        assert!(parse_probe_fingerprint("no added lines").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refresh_collects_probe_fingerprints_from_the_diff() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("leases.json");
        let mut probe = lease("task_e_probe");
        probe.is_probe = true;
        save_store(&store_path, &store_with(vec![probe])).unwrap();

        // The fake CLI answers `cloud list` with the probe task finished and
        // `cloud diff` with the fingerprint file.
        let command = dir.path().join("fake-codex");
        std::fs::write(
            &command,
            r#"#!/bin/sh
if [ "$2" = "list" ]; then
cat <<'EOF'
{"tasks": [{"id": "task_e_probe", "url": null, "title": "Intendant worker probe",
 "status": "ready", "updated_at": null, "environment_id": null,
 "environment_label": null}], "cursor": null}
EOF
elif [ "$2" = "diff" ]; then
cat <<'EOF'
+++ b/._intendant-probe/fingerprint.json
+{"intendant_probe":1,"hostname":"2c827f9104b2","boot_id":"c8da9ec6-aaaa-bbbb-cccc-000000000000","pid1_start":"68","cpus":4}
EOF
fi
"#,
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();

        let outcome = refresh_leases_with(
            command.to_str().unwrap(),
            &store_path,
            None,
            20,
            None,
            TEST_TTL_MS,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transitions.len(), 1);
        let worker = outcome.workers[0]
            .worker
            .as_ref()
            .expect("fingerprint collected into the returned outcome");
        assert_eq!(worker.hostname.as_deref(), Some("2c827f9104b2"));
        let stored = load_store(&store_path).unwrap();
        assert_eq!(
            stored.leases["task_e_probe"]
                .worker
                .as_ref()
                .and_then(|w| w.boot_id.as_deref()),
            Some("c8da9ec6-aaaa-bbbb-cccc-000000000000")
        );
        assert_eq!(stored.leases["task_e_probe"].turns_observed, 1);
    }

    #[test]
    fn first_sight_terminal_is_history_not_a_transition() {
        let mut store = store_with(vec![]);
        let transitions = sync_store(
            &mut store,
            &[task("task_e_done", "ready")],
            1_000,
            TEST_TTL_MS,
        );
        assert!(transitions.is_empty());
        assert_eq!(
            store.leases["task_e_done"].provider_state,
            ProviderLeaseState::Finished
        );
    }

    #[test]
    fn empty_provider_fields_never_erase_known_values() {
        let mut known = lease("task_e_123");
        known.title = "My submitted task".into();
        known.environment_id = Some("env_42".into());
        known.task_url = Some("https://chatgpt.com/codex/tasks/task_e_123".into());
        let mut store = store_with(vec![known]);
        // The real provider shape can return an empty title and null
        // environment/url fields.
        sync_store(
            &mut store,
            &[task("task_e_123", "running")],
            1_000,
            TEST_TTL_MS,
        );
        let lease = &store.leases["task_e_123"];
        assert_eq!(lease.title, "My submitted task");
        assert_eq!(lease.environment_id.as_deref(), Some("env_42"));
        assert_eq!(
            lease.task_url.as_deref(),
            Some("https://chatgpt.com/codex/tasks/task_e_123")
        );
    }

    #[test]
    fn prune_drops_only_inactive_terminal_leases() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("leases.json");

        let mut done_old = lease("task_e_done_old");
        done_old.provider_state = ProviderLeaseState::Finished;
        done_old.last_observed_unix_ms = 1;
        let mut done_connected = lease("task_e_done_connected");
        done_connected.provider_state = ProviderLeaseState::Finished;
        done_connected.attachment_state = AttachmentState::Connected;
        let running = lease("task_e_running");
        save_store(
            &store_path,
            &store_with(vec![done_old, done_connected, running]),
        )
        .unwrap();

        let outcome = prune_leases(&store_path, None).unwrap();
        assert_eq!(outcome.removed, vec!["task_e_done_old".to_string()]);
        assert_eq!(outcome.kept, 2);
        let remaining = load_store(&store_path).unwrap();
        assert!(remaining.leases.contains_key("task_e_done_connected"));
        assert!(remaining.leases.contains_key("task_e_running"));

        // An age cutoff spares recently-observed terminal leases.
        let mut done_fresh = lease("task_e_done_fresh");
        done_fresh.provider_state = ProviderLeaseState::Finished;
        done_fresh.last_observed_unix_ms = now_unix_ms();
        save_store(&store_path, &store_with(vec![done_fresh])).unwrap();
        let outcome = prune_leases(&store_path, Some(24 * 60 * 60 * 1000)).unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.kept, 1);
    }

    #[test]
    fn lease_store_round_trips_at_an_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("nested").join("leases.json");
        let _lock = StoreLock::acquire(&store_path).unwrap();
        let mut connected = lease("task_e_123");
        connected.attachment_state = AttachmentState::Connected;
        connected.attached_at_unix_ms = Some(42);
        save_store(&store_path, &store_with(vec![connected])).unwrap();
        let loaded = load_store(&store_path).unwrap();
        assert_eq!(loaded.leases["task_e_123"].attached_at_unix_ms, Some(42));
        assert!(store_lock_path(&store_path).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refresh_outcome_carries_window_tracked_and_cursor() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("leases.json");
        let mut tracked = lease("task_e_offwindow");
        tracked.attachment_state = AttachmentState::Connected;
        tracked.attached_at_unix_ms = Some(now_unix_ms());
        save_store(&store_path, &store_with(vec![tracked])).unwrap();

        let command = dir.path().join("fake-codex");
        std::fs::write(
            &command,
            r#"#!/bin/sh
cat <<'EOF'
{"tasks": [{"id": "task_e_new", "url": null, "title": "Fresh", "status": "running",
 "updated_at": null, "environment_id": null, "environment_label": null}],
 "cursor": "next-page"}
EOF
"#,
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();

        let outcome = refresh_leases_with(
            command.to_str().unwrap(),
            &store_path,
            None,
            20,
            None,
            TEST_TTL_MS,
        )
        .await
        .unwrap();
        assert_eq!(outcome.workers.len(), 1);
        assert_eq!(outcome.workers[0].task_id, "task_e_new");
        assert_eq!(outcome.tracked_active.len(), 1);
        assert_eq!(outcome.tracked_active[0].task_id, "task_e_offwindow");
        assert_eq!(outcome.cursor.as_deref(), Some("next-page"));
        assert!(outcome.transitions.is_empty());
        assert_eq!(load_store(&store_path).unwrap().leases.len(), 2);
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

    #[cfg(unix)]
    async fn scrubbed_git(cwd: &Path, args: &[&str]) {
        let status = crate::platform::spawn_command("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[cfg(unix)]
    fn fake_codex_emitting(dir: &Path, stdout: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let command = dir.join("fake-codex");
        let mut script = String::from("#!/bin/sh\ncat <<'FAKE_EOF'\n");
        script.push_str(stdout);
        script.push_str("\nFAKE_EOF\n");
        std::fs::write(&command, script).unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
        command
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pull_applies_task_diff_onto_fresh_worktree_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        scrubbed_git(&repo, &["init", "--quiet"]).await;
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        scrubbed_git(&repo, &["add", "README.md"]).await;
        scrubbed_git(
            &repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "init",
            ],
        )
        .await;

        let diff = "diff --git a/greeting.txt b/greeting.txt\n\
new file mode 100644\n\
index 0000000..ce01362\n\
--- /dev/null\n\
+++ b/greeting.txt\n\
@@ -0,0 +1 @@\n\
+hello";
        let codex = fake_codex_emitting(dir.path(), diff);

        let outcome = pull_task(
            codex.to_str().unwrap(),
            &repo,
            "task_e_pull",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.branch, "codex-cloud/task_e_pull");
        assert!(outcome.conflicts.is_empty());
        assert_eq!(
            std::fs::read_to_string(outcome.worktree.join("greeting.txt")).unwrap(),
            "hello\n"
        );
        // The main checkout is untouched; the branch exists.
        assert!(!repo.join("greeting.txt").exists());
        run_git(
            &repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/codex-cloud/task_e_pull",
            ],
        )
        .await
        .unwrap();

        // A second pull of the same task refuses instead of clobbering.
        let error = pull_task(
            codex.to_str().unwrap(),
            &repo,
            "task_e_pull",
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.contains("already exists"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pull_cleans_up_when_nothing_applies() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        scrubbed_git(&repo, &["init", "--quiet"]).await;
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        scrubbed_git(&repo, &["add", "README.md"]).await;
        scrubbed_git(
            &repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "init",
            ],
        )
        .await;

        let codex = fake_codex_emitting(dir.path(), "this is not a diff at all");
        let error = pull_task(
            codex.to_str().unwrap(),
            &repo,
            "task_e_bad",
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.contains("apply the task diff"), "{error}");
        assert!(!repo
            .join(".intendant")
            .join("worktrees")
            .join("codex-cloud")
            .join("task_e_bad")
            .exists());
        assert!(run_git(
            &repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/codex-cloud/task_e_bad",
            ],
        )
        .await
        .is_err());
    }

    fn test_auth() -> CodexAuth {
        CodexAuth {
            access_token: "test-token".into(),
            account_id: "acct-1".into(),
        }
    }

    /// One observed request from the stub backend.
    struct StubRequest {
        method: String,
        path: String,
        authorization: Option<String>,
        account_id: Option<String>,
        body: String,
    }

    /// Minimal scripted HTTP stub for the private-backend tests: answers
    /// the given responses in order and reports each observed request back
    /// over a channel (assertions happen on the test thread — a panic in
    /// the server thread would not fail the test).
    fn stub_backend(
        responses: Vec<(u16, String)>,
    ) -> (String, std::sync::mpsc::Receiver<StubRequest>) {
        use std::io::{BufRead as _, BufReader, Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let mut authorization = None;
                let mut account_id = None;
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    let Some((name, value)) = line.split_once(':') else {
                        continue;
                    };
                    match name.to_ascii_lowercase().as_str() {
                        "authorization" => authorization = Some(value.trim().to_string()),
                        "chatgpt-account-id" => account_id = Some(value.trim().to_string()),
                        "content-length" => content_length = value.trim().parse().unwrap_or(0),
                        _ => {}
                    }
                }
                let mut body_bytes = vec![0u8; content_length];
                if content_length > 0 && reader.read_exact(&mut body_bytes).is_err() {
                    return;
                }
                let _ = tx.send(StubRequest {
                    method,
                    path,
                    authorization,
                    account_id,
                    body: String::from_utf8_lossy(&body_bytes).into_owned(),
                });
                let response = format!(
                    "HTTP/1.1 {status} Stub\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = reader.get_mut().write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), rx)
    }

    #[test]
    fn codex_auth_reads_the_codex_cli_login() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"tokens":{"access_token":"bearer-secret-1","account_id":"acct-secret-1"}}"#,
        )
        .unwrap();
        let auth = load_codex_auth(dir.path()).unwrap();
        assert_eq!(auth.access_token, "bearer-secret-1");
        assert_eq!(auth.account_id, "acct-secret-1");
        let debug = format!("{auth:?}");
        assert!(
            !debug.contains("secret-1"),
            "CodexAuth Debug must redact credentials: {debug}"
        );

        // Older auth.json: the account id lives only in the id_token claim.
        use base64::Engine as _;
        let claims = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-from-jwt" }
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        std::fs::write(
            dir.path().join("auth.json"),
            format!(r#"{{"tokens":{{"access_token":"tok","id_token":"h.{payload}.s"}}}}"#),
        )
        .unwrap();
        let auth = load_codex_auth(dir.path()).unwrap();
        assert_eq!(auth.account_id, "acct-from-jwt");

        // API-key-only auth cannot drive Cloud follow-ups.
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"OPENAI_API_KEY":"sk-redacted"}"#,
        )
        .unwrap();
        let error = load_codex_auth(dir.path()).unwrap_err();
        assert!(error.contains("codex login"), "{error}");
    }

    #[test]
    fn followup_wire_body_matches_the_recovered_shape() {
        assert_eq!(
            followup_body("task_e_1", "task_e_1~assttrn_e_2", "do it"),
            serde_json::json!({
                "follow_up": {
                    "task_id": "task_e_1",
                    "turn_id": "task_e_1~assttrn_e_2",
                    "run_environment_in_qa_mode": false,
                },
                "input_items": [{
                    "type": "message",
                    "role": "user",
                    "content": [{ "content_type": "text", "text": "do it" }],
                }],
            })
        );
        assert!(is_assistant_turn_id("task_e_1~assttrn_e_2"));
        assert!(!is_assistant_turn_id("task_e_1~usrtrn_e_2"));
        assert!(!is_assistant_turn_id("task_e_1"));
    }

    #[test]
    fn followup_response_validation_requires_task_linkage() {
        let linked = serde_json::json!({
            "task": { "id": "task_e_1", "current_turn_id": "task_e_1~usrtrn_e_9" }
        });
        assert_eq!(
            validate_followup_response("task_e_1", "task_e_1~assttrn_e_2", &linked).unwrap(),
            vec!["task_e_1~usrtrn_e_9".to_string()]
        );

        let unlinked = serde_json::json!({ "ok": true });
        let error =
            validate_followup_response("task_e_1", "task_e_1~assttrn_e_2", &unlinked).unwrap_err();
        assert!(error.contains("compatibility break"), "{error}");
    }

    #[test]
    fn wham_errors_frame_login_and_compatibility_breaks() {
        assert!(wham_error(401, "https://x/wham/tasks", "").contains("codex login"));
        let compat = wham_error(404, "https://x/wham/tasks", "not found");
        assert!(compat.contains("compatibility break"), "{compat}");
        assert!(compat.contains("not found"), "{compat}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn followup_round_trip_against_a_stub_backend() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("leases.json");
        let mut done = lease("task_e_fu");
        done.provider_status = "ready".into();
        done.provider_state = ProviderLeaseState::Finished;
        save_store(&store_path, &store_with(vec![done])).unwrap();
        let codex = fake_codex_emitting(
            dir.path(),
            r#"{"tasks": [{"id": "task_e_fu", "url": null, "title": "t", "status": "ready",
 "updated_at": null, "environment_id": null, "environment_label": null}], "cursor": null}"#,
        );
        let (backend, requests) = stub_backend(vec![
            (
                200,
                r#"{"id":"task_e_fu","current_turn_id":"task_e_fu~assttrn_e_1"}"#.to_string(),
            ),
            (
                200,
                r#"{"task":{"id":"task_e_fu","current_turn_id":"task_e_fu~usrtrn_e_2"}}"#
                    .to_string(),
            ),
        ]);

        let receipt = follow_up_task_with(
            codex.to_str().unwrap(),
            &backend,
            &test_auth(),
            &store_path,
            "task_e_fu",
            "continue please",
            TEST_TTL_MS,
        )
        .await
        .unwrap();
        assert_eq!(receipt.parent_turn_id, "task_e_fu~assttrn_e_1");
        assert_eq!(
            receipt.new_turn_ids,
            vec!["task_e_fu~usrtrn_e_2".to_string()]
        );
        assert!(receipt.transitions.is_empty());

        let detail = requests.recv().unwrap();
        assert_eq!(detail.method, "GET");
        assert_eq!(detail.path, "/wham/tasks/task_e_fu");
        assert_eq!(detail.authorization.as_deref(), Some("Bearer test-token"));
        assert_eq!(detail.account_id.as_deref(), Some("acct-1"));
        let post = requests.recv().unwrap();
        assert_eq!(post.method, "POST");
        assert_eq!(post.path, "/wham/tasks");
        assert_eq!(post.authorization.as_deref(), Some("Bearer test-token"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&post.body).unwrap(),
            followup_body("task_e_fu", "task_e_fu~assttrn_e_1", "continue please")
        );

        // The accepted turn is recorded as a running edge, so warmth and
        // the next refresh's terminal-edge turn counting stay honest.
        let store = load_store(&store_path).unwrap();
        let lease = store.leases.get("task_e_fu").unwrap();
        assert_eq!(lease.provider_state, ProviderLeaseState::Running);
        assert!(lease.last_running_at_unix_ms.is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn followup_refuses_a_task_with_an_active_turn() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("leases.json");
        let codex = fake_codex_emitting(
            dir.path(),
            r#"{"tasks": [{"id": "task_e_busy", "url": null, "title": "t", "status": "running",
 "updated_at": null, "environment_id": null, "environment_label": null}], "cursor": null}"#,
        );
        // The unroutable backend proves the gate rejects before any HTTP.
        let error = follow_up_task_with(
            codex.to_str().unwrap(),
            "http://127.0.0.1:9",
            &test_auth(),
            &store_path,
            "task_e_busy",
            "too eager",
            TEST_TTL_MS,
        )
        .await
        .unwrap_err();
        assert!(error.contains("active turn"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn followup_fails_closed_when_the_detail_schema_drifts() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("leases.json");
        let codex = fake_codex_emitting(
            dir.path(),
            r#"{"tasks": [{"id": "task_e_drift", "url": null, "title": "t", "status": "ready",
 "updated_at": null, "environment_id": null, "environment_label": null}], "cursor": null}"#,
        );
        let (backend, _requests) = stub_backend(vec![(200, "{}".to_string())]);
        let error = follow_up_task_with(
            codex.to_str().unwrap(),
            &backend,
            &test_auth(),
            &store_path,
            "task_e_drift",
            "hello",
            TEST_TTL_MS,
        )
        .await
        .unwrap_err();
        assert!(error.contains("current_turn_id"), "{error}");
    }
}

//! `intendant ctl remote` — the ergonomic verb family over the daemon's
//! provider-neutral `remote_command` tool.
//!
//! The lane's delivery is SKILL + CTL (2026-08-07 ratification): sessions
//! learn WHEN to offload from the `intendant-remote-compute` skill and reach
//! the daemon tool through these verbs instead of hand-building
//! `tools call remote_command` JSON (the observed failure mode: an -32603
//! "missing field argv" stumble mid-offload). Every verb maps real flags to
//! the tool's op vocabulary, keeps daemon refusals verbatim, and exits
//! nonzero for anything short of a succeeded job. Identity rides the normal
//! ctl lanes: inside a supervised session the injected `INTENDANT_MCP_URL`
//! token binds the session (its recorded project root namespaces
//! `durable_sccache`); unsupervised loopback callers run unrestricted with
//! no project root, so `--cache durable_sccache` fails early there by
//! design.

use super::{
    call_tool, ensure_help, parse_command_args, print_json, single_text_content, CommandArgs,
    Config,
};
use serde_json::{Map, Value};

pub(super) async fn run_remote(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    ensure_help(raw, help_remote)?;
    if raw.is_empty() {
        help_remote();
        return Ok(());
    }
    match raw[0].as_str() {
        "start" => {
            let arguments = remote_start_args(&raw[1..])?;
            let response = call_tool(client, config, "remote_command", arguments).await?;
            let job = remote_job_from_response(&response)?;
            if config.raw {
                return print_json(&response);
            }
            if config.json {
                return print_json(&job);
            }
            let job_id = job_field(&job, "job_id").unwrap_or("?");
            println!(
                "started remote job {job_id} (state {}, host {})",
                job_field(&job, "state").unwrap_or("?"),
                job_field(&job, "host").unwrap_or("?"),
            );
            println!("  wait:   intendant ctl remote wait {job_id} --for 1800");
            println!("  status: intendant ctl remote status {job_id}");
            println!("  cancel: intendant ctl remote cancel {job_id}");
            Ok(())
        }
        "status" => {
            let job_id = remote_job_id(&raw[1..], "status", &[])?;
            let response = call_tool(
                client,
                config,
                "remote_command",
                serde_json::json!({ "op": "status", "job_id": job_id }),
            )
            .await?;
            let job = remote_job_from_response(&response)?;
            if config.raw {
                return print_json(&response);
            }
            if config.json {
                return print_json(&job);
            }
            print_job_report(&job);
            Ok(())
        }
        "wait" => {
            let args = parse_command_args(&raw[1..], &["--for"], &[])?;
            let job_id = positional_job_id(&args, "wait")?;
            let total_s = match args.one("--for") {
                Some(value) => value
                    .parse::<u64>()
                    .map_err(|_| format!("--for {value:?} is not a number of seconds"))?,
                None => DEFAULT_WAIT_FOR_S,
            };
            if total_s == 0 {
                return Err("--for must be at least 1 second".to_string());
            }
            // checked: a huge --for must be a CLI error, not an Instant
            // overflow panic.
            let deadline = std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(total_s))
                .ok_or_else(|| format!("--for {total_s}s is too large"))?;
            let mut last_progress = String::new();
            loop {
                let remaining_s = deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .as_secs();
                if remaining_s == 0 {
                    return Err(format!(
                        "remote job {job_id} is not terminal after {total_s}s — the job keeps \
                         running; re-run `intendant ctl remote wait {job_id}` or check \
                         `intendant ctl remote status {job_id}`"
                    ));
                }
                let response = call_tool(
                    client,
                    config,
                    "remote_command",
                    serde_json::json!({
                        "op": "wait",
                        "job_id": job_id,
                        "wait_s": wait_chunk_s(remaining_s),
                    }),
                )
                .await?;
                let job = remote_job_from_response(&response)?;
                let state = job_field(&job, "state").unwrap_or("?").to_string();
                if state_is_terminal(&state) {
                    if config.raw {
                        return print_json(&response);
                    }
                    if config.json {
                        print_json(&job)?;
                    } else {
                        print_job_report(&job);
                    }
                    return terminal_outcome(&job, &state);
                }
                // Progress heartbeat on stderr so scripts capturing stdout
                // see only the final report.
                let progress = progress_line(&job, &state);
                if progress != last_progress {
                    eprintln!("{progress} ({remaining_s}s left)");
                    last_progress = progress;
                }
            }
        }
        "cancel" => {
            let job_id = remote_job_id(&raw[1..], "cancel", &[])?;
            let response = call_tool(
                client,
                config,
                "remote_command",
                serde_json::json!({ "op": "cancel", "job_id": job_id }),
            )
            .await?;
            let job = remote_job_from_response(&response)?;
            if config.raw {
                return print_json(&response);
            }
            if config.json {
                return print_json(&job);
            }
            print_job_report(&job);
            Ok(())
        }
        other => Err(format!(
            "unknown remote command '{other}'. Run `intendant ctl remote --help`."
        )),
    }
}

/// Default total wait budget for `remote wait`. Matches the tool's default
/// command timeout; a cold acquisition can need more (`--for 3600`).
const DEFAULT_WAIT_FOR_S: u64 = 900;

/// One server-side `wait` op accepts 1-60 seconds; chunk the client budget.
fn wait_chunk_s(remaining_s: u64) -> u64 {
    remaining_s.clamp(1, 60)
}

/// Client-side terminal set. Pinned against
/// `remote_compute::RemoteCommandState::is_terminal` by a unit test so the
/// wait loop can never spin on a state the daemon considers finished.
fn state_is_terminal(state: &str) -> bool {
    matches!(state, "succeeded" | "failed" | "timed_out" | "cancelled")
}

/// Map the `remote start` argv to the tool's `op: start` arguments. The
/// remote command itself comes after `--` (everything past the first `--`
/// travels verbatim, later `--` included). Only ctl-syntax contradictions
/// are refused here; the daemon's validation owns the job grammar and its
/// named refusals surface verbatim.
fn remote_start_args(raw: &[String]) -> Result<Value, String> {
    let args = parse_command_args(
        raw,
        &[
            "--host",
            "--branch",
            "--revision",
            "--source",
            "--cache",
            "--cwd",
            "--env",
            "--timeout",
        ],
        &["--allow-dirty"],
    )
    .map_err(|error| format!("{error} (the remote command and its own flags go after `--`)"))?;
    if args.positional.is_empty() {
        return Err(
            "remote start requires the command after `--`, e.g. `intendant ctl remote start \
             --revision SHA -- cargo check --workspace`"
                .to_string(),
        );
    }
    let mut map = Map::new();
    map.insert("op".to_string(), Value::String("start".to_string()));
    map.insert(
        "argv".to_string(),
        Value::Array(
            args.positional
                .iter()
                .map(|arg| Value::String(arg.clone()))
                .collect(),
        ),
    );
    insert_flag_string(&mut map, "host", args.one("--host"));
    insert_flag_string(&mut map, "branch", args.one("--branch"));
    insert_flag_string(&mut map, "expected_revision", args.one("--revision"));
    insert_flag_string(&mut map, "cwd", args.one("--cwd"));
    if let Some(source) = args.one("--source") {
        if !matches!(source, "git_revision" | "working_tree") {
            return Err(format!(
                "--source {source:?} is not a source mode (git_revision or working_tree)"
            ));
        }
        map.insert("source".to_string(), Value::String(source.to_string()));
    }
    if let Some(cache) = args.one("--cache") {
        if !matches!(cache, "none" | "durable_sccache") {
            return Err(format!(
                "--cache {cache:?} is not a cache mode (none or durable_sccache)"
            ));
        }
        map.insert("cache".to_string(), Value::String(cache.to_string()));
    }
    let mut env = Map::new();
    for entry in args.all("--env") {
        let Some((key, value)) = entry.split_once('=') else {
            return Err(format!("--env {entry:?} is not KEY=VALUE"));
        };
        if key.is_empty() {
            return Err(format!("--env {entry:?} has an empty variable name"));
        }
        env.insert(key.to_string(), Value::String(value.to_string()));
    }
    if !env.is_empty() {
        map.insert("env".to_string(), Value::Object(env));
    }
    if let Some(timeout) = args.one("--timeout") {
        let timeout: u64 = timeout
            .parse()
            .map_err(|_| format!("--timeout {timeout:?} is not a number of seconds"))?;
        map.insert("timeout_s".to_string(), Value::from(timeout));
    }
    if args.has("--allow-dirty") {
        map.insert("require_clean".to_string(), Value::Bool(false));
    }
    Ok(Value::Object(map))
}

fn insert_flag_string(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn remote_job_id(raw: &[String], verb: &str, value_flags: &[&str]) -> Result<String, String> {
    let args = parse_command_args(raw, value_flags, &[])?;
    positional_job_id(&args, verb)
}

fn positional_job_id(args: &CommandArgs, verb: &str) -> Result<String, String> {
    match args.positional.as_slice() {
        [job_id] => Ok(job_id.clone()),
        [] => Err(format!(
            "remote {verb} requires the job id from `remote start` (remote-…)"
        )),
        more => Err(format!(
            "remote {verb} takes exactly one job id, got {}",
            more.len()
        )),
    }
}

/// Unwrap the tool's `{"ok": …}` envelope from the JSON-RPC response.
/// Transport/IAM errors and `ok: false` refusals both surface verbatim —
/// this lane never rewrites what the daemon said.
fn remote_job_from_response(response: &Value) -> Result<Value, String> {
    if let Some(error) = response.get("error") {
        return Err(format!(
            "MCP error: {}",
            serde_json::to_string_pretty(error).unwrap_or_else(|_| error.to_string())
        ));
    }
    let result = response
        .get("result")
        .ok_or_else(|| "JSON-RPC response missing result".to_string())?;
    let text = single_text_content(result)
        .ok_or_else(|| "remote_command result carried no text content".to_string())?;
    let envelope: Value = serde_json::from_str(text)
        .map_err(|_| format!("remote_command returned unparseable output: {text}"))?;
    match envelope.get("ok").and_then(Value::as_bool) {
        Some(true) => envelope
            .get("job")
            .cloned()
            .ok_or_else(|| "remote_command ok response carried no job".to_string()),
        Some(false) => Err(envelope
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or(text)
            .to_string()),
        None => Err(format!("remote_command returned unexpected output: {text}")),
    }
}

fn job_field<'a>(job: &'a Value, key: &str) -> Option<&'a str> {
    job.get(key).and_then(Value::as_str)
}

fn progress_line(job: &Value, state: &str) -> String {
    let mut line = format!(
        "remote job {} {state}",
        job_field(job, "job_id").unwrap_or("?")
    );
    if let Some(acquisition) = job.get("acquisition") {
        if let Some(stage) = acquisition.get("stage").and_then(Value::as_str) {
            line.push_str(&format!(" ({stage}"));
            if let Some(task) = acquisition.get("task_id").and_then(Value::as_str) {
                line.push_str(&format!(", task {task}"));
            }
            line.push(')');
        }
    }
    line
}

/// Human report for a job view: identity and state on the first line, then
/// acquisition/result detail, then the bounded remote output. Stdout keeps
/// the report; nothing here re-words daemon-reported errors.
fn print_job_report(job: &Value) {
    let mut head = format!(
        "job {}  state={}  host={}",
        job_field(job, "job_id").unwrap_or("?"),
        job_field(job, "state").unwrap_or("?"),
        job_field(job, "host").unwrap_or("?"),
    );
    if let Some(result) = job.get("result") {
        if let Some(exit) = result.get("exit_code").and_then(Value::as_i64) {
            head.push_str(&format!("  exit={exit}"));
        }
        if let Some(ms) = result.get("duration_ms").and_then(Value::as_u64) {
            head.push_str(&format!("  duration={:.1}s", ms as f64 / 1000.0));
        }
    }
    println!("{head}");
    if let Some(acquisition) = job.get("acquisition") {
        let stage = acquisition
            .get("stage")
            .and_then(Value::as_str)
            .unwrap_or("?");
        println!("  acquisition: {stage}");
        if let Some(url) = acquisition.get("task_url").and_then(Value::as_str) {
            println!("  task url: {url}");
        }
        if let Some(error) = acquisition
            .get("last_provider_error")
            .and_then(Value::as_str)
        {
            println!("  last provider error: {error}");
        }
    }
    if let Some(result) = job.get("result") {
        if let Some(cache) = result.get("cache") {
            println!(
                "  cache: {} hits+{} misses+{} writes+{} errors+{}",
                cache.get("mode").and_then(Value::as_str).unwrap_or("?"),
                cache.get("hits_delta").and_then(Value::as_u64).unwrap_or(0),
                cache
                    .get("misses_delta")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache
                    .get("writes_delta")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache
                    .get("errors_delta")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            );
        }
        for (name, truncated_key) in [
            ("stdout", "stdout_truncated"),
            ("stderr", "stderr_truncated"),
        ] {
            let text = result.get(name).and_then(Value::as_str).unwrap_or("");
            if !text.is_empty() {
                let truncated = result
                    .get(truncated_key)
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let marker = if truncated { " (truncated)" } else { "" };
                println!("--- {name}{marker} ---");
                println!("{}", text.trim_end_matches('\n'));
            }
        }
    }
    if let Some(error) = job_field(job, "error") {
        println!("error: {error}");
    }
}

/// Exit mapping for a terminal job: only `succeeded` composes as success,
/// so `ctl remote start … && ctl remote wait …` behaves like the local
/// command would. The failure line carries the daemon's error verbatim.
fn terminal_outcome(job: &Value, state: &str) -> Result<(), String> {
    if state == "succeeded" {
        return Ok(());
    }
    let error = job_field(job, "error")
        .or_else(|| {
            job.get("result")
                .and_then(|result| result.get("error"))
                .and_then(Value::as_str)
        })
        .unwrap_or("no error detail");
    Err(format!(
        "remote job {} {state}: {error}",
        job_field(job, "job_id").unwrap_or("?")
    ))
}

fn help_remote() {
    println!(
        "Usage:\n\
  intendant ctl remote start [FLAGS] -- PROGRAM [ARGS...]\n\
  intendant ctl remote status JOB_ID\n\
  intendant ctl remote wait JOB_ID [--for SECONDS]\n\
  intendant ctl remote cancel JOB_ID\n\
\n\
Run heavy platform-neutral work — workspace builds, broad test suites,\n\
clippy sweeps, benchmarks — on an acquired remote worker instead of\n\
loading this machine. This is the ergonomic form of the daemon's\n\
remote_command tool (raw: intendant ctl tools call remote_command); the\n\
daemon reuses or acquires a matching Codex Cloud worker, and scheduling\n\
stays provider-neutral when --host is omitted. When to offload and when\n\
to stay local: the intendant-remote-compute skill.\n\
\n\
start flags:\n\
  --revision SHA     Pushed commit the worker must report (required for the\n\
                     default git_revision source; optional base for\n\
                     working_tree)\n\
  --branch NAME      Pushed provider branch containing --revision; pass it\n\
                     when no supervised project root can derive it\n\
  --source MODE      git_revision (default) or working_tree (bounded\n\
                     snapshot of your uncommitted changes; needs a\n\
                     supervised session's project root)\n\
  --cache MODE       none (default) or durable_sccache (namespaced by the\n\
                     supervised session's project root; unsupervised\n\
                     callers must omit it and accept a cold build)\n\
  --cwd PATH         Repository-relative working directory\n\
  --env KEY=VALUE    Explicit child environment additions (repeatable)\n\
  --timeout SECONDS  Command timeout (default 900)\n\
  --allow-dirty      Skip the clean-checkout refusal\n\
  --host HOST        auto (default) or cloud:<codex-task-id>\n\
\n\
wait blocks until the job is terminal or --for SECONDS (default 900) is\n\
spent, prints the bounded remote stdout/stderr, and exits 0 only for a\n\
succeeded job. A cold worker can take tens of minutes to acquire: pass a\n\
bigger --for (acquisition allows 3600s) instead of resubmitting — matching\n\
requests coalesce onto one acquisition.\n\
\n\
Examples:\n\
  git push origin my-branch\n\
  intendant ctl remote start --branch my-branch --revision 0123abc \\\n\
      -- cargo clippy --workspace -- -D warnings\n\
  intendant ctl remote wait remote-1234abcd --for 1800\n\
  intendant ctl remote start --source working_tree -- cargo check --workspace\n\
  intendant ctl remote cancel remote-1234abcd\n\
\n\
Refusals and failures surface the daemon's words verbatim (acquisition\n\
stage, provider task URL, signed-out state). Report a failed lane rather\n\
than quietly running the expensive command locally."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn start_maps_every_flag_to_the_tool_vocabulary() {
        let value = remote_start_args(&strings(&[
            "--host",
            "cloud:task_e_123",
            "--branch",
            "feature/x",
            "--revision",
            "0123abc",
            "--source",
            "git_revision",
            "--cache",
            "durable_sccache",
            "--cwd",
            "crates/intendant-core",
            "--env",
            "RUST_LOG=debug",
            "--env",
            "CARGO_PROFILE_DEV_DEBUG=0",
            "--timeout",
            "1200",
            "--allow-dirty",
            "--",
            "cargo",
            "clippy",
            "--workspace",
            "--",
            "-D",
            "warnings",
        ]))
        .expect("full flag set parses");
        assert_eq!(value["op"], "start");
        // Everything after the FIRST `--` is the argv, later `--` included.
        assert_eq!(
            value["argv"],
            serde_json::json!(["cargo", "clippy", "--workspace", "--", "-D", "warnings"])
        );
        assert_eq!(value["host"], "cloud:task_e_123");
        assert_eq!(value["branch"], "feature/x");
        assert_eq!(value["expected_revision"], "0123abc");
        assert_eq!(value["source"], "git_revision");
        assert_eq!(value["cache"], "durable_sccache");
        assert_eq!(value["cwd"], "crates/intendant-core");
        assert_eq!(
            value["env"],
            serde_json::json!({"RUST_LOG": "debug", "CARGO_PROFILE_DEV_DEBUG": "0"})
        );
        assert_eq!(value["timeout_s"], 1200);
        assert_eq!(value["require_clean"], false);
    }

    #[test]
    fn start_omits_absent_flags_so_daemon_defaults_stay_authoritative() {
        let value = remote_start_args(&strings(&["--revision", "0123abc", "--", "cargo", "check"]))
            .expect("minimal start parses");
        let object = value.as_object().expect("object");
        for absent in [
            "host",
            "branch",
            "source",
            "cache",
            "cwd",
            "env",
            "timeout_s",
            "require_clean",
        ] {
            assert!(
                !object.contains_key(absent),
                "{absent} must be absent when its flag is not passed"
            );
        }
        assert_eq!(value["argv"], serde_json::json!(["cargo", "check"]));
    }

    #[test]
    fn start_refusals_teach_the_double_dash_and_flag_shapes() {
        // A remote command's own flag before `--` reads as an unknown ctl
        // flag; the error teaches where the argv goes.
        let error = remote_start_args(&strings(&["cargo", "clippy", "--workspace"]))
            .expect_err("bare remote flags refuse");
        assert!(error.contains("--workspace"), "{error}");
        assert!(error.contains("after `--`"), "{error}");

        let error = remote_start_args(&strings(&["--revision", "0123abc"]))
            .expect_err("missing argv refuses");
        assert!(error.contains("requires the command after `--`"), "{error}");

        let error = remote_start_args(&strings(&["--source", "tarball", "--", "true"]))
            .expect_err("bogus source refuses");
        assert!(error.contains("git_revision or working_tree"), "{error}");

        let error = remote_start_args(&strings(&["--cache", "s3", "--", "true"]))
            .expect_err("bogus cache refuses");
        assert!(error.contains("none or durable_sccache"), "{error}");

        let error = remote_start_args(&strings(&["--env", "NOVALUE", "--", "true"]))
            .expect_err("env without = refuses");
        assert!(error.contains("KEY=VALUE"), "{error}");

        let error = remote_start_args(&strings(&["--timeout", "soon", "--", "true"]))
            .expect_err("non-numeric timeout refuses");
        assert!(error.contains("not a number of seconds"), "{error}");
    }

    #[test]
    fn job_id_positional_is_required_and_single() {
        assert_eq!(
            remote_job_id(&strings(&["remote-abc"]), "status", &[]).unwrap(),
            "remote-abc"
        );
        let error = remote_job_id(&[], "status", &[]).expect_err("missing id refuses");
        assert!(error.contains("requires the job id"), "{error}");
        let error =
            remote_job_id(&strings(&["a", "b"]), "cancel", &[]).expect_err("two ids refuse");
        assert!(error.contains("exactly one job id"), "{error}");
    }

    #[test]
    fn wait_chunks_stay_inside_the_tool_bounds() {
        assert_eq!(wait_chunk_s(0), 1);
        assert_eq!(wait_chunk_s(1), 1);
        assert_eq!(wait_chunk_s(59), 59);
        assert_eq!(wait_chunk_s(60), 60);
        assert_eq!(wait_chunk_s(6_000), 60);
    }

    /// The client's terminal set must match the daemon's — a drifted set
    /// would make `wait` spin forever on a finished job (or stop early on
    /// a live one).
    #[test]
    fn client_terminal_set_matches_remote_command_state() {
        use crate::remote_compute::RemoteCommandState as State;
        for state in [
            State::Acquiring,
            State::Preparing,
            State::Queued,
            State::Running,
            State::Cancelling,
            State::Succeeded,
            State::Failed,
            State::TimedOut,
            State::Cancelled,
        ] {
            let wire = serde_json::to_value(state).expect("serialize state");
            let wire = wire.as_str().expect("state serializes as a string");
            assert_eq!(
                state_is_terminal(wire),
                state.is_terminal(),
                "terminal drift for {wire}"
            );
        }
    }

    fn rpc_result_with_text(text: &str) -> Value {
        serde_json::json!({
            "result": { "content": [ { "type": "text", "text": text } ] }
        })
    }

    #[test]
    fn envelope_surfaces_daemon_refusals_verbatim() {
        let refusal = "durable_sccache through home requires a supervised project root";
        let response =
            rpc_result_with_text(&serde_json::json!({ "ok": false, "error": refusal }).to_string());
        assert_eq!(
            remote_job_from_response(&response).expect_err("refusal is an error"),
            refusal,
            "the daemon's words must pass through unrewritten"
        );
    }

    #[test]
    fn envelope_unwraps_the_job_and_flags_transport_errors() {
        let response = rpc_result_with_text(
            &serde_json::json!({ "ok": true, "job": { "job_id": "remote-1", "state": "acquiring" } })
                .to_string(),
        );
        let job = remote_job_from_response(&response).expect("job unwraps");
        assert_eq!(job["job_id"], "remote-1");

        let error_response =
            serde_json::json!({ "error": { "code": -32603, "message": "denied" } });
        let error = remote_job_from_response(&error_response).expect_err("MCP error surfaces");
        assert!(error.contains("denied"), "{error}");

        let junk = rpc_result_with_text("not json");
        let error = remote_job_from_response(&junk).expect_err("junk refuses");
        assert!(error.contains("unparseable"), "{error}");
    }

    #[test]
    fn terminal_outcome_composes_like_the_local_command() {
        let succeeded = serde_json::json!({ "job_id": "remote-1" });
        assert!(terminal_outcome(&succeeded, "succeeded").is_ok());

        let failed = serde_json::json!({
            "job_id": "remote-2",
            "result": { "error": "command exited with status exit status: 101" }
        });
        let error = terminal_outcome(&failed, "failed").expect_err("failed job errs");
        assert!(
            error.contains("command exited with status exit status: 101"),
            "{error}"
        );

        let acquisition_dead = serde_json::json!({
            "job_id": "remote-3",
            "error": "\"codex\" exited with exit status: 1: Not signed in. Please run \
                      'codex login'",
        });
        let error = terminal_outcome(&acquisition_dead, "failed").expect_err("dead lane errs");
        assert!(error.contains("Not signed in"), "{error}");
    }
}

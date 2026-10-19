//! The command log: a durable, owner-only record of what callers did
//! through the daemon's request/response surfaces.
//!
//! Two streams live under `<state root>/command-log/`:
//!
//! - `terminals/<opened-at>-<terminal>-<instance>.jsonl` — one file per PTY
//!   incarnation: how it was opened, every input with the acting principal
//!   and the surface it arrived through, the shell's output as produced,
//!   who closed it and the exit status. Input from every surface lands here
//!   (MCP terminal tools, dashboard tiles, the dashboard tunnel) because it
//!   is recorded where the bytes enter the PTY.
//! - `mcp-calls.jsonl` — one line per `tools/call` served by the HTTP MCP
//!   gateway (the ChatGPT/Codex tunnel relay and every other HTTP MCP
//!   client): principal, tool, bounded arguments, duration, result size,
//!   and errors, permission refusals included.
//!
//! Why: an MCP client such as ChatGPT loses its whole context when its
//! conversation ends, and nothing outside that context recorded what it
//! ran. With this log the owner can see what happened, and a fresh client
//! session can read what its predecessor did.
//!
//! Everything typed into or printed by a logged terminal is written
//! verbatim, like shell history. Files are created 0600 inside 0700
//! directories in the owner's state root. Output is capped per PTY
//! incarnation (inputs, close and exit are always kept), the MCP stream
//! rotates once at a size bound, and terminal files older than the
//! retention window are pruned when the daemon resolves the root.
//! Recording is a best-effort side channel: a failed write disables that
//! file and never fails, retries or delays the caller's action.
//! `INTENDANT_COMMAND_LOG=off` (or `0`/`false`/`no`) disables both streams.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// Environment switch: `off`, `0`, `false`, `no` or `disabled` turn the
/// command log off. Anything else (or unset) leaves it on.
pub(crate) const COMMAND_LOG_ENV: &str = "INTENDANT_COMMAND_LOG";
const RECORD_VERSION: u32 = 1;
/// Output recorded per PTY incarnation. Inputs, close and exit records
/// are kept after the cap; the exit record carries the true total.
const TERMINAL_OUTPUT_CAP_BYTES: u64 = 8 * 1024 * 1024;
/// Longest text one record carries (one input write, one output chunk).
const RECORD_TEXT_CAP_BYTES: usize = 64 * 1024;
/// `mcp-calls.jsonl` moves to `mcp-calls.1.jsonl` past this size.
const MCP_LOG_ROTATE_BYTES: u64 = 32 * 1024 * 1024;
/// Serialized arguments beyond this are kept as a truncated string.
const MCP_ARGS_CAP_BYTES: usize = 4 * 1024;
/// Error and refusal text kept per MCP record.
const MCP_DETAIL_CAP_BYTES: usize = 512;
/// Terminal transcripts older than this are pruned at startup.
const TERMINAL_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const TERMINALS_DIR: &str = "terminals";
const MCP_LOG_FILE: &str = "mcp-calls.jsonl";
const MCP_LOG_ROTATED_FILE: &str = "mcp-calls.1.jsonl";

/// The daemon's command-log root, or `None` when disabled.
///
/// Unit-test builds always get `None`: in this binary's tests
/// `intendant_home()` is the LIVE state root (see `credential_audit.rs`),
/// so tests pass their own temporary root to the functions below.
pub(crate) fn default_root() -> Option<PathBuf> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        if cfg!(test) || disabled_by(std::env::var(COMMAND_LOG_ENV).ok().as_deref()) {
            return None;
        }
        let root = crate::platform::intendant_home().join("command-log");
        prune_terminal_transcripts(&root, SystemTime::now(), TERMINAL_RETENTION);
        Some(root)
    })
    .clone()
}

fn disabled_by(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "off" | "false" | "no" | "disabled")
    )
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Lossy UTF-8 text of `bytes`, cut to at most `cap` bytes on a char
/// boundary. The flag is true when anything was cut.
fn bounded_text(bytes: &[u8], cap: usize) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= cap {
        return (text.into_owned(), false);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// A terminal id reduced to a file-name-safe fragment.
fn file_safe(id: &str) -> String {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    if safe.is_empty() {
        "terminal".to_string()
    } else {
        safe
    }
}

fn append_line(file: &mut std::fs::File, value: &serde_json::Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    file.write_all(line.as_bytes())
}

/// Delete `*.jsonl` transcripts under `root/terminals` last modified
/// before `now - retention`. Errors are ignored: pruning is housekeeping.
fn prune_terminal_transcripts(root: &Path, now: SystemTime, retention: Duration) {
    let Some(cutoff) = now.checked_sub(retention) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(root.join(TERMINALS_DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// What a transcript records about the shell when it opens.
pub(crate) struct TranscriptOpen<'a> {
    pub terminal_id: &'a str,
    pub instance_id: &'a str,
    /// The owning IAM principal; `None` is the owner/root lane.
    pub owner: Option<&'a str>,
    pub shared: bool,
    pub scoped: bool,
    pub cwd: Option<&'a Path>,
    pub cols: u16,
    pub rows: u16,
}

/// The append-only transcript of one PTY incarnation.
pub(crate) struct TerminalTranscript {
    path: PathBuf,
    output_cap: u64,
    state: Mutex<TranscriptState>,
}

struct TranscriptState {
    /// `None` after the first failed write: the transcript stays off.
    file: Option<std::fs::File>,
    output_bytes: u64,
    suppressed: bool,
}

impl TerminalTranscript {
    /// Create the transcript file under `root/terminals` and write its
    /// `open` record. `None` when the file cannot be created; the shell
    /// then runs unlogged.
    pub(crate) fn create(root: &Path, open: &TranscriptOpen<'_>) -> Option<Self> {
        Self::create_with_cap(root, open, TERMINAL_OUTPUT_CAP_BYTES)
    }

    fn create_with_cap(root: &Path, open: &TranscriptOpen<'_>, output_cap: u64) -> Option<Self> {
        let dir = root.join(TERMINALS_DIR);
        intendant_core::state_paths::create_private_dir_all(&dir).ok()?;
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        let instance: String = open.instance_id.chars().take(12).collect();
        let path = dir.join(format!(
            "{stamp}-{}-{}.jsonl",
            file_safe(open.terminal_id),
            file_safe(&instance)
        ));
        let file = intendant_core::state_paths::private_file_options()
            .append(true)
            .open(&path)
            .ok()?;
        let transcript = Self {
            path,
            output_cap,
            state: Mutex::new(TranscriptState {
                file: Some(file),
                output_bytes: 0,
                suppressed: false,
            }),
        };
        transcript.record(serde_json::json!({
            "v": RECORD_VERSION,
            "at": now_rfc3339(),
            "ev": "open",
            "terminal_id": open.terminal_id,
            "instance_id": open.instance_id,
            "owner": open.owner.unwrap_or("root"),
            "shared": open.shared,
            "scoped": open.scoped,
            "cwd": open.cwd.map(|dir| dir.display().to_string()),
            "cols": open.cols,
            "rows": open.rows,
        }));
        Some(transcript)
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn record(&self, value: serde_json::Value) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::write(&mut state, &value);
    }

    fn write(state: &mut TranscriptState, value: &serde_json::Value) {
        if let Some(file) = state.file.as_mut() {
            if append_line(file, value).is_err() {
                state.file = None;
            }
        }
    }

    /// Bytes written to the shell's stdin: `by` is the acting principal
    /// (`root` for owner surfaces, `daemon` for the daemon's own writes)
    /// and `via` the surface (`mcp`, `dashboard`, `dashboard-tunnel`, …).
    pub(crate) fn input(&self, data: &[u8], by: &str, via: &str) {
        let (text, truncated) = bounded_text(data, RECORD_TEXT_CAP_BYTES);
        let mut record = serde_json::json!({
            "v": RECORD_VERSION,
            "at": now_rfc3339(),
            "ev": "input",
            "by": by,
            "via": via,
            "bytes": data.len(),
            "text": text,
        });
        if truncated {
            record["truncated"] = serde_json::Value::Bool(true);
        }
        self.record(record);
    }

    /// One chunk of shell output, until the per-incarnation cap; the
    /// first chunk past the cap leaves a single `output_suppressed` mark.
    pub(crate) fn output(&self, data: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.file.is_none() {
            return;
        }
        let before = state.output_bytes;
        state.output_bytes = before.saturating_add(data.len() as u64);
        if state.suppressed {
            return;
        }
        if before >= self.output_cap {
            state.suppressed = true;
            let mark = serde_json::json!({
                "v": RECORD_VERSION,
                "at": now_rfc3339(),
                "ev": "output_suppressed",
                "after_bytes": before,
            });
            Self::write(&mut state, &mark);
            return;
        }
        let (text, truncated) = bounded_text(data, RECORD_TEXT_CAP_BYTES);
        let mut record = serde_json::json!({
            "v": RECORD_VERSION,
            "at": now_rfc3339(),
            "ev": "output",
            "bytes": data.len(),
            "text": text,
        });
        if truncated {
            record["truncated"] = serde_json::Value::Bool(true);
        }
        Self::write(&mut state, &record);
    }

    /// The registry closed the shell on `by`'s request.
    pub(crate) fn closed(&self, by: &str) {
        self.record(serde_json::json!({
            "v": RECORD_VERSION,
            "at": now_rfc3339(),
            "ev": "close",
            "by": by,
        }));
    }

    /// The shell exited; `output_bytes` is the true total, cap or not.
    pub(crate) fn exit(&self, status: i32) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let record = serde_json::json!({
            "v": RECORD_VERSION,
            "at": now_rfc3339(),
            "ev": "exit",
            "status": status,
            "output_bytes": state.output_bytes,
        });
        Self::write(&mut state, &record);
    }
}

/// One MCP `tools/call` as the gateway served it.
pub(crate) struct McpCall<'a> {
    pub principal: &'a str,
    pub session: Option<&'a str>,
    pub tool: &'a str,
    pub args: &'a serde_json::Value,
    pub elapsed: Duration,
}

/// How the call ended.
pub(crate) enum McpCallOutcome<'a> {
    /// Refused by the access gate; the value is the refusal result sent.
    Denied(&'a serde_json::Value),
    /// A tool result (`CallToolResult` JSON), successful or not.
    Result(&'a serde_json::Value),
    /// A JSON-RPC error instead of a result.
    Error(&'a str),
}

/// Append one record to `root/mcp-calls.jsonl`. Best effort: errors are
/// dropped, never surfaced to the caller.
pub(crate) fn record_mcp_call(root: &Path, call: &McpCall<'_>, outcome: McpCallOutcome<'_>) {
    record_mcp_call_with(root, call, outcome, MCP_LOG_ROTATE_BYTES);
}

fn record_mcp_call_with(
    root: &Path,
    call: &McpCall<'_>,
    outcome: McpCallOutcome<'_>,
    rotate_bytes: u64,
) {
    let args_text = call.args.to_string();
    let (args, args_truncated) = if args_text.len() <= MCP_ARGS_CAP_BYTES {
        (call.args.clone(), false)
    } else {
        let (text, _) = bounded_text(args_text.as_bytes(), MCP_ARGS_CAP_BYTES);
        (serde_json::Value::String(text), true)
    };
    let mut record = serde_json::json!({
        "v": RECORD_VERSION,
        "at": now_rfc3339(),
        "principal": call.principal,
        "session": call.session,
        "tool": call.tool,
        "args": args,
        "ms": u64::try_from(call.elapsed.as_millis()).unwrap_or(u64::MAX),
    });
    if args_truncated {
        record["args_truncated"] = serde_json::Value::Bool(true);
        record["args_bytes"] = serde_json::json!(args_text.len());
    }
    let (status, detail) = match outcome {
        McpCallOutcome::Denied(result) => ("denied", result_text(result)),
        McpCallOutcome::Result(result) => {
            record["result_bytes"] = serde_json::json!(result.to_string().len());
            let (is_error, detail) = result_error(result);
            (if is_error { "error" } else { "ok" }, detail)
        }
        McpCallOutcome::Error(message) => ("protocol_error", Some(message.to_string())),
    };
    record["outcome"] = serde_json::Value::String(status.to_string());
    if let Some(detail) = detail {
        let (text, _) = bounded_text(detail.as_bytes(), MCP_DETAIL_CAP_BYTES);
        record["detail"] = serde_json::Value::String(text);
    }

    static MCP_LOG_LOCK: Mutex<()> = Mutex::new(());
    let _guard = MCP_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if intendant_core::state_paths::create_private_dir_all(root).is_err() {
        return;
    }
    let path = root.join(MCP_LOG_FILE);
    if std::fs::metadata(&path)
        .map(|meta| meta.len() >= rotate_bytes)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, root.join(MCP_LOG_ROTATED_FILE));
    }
    if let Ok(mut file) = intendant_core::state_paths::private_file_options()
        .append(true)
        .open(&path)
    {
        let _ = append_line(&mut file, &record);
    }
}

/// The first text block of a `CallToolResult`, if any.
fn result_text(result: &serde_json::Value) -> Option<String> {
    result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .and_then(|blocks| {
            blocks
                .iter()
                .find_map(|block| block.get("text").and_then(serde_json::Value::as_str))
        })
        .map(str::to_string)
}

/// Whether a `CallToolResult` reports failure — `isError`, or Intendant's
/// `{"ok": false, "error": …}` text convention — and its error text.
fn result_error(result: &serde_json::Value) -> (bool, Option<String>) {
    let flagged = result
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let text = result_text(result);
    let parsed = text
        .as_deref()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
    let ok_false = parsed
        .as_ref()
        .and_then(|value| value.get("ok"))
        .and_then(serde_json::Value::as_bool)
        == Some(false);
    if !(flagged || ok_false) {
        return (false, None);
    }
    let detail = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .map(|error| match error.as_str() {
            Some(text) => text.to_string(),
            None => error.to_string(),
        })
        .or(text);
    (true, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_records(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn open_spec<'a>(terminal_id: &'a str, cwd: &'a Path) -> TranscriptOpen<'a> {
        TranscriptOpen {
            terminal_id,
            instance_id: "0123456789abcdef0123",
            owner: Some("principal:test"),
            shared: false,
            scoped: true,
            cwd: Some(cwd),
            cols: 120,
            rows: 32,
        }
    }

    #[test]
    fn transcript_records_open_input_output_close_and_exit() {
        let root = tempfile::tempdir().unwrap();
        let t = TerminalTranscript::create(root.path(), &open_spec("shell/1 x", root.path()))
            .expect("transcript");
        let name = t.path().file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.ends_with("-shell_1_x-0123456789ab.jsonl"), "{name}");
        t.input(b"ls -la\r", "principal:test", "mcp");
        t.output(b"total 0\r\n");
        t.closed("root");
        t.exit(0);
        let records = read_records(t.path());
        let events: Vec<&str> = records.iter().map(|r| r["ev"].as_str().unwrap()).collect();
        assert_eq!(events, ["open", "input", "output", "close", "exit"]);
        assert_eq!(records[0]["owner"], "principal:test");
        assert_eq!(records[0]["scoped"], true);
        assert_eq!(records[1]["text"], "ls -la\r");
        assert_eq!(records[1]["by"], "principal:test");
        assert_eq!(records[1]["via"], "mcp");
        assert_eq!(records[1]["bytes"], 7);
        assert_eq!(records[2]["text"], "total 0\r\n");
        assert_eq!(records[3]["by"], "root");
        assert_eq!(records[4]["status"], 0);
        assert_eq!(records[4]["output_bytes"], 9);
        assert!(records.iter().all(|r| r["v"] == 1 && r["at"].is_string()));
    }

    #[cfg(unix)]
    #[test]
    fn transcript_and_mcp_log_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let log_root = root.path().join("command-log");
        let t = TerminalTranscript::create(&log_root, &open_spec("t", root.path())).unwrap();
        let args = serde_json::json!({});
        record_mcp_call(
            &log_root,
            &McpCall {
                principal: "p",
                session: None,
                tool: "whoami",
                args: &args,
                elapsed: Duration::ZERO,
            },
            McpCallOutcome::Error("boom"),
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(t.path()), 0o600);
        assert_eq!(mode(&log_root.join(MCP_LOG_FILE)), 0o600);
        assert_eq!(mode(&log_root), 0o700);
        assert_eq!(mode(&log_root.join(TERMINALS_DIR)), 0o700);
    }

    #[test]
    fn output_past_the_cap_is_marked_once_and_inputs_and_exit_survive() {
        let root = tempfile::tempdir().unwrap();
        let t = TerminalTranscript::create_with_cap(root.path(), &open_spec("t", root.path()), 10)
            .unwrap();
        t.output(b"0123456789AB"); // crosses the cap: kept whole
        t.output(b"more"); // past the cap: one mark
        t.output(b"even more"); // still past: nothing
        t.input(b"exit\r", "root", "dashboard");
        t.exit(3);
        let records = read_records(t.path());
        let events: Vec<&str> = records.iter().map(|r| r["ev"].as_str().unwrap()).collect();
        assert_eq!(
            events,
            ["open", "output", "output_suppressed", "input", "exit"]
        );
        assert_eq!(records[2]["after_bytes"], 12);
        assert_eq!(records[4]["output_bytes"], 25);
    }

    #[test]
    fn bounded_text_cuts_on_a_char_boundary() {
        let (text, cut) = bounded_text("aé".as_bytes(), 2);
        assert_eq!((text.as_str(), cut), ("a", true));
        let (text, cut) = bounded_text(b"abc", 3);
        assert_eq!((text.as_str(), cut), ("abc", false));
        let (text, cut) = bounded_text(&[0xff, b'a'], 16);
        assert_eq!((text.as_str(), cut), ("\u{fffd}a", false));
    }

    #[test]
    fn mcp_calls_record_outcomes_and_bound_arguments() {
        let root = tempfile::tempdir().unwrap();
        let small = serde_json::json!({"argv": ["terminal", "read", "t-1"]});
        let big = serde_json::json!({"input": "x".repeat(MCP_ARGS_CAP_BYTES * 2)});
        let ok = serde_json::json!({"content": [{"type": "text", "text": "{\"ok\":true}"}]});
        let failed = serde_json::json!({"content": [{"type": "text",
            "text": "{\"ok\":false,\"error\":\"shell already exited\"}"}]});
        let flagged = serde_json::json!({"isError": true,
            "content": [{"type": "text", "text": "plain failure"}]});
        let denied = serde_json::json!({"isError": true,
            "content": [{"type": "text", "text": "permission denied: terminal.write"}]});
        let call = |tool, args| McpCall {
            principal: "principal:local-process:loopback",
            session: Some("s-1"),
            tool,
            args,
            elapsed: Duration::from_millis(42),
        };
        record_mcp_call(
            root.path(),
            &call("inspect", &small),
            McpCallOutcome::Result(&ok),
        );
        record_mcp_call(
            root.path(),
            &call("terminal_write", &big),
            McpCallOutcome::Result(&failed),
        );
        record_mcp_call(
            root.path(),
            &call("act", &small),
            McpCallOutcome::Result(&flagged),
        );
        record_mcp_call(
            root.path(),
            &call("act", &small),
            McpCallOutcome::Denied(&denied),
        );
        record_mcp_call(
            root.path(),
            &call("act", &small),
            McpCallOutcome::Error("bad params"),
        );
        let records = read_records(&root.path().join(MCP_LOG_FILE));
        let outcomes: Vec<&str> = records
            .iter()
            .map(|r| r["outcome"].as_str().unwrap())
            .collect();
        assert_eq!(
            outcomes,
            ["ok", "error", "error", "denied", "protocol_error"]
        );
        assert_eq!(records[0]["args"], small);
        assert_eq!(records[0]["ms"], 42);
        assert_eq!(records[0]["session"], "s-1");
        assert!(records[0].get("detail").is_none());
        assert!(records[0]["result_bytes"].as_u64().unwrap() > 0);
        assert_eq!(records[1]["args_truncated"], true);
        assert!(records[1]["args"].as_str().unwrap().len() <= MCP_ARGS_CAP_BYTES);
        assert_eq!(records[1]["detail"], "shell already exited");
        assert_eq!(records[2]["detail"], "plain failure");
        assert_eq!(records[3]["detail"], "permission denied: terminal.write");
        assert!(records[3].get("result_bytes").is_none());
        assert_eq!(records[4]["detail"], "bad params");
    }

    #[test]
    fn mcp_log_rotates_once_past_the_bound() {
        let root = tempfile::tempdir().unwrap();
        let args = serde_json::json!({});
        let call = McpCall {
            principal: "p",
            session: None,
            tool: "whoami",
            args: &args,
            elapsed: Duration::ZERO,
        };
        for _ in 0..3 {
            record_mcp_call_with(root.path(), &call, McpCallOutcome::Error("e"), 1);
        }
        assert_eq!(read_records(&root.path().join(MCP_LOG_FILE)).len(), 1);
        assert_eq!(
            read_records(&root.path().join(MCP_LOG_ROTATED_FILE)).len(),
            1
        );
    }

    #[test]
    fn pruning_removes_only_stale_transcripts() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(TERMINALS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        let old = dir.join("old.jsonl");
        let fresh = dir.join("fresh.jsonl");
        let other = dir.join("notes.txt");
        for path in [&old, &fresh, &other] {
            std::fs::write(path, b"{}\n").unwrap();
        }
        let long_ago = now - Duration::from_secs(40 * 24 * 60 * 60);
        for path in [&old, &other] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        prune_terminal_transcripts(root.path(), now, TERMINAL_RETENTION);
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(other.exists(), "only transcripts are pruned");
    }

    #[test]
    fn the_switch_turns_off_only_on_explicit_values() {
        for off in ["0", "off", "OFF", " false ", "no", "disabled"] {
            assert!(disabled_by(Some(off)), "{off}");
        }
        for on in [None, Some(""), Some("1"), Some("on"), Some("yes")] {
            assert!(!disabled_by(on), "{on:?}");
        }
    }

    #[test]
    fn unit_test_builds_never_resolve_the_live_root() {
        assert_eq!(default_root(), None);
    }

    #[test]
    fn file_names_are_sanitized_and_bounded() {
        assert_eq!(file_safe("shell-0"), "shell-0");
        assert_eq!(file_safe("a/b\\c d"), "a_b_c_d");
        assert_eq!(file_safe(""), "terminal");
        assert_eq!(file_safe(&"x".repeat(100)).len(), 48);
    }
}

//! The command log: a durable, owner-only record of what callers did
//! through the daemon's request/response surfaces.
//!
//! Two streams live under `<state root>/command-log/`:
//!
//! - `terminals/<opened-at>-<terminal>-<instance>.jsonl` — one file per PTY
//!   incarnation of the daemon's registry: how it was opened, every input
//!   with the acting principal and the surface it arrived through, the
//!   shell's output as produced, who closed it and the exit status. Input
//!   from every surface lands here (MCP terminal tools and facade, dashboard
//!   tiles, the dashboard tunnel) because it is recorded where the bytes
//!   enter the PTY.
//! - `mcp-calls.jsonl` — one line per `tools/call` the HTTP MCP gateway
//!   serves to a caller that is not a supervised session (the ChatGPT/Codex
//!   tunnel relay, `ctl`, other HTTP MCP clients): principal, tool, bounded
//!   arguments, duration, result size and errors, refusals included.
//!   Supervised sessions are left out: their tool calls are already in
//!   their own session logs, and their volume would rotate the rest away.
//!
//! Why: an MCP client such as ChatGPT loses its whole context when its
//! conversation ends, and nothing outside that context recorded what it
//! ran. With this log the owner can see what happened.
//!
//! Input typed while the terminal has echo off in canonical mode (password
//! prompts: sudo, ssh/gpg passphrases, `read -s`) is recorded by length
//! only, on Unix; everything else typed into or printed by a logged
//! terminal is stored verbatim. Files are created owner-only (0600 inside
//! 0700 directories on Unix; inherited profile ACLs on Windows) in the
//! owner's state root. Sizes are bounded: output per PTY incarnation, the
//! whole transcript file, each record's text, MCP arguments and names; the
//! MCP stream rotates to uniquely named files of which the newest two are
//! kept; terminal transcripts older than the retention window are pruned
//! at startup and at most hourly afterwards. Recording is a best-effort
//! side channel: a failed write disables that file, MCP records are handed
//! to a writer thread (dropped and counted when it falls behind), and
//! nothing ever fails, retries or noticeably delays the caller's action.
//! `INTENDANT_COMMAND_LOG=off` (or `0`/`false`/`no`) disables both streams.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// Environment switch: `off`, `0`, `false`, `no` or `disabled` turn the
/// command log off. Anything else (or unset) leaves it on.
pub(crate) const COMMAND_LOG_ENV: &str = "INTENDANT_COMMAND_LOG";
const RECORD_VERSION: u32 = 1;
/// Raw output bytes recorded per PTY incarnation; later output is only
/// counted (the exit record carries the total).
const TERMINAL_OUTPUT_CAP_BYTES: u64 = 8 * 1024 * 1024;
/// Bytes written to one transcript file, every record included; past it a
/// single `transcript_full` mark ends the file (the exit still follows).
const TERMINAL_FILE_CAP_BYTES: u64 = 32 * 1024 * 1024;
/// Longest text one record carries (one input write, one output chunk).
const RECORD_TEXT_CAP_BYTES: usize = 64 * 1024;
/// `mcp-calls.jsonl` is rotated away past this size.
const MCP_LOG_ROTATE_BYTES: u64 = 32 * 1024 * 1024;
/// Rotated MCP files kept (newest first by name).
const MCP_ROTATED_KEEP: usize = 2;
/// Serialized arguments beyond this are kept as a truncated string.
const MCP_ARGS_CAP_BYTES: usize = 4 * 1024;
/// Error and refusal text kept per MCP record.
const MCP_DETAIL_CAP_BYTES: usize = 512;
/// Tool names, principals and session ids are caller-controlled strings.
const MCP_NAME_CAP_BYTES: usize = 256;
/// A result's first text block is parsed for `{"ok": false}` only below this.
const MCP_PARSE_CAP_BYTES: usize = 64 * 1024;
/// MCP records queued for the writer thread before new ones are dropped.
const MCP_QUEUE_RECORDS: usize = 1024;
/// Terminal transcripts older than this are pruned.
const TERMINAL_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const PRUNE_INTERVAL_SECS: u64 = 60 * 60;
const TERMINALS_DIR: &str = "terminals";
const MCP_LOG_FILE: &str = "mcp-calls.jsonl";
const MCP_ROTATED_PREFIX: &str = "mcp-calls.";

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

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
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

/// `value` cut to `cap` bytes on a char boundary.
fn bounded_str(value: &str, cap: usize) -> String {
    bounded_text(value.as_bytes(), cap).0
}

/// Length of the longest prefix of `bytes` that does not end inside a
/// UTF-8 sequence which later bytes could still complete (at most 3 bytes
/// are held back; invalid bytes are not held).
fn complete_utf8_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => {
            // Invalid bytes somewhere: only hold back an incomplete tail.
            let tail_start = bytes.len().saturating_sub(3);
            (tail_start..bytes.len())
                .find(|&start| {
                    matches!(std::str::from_utf8(&bytes[start..]),
                        Err(e) if e.error_len().is_none() && e.valid_up_to() == 0)
                        && bytes[start] >= 0xC0
                })
                .unwrap_or(bytes.len())
        }
    }
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

/// One JSON line, newline included.
fn json_line(value: &serde_json::Value) -> Option<String> {
    let mut line = serde_json::to_string(value).ok()?;
    line.push('\n');
    Some(line)
}

/// Delete `*.jsonl` transcripts directly under `root/terminals` last
/// modified before `now - retention`. Errors are ignored: housekeeping.
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
        // `DirEntry::metadata` does not follow symlinks: a link is judged by
        // its own mtime and removing it never touches its target.
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

/// Prune at most once per [`PRUNE_INTERVAL_SECS`] per process, so a
/// long-running daemon keeps the retention bound without a timer.
fn prune_if_due(root: &Path, now: SystemTime) {
    static LAST_PRUNE: AtomicU64 = AtomicU64::new(0);
    let now_secs = unix_secs(now);
    let last = LAST_PRUNE.load(Ordering::Relaxed);
    if now_secs.saturating_sub(last) < PRUNE_INTERVAL_SECS {
        return;
    }
    if LAST_PRUNE
        .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        prune_terminal_transcripts(root, now, TERMINAL_RETENTION);
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
    #[cfg(test)]
    path: PathBuf,
    output_cap: u64,
    file_cap: u64,
    state: Mutex<TranscriptState>,
}

struct TranscriptState {
    /// `None` after the first failed write: the transcript stays off.
    file: Option<std::fs::File>,
    /// Bytes written to the file so far.
    written: u64,
    /// The file cap was reached and its mark written.
    full: bool,
    /// Raw output bytes the shell produced, recorded or not.
    output_bytes: u64,
    output_suppressed: bool,
    /// The incomplete UTF-8 tail of the last output chunk, completed by
    /// the next one (the reader splits at arbitrary byte boundaries).
    carry: Vec<u8>,
}

impl TranscriptState {
    /// Append one record unless the file failed or is full. `force`
    /// writes even past the cap (the exit record).
    fn write(&mut self, value: &serde_json::Value, file_cap: u64, force: bool) {
        if self.file.is_none() || (self.full && !force) {
            return;
        }
        let Some(line) = json_line(value) else {
            return;
        };
        if !force && self.written.saturating_add(line.len() as u64) > file_cap {
            self.full = true;
            let mark = serde_json::json!({
                "v": RECORD_VERSION,
                "at": now_rfc3339(),
                "ev": "transcript_full",
                "after_bytes": self.written,
            });
            if let Some(mark) = json_line(&mark) {
                self.append(&mark);
            }
            return;
        }
        self.append(&line);
    }

    fn append(&mut self, line: &str) {
        if let Some(file) = self.file.as_mut() {
            if file.write_all(line.as_bytes()).is_ok() {
                self.written = self.written.saturating_add(line.len() as u64);
            } else {
                self.file = None;
            }
        }
    }
}

impl TerminalTranscript {
    /// Create the transcript file under `root/terminals` and write its
    /// `open` record. `None` when the file cannot be created; the shell
    /// then runs unlogged.
    pub(crate) fn create(root: &Path, open: &TranscriptOpen<'_>) -> Option<Self> {
        prune_if_due(root, SystemTime::now());
        Self::create_with_caps(
            root,
            open,
            TERMINAL_OUTPUT_CAP_BYTES,
            TERMINAL_FILE_CAP_BYTES,
        )
    }

    fn create_with_caps(
        root: &Path,
        open: &TranscriptOpen<'_>,
        output_cap: u64,
        file_cap: u64,
    ) -> Option<Self> {
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
            #[cfg(test)]
            path,
            output_cap,
            file_cap,
            state: Mutex::new(TranscriptState {
                file: Some(file),
                written: 0,
                full: false,
                output_bytes: 0,
                output_suppressed: false,
                carry: Vec::new(),
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
        state.write(&value, self.file_cap, false);
    }

    /// Bytes written to the shell's stdin: `by` is the acting principal
    /// (`root` for owner surfaces, `daemon` for the daemon's own writes)
    /// and `via` the surface (`mcp`, `dashboard`, `dashboard-tunnel`,
    /// `daemon`). With `secret` set (echo off in canonical mode: a
    /// password prompt) only the length is recorded.
    pub(crate) fn input(&self, data: &[u8], by: &str, via: &str, secret: bool) {
        let mut record = serde_json::json!({
            "v": RECORD_VERSION,
            "at": now_rfc3339(),
            "ev": "input",
            "by": by,
            "via": via,
            "bytes": data.len(),
        });
        if secret {
            record["redacted"] = serde_json::Value::String("echo_off".to_string());
        } else {
            let (text, truncated) = bounded_text(data, RECORD_TEXT_CAP_BYTES);
            record["text"] = serde_json::Value::String(text);
            if truncated {
                record["truncated"] = serde_json::Value::Bool(true);
            }
        }
        self.record(record);
    }

    /// One chunk of shell output, until the per-incarnation output cap;
    /// the first chunk past it leaves a single `output_suppressed` mark.
    /// A UTF-8 sequence split across chunks is recorded whole with the
    /// later chunk.
    pub(crate) fn output(&self, data: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.file.is_none() {
            return;
        }
        let before = state.output_bytes;
        state.output_bytes = before.saturating_add(data.len() as u64);
        if state.output_suppressed || state.full {
            return;
        }
        if before >= self.output_cap {
            state.output_suppressed = true;
            state.carry.clear();
            let mark = serde_json::json!({
                "v": RECORD_VERSION,
                "at": now_rfc3339(),
                "ev": "output_suppressed",
                "after_bytes": before,
            });
            state.write(&mark, self.file_cap, false);
            return;
        }
        let mut bytes = std::mem::take(&mut state.carry);
        bytes.extend_from_slice(data);
        let complete = complete_utf8_len(&bytes);
        state.carry = bytes.split_off(complete);
        if bytes.is_empty() {
            return;
        }
        let (text, truncated) = bounded_text(&bytes, RECORD_TEXT_CAP_BYTES);
        let mut record = serde_json::json!({
            "v": RECORD_VERSION,
            "at": now_rfc3339(),
            "ev": "output",
            "bytes": bytes.len(),
            "text": text,
        });
        if truncated {
            record["truncated"] = serde_json::Value::Bool(true);
        }
        state.write(&record, self.file_cap, false);
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

    /// The shell exited. `output_bytes` counts the output seen so far, cap
    /// or not (on Windows the exit can arrive before the last output).
    /// Written even past the file cap.
    pub(crate) fn exit(&self, status: i32) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let record = serde_json::json!({
            "v": RECORD_VERSION,
            "at": now_rfc3339(),
            "ev": "exit",
            "status": status,
            "output_bytes": state.output_bytes,
        });
        state.write(&record, self.file_cap, true);
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

/// Queue one record for `root/mcp-calls.jsonl`. Never blocks on disk: the
/// bounded record is built here and handed to a writer thread, and it is
/// dropped (and counted in the next written record) when the queue is
/// full or the thread could not start.
pub(crate) fn record_mcp_call(root: &Path, call: &McpCall<'_>, outcome: McpCallOutcome<'_>) {
    let record = build_mcp_record(call, outcome);
    let writer = mcp_writer();
    let queued = writer
        .sender
        .as_ref()
        .is_some_and(|sender| sender.try_send((root.to_path_buf(), record)).is_ok());
    if !queued {
        writer.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

struct McpWriter {
    sender: Option<std::sync::mpsc::SyncSender<(PathBuf, serde_json::Value)>>,
    dropped: std::sync::Arc<AtomicU64>,
}

fn mcp_writer() -> &'static McpWriter {
    static WRITER: OnceLock<McpWriter> = OnceLock::new();
    WRITER.get_or_init(|| {
        let dropped = std::sync::Arc::new(AtomicU64::new(0));
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<(PathBuf, serde_json::Value)>(MCP_QUEUE_RECORDS);
        let counter = dropped.clone();
        let spawned = std::thread::Builder::new()
            .name("command-log-mcp".to_string())
            .spawn(move || {
                for (root, mut record) in receiver {
                    let missed = counter.swap(0, Ordering::Relaxed);
                    if missed > 0 {
                        record["dropped_before"] = serde_json::json!(missed);
                    }
                    write_mcp_record(&root, &record, MCP_LOG_ROTATE_BYTES, MCP_ROTATED_KEEP);
                }
            });
        McpWriter {
            sender: spawned.ok().map(|_| sender),
            dropped,
        }
    })
}

/// The bounded JSON record of one call. Cheap on large results: sizes are
/// summed from the content blocks, and only a small first text block is
/// parsed for Intendant's `{"ok": false}` convention.
fn build_mcp_record(call: &McpCall<'_>, outcome: McpCallOutcome<'_>) -> serde_json::Value {
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
        "principal": bounded_str(call.principal, MCP_NAME_CAP_BYTES),
        "session": call.session.map(|session| bounded_str(session, MCP_NAME_CAP_BYTES)),
        "tool": bounded_str(call.tool, MCP_NAME_CAP_BYTES),
        "args": args,
        "ms": u64::try_from(call.elapsed.as_millis()).unwrap_or(u64::MAX),
    });
    if args_truncated {
        record["args_truncated"] = serde_json::Value::Bool(true);
        record["args_bytes"] = serde_json::json!(args_text.len());
    }
    let (status, detail) = match outcome {
        McpCallOutcome::Denied(result) => ("denied", result_text(result).map(str::to_string)),
        McpCallOutcome::Result(result) => {
            record["result_bytes"] = serde_json::json!(result_size(result));
            let (is_error, detail) = result_error(result);
            (if is_error { "error" } else { "ok" }, detail)
        }
        McpCallOutcome::Error(message) => ("protocol_error", Some(message.to_string())),
    };
    record["outcome"] = serde_json::Value::String(status.to_string());
    if let Some(detail) = detail {
        record["detail"] = serde_json::Value::String(bounded_str(&detail, MCP_DETAIL_CAP_BYTES));
    }
    record
}

/// Append `record` to `root/mcp-calls.jsonl`, first rotating the file to a
/// uniquely named `mcp-calls.<stamp>-<pid>-<seq>.jsonl` when it has reached
/// `rotate_bytes`, and keeping only the newest `keep` rotated files. Unique
/// names make rotation safe across two daemons appending during a graceful
/// update (neither can replace the other's rotated file).
fn write_mcp_record(root: &Path, record: &serde_json::Value, rotate_bytes: u64, keep: usize) {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let Some(line) = json_line(record) else {
        return;
    };
    if intendant_core::state_paths::create_private_dir_all(root).is_err() {
        return;
    }
    let path = root.join(MCP_LOG_FILE);
    let due = std::fs::metadata(&path)
        .map(|meta| meta.len() >= rotate_bytes)
        .unwrap_or(false);
    if due {
        let rotated = root.join(format!(
            "{MCP_ROTATED_PREFIX}{}-{}-{}.jsonl",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        if std::fs::rename(&path, rotated).is_ok() {
            prune_rotated_mcp_logs(root, keep);
        }
    }
    if let Ok(mut file) = intendant_core::state_paths::private_file_options()
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Keep the newest `keep` rotated MCP files (names sort chronologically).
fn prune_rotated_mcp_logs(root: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut rotated: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(MCP_ROTATED_PREFIX)
                        && name.ends_with(".jsonl")
                        && name != MCP_LOG_FILE
                })
        })
        .collect();
    rotated.sort();
    let excess = rotated.len().saturating_sub(keep);
    for path in rotated.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// The first text block of a `CallToolResult`, if any.
fn result_text(result: &serde_json::Value) -> Option<&str> {
    result
        .get("content")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .find_map(|block| block.get("text").and_then(serde_json::Value::as_str))
}

/// Approximate size of a `CallToolResult`: text and binary payloads of its
/// content blocks plus any structured content, without serializing blobs.
fn result_size(result: &serde_json::Value) -> usize {
    let blocks = result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .map(|block| {
                    block
                        .get("text")
                        .or_else(|| block.get("data"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::len)
                        .unwrap_or(0)
                })
                .sum::<usize>()
        })
        .unwrap_or(0);
    let structured = result
        .get("structuredContent")
        .map(|value| value.to_string().len())
        .unwrap_or(0);
    blocks + structured
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
        .filter(|text| text.len() <= MCP_PARSE_CAP_BYTES)
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
        .or_else(|| text.map(|text| bounded_str(text, MCP_DETAIL_CAP_BYTES)));
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

    fn record_now(root: &Path, call: &McpCall<'_>, outcome: McpCallOutcome<'_>) {
        write_mcp_record(
            root,
            &build_mcp_record(call, outcome),
            MCP_LOG_ROTATE_BYTES,
            MCP_ROTATED_KEEP,
        );
    }

    #[test]
    fn transcript_records_open_input_output_close_and_exit() {
        let root = tempfile::tempdir().unwrap();
        let t = TerminalTranscript::create(root.path(), &open_spec("shell/1 x", root.path()))
            .expect("transcript");
        let name = t.path().file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.ends_with("-shell_1_x-0123456789ab.jsonl"), "{name}");
        t.input(b"ls -la\r", "principal:test", "mcp", false);
        t.output(b"total 0\r\n");
        t.input(b"hunter2\r", "principal:test", "mcp", true);
        t.closed("root");
        t.exit(0);
        let records = read_records(t.path());
        let events: Vec<&str> = records.iter().map(|r| r["ev"].as_str().unwrap()).collect();
        assert_eq!(
            events,
            ["open", "input", "output", "input", "close", "exit"]
        );
        assert_eq!(records[0]["owner"], "principal:test");
        assert_eq!(records[0]["scoped"], true);
        assert_eq!(records[1]["text"], "ls -la\r");
        assert_eq!(records[1]["by"], "principal:test");
        assert_eq!(records[1]["via"], "mcp");
        assert_eq!(records[1]["bytes"], 7);
        assert_eq!(records[2]["text"], "total 0\r\n");
        assert_eq!(records[3]["redacted"], "echo_off");
        assert_eq!(records[3]["bytes"], 8);
        assert!(
            records[3].get("text").is_none(),
            "a password is never stored"
        );
        assert_eq!(records[4]["by"], "root");
        assert_eq!(records[5]["status"], 0);
        assert_eq!(records[5]["output_bytes"], 9);
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
        let call = McpCall {
            principal: "p",
            session: None,
            tool: "whoami",
            args: &args,
            elapsed: Duration::ZERO,
        };
        record_now(&log_root, &call, McpCallOutcome::Error("boom"));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(t.path()), 0o600);
        assert_eq!(mode(&log_root.join(MCP_LOG_FILE)), 0o600);
        assert_eq!(mode(&log_root), 0o700);
        assert_eq!(mode(&log_root.join(TERMINALS_DIR)), 0o700);
    }

    #[test]
    fn output_past_the_cap_is_marked_once_and_inputs_and_exit_survive() {
        let root = tempfile::tempdir().unwrap();
        let t = TerminalTranscript::create_with_caps(
            root.path(),
            &open_spec("t", root.path()),
            10,
            TERMINAL_FILE_CAP_BYTES,
        )
        .unwrap();
        t.output(b"0123456789AB"); // crosses the cap: kept whole
        t.output(b"more"); // past the cap: one mark
        t.output(b"even more"); // still past: nothing
        t.input(b"exit\r", "root", "dashboard", false);
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
    fn a_full_transcript_stops_with_one_mark_but_keeps_the_exit() {
        let root = tempfile::tempdir().unwrap();
        let t = TerminalTranscript::create_with_caps(
            root.path(),
            &open_spec("t", root.path()),
            TERMINAL_OUTPUT_CAP_BYTES,
            600,
        )
        .unwrap();
        for _ in 0..20 {
            t.input(&[b'x'; 40], "principal:test", "mcp", false);
        }
        t.output(b"late output");
        t.exit(0);
        let records = read_records(t.path());
        let events: Vec<&str> = records.iter().map(|r| r["ev"].as_str().unwrap()).collect();
        let full = events.iter().position(|e| *e == "transcript_full").unwrap();
        assert_eq!(
            events.iter().filter(|e| **e == "transcript_full").count(),
            1
        );
        assert_eq!(&events[full + 1..], ["exit"]);
        assert!(std::fs::metadata(t.path()).unwrap().len() < 900);
    }

    #[test]
    fn output_keeps_utf8_sequences_split_across_chunks_whole() {
        let root = tempfile::tempdir().unwrap();
        let t = TerminalTranscript::create(root.path(), &open_spec("t", root.path())).unwrap();
        let euro = "€".as_bytes(); // three bytes
        t.output(&[b'a', euro[0]]);
        t.output(&euro[1..]);
        t.output(b"\xffz");
        let records = read_records(t.path());
        let texts: Vec<&str> = records[1..]
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["a", "€", "\u{fffd}z"]);
    }

    #[test]
    fn complete_utf8_len_holds_back_only_an_incomplete_tail() {
        assert_eq!(complete_utf8_len(b"abc"), 3);
        assert_eq!(complete_utf8_len(&[b'a', 0xE2, 0x82]), 1);
        assert_eq!(complete_utf8_len(&[0xff, b'a', 0xE2]), 2);
        assert_eq!(complete_utf8_len(&[b'a', 0xff]), 2);
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
    fn mcp_calls_record_outcomes_and_bound_caller_strings() {
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
        let long_tool = "t".repeat(MCP_NAME_CAP_BYTES * 4);
        let call = |tool, args| McpCall {
            principal: "principal:local-process:loopback",
            session: None,
            tool,
            args,
            elapsed: Duration::from_millis(42),
        };
        record_now(
            root.path(),
            &call("inspect", &small),
            McpCallOutcome::Result(&ok),
        );
        record_now(
            root.path(),
            &call("terminal_write", &big),
            McpCallOutcome::Result(&failed),
        );
        record_now(
            root.path(),
            &call("act", &small),
            McpCallOutcome::Result(&flagged),
        );
        record_now(
            root.path(),
            &call(&long_tool, &small),
            McpCallOutcome::Denied(&denied),
        );
        record_now(
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
        assert_eq!(records[0]["result_bytes"], 11);
        assert!(records[0].get("detail").is_none());
        assert_eq!(records[1]["args_truncated"], true);
        assert!(records[1]["args"].as_str().unwrap().len() <= MCP_ARGS_CAP_BYTES);
        assert_eq!(records[1]["detail"], "shell already exited");
        assert_eq!(records[2]["detail"], "plain failure");
        assert_eq!(records[3]["detail"], "permission denied: terminal.write");
        assert_eq!(
            records[3]["tool"].as_str().unwrap().len(),
            MCP_NAME_CAP_BYTES
        );
        assert!(records[3].get("result_bytes").is_none());
        assert_eq!(records[4]["detail"], "bad params");
    }

    #[test]
    fn result_size_does_not_serialize_blobs() {
        let result = serde_json::json!({"content": [
            {"type": "text", "text": "abc"},
            {"type": "image", "data": "QUJDRA==", "mimeType": "image/png"},
        ]});
        assert_eq!(result_size(&result), 3 + 8);
    }

    #[test]
    fn mcp_log_rotates_to_unique_files_and_keeps_the_newest() {
        let root = tempfile::tempdir().unwrap();
        let record = serde_json::json!({"v": 1, "tool": "whoami"});
        for _ in 0..5 {
            write_mcp_record(root.path(), &record, 1, 2);
        }
        assert_eq!(read_records(&root.path().join(MCP_LOG_FILE)).len(), 1);
        let rotated: Vec<String> = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != MCP_LOG_FILE)
            .collect();
        assert_eq!(rotated.len(), 2, "{rotated:?}");
        assert!(rotated
            .iter()
            .all(|name| name.starts_with(MCP_ROTATED_PREFIX)));
    }

    #[test]
    fn queued_records_reach_the_file() {
        let root = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"argv": ["status"]});
        record_mcp_call(
            root.path(),
            &McpCall {
                principal: "p",
                session: None,
                tool: "inspect",
                args: &args,
                elapsed: Duration::ZERO,
            },
            McpCallOutcome::Error("queued"),
        );
        let path = root.path().join(MCP_LOG_FILE);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(read_records(&path)[0]["detail"], "queued");
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

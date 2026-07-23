//! Keyless scripted [`ChatProvider`] for headless end-to-end tests and
//! demos.
//!
//! Never auto-selected: only an explicit `PROVIDER=mock` opts in
//! (`provider::select_provider`), with the script supplied via
//! `INTENDANT_MOCK_SCRIPT=<path to JSON>`. The provider makes no network
//! calls and needs no API key, so the full production stack — CLI, agent
//! loop, tool dispatch, the sandboxed runtime subprocess, session logging,
//! the daemon — can run under CI exactly as shipped (see `tests/e2e/`).
//!
//! A script is a set of **profiles**, each a linear sequence of scripted
//! responses. Every session constructs its own provider instance
//! (`select_provider` per session), picks the first profile whose `match`
//! string appears in its conversation (falling back to a `match`-less
//! profile), and serves that profile's steps in order. Profiles are how one
//! script drives an orchestrator parent and its sub-agent children to
//! different behavior. Steps fail loudly: an unmet
//! `expect_transcript_contains` or a chat call past the last step returns a
//! provider error rather than improvising, so a drifted test dies instead
//! of green-looping.
//!
//! ```json
//! {
//!   "model": "mock-1",
//!   "profiles": [{
//!     "match": "",
//!     "steps": [
//!       { "content": "Running.",
//!         "tool_calls": [{ "name": "exec_command",
//!                          "arguments": { "nonce": 1, "command": "echo HI" } }] },
//!       { "expect_transcript_contains": "HI",
//!         "content": "Done.",
//!         "tool_calls": [{ "name": "signal_done",
//!                          "arguments": { "message": "complete" } }] }
//!     ]
//!   }]
//! }
//! ```

use crate::conversation::Message;
use crate::error::CallerError;
use crate::provider::{ChatProvider, ChatResponse, TokenUsage, ToolCall};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

pub const MOCK_SCRIPT_ENV: &str = "INTENDANT_MOCK_SCRIPT";
/// Opt-in request tap for prefix-stability e2es (coordination §2.6):
/// when set to a path, every chat call appends one JSONL record
/// `{"n":…,"messages_json":"…"}` whose `messages_json` field is the
/// exact serde serialization of the request's message Vec — so two
/// consecutive requests can be byte-compared for the tail-append-only
/// property. Test-only and off by default; requests are recorded as
/// received, before any script bookkeeping.
pub const MOCK_REQUEST_DUMP_ENV: &str = "INTENDANT_MOCK_REQUEST_DUMP";
const MOCK_FILE_BARRIER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct MockScript {
    #[serde(default)]
    model: Option<String>,
    profiles: Vec<MockProfile>,
}

#[derive(Debug, Deserialize)]
struct MockProfile {
    /// Substring that selects this profile from the conversation text; an
    /// empty (or omitted) match is the fallback profile.
    #[serde(default, rename = "match")]
    match_text: String,
    steps: Vec<MockStep>,
}

#[derive(Debug, Deserialize)]
struct MockStep {
    /// Assert the transcript contains this before answering — proves a
    /// prior tool result actually round-tripped through the runtime.
    #[serde(default)]
    expect_transcript_contains: Option<String>,
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<MockScriptToolCall>,
    /// Prompt-cache TTL stated by this step's usage (default 300). Smokes
    /// use short TTLs to walk the cache countdown → expiry-alert → cold
    /// pipeline on real timing.
    #[serde(default)]
    cache_ttl_seconds: Option<u32>,
    /// When set, this step's usage carries a "5h" rate-limit window at
    /// this used percentage (resets two hours out) — exercises the vitals
    /// limit gauges keyless.
    #[serde(default)]
    limit_used_pct: Option<u8>,
    /// Optional provider status for the scripted "5h" window
    /// ("allowed_warning", "rejected", …) — exercises the status-driven
    /// severity path (warning ingestion) keyless. Only meaningful
    /// alongside `limit_used_pct`.
    #[serde(default)]
    limit_status: Option<String>,
    /// Scripted think-time before this step answers. E2e rigs use it to
    /// hold a boot-started task's first step until their dashboard
    /// connection is up — nothing replays missed rail events (e.g.
    /// `user_question`) to late-joining websockets.
    #[serde(default)]
    delay_ms: u64,
    /// Optional deterministic test barrier. The step does not answer until
    /// this file exists, or fails loudly after a bounded wait. This is the
    /// race-free counterpart to `delay_ms` for boot-started tasks whose
    /// non-replayed rail events require a client to attach first.
    #[serde(default)]
    wait_for_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct MockScriptToolCall {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

#[derive(Debug)]
pub struct MockProvider {
    model: String,
    script: MockScript,
    /// (selected profile, next step) — chosen on first chat() call.
    cursor: Mutex<Option<(usize, usize)>>,
    /// Per-instance request ordinal for the [`MOCK_REQUEST_DUMP_ENV`] tap.
    dump_seq: AtomicUsize,
}

impl MockProvider {
    /// Construct from `INTENDANT_MOCK_SCRIPT`. Errors are configuration
    /// errors: mock is only ever explicitly selected, so a missing or
    /// malformed script is the operator's to fix.
    pub fn from_env() -> Result<Self, CallerError> {
        let path = std::env::var(MOCK_SCRIPT_ENV).map_err(|_| {
            CallerError::Config(format!(
                "PROVIDER=mock requires {MOCK_SCRIPT_ENV}=<path to script JSON>"
            ))
        })?;
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| CallerError::Config(format!("mock script {path} is unreadable: {e}")))?;
        Self::from_json(&raw)
    }

    pub fn from_json(raw: &str) -> Result<Self, CallerError> {
        let script: MockScript = serde_json::from_str(raw)
            .map_err(|e| CallerError::Config(format!("mock script is invalid JSON: {e}")))?;
        if script.profiles.is_empty() {
            return Err(CallerError::Config(
                "mock script declares no profiles".to_string(),
            ));
        }
        Ok(Self {
            model: script.model.clone().unwrap_or_else(|| "mock-1".to_string()),
            script,
            cursor: Mutex::new(None),
            dump_seq: AtomicUsize::new(0),
        })
    }

    /// The [`MOCK_REQUEST_DUMP_ENV`] tap: append one JSONL record for a
    /// request as received. The env read lives at this transport edge;
    /// tests exercise [`Self::dump_request_to`] with an injected path
    /// (hermetic — no process-global env mutation).
    fn dump_request_if_enabled(&self, messages: &[Message]) -> Result<(), CallerError> {
        match std::env::var(MOCK_REQUEST_DUMP_ENV) {
            Ok(path) if !path.is_empty() => self.dump_request_to(&path, messages),
            _ => Ok(()),
        }
    }

    /// Append one `{"n":…,"model":…,"messages_json":…}` JSONL record.
    /// Failures are loud config errors — the tap only exists inside
    /// tests, where a silent miss would fake a pass.
    fn dump_request_to(&self, path: &str, messages: &[Message]) -> Result<(), CallerError> {
        use std::io::Write;
        let n = self.dump_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let messages_json = serde_json::to_string(messages)
            .map_err(|e| CallerError::Config(format!("mock request dump serialize: {e}")))?;
        let record =
            serde_json::json!({ "n": n, "model": self.model, "messages_json": messages_json });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| CallerError::Config(format!("mock request dump {path}: {e}")))?;
        writeln!(file, "{record}")
            .map_err(|e| CallerError::Config(format!("mock request dump {path}: {e}")))
    }

    /// First profile whose `match` appears in the conversation, else the
    /// first match-less profile.
    fn select_profile(&self, transcript: &str) -> Option<usize> {
        self.script
            .profiles
            .iter()
            .position(|profile| {
                !profile.match_text.is_empty() && transcript.contains(&profile.match_text)
            })
            .or_else(|| {
                self.script
                    .profiles
                    .iter()
                    .position(|profile| profile.match_text.is_empty())
            })
    }

    fn tool_call(scripted: &MockScriptToolCall) -> ToolCall {
        static NEXT_CALL: AtomicUsize = AtomicUsize::new(1);
        let n = NEXT_CALL.fetch_add(1, Ordering::Relaxed);
        ToolCall {
            id: format!("mock_call_{n}"),
            call_id: format!("mock_call_{n}"),
            name: scripted.name.clone(),
            arguments: scripted.arguments.to_string(),
        }
    }
}

#[async_trait]
impl ChatProvider for MockProvider {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        self.dump_request_if_enabled(messages)?;
        let transcript: String = messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        // The guard protects only the cursor, and it must not live across
        // the think-time await below — scope it (async-Send analysis does
        // not credit an explicit drop).
        let (profile_index, step_index) = {
            let mut cursor = self.cursor.lock().expect("mock cursor poisoned");
            let (profile_index, step_index) = match *cursor {
                Some(state) => state,
                None => {
                    let profile = self.select_profile(&transcript).ok_or_else(|| {
                        CallerError::Config(
                            "mock script has no profile matching this conversation and no \
                             fallback profile (empty match)"
                                .to_string(),
                        )
                    })?;
                    (profile, 0)
                }
            };

            let profile = &self.script.profiles[profile_index];
            let Some(step) = profile.steps.get(step_index) else {
                return Err(CallerError::Config(format!(
                    "mock script exhausted: profile {profile_index} (match {:?}) has only {} steps \
                     but the loop asked for step {} — scripts must end in signal_done/submit_result",
                    profile.match_text,
                    profile.steps.len(),
                    step_index + 1,
                )));
            };

            if let Some(expected) = &step.expect_transcript_contains {
                if !transcript.contains(expected) {
                    return Err(CallerError::Config(format!(
                        "mock expectation failed at profile {profile_index} step {step_index}: \
                         transcript does not contain {expected:?}"
                    )));
                }
            }

            *cursor = Some((profile_index, step_index + 1));
            (profile_index, step_index)
        };
        let profile = &self.script.profiles[profile_index];
        let step = &profile.steps[step_index];
        if step.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(step.delay_ms)).await;
        }
        if let Some(path) = &step.wait_for_file {
            let deadline = tokio::time::Instant::now() + MOCK_FILE_BARRIER_TIMEOUT;
            loop {
                match std::fs::metadata(path) {
                    Ok(_) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(CallerError::Config(format!(
                            "mock step file barrier {} is unreadable: {error}",
                            path.display()
                        )));
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(CallerError::Config(format!(
                        "mock step timed out after {}s waiting for file barrier {}",
                        MOCK_FILE_BARRIER_TIMEOUT.as_secs(),
                        path.display()
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        // Plausible non-zero usage so budget and usage accounting run.
        // Cache counters emulate a warm prompt cache (first request writes,
        // later requests read half) so cache-vitals plumbing runs keyless.
        let prompt_tokens = (transcript.len() as u64 / 4).max(1);
        let completion_tokens = (step.content.len() as u64 / 4).max(1);
        let cached_tokens = if step_index == 0 {
            0
        } else {
            prompt_tokens / 2
        };
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let rate_limit_windows = step
            .limit_used_pct
            .map(|used_pct| {
                vec![crate::types::SessionLimitWindow {
                    label: "5h".to_string(),
                    used_pct: Some(used_pct),
                    resets_at_epoch: Some(now_epoch + 7200),
                    status: step.limit_status.clone(),
                    observed_at_epoch: Some(now_epoch),
                }]
            })
            .unwrap_or_default();
        Ok(ChatResponse {
            content: step.content.clone(),
            usage: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cached_tokens,
                cache_creation_tokens: prompt_tokens / 4,
                cache_ttl_seconds: Some(step.cache_ttl_seconds.unwrap_or(300)),
                rate_limit_windows,
            },
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls: step.tool_calls.iter().map(Self::tool_call).collect(),
            cu_calls: Vec::new(),
            raw_output: None,
        })
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn context_window(&self) -> u64 {
        200_000
    }

    fn max_output_tokens(&self) -> u64 {
        8_192
    }

    fn use_tools(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> Message {
        Message {
            role: role.to_string(),
            content: content.to_string(),
            ..Default::default()
        }
    }

    fn two_profile_script() -> MockProvider {
        MockProvider::from_json(
            r#"{
                "model": "mock-test",
                "profiles": [
                    { "match": "CHILD-TASK", "steps": [
                        { "content": "child answer",
                          "tool_calls": [{ "name": "submit_result",
                                           "arguments": { "status": "completed" } }] }
                    ]},
                    { "match": "", "steps": [
                        { "content": "step one",
                          "tool_calls": [{ "name": "exec_command",
                                           "arguments": { "nonce": 1, "command": "echo HI" } }] },
                        { "expect_transcript_contains": "HI",
                          "content": "step two",
                          "tool_calls": [{ "name": "signal_done", "arguments": {} }] }
                    ]}
                ]
            }"#,
        )
        .expect("valid script")
    }

    #[tokio::test]
    async fn serves_fallback_profile_steps_in_order() {
        let provider = two_profile_script();
        let mut conversation = vec![
            message("system", "you are an agent"),
            message("user", "do the thing"),
        ];

        let first = provider.chat(&conversation).await.expect("step one");
        assert_eq!(first.content, "step one");
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(first.tool_calls[0].name, "exec_command");
        assert!(first.usage.total_tokens > 0);

        conversation.push(message("tool", "exit 0\nHI"));
        let second = provider.chat(&conversation).await.expect("step two");
        assert_eq!(second.tool_calls[0].name, "signal_done");

        let exhausted = provider.chat(&conversation).await;
        assert!(exhausted
            .unwrap_err()
            .to_string()
            .contains("mock script exhausted"));
    }

    #[tokio::test]
    async fn matching_profile_wins_over_fallback() {
        let provider = two_profile_script();
        let conversation = [
            message("system", "sub-agent"),
            message("user", "CHILD-TASK: go"),
        ];
        let response = provider.chat(&conversation).await.expect("child step");
        assert_eq!(response.content, "child answer");
        assert_eq!(response.tool_calls[0].name, "submit_result");
    }

    #[tokio::test]
    async fn unmet_expectation_is_a_loud_error_not_an_answer() {
        let provider = two_profile_script();
        let mut conversation = vec![message("user", "do the thing")];
        provider.chat(&conversation).await.expect("step one");
        conversation.push(message("tool", "exit 1 — command not found"));
        let err = provider.chat(&conversation).await.unwrap_err().to_string();
        assert!(err.contains("mock expectation failed"), "{err}");
    }

    #[tokio::test]
    async fn file_barrier_holds_a_step_until_the_test_releases_it() {
        let dir = tempfile::tempdir().expect("barrier temp dir");
        let barrier = dir.path().join("client-attached");
        let provider = std::sync::Arc::new(
            MockProvider::from_json(
                &serde_json::json!({
                    "profiles": [{
                        "steps": [{
                            "wait_for_file": barrier,
                            "content": "released"
                        }]
                    }]
                })
                .to_string(),
            )
            .expect("valid barrier script"),
        );
        let chat = {
            let provider = std::sync::Arc::clone(&provider);
            tokio::spawn(async move {
                provider
                    .chat(&[message("user", "wait for the client")])
                    .await
            })
        };

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !chat.is_finished(),
            "step escaped before its barrier existed"
        );
        std::fs::write(&barrier, b"ready").expect("release barrier");
        let response = tokio::time::timeout(std::time::Duration::from_secs(1), chat)
            .await
            .expect("barrier release should wake the step")
            .expect("chat task should not panic")
            .expect("released step should answer");
        assert_eq!(response.content, "released");
    }

    /// The §2.6 request tap: consecutive requests dump their exact
    /// message-Vec serialization, and a pure tail append shows up as a
    /// byte-identical prefix (the same mechanism the coordination
    /// prefix e2e asserts through the real binaries, which set
    /// [`MOCK_REQUEST_DUMP_ENV`] on the child daemon's environment).
    #[test]
    fn request_dump_records_byte_comparable_serializations() {
        let dir = tempfile::tempdir().expect("dump dir");
        let dump = dir.path().join("requests.jsonl");
        let dump_path = dump.to_str().expect("utf-8 temp path");
        let provider = MockProvider::from_json(
            r#"{ "profiles": [{ "steps": [ { "content": "one" }, { "content": "two" } ] }] }"#,
        )
        .expect("valid script");

        let mut conversation = vec![message("system", "prompt"), message("user", "task")];
        provider
            .dump_request_to(dump_path, &conversation)
            .expect("first dump");
        conversation.push(message("assistant", "one"));
        conversation.push(message("user", "[System] coordination v1 space=test tail"));
        provider
            .dump_request_to(dump_path, &conversation)
            .expect("second dump");

        let dumped = std::fs::read_to_string(&dump).expect("dump file");
        let records: Vec<serde_json::Value> = dumped
            .lines()
            .map(|line| serde_json::from_str(line).expect("dump line parses"))
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["n"], 1);
        assert_eq!(records[1]["n"], 2);
        let first = records[0]["messages_json"].as_str().expect("first json");
        let second = records[1]["messages_json"].as_str().expect("second json");
        let first_open = first.strip_suffix(']').expect("array serialization");
        assert!(
            second.starts_with(first_open),
            "prior request must be a byte prefix:\n{first}\n{second}"
        );
        assert_eq!(second.as_bytes()[first_open.len()], b',', "pure append");
        let second_messages: Vec<serde_json::Value> =
            serde_json::from_str(second).expect("second parses");
        assert_eq!(
            second_messages.last().and_then(|m| m["content"].as_str()),
            Some("[System] coordination v1 space=test tail"),
            "the appended tail is the last message"
        );
    }

    #[test]
    fn missing_script_env_and_bad_json_are_config_errors() {
        assert!(MockProvider::from_json("not json")
            .unwrap_err()
            .to_string()
            .contains("invalid JSON"));
        assert!(MockProvider::from_json(r#"{"profiles": []}"#)
            .unwrap_err()
            .to_string()
            .contains("no profiles"));
    }
}

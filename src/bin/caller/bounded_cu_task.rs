//! A small, fail-closed computer-use loop for externally orchestrated proof work.
//!
//! Unlike an Intendant agent session, this lane has no shell, filesystem,
//! browser automation, delegation, escalation, or arbitrary function tools.
//! It starts from one exact display screenshot, permits only native computer
//! actions during `stage`, permits no action at all during `attest`, and returns
//! a compact receipt whose transcript contains hashes and action kinds rather
//! than potentially sensitive screen or typed text.

use async_trait::async_trait;
use base64::Engine as _;
use serde::de::{self, DeserializeSeed as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::computer_use::{CuAction, CuActionStatus};
use crate::conversation::{Conversation, ImageData, MessageProvenance, ToolCallRef};
use crate::provider::{ChatProvider, ChatResponse};

pub(crate) const BOUNDED_CU_MAX_TASK_BYTES: usize = 16 * 1024;
pub(crate) const BOUNDED_CU_MAX_RESULT_BYTES: usize = 16 * 1024;
const BOUNDED_CU_MAX_BATCH_ACTIONS: usize = 16;
const BOUNDED_CU_MAX_TOTAL_ACTIONS: u64 = 64;
const BOUNDED_CU_MAX_ACTION_PAYLOAD_BYTES: usize = 64 * 1024;
const BOUNDED_CU_MAX_TRANSCRIPT_EVENTS: usize = 512;
const BOUNDED_CU_MAX_TRANSCRIPT_DETAIL_BYTES: usize = 4 * 1024;
const BOUNDED_CU_STAGE_MAX_TURNS: u32 = 12;
const BOUNDED_CU_ATTEST_MAX_TURNS: u32 = 2;
const BOUNDED_CU_STAGE_TIMEOUT: Duration = Duration::from_secs(180);
const BOUNDED_CU_ATTEST_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_BINDING_BYTES: usize = 256;
const BOUNDED_CU_MAX_ISSUED_STAGE_RECEIPTS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BoundedCuTaskMode {
    Stage,
    Attest,
}

impl BoundedCuTaskMode {
    fn max_turns(self) -> u32 {
        match self {
            Self::Stage => BOUNDED_CU_STAGE_MAX_TURNS,
            Self::Attest => BOUNDED_CU_ATTEST_MAX_TURNS,
        }
    }

    fn timeout(self) -> Duration {
        match self {
            Self::Stage => BOUNDED_CU_STAGE_TIMEOUT,
            Self::Attest => BOUNDED_CU_ATTEST_TIMEOUT,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Stage => "stage",
            Self::Attest => "attest",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedCuTaskRequest {
    pub(crate) mode: BoundedCuTaskMode,
    pub(crate) attempt_id: String,
    pub(crate) workspace_id: String,
    pub(crate) display_id: u32,
    pub(crate) display_target: String,
    pub(crate) capture_generation: String,
    pub(crate) task: String,
    pub(crate) prior_receipt_id: Option<String>,
    pub(crate) prior_transcript_event_count: Option<u64>,
    pub(crate) prior_transcript_sha256: Option<String>,
    pub(crate) observation_sha256: Option<String>,
    pub(crate) prior_completed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BoundedCuTaskReceipt {
    pub(crate) schema_version: u32,
    pub(crate) receipt_id: String,
    pub(crate) mode: BoundedCuTaskMode,
    pub(crate) attempt_id: String,
    pub(crate) workspace_id: String,
    pub(crate) display_id: u32,
    pub(crate) display_target: String,
    pub(crate) capture_generation: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) started_at: String,
    pub(crate) completed_at: String,
    pub(crate) elapsed_ms: u64,
    pub(crate) turns: u32,
    pub(crate) cu_batch_count: u64,
    pub(crate) action_count: u64,
    pub(crate) input_event_count: u64,
    pub(crate) post_observation_input_event_count: u64,
    pub(crate) forbidden_tool_call_count: u64,
    pub(crate) escalation_count: u64,
    pub(crate) initial_frame_sha256: String,
    pub(crate) prior_receipt_id: Option<String>,
    pub(crate) prior_transcript_event_count: Option<u64>,
    pub(crate) prior_transcript_sha256: Option<String>,
    pub(crate) observation_sha256: Option<String>,
    pub(crate) current_transcript_event_count: u64,
    pub(crate) current_transcript: Vec<TranscriptEvent>,
    pub(crate) transcript_event_count: u64,
    pub(crate) transcript_sha256: String,
    pub(crate) task_sha256: String,
    pub(crate) result_sha256: String,
    pub(crate) result: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedCuTaskError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) retryable: bool,
}

impl BoundedCuTaskError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }
}

#[derive(Debug)]
pub(crate) struct BoundedCuActionOutcome {
    pub(crate) model_summary: String,
    pub(crate) screenshot: ImageData,
    pub(crate) statuses: Vec<CuActionStatus>,
}

#[async_trait]
pub(crate) trait BoundedCuActionExecutor: Send {
    async fn execute(
        &mut self,
        actions: &[CuAction],
    ) -> Result<BoundedCuActionOutcome, BoundedCuTaskError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TranscriptEvent {
    pub(crate) sequence: u64,
    pub(crate) turn: u32,
    pub(crate) kind: String,
    pub(crate) detail: String,
}

#[derive(Default)]
struct Transcript {
    events: Vec<TranscriptEvent>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RedactedActionProjection<'a> {
    Click {
        x: i32,
        y: i32,
        button: &'a crate::computer_use::MouseButton,
    },
    DoubleClick {
        x: i32,
        y: i32,
        button: &'a crate::computer_use::MouseButton,
    },
    TripleClick {
        x: i32,
        y: i32,
        button: &'a crate::computer_use::MouseButton,
    },
    MouseDown {
        x: i32,
        y: i32,
        button: &'a crate::computer_use::MouseButton,
    },
    MouseUp {
        x: i32,
        y: i32,
        button: &'a crate::computer_use::MouseButton,
    },
    Type,
    Paste,
    Key,
    HoldKey {
        ms: u64,
    },
    Scroll {
        x: i32,
        y: i32,
        direction: &'a crate::computer_use::ScrollDirection,
        amount: i32,
    },
    MoveMouse {
        x: i32,
        y: i32,
    },
    Drag {
        start_x: i32,
        start_y: i32,
        end_x: i32,
        end_y: i32,
    },
    Screenshot,
    Zoom {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    Wait {
        ms: u64,
    },
}

impl Transcript {
    fn push(&mut self, turn: u32, kind: &'static str, detail: String) {
        self.events.push(TranscriptEvent {
            sequence: self.events.len() as u64 + 1,
            turn,
            kind: kind.to_string(),
            detail,
        });
    }

    fn digest(&self, request: &BoundedCuTaskRequest) -> Result<String, BoundedCuTaskError> {
        transcript_digest(&self.events, request.prior_transcript_sha256.as_deref())
    }
}

fn transcript_digest(
    events: &[TranscriptEvent],
    prior_transcript_sha256: Option<&str>,
) -> Result<String, BoundedCuTaskError> {
    let bytes = serde_json::to_vec(events).map_err(|error| {
        BoundedCuTaskError::new(
            "bounded-cu-transcript-serialization-failed",
            error.to_string(),
            false,
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"intendant-bounded-cu-transcript-v1\0");
    if let Some(prior) = prior_transcript_sha256 {
        hasher.update(prior.as_bytes());
    }
    hasher.update(b"\0");
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_transcript_events(events: &[TranscriptEvent]) -> Result<(), BoundedCuTaskError> {
    if events.is_empty() || events.len() > BOUNDED_CU_MAX_TRANSCRIPT_EVENTS {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-transcript-shape-invalid",
            "redacted transcript event cardinality was outside its closed bound",
            false,
        ));
    }
    for (index, event) in events.iter().enumerate() {
        let sequence = u64::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1));
        if sequence != Some(event.sequence)
            || event.turn > BOUNDED_CU_STAGE_MAX_TURNS
            || !matches!(
                event.kind.as_str(),
                "request"
                    | "initial_frame"
                    | "provider_response"
                    | "cu_batch"
                    | "result"
                    | "invalid_result"
            )
            || event.detail.is_empty()
            || event.detail.len() > BOUNDED_CU_MAX_TRANSCRIPT_DETAIL_BYTES
            || !event
                .detail
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-transcript-shape-invalid",
                "redacted transcript event was unordered, unknown, controlled, or overlong",
                false,
            ));
        }
    }
    Ok(())
}

struct TaskCounters {
    turns: u32,
    cu_batches: u64,
    actions: u64,
    inputs: u64,
}

impl TaskCounters {
    fn new() -> Self {
        Self {
            turns: 0,
            cu_batches: 0,
            actions: 0,
            inputs: 0,
        }
    }
}

fn issued_stage_receipts() -> &'static std::sync::Mutex<VecDeque<BoundedCuTaskReceipt>> {
    static RECEIPTS: std::sync::OnceLock<std::sync::Mutex<VecDeque<BoundedCuTaskReceipt>>> =
        std::sync::OnceLock::new();
    RECEIPTS.get_or_init(|| std::sync::Mutex::new(VecDeque::new()))
}

pub(crate) fn remember_issued_stage_receipt(
    receipt: &BoundedCuTaskReceipt,
) -> Result<(), BoundedCuTaskError> {
    let mut receipts = issued_stage_receipts().lock().map_err(|_| {
        BoundedCuTaskError::new(
            "bounded-cu-receipt-registry-unavailable",
            "issued stage receipt registry was poisoned",
            false,
        )
    })?;
    receipts.retain(|existing| existing.receipt_id != receipt.receipt_id);
    receipts.push_back(receipt.clone());
    while receipts.len() > BOUNDED_CU_MAX_ISSUED_STAGE_RECEIPTS {
        receipts.pop_front();
    }
    Ok(())
}

fn bind_issued_stage_receipt(request: &mut BoundedCuTaskRequest) -> Result<(), BoundedCuTaskError> {
    let prior_receipt_id = request.prior_receipt_id.as_deref().ok_or_else(|| {
        BoundedCuTaskError::new(
            "bounded-cu-attestation-lineage-invalid",
            "attest mode requires an issued stage receipt ID",
            false,
        )
    })?;
    let prior = issued_stage_receipts()
        .lock()
        .map_err(|_| {
            BoundedCuTaskError::new(
                "bounded-cu-receipt-registry-unavailable",
                "issued stage receipt registry was poisoned",
                false,
            )
        })?
        .iter()
        .find(|receipt| receipt.receipt_id == prior_receipt_id)
        .cloned()
        .ok_or_else(|| {
            BoundedCuTaskError::new(
                "bounded-cu-prior-receipt-not-issued",
                "the referenced stage receipt was not issued by this daemon lifetime",
                false,
            )
        })?;
    if prior.schema_version != 1
        || prior.mode != BoundedCuTaskMode::Stage
        || receipt_id(&prior)? != prior.receipt_id
        || prior.attempt_id != request.attempt_id
        || prior.workspace_id != request.workspace_id
        || prior.display_id != request.display_id
        || prior.display_target != request.display_target
        || prior.capture_generation != request.capture_generation
        || prior.prior_receipt_id.is_some()
        || prior.prior_transcript_event_count.is_some()
        || prior.prior_transcript_sha256.is_some()
        || prior.observation_sha256.is_some()
        || prior.current_transcript_event_count != prior.transcript_event_count
        || prior.current_transcript_event_count != prior.current_transcript.len() as u64
        || validate_transcript_events(&prior.current_transcript).is_err()
        || transcript_digest(&prior.current_transcript, None)? != prior.transcript_sha256
    {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-prior-receipt-binding-mismatch",
            "issued stage receipt did not match the exact attempt, workspace, and display generation",
            false,
        ));
    }
    request.prior_transcript_event_count = Some(prior.transcript_event_count);
    request.prior_transcript_sha256 = Some(prior.transcript_sha256);
    request.prior_completed_at = Some(prior.completed_at);
    Ok(())
}

/// Run one strictly bounded CU-only task.
pub(crate) async fn run_bounded_cu_task(
    provider: &dyn ChatProvider,
    executor: &mut dyn BoundedCuActionExecutor,
    mut request: BoundedCuTaskRequest,
) -> Result<BoundedCuTaskReceipt, BoundedCuTaskError> {
    validate_bounded_cu_task_request(&request)?;
    if request.mode == BoundedCuTaskMode::Attest {
        bind_issued_stage_receipt(&mut request)?;
    }
    let timeout = request.mode.timeout();
    match tokio::time::timeout(timeout, run_inner(provider, executor, request)).await {
        Ok(Ok(receipt)) => Ok(receipt),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(BoundedCuTaskError::new(
            "bounded-cu-deadline-exceeded",
            format!(
                "bounded CU task exceeded its fixed {}s deadline",
                timeout.as_secs()
            ),
            true,
        )),
    }
}

async fn run_inner(
    provider: &dyn ChatProvider,
    executor: &mut dyn BoundedCuActionExecutor,
    mut request: BoundedCuTaskRequest,
) -> Result<BoundedCuTaskReceipt, BoundedCuTaskError> {
    if !provider.cu_enabled() {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-provider-not-capable",
            "selected provider does not expose native computer use",
            false,
        ));
    }
    let started_at = now_rfc3339();
    if let Some(prior_completed_at) = request.prior_completed_at.as_deref() {
        let prior = chrono::DateTime::parse_from_rfc3339(prior_completed_at).map_err(|_| {
            BoundedCuTaskError::new(
                "bounded-cu-prior-receipt-time-invalid",
                "issued stage receipt had an invalid completion timestamp",
                false,
            )
        })?;
        let started = chrono::DateTime::parse_from_rfc3339(&started_at).expect("own RFC3339 time");
        if started < prior {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-attestation-chronology-invalid",
                "attestation began before its issued stage receipt completed",
                false,
            ));
        }
    }
    let started_monotonic = Instant::now();
    let task_sha256 = sha256(request.task.as_bytes());
    let mut transcript = Transcript::default();
    transcript.push(
        0,
        "request",
        format!(
            "mode={};task_sha256={task_sha256};display_id={}",
            request.mode.as_str(),
            request.display_id
        ),
    );

    let initial = executor.execute(&[CuAction::Screenshot]).await?;
    if initial.statuses.as_slice() != [CuActionStatus::Verified] {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-initial-frame-failed",
            "the exact bound display could not produce a verified initial frame",
            false,
        ));
    }
    let initial_frame_sha256 = image_sha256(&initial.screenshot)?;
    if request.mode == BoundedCuTaskMode::Attest {
        request.observation_sha256 = Some(initial_frame_sha256.clone());
    }
    transcript.push(
        0,
        "initial_frame",
        format!(
            "sha256={initial_frame_sha256};statuses={}",
            status_labels(&initial.statuses)
        ),
    );
    let system_prompt = bounded_system_prompt(request.mode);
    let mut conversation = Conversation::new(system_prompt, provider.context_window());
    conversation.add_user_with_images(
        MessageProvenance::Task,
        request.task.clone(),
        vec![initial.screenshot],
    );

    let mut counters = TaskCounters::new();
    let max_turns = request.mode.max_turns();
    for turn in 1..=max_turns {
        counters.turns = turn;
        if provider.requires_image_stripping() {
            conversation.strip_old_images();
        }
        let response = provider
            .chat(conversation.messages())
            .await
            .map_err(|error| {
                BoundedCuTaskError::new("bounded-cu-provider-failed", error.to_string(), true)
            })?;
        record_response(&mut transcript, turn, &response);
        if !response.tool_calls.is_empty() {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-forbidden-tool-call",
                format!(
                    "provider returned {} function tool call(s) in a CU-only lane",
                    response.tool_calls.len()
                ),
                false,
            ));
        }
        if !response.cu_calls.is_empty() {
            if request.mode == BoundedCuTaskMode::Attest {
                return Err(BoundedCuTaskError::new(
                    "bounded-cu-post-observation-action-refused",
                    "attestation mode returned a native computer action; none was executed",
                    false,
                ));
            }
            apply_cu_calls(
                executor,
                &mut conversation,
                &mut transcript,
                &mut counters,
                turn,
                response,
            )
            .await?;
            continue;
        }

        conversation.add_assistant(response.content.clone());
        match parse_result(&response.content) {
            Ok((result, result_sha256)) => {
                transcript.push(turn, "result", format!("sha256={result_sha256}"));
                return build_receipt(
                    provider,
                    request,
                    ReceiptCompletion {
                        started_at,
                        elapsed_ms: u64::try_from(started_monotonic.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        counters,
                        transcript,
                        initial_frame_sha256,
                        result,
                        result_sha256,
                    },
                );
            }
            Err(error) if turn < max_turns => {
                transcript.push(turn, "invalid_result", format!("code={}", error.code));
                conversation.add_user(
                    MessageProvenance::SystemInjection,
                    "Your last response was not one bounded JSON object. Inspect the current frame only as allowed, then return the requested JSON object with no prose or Markdown."
                        .to_string(),
                );
            }
            Err(error) => return Err(error),
        }
    }
    Err(BoundedCuTaskError::new(
        "bounded-cu-turn-limit-exceeded",
        "bounded CU task exhausted its fixed turn limit",
        false,
    ))
}

async fn apply_cu_calls(
    executor: &mut dyn BoundedCuActionExecutor,
    conversation: &mut Conversation,
    transcript: &mut Transcript,
    counters: &mut TaskCounters,
    turn: u32,
    response: ChatResponse,
) -> Result<(), BoundedCuTaskError> {
    let mut call_ids = HashSet::with_capacity(response.cu_calls.len());
    let response_action_count = response.cu_calls.iter().try_fold(0_u64, |count, call| {
        if !call.metadata.pending_safety_checks.is_empty() {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-pending-safety-check-refused",
                "provider returned a pending computer-use safety check; no action was executed",
                false,
            ));
        }
        if call
            .actions
            .iter()
            .any(|action| matches!(action, CuAction::Paste { .. }))
        {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-paste-refused",
                "clipboard paste is not permitted in the bounded proof lane; use typed input",
                false,
            ));
        }
        if call.actions.iter().any(|action| {
            matches!(
                action,
                CuAction::MouseDown { .. } | CuAction::MouseUp { .. }
            )
        }) {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-split-input-edge-refused",
                "split mouse down/up actions are not permitted in the bounded proof lane",
                false,
            ));
        }
        if call.call_id.trim().is_empty()
            || call.call_id.len() > MAX_BINDING_BYTES
            || !call_ids.insert(call.call_id.as_str())
        {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-call-id-invalid",
                "native computer call IDs must be nonempty, bounded, and unique per response",
                false,
            ));
        }
        if call.actions.is_empty() || call.actions.len() > BOUNDED_CU_MAX_BATCH_ACTIONS {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-action-batch-invalid",
                "native computer action batch was empty or exceeded the fixed batch cap",
                false,
            ));
        }
        let payload_bytes = serde_json::to_vec(&call.actions).map_err(|error| {
            BoundedCuTaskError::new(
                "bounded-cu-action-serialization-failed",
                error.to_string(),
                false,
            )
        })?;
        if payload_bytes.len() > BOUNDED_CU_MAX_ACTION_PAYLOAD_BYTES {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-action-payload-too-large",
                "native computer action payload exceeded the fixed byte cap",
                false,
            ));
        }
        count.checked_add(call.actions.len() as u64).ok_or_else(|| {
            BoundedCuTaskError::new(
                "bounded-cu-action-count-overflow",
                "native computer action count overflowed",
                false,
            )
        })
    })?;
    if counters.actions.saturating_add(response_action_count) > BOUNDED_CU_MAX_TOTAL_ACTIONS {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-action-limit-exceeded",
            "native computer actions exceeded the fixed task cap",
            false,
        ));
    }

    let refs = response
        .cu_calls
        .iter()
        .map(|call| ToolCallRef {
            id: call.call_id.clone(),
            call_id: call.call_id.clone(),
            name: "computer".to_string(),
            arguments: String::new(),
        })
        .collect();
    conversation.add_assistant_tool_calls(response.content, refs, response.raw_output);
    for call in response.cu_calls {
        let action_kinds = call
            .actions
            .iter()
            .map(action_kind)
            .collect::<Vec<_>>()
            .join(",");
        let action_projection_sha256 = action_projection_sha256(&call.actions)?;
        let call_id_sha256 = sha256(call.call_id.as_bytes());
        let input_count = call
            .actions
            .iter()
            .filter(|action| is_input(action))
            .count() as u64;
        let outcome = executor.execute(&call.actions).await?;
        let frame_sha256 = image_sha256(&outcome.screenshot)?;
        if outcome.statuses.len() != call.actions.len() {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-action-cardinality-mismatch",
                "native computer result count did not match the action count",
                false,
            ));
        }
        if outcome.statuses.contains(&CuActionStatus::Failed) {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-action-failed",
                "at least one native computer action or its trailing observation failed",
                false,
            ));
        }
        counters.cu_batches += 1;
        counters.actions += call.actions.len() as u64;
        counters.inputs += input_count;
        transcript.push(
            turn,
            "cu_batch",
            format!(
                "actions={action_kinds};action_projection_sha256={action_projection_sha256};call_id_sha256={call_id_sha256};frame_sha256={frame_sha256};statuses={};inputs={input_count}",
                status_labels(&outcome.statuses)
            ),
        );
        conversation.add_cu_result(
            &call.call_id,
            &outcome.model_summary,
            vec![outcome.screenshot],
        );
    }
    Ok(())
}

struct ReceiptCompletion {
    started_at: String,
    elapsed_ms: u64,
    counters: TaskCounters,
    transcript: Transcript,
    initial_frame_sha256: String,
    result: serde_json::Value,
    result_sha256: String,
}

fn build_receipt(
    provider: &dyn ChatProvider,
    request: BoundedCuTaskRequest,
    completion: ReceiptCompletion,
) -> Result<BoundedCuTaskReceipt, BoundedCuTaskError> {
    let ReceiptCompletion {
        started_at,
        elapsed_ms,
        counters,
        transcript,
        initial_frame_sha256,
        result,
        result_sha256,
    } = completion;
    let completed_at = now_rfc3339();
    if completed_at < started_at {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-clock-regressed",
            "wall clock moved backwards during the bounded CU task",
            false,
        ));
    }
    let current_transcript_event_count = transcript.events.len() as u64;
    validate_transcript_events(&transcript.events)?;
    let transcript_event_count = request
        .prior_transcript_event_count
        .unwrap_or(0)
        .checked_add(current_transcript_event_count)
        .ok_or_else(|| {
            BoundedCuTaskError::new(
                "bounded-cu-transcript-count-overflow",
                "transcript event count overflowed",
                false,
            )
        })?;
    let transcript_sha256 = transcript.digest(&request)?;
    let task_sha256 = sha256(request.task.as_bytes());
    let mut receipt = BoundedCuTaskReceipt {
        schema_version: 1,
        receipt_id: String::new(),
        mode: request.mode,
        attempt_id: request.attempt_id,
        workspace_id: request.workspace_id,
        display_id: request.display_id,
        display_target: request.display_target,
        capture_generation: request.capture_generation,
        provider: provider.name().to_string(),
        model: provider.model().to_string(),
        started_at,
        completed_at,
        elapsed_ms,
        turns: counters.turns,
        cu_batch_count: counters.cu_batches,
        action_count: counters.actions,
        input_event_count: counters.inputs,
        post_observation_input_event_count: 0,
        forbidden_tool_call_count: 0,
        escalation_count: 0,
        initial_frame_sha256,
        prior_receipt_id: request.prior_receipt_id,
        prior_transcript_event_count: request.prior_transcript_event_count,
        prior_transcript_sha256: request.prior_transcript_sha256,
        observation_sha256: request.observation_sha256,
        current_transcript_event_count,
        current_transcript: transcript.events,
        transcript_event_count,
        transcript_sha256,
        task_sha256,
        result_sha256,
        result,
    };
    receipt.receipt_id = receipt_id(&receipt)?;
    Ok(receipt)
}

pub(crate) fn validate_bounded_cu_task_request(
    request: &BoundedCuTaskRequest,
) -> Result<(), BoundedCuTaskError> {
    for (name, value) in [
        ("attempt_id", request.attempt_id.as_str()),
        ("workspace_id", request.workspace_id.as_str()),
        ("capture_generation", request.capture_generation.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > MAX_BINDING_BYTES {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-binding-invalid",
                format!("{name} must be nonempty and at most {MAX_BINDING_BYTES} bytes"),
                false,
            ));
        }
    }
    if request.display_target != format!("display_{}", request.display_id) {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-display-binding-invalid",
            "display_target did not match display_id",
            false,
        ));
    }
    if request.task.trim().is_empty() || request.task.len() > BOUNDED_CU_MAX_TASK_BYTES {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-task-invalid",
            format!("task must be nonempty and at most {BOUNDED_CU_MAX_TASK_BYTES} bytes"),
            false,
        ));
    }
    match request.mode {
        BoundedCuTaskMode::Stage
            if request.prior_receipt_id.is_some()
                || request.prior_transcript_event_count.is_some()
                || request.prior_transcript_sha256.is_some()
                || request.observation_sha256.is_some()
                || request.prior_completed_at.is_some() =>
        {
            Err(BoundedCuTaskError::new(
                "bounded-cu-stage-lineage-invalid",
                "stage mode cannot claim a prior receipt or observation",
                false,
            ))
        }
        BoundedCuTaskMode::Attest => validate_attestation_lineage(request),
        BoundedCuTaskMode::Stage => Ok(()),
    }
}

fn validate_attestation_lineage(request: &BoundedCuTaskRequest) -> Result<(), BoundedCuTaskError> {
    let prior_receipt_id = request
        .prior_receipt_id
        .as_deref()
        .filter(|value| is_receipt_id(value));
    if prior_receipt_id.is_none()
        || request.prior_transcript_event_count.is_some()
        || request.prior_transcript_sha256.is_some()
        || request.observation_sha256.is_some()
        || request.prior_completed_at.is_some()
    {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-attestation-lineage-invalid",
            "attest mode accepts only an issued stage receipt ID; transcript and frame lineage are derived by the daemon",
            false,
        ));
    }
    Ok(())
}

fn bounded_system_prompt(mode: BoundedCuTaskMode) -> String {
    match mode {
        BoundedCuTaskMode::Stage => "You are a bounded computer-use operator on one isolated virtual display. Use only native computer actions to arrange the requested visual proof. You have no shell, filesystem, browser automation API, delegation, function tools, or escalation. Never navigate outside the requested task. When the proof is visibly ready, return exactly one JSON object matching the task's requested result shape, with no prose or Markdown."
            .to_string(),
        BoundedCuTaskMode::Attest => "You are a read-only visual attestor. Inspect the supplied current screenshot and return exactly one JSON object matching the task's requested result shape, with no prose or Markdown. Do not call any native computer action, including screenshot, wait, zoom, pointer, keyboard, or scroll. You have no shell, filesystem, browser automation API, delegation, function tools, or escalation."
            .to_string(),
    }
}

fn parse_result(raw: &str) -> Result<(serde_json::Value, String), BoundedCuTaskError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > BOUNDED_CU_MAX_RESULT_BYTES {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-result-size-invalid",
            format!("result must be nonempty and at most {BOUNDED_CU_MAX_RESULT_BYTES} bytes"),
            false,
        ));
    }
    let mut deserializer = serde_json::Deserializer::from_str(trimmed);
    let value = UniqueJsonValueSeed
        .deserialize(&mut deserializer)
        .and_then(|value| {
            deserializer.end()?;
            Ok(value)
        })
        .map_err(|_| {
            BoundedCuTaskError::new(
                "bounded-cu-result-json-invalid",
                "result was not one standalone duplicate-free JSON value",
                false,
            )
        })?;
    if !value.is_object() {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-result-shape-invalid",
            "result must be one JSON object",
            false,
        ));
    }
    let canonical = serde_json::to_vec(&value).map_err(|error| {
        BoundedCuTaskError::new(
            "bounded-cu-result-serialization-failed",
            error.to_string(),
            false,
        )
    })?;
    Ok((value, sha256(&canonical)))
}

struct UniqueJsonValueSeed;

impl<'de> de::DeserializeSeed<'de> for UniqueJsonValueSeed {
    type Value = serde_json::Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonValueVisitor)
    }
}

struct UniqueJsonValueVisitor;

impl<'de> Visitor<'de> for UniqueJsonValueVisitor {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a duplicate-free JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(UniqueJsonValueSeed)? {
            values.push(value);
        }
        Ok(serde_json::Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate JSON key: {key}")));
            }
            let value = map.next_value_seed(UniqueJsonValueSeed)?;
            values.insert(key, value);
        }
        Ok(serde_json::Value::Object(values))
    }
}

fn record_response(transcript: &mut Transcript, turn: u32, response: &ChatResponse) {
    transcript.push(
        turn,
        "provider_response",
        format!(
            "content_sha256={};content_bytes={};cu_calls={};tool_calls={}",
            sha256(response.content.as_bytes()),
            response.content.len(),
            response.cu_calls.len(),
            response.tool_calls.len()
        ),
    );
}

fn receipt_id(receipt: &BoundedCuTaskReceipt) -> Result<String, BoundedCuTaskError> {
    let mut projection = serde_json::to_value(receipt).map_err(|error| {
        BoundedCuTaskError::new(
            "bounded-cu-receipt-serialization-failed",
            error.to_string(),
            false,
        )
    })?;
    projection
        .as_object_mut()
        .expect("bounded CU receipt serializes as an object")
        .remove("receiptId");
    let bytes = serde_json::to_vec(&projection).map_err(|error| {
        BoundedCuTaskError::new(
            "bounded-cu-receipt-serialization-failed",
            error.to_string(),
            false,
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"intendant-bounded-cu-receipt-v1\0");
    hasher.update(bytes);
    Ok(format!("bcu-{:x}", hasher.finalize()))
}

fn action_kind(action: &CuAction) -> &'static str {
    match action {
        CuAction::Click { .. } => "click",
        CuAction::DoubleClick { .. } => "double_click",
        CuAction::TripleClick { .. } => "triple_click",
        CuAction::MouseDown { .. } => "mouse_down",
        CuAction::MouseUp { .. } => "mouse_up",
        CuAction::Type { .. } => "type",
        CuAction::Paste { .. } => "paste",
        CuAction::Key { .. } => "key",
        CuAction::HoldKey { .. } => "hold_key",
        CuAction::Scroll { .. } => "scroll",
        CuAction::MoveMouse { .. } => "move_mouse",
        CuAction::Drag { .. } => "drag",
        CuAction::Screenshot => "screenshot",
        CuAction::Zoom { .. } => "zoom",
        CuAction::Wait { .. } => "wait",
    }
}

fn redacted_action_projection(action: &CuAction) -> RedactedActionProjection<'_> {
    match action {
        CuAction::Click { x, y, button } => RedactedActionProjection::Click {
            x: *x,
            y: *y,
            button,
        },
        CuAction::DoubleClick { x, y, button } => RedactedActionProjection::DoubleClick {
            x: *x,
            y: *y,
            button,
        },
        CuAction::TripleClick { x, y, button } => RedactedActionProjection::TripleClick {
            x: *x,
            y: *y,
            button,
        },
        CuAction::MouseDown { x, y, button } => RedactedActionProjection::MouseDown {
            x: *x,
            y: *y,
            button,
        },
        CuAction::MouseUp { x, y, button } => RedactedActionProjection::MouseUp {
            x: *x,
            y: *y,
            button,
        },
        CuAction::Type { .. } => RedactedActionProjection::Type,
        CuAction::Paste { .. } => RedactedActionProjection::Paste,
        CuAction::Key { .. } => RedactedActionProjection::Key,
        CuAction::HoldKey { ms, .. } => RedactedActionProjection::HoldKey { ms: *ms },
        CuAction::Scroll {
            x,
            y,
            direction,
            amount,
        } => RedactedActionProjection::Scroll {
            x: *x,
            y: *y,
            direction,
            amount: *amount,
        },
        CuAction::MoveMouse { x, y } => RedactedActionProjection::MoveMouse { x: *x, y: *y },
        CuAction::Drag {
            start_x,
            start_y,
            end_x,
            end_y,
        } => RedactedActionProjection::Drag {
            start_x: *start_x,
            start_y: *start_y,
            end_x: *end_x,
            end_y: *end_y,
        },
        CuAction::Screenshot => RedactedActionProjection::Screenshot,
        CuAction::Zoom {
            x,
            y,
            width,
            height,
        } => RedactedActionProjection::Zoom {
            x: *x,
            y: *y,
            width: *width,
            height: *height,
        },
        CuAction::Wait { ms } => RedactedActionProjection::Wait { ms: *ms },
    }
}

fn action_projection_sha256(actions: &[CuAction]) -> Result<String, BoundedCuTaskError> {
    let projection = actions
        .iter()
        .map(redacted_action_projection)
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&projection).map_err(|error| {
        BoundedCuTaskError::new(
            "bounded-cu-action-serialization-failed",
            error.to_string(),
            false,
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"intendant-bounded-cu-action-projection-v1\0");
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn is_input(action: &CuAction) -> bool {
    !matches!(
        action,
        CuAction::Screenshot | CuAction::Zoom { .. } | CuAction::Wait { .. }
    )
}

fn status_labels(statuses: &[CuActionStatus]) -> String {
    statuses
        .iter()
        .map(|status| status.label())
        .collect::<Vec<_>>()
        .join(",")
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn image_sha256(image: &ImageData) -> Result<String, BoundedCuTaskError> {
    if image.media_type != "image/png" {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-frame-media-type-invalid",
            "bounded CU frames must be PNG images",
            false,
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .map_err(|_| {
            BoundedCuTaskError::new(
                "bounded-cu-frame-base64-invalid",
                "bounded CU frame was not valid canonical base64",
                false,
            )
        })?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-frame-png-invalid",
            "bounded CU frame did not begin with the PNG signature",
            false,
        ));
    }
    Ok(sha256(&bytes))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_receipt_id(value: &str) -> bool {
    value.strip_prefix("bcu-").is_some_and(is_sha256)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer_use::{CuCallMetadata, CuToolCall, MouseButton};
    use crate::error::CallerError;
    use crate::provider::{TokenUsage, ToolCall};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeProvider {
        responses: Mutex<VecDeque<ChatResponse>>,
    }

    impl FakeProvider {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl ChatProvider for FakeProvider {
        async fn chat(
            &self,
            _messages: &[crate::conversation::Message],
        ) -> Result<ChatResponse, CallerError> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| CallerError::Provider("no fake response".to_string()))
        }

        fn name(&self) -> &str {
            "fake"
        }

        fn model(&self) -> &str {
            "fake-cu"
        }

        fn context_window(&self) -> u64 {
            16_384
        }

        fn max_output_tokens(&self) -> u64 {
            1_024
        }

        fn cu_enabled(&self) -> bool {
            true
        }

        fn requires_image_stripping(&self) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct FakeExecutor {
        batches: Vec<Vec<String>>,
    }

    #[async_trait]
    impl BoundedCuActionExecutor for FakeExecutor {
        async fn execute(
            &mut self,
            actions: &[CuAction],
        ) -> Result<BoundedCuActionOutcome, BoundedCuTaskError> {
            self.batches.push(
                actions
                    .iter()
                    .map(|action| action_kind(action).to_string())
                    .collect(),
            );
            Ok(BoundedCuActionOutcome {
                model_summary: "Actions executed successfully.".to_string(),
                screenshot: ImageData {
                    media_type: "image/png".to_string(),
                    data: "iVBORw0KGgo=".to_string(),
                },
                statuses: vec![CuActionStatus::Verified; actions.len()],
            })
        }
    }

    fn response(content: &str) -> ChatResponse {
        ChatResponse {
            content: content.to_string(),
            usage: TokenUsage::default(),
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls: Vec::new(),
            cu_calls: Vec::new(),
            raw_output: None,
        }
    }

    fn stage_request() -> BoundedCuTaskRequest {
        BoundedCuTaskRequest {
            mode: BoundedCuTaskMode::Stage,
            attempt_id: "attempt-1".to_string(),
            workspace_id: "bw-1".to_string(),
            display_id: 99,
            display_target: "display_99".to_string(),
            capture_generation: "vdcg-1".to_string(),
            task: "Prepare proof and return {\"ready\":true}.".to_string(),
            prior_receipt_id: None,
            prior_transcript_event_count: None,
            prior_transcript_sha256: None,
            observation_sha256: None,
            prior_completed_at: None,
        }
    }

    fn attest_request(prior_receipt_id: String) -> BoundedCuTaskRequest {
        BoundedCuTaskRequest {
            mode: BoundedCuTaskMode::Attest,
            prior_receipt_id: Some(prior_receipt_id),
            task: "Inspect only and return {\"ready\":true}.".to_string(),
            ..stage_request()
        }
    }

    async fn issue_stage_receipt() -> BoundedCuTaskReceipt {
        let provider = FakeProvider::new(vec![response(r#"{"ready":true}"#)]);
        let mut executor = FakeExecutor::default();
        let receipt = run_bounded_cu_task(&provider, &mut executor, stage_request())
            .await
            .unwrap();
        remember_issued_stage_receipt(&receipt).unwrap();
        receipt
    }

    #[tokio::test]
    async fn stage_executes_only_native_actions_and_returns_bound_receipt() {
        let mut action_response = response("");
        action_response.cu_calls.push(CuToolCall {
            call_id: "cu-1".to_string(),
            actions: vec![CuAction::Click {
                x: 10,
                y: 20,
                button: MouseButton::Left,
            }],
            metadata: CuCallMetadata::default(),
        });
        let provider = FakeProvider::new(vec![action_response, response(r#"{"ready":true}"#)]);
        let mut executor = FakeExecutor::default();
        let receipt = run_bounded_cu_task(&provider, &mut executor, stage_request())
            .await
            .unwrap();

        assert_eq!(executor.batches, vec![vec!["screenshot"], vec!["click"]]);
        assert_eq!(receipt.action_count, 1);
        assert_eq!(receipt.input_event_count, 1);
        assert_eq!(receipt.post_observation_input_event_count, 0);
        assert_eq!(receipt.forbidden_tool_call_count, 0);
        assert_eq!(receipt.escalation_count, 0);
        assert_eq!(receipt.result["ready"], true);
        assert!(receipt.transcript_event_count >= 5);
        assert_eq!(
            receipt.current_transcript_event_count,
            receipt.current_transcript.len() as u64
        );
        validate_transcript_events(&receipt.current_transcript).unwrap();
        assert_eq!(
            transcript_digest(&receipt.current_transcript, None).unwrap(),
            receipt.transcript_sha256
        );
        assert!(is_sha256(&receipt.transcript_sha256));
        assert!(is_receipt_id(&receipt.receipt_id));
        assert_eq!(receipt_id(&receipt).unwrap(), receipt.receipt_id);

        let mut altered = receipt.clone();
        altered.input_event_count += 1;
        assert_ne!(receipt_id(&altered).unwrap(), receipt.receipt_id);
    }

    #[test]
    fn transcript_action_projection_omits_typed_and_key_material() {
        let first = vec![
            CuAction::Type {
                text: "123456".to_string(),
            },
            CuAction::Key {
                key: "hunter2".to_string(),
            },
            CuAction::HoldKey {
                key: "otp-123456".to_string(),
                ms: 250,
            },
        ];
        let second = vec![
            CuAction::Type {
                text: "654321".to_string(),
            },
            CuAction::Key {
                key: "different".to_string(),
            },
            CuAction::HoldKey {
                key: "otp-654321".to_string(),
                ms: 250,
            },
        ];

        let serialized = serde_json::to_string(
            &first
                .iter()
                .map(redacted_action_projection)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(!serialized.contains("123456"));
        assert!(!serialized.contains("hunter2"));
        assert_eq!(
            action_projection_sha256(&first).unwrap(),
            action_projection_sha256(&second).unwrap()
        );

        let shifted = vec![CuAction::Click {
            x: 11,
            y: 20,
            button: MouseButton::Left,
        }];
        let original = vec![CuAction::Click {
            x: 10,
            y: 20,
            button: MouseButton::Left,
        }];
        assert_ne!(
            action_projection_sha256(&original).unwrap(),
            action_projection_sha256(&shifted).unwrap()
        );
    }

    #[tokio::test]
    async fn attest_rejects_native_action_before_execution() {
        let stage = issue_stage_receipt().await;
        let mut action_response = response("");
        action_response.cu_calls.push(CuToolCall {
            call_id: "cu-1".to_string(),
            actions: vec![CuAction::Screenshot],
            metadata: CuCallMetadata::default(),
        });
        let provider = FakeProvider::new(vec![action_response]);
        let mut executor = FakeExecutor::default();
        let error = run_bounded_cu_task(&provider, &mut executor, attest_request(stage.receipt_id))
            .await
            .unwrap_err();

        assert_eq!(error.code, "bounded-cu-post-observation-action-refused");
        assert_eq!(executor.batches, vec![vec!["screenshot"]]);
    }

    #[tokio::test]
    async fn attest_derives_lineage_and_observation_from_issued_stage_and_frame() {
        let stage = issue_stage_receipt().await;
        let provider = FakeProvider::new(vec![response(r#"{"ready":true}"#)]);
        let mut executor = FakeExecutor::default();
        let receipt = run_bounded_cu_task(
            &provider,
            &mut executor,
            attest_request(stage.receipt_id.clone()),
        )
        .await
        .unwrap();

        assert_eq!(
            receipt.prior_receipt_id.as_deref(),
            Some(stage.receipt_id.as_str())
        );
        assert_eq!(
            receipt.prior_transcript_event_count,
            Some(stage.transcript_event_count)
        );
        assert_eq!(
            receipt.prior_transcript_sha256.as_deref(),
            Some(stage.transcript_sha256.as_str())
        );
        assert_eq!(
            receipt.observation_sha256.as_deref(),
            Some(receipt.initial_frame_sha256.as_str())
        );
        assert_eq!(
            transcript_digest(
                &receipt.current_transcript,
                Some(stage.transcript_sha256.as_str())
            )
            .unwrap(),
            receipt.transcript_sha256
        );
    }

    #[tokio::test]
    async fn function_tool_call_is_never_repaired_or_executed() {
        let mut tool_response = response("");
        tool_response.tool_calls.push(ToolCall {
            id: "tool-1".to_string(),
            call_id: "tool-1".to_string(),
            name: "escalate_to_agent".to_string(),
            arguments: r#"{"task":"escape"}"#.to_string(),
        });
        let provider = FakeProvider::new(vec![tool_response]);
        let mut executor = FakeExecutor::default();
        let error = run_bounded_cu_task(&provider, &mut executor, stage_request())
            .await
            .unwrap_err();

        assert_eq!(error.code, "bounded-cu-forbidden-tool-call");
        assert_eq!(executor.batches, vec![vec!["screenshot"]]);
    }

    #[tokio::test]
    async fn duplicate_cu_call_ids_are_rejected_before_any_input() {
        let mut action_response = response("");
        for _ in 0..2 {
            action_response.cu_calls.push(CuToolCall {
                call_id: "duplicate".to_string(),
                actions: vec![CuAction::Screenshot],
                metadata: CuCallMetadata::default(),
            });
        }
        let provider = FakeProvider::new(vec![action_response]);
        let mut executor = FakeExecutor::default();
        let error = run_bounded_cu_task(&provider, &mut executor, stage_request())
            .await
            .unwrap_err();

        assert_eq!(error.code, "bounded-cu-call-id-invalid");
        assert_eq!(executor.batches, vec![vec!["screenshot"]]);
    }

    #[tokio::test]
    async fn pending_safety_checks_are_rejected_before_any_input() {
        let mut action_response = response("");
        action_response.cu_calls.push(CuToolCall {
            call_id: "cu-safety".to_string(),
            actions: vec![CuAction::Click {
                x: 10,
                y: 20,
                button: MouseButton::Left,
            }],
            metadata: CuCallMetadata {
                pending_safety_checks: vec![serde_json::json!({"id": "check-1"})],
                safety_decision: None,
            },
        });
        let provider = FakeProvider::new(vec![action_response]);
        let mut executor = FakeExecutor::default();
        let error = run_bounded_cu_task(&provider, &mut executor, stage_request())
            .await
            .unwrap_err();

        assert_eq!(error.code, "bounded-cu-pending-safety-check-refused");
        assert_eq!(executor.batches, vec![vec!["screenshot"]]);
    }

    #[tokio::test]
    async fn paste_is_rejected_before_clipboard_mutation() {
        let mut action_response = response("");
        action_response.cu_calls.push(CuToolCall {
            call_id: "cu-paste".to_string(),
            actions: vec![CuAction::Paste {
                text: "secret".to_string(),
            }],
            metadata: CuCallMetadata::default(),
        });
        let provider = FakeProvider::new(vec![action_response]);
        let mut executor = FakeExecutor::default();
        let error = run_bounded_cu_task(&provider, &mut executor, stage_request())
            .await
            .unwrap_err();

        assert_eq!(error.code, "bounded-cu-paste-refused");
        assert_eq!(executor.batches, vec![vec!["screenshot"]]);
    }

    #[tokio::test]
    async fn split_mouse_edges_are_rejected_before_input() {
        let mut action_response = response("");
        action_response.cu_calls.push(CuToolCall {
            call_id: "cu-mouse-down".to_string(),
            actions: vec![CuAction::MouseDown {
                x: 10,
                y: 20,
                button: MouseButton::Left,
            }],
            metadata: CuCallMetadata::default(),
        });
        let provider = FakeProvider::new(vec![action_response]);
        let mut executor = FakeExecutor::default();
        let error = run_bounded_cu_task(&provider, &mut executor, stage_request())
            .await
            .unwrap_err();

        assert_eq!(error.code, "bounded-cu-split-input-edge-refused");
        assert_eq!(executor.batches, vec![vec!["screenshot"]]);
    }

    #[test]
    fn stage_and_attest_lineage_are_mutually_exclusive() {
        let mut stage = stage_request();
        stage.prior_receipt_id = Some(format!("bcu-{}", "a".repeat(64)));
        assert_eq!(
            validate_bounded_cu_task_request(&stage).unwrap_err().code,
            "bounded-cu-stage-lineage-invalid"
        );

        let attest = attest_request("not-a-receipt".to_string());
        assert_eq!(
            validate_bounded_cu_task_request(&attest).unwrap_err().code,
            "bounded-cu-attestation-lineage-invalid"
        );
    }

    #[test]
    fn provider_result_rejects_duplicate_keys_at_every_depth() {
        assert_eq!(
            parse_result(r#"{"ready":true,"ready":false}"#)
                .unwrap_err()
                .code,
            "bounded-cu-result-json-invalid"
        );
        assert_eq!(
            parse_result(r#"{"proof":{"visible":true,"visible":false}}"#)
                .unwrap_err()
                .code,
            "bounded-cu-result-json-invalid"
        );
    }
}

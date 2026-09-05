//! Provider-free, owner-bound proof sessions. The worker, not an HTTP request,
//! owns display exclusion and cancellation cleanup across external agent turns.
use super::*;
use crate::access::actor::{ActorBinding, ActorKind};
use crate::bounded_cu_task::{
    action_kind, action_projection_sha256, image_sha256, is_input, validate_bounded_action,
    UniqueJsonValueSeed,
};
use base64::Engine as _;
use serde::de::DeserializeSeed as _;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const PROFILE: &str = "intendant-external-cu-proof-v1";
const MAX_SESSIONS: usize = 8;
const STAGE_SECONDS: u64 = 180;
const FROZEN_SECONDS: u64 = 45;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExternalCuProofParams {
    /// Duplicate-key-safe JSON command. Operations: begin, actions, freeze,
    /// observe, finish, close, abort, status. No provider or model is selected.
    pub request: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Begin {
        attempt_id: String,
        workspace_id: String,
        display_id: u32,
        display_target: String,
        capture_generation: String,
        job_sha256: String,
    },
    Actions {
        proof_id: String,
        sequence: u64,
        actions_json: String,
    },
    Freeze {
        proof_id: String,
        sequence: u64,
    },
    Observe {
        proof_id: String,
        sequence: u64,
        pre_observation_sha256: String,
    },
    Finish {
        proof_id: String,
        sequence: u64,
        observation_sha256: String,
        claims_json: String,
    },
    Close {
        proof_id: String,
        sequence: u64,
    },
    Abort {
        proof_id: String,
        sequence: u64,
    },
    Status {
        proof_id: String,
    },
}
impl Request {
    fn id(&self) -> Option<&str> {
        match self {
            Self::Begin { .. } => None,
            Self::Actions { proof_id, .. }
            | Self::Freeze { proof_id, .. }
            | Self::Observe { proof_id, .. }
            | Self::Finish { proof_id, .. }
            | Self::Close { proof_id, .. }
            | Self::Abort { proof_id, .. }
            | Self::Status { proof_id } => Some(proof_id),
        }
    }
    fn sequence(&self) -> Option<u64> {
        match self {
            Self::Begin { .. } | Self::Status { .. } => None,
            Self::Actions { sequence, .. }
            | Self::Freeze { sequence, .. }
            | Self::Observe { sequence, .. }
            | Self::Finish { sequence, .. }
            | Self::Close { sequence, .. }
            | Self::Abort { sequence, .. } => Some(*sequence),
        }
    }
}
fn err(code: &'static str, message: &str) -> BoundedCuTaskError {
    BoundedCuTaskError::new(code, message, false)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn hash_value(domain: &str, value: &Value) -> String {
    let mut bytes = domain.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend(serde_json::to_vec(value).expect("JSON value"));
    digest(&bytes)
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn sha_ok(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn unique_json(raw: &str, maximum: usize) -> Result<Value, BoundedCuTaskError> {
    if raw.is_empty() || raw.len() > maximum {
        return Err(err(
            "external-proof-input-size",
            "proof JSON exceeds its fixed byte limit",
        ));
    }
    let mut de = serde_json::Deserializer::from_str(raw);
    let value = UniqueJsonValueSeed.deserialize(&mut de).map_err(|_| {
        err(
            "external-proof-json",
            "proof input must be duplicate-key-free JSON",
        )
    })?;
    de.end()
        .map_err(|_| err("external-proof-json", "proof input contains trailing data"))?;
    Ok(value)
}
fn parse(raw: &str) -> Result<Request, BoundedCuTaskError> {
    serde_json::from_value(unique_json(raw, 96 * 1024)?)
        .map_err(|_| err("external-proof-command", "unknown proof command or fields"))
}
fn actor_record(actor: &ActorBinding) -> Value {
    // Tenant projection, not serialization of the evolving ActorBinding seam.
    json!({"kind": actor.kind.as_str(), "principalId": actor.principal_id, "sessionId": actor.session_id})
}
fn binding(params: &RunBoundedCuTaskParams, job: &str) -> Value {
    json!({"attemptId":params.attempt_id,"workspaceId":params.workspace_id,
        "displayId":params.display_id,"displayTarget":params.display_target,
        "captureGeneration":params.capture_generation,"jobSha256":job})
}
fn actions(raw: &str) -> Result<Vec<CuAction>, BoundedCuTaskError> {
    let value = unique_json(raw, 64 * 1024)?;
    let values = value
        .as_array()
        .ok_or_else(|| err("external-proof-actions", "actions must be an array"))?;
    if values.is_empty() || values.len() > 16 {
        return Err(err(
            "external-proof-actions",
            "a batch requires 1..16 actions",
        ));
    }
    let mut out = Vec::new();
    for value in values {
        let action: CuAction = serde_json::from_value(value.clone())
            .map_err(|_| err("external-proof-action", "invalid action"))?;
        // Reject unknown fields rather than silently discarding instructions.
        let projected = serde_json::to_value(&action).expect("CU action");
        let supplied = value
            .as_object()
            .ok_or_else(|| err("external-proof-action", "action must be an object"))?;
        let known = projected.as_object().expect("CU action object");
        if supplied.keys().any(|key| !known.contains_key(key)) {
            return Err(err("external-proof-action", "unknown action field"));
        }
        if matches!(
            action,
            CuAction::Paste { .. }
                | CuAction::MouseDown { .. }
                | CuAction::MouseUp { .. }
                | CuAction::Zoom { .. }
        ) {
            return Err(err(
                "external-proof-action-forbidden",
                "clipboard, split input edges, and cropped frames are forbidden in proof sessions",
            ));
        }
        validate_bounded_action(&action)?;
        out.push(action);
    }
    Ok(out)
}
fn frame(outcome: &BoundedCuActionOutcome) -> Result<Value, BoundedCuTaskError> {
    if outcome.screenshot.data.len() > 12 * 1024 * 1024 {
        return Err(err("external-proof-frame-size", "proof PNG exceeds limit"));
    }
    let sha = image_sha256(&outcome.screenshot)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&outcome.screenshot.data)
        .map_err(|_| err("external-proof-frame", "invalid PNG encoding"))?;
    if bytes.len() < 33
        || &bytes[12..16] != b"IHDR"
        || base64::engine::general_purpose::STANDARD.encode(&bytes) != outcome.screenshot.data
    {
        return Err(err("external-proof-frame", "invalid canonical PNG frame"));
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("PNG width"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("PNG height"));
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err(err("external-proof-frame", "invalid proof geometry"));
    }
    Ok(
        json!({"sha256":sha,"byteLength":bytes.len(),"width":width,"height":height,"pngBase64":outcome.screenshot.data}),
    )
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Stage,
    Frozen,
    Observed,
    Finished,
}
impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Stage => "stage",
            Self::Frozen => "frozen",
            Self::Observed => "observed",
            Self::Finished => "finished",
        }
    }
}
struct Proof {
    id: String,
    binding: Value,
    actor: Value,
    phase: Phase,
    sequence: u64,
    started: Instant,
    started_at: String,
    frozen_at: Option<String>,
    observed_at: Option<String>,
    action_count: u64,
    input_count: u64,
    events: Vec<Value>,
    frame: Value,
    stage_digest: Option<String>,
    pre_observation: Option<String>,
    receipt: Option<Value>,
}
impl Proof {
    fn new(
        id: String,
        binding: Value,
        actor: Value,
        started: Instant,
        started_at: String,
        initial: Value,
    ) -> Self {
        let mut proof = Self {
            id,
            binding,
            actor,
            phase: Phase::Stage,
            sequence: 0,
            started,
            started_at,
            frozen_at: None,
            observed_at: None,
            action_count: 0,
            input_count: 0,
            events: Vec::new(),
            frame: initial,
            stage_digest: None,
            pre_observation: None,
            receipt: None,
        };
        proof.event(
            "initial_frame",
            json!({"frameSha256":proof.frame["sha256"]}),
        );
        proof
    }
    fn event(&mut self, kind: &str, detail: Value) {
        self.events
            .push(json!({"sequence":self.events.len(),"kind":kind,"detail":detail}));
    }
    fn snapshot(&self) -> Value {
        json!({"ok":true,"profile":PROFILE,"proofId":self.id,"sequence":self.sequence,"binding":self.binding,"actor":self.actor,
            "phase":self.phase.name(),"frame":self.frame,"receipt":self.receipt})
    }
    fn check_sequence(&self, request: &Request) -> Result<(), BoundedCuTaskError> {
        if request.sequence().is_some_and(|seq| seq != self.sequence) {
            return Err(err(
                "external-proof-sequence",
                "stale or replayed sequence; no action executed",
            ));
        }
        Ok(())
    }
    fn require_phase(&self, phase: Phase) -> Result<(), BoundedCuTaskError> {
        if self.phase != phase {
            return Err(err(
                "external-proof-phase",
                "operation is forbidden in the current proof phase",
            ));
        }
        Ok(())
    }
    fn make_receipt(&self, claims: &str) -> Result<Value, BoundedCuTaskError> {
        let completed = now();
        if completed < self.started_at {
            return Err(err("external-proof-clock", "wall clock moved backwards"));
        }
        let mut image = self.frame.clone();
        image.as_object_mut().expect("frame").remove("pngBase64");
        let mut receipt = json!({"schemaVersion":2,"profile":PROFILE,"proofId":self.id,
            "binding":self.binding,"actor":self.actor,
            "execution":{"kind":"external-actions","internalModelCalls":0,"claimsAuthority":"external-caller","grantsSubmissionAuthority":false},
            "startedAt":self.started_at,"frozenAt":self.frozen_at,"observedAt":self.observed_at,"completedAt":completed,
            "elapsedMs":self.started.elapsed().as_millis() as u64,"actionCount":self.action_count,"inputEventCount":self.input_count,
            "postObservationInputEventCount":0,"stageTranscriptSha256":self.stage_digest,
            "preObservationSha256":self.pre_observation,"observation":image,
            "transcript":self.events,"transcriptSha256":hash_value("intendant-external-cu-transcript-v1",&json!(self.events)),
            "claimsJson":claims,"claimsSha256":digest(claims.as_bytes())});
        let id = format!(
            "ecup-receipt:{}",
            hash_value("intendant-external-cu-receipt-v1", &receipt)
        );
        receipt["receiptId"] = json!(id);
        Ok(receipt)
    }
}

struct Message {
    request: Request,
    reply: oneshot::Sender<Result<Value, BoundedCuTaskError>>,
}
#[derive(Clone)]
struct Handle {
    actor: ActorBinding,
    display: u32,
    attempt: String,
    tx: mpsc::Sender<Message>,
}
fn registry() -> &'static StdMutex<HashMap<String, Handle>> {
    static REGISTRY: OnceLock<StdMutex<HashMap<String, Handle>>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}
struct Registration(String);
impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut registry) = registry().lock() {
            registry.remove(&self.0);
        }
    }
}
async fn cleanup(executor: NativeBoundedCuExecutor) -> Result<(), BoundedCuTaskError> {
    let mut executor = executor;
    let release = executor.release_pending_input_edges().await;
    let bound = validate_resource_binding(&executor.params, &executor.bus).await;
    let live = executor.validate_proof_session_liveness().await;
    let scratch = std::sync::Arc::clone(&executor.scratch_guard);
    drop(executor);
    let scratch_result = std::sync::Arc::try_unwrap(scratch)
        .map_err(|_| {
            err(
                "external-proof-cleanup-pending",
                "native work still owns private scratch",
            )
        })?
        .close()
        .map_err(|_| {
            err(
                "external-proof-cleanup-failed",
                "private proof scratch could not be removed",
            )
        });
    release?;
    bound?;
    live?;
    scratch_result
}
fn validate_step(proof: &Proof, request: &Request) -> Result<Vec<CuAction>, BoundedCuTaskError> {
    proof.check_sequence(request)?;
    match request {
        Request::Actions { actions_json, .. } => {
            proof.require_phase(Phase::Stage)?;
            let actions = actions(actions_json)?;
            if proof.action_count + actions.len() as u64 > 64 {
                return Err(err(
                    "external-proof-action-limit",
                    "session action limit is 64",
                ));
            }
            Ok(actions)
        }
        Request::Freeze { .. } => {
            proof.require_phase(Phase::Stage)?;
            Ok(vec![])
        }
        Request::Observe {
            pre_observation_sha256,
            ..
        } => {
            proof.require_phase(Phase::Frozen)?;
            if !sha_ok(pre_observation_sha256) {
                return Err(err(
                    "external-proof-observation",
                    "pre-observation digest is invalid",
                ));
            }
            Ok(vec![])
        }
        Request::Finish {
            observation_sha256,
            claims_json,
            ..
        } => {
            proof.require_phase(Phase::Observed)?;
            if !sha_ok(observation_sha256) || proof.frame["sha256"] != *observation_sha256 {
                return Err(err(
                    "external-proof-observation",
                    "claims do not bind the exact issued observation",
                ));
            }
            if !unique_json(claims_json, 16 * 1024)?.is_object() {
                return Err(err(
                    "external-proof-claims",
                    "external claims must be a JSON object",
                ));
            }
            Ok(vec![])
        }
        Request::Close { .. } => {
            proof.require_phase(Phase::Finished)?;
            Ok(vec![])
        }
        Request::Status { .. } | Request::Abort { .. } => Ok(vec![]),
        Request::Begin { .. } => Err(err("external-proof-command", "begin is not a continuation")),
    }
}
async fn step(
    proof: &mut Proof,
    executor: &mut NativeBoundedCuExecutor,
    request: &Request,
    actions: Vec<CuAction>,
) -> Result<(), BoundedCuTaskError> {
    match request {
        Request::Actions { .. } => {
            for action in actions {
                // Prevalidated the entire batch before executing even its first action.
                let outcome = executor.execute(std::slice::from_ref(&action)).await?;
                if outcome.statuses.len() != 1 || outcome.statuses[0] == CuActionStatus::Failed {
                    return Err(err("external-proof-action-failed", "native action failed"));
                }
                proof.frame = frame(&outcome)?;
                proof.action_count += 1;
                proof.input_count += u64::from(is_input(&action));
                proof.event("action",json!({"kind":action_kind(&action),"projectionSha256":action_projection_sha256(std::slice::from_ref(&action))?,
                    "status":outcome.statuses[0].label(),"input":is_input(&action),"frameSha256":proof.frame["sha256"]}));
            }
        }
        Request::Freeze { .. } => {
            executor.release_pending_input_edges().await?;
            proof.phase = Phase::Frozen;
            proof.frozen_at = Some(now());
            proof.event("freeze", json!({"frameSha256":proof.frame["sha256"]}));
            proof.stage_digest = Some(hash_value(
                "intendant-external-cu-stage-v1",
                &json!(proof.events),
            ));
        }
        Request::Observe {
            pre_observation_sha256,
            ..
        } => {
            executor.initial_frame_not_before = Some(Instant::now());
            let outcome = executor.execute(&[CuAction::Screenshot]).await?;
            proof.frame = frame(&outcome)?;
            proof.phase = Phase::Observed;
            proof.observed_at = Some(now());
            proof.pre_observation = Some(pre_observation_sha256.clone());
            proof.event("observation",json!({"frameSha256":proof.frame["sha256"],"preObservationSha256":pre_observation_sha256}));
        }
        Request::Finish { claims_json, .. } => {
            proof.event("external_claims",json!({"claimsSha256":digest(claims_json.as_bytes()),"frameSha256":proof.frame["sha256"]}));
            proof.receipt = Some(proof.make_receipt(claims_json)?);
            proof.phase = Phase::Finished;
        }
        _ => {}
    }
    validate_resource_binding(&executor.params, &executor.bus).await?;
    executor.validate_proof_session_liveness().await?;
    if !matches!(request, Request::Status { .. }) {
        proof.sequence += 1;
    }
    Ok(())
}
async fn worker(
    server: IntendantServer,
    id: String,
    params: RunBoundedCuTaskParams,
    job: String,
    actor: ActorBinding,
    mut rx: mpsc::Receiver<Message>,
    begin_reply: oneshot::Sender<Result<Value, BoundedCuTaskError>>,
) {
    let _registration = Registration(id.clone());
    let start = Instant::now();
    let started_at = now();
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(STAGE_SECONDS);
    let preparation = tokio::time::timeout(
        Duration::from_secs(20),
        server.prepare_proof_executor(&params),
    )
    .await;
    let (mut executor, _, scratch) = match preparation {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            let _ = begin_reply.send(Err(error));
            return;
        }
        Err(_) => {
            let _ = begin_reply.send(Err(err(
                "external-proof-setup-timeout",
                "proof setup exceeded 20 seconds",
            )));
            return;
        }
    };
    drop(scratch); // Executor and any in-flight native operation retain this guard.
    let initial =
        match tokio::time::timeout_at(deadline, executor.execute(&[CuAction::Screenshot])).await {
            Ok(Ok(outcome)) => frame(&outcome),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(err("external-proof-expired", "proof deadline expired")),
        };
    let initial = match initial {
        Ok(frame) => frame,
        Err(error) => {
            let _ = cleanup(executor).await;
            let _ = begin_reply.send(Err(error));
            return;
        }
    };
    let mut proof = Proof::new(
        id,
        binding(&params, &job),
        actor_record(&actor),
        start,
        started_at,
        initial,
    );
    if begin_reply.send(Ok(proof.snapshot())).is_err() {
        let _ = cleanup(executor).await;
        return;
    }
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let message = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(message)) => message,
            _ => break,
        };
        if tokio::time::Instant::now() >= deadline {
            let _ = message
                .reply
                .send(Err(err("external-proof-expired", "proof deadline expired")));
            break;
        }
        let actions = match validate_step(&proof, &message.request) {
            Ok(value) => value,
            Err(error) => {
                let _ = message.reply.send(Err(error));
                continue;
            }
        };
        if matches!(
            message.request,
            Request::Close { .. } | Request::Abort { .. }
        ) {
            let aborted = matches!(message.request, Request::Abort { .. });
            let result=cleanup(executor).await.map(|()|json!({"ok":true,"profile":PROFILE,"proofId":proof.id,
                "sequence":proof.sequence+1,"closed":true,"aborted":aborted,"cleanupComplete":true,
                "receiptId":if aborted {Value::Null}else{proof.receipt.as_ref().map(|r|r["receiptId"].clone()).unwrap_or(Value::Null)}}));
            let _ = message.reply.send(result);
            return;
        }
        let result = tokio::time::timeout_at(
            deadline,
            step(&mut proof, &mut executor, &message.request, actions),
        )
        .await;
        match result {
            Ok(Ok(())) => {
                if tokio::time::Instant::now() >= deadline {
                    let _ = cleanup(executor).await;
                    let _ = message.reply.send(Err(err(
                        "external-proof-expired",
                        "proof deadline expired before completion",
                    )));
                    return;
                }
                if matches!(message.request, Request::Freeze { .. }) {
                    deadline = tokio::time::Instant::now() + Duration::from_secs(FROZEN_SECONDS);
                }
                // Losing a reply never replays input. The next caller can inspect the sequence.
                let _ = message.reply.send(Ok(proof.snapshot()));
            }
            Ok(Err(error)) => {
                let _ = cleanup(executor).await;
                let _ = message.reply.send(Err(error));
                return;
            }
            Err(_) => {
                let _ = cleanup(executor).await;
                let _ = message.reply.send(Err(err(
                    "external-proof-expired",
                    "proof deadline expired; attempt must not be replayed",
                )));
                return;
            }
        }
    }
    // Abandoned sessions expire even when no client reconnects. The native
    // executor retains exclusion through any detached OS work during Drop.
    let _ = cleanup(executor).await;
}
impl IntendantServer {
    #[tool(
        description = "Drive an owner-bound proof session using explicit actions, with no model calls. The duplicate-key-safe request commands are begin/actions/freeze/observe/finish/close/abort/status. Sessions retain exact display exclusion, limits, actor binding and cleanup across calls. finish records external claims, never independent policy approval. Returns private PNG observations and a versioned execution receipt."
    )]
    pub(crate) async fn external_cu_proof(
        &self,
        Parameters(params): Parameters<ExternalCuProofParams>,
    ) -> String {
        self.external_cu_proof_as_caller(
            params,
            ToolCallerTrust::OwnerSurface,
            &ActorBinding::local_process(None),
        )
        .await
    }
    pub(crate) async fn external_cu_proof_as_caller(
        &self,
        params: ExternalCuProofParams,
        caller: ToolCallerTrust,
        actor: &ActorBinding,
    ) -> String {
        let result = self.external_proof_dispatch(params, caller, actor).await;
        match result {Ok(value)=>value.to_string(),Err(error)=>json!({"ok":false,"error":{"code":error.code,"message":error.message,"retryable":error.retryable}}).to_string()}
    }
    async fn external_proof_dispatch(
        &self,
        params: ExternalCuProofParams,
        caller: ToolCallerTrust,
        actor: &ActorBinding,
    ) -> Result<Value, BoundedCuTaskError> {
        require_owner_surface(caller)?;
        if actor.kind == ActorKind::Unattributed {
            return Err(err(
                "external-proof-actor",
                "proof sessions require a gate-resolved actor",
            ));
        }
        let request = parse(&params.request)?;
        let (reply, receiver) = oneshot::channel();
        if let Request::Begin {
            attempt_id,
            workspace_id,
            display_id,
            display_target,
            capture_generation,
            job_sha256,
        } = request
        {
            if !sha_ok(&job_sha256) || display_id == 0 {
                return Err(err(
                    "external-proof-binding",
                    "a job digest and non-user virtual display are required",
                ));
            }
            let params = RunBoundedCuTaskParams {
                mode: crate::bounded_cu_task::BoundedCuTaskMode::Stage,
                attempt_id,
                workspace_id,
                display_id,
                display_target,
                capture_generation,
                task: job_sha256.clone(),
                prior_receipt_id: None,
            };
            validate_bounded_cu_task_request(&BoundedCuTaskRequest {
                mode: params.mode,
                attempt_id: params.attempt_id.clone(),
                workspace_id: params.workspace_id.clone(),
                display_id: params.display_id,
                display_target: params.display_target.clone(),
                capture_generation: params.capture_generation.clone(),
                task: params.task.clone(),
                prior_receipt_id: None,
                prior_transcript_event_count: None,
                prior_transcript_sha256: None,
                observation_sha256: None,
                prior_completed_at: None,
            })?;
            let id = format!("ecup-{}", Uuid::new_v4().simple());
            let (tx, rx) = mpsc::channel(1);
            {
                let mut map = registry()
                    .lock()
                    .map_err(|_| err("external-proof-registry", "proof registry unavailable"))?;
                if map.len() >= MAX_SESSIONS
                    || map
                        .values()
                        .any(|h| h.display == params.display_id || h.attempt == params.attempt_id)
                {
                    return Err(err(
                        "external-proof-busy",
                        "display, attempt, or proof capacity already reserved",
                    ));
                }
                map.insert(
                    id.clone(),
                    Handle {
                        actor: actor.clone(),
                        display: params.display_id,
                        attempt: params.attempt_id.clone(),
                        tx,
                    },
                );
            }
            tokio::spawn(worker(
                self.clone(),
                id,
                params,
                job_sha256,
                actor.clone(),
                rx,
                reply,
            ));
        } else {
            let handle = {
                let map = registry()
                    .lock()
                    .map_err(|_| err("external-proof-registry", "proof registry unavailable"))?;
                map.get(request.id().unwrap_or_default())
                    .filter(|h| &h.actor == actor)
                    .cloned()
                    .ok_or_else(|| {
                        err(
                            "external-proof-not-found",
                            "proof is absent, expired, or owned by another actor",
                        )
                    })?
            };
            handle
                .tx
                .try_send(Message { request, reply })
                .map_err(|_| {
                    err(
                        "external-proof-busy",
                        "proof is busy or closing; inspect before retrying",
                    )
                })?;
        }
        receiver.await.map_err(|_| {
            err(
                "external-proof-closed",
                "proof worker terminated; no success may be inferred",
            )
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn proof() -> Proof {
        Proof::new(
            "ecup-test".into(),
            json!({"jobSha256":"a".repeat(64)}),
            actor_record(&ActorBinding::local_process(Some("principal:test".into()))),
            Instant::now(),
            now(),
            json!({"sha256":"b".repeat(64),"byteLength":33,"width":1,"height":1,"pngBase64":"private"}),
        )
    }
    #[test]
    fn external_commands_reject_duplicate_unknown_and_trailing_data() {
        for raw in [
            r#"{"op":"status","op":"abort","proof_id":"x"}"#,
            r#"{"op":"status","proof_id":"x","actor":"owner"}"#,
            r#"{"op":"status","proof_id":"x"} {}"#,
        ] {
            assert!(parse(raw).is_err());
        }
        assert!(parse(r#"{"op":"status","proof_id":"x"}"#).is_ok());
    }
    #[test]
    fn external_actions_are_bounded_closed_and_clipboard_free() {
        for raw in [
            "[]",
            r#"[{"type":"paste","text":"secret"}]"#,
            r#"[{"type":"mouse_down","x":1,"y":1}]"#,
            r#"[{"type":"wait","ms":5001}]"#,
            r#"[{"type":"key","key":"x","shell":"bad"}]"#,
            r#"[{"type":"type","text":"a","text":"b"}]"#,
        ] {
            assert!(actions(raw).is_err(), "{raw}");
        }
        assert!(
            actions(r#"[{"type":"click","x":1,"y":2},{"type":"type","text":"public"}]"#).is_ok()
        );
        assert!(actions(
            &json!((0..17)
                .map(|_| json!({"type":"screenshot"}))
                .collect::<Vec<_>>())
            .to_string()
        )
        .is_err());
        assert!(actions(&json!([{"type":"type","text":"x".repeat(4097)}]).to_string()).is_err());
    }
    #[test]
    fn external_replayed_sequence_never_advances_state() {
        let mut proof = proof();
        proof.sequence = 4;
        let request = Request::Actions {
            proof_id: proof.id.clone(),
            sequence: 3,
            actions_json: r#"[{"type":"key","key":"a"}]"#.into(),
        };
        assert_eq!(
            validate_step(&proof, &request).unwrap_err().code,
            "external-proof-sequence"
        );
        assert_eq!(proof.action_count, 0);
        assert_eq!(proof.sequence, 4);
    }
    #[test]
    fn external_freeze_is_irreversible_and_observation_is_one_shot() {
        let mut proof = proof();
        for phase in [Phase::Frozen, Phase::Observed, Phase::Finished] {
            proof.phase = phase;
            assert!(validate_step(
                &proof,
                &Request::Actions {
                    proof_id: proof.id.clone(),
                    sequence: 0,
                    actions_json: r#"[{"type":"screenshot"}]"#.into()
                }
            )
            .is_err());
            assert!(validate_step(
                &proof,
                &Request::Freeze {
                    proof_id: proof.id.clone(),
                    sequence: 0
                }
            )
            .is_err());
        }
        proof.phase = Phase::Frozen;
        let observe = Request::Observe {
            proof_id: proof.id.clone(),
            sequence: 0,
            pre_observation_sha256: "c".repeat(64),
        };
        assert!(validate_step(&proof, &observe).is_ok());
        proof.phase = Phase::Observed;
        assert!(validate_step(&proof, &observe).is_err());
    }
    #[test]
    fn external_claims_require_exact_issued_frame_and_unique_json() {
        let mut proof = proof();
        proof.phase = Phase::Observed;
        for (hash, claims) in [
            ("a".repeat(64), "{}"),
            ("b".repeat(64), r#"{"ready":true,"ready":false}"#),
            ("b".repeat(64), "[]"),
        ] {
            assert!(validate_step(
                &proof,
                &Request::Finish {
                    proof_id: proof.id.clone(),
                    sequence: 0,
                    observation_sha256: hash,
                    claims_json: claims.into()
                }
            )
            .is_err());
        }
        assert!(validate_step(
            &proof,
            &Request::Finish {
                proof_id: proof.id.clone(),
                sequence: 0,
                observation_sha256: "b".repeat(64),
                claims_json: "{}".into()
            }
        )
        .is_ok());
    }
    #[test]
    fn external_total_action_budget_cannot_be_split_across_calls() {
        let mut proof = proof();
        proof.action_count = 64;
        assert!(validate_step(
            &proof,
            &Request::Actions {
                proof_id: proof.id.clone(),
                sequence: 0,
                actions_json: r#"[{"type":"wait","ms":1}]"#.into()
            }
        )
        .is_err());
    }
    #[test]
    fn external_redaction_has_no_typed_text_or_low_entropy_text_hash() {
        let one = actions(r#"[{"type":"type","text":"1234"}]"#).unwrap();
        let two = actions(r#"[{"type":"type","text":"other-value"}]"#).unwrap();
        assert_eq!(
            action_projection_sha256(&one).unwrap(),
            action_projection_sha256(&two).unwrap()
        );
    }
    #[test]
    fn external_receipt_is_domain_separated_without_a_model_claim() {
        let mut proof = proof();
        proof.phase = Phase::Observed;
        proof.frozen_at = Some(now());
        proof.observed_at = Some(now());
        let receipt = proof.make_receipt(r#"{"ready":true}"#).unwrap();
        assert_eq!(receipt["execution"]["internalModelCalls"], 0);
        assert_eq!(receipt["execution"]["claimsAuthority"], "external-caller");
        assert!(receipt.get("provider").is_none());
        assert!(receipt.get("model").is_none());
        assert!(receipt["observation"].get("pngBase64").is_none());
        let mut payload = receipt.clone();
        let id = payload
            .as_object_mut()
            .unwrap()
            .remove("receiptId")
            .unwrap();
        assert_eq!(
            id,
            format!(
                "ecup-receipt:{}",
                hash_value("intendant-external-cu-receipt-v1", &payload)
            )
        );
        payload["claimsJson"] = json!("{}");
        assert_ne!(
            id,
            format!(
                "ecup-receipt:{}",
                hash_value("intendant-external-cu-receipt-v1", &payload)
            )
        );
    }
    #[test]
    fn external_actor_identity_is_not_a_caller_label() {
        let first = ActorBinding::local_process(Some("principal:a".into()));
        let second = ActorBinding::local_process(Some("principal:b".into()));
        assert_ne!(first, second);
        assert_ne!(actor_record(&first), actor_record(&second));
        assert!(require_owner_surface(ToolCallerTrust::Scoped).is_err());
    }
    #[test]
    fn external_close_requires_a_completed_claim_receipt() {
        let mut proof = proof();
        let close = Request::Close {
            proof_id: proof.id.clone(),
            sequence: 0,
        };
        assert!(validate_step(&proof, &close).is_err());
        proof.phase = Phase::Finished;
        assert!(validate_step(&proof, &close).is_ok());
    }
    #[test]
    fn external_tool_is_not_advertised_to_scoped_profiles() {
        for profile in [
            Some("core"),
            Some("codex-core"),
            Some("cli"),
            Some("minimal"),
            Some("screen"),
            Some("display"),
            Some("managed"),
            Some("facade"),
        ] {
            assert!(!tool_allowed_for_profile(
                "external_cu_proof",
                false,
                profile
            ));
        }
        let mut manual = Vec::new();
        append_manual_http_tool_definitions(&mut manual, false, None);
        let tool = manual
            .iter()
            .find(|tool| tool["name"] == "external_cu_proof")
            .expect("external proof manual definition");
        assert_eq!(
            tool["description"].as_str(),
            IntendantServer::external_cu_proof_tool_attr()
                .description
                .as_deref()
        );
    }
}

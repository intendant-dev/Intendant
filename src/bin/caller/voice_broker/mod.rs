//! The Voice broker: the daemon side of the ChatGPT-subscription voice
//! presence lane.
//!
//! One component, three duties: (1) own a dedicated hardened Codex App
//! Server child per call session (spawned on demand — the permanent
//! presence thread lives on disk and is resumed each boot, so nothing
//! long-lived needs to linger between calls); (2) relay WebRTC
//! signaling between the authenticated dashboard presence connection
//! and the provider (media flows browser⇄provider — the daemon never
//! carries audio); (3) execute the presence toolset for the backing
//! model over the dynamicTools lane under the R3 authority machinery
//! (owner anchor + verbatim spoken evidence + two-principal audit).
//!
//! Single-active by construction: one call slot, owned by the presence
//! connection that opened it; handover and connection death stop the
//! call (a dead browser must not leave a session running, and a dead
//! app-server is call-terminal for the browser).

mod app_server;
mod store;
mod tools_lane;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::event::{AppEvent, EventBus};
use crate::types::LogLevel;
use presence_core::voice::{
    validate_voice_pin, PresenceVoiceConfig, VoiceStatus, DEFAULT_REALTIME_VERSION,
};

use app_server::{
    AppServerClient, AppServerEvents, VoiceAppServer, DYNAMIC_TOOL_CALL_METHOD,
    VOICE_REQUEST_TIMEOUT_SECS,
};
use store::{CheckpointTurn, VoiceCheckpoint, VoiceThreadRecord};
use tools_lane::{
    append_audit_record, verify_spoken_evidence, EvidenceVerdict, VoiceAuthorityAuditRecord,
    VoiceOwnerAnchor, SPOKEN_INSTRUCTION_ARG, VOICE_ACTING_PRINCIPAL, VOICE_AUTHORITY_TOOLS,
    VOICE_READ_TOOLS,
};

pub(crate) use app_server::resolve_app_server_command;

/// Synthetic vitals identity for the broker's allowance pulls: the
/// windows fold into the shared codex-account view (same subscription,
/// same credential era).
const VOICE_VITALS_SESSION_ID: &str = "voice-broker";

/// How long the broker waits for the provider's SDP answer before the
/// start attempt is declared failed (the `{}` start response is
/// provisional — A4).
const ANSWER_TIMEOUT_SECS: u64 = 30;

/// Injection queue silence threshold: appendText/appendSpeech are held
/// while the model is speaking (busy appendSpeech is silently dropped
/// by the provider — A7) and flushed after this much assistant-delta
/// silence.
const INJECTION_SILENCE_MS: u64 = 1_500;

/// Cap on a forwarded provider-usage payload (decoration, never
/// authoritative — no reason to store more than this).
const USAGE_PAYLOAD_CAP_BYTES: usize = 8 * 1024;

/// Lineage reason recorded when the capability-safe default retires the
/// durable thread instead of resuming it: the tool lane can only be
/// declared at `thread/start`, so a successor is minted with the
/// declaration on the wire (N1).
const TOOL_LANE_REDECLARE_REASON: &str = "tool-lane-redeclare";

/// Broker construction settings (resolved once at wiring).
#[derive(Clone)]
pub(crate) struct VoiceBrokerSettings {
    /// Whether `[presence] live_provider` selects the chatgpt lane.
    pub(crate) provider_selected: bool,
    /// App Server command (override > codex command).
    pub(crate) app_server_command: String,
    /// Explicit codex home override (lease-materialized home wins at
    /// spawn either way).
    pub(crate) codex_home: Option<PathBuf>,
    /// Daemon state root (presence stores live under it).
    pub(crate) state_root: PathBuf,
    pub(crate) voice: PresenceVoiceConfig,
}

/// Handles the session task needs to reach the rest of the daemon.
#[derive(Clone)]
pub(crate) struct VoiceCallDeps {
    pub(crate) bus: EventBus,
    pub(crate) state_root: PathBuf,
    pub(crate) voice: PresenceVoiceConfig,
    /// Send half of the presence WS connection that owns the call.
    pub(crate) reply_tx: mpsc::UnboundedSender<String>,
    pub(crate) connection_id: String,
    /// Live owner-anchor probe: true iff the connection still holds
    /// the active presence slot. Probed at tool entry AND immediately
    /// before dispatch (mid-flight loss refuses the in-flight act).
    pub(crate) anchor_probe: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    pub(crate) query_ctx: Option<crate::web_gateway::WebQueryCtx>,
    pub(crate) task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
}

/// The gateway collaborators the broker binds to at listener startup.
pub(crate) struct VoiceBrokerWiring {
    pub(crate) shared_session: crate::web_gateway::SharedActiveSession,
    pub(crate) task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
    pub(crate) anchor_probe: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

/// The active call slot.
struct ActiveCall {
    connection_id: String,
    stop_tx: Option<oneshot::Sender<String>>,
}

/// The Voice broker. One per daemon, constructed at gateway wiring.
pub(crate) struct VoiceBroker {
    settings: VoiceBrokerSettings,
    bus: EventBus,
    wiring: std::sync::Mutex<Option<Arc<VoiceBrokerWiring>>>,
    active: tokio::sync::Mutex<Option<ActiveCall>>,
    status: std::sync::Mutex<VoiceStatus>,
    watch: Option<crate::external_agent::protocol_watch::ProtocolWatchHandle>,
    identity_announced: std::sync::atomic::AtomicBool,
    voice_log_seq: AtomicU64,
}

impl VoiceBroker {
    pub(crate) fn new(settings: VoiceBrokerSettings, bus: EventBus) -> Arc<Self> {
        let watch = crate::external_agent::protocol_watch::ProtocolWatchHandle::new_in(
            settings.state_root.clone(),
            crate::external_agent::AgentBackend::Codex,
            "voice",
            &settings.app_server_command,
        );
        let record = VoiceThreadRecord::load(&settings.state_root);
        let status = VoiceStatus {
            available: settings.provider_selected,
            realtime_version: Some(
                settings
                    .voice
                    .realtime_version
                    .clone()
                    .unwrap_or_else(|| DEFAULT_REALTIME_VERSION.to_string()),
            ),
            voice: settings.voice.voice.clone(),
            thread_id: record.thread_id.clone(),
            thread_lineage_count: record.lineage.len() as u32,
            ..VoiceStatus::default()
        };
        Arc::new(Self {
            settings,
            bus,
            wiring: std::sync::Mutex::new(None),
            active: tokio::sync::Mutex::new(None),
            status: std::sync::Mutex::new(status),
            watch,
            identity_announced: std::sync::atomic::AtomicBool::new(false),
            voice_log_seq: AtomicU64::new(1),
        })
    }

    /// Late wiring: the gateway hands over the shared session state,
    /// task channel, and the owner-anchor probe once those exist.
    pub(crate) fn wire(&self, wiring: VoiceBrokerWiring) {
        *self.wiring.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(wiring));
    }

    pub(crate) fn status(&self) -> VoiceStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_status(&self, update: impl FnOnce(&mut VoiceStatus)) -> VoiceStatus {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        update(&mut status);
        status.clone()
    }

    fn push_status(&self, reply_tx: &mpsc::UnboundedSender<String>) {
        let status = self.status();
        let msg = serde_json::json!({"t": "voice_status", "status": status});
        let _ = reply_tx.send(msg.to_string());
    }

    /// Open a voice call for `connection_id` with the browser's SDP
    /// offer. Refuses when a call is already active (single-active),
    /// when the chatgpt lane is not the configured provider, or when
    /// the voice pin fails family validation (A4) — all named errors.
    pub(crate) async fn start_call(
        self: &Arc<Self>,
        connection_id: &str,
        offer_sdp: String,
        reply_tx: mpsc::UnboundedSender<String>,
    ) {
        if !self.settings.provider_selected {
            send_voice_error(
                &reply_tx,
                "voice lane is not configured (set [presence] live_provider = \"chatgpt\")",
            );
            return;
        }
        let version = self
            .settings
            .voice
            .realtime_version
            .clone()
            .unwrap_or_else(|| DEFAULT_REALTIME_VERSION.to_string());
        if let Err(e) = validate_voice_pin(&version, self.settings.voice.voice.as_deref()) {
            send_voice_error(&reply_tx, &format!("voice pin invalid: {e}"));
            return;
        }
        let wiring = self
            .wiring
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(wiring) = wiring else {
            send_voice_error(&reply_tx, "voice broker is not wired yet");
            return;
        };
        if !(wiring.anchor_probe)(connection_id) {
            send_voice_error(
                &reply_tx,
                "voice calls require the active presence connection (this connection is passive)",
            );
            return;
        }
        let mut active = self.active.lock().await;
        if let Some(existing) = active.as_ref() {
            let holder = existing.connection_id.clone();
            drop(active);
            send_voice_error(
                &reply_tx,
                &format!("a voice call is already active (connection {holder}); stop it first"),
            );
            return;
        }
        let query_ctx = wiring.shared_session.read().await.query_ctx.clone();
        let (stop_tx, stop_rx) = oneshot::channel();
        let deps = VoiceCallDeps {
            bus: self.bus.clone(),
            state_root: self.settings.state_root.clone(),
            voice: self.settings.voice.clone(),
            reply_tx: reply_tx.clone(),
            connection_id: connection_id.to_string(),
            anchor_probe: wiring.anchor_probe.clone(),
            query_ctx,
            task_tx: wiring.task_tx.clone(),
        };
        let broker = self.clone();
        let conn = connection_id.to_string();
        tokio::spawn(async move {
            broker.run_call(deps, offer_sdp, stop_rx).await;
            broker.clear_call_slot(&conn).await;
        });
        *active = Some(ActiveCall {
            connection_id: connection_id.to_string(),
            stop_tx: Some(stop_tx),
        });
    }

    async fn clear_call_slot(&self, connection_id: &str) {
        let mut active = self.active.lock().await;
        if active
            .as_ref()
            .map(|c| c.connection_id == connection_id)
            .unwrap_or(false)
        {
            *active = None;
        }
    }

    async fn signal_stop(&self, connection_id: &str, reason: &str, only_if_owner: bool) {
        let mut active = self.active.lock().await;
        let owns = active
            .as_ref()
            .map(|c| c.connection_id == connection_id)
            .unwrap_or(false);
        if !owns && only_if_owner {
            return;
        }
        if let Some(call) = active.as_mut() {
            if let Some(tx) = call.stop_tx.take() {
                let _ = tx.send(reason.to_string());
            }
        }
    }

    /// Owner asked to stop (voice_stop frame).
    pub(crate) async fn stop_call(&self, connection_id: &str) {
        self.signal_stop(connection_id, "stopped", true).await;
    }

    /// The owning presence connection went away (browser death /
    /// WS drop): the call is stopped — a dead browser must not leave a
    /// realtime session running.
    pub(crate) async fn connection_closed(&self, connection_id: &str) {
        self.signal_stop(connection_id, "connection-closed", true)
            .await;
    }

    /// Presence handover force-disconnected the previous holder: the
    /// call it owned stops with it (single-active).
    pub(crate) async fn connection_superseded(&self, connection_id: &str) {
        self.signal_stop(connection_id, "handover", true).await;
    }

    /// Provider-reported usage forwarded from the browser data channel
    /// (A1/A2: decoration, labeled provider-reported, never window
    /// authority). Non-null `usage_limit.status` is the exhaustion
    /// signal driving the named degradation notice.
    pub(crate) async fn ingest_usage(&self, connection_id: &str, payload: serde_json::Value) {
        let owns = self
            .active
            .lock()
            .await
            .as_ref()
            .map(|c| c.connection_id == connection_id)
            .unwrap_or(false);
        if !owns {
            return;
        }
        if serde_json::to_string(&payload)
            .map(|s| s.len() > USAGE_PAYLOAD_CAP_BYTES)
            .unwrap_or(true)
        {
            return;
        }
        let pressure = payload
            .get("usage_limit")
            .and_then(|l| l.get("status"))
            .filter(|s| !s.is_null())
            .map(|s| s.to_string());
        let status = self.set_status(|s| {
            s.last_usage = Some(payload.clone());
            if let Some(p) = pressure.as_ref() {
                s.last_error = Some(format!("voice allowance pressure: {p}"));
            }
        });
        if let Some(p) = pressure {
            self.bus.send(AppEvent::PresenceLog {
                message: format!("voice allowance pressure signaled by provider: {p}"),
                level: Some(LogLevel::Warn),
                turn: None,
            });
        }
        let _ = status;
    }

    /// Owner purge lever (D1): delete the durable presence thread and
    /// drop the identity + lineage; the next call mints fresh.
    pub(crate) async fn purge_thread(&self, reply_tx: mpsc::UnboundedSender<String>) {
        if self.active.lock().await.is_some() {
            send_voice_error(&reply_tx, "cannot purge the presence thread during a call");
            return;
        }
        let mut record = VoiceThreadRecord::load(&self.settings.state_root);
        if let Some(thread_id) = record.thread_id.clone() {
            // Best-effort provider-side delete on a short-lived child.
            match VoiceAppServer::spawn(
                &self.settings.app_server_command,
                self.settings.codex_home.as_deref(),
                &store::neutral_cwd_path(&self.settings.state_root),
                self.watch.clone(),
            )
            .await
            {
                Ok(server) => {
                    let gate = server.initialize_and_gate().await;
                    if gate.is_ok() {
                        let _ = server
                            .client
                            .request(
                                "thread/delete",
                                Some(serde_json::json!({ "threadId": thread_id })),
                                VOICE_REQUEST_TIMEOUT_SECS,
                            )
                            .await;
                    }
                    server.shutdown().await;
                }
                Err(e) => {
                    eprintln!("[voice] Warning: purge spawn failed: {e}");
                }
            }
        }
        record.purge();
        if let Err(e) = record.save(&self.settings.state_root) {
            send_voice_error(&reply_tx, &format!("purge failed: {e}"));
            return;
        }
        let checkpoint = VoiceCheckpoint::default();
        let _ = checkpoint.save(&self.settings.state_root);
        self.set_status(|s| {
            s.thread_id = None;
            s.thread_lineage_count = 0;
        });
        self.push_status(&reply_tx);
        self.bus.send(AppEvent::PresenceLog {
            message: "voice presence thread purged by owner".to_string(),
            level: Some(LogLevel::Info),
            turn: None,
        });
    }

    /// Production call runner: spawn + gate the hardened child, then
    /// hand the streams to the transport-generic core.
    async fn run_call(
        self: &Arc<Self>,
        deps: VoiceCallDeps,
        offer_sdp: String,
        stop_rx: oneshot::Receiver<String>,
    ) {
        let mut server = match VoiceAppServer::spawn(
            &self.settings.app_server_command,
            self.settings.codex_home.as_deref(),
            &store::neutral_cwd_path(&self.settings.state_root),
            self.watch.clone(),
        )
        .await
        {
            Ok(server) => server,
            Err(e) => {
                send_voice_error(
                    &deps.reply_tx,
                    &format!("voice app-server spawn failed: {e}"),
                );
                return;
            }
        };
        match server.initialize_and_gate().await {
            Ok(version) => {
                if let Some(watch) = self.watch.as_ref() {
                    watch.mark_observed(version);
                }
            }
            Err(e) => {
                send_voice_error(&deps.reply_tx, &e);
                server.shutdown().await;
                return;
            }
        }
        // Pre-call allowance pull (D3: visible before connect).
        self.pull_and_ingest_rate_limits(&server.client).await;
        let events = match server.take_events() {
            Some(events) => events,
            None => {
                send_voice_error(&deps.reply_tx, "voice app-server events unavailable");
                server.shutdown().await;
                return;
            }
        };
        let outcome = self
            .drive_call(&deps, &server.client, events, offer_sdp, stop_rx)
            .await;
        // Post-call allowance pull + checkpoint fold, then teardown.
        self.pull_and_ingest_rate_limits(&server.client).await;
        self.finish_call(&deps, outcome);
        server.shutdown().await;
    }

    /// Transport-generic call core: thread setup, realtime session,
    /// event loop, tool lane. Everything R4 pins runs through here so
    /// the failure legs are testable against a scripted mock server.
    async fn drive_call<W>(
        self: &Arc<Self>,
        deps: &VoiceCallDeps,
        client: &AppServerClient<W>,
        mut events: AppServerEvents,
        offer_sdp: String,
        mut stop_rx: oneshot::Receiver<String>,
    ) -> CallOutcome
    where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let version = deps
            .voice
            .realtime_version
            .clone()
            .unwrap_or_else(|| DEFAULT_REALTIME_VERSION.to_string());
        // Thread: resume the durable identity, or mint (successor) with
        // lineage recorded (D1). Resume is owner-elected
        // (`trust_resume_tool_persistence`): the protocol declares
        // dynamicTools only at `thread/start`, and the verified server
        // lineage drops them on every from-disk resume, so the
        // capability-safe default retires the durable id into the
        // lineage and re-declares the tool lane on a successor (the
        // checkpoint, not the thread, is the authoritative memory).
        let mut record = VoiceThreadRecord::load(&deps.state_root);
        let mut resolved = ResolvedThread::default();
        let mut thread_ready = false;
        if let Some(thread_id) = record.thread_id.clone() {
            if !deps.voice.trust_resume_tool_persistence {
                record.pending_retire_reason = Some(TOOL_LANE_REDECLARE_REASON.to_string());
            } else {
                match client
                    .request(
                        "thread/resume",
                        Some(thread_pins(
                            &deps.state_root,
                            &deps.voice,
                            Some(thread_id.as_str()),
                            false,
                        )),
                        VOICE_REQUEST_TIMEOUT_SECS,
                    )
                    .await
                {
                    Ok(resp) => {
                        resolved.absorb(&resp);
                        resolved.thread_id = Some(thread_id);
                        thread_ready = true;
                        // Honest by name: nothing in the protocol lets the
                        // broker verify the resumed thread kept its
                        // declared tools — the owner elected to trust it.
                        deps.bus.send(AppEvent::PresenceLog {
                            message: "voice presence thread resumed; tool lane rides \
                                      owner-trusted provider-side persistence \
                                      (trust_resume_tool_persistence = true) — \
                                      thread/resume cannot re-declare dynamicTools"
                                .to_string(),
                            level: Some(LogLevel::Warn),
                            turn: None,
                        });
                    }
                    Err(e) => {
                        eprintln!(
                            "[voice] Warning: presence thread resume failed ({e}); minting successor"
                        );
                        record_resume_failure_reason(&mut record, &e);
                    }
                }
            }
        }
        if !thread_ready {
            match client
                .request(
                    "thread/start",
                    Some(thread_pins(&deps.state_root, &deps.voice, None, true)),
                    VOICE_REQUEST_TIMEOUT_SECS,
                )
                .await
            {
                Ok(resp) => {
                    resolved.absorb(&resp);
                    let new_id = resp
                        .get("thread")
                        .and_then(|t| t.get("id"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    match new_id {
                        Some(id) => {
                            let reason = record
                                .pending_retire_reason
                                .take()
                                .unwrap_or_else(|| "initial".to_string());
                            record.adopt(&id, now_epoch(), &reason);
                            if let Err(e) = record.save(&deps.state_root) {
                                eprintln!("[voice] Warning: thread record save failed: {e}");
                            }
                            resolved.thread_id = Some(id);
                        }
                        None => {
                            send_voice_error(
                                &deps.reply_tx,
                                "thread/start returned no thread id; voice unavailable",
                            );
                            return CallOutcome::failed("thread-start-no-id");
                        }
                    }
                }
                Err(e) => {
                    send_voice_error(
                        &deps.reply_tx,
                        &format!("presence thread start failed: {e}"),
                    );
                    return CallOutcome::failed("thread-start-failed");
                }
            }
        }
        let thread_id = resolved.thread_id.clone().unwrap_or_default();
        self.announce_vitals_identity(&thread_id);
        let status = self.set_status(|s| {
            s.active = false;
            s.thread_id = Some(thread_id.clone());
            s.thread_lineage_count = record.lineage.len() as u32;
            s.resolved_model = resolved.model.clone();
            s.resolved_effort = resolved.effort.clone();
            s.realtime_version = Some(version.clone());
            s.voice = deps.voice.voice.clone();
            s.last_error = None;
        });
        let _ = status;
        self.push_status(&deps.reply_tx);

        // Seed from the checkpoint (D1: checkpoint-authoritative) with
        // the live state block derived from the same status tool the
        // presence surfaces use.
        let checkpoint = VoiceCheckpoint::load(&deps.state_root);
        let state_block = build_state_block(deps);
        let seed = store::build_seed_items(&checkpoint, &state_block);
        let mut realtime_params = serde_json::json!({
            "threadId": thread_id,
            "outputModality": "audio",
            "version": version,
            "includeStartupContext": false,
            "transport": { "type": "webrtc", "sdp": offer_sdp },
        });
        if !seed.is_empty() {
            realtime_params["initialItems"] = serde_json::Value::Array(seed);
        }
        if let Some(voice) = deps.voice.voice.as_ref() {
            realtime_params["voice"] = serde_json::json!(voice);
        }
        if let Err(e) = client
            .request(
                "thread/realtime/start",
                Some(realtime_params),
                VOICE_REQUEST_TIMEOUT_SECS,
            )
            .await
        {
            send_voice_error(&deps.reply_tx, &format!("realtime start failed: {e}"));
            return CallOutcome::failed("realtime-start-failed");
        }

        // Event loop. The start response above is provisional (A4):
        // real failures race in as thread/realtime/error.
        let transcript = TranscriptState::default();
        let mut injections: std::collections::VecDeque<Injection> =
            std::collections::VecDeque::new();
        let mut last_assistant_delta_ms: Option<u64> = None;
        let mut answered = false;
        let mut bus_rx = deps.bus.subscribe();
        let mut last_phase = String::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let answer_deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(ANSWER_TIMEOUT_SECS);
        let mut call_turns: Vec<CheckpointTurn> = Vec::new();
        loop {
            tokio::select! {
                biased;
                stop = &mut stop_rx => {
                    let reason = stop.unwrap_or_else(|_| "stopped".to_string());
                    let _ = client
                        .request(
                            "thread/realtime/stop",
                            Some(serde_json::json!({ "threadId": thread_id })),
                            5,
                        )
                        .await;
                    // Wait briefly for the provider's closed notification,
                    // then report closed regardless — teardown never hangs
                    // on remote behavior.
                    let waited = tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        wait_for_closed(&mut events, &thread_id),
                    )
                    .await;
                    let provider_reason = waited.ok().flatten();
                    send_voice_closed(
                        &deps.reply_tx,
                        provider_reason.as_deref().unwrap_or(reason.as_str()),
                    );
                    return CallOutcome::closed(reason, call_turns, transcript);
                }
                exited = &mut events.exited => {
                    let _ = exited;
                    // R4: the app-server died — signaling is gone, the
                    // call is terminal for the browser.
                    send_voice_closed(&deps.reply_tx, "app-server-lost");
                    return CallOutcome::closed("app-server-lost".to_string(), call_turns, transcript);
                }
                Some(notification) = events.notifications.recv() => {
                    match notification.method.as_str() {
                        "thread/realtime/sdp" => {
                            if let Some(sdp) = notification.params.get("sdp").and_then(|s| s.as_str()) {
                                answered = true;
                                let msg = serde_json::json!({"t": "voice_answer", "sdp": sdp});
                                let _ = deps.reply_tx.send(msg.to_string());
                            }
                        }
                        "thread/realtime/started" => {
                            let session_version = notification
                                .params
                                .get("version")
                                .and_then(|v| v.as_str())
                                .map(str::to_string);
                            self.set_status(|s| {
                                s.active = true;
                                if let Some(v) = session_version {
                                    s.realtime_version = Some(v);
                                }
                            });
                            self.push_status(&deps.reply_tx);
                        }
                        "thread/realtime/error" => {
                            let message = notification
                                .params
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("realtime error")
                                .to_string();
                            send_voice_error(&deps.reply_tx, &message);
                            self.set_status(|s| {
                                s.active = false;
                                s.last_error = Some(message.clone());
                            });
                            if !answered {
                                // Start rejected asynchronously (A4).
                                return CallOutcome::failed("realtime-error-before-answer");
                            }
                            send_voice_closed(&deps.reply_tx, "error");
                            return CallOutcome::closed(format!("error: {message}"), call_turns, transcript);
                        }
                        "thread/realtime/closed" => {
                            let reason = notification
                                .params
                                .get("reason")
                                .and_then(|r| r.as_str())
                                .unwrap_or("closed")
                                .to_string();
                            send_voice_closed(&deps.reply_tx, &reason);
                            return CallOutcome::closed(reason, call_turns, transcript);
                        }
                        "thread/realtime/transcript/delta" => {
                            if notification.params.get("role").and_then(|r| r.as_str())
                                == Some("assistant")
                            {
                                last_assistant_delta_ms = Some(now_ms());
                            }
                        }
                        "model/rerouted" => {
                            // N5: mid-call backing-lane reroutes stay
                            // visible — status update for the voice card
                            // plus the named PresenceEvent through the
                            // presence pump. Stage A's acceptance of the
                            // account-default backing lane rests on this.
                            let from_model = notification
                                .params
                                .get("fromModel")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let to_model = notification
                                .params
                                .get("toModel")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let reason = notification.params.get("reason").map(|r| match r.as_str()
                            {
                                Some(s) => s.to_string(),
                                None => r.to_string(),
                            });
                            self.set_status(|s| {
                                s.resolved_model = Some(to_model.clone());
                            });
                            self.push_status(&deps.reply_tx);
                            deps.bus.send(AppEvent::VoiceModelRerouted {
                                from_model,
                                to_model,
                                reason,
                            });
                        }
                        "thread/realtime/transcript/done" => {
                            let role = notification
                                .params
                                .get("role")
                                .and_then(|r| r.as_str())
                                .unwrap_or("")
                                .to_string();
                            let text = notification
                                .params
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            if text.trim().is_empty() {
                                continue;
                            }
                            if role == "user" {
                                transcript.push_user(&text);
                            }
                            call_turns.push(CheckpointTurn {
                                role: role.clone(),
                                text: text.clone(),
                            });
                            let seq = self.voice_log_seq.fetch_add(1, Ordering::Relaxed);
                            deps.bus.send(AppEvent::VoiceLog {
                                text: format!("[{role}] {text}"),
                                seq,
                                tool_context: Some("transcript".to_string()),
                            });
                        }
                        _ => {
                            // Known-but-unrouted notifications are fine;
                            // unknown ones were already recorded by the
                            // drift watch at ingest.
                        }
                    }
                }
                Some(request) = events.server_requests.recv() => {
                    if request.method == DYNAMIC_TOOL_CALL_METHOD {
                        let response = self
                            .handle_dynamic_tool_call(deps, &transcript, &request.params)
                            .await;
                        let _ = client.respond(request.id, response).await;
                    }
                }
                event = bus_rx.recv() => {
                    match event {
                        Ok(app_event) => {
                            if let Some(presence_event) =
                                crate::presence::filter_event(&app_event, &mut last_phase)
                            {
                                if let Some(injection) = injection_for(&presence_event) {
                                    injections.push_back(injection);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    }
                }
                _ = tick.tick() => {
                    if !answered && tokio::time::Instant::now() >= answer_deadline {
                        send_voice_error(&deps.reply_tx, "no SDP answer from provider (timeout)");
                        return CallOutcome::failed("answer-timeout");
                    }
                    // A7: flush queued injections only behind model
                    // silence (busy appendSpeech is silently dropped by
                    // the provider; we queue instead).
                    let silent = last_assistant_delta_ms
                        .map(|t| now_ms().saturating_sub(t) >= INJECTION_SILENCE_MS)
                        .unwrap_or(true);
                    if answered && silent {
                        while let Some(injection) = injections.pop_front() {
                            let (method, params) = match injection {
                                Injection::Text(text) => (
                                    "thread/realtime/appendText",
                                    serde_json::json!({
                                        "threadId": thread_id,
                                        "role": "developer",
                                        "text": text,
                                    }),
                                ),
                                Injection::Speech(text) => (
                                    "thread/realtime/appendSpeech",
                                    serde_json::json!({
                                        "threadId": thread_id,
                                        "text": text,
                                    }),
                                ),
                            };
                            if client
                                .request(method, Some(params), VOICE_REQUEST_TIMEOUT_SECS)
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    /// The R3 gate + executor for one dynamic tool call.
    async fn handle_dynamic_tool_call(
        self: &Arc<Self>,
        deps: &VoiceCallDeps,
        transcript: &TranscriptState,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        let tool = params.get("tool").and_then(|t| t.as_str()).unwrap_or("");
        let call_id = params
            .get("callId")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let mut args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        // Arguments sometimes arrive JSON-encoded as a string.
        if let Some(raw) = args.as_str() {
            args = serde_json::from_str(raw).unwrap_or_else(|_| serde_json::json!({}));
        }
        let is_authority = VOICE_AUTHORITY_TOOLS.contains(&tool);
        let is_read = VOICE_READ_TOOLS.contains(&tool);
        if !is_authority && !is_read {
            return tool_response(false, format!("unknown tool \"{tool}\" on the voice lane"));
        }
        if is_read {
            let text = self.execute_presence_tool(deps, tool, &args).await;
            return tool_response(true, text);
        }

        // Authority-bearing act: anchor, evidence, dispatch, audit.
        let spoken = args
            .get(SPOKEN_INSTRUCTION_ARG)
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let mut audit = VoiceAuthorityAuditRecord {
            ts_epoch: now_epoch(),
            call_id,
            tool: tool.to_string(),
            acting_principal: VOICE_ACTING_PRINCIPAL.to_string(),
            attributed_owner: None,
            machine_mediated: true,
            spoken_instruction: if spoken.is_empty() {
                None
            } else {
                Some(spoken.clone())
            },
            evidence_verified: false,
            verdict: String::new(),
            detail: None,
        };

        // (a) Live owner anchor at entry.
        if !(deps.anchor_probe)(&deps.connection_id) {
            audit.verdict = "refused-anchor".to_string();
            self.write_audit(deps, &audit);
            return tool_response(
                false,
                "refused: no live owner anchor (the authenticated dashboard voice connection is gone); the action was not performed",
            );
        }
        audit.attributed_owner = Some(VoiceOwnerAnchor {
            connection_id: deps.connection_id.clone(),
        });

        // (b) Verbatim spoken evidence, mechanically verified.
        match verify_spoken_evidence(&spoken, &transcript.user_segments()) {
            EvidenceVerdict::Verified => {
                audit.evidence_verified = true;
            }
            EvidenceVerdict::Insufficient => {
                audit.verdict = "refused-evidence-insufficient".to_string();
                self.write_audit(deps, &audit);
                return tool_response(
                    false,
                    "refused: spoken_instruction evidence is missing or too short — quote the owner's exact words and ask again if unsure",
                );
            }
            EvidenceVerdict::NotInTranscript => {
                audit.verdict = "refused-evidence-unmatched".to_string();
                self.write_audit(deps, &audit);
                return tool_response(
                    false,
                    "refused: the quoted words were not found in the owner's live transcript; ask the owner to restate the instruction",
                );
            }
        }

        // (a) again — mid-flight anchor loss refuses the in-flight act.
        if !(deps.anchor_probe)(&deps.connection_id) {
            audit.verdict = "refused-anchor-midflight".to_string();
            self.write_audit(deps, &audit);
            return tool_response(
                false,
                "refused: the owner anchor was lost mid-flight; the action was not performed",
            );
        }

        // Dispatch (authorization unchanged: this is the same dispatch
        // the owner's authenticated presence surface drives).
        let mut exec_args = args.clone();
        if let Some(obj) = exec_args.as_object_mut() {
            obj.remove(SPOKEN_INSTRUCTION_ARG);
        }
        let text = self.execute_presence_tool(deps, tool, &exec_args).await;
        audit.verdict = "dispatched".to_string();
        audit.detail = Some(text.clone());
        self.write_audit(deps, &audit);
        tool_response(true, text)
    }

    /// Execute one presence tool exactly the way the browser presence
    /// lane does: presence-core dispatch, then task_tx / ControlMsg /
    /// query path.
    async fn execute_presence_tool(
        self: &Arc<Self>,
        deps: &VoiceCallDeps,
        tool: &str,
        args: &serde_json::Value,
    ) -> String {
        let state = deps
            .query_ctx
            .as_ref()
            .map(|ctx| {
                ctx.agent_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            })
            .unwrap_or_default();
        let action = crate::presence::dispatch_tool_call(tool, args, &state);
        if let crate::presence::PresenceAction::SubmitTask(envelope) = action {
            let msg = format!("Task submitted: {}", envelope.task);
            if let Some(tx) = deps.task_tx.as_ref() {
                let _ = tx.send(envelope).await;
            } else {
                let ctrl_action = crate::presence::PresenceAction::SubmitTask(envelope);
                if let Some((ctrl, _)) = crate::presence::action_to_control_msg(&ctrl_action) {
                    deps.bus.send(AppEvent::ControlCommand(ctrl));
                }
            }
            return msg;
        }
        if let Some((ctrl, msg)) = crate::presence::action_to_control_msg(&action) {
            deps.bus.send(AppEvent::ControlCommand(ctrl));
            return msg;
        }
        match action {
            crate::presence::PresenceAction::TextResult(text) => text,
            crate::presence::PresenceAction::NeedsIO { tool_name, args } => {
                if let Some(ctx) = deps.query_ctx.as_ref() {
                    match crate::presence::handle_tool_query(
                        &ctx.agent_state,
                        &ctx.project_root,
                        &ctx.log_dir,
                        &tool_name,
                        &args,
                        None,
                        ctx.context_injection.as_ref(),
                    )
                    .await
                    {
                        Some(result) => result.text,
                        None => format!("Unknown tool: {tool_name}"),
                    }
                } else {
                    "presence query context unavailable".to_string()
                }
            }
            _ => "unsupported action".to_string(),
        }
    }

    fn write_audit(&self, deps: &VoiceCallDeps, record: &VoiceAuthorityAuditRecord) {
        if let Err(e) = append_audit_record(&deps.state_root, record) {
            eprintln!("[voice] Warning: audit append failed: {e}");
        }
        deps.bus.send(AppEvent::PresenceLog {
            message: format!(
                "[voice-authority] {} {} (acting: {}, owner surface: {}, evidence: {}) — machine-mediated",
                record.verdict,
                record.tool,
                record.acting_principal,
                record
                    .attributed_owner
                    .as_ref()
                    .map(|a| a.connection_id.as_str())
                    .unwrap_or("<none>"),
                if record.evidence_verified { "verified" } else { "unverified" },
            ),
            level: Some(LogLevel::Info),
            turn: None,
        });
    }

    fn finish_call(&self, deps: &VoiceCallDeps, outcome: CallOutcome) {
        eprintln!("[voice] call ended: {}", outcome.reason);
        let presence_summary = deps
            .query_ctx
            .as_ref()
            .and_then(|ctx| ctx.presence_session.as_ref())
            .and_then(|session| {
                session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_checkpoint_summary()
            });
        if !outcome.turns.is_empty() || presence_summary.is_some() {
            let mut checkpoint = VoiceCheckpoint::load(&deps.state_root);
            checkpoint.fold_call(&outcome.turns, presence_summary.as_deref(), now_epoch());
            if let Err(e) = checkpoint.save(&deps.state_root) {
                eprintln!("[voice] Warning: checkpoint save failed: {e}");
            }
        }
        self.set_status(|s| {
            s.active = false;
        });
        self.push_status(&deps.reply_tx);
    }

    async fn pull_and_ingest_rate_limits<W>(&self, client: &AppServerClient<W>)
    where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        match client
            .request(
                "account/rateLimits/read",
                Some(serde_json::json!({})),
                VOICE_REQUEST_TIMEOUT_SECS,
            )
            .await
        {
            Ok(response) => {
                let windows = parse_keyed_rate_limit_windows(&response, now_epoch());
                if !windows.is_empty() {
                    self.bus.send(AppEvent::SessionRateLimits {
                        session_id: Some(VOICE_VITALS_SESSION_ID.to_string()),
                        windows,
                    });
                }
            }
            Err(e) => {
                eprintln!("[voice] Warning: rateLimits read failed: {e}");
            }
        }
    }

    fn announce_vitals_identity(&self, thread_id: &str) {
        if self
            .identity_announced
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        self.bus.send(AppEvent::SessionIdentity {
            session_id: VOICE_VITALS_SESSION_ID.to_string(),
            source: "codex".to_string(),
            backend_session_id: thread_id.to_string(),
        });
    }
}

/// Layer-2 pins (S-1..S-4 at the thread layer, per A6 pinned at BOTH
/// layers): neutral cwd, read-only sandbox, never approvals, pinned
/// metadata — plus the dynamicTools declaration and the D2 backing
/// pins on `thread/start`.
///
/// N1 protocol ground truth (source-verified on the codex-rs lineage
/// matching the Stage B binaries): dynamicTools is declared ONLY at
/// `thread/start` (`thread/start.dynamicTools`); `ThreadResumeParams`
/// has no such field and silently ignores unknown params (no
/// deny_unknown_fields), so a resume-time re-declaration is
/// unrepresentable — not merely unverified. The server's from-disk
/// resume path hardcodes an empty dynamic-tools set
/// (`resume_thread_with_history_with_source` → `Vec::new()`), and no
/// response or request surface exposes a thread's tool state, so the
/// client can neither re-declare, re-verify, nor detect-and-refuse
/// after the fact. The broker therefore only trusts a tool lane it
/// declared on the wire in this process: by default it retires the
/// durable id into the D1 lineage and starts a successor with the
/// declaration (`trust_resume_tool_persistence` is the owner's
/// escape hatch, verified live per deployed binary).
fn thread_pins(
    state_root: &std::path::Path,
    voice: &PresenceVoiceConfig,
    resume_thread_id: Option<&str>,
    declare_tools: bool,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "cwd": store::neutral_cwd_path(state_root).to_string_lossy(),
        "sandbox": "read-only",
        "approvalPolicy": "never",
    });
    if let Some(id) = resume_thread_id {
        params["threadId"] = serde_json::json!(id);
    } else {
        params["ephemeral"] = serde_json::json!(false);
        params["sessionStartSource"] = serde_json::json!("intendantVoice");
    }
    if let Some(model) = voice.backing_model.as_ref() {
        params["model"] = serde_json::json!(model);
    }
    if let Some(effort) = voice.backing_effort.as_ref() {
        params["config"] = serde_json::json!({ "model_reasoning_effort": effort });
    }
    if declare_tools {
        params["dynamicTools"] = serde_json::Value::Array(tools_lane::dynamic_tool_specs());
    }
    params
}

/// Resolved backing-lane facts from thread start/resume responses (D2:
/// the envelope shows what the provider actually granted).
#[derive(Default)]
struct ResolvedThread {
    thread_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

impl ResolvedThread {
    fn absorb(&mut self, response: &serde_json::Value) {
        if let Some(model) = response.get("model").and_then(|m| m.as_str()) {
            self.model = Some(model.to_string());
        }
        if let Some(effort) = response.get("reasoningEffort").and_then(|e| e.as_str()) {
            self.effort = Some(effort.to_string());
        }
    }
}

/// Shared transcript state for the live call: the user-role segments
/// feed the R3 evidence check; all segments feed the checkpoint.
#[derive(Default, Clone)]
struct TranscriptState {
    inner: Arc<std::sync::Mutex<Vec<String>>>,
}

impl TranscriptState {
    fn push_user(&self, text: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.push(text.to_string());
        let excess = inner
            .len()
            .saturating_sub(tools_lane::EVIDENCE_TRANSCRIPT_WINDOW);
        if excess > 0 {
            inner.drain(..excess);
        }
    }

    fn user_segments(&self) -> Vec<String> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

enum Injection {
    /// appendText role=developer — the silent state-update lane.
    Text(String),
    /// appendSpeech — the model voices it (summary-grade, A7).
    Speech(String),
}

/// Which presence events reach the live voice session, and on which
/// lane. Deliberately conservative: approvals and questions speak;
/// completions and errors update silently; phase spam stays out.
fn injection_for(event: &presence_core::PresenceEvent) -> Option<Injection> {
    use presence_core::PresenceEvent as E;
    let formatted = presence_core::format_event(event);
    match event {
        E::ApprovalNeeded { .. } | E::HumanQuestion { .. } => Some(Injection::Speech(formatted)),
        E::TaskComplete { .. }
        | E::ApprovalResolved { .. }
        | E::Error { .. }
        | E::BudgetWarning { .. } => Some(Injection::Text(format!(
            "State update (inform the owner if relevant): {formatted}"
        ))),
        _ => None,
    }
}

struct CallOutcome {
    reason: String,
    turns: Vec<CheckpointTurn>,
}

impl CallOutcome {
    fn closed(reason: String, turns: Vec<CheckpointTurn>, _transcript: TranscriptState) -> Self {
        Self { reason, turns }
    }
    fn failed(reason: &str) -> Self {
        Self {
            reason: reason.to_string(),
            turns: Vec::new(),
        }
    }
}

/// Wait for the closed notification after a stop (bounded by caller).
async fn wait_for_closed(events: &mut AppServerEvents, thread_id: &str) -> Option<String> {
    while let Some(notification) = events.notifications.recv().await {
        if notification.method == "thread/realtime/closed"
            && notification
                .params
                .get("threadId")
                .and_then(|t| t.as_str())
                .map(|t| t == thread_id)
                .unwrap_or(true)
        {
            return notification
                .params
                .get("reason")
                .and_then(|r| r.as_str())
                .map(str::to_string);
        }
    }
    None
}

fn build_state_block(deps: &VoiceCallDeps) -> String {
    let state = deps
        .query_ctx
        .as_ref()
        .map(|ctx| {
            ctx.agent_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
        .unwrap_or_default();
    let status =
        match crate::presence::dispatch_tool_call("check_status", &serde_json::json!({}), &state) {
            crate::presence::PresenceAction::TextResult(text) => text,
            _ => String::new(),
        };
    format!(
        "You are the voice of this Intendant daemon's presence layer. You hear the owner and can act through your tools; authority-bearing tools require quoting the owner's exact spoken words as spoken_instruction.\n\nLive state at session start:\n{status}"
    )
}

fn tool_response(success: bool, text: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "contentItems": [ { "type": "inputText", "text": text.into() } ],
        "success": success,
    })
}

fn send_voice_error(reply_tx: &mpsc::UnboundedSender<String>, message: &str) {
    let msg = serde_json::json!({"t": "voice_error", "message": message});
    let _ = reply_tx.send(msg.to_string());
}

fn send_voice_closed(reply_tx: &mpsc::UnboundedSender<String>, reason: &str) {
    let msg = serde_json::json!({"t": "voice_closed", "reason": reason});
    let _ = reply_tx.send(msg.to_string());
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse an `account/rateLimits/read` response into limit-keyed vitals
/// windows. The `codex` limit keeps the driver's plain duration labels
/// (same account, same windows); any OTHER limit id becomes its own
/// named window class — `{slug}-{duration}` from the provider's
/// `limitName`/`limitId`, never invented ahead of observation (A1).
fn parse_keyed_rate_limit_windows(
    response: &serde_json::Value,
    now_epoch: u64,
) -> Vec<crate::types::SessionLimitWindow> {
    let mut out = Vec::new();
    let keyed = response
        .get("rateLimitsByLimitId")
        .and_then(|k| k.as_object());
    match keyed {
        Some(map) => {
            for (limit_id, snapshot) in map {
                let wrapped = serde_json::json!({ "rateLimits": snapshot });
                let mut windows =
                    crate::external_agent::codex::codex_rate_limit_windows(&wrapped, now_epoch);
                if limit_id != "codex" {
                    let slug = snapshot
                        .get("limitName")
                        .and_then(|n| n.as_str())
                        .map(slugify)
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| slugify(limit_id));
                    for w in &mut windows {
                        w.label = format!("{slug}-{}", w.label);
                    }
                }
                out.extend(windows);
            }
        }
        None => {
            // Older shape: single snapshot under rateLimits.
            out.extend(crate::external_agent::codex::codex_rate_limit_windows(
                response, now_epoch,
            ));
        }
    }
    out
}

fn slugify(raw: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            dash = true;
        }
    }
    out
}

fn record_resume_failure_reason(record: &mut VoiceThreadRecord, error: &str) {
    record.pending_retire_reason = Some(format!("resume-failed: {error}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ControlMsg;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A scripted mock app-server over duplex pipes. Records every
    /// line it receives (requests AND the broker's responses to
    /// server-requests); answers thread lifecycle + realtime methods;
    /// can push arbitrary lines; the `__DIE__` sentinel drops the
    /// write half to simulate app-server death.
    fn spawn_mock_server(
        server_side: tokio::io::DuplexStream,
        fail_resume: bool,
        auto_answer_realtime: bool,
    ) -> (
        Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        mpsc::UnboundedSender<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let (read_half, mut write_half) = tokio::io::split(server_side);
        let requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let (push_tx, mut push_rx) = mpsc::unbounded_channel::<String>();
        let requests_task = requests.clone();
        let push_tx_inner = push_tx.clone();
        let handle = tokio::spawn(async move {
            let mut lines = BufReader::new(read_half).lines();
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break };
                        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                        requests_task
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(msg.clone());
                        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let id = msg.get("id").and_then(|i| i.as_u64());
                        let Some(id) = id else { continue };
                        let reply = match method {
                            "initialize" => Some(serde_json::json!({"ok": true})),
                            "experimentalFeature/list" => Some(serde_json::json!({
                                "features": [{"name": "realtime_conversation", "enabled": true}]
                            })),
                            "thread/resume" => {
                                if fail_resume {
                                    let line = serde_json::json!({
                                        "jsonrpc": "2.0", "id": id,
                                        "error": {"code": -32000, "message": "thread not found"}
                                    });
                                    let _ = write_half
                                        .write_all(format!("{line}\n").as_bytes())
                                        .await;
                                    let _ = write_half.flush().await;
                                    continue;
                                }
                                Some(serde_json::json!({
                                    "thread": {"id": msg["params"]["threadId"]},
                                    "model": "gpt-resumed", "reasoningEffort": "high"
                                }))
                            }
                            "thread/start" => Some(serde_json::json!({
                                "thread": {"id": "t-new"},
                                "model": "gpt-fresh", "reasoningEffort": "xhigh"
                            })),
                            "thread/realtime/start" => {
                                if auto_answer_realtime {
                                    let _ = push_tx_inner.send(
                                        serde_json::json!({
                                            "jsonrpc": "2.0",
                                            "method": "thread/realtime/sdp",
                                            "params": {"threadId": msg["params"]["threadId"], "sdp": "answer-sdp"}
                                        })
                                        .to_string(),
                                    );
                                    let _ = push_tx_inner.send(
                                        serde_json::json!({
                                            "jsonrpc": "2.0",
                                            "method": "thread/realtime/started",
                                            "params": {"threadId": msg["params"]["threadId"], "realtimeSessionId": "rs-1", "version": "v3"}
                                        })
                                        .to_string(),
                                    );
                                }
                                Some(serde_json::json!({}))
                            }
                            "thread/realtime/stop" => {
                                let _ = push_tx_inner.send(
                                    serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "method": "thread/realtime/closed",
                                        "params": {"threadId": msg["params"]["threadId"], "reason": "requested"}
                                    })
                                    .to_string(),
                                );
                                Some(serde_json::json!({}))
                            }
                            "account/rateLimits/read" => Some(serde_json::json!({
                                "rateLimitsByLimitId": {
                                    "codex": {"limitId": "codex", "limitName": null,
                                        "primary": {"usedPercent": 3, "windowDurationMins": 10080}},
                                }
                            })),
                            "thread/realtime/appendText" | "thread/realtime/appendSpeech" => {
                                Some(serde_json::json!({}))
                            }
                            "thread/delete" => Some(serde_json::json!({})),
                            _ => Some(serde_json::json!({})),
                        };
                        if let Some(result) = reply {
                            let line = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                            if write_half
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                            let _ = write_half.flush().await;
                        }
                    }
                    pushed = push_rx.recv() => {
                        let Some(pushed) = pushed else { break };
                        if pushed == "__DIE__" {
                            break; // drop write half → EOF at the client
                        }
                        if write_half.write_all(format!("{pushed}\n").as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = write_half.flush().await;
                    }
                }
            }
        });
        (requests, push_tx, handle)
    }

    struct Rig {
        broker: Arc<VoiceBroker>,
        deps: VoiceCallDeps,
        reply_rx: mpsc::UnboundedReceiver<String>,
        requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        push_tx: mpsc::UnboundedSender<String>,
        client: AppServerClient<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
        events: Option<AppServerEvents>,
        _tmp: tempfile::TempDir,
        _server: tokio::task::JoinHandle<()>,
    }

    fn build_rig(
        fail_resume: bool,
        auto_answer: bool,
        anchor: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Rig {
        build_rig_with_voice(
            fail_resume,
            auto_answer,
            anchor,
            PresenceVoiceConfig::default(),
        )
    }

    fn build_rig_with_voice(
        fail_resume: bool,
        auto_answer: bool,
        anchor: Arc<dyn Fn(&str) -> bool + Send + Sync>,
        voice: PresenceVoiceConfig,
    ) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let settings = VoiceBrokerSettings {
            provider_selected: true,
            app_server_command: "codex-unused".to_string(),
            codex_home: None,
            state_root: tmp.path().to_path_buf(),
            voice: voice.clone(),
        };
        let broker = VoiceBroker::new(settings, bus.clone());
        let (reply_tx, reply_rx) = mpsc::unbounded_channel();
        let deps = VoiceCallDeps {
            bus,
            state_root: tmp.path().to_path_buf(),
            voice,
            reply_tx,
            connection_id: "conn-1".to_string(),
            anchor_probe: anchor,
            query_ctx: None,
            task_tx: None,
        };
        let (server_side, client_side) = tokio::io::duplex(256 * 1024);
        let (requests, push_tx, server) = spawn_mock_server(server_side, fail_resume, auto_answer);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (client, events) = app_server::start_client(client_read, client_write, None);
        Rig {
            broker,
            deps,
            reply_rx,
            requests,
            push_tx,
            client,
            events: Some(events),
            _tmp: tmp,
            _server: server,
        }
    }

    async fn recv_t(rx: &mut mpsc::UnboundedReceiver<String>, want: &str) -> serde_json::Value {
        loop {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timeout waiting for {want}"))
                .expect("reply channel open");
            let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
            if v["t"] == want {
                return v;
            }
        }
    }

    fn seed_transcript(push_tx: &mpsc::UnboundedSender<String>, text: &str) {
        let _ = push_tx.send(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "thread/realtime/transcript/done",
                "params": {"threadId": "t-new", "role": "user", "text": text}
            })
            .to_string(),
        );
    }

    fn send_tool_call(
        push_tx: &mpsc::UnboundedSender<String>,
        id: u64,
        tool: &str,
        args: serde_json::Value,
    ) {
        let _ = push_tx.send(
            serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": "item/tool/call",
                "params": {"threadId": "t-new", "turnId": "turn-1", "callId": format!("call-{id}"),
                            "namespace": null, "tool": tool, "arguments": args}
            })
            .to_string(),
        );
    }

    async fn server_reply_for(
        requests: &Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        id: u64,
    ) -> Option<serde_json::Value> {
        // Tool-call responses flow client→server; the mock records them
        // like requests (they arrive on the same pipe).
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let seen = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(resp) = seen.iter().find(|m| {
                m.get("result").is_some()
                    && m.get("method").is_none()
                    && m.get("id").and_then(|i| i.as_u64()) == Some(id)
            }) {
                return Some(resp.clone());
            }
        }
        None
    }

    // Happy path: pins at the thread layer (A6 layer 2), the SDP answer
    // reaches the browser, and a clean stop rides the provider's closed
    // notification.
    #[tokio::test]
    async fn call_flow_delivers_answer_and_stop_closes_cleanly() {
        let rig = build_rig(false, true, Arc::new(|_c: &str| true));
        let (stop_tx, stop_rx) = oneshot::channel();
        let broker = rig.broker.clone();
        let deps = rig.deps.clone();
        let client = rig.client;
        let events = rig.events.unwrap();
        let mut reply_rx = rig.reply_rx;
        let task = tokio::spawn(async move {
            broker
                .drive_call(&deps, &client, events, "offer-sdp".to_string(), stop_rx)
                .await
        });
        let answer = recv_t(&mut reply_rx, "voice_answer").await;
        assert_eq!(answer["sdp"], "answer-sdp");
        let status = recv_t(&mut reply_rx, "voice_status").await;
        assert_eq!(status["status"]["thread_id"], "t-new");
        stop_tx.send("stopped".to_string()).unwrap();
        let closed = recv_t(&mut reply_rx, "voice_closed").await;
        assert_eq!(closed["reason"], "requested");
        let outcome = task.await.unwrap();
        assert_eq!(outcome.reason, "stopped");
        // Layer-2 pins on the wire: thread/start carried cwd, sandbox,
        // approvalPolicy, non-ephemeral, dynamicTools; realtime start
        // carried includeStartupContext:false + webrtc transport.
        let seen = rig
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let start = seen
            .iter()
            .find(|m| m["method"] == "thread/start")
            .expect("thread/start sent");
        assert_eq!(start["params"]["sandbox"], "read-only");
        assert_eq!(start["params"]["approvalPolicy"], "never");
        assert_eq!(start["params"]["ephemeral"], false);
        assert!(start["params"]["cwd"]
            .as_str()
            .unwrap()
            .contains("neutral-cwd"));
        let tools = start["params"]["dynamicTools"]
            .as_array()
            .expect("tools declared");
        assert_eq!(
            tools.len(),
            VOICE_AUTHORITY_TOOLS.len() + VOICE_READ_TOOLS.len()
        );
        let rt = seen
            .iter()
            .find(|m| m["method"] == "thread/realtime/start")
            .expect("realtime start sent");
        assert_eq!(rt["params"]["includeStartupContext"], false);
        assert_eq!(rt["params"]["outputModality"], "audio");
        assert_eq!(rt["params"]["transport"]["type"], "webrtc");
        assert_eq!(rt["params"]["transport"]["sdp"], "offer-sdp");
        assert_eq!(rt["params"]["version"], "v3");
    }

    fn trusting_voice() -> PresenceVoiceConfig {
        PresenceVoiceConfig {
            trust_resume_tool_persistence: true,
            ..PresenceVoiceConfig::default()
        }
    }

    // D1: resume failure mints a successor and records lineage. Resume
    // itself is owner-elected (`trust_resume_tool_persistence`).
    #[tokio::test]
    async fn resume_failure_mints_successor_with_lineage() {
        let rig = build_rig_with_voice(true, true, Arc::new(|_c: &str| true), trusting_voice());
        // Seed a durable identity that the mock will refuse to resume.
        let mut record = VoiceThreadRecord::default();
        record.adopt("t-old", 1, "initial");
        record.save(&rig.deps.state_root).unwrap();
        let (stop_tx, stop_rx) = oneshot::channel();
        let broker = rig.broker.clone();
        let deps = rig.deps.clone();
        let client = rig.client;
        let events = rig.events.unwrap();
        let mut reply_rx = rig.reply_rx;
        let task = tokio::spawn(async move {
            broker
                .drive_call(&deps, &client, events, "offer".to_string(), stop_rx)
                .await
        });
        let _ = recv_t(&mut reply_rx, "voice_answer").await;
        stop_tx.send("stopped".to_string()).unwrap();
        let _ = task.await.unwrap();
        let record = VoiceThreadRecord::load(&rig.deps.state_root);
        assert_eq!(record.thread_id.as_deref(), Some("t-new"));
        assert_eq!(record.lineage.len(), 1);
        assert_eq!(record.lineage[0].thread_id, "t-old");
        assert!(record.lineage[0].reason.starts_with("resume-failed"));
    }

    // N1 default: the capability-safe policy never sends thread/resume —
    // the durable id retires into the lineage with the named reason and
    // the successor's thread/start re-declares the tool lane on the
    // wire (the only declaration point the protocol has).
    #[tokio::test]
    async fn default_policy_redeclares_tool_lane_on_successor_instead_of_resuming() {
        let rig = build_rig(false, true, Arc::new(|_c: &str| true));
        let mut record = VoiceThreadRecord::default();
        record.adopt("t-old", 1, "initial");
        record.save(&rig.deps.state_root).unwrap();
        let (stop_tx, stop_rx) = oneshot::channel();
        let broker = rig.broker.clone();
        let deps = rig.deps.clone();
        let client = rig.client;
        let events = rig.events.unwrap();
        let mut reply_rx = rig.reply_rx;
        let task = tokio::spawn(async move {
            broker
                .drive_call(&deps, &client, events, "offer".to_string(), stop_rx)
                .await
        });
        let _ = recv_t(&mut reply_rx, "voice_answer").await;
        stop_tx.send("stopped".to_string()).unwrap();
        let _ = task.await.unwrap();
        let seen = rig
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert!(
            !seen.iter().any(|m| m["method"] == "thread/resume"),
            "default policy must not resume into an unverifiable tool lane"
        );
        let start = seen
            .iter()
            .find(|m| m["method"] == "thread/start")
            .expect("successor thread/start sent");
        let tools = start["params"]["dynamicTools"]
            .as_array()
            .expect("successor declares the tool lane");
        assert_eq!(
            tools.len(),
            VOICE_AUTHORITY_TOOLS.len() + VOICE_READ_TOOLS.len()
        );
        let record = VoiceThreadRecord::load(&rig.deps.state_root);
        assert_eq!(record.thread_id.as_deref(), Some("t-new"));
        assert_eq!(record.lineage.len(), 1);
        assert_eq!(record.lineage[0].thread_id, "t-old");
        assert_eq!(record.lineage[0].reason, TOOL_LANE_REDECLARE_REASON);
    }

    // N1 owner election: trusting resume reuses the durable thread; the
    // resume request carries NO dynamicTools field (the protocol has
    // none — pinned so a future re-declaration attempt can't silently
    // regress to an ignored unknown param), and the unverifiable
    // persistence is surfaced as a named PresenceLog warning.
    #[tokio::test]
    async fn trusted_resume_reuses_thread_and_surfaces_unverifiable_tool_lane() {
        let rig = build_rig_with_voice(false, true, Arc::new(|_c: &str| true), trusting_voice());
        let mut record = VoiceThreadRecord::default();
        record.adopt("t-old", 1, "initial");
        record.save(&rig.deps.state_root).unwrap();
        let mut bus_rx = rig.deps.bus.subscribe();
        let (stop_tx, stop_rx) = oneshot::channel();
        let broker = rig.broker.clone();
        let deps = rig.deps.clone();
        let client = rig.client;
        let events = rig.events.unwrap();
        let mut reply_rx = rig.reply_rx;
        let task = tokio::spawn(async move {
            broker
                .drive_call(&deps, &client, events, "offer".to_string(), stop_rx)
                .await
        });
        let _ = recv_t(&mut reply_rx, "voice_answer").await;
        stop_tx.send("stopped".to_string()).unwrap();
        let _ = task.await.unwrap();
        let seen = rig
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let resume = seen
            .iter()
            .find(|m| m["method"] == "thread/resume")
            .expect("trusted policy resumes the durable thread");
        assert_eq!(resume["params"]["threadId"], "t-old");
        assert!(
            resume["params"].get("dynamicTools").is_none(),
            "thread/resume has no dynamicTools field; sending one would be silently ignored"
        );
        assert!(!seen.iter().any(|m| m["method"] == "thread/start"));
        let record = VoiceThreadRecord::load(&rig.deps.state_root);
        assert_eq!(record.thread_id.as_deref(), Some("t-old"));
        assert!(record.lineage.is_empty());
        // The named warning rode the bus.
        let mut warned = false;
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::PresenceLog { message, level, .. } = event {
                if message.contains("trust_resume_tool_persistence")
                    && level == Some(LogLevel::Warn)
                {
                    warned = true;
                }
            }
        }
        assert!(
            warned,
            "trusted resume must surface the unverifiable tool lane"
        );
    }

    // R4: app-server death mid-call is call-terminal for the browser.
    #[tokio::test]
    async fn app_server_death_is_call_terminal() {
        let rig = build_rig(false, true, Arc::new(|_c: &str| true));
        let (_stop_tx, stop_rx) = oneshot::channel::<String>();
        let broker = rig.broker.clone();
        let deps = rig.deps.clone();
        let client = rig.client;
        let events = rig.events.unwrap();
        let mut reply_rx = rig.reply_rx;
        let push_tx = rig.push_tx.clone();
        let task = tokio::spawn(async move {
            broker
                .drive_call(&deps, &client, events, "offer".to_string(), stop_rx)
                .await
        });
        let _ = recv_t(&mut reply_rx, "voice_answer").await;
        push_tx.send("__DIE__".to_string()).unwrap();
        let closed = recv_t(&mut reply_rx, "voice_closed").await;
        assert_eq!(closed["reason"], "app-server-lost");
        let outcome = task.await.unwrap();
        assert_eq!(outcome.reason, "app-server-lost");
    }

    // A4: an async realtime error before any SDP answer is a named
    // failure to the browser, not a hang.
    #[tokio::test]
    async fn realtime_error_before_answer_is_named_failure() {
        let rig = build_rig(false, false, Arc::new(|_c: &str| true));
        let (_stop_tx, stop_rx) = oneshot::channel::<String>();
        let broker = rig.broker.clone();
        let deps = rig.deps.clone();
        let client = rig.client;
        let events = rig.events.unwrap();
        let mut reply_rx = rig.reply_rx;
        let push_tx = rig.push_tx.clone();
        let requests = rig.requests.clone();
        let task = tokio::spawn(async move {
            broker
                .drive_call(&deps, &client, events, "offer".to_string(), stop_rx)
                .await
        });
        // Wait for realtime/start to be sent, then push the async error.
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|m| m["method"] == "thread/realtime/start")
            {
                break;
            }
        }
        push_tx
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0", "method": "thread/realtime/error",
                    "params": {"threadId": "t-new", "message": "realtime voice `cedar` is not supported for v3"}
                })
                .to_string(),
            )
            .unwrap();
        let err = recv_t(&mut reply_rx, "voice_error").await;
        assert!(err["message"].as_str().unwrap().contains("cedar"));
        let outcome = task.await.unwrap();
        assert_eq!(outcome.reason, "realtime-error-before-answer");
    }

    // R3: the full gate — no anchor refuses; bad evidence refuses;
    // verified evidence dispatches a real ControlMsg; mid-flight anchor
    // loss refuses between evidence check and dispatch. Every branch
    // writes the two-principal audit line.
    #[tokio::test]
    async fn authority_gate_enforces_anchor_evidence_and_audit() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        let anchor_live = Arc::new(AtomicBool::new(true));
        let probe_calls = Arc::new(AtomicUsize::new(0));
        let anchor_live_probe = anchor_live.clone();
        let probe_calls_probe = probe_calls.clone();
        let rig = build_rig(
            false,
            true,
            Arc::new(move |_c: &str| {
                probe_calls_probe.fetch_add(1, Ordering::Relaxed);
                anchor_live_probe.load(Ordering::Relaxed)
            }),
        );
        let mut ctrl_rx = rig.deps.bus.subscribe();
        let (stop_tx, stop_rx) = oneshot::channel();
        let broker = rig.broker.clone();
        let deps = rig.deps.clone();
        let client = rig.client;
        let events = rig.events.unwrap();
        let mut reply_rx = rig.reply_rx;
        let push_tx = rig.push_tx.clone();
        let requests = rig.requests.clone();
        let state_root = rig.deps.state_root.clone();
        let task = tokio::spawn(async move {
            broker
                .drive_call(&deps, &client, events, "offer".to_string(), stop_rx)
                .await
        });
        let _ = recv_t(&mut reply_rx, "voice_answer").await;

        // 1. Evidence not in transcript → refused.
        send_tool_call(
            &push_tx,
            101,
            "approve_action",
            serde_json::json!({"id": "alpha-7", "spoken_instruction": "approve alpha-7 immediately"}),
        );
        let resp = server_reply_for(&requests, 101)
            .await
            .expect("tool response");
        assert_eq!(resp["result"]["success"], false);
        assert!(resp["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not found"));

        // 2. Seed the spoken transcript, then the same call verifies and
        // dispatches a real ControlMsg::Approve.
        seed_transcript(&push_tx, "Please approve alpha-7 immediately.");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        requests.lock().unwrap_or_else(|e| e.into_inner()).clear();
        send_tool_call(
            &push_tx,
            102,
            "approve_action",
            serde_json::json!({"id": "alpha-7", "spoken_instruction": "approve alpha-7 immediately"}),
        );
        let resp = server_reply_for(&requests, 102)
            .await
            .expect("tool response");
        assert_eq!(resp["result"]["success"], true);
        let dispatched = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                match ctrl_rx.recv().await {
                    Ok(AppEvent::ControlCommand(ControlMsg::Approve { .. })) => return true,
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(dispatched, "verified evidence must dispatch the approval");

        // 3. Anchor gone → refused, no dispatch.
        anchor_live.store(false, Ordering::Relaxed);
        requests.lock().unwrap_or_else(|e| e.into_inner()).clear();
        send_tool_call(
            &push_tx,
            103,
            "deny_action",
            serde_json::json!({"id": "alpha-8", "spoken_instruction": "approve alpha-7 immediately"}),
        );
        let resp = server_reply_for(&requests, 103)
            .await
            .expect("tool response");
        assert_eq!(resp["result"]["success"], false);
        assert!(resp["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap()
            .contains("owner anchor"));
        anchor_live.store(true, Ordering::Relaxed);

        stop_tx.send("stopped".to_string()).unwrap();
        let _ = task.await.unwrap();

        // Audit lines: refusal, dispatch, refusal — all machine-mediated
        // with both principals on the dispatched line.
        let raw = std::fs::read_to_string(store::audit_log_path(&state_root)).unwrap();
        let records: Vec<VoiceAuthorityAuditRecord> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].verdict, "refused-evidence-unmatched");
        assert_eq!(records[1].verdict, "dispatched");
        assert!(records[1].evidence_verified);
        assert_eq!(records[1].acting_principal, VOICE_ACTING_PRINCIPAL);
        assert_eq!(
            records[1].attributed_owner.as_ref().unwrap().connection_id,
            "conn-1"
        );
        assert!(records.iter().all(|r| r.machine_mediated));
        assert_eq!(records[2].verdict, "refused-anchor");
    }

    // R3 mid-flight: anchor alive at entry, gone at the pre-dispatch
    // re-probe → the in-flight act is refused.
    #[tokio::test]
    async fn midflight_anchor_loss_refuses_inflight_act() {
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_probe = calls.clone();
        // First probe (entry) true, second (pre-dispatch) false.
        let rig = build_rig(
            false,
            true,
            Arc::new(move |_c: &str| calls_probe.fetch_add(1, Ordering::Relaxed) == 0),
        );
        let (stop_tx, stop_rx) = oneshot::channel();
        let broker = rig.broker.clone();
        let deps = rig.deps.clone();
        let client = rig.client;
        let events = rig.events.unwrap();
        let mut reply_rx = rig.reply_rx;
        let push_tx = rig.push_tx.clone();
        let requests = rig.requests.clone();
        let state_root = rig.deps.state_root.clone();
        let task = tokio::spawn(async move {
            broker
                .drive_call(&deps, &client, events, "offer".to_string(), stop_rx)
                .await
        });
        let _ = recv_t(&mut reply_rx, "voice_answer").await;
        seed_transcript(&push_tx, "please skip the alpha-9 action now");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        requests.lock().unwrap_or_else(|e| e.into_inner()).clear();
        send_tool_call(
            &push_tx,
            201,
            "skip_action",
            serde_json::json!({"id": "alpha-9", "spoken_instruction": "skip the alpha-9 action now"}),
        );
        let resp = server_reply_for(&requests, 201)
            .await
            .expect("tool response");
        assert_eq!(resp["result"]["success"], false);
        assert!(resp["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap()
            .contains("mid-flight"));
        stop_tx.send("stopped".to_string()).unwrap();
        let _ = task.await.unwrap();
        let raw = std::fs::read_to_string(store::audit_log_path(&state_root)).unwrap();
        let records: Vec<VoiceAuthorityAuditRecord> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].verdict, "refused-anchor-midflight");
    }

    // ── N2: the start_call refusal ladder, entered at start_call ──
    //
    // The broker tests above enter at drive_call; these pin the named
    // refusals the gateway-facing entry point itself owes: provider
    // unselected, invalid voice pin, unwired broker, passive
    // connection, second call. None of them may reach an app-server
    // spawn.

    fn bare_broker(
        provider_selected: bool,
        voice: PresenceVoiceConfig,
    ) -> (Arc<VoiceBroker>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let settings = VoiceBrokerSettings {
            provider_selected,
            app_server_command: "codex-unused".to_string(),
            codex_home: None,
            state_root: tmp.path().to_path_buf(),
            voice,
        };
        (VoiceBroker::new(settings, EventBus::new()), tmp)
    }

    fn wire_with_anchor(broker: &Arc<VoiceBroker>, anchor_live: bool) {
        broker.wire(VoiceBrokerWiring {
            shared_session: crate::web_gateway::ActiveSessionState::empty(),
            task_tx: None,
            anchor_probe: Arc::new(move |_c: &str| anchor_live),
        });
    }

    async fn start_and_expect_error(broker: &Arc<VoiceBroker>, needle: &str) {
        let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
        broker
            .start_call("conn-1", "offer-sdp".to_string(), reply_tx)
            .await;
        let err = recv_t(&mut reply_rx, "voice_error").await;
        let message = err["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(needle),
            "expected a named refusal containing {needle:?}, got: {message}"
        );
    }

    #[tokio::test]
    async fn start_call_refuses_unselected_provider() {
        let (broker, _tmp) = bare_broker(false, PresenceVoiceConfig::default());
        start_and_expect_error(&broker, "not configured").await;
    }

    #[tokio::test]
    async fn start_call_refuses_invalid_voice_pin() {
        // cedar is a v2 voice; the default version is v3 (v1 family) —
        // the A4 family validation refuses before anything spawns.
        let voice = PresenceVoiceConfig {
            voice: Some("cedar".to_string()),
            ..PresenceVoiceConfig::default()
        };
        let (broker, _tmp) = bare_broker(true, voice);
        start_and_expect_error(&broker, "voice pin invalid").await;
    }

    #[tokio::test]
    async fn start_call_refuses_unwired_broker() {
        let (broker, _tmp) = bare_broker(true, PresenceVoiceConfig::default());
        start_and_expect_error(&broker, "not wired").await;
    }

    #[tokio::test]
    async fn start_call_refuses_passive_connection() {
        let (broker, _tmp) = bare_broker(true, PresenceVoiceConfig::default());
        wire_with_anchor(&broker, false);
        start_and_expect_error(&broker, "active presence connection").await;
    }

    #[tokio::test]
    async fn start_call_refuses_second_call_while_one_is_active() {
        let (broker, _tmp) = bare_broker(true, PresenceVoiceConfig::default());
        wire_with_anchor(&broker, true);
        *broker.active.lock().await = Some(ActiveCall {
            connection_id: "conn-holder".to_string(),
            stop_tx: None,
        });
        start_and_expect_error(&broker, "already active").await;
        // The named error names the holding connection.
        let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
        broker
            .start_call("conn-1", "offer-sdp".to_string(), reply_tx)
            .await;
        let err = recv_t(&mut reply_rx, "voice_error").await;
        assert!(err["message"].as_str().unwrap().contains("conn-holder"));
    }

    // ── N5: model/rerouted mid-call → status update + named event ──
    #[tokio::test]
    async fn model_rerouted_midcall_updates_status_and_emits_named_event() {
        let rig = build_rig(false, true, Arc::new(|_c: &str| true));
        let mut bus_rx = rig.deps.bus.subscribe();
        let (stop_tx, stop_rx) = oneshot::channel();
        let broker = rig.broker.clone();
        let deps = rig.deps.clone();
        let client = rig.client;
        let events = rig.events.unwrap();
        let mut reply_rx = rig.reply_rx;
        let push_tx = rig.push_tx.clone();
        let task = tokio::spawn(async move {
            broker
                .drive_call(&deps, &client, events, "offer".to_string(), stop_rx)
                .await
        });
        let _ = recv_t(&mut reply_rx, "voice_answer").await;
        // Drain the per-start status pushes so the reroute's own status
        // push is the one asserted below.
        let started = recv_t(&mut reply_rx, "voice_status").await;
        assert_eq!(started["status"]["resolved_model"], "gpt-fresh");
        push_tx
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0", "method": "model/rerouted",
                    "params": {"threadId": "t-new", "turnId": "turn-1",
                                "fromModel": "gpt-fresh", "toModel": "gpt-fallback",
                                "reason": "capacity"}
                })
                .to_string(),
            )
            .unwrap();
        // Status update: the voice card's resolved model follows the
        // reroute mid-call.
        let rerouted = loop {
            let status = recv_t(&mut reply_rx, "voice_status").await;
            if status["status"]["resolved_model"] == "gpt-fallback" {
                break status;
            }
        };
        assert_eq!(rerouted["status"]["resolved_model"], "gpt-fallback");
        assert_eq!(
            rig.broker.status().resolved_model.as_deref(),
            Some("gpt-fallback")
        );
        // Named event on the bus, mapped to the named PresenceEvent by
        // the presence pump's filter.
        let named = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                match bus_rx.recv().await {
                    Ok(AppEvent::VoiceModelRerouted {
                        from_model,
                        to_model,
                        reason,
                    }) => return Some((from_model, to_model, reason)),
                    Ok(_) => continue,
                    Err(_) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
        .expect("named VoiceModelRerouted event on the bus");
        assert_eq!(named.0, "gpt-fresh");
        assert_eq!(named.1, "gpt-fallback");
        assert_eq!(named.2.as_deref(), Some("capacity"));
        let mut phase = String::new();
        let presence_event = crate::presence::filter_event(
            &AppEvent::VoiceModelRerouted {
                from_model: named.0,
                to_model: named.1,
                reason: named.2,
            },
            &mut phase,
        )
        .expect("reroute maps to a named PresenceEvent");
        let formatted = presence_core::format_event(&presence_event);
        assert!(formatted.contains("gpt-fresh") && formatted.contains("gpt-fallback"));
        stop_tx.send("stopped".to_string()).unwrap();
        let _ = task.await.unwrap();
    }

    // Read tools need no evidence and dispatch straight through.
    #[tokio::test]
    async fn read_tools_serve_without_evidence() {
        let rig = build_rig(false, true, Arc::new(|_c: &str| true));
        let (stop_tx, stop_rx) = oneshot::channel();
        let broker = rig.broker.clone();
        let deps = rig.deps.clone();
        let client = rig.client;
        let events = rig.events.unwrap();
        let mut reply_rx = rig.reply_rx;
        let push_tx = rig.push_tx.clone();
        let requests = rig.requests.clone();
        let task = tokio::spawn(async move {
            broker
                .drive_call(&deps, &client, events, "offer".to_string(), stop_rx)
                .await
        });
        let _ = recv_t(&mut reply_rx, "voice_answer").await;
        send_tool_call(&push_tx, 301, "check_status", serde_json::json!({}));
        let resp = server_reply_for(&requests, 301)
            .await
            .expect("tool response");
        assert_eq!(resp["result"]["success"], true);
        stop_tx.send("stopped".to_string()).unwrap();
        let _ = task.await.unwrap();
    }

    // Vitals: keyed windows — codex keeps plain labels; other limit ids
    // become their own named window class (A1: named from provider
    // identifiers, never invented).
    #[test]
    fn keyed_rate_limit_windows_label_by_limit_id() {
        let response = serde_json::json!({
            "rateLimitsByLimitId": {
                "codex": {"limitId": "codex", "limitName": null,
                    "primary": {"usedPercent": 3, "windowDurationMins": 10080}},
                "codex_bengalfox": {"limitId": "codex_bengalfox", "limitName": "GPT-5.3-Codex-Spark",
                    "primary": {"usedPercent": 1, "windowDurationMins": 10080}},
            }
        });
        let mut windows = parse_keyed_rate_limit_windows(&response, 1000);
        windows.sort_by(|a, b| a.label.cmp(&b.label));
        let labels: Vec<&str> = windows.iter().map(|w| w.label.as_str()).collect();
        assert!(labels.contains(&"7d"));
        assert!(labels.contains(&"gpt-5-3-codex-spark-7d"));
    }

    #[test]
    fn slugify_flattens_provider_names() {
        assert_eq!(slugify("GPT-5.3-Codex-Spark"), "gpt-5-3-codex-spark");
        assert_eq!(slugify("codex_bengalfox"), "codex-bengalfox");
    }
}

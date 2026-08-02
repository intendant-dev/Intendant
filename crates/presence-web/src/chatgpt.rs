//! ChatGPT-subscription voice provider (browser side).
//!
//! Unlike the WebSocket providers, media rides a WebRTC peer connection
//! browser⇄provider and the daemon carries signaling only — so this
//! provider is a pure translation layer: inputs (UI intents, JS RTC
//! events, presence-WS voice messages, signaling loss) drive the
//! presence-core [`VoiceCallMachine`], and the machine's commands come
//! back out as effects the wasm layer executes (send on the presence
//! WS, issue an RTC verb to the JS glue, update the UI). Every teardown
//! decision — own-pc close on stop, the bounded data-channel grace for
//! the final usage flush, signaling-loss-is-call-terminal — is the
//! machine's, pinned by its native tests; the JS glue executes verbs
//! and decides nothing.
//!
//! Target-agnostic on purpose: no web_sys, no js-sys — natively
//! testable.

use presence_core::voice::{VoiceCallCommand, VoiceCallEvent, VoiceCallMachine, VoiceCallPhase};

/// Effects the wasm layer executes after each provider step.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatGptEffect {
    /// Send a raw JSON string over the presence WS.
    ServerSend(String),
    /// Issue an RTC verb to the JS glue (`{"kind": …, …}`).
    RtcCommand(serde_json::Value),
    /// The call went active — surface voice-ready UI.
    VoiceReady,
    /// The call reached a terminal state.
    VoiceClosed { reason: String },
    /// Voice-lane status payload for the dashboard card.
    StatusUpdate(serde_json::Value),
    /// Named diagnostic for the voice status line / logs.
    Diagnostic { kind: String, detail: String },
}

pub struct ChatGptProvider {
    pub connected: bool,
    machine: VoiceCallMachine,
}

impl Default for ChatGptProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatGptProvider {
    pub fn new() -> Self {
        Self {
            connected: false,
            machine: VoiceCallMachine::new(),
        }
    }

    /// Whether the JS glue should keep the coarse tick interval alive.
    pub fn wants_ticks(&self) -> bool {
        !matches!(
            self.machine.phase(),
            VoiceCallPhase::Idle | VoiceCallPhase::Closed { .. }
        )
    }

    pub fn connect(&mut self, now_ms: u64) -> Vec<ChatGptEffect> {
        self.step(VoiceCallEvent::ConnectRequested, now_ms)
    }

    pub fn disconnect(&mut self, now_ms: u64) -> Vec<ChatGptEffect> {
        self.step(VoiceCallEvent::LocalStopRequested, now_ms)
    }

    /// The presence WS dropped: call-terminal with the bounded grace.
    pub fn signaling_lost(&mut self, now_ms: u64) -> Vec<ChatGptEffect> {
        self.step(VoiceCallEvent::SignalingLost, now_ms)
    }

    /// An event from the JS RTC glue.
    pub fn rtc_event(&mut self, kind: &str, payload: &str, now_ms: u64) -> Vec<ChatGptEffect> {
        match kind {
            "offer_ready" => self.step(
                VoiceCallEvent::OfferReady {
                    sdp: payload.to_string(),
                },
                now_ms,
            ),
            "pc_connected" => self.step(VoiceCallEvent::PcConnected, now_ms),
            "pc_terminated" => self.step(
                VoiceCallEvent::PcTerminated {
                    detail: payload.to_string(),
                },
                now_ms,
            ),
            "mic_error" => {
                // No capture, no call: surface the named failure and tear
                // down whatever exists.
                let mut effects = self.step(
                    VoiceCallEvent::PcTerminated {
                        detail: format!("mic: {payload}"),
                    },
                    now_ms,
                );
                effects.push(ChatGptEffect::Diagnostic {
                    kind: "voice_mic_error".to_string(),
                    detail: payload.to_string(),
                });
                effects
            }
            "dc_event" => self.handle_dc_event(payload, now_ms),
            "tick" => self.step(VoiceCallEvent::Tick, now_ms),
            other => vec![ChatGptEffect::Diagnostic {
                kind: "voice_rtc_unknown_event".to_string(),
                detail: other.to_string(),
            }],
        }
    }

    /// A voice message from the daemon over the presence WS.
    pub fn server_message(
        &mut self,
        t: &str,
        msg: &serde_json::Value,
        now_ms: u64,
    ) -> Vec<ChatGptEffect> {
        match t {
            "voice_answer" => {
                let sdp = msg.get("sdp").and_then(|s| s.as_str()).unwrap_or("");
                self.step(
                    VoiceCallEvent::AnswerReceived {
                        sdp: sdp.to_string(),
                    },
                    now_ms,
                )
            }
            "voice_error" => {
                let message = msg
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("voice error")
                    .to_string();
                let mut effects = self.step(
                    VoiceCallEvent::ServerError {
                        message: message.clone(),
                    },
                    now_ms,
                );
                effects.push(ChatGptEffect::Diagnostic {
                    kind: "voice_error".to_string(),
                    detail: message,
                });
                effects
            }
            "voice_closed" => {
                let reason = msg
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .map(str::to_string);
                self.step(VoiceCallEvent::ServerClosed { reason }, now_ms)
            }
            "voice_status" => msg
                .get("status")
                .cloned()
                .map(|status| vec![ChatGptEffect::StatusUpdate(status)])
                .unwrap_or_default(),
            other => vec![ChatGptEffect::Diagnostic {
                kind: "voice_unknown_server_message".to_string(),
                detail: other.to_string(),
            }],
        }
    }

    /// A realtime-events data-channel message. Consumption telemetry is
    /// forwarded to the daemon (provider-reported decoration, A2); a
    /// usage event during the drain window is the final flush that
    /// short-circuits teardown.
    fn handle_dc_event(&mut self, raw: &str, now_ms: u64) -> Vec<ChatGptEffect> {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(raw) else {
            return Vec::new();
        };
        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if event_type != "session.usage.updated" {
            return Vec::new();
        }
        let mut effects = vec![ChatGptEffect::ServerSend(
            serde_json::json!({ "t": "voice_usage", "payload": event }).to_string(),
        )];
        // Only the Draining phase consumes this as the final flush; the
        // machine ignores it elsewhere.
        effects.extend(self.step(VoiceCallEvent::FinalUsageReceived, now_ms));
        effects
    }

    fn step(&mut self, event: VoiceCallEvent, now_ms: u64) -> Vec<ChatGptEffect> {
        let commands = self.machine.handle(event, now_ms);
        let mut effects = Vec::with_capacity(commands.len());
        for command in commands {
            match command {
                VoiceCallCommand::CreateCall => {
                    effects.push(ChatGptEffect::RtcCommand(
                        serde_json::json!({ "kind": "create_call" }),
                    ));
                }
                VoiceCallCommand::SendOffer { sdp } => {
                    effects.push(ChatGptEffect::ServerSend(
                        serde_json::json!({ "t": "voice_start", "sdp": sdp }).to_string(),
                    ));
                }
                VoiceCallCommand::ApplyAnswer { sdp } => {
                    effects.push(ChatGptEffect::RtcCommand(
                        serde_json::json!({ "kind": "apply_answer", "sdp": sdp }),
                    ));
                }
                VoiceCallCommand::StopMic => {
                    effects.push(ChatGptEffect::RtcCommand(
                        serde_json::json!({ "kind": "stop_mic" }),
                    ));
                }
                VoiceCallCommand::ClosePc => {
                    effects.push(ChatGptEffect::RtcCommand(
                        serde_json::json!({ "kind": "close_pc" }),
                    ));
                }
                VoiceCallCommand::NotifyServerStop => {
                    effects.push(ChatGptEffect::ServerSend(
                        serde_json::json!({ "t": "voice_stop" }).to_string(),
                    ));
                }
                VoiceCallCommand::ReportActive => {
                    self.connected = true;
                    effects.push(ChatGptEffect::VoiceReady);
                }
                VoiceCallCommand::ReportClosed { reason } => {
                    self.connected = false;
                    effects.push(ChatGptEffect::VoiceClosed { reason });
                }
            }
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use presence_core::voice::VOICE_DC_GRACE_MS;

    fn drain_to_active(p: &mut ChatGptProvider) {
        p.connect(0);
        p.rtc_event("offer_ready", "offer-sdp", 1);
        p.server_message("voice_answer", &serde_json::json!({"sdp": "answer-sdp"}), 2);
        p.rtc_event("pc_connected", "", 3);
        assert!(p.connected);
    }

    #[test]
    fn connect_flow_emits_expected_effects_in_order() {
        let mut p = ChatGptProvider::new();
        assert_eq!(
            p.connect(0),
            vec![ChatGptEffect::RtcCommand(
                serde_json::json!({"kind": "create_call"})
            )]
        );
        let effects = p.rtc_event("offer_ready", "offer-sdp", 1);
        assert_eq!(
            effects,
            vec![ChatGptEffect::ServerSend(
                serde_json::json!({"t": "voice_start", "sdp": "offer-sdp"}).to_string()
            )]
        );
        let effects =
            p.server_message("voice_answer", &serde_json::json!({"sdp": "answer-sdp"}), 2);
        assert_eq!(
            effects,
            vec![ChatGptEffect::RtcCommand(
                serde_json::json!({"kind": "apply_answer", "sdp": "answer-sdp"})
            )]
        );
        let effects = p.rtc_event("pc_connected", "", 3);
        assert_eq!(effects, vec![ChatGptEffect::VoiceReady]);
        assert!(p.connected && p.wants_ticks());
    }

    // R5/A3 through the provider: disconnect stops the mic + notifies
    // the daemon immediately, holds the DC grace for the usage flush,
    // forwards the flush as voice_usage, and closes its own pc.
    #[test]
    fn disconnect_drains_then_final_usage_closes_pc() {
        let mut p = ChatGptProvider::new();
        drain_to_active(&mut p);
        let effects = p.disconnect(1_000);
        assert!(effects.contains(&ChatGptEffect::ServerSend(
            serde_json::json!({"t": "voice_stop"}).to_string()
        )));
        assert!(effects.contains(&ChatGptEffect::RtcCommand(
            serde_json::json!({"kind": "stop_mic"})
        )));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, ChatGptEffect::RtcCommand(c) if c["kind"] == "close_pc")));
        // Final usage flush arrives on the DC during the drain window.
        let usage = serde_json::json!({"type": "session.usage.updated", "tokens": {"total": 5}});
        let effects = p.rtc_event("dc_event", &usage.to_string(), 5_000);
        assert!(effects.iter().any(|e| matches!(
            e,
            ChatGptEffect::ServerSend(s) if s.contains("voice_usage")
        )));
        assert!(effects
            .iter()
            .any(|e| matches!(e, ChatGptEffect::RtcCommand(c) if c["kind"] == "close_pc")));
        assert!(effects
            .iter()
            .any(|e| matches!(e, ChatGptEffect::VoiceClosed { .. })));
        assert!(!p.connected && !p.wants_ticks());
    }

    // R5/A3: with no flush, the provider's own deadline closes the pc —
    // never waiting on remote teardown.
    #[test]
    fn disconnect_deadline_closes_pc_without_flush() {
        let mut p = ChatGptProvider::new();
        drain_to_active(&mut p);
        p.disconnect(1_000);
        assert!(p
            .rtc_event("tick", "", 1_000 + VOICE_DC_GRACE_MS - 1)
            .is_empty());
        let effects = p.rtc_event("tick", "", 1_000 + VOICE_DC_GRACE_MS);
        assert!(effects
            .iter()
            .any(|e| matches!(e, ChatGptEffect::RtcCommand(c) if c["kind"] == "close_pc")));
    }

    // R4: signaling loss mid-call stops the mic NOW, sends nothing to
    // the (dead) daemon, and the pc closes within the bounded grace.
    #[test]
    fn signaling_loss_is_call_terminal() {
        let mut p = ChatGptProvider::new();
        drain_to_active(&mut p);
        let effects = p.signaling_lost(2_000);
        assert_eq!(
            effects,
            vec![ChatGptEffect::RtcCommand(
                serde_json::json!({"kind": "stop_mic"})
            )]
        );
        let effects = p.rtc_event(
            "tick",
            "",
            2_000 + presence_core::voice::VOICE_SIGNALING_LOSS_GRACE_MS,
        );
        assert!(effects
            .iter()
            .any(|e| matches!(e, ChatGptEffect::RtcCommand(c) if c["kind"] == "close_pc")));
    }

    #[test]
    fn usage_telemetry_forwards_during_active_call_without_closing() {
        let mut p = ChatGptProvider::new();
        drain_to_active(&mut p);
        let usage = serde_json::json!({"type": "session.usage.updated", "tokens": {"total": 3}});
        let effects = p.rtc_event("dc_event", &usage.to_string(), 4_000);
        assert!(effects.iter().any(|e| matches!(
            e,
            ChatGptEffect::ServerSend(s) if s.contains("voice_usage")
        )));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, ChatGptEffect::RtcCommand(c) if c["kind"] == "close_pc")));
        assert!(p.connected, "periodic usage never ends the call");
    }

    #[test]
    fn server_error_before_answer_is_terminal_and_named() {
        let mut p = ChatGptProvider::new();
        p.connect(0);
        p.rtc_event("offer_ready", "offer-sdp", 1);
        let effects = p.server_message(
            "voice_error",
            &serde_json::json!({"message": "voice pin invalid"}),
            2,
        );
        assert!(effects
            .iter()
            .any(|e| matches!(e, ChatGptEffect::VoiceClosed { .. })));
        assert!(effects.iter().any(|e| matches!(
            e,
            ChatGptEffect::Diagnostic { kind, .. } if kind == "voice_error"
        )));
    }

    #[test]
    fn status_updates_pass_through() {
        let mut p = ChatGptProvider::new();
        let effects = p.server_message(
            "voice_status",
            &serde_json::json!({"status": {"active": true, "thread_id": "t-1"}}),
            0,
        );
        assert_eq!(
            effects,
            vec![ChatGptEffect::StatusUpdate(
                serde_json::json!({"active": true, "thread_id": "t-1"})
            )]
        );
    }
}

//! Voice-presence shared vocabulary: the ChatGPT-subscription voice lane.
//!
//! This module carries the pieces both sides of the presence WS speak —
//! the `[presence.voice]` config surface, the realtime voice-family
//! tables, the signaling payload structs — plus the browser call state
//! machine that owns every teardown decision. The machine is pure and
//! deterministic (time enters only as caller-supplied milliseconds) so
//! the teardown rules are pinned by native unit tests: the browser JS
//! executes commands, it never decides policy.
//!
//! Design basis (Track VP Stage A intake + Stage B gate ruling):
//! - A3: the provider closes its own RTCPeerConnection on stop — never
//!   waits for remote teardown (server-side media teardown is not
//!   prompt; ~25 s of silence RTP observed post-stop). A bounded
//!   data-channel grace before the close captures the final
//!   `session.usage.updated` flush; if it never comes, usage stays
//!   advisory-only.
//! - A4: realtime v3 uses the **v1** voice family; voice pins are
//!   validated against the version-correct family before start.
//! - R4: signaling loss is call-terminal with a bounded grace — a dead
//!   daemon must never leave a live mic streaming, so the mic capture
//!   stops the moment the call leaves the live phases, and the peer
//!   connection closes by deadline even if nothing else arrives.

use serde::{Deserialize, Serialize};

/// Provider name for the ChatGPT-subscription voice lane
/// (`[presence] live_provider = "chatgpt"`).
pub const CHATGPT_VOICE_PROVIDER: &str = "chatgpt";

// ── [presence.voice] config ──

/// Configuration for the ChatGPT-subscription voice lane
/// (`[presence.voice]` in `intendant.toml`).
///
/// Unset pins resolve to the account's own defaults (the backing lane
/// the subscription already runs; the provider's default voice for the
/// realtime version) — never to hardcoded model slugs. The resolved
/// values are surfaced on the voice status card after every start.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PresenceVoiceConfig {
    /// Backing (thread) model pin. Unset = the account's default
    /// backing lane, surfaced as resolved after start.
    #[serde(default)]
    pub backing_model: Option<String>,
    /// Backing-model reasoning effort pin. Unset = account default.
    #[serde(default)]
    pub backing_effort: Option<String>,
    /// Realtime voice pin, validated against the version-correct
    /// family (see [`validate_voice_pin`]). Unset = provider default.
    #[serde(default)]
    pub voice: Option<String>,
    /// Realtime protocol version: "v1" | "v2" | "v3". Unset = v3.
    #[serde(default)]
    pub realtime_version: Option<String>,
    /// Explicit App Server binary override (e.g. a bundled desktop-app
    /// codex). Unset = the configured `[codex] command`.
    #[serde(default)]
    pub app_server_command: Option<String>,
}

/// Default realtime protocol version for the ChatGPT lane.
pub const DEFAULT_REALTIME_VERSION: &str = "v3";

/// The v1 realtime voice family (also the family realtime **v3**
/// accepts — Stage B ground truth: `cedar` (v2) is rejected on v3 with
/// exactly these nine names enumerated).
pub const REALTIME_V1_VOICES: &[&str] = &[
    "juniper", "maple", "spruce", "ember", "vale", "breeze", "arbor", "sol", "cove",
];

/// The v2 realtime voice family (live-listed by the provider).
pub const REALTIME_V2_VOICES: &[&str] = &[
    "alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse", "marin", "cedar",
];

/// The voice family a realtime protocol version accepts. `None` for an
/// unknown version string (callers refuse, never guess).
pub fn voice_family_for_version(version: &str) -> Option<&'static [&'static str]> {
    match version {
        // Stage B B3a: v3 rejects v2 voices and enumerates the v1 family.
        "v1" | "v3" => Some(REALTIME_V1_VOICES),
        "v2" => Some(REALTIME_V2_VOICES),
        _ => None,
    }
}

/// Validate a voice pin against the family the realtime version
/// accepts. `Ok(())` when the pin is unset (provider default applies).
pub fn validate_voice_pin(version: &str, voice: Option<&str>) -> Result<(), String> {
    let family = voice_family_for_version(version)
        .ok_or_else(|| format!("unknown realtime version \"{version}\" (expected v1|v2|v3)"))?;
    match voice {
        None => Ok(()),
        Some(v) if family.contains(&v) => Ok(()),
        Some(v) => Err(format!(
            "voice \"{v}\" is not in the {version} family (supported: {})",
            family.join(", ")
        )),
    }
}

// ── Signaling payloads (presence WS, stringly-typed "t" envelopes) ──

/// Browser → server (`"t":"voice_start"`): the WebRTC SDP offer that
/// opens a voice call. Media never touches the daemon — this is
/// signaling relay into the Voice broker only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceStart {
    pub sdp: String,
}

/// Server → browser (`"t":"voice_answer"`): the provider's SDP answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceAnswer {
    pub sdp: String,
}

/// Server → browser (`"t":"voice_error"`): a named voice-lane failure
/// (capability gate, entitlement, start rejection, broker fault). The
/// lane degrades to text presence with this notice — never silently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceError {
    pub message: String,
}

/// Server → browser (`"t":"voice_closed"`): the realtime session ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceClosed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Browser → server (`"t":"voice_usage"`): a provider-reported
/// consumption event forwarded verbatim from the realtime events data
/// channel (`session.usage.updated`). Decoration, never authoritative
/// window state — the account rate-limit plane stays the authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceUsage {
    pub payload: serde_json::Value,
}

/// Resolved voice-lane status for the dashboard voice card
/// (server → browser `"t":"voice_status"`). `resolved_*` fields carry
/// what the provider actually granted at the last start — the D2
/// surfacing duty — not what config asked for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoiceStatus {
    pub available: bool,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Number of predecessor presence threads in the lineage (successor
    /// mints after resume failure).
    #[serde(default)]
    pub thread_lineage_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Last provider-reported usage payload, labeled provider-reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<serde_json::Value>,
}

// ── The browser call state machine ──

/// Data-channel grace after a stop before the peer connection closes:
/// long enough to capture the final `session.usage.updated` flush
/// (observed at +20.6 s), bounded so teardown never hangs on it.
pub const VOICE_DC_GRACE_MS: u64 = 25_000;

/// Bounded grace after signaling loss before the peer connection
/// closes. The mic stops immediately at signaling loss; this only
/// bounds how long the (silent) peer connection may linger.
pub const VOICE_SIGNALING_LOSS_GRACE_MS: u64 = 10_000;

/// Call phases. `Draining` is the A3 teardown window: mic already
/// stopped, peer connection held open only for the final usage flush.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceCallPhase {
    Idle,
    /// Waiting for the JS glue to produce the SDP offer.
    Preparing,
    /// Offer sent to the daemon; waiting for the answer.
    Offering,
    /// Answer applied; waiting for the peer connection to connect.
    Connecting,
    Active,
    /// Teardown window: mic stopped; peer connection closes at
    /// `deadline_ms` or on the final usage flush, whichever first.
    Draining { deadline_ms: u64, reason: String },
    Closed { reason: String },
}

/// Inputs to the machine. Fed by the provider from three sources: the
/// JS glue (offer/pc/mic events), the presence WS (answer/closed/error,
/// signaling loss), and a coarse timer tick.
#[derive(Debug, Clone)]
pub enum VoiceCallEvent {
    ConnectRequested,
    /// JS produced the local SDP offer.
    OfferReady { sdp: String },
    /// The daemon relayed the provider's SDP answer.
    AnswerReceived { sdp: String },
    PcConnected,
    /// The peer connection died underneath us (ICE failure, close).
    PcTerminated { detail: String },
    /// Local stop: user clicked stop, or the provider is shutting down.
    LocalStopRequested,
    /// Daemon says the realtime session ended.
    ServerClosed { reason: Option<String> },
    /// Daemon reports a voice-lane error.
    ServerError { message: String },
    /// The presence WS dropped: call-terminal (R4).
    SignalingLost,
    /// The final post-stop usage flush arrived on the data channel.
    FinalUsageReceived,
    /// Coarse timer tick; drives drain deadlines.
    Tick,
}

/// Commands the JS glue executes verbatim. Policy stays in the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceCallCommand {
    /// Create the peer connection + mic + events data channel, then
    /// feed back `OfferReady`.
    CreateCall,
    /// Send the offer to the daemon over the presence WS.
    SendOffer { sdp: String },
    /// Apply the remote SDP answer to the peer connection.
    ApplyAnswer { sdp: String },
    /// Stop the microphone capture tracks *now*.
    StopMic,
    /// Close the peer connection.
    ClosePc,
    /// Tell the daemon to stop the realtime session (`voice_stop`).
    NotifyServerStop,
    /// Surface the terminal state to the UI.
    ReportClosed { reason: String },
    /// Surface the active state to the UI.
    ReportActive,
}

/// The deterministic call state machine. Owns every teardown decision;
/// see the module docs for the rules it pins.
#[derive(Debug)]
pub struct VoiceCallMachine {
    phase: VoiceCallPhase,
    /// True from mic acquisition (CreateCall) until StopMic was issued.
    mic_may_be_live: bool,
}

impl Default for VoiceCallMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceCallMachine {
    pub fn new() -> Self {
        Self {
            phase: VoiceCallPhase::Idle,
            mic_may_be_live: false,
        }
    }

    pub fn phase(&self) -> &VoiceCallPhase {
        &self.phase
    }

    pub fn is_live(&self) -> bool {
        matches!(
            self.phase,
            VoiceCallPhase::Preparing
                | VoiceCallPhase::Offering
                | VoiceCallPhase::Connecting
                | VoiceCallPhase::Active
        )
    }

    /// Emit `StopMic` exactly once per call, the moment the call leaves
    /// the live phases. No path reaches `Draining`/`Closed` with the
    /// mic still marked live — pinned by test.
    fn stop_mic(&mut self, cmds: &mut Vec<VoiceCallCommand>) {
        if self.mic_may_be_live {
            self.mic_may_be_live = false;
            cmds.push(VoiceCallCommand::StopMic);
        }
    }

    fn close_now(&mut self, reason: String, cmds: &mut Vec<VoiceCallCommand>) {
        self.stop_mic(cmds);
        cmds.push(VoiceCallCommand::ClosePc);
        cmds.push(VoiceCallCommand::ReportClosed {
            reason: reason.clone(),
        });
        self.phase = VoiceCallPhase::Closed { reason };
    }

    fn drain(
        &mut self,
        now_ms: u64,
        grace_ms: u64,
        reason: String,
        cmds: &mut Vec<VoiceCallCommand>,
    ) {
        self.stop_mic(cmds);
        self.phase = VoiceCallPhase::Draining {
            deadline_ms: now_ms.saturating_add(grace_ms),
            reason,
        };
    }

    /// Advance the machine. `now_ms` is any monotonic-enough clock in
    /// milliseconds; it is compared only against deadlines the machine
    /// itself minted from earlier `now_ms` values.
    pub fn handle(&mut self, event: VoiceCallEvent, now_ms: u64) -> Vec<VoiceCallCommand> {
        use VoiceCallEvent as E;
        use VoiceCallPhase as P;
        let mut cmds = Vec::new();
        match (&self.phase, event) {
            (P::Idle, E::ConnectRequested) => {
                self.phase = P::Preparing;
                self.mic_may_be_live = true;
                cmds.push(VoiceCallCommand::CreateCall);
            }
            (P::Preparing, E::OfferReady { sdp }) => {
                self.phase = P::Offering;
                cmds.push(VoiceCallCommand::SendOffer { sdp });
            }
            (P::Offering, E::AnswerReceived { sdp }) => {
                self.phase = P::Connecting;
                cmds.push(VoiceCallCommand::ApplyAnswer { sdp });
            }
            (P::Connecting, E::PcConnected) => {
                self.phase = P::Active;
                cmds.push(VoiceCallCommand::ReportActive);
            }

            // ── Local stop: A3 — close our own peer connection on our
            // own clock; the DC grace only captures the usage flush.
            (P::Preparing | P::Offering | P::Connecting | P::Active, E::LocalStopRequested) => {
                cmds.push(VoiceCallCommand::NotifyServerStop);
                self.drain(now_ms, VOICE_DC_GRACE_MS, "stopped".into(), &mut cmds);
            }

            // ── Server ended the session: drain for the usage flush.
            (P::Preparing | P::Offering | P::Connecting | P::Active, E::ServerClosed { reason }) => {
                let reason = reason.unwrap_or_else(|| "closed".into());
                self.drain(now_ms, VOICE_DC_GRACE_MS, reason, &mut cmds);
            }

            // ── Errors before media exists close immediately; errors on
            // a live call drain like a close (usage may still flush).
            (P::Preparing | P::Offering, E::ServerError { message }) => {
                self.close_now(format!("error: {message}"), &mut cmds);
            }
            (P::Connecting | P::Active, E::ServerError { message }) => {
                self.drain(
                    now_ms,
                    VOICE_DC_GRACE_MS,
                    format!("error: {message}"),
                    &mut cmds,
                );
            }

            // ── R4: signaling loss is call-terminal. Mic stops now; the
            // peer connection lingers at most the bounded grace (no
            // server stop can be sent — the daemon is unreachable).
            (P::Preparing | P::Offering, E::SignalingLost) => {
                self.close_now("signaling-lost".into(), &mut cmds);
            }
            (P::Connecting | P::Active, E::SignalingLost) => {
                self.drain(
                    now_ms,
                    VOICE_SIGNALING_LOSS_GRACE_MS,
                    "signaling-lost".into(),
                    &mut cmds,
                );
            }

            // ── The pc died underneath any phase: terminal.
            (
                P::Preparing | P::Offering | P::Connecting | P::Active,
                E::PcTerminated { detail },
            ) => {
                self.close_now(format!("pc-terminated: {detail}"), &mut cmds);
            }

            // ── Draining: final usage or deadline closes the pc; a late
            // pc death just completes the close.
            (P::Draining { reason, .. }, E::FinalUsageReceived) => {
                let reason = reason.clone();
                cmds.push(VoiceCallCommand::ClosePc);
                cmds.push(VoiceCallCommand::ReportClosed {
                    reason: reason.clone(),
                });
                self.phase = P::Closed { reason };
            }
            (P::Draining { deadline_ms, reason }, E::Tick) => {
                if now_ms >= *deadline_ms {
                    let reason = reason.clone();
                    cmds.push(VoiceCallCommand::ClosePc);
                    cmds.push(VoiceCallCommand::ReportClosed {
                        reason: reason.clone(),
                    });
                    self.phase = P::Closed { reason };
                }
            }
            (P::Draining { reason, .. }, E::PcTerminated { .. }) => {
                let reason = reason.clone();
                cmds.push(VoiceCallCommand::ReportClosed {
                    reason: reason.clone(),
                });
                self.phase = P::Closed { reason };
            }
            // A ServerClosed while draining confirms what we already
            // know; keep the original reason and deadline.
            (P::Draining { .. }, E::ServerClosed { .. } | E::ServerError { .. }) => {}

            // Everything else is a no-op for the current phase.
            _ => {}
        }
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advance(m: &mut VoiceCallMachine, ev: VoiceCallEvent, now: u64) -> Vec<VoiceCallCommand> {
        m.handle(ev, now)
    }

    fn connect_to_active(m: &mut VoiceCallMachine) {
        advance(m, VoiceCallEvent::ConnectRequested, 0);
        advance(m, VoiceCallEvent::OfferReady { sdp: "o".into() }, 1);
        advance(m, VoiceCallEvent::AnswerReceived { sdp: "a".into() }, 2);
        advance(m, VoiceCallEvent::PcConnected, 3);
        assert_eq!(*m.phase(), VoiceCallPhase::Active);
    }

    #[test]
    fn happy_path_commands_in_order() {
        let mut m = VoiceCallMachine::new();
        assert_eq!(
            advance(&mut m, VoiceCallEvent::ConnectRequested, 0),
            vec![VoiceCallCommand::CreateCall]
        );
        assert_eq!(
            advance(&mut m, VoiceCallEvent::OfferReady { sdp: "o".into() }, 1),
            vec![VoiceCallCommand::SendOffer { sdp: "o".into() }]
        );
        assert_eq!(
            advance(&mut m, VoiceCallEvent::AnswerReceived { sdp: "a".into() }, 2),
            vec![VoiceCallCommand::ApplyAnswer { sdp: "a".into() }]
        );
        assert_eq!(
            advance(&mut m, VoiceCallEvent::PcConnected, 3),
            vec![VoiceCallCommand::ReportActive]
        );
    }

    // A3 pin: on local stop the provider closes its own pc by its own
    // deadline — never waits for remote teardown — and the mic stops
    // immediately, before any grace.
    #[test]
    fn local_stop_stops_mic_now_and_closes_pc_by_own_deadline() {
        let mut m = VoiceCallMachine::new();
        connect_to_active(&mut m);
        let cmds = advance(&mut m, VoiceCallEvent::LocalStopRequested, 1_000);
        assert_eq!(
            cmds,
            vec![
                VoiceCallCommand::NotifyServerStop,
                VoiceCallCommand::StopMic,
            ]
        );
        match m.phase() {
            VoiceCallPhase::Draining { deadline_ms, .. } => {
                assert_eq!(*deadline_ms, 1_000 + VOICE_DC_GRACE_MS)
            }
            other => panic!("expected Draining, got {other:?}"),
        }
        // No server event ever arrives; the deadline alone closes it.
        assert!(advance(&mut m, VoiceCallEvent::Tick, 1_000 + VOICE_DC_GRACE_MS - 1).is_empty());
        let cmds = advance(&mut m, VoiceCallEvent::Tick, 1_000 + VOICE_DC_GRACE_MS);
        assert!(cmds.contains(&VoiceCallCommand::ClosePc));
        assert!(matches!(m.phase(), VoiceCallPhase::Closed { .. }));
    }

    // A3 pin: the final usage flush short-circuits the drain.
    #[test]
    fn final_usage_flush_short_circuits_drain() {
        let mut m = VoiceCallMachine::new();
        connect_to_active(&mut m);
        advance(&mut m, VoiceCallEvent::LocalStopRequested, 1_000);
        let cmds = advance(&mut m, VoiceCallEvent::FinalUsageReceived, 5_000);
        assert!(cmds.contains(&VoiceCallCommand::ClosePc));
        assert!(matches!(m.phase(), VoiceCallPhase::Closed { .. }));
    }

    // R4 pin: signaling loss is call-terminal — mic stops immediately,
    // pc closes within the bounded grace, and no stop is sent to the
    // unreachable daemon.
    #[test]
    fn signaling_loss_is_call_terminal_with_bounded_grace() {
        let mut m = VoiceCallMachine::new();
        connect_to_active(&mut m);
        let cmds = advance(&mut m, VoiceCallEvent::SignalingLost, 2_000);
        assert_eq!(cmds, vec![VoiceCallCommand::StopMic]);
        assert!(!cmds.contains(&VoiceCallCommand::NotifyServerStop));
        match m.phase() {
            VoiceCallPhase::Draining {
                deadline_ms,
                reason,
            } => {
                assert_eq!(*deadline_ms, 2_000 + VOICE_SIGNALING_LOSS_GRACE_MS);
                assert_eq!(reason, "signaling-lost");
            }
            other => panic!("expected Draining, got {other:?}"),
        }
        let cmds = advance(
            &mut m,
            VoiceCallEvent::Tick,
            2_000 + VOICE_SIGNALING_LOSS_GRACE_MS,
        );
        assert!(cmds.contains(&VoiceCallCommand::ClosePc));
    }

    // R4 pin: signaling loss before media exists closes immediately.
    #[test]
    fn signaling_loss_before_answer_closes_immediately() {
        let mut m = VoiceCallMachine::new();
        advance(&mut m, VoiceCallEvent::ConnectRequested, 0);
        advance(&mut m, VoiceCallEvent::OfferReady { sdp: "o".into() }, 1);
        let cmds = advance(&mut m, VoiceCallEvent::SignalingLost, 2);
        assert_eq!(
            cmds,
            vec![
                VoiceCallCommand::StopMic,
                VoiceCallCommand::ClosePc,
                VoiceCallCommand::ReportClosed {
                    reason: "signaling-lost".into()
                },
            ]
        );
    }

    // Mic-law pin: no path from a live phase into Draining/Closed keeps
    // the mic live. Exhaustive over the terminal-driving events from
    // every live phase.
    #[test]
    fn every_exit_from_live_phases_stops_the_mic() {
        let build = |target: &str| {
            let mut m = VoiceCallMachine::new();
            advance(&mut m, VoiceCallEvent::ConnectRequested, 0);
            if target == "preparing" {
                return m;
            }
            advance(&mut m, VoiceCallEvent::OfferReady { sdp: "o".into() }, 1);
            if target == "offering" {
                return m;
            }
            advance(&mut m, VoiceCallEvent::AnswerReceived { sdp: "a".into() }, 2);
            if target == "connecting" {
                return m;
            }
            advance(&mut m, VoiceCallEvent::PcConnected, 3);
            m
        };
        let exits: Vec<VoiceCallEvent> = vec![
            VoiceCallEvent::LocalStopRequested,
            VoiceCallEvent::ServerClosed { reason: None },
            VoiceCallEvent::ServerError {
                message: "x".into(),
            },
            VoiceCallEvent::SignalingLost,
            VoiceCallEvent::PcTerminated { detail: "x".into() },
        ];
        for phase in ["preparing", "offering", "connecting", "active"] {
            for ev in &exits {
                let mut m = build(phase);
                let cmds = advance(&mut m, ev.clone(), 100);
                assert!(
                    cmds.contains(&VoiceCallCommand::StopMic),
                    "phase {phase} exit {ev:?} must stop the mic (got {cmds:?})"
                );
                assert!(
                    !m.is_live(),
                    "phase {phase} exit {ev:?} must leave the live phases"
                );
            }
        }
    }

    #[test]
    fn server_close_drains_for_usage_flush() {
        let mut m = VoiceCallMachine::new();
        connect_to_active(&mut m);
        advance(
            &mut m,
            VoiceCallEvent::ServerClosed {
                reason: Some("requested".into()),
            },
            1_000,
        );
        match m.phase() {
            VoiceCallPhase::Draining { reason, .. } => assert_eq!(reason, "requested"),
            other => panic!("expected Draining, got {other:?}"),
        }
    }

    #[test]
    fn start_error_before_media_closes_immediately() {
        let mut m = VoiceCallMachine::new();
        advance(&mut m, VoiceCallEvent::ConnectRequested, 0);
        advance(&mut m, VoiceCallEvent::OfferReady { sdp: "o".into() }, 1);
        // A4: start acceptance is provisional — the rejection arrives as
        // an async error while we wait for the answer.
        let cmds = advance(
            &mut m,
            VoiceCallEvent::ServerError {
                message: "voice not supported for v3".into(),
            },
            2,
        );
        assert!(cmds.contains(&VoiceCallCommand::ClosePc));
        assert!(matches!(m.phase(), VoiceCallPhase::Closed { .. }));
    }

    #[test]
    fn draining_absorbs_late_server_events_without_extending() {
        let mut m = VoiceCallMachine::new();
        connect_to_active(&mut m);
        advance(&mut m, VoiceCallEvent::LocalStopRequested, 1_000);
        let deadline = match m.phase() {
            VoiceCallPhase::Draining { deadline_ms, .. } => *deadline_ms,
            other => panic!("expected Draining, got {other:?}"),
        };
        assert!(advance(
            &mut m,
            VoiceCallEvent::ServerClosed {
                reason: Some("requested".into())
            },
            2_000,
        )
        .is_empty());
        match m.phase() {
            VoiceCallPhase::Draining {
                deadline_ms,
                reason,
            } => {
                assert_eq!(*deadline_ms, deadline);
                assert_eq!(reason, "stopped");
            }
            other => panic!("expected Draining, got {other:?}"),
        }
    }

    #[test]
    fn idle_ignores_stray_events() {
        let mut m = VoiceCallMachine::new();
        assert!(advance(&mut m, VoiceCallEvent::Tick, 0).is_empty());
        assert!(advance(&mut m, VoiceCallEvent::SignalingLost, 0).is_empty());
        assert!(advance(&mut m, VoiceCallEvent::FinalUsageReceived, 0).is_empty());
        assert_eq!(*m.phase(), VoiceCallPhase::Idle);
    }

    // A4 pin: v3 uses the v1 voice family; v2 pins are refused with the
    // family enumerated; unknown versions refuse rather than guess.
    #[test]
    fn voice_family_validation_matches_stage_b_ground_truth() {
        assert!(validate_voice_pin("v3", Some("sol")).is_ok());
        assert!(validate_voice_pin("v3", Some("cove")).is_ok());
        assert!(validate_voice_pin("v3", None).is_ok());
        let err = validate_voice_pin("v3", Some("cedar")).unwrap_err();
        assert!(err.contains("juniper") && err.contains("cove"), "{err}");
        assert!(validate_voice_pin("v2", Some("cedar")).is_ok());
        assert!(validate_voice_pin("v1", Some("cove")).is_ok());
        assert!(validate_voice_pin("v4", Some("sol")).is_err());
        assert_eq!(REALTIME_V1_VOICES.len(), 9);
        assert_eq!(REALTIME_V2_VOICES.len(), 10);
    }
}

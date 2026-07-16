//! From SDP offer to connected peer: answer-SDP sanitization, SSRC/ufrag
//! generation, RTP codec parameters, offer parsing (video mid, simulcast
//! recv rids, federated detection), and the `impl WebRtcPeer` construction
//! and command API (`build_with_codec_set`, `new`, the send_* methods,
//! `add_ice_candidate`, `close`).

use super::*;

/// Sanitize an rtc 0.9-emitted answer SDP, fixing two SDP-writer bugs
/// that fire on multi-RID simulcast send (specifically when our peer
/// answers a browser offer that requested `a=simulcast:recv f;h;q`):
///
///   1. **Duplicate `a=rid:<rid> send` lines.** rtc 0.9 emits each RID
///      `send` line twice — six lines for f/h/q instead of three.
///      Fix: dedupe by full line content within each m= section.
///
///   2. **Malformed `a=simulcast:` attribute.** rtc 0.9 concatenates
///      the direction + RID list as if the answer were bidirectional,
///      producing `a=simulcast:send f;h;q send f;h;q` instead of the
///      RFC 8853-correct `a=simulcast:send f;h;q`. WebKit's parser
///      rejects this with `SyntaxError: Malformed simulcast line`.
///      Fix: when an `a=simulcast:` line repeats the same direction
///      twice, keep only the first `<dir> <list>` pair.
///
/// Pure / idempotent: already-clean SDP is unchanged, single-RID
/// answers (H.264, VP8 floor-only) are unchanged, and the function
/// has no side effects. Tested via `sanitize_answer_sdp_*` below.
///
/// Section-aware: `seen_rids` resets at every `m=` boundary so a
/// theoretical multi-section SDP that legitimately reuses RIDs
/// across audio + video isn't silently flattened.
///
/// Line-ending preserving: detects CRLF vs LF on input and preserves
/// the same on output, including a trailing terminator if present.
pub(crate) fn sanitize_answer_sdp(sdp: &str) -> String {
    let line_ending = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut seen_rids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::with_capacity(sdp.lines().count());

    for line in sdp.lines() {
        if line.starts_with("m=") {
            seen_rids.clear();
            out.push(line.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("a=rid:") {
            // dedupe by the post-`a=rid:` content (rid + dir + params)
            if !seen_rids.insert(rest.to_string()) {
                continue;
            }
            out.push(line.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("a=simulcast:") {
            // Valid forms (RFC 8853):
            //   a=simulcast:send f;h;q
            //   a=simulcast:recv f;h;q
            //   a=simulcast:send f;h;q recv x          (bidirectional)
            // Bug form rtc 0.9 emits:
            //   a=simulcast:send f;h;q send f;h;q      (same dir twice)
            // Fix: when the second direction equals the first, drop the
            // second pair; otherwise pass through.
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 4 && parts[0] == parts[2] {
                out.push(format!("a=simulcast:{} {}", parts[0], parts[1]));
            } else {
                out.push(line.to_string());
            }
            continue;
        }
        out.push(line.to_string());
    }

    let mut result = out.join(line_ending);
    if sdp.ends_with("\r\n") || sdp.ends_with('\n') {
        result.push_str(line_ending);
    }
    result
}

pub(crate) fn new_ssrc() -> u32 {
    let raw = uuid::Uuid::new_v4().as_u128() as u32;
    raw.max(1)
}

pub(crate) fn new_ice_fragment() -> String {
    uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect()
}

pub(crate) fn new_ice_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub(crate) fn video_rtcp_feedback() -> Vec<RTCPFeedback> {
    vec![
        RTCPFeedback {
            typ: "goog-remb".to_string(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "ccm".to_string(),
            parameter: "fir".to_string(),
        },
        RTCPFeedback {
            typ: "nack".to_string(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "nack".to_string(),
            parameter: "pli".to_string(),
        },
    ]
}

pub(crate) fn rtc_codec_parameters(codec: CodecKind) -> Result<RTCRtpCodecParameters, CallerError> {
    let rtp_codec = match codec {
        CodecKind::Vp8 => RTCRtpCodec {
            mime_type: RTC_MIME_TYPE_VP8.to_string(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: video_rtcp_feedback(),
        },
        CodecKind::H264 => RTCRtpCodec {
            mime_type: RTC_MIME_TYPE_H264.to_string(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_string(),
            rtcp_feedback: video_rtcp_feedback(),
        },
        CodecKind::Vp9 | CodecKind::Av1 => {
            return Err(CallerError::WebRtc(format!(
                "codec {} not yet wired to rtc media engine",
                codec
            )));
        }
    };
    let payload_type = match codec {
        CodecKind::Vp8 => 96,
        CodecKind::H264 => 125,
        CodecKind::Vp9 | CodecKind::Av1 => unreachable!(),
    };
    Ok(RTCRtpCodecParameters {
        rtp_codec,
        payload_type,
    })
}

pub(crate) fn first_video_mid_from_offer(sdp: &str) -> Option<String> {
    let mut in_video = false;
    for raw in sdp.lines() {
        let line = raw.trim_end_matches('\r');
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video ");
            continue;
        }
        if in_video {
            if let Some(mid) = line.strip_prefix("a=mid:") {
                let mid = mid.trim();
                if !mid.is_empty() {
                    return Some(mid.to_string());
                }
            }
        }
    }
    None
}

/// Pick the single RID for a federated single-encoding peer (offer
/// without `a=simulcast:recv`). **#48 tuning**: returns the floor
/// (`pool_rids.last()`), not the top (`pool_rids[0]`).
///
/// Rationale: the only consumer of this code path is the federated
/// `PeerDisplayConnection` (post-`e815bac` it strips `a=simulcast:recv`
/// from its offer; the local `DisplaySlot` always injects it and goes
/// down the multi-RID branch instead). Federated runs over a TURN-relay
/// path where moderate sustained packet loss (~5-10 %) is the
/// operational baseline. At the full-layer's 2.5 Mbps target, keyframes
/// run ~500 KB ≈ 420 RTP packets; intact-arrival probability at 8 %
/// loss is `0.92^420 ≈ 1.4e-15` — effectively zero. At the floor's
/// 125 kbps quarter-resolution target, keyframes are ~20 KB ≈ 17
/// packets; intact-arrival is `0.92^17 ≈ 24 %` — recovered within a
/// few PLI cycles.
///
/// Loss-tolerance dominates resolution here: a usable low-resolution
/// stream beats a frozen full-resolution one. When the operator wants
/// higher quality on a clean link, that's a future capacity policy
/// concern (track #48 follow-up): observe loss + dynamically
/// renegotiate to a higher RID. This baseline is "make federated work
/// at all under realistic loss."
///
/// Robust against partial-layer pools: `pool_rids.last()` degrades to
/// "best available floor" — when the source resolution is too small for
/// quarter (per `MIN_LAYER_DIM` filter in `LayerSpec::vp8_simulcast`),
/// last is `h` or `f`. Caller guarantees `pool_rids` is non-empty.
///
/// **Federated H.264.** When the active codec is H.264 the pool produces a
/// single on-demand layer at the `fed` RID
/// ([`crate::encode::pool::SimulcastRid::federated`]), already at
/// quarter resolution + a capped bitrate via `LayerSpec::single_federated`
/// — so `pool_rids` is `[fed]` and `last()` returns it. The loss math
/// above (small IDR survives the relay) is achieved at the encoder for
/// H.264 rather than by floor-picking among simulcast tiers as it is for
/// VP8; this function just returns the one RID the H.264 pool offered.
pub(crate) fn select_single_rid_for_federated_offer(pool_rids: &[SimulcastRid]) -> SimulcastRid {
    pool_rids
        .last()
        .expect("caller must guarantee pool_rids is non-empty")
        .clone()
}

/// Parse the offer SDP's `a=simulcast:recv <rid>;<rid>;...` line and return
/// the RIDs the browser is willing to receive.
///
/// Returns:
/// - `None` if the offer's video section has no `a=simulcast:recv`
///   directive at all (the federated [`PeerDisplayConnection`] path post-#46
///   diagnostic landed at `e815bac`, and any offerer that hasn't munged
///   simulcast:recv into its track shape).
/// - `Some(vec)` of `SimulcastRid`s, in offer order, when the directive is
///   present (the local `DisplaySlot` path in `static/app.html`, which
///   defaults to `a=simulcast:recv f` and can opt into `f;h;q` before
///   `setLocalDescription`).
///
/// The caller in [`WebRtcPeer::new`] uses this to **intersect** the
/// peer's [`active_rids`] (derived from encoder pool subscriptions) with
/// what the offer actually requested. Without the intersection, the peer
/// would honestly answer with 3 RIDs even when the offer was
/// single-encoding — and the browser would receive a multi-RID
/// `a=simulcast:send full;half;quarter` answer with no `a=ssrc`
/// declarations to pair RIDs with packets, drop every RTP packet, and
/// stay at `framesDecoded=0`. Confirmed empirically against Chrome via
/// `pliCount > 0`, `packetsReceived > 0`, `framesDecoded == 0`. See the
/// `WebRtcPeer::new` callsite for the intersection logic and the
/// `parse_offer_simulcast_recv_rids_*` tests below.
///
/// Section-aware: only the first `m=video` section is consulted. Audio
/// `simulcast` lines (rare) are ignored. The function returns `None`
/// for any offer without a video section, matching the existing
/// `first_video_mid_from_offer` semantics.
///
/// Forward-compat: unknown / non-canonical RID names are passed through
/// to [`SimulcastRid::from_str_loose`]. Tokens that don't parse to a
/// known RID variant are silently dropped from the returned list rather
/// than failing the whole parse — keeps the answer-side intersection
/// useful even if a future browser advertises an unrecognized RID
/// alongside known ones.
pub(crate) fn parse_offer_simulcast_recv_rids(sdp: &str) -> Option<Vec<SimulcastRid>> {
    let mut in_video = false;
    for raw in sdp.lines() {
        let line = raw.trim_end_matches('\r');
        if line.starts_with("m=") {
            if in_video {
                // Past the first video section without finding it.
                return None;
            }
            in_video = line.starts_with("m=video ");
            continue;
        }
        if !in_video {
            continue;
        }
        let Some(rest) = line.strip_prefix("a=simulcast:") else {
            continue;
        };
        // Valid forms (RFC 8853):
        //   a=simulcast:recv f;h;q
        //   a=simulcast:send f;h;q recv x          (bidirectional)
        // We only care about the recv side from the offerer's POV.
        let parts: Vec<&str> = rest.split_whitespace().collect();
        let mut i = 0;
        while i + 1 < parts.len() {
            if parts[i] == "recv" {
                let rids: Vec<SimulcastRid> = parts[i + 1]
                    .split(';')
                    .filter_map(SimulcastRid::from_str_loose)
                    .collect();
                return Some(rids);
            }
            i += 2;
        }
    }
    None
}

/// Whether an offer is from a **federated** viewer (a
/// `PeerDisplayConnection` reaching us over the TURN relay), as opposed to
/// a local `DisplaySlot`.
///
/// The discriminator is the same one [`WebRtcPeer::new`] already uses to
/// decide between the local recv-simulcast and federated single-encoding answer
/// paths: the local `DisplaySlot` injects recv-simulcast (default `f`, opt-in
/// `f;h;q`) before `setLocalDescription`, whereas the federated
/// `PeerDisplayConnection` offer carries no `a=simulcast:recv` directive.
/// So "no recv-simulcast line" means "federated single-encoding peer".
///
/// `handle_offer_pool_mode` consults this to mark the peer's
/// [`PeerCodecPreferences`] federated, which makes the pool spawn the
/// loss-resilient quarter-resolution / capped-bitrate on-demand H.264
/// layer ([`crate::encode::pool::LayerSpec::single_federated`]).
pub(crate) fn offer_is_federated(offer_sdp: &str) -> bool {
    parse_offer_simulcast_recv_rids(offer_sdp).is_none()
}

#[cfg(test)]
mod parse_offer_simulcast_recv_rids_tests {
    use super::*;

    /// Federated `PeerDisplayConnection` path: offer has no
    /// `a=simulcast:recv`. The fix-site uses the `None` return to
    /// narrow active_rids to a single layer.
    #[test]
    fn federated_offer_without_simulcast_returns_none() {
        let sdp = "v=0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=mid:0\r\n\
                   a=rtpmap:96 VP8/90000\r\n\
                   a=recvonly\r\n";
        assert_eq!(parse_offer_simulcast_recv_rids(sdp), None);
    }

    /// Opt-in multi-RID `DisplaySlot` path: offer contains
    /// `a=simulcast:recv f;h;q`. Returns the three RIDs in offer order — the
    /// fix-site keeps all three because the browser explicitly asked for them.
    #[test]
    fn local_offer_with_full_simulcast_returns_all_three() {
        let sdp = "v=0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=mid:0\r\n\
                   a=rtpmap:96 VP8/90000\r\n\
                   a=rid:f recv\r\n\
                   a=rid:h recv\r\n\
                   a=rid:q recv\r\n\
                   a=simulcast:recv f;h;q\r\n\
                   a=recvonly\r\n";
        assert_eq!(
            parse_offer_simulcast_recv_rids(sdp),
            Some(vec![
                SimulcastRid::full(),
                SimulcastRid::half(),
                SimulcastRid::quarter(),
            ]),
        );
    }

    /// Subset offer (e.g. a constrained-bandwidth browser asking for
    /// half + quarter only). The fix-site intersects with the peer's
    /// own active_rids and forwards the overlap.
    #[test]
    fn offer_with_subset_returns_subset_in_offer_order() {
        let sdp = "v=0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=simulcast:recv h;q\r\n";
        assert_eq!(
            parse_offer_simulcast_recv_rids(sdp),
            Some(vec![SimulcastRid::half(), SimulcastRid::quarter()]),
        );
    }

    /// Bidirectional `simulcast` line (`a=simulcast:send X recv Y`) —
    /// uncommon but RFC 8853-valid. Parser must walk past the `send`
    /// half and find the `recv` half.
    #[test]
    fn bidirectional_simulcast_picks_recv_half() {
        let sdp = "v=0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=simulcast:send x recv f;h\r\n";
        assert_eq!(
            parse_offer_simulcast_recv_rids(sdp),
            Some(vec![SimulcastRid::full(), SimulcastRid::half()]),
        );
    }

    /// `a=simulcast:` lines outside the video section are ignored
    /// (defensive — audio sections never have simulcast in our setup,
    /// but the section-awareness keeps a future audio simulcast from
    /// confusing the parser).
    #[test]
    fn audio_simulcast_is_ignored() {
        let sdp = "v=0\r\n\
                   m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                   a=simulcast:recv f;h\r\n";
        assert_eq!(parse_offer_simulcast_recv_rids(sdp), None);
    }

    // ----- #48 floor-pick tests --------------------------------------------

    /// **#48 acceptance**: full simulcast pool → federated single-RID
    /// peer picks the floor (`q`), not the top (`f`). The top would
    /// produce ~500 KB keyframes that can't survive 8 % loss
    /// (`0.92^420 ≈ 1.4e-15`); the floor produces ~20 KB keyframes
    /// (`0.92^17 ≈ 24 %`) that recover within seconds of PLI.
    #[test]
    fn select_floor_for_full_simulcast_pool() {
        let pool = vec![
            SimulcastRid::full(),
            SimulcastRid::half(),
            SimulcastRid::quarter(),
        ];
        assert_eq!(
            select_single_rid_for_federated_offer(&pool),
            SimulcastRid::quarter(),
        );
    }

    /// Defensive: partial pool (small source: quarter dropped by
    /// `MIN_LAYER_DIM` filter in `LayerSpec::vp8_simulcast`) → pick
    /// the best available floor. Degrades to `h` rather than failing
    /// or skipping back to `f`.
    #[test]
    fn select_floor_for_two_layer_pool() {
        let pool = vec![SimulcastRid::full(), SimulcastRid::half()];
        assert_eq!(
            select_single_rid_for_federated_offer(&pool),
            SimulcastRid::half(),
        );
    }

    /// Tiny source: only `f` survives. Floor *is* `f`. Federated
    /// peer picks `f` and accepts the higher loss-vulnerability —
    /// nothing else to fall back to.
    #[test]
    fn select_full_when_only_one_layer() {
        let pool = vec![SimulcastRid::full()];
        assert_eq!(
            select_single_rid_for_federated_offer(&pool),
            SimulcastRid::full(),
        );
    }

    /// Forward-compat: unknown RID tokens silently drop, known ones
    /// pass through. An offer mixing recognized + future RID names
    /// must not break the intersection — it just narrows to the
    /// intersection of (peer's RIDs) ∩ (recognized offer RIDs).
    #[test]
    fn unknown_rid_tokens_are_dropped_known_pass_through() {
        let sdp = "v=0\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                   a=simulcast:recv f;ultra;q\r\n";
        assert_eq!(
            parse_offer_simulcast_recv_rids(sdp),
            Some(vec![SimulcastRid::full(), SimulcastRid::quarter()]),
        );
    }
}

impl WebRtcPeer {
    /// Create a new peer from an SDP offer, returning `(Self, answer_sdp)`.
    ///
    /// Steps:
    /// 1. Build an [`RTCPeerConnection`] with the active pool codec registered.
    /// 2. Bind a per-peer UDP socket and register it as a host candidate.
    /// 3. Apply the browser offer and generate the SDP answer.
    /// 4. Spawn the driver task and return.
    ///
    /// ## `active_codec` + `active_rids` contract
    ///
    /// Each peer gets its own RTC peer connection. The caller passes
    /// the single codec this peer should negotiate (the active codec
    /// selected from "what the encoder pool can currently produce"
    /// AND "what the peer's offer advertised") plus the simulcast
    /// RIDs the pool is currently producing for that codec. The
    /// caller derives `active_rids` from the initial pool
    /// subscriptions filtered to the active codec — NOT from
    /// `pool.always_on()` directly — so the answer SDP advertises
    /// exactly what the intake will forward (per phase 4c
    /// correction #2).
    ///
    /// VP8 simulcast lights up here when `active_rids.len() > 1`:
    /// the track is built with N encodings (one per RID, each with
    /// its own SSRC), and the answer SDP carries
    /// `a=simulcast:send full;half;quarter` plus `a=rid:* send`
    /// lines automatically as a consequence of the multi-encoding
    /// track shape. For single-codec / single-layer paths (default local
    /// DisplaySlot `f`, federated floor RID, H.264, or VP8 with only one
    /// surviving layer post-MIN_LAYER_DIM filter) `active_rids.len() == 1`
    /// and the answer is plain sendonly.
    ///
    /// Empty / no-overlap cases are surfaced to `handle_offer` as
    /// [`CallerError::WebRtc`] errors rather than producing a silent
    /// broken stream — matches the "no compatible codec, clean
    /// reject" contract from the multi-viewer redesign.
    ///
    /// `ice_tx` carries server→browser trickle ICE candidates. Host and
    /// ICE-TCP candidates are emitted inline in the answer SDP, but the
    /// server-reflexive (srflx) candidate is gathered off the critical
    /// path by the driver's UDP forwarders (see audit F8) and trickled
    /// through this channel as it arrives. The browser also trickles its
    /// own candidates back via `add_ice_candidate`.
    ///
    /// Returns `(peer, encoded_frame_tx, answer_sdp)`. The
    /// `encoded_frame_tx` is the sender side of the per-peer
    /// encoded frame channel — the caller (`Self::new`) hands it
    /// directly to `pool_frame_intake` rather than parking it on
    /// the struct.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn build_with_codec_set(
        peer_id: PeerId,
        offer_sdp: &str,
        active_codec: CodecKind,
        active_rids: &[SimulcastRid],
        ice_config: &IceConfig,
        tcp_peer_registry: Option<Arc<TcpPeerRegistry>>,
        tcp_advertised_addr: Option<SocketAddr>,
        input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
        interactive_source: Option<Arc<crate::BrowserInputSource>>,
        clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
        clipboard_authorized: crate::BrowserInputAuthorization,
        authority_handler: AuthorityChannelHandler,
        tile_control_handler: TileControlHandler,
        ice_tx: mpsc::Sender<(PeerId, String)>,
        keyframe_request_tx: mpsc::Sender<SimulcastRid>,
    ) -> Result<(Self, mpsc::Sender<OutboundEncodedFrame>, String), CallerError> {
        if active_rids.is_empty() {
            return Err(CallerError::WebRtc(
                "active_rids is empty — caller must derive at least one \
                 RID from the peer's initial pool subscriptions before \
                 constructing WebRtcPeer"
                    .to_string(),
            ));
        }

        let codec_params = rtc_codec_parameters(active_codec)?;
        let video_mid = first_video_mid_from_offer(offer_sdp).unwrap_or_else(|| "0".to_string());

        // We need to know the local ufrag before SDP generation so the TCP
        // dispatcher can route accepted ICE-TCP sockets to this peer.
        let local_ufrag = new_ice_fragment();
        let local_pwd = new_ice_password();

        let mut setting_engine = SettingEngine::default();
        setting_engine.set_ice_credentials(local_ufrag.clone(), local_pwd);
        // Pin the answerer's DTLS role to `Server` so the generated
        // answer carries `a=setup:passive`. Per RFC 5763 § 5 that makes
        // the browser the DTLS client and the initiator of the
        // handshake — which is the path the rtc 0.9 stack actually
        // drives. Letting the answer default to `a=setup:active` (the
        // alternative role for an answerer to `actpass`) leaves rtc's
        // DTLS state machine waiting for an event that never fires
        // over the selected ICE-TCP candidate: in our slice-3a.2 setup
        // the connection stalls at STUN keepalives forever, no DTLS
        // bytes are ever emitted, no SRTP context is established,
        // write_rtp returns Ok but produces no encrypted output, and
        // the dashboard renders black indefinitely. Diagnosed in
        // #41 (RFC 7983 byte-class instrumentation across all four
        // hops showed Stun-only both ways with `a=setup:active` in
        // the answer); the fix is named explicit role assignment
        // here, before the RTCPeerConnection is built so all generated
        // SDP carries the pinned role. See the
        // `build_with_codec_set_pins_setup_passive_in_answer` test.
        setting_engine
            .set_answering_dtls_role(RTCDtlsRole::Server)
            .map_err(|e| CallerError::WebRtc(format!("set answering DTLS role: {e}")))?;

        let mut media_engine = MediaEngine::default();
        media_engine
            .register_codec(codec_params.clone(), RtpCodecKind::Video)
            .map_err(|e| CallerError::WebRtc(format!("register codec: {e}")))?;
        for uri in [
            "urn:ietf:params:rtp-hdrext:sdes:mid",
            "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
            "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id",
        ] {
            media_engine
                .register_header_extension(
                    RTCRtpHeaderExtensionCapability {
                        uri: uri.to_string(),
                    },
                    RtpCodecKind::Video,
                    None,
                )
                .map_err(|e| CallerError::WebRtc(format!("register RTP extension: {e}")))?;
        }

        // Phase 4c: build one encoding per active RID. For VP8
        // simulcast (active_rids = [full, half, quarter]) this produces
        // a 3-encoding track and `RTCPeerConnection::create_answer` then
        // emits `a=simulcast:send full;half;quarter` + `a=rid:* send`
        // lines as a consequence of the multi-encoding shape — server-
        // side answer-side simulcast that str0m couldn't do, hence the
        // migration to rtc 0.9.
        //
        // For single-RID (H.264 or VP8 with all simulcast layers
        // dropped below MIN_LAYER_DIM) this produces a single
        // encoding and the answer is plain sendonly with no
        // simulcast lines — bit-for-bit equivalent to pre-4c output.
        //
        // Each encoding gets its own SSRC because rtc's
        // `RTCRtpSender::write_rtp` routes packets to encodings by
        // matching `packet.header.ssrc` against the encoding's
        // declared SSRC. The driver looks up the SSRC by RID at
        // write time via `state.rtp.by_rid` (populated below from
        // `encodings_by_rid`).
        let mut encodings = Vec::with_capacity(active_rids.len());
        let mut encodings_by_rid: Vec<(SimulcastRid, u32)> = Vec::with_capacity(active_rids.len());
        for rid in active_rids {
            let ssrc = new_ssrc();
            encodings_by_rid.push((rid.clone(), ssrc));
            encodings.push(RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    rid: rid.as_str().to_string(),
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: codec_params.rtp_codec.clone(),
                ..Default::default()
            });
        }
        let track = MediaStreamTrack::new(
            format!("display-{peer_id}"),
            format!("display-video-{peer_id}"),
            format!("display-video-{peer_id}"),
            RtpCodecKind::Video,
            encodings,
        );

        // **Phase 4d.3b — TWCC signal pipeline.** Wire rtc 0.9's
        // interceptor registry with SR/RR + TWCC sender + the custom
        // `TwccTapInterceptor`, which observes inbound RTCP at the
        // chain's outermost `handle_read`, downcasts each
        // `TransportLayerCc` packet, and projects a compact
        // [`TwccEvent`] onto an unbounded mpsc channel.
        //
        // **Why a custom tap, not rtc's stats path:** rtc 0.9 consumes
        // RTCP internally and never surfaces it via
        // `RTCMessage::RtcpPacket`, and its
        // `RTCRemoteInboundRtpStreamStats` accumulator stays at all-
        // zero defaults regardless of which interceptors are wired.
        // Tapping the interceptor chain is the only place we can
        // observe TWCC without patching rtc 0.9. See
        // [`crate::twcc_tap`] module docs for the full
        // background.
        //
        // **Chain order:** `Registry::with(...)` puts the supplied
        // wrapper outermost, so call sequence
        //
        //   `Registry::new() → configure_rtcp_reports(.) →
        //    configure_twcc_sender_only(.) →
        //    .with(NackResponderBuilder::new().with_size(2048).build()) →
        //    .with(|inner| TwccTapInterceptor::new(inner, tx))`
        //
        // produces a chain whose outermost layer is the tap. The tap
        // observes, then forwards to the NACK responder, then
        // twcc_sender_only, then rtcp_reports, then rtc's internals —
        // keeping the existing stack's behaviour intact. The tap
        // mutates nothing.
        //
        // **NACK retransmission (loss recovery).** `video_rtcp_feedback`
        // advertises `a=rtcp-fb ... nack`, so a browser receiver sends
        // RTCP `TransportLayerNack` for gaps. Until now nothing handled
        // them — the answer promised retransmission and delivered none.
        // The rtc 0.9 `NackResponderInterceptor` (added here via
        // `NackResponderBuilder::new().with_size(2048).build()`)
        // `bind_local_stream`s every outbound video stream whose
        // negotiated feedback includes generic `nack` (it does), buffers
        // each outbound RTP packet on `handle_write`, and on inbound
        // NACK retransmits the requested sequence numbers from that
        // buffer. We never configure an RTX SSRC / payload type, so the
        // responder retransmits same-SSRC, in-band (rtc-interceptor 0.9
        // `nack/responder.rs`: "No RTX: retransmit original packet
        // as-is") — no SDP change is needed and `sanitize_answer_sdp`
        // (which only rewrites `a=rid:` / `a=simulcast:` lines) leaves
        // the advertised `a=rtcp-fb nack` intact. `with_size(2048)`
        // (a power of two, the responder's hard constraint) buffers
        // ~2048 packets per stream — at the federated floor's small
        // packetization that comfortably covers the round-trip a NACK
        // needs to arrive while the packet is still retransmittable.
        //
        // Placed INSIDE the tap (applied before `.with(tap)`) so the
        // tap stays the documented outermost observer; the responder
        // sits between the tap and `twcc_sender_only`. Retransmitted
        // packets the responder injects are drained via its
        // `poll_write` and flow inward to rtc's transport, exactly like
        // primary RTP.
        //
        // The aggregator that consumes `twcc_tap_rx` is spawned
        // below, after `shutdown` is created, so it shares the
        // peer's cancellation token.
        let (twcc_tap_tx, twcc_tap_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::twcc_tap::TwccEvent>();
        let registry = rtc::interceptor::Registry::new();
        let registry =
            rtc::peer_connection::configuration::interceptor_registry::configure_rtcp_reports(
                registry,
            );
        let registry =
            rtc::peer_connection::configuration::interceptor_registry::configure_twcc_sender_only(
                registry,
                &mut media_engine,
            )
            .map_err(|e| CallerError::WebRtc(format!("configure twcc: {e}")))?;
        let registry = registry.with(
            rtc::interceptor::NackResponderBuilder::new()
                .with_size(2048)
                .build(),
        );
        let registry = registry
            .with(|inner| crate::twcc_tap::TwccTapInterceptor::new(inner, twcc_tap_tx.clone()));

        let mut rtc = RTCPeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_setting_engine(setting_engine)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build()
            .map_err(|e| CallerError::WebRtc(format!("build rtc peer: {e}")))?;
        let sender_id = rtc
            .add_track(track)
            .map_err(|e| CallerError::WebRtc(format!("add video track: {e}")))?;

        // --- Bind one UDP socket per local interface -----------------------
        // The ICE agent matches incoming packets against local candidates by
        // `(local_address, port)`. A single wildcard bind would surface as
        // `0.0.0.0:port` on `socket.local_addr()`, which never matches the
        // concrete-IP candidates we'd advertise — connectivity checks then
        // can't form a valid pair. So we bind a separate socket per
        // interface and emit a host candidate that exactly matches each
        // socket's local address.
        let mut sockets: Vec<Arc<UdpSocket>> = Vec::new();
        // WebRTC needs loopback so a browser on the same machine can
        // pair against the daemon's host candidates. Each socket's local
        // address is also the srflx host base: the driver's forwarders
        // read it back via `local_addr()` when gathering the srflx
        // candidate off the critical path (audit F8), so we don't need to
        // carry the bases separately here.
        let local_addrs = intendant_core::net::routable_local_addrs(true);
        for iface_addr in &local_addrs {
            let bind_addr = SocketAddr::new(*iface_addr, 0);
            let socket = match UdpSocket::bind(bind_addr).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[display/webrtc] skipping UDP bind on {iface_addr}: {e}");
                    continue;
                }
            };
            let local = match socket.local_addr() {
                Ok(a) => a,
                Err(e) => {
                    eprintln!(
                        "[display/webrtc] skipping UDP socket on {iface_addr}: local_addr {e}"
                    );
                    continue;
                }
            };
            let candidate = host_candidate_init(local, RTCIceProtocol::Udp);
            match rtc.add_local_candidate(candidate) {
                Ok(()) => sockets.push(Arc::new(socket)),
                Err(e) => eprintln!("[display/webrtc] skipping UDP host candidate {local}: {e}"),
            }
        }
        if sockets.is_empty() {
            return Err(CallerError::WebRtc(
                "no usable local UDP sockets bound".to_string(),
            ));
        }

        // --- Server-reflexive (srflx) UDP candidates via STUN -------------
        //
        // ICE host candidates carry the socket's *local* address. On a
        // NAT'd host (e.g. a GCP VM with internal `10.x` / loopback only)
        // those are unreachable from a remote browser, so without a public
        // candidate the only thing that can pair is the ICE-TCP candidate —
        // which has Windows transport problems. To advertise a reachable
        // UDP path we ask the configured STUN server what public `IP:port`
        // it observes for each of our ICE sockets and add that as a srflx
        // candidate. Because the binding request goes out the *same* socket
        // ICE will use, the mapping matches the candidate's base, so a 1:1
        // NAT (GCP) returns the public IP the browser can reach directly.
        //
        // CRITICAL-PATH NOTE (audit F8): the gathering is deliberately NOT
        // done here. Doing it on the answer path — even concurrently across
        // sockets — meant every peer setup waited up to STUN_BINDING_TIMEOUT
        // (1.5s) before `create_answer` whenever the STUN server was
        // blocked/unreachable (e.g. UDP egress firewalled), since concurrency
        // only dedupes the one timeout, it does not remove it from the path.
        //
        // Instead the srflx candidate is gathered and *trickled*: the answer
        // below is created and returned to the signaling layer immediately
        // with host + ICE-TCP candidates, and each per-socket UDP forwarder
        // in the driver folds a STUN Binding exchange into its read loop
        // (single reader → no recv race with the ICE traffic the same socket
        // carries). When a mapping arrives the driver adds the srflx
        // candidate to its `RTCPeerConnection` and sends it to the browser
        // over the already-wired server→browser ICE trickle channel
        // (`ice_tx` → web_gateway `display_ice` → `pc.addIceCandidate`,
        // which the browser buffers until the answer is applied). A
        // reachable STUN server therefore still advertises the srflx
        // candidate (just off the critical path); an unreachable one adds
        // zero setup latency because nothing on the answer path waits on it.
        //
        // The ICE sockets and the STUN server config (`ice_config`) are
        // handed to the driver below to drive this; each forwarder derives
        // its socket's host base from `local_addr()`.

        // --- ICE-TCP candidate (Host-header derived, pair-friendly) ------
        //
        // Earlier iterations tried to advertise `127.0.0.1:<http_port>` as
        // the TCP candidate, hoping the browser's own loopback would be
        // mapped back to us via port-forward / SSH tunnel. Firefox (and
        // Chrome, confirmed experimentally via getStats) silently *filter*
        // remote loopback candidates from candidate-pair formation as an
        // anti-rebinding mitigation — the candidate shows up in the remote
        // list but never pairs, ICE stalls, nothing works.
        //
        // So instead the web gateway parses the `Host:` header from the
        // browser's WebSocket handshake (the one address we KNOW the
        // browser thinks reaches us) and hands us the resulting
        // `SocketAddr` as `tcp_advertised_addr`. If it's a non-loopback
        // IP we advertise exactly that — the browser will happily form a
        // pair because the IP matches what it's already using for HTTP
        // and isn't loopback so the filter doesn't trigger.
        //
        // Users accessing via `http://localhost:...` still get `None`
        // here (Host header is `localhost`, which doesn't parse as an IP
        // — or parses as loopback which we also reject): they don't get
        // a TCP path at all. Their workaround is to bind the port-forward
        // on a non-loopback interface and connect via the LAN IP. There's
        // no clever ICE trick that gets around Firefox's loopback filter
        // for that case.
        //
        // On the server side we "lie" to the RTC core about the inbound
        // destination: regardless of what `stream.local_addr()` says
        // (typically the VM's internal interface IP behind the NAT), we
        // pass `destination = tcp_advertised_addr` to `handle_read`.
        // ICE matches the lied-about destination to its single local
        // TCP candidate and forms a clean pair; data still flows because
        // the TCP stream is bidirectional and we own the write half
        // directly, no kernel routing involved.
        let mut peer_registration = None;
        let mut tcp_conn_rx: Option<mpsc::Receiver<AcceptedTcpConnection>> = None;
        let mut tcp_advertised: Option<SocketAddr> = None;
        if let (Some(registry), Some(advertised)) = (
            tcp_peer_registry.as_ref(),
            tcp_advertised_addr.filter(|a| !a.ip().is_loopback() && !a.ip().is_unspecified()),
        ) {
            let (registration, rx) = registry.register(local_ufrag.clone());
            peer_registration = Some(registration);
            tcp_conn_rx = Some(rx);
            tcp_advertised = Some(advertised);
            // RFC 6544 requires TCP ICE candidates to carry a `tcptype`
            // attribute. `Candidate::host(addr, "tcp")` doesn't set it,
            // and browsers drop TCP candidates that lack it. The builder
            // lets us set `tcptype: passive` — "the remote actively opens
            // the TCP connection to us", the correct role for a
            // server-side host candidate.
            let candidate = host_candidate_init(advertised, RTCIceProtocol::Tcp);
            if let Err(e) = rtc.add_local_candidate(candidate) {
                eprintln!("[display/webrtc] failed to add TCP host candidate {advertised}: {e}");
            }
            eprintln!(
                "[display/webrtc] peer {peer_id}: ICE-TCP enabled on {advertised} for ufrag {local_ufrag}"
            );
        } else if tcp_peer_registry.is_some() {
            // Registry available but no suitable advertised address — the
            // browser connected via hostname/loopback so we have no
            // non-loopback IP to advertise. Log once so operators can
            // spot the "why does TCP never kick in" case.
            eprintln!(
                "[display/webrtc] peer {peer_id}: no ICE-TCP candidate advertised (no non-loopback Host header)"
            );
        }

        // --- Parse the offer and produce the answer ----------------------
        let offer = RTCSessionDescription::offer(offer_sdp.to_string())
            .map_err(|e| CallerError::WebRtc(format!("parse offer: {e}")))?;
        rtc.set_remote_description(offer)
            .map_err(|e| CallerError::WebRtc(format!("set remote offer: {e}")))?;
        let answer = rtc
            .create_answer(None)
            .map_err(|e| CallerError::WebRtc(format!("create answer: {e}")))?;
        rtc.set_local_description(answer.clone())
            .map_err(|e| CallerError::WebRtc(format!("set local answer: {e}")))?;
        // Sanitized-wire-only shim — narrow workaround for two rtc 0.9
        // SDP-writer bugs that fire on multi-RID simulcast send:
        //   1. each `a=rid:<rid> send` line emitted twice (six lines for
        //      f/h/q instead of three);
        //   2. `a=simulcast:send f;h;q send f;h;q` instead of the RFC
        //      8853-correct `a=simulcast:send f;h;q`.
        // WebKit rejects (2) with `SyntaxError: Malformed simulcast
        // line` on setRemoteDescription, so the answer never lands and
        // no media flows. See `sanitize_answer_sdp` above for the
        // exact transformation + test coverage.
        //
        // Why the call sequence is what it is: rtc 0.9 caches the
        // `create_answer` result in `PeerConnectionInternal.last_answer`
        // (peer_connection/mod.rs:944) and `set_local_description`
        // does a direct string-equality check against that cache
        // (peer_connection/internal.rs:373, :408). Sanitizing
        // *between* `create_answer` and `set_local_description`
        // therefore fails with `ErrSDPDoesNotMatchAnswer`. So the
        // contract here is:
        //
        //   - Pass the *literal* `create_answer` output to
        //     `set_local_description` (above) — rtc's strict gate is
        //     satisfied, rtc's local state stays internally consistent
        //     with the malformed SDP it produced.
        //   - Sanitize *only* the bytes we ship on the wire — WebKit
        //     accepts the corrected line, media flows.
        //
        // Yes, this leaves the rtc state's local SDP and the wire SDP
        // diverged. The hypothesis being tested is that the divergence
        // is benign because rtc's media plane was built from track
        // encodings, not from re-parsing its own emitted SDP — so the
        // doubled `a=rid:` lines and `a=simulcast:send ... send ...`
        // attribute are pure signaling artifacts. If this experiment
        // proves the hypothesis, the shim stays as a narrow
        // compatibility band-aid until the dep is patched.
        //
        // Long-term fix: patch rtc 0.9's SDP writer at source — fix
        // the per-RID `send` line duplication and the doubled-direction
        // `a=simulcast:` emission — and remove this sanitizer call,
        // not the dep's validation gate. Relaxing the gate was
        // explicitly excluded as the wrong fix shape.
        let answer_sdp = sanitize_answer_sdp(&answer.sdp);
        // Dump every a=candidate line from the answer so we can see exactly
        // what the RTC core emitted — this is the fastest way to diagnose
        // "browser never tries to connect to the TCP candidate" symptoms.
        for line in answer_sdp.lines().filter(|l| l.starts_with("a=candidate:")) {
            eprintln!("[display/webrtc] peer {peer_id}: answer {line}");
        }

        // --- Spawn the driver --------------------------------------------
        let (encoded_frame_tx, encoded_frame_rx) =
            mpsc::channel::<OutboundEncodedFrame>(ENCODED_FRAME_CHANNEL);
        let (command_tx, command_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL);
        // Phase 4d.1: per-peer observed send bitrate (`bytes_sent`
        // delta, local egress only — see WebRtcPeer::observed_send_bitrate_rx
        // for the semantic distinction from capacity). Initial value
        // None: the driver's first poll seeds the per-SSRC `prev`
        // map and returns no delta; the second poll, one
        // TWCC_POLL_INTERVAL later, publishes the first measurable
        // rate (None still until any RTP has actually been sent).
        let (observed_send_bitrate_tx, observed_send_bitrate_rx) =
            watch::channel::<Option<u64>>(None);
        // Phase 4d.3a: per-peer per-RID receiver-feedback health
        // (RR-derived, populated from rtc 0.9's
        // `RTCRemoteInboundRtpStreamStats`). Initial value is the
        // empty map: no RR has arrived yet. Per-RID entries appear
        // as RRs land for each outbound SSRC. Layer-selection
        // policy (4d.3b/c) treats missing RIDs as "no signal yet,
        // stay conservative" rather than as "healthy."
        let (remote_inbound_health_tx, remote_inbound_health_rx) =
            watch::channel::<HashMap<SimulcastRid, PeerLayerHealth>>(HashMap::new());
        // **Phase 4d.3b**: per-peer aggregate TWCC health, derived
        // from inbound `TransportLayerCC` packets observed by the
        // [`crate::twcc_tap::TwccTapInterceptor`] wired
        // into the rtc interceptor chain above. Initial value
        // `None`: the aggregator hasn't published its first
        // 1-second window yet. After the first publish the channel
        // stays `Some(_)`, replaced once per window. The capacity
        // policy in [`crate::aggregator`] subscribes via
        // [`WebRtcPeer::subscribe_twcc_health`].
        let (twcc_health_tx, twcc_health_rx) =
            watch::channel::<Option<crate::twcc_tap::TwccHealth>>(None);
        let shutdown = CancellationToken::new();
        // Aggregator task drains `twcc_tap_rx` and publishes one
        // `TwccHealth` per second. Exits on `shutdown.cancelled()`,
        // on the tap channel closing (rtc dropped → tap dropped →
        // sender dropped → recv returns None), or on all watch
        // receivers dropping.
        crate::twcc_tap::spawn_twcc_health_aggregator(
            twcc_tap_rx,
            twcc_health_tx,
            shutdown.clone(),
        );

        // Phase 4c: pass the full per-RID encoding map through to the
        // driver. For VP8 simulcast `encodings_by_rid` carries
        // (full, ssrc_f), (half, ssrc_h), (quarter, ssrc_q); the
        // driver builds one packetizer per RID + uses the matching
        // SSRC at write time so `RTCRtpSender::write_rtp` routes to
        // the right encoding.
        tokio::spawn(driver(
            peer_id,
            rtc,
            RtpSendConfig {
                sender_id,
                mid: video_mid,
                codec: codec_params.rtp_codec,
                encodings: encodings_by_rid,
            },
            sockets,
            tcp_conn_rx,
            tcp_advertised,
            peer_registration,
            encoded_frame_rx,
            command_rx,
            input_handler,
            interactive_source.clone(),
            clipboard_handler,
            clipboard_authorized.clone(),
            authority_handler,
            tile_control_handler,
            keyframe_request_tx,
            observed_send_bitrate_tx,
            remote_inbound_health_tx,
            // F8: srflx gathering is folded into the driver's UDP
            // forwarders and trickled via `ice_tx`, off the answer path.
            // `ice_config` carries the STUN server config the driver
            // resolves (DNS) and queries off-path; cloning a small config
            // struct keeps that resolution out of `create_answer`.
            ice_config.clone(),
            ice_tx,
            shutdown.clone(),
        ));

        Ok((
            Self {
                peer_id,
                command_tx,
                clipboard_authorized,
                interactive_source,
                observed_send_bitrate_rx,
                remote_inbound_health_rx,
                twcc_health_rx,
                active_rids: active_rids.to_vec(),
                shutdown,
            },
            encoded_frame_tx,
            answer_sdp,
        ))
    }

    /// Build a peer that consumes frames from the shared
    /// [`EncoderPool`] and forwards them to the browser via the RTC driver.
    /// The only public constructor (3c.4c renamed `new_pool_mode` →
    /// `new` after the legacy single-encoder fan-out was deleted in
    /// 3c.4b).
    ///
    /// `codec_set` is derived from `subscriptions` rather than from
    /// the original peer offer prefs. This is the contract that
    /// keeps the partial-result path safe: the SDP we negotiate
    /// enables exactly the codecs the pool can serve, so the peer
    /// can never select a codec we'll silently drop frames for.
    /// Empty subscriptions upstream means the offer handler should
    /// reject before reaching here; we forward the empty case as a
    /// clean `WebRtc("empty subscription set")` rather than silently
    /// constructing a peer with no codecs.
    ///
    /// `lease` and `prefs` are handed to a per-peer `pool_frame_intake`
    /// task that owns the lease's lifetime. On any subscription's
    /// `RecvError::Closed` (typically: `EncoderPool::on_resize`
    /// dropping a slot), the intake task drops the lease, calls
    /// `pool.subscribe(prefs)` for fresh subscriptions+lease, and
    /// resumes forwarding from the new handles. If resubscribe
    /// returns `NoCompatibleCodec`, the intake task signals peer
    /// shutdown via the WebRtcPeer's cancellation token — peers that
    /// can't be served any longer are torn down rather than left in
    /// a black-stream state.
    ///
    /// `drops_counter` is incremented every time the intake's forwarder
    /// drops a frame because the driver's encoded-frame `mpsc` is full
    /// (peer is slow). Callers should share this counter with their
    /// metrics aggregation so the `peer_drops` field on
    /// `DisplayMetricsSnapshot` reflects total drops across all peers.
    /// Tests can pass a fresh `Arc::new(AtomicU64::new(0))` and inspect
    /// it directly.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        peer_id: PeerId,
        offer_sdp: &str,
        ice_config: &IceConfig,
        tcp_peer_registry: Option<Arc<TcpPeerRegistry>>,
        tcp_advertised_addr: Option<SocketAddr>,
        input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
        interactive_source: Option<Arc<crate::BrowserInputSource>>,
        clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
        clipboard_authorized: crate::BrowserInputAuthorization,
        authority_handler: AuthorityChannelHandler,
        tile_control_handler: TileControlHandler,
        ice_tx: mpsc::Sender<(PeerId, String)>,
        pool: Arc<EncoderPool>,
        subscriptions: Vec<EncoderSubscription>,
        lease: PoolLease,
        prefs: PeerCodecPreferences,
        drops_counter: Arc<AtomicU64>,
    ) -> Result<(Self, String), CallerError> {
        if subscriptions.is_empty() {
            return Err(CallerError::WebRtc(
                "new: empty subscription set — offer handler must \
                 reject before reaching here"
                    .to_string(),
            ));
        }
        let active_codec =
            active_codec_from_subscriptions(&subscriptions, &prefs).ok_or_else(|| {
                CallerError::WebRtc(
                    "new: no subscription matched peer codec preferences".to_string(),
                )
            })?;
        let active_codec_set = [active_codec];
        // Filter the peer's original prefs against the single codec the
        // intake will actually forward. The answer and all future
        // resubscribes are locked to this codec, so the RTC sender cannot
        // negotiate one codec while pool_frame_intake selects another.
        let negotiated_prefs = filter_prefs_to_negotiated(&prefs, &active_codec_set);
        // Defensive — should be unreachable: subscriptions is non-empty
        // (early return above), active_codec came from subscriptions,
        // and active_codec also appears in original prefs. So the
        // intersection is non-empty. If it's empty here, something
        // upstream is producing subs for a codec the prefs doesn't
        // include — fail loud.
        if negotiated_prefs.is_empty() {
            return Err(CallerError::WebRtc(
                "new: filter_prefs_to_negotiated produced empty set; \
                 pool returned subscriptions for codecs not in peer prefs"
                    .to_string(),
            ));
        }

        // Phase 4c: derive the RID set the peer's track will advertise
        // from the initial subscriptions filtered to the active codec
        // — per the user's correction #2, the answer SDP must match
        // exactly what the peer subscribed to (NOT what
        // `pool.always_on()` happens to advertise globally; an
        // on-demand encoder construction failure could produce a
        // subscription set narrower than the pool's general layout).
        // Order is preserved as encountered in the subscriptions
        // (which is layer order from `vp8_simulcast`: full / half /
        // quarter), so the answer's `a=rid` lines come out in
        // preference order.
        let pool_rids: Vec<SimulcastRid> = subscriptions
            .iter()
            .filter(|s| s.id.codec == active_codec)
            .map(|s| s.id.rid.clone())
            .collect();
        // Defensive — `active_codec_from_subscriptions` returned
        // Some, so at least one subscription has this codec.
        // Treating an empty pool_rids as a bug rather than a soft
        // failure: build_with_codec_set rejects empty too, so this
        // is a redundant guard with a more specific error message.
        if pool_rids.is_empty() {
            return Err(CallerError::WebRtc(format!(
                "new: active_codec={active_codec:?} resolved but no \
                 subscriptions match it — internal pool/peer state \
                 divergence",
            )));
        }
        // #46 fix: intersect the pool's RIDs with what the offer
        // actually requested via `a=simulcast:recv`. The pool can expose
        // multiple RIDs (f/h/q), but a federated [`PeerDisplayConnection`]
        // offer post-`e815bac` does not include `a=simulcast:recv` — sending an answer
        // declaring 3 RIDs against such an offer produces an
        // `a=simulcast:send f;h;q` answer with no `a=ssrc` declarations
        // (rtc 0.9 SDP-writer bug), which Chrome / WebKit silently
        // refuse to decode. Empirical signature: `framesDecoded == 0`
        // forever, `packetsReceived > 0`, `pliCount > 0`. The local
        // [`DisplaySlot`] path injects recv-simulcast before
        // setLocalDescription. Its default `f` request stays single-RID;
        // opt-in `f;h;q` keeps the multi-RID send path. The federated
        // path narrows to the floor RID.
        //
        // Three branches:
        //  - Offer has no `a=simulcast:recv` → narrow to a single
        //    layer (the floor one selected by `select_single_rid_for_federated_offer`).
        //  - Offer has `a=simulcast:recv [...]` → intersect pool_rids
        //    with the offer's recv list, preserving pool order. Empty
        //    intersection is a hard error (no overlap = no codec).
        //  - Offer requests RIDs the pool isn't producing right now
        //    (e.g. an on-demand layer construction failure) → silently
        //    drop those RIDs from the answer.
        let active_rids: Vec<SimulcastRid> = match parse_offer_simulcast_recv_rids(offer_sdp) {
            None => {
                // Single-encoding offer → narrow to one layer.
                //
                // **#48 tuning**: pick the **floor** (last in
                // `pool_rids`, which is spec-ordered descending
                // bitrate per `LayerSpec::vp8_simulcast` — q
                // (125 kbps @ ¼ res) when all three layers are
                // present), not `pool_rids[0]` (the full layer at
                // 2.5 Mbps). Rationale: the federated path is the
                // only consumer of single-encoding negotiation
                // (`PeerDisplayConnection` post-#46/`e815bac`),
                // and runs over a TURN-relay where moderate (~5-
                // 10 %) sustained packet loss is the operational
                // baseline. At full-layer keyframe sizes (~500 KB
                // = ~420 RTP packets), 8 % loss makes intact
                // delivery `0.92^420 ≈ 1.4e-15` — effectively
                // impossible. Quarter-layer keyframes (~20 KB =
                // ~17 packets) have `0.92^17 ≈ 24 %` intact
                // probability and recover within seconds. Loss-
                // tolerance dominates resolution for "stream
                // remains usable under loss" — full-resolution
                // single-RID federated was empirically frozen
                // (~0.4 fps decoded at 8 % loss before this
                // tuning).
                //
                // The local `DisplaySlot` path is unaffected: it injects
                // recv-simulcast into its offer, so it hits the
                // `Some(offer_rids)` branch below. The default request is
                // `f`; the opt-in adaptive path requests `f;h;q`.
                //
                // Robust against partial-layer pools: if pool
                // dropped the quarter layer because the source is
                // too small (`MIN_LAYER_DIM` filter in
                // `vp8_simulcast`), `pool_rids.last()` becomes
                // `h` or `f` — degrades to "best available floor"
                // rather than failing.
                vec![select_single_rid_for_federated_offer(&pool_rids)]
            }
            Some(offer_rids) => {
                let intersected: Vec<SimulcastRid> = pool_rids
                    .iter()
                    .filter(|r| offer_rids.contains(r))
                    .cloned()
                    .collect();
                if intersected.is_empty() {
                    return Err(CallerError::WebRtc(format!(
                        "new: offer's a=simulcast:recv RIDs \
                             {offer_rids:?} have no overlap with pool's \
                             active RIDs {pool_rids:?} for codec \
                             {active_codec:?}",
                    )));
                }
                intersected
            }
        };
        // Phase 4e: keyframe-request channel from driver → intake.
        // Driver pushes a `SimulcastRid` to this channel for every
        // PLI / FIR whose target SSRC matches one of our outbound
        // encodings; intake reads from the channel and calls
        // `pool.request_keyframe(active_codec, Some(rid))` so PLI
        // recovery hits ONLY the affected layer's encoder. Bounded +
        // lossy by design — see `KEYFRAME_REQUEST_CHANNEL` doc.
        let (keyframe_request_tx, keyframe_request_rx) =
            mpsc::channel::<SimulcastRid>(KEYFRAME_REQUEST_CHANNEL);
        let (peer, encoded_frame_tx, answer_sdp) = Self::build_with_codec_set(
            peer_id,
            offer_sdp,
            active_codec,
            &active_rids,
            ice_config,
            tcp_peer_registry,
            tcp_advertised_addr,
            input_handler,
            interactive_source,
            clipboard_handler,
            clipboard_authorized,
            authority_handler,
            tile_control_handler,
            ice_tx,
            keyframe_request_tx,
        )
        .await?;
        // Spawn the intake task. It owns the encoded_frame_tx (no
        // longer parked on Self after the 3c.4d follow-up cleanup)
        // and a clone of `shutdown` so it can push frames into the
        // existing driver and exit when the peer is torn down. The
        // task owns the lease and resubscribes as needed (see
        // `pool_frame_intake` for the Closed-handling contract).
        //
        // #46 fix companion: filter the subscriptions handed to the
        // intake down to the active RIDs. The driver was built with
        // `active_rids` (intersected with the offer's
        // `a=simulcast:recv`), so it knows about exactly those RIDs;
        // forwarding frames for any other RID hits the driver's
        // "frame for unknown rid" defensive return + log spam (see
        // step 3b in the driver). The pool's full subscription set
        // is preserved through `lease` (refcount + Drop semantics
        // unchanged) so the always-on encoders keep producing for
        // any other peer that wants those layers.
        let active_rid_set: std::collections::HashSet<SimulcastRid> =
            active_rids.iter().cloned().collect();
        let intake_subscriptions: Vec<EncoderSubscription> = subscriptions
            .into_iter()
            .filter(|s| active_rid_set.contains(&s.id.rid))
            .collect();
        let intake_shutdown = peer.shutdown.clone();
        tokio::spawn(pool_frame_intake(
            pool,
            negotiated_prefs,
            intake_subscriptions,
            lease,
            encoded_frame_tx,
            drops_counter,
            keyframe_request_rx,
            intake_shutdown,
        ));

        Ok((peer, answer_sdp))
    }

    /// Send a clipboard update to the browser via the clipboard data channel.
    ///
    /// Returns `Ok(true)` if the command was queued, `Ok(false)` when live
    /// interactive authority is absent or the driver is shutting down.
    pub async fn send_clipboard(&self, content: &ClipboardContent) -> Result<bool, CallerError> {
        let Ok(admitted_revision) = self.clipboard_authorized.admission_revision() else {
            return Ok(false);
        };
        match self
            .command_tx
            .send(Command::SendClipboard {
                content: content.clone(),
                admitted_revision,
            })
            .await
        {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// F-1.2: push a personalized display-input authority state to the
    /// browser over the federated `display_input_authority` data
    /// channel. Used by the federated authority broadcast loop to
    /// fan personalized `you | other | unclaimed` snapshots out to
    /// each subscribed federated WebRtcPeer.
    ///
    /// If the data channel is not yet open at the time of the call,
    /// the message is queued in the driver state and emitted on
    /// `OnDataChannel(OnOpen)` for the matching label. This bootstrap
    /// path is load-bearing: the broadcast loop registers a federated
    /// WebRtcPeer as a subscriber the moment the federation registry
    /// adds it, which can be — and usually is — before the browser's
    /// data channels finish negotiating. Without queueing, the
    /// browser's chip would stall at `unknown` until the next
    /// authority transition.
    ///
    /// Returns `Ok(true)` if the command was queued for the driver,
    /// `Ok(false)` if the driver is shutting down. Send-success at
    /// the channel layer is best-effort and not surfaced; the
    /// federated path tolerates dropped frames at this layer because
    /// the broadcast loop is the primary state-of-truth source and
    /// will re-broadcast on every transition.
    pub async fn send_authority_state(
        &self,
        display_id: u32,
        state: DisplayInputAuthorityState,
    ) -> Result<bool, CallerError> {
        match self
            .command_tx
            .send(Command::SendAuthorityState { display_id, state })
            .await
        {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// D-3b: send a reliable tile-control binary frame to the browser.
    /// Queues in the driver until `tile-control` opens.
    pub async fn send_tile_control_frame(&self, data: bytes::Bytes) -> Result<bool, CallerError> {
        self.send_tile_frame(TileDataChannel::Control, data).await
    }

    /// D-3b: send a reliable tile-snapshot binary frame to the browser.
    /// Queues in the driver until `tile-snapshot` opens.
    pub async fn send_tile_snapshot_frame(&self, data: bytes::Bytes) -> Result<bool, CallerError> {
        self.send_tile_frame(TileDataChannel::Snapshot, data).await
    }

    /// D-3b: send an unreliable/supersedable tile-delta binary frame
    /// to the browser. If the channel is not open, the driver drops
    /// the frame rather than queueing stale deltas.
    pub async fn send_tile_delta_frame(&self, data: bytes::Bytes) -> Result<bool, CallerError> {
        self.send_tile_frame(TileDataChannel::Deltas, data).await
    }

    pub(crate) async fn send_tile_frame(
        &self,
        channel: TileDataChannel,
        data: bytes::Bytes,
    ) -> Result<bool, CallerError> {
        match self
            .command_tx
            .send(Command::SendTileFrame { channel, data })
            .await
        {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Add a trickle ICE candidate from the remote peer.
    ///
    /// The browser sends `{candidate, sdpMid, sdpMLineIndex}`; we only need
    /// the `candidate` string (RFC 5245 format) for the RTC core.
    ///
    /// Browsers obfuscate host candidates as mDNS `.local` hostnames. Resolve
    /// the hostname via the system resolver (nss-mdns / Avahi on Linux,
    /// Bonjour on macOS) and rewrite the candidate string before forwarding to
    /// the driver. Candidates that already contain a literal IP pass through
    /// unchanged.
    pub async fn add_ice_candidate(&self, candidate_json: &str) -> Result<(), CallerError> {
        let parsed: serde_json::Value = serde_json::from_str(candidate_json)
            .map_err(|e| CallerError::WebRtc(format!("parse ICE candidate: {e}")))?;
        let candidate_str = parsed["candidate"].as_str().unwrap_or("");
        if candidate_str.is_empty() {
            return Ok(());
        }
        let resolved = match resolve_mdns_in_candidate(candidate_str).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[display/webrtc] mdns resolve failed: {e}, dropping candidate");
                return Ok(());
            }
        };
        self.command_tx
            .send(Command::AddIceCandidate(resolved))
            .await
            .map_err(|_| CallerError::WebRtc("driver gone".to_string()))?;
        Ok(())
    }

    /// Gracefully close this peer.
    pub async fn close(&self) {
        if let Some(source) = self.interactive_source.as_ref() {
            source.invalidate("the WebRTC display peer closed");
        }
        self.shutdown.cancel();
        // Driver exits on the next select! iteration; channels close on drop.
    }

    /// Resolves once this peer has begun teardown — the driver cancels the
    /// token on exit (ICE failure, drain error), `close()` cancels it for
    /// external teardown, and the pool intake task cancels it when the peer
    /// can no longer be served. The session's per-peer reaper keys on this
    /// to deregister the peer from the session maps.
    pub fn closed(&self) -> tokio_util::sync::WaitForCancellationFutureOwned {
        self.shutdown.clone().cancelled_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit F8 regression guard: a blocked/unreachable STUN server must
    /// NOT delay answer creation. Drives `build_with_codec_set` with a STUN
    /// URL pointing at a real bound-but-SILENT local UDP socket (it accepts
    /// the Binding Request but never replies — the same modelling
    /// `stun_binding_times_out_against_silent_server` uses, robust across
    /// OSes that would otherwise fast-fail an unroutable send) and asserts
    /// the answer is produced far inside `STUN_BINDING_TIMEOUT`. The srflx
    /// gather now runs in the spawned driver, off the critical path, so the
    /// answer no longer waits on the 1.5s STUN timeout. Under the old
    /// blocking code this socket forces the full timeout, so the assertion
    /// fails loudly if blocking ever returns to the answer path. The answer
    /// still advertises the host candidate inline.
    #[tokio::test]
    async fn build_with_codec_set_answer_not_blocked_by_unreachable_stun() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_video_offer_for_rtc();
        let active_rids = vec![SimulcastRid::full()];
        // Bound-but-silent UDP socket: accepts the request, never answers.
        // Held for the duration so the OS keeps the port reserved.
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();
        // Literal-IP STUN URL (no DNS on the path either).
        let ice_config = IceConfig {
            ice_servers: vec![crate::IceServer {
                urls: vec![format!("stun:{}:{}", silent_addr.ip(), silent_addr.port())],
                username: None,
                credential: None,
            }],
        };
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_authorized = crate::BrowserInputAuthorization::new(Arc::new(|| true));
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let started = std::time::Instant::now();
        let (peer, _frame_tx, answer_sdp) = WebRtcPeer::build_with_codec_set(
            7,
            &offer_sdp,
            CodecKind::Vp8,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            None,
            clipboard_handler,
            clipboard_authorized,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("answer must be produced despite unreachable STUN");
        let elapsed = started.elapsed();

        // The whole point of F8: well under the STUN binding timeout. A
        // generous fraction (half) leaves headroom for slow CI while still
        // failing loudly if the blocking gather ever returns to the path.
        assert!(
            elapsed < STUN_BINDING_TIMEOUT / 2,
            "answer creation blocked on STUN ({elapsed:?} >= {:?}/2)",
            STUN_BINDING_TIMEOUT
        );
        // Host (UDP) candidate is still advertised inline in the answer.
        assert!(
            answer_sdp.contains("typ host"),
            "answer advertises host candidate(s): {answer_sdp}"
        );
        // srflx is gathered off-path in the driver and would be trickled
        // via `ice_tx` only if reachable — it must NOT appear inline.
        assert!(
            !answer_sdp.contains("typ srflx"),
            "srflx is trickled, not emitted inline in the answer: {answer_sdp}"
        );
        peer.close().await;
    }

    #[test]
    fn first_video_mid_from_offer_ignores_non_video_m_lines() {
        let offer = "v=0\r\n\
                     m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                     a=mid:data\r\n\
                     m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                     a=mid:screen\r\n";
        assert_eq!(first_video_mid_from_offer(offer).as_deref(), Some("screen"));
    }

    #[test]
    fn first_video_mid_from_offer_returns_none_when_absent() {
        let offer = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:audio\r\n";
        assert_eq!(first_video_mid_from_offer(offer), None);
    }

    /// `offer_is_federated` is the discriminator that flips
    /// `PeerCodecPreferences::federated`. A local `DisplaySlot` offer
    /// carries `a=simulcast:recv` (default `f`, opt-in `f;h;q`) → not
    /// federated; a `PeerDisplayConnection` offer omits it → federated.
    #[test]
    fn offer_is_federated_distinguishes_local_from_federated() {
        let local_offer = "v=0\r\n\
                           m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                           a=mid:0\r\n\
                           a=rtpmap:96 VP8/90000\r\n\
                           a=simulcast:recv f;h;q\r\n\
                           a=recvonly\r\n";
        assert!(
            !offer_is_federated(local_offer),
            "offer with a=simulcast:recv is a local DisplaySlot, not federated"
        );

        let federated_offer = "v=0\r\n\
                              m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                              a=mid:0\r\n\
                              a=rtpmap:96 VP8/90000\r\n\
                              a=recvonly\r\n";
        assert!(
            offer_is_federated(federated_offer),
            "offer without a=simulcast:recv is a federated PeerDisplayConnection"
        );
    }

    /// rtc 0.9 uses rustls 0.23, which requires a process-level
    /// `CryptoProvider`. Production code paths that build an Rtc
    /// transitively need it; the test fixtures call this at the top
    /// of every test that constructs a real `RTCPeerConnection`.
    /// Idempotent — `install_default` returns Err on second call,
    /// which we discard.
    fn ensure_rustls_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    /// **Phase 4c**: synthetic recvonly VP8 offer in the shape rtc 0.9
    /// requires (`a=fingerprint`, `a=ice-ufrag`/`pwd`, `a=setup`,
    /// `a=rtpmap`). Used by the build_with_codec_set integration
    /// tests below to drive `RTCPeerConnection::create_answer`
    /// without standing up a real browser.
    ///
    /// **Phase 4c follow-up (b)**: includes the opt-in recv-side simulcast
    /// hint (`a=rid:f/h/q recv` + `a=simulcast:recv f;h;q`) plus the
    /// repaired-rtp-stream-id extmap so the answer-side multi-RID path is
    /// exercised by the test the same way the browser side exercises it when
    /// `DISPLAY_SIMULCAST_RIDS` is switched to `['f','h','q']`.
    /// Without the offer's `recv` advertisement, rtc 0.9 omits
    /// `a=simulcast:send` from the answer regardless of how many
    /// encodings the track has — see the test's panic message for
    /// the full reasoning.
    fn synth_recvonly_video_offer_for_rtc() -> String {
        concat!(
            "v=0\r\n",
            "o=- 1 2 IN IP4 0.0.0.0\r\n",
            "s=-\r\n",
            "t=0 0\r\n",
            "a=group:BUNDLE 0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "c=IN IP4 0.0.0.0\r\n",
            "a=mid:0\r\n",
            "a=recvonly\r\n",
            "a=rtcp-mux\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\n",
            "a=extmap:2 urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id\r\n",
            "a=extmap:3 urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id\r\n",
            "a=rid:f recv\r\n",
            "a=rid:h recv\r\n",
            "a=rid:q recv\r\n",
            "a=simulcast:recv f;h;q\r\n",
            "a=ice-ufrag:testufrag1234\r\n",
            "a=ice-pwd:testpassword12345678901234\r\n",
            "a=fingerprint:sha-256 ",
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:",
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n",
            "a=setup:actpass\r\n",
        )
        .to_string()
    }

    /// Same as `synth_recvonly_video_offer_for_rtc` but advertising
    /// H.264 only (Constrained Baseline, packetization-mode=1).
    /// Used by the H.264-only answer test to verify no simulcast
    /// lines appear when active_rids has length 1.
    fn synth_recvonly_h264_video_offer_for_rtc() -> String {
        concat!(
            "v=0\r\n",
            "o=- 1 2 IN IP4 0.0.0.0\r\n",
            "s=-\r\n",
            "t=0 0\r\n",
            "a=group:BUNDLE 0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "c=IN IP4 0.0.0.0\r\n",
            "a=mid:0\r\n",
            "a=recvonly\r\n",
            "a=rtcp-mux\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e01f;packetization-mode=1;",
            "level-asymmetry-allowed=1\r\n",
            "a=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\n",
            "a=ice-ufrag:testufrag1234\r\n",
            "a=ice-pwd:testpassword12345678901234\r\n",
            "a=fingerprint:sha-256 ",
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:",
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n",
            "a=setup:actpass\r\n",
        )
        .to_string()
    }

    /// **Phase 4c**: a multi-encoding track (one encoding per
    /// `active_rids` entry) emits `a=simulcast:send` plus per-rid
    /// `a=rid:<rid> send` lines in the answer SDP. This is the wire-
    /// level contract that browser-visible simulcast depends on —
    /// if these lines are missing, the browser sees a single-stream
    /// answer regardless of how many encodings the track was built
    /// with, and the multi-RID forwarder's frames for non-advertised
    /// rids are silently dropped at the wire.
    ///
    /// Pin: VP8 with active_rids=[full, half, quarter] yields an
    /// answer containing `a=simulcast:send full;half;quarter` and
    /// matching `a=rid:* send` lines for each rid.
    ///
    /// This test exercises `build_with_codec_set` end-to-end (the
    /// only way to verify the rtc-side answer SDP shape), but
    /// abandons the spawned driver task by dropping the returned
    /// peer at scope-end. The driver self-terminates on shutdown
    /// signal (peer Drop fires shutdown.cancel()).
    #[tokio::test]
    async fn build_with_codec_set_emits_simulcast_send_for_multi_rid_vp8() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_video_offer_for_rtc();
        let active_rids = vec![
            SimulcastRid::full(),
            SimulcastRid::half(),
            SimulcastRid::quarter(),
        ];
        let ice_config = crate::IceConfig::default();
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_authorized = crate::BrowserInputAuthorization::new(Arc::new(|| true));
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let (peer, _frame_tx, answer_sdp) = WebRtcPeer::build_with_codec_set(
            42,
            &offer_sdp,
            CodecKind::Vp8,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            None,
            clipboard_handler,
            clipboard_authorized,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("build_with_codec_set must succeed for VP8 multi-rid");

        // Summary line with all three rids in preference order
        // (f / h / q — LiveKit / mediasoup convention, see RID_FULL
        // etc. in pool.rs). The order matches the `active_rids`
        // slice order; rtc emits them in track-encoding order.
        //
        // After the sanitized-wire-only shim landed in
        // `build_with_codec_set` (sanitize_answer_sdp called on the
        // wire bytes after set_local_description), these assertions
        // pin the exact shape of the WIRE answer that ships to the
        // browser:
        //
        //   - exactly ONE `a=simulcast:send f;h;q` line — not two
        //     (rtc 0.9's doubled-direction emission was sanitized);
        //   - exactly ONE `a=rid:<rid> send` line per active rid —
        //     not two (rtc 0.9's per-RID duplication was sanitized);
        //   - no `send f;h;q send` substring (the sentinel pattern
        //     of the rtc-0.9 SDP-writer bug);
        //   - the wire answer parses as a valid RTCSessionDescription
        //     of type `answer` — the parse-check that proves WebKit's
        //     parser would accept it.
        //
        // Test-gap discipline: the operator caught earlier that
        // `assert!(answer_sdp.contains("a=simulcast:send f;h;q"))`
        // happily passes against the malformed
        // `a=simulcast:send f;h;q send f;h;q`. Exact-count assertions
        // + explicit substring negation + parse-check are the
        // discipline that catches that class of bug.
        assert_eq!(
            answer_sdp.matches("a=simulcast:send f;h;q").count(),
            1,
            "wire answer must contain exactly ONE \
             `a=simulcast:send f;h;q` line; got:\n{answer_sdp}"
        );
        assert!(
            !answer_sdp.contains("send f;h;q send"),
            "wire answer must NOT contain the rtc-0.9 doubled-direction \
             sentinel `send f;h;q send`; got:\n{answer_sdp}"
        );
        for rid in [
            SimulcastRid::full(),
            SimulcastRid::half(),
            SimulcastRid::quarter(),
        ] {
            let line = format!("a=rid:{} send", rid.as_str());
            assert_eq!(
                answer_sdp.matches(&line).count(),
                1,
                "wire answer must contain exactly ONE `{line}` line \
                 (rtc 0.9 emits two; sanitize_answer_sdp dedupes); \
                 got:\n{answer_sdp}"
            );
        }
        // Sanity: NO recv direction (we're sendonly answerer).
        assert!(
            !answer_sdp.contains("a=simulcast:recv"),
            "answer must NOT contain a=simulcast:recv (we're sendonly); \
             got:\n{answer_sdp}"
        );
        // Parse-check: the sanitized wire answer must be acceptable
        // to rtc's own SDP parser as a type-`answer` description.
        // This is the strongest available proxy in pure-Rust tests
        // for "WebKit's parser would accept it" — both consume RFC
        // 8853-conformant simulcast.
        RTCSessionDescription::answer(answer_sdp.clone()).expect(
            "sanitized wire answer must parse as a valid \
             RTCSessionDescription of type `answer`",
        );

        // Clean up the spawned driver. Dropping `peer` cancels its
        // shutdown token, the driver task exits on the next select.
        drop(peer);
    }

    /// The answerer's DTLS role MUST be `passive` so the browser
    /// becomes the DTLS client and initiates the handshake. With
    /// the role left to default to `active`, the rtc 0.9 stack
    /// signals `a=setup:active` but never actually emits a
    /// ClientHello over the selected ICE-TCP candidate — the session
    /// stalls at STUN keepalives forever and the dashboard renders
    /// black. Diagnosed across four hops in #41 (RFC 7983 byte-class
    /// instrumentation showed Stun-only in every direction) and
    /// fixed by `setting_engine.set_answering_dtls_role(Server)`
    /// in `build_with_codec_set`. This test pins both the
    /// affirmative (passive present) and the negative (active
    /// absent) so a future refactor that drops the role assignment
    /// re-introduces the regression loudly instead of silently.
    #[tokio::test]
    async fn build_with_codec_set_pins_setup_passive_in_answer() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_video_offer_for_rtc();
        let active_rids = vec![SimulcastRid::full()];
        let ice_config = crate::IceConfig::default();
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_authorized = crate::BrowserInputAuthorization::new(Arc::new(|| true));
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let (peer, _frame_tx, answer_sdp) = WebRtcPeer::build_with_codec_set(
            42,
            &offer_sdp,
            CodecKind::Vp8,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            None,
            clipboard_handler,
            clipboard_authorized,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("build_with_codec_set must succeed for the role-pin test");

        assert!(
            answer_sdp.contains("a=setup:passive"),
            "answer must contain `a=setup:passive` so the browser becomes \
             the DTLS client and initiates the handshake; got:\n{answer_sdp}"
        );
        assert!(
            !answer_sdp.contains("a=setup:active"),
            "answer must NOT contain `a=setup:active` — that role left \
             rtc's DTLS state machine waiting forever over ICE-TCP \
             (diagnosed in #41 / fixed in #42); got:\n{answer_sdp}"
        );

        drop(peer);
    }

    /// **Phase 4c**: a single-encoding track (active_rids has length
    /// 1) emits NO simulcast lines in the answer. This pins the
    /// fall-through path for H.264 (single-layer by design — see
    /// `LayerSpec::single`'s rationale) and for VP8 cases where all
    /// simulcast layers but full dropped below MIN_LAYER_DIM.
    ///
    /// If this test fires, the unconditional simulcast emission
    /// regressed — every peer would advertise simulcast even when
    /// only one encoding exists, and browsers would request
    /// keyframes for rids the encoder pool can't serve.
    #[tokio::test]
    async fn build_with_codec_set_emits_no_simulcast_for_single_rid_h264() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_h264_video_offer_for_rtc();
        // Single rid → no simulcast in answer.
        let active_rids = vec![SimulcastRid::full()];
        let ice_config = crate::IceConfig::default();
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_authorized = crate::BrowserInputAuthorization::new(Arc::new(|| true));
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let (peer, _frame_tx, answer_sdp) = WebRtcPeer::build_with_codec_set(
            43,
            &offer_sdp,
            CodecKind::H264,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            None,
            clipboard_handler,
            clipboard_authorized,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("build_with_codec_set must succeed for H.264 single-rid");

        assert!(
            !answer_sdp.contains("a=simulcast:"),
            "single-encoding track must NOT advertise simulcast; \
             got:\n{answer_sdp}"
        );
        assert!(
            !answer_sdp.contains("a=rid:"),
            "single-encoding track must NOT advertise per-rid lines; \
             got:\n{answer_sdp}"
        );

        drop(peer);
    }

    /// **Phase 4d.1**: a freshly-constructed `WebRtcPeer` exposes an
    /// observed-send-bitrate signal that starts at `None` (the watch
    /// channel's initial value). The driver's first poll seeds the
    /// per-SSRC `prev` map and publishes nothing; subsequent polls
    /// publish a delta. With no RTP traffic in this test (no real
    /// ICE flow, no media writes), `bytes_sent` stays at 0 and the
    /// helper produces `None` indefinitely — so the steady state
    /// here is `None`.
    ///
    /// Pin both APIs:
    /// - `current_observed_send_bitrate()` for one-shot reads.
    /// - `subscribe_observed_send_bitrate()` for change-driven consumers.
    #[tokio::test]
    async fn web_rtc_peer_exposes_observed_send_bitrate_api_starting_at_none() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_video_offer_for_rtc();
        let active_rids = vec![SimulcastRid::full()];
        let ice_config = IceConfig::default();
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_authorized = crate::BrowserInputAuthorization::new(Arc::new(|| true));
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let (peer, _frame_tx, _answer_sdp) = WebRtcPeer::build_with_codec_set(
            44,
            &offer_sdp,
            CodecKind::Vp8,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            None,
            clipboard_handler,
            clipboard_authorized,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("build_with_codec_set must succeed");

        // One-shot read: None initially.
        assert_eq!(
            peer.current_observed_send_bitrate(),
            None,
            "freshly-constructed peer's observed send bitrate must \
             be None until the driver computes a `bytes_sent` delta"
        );

        // Subscriber: initial `borrow` returns None too. `borrow()`
        // reads the current value without marking it as "seen" —
        // identical semantics to `current_observed_send_bitrate()`
        // for the initial state.
        let rx = peer.subscribe_observed_send_bitrate();
        assert_eq!(
            *rx.borrow(),
            None,
            "fresh subscriber must observe None as the initial value"
        );

        // Independent receivers: a second subscribe yields a separate
        // Receiver; mutations on one (in this case, `borrow_and_update`
        // which marks the current value as seen) don't affect the other.
        let mut rx2 = peer.subscribe_observed_send_bitrate();
        assert_eq!(*rx2.borrow_and_update(), None);
        // The first receiver still sees None — independent state.
        assert_eq!(*rx.borrow(), None);

        drop(peer);
    }

    /// **Phase 4d.3a**: a freshly-constructed `WebRtcPeer` exposes a
    /// remote-inbound-health watch that starts at the empty map (the
    /// watch channel's initial value). The driver's first poll
    /// publishes whatever's in `report.iter_by_type(RTCStatsType::RemoteInboundRTP)`
    /// at that moment — empty until any RR has arrived.
    ///
    /// In this test there's no real ICE flow, so no RTP is ever sent
    /// and no RR is ever received → the steady state is the empty
    /// map.
    ///
    /// Pin both APIs:
    /// - `current_remote_inbound_health()` for one-shot reads.
    /// - `subscribe_remote_inbound_health()` for change-driven consumers
    ///   (the layer-selection aggregator in 4d.3c).
    #[tokio::test]
    async fn web_rtc_peer_exposes_remote_inbound_health_api_starting_at_empty() {
        ensure_rustls_crypto_provider();
        let offer_sdp = synth_recvonly_video_offer_for_rtc();
        let active_rids = vec![SimulcastRid::full()];
        let ice_config = IceConfig::default();
        let input_handler: Arc<dyn Fn(InputEvent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync> = Arc::new(|_| {});
        let clipboard_authorized = crate::BrowserInputAuthorization::new(Arc::new(|| true));
        let authority_handler = noop_authority_handler();
        let tile_control_handler = noop_tile_control_handler();
        let (ice_tx, _ice_rx) = mpsc::channel::<(PeerId, String)>(8);
        let (kf_tx, _kf_rx) = mpsc::channel::<SimulcastRid>(8);

        let (peer, _frame_tx, _answer_sdp) = WebRtcPeer::build_with_codec_set(
            44,
            &offer_sdp,
            CodecKind::Vp8,
            &active_rids,
            &ice_config,
            None,
            None,
            input_handler,
            None,
            clipboard_handler,
            clipboard_authorized,
            authority_handler,
            tile_control_handler,
            ice_tx,
            kf_tx,
        )
        .await
        .expect("build_with_codec_set must succeed");

        // One-shot read: empty map initially.
        let snapshot = peer.current_remote_inbound_health();
        assert!(
            snapshot.is_empty(),
            "freshly-constructed peer's remote-inbound-health snapshot \
             must be empty until the driver projects a non-empty \
             remote-inbound-rtp set into per-RID health; got {snapshot:?}",
        );

        // Subscriber: initial `borrow` returns empty too. Mirrors
        // `current_remote_inbound_health` for the initial state.
        let rx = peer.subscribe_remote_inbound_health();
        assert!(rx.borrow().is_empty());
        // Independent receivers: a second subscribe returns its own
        // receiver carrying the same initial value.
        let rx2 = peer.subscribe_remote_inbound_health();
        assert!(rx2.borrow().is_empty());

        drop(peer);
    }

    // ----- sanitize_answer_sdp -----------------------------------
    //
    // Pure-helper tests for the rtc 0.9 SDP-writer workaround.
    // See `sanitize_answer_sdp` doc-comment for the bugs being
    // addressed. Each test fixes one input/output pair so a future
    // regression that re-introduces duplicate rids or the doubled
    // simulcast direction fires loudly.

    /// rtc 0.9 emits each `a=rid:<rid> send` line twice for multi-RID
    /// send. The sanitizer must dedupe each to exactly one occurrence
    /// while preserving line order (first-seen wins) and untouched
    /// surrounding lines.
    #[test]
    fn sanitize_answer_sdp_dedupes_duplicate_rid_send_lines() {
        let input = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rid:f send\r\n",
            "a=rid:h send\r\n",
            "a=rid:q send\r\n",
            "a=rid:f send\r\n",
            "a=rid:h send\r\n",
            "a=rid:q send\r\n",
            "a=simulcast:send f;h;q\r\n",
        );
        let out = super::sanitize_answer_sdp(input);
        assert_eq!(out.matches("a=rid:f send").count(), 1, "got:\n{out}");
        assert_eq!(out.matches("a=rid:h send").count(), 1, "got:\n{out}");
        assert_eq!(out.matches("a=rid:q send").count(), 1, "got:\n{out}");
        // Preserves CRLF line endings.
        assert!(out.contains("\r\n"), "must preserve CRLF; got:\n{out}");
        // Surrounding lines untouched.
        assert!(out.contains("a=rtpmap:96 VP8/90000"));
        assert!(out.contains("a=simulcast:send f;h;q"));
    }

    /// `a=simulcast:send f;h;q send f;h;q` (rtc 0.9 doubled-direction
    /// bug) must collapse to `a=simulcast:send f;h;q`. The substring
    /// `send f;h;q send` is the regression marker.
    #[test]
    fn sanitize_answer_sdp_collapses_doubled_simulcast_send_direction() {
        let input = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=simulcast:send f;h;q send f;h;q\r\n",
        );
        let out = super::sanitize_answer_sdp(input);
        assert!(
            out.contains("a=simulcast:send f;h;q"),
            "must contain a=simulcast:send f;h;q; got:\n{out}"
        );
        assert!(
            !out.contains("send f;h;q send"),
            "must NOT contain doubled-direction substring `send f;h;q \
             send`; got:\n{out}"
        );
        // Exactly one a=simulcast: line in the output.
        let count = out
            .lines()
            .filter(|l| l.starts_with("a=simulcast:"))
            .count();
        assert_eq!(
            count, 1,
            "exactly one a=simulcast: line; got {count}\n{out}"
        );
    }

    /// Already-clean SDP must pass through unchanged. Dedupe is
    /// idempotent: re-applying the sanitizer to its own output is a
    /// no-op.
    #[test]
    fn sanitize_answer_sdp_already_clean_unchanged() {
        let input = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rid:f send\r\n",
            "a=rid:h send\r\n",
            "a=rid:q send\r\n",
            "a=simulcast:send f;h;q\r\n",
        );
        let out = super::sanitize_answer_sdp(input);
        assert_eq!(out, input, "clean input must pass through unchanged");
        // Idempotent: sanitize(sanitize(x)) == sanitize(x).
        let twice = super::sanitize_answer_sdp(&out);
        assert_eq!(twice, out, "sanitizer must be idempotent");
    }

    /// H.264 / single-RID answers (no `a=rid:` or `a=simulcast:`
    /// lines at all — the federated peer-display path post-#46 fix
    /// and any single-encoding answer) must pass through untouched.
    #[test]
    fn sanitize_answer_sdp_single_rid_no_simulcast_unchanged() {
        let input = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 98\r\n",
            "a=rtpmap:98 H264/90000\r\n",
            "a=fmtp:98 profile-level-id=42e01f;packetization-mode=1\r\n",
            "a=ssrc:2616664936 cname:display-1\r\n",
        );
        let out = super::sanitize_answer_sdp(input);
        assert_eq!(out, input);
    }

    /// Bidirectional simulcast (`send f;h;q recv x`) is valid per
    /// RFC 8853 — the second pair has a different direction. The
    /// sanitizer must NOT collapse it. Distinguishes the bug shape
    /// (same direction twice) from valid bidirectional shape.
    #[test]
    fn sanitize_answer_sdp_preserves_valid_bidirectional_simulcast() {
        let input = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=simulcast:send f;h;q recv x\r\n",
        );
        let out = super::sanitize_answer_sdp(input);
        assert!(
            out.contains("a=simulcast:send f;h;q recv x"),
            "valid bidirectional simulcast must pass through; got:\n{out}"
        );
    }
}

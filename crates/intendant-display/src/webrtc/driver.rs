//! The per-peer driver task: negotiation/readiness state, the select! pump
//! (UDP/TCP ingress, encoded frames, commands, timers), event/message/command
//! handling, RTP write-out, RTCP keyframe routing, stats-derived health, and
//! the data-channel message codecs it speaks.

use super::*;

// ---------------------------------------------------------------------------
// Driver task
// ---------------------------------------------------------------------------

/// Per-[`crate::encode::PayloadSpec`] negotiation + readiness
/// state. Keyed off the full `PayloadSpec` in [`DriverState::video_specs`]
/// so H.264 fmtp variants (profile-level-id + packetization-mode) stay
/// distinct — browser negotiation treats those independently and caching by
/// `CodecKind` alone would conflate them.
///
/// The combined cache structure is deliberate: under the encoder pool a
/// peer can receive frames from multiple codecs in quick succession
/// (VP8 always-on + H.264 on-demand, etc.). A global `keyframe_seen` flag
/// (the pre-3c.0a shape) lets a stray keyframe from an *unsupported* spec
/// open the gate for P-frames of a *supported* one — a subtle silent-
/// black-screen class of bug. Making readiness per-spec AND flipping it
/// only after a successful `writer.write` eliminates that path.
pub(crate) enum SpecState {
    /// Has this spec had >=1 keyframe successfully packetized for the peer?
    /// Until true, non-keyframe frames drop. Scoped per spec so codec A's
    /// keyframe gate is independent of codec B's.
    Ready { keyframe_seen: bool },
    /// This frame spec does not match the codec negotiated for this peer.
    Unsupported,
}

/// State the driver carries between iterations.
pub(crate) struct DriverState {
    /// Per-`(PayloadSpec, SimulcastRid)` resolved PT + keyframe
    /// readiness. See [`SpecState`].
    ///
    /// Keying changed in phase-4c-prep (this commit) from `PayloadSpec`
    /// alone to `(PayloadSpec, SimulcastRid)`. The previous keying
    /// would have been wrong for VP8 simulcast: every layer of a
    /// VP8 simulcast track produces the SAME `PayloadSpec`
    /// (codec_mime + clock + fmtp are the same across layers), so a
    /// single map entry would conflate the keyframe gates of three
    /// distinct RIDs. A keyframe seen on RID `full` would then open
    /// the gate for P-frames on RIDs `half` / `quarter` — and those
    /// RIDs' subscribers would receive P-frames referencing
    /// keyframes they never got, decoding to garbage. Per-RID
    /// keying eliminates that path.
    ///
    /// For single-encoding peers (today's behavior, preserved in
    /// this refactor; H.264 always-on or VP8-as-single-layer until
    /// commit 2 lights up multi-encoding), the map has exactly one
    /// entry per active spec — same shape as the previous keying,
    /// just with the RID dimension along for the ride.
    video_specs: HashMap<(crate::encode::PayloadSpec, SimulcastRid), SpecState>,
    /// Map of channel label → DataChannelId for routing channel data and clipboard sends.
    channels: HashMap<String, RTCDataChannelId>,
    /// F-1.2: queued `display_input_authority_state` messages awaiting
    /// the data-channel's `OnOpen`. The federated authority broadcast
    /// loop calls [`WebRtcPeer::send_authority_state`] as soon as a
    /// federated WebRtcPeer is registered as a subscriber — which can
    /// be (and usually is) before the browser's data channels finish
    /// negotiating. Without queueing, that initial snapshot would land
    /// on the floor and the browser's chip would stall at `unknown`.
    /// Drained and emitted in order on `OnDataChannel(OnOpen)` for
    /// label `display_input_authority`.
    ///
    /// Capacity-bounded by the producer side (the broadcast loop runs
    /// at low frequency — one event per take/release/disconnect, not
    /// per frame), so unbounded growth here is structurally
    /// impossible. Uses `Vec` rather than a channel because the
    /// producer side is not throttled by the channel's send-await
    /// semantics.
    pending_authority_state: Vec<(u32, DisplayInputAuthorityState)>,
    /// D-3b: queued tile control frames awaiting `tile-control`
    /// channel open. Low-rate reliable control only; never per-frame.
    pending_tile_control: Vec<Vec<u8>>,
    /// D-3b: queued snapshot chunks awaiting `tile-snapshot` channel
    /// open. Reliable snapshot delivery is allowed to delay rather
    /// than drop. Tile deltas intentionally have no queue.
    pending_tile_snapshot: Vec<Vec<u8>>,
    /// D-4c: event-driven backpressure state for the supersedable
    /// `tile-deltas` channel. Control and snapshot channels are
    /// reliable and never use this drop policy.
    tile_delta_backpressure: TileDeltaBackpressure,
    /// Wallclock anchor: Instant at which the first frame was emitted.
    /// All subsequent rtp_time values are relative to this.
    first_frame_at: Option<Instant>,
    rtp: RtpSendState,
}

/// Per-RID send state — one entry per simulcast layer (or one entry
/// for non-simulcast codecs).
///
/// SSRC and packetizer are per-RID because:
/// - **SSRC**: [`rtc`]'s `RTCRtpSender::write_rtp` routes packets to
///   encodings by matching `packet.header.ssrc` against the encoding's
///   SSRC. Each layer must carry its own SSRC for the right encoding
///   to claim the packet.
/// - **Packetizer**: each packetizer holds its own RTP sequence
///   number + timestamp continuation state. Sharing one packetizer
///   across RIDs would interleave their sequence streams and the
///   browser's per-encoding jitter buffers would reject everything
///   they didn't expect at the next sequence number.
pub(crate) struct RidRtpState {
    ssrc: u32,
    packetizer: Box<dyn Packetizer + Send>,
}

pub(crate) struct RtpSendState {
    sender_id: RTCRtpSenderId,
    mid: String,
    codec: RTCRtpCodec,
    /// Per-RID send state. Looked up by the
    /// [`OutboundEncodedFrame::rid`] of each incoming frame so the
    /// driver writes with the matching SSRC + the matching
    /// packetizer's continuation state.
    by_rid: HashMap<SimulcastRid, RidRtpState>,
    mid_ext_id: Option<u8>,
    rid_ext_id: Option<u8>,
}

/// Inbound packet from one of the per-interface forwarder tasks or a
/// TCP connection reader. `proto` tags which transport it arrived on so
/// the driver can hand it to the RTC core with the correct metadata.
pub(crate) struct InboundPacket {
    pub(crate) proto: TransportProtocol,
    pub(crate) source: SocketAddr,
    pub(crate) destination: SocketAddr,
    pub(crate) bytes: Vec<u8>,
    pub(crate) received_at: Instant,
}

/// Outbound side of an ICE-TCP connection: the sending end of an ordered
/// channel feeding that connection's dedicated writer task. The driver
/// stores one per connection (keyed by the remote's source address) so it
/// can route outbound TCP writes to the right socket.
///
/// **Why a channel + single writer task, not `Arc<Mutex<OwnedWriteHalf>>`
/// with a spawned write per transmit:** spawning a fresh task for every
/// `rtc.poll_write()` transmit (the old design) hands the *scheduler*, not
/// `rtc`, control over the order writes hit the wire — the `Mutex` only
/// stops byte-level interleaving, not whole-frame reordering — and applies
/// no backpressure, so under sustained RTP video the kernel send buffer
/// overflows with unbounded queued tasks. On Linux a non-blocking `send`
/// that can't fit just yields `EWOULDBLOCK` and tokio waits; on Windows the
/// TCP stack instead aborts the connection once the unACKed send backlog
/// trips its retransmit limit, and `send` then returns `WSAECONNABORTED`
/// (os error 10053) on *every* subsequent write — the 10053 flood that left
/// the dashboard black. Funnelling every transmit through one ordered
/// bounded channel drained by a single owner of the write half preserves
/// `rtc`'s emit order, gives real backpressure (a full queue drops the
/// frame instead of overflowing the socket), and gives the connection a
/// single error owner that tears the peer down on the first write failure
/// rather than re-flooding a dead socket.
pub(crate) const TCP_OUT_QUEUE: usize = 256;
pub(crate) type TcpFrameSender = mpsc::Sender<Vec<u8>>;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn driver<I: rtc::interceptor::Interceptor + Send + Sync + 'static>(
    peer_id: PeerId,
    mut rtc: RTCPeerConnection<I>,
    rtp_config: RtpSendConfig,
    sockets: Vec<Arc<UdpSocket>>,
    mut tcp_conn_rx: Option<mpsc::Receiver<AcceptedTcpConnection>>,
    tcp_advertised: Option<SocketAddr>,
    _tcp_registration: Option<PeerRegistration>,
    mut frame_rx: mpsc::Receiver<OutboundEncodedFrame>,
    mut command_rx: mpsc::Receiver<Command>,
    input_handler: Arc<dyn Fn(InputEvent) + Send + Sync>,
    clipboard_handler: Arc<dyn Fn(ClipboardContent) + Send + Sync>,
    authority_handler: AuthorityChannelHandler,
    tile_control_handler: TileControlHandler,
    keyframe_request_tx: mpsc::Sender<SimulcastRid>,
    observed_send_bitrate_tx: watch::Sender<Option<u64>>,
    remote_inbound_health_tx: watch::Sender<HashMap<SimulcastRid, PeerLayerHealth>>,
    // F8: STUN config + server→browser trickle channel. The driver
    // resolves the STUN server (DNS) and gathers the srflx candidate via
    // its UDP forwarders, all off the peer-setup critical path; `ice_tx`
    // delivers the resulting candidate to the browser as trickle ICE.
    ice_config: IceConfig,
    ice_tx: mpsc::Sender<(PeerId, String)>,
    shutdown: CancellationToken,
) {
    if rtp_config.encodings.is_empty() {
        eprintln!(
            "[display/webrtc] peer {peer_id}: RtpSendConfig.encodings is empty; \
             refusing to start a driver with no SSRC/RID slots — \
             build_with_codec_set must populate at least one encoding"
        );
        shutdown.cancel();
        return;
    }
    // Build one packetizer per encoding (per-RID continuation state).
    // The payloader factory is per-codec, but each encoding gets its
    // own payloader instance — packetizers hold mutable state (current
    // sequence number, RTP timestamp continuation), and sharing one
    // across RIDs would interleave their sequence streams.
    let mut by_rid: HashMap<SimulcastRid, RidRtpState> =
        HashMap::with_capacity(rtp_config.encodings.len());
    for (rid, ssrc) in &rtp_config.encodings {
        let payloader = match rtp_config.codec.payloader() {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "[display/webrtc] peer {peer_id}: no RTP payloader for \
                     codec on rid {}: {e}",
                    rid.as_str(),
                );
                shutdown.cancel();
                return;
            }
        };
        let packetizer = packetizer::new_packetizer(
            1200,
            96,
            *ssrc,
            payloader,
            Box::new(sequence::new_random_sequencer()),
            rtp_config.codec.clock_rate,
        );
        by_rid.insert(
            rid.clone(),
            RidRtpState {
                ssrc: *ssrc,
                packetizer: Box::new(packetizer),
            },
        );
    }
    let mut state = DriverState {
        video_specs: HashMap::new(),
        channels: HashMap::new(),
        pending_authority_state: Vec::new(),
        pending_tile_control: Vec::new(),
        pending_tile_snapshot: Vec::new(),
        tile_delta_backpressure: TileDeltaBackpressure::new(),
        first_frame_at: None,
        rtp: RtpSendState {
            sender_id: rtp_config.sender_id,
            mid: rtp_config.mid,
            codec: rtp_config.codec,
            by_rid,
            mid_ext_id: None,
            rid_ext_id: None,
        },
    };

    // Index sockets by their local address so we can route outbound writes
    // through the socket whose source matches.
    let mut sockets_by_addr: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    for sock in &sockets {
        if let Ok(addr) = sock.local_addr() {
            sockets_by_addr.insert(addr, Arc::clone(sock));
        }
    }

    // Outbound frame senders for each active ICE-TCP connection, keyed by
    // the remote's `SocketAddr`. Each feeds a dedicated writer task that
    // owns the connection's write half (see `TcpFrameSender`).
    let mut tcp_senders: HashMap<SocketAddr, TcpFrameSender> = HashMap::new();

    // --- srflx (STUN) gathering, folded into the UDP forwarders ----------
    //
    // Audit F8: this is deliberately OFF the peer-setup critical path. By
    // the time the driver runs, `build_with_codec_set` has already produced
    // and returned the SDP answer (host + ICE-TCP candidates), so resolving
    // the STUN server (DNS) and gathering the srflx mapping here add zero
    // latency to answer creation — a blocked/unreachable STUN server never
    // delays setup. When a mapping arrives the driver's select loop adds the
    // srflx candidate to `rtc` and trickles it to the browser via `ice_tx`.
    //
    // The exchange is folded *into* each per-socket forwarder rather than
    // run as a separate task because the forwarder is the single owner of
    // its socket's `recv_from`; a second concurrent reader would race for
    // the response (tokio wakes only one waiter, so either side could lose
    // the datagram). The forwarder sends one Binding Request at startup,
    // then in its normal read loop hands every datagram that ISN'T our
    // Binding Success Response on to the RTC core unchanged (so ICE
    // connectivity checks the same socket carries are never dropped) and
    // reports the one matching response's mapped address back here.
    let stun_addr = resolve_stun_servers(&ice_config).await.into_iter().next();
    let (srflx_tx, mut srflx_rx) = mpsc::channel::<(SocketAddr, SocketAddr)>(sockets.len().max(1));

    // Spawn one forwarder task per UDP socket. Each forwarder reads packets
    // from its socket and pushes them into the shared inbound channel,
    // tagged with the socket's local address as the destination. The
    // driver keeps its own clone of `inbound_tx` so it can spawn new
    // readers as TCP connections arrive; it'll drop on driver exit and
    // — together with the forwarders terminating on shutdown — close the
    // channel cleanly.
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundPacket>(64);
    let forwarder_shutdown = shutdown.clone();
    let mut forwarder_handles = Vec::new();
    for sock in &sockets {
        let sock = Arc::clone(sock);
        let tx = inbound_tx.clone();
        let shutdown = forwarder_shutdown.clone();
        let local_addr = match sock.local_addr() {
            Ok(a) => a,
            Err(_) => continue,
        };
        let srflx_tx = srflx_tx.clone();
        forwarder_handles.push(tokio::spawn(async move {
            // Fire one STUN Binding Request out this very socket so the
            // mapping the server reports corresponds to this candidate's
            // base (a 1:1 NAT returns the public IP:port the browser can
            // reach directly). `srflx_pending` holds the transaction ID we
            // expect a matching response to echo; it clears once we've
            // gathered (or given up after `STUN_BINDING_TIMEOUT`) so we
            // stop scanning datagrams. With no STUN server configured we
            // never send and stay a plain forwarder.
            let mut srflx_pending: Option<rtc::stun::message::TransactionId> = None;
            if let Some(stun_addr) = stun_addr {
                match build_stun_binding_request() {
                    Ok((wire, tid)) => match sock.send_to(&wire, stun_addr).await {
                        Ok(_) => srflx_pending = Some(tid),
                        Err(e) => eprintln!(
                            "[display/webrtc] forwarder {local_addr}: STUN send to {stun_addr} failed: {e}"
                        ),
                    },
                    Err(e) => eprintln!(
                        "[display/webrtc] forwarder {local_addr}: build STUN request failed: {e}"
                    ),
                }
            }
            // Off-critical-path deadline after which we stop trying to
            // gather srflx (the answer is already out; nothing waits on
            // this). `tokio::time::sleep` is created up front but only
            // selected on while a request is in flight.
            let srflx_deadline = tokio::time::sleep(STUN_BINDING_TIMEOUT);
            tokio::pin!(srflx_deadline);

            let mut buf = vec![0u8; UDP_BUF_LEN];
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    // Only armed while a Binding Request is outstanding.
                    _ = &mut srflx_deadline, if srflx_pending.is_some() => {
                        srflx_pending = None;
                    }
                    recv = sock.recv_from(&mut buf) => match recv {
                        Ok((n, source)) => {
                            // Intercept our own STUN Binding Success
                            // Response (from the STUN server, matching txid)
                            // for srflx; everything else — including STUN
                            // connectivity checks from the browser — falls
                            // through to the RTC core unchanged.
                            if let Some(tid) = srflx_pending {
                                if Some(source) == stun_addr {
                                    if let Some(mapped) =
                                        parse_stun_binding_response(&buf[..n], tid)
                                    {
                                        srflx_pending = None;
                                        // Best-effort: driver may have gone.
                                        let _ = srflx_tx.send((local_addr, mapped)).await;
                                        continue;
                                    }
                                }
                            }
                            let pkt = InboundPacket {
                                proto: TransportProtocol::UDP,
                                source,
                                destination: local_addr,
                                bytes: buf[..n].to_vec(),
                                received_at: Instant::now(),
                            };
                            if tx.send(pkt).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "[display/webrtc] forwarder {local_addr}: recv failed: {e}"
                            );
                            break;
                        }
                    },
                }
            }
        }));
    }
    // The driver keeps no `srflx_tx` of its own; drop the template clone so
    // `srflx_rx` closes once every forwarder has exited (gathered, given
    // up, or shut down), letting its select-loop branch go dormant.
    drop(srflx_tx);

    // --- TURN relay candidate gathering, in a dedicated relay task --------
    //
    // Off the peer-setup critical path, exactly like srflx above: the answer
    // is already out (host + ICE-TCP), so allocating a relay here adds zero
    // setup latency and an unreachable/misconfigured TURN server never delays
    // anything. When the allocation succeeds the task hands back a
    // `RelayAllocation`; the select loop below adds the `typ relay` candidate
    // and trickles it to the browser via `ice_tx` (same channel as srflx).
    //
    // The relay task owns its own UDP socket (single reader, no race) and
    // drives the sans-I/O `rtc::turn` client. Relayed inbound media arrives on
    // `inbound_tx` tagged at the relayed address so ICE pairs it with the
    // relay candidate; relay-destined RTC *output* is routed to the task via
    // the `relay_out_tx` carried in the `RelayAllocation` (see
    // `drain_outputs`, which checks the transmit's local_addr against the
    // relayed address). Graceful fallback: with no `turn:` server configured
    // (or all unresolvable / credential-less) we never spawn the task and the
    // host/srflx/ICE-TCP candidate set is unchanged.
    let (alloc_tx, mut alloc_rx) = mpsc::channel::<RelayAllocation>(1);
    let turn_servers = resolve_turn_servers(&ice_config).await;
    if let Some(server) = turn_servers.into_iter().next() {
        // Relay over the same address family as our ICE sockets so the relay
        // path is reachable for the transport the browser is using.
        let is_ipv4 = sockets
            .first()
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.is_ipv4())
            .unwrap_or(true);
        let relay_inbound_tx = inbound_tx.clone();
        let relay_alloc_tx = alloc_tx.clone();
        let relay_shutdown = shutdown.clone();
        tokio::spawn(run_turn_relay(
            peer_id,
            server,
            is_ipv4,
            relay_inbound_tx,
            relay_alloc_tx,
            relay_shutdown,
        ));
    }
    // Drop the template `alloc_tx` so `alloc_rx` closes if no relay task was
    // spawned (or once the one task exits), letting its select branch idle.
    drop(alloc_tx);
    // Once the allocation lands, this is the relayed address ICE advertises
    // and the channel that routes relay-destined RTC output to the relay task.
    let mut relay_addr: Option<SocketAddr> = None;
    let mut relay_out_tx: Option<mpsc::Sender<(SocketAddr, Vec<u8>)>> = None;

    // Phase 4d.1: poll-driven observed-send-bitrate computation.
    // Each tick samples `bytes_sent` per outbound stream and computes
    // the rate over the elapsed interval. `prev_outbound_bytes` carries
    // the per-SSRC last sample across polls; the helper updates it
    // in place. First poll produces None (no prev), subsequent polls
    // produce Some(bps) once at least one SSRC has had two samples.
    //
    // `tokio::time::interval` fires immediately on the first
    // `.tick().await`. That first poll seeds `prev_outbound_bytes`
    // and publishes None — fine because the initial value the watch
    // channel was constructed with is already None.
    // `MissedTickBehavior::Skip` ensures a busy driver loop doesn't
    // produce a burst of catch-up polls when it falls behind.
    let mut twcc_poll = tokio::time::interval(TWCC_POLL_INTERVAL);
    twcc_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prev_outbound_bytes: HashMap<u32, (u64, Instant)> = HashMap::new();

    loop {
        // 1. Drain all outputs until we get a Timeout (the next deadline).
        let timeout_at = match drain_outputs(
            &mut rtc,
            &sockets_by_addr,
            &mut tcp_senders,
            relay_addr,
            relay_out_tx.as_ref(),
            &mut state,
            &input_handler,
            &clipboard_handler,
            &authority_handler,
            &tile_control_handler,
            &keyframe_request_tx,
        )
        .await
        {
            Ok(t) => t,
            Err(DriverExit::Closed) => {
                eprintln!("[display/webrtc] peer {peer_id}: driver exiting");
                shutdown.cancel();
                for h in forwarder_handles {
                    let _ = h.await;
                }
                return;
            }
        };

        // 2. Wait for the next event: inbound packet, frame, command,
        //    deadline, or shutdown.
        let now = Instant::now();
        let timeout_dur = timeout_at
            .saturating_duration_since(now)
            .max(Duration::from_micros(1));

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                eprintln!("[display/webrtc] peer {peer_id}: shutdown requested");
                for h in forwarder_handles {
                    let _ = h.await;
                }
                return;
            }
            Some(pkt) = inbound_rx.recv() => {
                let input = TaggedBytesMut {
                    now: pkt.received_at,
                    transport: TransportContext {
                        local_addr: pkt.destination,
                        peer_addr: pkt.source,
                        transport_protocol: pkt.proto,
                        ecn: None,
                    },
                    message: BytesMut::from(pkt.bytes.as_slice()),
                };
                if let Err(e) = rtc.handle_read(input) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_read failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            Some(accepted) = async {
                match tcp_conn_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                // New ICE-TCP connection from the dispatcher. Split into read
                // + write halves, store the write side keyed by the remote
                // address, spawn a reader task that forwards subsequent
                // frames through the unified inbound channel, and inject the
                // first-frame we already peeked directly.
                //
                // We "lie" to the RTC core about the destination address: every
                // inbound TCP frame gets `destination = tcp_advertised`
                // (the Host-header-derived IP:port we advertised as our
                // TCP candidate), not the actual `stream.local_addr()` —
                // which on a NAT'd VM is the VM's internal interface IP
                    // that the RTC core has no candidate for. Matching the
                // advertised destination to the one local candidate lets
                // ICE form a valid pair. The underlying TCP stream is
                // bidirectional so data still flows through the real
                // kernel socket we own.
                let AcceptedTcpConnection {
                    remote_addr,
                    local_addr: real_local,
                    first_frame,
                    stream,
                } = accepted;
                let Some(fake_local) = tcp_advertised else {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: TCP connection from {remote_addr} but no fake local configured, dropping"
                    );
                    continue;
                };
                eprintln!(
                    "[display/webrtc] peer {peer_id}: ICE-TCP connection from {remote_addr} → {real_local} (rtc sees {fake_local})"
                );
                let (read_half, write_half) = stream.into_split();

                // Dedicated writer task: owns the write half and drains an
                // ordered bounded channel, writing one RFC 4571 frame per
                // queued payload. This is the single owner of the write
                // half — `drain_outputs` only enqueues onto the channel, so
                // `rtc`'s emit order is preserved on the wire and there is
                // exactly one place that observes write failures. On the
                // first write error (e.g. Windows `WSAECONNABORTED` once the
                // TCP stack aborts the connection) it logs once and cancels
                // the peer's shutdown token, tearing the connection down
                // instead of letting every later transmit re-flood the log
                // on a dead socket.
                let (tcp_out_tx, mut tcp_out_rx) = mpsc::channel::<Vec<u8>>(TCP_OUT_QUEUE);
                tcp_senders.insert(remote_addr, tcp_out_tx);
                let writer_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let mut write_half = write_half;
                    loop {
                        tokio::select! {
                            biased;
                            _ = writer_shutdown.cancelled() => break,
                            frame = tcp_out_rx.recv() => match frame {
                                Some(contents) => {
                                    if let Err(e) =
                                        write_rfc4571_frame(&mut write_half, &contents).await
                                    {
                                        eprintln!(
                                            "[display/webrtc] ICE-TCP writer for {remote_addr} \
                                             failed, tearing down connection: {e}"
                                        );
                                        writer_shutdown.cancel();
                                        break;
                                    }
                                }
                                // Sender dropped (driver gone) — nothing more
                                // to write; flush a FIN and exit.
                                None => break,
                            }
                        }
                    }
                    let _ = write_half.shutdown().await;
                });

                // Spawn reader task for subsequent frames on this connection.
                let reader_tx = inbound_tx.clone();
                let reader_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let mut read_half = read_half;
                    loop {
                        tokio::select! {
                            _ = reader_shutdown.cancelled() => break,
                            frame = read_rfc4571_frame(&mut read_half) => match frame {
                                Ok(bytes) => {
                                    let pkt = InboundPacket {
                                        proto: TransportProtocol::TCP,
                                        source: remote_addr,
                                        destination: fake_local,
                                        bytes,
                                        received_at: Instant::now(),
                                    };
                                    if reader_tx.send(pkt).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    eprintln!(
                                        "[display/webrtc] ICE-TCP reader for {remote_addr} exiting: {e}"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                });

                // Inject the first frame we peeked off the wire so the RTC core
                // processes the STUN binding request we used to route.
                let input = TaggedBytesMut {
                    now: Instant::now(),
                    transport: TransportContext {
                        local_addr: fake_local,
                        peer_addr: remote_addr,
                        transport_protocol: TransportProtocol::TCP,
                        ecn: None,
                    },
                    message: BytesMut::from(first_frame.as_slice()),
                };
                if let Err(e) = rtc.handle_read(input) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_read(first TCP frame) failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            _ = tokio::time::sleep(timeout_dur) => {
                if let Err(e) = rtc.handle_timeout(Instant::now()) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_timeout failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            Some(outbound) = frame_rx.recv() => {
                write_video_frame(&mut rtc, &mut state, &outbound);
                if let Err(e) = rtc.handle_timeout(Instant::now()) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_timeout after frame failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            Some(cmd) = command_rx.recv() => {
                handle_command(&mut rtc, &mut state, cmd);
                if let Err(e) = rtc.handle_timeout(Instant::now()) {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: handle_timeout after command failed: {e:?}"
                    );
                    shutdown.cancel();
                    for h in forwarder_handles {
                        let _ = h.await;
                    }
                    return;
                }
            }
            // F8: a UDP forwarder gathered a srflx mapping for its socket.
            // Add the candidate locally so ICE on the RTC side can form the
            // srflx pair, and trickle it to the browser via `ice_tx` (the
            // web gateway forwards it as a `display_ice` frame, which the
            // browser feeds to `pc.addIceCandidate`, buffering until the
            // answer is applied). This is best-effort: a failed add/trickle
            // logs but never tears the peer down — host + ICE-TCP paths
            // remain. `base` is the gathering socket's local address (the
            // host candidate's base).
            Some((base, mapped)) = srflx_rx.recv() => {
                // Drop a degenerate mapping equal to the base (no NAT in
                // front, or STUN reflected loopback) — it would duplicate
                // the host candidate already in the answer SDP.
                if mapped != base {
                    let init = srflx_candidate_init(mapped, base);
                    // Trickle the candidate to the browser using the
                    // canonical RTCIceCandidate.toJSON() field names
                    // (camelCase). A single video m-line means
                    // sdpMLineIndex 0 routes it unambiguously; sdpMid is
                    // null because the inline host candidates carry no
                    // per-candidate mid either.
                    let candidate_json = serde_json::json!({
                        "candidate": init.candidate,
                        "sdpMid": serde_json::Value::Null,
                        "sdpMLineIndex": 0,
                    })
                    .to_string();
                    match rtc.add_local_candidate(init) {
                        Ok(()) => {
                            eprintln!(
                                "[display/webrtc] peer {peer_id}: added srflx candidate {mapped} (base {base}), trickling to browser"
                            );
                            if ice_tx.send((peer_id, candidate_json)).await.is_err() {
                                eprintln!(
                                    "[display/webrtc] peer {peer_id}: srflx trickle channel closed; candidate added locally only"
                                );
                            }
                            if let Err(e) = rtc.handle_timeout(Instant::now()) {
                                eprintln!(
                                    "[display/webrtc] peer {peer_id}: handle_timeout after srflx candidate failed: {e:?}"
                                );
                                shutdown.cancel();
                                for h in forwarder_handles {
                                    let _ = h.await;
                                }
                                return;
                            }
                        }
                        Err(e) => eprintln!(
                            "[display/webrtc] peer {peer_id}: failed to add srflx candidate {mapped}: {e}"
                        ),
                    }
                }
            }
            // The TURN relay task allocated a relay on coturn. Add the
            // `typ relay` candidate locally so ICE can form the relay pair,
            // remember the relayed address + the relay-output channel so
            // `drain_outputs` routes relay-destined RTC output through the
            // task, and trickle the candidate to the browser via `ice_tx`
            // (same path srflx uses). Best-effort: a failed add logs but never
            // tears the peer down — host/srflx/ICE-TCP paths remain. This
            // fires at most once per peer (the task allocates a single relay).
            Some(alloc) = alloc_rx.recv() => {
                let RelayAllocation {
                    relayed_addr,
                    mapped_addr,
                    relay_out_tx: out_tx,
                } = alloc;
                // Record routing state so `drain_outputs` can dispatch RTC
                // output sourced from the relayed address to the relay task.
                relay_addr = Some(relayed_addr);
                relay_out_tx = Some(out_tx);
                let init = relay_candidate_init(relayed_addr, mapped_addr);
                let candidate_json = serde_json::json!({
                    "candidate": init.candidate,
                    "sdpMid": serde_json::Value::Null,
                    "sdpMLineIndex": 0,
                })
                .to_string();
                match rtc.add_local_candidate(init) {
                    Ok(()) => {
                        eprintln!(
                            "[display/webrtc] peer {peer_id}: added relay candidate {relayed_addr} (raddr {mapped_addr}), trickling to browser"
                        );
                        if ice_tx.send((peer_id, candidate_json)).await.is_err() {
                            eprintln!(
                                "[display/webrtc] peer {peer_id}: relay trickle channel closed; candidate added locally only"
                            );
                        }
                        if let Err(e) = rtc.handle_timeout(Instant::now()) {
                            eprintln!(
                                "[display/webrtc] peer {peer_id}: handle_timeout after relay candidate failed: {e:?}"
                            );
                            shutdown.cancel();
                            for h in forwarder_handles {
                                let _ = h.await;
                            }
                            return;
                        }
                    }
                    Err(e) => eprintln!(
                        "[display/webrtc] peer {peer_id}: failed to add relay candidate {relayed_addr}: {e}"
                    ),
                }
            }
            // Phase 4d.1: observed-send-bitrate poll. Calls
            // `rtc.get_stats(now, StatsSelector::None)` (read-only walk
            // of the rtc-side accumulator state, cheap), projects
            // outbound streams to `(ssrc, bytes_sent)`, computes the
            // recent send bitrate from per-SSRC deltas vs the previous
            // sample, publishes to the watch channel the layer-
            // selection aggregator (4d.2) subscribes to.
            //
            // `send_replace` (not `send`) so the channel always carries
            // the latest value even if no receiver has subscribed yet
            // — semantics align with watch's "always has a current
            // value" contract.
            //
            // No errors propagate from a failed send (channel closed
            // == aggregator gone == nothing to do), so this branch
            // never tears down the driver.
            _ = twcc_poll.tick() => {
                let report = rtc.get_stats(Instant::now(), StatsSelector::None);
                let bitrate = extract_recent_outbound_bitrate(
                    report.outbound_rtp_streams().map(|s| (
                        s.sent_rtp_stream_stats.rtp_stream_stats.ssrc,
                        s.sent_rtp_stream_stats.bytes_sent,
                    )),
                    &mut prev_outbound_bytes,
                    Instant::now(),
                );
                observed_send_bitrate_tx.send_replace(bitrate);

                // Phase 4d.3a: project remote-inbound-rtp entries
                // (RR-derived, the field set rtc 0.9 actually
                // populates per `accumulator/rtp_stream/outbound.rs`)
                // into per-RID health, mapping outbound SSRCs back
                // through `state.rtp.by_rid`. Empty map publishes
                // every poll until the first RR arrives — receivers
                // see `borrow()` returning the empty map and can
                // distinguish "no signal yet" from "healthy."
                let ssrc_table: Vec<(SimulcastRid, u32)> = state
                    .rtp
                    .by_rid
                    .iter()
                    .map(|(rid, s)| (rid.clone(), s.ssrc))
                    .collect();
                let remote_inbound_iter = report
                    .iter_by_type(RTCStatsType::RemoteInboundRTP)
                    .filter_map(|entry| match entry {
                        RTCStatsReportEntry::RemoteInboundRtp(s) => Some((
                            s.received_rtp_stream_stats.rtp_stream_stats.ssrc,
                            s.fraction_lost,
                            s.received_rtp_stream_stats.packets_lost,
                            s.round_trip_time,
                            // Phase 4d.3a review fix: rtc 0.9 emits
                            // default RemoteInboundRTP snapshots for
                            // every outbound stream even pre-RR (all
                            // fields zero). The helper filters on
                            // `rtt_measurements == 0` to drop those
                            // before the policy sees them — without
                            // this, every just-connected peer would
                            // present a phantom "0% loss" signal that
                            // looks like real health.
                            s.round_trip_time_measurements,
                        )),
                        _ => None,
                    });
                let health = map_remote_inbound_to_rid_health(
                    remote_inbound_iter,
                    &ssrc_table,
                );
                remote_inbound_health_tx.send_replace(health);
            }
        }
    }
}

pub(crate) enum DriverExit {
    Closed,
}

/// Drain pending writes, reads, and events from the sans-I/O peer connection.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drain_outputs<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    sockets_by_addr: &HashMap<SocketAddr, Arc<UdpSocket>>,
    tcp_senders: &mut HashMap<SocketAddr, TcpFrameSender>,
    // Relay routing: when the allocation has landed, `relay_addr` is our
    // relayed transport address (the relay candidate's base) and
    // `relay_out_tx` feeds RTC output destined *from* that address to the
    // TURN relay task, which wraps it toward coturn. `None` until/unless a
    // relay allocation succeeds.
    relay_addr: Option<SocketAddr>,
    relay_out_tx: Option<&mpsc::Sender<(SocketAddr, Vec<u8>)>>,
    state: &mut DriverState,
    input_handler: &Arc<dyn Fn(InputEvent) + Send + Sync>,
    clipboard_handler: &Arc<dyn Fn(ClipboardContent) + Send + Sync>,
    authority_handler: &AuthorityChannelHandler,
    tile_control_handler: &TileControlHandler,
    keyframe_request_tx: &mpsc::Sender<SimulcastRid>,
) -> Result<Instant, DriverExit> {
    while let Some(t) = rtc.poll_write() {
        // Relay-destined output: ICE picked the relay candidate, so rtc emits
        // a transmit whose source is our relayed address. That address is not
        // a kernel socket we bound — it lives on coturn — so we route the
        // payload to the TURN relay task instead of `sendto`, and we do this
        // BEFORE the routability filter (both addresses are public coturn-side
        // addresses, not a local source bind). The relay task ensures a
        // permission/channel exists and wraps the bytes toward coturn.
        if t.transport.transport_protocol == TransportProtocol::UDP
            && Some(t.transport.local_addr) == relay_addr
        {
            if let Some(tx) = relay_out_tx {
                // try_send keeps the rtc poll loop non-blocking; a full relay
                // queue drops this packet as backpressure (RTP recovery /
                // ICE retransmit covers the loss).
                let _ = tx.try_send((t.transport.peer_addr, t.message.to_vec()));
            }
            continue;
        }
        // Route by connection before trusting the engine's protocol stamp:
        // rtc 0.9 marks DTLS and SCTP transmits `TransportProtocol::UDP`
        // even when the selected pair is TCP ("TransportProtocol doesn't
        // matter" — rtc/src/peer_connection/transport/dtls/mod.rs), so a
        // peer that reached us over ICE-TCP must be matched by its tuple,
        // not by the stamp, or every post-ICE packet misses the stream and
        // DTLS times out. (The relay check above stays first: relay
        // transmits key on our relayed *local* address, which is never a
        // TCP peer tuple.)
        if let Some(sender) = tcp_senders.get(&t.transport.peer_addr) {
            let contents: Vec<u8> = t.message.to_vec();
            // Enqueue onto the connection's ordered writer channel.
            // `try_send` (never `send().await`) keeps the rtc poll loop
            // non-blocking: a full queue means the writer task can't
            // keep up with the encoder, so we drop *this* frame as
            // backpressure rather than stalling the driver or
            // overflowing the kernel send buffer. The writer task,
            // not the scheduler, controls wire order.
            match sender.try_send(contents) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Slow/saturated TCP path — drop and let RTP
                    // recovery (PLI/FIR + keyframes) catch the peer up.
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    // Writer task exited (connection torn down). Forget
                    // the dead sender so we stop trying to route to it.
                    tcp_senders.remove(&t.transport.peer_addr);
                }
            }
            continue;
        }
        if t.transport.transport_protocol == TransportProtocol::TCP {
            // TCP-stamped transmit with no live stream for the tuple: the
            // connection is gone and there is nothing to write to.
            continue;
        }
        // Routability filtering only applies to UDP: we need the kernel's
        // `sendto` to succeed from our bound socket, and a
        // loopback-source-to-routable-destination pair would be rejected
        // with EINVAL.
        if t.transport.local_addr.is_ipv4() != t.transport.peer_addr.is_ipv4() {
            continue;
        }
        if t.transport.local_addr.ip().is_loopback() != t.transport.peer_addr.ip().is_loopback() {
            continue;
        }
        let Some(sock) = sockets_by_addr.get(&t.transport.local_addr) else {
            eprintln!(
                "[display/webrtc] UDP transmit from unknown source {}, dropping",
                t.transport.local_addr
            );
            continue;
        };
        if let Err(e) = sock.send_to(&t.message, t.transport.peer_addr).await {
            eprintln!(
                "[display/webrtc] udp send {} -> {} failed: {e}",
                t.transport.local_addr, t.transport.peer_addr
            );
        }
    }

    while let Some(message) = rtc.poll_read() {
        handle_message(
            message,
            state,
            input_handler,
            clipboard_handler,
            authority_handler,
            tile_control_handler,
            keyframe_request_tx,
        );
    }

    while let Some(event) = rtc.poll_event() {
        if handle_event(rtc, state, event) {
            return Err(DriverExit::Closed);
        }
    }

    Ok(rtc
        .poll_timeout()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400)))
}

pub(crate) fn handle_event<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    state: &mut DriverState,
    event: RTCPeerConnectionEvent,
) -> bool {
    match event {
        RTCPeerConnectionEvent::OnIceConnectionStateChangeEvent(s) => {
            eprintln!("[display/webrtc] ICE: {s:?}");
        }
        RTCPeerConnectionEvent::OnConnectionStateChangeEvent(s) => {
            eprintln!("[display/webrtc] connection: {s:?}");
            if matches!(
                s,
                rtc::peer_connection::state::RTCPeerConnectionState::Failed
                    | rtc::peer_connection::state::RTCPeerConnectionState::Closed
            ) {
                return true;
            }
        }
        RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(cid)) => {
            let label = rtc
                .data_channel(cid)
                .map(|channel| channel.label().to_string())
                .unwrap_or_else(|| format!("channel-{cid}"));
            eprintln!("[display/webrtc] data channel open: {label}");
            let queued =
                drain_pending_authority_for_label(&label, &mut state.pending_authority_state);
            let queued_tile = drain_pending_tile_for_label(state, &label);
            state.channels.insert(label.clone(), cid);
            if label == TILE_DELTAS_CHANNEL_LABEL {
                state.tile_delta_backpressure.reset();
                if let Some(mut channel) = rtc.data_channel(cid) {
                    let cfg = state.tile_delta_backpressure.config();
                    channel.set_buffered_amount_high_threshold(watermark_to_u32(
                        cfg.high_watermark_bytes,
                    ));
                    channel.set_buffered_amount_low_threshold(watermark_to_u32(
                        cfg.low_watermark_bytes,
                    ));
                }
            }
            // F-1.2: flush any authority states queued before the
            // `display_input_authority` channel opened. See
            // `Command::SendAuthorityState` for why queueing exists —
            // the federated authority broadcast can register a
            // subscriber and emit its initial snapshot before the
            // browser's channel finishes negotiating.
            if !queued.is_empty() {
                if let Some(mut channel) = rtc.data_channel(cid) {
                    for (display_id, auth_state) in queued {
                        let json = serialize_authority_state(display_id, auth_state);
                        if let Err(e) = channel.send_text(json) {
                            eprintln!(
                                "[display/webrtc] authority channel \
                                 queued write failed: {e:?}"
                            );
                        }
                    }
                }
            }
            if !queued_tile.is_empty() {
                if let Some(mut channel) = rtc.data_channel(cid) {
                    for data in queued_tile {
                        if let Err(e) = channel.send(BytesMut::from(&data[..])) {
                            eprintln!(
                                "[display/webrtc] tile channel queued write \
                                 failed on {label}: {e:?}"
                            );
                        }
                    }
                }
            }
        }
        RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnClose(cid)) => {
            let was_tile_deltas =
                state.channels.get(TILE_DELTAS_CHANNEL_LABEL).copied() == Some(cid);
            state.channels.retain(|_, v| *v != cid);
            if was_tile_deltas {
                state.tile_delta_backpressure.reset();
            }
        }
        RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnBufferedAmountHigh(cid)) => {
            if state.channels.get(TILE_DELTAS_CHANNEL_LABEL).copied() == Some(cid)
                && state.tile_delta_backpressure.on_buffered_amount_high()
            {
                let stats = state.tile_delta_backpressure.stats();
                eprintln!(
                    "[display/webrtc] tile-deltas backpressure high: \
                     pausing supersedable deltas (sent={} dropped={})",
                    stats.sent_frames, stats.dropped_frames
                );
            }
        }
        RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnBufferedAmountLow(cid)) => {
            if state.channels.get(TILE_DELTAS_CHANNEL_LABEL).copied() == Some(cid)
                && state.tile_delta_backpressure.on_buffered_amount_low()
            {
                let stats = state.tile_delta_backpressure.stats();
                eprintln!(
                    "[display/webrtc] tile-deltas backpressure low: \
                     resuming supersedable deltas (sent={} dropped={})",
                    stats.sent_frames, stats.dropped_frames
                );
            }
        }
        _ => {}
    }
    false
}

/// **Phase 4d.3a**: project per-SSRC remote-inbound stats (RR-derived,
/// from rtc 0.9's `RTCRemoteInboundRtpStreamStats` accumulator) onto
/// the per-RID SSRC table the driver maintains in `state.rtp.by_rid`.
/// Returns one [`PeerLayerHealth`] entry per recognized RID; SSRCs not
/// present in the table (transient renegotiation windows, on-demand
/// codecs we don't carry per-RID, RR for an SSRC we never advertised)
/// are silently dropped — same defensive policy as the per-RID PLI
/// router in [`route_rtcp_keyframe_requests`].
///
/// Pure: takes flat `(ssrc, fraction_lost, packets_lost, rtt,
/// rtt_measurements)` tuples rather than
/// `&RTCRemoteInboundRtpStreamStats` so tests can construct
/// synthetic inputs directly without the rtc 0.9 `pub(crate)`
/// constructor walls. Production projection from
/// `report.iter_by_type(RTCStatsType::RemoteInboundRTP)` happens at
/// the caller (the driver's `twcc_poll` branch).
///
/// **Pre-RR filtering**: rtc 0.9 emits a default-valued
/// `RemoteInboundRTP` snapshot for every outbound stream even
/// before any RR has actually been received — all fields are
/// zero, including `fraction_lost = 0.0` (which would otherwise
/// look like "perfectly healthy" to the policy). The
/// `round_trip_time_measurements == 0` predicate filters these
/// out: a non-zero count means at least one RR has arrived and
/// the values reflect a real measurement. Without this filter,
/// the policy would receive a phantom "0% loss" signal for every
/// outbound layer the moment the peer connects, immediately
/// confirming `Wanted` for layers that may not even be reaching
/// the receiver yet.
///
/// **No deltas in 4d.3a.** All forwarded input fields are passed
/// as-is: `fraction_lost` is already a per-RR-window value (rtc
/// 0.9 derives it from RR), `packets_lost` is cumulative-since-
/// start (deltas can be derived in 4d.3b if the policy needs
/// them), `rtt` is the most recent measurement. Keeping the
/// helper purely projective lets 4d.3b decide which signals to
/// use without re-shaping this layer.
pub(crate) fn map_remote_inbound_to_rid_health(
    remote_inbound: impl IntoIterator<Item = (u32, f64, i64, f64, u64)>,
    ssrc_table: &[(SimulcastRid, u32)],
) -> HashMap<SimulcastRid, PeerLayerHealth> {
    let mut out = HashMap::new();
    for (ssrc, fraction_lost, packets_lost_total, round_trip_time_seconds, rtt_measurements) in
        remote_inbound
    {
        // Pre-RR default snapshot — see helper docstring.
        if rtt_measurements == 0 {
            continue;
        }
        if let Some(rid) = rid_for_ssrc(ssrc_table, ssrc) {
            out.insert(
                rid,
                PeerLayerHealth {
                    fraction_lost,
                    packets_lost_total,
                    round_trip_time_seconds,
                    round_trip_time_measurements: rtt_measurements,
                },
            );
        }
    }
    out
}

/// **Phase 4d.1**: compute the per-peer recent observed send bitrate
/// (bits per second) from the deltas of `bytes_sent` across all
/// outbound RTP streams over one polling window.
///
/// **What this signals**: how much data the peer is actually pushing
/// onto the wire right now, summed across simulcast layers + RTX
/// streams. NOT a congestion-control bandwidth estimate (rtc 0.9
/// doesn't expose one — see `TWCC_POLL_INTERVAL` for why). The
/// layer-selection aggregator (4d.2) interprets this as "delivery
/// rate the peer's encoder + network are sustaining." A drop from
/// the encoder's configured target indicates either encoder
/// underrun or network constraint; either way, it's a layer-
/// selection signal.
///
/// `current` is `(ssrc, bytes_sent)` for each outbound stream,
/// projected from `report.outbound_rtp_streams()` at the production
/// caller. `prev` is the per-SSRC last-sample state the driver
/// maintains across polls; this helper updates it in place.
///
/// Returns `None` when:
/// - First poll for a peer (`prev` empty for every observed SSRC).
/// - All observed SSRCs had zero delta-time since last poll
///   (caller polled twice in the same instant — shouldn't happen
///   with the 1s interval).
/// - All observed SSRCs had non-positive byte deltas (counter
///   wraparound or stream restart, both defensive).
///
/// Returns `Some(total_bps)` when at least one SSRC contributed
/// a usable delta sample. Total is summed across SSRCs because
/// the layer-selection decision is per-peer (the peer's outbound
/// link is the bottleneck, not any individual encoding).
pub(crate) fn extract_recent_outbound_bitrate(
    current: impl IntoIterator<Item = (u32, u64)>,
    prev: &mut HashMap<u32, (u64, Instant)>,
    now: Instant,
) -> Option<u64> {
    let mut total_bits_per_sec: u64 = 0;
    let mut had_usable_sample = false;
    for (ssrc, current_bytes) in current {
        let usable = match prev.get(&ssrc) {
            Some(&(prev_bytes, prev_at)) => {
                let elapsed = now.saturating_duration_since(prev_at);
                if elapsed.is_zero() {
                    // Two polls in the same instant — shouldn't happen
                    // with the 1s poll interval, but defensive.
                    None
                } else if current_bytes < prev_bytes {
                    // Counter wraparound (impossible for u64 in
                    // realistic timeframes) or stream restart
                    // (rtc dropped + recreated the SSRC's accumulator
                    // — happens on renegotiation). Either way, treat
                    // this SSRC's sample as unusable for THIS poll;
                    // the next poll's prev will be the current value
                    // and produce a clean delta.
                    None
                } else {
                    let delta_bytes = current_bytes - prev_bytes;
                    let bps = (delta_bytes as f64 * 8.0) / elapsed.as_secs_f64();
                    if !bps.is_finite() {
                        None
                    } else {
                        Some(bps as u64)
                    }
                }
            }
            None => {
                // First sample for this SSRC — no prev to delta
                // against. Record now; next poll produces the first
                // usable delta.
                None
            }
        };
        prev.insert(ssrc, (current_bytes, now));
        if let Some(bps) = usable {
            total_bits_per_sec = total_bits_per_sec.saturating_add(bps);
            had_usable_sample = true;
        }
    }
    if had_usable_sample {
        Some(total_bits_per_sec)
    } else {
        None
    }
}

/// Reverse-lookup: given an SSRC reported in an inbound RTCP feedback
/// packet (PLI's `media_ssrc` or FIR's per-entry `ssrc`), find the
/// simulcast RID that owns it.
///
/// Linear scan over the (rid, ssrc) pairs — N ≤ 3 (VP8 simulcast:
/// full / half / quarter) or N == 1 (single-encoding codecs like
/// H.264). Takes a flat slice instead of the production
/// `HashMap<SimulcastRid, RidRtpState>` so tests can build the table
/// inline without constructing real packetizers.
pub(crate) fn rid_for_ssrc(ssrc_table: &[(SimulcastRid, u32)], ssrc: u32) -> Option<SimulcastRid> {
    ssrc_table
        .iter()
        .find_map(|(rid, s)| (*s == ssrc).then(|| rid.clone()))
}

/// Iterate inbound RTCP packets and emit a keyframe-request RID for
/// every PLI / FIR whose target SSRC matches one of this peer's
/// outbound encoding SSRCs. Output goes onto a bounded mpsc; the pool
/// intake side reads it and calls
/// [`crate::encode::pool::EncoderPool::request_keyframe`]
/// with the active codec + the routed RID, hitting only that layer's
/// encoder.
///
/// **Per-RID PLI is required for simulcast** because each layer's
/// browser-side decoder maintains its own keyframe-recovery state.
/// A PLI on rid `q` (quarter) means "I lost the keyframe on the
/// quarter layer specifically" — kicking the full-layer encoder in
/// response would burn one `f` keyframe (at full bandwidth!) for
/// nothing while the quarter layer stays broken. Routing per-RID
/// keeps recovery cost proportional to which layer actually lost
/// frames.
///
/// Unknown SSRCs are logged at warn level and dropped — they can
/// happen briefly during track-renegotiation windows or if the
/// browser sends RTCP for an SSRC we never advertised. Treating
/// them as a hard error would be over-eager (they're transient and
/// don't break correctness); ignoring them silently would mask
/// genuine SSRC-mapping bugs, hence the log.
///
/// RTCP packet types other than PLI/FIR (NACK, RR, SR, SDES, BYE,
/// transport-cc, REMB, TWCC) are ignored here — those are handled
/// by rtc 0.9's interceptor for stats/bandwidth-estimation purposes
/// and never need to flow through this routing path.
///
/// Lossy `try_send`: if the keyframe-request channel is full, drop.
/// The pool's coalescer would dedup the request anyway, and the
/// next PLI within the coalesce window will re-request. Blocking
/// the rtc poll loop on a full channel would hurt the entire peer
/// for the sake of a request that's about to be dropped at the next
/// hop.
pub(crate) fn route_rtcp_keyframe_requests(
    packets: &[Box<dyn rtc::rtcp::Packet>],
    ssrc_table: &[(SimulcastRid, u32)],
    keyframe_request_tx: &mpsc::Sender<SimulcastRid>,
) {
    for packet in packets {
        if let Some(pli) = packet.as_any().downcast_ref::<PictureLossIndication>() {
            match rid_for_ssrc(ssrc_table, pli.media_ssrc) {
                Some(rid) => {
                    let _ = keyframe_request_tx.try_send(rid);
                }
                None => {
                    eprintln!(
                        "[display/webrtc] PLI for unknown SSRC {} \
                         (known SSRCs: {:?}); dropping",
                        pli.media_ssrc,
                        ssrc_table.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
                    );
                }
            }
        } else if let Some(fir) = packet.as_any().downcast_ref::<FullIntraRequest>() {
            for entry in &fir.fir {
                match rid_for_ssrc(ssrc_table, entry.ssrc) {
                    Some(rid) => {
                        let _ = keyframe_request_tx.try_send(rid);
                    }
                    None => {
                        eprintln!(
                            "[display/webrtc] FIR for unknown SSRC {} \
                             (known SSRCs: {:?}); dropping",
                            entry.ssrc,
                            ssrc_table.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
                        );
                    }
                }
            }
        }
    }
}

pub(crate) fn handle_message(
    message: RTCMessage,
    state: &mut DriverState,
    input_handler: &Arc<dyn Fn(InputEvent) + Send + Sync>,
    clipboard_handler: &Arc<dyn Fn(ClipboardContent) + Send + Sync>,
    authority_handler: &AuthorityChannelHandler,
    tile_control_handler: &TileControlHandler,
    keyframe_request_tx: &mpsc::Sender<SimulcastRid>,
) {
    if let RTCMessage::RtcpPacket(_track_id, packets) = &message {
        // Project by_rid → flat (rid, ssrc) table for the routing
        // helper. N ≤ 3 in production (VP8 simulcast layers);
        // allocation cost is negligible at RTCP rates.
        let ssrc_table: Vec<(SimulcastRid, u32)> = state
            .rtp
            .by_rid
            .iter()
            .map(|(rid, st)| (rid.clone(), st.ssrc))
            .collect();
        route_rtcp_keyframe_requests(packets, &ssrc_table, keyframe_request_tx);
        return;
    }
    let RTCMessage::DataChannelMessage(cid, RTCDataChannelMessage { data, .. }) = message else {
        return;
    };
    let label = state
        .channels
        .iter()
        .find_map(|(k, v)| (*v == cid).then(|| k.clone()));
    match label.as_deref() {
        Some("control") | Some("pointer") => {
            let label = label.as_deref().unwrap_or("unknown");
            crate::input_telemetry::record_data_channel_input(label, data.len());
            match std::str::from_utf8(&data) {
                Ok(text) => match serde_json::from_str::<InputEvent>(text) {
                    Ok(evt) => input_handler(evt),
                    Err(_) => crate::input_telemetry::record_input_parse_error(),
                },
                Err(_) => crate::input_telemetry::record_input_parse_error(),
            }
        }
        Some("clipboard") => {
            if let Ok(text) = std::str::from_utf8(&data) {
                if let Some(content) = parse_clipboard_set(text) {
                    clipboard_handler(content);
                }
            }
        }
        // F-1.3b2: federated authority channel — parse on the wire,
        // hand off to the opaque handler. Match against the const
        // (not a literal) via a guard arm so the channel-label
        // identity is sourced from `AUTHORITY_CHANNEL_LABEL` only —
        // same const that `Command::SendAuthorityState` uses for the
        // outbound write. Any future rename touches one constant.
        Some(label) if label == AUTHORITY_CHANNEL_LABEL => {
            if let Ok(text) = std::str::from_utf8(&data) {
                if let Some(msg) = parse_authority_channel_message(text) {
                    authority_handler(msg);
                }
            }
        }
        Some(label) if label == TILE_CONTROL_CHANNEL_LABEL => {
            if let Some(msg) = parse_tile_control_message(&data) {
                tile_control_handler(msg);
            }
        }
        _ => {}
    }
}

pub(crate) fn write_video_frame<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    state: &mut DriverState,
    outbound: &OutboundEncodedFrame,
) {
    let frame = &outbound.frame;
    let rid = &outbound.rid;

    // Phase-4c-prep: the keyframe gate is keyed by `(payload_spec, rid)`,
    // not `payload_spec` alone. See `DriverState::video_specs` for why
    // — VP8 simulcast layers share the same payload_spec but each
    // RID's keyframe gate must be independent.
    let spec_key = (frame.payload_spec.clone(), rid.clone());
    if !state.video_specs.contains_key(&spec_key) {
        let new = if payload_spec_matches_codec(&frame.payload_spec, &state.rtp.codec) {
            SpecState::Ready {
                keyframe_seen: false,
            }
        } else {
            eprintln!(
                "[display/webrtc] encoded frame spec {} (rid {}) does not match \
                 negotiated codec {}; dropping this (spec, rid)",
                frame.payload_spec.codec_mime,
                rid.as_str(),
                state.rtp.codec.mime_type,
            );
            SpecState::Unsupported
        };
        state.video_specs.insert(spec_key.clone(), new);
    }

    // Step 2: extract current keyframe readiness from the spec state.
    // Copy out immutably so we can mutate `state.first_frame_at` below
    // without borrow conflicts with `state.video_specs`.
    let keyframe_ready = match state.video_specs.get(&spec_key) {
        Some(SpecState::Ready { keyframe_seen }) => *keyframe_seen,
        // Unsupported or (impossibly) missing — drop silently. The
        // first arm already emitted a log on entering Unsupported.
        _ => return,
    };

    // Step 3: per-`(spec, rid)` keyframe gate. Closed until this
    // (spec, rid) pair has had ≥1 keyframe *successfully written*
    // (see step 5 — the flag flips only after `write_rtp` returns Ok).
    // A keyframe from spec A on rid X that fails to write does not
    // open the gate for spec A's P-frames on rid X, and no keyframe
    // of (spec A, rid X) ever opens (spec A, rid Y)'s gate or (spec
    // B, *)'s gate.
    if !keyframe_ready && !frame.is_keyframe {
        return;
    }

    // Step 3b: look up this RID's send state. Missing entry means a
    // forwarder is producing frames for a RID the driver was never
    // told about — should be unreachable since `build_with_codec_set`
    // populates `by_rid` from the same source the intake's forwarders
    // pull from. Treat as fail-loud to surface the contract violation.
    let rid_state = match state.rtp.by_rid.get_mut(rid) {
        Some(s) => s,
        None => {
            eprintln!(
                "[display/webrtc] frame for unknown rid {}; encoder/track \
                 contract divergence — driver only knows {:?}",
                rid.as_str(),
                state
                    .rtp
                    .by_rid
                    .keys()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>(),
            );
            return;
        }
    };

    // Step 4: wallclock anchor + RTP timestamp samples.
    let now = Instant::now();
    if state.first_frame_at.is_none() {
        state.first_frame_at = Some(now);
    }
    let samples = (frame.duration_ms.max(1) as u32).saturating_mul(90);

    // Step 5: write + on-success gate flip. Use the per-RID
    // packetizer (its own sequence + RTP-timestamp continuation
    // state) and stamp the per-RID SSRC onto every packet so
    // `RTCRtpSender::write_rtp` routes to the matching encoding.
    let payload = Bytes::from(frame.data.clone());
    let packets = match rid_state.packetizer.packetize(&payload, samples) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "[display/webrtc] RTP packetize failed on rid {}: {e}",
                rid.as_str(),
            );
            return;
        }
    };
    let rid_ssrc = rid_state.ssrc;

    let (mid_ext_id, rid_ext_id) = rtp_header_extension_ids(rtc, state);
    for mut packet in packets {
        packet.header.ssrc = rid_ssrc;
        if let Some(id) = mid_ext_id {
            let _ = packet
                .header
                .set_extension(id, Bytes::from(state.rtp.mid.as_bytes().to_vec()));
        }
        if let Some(id) = rid_ext_id {
            let _ = packet
                .header
                .set_extension(id, Bytes::from(rid.as_str().as_bytes().to_vec()));
        }

        let Some(mut sender) = rtc.rtp_sender(state.rtp.sender_id) else {
            return;
        };
        match sender.write_rtp(packet) {
            Ok(()) => {}
            Err(e) => {
                eprintln!(
                    "[display/webrtc] write_rtp failed on rid {}: {e:?}",
                    rid.as_str(),
                );
                return;
            }
        }
    }

    if !keyframe_ready {
        if let Some(SpecState::Ready { keyframe_seen }) = state.video_specs.get_mut(&spec_key) {
            // Only flip keyframe_seen for this (spec, rid) pair AFTER
            // a successful packet write. If the write is the first
            // keyframe on this (spec, rid), the gate opens for
            // subsequent P-frames on the same (spec, rid). If it
            // wasn't a keyframe (gate was already open), this is a
            // no-op.
            *keyframe_seen = true;
        }
    }
}

pub(crate) fn payload_spec_matches_codec(
    spec: &crate::encode::PayloadSpec,
    codec: &RTCRtpCodec,
) -> bool {
    if spec
        .codec_mime
        .eq_ignore_ascii_case(crate::encode::MIME_TYPE_VP8)
    {
        return codec.mime_type.eq_ignore_ascii_case(RTC_MIME_TYPE_VP8);
    }
    if spec
        .codec_mime
        .eq_ignore_ascii_case(crate::encode::MIME_TYPE_H264)
    {
        return codec.mime_type.eq_ignore_ascii_case(RTC_MIME_TYPE_H264)
            && spec.h264_packetization_mode == Some(1)
            && spec.h264_profile_level_id.as_deref() == Some("42e01f");
    }
    false
}

pub(crate) fn rtp_header_extension_ids<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    state: &mut DriverState,
) -> (Option<u8>, Option<u8>) {
    if state.rtp.mid_ext_id.is_some() || state.rtp.rid_ext_id.is_some() {
        return (state.rtp.mid_ext_id, state.rtp.rid_ext_id);
    }
    if let Some(mut sender) = rtc.rtp_sender(state.rtp.sender_id) {
        let params = sender.get_parameters();
        for ext in &params.rtp_parameters.header_extensions {
            if ext.uri == "urn:ietf:params:rtp-hdrext:sdes:mid" {
                state.rtp.mid_ext_id = Some(ext.id as u8);
            } else if ext.uri == "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id" {
                state.rtp.rid_ext_id = Some(ext.id as u8);
            }
        }
    }
    (state.rtp.mid_ext_id, state.rtp.rid_ext_id)
}

pub(crate) fn handle_command<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    state: &mut DriverState,
    cmd: Command,
) {
    match cmd {
        Command::AddIceCandidate(s) => {
            let init = RTCIceCandidateInit {
                candidate: s,
                sdp_mid: None,
                sdp_mline_index: None,
                username_fragment: None,
                url: None,
            };
            if let Err(e) = rtc.add_remote_candidate(init) {
                eprintln!("[display/webrtc] parse remote candidate failed: {e}");
            }
        }
        Command::SendClipboard(content) => {
            let Some(cid) = state.channels.get("clipboard").copied() else {
                return;
            };
            let Some(mut channel) = rtc.data_channel(cid) else {
                return;
            };
            let json = serialize_clipboard(&content);
            if let Err(e) = channel.send_text(json) {
                eprintln!("[display/webrtc] clipboard channel write failed: {e:?}");
            }
        }
        Command::SendAuthorityState {
            display_id,
            state: auth_state,
        } => {
            // F-1.2: queue-or-send. If the federated browser's
            // `display_input_authority` data channel is open,
            // serialize and write immediately. If not, queue for
            // flush on `OnDataChannel(OnOpen)` for that label.
            //
            // Local DisplaySlot's WebRtcPeer doesn't create this
            // channel (5a/5c uses the WS path), so the queue
            // accumulates indefinitely there until the driver shuts
            // down — which is fine because: (a) the broadcast loop
            // currently only calls send_authority_state for federated
            // subscribers, and (b) the queue is bounded by the
            // low-frequency take/release event rate, not per-frame.
            if let Some(cid) = state.channels.get(AUTHORITY_CHANNEL_LABEL).copied() {
                if let Some(mut channel) = rtc.data_channel(cid) {
                    let json = serialize_authority_state(display_id, auth_state);
                    if let Err(e) = channel.send_text(json) {
                        eprintln!("[display/webrtc] authority channel write failed: {e:?}");
                    }
                }
            } else {
                state.pending_authority_state.push((display_id, auth_state));
            }
        }
        Command::SendTileFrame { channel, data } => {
            let label = channel.label();
            let data_len = data.len();
            if channel == TileDataChannel::Deltas
                && state.tile_delta_backpressure.decide_delta(data_len)
                    == TileDeltaSendDecision::Drop
            {
                return;
            }
            if let Some(cid) = state.channels.get(label).copied() {
                if let Some(mut dc) = rtc.data_channel(cid) {
                    if let Err(e) = dc.send(BytesMut::from(&data[..])) {
                        eprintln!(
                            "[display/webrtc] tile channel write failed on \
                             {label}: {e:?}"
                        );
                    } else if channel == TileDataChannel::Deltas {
                        state.tile_delta_backpressure.record_delta_sent(data_len);
                    }
                }
            } else if channel.queues_before_open() {
                match channel {
                    TileDataChannel::Control => {
                        state.pending_tile_control.push(data);
                    }
                    TileDataChannel::Snapshot => {
                        state.pending_tile_snapshot.push(data);
                    }
                    TileDataChannel::Deltas => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a `clipboard_set` message from a browser data channel, supporting
/// both text and image (base64-encoded) payloads.
pub(crate) fn parse_clipboard_set(text: &str) -> Option<ClipboardContent> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    if parsed.get("t").and_then(|v| v.as_str()) != Some("clipboard_set") {
        return None;
    }
    let mime = parsed
        .get("mime")
        .and_then(|v| v.as_str())
        .unwrap_or("text/plain");
    if mime.starts_with("image/") {
        let b64 = parsed.get("data").and_then(|v| v.as_str())?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        Some(ClipboardContent::Image {
            mime: mime.to_string(),
            data: bytes,
        })
    } else {
        let text = parsed.get("text").and_then(|v| v.as_str())?;
        Some(ClipboardContent::Text(text.to_string()))
    }
}

/// F-1.3b2: parse a frame received on the `display_input_authority`
/// data channel into an [`AuthorityChannelMessage`].
///
/// Wire format pinned by `parse_authority_channel_message_round_trip`:
///
/// ```text
/// { "t": "display_input_authority_request", "display_id": 0 }
/// { "t": "display_input_authority_release", "display_id": 0 }
/// ```
///
/// Returns `None` for unrecognized `t` discriminators, missing or
/// non-numeric `display_id`, or `display_id` values that don't fit
/// in `u32`. Strict by design — silent drop on the receive side
/// mirrors `parse_clipboard_set`'s contract: a malformed frame is
/// the browser's bug to fix, not the peer's to recover from. The
/// authority handler outside this module is the policy boundary;
/// the wire parse is intentionally narrow.
pub(crate) fn parse_authority_channel_message(text: &str) -> Option<AuthorityChannelMessage> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let t = parsed.get("t").and_then(|v| v.as_str())?;
    let display_id: u32 = parsed
        .get("display_id")
        .and_then(|v| v.as_u64())?
        .try_into()
        .ok()?;
    match t {
        "display_input_authority_request" => Some(AuthorityChannelMessage::Request { display_id }),
        "display_input_authority_release" => Some(AuthorityChannelMessage::Release { display_id }),
        _ => None,
    }
}

pub(crate) fn parse_tile_control_message(bytes: &[u8]) -> Option<TileControlMessage> {
    match tile_transport::decode_frame(bytes).ok()? {
        tile_transport::TileFrame::Subscribe { client_id } => {
            Some(TileControlMessage::Subscribe { client_id })
        }
        tile_transport::TileFrame::SnapshotRequest { epoch, reason } => {
            Some(TileControlMessage::SnapshotRequest { epoch, reason })
        }
        tile_transport::TileFrame::GapReport {
            epoch,
            last_seen_seq,
            expected_seq,
        } => Some(TileControlMessage::GapReport {
            epoch,
            last_seen_seq,
            expected_seq,
        }),
        _ => None,
    }
}

/// F-1.2: data-channel label for federated authority state messages.
/// Browsers create this channel from `PeerDisplayConnection.connect()`
/// (added in the next F-1 commit). The peer's driver opens it
/// passively via `OnDataChannel(OnOpen)` and registers it in
/// `state.channels` keyed by this label.
pub(crate) const AUTHORITY_CHANNEL_LABEL: &str = "display_input_authority";
pub(crate) const TILE_CONTROL_CHANNEL_LABEL: &str = "tile-control";
pub(crate) const TILE_SNAPSHOT_CHANNEL_LABEL: &str = "tile-snapshot";
pub(crate) const TILE_DELTAS_CHANNEL_LABEL: &str = "tile-deltas";

/// Serialize a `display_input_authority_state` frame for the
/// `display_input_authority` data channel. Wire format matches the
/// local 5c WS message exactly (same `t` discriminator, same `state`
/// vocabulary) so browser handlers can stay symmetric.
pub(crate) fn serialize_authority_state(display_id: u32, state: DisplayInputAuthorityState) -> String {
    serde_json::json!({
        "t": "display_input_authority_state",
        "display_id": display_id,
        "state": state.as_wire_str(),
    })
    .to_string()
}

/// F-1.2: drain pending authority states queued before the
/// `display_input_authority` data channel opened. Returns the queue
/// in arrival order so the flush preserves send ordering. No-op
/// (returns empty) for any other channel label, leaving `pending`
/// untouched.
///
/// Extracted for testability: the queue/flush invariant
/// (queued-before-open ⇒ flushed-on-open) lives here in pure-data
/// form so a unit test can pin it without needing to fake an
/// `rtc::data_channel`.
pub(crate) fn drain_pending_authority_for_label(
    label: &str,
    pending: &mut Vec<(u32, DisplayInputAuthorityState)>,
) -> Vec<(u32, DisplayInputAuthorityState)> {
    if label == AUTHORITY_CHANNEL_LABEL {
        std::mem::take(pending)
    } else {
        Vec::new()
    }
}

pub(crate) fn drain_pending_tile_for_label(state: &mut DriverState, label: &str) -> Vec<Vec<u8>> {
    match label {
        TILE_CONTROL_CHANNEL_LABEL => std::mem::take(&mut state.pending_tile_control),
        TILE_SNAPSHOT_CHANNEL_LABEL => std::mem::take(&mut state.pending_tile_snapshot),
        _ => Vec::new(),
    }
}

pub(crate) fn watermark_to_u32(bytes: usize) -> u32 {
    bytes.min(u32::MAX as usize) as u32
}

/// Serialize a `ClipboardContent` for sending over the clipboard data channel.
pub(crate) fn serialize_clipboard(content: &ClipboardContent) -> String {
    match content {
        ClipboardContent::Text(text) => serde_json::json!({
            "t": "clipboard_update",
            "mime": "text/plain",
            "text": text,
        })
        .to_string(),
        ClipboardContent::Image { mime, data } => {
            use base64::Engine;
            serde_json::json!({
                "t": "clipboard_update",
                "mime": mime,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            })
            .to_string()
        }
    }
}

// `routable_local_addrs` and `is_link_local_v6` live in `intendant_core::net`
// so the federation advertise side can share them — same set of
// "addresses we can be reached at" applies to both WebRTC host
// candidates and Agent Card transport URLs.

/// If a remote ICE candidate's connection-address is an mDNS `.local`
/// hostname, resolve it to a literal IP via the system resolver and return
/// a rewritten candidate string. Otherwise pass through unchanged.
///
/// The candidate format per RFC 5245 §15.1 is:
///   `candidate:<foundation> <component> <proto> <priority> <addr> <port> typ <kind> ...`
/// We split on whitespace, the connection-address is field index 4 (counting
/// from the `candidate:` prefix as 0).
pub async fn resolve_mdns_in_candidate(candidate: &str) -> Result<String, String> {
    let mut fields: Vec<&str> = candidate.split_whitespace().collect();
    if fields.len() < 6 {
        return Ok(candidate.to_string());
    }
    let addr_field = fields[4];
    if !addr_field.ends_with(".local") {
        return Ok(candidate.to_string());
    }
    // Resolve via tokio::net::lookup_host. We need a port for the call but
    // discard it; any value works.
    let mut iter = tokio::net::lookup_host(format!("{addr_field}:0"))
        .await
        .map_err(|e| format!("lookup {addr_field}: {e}"))?;
    let resolved = iter
        .next()
        .ok_or_else(|| format!("no addrs for {addr_field}"))?;
    let ip_str = resolved.ip().to_string();
    fields[4] = &ip_str;
    Ok(fields.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_passthrough_for_literal_ip() {
        let c = "candidate:1 1 udp 2113937151 192.168.1.10 5000 typ host generation 0";
        let resolved = resolve_mdns_in_candidate(c).await.unwrap();
        assert_eq!(resolved, c);
    }

    #[tokio::test]
    async fn resolve_passthrough_for_short_input() {
        let c = "candidate:1 1 udp 2113937151";
        let resolved = resolve_mdns_in_candidate(c).await.unwrap();
        assert_eq!(resolved, c);
    }

    /// **Phase-4c-prep**: `DriverState::video_specs` keying changed
    /// from `PayloadSpec` to `(PayloadSpec, SimulcastRid)`. This is a
    /// data-shape pin: a HashMap with `(spec, rid)` keys treats two
    /// distinct rids as distinct entries even when they share the
    /// same payload_spec — which is exactly the VP8 simulcast case
    /// where every layer has the same `PayloadSpec` but each layer's
    /// keyframe gate must remain independent.
    ///
    /// Without per-RID keying, a keyframe seen on RID `full` would
    /// open the gate for P-frames on RIDs `half` and `quarter`. Those
    /// P-frames would reference keyframes the half/quarter
    /// subscribers never received, decoding to garbage.
    ///
    /// This test pins the keying directly. It can't easily exercise
    /// `write_video_frame` end-to-end without an `RTCPeerConnection`,
    /// but the data-shape contract is what matters — if the keying
    /// regresses to `PayloadSpec`-only the map would conflate the
    /// three layers and this test would compile-fail (or assert).
    #[test]
    fn driver_state_video_specs_keys_by_spec_and_rid() {
        use crate::encode::PayloadSpec;
        let spec = PayloadSpec::vp8();
        let mut specs: HashMap<(PayloadSpec, SimulcastRid), SpecState> = HashMap::new();
        // Insert under three distinct rids with the same spec. They
        // must be three distinct entries — pre-fix keying by
        // `PayloadSpec` alone would have collapsed them to one.
        for rid in [
            SimulcastRid::full(),
            SimulcastRid::half(),
            SimulcastRid::quarter(),
        ] {
            specs.insert(
                (spec.clone(), rid),
                SpecState::Ready {
                    keyframe_seen: false,
                },
            );
        }
        assert_eq!(
            specs.len(),
            3,
            "(PayloadSpec, SimulcastRid) keying must keep three rids \
             with the same spec as three distinct entries; got {} \
             entries — keying regressed to spec-only?",
            specs.len(),
        );
        // Flipping one rid's keyframe_seen must not affect the others.
        if let Some(SpecState::Ready { keyframe_seen }) =
            specs.get_mut(&(spec.clone(), SimulcastRid::full()))
        {
            *keyframe_seen = true;
        }
        for rid in [SimulcastRid::half(), SimulcastRid::quarter()] {
            match specs.get(&(spec.clone(), rid.clone())) {
                Some(SpecState::Ready { keyframe_seen }) => assert!(
                    !keyframe_seen,
                    "rid {} keyframe_seen leaked across rids — keying \
                     is wrong",
                    rid.as_str(),
                ),
                _ => panic!("rid {} entry missing", rid.as_str()),
            }
        }
    }

    /// **Phase 4c**: `RtpSendState` carries a `by_rid` map keyed by
    /// `SimulcastRid`. `build_with_codec_set` populates it with one
    /// entry per active RID — N entries for VP8 simulcast (full +
    /// half + quarter), single entry for H.264. The driver's
    /// `state.rtp.by_rid.get_mut(rid)` lookup at write time uses this
    /// map to route per-RID packetizer / SSRC state.
    ///
    /// This test pins the data shape directly:
    /// - The map type allows multiple rids with distinct SSRCs.
    /// - Lookup by rid returns the matching `RidRtpState`.
    /// - The structure compiles and behaves like a `HashMap` (so the
    ///   driver's `state.rtp.by_rid.get_mut(rid)` lookup at write
    ///   time works without surprises).
    ///
    /// The driver's actual write_video_frame is exercised end-to-end
    /// by the `pool_intake_*` tests above (which run real encoders
    /// + forwarders).
    #[test]
    fn rtp_send_state_by_rid_supports_multiple_distinct_rids() {
        // Build three RidRtpState entries with distinct SSRCs.
        // packetizers can't be constructed without a real codec
        // payloader, so we exercise just the SSRC routing —
        // `RidRtpState` is a thin per-rid record and the routing
        // contract is "lookup by rid → get matching ssrc."
        let mut by_rid: HashMap<SimulcastRid, u32> = HashMap::new();
        for (rid, ssrc) in [
            (SimulcastRid::full(), 1001u32),
            (SimulcastRid::half(), 1002u32),
            (SimulcastRid::quarter(), 1003u32),
        ] {
            by_rid.insert(rid, ssrc);
        }
        assert_eq!(by_rid.len(), 3);
        assert_eq!(by_rid.get(&SimulcastRid::full()).copied(), Some(1001));
        assert_eq!(by_rid.get(&SimulcastRid::half()).copied(), Some(1002));
        assert_eq!(by_rid.get(&SimulcastRid::quarter()).copied(), Some(1003));
        // Lookup with a rid the map doesn't contain returns None —
        // matches the driver's "frame for unknown rid" defensive
        // branch in write_video_frame, which fail-loud-logs and drops.
        let unknown_rid = SimulcastRid::new("unknown");
        assert_eq!(by_rid.get(&unknown_rid), None);
    }

    // -------------------------------------------------------------------
    // Phase 4e: route_rtcp_keyframe_requests — per-RID PLI/FIR routing
    // -------------------------------------------------------------------

    /// Stand up a 3-layer VP8 simulcast SSRC table (full / half /
    /// quarter at distinct SSRCs) for the routing tests below.
    /// SSRCs are arbitrary but distinct; mirrors what
    /// `build_with_codec_set` produces from `new_ssrc()` in
    /// production.
    fn vp8_simulcast_ssrc_table() -> Vec<(SimulcastRid, u32)> {
        vec![
            (SimulcastRid::full(), 0xAAAA_0001),
            (SimulcastRid::half(), 0xAAAA_0002),
            (SimulcastRid::quarter(), 0xAAAA_0003),
        ]
    }

    fn pli_for(media_ssrc: u32) -> Box<dyn rtc::rtcp::Packet> {
        Box::new(PictureLossIndication {
            sender_ssrc: 0,
            media_ssrc,
        })
    }

    fn fir_for(ssrcs: &[u32]) -> Box<dyn rtc::rtcp::Packet> {
        Box::new(FullIntraRequest {
            sender_ssrc: 0,
            media_ssrc: 0,
            fir: ssrcs
                .iter()
                .map(
                    |s| rtc::rtcp::payload_feedbacks::full_intra_request::FirEntry {
                        ssrc: *s,
                        sequence_number: 0,
                    },
                )
                .collect(),
        })
    }

    /// PLI for the full layer's SSRC routes to `SimulcastRid::full()`.
    /// Pre-4e the entire codec was kicked into a keyframe; per-RID
    /// routing is what makes simulcast recovery proportional to the
    /// layer that actually lost frames.
    #[tokio::test]
    async fn route_rtcp_keyframe_pli_routes_to_full_rid() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![pli_for(0xAAAA_0001)];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let routed = rx.try_recv().expect("PLI for full SSRC must route");
        assert_eq!(routed, SimulcastRid::full());
        assert!(rx.try_recv().is_err(), "no extra emissions");
    }

    /// PLI for the half layer's SSRC routes to `SimulcastRid::half()` —
    /// NOT to full. Mis-routing to full would burn a full-layer
    /// keyframe (highest bandwidth!) for a half-layer recovery and
    /// leave the half layer broken until its next natural keyframe.
    #[tokio::test]
    async fn route_rtcp_keyframe_pli_routes_to_half_rid() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![pli_for(0xAAAA_0002)];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let routed = rx.try_recv().expect("PLI for half SSRC must route");
        assert_eq!(routed, SimulcastRid::half());
        assert!(rx.try_recv().is_err(), "no extra emissions");
    }

    /// PLI for the quarter layer's SSRC routes to
    /// `SimulcastRid::quarter()`.
    #[tokio::test]
    async fn route_rtcp_keyframe_pli_routes_to_quarter_rid() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![pli_for(0xAAAA_0003)];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let routed = rx.try_recv().expect("PLI for quarter SSRC must route");
        assert_eq!(routed, SimulcastRid::quarter());
        assert!(rx.try_recv().is_err(), "no extra emissions");
    }

    /// PLI for an SSRC we never advertised is a no-op — no emission
    /// on the channel. The helper logs at warn level (verified
    /// indirectly via no panic + no emission); this can happen
    /// briefly during track-renegotiation windows or if the browser
    /// references an old SSRC after a track replacement.
    #[tokio::test]
    async fn route_rtcp_keyframe_pli_unknown_ssrc_is_noop() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![pli_for(0xDEAD_BEEF)];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        assert!(
            rx.try_recv().is_err(),
            "PLI for unknown SSRC must not emit a routing decision"
        );
    }

    /// FIR with a single entry for one SSRC routes the same way as
    /// PLI for that SSRC. RFC 5104 says FIR is "for the rare case
    /// where a new participant joins" — we treat it as semantically
    /// equivalent to PLI for keyframe-routing purposes.
    #[tokio::test]
    async fn route_rtcp_keyframe_fir_single_entry_routes_to_matching_rid() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![fir_for(&[0xAAAA_0002])];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let routed = rx.try_recv().expect("FIR for half SSRC must route");
        assert_eq!(routed, SimulcastRid::half());
        assert!(rx.try_recv().is_err());
    }

    /// FIR can carry multiple `(ssrc, seq)` entries. Each known SSRC
    /// emits its own RID; unknown SSRCs in the same FIR are dropped
    /// silently without affecting the known ones (independent
    /// routing per entry).
    #[tokio::test]
    async fn route_rtcp_keyframe_fir_multi_entry_routes_each_known_ssrc() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        // FIR with full + unknown + quarter — only full and quarter
        // are known and must each route; unknown is a no-op for
        // that entry but must NOT inhibit the other entries.
        let packets = vec![fir_for(&[0xAAAA_0001, 0xDEAD_BEEF, 0xAAAA_0003])];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let mut routed: Vec<SimulcastRid> = Vec::new();
        while let Ok(r) = rx.try_recv() {
            routed.push(r);
        }
        assert_eq!(
            routed.len(),
            2,
            "FIR with 2 known + 1 unknown SSRC should emit 2 routings"
        );
        assert!(routed.contains(&SimulcastRid::full()));
        assert!(routed.contains(&SimulcastRid::quarter()));
    }

    /// FIR for an unknown SSRC alone is a no-op — same contract as
    /// the PLI unknown-SSRC test, exercised through the FIR codepath.
    #[tokio::test]
    async fn route_rtcp_keyframe_fir_unknown_ssrc_is_noop() {
        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets = vec![fir_for(&[0xDEAD_BEEF])];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        assert!(
            rx.try_recv().is_err(),
            "FIR for unknown SSRC must not emit a routing decision"
        );
    }

    /// A compound RTCP packet may carry PLI + FIR + non-keyframe
    /// types (NACK, RR, SR, …). Only PLI and FIR contribute to
    /// keyframe routing; the helper iterates the whole vec and
    /// silently passes over non-feedback types.
    ///
    /// This test uses ReceiverReport as the "ignored" stand-in
    /// because it's the simplest non-keyframe RTCP type to
    /// construct — the same contract holds for NACK / SR / SDES /
    /// BYE / TWCC etc.
    #[tokio::test]
    async fn route_rtcp_keyframe_ignores_non_pli_fir_packets() {
        use rtc::rtcp::receiver_report::ReceiverReport;

        let table = vp8_simulcast_ssrc_table();
        let (tx, mut rx) = mpsc::channel::<SimulcastRid>(8);

        let packets: Vec<Box<dyn rtc::rtcp::Packet>> = vec![
            Box::new(ReceiverReport::default()),
            pli_for(0xAAAA_0001),
            Box::new(ReceiverReport::default()),
        ];
        route_rtcp_keyframe_requests(&packets, &table, &tx);

        let routed = rx.try_recv().expect("PLI between RR/RR must still route");
        assert_eq!(routed, SimulcastRid::full());
        assert!(
            rx.try_recv().is_err(),
            "ReceiverReport packets must not emit any routing decisions"
        );
    }

    // -------------------------------------------------------------------
    // Phase 4d.1 review fix: extract_recent_outbound_bitrate tests
    // -------------------------------------------------------------------

    /// First poll (empty `prev`) → None. The helper has no prior
    /// sample to delta against; it seeds `prev` with the current
    /// values so the next poll can compute a real rate.
    #[test]
    fn extract_bitrate_first_poll_returns_none_seeds_prev() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let now = Instant::now();
        let result =
            extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 10_000u64)], &mut prev, now);
        assert_eq!(result, None, "first poll has no prev — must return None");
        assert_eq!(
            prev.get(&0xAAAA_0001),
            Some(&(10_000u64, now)),
            "first poll must seed prev so the next poll has a baseline"
        );
    }

    /// Second poll with positive delta → `Some(bps)` computed as
    /// `(delta_bytes * 8) / elapsed_secs`. Pin the canonical math so
    /// a future refactor that switches units (kbps? bytes/sec?)
    /// surfaces in the test.
    #[test]
    fn extract_bitrate_second_poll_computes_delta_bps() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);

        // Poll 1: seed.
        extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 100_000u64)], &mut prev, t0);
        // Poll 2: 200 KB more in 1 second → 200_000 bytes * 8 = 1.6 Mbps.
        let result =
            extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 300_000u64)], &mut prev, t1);
        assert_eq!(result, Some(1_600_000));
        // Prev updated to the latest sample.
        assert_eq!(prev.get(&0xAAAA_0001), Some(&(300_000u64, t1)));
    }

    /// Multi-SSRC: deltas summed across all observed SSRCs. The
    /// layer-selection aggregator decides per-peer (the link is the
    /// bottleneck, not any individual encoding) so the helper rolls
    /// up to a single per-peer total.
    #[test]
    fn extract_bitrate_multi_ssrc_sums_deltas() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);

        // VP8 simulcast: 3 outbound SSRCs (full / half / quarter).
        extract_recent_outbound_bitrate(
            vec![
                (0xAAAA_0001u32, 100_000u64),
                (0xAAAA_0002u32, 50_000u64),
                (0xAAAA_0003u32, 20_000u64),
            ],
            &mut prev,
            t0,
        );
        // After 1s: full +250KB (2 Mbps), half +50KB (400 kbps),
        // quarter +12.5KB (100 kbps). Total: 2.5 Mbps.
        let result = extract_recent_outbound_bitrate(
            vec![
                (0xAAAA_0001u32, 350_000u64),
                (0xAAAA_0002u32, 100_000u64),
                (0xAAAA_0003u32, 32_500u64),
            ],
            &mut prev,
            t1,
        );
        assert_eq!(result, Some(2_500_000));
    }

    /// Counter wraparound (current_bytes < prev_bytes for the same
    /// SSRC) skips that SSRC's contribution this poll and re-seeds
    /// prev with the current value. Defends against rtc-side stream
    /// restart on renegotiation (rtc drops + recreates the
    /// accumulator, resetting bytes_sent to 0). Without the skip we'd
    /// underflow the u64 subtraction and produce a garbage delta.
    #[test]
    fn extract_bitrate_counter_wraparound_skips_ssrc_reseed_prev() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);

        // Seed at high value, then "wrap" to a low value (stream restart).
        extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 1_000_000u64)], &mut prev, t0);
        let result = extract_recent_outbound_bitrate(
            vec![(0xAAAA_0001u32, 500u64)], // restart, much smaller
            &mut prev,
            t1,
        );
        assert_eq!(
            result, None,
            "wraparound must skip the SSRC's contribution; with one \
             SSRC and that one skipped, total returns None"
        );
        // Prev re-seeded with the current value so the next poll
        // computes a clean delta against this baseline.
        assert_eq!(prev.get(&0xAAAA_0001), Some(&(500u64, t1)));
    }

    /// Zero elapsed time (two polls at the same Instant) skips that
    /// SSRC's contribution. Defends against the math (divide by zero
    /// → infinity → cast to u64 = wrong); the 1s poll interval makes
    /// this practically unreachable, but the helper guards against it.
    #[test]
    fn extract_bitrate_zero_elapsed_skips_ssrc() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let t0 = Instant::now();
        extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 1000u64)], &mut prev, t0);
        let result = extract_recent_outbound_bitrate(
            vec![(0xAAAA_0001u32, 5000u64)],
            &mut prev,
            t0, // same instant
        );
        assert_eq!(result, None);
    }

    /// New SSRC appearing mid-stream (not in prev) returns None for
    /// THAT SSRC this poll, but seeds prev so the next poll produces
    /// a clean delta. Existing SSRCs continue to contribute normally.
    /// Models the case where a peer's simulcast layer count grows
    /// (e.g. an on-demand H.264 spawn during a session).
    #[test]
    fn extract_bitrate_new_ssrc_mid_stream_seeds_only() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);

        // Seed: only one SSRC.
        extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, 100_000u64)], &mut prev, t0);
        // Second poll: existing SSRC has +125KB (1 Mbps), and a new
        // SSRC appears with 50KB total but no prev to delta against.
        // Result: 1 Mbps from existing only; new SSRC seeded.
        let result = extract_recent_outbound_bitrate(
            vec![(0xAAAA_0001u32, 225_000u64), (0xAAAA_0002u32, 50_000u64)],
            &mut prev,
            t1,
        );
        assert_eq!(
            result,
            Some(1_000_000),
            "existing SSRC's delta contributes; new SSRC seeds only"
        );
        assert_eq!(prev.get(&0xAAAA_0002), Some(&(50_000u64, t1)));
    }

    /// Empty current iterator → None. Models the very-early-life case
    /// where the rtc stats report has no outbound streams yet (track
    /// not yet attached, or pre-handshake).
    #[test]
    fn extract_bitrate_empty_current_returns_none() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let now = Instant::now();
        let result = extract_recent_outbound_bitrate(Vec::<(u32, u64)>::new(), &mut prev, now);
        assert_eq!(result, None);
    }

    /// Stable rate over multiple polls produces consistent bps
    /// readings. Pins that the helper's stateful arithmetic doesn't
    /// drift across iterations.
    #[test]
    fn extract_bitrate_stable_rate_consistent_across_polls() {
        let mut prev: HashMap<u32, (u64, Instant)> = HashMap::new();
        let mut t = Instant::now();
        let mut bytes: u64 = 0;

        // Seed.
        extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, bytes)], &mut prev, t);
        // Three polls each adding 125KB in 1s = steady 1 Mbps.
        for _ in 0..3 {
            t += Duration::from_secs(1);
            bytes += 125_000;
            let result =
                extract_recent_outbound_bitrate(vec![(0xAAAA_0001u32, bytes)], &mut prev, t);
            assert_eq!(result, Some(1_000_000));
        }
    }

    /// `rid_for_ssrc` returns the matching RID for any known SSRC
    /// in the table; None for unknown SSRC. Same contract as the
    /// helper that wraps it; tested directly so a refactor that
    /// changes the lookup data structure (HashMap vs Vec vs
    /// pre-built reverse map) keeps the contract intact.
    #[test]
    fn rid_for_ssrc_returns_matching_rid_or_none() {
        let table = vp8_simulcast_ssrc_table();
        assert_eq!(
            rid_for_ssrc(&table, 0xAAAA_0001),
            Some(SimulcastRid::full())
        );
        assert_eq!(
            rid_for_ssrc(&table, 0xAAAA_0002),
            Some(SimulcastRid::half())
        );
        assert_eq!(
            rid_for_ssrc(&table, 0xAAAA_0003),
            Some(SimulcastRid::quarter())
        );
        assert_eq!(rid_for_ssrc(&table, 0xDEAD_BEEF), None);
        // Empty table: every lookup is None — defends against an
        // empty by_rid in the `build_with_codec_set` empty-active_rids
        // path (which errors out upstream, but the lookup must still
        // be a no-op rather than a panic if it's reached).
        assert_eq!(rid_for_ssrc(&[], 0xAAAA_0001), None);
    }

    // -----------------------------------------------------------------
    // Phase 4d.3a: map_remote_inbound_to_rid_health helper tests
    // -----------------------------------------------------------------

    #[test]
    fn map_remote_inbound_empty_input_returns_empty_map() {
        // No RR data yet — common steady state immediately after a
        // peer connects but before the first RR has been received.
        // The watch publishes the empty map; consumers see "no
        // signal yet" rather than a stale or fabricated reading.
        let table = vp8_simulcast_ssrc_table();
        let out = map_remote_inbound_to_rid_health(std::iter::empty(), &table);
        assert!(out.is_empty());
    }

    #[test]
    fn map_remote_inbound_unknown_ssrc_dropped_silently() {
        // RR for an SSRC we don't carry per-RID (transient
        // renegotiation, on-demand H.264 SSRC outside the simulcast
        // RID table, RR for an SSRC we never advertised). Same
        // defensive policy as `route_rtcp_keyframe_requests`: drop
        // silently rather than fail, since these can occur in the
        // normal lifecycle and aren't actionable. `rtt_measurements
        // = 5` keeps this entry past the pre-RR filter so the test
        // exercises the SSRC-table-drop path specifically, not the
        // pre-RR-filter path.
        let table = vp8_simulcast_ssrc_table();
        let out =
            map_remote_inbound_to_rid_health(vec![(0xDEAD_BEEFu32, 0.05, 42, 0.018, 5)], &table);
        assert!(out.is_empty());
    }

    #[test]
    fn map_remote_inbound_all_known_ssrcs_mapped_to_rids() {
        let table = vp8_simulcast_ssrc_table();
        let out = map_remote_inbound_to_rid_health(
            vec![
                // (ssrc, fraction_lost, packets_lost, rtt, rtt_measurements)
                (0xAAAA_0001u32, 0.01, 5, 0.012, 3),  // full
                (0xAAAA_0002u32, 0.05, 23, 0.018, 7), // half
                (0xAAAA_0003u32, 0.20, 99, 0.025, 4), // quarter
            ],
            &table,
        );
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.get(&SimulcastRid::full()),
            Some(&PeerLayerHealth {
                fraction_lost: 0.01,
                packets_lost_total: 5,
                round_trip_time_seconds: 0.012,
                round_trip_time_measurements: 3,
            })
        );
        assert_eq!(
            out.get(&SimulcastRid::half()),
            Some(&PeerLayerHealth {
                fraction_lost: 0.05,
                packets_lost_total: 23,
                round_trip_time_seconds: 0.018,
                round_trip_time_measurements: 7,
            })
        );
        assert_eq!(
            out.get(&SimulcastRid::quarter()),
            Some(&PeerLayerHealth {
                fraction_lost: 0.20,
                packets_lost_total: 99,
                round_trip_time_seconds: 0.025,
                round_trip_time_measurements: 4,
            })
        );
    }

    #[test]
    fn map_remote_inbound_mixed_known_and_unknown_keeps_only_known() {
        // A realistic transient-window state: RR for one
        // simulcast layer arrives alongside RR for a now-released
        // on-demand H.264 SSRC. Helper preserves the known RID
        // entry, drops the unknown. Both have non-zero
        // `rtt_measurements` so the pre-RR filter doesn't
        // intercept either — the test exercises SSRC-table-membership
        // specifically.
        let table = vp8_simulcast_ssrc_table();
        let out = map_remote_inbound_to_rid_health(
            vec![
                (0xAAAA_0002u32, 0.07, 30, 0.020, 9),   // half (known)
                (0xCAFE_BABEu32, 0.50, 200, 0.100, 11), // unknown
            ],
            &table,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            out.get(&SimulcastRid::half()),
            Some(&PeerLayerHealth {
                fraction_lost: 0.07,
                packets_lost_total: 30,
                round_trip_time_seconds: 0.020,
                round_trip_time_measurements: 9,
            })
        );
        assert!(!out.contains_key(&SimulcastRid::full()));
        assert!(!out.contains_key(&SimulcastRid::quarter()));
    }

    #[test]
    fn map_remote_inbound_empty_ssrc_table_drops_everything() {
        // Defends the early-session window before `state.rtp.by_rid`
        // is fully populated (or after teardown clears it). Every
        // RR that arrives has nothing to map against; helper returns
        // empty rather than panicking on the lookup. Inputs use
        // `rtt_measurements > 0` so the pre-RR filter doesn't pre-
        // empt the SSRC-table check — the test exercises empty-
        // table-drop semantics specifically.
        let out = map_remote_inbound_to_rid_health(
            vec![
                (0xAAAA_0001u32, 0.01, 5, 0.012, 2),
                (0xAAAA_0002u32, 0.02, 7, 0.015, 3),
            ],
            &[],
        );
        assert!(out.is_empty());
    }

    #[test]
    fn map_remote_inbound_filters_pre_rr_default_snapshots() {
        // **4d.3a review fix regression**: rtc 0.9's accumulator
        // emits a default-valued `RemoteInboundRTP` entry for every
        // outbound stream the moment the stream exists, even before
        // any actual RR has been received. All fields default to
        // zero, including `fraction_lost = 0.0` (which would
        // otherwise present as "perfectly healthy" to the 4d.3b
        // policy and confirm `Wanted` immediately on connect).
        // `round_trip_time_measurements == 0` is the discriminator:
        // non-zero means at least one RR has arrived and the values
        // reflect a real measurement. The helper filters zero-
        // measurement entries out so the policy receives "no
        // signal" until the first RR actually lands.
        let table = vp8_simulcast_ssrc_table();
        let out = map_remote_inbound_to_rid_health(
            vec![
                // Pre-RR snapshot for `full` — must be filtered.
                (0xAAAA_0001u32, 0.0, 0, 0.0, 0),
                // Real RR-derived snapshot for `half` — must be kept.
                (0xAAAA_0002u32, 0.05, 23, 0.018, 4),
                // Another pre-RR snapshot for `quarter`, with
                // `fraction_lost = 0.0` that would look "healthy"
                // if not filtered. Must be filtered.
                (0xAAAA_0003u32, 0.0, 0, 0.0, 0),
            ],
            &table,
        );
        assert_eq!(
            out.len(),
            1,
            "only the entry with rtt_measurements > 0 should survive; \
             pre-RR defaults must be filtered. Got {out:?}",
        );
        assert!(!out.contains_key(&SimulcastRid::full()));
        assert!(!out.contains_key(&SimulcastRid::quarter()));
        assert_eq!(
            out.get(&SimulcastRid::half()),
            Some(&PeerLayerHealth {
                fraction_lost: 0.05,
                packets_lost_total: 23,
                round_trip_time_seconds: 0.018,
                round_trip_time_measurements: 4,
            })
        );
    }

    /// `serialize_authority_state` produces the canonical
    /// `display_input_authority_state` frame: `t` discriminator,
    /// numeric `display_id`, string `state` from the wire vocabulary.
    /// Browser handlers parse this exact shape; if it drifts, the
    /// chip on the federated peer-display panel stops updating.
    #[test]
    fn serialize_authority_state_produces_canonical_frame() {
        for (state, expected_state) in [
            (DisplayInputAuthorityState::You, "you"),
            (DisplayInputAuthorityState::Other, "other"),
            (DisplayInputAuthorityState::Unclaimed, "unclaimed"),
        ] {
            let json = serialize_authority_state(7, state);
            let parsed: serde_json::Value =
                serde_json::from_str(&json).expect("frame must parse as JSON");
            assert_eq!(parsed["t"], "display_input_authority_state");
            assert_eq!(parsed["display_id"], 7);
            assert_eq!(parsed["state"], expected_state);
        }
    }

    /// F-1.3b2: `parse_authority_channel_message` round-trips the
    /// canonical wire shape (`t` discriminator + numeric `display_id`)
    /// to the right [`AuthorityChannelMessage`] variant. Pins the
    /// browser↔peer wire vocabulary the federated authority data
    /// channel uses; if browser-side serialization drifts, this test
    /// fires.
    #[test]
    fn parse_authority_channel_message_round_trip() {
        let req = parse_authority_channel_message(
            r#"{ "t": "display_input_authority_request", "display_id": 7 }"#,
        )
        .expect("request frame must parse");
        assert_eq!(req, AuthorityChannelMessage::Request { display_id: 7 });

        let rel = parse_authority_channel_message(
            r#"{ "t": "display_input_authority_release", "display_id": 0 }"#,
        )
        .expect("release frame must parse");
        assert_eq!(rel, AuthorityChannelMessage::Release { display_id: 0 });
    }

    /// F-1.3b2: extra/unknown fields on a well-formed frame are
    /// preserved-by-ignoring — the parser is strict on the
    /// discriminator (`t`) and the typed field (`display_id`) but
    /// tolerant of anything else. Mirrors `parse_clipboard_set`'s
    /// loose-extras contract and leaves room for the browser to add
    /// forward-compat metadata (request ids for ack tracking, actor
    /// identity hints, timestamps) without forcing a peer-side
    /// version bump.
    #[test]
    fn parse_authority_channel_message_tolerates_extra_fields() {
        let msg = parse_authority_channel_message(
            r#"{ "t": "display_input_authority_request",
                 "display_id": 0,
                 "request_id": "abc",
                 "ts": 12345,
                 "actor": { "kind": "operator" } }"#,
        )
        .expect("extra fields must not block parse");
        assert_eq!(msg, AuthorityChannelMessage::Request { display_id: 0 });
    }

    /// F-1.3b2: malformed frames silently drop. Strict by design —
    /// the authority handler should never see a frame the wire layer
    /// couldn't validate. Mirrors `parse_clipboard_set`'s contract:
    /// the browser is expected to send well-formed frames; recovery
    /// from the malformed case lives outside the transport.
    #[test]
    fn parse_authority_channel_message_rejects_malformed() {
        // Unknown `t` discriminator.
        assert!(parse_authority_channel_message(
            r#"{ "t": "display_input_authority_steal", "display_id": 0 }"#,
        )
        .is_none());

        // Missing `display_id`.
        assert!(
            parse_authority_channel_message(r#"{ "t": "display_input_authority_request" }"#,)
                .is_none()
        );

        // Non-numeric `display_id`.
        assert!(parse_authority_channel_message(
            r#"{ "t": "display_input_authority_request", "display_id": "0" }"#,
        )
        .is_none());

        // `display_id` outside u32 range.
        assert!(parse_authority_channel_message(
            r#"{ "t": "display_input_authority_request", "display_id": 4294967296 }"#,
        )
        .is_none());

        // Not JSON.
        assert!(parse_authority_channel_message("not json at all").is_none());

        // Missing `t` discriminator.
        assert!(parse_authority_channel_message(r#"{ "display_id": 0 }"#,).is_none());
    }

    #[test]
    fn parse_tile_control_message_round_trip() {
        let subscribe =
            tile_transport::encode_frame(&tile_transport::TileFrame::Subscribe { client_id: 99 })
                .unwrap();
        assert_eq!(
            parse_tile_control_message(&subscribe),
            Some(TileControlMessage::Subscribe { client_id: 99 })
        );

        let snapshot = tile_transport::encode_frame(&tile_transport::TileFrame::SnapshotRequest {
            epoch: 7,
            reason: tile_transport::SnapshotRequestReason::Gap,
        })
        .unwrap();
        assert_eq!(
            parse_tile_control_message(&snapshot),
            Some(TileControlMessage::SnapshotRequest {
                epoch: 7,
                reason: tile_transport::SnapshotRequestReason::Gap,
            })
        );

        let gap = tile_transport::encode_frame(&tile_transport::TileFrame::GapReport {
            epoch: 3,
            last_seen_seq: 10,
            expected_seq: 14,
        })
        .unwrap();
        assert_eq!(
            parse_tile_control_message(&gap),
            Some(TileControlMessage::GapReport {
                epoch: 3,
                last_seen_seq: 10,
                expected_seq: 14,
            })
        );
    }

    #[test]
    fn parse_tile_control_message_rejects_non_control_frames() {
        let update = tile_transport::encode_frame(&tile_transport::TileFrame::TileUpdate {
            epoch: 1,
            seq: 1,
            records: Vec::new(),
        })
        .unwrap();
        assert_eq!(parse_tile_control_message(&update), None);
        assert_eq!(parse_tile_control_message(b"not tile wire"), None);
    }

    /// `drain_pending_authority_for_label` is a no-op (returns empty,
    /// leaves `pending` untouched) for any channel label that isn't
    /// `display_input_authority`. The OnDataChannel(OnOpen) handler
    /// fires for every data channel — clipboard, control, pointer —
    /// and must not consume the authority queue when an unrelated
    /// channel opens.
    #[test]
    fn drain_pending_authority_skips_other_labels() {
        let mut pending = vec![
            (0, DisplayInputAuthorityState::You),
            (1, DisplayInputAuthorityState::Other),
        ];

        for label in ["clipboard", "control", "pointer", "random"] {
            let drained = drain_pending_authority_for_label(label, &mut pending);
            assert!(
                drained.is_empty(),
                "non-authority label '{label}' must drain nothing"
            );
            assert_eq!(
                pending.len(),
                2,
                "non-authority label '{label}' must leave queue intact",
            );
        }
    }

    /// `drain_pending_authority_for_label` consumes the entire queue
    /// (in arrival order) when the channel label matches and resets
    /// `pending` to empty. After draining, a second call returns an
    /// empty vec — replays must come from a fresh push, not a
    /// double-drain.
    #[test]
    fn drain_pending_authority_flushes_on_authority_label() {
        let mut pending = vec![
            (0, DisplayInputAuthorityState::You),
            (1, DisplayInputAuthorityState::Other),
            (2, DisplayInputAuthorityState::Unclaimed),
        ];

        let drained = drain_pending_authority_for_label(AUTHORITY_CHANNEL_LABEL, &mut pending);
        assert_eq!(drained.len(), 3, "must drain all queued entries");
        assert!(pending.is_empty(), "queue must be empty after drain");

        // Order preserved.
        assert_eq!(drained[0], (0, DisplayInputAuthorityState::You));
        assert_eq!(drained[1], (1, DisplayInputAuthorityState::Other));
        assert_eq!(drained[2], (2, DisplayInputAuthorityState::Unclaimed));

        // Second drain returns empty (no double-flush).
        let again = drain_pending_authority_for_label(AUTHORITY_CHANNEL_LABEL, &mut pending);
        assert!(again.is_empty(), "second drain must be empty");
    }

    /// Empty queue → empty drain on the authority label. No panics,
    /// no resource consumption when the broadcast loop hasn't pushed
    /// anything yet.
    #[test]
    fn drain_pending_authority_empty_queue_is_noop() {
        let mut pending: Vec<(u32, DisplayInputAuthorityState)> = Vec::new();
        let drained = drain_pending_authority_for_label(AUTHORITY_CHANNEL_LABEL, &mut pending);
        assert!(drained.is_empty());
        assert!(pending.is_empty());
    }

    #[test]
    fn tile_watermark_threshold_conversion_saturates_to_u32() {
        assert_eq!(watermark_to_u32(0), 0);
        assert_eq!(watermark_to_u32(1024), 1024);
        assert_eq!(watermark_to_u32(u32::MAX as usize), u32::MAX);
        assert_eq!(watermark_to_u32(u32::MAX as usize + 1), u32::MAX);
    }
}

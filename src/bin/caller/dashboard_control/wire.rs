//! The control-channel wire loop: the per-peer driver task, outbound
//! event/response/frame/byte-stream senders, output draining, frame-text
//! assembly and response chunking, and the queued-frame drain.

use super::*;

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) async fn control_driver<I: rtc::interceptor::Interceptor + Send + Sync + 'static>(
    mut rtc: RTCPeerConnection<I>,
    sockets: Vec<Arc<UdpSocket>>,
    mut tcp_conn_rx: Option<mpsc::Receiver<crate::display::webrtc::AcceptedTcpConnection>>,
    tcp_advertised: Option<SocketAddr>,
    _tcp_registration: Option<crate::display::webrtc::PeerRegistration>,
    mut runtime: ControlRuntime,
    mut event_rx: tokio::sync::broadcast::Receiver<String>,
    mut command_rx: mpsc::Receiver<ControlCommand>,
    shutdown: CancellationToken,
) {
    let mut sockets_by_addr: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundPacket>(64);
    let mut forwarder_handles = Vec::new();
    for sock in sockets {
        let local = match sock.local_addr() {
            Ok(local) => local,
            Err(_) => continue,
        };
        sockets_by_addr.insert(local, Arc::clone(&sock));
        let tx = inbound_tx.clone();
        let shutdown = shutdown.clone();
        forwarder_handles.push(tokio::spawn(async move {
            let mut buf = vec![0u8; UDP_BUF_LEN];
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    recv = sock.recv_from(&mut buf) => match recv {
                        Ok((n, source)) => {
                            let pkt = InboundPacket {
                                proto: TransportProtocol::UDP,
                                source,
                                destination: local,
                                bytes: buf[..n].to_vec(),
                                received_at: Instant::now(),
                            };
                            if tx.send(pkt).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("[dashboard/control] UDP recv failed on {local}: {e}");
                            break;
                        }
                    }
                }
            }
        }));
    }
    let mut tcp_senders: HashMap<SocketAddr, TcpFrameSender> = HashMap::new();
    let mut channels: HashMap<String, rtc::data_channel::RTCDataChannelId> = HashMap::new();
    let (task_tx, mut task_rx) = mpsc::channel::<ControlTaskResponse>(64);
    let mut pending_requests: HashMap<String, CancellationToken> = HashMap::new();
    let mut outbound_queue = OutboundControlQueue::new();
    let mut inbound_uploads: HashMap<String, InboundUploadState> = HashMap::new();
    let (terminal_events_tx, mut terminal_events_rx) =
        mpsc::unbounded_channel::<serde_json::Value>();
    runtime.control_frames_tx = Some(terminal_events_tx.clone());
    // PTY output rides its own small BOUNDED lane (the tunnel twin of
    // ws_session's TERMINAL_FORWARD_LANE_CAP): the per-terminal forwarders
    // `send().await` here, and the driver drains it only while the control
    // channel's SCTP buffer sits below the high watermark. When the wire
    // congests (frozen tab), the lane fills, the forwarders park, and
    // terminal.rs's per-listener drop-oldest bound re-engages — instead of
    // bulk PTY output growing the unbounded SCTP pending queue at PTY
    // rate. The low-rate control_frames lane (acks for share/egress/
    // presence) stays unbounded.
    let (terminal_output_tx, mut terminal_output_rx) =
        mpsc::channel::<serde_json::Value>(TERMINAL_OUTPUT_LANE_CAP);
    let mut terminal_lane_paused = false;
    let mut terminal_forwarders: HashMap<(String, String), tokio::task::JoinHandle<()>> =
        HashMap::new();
    // Per-connection ordered display-input lane (F1): `display_input`
    // frames are handed to ONE forwarder task in dispatch order instead
    // of spawning a task per event (which raced kd/ku / md/mu pairs
    // across runtime workers). Dropping `display_input_tx` when this
    // driver exits ends the forwarder; the shared shutdown token covers
    // the cancel path.
    let display_input_tx = spawn_display_input_forwarder(runtime.clone(), shutdown.clone());
    let mut display_authority_rx = runtime
        .display_authority
        .as_ref()
        .map(DashboardDisplayAuthorityBridge::subscribe);
    let mut drop_stats = TransmitDropStats::default();
    let mut authority_tick = tokio::time::interval(LIVE_AUTHORITY_RECHECK_INTERVAL);
    authority_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if !runtime.grant.opening_authority_is_current() {
            shutdown.cancel();
            break;
        }
        let timeout_at = match drain_control_outputs(
            &mut rtc,
            &sockets_by_addr,
            &mut tcp_senders,
            &mut drop_stats,
            &mut channels,
            &mut runtime,
            &task_tx,
            &mut pending_requests,
            &mut outbound_queue,
            &mut inbound_uploads,
            &terminal_events_tx,
            &terminal_output_tx,
            &mut terminal_forwarders,
            &display_input_tx,
            &mut terminal_lane_paused,
        )
        .await
        {
            Ok(timeout_at) => timeout_at,
            Err(()) => {
                shutdown.cancel();
                break;
            }
        };
        let timeout_dur = timeout_at
            .saturating_duration_since(Instant::now())
            .max(Duration::from_micros(1));

        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = authority_tick.tick() => {
                if !runtime.grant.opening_authority_is_current() {
                    shutdown.cancel();
                    break;
                }
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
                    eprintln!("[dashboard/control] handle_read failed: {e:?}");
                    shutdown.cancel();
                    break;
                }
            }
            Some(accepted) = async {
                match tcp_conn_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let crate::display::webrtc::AcceptedTcpConnection {
                    remote_addr,
                    local_addr: real_local,
                    first_frame,
                    stream,
                } = accepted;
                let Some(fake_local) = tcp_advertised else {
                    eprintln!(
                        "[dashboard/control] TCP connection from {remote_addr} but no advertised local configured, dropping"
                    );
                    continue;
                };
                eprintln!(
                    "[dashboard/control] ICE-TCP connection from {remote_addr} -> {real_local} (rtc sees {fake_local})"
                );
                let (read_half, write_half) = stream.into_split();
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
                                        crate::display::webrtc::write_rfc4571_frame(&mut write_half, &contents).await
                                    {
                                        eprintln!(
                                            "[dashboard/control] ICE-TCP writer for {remote_addr} failed, tearing down connection: {e}"
                                        );
                                        writer_shutdown.cancel();
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                    }
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut write_half).await;
                });

                let reader_tx = inbound_tx.clone();
                let reader_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let mut read_half = read_half;
                    loop {
                        tokio::select! {
                            _ = reader_shutdown.cancelled() => break,
                            frame = crate::display::webrtc::read_rfc4571_frame(&mut read_half) => match frame {
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
                                        "[dashboard/control] ICE-TCP reader for {remote_addr} exiting: {e}"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                });

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
                    eprintln!("[dashboard/control] handle_read(first TCP frame) failed: {e:?}");
                    shutdown.cancel();
                    break;
                }
            }
            Some(cmd) = command_rx.recv() => {
                match cmd {
                    ControlCommand::AddIceCandidate(candidate) => {
                        let init = RTCIceCandidateInit {
                            candidate,
                            sdp_mid: None,
                            sdp_mline_index: None,
                            username_fragment: None,
                            url: None,
                        };
                        if let Err(e) = rtc.add_remote_candidate(init) {
                            eprintln!("[dashboard/control] parse remote candidate failed: {e}");
                        }
                    }
                }
                let _ = rtc.handle_timeout(Instant::now());
            }
            Some(task_response) = task_rx.recv() => {
                if !runtime.grant.opening_authority_is_current() {
                    shutdown.cancel();
                    break;
                }
                if pending_requests.contains_key(&task_response.id) {
                    let task_id = task_response.id.clone();
                    let done = task_response.done;
                    send_control_task_response(
                        &mut rtc,
                        &channels,
                        &mut outbound_queue,
                        runtime.response_credit_enabled,
                        task_response,
                    );
                    if done {
                        pending_requests.remove(&task_id);
                    }
                }
                let _ = rtc.handle_timeout(Instant::now());
            }
            event = event_rx.recv(), if runtime.events_subscribed => {
                if !runtime.grant.opening_authority_is_current() {
                    shutdown.cancel();
                    break;
                }
                match event {
                    Ok(line) => {
                        let owner = runtime.grant.has_owner_dashboard_authority();
                        if !owner
                            && DashboardControlGrant::dashboard_event_line_requires_owner(&line)
                        {
                            continue;
                        }
                        if !owner {
                            let private = {
                                let active_session = runtime.shared_session.read().await;
                                match active_session.session_registry.as_ref() {
                                    Some(session_registry) => {
                                        let registry = session_registry.read().await;
                                        runtime
                                            .grant
                                            .dashboard_event_targets_hidden_display(
                                                &line, &registry,
                                            )
                                    }
                                    None => false,
                                }
                            };
                            if private {
                                continue;
                            }
                        }
                        runtime.events_sent = runtime.events_sent.saturating_add(1);
                        let frame = event_lane_frame(runtime.events_sent, &line);
                        send_control_text(&mut rtc, &channels, frame);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        let frame = serde_json::json!({
                            "t": "event_gap",
                            "skipped": skipped,
                        });
                        send_control_text(&mut rtc, &channels, frame.to_string());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        runtime.events_subscribed = false;
                    }
                }
                let _ = rtc.handle_timeout(Instant::now());
            }
            Some(frame) = terminal_events_rx.recv() => {
                if !runtime.grant.opening_authority_is_current() {
                    shutdown.cancel();
                    break;
                }
                send_control_text(&mut rtc, &channels, frame.to_string());
                let _ = rtc.handle_timeout(Instant::now());
            }
            // Bounded PTY-output lane, gated on the control channel's SCTP
            // buffered-amount watermark: while paused the forwarders park
            // on the full lane and terminal.rs's drop-oldest bound holds
            // the memory line.
            Some(frame) = terminal_output_rx.recv(), if !terminal_lane_paused => {
                if !runtime.grant.opening_authority_is_current() {
                    shutdown.cancel();
                    break;
                }
                send_control_text(&mut rtc, &channels, frame.to_string());
                let _ = rtc.handle_timeout(Instant::now());
            }
            authority = async {
                match display_authority_rx.as_mut() {
                    Some(rx) => Some(rx.recv().await),
                    None => std::future::pending::<Option<Result<u32, tokio::sync::broadcast::error::RecvError>>>().await,
                }
            }, if runtime.events_subscribed && display_authority_rx.is_some() => {
                if !runtime.grant.opening_authority_is_current() {
                    shutdown.cancel();
                    break;
                }
                match authority {
                    Some(Ok(display_id)) => {
                        send_display_authority_event(&mut rtc, &channels, &mut runtime, display_id);
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                        let snapshots = display_authority_snapshot_frames(&runtime).await;
                        for frame in snapshots {
                            send_event_payload(&mut rtc, &channels, &mut runtime, frame);
                        }
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) | None => {
                        display_authority_rx = runtime
                            .display_authority
                            .as_ref()
                            .map(DashboardDisplayAuthorityBridge::subscribe);
                    }
                }
                let _ = rtc.handle_timeout(Instant::now());
            }
            _ = tokio::time::sleep(timeout_dur) => {
                if let Err(e) = rtc.handle_timeout(Instant::now()) {
                    eprintln!("[dashboard/control] handle_timeout failed: {e:?}");
                    shutdown.cancel();
                    break;
                }
            }
        }
    }

    // Invalidate every interactive display guard minted by this control
    // session before any other teardown can yield. The display transport is
    // separate WebRTC and may reap a beat later; it must not retain input or
    // clipboard authority during that window.
    shutdown.cancel();
    remove_dashboard_display_peers(&runtime).await;
    for (_, token) in pending_requests {
        token.cancel();
    }
    for (_, handle) in terminal_forwarders {
        handle.abort();
        let _ = handle.await;
    }
    // Egress relays die with their session: no more frames can arrive,
    // so drop the registration and fail any in-flight relayed requests.
    crate::credential_egress::unregister_session(&runtime.session_id);
    runtime.tabs.unregister(&runtime.session_id);
    if let Some(bridge) = &runtime.display_authority {
        bridge.cleanup(&runtime.session_id);
    }
    if let Some(bridge) = &runtime.presence {
        bridge.cleanup(runtime.session_id.clone()).await;
    }
    for handle in forwarder_handles {
        let _ = handle.await;
    }
}

/// Build one event-lane frame `{"t":"event","seq":N,"payload":<line>}` by
/// splicing the already-serialized outbound line into the envelope instead
/// of parse→wrap→re-serialize per event per tunnel: every producer into the
/// outbound broadcast serializes JSON objects, so the line embeds verbatim.
/// Lines that don't look like a JSON object/array (never produced today)
/// take the legacy parse path and wrap as `{"raw": <line>}`.
pub(crate) fn event_lane_frame(seq: u64, line: &str) -> String {
    let trimmed = line.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let mut frame = String::with_capacity(line.len() + 40);
        frame.push_str("{\"t\":\"event\",\"seq\":");
        frame.push_str(&seq.to_string());
        frame.push_str(",\"payload\":");
        frame.push_str(line);
        frame.push('}');
        return frame;
    }
    let payload = serde_json::from_str::<serde_json::Value>(line)
        .unwrap_or_else(|_| serde_json::json!({ "raw": line }));
    serde_json::json!({
        "t": "event",
        "seq": seq,
        "payload": payload,
    })
    .to_string()
}

pub(crate) fn send_event_payload<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    runtime: &mut ControlRuntime,
    payload: serde_json::Value,
) {
    runtime.events_sent = runtime.events_sent.saturating_add(1);
    let frame = serde_json::json!({
        "t": "event",
        "seq": runtime.events_sent,
        "payload": payload,
    });
    send_control_text(rtc, channels, frame.to_string());
}

pub(crate) fn send_display_authority_event<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    runtime: &mut ControlRuntime,
    display_id: u32,
) {
    if !runtime.grant.has_owner_dashboard_authority() {
        let Ok(active_session) = runtime.shared_session.try_read() else {
            return;
        };
        let Some(session_registry) = active_session.session_registry.as_ref() else {
            return;
        };
        let Ok(registry) = session_registry.try_read() else {
            return;
        };
        if runtime
            .grant
            .display_session(&registry, display_id)
            .is_none()
        {
            return;
        }
    }
    let Some(bridge) = runtime.display_authority.as_ref() else {
        return;
    };
    if let Some(frame) = bridge.state_frame(&runtime.session_id, display_id) {
        send_event_payload(rtc, channels, runtime, frame);
    }
}

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) async fn drain_control_outputs<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    sockets_by_addr: &HashMap<SocketAddr, Arc<UdpSocket>>,
    tcp_senders: &mut HashMap<SocketAddr, TcpFrameSender>,
    drop_stats: &mut TransmitDropStats,
    channels: &mut HashMap<String, rtc::data_channel::RTCDataChannelId>,
    runtime: &mut ControlRuntime,
    task_tx: &mpsc::Sender<ControlTaskResponse>,
    pending_requests: &mut HashMap<String, CancellationToken>,
    outbound_queue: &mut OutboundControlQueue,
    inbound_uploads: &mut HashMap<String, InboundUploadState>,
    terminal_events_tx: &mpsc::UnboundedSender<serde_json::Value>,
    terminal_output_tx: &mpsc::Sender<serde_json::Value>,
    terminal_forwarders: &mut HashMap<(String, String), tokio::task::JoinHandle<()>>,
    display_input_tx: &DisplayInputForwarder,
    terminal_lane_paused: &mut bool,
) -> Result<Instant, ()> {
    while let Some(t) = rtc.poll_write() {
        // Route by connection first, engine stamp second: rtc < 0.9.1
        // stamped DTLS/SCTP transmits `TransportProtocol::UDP` even on a
        // TCP pair, misrouting every post-ICE packet (webrtc-rs/rtc#109,
        // fixed by our upstream PR #110, released as 0.9.1 — which we
        // run). Tuple-first routing stays regardless: the tuple is the
        // engine's own connection key (rtc-shared `FiveTuple`), and it
        // keeps any future stamping regression from presenting as a
        // silent DTLS timeout again.
        if let Some(sender) = tcp_senders.get(&t.transport.peer_addr) {
            let contents = t.message.to_vec();
            match sender.try_send(contents) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tcp_senders.remove(&t.transport.peer_addr);
                }
            }
            continue;
        }
        if t.transport.transport_protocol == TransportProtocol::TCP {
            // TCP-stamped transmit with no live stream for the tuple: the
            // connection is gone and there is nothing to write to.
            drop_stats.tcp_without_stream += 1;
            continue;
        }
        if t.transport.local_addr.is_ipv4() != t.transport.peer_addr.is_ipv4() {
            drop_stats.cross_family += 1;
            continue;
        }
        if t.transport.local_addr.ip().is_loopback() != t.transport.peer_addr.ip().is_loopback() {
            drop_stats.loopback_mismatch += 1;
            continue;
        }
        let Some(sock) = sockets_by_addr.get(&t.transport.local_addr) else {
            drop_stats.unknown_udp_source += 1;
            eprintln!(
                "[dashboard/control] UDP transmit from unknown source {}, dropping",
                t.transport.local_addr
            );
            continue;
        };
        if let Err(e) = sock.send_to(&t.message, t.transport.peer_addr).await {
            eprintln!(
                "[dashboard/control] udp send {} -> {} failed: {e}",
                t.transport.local_addr, t.transport.peer_addr
            );
        }
    }

    while let Some(message) = rtc.poll_read() {
        let RTCMessage::DataChannelMessage(cid, msg) = message else {
            continue;
        };
        let label = channels
            .iter()
            .find_map(|(label, id)| (*id == cid).then(|| label.clone()));
        if label.as_deref() != Some(CONTROL_CHANNEL_LABEL) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&msg.data) else {
            continue;
        };
        if let Some(response) = control_frame_response(
            text,
            runtime,
            task_tx,
            pending_requests,
            outbound_queue,
            inbound_uploads,
            terminal_events_tx,
            terminal_output_tx,
            terminal_forwarders,
            display_input_tx,
        ) {
            send_control_frame(
                rtc,
                channels,
                outbound_queue,
                runtime.response_credit_enabled,
                response,
            );
        }
    }

    while let Some(event) = rtc.poll_event() {
        match event {
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(cid)) => {
                let label = rtc
                    .data_channel(cid)
                    .map(|channel| channel.label().to_string())
                    .unwrap_or_else(|| format!("channel-{cid}"));
                eprintln!("[dashboard/control] data channel open: {label}");
                if label == CONTROL_CHANNEL_LABEL {
                    // Arm the SCTP buffered-amount watermarks that gate the
                    // bounded terminal-output lane (the display pipeline's
                    // event-driven backpressure pattern).
                    if let Some(mut channel) = rtc.data_channel(cid) {
                        channel.set_buffered_amount_high_threshold(watermark_to_u32(
                            TERMINAL_LANE_BUFFERED_HIGH_WATERMARK_BYTES,
                        ));
                        channel.set_buffered_amount_low_threshold(watermark_to_u32(
                            TERMINAL_LANE_BUFFERED_LOW_WATERMARK_BYTES,
                        ));
                    }
                    *terminal_lane_paused = false;
                }
                channels.insert(label, cid);
            }
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnClose(cid)) => {
                if channels.get(CONTROL_CHANNEL_LABEL).copied() == Some(cid) {
                    // Never leave the terminal lane parked behind a channel
                    // that can no longer drain.
                    *terminal_lane_paused = false;
                }
                channels.retain(|_, id| *id != cid);
            }
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnBufferedAmountHigh(
                cid,
            )) => {
                if channels.get(CONTROL_CHANNEL_LABEL).copied() == Some(cid) {
                    *terminal_lane_paused = true;
                }
            }
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnBufferedAmountLow(
                cid,
            )) => {
                if channels.get(CONTROL_CHANNEL_LABEL).copied() == Some(cid) {
                    *terminal_lane_paused = false;
                }
            }
            RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                eprintln!("[dashboard/control] connection: {state:?}");
                if matches!(
                    state,
                    rtc::peer_connection::state::RTCPeerConnectionState::Failed
                        | rtc::peer_connection::state::RTCPeerConnectionState::Closed
                ) {
                    if drop_stats.any() {
                        eprintln!(
                            "[dashboard/control] transmit drops this session: {} cross-family, {} loopback-mismatch, {} unknown-udp-source, {} tcp-without-stream",
                            drop_stats.cross_family,
                            drop_stats.loopback_mismatch,
                            drop_stats.unknown_udp_source,
                            drop_stats.tcp_without_stream
                        );
                    }
                    return Err(());
                }
            }
            RTCPeerConnectionEvent::OnIceConnectionStateChangeEvent(state) => {
                eprintln!("[dashboard/control] ICE: {state:?}");
            }
            _ => {}
        }
    }

    drain_queued_control_frames(rtc, channels, outbound_queue);

    Ok(rtc
        .poll_timeout()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400)))
}

pub(crate) fn send_control_text<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    text: String,
) {
    let Some(cid) = channels.get(CONTROL_CHANNEL_LABEL).copied() else {
        return;
    };
    if let Some(mut channel) = rtc.data_channel(cid) {
        if let Err(e) = channel.send_text(text) {
            eprintln!("[dashboard/control] data channel write failed: {e:?}");
        }
    }
}

pub(crate) fn send_control_task_response<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    outbound_queue: &mut OutboundControlQueue,
    response_credit_enabled: bool,
    response: ControlTaskResponse,
) {
    if let Some(byte_stream) = response.byte_stream {
        send_control_byte_stream(
            rtc,
            channels,
            outbound_queue,
            response_credit_enabled,
            byte_stream,
        );
    } else {
        send_control_frame(
            rtc,
            channels,
            outbound_queue,
            response_credit_enabled,
            response.frame,
        );
    }
}

pub(crate) fn send_control_frame<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    outbound_queue: &mut OutboundControlQueue,
    response_credit_enabled: bool,
    frame: serde_json::Value,
) {
    let request_id = frame
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match control_frame_text_parts(
        frame,
        CONTROL_RESPONSE_CHUNK_THRESHOLD_BYTES,
        CONTROL_RESPONSE_CHUNK_BYTES,
    ) {
        ControlFrameTexts::Immediate(frames) => {
            for text in frames {
                if response_credit_enabled && !outbound_queue.is_empty() && !request_id.is_empty() {
                    outbound_queue.enqueue_immediate(request_id.clone(), text);
                } else {
                    send_control_text(rtc, channels, text);
                }
            }
            drain_queued_control_frames(rtc, channels, outbound_queue);
        }
        ControlFrameTexts::Chunked(plan) => {
            if response_credit_enabled {
                if outbound_queue.enqueue_chunked(plan) {
                    drain_queued_control_frames(rtc, channels, outbound_queue);
                } else {
                    // Admission control: the per-connection queue byte cap
                    // is exhausted — answer the request instead of pinning
                    // another payload behind a quiet client.
                    send_control_text(
                        rtc,
                        channels,
                        dashboard_control_error_response(
                            request_id,
                            "outbound response queue is full",
                        )
                        .to_string(),
                    );
                }
            } else {
                for text in plan.render_all() {
                    send_control_text(rtc, channels, text);
                }
            }
        }
    }
}

pub(crate) fn send_control_byte_stream<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    outbound_queue: &mut OutboundControlQueue,
    response_credit_enabled: bool,
    byte_stream: ControlByteStream,
) {
    match byte_stream_frame_text_parts(byte_stream, CONTROL_BYTE_STREAM_CHUNK_BYTES) {
        ControlFrameTexts::Immediate(frames) => {
            for text in frames {
                send_control_text(rtc, channels, text);
            }
        }
        ControlFrameTexts::Chunked(plan) => {
            if response_credit_enabled {
                let request_id = plan.request_id.clone();
                if outbound_queue.enqueue_chunked(plan) {
                    drain_queued_control_frames(rtc, channels, outbound_queue);
                } else {
                    // Admission control at the queue byte cap (see
                    // `send_control_frame`).
                    send_control_text(
                        rtc,
                        channels,
                        dashboard_control_error_response(
                            request_id,
                            "outbound response queue is full",
                        )
                        .to_string(),
                    );
                }
            } else {
                for text in plan.render_all() {
                    send_control_text(rtc, channels, text);
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn control_frame_texts(frame: serde_json::Value) -> Vec<String> {
    match control_frame_text_parts(
        frame,
        CONTROL_RESPONSE_CHUNK_THRESHOLD_BYTES,
        CONTROL_RESPONSE_CHUNK_BYTES,
    ) {
        ControlFrameTexts::Immediate(frames) => frames,
        ControlFrameTexts::Chunked(plan) => plan.render_all(),
    }
}

pub(crate) fn byte_stream_frame_text_parts(
    byte_stream: ControlByteStream,
    chunk_bytes: usize,
) -> ControlFrameTexts {
    let request_id = byte_stream.id;
    let chunk_id = byte_stream.stream_id;
    if request_id.is_empty() || chunk_id.is_empty() || chunk_bytes == 0 {
        return ControlFrameTexts::Immediate(Vec::new());
    }

    let total_bytes = byte_stream.bytes.len();
    let chunk_count = total_bytes.div_ceil(chunk_bytes);
    let start = serde_json::json!({
        "t": "byte_stream_start",
        "id": request_id,
        "stream_id": chunk_id,
        "encoding": "base64",
        "content_type": byte_stream.content_type,
        "filename": byte_stream.filename,
        "total_bytes": total_bytes,
        "chunks": chunk_count,
    })
    .to_string();
    let end = serde_json::json!({
        "t": "byte_stream_end",
        "id": request_id,
        "stream_id": chunk_id,
        "ok": true,
        "chunks": chunk_count,
        "result": byte_stream.result,
    })
    .to_string();
    // The payload moves into the plan raw; chunk frames render lazily at
    // send time (see `ChunkedFramePlan`).
    ControlFrameTexts::Chunked(ChunkedFramePlan::byte_stream(
        request_id,
        chunk_id,
        start,
        end,
        byte_stream.bytes,
        chunk_bytes,
    ))
}

#[cfg(test)]
pub(crate) fn byte_stream_frame_texts(
    byte_stream: ControlByteStream,
    chunk_bytes: usize,
) -> Vec<String> {
    match byte_stream_frame_text_parts(byte_stream, chunk_bytes) {
        ControlFrameTexts::Immediate(frames) => frames,
        ControlFrameTexts::Chunked(plan) => plan.render_all(),
    }
}

pub(crate) fn control_frame_text_parts(
    frame: serde_json::Value,
    threshold_bytes: usize,
    chunk_bytes: usize,
) -> ControlFrameTexts {
    let text = frame.to_string();
    let frame_type = frame.get("t").and_then(|v| v.as_str());
    if !matches!(frame_type, Some("response") | Some("stream_event"))
        || text.len() <= threshold_bytes
        || chunk_bytes == 0
    {
        return ControlFrameTexts::Immediate(vec![text]);
    }
    let request_id = frame
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if request_id.is_empty() {
        return ControlFrameTexts::Immediate(vec![text]);
    }
    let chunk_id = frame
        .get("chunk_id")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            frame
                .get("seq")
                .and_then(|v| v.as_u64())
                .map(|seq| format!("{request_id}:{seq}"))
        })
        .unwrap_or_else(|| request_id.clone());

    let total_bytes = text.len();
    let chunk_count = total_bytes.div_ceil(chunk_bytes);
    let start = serde_json::json!({
        "t": "response_start",
        "id": request_id,
        "chunk_id": chunk_id,
        "encoding": "base64-json-frame",
        "total_bytes": total_bytes,
        "chunks": chunk_count,
    })
    .to_string();
    let end = serde_json::json!({
        "t": "response_end",
        "id": request_id,
        "chunk_id": chunk_id,
        "chunks": chunk_count,
    })
    .to_string();
    // The serialized response moves into the plan raw; chunk frames render
    // lazily at send time (see `ChunkedFramePlan`).
    ControlFrameTexts::Chunked(ChunkedFramePlan::response(
        request_id,
        chunk_id,
        start,
        end,
        text.into_bytes(),
        chunk_bytes,
    ))
}

#[cfg(test)]
pub(crate) fn chunk_control_response_frame(
    frame: serde_json::Value,
    threshold_bytes: usize,
    chunk_bytes: usize,
) -> Vec<String> {
    match control_frame_text_parts(frame, threshold_bytes, chunk_bytes) {
        ControlFrameTexts::Immediate(frames) => frames,
        ControlFrameTexts::Chunked(plan) => plan.render_all(),
    }
}

pub(crate) fn drain_queued_control_frames<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    outbound_queue: &mut OutboundControlQueue,
) {
    loop {
        let mut pop_front = false;
        let mut completed = false;
        match outbound_queue.frames.front_mut() {
            Some(QueuedControlFrame::Immediate { .. }) => {
                pop_front = true;
            }
            Some(QueuedControlFrame::Chunked(queued)) => {
                if !queued.started {
                    // Sent exactly once; taking it avoids the clone (the
                    // byte accounting was memoized at enqueue).
                    let start = std::mem::take(&mut queued.plan.start);
                    queued.started = true;
                    send_control_text(rtc, channels, start);
                }
                while queued.credit > 0 {
                    // Rendered lazily per credit — never materialized up
                    // front, never cloned at send.
                    let Some(text) = queued.plan.render_chunk(queued.next_chunk) else {
                        break;
                    };
                    queued.next_chunk += 1;
                    queued.credit -= 1;
                    send_control_text(rtc, channels, text);
                }
                if queued.next_chunk >= queued.plan.chunk_count() {
                    completed = true;
                }
            }
            None => break,
        }
        if completed {
            if let Some(QueuedControlFrame::Chunked(queued)) = outbound_queue.pop_front() {
                send_control_text(rtc, channels, queued.plan.end);
            }
            continue;
        }
        if pop_front {
            if let Some(QueuedControlFrame::Immediate { text, .. }) = outbound_queue.pop_front() {
                send_control_text(rtc, channels, text);
            }
            continue;
        }
        break;
    }
}

pub(crate) fn dashboard_control_error_response(
    id: String,
    message: impl Into<String>,
) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": false,
        "error": message.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spliced fast path must be byte-equivalent (as JSON) to the old
    /// parse→wrap→serialize path: same `t`/`seq`, payload embedded as the
    /// parsed object, no double-encoding.
    #[test]
    fn event_lane_frame_splices_serialized_lines_without_reparsing() {
        let line = r#"{"event":"status","session_id":"s-1","turn":3}"#;
        let frame = event_lane_frame(42, line);
        let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(parsed["t"], "event");
        assert_eq!(parsed["seq"], 42);
        assert_eq!(parsed["payload"]["event"], "status");
        assert_eq!(parsed["payload"]["session_id"], "s-1");
        assert_eq!(parsed["payload"]["turn"], 3);
        // The payload text is embedded verbatim — proof no reserialization
        // (which would reorder keys) happened on the fast path.
        assert!(frame.contains(line));
    }

    #[test]
    fn event_lane_frame_wraps_non_json_lines_as_raw() {
        let frame = event_lane_frame(7, "not json at all");
        let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(parsed["t"], "event");
        assert_eq!(parsed["seq"], 7);
        assert_eq!(parsed["payload"]["raw"], "not json at all");
    }

    #[test]
    fn oversized_response_frames_are_chunked_and_reassemble() {
        let frame = serde_json::json!({
            "t": "response",
            "id": "large-1",
            "ok": true,
            "result": {
                "text": "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
            }
        });
        let original = frame.to_string();
        let frames = chunk_control_response_frame(frame, 40, 12);
        assert!(frames.len() > 3, "expected start/chunks/end frames");

        let start: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(start["t"], "response_start");
        assert_eq!(start["id"], "large-1");
        assert_eq!(start["encoding"], "base64-json-frame");
        assert_eq!(start["total_bytes"], original.len());

        let end: serde_json::Value = serde_json::from_str(frames.last().unwrap()).unwrap();
        assert_eq!(end["t"], "response_end");
        assert_eq!(end["id"], "large-1");
        assert_eq!(end["chunks"], start["chunks"]);

        let mut bytes = Vec::new();
        for (seq, text) in frames[1..frames.len() - 1].iter().enumerate() {
            let chunk: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(chunk["t"], "response_chunk");
            assert_eq!(chunk["id"], "large-1");
            assert_eq!(chunk["seq"], seq);
            let encoded = chunk["data"].as_str().unwrap();
            bytes.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .unwrap(),
            );
        }
        assert_eq!(String::from_utf8(bytes).unwrap(), original);
    }

    #[test]
    fn oversized_stream_event_frames_are_chunked_with_chunk_ids() {
        let frame = serde_json::json!({
            "t": "stream_event",
            "id": "stream-1",
            "seq": 7,
            "chunk_id": "stream-1:7",
            "event": {
                "type": "replace",
                "sessions": ["x".repeat(128)]
            }
        });
        let frames = chunk_control_response_frame(frame.clone(), 40, 24);
        assert!(frames.len() > 3, "expected stream event chunking");

        let start: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(start["t"], "response_start");
        assert_eq!(start["id"], "stream-1");
        assert_eq!(start["chunk_id"], "stream-1:7");

        let end: serde_json::Value = serde_json::from_str(frames.last().unwrap()).unwrap();
        assert_eq!(end["t"], "response_end");
        assert_eq!(end["id"], "stream-1");
        assert_eq!(end["chunk_id"], "stream-1:7");

        let mut bytes = Vec::new();
        for text in &frames[1..frames.len() - 1] {
            let chunk: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(chunk["t"], "response_chunk");
            assert_eq!(chunk["id"], "stream-1");
            assert_eq!(chunk["chunk_id"], "stream-1:7");
            bytes.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(chunk["data"].as_str().unwrap())
                    .unwrap(),
            );
        }
        assert_eq!(String::from_utf8(bytes).unwrap(), frame.to_string());
    }

    #[test]
    fn byte_stream_frames_are_chunked_and_credit_addressable() {
        let bytes: Vec<u8> = (0..73).map(|i| (i % 251) as u8).collect();
        let stream = ControlByteStream {
            id: "download-1".to_string(),
            stream_id: "download-1:file".to_string(),
            content_type: "application/octet-stream".to_string(),
            filename: Some("artifact.bin".to_string()),
            bytes: bytes.clone(),
            result: serde_json::json!({
                "ok": true,
                "filename": "artifact.bin",
                "size": bytes.len(),
            }),
        };
        let frames = byte_stream_frame_texts(stream, 13);
        assert_eq!(frames.len(), 8, "expected start + 6 chunks + end");

        let start: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(start["t"], "byte_stream_start");
        assert_eq!(start["id"], "download-1");
        assert_eq!(start["stream_id"], "download-1:file");
        assert_eq!(start["encoding"], "base64");
        assert_eq!(start["content_type"], "application/octet-stream");
        assert_eq!(start["filename"], "artifact.bin");
        assert_eq!(start["total_bytes"], bytes.len());
        assert_eq!(start["chunks"], 6);

        let mut decoded = Vec::new();
        for (seq, text) in frames[1..frames.len() - 1].iter().enumerate() {
            let chunk: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(chunk["t"], "byte_stream_chunk");
            assert_eq!(chunk["id"], "download-1");
            assert_eq!(chunk["stream_id"], "download-1:file");
            assert_eq!(chunk["seq"], seq);
            decoded.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(chunk["data"].as_str().unwrap())
                    .unwrap(),
            );
        }
        assert_eq!(decoded, bytes);

        let end: serde_json::Value = serde_json::from_str(frames.last().unwrap()).unwrap();
        assert_eq!(end["t"], "byte_stream_end");
        assert_eq!(end["id"], "download-1");
        assert_eq!(end["stream_id"], "download-1:file");
        assert_eq!(end["chunks"], 6);
        assert_eq!(end["result"]["filename"], "artifact.bin");
    }

    #[test]
    fn default_response_chunks_stay_below_datachannel_edge() {
        let frame = serde_json::json!({
            "t": "response",
            "id": "large-2",
            "ok": true,
            "result": {
                "text": "x".repeat(CONTROL_RESPONSE_CHUNK_THRESHOLD_BYTES * 4)
            }
        });
        let frames = control_frame_texts(frame);
        assert!(frames.len() > 3, "expected default chunking");

        for text in frames {
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            if parsed["t"] == "response_chunk" {
                assert!(
                    text.len() < 32 * 1024,
                    "chunk frame is too close to common DataChannel limits: {} bytes",
                    text.len()
                );
            }
        }
    }

    #[test]
    fn small_or_non_response_frames_are_not_chunked() {
        let response = serde_json::json!({"t":"response","id":"small","ok":true,"result":{}});
        assert_eq!(chunk_control_response_frame(response, 4096, 16).len(), 1);
        let event = serde_json::json!({"t":"event","id":"e1","payload":{"text":"large enough"}});
        assert_eq!(chunk_control_response_frame(event, 1, 1).len(), 1);
    }

    /// The outbound queue keeps exact byte accounting across enqueue,
    /// cancel, and drain-to-empty, and refuses chunked admission once the
    /// resident bytes reach the per-connection cap.
    #[test]
    fn outbound_queue_accounts_bytes_and_caps_chunked_admission() {
        fn plan(id: &str, payload_len: usize) -> ChunkedFramePlan {
            ChunkedFramePlan::response(
                id.to_string(),
                format!("{id}:0"),
                "start".into(),
                "end".into(),
                vec![b'x'; payload_len],
                CONTROL_RESPONSE_CHUNK_BYTES,
            )
        }

        let mut queue = OutboundControlQueue::new();
        assert!(queue.enqueue_chunked(plan("a", 1024)));
        queue.enqueue_immediate("b".into(), "{\"t\":\"response\"}".into());
        assert!(queue.queued_bytes > 1024);

        // Cancel debits exactly what was credited.
        assert!(queue.cancel("a"));
        assert!(queue.cancel("b"));
        assert_eq!(queue.queued_bytes, 0);
        assert!(queue.frames.is_empty());

        // Admission control refuses chunked frames at the cap (saturate
        // the accounting directly rather than allocating 128 MiB).
        queue.queued_bytes = CONTROL_OUTBOUND_QUEUE_MAX_BYTES;
        assert!(!queue.enqueue_chunked(plan("c", 1024)));
        assert!(queue.frames.is_empty());
        queue.queued_bytes = 0;

        // Draining to empty returns the accounting to zero.
        assert!(queue.enqueue_chunked(plan("d", 4096)));
        queue.enqueue_immediate("e".into(), "{}".into());
        while queue.pop_front().is_some() {}
        assert_eq!(queue.queued_bytes, 0);
    }

    /// A queued chunked frame renders each chunk lazily and exactly once,
    /// byte-identical to the eager path (`render_all` is the eager
    /// rendering the reassembly tests above already pin).
    #[test]
    fn chunked_plan_renders_lazily_with_stable_sequence() {
        let payload: Vec<u8> = (0..100u8).collect();
        let plan = ChunkedFramePlan::byte_stream(
            "dl".into(),
            "dl:file".into(),
            "start".into(),
            "end".into(),
            payload.clone(),
            16,
        );
        assert_eq!(plan.chunk_count(), 7);
        assert!(plan.render_chunk(7).is_none());
        let mut decoded = Vec::new();
        for seq in 0..plan.chunk_count() {
            let text = plan.render_chunk(seq).unwrap();
            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(frame["t"], "byte_stream_chunk");
            assert_eq!(frame["seq"], seq);
            decoded.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(frame["data"].as_str().unwrap())
                    .unwrap(),
            );
        }
        assert_eq!(decoded, payload);
    }
}

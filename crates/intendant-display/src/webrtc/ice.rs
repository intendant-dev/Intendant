//! ICE candidate gathering: host/srflx/relay candidate construction, the
//! STUN Binding request/response wire helpers, STUN/TURN server resolution
//! from [`IceConfig`], and the long-lived TURN relay allocation task.

use super::*;

pub(crate) fn host_candidate_init(addr: SocketAddr, protocol: RTCIceProtocol) -> RTCIceCandidateInit {
    let (foundation, proto, priority, tcp_suffix) = match protocol {
        RTCIceProtocol::Udp => ("1", "udp", 2_130_706_431u32, ""),
        RTCIceProtocol::Tcp => ("9001", "tcp", 1_677_721_855u32, " tcptype passive"),
        RTCIceProtocol::Unspecified => ("1", "udp", 1_000_000_000u32, ""),
    };
    RTCIceCandidateInit {
        candidate: format!(
            "candidate:{foundation} 1 {proto} {priority} {} {} typ host{tcp_suffix} generation 0",
            addr.ip(),
            addr.port()
        ),
        sdp_mid: Some(String::new()),
        sdp_mline_index: Some(0),
        username_fragment: None,
        url: None,
    }
}

/// Build a server-reflexive (srflx) UDP ICE candidate.
///
/// `mapped` is the public `IP:port` the STUN server observed for this
/// socket; `base` is the local host address the socket is bound to (the
/// candidate's `raddr`/`rport`, required by RFC 5245 § 4.3 for srflx
/// candidates). The foundation differs from host candidates so the two
/// don't collapse into one pair, and the type-preference byte of the
/// priority is `100 << 24` (srflx) rather than host's `126 << 24`, so
/// host pairs are still tried first while srflx provides the reachable
/// public path for NAT'd peers.
pub(crate) fn srflx_candidate_init(mapped: SocketAddr, base: SocketAddr) -> RTCIceCandidateInit {
    // Priority = (type-pref << 24) | (local-pref << 8) | (256 - component).
    // srflx type preference 100, local preference 65535, component 1.
    let priority = (100u32 << 24) | (65_535u32 << 8) | (256 - 1);
    RTCIceCandidateInit {
        candidate: format!(
            "candidate:2 1 udp {priority} {} {} typ srflx raddr {} rport {} generation 0",
            mapped.ip(),
            mapped.port(),
            base.ip(),
            base.port()
        ),
        sdp_mid: Some(String::new()),
        sdp_mline_index: Some(0),
        username_fragment: None,
        url: None,
    }
}

/// How long, after sending its Binding Request, a UDP forwarder keeps
/// watching for the matching STUN Binding Success Response before it
/// stops trying to gather a srflx candidate for its socket.
///
/// This is NOT on the peer-setup critical path (see the srflx gathering
/// block in the driver below): the SDP answer is created and returned to
/// the signaling layer with host + ICE-TCP candidates *before* any STUN
/// traffic is sent, and the forwarder keeps forwarding ICE/DTLS/media
/// packets to the RTC core throughout this window. So a blocked or
/// unreachable STUN server costs zero added setup latency — when the
/// response never comes, this deadline simply elapses and no srflx
/// candidate is trickled. A reachable server answers in a few ms and the
/// srflx candidate is trickled to the peer well within it.
pub(crate) const STUN_BINDING_TIMEOUT: Duration = Duration::from_millis(1500);

/// Build a STUN Binding Request, returning the wire bytes and the
/// transaction ID a response must echo to be accepted.
///
/// Built with `rtc::stun` (already a transitive dependency via the `rtc`
/// meta-crate — no new dep): `Message::build` writes the 20-byte header
/// with the magic cookie and a random transaction ID, sets the message
/// type to `BINDING_REQUEST`, and `marshal_binary` yields the wire bytes.
pub(crate) fn build_stun_binding_request() -> Result<(Vec<u8>, rtc::stun::message::TransactionId), String> {
    use rtc::stun::message::{Message, BINDING_REQUEST};

    let mut request = Message::new();
    request
        .build(&[
            Box::new(rtc::stun::message::TransactionId::new()),
            Box::new(BINDING_REQUEST),
        ])
        .map_err(|e| format!("build STUN binding request: {e}"))?;
    let request_tid = request.transaction_id;
    let wire = request
        .marshal_binary()
        .map_err(|e| format!("marshal STUN binding request: {e}"))?;
    Ok((wire, request_tid))
}

/// Try to interpret `buf` as the STUN Binding Success Response to a
/// request we sent with `expected_tid`, returning the public `IP:port`
/// from its `XOR-MAPPED-ADDRESS` attribute.
///
/// Returns `None` for anything that isn't our response — a non-STUN
/// datagram (an ICE connectivity check the same socket also carries), a
/// STUN message with a different transaction ID, a non-success class, or
/// a missing/malformed `XOR-MAPPED-ADDRESS`. The caller forwards those
/// `None` cases on to the RTC core unchanged, so folding this check into
/// the UDP read path never drops connectivity-check traffic. Validated by
/// `unmarshal_binary` (magic cookie + length) before
/// `XorMappedAddress::get_from` decodes the attribute. Never panics.
pub(crate) fn parse_stun_binding_response(
    buf: &[u8],
    expected_tid: rtc::stun::message::TransactionId,
) -> Option<SocketAddr> {
    use rtc::stun::message::{Getter, Message};
    use rtc::stun::xoraddr::XorMappedAddress;

    let mut response = Message::new();
    if response.unmarshal_binary(buf).is_err() {
        return None;
    }
    if response.transaction_id != expected_tid {
        return None;
    }
    if response.typ != rtc::stun::message::BINDING_SUCCESS {
        return None;
    }
    let mut mapped = XorMappedAddress::default();
    if mapped.get_from(&response).is_err() {
        return None;
    }
    Some(SocketAddr::new(mapped.ip, mapped.port))
}

/// Test-only round-trip helper: send a Binding Request from `socket` to
/// `stun_addr` and await the matching Binding Success Response, returning
/// the mapped address. Composes the same `build_stun_binding_request` /
/// `parse_stun_binding_response` building blocks the production UDP
/// forwarder folds into its read loop, so the tests exercise the real
/// wire build + parse path. Production no longer uses a blocking
/// round-trip (it would need a second reader on the ICE socket); the
/// forwarder intercepts the response inline instead.
#[cfg(test)]
pub(crate) async fn stun_binding_mapped_addr(
    socket: &UdpSocket,
    stun_addr: SocketAddr,
) -> Result<SocketAddr, String> {
    let (wire, request_tid) = build_stun_binding_request()?;
    let exchange = async {
        socket
            .send_to(&wire, stun_addr)
            .await
            .map_err(|e| format!("send STUN binding request to {stun_addr}: {e}"))?;
        let mut buf = [0u8; 1500];
        loop {
            let (n, from) = socket
                .recv_from(&mut buf)
                .await
                .map_err(|e| format!("recv STUN response: {e}"))?;
            if from != stun_addr {
                continue;
            }
            if let Some(mapped) = parse_stun_binding_response(&buf[..n], request_tid) {
                return Ok(mapped);
            }
        }
    };
    match tokio::time::timeout(STUN_BINDING_TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "STUN binding to {stun_addr} timed out after {STUN_BINDING_TIMEOUT:?}"
        )),
    }
}

/// Extract STUN server `host:port` socket addresses from an [`IceConfig`].
///
/// Each ICE server may carry several URLs; we keep only `stun:`/`stuns:`
/// entries (TURN is out of scope for srflx gathering) and resolve each via
/// DNS. The configured default is `stun:stun.l.google.com:19302`; a STUN
/// URL without an explicit port falls back to the IANA default 3478.
///
/// Returns deduplicated resolved addresses. An empty result (no STUN
/// servers configured, or all failed to resolve) means srflx gathering is
/// skipped entirely — host/ICE-TCP candidates still work.
pub(crate) async fn resolve_stun_servers(ice_config: &IceConfig) -> Vec<SocketAddr> {
    use rtc::stun::uri::Uri;

    let mut out: Vec<SocketAddr> = Vec::new();
    for server in &ice_config.ice_servers {
        for url in &server.urls {
            let uri = match Uri::parse_uri(url) {
                Ok(u) => u,
                Err(_) => continue,
            };
            // `scheme` is the URL scheme ("stun"/"stuns"); skip turn/turns.
            if uri.scheme != "stun" && uri.scheme != "stuns" {
                continue;
            }
            let port = uri.port.unwrap_or(rtc::stun::DEFAULT_PORT);
            let host_port = format!("{}:{}", uri.host, port);
            let resolved = tokio::net::lookup_host(host_port.clone()).await;
            match resolved {
                Ok(addrs) => {
                    for addr in addrs {
                        if !out.contains(&addr) {
                            out.push(addr);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[display/webrtc] STUN server {host_port} resolve failed: {e}");
                }
            }
        }
    }
    out
}

// --- TURN relay candidate gathering ----------------------------------------
//
// The server-side `rtc` peer only advertises host (per-interface UDP),
// srflx (STUN-derived public UDP), and ICE-TCP candidates. On a host with no
// inbound reachability at all — a Docker container behind a cloud NAT
// with no inbound UDP and a srflx mapping the browser still can't
// dial — none of those pair, and a relay-only browser
// (`iceTransportPolicy: 'relay'`, which the federated path forces) has
// nothing to connect to. To give such peers a reachable path the server peer
// allocates its OWN relay on the configured coturn (the same `turn:` server
// the browser uses) and advertises a `typ relay` candidate carrying the
// relayed transport address. Media to/from a NAT'd peer then bounces through
// coturn from both ends.
//
// This reuses `rtc`'s sans-I/O TURN client (`rtc::turn::client`, re-exported
// by the `rtc` 0.9 meta-crate — no new dependency, the analogue of how srflx
// reuses `rtc::stun`). The client owns no sockets: it is driven through the
// same `sansio::Protocol` trait (`RtcProtocol`, already imported) as the
// `RTCPeerConnection` — `handle_read` for inbound bytes, `poll_write` for
// outbound, `poll_event` for Allocate/CreatePermission/Data events,
// `poll_timeout`/`handle_timeout` for retransmit + automatic allocation and
// permission refresh. The app owns the relay UDP socket and pumps it.

/// A resolved TURN server: the long-term credentials and resolved transport
/// address parsed out of a `turn:`/`turns:` entry in `[webrtc].ice_servers`.
#[derive(Clone, Debug)]
pub(crate) struct TurnServerCfg {
    /// Resolved `IP:port` of the TURN server's signaling transport.
    addr: SocketAddr,
    /// Long-term-credential username (the `username` field of the ICE server).
    username: String,
    /// Long-term-credential password (the `credential` field).
    password: String,
}

/// Maximum time the relay task waits for a TURN Allocate success response
/// before giving up. Like the STUN srflx timeout, this is OFF the
/// peer-setup critical path — the SDP answer is created and returned with
/// host + ICE-TCP candidates before any TURN traffic is sent, and the relay
/// candidate is *trickled* once the allocation succeeds. A blocked or
/// unreachable TURN server therefore costs zero added setup latency: the
/// allocation simply times out and no relay candidate is trickled, leaving
/// the host/srflx/ICE-TCP candidate set intact.
pub(crate) const RELAY_ALLOCATE_TIMEOUT: Duration = Duration::from_millis(2500);

/// Hard cap on a TURN allocation's lifetime in the relay task. The sans-I/O
/// turn client auto-refreshes the allocation at half its server-granted
/// lifetime (and permissions on their own timer) via `handle_timeout`; this
/// cap only bounds the `poll_timeout`-driven sleep so an idle relay still
/// wakes periodically to service refreshes even if the server grants a very
/// long lifetime.
pub(crate) const RELAY_REFRESH_POLL_CAP: Duration = Duration::from_secs(30);

/// Build a relay (`typ relay`) UDP ICE candidate.
///
/// `relayed` is the relayed transport address the TURN server allocated for
/// us (the candidate's transport address — what the remote peer dials, which
/// the TURN server then forwards to our relay socket). `mapped` is the
/// server-reflexive address the TURN server observed for our relay socket
/// (the `XOR-MAPPED-ADDRESS` in the Allocate response), used as the
/// candidate's `raddr`/`rport` per RFC 5245 § 4.3 (relayed candidates carry
/// their reflexive base there). The foundation ("3") differs from host ("1")
/// and srflx ("2") so the candidates don't collapse, and the type-preference
/// byte of the priority is `0 << 24` (relay) — the lowest, so direct host /
/// srflx pairs are always tried first and the relay is the last resort.
pub(crate) fn relay_candidate_init(relayed: SocketAddr, mapped: SocketAddr) -> RTCIceCandidateInit {
    // Priority = (type-pref << 24) | (local-pref << 8) | (256 - component).
    // relay type preference 0 (so the `0 << 24` term vanishes — the lowest
    // type preference, ensuring host/srflx pairs are tried before the relay),
    // local preference 65535, component 1.
    let priority = (65_535u32 << 8) | (256 - 1);
    RTCIceCandidateInit {
        candidate: format!(
            "candidate:3 1 udp {priority} {} {} typ relay raddr {} rport {} generation 0",
            relayed.ip(),
            relayed.port(),
            mapped.ip(),
            mapped.port()
        ),
        sdp_mid: Some(String::new()),
        sdp_mline_index: Some(0),
        username_fragment: None,
        url: None,
    }
}

/// Extract plain-UDP TURN servers (`turn:`) with their credentials from an
/// [`IceConfig`], resolving each host to a transport address.
///
/// `rtc::stun::uri::Uri::parse_uri` deliberately rejects the `turn:`/`turns:`
/// schemes (it is a STUN-only RFC 7064 parser), so this parses the RFC 7065
/// TURN URI form by hand: `turn:host[:port][?transport=...]`. The
/// `?transport=` query (UDP/TCP hint) is ignored — we always allocate over
/// plain UDP from our relay socket. `turns:` (TURN-over-TLS, typically port
/// 5349) entries are skipped: the sans-I/O client speaks plain UDP STUN/TURN
/// and cannot drive a TLS endpoint; coturn exposes the same allocation
/// surface on the plain `turn:` URL we use. Credentials come from the ICE
/// server's `username`/`credential` fields (the WebRTC config model carries
/// TURN long-term credentials there, not in the URI). Entries without both a
/// username and a credential are skipped: an unauthenticated Allocate just
/// draws a 401 and wastes the timeout. The port defaults to the IANA TURN
/// port 3478 when absent.
///
/// Returns deduplicated resolved servers. An empty result (no plain `turn:`
/// server configured, or all failed to resolve / lacked credentials) means
/// relay gathering is skipped entirely — host/srflx/ICE-TCP candidates still
/// work.
pub(crate) async fn resolve_turn_servers(ice_config: &IceConfig) -> Vec<TurnServerCfg> {
    let mut out: Vec<TurnServerCfg> = Vec::new();
    for server in &ice_config.ice_servers {
        // TURN long-term credentials are mandatory; without them the
        // Allocate is unauthenticated and the server answers 401.
        let (Some(username), Some(password)) = (&server.username, &server.credential) else {
            continue;
        };
        for url in &server.urls {
            // Split off any RFC 7065 `?transport=` query — we always allocate
            // over plain UDP from our relay socket.
            let base = url.split('?').next().unwrap_or(url);
            // Only plain `turn:` (UDP/TCP, we use UDP). `turns:` is
            // TURN-over-(D)TLS on a TLS port (typically 5349): our sans-I/O
            // client speaks plain UDP STUN/TURN, so a TLS endpoint is
            // unreachable for it. Coturn deployments expose the same
            // allocation surface on the plain `turn:` URL, which we use; the
            // `turns:` entry (often listed first for browsers) is skipped.
            let Some(rest) = base.strip_prefix("turn:") else {
                continue; // turns:/stun:/stuns:/other — not usable over UDP here.
            };
            // `rest` is `host[:port]`, with IPv6 hosts bracketed as
            // `[::1]:3478`. Parse host + optional port.
            let (host, port) = parse_turn_host_port(rest);
            let host_port = format!("{host}:{port}");
            match tokio::net::lookup_host(host_port.clone()).await {
                Ok(addrs) => {
                    for addr in addrs {
                        if !out.iter().any(|c| c.addr == addr) {
                            out.push(TurnServerCfg {
                                addr,
                                username: username.clone(),
                                password: password.clone(),
                            });
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[display/webrtc] TURN server {host_port} resolve failed: {e}");
                }
            }
        }
    }
    out
}

/// Split a TURN URI authority (`host[:port]`, IPv6 hosts bracketed) into a
/// host string suitable for DNS lookup and a port, defaulting to the IANA
/// TURN port 3478. Bracketed IPv6 literals keep their brackets stripped for
/// the host but the trailing `:port` (outside the brackets) is honored.
pub(crate) fn parse_turn_host_port(authority: &str) -> (String, u16) {
    const DEFAULT_TURN_PORT: u16 = 3478;
    if let Some(close) = authority
        .strip_prefix('[')
        .and_then(|_| authority.find(']'))
    {
        // IPv6 literal: `[<addr>]` optionally followed by `:port`.
        let host = authority[1..close].to_string();
        let after = &authority[close + 1..];
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_TURN_PORT);
        (host, port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        // `host:port` (IPv4 or hostname). If the part after the last colon
        // isn't a valid port, treat the whole thing as a bare host.
        match port.parse() {
            Ok(p) => (host.to_string(), p),
            Err(_) => (authority.to_string(), DEFAULT_TURN_PORT),
        }
    } else {
        (authority.to_string(), DEFAULT_TURN_PORT)
    }
}

/// Outcome of a successful relay allocation, handed back to the driver so it
/// can advertise the relay candidate and route media through the relay task.
pub(crate) struct RelayAllocation {
    /// The relayed transport address coturn allocated (the candidate's
    /// transport address — what the remote peer dials).
    pub(crate) relayed_addr: SocketAddr,
    /// The reflexive base the TURN server observed for our relay socket
    /// (Allocate response `XOR-MAPPED-ADDRESS`), used as the candidate's
    /// `raddr`/`rport`.
    pub(crate) mapped_addr: SocketAddr,
    /// Driver → relay: RTC outbound bytes that ICE wants to send *from* the
    /// relayed address. Each item is `(peer_addr, bytes)`; the relay task
    /// ensures a permission exists for `peer_addr` then sends via the TURN
    /// client (Send indication → ChannelData once a channel is bound).
    pub(crate) relay_out_tx: mpsc::Sender<(SocketAddr, Vec<u8>)>,
}

/// Drive a sans-I/O TURN client to allocate a relay on `server`, then run the
/// relay's I/O loop until shutdown.
///
/// Owns a freshly-bound relay UDP socket (the single owner of its
/// `recv_from`, so there is no read race — mirrors the per-socket forwarder
/// pattern). On allocation success it sends a [`RelayAllocation`] back over
/// `alloc_tx` (carrying the relayed address for the candidate and the
/// driver→relay media channel) and then loops:
///
///   - drains the turn client's `poll_write` to the relay socket (TURN
///     control + wrapped media to the TURN server),
///   - reads the relay socket and feeds bytes to `handle_read` (the client
///     demultiplexes STUN responses, Data indications and ChannelData),
///   - drains `poll_event`: relayed inbound media
///     (`DataIndicationOrChannelData`) is unwrapped and pushed to the driver
///     via `inbound_tx` tagged as arriving *at* the relayed address from the
///     remote peer (so rtc pairs it with the relay candidate); the first
///     time a new peer is seen a `create_permission` is issued,
///   - services `poll_timeout`/`handle_timeout` so the turn client
///     retransmits unacked requests and auto-refreshes the allocation and
///     permissions,
///   - forwards driver→relay media (`relay_out_rx`) through the client's
///     `Relay::send_to`.
///
/// On allocation failure or timeout it logs and returns without sending a
/// `RelayAllocation`; the driver proceeds with host/srflx/ICE-TCP only.
pub(crate) async fn run_turn_relay(
    peer_id: PeerId,
    server: TurnServerCfg,
    is_ipv4: bool,
    inbound_tx: mpsc::Sender<InboundPacket>,
    alloc_tx: mpsc::Sender<RelayAllocation>,
    shutdown: CancellationToken,
) {
    use rtc::turn::client::{Client, ClientConfig, Event as TurnEvent};

    // Bind the relay socket on the same family as the TURN server so the
    // kernel can route our datagrams to it. Wildcard bind (port 0) — this
    // socket only ever talks to the TURN server and (post-allocate) carries
    // wrapped media, never a directly-advertised candidate of its own.
    let bind_addr: SocketAddr = if is_ipv4 {
        SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), 0)
    } else {
        SocketAddr::new(std::net::Ipv6Addr::UNSPECIFIED.into(), 0)
    };
    let socket = match UdpSocket::bind(bind_addr).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[display/webrtc] peer {peer_id}: TURN relay socket bind failed: {e}");
            return;
        }
    };
    let local_addr = match socket.local_addr() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[display/webrtc] peer {peer_id}: TURN relay socket local_addr failed: {e}");
            return;
        }
    };

    let mut client = match Client::new(ClientConfig {
        stun_serv_addr: String::new(),
        turn_serv_addr: server.addr.to_string(),
        local_addr,
        transport_protocol: TransportProtocol::UDP,
        username: server.username.clone(),
        password: server.password.clone(),
        realm: String::new(),
        software: String::new(),
        rto_in_ms: 0,
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[display/webrtc] peer {peer_id}: TURN client init failed: {e}");
            return;
        }
    };

    // Kick off the allocation. The client pushes the Allocate request onto
    // its transmit queue; we pump it below.
    if let Err(e) = client.allocate() {
        eprintln!("[display/webrtc] peer {peer_id}: TURN allocate() failed: {e}");
        return;
    }

    // Phase 1: drive the allocation handshake (anonymous → 401 → authed
    // Allocate) until we get a relayed address or time out. Permissions are
    // created lazily once we learn a peer's address from inbound relayed data.
    let mut relayed_addr: Option<SocketAddr> = None;
    let mut recv_buf = vec![0u8; UDP_BUF_LEN];
    let allocate_deadline = tokio::time::Instant::now() + RELAY_ALLOCATE_TIMEOUT;

    'allocate: loop {
        // Flush any pending TURN control writes to the server.
        if pump_turn_writes(&mut client, &socket).await.is_err() {
            return;
        }
        // Drain events produced so far.
        while let Some(event) = client.poll_event() {
            match event {
                TurnEvent::AllocateResponse(_, addr) => {
                    relayed_addr = Some(addr);
                    // The reflexive base is the XOR-MAPPED-ADDRESS the client
                    // recorded; if the client didn't surface one we fall back
                    // to the local socket address (still a valid raddr base).
                    break;
                }
                TurnEvent::AllocateError(_, e) => {
                    eprintln!("[display/webrtc] peer {peer_id}: TURN allocate rejected: {e}");
                    return;
                }
                _ => {}
            }
        }
        if relayed_addr.is_some() {
            break 'allocate;
        }

        let now = tokio::time::Instant::now();
        if now >= allocate_deadline {
            eprintln!(
                "[display/webrtc] peer {peer_id}: TURN allocate timed out after {RELAY_ALLOCATE_TIMEOUT:?} (no relay candidate)"
            );
            return;
        }
        // Next turn-client timer (retransmit), bounded by the overall
        // allocate deadline.
        let client_timeout = client
            .poll_timeout()
            .map(tokio_instant_from_std)
            .unwrap_or(allocate_deadline)
            .min(allocate_deadline);
        let sleep_until = tokio::time::sleep_until(client_timeout);
        tokio::pin!(sleep_until);

        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = &mut sleep_until => {
                let _ = client.handle_timeout(Instant::now());
            }
            recv = socket.recv_from(&mut recv_buf) => match recv {
                Ok((n, from)) => {
                    feed_turn_inbound(&mut client, &recv_buf[..n], from, local_addr);
                }
                Err(e) => {
                    eprintln!("[display/webrtc] peer {peer_id}: TURN relay recv failed: {e}");
                    return;
                }
            },
        }
    }

    let relayed_addr = match relayed_addr {
        Some(a) => a,
        None => return,
    };
    // raddr/rport for the relay candidate: the reflexive base. The turn
    // client doesn't expose the Allocate response's XOR-MAPPED-ADDRESS, so we
    // use the relay socket's local address. The raddr is informational for
    // pairing (RFC 5245 §4.3); the relayed address is what the peer dials.
    let mapped = local_addr;

    // Hand the allocation back to the driver: it adds the relay candidate and
    // begins routing relay-destined RTC output to us.
    let (relay_out_tx, mut relay_out_rx) = mpsc::channel::<(SocketAddr, Vec<u8>)>(256);
    if alloc_tx
        .send(RelayAllocation {
            relayed_addr,
            mapped_addr: mapped,
            relay_out_tx,
        })
        .await
        .is_err()
    {
        // Driver gone before we finished allocating; tear the allocation down.
        let _ = client.close();
        let _ = pump_turn_writes(&mut client, &socket).await;
        return;
    }
    eprintln!(
        "[display/webrtc] peer {peer_id}: TURN relay allocated {relayed_addr} via {} (relay socket {local_addr})",
        server.addr
    );

    // Phase 2: steady-state relay I/O loop. Permissions for peers are created
    // on first sight of inbound relayed data; the turn client auto-refreshes
    // the allocation + permissions through `handle_timeout`.
    let mut permitted: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();
    loop {
        if pump_turn_writes(&mut client, &socket).await.is_err() {
            return;
        }
        // Drain relay events: relayed inbound media + permission results.
        while let Some(event) = client.poll_event() {
            match event {
                TurnEvent::DataIndicationOrChannelData(_, peer_addr, data) => {
                    // A peer (the browser, via coturn) sent us a packet. Make
                    // sure we have a permission so our replies can flow back,
                    // then inject the unwrapped payload into the RTC core as
                    // if it arrived at our relayed address (so ICE pairs it
                    // with the relay candidate).
                    if permitted.insert(peer_addr) {
                        if let Ok(mut relay) = client.relay(relayed_addr) {
                            if let Err(e) = relay.create_permission(peer_addr) {
                                eprintln!(
                                    "[display/webrtc] peer {peer_id}: TURN create_permission({peer_addr}) failed: {e}"
                                );
                            }
                        }
                    }
                    let pkt = InboundPacket {
                        proto: TransportProtocol::UDP,
                        source: peer_addr,
                        destination: relayed_addr,
                        bytes: data.to_vec(),
                        received_at: Instant::now(),
                    };
                    if inbound_tx.send(pkt).await.is_err() {
                        return; // driver gone
                    }
                }
                TurnEvent::CreatePermissionError(_, e) => {
                    eprintln!(
                        "[display/webrtc] peer {peer_id}: TURN create_permission rejected: {e}"
                    );
                }
                _ => {}
            }
        }
        // Flush any control traffic the permission/event handling produced.
        if pump_turn_writes(&mut client, &socket).await.is_err() {
            return;
        }

        let client_timeout = client
            .poll_timeout()
            .map(tokio_instant_from_std)
            .unwrap_or_else(|| tokio::time::Instant::now() + RELAY_REFRESH_POLL_CAP)
            .min(tokio::time::Instant::now() + RELAY_REFRESH_POLL_CAP);
        let sleep_until = tokio::time::sleep_until(client_timeout);
        tokio::pin!(sleep_until);

        tokio::select! {
            _ = shutdown.cancelled() => {
                // Release the allocation politely so coturn frees the port.
                let _ = client.close();
                let _ = pump_turn_writes(&mut client, &socket).await;
                return;
            }
            _ = &mut sleep_until => {
                if client.handle_timeout(Instant::now()).is_err() {
                    return;
                }
            }
            recv = socket.recv_from(&mut recv_buf) => match recv {
                Ok((n, from)) => {
                    feed_turn_inbound(&mut client, &recv_buf[..n], from, local_addr);
                }
                Err(e) => {
                    eprintln!("[display/webrtc] peer {peer_id}: TURN relay recv failed: {e}");
                    return;
                }
            },
            out = relay_out_rx.recv() => match out {
                Some((peer_addr, bytes)) => {
                    // RTC wants to send to `peer_addr` via the relay. Ensure a
                    // permission exists, then hand the payload to the turn
                    // client, which wraps it (Send indication, upgrading to a
                    // bound channel) toward coturn.
                    if permitted.insert(peer_addr) {
                        if let Ok(mut relay) = client.relay(relayed_addr) {
                            let _ = relay.create_permission(peer_addr);
                        }
                    }
                    if let Ok(mut relay) = client.relay(relayed_addr) {
                        if let Err(e) = relay.send_to(&bytes, peer_addr) {
                            // ErrNoPermission here is expected for the very
                            // first packet to a peer (permission still in
                            // flight); the RTC core retransmits, so a single
                            // dropped check is harmless. Log other errors.
                            if !is_turn_no_permission(&e) {
                                eprintln!(
                                    "[display/webrtc] peer {peer_id}: TURN send_to({peer_addr}) failed: {e}"
                                );
                            }
                        }
                    }
                }
                None => {
                    // Driver dropped the relay sender — peer is going away.
                    let _ = client.close();
                    let _ = pump_turn_writes(&mut client, &socket).await;
                    return;
                }
            },
        }
    }
}

/// Drain the turn client's outbound transmit queue to the relay socket.
/// Each `poll_write` item is a `TaggedBytesMut` whose `transport.peer_addr`
/// is the destination (the TURN server). Returns `Err(())` if a send fails
/// fatally (socket dead) so the caller can tear the relay task down.
pub(crate) async fn pump_turn_writes(
    client: &mut rtc::turn::client::Client,
    socket: &UdpSocket,
) -> Result<(), ()> {
    while let Some(transmit) = client.poll_write() {
        if let Err(e) = socket
            .send_to(&transmit.message, transmit.transport.peer_addr)
            .await
        {
            eprintln!(
                "[display/webrtc] TURN relay send to {} failed: {e}",
                transmit.transport.peer_addr
            );
            return Err(());
        }
    }
    Ok(())
}

/// Feed a datagram received on the relay socket into the turn client. Wraps
/// the raw bytes in a `TaggedBytesMut` with `peer_addr = from` (the source),
/// which the client uses to demultiplex (STUN response from the TURN server
/// vs. relayed application data). Parse/handling errors are logged at trace
/// level only — the client discards malformed packets and keeps running.
pub(crate) fn feed_turn_inbound(
    client: &mut rtc::turn::client::Client,
    bytes: &[u8],
    from: SocketAddr,
    local_addr: SocketAddr,
) {
    let tagged = TaggedBytesMut {
        now: Instant::now(),
        transport: TransportContext {
            local_addr,
            peer_addr: from,
            transport_protocol: TransportProtocol::UDP,
            ecn: None,
        },
        message: BytesMut::from(bytes),
    };
    // The turn client returns Err for non-STUN traffic from the STUN server
    // and for malformed packets; both are safe to ignore here (we have no
    // STUN server configured on this client, only TURN).
    if let Err(err) = client.handle_read(tagged) {
        eprintln!("[display/webrtc] trace: TURN inbound from {from} ignored: {err}");
    }
}

/// True if `e` is the turn client's "no permission yet" error, which is the
/// expected transient for the first packet sent to a freshly-seen peer.
pub(crate) fn is_turn_no_permission(e: &rtc::shared::error::Error) -> bool {
    matches!(e, rtc::shared::error::Error::ErrNoPermission)
}

/// Convert a `std::time::Instant` deadline (what the sans-I/O turn client's
/// `poll_timeout` returns) into a `tokio::time::Instant` for `sleep_until`.
pub(crate) fn tokio_instant_from_std(when: Instant) -> tokio::time::Instant {
    let now_std = Instant::now();
    let now_tokio = tokio::time::Instant::now();
    if when <= now_std {
        now_tokio
    } else {
        now_tokio + (when - now_std)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- srflx (STUN server-reflexive) gathering tests ---

    #[test]
    fn srflx_candidate_init_formats_typ_srflx_with_raddr_rport() {
        use std::net::{Ipv4Addr, SocketAddr};
        let mapped = SocketAddr::new(Ipv4Addr::new(34, 173, 63, 221).into(), 50000);
        let base = SocketAddr::new(Ipv4Addr::new(10, 128, 0, 2).into(), 40000);
        let init = srflx_candidate_init(mapped, base);
        // Public mapped address is the candidate's transport address.
        assert!(
            init.candidate.contains("udp"),
            "udp transport: {}",
            init.candidate
        );
        assert!(
            init.candidate.contains("34.173.63.221 50000 typ srflx"),
            "mapped addr + typ srflx: {}",
            init.candidate
        );
        // RFC 5245 §4.3: srflx candidate carries the host base as raddr/rport.
        assert!(
            init.candidate.contains("raddr 10.128.0.2 rport 40000"),
            "raddr/rport = base: {}",
            init.candidate
        );
        // srflx must outrank ICE-TCP (1_677_721_855) but rank below UDP host
        // (2_130_706_431) so host pairs are still tried first.
        let priority: u32 = init
            .candidate
            .split_whitespace()
            .nth(3)
            .and_then(|p| p.parse().ok())
            .expect("priority field");
        assert!(
            priority < 2_130_706_431 && priority > 1_677_721_855,
            "srflx priority {priority} between ICE-TCP and UDP host"
        );
    }

    #[tokio::test]
    async fn resolve_stun_servers_parses_stun_url_and_skips_turn() {
        use std::net::Ipv4Addr;
        // Mix of a resolvable literal-IP stun URL and a turn URL that must
        // be ignored (srflx only needs STUN). Using a literal IP avoids a
        // DNS dependency in the unit test.
        let ice_config = IceConfig {
            ice_servers: vec![crate::IceServer {
                urls: vec![
                    "stun:127.0.0.1:19302".to_string(),
                    "turn:127.0.0.1:3478".to_string(),
                ],
                username: None,
                credential: None,
            }],
        };
        let resolved = resolve_stun_servers(&ice_config).await;
        assert_eq!(
            resolved,
            vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 19302)],
            "only the stun: URL resolved, turn: skipped"
        );
    }

    #[tokio::test]
    async fn resolve_stun_servers_empty_when_no_servers() {
        let resolved = resolve_stun_servers(&IceConfig::default()).await;
        assert!(resolved.is_empty(), "no ice servers -> no STUN addrs");
    }

    // --- TURN relay gathering tests ---

    #[test]
    fn relay_candidate_init_formats_typ_relay_with_raddr_rport() {
        use std::net::{Ipv4Addr, SocketAddr};
        // Relayed = the address coturn allocated (what the peer dials);
        // mapped = our relay socket's reflexive base (the raddr/rport).
        let relayed = SocketAddr::new(Ipv4Addr::new(203, 0, 113, 9).into(), 51000);
        let mapped = SocketAddr::new(Ipv4Addr::new(10, 0, 0, 5).into(), 44000);
        let init = relay_candidate_init(relayed, mapped);
        assert!(
            init.candidate.contains("udp"),
            "udp transport: {}",
            init.candidate
        );
        // The relayed address is the candidate's transport address + typ relay.
        assert!(
            init.candidate.contains("203.0.113.9 51000 typ relay"),
            "relayed addr + typ relay: {}",
            init.candidate
        );
        // RFC 5245 §4.3: relay candidate carries its reflexive base as
        // raddr/rport.
        assert!(
            init.candidate.contains("raddr 10.0.0.5 rport 44000"),
            "raddr/rport = mapped base: {}",
            init.candidate
        );
        // Relay uses the lowest type preference (0) so host/srflx/ICE-TCP
        // pairs are all tried before the relay last-resort path.
        let priority: u32 = init
            .candidate
            .split_whitespace()
            .nth(3)
            .and_then(|p| p.parse().ok())
            .expect("priority field");
        assert!(
            priority < 1_677_721_855,
            "relay priority {priority} ranks below ICE-TCP (and host/srflx)"
        );
        // Distinct foundation so it doesn't collapse with host("1")/srflx("2").
        assert!(
            init.candidate.starts_with("candidate:3 "),
            "relay foundation 3: {}",
            init.candidate
        );
    }

    #[tokio::test]
    async fn resolve_turn_servers_parses_turn_url_skips_stun_and_turns() {
        use std::net::Ipv4Addr;
        // Mirrors the real coturn config shape: a `turns:` (TLS) URL listed
        // first (skipped — UDP client can't drive TLS), a `stun:` URL
        // (skipped — STUN handled elsewhere), and the plain `turn:` URL
        // (kept). Different ports prove we pick the plain-UDP 3478 entry, not
        // the TLS 5349 one. Literal IP avoids a DNS dependency.
        let ice_config = IceConfig {
            ice_servers: vec![crate::IceServer {
                urls: vec![
                    "turns:127.0.0.1:5349?transport=tcp".to_string(),
                    "stun:127.0.0.1:19302".to_string(),
                    "turn:127.0.0.1:3478?transport=udp".to_string(),
                ],
                username: Some("intendant".to_string()),
                credential: Some("secret".to_string()),
            }],
        };
        let resolved = resolve_turn_servers(&ice_config).await;
        assert_eq!(
            resolved.len(),
            1,
            "only the plain turn: URL resolved (turns:/stun: skipped), got {resolved:?}"
        );
        assert_eq!(
            resolved[0].addr,
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 3478),
            "picked the plain-UDP 3478 endpoint, not the TLS 5349 one"
        );
        assert_eq!(resolved[0].username, "intendant");
        assert_eq!(resolved[0].password, "secret");
    }

    #[tokio::test]
    async fn resolve_turn_servers_skips_credential_less_servers() {
        // A turn server with no username/credential is unusable (the Allocate
        // would draw a 401), so it's skipped rather than wasting the timeout.
        let ice_config = IceConfig {
            ice_servers: vec![crate::IceServer {
                urls: vec!["turn:127.0.0.1:3478".to_string()],
                username: None,
                credential: None,
            }],
        };
        let resolved = resolve_turn_servers(&ice_config).await;
        assert!(
            resolved.is_empty(),
            "credential-less turn server skipped, got {resolved:?}"
        );
    }

    #[tokio::test]
    async fn resolve_turn_servers_empty_when_no_servers() {
        let resolved = resolve_turn_servers(&IceConfig::default()).await;
        assert!(resolved.is_empty(), "no ice servers -> no TURN servers");
    }

    #[tokio::test]
    async fn resolve_turn_servers_ignores_transport_query() {
        use std::net::Ipv4Addr;
        // RFC 7065 `?transport=tcp` query must be stripped before host:port
        // parsing — we always allocate over UDP regardless of the hint.
        let ice_config = IceConfig {
            ice_servers: vec![crate::IceServer {
                urls: vec!["turn:127.0.0.1:3478?transport=tcp".to_string()],
                username: Some("u".to_string()),
                credential: Some("p".to_string()),
            }],
        };
        let resolved = resolve_turn_servers(&ice_config).await;
        assert_eq!(resolved.len(), 1, "transport query stripped, server kept");
        assert_eq!(
            resolved[0].addr,
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 3478)
        );
    }

    #[test]
    fn parse_turn_host_port_variants() {
        // Explicit port.
        assert_eq!(
            parse_turn_host_port("turn.example.com:3478"),
            ("turn.example.com".to_string(), 3478)
        );
        // Bare host -> IANA default 3478.
        assert_eq!(
            parse_turn_host_port("turn.example.com"),
            ("turn.example.com".to_string(), 3478)
        );
        // IPv4 literal with port.
        assert_eq!(
            parse_turn_host_port("203.0.113.9:5349"),
            ("203.0.113.9".to_string(), 5349)
        );
        // Bracketed IPv6 literal with port (brackets stripped from host).
        assert_eq!(
            parse_turn_host_port("[2001:db8::1]:3478"),
            ("2001:db8::1".to_string(), 3478)
        );
        // Bracketed IPv6 literal without port -> default.
        assert_eq!(
            parse_turn_host_port("[2001:db8::1]"),
            ("2001:db8::1".to_string(), 3478)
        );
    }

    #[tokio::test]
    async fn stun_binding_round_trips_against_local_responder() {
        // Stand up a tiny local "STUN server" that answers any Binding
        // Request with a Binding Success carrying the peer's address as
        // XOR-MAPPED-ADDRESS — exercising our request build + response
        // parse path without touching the network.
        use rtc::stun::message::{Message, Setter, BINDING_SUCCESS};
        use rtc::stun::xoraddr::XorMappedAddress;

        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            let mut req = Message::new();
            req.unmarshal_binary(&buf[..n]).unwrap();
            // Echo the requester's address back as XOR-MAPPED-ADDRESS,
            // preserving the request's transaction ID (required for the
            // client to accept the response).
            let mut resp = Message::new();
            resp.build(&[
                Box::new(req.transaction_id) as Box<dyn Setter>,
                Box::new(BINDING_SUCCESS),
                Box::new(XorMappedAddress {
                    ip: from.ip(),
                    port: from.port(),
                }),
            ])
            .unwrap();
            let wire = resp.marshal_binary().unwrap();
            server.send_to(&wire, from).await.unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();
        let mapped = stun_binding_mapped_addr(&client, server_addr)
            .await
            .expect("binding success");
        assert_eq!(
            mapped, client_addr,
            "parsed XOR-MAPPED-ADDRESS == client's own address"
        );
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn stun_binding_times_out_against_silent_server() {
        // A bound-but-silent UDP socket never answers; the client must
        // give up after STUN_BINDING_TIMEOUT rather than hang forever.
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let started = std::time::Instant::now();
        let result = stun_binding_mapped_addr(&client, silent_addr).await;
        assert!(result.is_err(), "no response -> Err, got {result:?}");
        assert!(
            started.elapsed() < STUN_BINDING_TIMEOUT + Duration::from_secs(1),
            "returned promptly after timeout, took {:?}",
            started.elapsed()
        );
    }

    /// `parse_stun_binding_response` is the load-bearing predicate that
    /// lets the srflx gather be folded into the UDP forwarder's single
    /// read loop (audit F8): it must return `Some(mapped)` ONLY for the
    /// Binding Success Response matching the request's transaction ID, and
    /// `None` for everything else so those datagrams fall through to the
    /// RTC core. Builds a real `rtc::stun` Binding Success carrying a known
    /// XOR-MAPPED-ADDRESS.
    #[test]
    fn parse_stun_binding_response_matches_only_our_success() {
        use rtc::stun::message::{Message, Setter, BINDING_SUCCESS};
        use rtc::stun::xoraddr::XorMappedAddress;
        use std::net::Ipv4Addr;

        let (_wire, tid) = build_stun_binding_request().expect("build request");
        let mapped_ip = Ipv4Addr::new(203, 0, 113, 7);
        let mapped_port = 51234u16;
        let mut resp = Message::new();
        resp.build(&[
            Box::new(tid) as Box<dyn Setter>,
            Box::new(BINDING_SUCCESS),
            Box::new(XorMappedAddress {
                ip: mapped_ip.into(),
                port: mapped_port,
            }),
        ])
        .unwrap();
        let success = resp.marshal_binary().unwrap();

        // Matching txid + success class -> the mapped address.
        assert_eq!(
            parse_stun_binding_response(&success, tid),
            Some(SocketAddr::new(mapped_ip.into(), mapped_port)),
            "matching Binding Success yields its XOR-MAPPED-ADDRESS"
        );

        // A *different* expected txid must not match (so two sockets'
        // gathers can't steal each other's responses).
        let (_w2, other_tid) = build_stun_binding_request().expect("build request");
        assert_eq!(
            parse_stun_binding_response(&success, other_tid),
            None,
            "transaction-id mismatch is rejected"
        );

        // A non-STUN datagram (e.g. an ICE connectivity check or media)
        // must pass through (None) so the forwarder forwards it.
        assert_eq!(
            parse_stun_binding_response(b"not a stun message at all", tid),
            None,
            "non-STUN bytes are not mistaken for our response"
        );

        // A STUN Binding *Request* (wrong class) is also not our response.
        let (request_wire, req_tid) = build_stun_binding_request().expect("build request");
        assert_eq!(
            parse_stun_binding_response(&request_wire, req_tid),
            None,
            "a non-success STUN class is rejected"
        );
    }

    /// The srflx candidate trickled to the browser must carry the
    /// canonical `RTCIceCandidate.toJSON()` field names so
    /// `pc.addIceCandidate` accepts it: `candidate` (the SDP attribute
    /// value), `sdpMid`, and `sdpMLineIndex`. This mirrors the JSON the
    /// driver builds in the `srflx_rx` select branch; if that shape drifts
    /// the browser silently drops the candidate and the off-path srflx
    /// path stops advertising.
    #[test]
    fn srflx_trickle_json_has_canonical_candidate_fields() {
        use std::net::Ipv4Addr;
        let mapped = SocketAddr::new(Ipv4Addr::new(34, 173, 63, 221).into(), 50000);
        let base = SocketAddr::new(Ipv4Addr::new(10, 128, 0, 2).into(), 40000);
        let init = srflx_candidate_init(mapped, base);
        let candidate_json = serde_json::json!({
            "candidate": init.candidate,
            "sdpMid": serde_json::Value::Null,
            "sdpMLineIndex": 0,
        });
        let v: serde_json::Value =
            serde_json::from_str(&candidate_json.to_string()).expect("valid JSON");
        assert!(
            v["candidate"]
                .as_str()
                .is_some_and(|s| s.contains("typ srflx")),
            "candidate field carries the srflx SDP attribute: {v}"
        );
        assert!(v["sdpMid"].is_null(), "sdpMid present (null): {v}");
        assert_eq!(
            v["sdpMLineIndex"].as_u64(),
            Some(0),
            "sdpMLineIndex routes to the single video m-line: {v}"
        );
    }
}

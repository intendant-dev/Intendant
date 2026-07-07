//! ICE-TCP multiplexing: the shared TCP peer registry (ufrag -> per-peer
//! connection channel), the federation TCP relay registry (ufrag -> outbound
//! peer address), ufrag/SDP manipulation helpers, RFC 4571 framing, and the
//! minimal STUN USERNAME parser the registries route on.

use super::*;

// ---------------------------------------------------------------------------
// TCP peer registry (ufrag → per-peer connection channel)
// ---------------------------------------------------------------------------
//
// `TcpPeerRegistry` is a pure demux registry with no listener of its own.
// One instance is created at web_gateway startup and shared across all
// display sessions. The web_gateway's accept loop (which already peeks
// every incoming TCP connection for HTTP vs. WebSocket) grows a third
// branch: if the first bytes look like an RFC 4571-framed STUN binding
// request, read one full frame, then call `route_accepted` to hand the
// connection to the matching peer. HTTP-on-the-same-port works untouched
// because the peek is non-destructive and STUN traffic is
// byte-distinguishable from HTTP methods (no printable ASCII at offset 0)
// and TLS handshakes (no 0x16 at offset 0).

/// Shared peer registry: ufrag → handoff channel. Peers register at
/// construction time; `route_accepted` looks up the matching peer for an
/// incoming TCP connection.
pub struct TcpPeerRegistry {
    registry: std::sync::Mutex<HashMap<String, mpsc::Sender<AcceptedTcpConnection>>>,
}

/// A TCP connection that has been matched to a peer by its first STUN frame.
/// Carries the first frame (which the peer still needs to process) alongside
/// the stream so the peer can read subsequent frames and write outbound
/// transmits.
pub struct AcceptedTcpConnection {
    pub remote_addr: SocketAddr,
    pub local_addr: SocketAddr,
    /// The first frame we already read off the wire (needed for STUN ufrag
    /// matching). The peer's driver must feed this to the sans-I/O RTC core.
    pub first_frame: Vec<u8>,
    pub stream: TcpStream,
}

impl TcpPeerRegistry {
    /// Create an empty registry. Share the returned `Arc` across every
    /// caller that needs to register a peer or route a connection.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            registry: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Register a peer's local ufrag and return the receiver side of the
    /// per-peer connection channel. Drop the returned `PeerRegistration` to
    /// unregister on peer close.
    pub fn register(
        self: &Arc<Self>,
        local_ufrag: String,
    ) -> (PeerRegistration, mpsc::Receiver<AcceptedTcpConnection>) {
        let (tx, rx) = mpsc::channel::<AcceptedTcpConnection>(8);
        self.registry
            .lock()
            .unwrap()
            .insert(local_ufrag.clone(), tx);
        (
            PeerRegistration {
                registry: Arc::clone(self),
                local_ufrag,
            },
            rx,
        )
    }

    /// Route an already-accepted TCP connection plus its peeked first RFC
    /// 4571 frame to the peer whose local ufrag matches the STUN USERNAME
    /// in that frame. Called by the web_gateway's accept loop when it
    /// detects STUN-framed traffic on the HTTP port.
    pub async fn route_accepted(
        self: &Arc<Self>,
        stream: TcpStream,
        first_frame: Vec<u8>,
        remote_addr: SocketAddr,
    ) -> Result<(), String> {
        let local_addr = stream
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?;

        let username = parse_stun_username(&first_frame)
            .ok_or_else(|| "first frame is not a STUN binding request with USERNAME".to_string())?;

        // Per RFC 8445 §7.2.2, the STUN USERNAME attribute for an ICE
        // connectivity check sent from A to B is formatted as
        // `<B_ufrag>:<A_ufrag>` — target peer's ufrag first, sender's
        // ufrag second. When a browser → server request arrives at the
        // server, the FIRST segment is the server's ufrag (us, the
        // demux key) and the second is the browser's ufrag (which we
        // don't care about here). Getting this backwards makes every
        // incoming TCP connection fail routing lookup.
        let local_ufrag = username
            .split_once(':')
            .map(|(target, _sender)| target.to_string())
            .ok_or_else(|| format!("bad USERNAME format: {username:?}"))?;

        let tx = {
            let guard = self.registry.lock().unwrap();
            guard.get(&local_ufrag).cloned()
        };
        let Some(tx) = tx else {
            return Err(format!("no peer registered for ufrag {local_ufrag:?}"));
        };

        let accepted = AcceptedTcpConnection {
            remote_addr,
            local_addr,
            first_frame,
            stream,
        };
        tx.send(accepted).await.map_err(|_| {
            "peer channel closed before we could hand over the connection".to_string()
        })?;
        Ok(())
    }
}

/// RAII guard that unregisters a peer's ufrag from the registry on drop.
pub struct PeerRegistration {
    registry: Arc<TcpPeerRegistry>,
    local_ufrag: String,
}

impl Drop for PeerRegistration {
    fn drop(&mut self) {
        self.registry
            .registry
            .lock()
            .unwrap()
            .remove(&self.local_ufrag);
    }
}

impl TcpPeerRegistry {
    /// Return `true` if a peer with this ufrag is currently registered.
    /// Non-consuming — used by the gateway's accept loop to decide
    /// between local dispatch (this registry) and relay dispatch
    /// ([`TcpRelayRegistry`]). Separate from `route_accepted` because
    /// that call takes the stream by value, and we need to commit to
    /// one registry before handing over.
    pub fn contains_ufrag(&self, ufrag: &str) -> bool {
        self.registry.lock().unwrap().contains_key(ufrag)
    }
}

// ---------------------------------------------------------------------------
// TCP relay registry (ufrag → outbound peer address)
// ---------------------------------------------------------------------------
//
// Slice 3b: the federation-level equivalent of `TcpPeerRegistry`. Each
// entry maps a REMOTE peer's ICE ufrag to an outbound `SocketAddr`
// pointing at that peer's HTTP listener. When the gateway's accept
// loop sees an incoming STUN-framed TCP connection whose ufrag is
// here (and not in the local `TcpPeerRegistry`), the primary opens a
// fresh TCP connection to the outbound address, re-frames the peeked
// first frame, writes it, then bidirectionally shuttles bytes between
// the browser's stream and the peer's stream.
//
// The entries get populated by the `OutboundEvent::PeerEventForwarded`
// translator when it sees a federated `WebRtcSignal::Answer` flowing
// back from a peer to the browser: the translator parses the Answer's
// SDP for the peer's ICE ufrag, resolves the peer's
// `browser_tcp_via_url` / `ws_url` to a SocketAddr, and registers
// (ufrag → SocketAddr) here.
//
// When the browser's `RTCPeerConnection` tries the primary-relay TCP
// candidate (injected into the Answer SDP by the same translator
// alongside the peer's direct candidate), the connection lands on the
// primary's HTTP listener with the peer's ufrag in its first STUN
// USERNAME. `TcpPeerRegistry::contains_ufrag` returns false (no local
// match), `TcpRelayRegistry::contains_ufrag` returns true, the accept
// loop dispatches to the relay path.

/// Registry of `ufrag → outbound peer address` entries for federation-
/// level TCP relay. See the module-level comment above for flow.
pub struct TcpRelayRegistry {
    registry: std::sync::Mutex<HashMap<String, SocketAddr>>,
}

impl TcpRelayRegistry {
    /// Create an empty registry. Share the returned `Arc` across every
    /// caller that needs to register relay targets or route incoming
    /// TCP connections.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            registry: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Associate a remote peer's ICE ufrag with the outbound
    /// [`SocketAddr`] the primary will dial when it sees an incoming
    /// TCP connection carrying that ufrag. Idempotent — re-registering
    /// the same ufrag updates the outbound address (useful for peer
    /// reconnects that issue a fresh Answer with a new address).
    pub fn register(&self, ufrag: String, outbound: SocketAddr) {
        self.registry.lock().unwrap().insert(ufrag, outbound);
    }

    /// Remove a ufrag entry. Called when the corresponding federated
    /// WebRTC session closes (browser-initiated close, peer teardown,
    /// transport disconnect). Missing entries are silently ignored
    /// — idempotent cleanup.
    #[allow(dead_code)]
    pub fn unregister(&self, ufrag: &str) {
        self.registry.lock().unwrap().remove(ufrag);
    }

    /// Look up the outbound address for a ufrag. Returns `None` when
    /// no relay entry exists (typical case for ufrags belonging to
    /// locally-hosted WebRTC peers handled by `TcpPeerRegistry`).
    pub fn lookup(&self, ufrag: &str) -> Option<SocketAddr> {
        self.registry.lock().unwrap().get(ufrag).copied()
    }

    /// Return `true` if an entry exists for this ufrag. Non-consuming
    /// — used by the gateway's accept loop to dispatch between local
    /// and relay paths.
    pub fn contains_ufrag(&self, ufrag: &str) -> bool {
        self.registry.lock().unwrap().contains_key(ufrag)
    }

    /// Route an already-accepted STUN-framed TCP connection through
    /// the relay: dial the peer, re-frame and write the peeked first
    /// frame, then spawn a bidirectional byte-forwarding task for the
    /// remainder. Returns an error if the lookup misses or the
    /// outbound connect fails — caller closes the stream in that case.
    ///
    /// `first_frame` is the RFC 4571 payload (without the 2-byte length
    /// prefix) that the gateway already consumed from the stream; we
    /// re-wrap it before writing to the peer so the peer's own accept
    /// loop sees the same framed STUN bytes it would have seen from a
    /// direct browser connection.
    pub async fn route_accepted(
        self: &Arc<Self>,
        stream: TcpStream,
        first_frame: Vec<u8>,
    ) -> Result<(), String> {
        let username = parse_stun_username(&first_frame)
            .ok_or_else(|| "first frame is not a STUN binding request with USERNAME".to_string())?;
        // Target ufrag is the first half of `target:sender`, same as
        // TcpPeerRegistry's dispatch — RFC 8445 §7.2.2.
        let local_ufrag = username
            .split_once(':')
            .map(|(target, _sender)| target.to_string())
            .ok_or_else(|| format!("bad USERNAME format: {username:?}"))?;

        let outbound_addr = self
            .lookup(&local_ufrag)
            .ok_or_else(|| format!("no relay registered for ufrag {local_ufrag:?}"))?;

        // Dial the peer. If this fails, the browser's ICE will see
        // the TCP pair as unformable and (usually) fall back to UDP
        // or time out — no retry at this layer.
        let mut outbound = TcpStream::connect(outbound_addr)
            .await
            .map_err(|e| format!("dial {outbound_addr}: {e}"))?;

        // Re-frame the peeked first frame and write it to the peer so
        // the peer's accept loop sees the same RFC 4571-framed STUN
        // bytes the browser originally sent.
        write_rfc4571_frame(&mut outbound, &first_frame)
            .await
            .map_err(|e| format!("write first frame to {outbound_addr}: {e}"))?;

        // Spawn a bidirectional byte-forwarder. `copy_bidirectional`
        // handles both directions concurrently and exits when either
        // side closes — matches the ICE-TCP lifecycle where a single
        // candidate pair's TCP connection lives for the WebRTC
        // session's duration.
        let mut stream = stream;
        tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut outbound).await;
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public helpers for ufrag / SDP manipulation (slice 3b)
// ---------------------------------------------------------------------------

/// Parse the ICE `ufrag` out of an SDP Answer. Looks for the first
/// session-level or media-level `a=ice-ufrag:<value>` attribute and
/// returns the value. Returns `None` if no such attribute is present,
/// which is a malformed SDP per RFC 5245 — callers treat it as
/// "this Answer isn't relay-able, skip the rewrite."
///
/// Exposed publicly so the `OutboundEvent` translator in
/// `web_gateway.rs` can extract the ufrag from an incoming federated
/// `WebRtcSignal::Answer` and register it in [`TcpRelayRegistry`]
/// keyed to the outbound peer address.
pub fn parse_sdp_ice_ufrag(sdp: &str) -> Option<String> {
    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("a=ice-ufrag:") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Parse just the STUN USERNAME attribute's ufrag out of an RFC 4571
/// frame payload. Wrapper around `parse_stun_username` + the
/// `target:sender` split used by ICE. Returns the TARGET ufrag (the
/// first half) — the one keyed in the ufrag registries.
///
/// Returns `None` when the frame isn't a STUN binding request, lacks
/// a USERNAME attribute, or the username isn't in the expected
/// `target:sender` format.
pub fn parse_first_frame_ufrag(first_frame: &[u8]) -> Option<String> {
    let username = parse_stun_username(first_frame)?;
    username
        .split_once(':')
        .map(|(target, _sender)| target.to_string())
}

/// Inject an additional ICE-TCP host candidate into an SDP Answer,
/// pointing at the primary daemon's own address so the browser has a
/// relay-path candidate alongside the peer's direct candidate.
///
/// The injected line is placed immediately after the first existing
/// `a=candidate:` line (or, if there are no candidate lines, at the
/// end of the first media section). `foundation` is deliberately
/// distinct from normal local candidate values to avoid collision; `priority`
/// is set lower than a typical host-TCP-passive candidate so ICE
/// prefers the peer's direct candidate when reachable and only falls
/// back to the relay when direct fails.
///
/// IPv6 addresses are emitted in canonical form; IPv4 addresses as
/// dotted-quad. `component_id` is always 1
/// (RTP; same-stream RTCP multiplexed per `a=rtcp-mux`).
///
/// Returns the modified SDP as a new `String`. Pure function — never
/// mutates the input.
pub fn inject_relay_tcp_candidate(sdp: &str, primary_addr: SocketAddr) -> String {
    // Priority formula per RFC 5245 §4.1.2.1:
    //   priority = (2^24)*type_pref + (2^8)*local_pref + (256 - component_id)
    //
    // type_pref for host is 126; we use 100 so the relay candidate's
    // priority is strictly below a typical peer-direct host TCP
    // candidate (host candidates normally use type_pref 126). local_pref
    // is 0 (single interface) since the distinction doesn't help here.
    //
    // Result: priority = (2^24)*100 + 0 + 255 = 1_677_721_855.
    let type_pref: u32 = 100;
    let local_pref: u32 = 0;
    let component_id: u32 = 1;
    let priority =
        (1u32 << 24).saturating_mul(type_pref) + (1u32 << 8) * local_pref + (256 - component_id);
    let ip = match primary_addr.ip() {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => v6.to_string(),
    };
    let port = primary_addr.port();
    // Foundation 9001 is arbitrary; picked to not collide with common
    // typical sequential foundations (1, 2, ...). Same foundation for
    // every injected candidate is fine per RFC 5245 since foundations
    // only need to be unique-per-stream within a single side's set.
    let candidate_line = format!(
        "a=candidate:9001 {component_id} tcp {priority} {ip} {port} typ host tcptype passive generation 0"
    );

    // Walk the SDP line by line. Insert the new candidate immediately
    // after the first existing `a=candidate:` line (keeps the candidate
    // block contiguous, which matches how SDP is conventionally laid
    // out). If there are no existing candidate lines, append at the
    // end. Preserve line endings as they were (CRLF or LF).
    let newline = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut inserted = false;
    let mut out = String::with_capacity(sdp.len() + candidate_line.len() + 2);
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        if !inserted
            && line
                .trim_end_matches(['\r', '\n'])
                .starts_with("a=candidate:")
        {
            out.push_str(&candidate_line);
            out.push_str(newline);
            inserted = true;
        }
    }
    if !inserted {
        // No existing candidates — append at end, making sure we've
        // got a newline separator first.
        if !out.ends_with('\n') {
            out.push_str(newline);
        }
        out.push_str(&candidate_line);
        out.push_str(newline);
    }
    out
}

// ---------------------------------------------------------------------------
// RFC 4571 framing
// ---------------------------------------------------------------------------

/// Read one RFC 4571 framed payload from a `tokio::io::AsyncRead`:
/// 2-byte big-endian length header followed by `length` bytes of payload.
///
/// Generic over the read source so we can reuse it for a `TcpStream`
/// (during dispatcher probe) and an `OwnedReadHalf` (inside the per-peer
/// reader task after `into_split`).
pub async fn read_rfc4571_frame<R>(r: &mut R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 2];
    r.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len == 0 || len > TCP_MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("RFC 4571 frame length {len} out of bounds"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Public wrapper around `read_rfc4571_frame` for the web gateway's
/// ICE-TCP detection path. The gateway peeks the first bytes to decide
/// between HTTP/WS/ICE-TCP, and when it picks ICE-TCP it needs to consume
/// that first frame from the stream before handing ownership to the
/// `TcpPeerRegistry`. We don't want to re-export the generic helper
/// cross-module, so this is a concrete version for `TcpStream`.
pub async fn read_rfc4571_frame_pub(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    read_rfc4571_frame(stream).await
}

/// Write one RFC 4571 framed payload: prepend a 2-byte BE length header,
/// then the payload bytes.
pub async fn write_rfc4571_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    payload: &[u8],
) -> std::io::Result<()> {
    if payload.len() > TCP_MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("RFC 4571 frame too large: {}", payload.len()),
        ));
    }
    let len = payload.len() as u16;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(payload).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal STUN parser (USERNAME attribute only)
// ---------------------------------------------------------------------------

/// Parse just enough of a STUN message (RFC 5389) to extract the USERNAME
/// attribute value (type 0x0006). Returns `None` for non-STUN or malformed
/// input, or STUN messages without a USERNAME attribute.
pub(crate) fn parse_stun_username(bytes: &[u8]) -> Option<String> {
    // Header: 20 bytes
    //   type (2) | length (2) | magic cookie (4) | transaction id (12)
    if bytes.len() < 20 {
        return None;
    }
    // Magic cookie must be 0x2112A442 per RFC 5389.
    if bytes[4..8] != [0x21, 0x12, 0xA4, 0x42] {
        return None;
    }
    let msg_length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    let attrs_end = 20usize.checked_add(msg_length)?;
    if bytes.len() < attrs_end {
        return None;
    }

    let mut offset = 20usize;
    while offset + 4 <= attrs_end {
        let attr_type = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        let attr_length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        let value_start = offset + 4;
        let value_end = value_start.checked_add(attr_length)?;
        if value_end > attrs_end {
            return None;
        }
        if attr_type == 0x0006 {
            // USERNAME — UTF-8 string per RFC 5389 §15.3.
            return std::str::from_utf8(&bytes[value_start..value_end])
                .ok()
                .map(String::from);
        }
        // Advance past value, padded to a 4-byte boundary.
        let pad = (4 - (attr_length % 4)) % 4;
        offset = value_end + pad;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- STUN parser tests ---

    fn make_stun_binding_request(username: &str) -> Vec<u8> {
        // Minimal STUN Binding Request with USERNAME attribute.
        // Header: type 0x0001, length TBD, magic 0x2112A442, txid 12 zeros.
        let username_bytes = username.as_bytes();
        let attr_len = username_bytes.len();
        let padded = (attr_len + 3) & !3;
        let msg_len = 4 + padded; // attr header (4) + padded value

        let mut buf = Vec::new();
        buf.extend_from_slice(&0x0001u16.to_be_bytes()); // type
        buf.extend_from_slice(&(msg_len as u16).to_be_bytes()); // length
        buf.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]); // magic
        buf.extend_from_slice(&[0u8; 12]); // transaction ID
                                           // USERNAME attribute
        buf.extend_from_slice(&0x0006u16.to_be_bytes()); // attr type
        buf.extend_from_slice(&(attr_len as u16).to_be_bytes());
        buf.extend_from_slice(username_bytes);
        buf.resize(buf.len() + padded - attr_len, 0); // padding
        buf
    }

    #[test]
    fn stun_username_extracted() {
        let pkt = make_stun_binding_request("serverufrag:browserufrag");
        assert_eq!(
            parse_stun_username(&pkt),
            Some("serverufrag:browserufrag".to_string())
        );
    }

    #[test]
    fn stun_username_missing() {
        // STUN packet with no attributes at all
        let mut pkt = vec![0u8; 20];
        pkt[0] = 0x00;
        pkt[1] = 0x01; // Binding Request
        pkt[2] = 0x00;
        pkt[3] = 0x00; // length = 0
        pkt[4..8].copy_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        assert_eq!(parse_stun_username(&pkt), None);
    }

    #[test]
    fn stun_not_stun() {
        // Wrong magic cookie
        assert_eq!(parse_stun_username(&[0u8; 20]), None);
    }

    #[test]
    fn stun_too_short() {
        assert_eq!(parse_stun_username(&[0u8; 5]), None);
    }

    #[test]
    fn ufrag_split_extracts_target_not_sender() {
        // RFC 8445 §7.2.2: USERNAME = <target_ufrag>:<sender_ufrag>
        // When routing a browser → server request, the FIRST half is the
        // server's ufrag (us), the second is the browser's. The original
        // bug was taking the second half and failing every lookup.
        let username = "serverABC:browserXYZ";
        let target = username
            .split_once(':')
            .map(|(target, _sender)| target.to_string());
        assert_eq!(target, Some("serverABC".to_string()));
    }

    // --- Slice 3b: relay helpers ---

    /// `parse_sdp_ice_ufrag` finds the first `a=ice-ufrag:` attribute
    /// and returns its value. Handles both session-level and
    /// media-level attributes transparently — ICE ufrag can appear in
    /// either per RFC 5245.
    #[test]
    fn parse_sdp_ice_ufrag_finds_session_or_media_level() {
        let sdp = "v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\na=ice-ufrag:abc123\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n";
        assert_eq!(parse_sdp_ice_ufrag(sdp).as_deref(), Some("abc123"));
        // Also LF-only input, since some producers emit LF not CRLF.
        let sdp_lf = "v=0\nm=video 9 UDP/TLS/RTP/SAVPF 96\na=ice-ufrag:xyz789\n";
        assert_eq!(parse_sdp_ice_ufrag(sdp_lf).as_deref(), Some("xyz789"));
    }

    /// Malformed SDPs — no `a=ice-ufrag:` line, or empty value — return
    /// `None`. The translator treats that as "can't relay this Answer."
    #[test]
    fn parse_sdp_ice_ufrag_returns_none_on_malformed() {
        assert_eq!(
            parse_sdp_ice_ufrag("v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\n"),
            None
        );
        assert_eq!(parse_sdp_ice_ufrag("a=ice-ufrag:\r\n"), None);
        assert_eq!(parse_sdp_ice_ufrag(""), None);
    }

    /// `parse_first_frame_ufrag` extracts the TARGET (server-side)
    /// ufrag from a STUN binding request's USERNAME attribute, which
    /// is the `target:sender` format per RFC 8445.
    #[test]
    fn parse_first_frame_ufrag_picks_target_half() {
        let frame = make_stun_binding_request("peerXYZ:browserABC");
        assert_eq!(parse_first_frame_ufrag(&frame).as_deref(), Some("peerXYZ"));
    }

    /// Non-STUN input or USERNAME missing the `:` separator returns
    /// `None`. Guards against the translator logging a spurious
    /// "relay missed" on garbage input.
    #[test]
    fn parse_first_frame_ufrag_returns_none_on_non_stun() {
        assert_eq!(parse_first_frame_ufrag(b"GET / HTTP/1.1\r\n"), None);
        let bad = make_stun_binding_request("no-colon-here");
        assert_eq!(parse_first_frame_ufrag(&bad), None);
    }

    /// `inject_relay_tcp_candidate` adds a new `a=candidate:` line
    /// right after the first existing one, preserving the original
    /// line and the rest of the SDP verbatim.
    #[test]
    fn inject_relay_tcp_candidate_adds_line_after_first_existing() {
        use std::net::{Ipv4Addr, SocketAddr};
        let original = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=candidate:1 1 tcp 2113937151 10.0.0.1 8765 typ host tcptype passive\r\na=end-of-candidates\r\n";
        let addr = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let rewritten = inject_relay_tcp_candidate(original, addr);
        assert!(
            rewritten.contains("a=candidate:1 1 tcp 2113937151 10.0.0.1 8765"),
            "original candidate line preserved: {rewritten}"
        );
        assert!(
            rewritten.contains("a=candidate:9001 1 tcp "),
            "injected relay candidate present (foundation 9001): {rewritten}"
        );
        assert!(
            rewritten.contains("192.168.1.42 8765"),
            "injected candidate carries primary address: {rewritten}"
        );
        assert!(
            rewritten.contains("a=end-of-candidates"),
            "post-candidate lines preserved: {rewritten}"
        );
        // CRLF preserved.
        assert!(rewritten.contains("\r\n"), "CRLF preserved");
    }

    /// When the SDP has no existing candidate lines, injection
    /// appends at the end (rather than failing).
    #[test]
    fn inject_relay_tcp_candidate_appends_when_no_candidates_present() {
        use std::net::{Ipv4Addr, SocketAddr};
        let original = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n";
        let addr = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 197).into(), 8765);
        let rewritten = inject_relay_tcp_candidate(original, addr);
        assert!(
            rewritten.contains("a=candidate:9001 "),
            "injected: {rewritten}"
        );
        assert!(rewritten.starts_with("v=0\r\n"), "SDP preamble preserved");
    }

    /// Injected candidate has `typ host tcptype passive` — what
    /// browsers expect for a TCP passive candidate they can dial.
    #[test]
    fn inject_relay_tcp_candidate_uses_host_passive_type() {
        use std::net::{Ipv4Addr, SocketAddr};
        let addr = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        let rewritten = inject_relay_tcp_candidate("", addr);
        assert!(
            rewritten.contains("typ host tcptype passive"),
            "expected host+passive: {rewritten}"
        );
    }

    /// IPv6 addresses render as their canonical string form (no
    /// brackets — SDP candidate IPs aren't bracketed, unlike URLs).
    #[test]
    fn inject_relay_tcp_candidate_renders_ipv6_without_brackets() {
        use std::net::{Ipv6Addr, SocketAddr};
        let addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8443);
        let rewritten = inject_relay_tcp_candidate("", addr);
        assert!(
            rewritten.contains("::1 8443"),
            "IPv6 in candidate line without brackets: {rewritten}"
        );
        assert!(
            !rewritten.contains("[::1]"),
            "no brackets in candidate line (URL-style brackets are SDP-invalid)"
        );
    }

    /// `TcpRelayRegistry` round-trips entries and reports presence.
    /// Locks the contract the gateway's accept-loop dispatch relies on.
    #[test]
    fn tcp_relay_registry_roundtrip() {
        use std::net::{Ipv4Addr, SocketAddr};
        let reg = TcpRelayRegistry::new();
        let addr = SocketAddr::new(Ipv4Addr::new(192, 168, 64, 3).into(), 8765);
        assert!(!reg.contains_ufrag("abc"));
        assert_eq!(reg.lookup("abc"), None);
        reg.register("abc".into(), addr);
        assert!(reg.contains_ufrag("abc"));
        assert_eq!(reg.lookup("abc"), Some(addr));
        reg.unregister("abc");
        assert!(!reg.contains_ufrag("abc"));
        // Double-unregister is idempotent.
        reg.unregister("abc");
    }

    /// Re-registering the same ufrag updates the outbound address
    /// (reconnect case — same peer issues a fresh answer with a new
    /// address).
    #[test]
    fn tcp_relay_registry_reregister_updates_address() {
        use std::net::{Ipv4Addr, SocketAddr};
        let reg = TcpRelayRegistry::new();
        let a1 = SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), 8765);
        let a2 = SocketAddr::new(Ipv4Addr::new(10, 0, 0, 2).into(), 9090);
        reg.register("same-ufrag".into(), a1);
        reg.register("same-ufrag".into(), a2);
        assert_eq!(reg.lookup("same-ufrag"), Some(a2));
    }
}

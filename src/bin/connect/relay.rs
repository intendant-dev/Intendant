//! SNI-passthrough reachability relay (docs/src/self-hosted-rendezvous.md).
//!
//! A NAT'd daemon is unreachable at its fleet name except on the LAN: fleet
//! DNS publishes addresses, it does not tunnel. This subsystem adds a lane
//! that makes the fleet name reachable from anywhere WITHOUT the relay ever
//! seeing plaintext:
//!
//!   - The daemon holds a persistent authenticated control channel to this
//!     service (`POST /api/relay/next`, daemon-signed with the same freshness
//!     discipline as the fleet-DNS publishes). Each poll refreshes the
//!     daemon's tunnel presence, keyed by its derived fleet label.
//!   - A separate raw TCP listener (`--relay-listen`) receives browser TLS
//!     connections. It PEEKS the ClientHello to read the SNI **without
//!     terminating TLS**. When the SNI names a fleet label with an active
//!     tunnel, the relay mints a single-use nonce, hands it to the daemon over
//!     the control channel, waits for the daemon to dial back a data
//!     connection carrying that nonce, and then splices raw bytes both ways.
//!   - The browser's TLS handshake therefore completes against the DAEMON's
//!     own fleet certificate; this service moves only ciphertext.
//!
//! Trust posture: availability-only. The relay terminates no TLS, holds no
//! certificate, mints no authority, and never inspects plaintext. Routing a
//! fleet SNI to a daemon does not change how the daemon classifies that
//! connection — it still arrives bearing the fleet SNI, which the daemon
//! gateway treats as discovery-only exactly as it does today. Abuse is bounded
//! by per-source-IP and per-tunnel connection caps, a per-connection byte cap,
//! and idle teardown.
//!
//! All of this is dark by default and gated behind the `--relay-*` config
//! group (all-or-nothing, mirroring the `--dns-*` group).

use super::*;

use std::net::IpAddr;
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// Daemon-signed control-channel protocol tag. The daemon long-polls this
/// endpoint to receive dial-back requests; the signature proves the poll comes
/// from the registered identity key, mirroring the fleet-DNS publish auth.
pub(crate) const RELAY_CONTROL_PROTOCOL: &str = "intendant-connect-relay-control-v1";
/// Daemon-signed DNS relay-mode protocol tag: "answer my fleet label with the
/// relay's address instead of my own".
pub(crate) const DNS_RELAY_PROTOCOL: &str = "intendant-connect-dns-relay-v1";

/// First line a daemon writes on a dial-back data connection: this magic and
/// the single-use nonce it received on the control channel. Deliberately does
/// NOT begin with the TLS handshake content type (`0x16`), so the relay's
/// first-byte demux never confuses a dial-back with a browser ClientHello.
const DIALBACK_MAGIC: &str = "ITRLY1";
/// Bytes read while looking for the dial-back hello's terminating newline.
const DIALBACK_HELLO_MAX_BYTES: usize = 160;

/// Max concurrent relay connections accepted from one source IP (browser
/// side). Bounds a single abusive client; daemon dial-back connections are
/// nonce-gated and exempt.
pub(crate) const RELAY_MAX_CONNS_PER_IP: u32 = 64;
/// Max concurrent spliced browser connections routed into one daemon tunnel.
pub(crate) const RELAY_MAX_CONNS_PER_TUNNEL: u32 = 128;
/// Max unclaimed dial-back nonces queued for one tunnel's control poll.
pub(crate) const RELAY_MAX_PENDING_PER_TUNNEL: usize = 64;
/// Idle teardown: a spliced connection with no bytes in either direction for
/// this long is closed.
pub(crate) const RELAY_SPLICE_IDLE: Duration = Duration::from_secs(120);
/// Per-direction byte cap on a single spliced connection. Generous — it bounds
/// one abusive connection, not a legitimate session — and both directions are
/// capped independently.
pub(crate) const RELAY_SPLICE_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// How long a browser connection waits for the daemon's dial-back before the
/// relay gives up and closes it.
pub(crate) const RELAY_DIALBACK_TIMEOUT: Duration = Duration::from_secs(10);
/// A tunnel counts as active only if its control channel polled within this
/// window (mirrors the daemon "online" liveness used elsewhere).
pub(crate) const RELAY_TUNNEL_LIVENESS_MS: u64 = 45_000;
/// Cap on bytes peeked while waiting for a complete ClientHello, and the wait
/// budget for it to arrive. A ClientHello far larger than this, or one that
/// never completes, is refused.
pub(crate) const RELAY_CLIENT_HELLO_PEEK_CAP: usize = 8192;
pub(crate) const RELAY_CLIENT_HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Long-poll cap for the control channel — kept below the global request
/// deadline so a parked poll always ends naturally inside the shutdown drain.
const RELAY_CONTROL_POLL_CAP_MS: u64 = 15_000;

// ── ClientHello SNI peek parser ─────────────────────────────────────────────

/// Outcome of peeking a (possibly partial) TLS ClientHello for its SNI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SniPeek {
    /// A valid TLS ClientHello prefix, but truncated — peek more bytes.
    NeedMore,
    /// Not a TLS handshake ClientHello at all — refuse.
    NotTls,
    /// A complete ClientHello carrying no SNI host_name — refuse (the relay
    /// routes solely by SNI).
    NoSni,
    /// The SNI host_name.
    Sni(String),
}

/// Why a structural read could not complete.
enum Take {
    NeedMore,
    NotTls,
}

fn read_u8(b: &[u8], pos: &mut usize) -> Result<u8, Take> {
    let v = *b.get(*pos).ok_or(Take::NeedMore)?;
    *pos += 1;
    Ok(v)
}

fn read_u16(b: &[u8], pos: &mut usize) -> Result<usize, Take> {
    let hi = read_u8(b, pos)? as usize;
    let lo = read_u8(b, pos)? as usize;
    Ok((hi << 8) | lo)
}

fn skip(b: &[u8], pos: &mut usize, n: usize) -> Result<(), Take> {
    let end = pos.checked_add(n).ok_or(Take::NotTls)?;
    if end > b.len() {
        return Err(Take::NeedMore);
    }
    *pos = end;
    Ok(())
}

/// Parse a TLS record + ClientHello prefix for its SNI host_name. Panic-free
/// on ANY input: every read is bounds-checked, so non-TLS garbage refuses
/// cleanly and a truncated handshake asks for more bytes.
pub(crate) fn parse_client_hello_sni(buf: &[u8]) -> SniPeek {
    match parse_sni_inner(buf) {
        Ok(Some(name)) => SniPeek::Sni(name),
        Ok(None) => SniPeek::NoSni,
        Err(Take::NeedMore) => SniPeek::NeedMore,
        Err(Take::NotTls) => SniPeek::NotTls,
    }
}

fn parse_sni_inner(buf: &[u8]) -> Result<Option<String>, Take> {
    let mut pos = 0;
    // TLS record header: content_type(1) legacy_version(2) length(2).
    if read_u8(buf, &mut pos)? != 0x16 {
        return Err(Take::NotTls); // not a handshake record
    }
    if read_u8(buf, &mut pos)? != 0x03 {
        return Err(Take::NotTls); // TLS legacy record version is 3.x
    }
    let _record_minor = read_u8(buf, &mut pos)?;
    let _record_len = read_u16(buf, &mut pos)?; // bound by the buffer, not this
                                                // Handshake header: msg_type(1) length(3).
    if read_u8(buf, &mut pos)? != 0x01 {
        return Err(Take::NotTls); // not a ClientHello
    }
    let _hi = read_u8(buf, &mut pos)?;
    let _mid = read_u8(buf, &mut pos)?;
    let _lo = read_u8(buf, &mut pos)?;
    // ClientHello body: legacy_version(2) random(32) then variable fields.
    let _legacy_version = read_u16(buf, &mut pos)?;
    skip(buf, &mut pos, 32)?;
    let sid_len = read_u8(buf, &mut pos)? as usize;
    skip(buf, &mut pos, sid_len)?;
    let cipher_len = read_u16(buf, &mut pos)?;
    skip(buf, &mut pos, cipher_len)?;
    let comp_len = read_u8(buf, &mut pos)? as usize;
    skip(buf, &mut pos, comp_len)?;
    // Extensions block.
    let ext_total = read_u16(buf, &mut pos)?;
    let ext_end = pos.checked_add(ext_total).ok_or(Take::NotTls)?;
    if ext_end > buf.len() {
        return Err(Take::NeedMore);
    }
    while pos < ext_end {
        if pos + 4 > ext_end {
            return Err(Take::NotTls); // extension header straddles the block end
        }
        let ext_type = read_u16(buf, &mut pos)?;
        let ext_len = read_u16(buf, &mut pos)?;
        let ext_data_end = pos.checked_add(ext_len).ok_or(Take::NotTls)?;
        if ext_data_end > ext_end {
            return Err(Take::NotTls);
        }
        if ext_type == 0x0000 {
            return parse_server_name_ext(&buf[pos..ext_data_end]);
        }
        pos = ext_data_end;
    }
    Ok(None)
}

fn parse_server_name_ext(data: &[u8]) -> Result<Option<String>, Take> {
    let mut pos = 0;
    let list_len = read_u16(data, &mut pos)?;
    let list_end = pos.checked_add(list_len).ok_or(Take::NotTls)?;
    if list_end > data.len() {
        return Err(Take::NotTls); // the extension declared its own length already
    }
    while pos < list_end {
        if pos + 3 > list_end {
            return Err(Take::NotTls);
        }
        let name_type = read_u8(data, &mut pos)?;
        let name_len = read_u16(data, &mut pos)?;
        let name_end = pos.checked_add(name_len).ok_or(Take::NotTls)?;
        if name_end > list_end {
            return Err(Take::NotTls);
        }
        if name_type == 0x00 {
            let raw = &data[pos..name_end];
            return match std::str::from_utf8(raw) {
                Ok(name) if is_plausible_sni(name) => Ok(Some(name.to_ascii_lowercase())),
                _ => Err(Take::NotTls),
            };
        }
        pos = name_end;
    }
    Ok(None)
}

/// A defensive shape check on the SNI before it is used as a routing key: DNS
/// names only, bounded length, no path/scheme/control characters. Routing is
/// still by exact label match against registered tunnels; this only keeps
/// junk out of logs and lookups.
fn is_plausible_sni(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// The leftmost DNS label of a fleet SNI (`d-<hash>.<zone>` -> `d-<hash>`).
/// This is the routing key: it equals `dns::daemon_label(daemon_id)` for the
/// daemon that owns the name.
fn sni_route_label(sni: &str) -> Option<String> {
    let label = sni.trim().trim_end_matches('.').split('.').next()?;
    if label.is_empty() {
        None
    } else {
        Some(label.to_ascii_lowercase())
    }
}

// ── Relay state ─────────────────────────────────────────────────────────────

/// A daemon's live control-channel presence plus its queue of unclaimed
/// dial-back nonces.
struct TunnelEntry {
    last_seen_unix_ms: u64,
    pending: VecDeque<String>,
}

/// Shared relay state, hung off `AppState` (None when the relay is disabled),
/// exactly as `dns_zone` hangs the fleet DNS zone.
pub(crate) struct RelayState {
    /// Where the raw relay listener binds.
    listen: SocketAddr,
    /// Public address(es) advertised in fleet DNS for relay-mode daemons.
    advertise_addrs: Vec<IpAddr>,
    /// label -> control-channel presence.
    tunnels: StdMutex<HashMap<String, TunnelEntry>>,
    /// nonce -> the browser splicer waiting for the daemon's dial-back.
    pending_dialbacks: StdMutex<HashMap<String, oneshot::Sender<TcpStream>>>,
    /// Wakes parked control polls when a nonce is enqueued for them.
    control_notify: Notify,
    /// Per-source-IP concurrent browser connections.
    ip_conns: StdMutex<HashMap<IpAddr, u32>>,
    /// Per-tunnel concurrent spliced browser connections.
    tunnel_splices: StdMutex<HashMap<String, u32>>,
}

impl RelayState {
    pub(crate) fn new(listen: SocketAddr, advertise_addrs: Vec<IpAddr>) -> Self {
        Self {
            listen,
            advertise_addrs,
            tunnels: StdMutex::new(HashMap::new()),
            pending_dialbacks: StdMutex::new(HashMap::new()),
            control_notify: Notify::new(),
            ip_conns: StdMutex::new(HashMap::new()),
            tunnel_splices: StdMutex::new(HashMap::new()),
        }
    }

    pub(crate) fn advertise_addrs(&self) -> &[IpAddr] {
        &self.advertise_addrs
    }

    pub(crate) fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// Refresh a tunnel's control-channel presence on each poll.
    fn touch_tunnel(&self, label: &str, now: u64) {
        let mut tunnels = self.tunnels.lock().expect("relay tunnels poisoned");
        tunnels
            .entry(label.to_string())
            .or_insert_with(|| TunnelEntry {
                last_seen_unix_ms: now,
                pending: VecDeque::new(),
            })
            .last_seen_unix_ms = now;
    }

    /// Pop the next unclaimed dial-back nonce for a tunnel, if any.
    fn pop_pending(&self, label: &str) -> Option<String> {
        let mut tunnels = self.tunnels.lock().expect("relay tunnels poisoned");
        tunnels.get_mut(label)?.pending.pop_front()
    }

    /// Whether a tunnel has an active (recently polled) control channel.
    fn tunnel_active(&self, label: &str, now: u64) -> bool {
        let tunnels = self.tunnels.lock().expect("relay tunnels poisoned");
        tunnels.get(label).is_some_and(|entry| {
            now.saturating_sub(entry.last_seen_unix_ms) <= RELAY_TUNNEL_LIVENESS_MS
        })
    }

    /// Queue a dial-back nonce for a tunnel's control poll. Fails closed if the
    /// tunnel is not active or its queue is at capacity.
    fn enqueue_dialback(&self, label: &str, nonce: String, now: u64) -> bool {
        let mut tunnels = self.tunnels.lock().expect("relay tunnels poisoned");
        let Some(entry) = tunnels.get_mut(label) else {
            return false;
        };
        if now.saturating_sub(entry.last_seen_unix_ms) > RELAY_TUNNEL_LIVENESS_MS
            || entry.pending.len() >= RELAY_MAX_PENDING_PER_TUNNEL
        {
            return false;
        }
        entry.pending.push_back(nonce);
        drop(tunnels);
        self.control_notify.notify_waiters();
        true
    }

    /// Drop tunnels whose control channel has gone quiet, and reap any nonces
    /// queued under them. Called periodically so a daemon that stops polling
    /// stops being routable.
    fn sweep(&self, now: u64) {
        self.tunnels
            .lock()
            .expect("relay tunnels poisoned")
            .retain(|_, entry| {
                now.saturating_sub(entry.last_seen_unix_ms) <= RELAY_TUNNEL_LIVENESS_MS
            });
    }

    fn acquire_ip_conn(self: &Arc<Self>, ip: IpAddr) -> Option<IpConnGuard> {
        let mut map = self.ip_conns.lock().expect("relay ip conns poisoned");
        let count = map.entry(ip).or_insert(0);
        if *count >= RELAY_MAX_CONNS_PER_IP {
            return None;
        }
        *count += 1;
        Some(IpConnGuard {
            relay: self.clone(),
            ip,
        })
    }

    fn release_ip_conn(&self, ip: IpAddr) {
        let mut map = self.ip_conns.lock().expect("relay ip conns poisoned");
        if let Some(count) = map.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&ip);
            }
        }
    }

    fn acquire_tunnel_splice(self: &Arc<Self>, label: &str) -> Option<TunnelSpliceGuard> {
        let mut map = self.tunnel_splices.lock().expect("relay splices poisoned");
        let count = map.entry(label.to_string()).or_insert(0);
        if *count >= RELAY_MAX_CONNS_PER_TUNNEL {
            return None;
        }
        *count += 1;
        Some(TunnelSpliceGuard {
            relay: self.clone(),
            label: label.to_string(),
        })
    }

    fn release_tunnel_splice(&self, label: &str) {
        let mut map = self.tunnel_splices.lock().expect("relay splices poisoned");
        if let Some(count) = map.get_mut(label) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(label);
            }
        }
    }
}

struct IpConnGuard {
    relay: Arc<RelayState>,
    ip: IpAddr,
}

impl Drop for IpConnGuard {
    fn drop(&mut self) {
        self.relay.release_ip_conn(self.ip);
    }
}

struct TunnelSpliceGuard {
    relay: Arc<RelayState>,
    label: String,
}

impl Drop for TunnelSpliceGuard {
    fn drop(&mut self) {
        self.relay.release_tunnel_splice(&self.label);
    }
}

// ── Control channel: daemon long-poll for dial-back requests ────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct RelayNextRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
    issued_at_unix_ms: u64,
    signature: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn relay_control_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
) -> String {
    format!("{RELAY_CONTROL_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n")
}

/// Require the relay to be enabled, then run the shared daemon-signed
/// verification (bearer gate, rate limit, protocol + freshness, key pin).
/// `protocol` is `(got, expected)`, mirroring `verified_daemon_request`.
async fn relay_request_daemon(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    rate_key: &str,
    protocol: (&str, &str),
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
) -> ApiResult<DaemonRecord> {
    if state.relay.is_none() {
        return Err(ApiError::not_found(
            "reachability relay is not enabled on this rendezvous",
        ));
    }
    verified_daemon_request(
        state,
        headers,
        (rate_key, 120, 60_000),
        protocol,
        daemon_id,
        daemon_public_key,
        issued_at_unix_ms,
    )
    .await
}

/// Daemon control-channel long-poll. Authenticated by the daemon identity key
/// (same signed/freshness discipline as `/api/dns/publish`). Registers the
/// daemon's tunnel presence, then parks until a dial-back nonce is available
/// or the poll times out.
pub(crate) async fn relay_next(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RelayNextRequest>,
) -> ApiResult<Response> {
    let daemon_id = body.daemon_id.trim().to_string();
    let daemon = relay_request_daemon(
        &state,
        &headers,
        "relay_next",
        (&body.protocol, RELAY_CONTROL_PROTOCOL),
        &daemon_id,
        &body.daemon_public_key,
        body.issued_at_unix_ms,
    )
    .await?;
    let payload = relay_control_signing_payload(
        &daemon_id,
        &daemon.daemon_public_key,
        body.issued_at_unix_ms,
    );
    if !verify_ed25519_b64u(
        &daemon.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        return Err(ApiError::bad_request("relay control signature invalid"));
    }
    let relay = state
        .relay
        .as_ref()
        .expect("relay presence checked in relay_request_daemon")
        .clone();
    let Some(label) = daemon_label(&daemon_id) else {
        return Err(ApiError::bad_request(
            "daemon id does not derive a fleet label",
        ));
    };
    relay.touch_tunnel(&label, now_unix_ms());

    let timeout = Duration::from_millis(
        body.timeout_ms
            .unwrap_or(RELAY_CONTROL_POLL_CAP_MS)
            .min(RELAY_CONTROL_POLL_CAP_MS),
    );
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(nonce) = relay.pop_pending(&label) {
            return Ok(Json(json!({
                "ok": true,
                "dialback": { "nonce": nonce },
            }))
            .into_response());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        let remaining = deadline.saturating_duration_since(now);
        // Re-touch keeps the tunnel live across a full parked poll.
        relay.touch_tunnel(&label, now_unix_ms());
        if tokio::time::timeout(remaining, relay.control_notify.notified())
            .await
            .is_err()
        {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
    }
}

// ── DNS relay-mode publish ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct DnsRelayRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
    issued_at_unix_ms: u64,
    signature: String,
    /// true = answer this daemon's fleet label with the relay's address;
    /// false = stop (the daemon reverts to direct address publishing).
    #[serde(default)]
    enable: bool,
}

fn dns_relay_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
    enable: bool,
) -> String {
    let enable = if enable { "1" } else { "0" };
    format!(
        "{DNS_RELAY_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n{enable}\n"
    )
}

/// Daemon-signed relay-mode DNS publish: point the daemon's fleet label at the
/// relay's public address instead of the daemon's own addresses. Requires BOTH
/// the fleet DNS zone and the relay to be enabled (the former to serve the
/// record, the latter to supply the advertised address). The zone serves the
/// substituted address verbatim — the store/serve split is intact.
pub(crate) async fn dns_relay(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DnsRelayRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let daemon_id = body.daemon_id.trim().to_string();
    if state.dns_zone.is_none() {
        return Err(ApiError::not_found(
            "fleet dns is not enabled on this rendezvous",
        ));
    }
    let daemon = relay_request_daemon(
        &state,
        &headers,
        "dns_relay",
        (&body.protocol, DNS_RELAY_PROTOCOL),
        &daemon_id,
        &body.daemon_public_key,
        body.issued_at_unix_ms,
    )
    .await?;
    let payload = dns_relay_signing_payload(
        &daemon_id,
        &daemon.daemon_public_key,
        body.issued_at_unix_ms,
        body.enable,
    );
    if !verify_ed25519_b64u(
        &daemon.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        return Err(ApiError::bad_request("dns relay signature invalid"));
    }
    let relay = state
        .relay
        .as_ref()
        .expect("relay presence checked in relay_request_daemon")
        .clone();
    let advertise: Vec<IpAddr> = relay.advertise_addrs().to_vec();
    if body.enable && advertise.is_empty() {
        return Err(ApiError::not_found(
            "this relay advertises no public address (set --relay-address)",
        ));
    }
    let zone = state.dns_zone.as_ref().expect("checked above").clone();
    let name = zone
        .daemon_fqdn(&daemon_id)
        .ok_or_else(|| ApiError::bad_request("daemon id does not derive a DNS label"))?;
    let addresses = if body.enable {
        advertise.clone()
    } else {
        Vec::new()
    };
    zone.set_daemon_addresses(&daemon_id, &addresses)
        .map_err(ApiError::bad_request)?;
    let now = now_unix_ms();
    {
        let mut store = state.store.lock().await;
        store.dns_records.retain(|r| r.daemon_id != daemon_id);
        if body.enable {
            store.dns_records.push(DnsRecordEntry {
                daemon_id: daemon_id.clone(),
                addresses: advertise.iter().map(|ip| ip.to_string()).collect(),
                updated_unix_ms: now,
                via_relay: true,
            });
        }
        audit(
            &mut store,
            "dns_relay",
            daemon.owner_user_id,
            Some(daemon_id.clone()),
            json!({ "name": name, "enable": body.enable }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({
        "ok": true,
        "zone": zone.origin_utf8(),
        "name": name,
        "via_relay": body.enable,
        "addresses": addresses.iter().map(|ip| ip.to_string()).collect::<Vec<_>>(),
    })))
}

// ── Raw relay listener: browser TLS splice + daemon dial-back ───────────────

/// Bind the raw relay TCP socket and return the accept loop future. Binding is
/// eager so a misconfigured listener fails startup loudly, matching
/// `bind_fleet_dns`.
pub(crate) async fn bind_relay(
    state: Arc<AppState>,
    relay: Arc<RelayState>,
) -> Result<impl std::future::Future<Output = ()>, String> {
    let listener = TcpListener::bind(relay.listen)
        .await
        .map_err(|e| format!("bind relay listener {}: {e}", relay.listen))?;
    Ok(run_relay_accept_loop(state, relay, listener))
}

async fn run_relay_accept_loop(
    _state: Arc<AppState>,
    relay: Arc<RelayState>,
    listener: TcpListener,
) {
    // Periodic sweep of quiet tunnels alongside accepting.
    let sweep_relay = relay.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            sweep_relay.sweep(now_unix_ms());
        }
    });
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                // Transient accept errors: back off briefly and continue. The
                // relay is best-effort availability, so it never tears itself
                // down on an accept hiccup.
                eprintln!("[relay] accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        let relay = relay.clone();
        tokio::spawn(async move {
            handle_relay_connection(relay, stream, peer.ip()).await;
        });
    }
}

async fn handle_relay_connection(relay: Arc<RelayState>, stream: TcpStream, peer_ip: IpAddr) {
    // One peeked byte disambiguates a browser TLS ClientHello (0x16) from a
    // daemon dial-back hello (ASCII magic). `peek` leaves the bytes in the
    // kernel buffer so the browser path can forward the ClientHello verbatim.
    let mut first = [0u8; 1];
    match stream.peek(&mut first).await {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    if first[0] == 0x16 {
        handle_browser_connection(relay, stream, peer_ip).await;
    } else {
        handle_dialback_connection(relay, stream).await;
    }
}

/// A browser TLS connection: peek the ClientHello for its SNI (no
/// termination), route to the matching active tunnel, and splice.
async fn handle_browser_connection(relay: Arc<RelayState>, stream: TcpStream, peer_ip: IpAddr) {
    let Some(_ip_guard) = relay.acquire_ip_conn(peer_ip) else {
        return; // per-source-IP cap
    };
    let sni = match peek_sni(&stream).await {
        Some(sni) => sni,
        None => return, // fragmented-forever, oversized, non-TLS, or no-SNI: refuse
    };
    let Some(label) = sni_route_label(&sni) else {
        return;
    };
    let now = now_unix_ms();
    if !relay.tunnel_active(&label, now) {
        return; // no active tunnel for this fleet name
    }
    let Some(_splice_guard) = relay.acquire_tunnel_splice(&label) else {
        return; // per-tunnel cap
    };

    // Mint a single-use nonce, register the browser waiter, and ask the daemon
    // to dial back.
    let nonce = random_b64u(32);
    let (tx, rx) = oneshot::channel::<TcpStream>();
    {
        let mut pending = relay
            .pending_dialbacks
            .lock()
            .expect("relay dialbacks poisoned");
        pending.insert(nonce.clone(), tx);
    }
    if !relay.enqueue_dialback(&label, nonce.clone(), now) {
        relay
            .pending_dialbacks
            .lock()
            .expect("relay dialbacks poisoned")
            .remove(&nonce);
        return;
    }

    let data_stream = match tokio::time::timeout(RELAY_DIALBACK_TIMEOUT, rx).await {
        Ok(Ok(data_stream)) => data_stream,
        _ => {
            // Timed out or the waiter was dropped: reclaim the nonce slot.
            relay
                .pending_dialbacks
                .lock()
                .expect("relay dialbacks poisoned")
                .remove(&nonce);
            return;
        }
    };
    // Pure ciphertext splice: the browser's TLS records flow to the daemon,
    // whose fleet certificate completes the handshake. This service never sees
    // plaintext.
    splice(
        stream,
        data_stream,
        RELAY_SPLICE_MAX_BYTES,
        RELAY_SPLICE_IDLE,
    )
    .await;
}

/// A daemon dial-back data connection: read `ITRLY1 <nonce>\n`, hand this
/// stream to the waiting browser splicer.
async fn handle_dialback_connection(relay: Arc<RelayState>, mut stream: TcpStream) {
    let Some(nonce) = read_dialback_nonce(&mut stream).await else {
        return;
    };
    let sender = relay
        .pending_dialbacks
        .lock()
        .expect("relay dialbacks poisoned")
        .remove(&nonce);
    let Some(sender) = sender else {
        return; // unknown / expired / already-claimed nonce
    };
    // The browser splicer owns the splice; hand off this (post-hello) stream.
    let _ = sender.send(stream);
}

/// Read a bounded dial-back hello line and extract the nonce.
async fn read_dialback_nonce(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        match tokio::time::timeout(RELAY_DIALBACK_TIMEOUT, stream.read(&mut byte)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return None,
            Ok(Ok(_)) => {}
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > DIALBACK_HELLO_MAX_BYTES {
            return None;
        }
    }
    let line = std::str::from_utf8(&buf).ok()?;
    let mut parts = line.trim().splitn(2, ' ');
    if parts.next()? != DIALBACK_MAGIC {
        return None;
    }
    let nonce = parts.next()?.trim();
    if nonce.is_empty() || nonce.len() > 64 {
        return None;
    }
    Some(nonce.to_string())
}

/// Peek a complete ClientHello (up to a cap / timeout) and return its SNI.
/// Handles fragmented ClientHellos by re-peeking as more bytes arrive.
async fn peek_sni(stream: &TcpStream) -> Option<String> {
    let deadline = tokio::time::Instant::now() + RELAY_CLIENT_HELLO_TIMEOUT;
    let mut buf = vec![0u8; RELAY_CLIENT_HELLO_PEEK_CAP];
    loop {
        // `peek` always returns from byte zero, so a later peek with more
        // arrived bytes re-parses the growing prefix.
        let n = match stream.peek(&mut buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => n,
        };
        match parse_client_hello_sni(&buf[..n]) {
            SniPeek::Sni(name) => return Some(name),
            SniPeek::NoSni | SniPeek::NotTls => return None,
            SniPeek::NeedMore => {
                if n >= buf.len() {
                    return None; // ClientHello larger than the peek cap: refuse
                }
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
                // Wait for more bytes to land, then re-peek.
                if tokio::time::timeout(Duration::from_millis(50), readable(stream))
                    .await
                    .is_err()
                {
                    // Nothing new yet; loop re-checks the deadline.
                }
            }
        }
    }
}

async fn readable(stream: &TcpStream) {
    let _ = stream.readable().await;
}

/// Bidirectional byte splice with a per-direction byte cap and idle teardown.
/// When either direction finishes (EOF, error, cap, or idle) both halves drop
/// and the connection closes.
async fn splice(browser: TcpStream, daemon: TcpStream, max_bytes: u64, idle: Duration) {
    let (browser_r, browser_w) = browser.into_split();
    let (daemon_r, daemon_w) = daemon.into_split();
    let to_daemon = copy_half(browser_r, daemon_w, max_bytes, idle);
    let to_browser = copy_half(daemon_r, browser_w, max_bytes, idle);
    tokio::select! {
        _ = to_daemon => {}
        _ = to_browser => {}
    }
}

async fn copy_half<R, W>(mut reader: R, mut writer: W, max_bytes: u64, idle: Duration)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 16 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = match tokio::time::timeout(idle, reader.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => n,
        };
        total = total.saturating_add(n as u64);
        if total > max_bytes {
            break;
        }
        if writer.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};
    use tokio::net::{TcpListener, TcpStream};

    // ── ClientHello builder + SNI parser units ──────────────────────────────

    /// Build a minimal but structurally valid TLS ClientHello record carrying
    /// the given SNI (or none).
    fn build_client_hello(sni: Option<&str>) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]); // legacy_version = TLS 1.2
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0x00); // session_id length = 0
        body.extend_from_slice(&[0x00, 0x02]); // cipher_suites length
        body.extend_from_slice(&[0x00, 0x2f]); // one suite
        body.push(0x01); // compression_methods length
        body.push(0x00); // null compression

        let mut exts = Vec::new();
        if let Some(sni) = sni {
            let host = sni.as_bytes();
            let mut sn_list = Vec::new();
            sn_list.push(0x00); // name_type = host_name
            sn_list.extend_from_slice(&(host.len() as u16).to_be_bytes());
            sn_list.extend_from_slice(host);
            let mut ext_data = Vec::new();
            ext_data.extend_from_slice(&(sn_list.len() as u16).to_be_bytes());
            ext_data.extend_from_slice(&sn_list);
            exts.extend_from_slice(&[0x00, 0x00]); // extension type = server_name
            exts.extend_from_slice(&(ext_data.len() as u16).to_be_bytes());
            exts.extend_from_slice(&ext_data);
        }
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);

        let mut handshake = Vec::new();
        handshake.push(0x01); // ClientHello
        let blen = body.len();
        handshake.push((blen >> 16) as u8);
        handshake.push((blen >> 8) as u8);
        handshake.push(blen as u8);
        handshake.extend_from_slice(&body);

        let mut record = Vec::new();
        record.push(0x16); // handshake content type
        record.extend_from_slice(&[0x03, 0x01]); // record version
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);
        record
    }

    #[test]
    fn sni_parser_reads_a_valid_client_hello() {
        let hello = build_client_hello(Some("D-ABC123.Fleet.Example.Test"));
        assert_eq!(
            parse_client_hello_sni(&hello),
            SniPeek::Sni("d-abc123.fleet.example.test".to_string()),
            "SNI is returned lowercased"
        );
    }

    #[test]
    fn sni_parser_reports_no_sni_when_absent() {
        let hello = build_client_hello(None);
        assert_eq!(parse_client_hello_sni(&hello), SniPeek::NoSni);
    }

    #[test]
    fn sni_parser_asks_for_more_on_a_fragmented_hello() {
        let hello = build_client_hello(Some("d-frag.fleet.example.test"));
        // Every strict prefix is either NeedMore (a valid TLS prefix) — the
        // whole point of the peek loop — never a false Sni/NoSni verdict.
        for cut in 1..hello.len() {
            assert_eq!(
                parse_client_hello_sni(&hello[..cut]),
                SniPeek::NeedMore,
                "prefix of length {cut} must ask for more"
            );
        }
        assert_eq!(
            parse_client_hello_sni(&hello),
            SniPeek::Sni("d-frag.fleet.example.test".to_string())
        );
    }

    #[test]
    fn sni_parser_refuses_non_tls_garbage() {
        assert_eq!(
            parse_client_hello_sni(b"GET / HTTP/1.1\r\n"),
            SniPeek::NotTls
        );
        assert_eq!(parse_client_hello_sni(b"\x00\x14\x00\x01"), SniPeek::NotTls); // STUN-ish
        assert_eq!(parse_client_hello_sni(&[0x16, 0x99, 0x01]), SniPeek::NotTls); // bad version
        assert_eq!(parse_client_hello_sni(&[0x16]), SniPeek::NeedMore); // could be TLS
        assert_eq!(parse_client_hello_sni(&[]), SniPeek::NeedMore);
        // Not a ClientHello handshake type.
        let mut not_ch = build_client_hello(Some("x.test"));
        not_ch[5] = 0x02; // ServerHello
        assert_eq!(parse_client_hello_sni(&not_ch), SniPeek::NotTls);
    }

    #[test]
    fn sni_parser_never_panics_on_random_bytes() {
        // Fuzz-lite: no input, however malformed, may panic. A prefixed 0x16
        // exercises the deeper structural bounds checks.
        for seed in 0u16..2000 {
            let mut bytes = Vec::new();
            let mut x = (seed as u32).wrapping_mul(2654435761);
            for _ in 0..(seed % 300) {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                bytes.push((x >> 16) as u8);
            }
            let _ = parse_client_hello_sni(&bytes);
            let prefixed: Vec<u8> = std::iter::once(0x16).chain(bytes).collect();
            let _ = parse_client_hello_sni(&prefixed);
        }
    }

    #[test]
    fn sni_parser_refuses_lengths_past_the_extensions_block() {
        // Corrupt the server_name_list length to claim more than the extension
        // holds — must refuse, not panic or over-read.
        let mut hello = build_client_hello(Some("d-x.fleet.example.test"));
        let len = hello.len();
        hello[len - 3] = 0xff; // inside the SNI host length region
        assert!(matches!(
            parse_client_hello_sni(&hello),
            SniPeek::NotTls | SniPeek::NeedMore
        ));
    }

    #[test]
    fn route_label_is_the_leftmost_dns_label() {
        assert_eq!(
            sni_route_label("d-abc123.fleet.example.test").as_deref(),
            Some("d-abc123")
        );
        assert_eq!(
            sni_route_label("D-ABC123.Fleet.Test.").as_deref(),
            Some("d-abc123")
        );
        assert_eq!(sni_route_label("").as_deref(), None);
        assert_eq!(sni_route_label(".").as_deref(), None);
    }

    // ── Caps ────────────────────────────────────────────────────────────────

    fn relay_state() -> Arc<RelayState> {
        Arc::new(RelayState::new(
            "127.0.0.1:0".parse().unwrap(),
            vec!["203.0.113.10".parse().unwrap()],
        ))
    }

    #[test]
    fn per_ip_connection_cap_is_enforced_and_released() {
        let relay = relay_state();
        let ip: IpAddr = "198.51.100.7".parse().unwrap();
        let mut guards = Vec::new();
        for _ in 0..RELAY_MAX_CONNS_PER_IP {
            guards.push(relay.acquire_ip_conn(ip).expect("under cap"));
        }
        assert!(relay.acquire_ip_conn(ip).is_none(), "at cap, refuse");
        drop(guards.pop());
        assert!(
            relay.acquire_ip_conn(ip).is_some(),
            "a freed slot admits the next connection"
        );
    }

    #[test]
    fn per_tunnel_splice_cap_is_enforced_and_released() {
        let relay = relay_state();
        let label = "d-cap";
        let mut guards = Vec::new();
        for _ in 0..RELAY_MAX_CONNS_PER_TUNNEL {
            guards.push(relay.acquire_tunnel_splice(label).expect("under cap"));
        }
        assert!(
            relay.acquire_tunnel_splice(label).is_none(),
            "at cap, refuse"
        );
        drop(guards.pop());
        assert!(relay.acquire_tunnel_splice(label).is_some());
    }

    #[test]
    fn dialback_queue_only_grows_for_an_active_tunnel_and_is_bounded() {
        let relay = relay_state();
        let now = crate::now_unix_ms();
        // No tunnel yet: enqueue refuses.
        assert!(!relay.enqueue_dialback("d-none", "n0".to_string(), now));
        relay.touch_tunnel("d-live", now);
        assert!(relay.tunnel_active("d-live", now));
        for i in 0..RELAY_MAX_PENDING_PER_TUNNEL {
            assert!(relay.enqueue_dialback("d-live", format!("n{i}"), now));
        }
        assert!(
            !relay.enqueue_dialback("d-live", "overflow".to_string(), now),
            "the pending queue is capped"
        );
        // A stale tunnel is inactive and unroutable.
        assert!(!relay.tunnel_active("d-live", now + RELAY_TUNNEL_LIVENESS_MS + 1));
    }

    // ── Dial-back framing ───────────────────────────────────────────────────

    async fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr);
        let server = async { listener.accept().await.unwrap().0 };
        let (client, server) = tokio::join!(client, server);
        (client.unwrap(), server)
    }

    #[tokio::test]
    async fn dialback_hello_parses_magic_and_nonce() {
        let (mut client, mut server) = connected_pair().await;
        client
            .write_all(b"ITRLY1 the-nonce-value\nextra ciphertext")
            .await
            .unwrap();
        let nonce = read_dialback_nonce(&mut server).await;
        assert_eq!(nonce.as_deref(), Some("the-nonce-value"));
    }

    #[tokio::test]
    async fn dialback_hello_rejects_wrong_magic() {
        let (mut client, mut server) = connected_pair().await;
        client.write_all(b"NOPE abc\n").await.unwrap();
        assert_eq!(read_dialback_nonce(&mut server).await, None);
    }

    // ── Registration / auth on the control channel ──────────────────────────

    fn test_identity() -> (Ed25519KeyPair, String) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public = crate::b64u(key.public_key().as_ref());
        (key, public)
    }

    fn registered_store(daemon_id: &str, public_key: &str) -> crate::Store {
        let now = crate::now_unix_ms();
        let mut store = crate::Store::default();
        store.daemons.push(crate::DaemonRecord {
            daemon_id: daemon_id.to_string(),
            label: None,
            daemon_public_key: public_key.to_string(),
            owner_user_id: None,
            claim_code_hash: None,
            claim_code_created_unix_ms: None,
            last_registration_proof_unix_ms: None,
            route_link_revision: 0,
            last_unclaim_proof_unix_ms: None,
            registered_unix_ms: now,
            last_seen_unix_ms: now,
            updated_unix_ms: now,
            presence_hours: Vec::new(),
        });
        store
    }

    #[tokio::test]
    async fn relay_next_requires_the_relay_to_be_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let (key, public) = test_identity();
        let daemon_id = "relay-disabled";
        let state = crate::build_test_state(
            dir.path(),
            registered_store(daemon_id, &public),
            crate::TestStateOverrides {
                open_daemon_registration: true,
                ..Default::default()
            },
        );
        let issued = crate::now_unix_ms();
        let payload = relay_control_signing_payload(daemon_id, &public, issued);
        let body = RelayNextRequest {
            protocol: RELAY_CONTROL_PROTOCOL.to_string(),
            daemon_id: daemon_id.to_string(),
            daemon_public_key: public.clone(),
            issued_at_unix_ms: issued,
            signature: crate::b64u(key.sign(payload.as_bytes()).as_ref()),
            timeout_ms: Some(10),
        };
        let err = relay_next(
            axum::extract::State(state),
            HeaderMap::new(),
            axum::Json(body),
        )
        .await
        .err()
        .expect("relay disabled must reject");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn relay_next_rejects_a_bad_signature() {
        let dir = tempfile::tempdir().unwrap();
        let (_key, public) = test_identity();
        let daemon_id = "relay-badsig";
        let relay = relay_state();
        let state = crate::build_test_state(
            dir.path(),
            registered_store(daemon_id, &public),
            crate::TestStateOverrides {
                open_daemon_registration: true,
                relay: Some(relay),
                ..Default::default()
            },
        );
        let issued = crate::now_unix_ms();
        let body = RelayNextRequest {
            protocol: RELAY_CONTROL_PROTOCOL.to_string(),
            daemon_id: daemon_id.to_string(),
            daemon_public_key: public.clone(),
            issued_at_unix_ms: issued,
            signature: crate::b64u(&[0u8; 64]),
            timeout_ms: Some(10),
        };
        let err = relay_next(
            axum::extract::State(state),
            HeaderMap::new(),
            axum::Json(body),
        )
        .await
        .err()
        .expect("a bad signature must reject");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn dns_relay_publishes_the_relay_address_and_records_via_relay() {
        let dir = tempfile::tempdir().unwrap();
        let (key, public) = test_identity();
        let daemon_id = "relay-dns-daemon";
        let relay = relay_state();
        let advertise = relay.advertise_addrs()[0];
        let zone =
            Arc::new(crate::FleetZone::new("fleet.example.test", "ns.example.test").unwrap());
        let state = crate::build_test_state(
            dir.path(),
            registered_store(daemon_id, &public),
            crate::TestStateOverrides {
                open_daemon_registration: true,
                dns_zone: Some(zone.clone()),
                relay: Some(relay),
                ..Default::default()
            },
        );
        let issued = crate::now_unix_ms();
        let payload = dns_relay_signing_payload(daemon_id, &public, issued, true);
        let body = DnsRelayRequest {
            protocol: DNS_RELAY_PROTOCOL.to_string(),
            daemon_id: daemon_id.to_string(),
            daemon_public_key: public.clone(),
            issued_at_unix_ms: issued,
            signature: crate::b64u(key.sign(payload.as_bytes()).as_ref()),
            enable: true,
        };
        let response = dns_relay(
            axum::extract::State(state.clone()),
            HeaderMap::new(),
            axum::Json(body),
        )
        .await
        .expect("relay-mode publish succeeds");
        assert_eq!(response.0["via_relay"], serde_json::json!(true));

        // The store records relay mode; the zone answers the relay address.
        let store = state.store.lock().await;
        let record = store
            .dns_records
            .iter()
            .find(|r| r.daemon_id == daemon_id)
            .expect("record persisted");
        assert!(record.via_relay);
        assert_eq!(record.addresses, vec![advertise.to_string()]);
    }

    // ── In-process loopback end-to-end ──────────────────────────────────────

    #[derive(Debug)]
    struct CapturingVerifier {
        seen: StdMutex<Option<Vec<u8>>>,
        provider: Arc<rustls::crypto::CryptoProvider>,
    }

    impl CapturingVerifier {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                seen: StdMutex::new(None),
                provider: Arc::new(rustls::crypto::ring::default_provider()),
            })
        }
        fn captured(&self) -> Option<Vec<u8>> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl rustls::client::danger::ServerCertVerifier for CapturingVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            *self.seen.lock().unwrap() = Some(end_entity.as_ref().to_vec());
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// A stand-in daemon gateway: a TLS server presenting the fleet
    /// certificate. It mirrors the real gateway's fleet-SNI rule — the shell
    /// is served, a protected route is refused — so the test proves the relay
    /// hands the browser's ClientHello (fleet SNI and all) through unchanged.
    async fn spawn_fleet_gateway(fleet_name: &str) -> (SocketAddr, Vec<u8>) {
        let cert = rcgen::generate_simple_self_signed(vec![fleet_name.to_string()]).unwrap();
        let cert_der = cert.cert.der().to_vec();
        let key_der = cert.signing_key.serialize_der();
        let certs = vec![rustls::pki_types::CertificateDer::from(cert_der.clone())];
        let key = rustls::pki_types::PrivateKeyDer::try_from(key_der).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fleet_name = fleet_name.to_string();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                let fleet_name = fleet_name.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(stream).await else {
                        return;
                    };
                    let is_fleet = tls
                        .get_ref()
                        .1
                        .server_name()
                        .is_some_and(|sni| sni.eq_ignore_ascii_case(&fleet_name));
                    let mut buf = vec![0u8; 2048];
                    let n = tls.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let (status, body) = if request.starts_with("GET /api/protected") {
                        // Discovery-only: a fleet-SNI connection refuses control.
                        if is_fleet {
                            ("403 Forbidden", "discovery-only")
                        } else {
                            ("200 OK", "control")
                        }
                    } else {
                        ("200 OK", "shell")
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = tls.write_all(response.as_bytes()).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        (addr, cert_der)
    }

    /// The fake daemon's control loop + dial-back plumbing.
    fn spawn_fake_daemon(
        connect_base: String,
        relay_addr: SocketAddr,
        gateway_addr: SocketAddr,
        daemon_id: String,
        public: String,
        key: Arc<Ed25519KeyPair>,
    ) {
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            loop {
                let issued = crate::now_unix_ms();
                let payload = relay_control_signing_payload(&daemon_id, &public, issued);
                let signature = crate::b64u(key.sign(payload.as_bytes()).as_ref());
                let request = client
                    .post(format!("{connect_base}/api/relay/next"))
                    .json(&serde_json::json!({
                        "protocol": RELAY_CONTROL_PROTOCOL,
                        "daemon_id": daemon_id,
                        "daemon_public_key": public,
                        "issued_at_unix_ms": issued,
                        "signature": signature,
                        "timeout_ms": 1000,
                    }))
                    .send()
                    .await;
                let Ok(response) = request else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                if response.status().as_u16() == 204 {
                    continue;
                }
                let Ok(value) = response.json::<serde_json::Value>().await else {
                    continue;
                };
                let Some(nonce) = value
                    .pointer("/dialback/nonce")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                else {
                    continue;
                };
                tokio::spawn(async move {
                    let Ok(mut data) = TcpStream::connect(relay_addr).await else {
                        return;
                    };
                    if data
                        .write_all(format!("{DIALBACK_MAGIC} {nonce}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let Ok(gateway) = TcpStream::connect(gateway_addr).await else {
                        return;
                    };
                    splice(data, gateway, RELAY_SPLICE_MAX_BYTES, RELAY_SPLICE_IDLE).await;
                });
            }
        });
    }

    async fn browser_fetch(
        relay_addr: SocketAddr,
        fleet_name: &str,
        path: &str,
    ) -> (String, Option<Vec<u8>>) {
        let verifier = CapturingVerifier::new();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let server_name = rustls::pki_types::ServerName::try_from(fleet_name.to_string()).unwrap();
        let tcp = TcpStream::connect(relay_addr).await.unwrap();
        let mut tls = connector.connect(server_name, tcp).await.unwrap();
        let request =
            format!("GET {path} HTTP/1.1\r\nhost: {fleet_name}\r\nconnection: close\r\n\r\n");
        tls.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let _ = tls.read_to_end(&mut response).await;
        (
            String::from_utf8_lossy(&response).to_string(),
            verifier.captured(),
        )
    }

    #[tokio::test]
    async fn browser_reaches_daemon_fleet_cert_through_the_relay() {
        // Registered daemon identity.
        let (key, public) = test_identity();
        let key = Arc::new(key);
        let daemon_id = "relay-e2e-daemon";
        let label = daemon_label(daemon_id).unwrap();
        let fleet_name = format!("{label}.fleet.example.test");

        // Fake daemon gateway serving the fleet certificate.
        let (gateway_addr, daemon_cert_der) = spawn_fleet_gateway(&fleet_name).await;

        // Connect service (control channel) + relay listener sharing state.
        let dir = tempfile::tempdir().unwrap();
        let relay = relay_state();
        let state = crate::build_test_state(
            dir.path(),
            registered_store(daemon_id, &public),
            crate::TestStateOverrides {
                open_daemon_registration: true,
                relay: Some(relay.clone()),
                ..Default::default()
            },
        );
        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_addr = http_listener.local_addr().unwrap();
        {
            let router = crate::connect_router(state.clone());
            tokio::spawn(async move {
                axum::serve(http_listener, router).await.unwrap();
            });
        }
        let relay_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay_listener.local_addr().unwrap();
        tokio::spawn(run_relay_accept_loop(state.clone(), relay, relay_listener));

        // Fake daemon: control loop + dial-back into its own gateway.
        spawn_fake_daemon(
            format!("http://{http_addr}"),
            relay_addr,
            gateway_addr,
            daemon_id.to_string(),
            public.clone(),
            key,
        );

        // Wait for the daemon's first control poll to register its tunnel
        // before dialling in — otherwise the browser races ahead of the tunnel
        // and is (correctly) refused.
        let active = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state
                    .relay
                    .as_ref()
                    .unwrap()
                    .tunnel_active(&label, crate::now_unix_ms())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(active.is_ok(), "daemon tunnel never became active");

        // The browser reaches the fleet name through the relay and the shell
        // is served — over the DAEMON's certificate.
        let (shell, seen_cert) = tokio::time::timeout(
            Duration::from_secs(10),
            browser_fetch(relay_addr, &fleet_name, "/"),
        )
        .await
        .expect("browser round-trip completes");
        assert!(shell.contains("200 OK"), "shell served: {shell}");
        assert!(shell.contains("shell"));
        assert_eq!(
            seen_cert.as_deref(),
            Some(daemon_cert_der.as_slice()),
            "the certificate the browser saw is the daemon's fleet certificate"
        );

        // Discovery-only preserved: a protected route over the fleet name still
        // refuses. The relay changed nothing about how the daemon classifies
        // the connection.
        let (protected, _) = tokio::time::timeout(
            Duration::from_secs(10),
            browser_fetch(relay_addr, &fleet_name, "/api/protected"),
        )
        .await
        .expect("browser round-trip completes");
        assert!(
            protected.contains("403 Forbidden"),
            "protected route refuses over the fleet name: {protected}"
        );

        // An SNI with no active tunnel is refused: the relay closes the
        // connection, so the TLS handshake never completes and no certificate
        // is ever seen. Proves the relay does not blindly splice.
        let stray = daemon_label("some-other-daemon").unwrap();
        let stray_name = format!("{stray}.fleet.example.test");
        let verifier = CapturingVerifier::new();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let server_name = rustls::pki_types::ServerName::try_from(stray_name).unwrap();
        let tcp = TcpStream::connect(relay_addr).await.unwrap();
        let handshake =
            tokio::time::timeout(Duration::from_secs(3), connector.connect(server_name, tcp)).await;
        assert!(
            matches!(handshake, Ok(Err(_)) | Err(_)),
            "an unknown fleet name never completes a TLS handshake"
        );
        assert!(
            verifier.captured().is_none(),
            "an unknown fleet name never yields a certificate"
        );
    }
}

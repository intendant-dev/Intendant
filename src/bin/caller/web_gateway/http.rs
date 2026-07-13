//! Generic HTTP mechanics for the gateway: the demuxed-stream surface
//! (AsyncReadWrite / DemuxStream) and its finalize, request-target and
//! header/query parsing, JSON/HTML response builders with their CORS
//! wrappers, conditional-request and gzip helpers, and the capped body
//! readers shared by every handler family.

use super::*;

/// Unified read/write surface for a demuxed dashboard connection.
///
/// After the per-connection demux peels off raw ICE-TCP (which stays a
/// concrete `TcpStream`), the surviving connection is either a plain
/// `TcpStream` or a TLS-wrapped `tokio_rustls::server::TlsStream<TcpStream>`.
/// Both implement `AsyncRead + AsyncWrite + Unpin + Send`, so the WebSocket
/// and HTTP handling that follows operates through this boxed trait object
/// — identical code path for HTTP and HTTPS, plain WS and WSS. The
/// `tokio_tungstenite::accept_async` upgrade and every `read_exact` /
/// `write_all` call are already generic over these trait bounds; only the
/// three small body-reading helpers had to be made generic to match.
pub(crate) trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

/// Demuxed connection (plain TCP or TLS) used for all post-demux
/// HTTP/WebSocket handling, plus the per-exchange keep-alive state the
/// request loop and the write edges share (see the `keep_alive` module
/// docs for the decision table).
///
/// Reads serve the `replay` buffer first — the request loop pushes each
/// follow-up request's already-captured segment there so dispatch (and a
/// WebSocket upgrade arriving on a kept-alive connection) see the request
/// from byte zero, exactly as the first request is delivered (kernel
/// buffer on the plain path, `PrefixedStream` on TLS). Writes delegate
/// straight to the inner stream.
pub(crate) struct DemuxStream {
    inner: std::pin::Pin<Box<dyn AsyncReadWrite>>,
    replay: Vec<u8>,
    replay_pos: usize,
    /// Request leg of the keep-alive verdict (client headers + loop
    /// budget + segment framing); set by the request loop per request.
    client_allows_reuse: bool,
    /// Body leg: the request body was provably consumed in full; set by
    /// dispatch, reset by [`Self::begin_request`]. Fail-closed default.
    request_consumed: bool,
    /// Give-back channel to the request loop: a write edge that finished
    /// a self-framing response on a reusable exchange parks the stream
    /// here instead of closing it. `Weak` — only the connection task
    /// holds the slot strongly, so unarmed streams (handler test
    /// fixtures, one-shot tools) fail closed to the historical
    /// write-then-finalize behavior.
    parked: std::sync::Weak<Mutex<Option<DemuxStream>>>,
}

/// The request loop's strong handle to the keep-alive give-back slot.
pub(crate) type ParkedStreamSlot = Arc<Mutex<Option<DemuxStream>>>;

impl DemuxStream {
    /// Wrap a demuxed transport. Keep-alive starts disarmed: every write
    /// edge behaves exactly like the historical close-per-request server
    /// until the request loop arms the slot and begins a request.
    pub(crate) fn new(inner: std::pin::Pin<Box<dyn AsyncReadWrite>>) -> Self {
        Self {
            inner,
            replay: Vec::new(),
            replay_pos: 0,
            client_allows_reuse: false,
            request_consumed: false,
            parked: std::sync::Weak::new(),
        }
    }

    pub(crate) fn new_parked_slot() -> ParkedStreamSlot {
        Arc::new(Mutex::new(None))
    }

    pub(crate) fn arm_keep_alive(&mut self, slot: &ParkedStreamSlot) {
        self.parked = Arc::downgrade(slot);
    }

    /// Begin one request/response exchange: record the request leg's
    /// verdict and reset the body leg (fail-closed until dispatch proves
    /// the body was consumed).
    pub(crate) fn begin_request(&mut self, client_allows_reuse: bool) {
        self.client_allows_reuse = client_allows_reuse;
        self.request_consumed = false;
    }

    /// Body leg: dispatch proved this request's body is fully consumed
    /// (or that there was none to consume).
    pub(crate) fn mark_request_body_consumed(&mut self) {
        self.request_consumed = true;
    }

    /// The verdict a write edge consults before opting in: park (and
    /// emit `Connection: keep-alive`) only when the request leg, the
    /// body leg, and a live loop slot all agree. Anything else — and
    /// every write edge that never asks — closes.
    pub(crate) fn exchange_reusable(&self) -> bool {
        self.client_allows_reuse && self.request_consumed && self.parked.strong_count() > 0
    }

    /// Serve `segment` to readers before the socket (the request loop's
    /// captured next-request head + any body prefix read with it).
    pub(crate) fn push_replay(&mut self, segment: &[u8]) {
        // The previous request drained its replay in full (dispatch
        // consumes exactly the segment it was handed), so this replaces
        // rather than appends.
        debug_assert!(self.replay_pos >= self.replay.len());
        self.replay = segment.to_vec();
        self.replay_pos = 0;
    }

    /// Response-leg opt-in: flush this exchange's response through to
    /// the socket — rustls buffers ciphertext, so this is the flush half
    /// of the [`finalize_http_stream`] contract applied per response —
    /// then hand the stream back to the request loop for the next
    /// request. Falls back to a clean close when the flush fails or the
    /// loop is gone (unarmed slot / dead task).
    pub(crate) async fn park(mut self) {
        use tokio::io::AsyncWriteExt;
        if self.flush().await.is_err() {
            let _ = self.shutdown().await;
            return;
        }
        let Some(slot) = self.parked.upgrade() else {
            let _ = self.shutdown().await;
            return;
        };
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(self);
    }
}

impl AsyncRead for DemuxStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.replay_pos < this.replay.len() {
            let remaining = &this.replay[this.replay_pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.replay_pos += n;
            // Serve only replay bytes on this poll (same shape as
            // web_tls::PrefixedStream); the next read drains the inner
            // stream.
            return std::task::Poll::Ready(Ok(()));
        }
        this.inner.as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for DemuxStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.get_mut().inner.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.get_mut().inner.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.get_mut().inner.as_mut().poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.get_mut().inner.as_mut().poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Finalize an HTTP connection before its stream drops: flush, then shut
/// down cleanly.
///
/// The flush matters because the TLS path's stream is a
/// `tokio_rustls::server::TlsStream`, which buffers *ciphertext* inside
/// the rustls session: `write_all` only guarantees the plaintext was
/// accepted into that buffer, not that the encrypted records reached the
/// socket. Dropping the `TlsStream` without flushing discards the
/// unwritten tail records, truncating large bodies (e.g. the ~871 KB
/// `app.html` arrived ~19.5 KB short over HTTPS).
///
/// Calling `flush` drives rustls to emit all buffered ciphertext to the
/// TCP socket; `shutdown` then writes the TLS `close_notify` and the TCP
/// FIN, closing the session cleanly. On the plain path both delegate
/// straight through to the `TcpStream` (flush is a no-op, shutdown sends
/// the FIN we'd send on drop anyway).
///
/// With HTTP/1.1 keep-alive this runs at CONNECTION end — after the last
/// exchange of a kept-alive connection, or immediately after a
/// `Connection: close` exchange (the historical one-shot shape). Between
/// kept-alive exchanges the flush half runs per response inside
/// [`DemuxStream::park`], so a parked response always reaches the client
/// before the loop waits for the next request.
pub(crate) async fn finalize_http_stream(stream: &mut DemuxStream) {
    use tokio::io::AsyncWriteExt;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// Gzip-compress `data` (pure-Rust miniz_oxide backend via flate2).
///
/// `Compression::best()`: every caller compresses once and caches the
/// result (embedded static assets once per process, app.html once per
/// gateway spawn — the `INTENDANT_APP_HTML_PATH` dev override serves
/// uncompressed), so the one-time CPU cost buys smaller transfers on
/// every subsequent request.
pub(crate) fn gzip_compress(data: &[u8]) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::with_capacity(data.len() / 2), Compression::best());
    // Writing to a Vec cannot fail.
    let _ = encoder.write_all(data);
    encoder.finish().unwrap_or_default()
}

/// Below this size gzip isn't worth the header bytes + CPU — the response
/// fits in a packet or two either way (e.g. audio-processor.js, ~1.4 KB).
pub(crate) const GZIP_MIN_BYTES: usize = 4096;

/// Split an HTTP request line (`GET /path?query HTTP/1.1`) into
/// `(method, path, query)`. The query string is returned without the `?`;
/// missing pieces come back as empty strings.
pub(crate) fn parse_request_target(request_line: &str) -> (&str, &str, &str) {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    (method, path, query)
}

/// True when `path` is exactly `base` or nested beneath it (`base` + `/`).
/// The parsed-path twin of the retired `request_line.contains(...)` routing:
/// a query string that merely mentions `base`, or a longer path that shares
/// its prefix (`/api/peersonal`), no longer matches — only the route itself
/// and its sub-routes do. `path` must come from `parse_request_target`, so
/// the query is already stripped.
pub(crate) fn path_is_or_under(path: &str, base: &str) -> bool {
    path == base
        || path
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub(crate) fn http_header_value<'a>(header_text: &'a str, header_name: &str) -> Option<&'a str> {
    header_text.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case(header_name) {
            Some(value.trim())
        } else {
            None
        }
    })
}

pub(crate) fn http_header_present(header_text: &str, header_name: &str) -> bool {
    http_header_value(header_text, header_name).is_some()
}

/// Whether the request head's `Accept-Encoding` admits gzip (tolerant:
/// case-insensitive header name and token, `x-gzip` alias, `;q=0` rejects).
pub(crate) fn accept_encoding_allows_gzip(header_text: &str) -> bool {
    for line in header_text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("accept-encoding") {
            continue;
        }
        for part in value.split(',') {
            let mut sections = part.split(';');
            let token = sections.next().unwrap_or("").trim();
            if !token.eq_ignore_ascii_case("gzip") && !token.eq_ignore_ascii_case("x-gzip") {
                continue;
            }
            let q_zero = sections.any(|param| {
                let mut kv = param.splitn(2, '=');
                let key = kv.next().unwrap_or("").trim();
                let value = kv.next().unwrap_or("").trim();
                key.eq_ignore_ascii_case("q")
                    && value.parse::<f32>().map(|q| q <= 0.0).unwrap_or(false)
            });
            if !q_zero {
                return true;
            }
        }
    }
    false
}

/// Whether the request head carries an `If-None-Match` matching
/// `etag_token` (the bare hash, without quotes). Tolerant of `W/` weak
/// prefixes, quoted/unquoted tokens, comma-separated lists, and `*`.
pub(crate) fn if_none_match_matches(header_text: &str, etag_token: &str) -> bool {
    for line in header_text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("if-none-match") {
            continue;
        }
        for candidate in value.split(',') {
            let candidate = candidate.trim();
            if candidate == "*" {
                return true;
            }
            let candidate = candidate
                .strip_prefix("W/")
                .or_else(|| candidate.strip_prefix("w/"))
                .unwrap_or(candidate)
                .trim()
                .trim_matches('"');
            if candidate == etag_token {
                return true;
            }
        }
    }
    false
}

/// Parse a raw HTTP request blob for the `Host:` header and return its
/// hostname portion as an `IpAddr` if it's a literal IP (v4 or v6).
///
/// We need the address the browser is using to reach us — and the Host
/// header is the one piece of the HTTP handshake that actually contains
/// that. Loopback and unspecified addresses are rejected because they
/// don't survive Firefox's remote-candidate filter and wouldn't pair
/// anyway. Hostnames (like `localhost` or `dashboard.internal`) return
/// `None` — there's no ICE-TCP candidate we can usefully emit for those.
pub(crate) fn extract_host_header_ip(headers: &str) -> Option<std::net::IpAddr> {
    for line in headers.lines() {
        // Look for the Host: header line, case-insensitive. `strip_prefix`
        // returning None means "this isn't the Host line" — we must
        // continue the loop, not propagate with `?`.
        let Some(rest) = line
            .strip_prefix("Host: ")
            .or_else(|| line.strip_prefix("host: "))
            .or_else(|| line.strip_prefix("HOST: "))
        else {
            continue;
        };
        // `rest` is `host[:port]` where host can be:
        //   - IPv4 literal: 192.0.2.1
        //   - Bracketed IPv6 literal: [2001:db8::1]
        //   - Hostname: example.com
        let host_part = if let Some(inner) = rest.strip_prefix('[') {
            // IPv6 literal in brackets; chop at the closing bracket.
            inner.split(']').next()?
        } else if let Some(colon) = rest.find(':') {
            &rest[..colon]
        } else {
            rest
        };
        let trimmed = host_part.trim();
        let ip = trimmed.parse::<std::net::IpAddr>().ok()?;
        if ip.is_loopback() || ip.is_unspecified() {
            return None;
        }
        return Some(ip);
    }
    None
}

pub(crate) fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod host_header_tests {
    use super::extract_host_header_ip;
    use std::net::IpAddr;

    #[test]
    fn ipv4_with_port() {
        let headers = "GET / HTTP/1.1\r\nHost: 192.168.1.10:8765\r\n\r\n";
        assert_eq!(
            extract_host_header_ip(headers),
            Some("192.168.1.10".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn ipv6_bracketed() {
        let headers = "GET / HTTP/1.1\r\nHost: [2001:db8::1]:8765\r\n\r\n";
        assert_eq!(
            extract_host_header_ip(headers),
            Some("2001:db8::1".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn hostname_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: dashboard.internal:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn localhost_literal_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: localhost:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn loopback_ipv4_literal_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: 127.0.0.1:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn loopback_ipv6_literal_returns_none() {
        let headers = "GET / HTTP/1.1\r\nHost: [::1]:8765\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn no_host_header() {
        let headers = "GET / HTTP/1.1\r\n\r\n";
        assert_eq!(extract_host_header_ip(headers), None);
    }

    #[test]
    fn case_insensitive_header_name() {
        let headers = "GET / HTTP/1.1\r\nhost: 10.0.0.5:8765\r\n\r\n";
        assert_eq!(
            extract_host_header_ip(headers),
            Some("10.0.0.5".parse::<IpAddr>().unwrap())
        );
    }
}

/// Stream the body of an HTTP request into a fresh tempfile, honouring
/// `Content-Length` and bailing out early if the body exceeds `max_bytes`.
///
/// Returns the [`SpooledBody`] handle on success — the HTTP side of the
/// Streaming lane (transport-unification S8): the tunnel's upload-frame
/// spool ends in the same handle, and the shared neutral fns commit it
/// into the store via [`crate::upload_store::commit_upload`], which
/// atomically renames it into place.
///
/// This is the binary counterpart to `read_request_body_capped` — same peek-then-
/// stream pattern, but sinks to disk instead of a UTF-8 `String`.
pub(crate) async fn stream_body_to_tempfile<S: AsyncRead + Unpin>(
    header_text: &str,
    initial_request_bytes: &[u8],
    stream: &mut S,
    max_bytes: usize,
) -> Result<SpooledBody, String> {
    use std::io::Write;
    use tokio::io::AsyncReadExt;

    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| "missing or invalid Content-Length".to_string())?;
    if content_length == 0 {
        return Err("empty body".to_string());
    }
    if content_length > max_bytes {
        return Err(format!(
            "body too large: {} bytes (cap is {})",
            content_length, max_bytes
        ));
    }

    let peeked_body = initial_body_bytes(initial_request_bytes)?;
    let mut tmp = tempfile::NamedTempFile::new().map_err(|e| format!("create tempfile: {e}"))?;

    // Write whatever body bytes we already have from the peek. These come
    // back through the same header_text split, so they're the leading
    // content_length bytes — truncate defensively in case the peek read
    // slightly more than the body.
    let peeked_n = peeked_body.len().min(content_length);
    tmp.write_all(&peeked_body[..peeked_n])
        .map_err(|e| format!("write tempfile: {e}"))?;
    let mut written = peeked_n;

    // Pull the rest from the socket in 64 KB chunks. The cap bails early;
    // the final total is asserted to equal Content-Length so we don't store
    // a truncated file.
    let mut buf = vec![0u8; 64 * 1024];
    while written < content_length {
        let want = (content_length - written).min(buf.len());
        match stream.read(&mut buf[..want]).await {
            Ok(0) => {
                return Err(format!(
                    "connection closed mid-upload at {} / {} bytes",
                    written, content_length
                ));
            }
            Ok(n) => {
                tmp.as_file_mut()
                    .write_all(&buf[..n])
                    .map_err(|e| format!("write tempfile: {e}"))?;
                written += n;
            }
            Err(e) => return Err(format!("socket read: {e}")),
        }
    }
    tmp.as_file_mut()
        .flush()
        .map_err(|e| format!("flush tempfile: {e}"))?;
    Ok(SpooledBody { tmp, len: written })
}

pub(crate) fn initial_body_bytes(initial_request_bytes: &[u8]) -> Result<&[u8], String> {
    initial_request_bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|idx| &initial_request_bytes[idx + 4..])
        .ok_or_else(|| "incomplete HTTP headers".to_string())
}

/// One buffered HTTP/1.1 response under construction: status line, ordered
/// headers, full body. This is the single place the gateway's `\r\n`
/// framing is emitted — the string helpers below and ported hand-rolled
/// `format!("HTTP/1.1 …")` sites all serialize through it. Streaming
/// responses (session NDJSON, MCP notification drains, recording segments)
/// keep writing by hand; `BodyPolicy`-driven request-body reads land with
/// the dispatch-side consumption step.
pub(crate) struct HttpResponse {
    status: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    /// Start from a status line ("200 OK", "404 Not Found", …). Headers
    /// are emitted in insertion order; nothing is implicit.
    pub(crate) fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Status + Content-Type + Content-Length + body; every further
    /// header is appended in call order, so ported hand-rolled sites keep
    /// their historical header layout.
    pub(crate) fn with_content(
        status: impl Into<String>,
        content_type: impl Into<String>,
        body: impl Into<Vec<u8>>,
    ) -> Self {
        let body = body.into();
        Self::new(status)
            .header("Content-Type", content_type.into())
            .header("Content-Length", body.len().to_string())
            .with_body(body)
    }

    /// The canonical JSON shape: Content-Type, Content-Length,
    /// `Cache-Control: no-cache`, `Connection: close` — byte-identical to
    /// the historical `json_response` framing.
    pub(crate) fn json(status: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self::with_content(status, "application/json", body)
            .header("Cache-Control", "no-cache")
            .header("Connection", "close")
    }

    /// The canonical HTML shape — byte-identical to the historical
    /// `html_response` framing (which bakes a wildcard CORS header in).
    pub(crate) fn html(status: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self::with_content(status, "text/html; charset=utf-8", body)
            .header("Cache-Control", "no-cache")
            .header("Access-Control-Allow-Origin", "*")
            .header("Connection", "close")
    }

    pub(crate) fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Append a pre-rendered `Name: value\r\n[…]` segment — the bridge for
    /// helpers that still build header strings (the MCP CORS segment, the
    /// preflight postures). Empty input is a no-op.
    pub(crate) fn header_segment(mut self, segment: &str) -> Self {
        for line in segment.split("\r\n").filter(|l| !l.is_empty()) {
            if let Some((name, value)) = line.split_once(": ") {
                self.headers.push((name.to_string(), value.to_string()));
            }
        }
        self
    }

    fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// Append the wildcard CORS header — same position (last) as the
    /// historical `with_public_cors` post-processor.
    pub(crate) fn public_cors(self) -> Self {
        self.header("Access-Control-Allow-Origin", "*")
    }

    /// Set this response's connection tail from the exchange's keep-alive
    /// verdict. A close verdict keeps the historical bytes exactly: an
    /// existing `Connection` header (every legacy shape bakes `close`) is
    /// left untouched, and `Connection: close` is appended only when
    /// absent. A keep-alive verdict replaces any baked tail with
    /// `Connection: keep-alive` + `Keep-Alive: timeout=N` — emitted only
    /// by write edges that will actually park the connection (see the
    /// `keep_alive` module docs).
    pub(crate) fn connection_reuse(mut self, keep_alive: bool) -> Self {
        if keep_alive {
            self.headers.retain(|(name, _)| {
                !name.eq_ignore_ascii_case("connection") && !name.eq_ignore_ascii_case("keep-alive")
            });
            self.header("Connection", "keep-alive")
                .header("Keep-Alive", keep_alive_header_value())
        } else if self
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("connection"))
        {
            self
        } else {
            self.header("Connection", "close")
        }
    }

    /// Fleet-allowlist CORS posture: strip any wildcard, echo the origin
    /// only when it passed the allowlist, and mark `Vary: Origin`. The
    /// one fleet renderer (its string-form ancestor retired with the S6
    /// access-family conversion).
    pub(crate) fn fleet_cors(mut self, allowed_origin: Option<&str>) -> Self {
        self.headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("access-control-allow-origin"));
        if let Some(origin) = allowed_origin {
            self = self.header("Access-Control-Allow-Origin", origin.to_string());
        }
        self.header("Vary", "Origin")
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {}\r\n", self.status);
        for (name, value) in &self.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }

    pub(crate) fn into_string(self) -> String {
        // The gateway's buffered responses are all valid UTF-8 today; the
        // lossy conversion is a guard rail, not an expected path.
        String::from_utf8_lossy(&self.into_bytes()).into_owned()
    }
}

pub(crate) fn json_response(status: &str, body: String) -> String {
    HttpResponse::json(status, body).into_string()
}

pub(crate) fn json_ok(value: serde_json::Value) -> String {
    json_response("200 OK", value.to_string())
}

pub(crate) fn json_error(status: &str, message: impl AsRef<str>) -> String {
    json_response(
        status,
        serde_json::json!({ "error": message.as_ref() }).to_string(),
    )
}

pub(crate) fn html_response(status: &str, body: String) -> String {
    HttpResponse::html(status, body).into_string()
}

pub(crate) fn request_query_param(request_line: &str, key: &str) -> Option<String> {
    let path = request_line.split_whitespace().nth(1)?;
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key && !v.trim().is_empty() {
            return Some(percent_decode_query_value(v));
        }
    }
    None
}

/// Parse a query-string value by key out of a full `request_line`
/// (e.g. `POST /api/session/current/uploads?name=foo.pdf&destination=task HTTP/1.1`).
/// Returns the URL-decoded value, or `None` if the key isn't present.
pub(crate) fn query_param(request_line: &str, key: &str) -> Option<String> {
    let path_and_q = request_line.split_whitespace().nth(1)?;
    let query = path_and_q.split_once('?')?.1;
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        let v = it.next().unwrap_or("");
        if k == key {
            return Some(url_decode(v));
        }
    }
    None
}

/// Minimal `application/x-www-form-urlencoded` decoder: `%HH` → byte,
/// `+` → space. Good enough for filenames/destinations on the upload
/// path; we don't invite the full urlencoding crate just for this.
pub(crate) fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h = &bytes[i + 1..i + 3];
                match std::str::from_utf8(h)
                    .ok()
                    .and_then(|hs| u8::from_str_radix(hs, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn dashboard_http_header_value<'a>(header_text: &'a str, name: &str) -> Option<&'a str> {
    header_text.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case(name) {
            Some(value.trim())
        } else {
            None
        }
    })
}

/// Extract the `Content-Type` request header value, or a generic default.
pub(crate) fn content_type_header(header_text: &str) -> String {
    header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-type:"))
        .and_then(|l| l.split(':').nth(1))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

pub(crate) fn percent_decode_query_value(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub(crate) fn extract_origin_header(header_text: &str) -> Option<String> {
    header_text
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("origin:"))
        .map(|line| line[line.find(':').unwrap_or(6) + 1..].trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn extract_host_header(header_text: &str) -> Option<String> {
    header_text
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("host:"))
        .map(|line| line[line.find(':').unwrap_or(4) + 1..].trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Decide whether a cross-origin caller may use the fleet Access APIs.
/// Allowed origins are: this daemon itself (same-origin requests also send
/// an Origin header on POST), the macOS app bundle's custom scheme, this
/// daemon's outbound peer routes, and its approved inbound peer identities.
/// Everything else — including `Origin: null` — is refused. Authentication
/// is still mTLS/IAM; this gate only decides which *pages* may drive it.
/// True for the request's own origin (Origin matches the Host header under
/// the connection's scheme) and for the macOS app bundle's custom scheme —
/// web content can never carry a custom-scheme origin, and the app's native
/// proxy is not subject to CORS anyway.
pub(crate) fn is_own_or_app_origin(origin: &str, is_tls: bool, header_text: &str) -> bool {
    let origin = origin.trim();
    if origin.eq_ignore_ascii_case("null") || origin.is_empty() {
        return false;
    }
    if origin.to_ascii_lowercase().starts_with("intendant://") {
        return true;
    }
    let Some(normalized) = normalized_origin(origin) else {
        return false;
    };
    if let Some(host) = extract_host_header(header_text) {
        let scheme = if is_tls { "https" } else { "http" };
        if normalized_origin(&format!("{scheme}://{host}")).as_deref() == Some(&normalized) {
            return true;
        }
    }
    false
}

/// The bootstrap surfaces that are *designed* for foreign-origin browsers:
/// local Connect signaling (a page from a rendezvous origin negotiates a
/// tunnel whose real authentication is the daemon-signed binding plus IAM)
/// and the public peer-access doorbell. Their responses stay
/// wildcard-readable; everything else is same-origin or fleet-echoed.
pub(crate) fn with_public_cors(response: String) -> String {
    let Some(split) = response.find("\r\n\r\n") else {
        return response;
    };
    let (head, rest) = response.split_at(split);
    format!("{head}\r\nAccess-Control-Allow-Origin: *{rest}")
}

/// Body cap for the public /connect/dashboard signaling arms (SDP offers
/// and ICE batches stay far below this; the lane is reachable before any
/// authentication, so it must never buffer unbounded input).
pub(crate) const CONNECT_SIGNALING_BODY_CAP_BYTES: usize = 256 * 1024;

pub(crate) async fn read_request_body_capped<S: AsyncRead + Unpin>(
    stream: &mut S,
    header_text: &str,
    max_bytes: usize,
) -> Result<String, (u16, String)> {
    use tokio::io::AsyncReadExt;
    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if content_length > max_bytes {
        return Err((
            413,
            serde_json::json!({"error": "request body too large"}).to_string(),
        ));
    }
    if content_length == 0 {
        return Ok(String::new());
    }
    let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
    if peeked_body.len() >= content_length {
        return Ok(crate::types::truncate_str(peeked_body, content_length).to_string());
    }
    let remaining = content_length.saturating_sub(peeked_body.len());
    let mut full = peeked_body.to_string();
    let mut rest = vec![0u8; remaining];
    if stream.read_exact(&mut rest).await.is_ok() {
        full.push_str(&String::from_utf8_lossy(&rest));
    }
    Ok(full)
}

/// Numeric code of a status line (`"404 Not Found"` → 404); unparseable
/// input collapses to 500. Inverse of [`status_reason`] for the (status,
/// body) helper cores predating [`crate::web_gateway::ApiResponse`].
pub(crate) fn status_line_code(status_line: &str) -> u16 {
    status_line
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(500)
}

pub(crate) fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "200 OK",
        201 => "201 Created",
        206 => "206 Partial Content",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        403 => "403 Forbidden",
        404 => "404 Not Found",
        405 => "405 Method Not Allowed",
        409 => "409 Conflict",
        413 => "413 Payload Too Large",
        416 => "416 Range Not Satisfiable",
        429 => "429 Too Many Requests",
        500 => "500 Internal Server Error",
        // The peers family's relay-failure class (peer_error_response,
        // coordinator delegation): NotConnected/Transport/Auth/Rejected
        // answer 502 through the shared renderer since the S7
        // conversion.
        502 => "502 Bad Gateway",
        503 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    }
}

/// Extract a token from the `?token=...` query parameter of an HTTP
/// request line. Used by the WebSocket upgrade auth path because the
/// browser cannot set arbitrary headers on `WebSocket` opens — the
/// dashboard appends `?token=...` to the /ws URL instead.
///
/// `request_line` is the first line of the HTTP request, e.g.
/// `"GET /ws?token=abc HTTP/1.1"`. Returns the extracted token if
/// present, `None` if there's no `?token=` parameter.
pub(crate) fn extract_token_query_param(request_line: &str) -> Option<String> {
    // No URL-decoding: bearer tokens are typically URL-safe
    // (hex / base64-url). If a token contains characters that
    // require encoding, the operator can either pick a
    // different token or send via Authorization header (which
    // doesn't have the URL-encoding constraint).
    extract_query_param(request_line, "token")
}

/// Extract a named query parameter from an HTTP request line (same
/// no-URL-decoding contract as the token extractor above — callers pass
/// URL-safe values).
pub(crate) fn extract_query_param(request_line: &str, name: &str) -> Option<String> {
    let path_and_query = request_line.split_whitespace().nth(1)?;
    let query = path_and_query.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(value) = pair
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(value.to_string());
        }
    }
    None
}

/// Case-insensitive lookup of one header's value in a raw HTTP header
/// block (request line + `Name: value` lines).
pub(crate) fn extract_header_value(header_text: &str, name: &str) -> Option<String> {
    for line in header_text.lines().skip(1) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            let value = value.trim();
            return (!value.is_empty()).then(|| value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_response_builder_reproduces_legacy_framing_byte_for_byte() {
        // The five string helpers now serialize through HttpResponse; pin
        // the exact historical bytes so the rebase stays byte-identical.
        assert_eq!(
            json_response("200 OK", "{\"ok\":true}".to_string()),
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 11\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             \r\n\
             {\"ok\":true}"
        );
        assert_eq!(
            html_response("404 Not Found", "<h1>gone</h1>".to_string()),
            "HTTP/1.1 404 Not Found\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             Content-Length: 13\r\n\
             Cache-Control: no-cache\r\n\
             Access-Control-Allow-Origin: *\r\n\
             Connection: close\r\n\
             \r\n\
             <h1>gone</h1>"
        );
        // The builder's CORS postures match the historical
        // post-processor bytes (the string-form fleet helper is gone —
        // the builder is the one fleet renderer, so its shapes are
        // pinned literally).
        assert_eq!(
            HttpResponse::json("200 OK", "{}")
                .public_cors()
                .into_string(),
            with_public_cors(json_response("200 OK", "{}".to_string()))
        );
        assert_eq!(
            HttpResponse::json("200 OK", "{}")
                .fleet_cors(Some("https://fleet.example"))
                .into_string(),
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 2\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             Access-Control-Allow-Origin: https://fleet.example\r\n\
             Vary: Origin\r\n\
             \r\n\
             {}"
        );
        assert_eq!(
            HttpResponse::json("200 OK", "{}")
                .fleet_cors(None)
                .into_string(),
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 2\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             Vary: Origin\r\n\
             \r\n\
             {}"
        );
    }

    #[test]
    fn connection_reuse_close_is_byte_identical_to_legacy_shapes() {
        // Close verdict on a shape that bakes `Connection: close`
        // (every legacy helper): a strict no-op.
        assert_eq!(
            HttpResponse::json("200 OK", "{}")
                .connection_reuse(false)
                .into_string(),
            json_response("200 OK", "{}".to_string())
        );
        // Close verdict on a shape with no Connection header: one is
        // appended so the client knows not to reuse.
        let bare = HttpResponse::with_content("200 OK", "text/plain", "x")
            .connection_reuse(false)
            .into_string();
        assert!(bare.contains("Connection: close\r\n"), "{bare}");
    }

    #[test]
    fn connection_reuse_keep_alive_replaces_baked_close() {
        let text = HttpResponse::json("200 OK", "{}")
            .connection_reuse(true)
            .into_string();
        assert!(!text.contains("Connection: close"), "{text}");
        assert!(text.contains("Connection: keep-alive\r\n"), "{text}");
        assert!(
            text.contains(&format!("Keep-Alive: timeout={KEEP_ALIVE_IDLE_SECS}\r\n")),
            "{text}"
        );
        assert_eq!(text.matches("Connection").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn demux_stream_defaults_fail_closed_and_replay_serves_first() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, server) = tokio::io::duplex(1024);
        let mut stream = DemuxStream::new(Box::pin(server));
        // Fail-closed defaults: no verdict, no armed slot.
        assert!(!stream.exchange_reusable());
        stream.begin_request(true);
        stream.mark_request_body_consumed();
        // Still not reusable: the parked slot was never armed (the
        // fixture / one-shot shape), so a park would just close.
        assert!(!stream.exchange_reusable());
        let slot = DemuxStream::new_parked_slot();
        stream.arm_keep_alive(&slot);
        assert!(stream.exchange_reusable());
        // begin_request resets the body leg (fail-closed per request).
        stream.begin_request(true);
        assert!(!stream.exchange_reusable());

        // Replay bytes are served before the transport's own bytes.
        client.write_all(b"tail").await.unwrap();
        stream.push_replay(b"head ");
        let mut got = [0u8; 9];
        stream.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"head tail");
    }

    #[tokio::test]
    async fn demux_stream_park_hands_the_stream_back_through_the_slot() {
        let (_client, server) = tokio::io::duplex(64);
        let mut stream = DemuxStream::new(Box::pin(server));
        let slot = DemuxStream::new_parked_slot();
        stream.arm_keep_alive(&slot);
        stream.begin_request(true);
        stream.mark_request_body_consumed();
        stream.park().await;
        let parked = slot
            .lock()
            .unwrap()
            .take()
            .expect("park() must hand the stream back through the slot");
        drop(parked);
    }

    #[tokio::test]
    async fn read_request_body_capped_truncates_peeked_body_on_char_boundary() {
        let mut stream = tokio::io::empty();
        let header_text = "POST /api/peers HTTP/1.1\r\nContent-Length: 1\r\n\r\n\u{00e9}";
        let body = read_request_body_capped(&mut stream, header_text, 32)
            .await
            .unwrap();
        assert_eq!(body, "");
    }

    #[test]
    fn initial_body_bytes_preserves_non_utf8_upload_prefix() {
        let mut request =
            b"POST /api/session/current/uploads HTTP/1.1\r\nContent-Length: 4\r\n\r\n".to_vec();
        request.extend_from_slice(&[0xff, 0x00, 0x80, b'a']);

        assert_eq!(
            initial_body_bytes(&request).unwrap(),
            &[0xff, 0x00, 0x80, b'a']
        );
    }

    #[test]
    fn initial_body_bytes_rejects_incomplete_headers() {
        let request = b"POST /api/session/current/uploads HTTP/1.1\r\nContent-Length: 4\r\n";
        assert!(initial_body_bytes(request).is_err());
    }

    #[test]
    fn parse_request_target_splits_method_path_query() {
        assert_eq!(
            parse_request_target("GET /wasm-station/station_web_bg.wasm?v=abc123 HTTP/1.1"),
            ("GET", "/wasm-station/station_web_bg.wasm", "v=abc123")
        );
        assert_eq!(
            parse_request_target("HEAD /app HTTP/1.1"),
            ("HEAD", "/app", "")
        );
        assert_eq!(
            parse_request_target("POST /api/settings HTTP/1.1"),
            ("POST", "/api/settings", "")
        );
        assert_eq!(parse_request_target(""), ("", "", ""));
    }

    #[test]
    fn path_is_or_under_matches_routes_not_substrings() {
        assert!(path_is_or_under("/api/peers", "/api/peers"));
        assert!(path_is_or_under("/api/peers/p-1/message", "/api/peers"));
        assert!(!path_is_or_under("/api/peersonal", "/api/peers"));
        assert!(!path_is_or_under("/api", "/api/peers"));
        // Callers pass the parse_request_target path, so a query string
        // mentioning a route never reaches this predicate as path text.
        let (_, path, _) = parse_request_target("GET /api/fs/stat?path=/api/peers HTTP/1.1");
        assert!(!path_is_or_under(path, "/api/peers"));
    }

    #[test]
    fn if_none_match_tolerates_quotes_weak_prefix_and_lists() {
        assert!(if_none_match_matches(
            "GET / HTTP/1.1\r\nIf-None-Match: \"abc\"\r\n",
            "abc"
        ));
        assert!(if_none_match_matches("If-None-Match: W/\"abc\"", "abc"));
        assert!(if_none_match_matches("if-none-match: w/\"abc\"", "abc"));
        assert!(if_none_match_matches(
            "If-None-Match: \"x\", W/\"abc\" , \"y\"",
            "abc"
        ));
        assert!(if_none_match_matches("If-None-Match: abc", "abc"));
        assert!(if_none_match_matches("If-None-Match: *", "abc"));
        assert!(!if_none_match_matches("If-None-Match: \"def\"", "abc"));
        assert!(!if_none_match_matches("X-Custom: \"abc\"", "abc"));
        assert!(!if_none_match_matches("GET / HTTP/1.1\r\n", "abc"));
    }

    #[test]
    fn accept_encoding_gzip_negotiation() {
        assert!(accept_encoding_allows_gzip(
            "GET / HTTP/1.1\r\nAccept-Encoding: gzip, deflate, br\r\n"
        ));
        assert!(accept_encoding_allows_gzip("Accept-Encoding: GZIP"));
        assert!(accept_encoding_allows_gzip("accept-encoding: x-gzip;q=0.5"));
        assert!(accept_encoding_allows_gzip(
            "Accept-Encoding: br, gzip;q=0.8"
        ));
        assert!(!accept_encoding_allows_gzip("Accept-Encoding: br"));
        assert!(!accept_encoding_allows_gzip("Accept-Encoding: gzip;q=0"));
        assert!(!accept_encoding_allows_gzip("Accept-Encoding: gzip;q=0.0"));
        assert!(!accept_encoding_allows_gzip("GET / HTTP/1.1\r\n"));
    }

    #[test]
    fn fleet_cors_paths_cover_exactly_the_access_apis() {
        // is_fleet_cors_access_path derives from the route table (S6):
        // every FleetAllowlist row's path answers true — a new fleet
        // row (the fleet-cert ROW-NEW is the first) joins the write-side
        // origin gate by declaration instead of by remembering to edit
        // a hand-kept list.
        for path in [
            "/api/access/overview",
            "/api/access/iam/state",
            "/api/access/enrollment-requests",
            "/api/access/enrollment-requests/decide",
            "/api/access/iam/user-client-grants",
            "/api/access/iam/grants/update",
            "/api/access/orgs/trust",
            "/api/access/orgs/revoke",
            "/api/access/connect/status",
            "/api/access/connect/claim-code",
            "/api/access/connect/config",
            "/api/access/connect/unclaim",
            "/api/access/tier",
            "/api/access/hosted-ceiling",
            "/api/access/fleet-cert/request",
        ] {
            assert!(is_fleet_cors_access_path(path), "{path}");
        }
        assert!(!is_fleet_cors_access_path("/api/peers"));
        assert!(!is_fleet_cors_access_path("/config"));
        // Public and own-origin access rows stay out of the fleet set.
        assert!(!is_fleet_cors_access_path("/api/access/org-grants"));
        assert!(!is_fleet_cors_access_path("/api/access/org-grants/issue"));

        // A pre-existing wildcard must be REPLACED, never duplicated.
        let with = HttpResponse::with_content("200 OK", "application/json", "{}")
            .header("Access-Control-Allow-Origin", "*")
            .fleet_cors(Some("https://daemon.local:8765"))
            .into_string();
        assert_eq!(with.matches("Access-Control-Allow-Origin").count(), 1);
        assert!(with.contains("Access-Control-Allow-Origin: https://daemon.local:8765"));
        assert!(with.contains("Vary: Origin"));
        assert!(with.ends_with("\r\n\r\n{}"));
        let without = HttpResponse::with_content("200 OK", "application/json", "{}")
            .header("Access-Control-Allow-Origin", "*")
            .fleet_cors(None)
            .into_string();
        assert!(!without.contains("Access-Control-Allow-Origin"));
        assert!(without.contains("Vary: Origin"));
    }

    #[test]
    fn json_responses_are_same_origin_by_default() {
        let response = json_response("200 OK", "{}".to_string());
        assert!(!response.contains("Access-Control-Allow-Origin"));
        let public = with_public_cors(response);
        assert!(public.contains("Access-Control-Allow-Origin: *"));
        assert!(is_own_or_app_origin(
            "https://daemon.local:8765",
            true,
            "GET / HTTP/1.1\r\nHost: daemon.local:8765\r\n",
        ));
        assert!(is_own_or_app_origin("intendant://backend", true, ""));
        assert!(!is_own_or_app_origin("null", true, ""));
        assert!(!is_own_or_app_origin(
            "https://evil.example",
            true,
            "GET / HTTP/1.1\r\nHost: daemon.local:8765\r\n",
        ));
    }

    #[test]
    fn extract_token_query_param_finds_token() {
        assert_eq!(
            extract_token_query_param("GET /ws?token=abc HTTP/1.1"),
            Some("abc".to_string())
        );
    }

    #[test]
    fn extract_token_query_param_finds_token_among_others() {
        assert_eq!(
            extract_token_query_param("GET /ws?other=x&token=abc&more=y HTTP/1.1"),
            Some("abc".to_string())
        );
    }

    #[test]
    fn extract_token_query_param_returns_none_when_absent() {
        assert_eq!(extract_token_query_param("GET /ws HTTP/1.1"), None);
        assert_eq!(extract_token_query_param("GET /ws?other=x HTTP/1.1"), None);
    }

    #[test]
    fn extract_token_query_param_handles_no_request_line() {
        assert_eq!(extract_token_query_param(""), None);
    }
}

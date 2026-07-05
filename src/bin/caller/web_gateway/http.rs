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

/// Boxed demuxed connection (plain TCP or TLS), used for all post-demux
/// HTTP/WebSocket handling.
pub(crate) type DemuxStream = std::pin::Pin<Box<dyn AsyncReadWrite>>;

/// Finalize a one-shot HTTP response on a demuxed stream before it drops.
///
/// Every dashboard HTTP reply is a single buffered response sent with
/// `Connection: close`, after which the connection task returns and the
/// boxed [`DemuxStream`] is dropped. For a plain `TcpStream` that's fine —
/// the kernel keeps queued bytes and flushes them on close. But the TLS
/// path's stream is a `tokio_rustls::server::TlsStream`, which buffers
/// *ciphertext* inside the rustls session: `write_all` only guarantees the
/// plaintext was accepted into that buffer, not that the encrypted records
/// reached the socket. Dropping the `TlsStream` without flushing discards
/// the unwritten tail records, truncating large bodies (e.g. the ~871 KB
/// `app.html` arrived ~19.5 KB short over HTTPS).
///
/// Calling `flush` drives rustls to emit all buffered ciphertext to the
/// TCP socket; `shutdown` then writes the TLS `close_notify` and the TCP
/// FIN, closing the session cleanly. On the plain path both delegate
/// straight through to the `TcpStream` (flush is a no-op, shutdown sends
/// the FIN we'd send on drop anyway), so behavior there is unchanged.
pub(crate) async fn finalize_http_stream(stream: &mut DemuxStream) {
    use tokio::io::AsyncWriteExt;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

pub(crate) fn json_response_body(body: String) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body
    )
}

/// Gzip-compress `data` (pure-Rust miniz_oxide backend via flate2).
pub(crate) fn gzip_compress(data: &[u8]) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::with_capacity(data.len() / 2), Compression::default());
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

/// Read the full POST body (honoring Content-Length). Returns the peeked
/// prefix if the headers already carried the entire payload; otherwise reads
/// the remainder from the stream.
pub(crate) async fn read_post_body<S: AsyncRead + Unpin>(
    header_text: &str,
    stream: &mut S,
) -> String {
    use tokio::io::AsyncReadExt;
    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
    if peeked_body.len() >= content_length {
        return crate::types::truncate_str(peeked_body, content_length).to_string();
    }
    let mut full = peeked_body.to_string();
    let remaining = content_length.saturating_sub(peeked_body.len());
    if remaining > 0 {
        let mut rest = vec![0u8; remaining];
        if stream.read_exact(&mut rest).await.is_ok() {
            full.push_str(&String::from_utf8_lossy(&rest));
        }
    }
    full
}

/// Stream the body of an HTTP request into a fresh tempfile, honouring
/// `Content-Length` and bailing out early if the body exceeds `max_bytes`.
///
/// Returns `(tempfile, size)` on success. Designed so the caller can then
/// commit the tempfile into the upload store via
/// [`crate::upload_store::commit_upload`], which atomically renames it
/// into place.
///
/// This is the binary counterpart to `read_post_body` — same peek-then-
/// stream pattern, but sinks to disk instead of a UTF-8 `String`.
pub(crate) async fn stream_body_to_tempfile<S: AsyncRead + Unpin>(
    header_text: &str,
    initial_request_bytes: &[u8],
    stream: &mut S,
    max_bytes: usize,
) -> Result<(tempfile::NamedTempFile, usize), String> {
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
    Ok((tmp, written))
}

pub(crate) fn initial_body_bytes(initial_request_bytes: &[u8]) -> Result<&[u8], String> {
    initial_request_bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|idx| &initial_request_bytes[idx + 4..])
        .ok_or_else(|| "incomplete HTTP headers".to_string())
}

pub(crate) fn json_response(status: &str, body: String) -> String {
    format!(
        "HTTP/1.1 {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        status,
        body.len(),
        body
    )
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
    format!(
        "HTTP/1.1 {}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        status,
        body.len(),
        body
    )
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

/// Rewrite a JSON response's CORS posture for the fleet Access APIs: echo
/// the specific origin only when it passed the fleet allowlist (stripping
/// any pre-existing ACAO — duplicates are invalid to browsers). These six
/// routes must never be wildcard-readable — a cert-installed browser would
/// happily authenticate reads for any website.
pub(crate) fn with_fleet_cors(response: String, allowed_origin: Option<&str>) -> String {
    let Some(split) = response.find("\r\n\r\n") else {
        return response;
    };
    let (head, rest) = response.split_at(split);
    let mut lines: Vec<&str> = head
        .split("\r\n")
        .filter(|line| {
            !line
                .to_ascii_lowercase()
                .starts_with("access-control-allow-origin:")
        })
        .collect();
    let echo;
    if let Some(origin) = allowed_origin {
        echo = format!("Access-Control-Allow-Origin: {origin}");
        lines.push(&echo);
    }
    lines.push("Vary: Origin");
    format!("{}{rest}", lines.join("\r\n"))
}

/// Read the body of an HTTP request from `stream`, given the already-
/// peeked `header_text` (which may include a partial body in its
/// trailing portion after the `\r\n\r\n` delimiter). Returns the body
/// as an owned `String`.
///
/// Reads exactly `Content-Length` bytes total — the prefix already
/// in `header_text` plus any remainder still in the socket. Returns
/// an empty string when no `Content-Length` header is present.
///
/// Factored out of the original inline body-reading block in the
/// `/api/peers` handler so the per-peer outbound op handlers below
/// can share it without duplicating the peek-then-stream pattern.
pub(crate) async fn read_request_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    header_text: &str,
) -> String {
    use tokio::io::AsyncReadExt;
    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if content_length == 0 {
        return String::new();
    }
    let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
    if peeked_body.len() >= content_length {
        return crate::types::truncate_str(peeked_body, content_length).to_string();
    }
    let remaining = content_length.saturating_sub(peeked_body.len());
    let mut full = peeked_body.to_string();
    let mut rest = vec![0u8; remaining];
    if stream.read_exact(&mut rest).await.is_ok() {
        full.push_str(&String::from_utf8_lossy(&rest));
    }
    full
}

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

pub(crate) fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "200 OK",
        201 => "201 Created",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        403 => "403 Forbidden",
        404 => "404 Not Found",
        405 => "405 Method Not Allowed",
        413 => "413 Payload Too Large",
        429 => "429 Too Many Requests",
        500 => "500 Internal Server Error",
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
    let path_and_query = request_line.split_whitespace().nth(1)?;
    let query = path_and_query.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("token=") {
            // No URL-decoding: bearer tokens are typically URL-safe
            // (hex / base64-url). If a token contains characters that
            // require encoding, the operator can either pick a
            // different token or send via Authorization header (which
            // doesn't have the URL-encoding constraint).
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for path in [
            "/api/access/overview",
            "/api/access/iam/state",
            "/api/access/enrollment-requests",
            "/api/access/enrollment-requests/decide",
            "/api/access/iam/user-client-grants",
            "/api/access/iam/grants/update",
        ] {
            assert!(is_fleet_cors_access_path(path), "{path}");
        }
        assert!(!is_fleet_cors_access_path("/api/peers"));
        assert!(!is_fleet_cors_access_path("/config"));

        // The generic helper's wildcard must be REPLACED, never duplicated.
        let with = with_fleet_cors(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\n\r\n{}".to_string(),
            Some("https://daemon.local:8765"),
        );
        assert_eq!(with.matches("Access-Control-Allow-Origin").count(), 1);
        assert!(with.contains("Access-Control-Allow-Origin: https://daemon.local:8765"));
        assert!(with.contains("Vary: Origin"));
        assert!(with.ends_with("\r\n\r\n{}"));
        let without = with_fleet_cors(
            "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\n\r\n{}".to_string(),
            None,
        );
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

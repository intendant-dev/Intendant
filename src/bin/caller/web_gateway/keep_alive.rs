//! HTTP/1.1 keep-alive: the reusability decision table and the
//! between-requests head reader for the gateway's per-connection request
//! loop (`spawn_web_gateway`'s accept task).
//!
//! The gateway historically closed the connection after every response
//! (`Connection: close` on all of them), so a cold dashboard load paid a
//! TCP+TLS handshake per asset (~16 measured). The request loop reuses
//! the connection instead — under rules that FAIL TOWARD CLOSE, because
//! a wrongly kept-alive connection risks protocol corruption (response
//! bytes mistaken for the next response, request residue parsed as the
//! next request) while a wrongly closed one only costs a handshake.
//!
//! The decision has three independent legs, all of which must pass
//! before a connection is parked for reuse:
//!
//! 1. **Request leg** ([`request_allows_keep_alive`] + the loop's
//!    request budget + [`segment_is_single_request`]): the client's
//!    HTTP version and `Connection` tokens permit reuse, the
//!    per-connection request cap isn't exhausted, and the captured
//!    request segment carries no pipelined surplus (the head read
//!    consumes the whole segment, so surplus bytes would be lost —
//!    closing makes the client resend them, exactly as the
//!    close-per-request server always did).
//! 2. **Body leg** ([`DemuxStream::mark_request_body_consumed`], set by
//!    dispatch): the request body was provably consumed in full — no
//!    body at all ([`request_is_bodyless`]), or a table route whose
//!    declared `BodyPolicy` had dispatch read exactly `Content-Length`
//!    bytes ([`request_body_is_delimited`] guarding the mark).
//!    `BodyPolicy::Streaming` routes (uploads, transfer chunks, the
//!    doorbell) never set the mark: their handlers drive the stream
//!    themselves and may stop mid-body on errors.
//! 3. **Response leg** (the write edges): only writers that emit
//!    self-framing responses — a `Content-Length` header, or a
//!    body-less status (204/304) — opt in, by parking the stream via
//!    [`DemuxStream::park`] instead of finalizing it. Streaming writers
//!    (the sessions/search NDJSON lanes, `100 Continue` upload flows,
//!    hand-rolled multi-write handlers) never park, so an unenumerated
//!    streaming path fails safe to close, never to corruption.
//!
//! The `Connection` response header is derived from the same verdict at
//! the write edge ([`super::HttpResponse::connection_reuse`],
//! [`apply_keep_alive_header_tail`]): `keep-alive` + `Keep-Alive:
//! timeout=N` exactly when the stream will actually be parked, else the
//! historical `Connection: close`, byte-identical.
//!
//! h2 note: ALPN stays pinned to `http/1.1` (`web_tls.rs`); this module
//! is deliberately HTTP/1.1-only. HTTP/2 would be a server-stack
//! migration (a different program), not an extension of this loop.

/// Idle seconds the request loop waits between kept-alive requests, and
/// the value advertised in `Keep-Alive: timeout=N`. Bounds how long an
/// idle browser pins a connection task; browsers reuse well inside it.
pub(crate) const KEEP_ALIVE_IDLE_SECS: u64 = 30;

/// Requests served on one connection before the loop stops offering
/// keep-alive (the final response says `Connection: close`). Bounds
/// resource pinning by a single pathological client; a page load is a
/// couple dozen requests, so real dashboards never hit it.
pub(crate) const KEEP_ALIVE_MAX_REQUESTS: u32 = 500;

/// Cap on a follow-up request head (status line + headers). The first
/// request is bounded by the demux peek/first-read buffer; this bounds
/// the loop's accumulating reader the same order of magnitude.
pub(crate) const NEXT_REQUEST_HEAD_CAP_BYTES: usize = 16 * 1024;

/// The `Keep-Alive` response header value matching the loop's idle
/// timeout.
pub(crate) fn keep_alive_header_value() -> String {
    format!("timeout={KEEP_ALIVE_IDLE_SECS}")
}

/// Request leg: whether the client's request line + `Connection` tokens
/// permit reuse. HTTP/1.1 defaults to keep-alive unless a `close` token
/// is present; HTTP/1.0 requires an explicit `keep-alive` token; any
/// other (or unparseable) version fails toward close. `Connection` is a
/// comma-separated token list and may repeat; a `close` token anywhere
/// wins.
pub(crate) fn request_allows_keep_alive(header_text: &str) -> bool {
    let request_line = header_text.lines().next().unwrap_or("");
    let version = request_line.split_whitespace().nth(2).unwrap_or("");
    let mut close_token = false;
    let mut keep_alive_token = false;
    for line in header_text.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("connection") {
            continue;
        }
        for token in value.split(',') {
            let token = token.trim();
            if token.eq_ignore_ascii_case("close") {
                close_token = true;
            } else if token.eq_ignore_ascii_case("keep-alive") {
                keep_alive_token = true;
            }
        }
    }
    match version {
        "HTTP/1.1" => !close_token,
        "HTTP/1.0" => keep_alive_token && !close_token,
        _ => false,
    }
}

/// Whether any `Transfer-Encoding` header is present. The gateway never
/// parses transfer-encoded (chunked) request bodies — such a body can't
/// be delimited, so the exchange must close.
fn has_transfer_encoding(header_text: &str) -> bool {
    header_text.lines().skip(1).any(|line| {
        line.split_once(':')
            .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("transfer-encoding"))
    })
}

/// Strict `Content-Length`: `Ok(0)` when absent, `Ok(n)` when every
/// `Content-Length` header parses to the same `n`, `Err(())` on any
/// invalid or conflicting value (fail toward close — a request whose
/// body we can't delimit with certainty must not share a connection).
fn parsed_content_length(header_text: &str) -> Result<usize, ()> {
    let mut seen: Option<usize> = None;
    for line in header_text.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("content-length") {
            continue;
        }
        let parsed: usize = value.trim().parse().map_err(|_| ())?;
        match seen {
            Some(existing) if existing != parsed => return Err(()),
            _ => seen = Some(parsed),
        }
    }
    Ok(seen.unwrap_or(0))
}

/// Body leg, well-formedness half: the request's body (possibly empty)
/// is delimitable by our reader — no `Transfer-Encoding`, and a single
/// consistent `Content-Length` (or none). Guards the dispatch-side
/// consumed mark: `read_request_body_capped` reads by the first
/// `Content-Length` it finds, so the mark is only sound when the strict
/// parse agrees the request means what that reader read.
pub(crate) fn request_body_is_delimited(header_text: &str) -> bool {
    !has_transfer_encoding(header_text) && parsed_content_length(header_text).is_ok()
}

/// Body leg, trivial half: the request provably has no body at all.
/// True for the whole static/GET surface; the chain arms and
/// `BodyPolicy::None` routes use this as their consumed mark.
pub(crate) fn request_is_bodyless(header_text: &str) -> bool {
    !has_transfer_encoding(header_text) && parsed_content_length(header_text) == Ok(0)
}

/// Request leg, framing half: the captured segment (head + any body
/// prefix the transport read along with it) contains exactly one
/// request — a complete head and no bytes beyond `Content-Length`.
/// Pipelined surplus fails toward close: the head read consumed the
/// segment wholesale, so the surplus can't be replayed to the next
/// loop iteration; answering `Connection: close` makes the client
/// resend the surplus request on a fresh connection, which is exactly
/// the close-per-request behavior this loop replaced.
pub(crate) fn segment_is_single_request(segment: &[u8]) -> bool {
    let Some(head_end) = segment.windows(4).position(|w| w == b"\r\n\r\n") else {
        // Incomplete head in the captured segment: the request is
        // arriving fragmented and body accounting below would be
        // guesswork.
        return false;
    };
    let head_text = String::from_utf8_lossy(&segment[..head_end + 4]);
    if has_transfer_encoding(&head_text) {
        return false;
    }
    let Ok(content_length) = parsed_content_length(&head_text) else {
        return false;
    };
    // The body may extend past the segment (the rest is still in the
    // socket, and the capped body readers consume exactly the
    // remainder); only bytes BEYOND head+body make the segment
    // multi-request.
    segment.len() <= head_end + 4 + content_length
}

/// Rewrite an [`ApiResponse`]-style header tail for a kept-alive
/// exchange: drop any `Connection`/`Keep-Alive` entries the handler
/// baked in (the historical `close`) and append the keep-alive pair.
/// Close-path responses never come through here — their tails render
/// untouched, byte-identical to the pinned goldens.
pub(crate) fn apply_keep_alive_header_tail(headers: &mut Vec<(&'static str, String)>) {
    headers.retain(|(name, _)| {
        !name.eq_ignore_ascii_case("connection") && !name.eq_ignore_ascii_case("keep-alive")
    });
    headers.push(("Connection", "keep-alive".to_string()));
    headers.push(("Keep-Alive", keep_alive_header_value()));
}

/// Read the next request's segment off a kept-alive connection: bytes
/// up to and including the first `\r\n\r\n` (plus any body prefix that
/// arrived in the same reads), bounded by the idle timeout and the head
/// cap. `None` — clean EOF, timeout, oversize head, or a read error —
/// means the connection is done and the caller finalizes it.
pub(crate) async fn read_next_request_head<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    idle_timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(idle_timeout, async {
        let mut segment: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            segment.extend_from_slice(&chunk[..n]);
            if segment.windows(4).any(|w| w == b"\r\n\r\n") {
                return Some(segment);
            }
            if segment.len() > NEXT_REQUEST_HEAD_CAP_BYTES {
                return None;
            }
        }
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Request leg: version + Connection tokens ──

    #[test]
    fn http11_defaults_to_keep_alive() {
        assert!(request_allows_keep_alive(
            "GET /config HTTP/1.1\r\nHost: x\r\n\r\n"
        ));
    }

    #[test]
    fn http11_connection_close_closes() {
        assert!(!request_allows_keep_alive(
            "GET / HTTP/1.1\r\nConnection: close\r\n\r\n"
        ));
        // Case-insensitive header name and token.
        assert!(!request_allows_keep_alive(
            "GET / HTTP/1.1\r\nCONNECTION: Close\r\n\r\n"
        ));
        // `close` anywhere in a token list wins.
        assert!(!request_allows_keep_alive(
            "GET / HTTP/1.1\r\nConnection: keep-alive, close\r\n\r\n"
        ));
        // A second Connection header carrying close also wins.
        assert!(!request_allows_keep_alive(
            "GET / HTTP/1.1\r\nConnection: keep-alive\r\nConnection: close\r\n\r\n"
        ));
    }

    #[test]
    fn http11_unrelated_connection_tokens_keep_alive() {
        // `Connection: Upgrade` (non-WS upgrades land here) is not close.
        assert!(request_allows_keep_alive(
            "GET / HTTP/1.1\r\nConnection: Upgrade\r\n\r\n"
        ));
    }

    #[test]
    fn http10_requires_explicit_keep_alive() {
        assert!(!request_allows_keep_alive(
            "GET / HTTP/1.0\r\nHost: x\r\n\r\n"
        ));
        assert!(request_allows_keep_alive(
            "GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n"
        ));
        assert!(!request_allows_keep_alive(
            "GET / HTTP/1.0\r\nConnection: keep-alive, close\r\n\r\n"
        ));
    }

    #[test]
    fn unknown_or_missing_version_closes() {
        assert!(!request_allows_keep_alive("GET / HTTP/2.0\r\n\r\n"));
        assert!(!request_allows_keep_alive("GET /\r\n\r\n"));
        assert!(!request_allows_keep_alive(""));
        assert!(!request_allows_keep_alive("garbage\r\n\r\n"));
    }

    // ── Body leg: Content-Length / Transfer-Encoding ──

    #[test]
    fn bodyless_when_no_content_length() {
        assert!(request_is_bodyless("GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert!(request_is_bodyless(
            "GET / HTTP/1.1\r\nContent-Length: 0\r\n\r\n"
        ));
    }

    #[test]
    fn body_present_or_undelimited_is_not_bodyless() {
        assert!(!request_is_bodyless(
            "POST / HTTP/1.1\r\nContent-Length: 12\r\n\r\n"
        ));
        assert!(!request_is_bodyless(
            "POST / HTTP/1.1\r\nContent-Length: nope\r\n\r\n"
        ));
        assert!(!request_is_bodyless(
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"
        ));
    }

    #[test]
    fn delimited_rejects_conflicts_and_transfer_encoding() {
        assert!(request_body_is_delimited(
            "POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\n"
        ));
        // Duplicate-but-equal collapses (lenient like intermediaries).
        assert!(request_body_is_delimited(
            "POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n"
        ));
        assert!(!request_body_is_delimited(
            "POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n"
        ));
        assert!(!request_body_is_delimited(
            "POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n"
        ));
        // Smuggling shape: TE + CL together always closes.
        assert!(!request_body_is_delimited(
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n"
        ));
    }

    // ── Request leg: single-request segment framing ──

    #[test]
    fn single_request_segment_shapes() {
        // Bare GET, nothing after the head.
        assert!(segment_is_single_request(
            b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"
        ));
        // POST whose body is partially (or fully) in the segment.
        assert!(segment_is_single_request(
            b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhel"
        ));
        assert!(segment_is_single_request(
            b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello"
        ));
    }

    #[test]
    fn pipelined_or_malformed_segments_close() {
        // A second pipelined request in the same segment.
        assert!(!segment_is_single_request(
            b"GET / HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\n\r\n"
        ));
        // Body overrun past Content-Length.
        assert!(!segment_is_single_request(
            b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nhello"
        ));
        // Incomplete head.
        assert!(!segment_is_single_request(b"GET / HTTP/1.1\r\nHost: x"));
        // Undelimitable bodies.
        assert!(!segment_is_single_request(
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"
        ));
        assert!(!segment_is_single_request(
            b"POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n"
        ));
    }

    // ── Header tail rewrite ──

    #[test]
    fn keep_alive_tail_replaces_baked_close() {
        let mut headers = vec![
            ("Cache-Control", "no-cache".to_string()),
            ("Connection", "close".to_string()),
        ];
        apply_keep_alive_header_tail(&mut headers);
        assert_eq!(
            headers,
            vec![
                ("Cache-Control", "no-cache".to_string()),
                ("Connection", "keep-alive".to_string()),
                ("Keep-Alive", format!("timeout={KEEP_ALIVE_IDLE_SECS}")),
            ]
        );
    }

    #[test]
    fn keep_alive_tail_appends_when_absent() {
        let mut headers = vec![("Cache-Control", "no-store".to_string())];
        apply_keep_alive_header_tail(&mut headers);
        assert_eq!(headers[1].0, "Connection");
        assert_eq!(headers[1].1, "keep-alive");
        assert_eq!(headers[2].0, "Keep-Alive");
    }

    // ── Between-requests head reader ──

    #[tokio::test]
    async fn next_head_reads_across_fragmented_writes() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        use tokio::io::AsyncWriteExt;
        let writer = tokio::spawn(async move {
            client.write_all(b"GET /a HTTP/1.1\r\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            client.write_all(b"Host: x\r\n\r\n").await.unwrap();
            client
        });
        let head = read_next_request_head(&mut server, std::time::Duration::from_secs(2))
            .await
            .expect("head");
        assert_eq!(head, b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n");
        drop(writer.await.unwrap());
    }

    #[tokio::test]
    async fn next_head_none_on_eof_timeout_and_oversize() {
        // Clean EOF (client hung up between requests).
        let (client, mut server) = tokio::io::duplex(64);
        drop(client);
        assert!(
            read_next_request_head(&mut server, std::time::Duration::from_secs(1))
                .await
                .is_none()
        );

        // Idle timeout: nothing ever arrives.
        let (_client, mut server) = tokio::io::duplex(64);
        assert!(
            read_next_request_head(&mut server, std::time::Duration::from_millis(50))
                .await
                .is_none()
        );

        // Oversize head without a terminator.
        let (mut client, mut server) = tokio::io::duplex(1 << 20);
        use tokio::io::AsyncWriteExt;
        let junk = vec![b'a'; NEXT_REQUEST_HEAD_CAP_BYTES + 4096];
        let writer = tokio::spawn(async move {
            let _ = client.write_all(&junk).await;
            client
        });
        assert!(
            read_next_request_head(&mut server, std::time::Duration::from_secs(2))
                .await
                .is_none()
        );
        drop(writer.await.unwrap());
    }
}

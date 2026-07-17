//! The shared SSE streaming core: byte-accurate chunk→event framing and
//! the provider-agnostic stream driver. The three providers carried
//! near-identical hand-rolled SSE loops that had already drifted on
//! protocol details; the mechanics live here exactly once, and each
//! provider keeps only its event vocabulary as an [`SseFold`]
//! implementation (the same seam discipline as the request-build helpers:
//! behavior differences stay per-provider, never `if provider == …` in
//! shared code).

use super::{ProviderHttpResponse, StreamEvent};
use crate::error::CallerError;
use futures_util::StreamExt;

/// Byte-accurate SSE event framing. Network chunks accumulate as raw
/// bytes and are drained by offset (`pos`), compacting only when the
/// consumed prefix outgrows the live tail — no per-line reallocation.
/// Text conversion happens per complete line, so a chunk boundary that
/// splits a multibyte UTF-8 codepoint (common with CJK/emoji streaming)
/// never manufactures U+FFFD replacement characters; a genuinely invalid
/// byte still degrades to U+FFFD.
///
/// Framing follows the SSE wire format: `\n` line endings with trailing
/// `\r`s stripped (CRLF parity with the old `trim_end_matches('\r')`),
/// blank-line event boundaries, multi-line `data:` fields joined with
/// `\n` (the behavior `parse_sse_line` documented but none of the three
/// loops implemented), and an optional `event:` type. Comment (`:`),
/// `id:`, `retry:`, and unknown lines are ignored, as the per-line loops
/// ignored them. Only `data: `/`event: ` with the space are recognized —
/// the exact acceptance set of the loops this replaces.
pub(crate) struct SseFramer {
    /// Raw unconsumed bytes; `pos` is the consumed prefix.
    buf: Vec<u8>,
    pos: usize,
    /// Accumulators for the in-progress event.
    event_type: Option<String>,
    data: String,
    have_data: bool,
    /// Storage for the event handed out by the last `next_event`/`finish`
    /// call — [`SseEvent`] borrows from here instead of allocating.
    out_event: Option<String>,
    out_data: String,
}

/// One framed SSE event, borrowed from the framer.
pub(crate) struct SseEvent<'a> {
    /// The `event:` field, when the provider sent one (Anthropic does).
    /// None of the current folds dispatch on it — the payload's own JSON
    /// `type` field is the vocabulary — but the framer surfaces it so
    /// they could (framing tests pin it).
    #[allow(dead_code)]
    pub(crate) event: Option<&'a str>,
    /// The joined `data:` payload. Never empty: events without data lines
    /// dispatch nothing (SSE spec, and the old loops skipped blank and
    /// non-`data:` lines the same way).
    pub(crate) data: &'a str,
}

impl SseFramer {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            pos: 0,
            event_type: None,
            data: String::new(),
            have_data: false,
            out_event: None,
            out_data: String::new(),
        }
    }

    /// Append a raw network chunk.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        // Compact when the dead prefix outgrows the live tail: amortized
        // O(bytes) total, instead of a per-line memmove of the remainder.
        if self.pos > 0 && self.pos > self.buf.len() / 2 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// Pop the next complete event, if a blank-line boundary has arrived.
    /// The partial tail (unterminated line or unterminated event) stays
    /// buffered for the next chunk.
    pub(crate) fn next_event(&mut self) -> Option<SseEvent<'_>> {
        loop {
            let rel_newline = self.buf[self.pos..].iter().position(|&b| b == b'\n')?;
            let line_start = self.pos;
            let mut line_end = line_start + rel_newline;
            self.pos = line_end + 1;
            while line_end > line_start && self.buf[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            if line_end == line_start {
                // Blank line: event boundary. Dispatch only when data
                // accumulated — an event with an empty data buffer
                // dispatches nothing (SSE spec; heartbeat blank lines and
                // bare `event:` lines fell through the old loops too).
                if self.have_data {
                    self.out_event = self.event_type.take();
                    self.out_data = std::mem::take(&mut self.data);
                    self.have_data = false;
                    return Some(SseEvent {
                        event: self.out_event.as_deref(),
                        data: &self.out_data,
                    });
                }
                self.event_type = None;
                continue;
            }
            let line = String::from_utf8_lossy(&self.buf[line_start..line_end]);
            if let Some(value) = line.strip_prefix("data: ") {
                if self.have_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.have_data = true;
            } else if let Some(value) = line.strip_prefix("event: ") {
                self.event_type = Some(value.to_string());
            }
        }
    }

    /// Flush the in-progress event at end-of-stream. The per-line loops
    /// processed every complete `data:` line immediately, so a stream
    /// whose final event is not blank-line-terminated must still
    /// dispatch — spec-pedantic discard here would be silent data loss
    /// relative to the loops this replaces. A trailing *partial line*
    /// (no `\n`) stays undelivered, exactly as before.
    pub(crate) fn finish(&mut self) -> Option<SseEvent<'_>> {
        if !self.have_data {
            return None;
        }
        self.out_event = self.event_type.take();
        self.out_data = std::mem::take(&mut self.data);
        self.have_data = false;
        Some(SseEvent {
            event: self.out_event.as_deref(),
            data: &self.out_data,
        })
    }
}

/// Why a stream died after a successful open. Typed so the agent loop can
/// classify mid-stream failures structurally; the legacy
/// `"Stream error"` string match stays as a compatibility fallback, not
/// the contract.
pub(crate) enum StreamFailure {
    /// The chunk stream failed mid-body. Open-side failures never reach
    /// the driver: request-open status and transport retries live in
    /// `send_with_retry`, and the callers accept the status before
    /// handing the response over.
    Chunk { source: String },
    /// A fold rejected an event. None of the current folds produce one —
    /// unparseable payloads degrade to a logged drop (see [`EventJson`])
    /// exactly as the old loops silently dropped them — but the lane
    /// exists so a fold can fail typed.
    Fold(CallerError),
}

impl StreamFailure {
    /// Collapse into the caller-facing error, preserving the exact legacy
    /// message shape (`Provider error: Stream error: …`) that session
    /// logs carry and the agent loop's fallback string match accepts.
    pub(crate) fn into_caller_error(self) -> CallerError {
        match self {
            StreamFailure::Chunk { source } => CallerError::StreamChunk(source),
            StreamFailure::Fold(e) => e,
        }
    }
}

/// A provider's arm of the stream driver: folds one SSE `data:` payload
/// into its in-progress response state. `[DONE]` never reaches a fold —
/// the driver consumes it.
pub(crate) trait SseFold {
    fn on_data(
        &mut self,
        data: &str,
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<(), CallerError>;
}

/// Run one SSE response through a provider fold: bytes → [`SseFramer`] →
/// `[DONE]` filtering → `fold.on_data` per event, with the end-of-stream
/// flush. The single implementation of the shell the three provider
/// loops used to mirror.
pub(crate) async fn run_sse_stream<F: SseFold>(
    response: ProviderHttpResponse,
    fold: &mut F,
    on_event: &(dyn Fn(StreamEvent) + Send + Sync),
) -> Result<(), StreamFailure> {
    drive_sse_stream(response.bytes_stream(), fold, on_event).await
}

/// [`run_sse_stream`]'s transport-generic core, split out so tests can
/// drive the full framer+fold path from scripted chunk sequences without
/// an HTTP response.
pub(crate) async fn drive_sse_stream<F, S>(
    mut stream: S,
    fold: &mut F,
    on_event: &(dyn Fn(StreamEvent) + Send + Sync),
) -> Result<(), StreamFailure>
where
    F: SseFold,
    S: futures_util::Stream<Item = Result<bytes::Bytes, String>> + Unpin,
{
    let mut framer = SseFramer::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| StreamFailure::Chunk { source })?;
        framer.push(&chunk);
        while let Some(event) = framer.next_event() {
            if event.data == "[DONE]" {
                continue;
            }
            fold.on_data(event.data, on_event)
                .map_err(StreamFailure::Fold)?;
        }
    }
    if let Some(event) = framer.finish() {
        if event.data != "[DONE]" {
            fold.on_data(event.data, on_event)
                .map_err(StreamFailure::Fold)?;
        }
    }
    Ok(())
}

/// The shared `data:` → JSON parse with first-failure observability. The
/// old loops dropped unparseable payloads silently (`if let Ok(...)`);
/// the first one per stream now leaves a masked, truncated stderr note so
/// a protocol break is diagnosable. Each fold owns one — the flag is
/// per-stream state.
pub(crate) struct EventJson {
    logged_unparseable: bool,
}

impl EventJson {
    pub(crate) fn new() -> Self {
        Self {
            logged_unparseable: false,
        }
    }

    pub(crate) fn parse(&mut self, data: &str) -> Option<serde_json::Value> {
        match serde_json::from_str(data) {
            Ok(value) => Some(value),
            Err(e) => {
                if !self.logged_unparseable {
                    self.logged_unparseable = true;
                    let head: String = data.chars().take(120).collect();
                    eprintln!(
                        "[provider] dropping unparseable SSE data payload (first this stream): {e}: {}",
                        super::mask_api_keys(&head)
                    );
                }
                None
            }
        }
    }
}

/// Fixture plumbing for the per-provider fold tests: drive a fold over a
/// scripted SSE transcript exactly the way `run_sse_stream` would,
/// split into fixed-size chunks (small sizes exercise every boundary,
/// including mid-multibyte and mid-line), returning the Delta events
/// emitted along the way.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) async fn drive_transcript<F: SseFold>(
        fold: &mut F,
        transcript: &str,
        chunk_size: usize,
    ) -> Vec<String> {
        let deltas = std::sync::Mutex::new(Vec::new());
        let on_event = |event: StreamEvent| {
            if let StreamEvent::Delta(text) = event {
                deltas.lock().unwrap().push(text);
            }
        };
        let chunks: Vec<Result<bytes::Bytes, String>> = transcript
            .as_bytes()
            .chunks(chunk_size.max(1))
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        drive_sse_stream(futures_util::stream::iter(chunks), fold, &on_event)
            .await
            .map_err(|failure| failure.into_caller_error().to_string())
            .expect("scripted transcript must fold cleanly");
        deltas.into_inner().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Drain every currently complete event as owned (type, data) pairs.
    fn drain_events(framer: &mut SseFramer) -> Vec<(Option<String>, String)> {
        let mut events = Vec::new();
        while let Some(ev) = framer.next_event() {
            events.push((ev.event.map(str::to_string), ev.data.to_string()));
        }
        events
    }

    #[test]
    fn framer_single_data_event() {
        let mut framer = SseFramer::new();
        framer.push(b"data: {\"k\":1}\n\n");
        let events = drain_events(&mut framer);
        assert_eq!(events, vec![(None, "{\"k\":1}".to_string())]);
        assert!(framer.next_event().is_none());
        assert!(framer.finish().is_none());
    }

    #[test]
    fn framer_event_type_and_data() {
        let mut framer = SseFramer::new();
        framer.push(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n");
        let events = drain_events(&mut framer);
        assert_eq!(
            events,
            vec![(
                Some("message_start".to_string()),
                "{\"type\":\"message_start\"}".to_string()
            )]
        );
    }

    #[test]
    fn framer_multibyte_split_across_two_chunks() {
        // "…" is E2 80 A6; split it mid-sequence the way a network chunk
        // boundary can. Converting per chunk would corrupt each half into
        // U+FFFD.
        let payload = "data: a…b\n\n".as_bytes();
        let mut framer = SseFramer::new();
        framer.push(&payload[..9]); // "data: a" + E2 80 (mid-codepoint)
        assert!(framer.next_event().is_none());
        framer.push(&payload[9..]); // A6 + "b\n\n"
        let events = drain_events(&mut framer);
        assert_eq!(events, vec![(None, "a…b".to_string())]);
    }

    #[test]
    fn framer_emoji_split_across_three_chunks() {
        // "🦀" is F0 9F A6 80 — four bytes, split across three pushes.
        let payload = "data: 🦀!\n\n".as_bytes();
        let mut framer = SseFramer::new();
        framer.push(&payload[..7]); // "data: " + F0
        assert!(framer.next_event().is_none());
        framer.push(&payload[7..9]); // 9F A6
        assert!(framer.next_event().is_none());
        framer.push(&payload[9..]); // 80 "!\n\n"
        let events = drain_events(&mut framer);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, "🦀!");
        assert!(!events[0].1.contains('\u{FFFD}'));
    }

    #[test]
    fn framer_one_byte_chunks() {
        let payload = b"event: delta\ndata: {\"a\":\"b\"}\n\ndata: [DONE]\n\n";
        let mut framer = SseFramer::new();
        let mut events = Vec::new();
        for byte in payload.iter() {
            framer.push(std::slice::from_ref(byte));
            for (ev, data) in drain_events(&mut framer) {
                events.push((ev, data));
            }
        }
        assert_eq!(
            events,
            vec![
                (Some("delta".to_string()), "{\"a\":\"b\"}".to_string()),
                (None, "[DONE]".to_string()),
            ]
        );
    }

    #[test]
    fn framer_crlf_and_heartbeat_blank_lines() {
        let mut framer = SseFramer::new();
        // CRLF endings, a stray heartbeat blank pair, and trailing '\r's
        // stripped in trim_end_matches parity.
        framer.push(b"data: hi\r\n\r\n\r\ndata: x\r\r\n\n");
        let events = drain_events(&mut framer);
        assert_eq!(
            events,
            vec![(None, "hi".to_string()), (None, "x".to_string())]
        );
    }

    #[test]
    fn framer_joins_multi_line_data() {
        // The SSE spec behavior parse_sse_line documented but no loop
        // implemented: multiple data lines in one event join with '\n'.
        let mut framer = SseFramer::new();
        framer.push(b"data: {\"a\":\ndata: 1}\n\n");
        let events = drain_events(&mut framer);
        assert_eq!(events, vec![(None, "{\"a\":\n1}".to_string())]);
    }

    #[test]
    fn framer_ignores_comments_ids_and_unspaced_prefixes() {
        let mut framer = SseFramer::new();
        // Comment, id, retry, and a space-less "data:" line are all
        // outside the acceptance set of the loops this replaces.
        framer.push(b": keep-alive\nid: 7\nretry: 100\ndata:nospace\ndata: real\n\n");
        let events = drain_events(&mut framer);
        assert_eq!(events, vec![(None, "real".to_string())]);
    }

    #[test]
    fn framer_bare_event_line_dispatches_nothing() {
        let mut framer = SseFramer::new();
        framer.push(b"event: ping\n\ndata: after\n\n");
        let events = drain_events(&mut framer);
        // The dataless ping event vanishes; its type must NOT leak onto
        // the next event.
        assert_eq!(events, vec![(None, "after".to_string())]);
    }

    #[test]
    fn framer_finish_flushes_unterminated_final_event() {
        let mut framer = SseFramer::new();
        framer.push(b"data: {\"done\":true}\n");
        assert!(framer.next_event().is_none());
        let ev = framer.finish().expect("final event must flush at EOF");
        assert_eq!(ev.data, "{\"done\":true}");
        assert!(framer.finish().is_none());
    }

    #[test]
    fn framer_finish_drops_partial_line() {
        // An unterminated *line* (no '\n') was never processed by the old
        // loops; only complete data lines flush.
        let mut framer = SseFramer::new();
        framer.push(b"data: complete\ndata: partial-tail");
        assert!(framer.next_event().is_none());
        let ev = framer.finish().expect("complete line flushes");
        assert_eq!(ev.data, "complete");
    }

    #[test]
    fn framer_partial_tail_survives_compaction() {
        let mut framer = SseFramer::new();
        // Many events churn pos forward so push's compaction actually
        // runs; a split multibyte tail must survive the memmove.
        for i in 0..200 {
            framer.push(format!("data: event-{i}\n\n").as_bytes());
            let events = drain_events(&mut framer);
            assert_eq!(events, vec![(None, format!("event-{i}"))]);
        }
        let payload = "data: a…b\n\n".as_bytes();
        framer.push(&payload[..9]);
        assert!(framer.next_event().is_none());
        framer.push(&payload[9..]);
        let events = drain_events(&mut framer);
        assert_eq!(events, vec![(None, "a…b".to_string())]);
    }

    // --- Driver ---

    /// Records every payload handed to the fold.
    struct RecordingFold {
        seen: Vec<String>,
        fail_on: Option<String>,
    }

    impl RecordingFold {
        fn new() -> Self {
            Self {
                seen: Vec::new(),
                fail_on: None,
            }
        }
    }

    impl SseFold for RecordingFold {
        fn on_data(
            &mut self,
            data: &str,
            on_event: &(dyn Fn(StreamEvent) + Send + Sync),
        ) -> Result<(), CallerError> {
            if self.fail_on.as_deref() == Some(data) {
                return Err(CallerError::Provider("fold rejected".to_string()));
            }
            self.seen.push(data.to_string());
            on_event(StreamEvent::Delta(data.to_string()));
            Ok(())
        }
    }

    fn chunk_stream(
        chunks: Vec<Result<&'static [u8], &'static str>>,
    ) -> impl futures_util::Stream<Item = Result<bytes::Bytes, String>> + Unpin {
        futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| c.map(bytes::Bytes::from_static).map_err(|e| e.to_string())),
        )
    }

    #[tokio::test]
    async fn driver_folds_events_and_filters_done() {
        let mut fold = RecordingFold::new();
        let deltas: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let on_event = |event: StreamEvent| {
            if let StreamEvent::Delta(text) = event {
                deltas.lock().unwrap().push(text);
            }
        };
        let stream = chunk_stream(vec![
            Ok(b"data: one\n\nda"),
            Ok(b"ta: two\n\ndata: [DONE]\n\n"),
        ]);
        drive_sse_stream(stream, &mut fold, &on_event)
            .await
            .map_err(|_| "driver failed")
            .unwrap();
        assert_eq!(fold.seen, vec!["one", "two"]);
        assert_eq!(*deltas.lock().unwrap(), vec!["one", "two"]);
    }

    #[tokio::test]
    async fn driver_flushes_final_event_without_blank_line() {
        let mut fold = RecordingFold::new();
        let stream = chunk_stream(vec![Ok(b"data: a\n\ndata: tail\n")]);
        drive_sse_stream(stream, &mut fold, &|_| {})
            .await
            .map_err(|_| "driver failed")
            .unwrap();
        assert_eq!(fold.seen, vec!["a", "tail"]);
    }

    #[tokio::test]
    async fn driver_chunk_failure_is_typed_and_preserves_prior_events() {
        let mut fold = RecordingFold::new();
        let stream = chunk_stream(vec![Ok(b"data: before\n\n"), Err("connection reset")]);
        let failure = drive_sse_stream(stream, &mut fold, &|_| {})
            .await
            .expect_err("chunk error must surface");
        match failure {
            StreamFailure::Chunk { source } => assert_eq!(source, "connection reset"),
            StreamFailure::Fold(_) => panic!("expected Chunk failure"),
        }
        // Deltas already emitted before the failure stand, as they did
        // when the old loops bubbled the error after processing lines.
        assert_eq!(fold.seen, vec!["before"]);
    }

    #[tokio::test]
    async fn driver_fold_error_surfaces_as_fold_failure() {
        let mut fold = RecordingFold::new();
        fold.fail_on = Some("bad".to_string());
        let stream = chunk_stream(vec![Ok(b"data: ok\n\ndata: bad\n\n")]);
        let failure = drive_sse_stream(stream, &mut fold, &|_| {})
            .await
            .expect_err("fold error must surface");
        assert!(matches!(failure, StreamFailure::Fold(_)));
        assert_eq!(fold.seen, vec!["ok"]);
    }

    #[test]
    fn stream_failure_maps_to_legacy_error_shape() {
        // The chunk lane renders exactly the string the session logs and
        // the agent loop's fallback match have always carried.
        let err = StreamFailure::Chunk {
            source: "connection reset".to_string(),
        }
        .into_caller_error();
        assert!(matches!(err, CallerError::StreamChunk(_)));
        assert_eq!(
            err.to_string(),
            "Provider error: Stream error: connection reset"
        );

        let fold_err = StreamFailure::Fold(CallerError::Provider("x".to_string()));
        assert!(matches!(
            fold_err.into_caller_error(),
            CallerError::Provider(_)
        ));
    }

    #[test]
    fn event_json_parses_and_logs_drop_once() {
        let mut json = EventJson::new();
        assert_eq!(json.parse("{\"a\":1}"), Some(serde_json::json!({"a": 1})));
        // Unparseable payloads degrade to None (dropped by folds); the
        // once-per-stream log flag flips on the first one.
        assert!(json.parse("not json").is_none());
        assert!(json.logged_unparseable);
        assert!(json.parse("still not json").is_none());
        assert_eq!(json.parse("2"), Some(serde_json::json!(2)));
    }
}

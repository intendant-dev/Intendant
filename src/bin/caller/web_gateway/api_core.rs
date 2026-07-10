//! Transport-neutral API core (transport-unification design §2.1): the
//! request/response vocabulary shared by every lane. The HTTP lane's fs
//! GET trio flows through it today (the S1 pilot); the datachannel
//! tunnel delegates family by family from S2. This module holds types
//! only — each lane's adapter lives next to that lane
//! (`http_dispatch::write_api_response` for HTTP; the tunnel framer
//! follows in S2).

/// One API invocation, however it arrived.
///
/// `params` is the canonical structured-argument shape — the tunnel's
/// `params` object verbatim; on HTTP it is synthesized by the route's
/// shim from query pairs (and, for JSON-body routes, the parsed body).
///
/// Deliberately not carried yet: the acting [`RequestAuthority`] (the
/// pre-dispatch gates stay the enforcement point until a converted
/// family consumes actor identity in-handler) and the raw-body spool
/// for `Streaming` routes (the S8 upload lane).
pub(crate) struct ApiRequest {
    pub(crate) params: serde_json::Value,
    /// Byte-range request (fs read; transfer download from S9).
    pub(crate) range: Option<ByteRange>,
}

impl ApiRequest {
    /// String param by key — absent or non-string collapses to `""`,
    /// matching both today's HTTP `unwrap_or_default()` reads and the
    /// tunnel's `string_param`.
    pub(crate) fn str_param(&self, key: &str) -> &str {
        self.params
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
    }
}

/// A byte-range request as it arrived on its transport. Satisfiability
/// (and the exact 416 wording) is resolved against the target's size
/// inside the handler, exactly where today's code resolves it. The two
/// forms keep their historical semantics: the HTTP header is
/// end-inclusive; the tunnel's offset/length form is start+count with
/// an end-exclusive `range_end` in the response meta.
pub(crate) enum ByteRange {
    /// Verbatim HTTP `Range` header value (e.g. `bytes=100-199`).
    HttpHeader(String),
    /// The datachannel tunnel's resumable form (`offset`/`length`
    /// params, pre-normalized by the tunnel adapter). `length: None`
    /// reads to end of file.
    OffsetLength { offset: u64, length: Option<u64> },
}

/// JSON response body in whichever form the handler already has (risk
/// R8: `PreSerialized` keeps today's string-building hot paths
/// allocation-identical; `Value` is for structured code).
pub(crate) enum JsonBody {
    PreSerialized(String),
    Value(serde_json::Value),
}

impl JsonBody {
    pub(crate) fn into_string(self) -> String {
        match self {
            JsonBody::PreSerialized(body) => body,
            JsonBody::Value(value) => value.to_string(),
        }
    }
}

/// Byte-lane payload. In-memory only, matching today's fully buffered
/// responses; `File`-backed streaming is a designed-for follow-up (risk
/// R7's non-goal), not a rider on this program.
pub(crate) enum BytesPayload {
    InMemory(Vec<u8>),
}

/// One API response, however it will leave the daemon. The HTTP adapter
/// renders it byte-identically to the historical handler output (the
/// golden transcripts prove it); the tunnel adapter (S2) frames `Json`
/// as a `response` frame and `Bytes` as a `byte_stream_*` sequence.
pub(crate) enum ApiResponse {
    Json {
        status: u16,
        body: JsonBody,
        /// The header tail after `Content-Type`/`Content-Length`, in
        /// wire order. Carried per response because the legacy shapes
        /// differ (the fs read 416 interleaves its range headers with
        /// the canonical tail). HTTP-lane decoration only — the tunnel
        /// framer consumes just `status` and `body`.
        headers: Vec<(&'static str, String)>,
    },
    Bytes {
        status: u16,
        content_type: String,
        /// Same contract as `Json::headers`.
        headers: Vec<(&'static str, String)>,
        bytes: BytesPayload,
        /// Sidecar result for the byte lane: the tunnel adapter emits it
        /// verbatim as `byte_stream_end.result` (the historical
        /// `{ok, path, filename, …, range_start, range_end, resumable,
        /// sha256}` object). The HTTP adapter ignores it today — the
        /// header-form responses already carry their meta as headers;
        /// the transfer rows (S9) define meta's HTTP rendering. `Null`
        /// when the response has no sidecar.
        meta: serde_json::Value,
    },
}

impl ApiResponse {
    /// The canonical JSON envelope — byte-identical to the historical
    /// `json_response` framing (`Cache-Control: no-cache` +
    /// `Connection: close` after `Content-Type`/`Content-Length`).
    pub(crate) fn json(status: u16, body: JsonBody) -> Self {
        ApiResponse::Json {
            status,
            body,
            headers: vec![
                ("Cache-Control", "no-cache".to_string()),
                ("Connection", "close".to_string()),
            ],
        }
    }

    /// `{"error": message}` in the canonical envelope — the historical
    /// `json_error` shape.
    pub(crate) fn json_error(status: u16, message: impl AsRef<str>) -> Self {
        Self::json(
            status,
            JsonBody::Value(serde_json::json!({ "error": message.as_ref() })),
        )
    }
}

/// Unified request authority (transport-unification design §2.3): the
/// acting principal plus its pre-loaded local IAM state, evaluated
/// through the one evaluator every lane shares. `HttpAccessContext` is
/// an alias of this type — the HTTP lane builds it once per connection
/// and every existing gate keeps reading it unchanged; the tunnel's
/// `DashboardControlGrant` converges via a `from_control_grant`
/// constructor when its families delegate (S2+). Filesystem scope and
/// custody attribution stay with the per-lane gates until the unified
/// `authorize_filesystem` stage.
#[derive(Debug)]
pub(crate) struct RequestAuthority {
    pub(crate) principal: crate::access::iam::AccessPrincipal,
    pub(crate) iam_state: Option<crate::access::iam::LocalIamState>,
}

impl RequestAuthority {
    pub(crate) fn decision(
        &self,
        op: crate::peer::access_policy::PeerOperation,
    ) -> crate::access::iam::AccessDecision {
        match &self.iam_state {
            Some(state) => crate::access::iam::evaluate_principal_operation_with_state(
                state,
                &self.principal,
                op,
            ),
            None => crate::access::iam::evaluate_principal_operation(&self.principal, op),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn str_param_collapses_absent_and_non_string_values() {
        let request = ApiRequest {
            params: serde_json::json!({ "path": "/tmp/x", "count": 3 }),
            range: None,
        };
        assert_eq!(request.str_param("path"), "/tmp/x");
        assert_eq!(request.str_param("count"), "");
        assert_eq!(request.str_param("missing"), "");
    }

    #[test]
    fn json_body_serializes_identically_from_both_forms() {
        let value = serde_json::json!({ "ok": true, "n": 7 });
        let pre = JsonBody::PreSerialized(value.to_string());
        assert_eq!(pre.into_string(), JsonBody::Value(value).into_string());
    }

    #[test]
    fn request_authority_root_principal_allows_filesystem_read() {
        let authority = RequestAuthority {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
                "unit-test", "https",
            ),
            iam_state: None,
        };
        let decision =
            authority.decision(crate::peer::access_policy::PeerOperation::FilesystemRead);
        assert!(decision.allowed, "{decision:?}");
    }
}

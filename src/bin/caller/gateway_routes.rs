//! Declarative route table for the web gateway.
//!
//! One declaration per HTTP API route (`/api/*`, `/session`, and `/mcp`). Four things
//! that used to be hand-synchronized across distant regions of
//! `web_gateway.rs` all derive from this table, so they cannot drift:
//!
//! 1. **Dispatch** — the request loop consults [`match_route`] and serves
//!    a matching route through its [`RouteHandlerId`] arm. The remaining
//!    if/else chain covers only the non-API surface (connect bootstrap,
//!    recordings, frames, debug, config, static assets, the SPA
//!    fallback).
//! 2. **IAM classification** — `dashboard_http_operation` is a pure
//!    [`classify`] lookup: a declared route carries its operation; an
//!    undeclared (method, path) is not a route and carries none.
//! 3. **Preflight** — `OPTIONS` answers derive the CORS posture and the
//!    `Access-Control-Allow-Methods` union from the same declarations,
//!    so a preflight can never disagree with its endpoint.
//! 4. **Docs** — [`render_endpoint_docs`] renders the endpoint table in
//!    `docs/src/web-dashboard.md`; the `endpoint_docs_match_chapter`
//!    drift test pins the chapter to it.
//! 5. **Tunnel exposure** — a row's optional [`TunnelSpec`] declares its
//!    dashboard-control datachannel twin. The tunnel's method table
//!    (`dashboard_control::control_method_spec`) resolves these rows
//!    first, then its residue `CONTROL_ONLY_METHODS`, so a twinned method's
//!    IAM operation derives from the same declaration HTTP gates on
//!    (transport-unification design §2.2) instead of a second table that
//!    can drift.
//!
//! **Never add an API route by editing the dispatch chain**: declare it
//! here and give it a handler arm in `web_gateway.rs`'s table-dispatch
//! match. Table invariants (no shadowed routes, non-empty docs, pattern
//! hygiene, posture consistency) are enforced by unit tests in this
//! module.
//!
//! `BodyPolicy` (and response-header emission generally) is still
//! declarative: handlers read their own bodies exactly as their legacy
//! arms did. Moving body consumption and response serialization into
//! dispatch is the planned follow-up (phase 4 of the route-table
//! design), not yet mechanical.

use crate::peer::access_policy::PeerOperation;
use crate::web_gateway::path_is_or_under;

/// How one fixed path segment is matched in a [`PathPattern::Segments`]
/// route. Deliberately minimal — capture / literal / one-of covers every
/// existing route; anything fancier reintroduces the ambiguity the table
/// exists to kill. If a future route seems to need more, reshape the
/// route instead of growing this language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentSpec {
    /// Any single non-empty segment; handed to the handler as a capture.
    /// The name is used for docs rendering (`/api/peers/{peer_id}/…`).
    Capture(&'static str),
    /// This exact segment.
    Literal(&'static str),
    /// One of these exact segments; the matched value is also captured.
    /// Vocabulary reserved for future multi-leaf declarations — still
    /// unadopted. The S7 peers carve settled the sub-router shape
    /// without it: per-leaf `Capture`/`Literal` rows declare each leaf
    /// (and its datachannel twin) while the family's `Any` catch-all
    /// row stays last, so garbage subpaths keep the handler-owned JSON
    /// 404s instead of dropping to the SPA shell. The doorbell keeps
    /// its bare catch-all (phase 4d).
    #[allow(dead_code)]
    OneOf(&'static [&'static str]),
}

/// How a route's path is matched. Deliberately NOT a regex/glob router —
/// three shapes cover the whole existing surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathPattern {
    /// `req_path == base`.
    Exact(&'static str),
    /// `path_is_or_under(req_path, base)` — the route owns the subtree
    /// (exact path or any real `/`-separated descendant; look-alike
    /// longer prefixes like `/api/sessionsfoo` do not match).
    Under(&'static str),
    /// `base` plus a fixed segment shape (`/api/peers/{id}/message`).
    /// The segment count is exact — no open tails.
    Segments(&'static str, &'static [SegmentSpec]),
}

/// The HTTP method a route answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteMethod {
    Get,
    Post,
    Delete,
    /// Matches any method. Used only to port legacy arms that were
    /// method-agnostic in the hand-written chain, so their observable
    /// behavior survives the move byte-for-byte. New routes declare real
    /// methods; tightening an `Any` route is a deliberate follow-up
    /// change, not part of the mechanical migration.
    Any,
}

impl RouteMethod {
    pub(crate) fn matches(self, req_method: &str) -> bool {
        match self {
            RouteMethod::Get => req_method == "GET",
            RouteMethod::Post => req_method == "POST",
            RouteMethod::Delete => req_method == "DELETE",
            RouteMethod::Any => true,
        }
    }

    // Live through render_endpoint_docs (see its note on call sites).
    #[allow(dead_code)]
    fn doc_label(self) -> &'static str {
        match self {
            RouteMethod::Get => "GET",
            RouteMethod::Post => "POST",
            RouteMethod::Delete => "DELETE",
            RouteMethod::Any => "any",
        }
    }
}

/// Who may call the route. This is the fact the IAM gate
/// (`dashboard_http_operation` consumers) and the origin gate used to
/// re-derive independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteAuthz {
    /// Gate on a `PeerOperation` via `HttpAccessContext::decision` before
    /// dispatch (the `dashboard_http_operation` behavior).
    Operation(PeerOperation),
    /// Deliberately public: no IAM gate; the payload's own
    /// signature/shape is the authority (the peer access-request
    /// doorbell, the signed org-grant/revocation endpoints). Forces a
    /// route to SAY it is public instead of falling through a match.
    Public,
    /// `/mcp`: token-bound inside the handler, with the scoped
    /// own/app-origin CORS echo. Classifies as no `PeerOperation` (the
    /// MCP layer enforces per-tool IAM itself).
    McpToken,
    /// The federation surface (`/api/peers`, `/api/coordinator/*`):
    /// the IAM operation is method-and-path dependent and already
    /// canonically defined by `federation_http_operation` — the same
    /// function the pre-dispatch federation bearer gate consults.
    /// Delegating keeps one source of truth instead of transcribing its
    /// ladder into rows that would drift from the gate.
    PeerFederation,
}

/// Which CORS/preflight posture the route gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CorsPosture {
    /// Same-origin or the `intendant://` app scheme only (the default
    /// for `/api/*`): preflight echoes an allowed origin, otherwise no
    /// ACAO header.
    OwnOrigin,
    /// The fleet Access APIs: echo only fleet-allowlisted origins.
    FleetAllowlist,
    /// The session-list lanes the multi-daemon Stats tab reads across
    /// daemons: echo fleet-allowlisted origins exactly like
    /// [`CorsPosture::FleetAllowlist`], and additionally echo loopback
    /// origins when the request itself arrived over loopback — a
    /// sibling daemon's dashboard on another port of the same machine.
    /// These rows historically baked a wildcard ACAO into their
    /// responses; the allowlist echo replaces it, so arbitrary web
    /// origins get no header at all (and are refused pre-dispatch).
    FleetOrLoopback,
    /// The public doorbell class: open by design.
    Public,
}

/// How the request body is consumed. Declarative during the migration
/// (handlers keep their exact legacy reads); phase 4 moves consumption
/// into dispatch so handlers can't forget caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyPolicy {
    /// No body is consumed for this route.
    None,
    /// Dispatch reads the body before the handler runs, capped at
    /// [`DEFAULT_BODY_CAP_BYTES`]; over-cap requests get a 413 with the
    /// route's CORS posture.
    Default,
    /// Dispatch reads the body with this route-specific cap.
    Capped(usize),
    /// The handler owns the stream: uploads spool to a tempfile, and the
    /// doorbell's cap comes from runtime config
    /// (`peer_access_requests`), which the table cannot carry.
    Streaming,
}

/// Body cap for [`BodyPolicy::Default`] routes — JSON command bodies
/// (settings, session ops, access administration, peer quick controls).
/// Before dispatch-side consumption these reads were UNBOUNDED: any
/// authenticated caller could allocate arbitrary memory with one huge
/// Content-Length. Generous headroom over every legitimate payload.
pub(crate) const DEFAULT_BODY_CAP_BYTES: usize = 4 * 1024 * 1024;

/// Body cap for `POST /mcp` — JSON-RPC tool calls can legitimately carry
/// file-sized arguments (fs tools, upload-adjacent flows).
pub(crate) const MCP_BODY_CAP_BYTES: usize = 16 * 1024 * 1024;

/// Body cap for the visual-freshness diagnostics sink (NDJSON transcript
/// batches).
pub(crate) const DIAGNOSTICS_BODY_CAP_BYTES: usize = 16 * 1024 * 1024;

/// Body cap for the Claude sign-in ceremony start (`{"mode": …}` only).
pub(crate) const CLAUDE_AUTH_START_BODY_CAP_BYTES: usize = 4 * 1024;

/// Body cap for the ceremony's pasted authorization code — a short token;
/// anything bigger is not one.
pub(crate) const CLAUDE_AUTH_CODE_BODY_CAP_BYTES: usize = 2 * 1024;

/// Body cap for the Codex sign-in ceremony start (`{"mode": …}` only).
pub(crate) const CODEX_AUTH_START_BODY_CAP_BYTES: usize = 4 * 1024;

/// Links a table row to its dispatch arm in `web_gateway.rs`. The match
/// there is exhaustive, so a declared route without an arm — or an arm
/// whose route was deleted — fails to compile; the uniqueness invariant
/// test catches a handler bound to two rows unintentionally (deliberate
/// shared-handler groups — one handler serving several adjacent
/// method/shape rows — are listed there explicitly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RouteHandlerId {
    FsStat,
    FsList,
    FsRead,
    FsMkdir,
    FsWrite,
    FsRename,
    FsDelete,
    TransferJobs,
    TransferJobCreate,
    TransferUploadChunk,
    TransferUploadCommit,
    /// Shared by the native DELETE row and the WKWebView POST
    /// `/delete`-suffix fallback (the session-delete two-shape pattern;
    /// both shapes capture the same `{id}`).
    TransferJobDelete,
    TransferDownloadRead,
    SessionToken,
    SessionCurrentChanges,
    CurrentHistory,
    CurrentRollback,
    CurrentRedo,
    CurrentPrune,
    CurrentAgentOutput,
    CurrentUploadsPost,
    CurrentUploadsGet,
    CurrentUploadDelete,
    /// Shared by the five delete-shape rows (native DELETE and the
    /// WKWebView POST `/delete`-suffix fallback); the handler filters the
    /// literal `delete` segment out, so all shapes converge.
    SessionDelete,
    SessionAgentOutput,
    /// Shared by the four `Under` rows covering session detail and the
    /// per-session artifact sub-routes (context-snapshot, recordings,
    /// report, frames). The sub-router's internal shapes are deliberately
    /// still open-world (unknown tails serve session detail, exactly as
    /// the legacy catch-all did); carving them into exact `Segments`
    /// leaves is a follow-up behavior decision, not part of the
    /// mechanical migration.
    SessionSubRouter,
    SessionForkPoints,
    /// Background-task inspector list (supervised Claude Code sessions).
    SessionBackgroundTasks,
    /// Read-only tail of one background task's output file (path served
    /// exclusively from the daemon-side task registry).
    SessionBackgroundTaskOutput,
    McAnchors,
    McRecords,
    McFission,
    WorktreesInspect,
    WorktreesRemove,
    WorktreesClean,
    WorktreesMerge,
    WorktreesScan,
    WorktreesList,
    SessionsStream,
    SessionsSearch,
    SessionsMessageSearch,
    SessionsList,
    ProjectRoot,
    SettingsPost,
    SettingsGet,
    ApiKeysPost,
    ApiKeyStatus,
    /// Claude sign-in ceremony: start `claude auth login` on a private PTY.
    ClaudeAuthStart,
    /// Ceremony state + validated sign-in URL + account info on success.
    ClaudeAuthStatus,
    /// Submit the pasted authorization code to the ceremony.
    ClaudeAuthCode,
    /// Cancel the ceremony (Ctrl-C + reap; non-destructive to the store).
    ClaudeAuthCancel,
    /// Codex sign-in ceremony: start `codex login --device-auth` on a
    /// private PTY.
    CodexAuthStart,
    /// Ceremony state + verification URL + one-time code + account info
    /// on success.
    CodexAuthStatus,
    /// Cancel the Codex ceremony (Ctrl-C + reap; non-destructive).
    CodexAuthCancel,
    ExternalAgents,
    DiagnosticsVisualFreshness,
    Displays,
    /// The public peer access-request doorbell (create + status poll).
    Doorbell,
    HostedControlBootstrap,
    HostedControlRequestCreate,
    HostedControlRequestPoll,
    HostedControlAnchorDecision,
    HostedControlWsTicket,
    HostedControlManagement,
    /// Shared by the user-client-grants / grants-update pair (one legacy
    /// arm served both paths).
    AccessIamGrants,
    AccessOrgGrantPresent,
    AccessOrgRevocations,
    /// Shared by the signed public org endpoints (ORL apply + grant
    /// renew) — one legacy arm, per-path body caps inside.
    AccessOrgApplyRenew,
    /// Shared by the seven org administration paths (trust, revoke,
    /// issue, revoke-member, issuers init/delegate/install).
    AccessOrgManage,
    AccessEnrollmentDecide,
    AccessEnrollmentRequests,
    AccessIamState,
    AccessOverview,
    AccessConnectStatus,
    AccessConnectClaimCode,
    AccessConnectConfig,
    AccessConnectUnclaim,
    /// Daemon trust-tier label (docs/src/trust-tiers.md).
    AccessTierSettings,
    /// Fleet certificate request (async-start; progress rides the
    /// connect status payload).
    AccessFleetCertRequest,
    DashboardTargets,
    /// Live dashboard connections (tab presence): the tabs registry
    /// snapshot with voice/input-authority ownership joined in.
    DashboardTabs,
    /// Agenda ledger snapshot (items + counts).
    AgendaList,
    /// Apply one agenda command (add/answer/patch/transitions/effects).
    AgendaOp,
    /// Merge-patch the owner's reminder delivery policy.
    AgendaReminderPolicy,
    /// Bounded Memory claim search (q/limit/candidates query params).
    MemorySearch,
    /// Read one Memory claim by id prefix (id query param).
    MemoryClaim,
    /// Propose one Memory claim (the candidate lane).
    MemoryPropose,
    /// The whole /api/peers registry + pairing sub-router, moved
    /// verbatim (its internal shapes stay as they were; leaf-shape
    /// declarations are a deliberate follow-up, not part of the
    /// mechanical migration).
    PeersSubRouter,
    CoordinatorRoute,
    McpPost,
    /// Shared by the GET/DELETE /mcp rows (stateless 405 responder).
    McpStream,
}

/// Datachannel (dashboard-control tunnel) twin of a route row — the
/// `tunnel:` column of the transport-unification design (§2.2). The wire
/// method name is unchanged from its legacy `CONTROL_ONLY_METHODS` entry: the
/// datachannel wire never changes; the name becomes an alias of this
/// row. The IAM operation gating the tunnel method is **derived from the
/// row** ([`Route::tunnel_operation`]) — declaring it once is the point
/// of the column: the two transports cannot drift apart again. (§2.2's
/// `lanes`/`http_params` fields arrive with the migration stages that
/// consume them; adding them before a consumer exists would be dead
/// weight.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TunnelSpec {
    /// Wire method name (e.g. `"api_fs_write"`), exactly as datachannel
    /// clients send it in `t:"request"` frames.
    pub(crate) name: &'static str,
    /// Documented divergent-by-design IAM override. `None` — the default
    /// and the point — derives the operation from the row's
    /// [`RouteAuthz::Operation`]. `Some((op, reason))` gates the tunnel
    /// method on `op` instead; the reason string is mandatory, and the
    /// `tunnel_op_overrides_are_a_closed_documented_enumeration` test
    /// pins the override list closed (design §2.7: the signed-org
    /// doorbell rows — Public on HTTP, session-gated on the tunnel — are
    /// the intended occupants when their family ports).
    pub(crate) op_override: Option<(PeerOperation, &'static str)>,
    /// Advertised in the tunnel `features` handshake (the legacy
    /// `advertised` flag).
    pub(crate) advertised: bool,
    /// May also (or only) be delivered as an upload frame (the legacy
    /// `upload` flag; feeds `authorize_dashboard_control_upload`).
    pub(crate) upload: bool,
}

/// Advertised, non-upload tunnel twin (the common shape).
const fn tunnel_method(name: &'static str) -> TunnelSpec {
    TunnelSpec {
        name,
        op_override: None,
        advertised: true,
        upload: false,
    }
}

/// Advertised tunnel twin whose IAM operation deliberately diverges
/// from the row (design §2.7): `op` gates the tunnel method, `reason`
/// documents why, and the override enumeration test pins the set
/// closed.
const fn tunnel_method_with_op(
    name: &'static str,
    op: PeerOperation,
    reason: &'static str,
) -> TunnelSpec {
    TunnelSpec {
        name,
        op_override: Some((op, reason)),
        advertised: true,
        upload: false,
    }
}

/// Advertised tunnel twin that may also arrive as an upload frame.
const fn tunnel_uploadable(name: &'static str) -> TunnelSpec {
    TunnelSpec {
        name,
        op_override: None,
        advertised: true,
        upload: true,
    }
}

/// Upload-frame-only tunnel twin: never dispatched on the request lane,
/// advertised through the "upload_frames" transport feature rather than
/// by name (the legacy `upload_only` shape).
const fn tunnel_upload_only(name: &'static str) -> TunnelSpec {
    TunnelSpec {
        name,
        op_override: None,
        advertised: false,
        upload: true,
    }
}

pub(crate) struct Route {
    pub(crate) method: RouteMethod,
    pub(crate) pattern: PathPattern,
    pub(crate) authz: RouteAuthz,
    pub(crate) cors: CorsPosture,
    pub(crate) body: BodyPolicy,
    pub(crate) handler: RouteHandlerId,
    /// One line, becomes the docs endpoint-table row. Required — an
    /// empty doc string fails the invariant test.
    pub(crate) doc: &'static str,
    /// Datachannel exposure of this row. `None` = HTTP-only. Methods
    /// with no HTTP twin live in `CONTROL_ONLY_METHODS` instead (the
    /// residue half of the tunnel method table; the differential pin
    /// test in `dashboard_control` freezes which methods live where).
    pub(crate) tunnel: Option<TunnelSpec>,
}

impl Route {
    /// Attach the row's datachannel twin (builder-style; rows are const).
    const fn with_tunnel(mut self, spec: TunnelSpec) -> Route {
        self.tunnel = Some(spec);
        self
    }

    /// The IAM operation this row's tunnel twin gates on: the documented
    /// override when declared, otherwise the row's own
    /// [`RouteAuthz::Operation`] — or, for `PeerFederation` rows, the
    /// `federation_http_operation` ladder applied to the row's
    /// [`Route::canonical_leaf`] (design §2.2 / S7): the same source the
    /// HTTP gate consults classifies the twin, so the two transports
    /// cannot drift. `None` means no fail-closed derivation exists (a
    /// Public/McpToken row without an override; a federation row whose
    /// leaf the ladder does not classify): the tunnel method table skips
    /// such a row entirely, so the authorizer's unknown-method deny is
    /// the runtime backstop, and the
    /// `tunnel_specs_are_unique_and_derive_operations_fail_closed`
    /// invariant test keeps the state unlandable.
    pub(crate) fn tunnel_operation(&self) -> Option<PeerOperation> {
        let spec = self.tunnel.as_ref()?;
        if let Some((op, _reason)) = spec.op_override {
            return Some(op);
        }
        match self.authz {
            RouteAuthz::Operation(op) => Some(op),
            RouteAuthz::PeerFederation => {
                let (verb, path) = self.canonical_leaf()?;
                crate::peer::access_policy::federation_http_operation(verb, &path)
            }
            RouteAuthz::Public | RouteAuthz::McpToken => None,
        }
    }

    /// The single (verb, path) leaf this row canonically declares —
    /// captures render as `{name}` placeholders, which the federation
    /// ladder treats like any non-empty segment. `None` for shapes with
    /// no single leaf (method-`Any` rows, `Under` subtrees, `OneOf`
    /// segments): a federation row of those shapes cannot carry a
    /// ladder-derived tunnel twin (fail-closed; the invariant test
    /// rejects the combination).
    fn canonical_leaf(&self) -> Option<(&'static str, String)> {
        let verb = match self.method {
            RouteMethod::Get => "GET",
            RouteMethod::Post => "POST",
            RouteMethod::Delete => "DELETE",
            RouteMethod::Any => return None,
        };
        let path = match self.pattern {
            PathPattern::Exact(base) => base.to_string(),
            PathPattern::Under(_) => return None,
            PathPattern::Segments(base, specs) => {
                let mut out = base.to_string();
                for spec in specs {
                    out.push('/');
                    match spec {
                        SegmentSpec::Capture(name) => {
                            out.push('{');
                            out.push_str(name);
                            out.push('}');
                        }
                        SegmentSpec::Literal(literal) => out.push_str(literal),
                        SegmentSpec::OneOf(_) => return None,
                    }
                }
                out
            }
        };
        Some((verb, path))
    }
}

/// Compact constructor for the common row shape: IAM-gated via a
/// `PeerOperation`, own-origin CORS.
const fn op_route(
    method: RouteMethod,
    pattern: PathPattern,
    op: PeerOperation,
    body: BodyPolicy,
    handler: RouteHandlerId,
    doc: &'static str,
) -> Route {
    Route {
        method,
        pattern,
        authz: RouteAuthz::Operation(op),
        cors: CorsPosture::OwnOrigin,
        body,
        handler,
        doc,
        tunnel: None,
    }
}

/// Session-list rows the multi-daemon Stats tab fetches cross-origin:
/// IAM-gated like `op_route`, with the fleet-or-loopback CORS echo
/// instead of own-origin.
const fn fleet_or_loopback_route(
    method: RouteMethod,
    pattern: PathPattern,
    op: PeerOperation,
    body: BodyPolicy,
    handler: RouteHandlerId,
    doc: &'static str,
) -> Route {
    Route {
        method,
        pattern,
        authz: RouteAuthz::Operation(op),
        cors: CorsPosture::FleetOrLoopback,
        body,
        handler,
        doc,
        tunnel: None,
    }
}

/// Fleet Access API rows: IAM-gated, fleet-allowlisted CORS.
const fn fleet_route(
    method: RouteMethod,
    pattern: PathPattern,
    op: PeerOperation,
    body: BodyPolicy,
    handler: RouteHandlerId,
    doc: &'static str,
) -> Route {
    Route {
        method,
        pattern,
        authz: RouteAuthz::Operation(op),
        cors: CorsPosture::FleetAllowlist,
        body,
        handler,
        doc,
        tunnel: None,
    }
}

/// Deliberately public rows (doorbell class): no IAM gate, public CORS.
const fn public_route(
    method: RouteMethod,
    pattern: PathPattern,
    body: BodyPolicy,
    handler: RouteHandlerId,
    doc: &'static str,
) -> Route {
    Route {
        method,
        pattern,
        authz: RouteAuthz::Public,
        cors: CorsPosture::Public,
        body,
        handler,
        doc,
        tunnel: None,
    }
}

/// Federation-surface rows: the IAM operation delegates to
/// `federation_http_operation` (see `RouteAuthz::PeerFederation`).
const fn federation_route(
    method: RouteMethod,
    pattern: PathPattern,
    body: BodyPolicy,
    handler: RouteHandlerId,
    doc: &'static str,
) -> Route {
    Route {
        method,
        pattern,
        authz: RouteAuthz::PeerFederation,
        cors: CorsPosture::OwnOrigin,
        body,
        handler,
        doc,
        tunnel: None,
    }
}

/// The route table. **Match order is declaration order** — first match
/// wins, and the no-shadowing invariant test keeps every row reachable.
/// Keep `Exact`/`Segments` rows of a family before an `Under` row of the
/// same base.
pub(crate) static ROUTES: &[Route] = &[
    // ── Filesystem (scoped by authorize_http_filesystem_access; the GET
    //    trio is additionally pre-gated by peer_filesystem_query_request).
    //    First family carrying the tunnel column: the datachannel twins'
    //    IAM operations derive from these rows (their legacy
    //    CONTROL_ONLY_METHODS entries are gone).
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/fs/stat"),
        PeerOperation::FilesystemRead,
        BodyPolicy::None,
        RouteHandlerId::FsStat,
        "Stat a filesystem path (scope-checked)",
    )
    .with_tunnel(tunnel_method("api_fs_stat")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/fs/list"),
        PeerOperation::FilesystemRead,
        BodyPolicy::None,
        RouteHandlerId::FsList,
        "List a directory (scope-checked)",
    )
    .with_tunnel(tunnel_method("api_fs_list")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/fs/read"),
        PeerOperation::FilesystemRead,
        BodyPolicy::None,
        RouteHandlerId::FsRead,
        "Read file bytes (scope-checked; supports byte ranges)",
    )
    .with_tunnel(tunnel_method("api_fs_read")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/fs/mkdir"),
        PeerOperation::FilesystemWrite,
        BodyPolicy::Default,
        RouteHandlerId::FsMkdir,
        "Create a directory (scope-checked)",
    )
    .with_tunnel(tunnel_method("api_fs_mkdir")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/fs/write"),
        PeerOperation::FilesystemWrite,
        // The JSON envelope adds base64/escaping overhead on top of the
        // content cap apply_dashboard_fs_write enforces; allow half again.
        BodyPolicy::Capped(
            crate::web_gateway::UPLOAD_MAX_BYTES + crate::web_gateway::UPLOAD_MAX_BYTES / 2,
        ),
        RouteHandlerId::FsWrite,
        "Write file bytes (scope-checked; sha256-guarded overwrite)",
    )
    // Tunnel-side the content may also ride upload frames (the JSON
    // `content_b64` / upload-frame asymmetry is preserved by design).
    .with_tunnel(tunnel_uploadable("api_fs_write")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/fs/rename"),
        PeerOperation::FilesystemWrite,
        BodyPolicy::Default,
        RouteHandlerId::FsRename,
        "Move/rename a file or directory (scope-checked)",
    )
    .with_tunnel(tunnel_method("api_fs_rename")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/fs/delete"),
        PeerOperation::FilesystemWrite,
        BodyPolicy::Default,
        RouteHandlerId::FsDelete,
        "Delete a file or directory (scope-checked)",
    )
    .with_tunnel(tunnel_method("api_fs_delete")),
    // ── Transfer jobs (design §4, task #6): the resumable jobs protocol
    //    — create / capped chunk appends / commit / ranged download —
    //    over direct HTTP, twinning the datachannel api_transfer_*
    //    methods onto the same neutral cores. Create scope-checks its
    //    target path (both kinds); the job-addressed rows carry no path
    //    of their own — scope-restricted callers are re-checked against
    //    the resolved job's real filesystem path (and the list row is
    //    scope-filtered), through the same shared helper the tunnel
    //    authorizer uses (`web_gateway::check_scoped_transfer_job`).
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/transfers"),
        PeerOperation::FilesystemRead,
        BodyPolicy::None,
        RouteHandlerId::TransferJobs,
        "List transfer jobs, newest first (`?id=` filters by job id or resume token)",
    )
    .with_tunnel(tunnel_method("api_transfer_jobs")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/transfers"),
        PeerOperation::FilesystemWrite,
        BodyPolicy::Default,
        RouteHandlerId::TransferJobCreate,
        "Create a transfer job (kind download|upload, path/destination, name, \
         total_size, sha256, conflict; fs-scope-checked on the target path)",
    )
    .with_tunnel(tunnel_method("api_transfer_job_create")),
    // The chunk row streams its raw body into the same SpooledBody spool
    // the tunnel's upload frames fill, capped per request at
    // TRANSFER_HTTP_CHUNK_MAX_BYTES (resumability makes small chunks
    // cheap). The tunnel twin may also (and in practice only does)
    // arrive as upload frames.
    op_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/transfers",
            &[SegmentSpec::Capture("id"), SegmentSpec::Literal("chunk")],
        ),
        PeerOperation::FilesystemWrite,
        BodyPolicy::Streaming,
        RouteHandlerId::TransferUploadChunk,
        "Append one raw-body chunk to an upload job (`?offset=`; \u{2264} 32 MiB per chunk)",
    )
    .with_tunnel(tunnel_uploadable("api_transfer_upload_chunk")),
    op_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/transfers",
            &[SegmentSpec::Capture("id"), SegmentSpec::Literal("commit")],
        ),
        PeerOperation::FilesystemWrite,
        BodyPolicy::Default,
        RouteHandlerId::TransferUploadCommit,
        "Verify (size + declared sha256) and atomically rename a finished upload into place",
    )
    .with_tunnel(tunnel_method("api_transfer_upload_commit")),
    op_route(
        RouteMethod::Delete,
        PathPattern::Segments("/api/transfers", &[SegmentSpec::Capture("id")]),
        PeerOperation::FilesystemWrite,
        BodyPolicy::None,
        RouteHandlerId::TransferJobDelete,
        "Delete a transfer job (cancels partials; removes managed artifacts)",
    )
    .with_tunnel(tunnel_method("api_transfer_job_delete")),
    op_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/transfers",
            &[SegmentSpec::Capture("id"), SegmentSpec::Literal("delete")],
        ),
        PeerOperation::FilesystemWrite,
        BodyPolicy::None,
        RouteHandlerId::TransferJobDelete,
        "Delete a transfer job (WKWebView POST fallback)",
    ),
    op_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/transfers",
            &[SegmentSpec::Capture("id"), SegmentSpec::Literal("download")],
        ),
        PeerOperation::FilesystemRead,
        BodyPolicy::None,
        RouteHandlerId::TransferDownloadRead,
        "Read download-job bytes (`?offset=&length=` or `Range` \u{2192} 206; \
         resume metadata echoed as X-Transfer-* headers, X-Content-Sha256 on full reads)",
    )
    .with_tunnel(tunnel_method("api_transfer_download_read")),
    // The legacy spelling predates the /api namespace, but this endpoint
    // spends a daemon-held provider credential. Keep its IAM and own-origin
    // posture in the same route table as the rest of the dashboard API.
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/session"),
        PeerOperation::CredentialsManage,
        BodyPolicy::None,
        RouteHandlerId::SessionToken,
        "Mint an ephemeral Gemini Live / OpenAI Realtime token from a daemon-held provider credential",
    ),
    // ── Current-session routes (exact/subtree rows precede the generic
    //    session sub-router rows below).
    op_route(
        RouteMethod::Get,
        PathPattern::Under("/api/session/current/changes"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::SessionCurrentChanges,
        "List the session's changed files, or the unified diff for one file (subpath)",
    )
    .with_tunnel(tunnel_method("api_session_current_changes")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/session/current/history"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::CurrentHistory,
        "Serialized rollback History for the current session",
    )
    .with_tunnel(tunnel_method("api_session_current_history")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/session/current/rollback"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::CurrentRollback,
        "Roll the current session back to a round (optionally reverting files)",
    )
    .with_tunnel(tunnel_method("api_session_current_rollback")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/agenda"),
        PeerOperation::AgendaRead,
        BodyPolicy::None,
        RouteHandlerId::AgendaList,
        "Agenda ledger snapshot: items (oldest first) plus status counts",
    )
    .with_tunnel(tunnel_method("api_agenda_list")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/agenda/op"),
        PeerOperation::AgendaWrite,
        BodyPolicy::Default,
        RouteHandlerId::AgendaOp,
        "Apply one agenda command (add, answer, patch, transitions, or scheduled-session propose/approve/revoke)",
    )
    .with_tunnel(tunnel_method("api_agenda_op")),
    // Reminder delivery policy is owner policy, not agenda authorship:
    // it rides the Settings operation (quiet hours and urgency decide how
    // loudly the daemon speaks — the same class as its other knobs), so
    // an agenda.write holder cannot raise its own reminder's loudness.
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/agenda/reminders/policy"),
        PeerOperation::Settings,
        BodyPolicy::Default,
        RouteHandlerId::AgendaReminderPolicy,
        "Merge-patch the agenda reminder policy (quiet hours, urgency, per-item overrides)",
    )
    .with_tunnel(tunnel_method("api_agenda_reminder_policy")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/memory/search"),
        PeerOperation::MemoryRead,
        BodyPolicy::None,
        RouteHandlerId::MemorySearch,
        "Bounded Memory claim search (q, limit, candidates); results carry derived status",
    )
    .with_tunnel(tunnel_method("api_memory_search")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/memory/claim"),
        PeerOperation::MemoryRead,
        BodyPolicy::None,
        RouteHandlerId::MemoryClaim,
        "Read one Memory claim by id prefix (id); status derived at read time",
    )
    .with_tunnel(tunnel_method("api_memory_claim")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/memory/propose"),
        PeerOperation::MemoryWrite,
        BodyPolicy::Default,
        RouteHandlerId::MemoryPropose,
        "Propose one Memory claim (candidate lane; ephemeral plane in P1.1)",
    )
    .with_tunnel(tunnel_method("api_memory_propose")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/session/current/redo"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::CurrentRedo,
        "Redo the last rolled-back round",
    )
    .with_tunnel(tunnel_method("api_session_current_redo")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/session/current/prune"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::CurrentPrune,
        "Prune rollback state for the current session",
    )
    .with_tunnel(tunnel_method("api_session_current_prune")),
    // POST-shaped read (the body carries output ids; nothing is written).
    // Manage-gated only because the whole current/* family deliberately
    // is (see the sub-router comment below), not because it mutates.
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/session/current/agent-output"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::CurrentAgentOutput,
        "Fetch the current session's persisted agent output by id (POST-shaped read)",
    )
    .with_tunnel(tunnel_method("api_session_current_agent_output")),
    // The staged-upload POST's datachannel twin arrives only as upload
    // frames (upload_start/chunk/end), never on the request lane.
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/session/current/uploads"),
        PeerOperation::SessionManage,
        BodyPolicy::Streaming,
        RouteHandlerId::CurrentUploadsPost,
        "Upload a file attachment (raw streamed body; name/destination in query)",
    )
    .with_tunnel(tunnel_upload_only("api_session_current_upload")),
    // The uploads GET family: list (exact) and raw-fetch (segments) rows
    // carved ahead of the Under catch-all so each carries its
    // datachannel twin; all three share the handler, which routes by
    // path (the catch-all keeps the handler-owned JSON 404 for unknown
    // upload subpaths).
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/session/current/uploads"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::CurrentUploadsGet,
        "List uploads for the current session",
    )
    .with_tunnel(tunnel_method("api_session_current_uploads")),
    // Capture named `id`: the facade's HTTP adapter lifts template
    // segments by name and the raw fetch's callers pass `{ id }` (the
    // tunnel's primary alias) — pinned by the descriptor parity test.
    op_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/session/current/uploads",
            &[SegmentSpec::Capture("id"), SegmentSpec::Literal("raw")],
        ),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::CurrentUploadsGet,
        "Fetch one upload's raw bytes (attachment; MIME sniffing disabled)",
    )
    .with_tunnel(tunnel_method("api_session_current_upload_raw")),
    op_route(
        RouteMethod::Get,
        PathPattern::Under("/api/session/current/uploads"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::CurrentUploadsGet,
        "Unknown upload subpaths (handler-owned JSON 404)",
    ),
    op_route(
        RouteMethod::Delete,
        PathPattern::Segments(
            "/api/session/current/uploads",
            &[SegmentSpec::Capture("upload_id")],
        ),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::CurrentUploadDelete,
        "Delete one upload (file + sidecar)",
    )
    .with_tunnel(tunnel_method("api_session_current_upload_delete")),
    // ── Session deletion. Five accepted wire shapes (native DELETE plus
    //    the WKWebView POST fallback with a literal `delete` suffix); one
    //    handler serves all of them by filtering the `delete` segment.
    //    The datachannel twin rides the canonical shape below (one tunnel
    //    name per row; all five rows share the operation and handler).
    op_route(
        RouteMethod::Delete,
        PathPattern::Segments("/api/session", &[SegmentSpec::Capture("id")]),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::SessionDelete,
        "Delete a session's data",
    )
    .with_tunnel(tunnel_method("api_session_delete")),
    op_route(
        RouteMethod::Delete,
        PathPattern::Segments(
            "/api/session",
            &[SegmentSpec::Capture("id"), SegmentSpec::Capture("target")],
        ),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::SessionDelete,
        "Delete one data kind for a session (recordings, frames, …)",
    ),
    op_route(
        RouteMethod::Delete,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Capture("target"),
                SegmentSpec::Literal("delete"),
            ],
        ),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::SessionDelete,
        "Delete one data kind for a session (suffix form)",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/session",
            &[SegmentSpec::Capture("id"), SegmentSpec::Literal("delete")],
        ),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::SessionDelete,
        "Delete a session's data (POST fallback for WKWebView)",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Capture("target"),
                SegmentSpec::Literal("delete"),
            ],
        ),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::SessionDelete,
        "Delete one data kind for a session (POST fallback)",
    ),
    // POST-shaped read: the body carries output ids and the handler
    // (`session_agent_output_api_response`) only fetches persisted
    // stdout/stderr chunks back out of the session's log — nothing is
    // appended, so it is inspect-grade like every other by-id session
    // read. The legacy verb-shaped classifier (POST under /api/session ⇒
    // manage) mis-tagged it and diverged from the tunnel twin — now the
    // twin is this row's tunnel column and its operation derives from
    // here (`formerly_divergent_twins_gate_identically_on_both_lanes`
    // stays as the end-to-end assertion).
    op_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Literal("agent-output"),
            ],
        ),
        PeerOperation::SessionInspect,
        BodyPolicy::Default,
        RouteHandlerId::SessionAgentOutput,
        "Fetch a session's persisted agent output by id (POST-shaped read)",
    )
    .with_tunnel(tunnel_method("api_session_agent_output")),
    // Declared ahead of the sub-router group: the `{id}/fork-points` leaf
    // must win before the group's `Under("/api/session")` catch-all, and
    // shared-handler groups must stay contiguous.
    op_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Literal("fork-points"),
            ],
        ),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionForkPoints,
        "Unified fork-point catalog for a session (anchors + eligibility, backend-tagged)",
    )
    .with_tunnel(tunnel_method("api_session_fork_points")),
    // ── Background-task inspector (supervised Claude Code): the list and
    //    the read-only output peek. Like fork-points, declared ahead of
    //    the sub-router group so the leaves beat its catch-alls. The
    //    output route serves paths exclusively from the daemon-side task
    //    registry — the client names a task id, never a path.
    op_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Literal("background-tasks"),
            ],
        ),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionBackgroundTasks,
        "Background tasks a supervised session announced (id, description, status, output availability)",
    )
    .with_tunnel(tunnel_method("api_session_background_tasks")),
    op_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Literal("background-tasks"),
                SegmentSpec::Capture("task"),
                SegmentSpec::Literal("output"),
            ],
        ),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionBackgroundTaskOutput,
        "Tail of one background task's output file (tail_kb query, capped; registry-resolved path)",
    )
    .with_tunnel(tunnel_method("api_session_background_task_output")),
    // ── Session detail + artifacts sub-router. Method-explicit ports of
    //    the method-blind legacy catch-all: the classifier's historical
    //    split (current/* manage-gated on every method; {id} inspect on
    //    GET, manage on writes) is preserved row by row.
    op_route(
        RouteMethod::Get,
        PathPattern::Under("/api/session/current"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "Current-session detail and artifact sub-routes",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Under("/api/session/current"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "Current-session detail sub-routes (POST fallback callers)",
    ),
    // Converted read leaves of the method-blind catch-all, carved as
    // method-explicit rows so each can carry its datachannel twin
    // (transport-unification S4a; one tunnel name per row). Same
    // operation and shared handler as the Under rows they were served by
    // — classification-preserving; the generic rows below keep the
    // unconverted artifact leaves and the POST-fallback callers.
    op_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Literal("context-snapshot"),
            ],
        ),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "Replay one archived context snapshot (file/request_id/request_index/ts selector)",
    )
    .with_tunnel(tunnel_method("api_session_context_snapshot")),
    op_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/session",
            &[SegmentSpec::Capture("id"), SegmentSpec::Literal("report")],
        ),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "Session report zip (text artifacts; id=current targets the live session)",
    )
    .with_tunnel(tunnel_method("api_session_report")),
    op_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Literal("recordings"),
            ],
        ),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "List a session's recording streams",
    )
    .with_tunnel(tunnel_method("api_session_recordings")),
    op_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Literal("recordings"),
                SegmentSpec::Capture("stream"),
                SegmentSpec::Capture("asset"),
            ],
        ),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "Recording assets: segments listing, playlist.m3u8, or a segment file",
    )
    .with_tunnel(tunnel_method("api_session_recording_asset")),
    op_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Literal("frames"),
                SegmentSpec::Capture("filename"),
            ],
        ),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "Session frame image asset",
    )
    .with_tunnel(tunnel_method("api_session_frame_asset")),
    op_route(
        RouteMethod::Get,
        PathPattern::Segments("/api/session", &[SegmentSpec::Capture("id")]),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "Session detail (paged replay entries; limit/before/source)",
    )
    .with_tunnel(tunnel_method("api_session_detail")),
    op_route(
        RouteMethod::Get,
        PathPattern::Under("/api/session"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "Session artifact sub-routes: recordings (+segments/playlist), report zip, frames",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Under("/api/session"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "Session detail sub-routes (POST fallback callers)",
    ),
    // ── Managed-context inspection.
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/managed-context/anchors"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::McAnchors,
        "Managed-context anchor catalog",
    )
    .with_tunnel(tunnel_method("api_managed_context_anchors")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/managed-context/records"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::McRecords,
        "Managed-context record index",
    )
    .with_tunnel(tunnel_method("api_managed_context_records")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/managed-context/fission"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::McFission,
        "Managed-context fission state",
    )
    .with_tunnel(tunnel_method("api_managed_context_fission")),
    // ── Worktrees.
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/worktrees/inspect"),
        PeerOperation::SessionInspect,
        BodyPolicy::Default,
        RouteHandlerId::WorktreesInspect,
        "Inspect one worktree (branch, ahead/behind, dirty state)",
    )
    .with_tunnel(tunnel_method("api_worktrees_inspect")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/worktrees/remove"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::WorktreesRemove,
        "Remove a worktree from the inventory",
    )
    .with_tunnel(tunnel_method("api_worktrees_remove")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/worktrees/clean"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::WorktreesClean,
        "Delete a worktree's Cargo target/ dir (CACHEDIR.TAG-gated) to reclaim disk, keeping the checkout",
    )
    .with_tunnel(tunnel_method("api_worktrees_clean")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/worktrees/merge"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::WorktreesMerge,
        "Merge a session's linked worktree branch into its base checkout, then remove the checkout",
    )
    .with_tunnel(tunnel_method("api_worktrees_merge")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/worktrees/scan"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::WorktreesScan,
        "Rescan the worktree inventory (refreshes the cache)",
    )
    .with_tunnel(tunnel_method("api_worktrees_scan")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/worktrees"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::WorktreesList,
        "Cached worktree inventory",
    )
    .with_tunnel(tunnel_method("api_worktrees")),
    // ── Session listing. The stream/search rows close a historical gap:
    //    dispatch served them but the hand classifier never gated them
    //    for browser principals (peers were already SessionInspect-gated
    //    by federation_http_operation). Declaring the operation here is
    //    the fail-closed fix; the differential test allowlists it.
    fleet_or_loopback_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/sessions/stream"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionsStream,
        "NDJSON stream of the session list",
    )
    .with_tunnel(tunnel_method("api_sessions_stream")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/sessions/search"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionsSearch,
        "Search sessions (q, source, mode, project filters)",
    )
    .with_tunnel(tunnel_method("api_sessions_search")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/sessions/message-search"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionsMessageSearch,
        "Message-lane search over the shard index (q, source, superseded, subagents, cursor)",
    )
    .with_tunnel(tunnel_method("api_sessions_message_search")),
    fleet_or_loopback_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/sessions"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionsList,
        "List sessions (id filter, limit, usage view; fleet/loopback CORS echo for the multi-daemon Stats tab)",
    )
    .with_tunnel(tunnel_method("api_sessions")),
    // ── Settings / info endpoints. Ported method-blind from the legacy
    //    chain as `Any` rows, then tightened to their real methods: an
    //    undeclared method on a declared path now gets the dispatch-level
    //    405-with-Allow instead of reaching a read handler.
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/project-root"),
        PeerOperation::Settings,
        BodyPolicy::None,
        RouteHandlerId::ProjectRoot,
        "Project root path this daemon serves",
    )
    .with_tunnel(tunnel_method("api_project_root")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/settings"),
        PeerOperation::Settings,
        BodyPolicy::Default,
        RouteHandlerId::SettingsPost,
        "Update runtime settings",
    )
    .with_tunnel(tunnel_method("api_settings_save")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/settings"),
        PeerOperation::Settings,
        BodyPolicy::None,
        RouteHandlerId::SettingsGet,
        "Current runtime settings",
    )
    .with_tunnel(tunnel_method("api_settings")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/api-keys"),
        PeerOperation::Settings,
        BodyPolicy::Default,
        RouteHandlerId::ApiKeysPost,
        "Store provider API keys in the project .env",
    )
    .with_tunnel(tunnel_method("api_api_keys_save")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/api-key-status"),
        PeerOperation::Settings,
        BodyPolicy::None,
        RouteHandlerId::ApiKeyStatus,
        "Which provider keys are configured (presence only)",
    )
    .with_tunnel(tunnel_method("api_key_status")),
    // ── Claude sign-in ceremony (claude_auth_ceremony.rs): the dashboard
    //    walks the owner through `claude auth login` on a daemon-private
    //    PTY. Gated on the credential-custody operation — the same class
    //    as vault leases and egress, which no peer profile (peer-root
    //    included) and no scoped default carries — and the handlers
    //    additionally hard-refuse hosted-provenance clients and custody-
    //    managed (leased / client-egress) daemons.
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/claude-auth/start"),
        PeerOperation::CredentialsManage,
        BodyPolicy::Capped(CLAUDE_AUTH_START_BODY_CAP_BYTES),
        RouteHandlerId::ClaudeAuthStart,
        "Start the Claude sign-in ceremony (`claude auth login` on a daemon-private PTY)",
    )
    .with_tunnel(tunnel_method("api_claude_auth_start")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/claude-auth/status"),
        PeerOperation::CredentialsManage,
        BodyPolicy::None,
        RouteHandlerId::ClaudeAuthStatus,
        "Claude sign-in ceremony state (validated sign-in URL; account info on success)",
    )
    .with_tunnel(tunnel_method("api_claude_auth_status")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/claude-auth/code"),
        PeerOperation::CredentialsManage,
        BodyPolicy::Capped(CLAUDE_AUTH_CODE_BODY_CAP_BYTES),
        RouteHandlerId::ClaudeAuthCode,
        "Submit the pasted authorization code to the Claude sign-in ceremony",
    )
    .with_tunnel(tunnel_method("api_claude_auth_code")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/claude-auth/cancel"),
        PeerOperation::CredentialsManage,
        BodyPolicy::None,
        RouteHandlerId::ClaudeAuthCancel,
        "Cancel the Claude sign-in ceremony (non-destructive; prior login keeps working)",
    )
    .with_tunnel(tunnel_method("api_claude_auth_cancel")),
    // ── Codex sign-in ceremony (codex_auth_ceremony.rs): the dashboard
    //    walks the owner through `codex login --device-auth` on a
    //    daemon-private PTY — same custody class and gates as the Claude
    //    family above (credentials.manage + hosted-provenance +
    //    lease/egress refusals), one ceremony at a time across both
    //    providers. No code-submission leaf: the owner types the
    //    one-time code into OpenAI's page, never back into the daemon.
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/codex-auth/start"),
        PeerOperation::CredentialsManage,
        BodyPolicy::Capped(CODEX_AUTH_START_BODY_CAP_BYTES),
        RouteHandlerId::CodexAuthStart,
        "Start the Codex sign-in ceremony (`codex login --device-auth` on a daemon-private PTY)",
    )
    .with_tunnel(tunnel_method("api_codex_auth_start")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/codex-auth/status"),
        PeerOperation::CredentialsManage,
        BodyPolicy::None,
        RouteHandlerId::CodexAuthStatus,
        "Codex sign-in ceremony state (verification URL + one-time code; account info on success)",
    )
    .with_tunnel(tunnel_method("api_codex_auth_status")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/codex-auth/cancel"),
        PeerOperation::CredentialsManage,
        BodyPolicy::None,
        RouteHandlerId::CodexAuthCancel,
        "Cancel the Codex sign-in ceremony (non-destructive; prior login keeps working)",
    )
    .with_tunnel(tunnel_method("api_codex_auth_cancel")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/external-agents"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::ExternalAgents,
        "Detected external coding agents (codex, claude)",
    )
    .with_tunnel(tunnel_method("api_external_agents")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/diagnostics/visual-freshness"),
        PeerOperation::DisplayInput,
        BodyPolicy::Capped(DIAGNOSTICS_BODY_CAP_BYTES),
        RouteHandlerId::DiagnosticsVisualFreshness,
        "Visual-freshness diagnostics transcript sink (NDJSON body)",
    )
    .with_tunnel(tunnel_method("api_diagnostics_visual_freshness")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/displays"),
        PeerOperation::DisplayView,
        BodyPolicy::None,
        RouteHandlerId::Displays,
        "Enumerate active displays",
    )
    .with_tunnel(tunnel_method("api_displays")),
    // ── Public doorbell + signed org endpoints. The payload's own
    //    signature/shape is the authority; RouteAuthz::Public makes the
    //    no-IAM-gate decision explicit (these paths are also exempted by
    //    the mTLS and origin gates, which match on the same constants).
    //    The doorbell stays a method-`Any` catch-all row on purpose:
    //    its handler routes POST-knock vs GET-poll internally and answers
    //    garbage subpaths with a public-CORS JSON 404 — per-shape rows
    //    would drop those to the SPA-shell fallback instead.
    public_route(
        RouteMethod::Any,
        PathPattern::Under(crate::peer::access_request::PUBLIC_REQUEST_PATH),
        BodyPolicy::Streaming,
        RouteHandlerId::Doorbell,
        "Peer access-request doorbell: knock (POST, size-capped) or poll one request's status (GET subpath)",
    ),
    public_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/hosted-control/bootstrap"),
        BodyPolicy::None,
        RouteHandlerId::HostedControlBootstrap,
        "Hosted-control doorbell bootstrap (dark unless enabled; no authority)",
    ),
    public_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/hosted-control/requests"),
        BodyPolicy::Default,
        RouteHandlerId::HostedControlRequestCreate,
        "Submit a bounded hosted-control lease request to daemon-local IAM",
    ),
    public_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/hosted-control/requests/poll"),
        BodyPolicy::Default,
        RouteHandlerId::HostedControlRequestPoll,
        "Poll one hosted-control request with proof by its browser key",
    ),
    public_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/hosted-control/anchor-decisions"),
        BodyPolicy::Default,
        RouteHandlerId::HostedControlAnchorDecision,
        "Present a signed application-anchor decision document",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/hosted-control/ws-ticket"),
        PeerOperation::PresenceRead,
        BodyPolicy::None,
        RouteHandlerId::HostedControlWsTicket,
        "Mint one short-lived, single-use WebSocket ticket from a proved hosted lease",
    ),
    public_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/org-grants"),
        BodyPolicy::Capped(crate::access::org::MAX_ORG_GRANT_DOC_BYTES),
        RouteHandlerId::AccessOrgGrantPresent,
        "Present a signed org grant document (verified against locally trusted org keys)",
    )
    .with_tunnel(tunnel_method_with_op(
        "api_access_org_present",
        PeerOperation::AccessInspect,
        "Public doorbell on HTTP (the signed document is the authorization); the tunnel requires a bound session — stricter on purpose",
    )),
    public_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/access/orgs",
            &[
                SegmentSpec::Capture("org_handle"),
                SegmentSpec::Literal("revocations"),
            ],
        ),
        BodyPolicy::None,
        RouteHandlerId::AccessOrgRevocations,
        "Org revocation list (ORL) for a trusted org",
    )
    .with_tunnel(tunnel_method_with_op(
        "api_access_org_orl",
        PeerOperation::AccessInspect,
        "Public read on HTTP; the tunnel requires a bound session — stricter on purpose",
    )),
    public_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/orgs/revocations/apply"),
        BodyPolicy::Capped(crate::access::org::MAX_ORG_ORL_BYTES),
        RouteHandlerId::AccessOrgApplyRenew,
        "Apply a signed org revocation list",
    )
    .with_tunnel(tunnel_method_with_op(
        "api_access_org_orl_apply",
        PeerOperation::PresenceRead,
        "Public doorbell on HTTP (the root signature is the authority); any bound session may courier one through the tunnel",
    )),
    public_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/org-grants/renew"),
        BodyPolicy::Capped(crate::access::org::MAX_ORG_GRANT_DOC_BYTES),
        RouteHandlerId::AccessOrgApplyRenew,
        "Renew an org grant document (signed payload)",
    )
    .with_tunnel(tunnel_method_with_op(
        "api_access_org_renew",
        PeerOperation::AccessInspect,
        "Public doorbell on HTTP (the signed document is the authorization); the tunnel requires a bound session — stricter on purpose",
    )),
    // ── Access administration (fleet-CORS where the anchor page needs
    //    to read responses; own-origin otherwise).
    fleet_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/iam/user-client-grants"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessIamGrants,
        "Upsert a user-client grant",
    )
    .with_tunnel(tunnel_method("api_access_iam_upsert_user_client_grant")),
    fleet_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/iam/grants/update"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessIamGrants,
        "Update an IAM grant",
    )
    .with_tunnel(tunnel_method("api_access_iam_update_grant")),
    fleet_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/orgs/trust"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Trust an org root key on this daemon",
    )
    .with_tunnel(tunnel_method("api_access_org_trust")),
    fleet_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/orgs/revoke"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Withdraw trust in an org root key",
    )
    .with_tunnel(tunnel_method("api_access_org_revoke")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/org-grants/issue"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Issue an org grant (org root/issuer key on this daemon)",
    )
    .with_tunnel(tunnel_method("api_access_org_issue")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/org-grants/revoke-member"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Revoke an org member (appends to the ORL)",
    )
    .with_tunnel(tunnel_method("api_access_org_revoke_member")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/org-grants/issuers/init"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Initialize an org issuer key",
    )
    .with_tunnel(tunnel_method("api_access_org_issuer_init")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/org-grants/issuers/delegate"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Delegate to an org issuer",
    )
    .with_tunnel(tunnel_method("api_access_org_issuer_delegate")),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/org-grants/issuers/install"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Install a delegated org issuer key",
    )
    .with_tunnel(tunnel_method("api_access_org_issuer_install")),
    fleet_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/enrollment-requests/decide"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessEnrollmentDecide,
        "Staged decision API; the default product has no queue writer",
    )
    .with_tunnel(tunnel_method("api_access_enrollment_decide")),
    fleet_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/access/enrollment-requests"),
        PeerOperation::AccessInspect,
        BodyPolicy::None,
        RouteHandlerId::AccessEnrollmentRequests,
        "Staged enrollment capability and normally empty queue",
    )
    .with_tunnel(tunnel_method("api_access_enrollment_requests")),
    fleet_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/access/iam/state"),
        PeerOperation::AccessInspect,
        BodyPolicy::None,
        RouteHandlerId::AccessIamState,
        "Local IAM state (roles, grants, bindings)",
    )
    .with_tunnel(tunnel_method("api_access_iam_state")),
    fleet_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/access/overview"),
        PeerOperation::AccessInspect,
        BodyPolicy::None,
        RouteHandlerId::AccessOverview,
        "Access overview for the calling principal",
    )
    .with_tunnel(tunnel_method("api_access_overview")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/access/hosted-control"),
        PeerOperation::AccessManage,
        BodyPolicy::None,
        RouteHandlerId::HostedControlManagement,
        "Hosted-control policy, pending request, active lease, and signed-app anchor state",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Under("/api/access/hosted-control"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::HostedControlManagement,
        "Decide requests, revoke leases, change policy, or mark hosted-eligible sessions",
    ),
    // ── Connect rendezvous administration. Status is inspect-grade but
    //    never carries the one-time claim code; revealing the code is its own
    //    manage-gated route so the sensitive one-time-code response has the
    //    strictest gate.
    fleet_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/access/connect/status"),
        PeerOperation::AccessInspect,
        BodyPolicy::None,
        RouteHandlerId::AccessConnectStatus,
        "Connect rendezvous status (discovery-link state and provenance; no claim code)",
    )
    .with_tunnel(tunnel_method("api_access_connect_status")),
    fleet_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/access/connect/claim-code"),
        PeerOperation::AccessManage,
        BodyPolicy::None,
        RouteHandlerId::AccessConnectClaimCode,
        "Reveal the current one-time twelve-word claim code (unlinked daemons only)",
    )
    .with_tunnel(tunnel_method("api_access_connect_claim_code")),
    fleet_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/connect/config"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessConnectConfig,
        "Enable/disable the Connect client (persists to intendant.toml, applies live)",
    )
    .with_tunnel(tunnel_method("api_access_connect_config")),
    fleet_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/connect/unclaim"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessConnectUnclaim,
        "Unlink this daemon's discovery record from its Connect account (daemon-signed)",
    )
    .with_tunnel(tunnel_method("api_access_connect_unclaim")),
    fleet_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/tier"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessTierSettings,
        "Set this daemon's trust tier label (integrated/disposable; null clears)",
    )
    .with_tunnel(tunnel_method("api_access_set_tier")),
    fleet_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/access/fleet-cert/request"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessFleetCertRequest,
        "Request a fleet certificate (publish addresses, run the ACME DNS-01 order; async start)",
    )
    .with_tunnel(tunnel_method("api_fleet_cert_request")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/dashboard/targets"),
        PeerOperation::AccessInspect,
        BodyPolicy::None,
        RouteHandlerId::DashboardTargets,
        "Dashboard target list (this daemon + connected peers)",
    )
    .with_tunnel(tunnel_method("api_dashboard_targets")),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/dashboard/tabs"),
        PeerOperation::AccessInspect,
        BodyPolicy::None,
        RouteHandlerId::DashboardTabs,
        "Live dashboard connections (open tabs) with voice/input-authority holders",
    )
    .with_tunnel(tunnel_method("api_dashboard_tabs")),
    // ── Federation surface: registry, pairing, quick controls,
    //    capability routing. Carved per-leaf rows (S7) declare each
    //    leaf's datachannel twin — the twin's IAM operation derives from
    //    federation_http_operation on the row's canonical leaf, the same
    //    ladder the HTTP gate consults — while every row shares the one
    //    sub-router handler and the same BodyPolicy the legacy catch-all
    //    carried, so dispatch behavior is unchanged row by row. The
    //    method-`Any` catch-all row stays LAST on purpose: unknown
    //    subpaths and undeclared methods keep the handler's JSON 404/405
    //    shapes instead of dropping to the SPA-shell fallback (per-leaf
    //    rows alone would either lose that or need a catch-all that
    //    neuters them — this carve keeps both).
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[SegmentSpec::Literal("pairing"), SegmentSpec::Literal("invite")],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Issue a peer-scoped mTLS pairing invite",
    )
    .with_tunnel(tunnel_method("api_peer_pairing_invite")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Literal("pairing"),
                SegmentSpec::Literal("request-access"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Start an outgoing access request against a remote daemon's doorbell",
    )
    .with_tunnel(tunnel_method("api_peer_pairing_request_access")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Literal("pairing"),
                SegmentSpec::Literal("request-access"),
                SegmentSpec::Literal("poll"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Poll an outgoing access request (installs the approved identity)",
    )
    .with_tunnel(tunnel_method("api_peer_pairing_request_access_poll")),
    federation_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Literal("pairing"),
                SegmentSpec::Literal("requests"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "List pending/decided peer access requests",
    )
    .with_tunnel(tunnel_method("api_peer_pairing_requests")),
    federation_route(
        RouteMethod::Get,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Literal("pairing"),
                SegmentSpec::Literal("identities"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "List approved/revoked peer identities",
    )
    .with_tunnel(tunnel_method("api_peer_pairing_identities")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Literal("pairing"),
                SegmentSpec::Literal("identities"),
                SegmentSpec::Literal("revoke"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Revoke a peer identity",
    )
    .with_tunnel(tunnel_method("api_peer_pairing_identity_revoke")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Literal("pairing"),
                SegmentSpec::Literal("requests"),
                SegmentSpec::Capture("code"),
                SegmentSpec::Capture("decision"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Decide a pending access request (approve or deny)",
    )
    .with_tunnel(tunnel_method("api_peer_pairing_request_decision")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[SegmentSpec::Literal("pairing"), SegmentSpec::Literal("join")],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Import a pairing invite and register the peer",
    )
    .with_tunnel(tunnel_method("api_peer_pairing_join")),
    federation_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/peers"),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "List registered peers (snapshots)",
    )
    .with_tunnel(tunnel_method("api_peers")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/peers"),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Add a peer by card URL (optionally persisted)",
    )
    .with_tunnel(tunnel_method("api_peer_add")),
    federation_route(
        RouteMethod::Delete,
        PathPattern::Exact("/api/peers"),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Remove a registered peer",
    )
    .with_tunnel(tunnel_method("api_peer_remove")),
    federation_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/peers/eligible"),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "List connected peers satisfying every ?capability= filter",
    )
    .with_tunnel(tunnel_method("api_peer_eligible")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Capture("peer_id"),
                SegmentSpec::Literal("message"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Send a message to a connected peer",
    )
    .with_tunnel(tunnel_method("api_peer_message")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[SegmentSpec::Capture("peer_id"), SegmentSpec::Literal("task")],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Delegate a task to a connected peer",
    )
    .with_tunnel(tunnel_method("api_peer_task")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Capture("peer_id"),
                SegmentSpec::Literal("approval"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Resolve a peer-forwarded approval request",
    )
    .with_tunnel(tunnel_method("api_peer_approval")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Capture("peer_id"),
                SegmentSpec::Literal("webrtc"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Relay display WebRTC signaling to a connected peer",
    )
    .with_tunnel(tunnel_method("api_peer_webrtc_signal")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Capture("peer_id"),
                SegmentSpec::Literal("file-transfer-webrtc"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Relay file-transfer WebRTC signaling to a connected peer",
    )
    .with_tunnel(tunnel_method("api_peer_file_transfer_signal")),
    federation_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/peers",
            &[
                SegmentSpec::Capture("peer_id"),
                SegmentSpec::Literal("dashboard-control-webrtc"),
            ],
        ),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Relay dashboard-control WebRTC signaling to a connected peer",
    )
    .with_tunnel(tunnel_method("api_peer_dashboard_control_signal")),
    federation_route(
        RouteMethod::Any,
        PathPattern::Under("/api/peers"),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Peers sub-router catch-all (handler-owned JSON 404/405 for unknown subpaths and undeclared methods)",
    ),
    // Owner decision 2026-07-11: coordinator routing gates on PeerUse on
    // both transport lanes (the quick-controls doctrine — routing a task
    // through the coordinator dispatches it to a capability-matched peer
    // under this daemon's peer identity, the same action class as
    // POST /api/peers/{id}/task). The tunnel twin derives from the
    // federation ladder like every other twinned method here; this
    // supersedes the previously preserved per-lane split (HTTP: Task,
    // tunnel: PeerManage via a documented op override).
    federation_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/coordinator/route"),
        BodyPolicy::Default,
        RouteHandlerId::CoordinatorRoute,
        "Capability-based task routing through the Coordinator",
    )
    .with_tunnel(tunnel_method("api_coordinator_route")),
    // ── MCP Streamable HTTP (token-bound inside the handler; the
    //    per-tool IAM gate lives in the MCP layer).
    Route {
        method: RouteMethod::Post,
        pattern: PathPattern::Exact("/mcp"),
        authz: RouteAuthz::McpToken,
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::Capped(MCP_BODY_CAP_BYTES),
        handler: RouteHandlerId::McpPost,
        doc: "MCP Streamable HTTP endpoint (JSON-RPC requests + notifications)",
        tunnel: None,
    },
    Route {
        method: RouteMethod::Get,
        pattern: PathPattern::Exact("/mcp"),
        authz: RouteAuthz::McpToken,
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::None,
        handler: RouteHandlerId::McpStream,
        doc: "MCP SSE stream (405: stateless server)",
        tunnel: None,
    },
    Route {
        method: RouteMethod::Delete,
        pattern: PathPattern::Exact("/mcp"),
        authz: RouteAuthz::McpToken,
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::None,
        handler: RouteHandlerId::McpStream,
        doc: "MCP session delete (405: stateless server)",
        tunnel: None,
    },
];

fn segments_match<'p>(
    req_path: &'p str,
    base: &str,
    specs: &[SegmentSpec],
    captures: &mut Vec<&'p str>,
) -> bool {
    let Some(rest) = req_path.strip_prefix(base) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('/') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != specs.len() {
        return false;
    }
    for (part, spec) in parts.iter().zip(specs) {
        if part.is_empty() {
            return false;
        }
        match spec {
            SegmentSpec::Capture(_) => captures.push(part),
            SegmentSpec::Literal(literal) => {
                if part != literal {
                    return false;
                }
            }
            SegmentSpec::OneOf(options) => {
                if !options.iter().any(|option| option == part) {
                    return false;
                }
                captures.push(part);
            }
        }
    }
    true
}

fn pattern_matches<'p>(
    pattern: &PathPattern,
    req_path: &'p str,
    captures: &mut Vec<&'p str>,
) -> bool {
    match pattern {
        PathPattern::Exact(base) => req_path == *base,
        PathPattern::Under(base) => path_is_or_under(req_path, base),
        PathPattern::Segments(base, specs) => segments_match(req_path, base, specs, captures),
    }
}

/// First declared route matching (method, path), with its captured
/// segments (in declaration order of `Capture`/`OneOf` specs).
pub(crate) fn match_route<'p>(
    req_method: &str,
    req_path: &'p str,
) -> Option<(&'static Route, Vec<&'p str>)> {
    for route in ROUTES {
        if !route.method.matches(req_method) {
            continue;
        }
        let mut captures = Vec::new();
        if pattern_matches(&route.pattern, req_path, &mut captures) {
            return Some((route, captures));
        }
    }
    None
}

/// Result of consulting the table for IAM classification.
pub(crate) enum TableClassification {
    /// A declared route matched; carries its IAM operation (`None` for
    /// `Public` and `McpToken` routes, which the pre-dispatch gate must
    /// not evaluate an operation for).
    Matched(Option<PeerOperation>),
    /// No declared route matched — fall back to the residual
    /// hand-written classifier until every family has ported.
    NoMatch,
}

pub(crate) fn classify(req_method: &str, req_path: &str) -> TableClassification {
    match match_route(req_method, req_path) {
        Some((route, _)) => TableClassification::Matched(match route.authz {
            RouteAuthz::Operation(op) => Some(op),
            RouteAuthz::Public | RouteAuthz::McpToken => None,
            RouteAuthz::PeerFederation => {
                crate::peer::access_policy::federation_http_operation(req_method, req_path)
            }
        }),
        None => TableClassification::NoMatch,
    }
}

/// Every (route, tunnel spec) pair, in declaration order — the ROW half
/// of the tunnel method partition. `dashboard_control` unions this with
/// its residue `CONTROL_ONLY_METHODS` (rows first) to build the effective
/// method table; its differential pin test freezes exactly which methods
/// live on which side.
pub(crate) fn tunnel_specs() -> impl Iterator<Item = (&'static Route, &'static TunnelSpec)> {
    ROUTES
        .iter()
        .filter_map(|route| route.tunnel.as_ref().map(|spec| (route, spec)))
}

/// CORS/preflight posture for a path: the first row matching it on any
/// method. The posture-consistency invariant test guarantees every row
/// sharing a path agrees, so "first" is well-defined — and preflight
/// deriving from the same declaration as dispatch is what makes the
/// preflight-looser-than-endpoint bug class unrepresentable.
pub(crate) fn preflight_posture(req_path: &str) -> Option<CorsPosture> {
    ROUTES.iter().find_map(|route| {
        let mut scratch = Vec::new();
        pattern_matches(&route.pattern, req_path, &mut scratch).then_some(route.cors)
    })
}

/// `Access-Control-Allow-Methods` value for a path: the union of
/// declared methods across all rows matching it, plus OPTIONS. `None`
/// when no route matches (callers fall back to the legacy fixed lists).
pub(crate) fn allowed_methods_for_path(req_path: &str) -> Option<String> {
    let (mut get, mut post, mut delete, mut matched) = (false, false, false, false);
    for route in ROUTES {
        let mut scratch = Vec::new();
        if !pattern_matches(&route.pattern, req_path, &mut scratch) {
            continue;
        }
        matched = true;
        match route.method {
            RouteMethod::Get => get = true,
            RouteMethod::Post => post = true,
            RouteMethod::Delete => delete = true,
            RouteMethod::Any => {
                get = true;
                post = true;
                delete = true;
            }
        }
    }
    if !matched {
        return None;
    }
    let mut methods: Vec<&str> = Vec::new();
    if get {
        methods.push("GET");
    }
    if post {
        methods.push("POST");
    }
    if delete {
        methods.push("DELETE");
    }
    methods.push("OPTIONS");
    Some(methods.join(", "))
}

// Live through render_endpoint_docs (see its note on call sites).
#[allow(dead_code)]
fn pattern_doc_label(pattern: &PathPattern) -> String {
    match pattern {
        PathPattern::Exact(base) => (*base).to_string(),
        PathPattern::Under(base) => format!("{base}[/…]"),
        PathPattern::Segments(base, specs) => {
            let mut out = (*base).to_string();
            for spec in *specs {
                out.push('/');
                match spec {
                    SegmentSpec::Capture(name) => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                    SegmentSpec::Literal(literal) => out.push_str(literal),
                    SegmentSpec::OneOf(options) => {
                        out.push('{');
                        out.push_str(&options.join("|"));
                        out.push('}');
                    }
                }
            }
            out
        }
    }
}

// Live through render_endpoint_docs (see its note on call sites).
#[allow(dead_code)]
fn authz_doc_label(authz: &RouteAuthz) -> String {
    match authz {
        RouteAuthz::Operation(op) => format!("{op:?}"),
        RouteAuthz::Public => "public".to_string(),
        RouteAuthz::McpToken => "MCP token".to_string(),
        RouteAuthz::PeerFederation => "federation (per method/path)".to_string(),
    }
}

/// Render the endpoint table for `docs/src/web-dashboard.md`. Phase 3
/// pins the chapter to this output between markers; regeneration happens
/// through the test harness (`cargo test … -- --nocapture` prints it on
/// drift), so its only call sites are tests by design — hence the allow.
#[allow(dead_code)]
pub(crate) fn render_endpoint_docs() -> String {
    let mut out = String::from(
        "| Method | Path | Authorization | CORS | Body | Description |\n\
         |---|---|---|---|---|---|\n",
    );
    for route in ROUTES {
        let cors = match route.cors {
            CorsPosture::OwnOrigin => "own origin",
            CorsPosture::FleetAllowlist => "fleet allowlist",
            CorsPosture::FleetOrLoopback => "fleet or loopback",
            CorsPosture::Public => "public",
        };
        let body = match route.body {
            BodyPolicy::None => "none".to_string(),
            BodyPolicy::Default => "bounded".to_string(),
            BodyPolicy::Capped(cap) => {
                if cap % (1024 * 1024) == 0 {
                    format!("\u{2264} {} MiB", cap / (1024 * 1024))
                } else {
                    format!("\u{2264} {} KiB", cap.div_ceil(1024))
                }
            }
            BodyPolicy::Streaming => "streaming".to_string(),
        };
        out.push_str(&format!(
            "| {} | `{}` | {} | {} | {} | {} |\n",
            route.method.doc_label(),
            pattern_doc_label(&route.pattern),
            authz_doc_label(&route.authz),
            cors,
            body,
            route.doc,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Concrete paths that exercise a pattern, chosen so that a later
    /// route fully covered by an earlier pattern is detected: probe
    /// segments are distinct from any literal, and `Under` probes include
    /// the base itself plus one- and two-level descendants.
    fn representative_paths(pattern: &PathPattern) -> Vec<String> {
        match pattern {
            PathPattern::Exact(base) => vec![(*base).to_string()],
            PathPattern::Under(base) => vec![
                (*base).to_string(),
                format!("{base}/__probe__"),
                format!("{base}/__probe__/__deep__"),
            ],
            PathPattern::Segments(base, specs) => {
                let canonical: Vec<&str> = specs
                    .iter()
                    .map(|spec| match spec {
                        SegmentSpec::Capture(_) => "__cap__",
                        SegmentSpec::Literal(literal) => literal,
                        SegmentSpec::OneOf(options) => options[0],
                    })
                    .collect();
                let mut paths = vec![format!("{base}/{}", canonical.join("/"))];
                // Vary each OneOf position through its alternatives so a
                // partially-overlapping earlier route can't hide.
                for (index, spec) in specs.iter().enumerate() {
                    if let SegmentSpec::OneOf(options) = spec {
                        for option in options.iter().skip(1) {
                            let mut variant = canonical.clone();
                            variant[index] = option;
                            paths.push(format!("{base}/{}", variant.join("/")));
                        }
                    }
                }
                paths
            }
        }
    }

    /// True when every method `later` accepts is also accepted by
    /// `earlier` — the condition under which an earlier row can actually
    /// starve a later one. A method-specific row before an `Any` row on
    /// the same path is fine (the `Any` row still serves the remaining
    /// methods, exactly like the legacy specific-arm-then-blind-arm
    /// pattern it ports).
    fn method_covers(earlier: RouteMethod, later: RouteMethod) -> bool {
        earlier == RouteMethod::Any || earlier == later
    }

    fn pattern_base(pattern: &PathPattern) -> &'static str {
        match pattern {
            PathPattern::Exact(base)
            | PathPattern::Under(base)
            | PathPattern::Segments(base, _) => base,
        }
    }

    #[test]
    fn every_route_is_reachable_no_shadowing() {
        for (j, later) in ROUTES.iter().enumerate() {
            for (i, earlier) in ROUTES[..j].iter().enumerate() {
                if !method_covers(earlier.method, later.method) {
                    continue;
                }
                let mut scratch = Vec::new();
                let fully_covered = representative_paths(&later.pattern).iter().all(|path| {
                    scratch.clear();
                    pattern_matches(&earlier.pattern, path, &mut scratch)
                });
                assert!(
                    !fully_covered,
                    "route {j} ({:?} {:?}) is unreachable: shadowed by route {i} ({:?} {:?})",
                    later.method, later.pattern, earlier.method, earlier.pattern,
                );
            }
        }
    }

    #[test]
    fn every_route_has_a_doc_line() {
        for route in ROUTES {
            assert!(
                !route.doc.trim().is_empty(),
                "route {:?} {:?} has an empty doc string",
                route.method,
                route.pattern,
            );
        }
    }

    #[test]
    fn rows_sharing_a_path_agree_on_cors_posture() {
        // preflight_posture answers per path (OPTIONS carries no target
        // method), so every row matching a given path must declare the
        // same posture.
        for route in ROUTES {
            for path in representative_paths(&route.pattern) {
                let mut scratch = Vec::new();
                for other in ROUTES {
                    scratch.clear();
                    if pattern_matches(&other.pattern, &path, &mut scratch) {
                        assert_eq!(
                            other.cors, route.cors,
                            "rows matching {path} disagree on CORS posture \
                             ({:?} vs {:?})",
                            other.pattern, route.pattern,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn preflight_derivations_answer_for_declared_paths() {
        assert_eq!(preflight_posture("/mcp"), Some(CorsPosture::OwnOrigin));
        assert_eq!(
            preflight_posture("/api/access/overview"),
            Some(CorsPosture::FleetAllowlist)
        );
        assert_eq!(
            preflight_posture(crate::peer::access_request::PUBLIC_REQUEST_PATH),
            Some(CorsPosture::Public)
        );
        assert_eq!(
            preflight_posture("/api/fs/write"),
            Some(CorsPosture::OwnOrigin)
        );
        // The Stats-tab session-list lanes: fleet/loopback echo, never
        // wildcard. Their neighbors keep the own-origin default.
        assert_eq!(
            preflight_posture("/api/sessions"),
            Some(CorsPosture::FleetOrLoopback)
        );
        assert_eq!(
            preflight_posture("/api/sessions/stream"),
            Some(CorsPosture::FleetOrLoopback)
        );
        assert_eq!(
            preflight_posture("/api/sessions/search"),
            Some(CorsPosture::OwnOrigin)
        );
        assert_eq!(preflight_posture("/api/sessionsfoo"), None);

        assert_eq!(
            allowed_methods_for_path("/mcp").as_deref(),
            Some("GET, POST, DELETE, OPTIONS")
        );
        assert_eq!(
            allowed_methods_for_path("/api/fs/write").as_deref(),
            Some("POST, OPTIONS")
        );
        assert_eq!(
            allowed_methods_for_path("/api/session/current/uploads").as_deref(),
            Some("GET, POST, DELETE, OPTIONS")
        );
        assert_eq!(allowed_methods_for_path("/api/sessionsfoo"), None);
    }

    #[test]
    fn pattern_bases_are_normalized() {
        for route in ROUTES {
            let base = pattern_base(&route.pattern);
            assert!(base.starts_with('/'), "base {base:?} must start with /");
            assert!(
                base.len() == 1 || !base.ends_with('/'),
                "base {base:?} must not end with /",
            );
        }
    }

    #[test]
    fn handler_ids_are_unique_outside_declared_shared_groups() {
        // Handlers deliberately serving several wire shapes; their rows
        // must be contiguous so the sharing is visible in the table.
        let shared: &[RouteHandlerId] = &[
            RouteHandlerId::CurrentUploadsGet,
            RouteHandlerId::TransferJobDelete,
            RouteHandlerId::SessionDelete,
            RouteHandlerId::SessionSubRouter,
            RouteHandlerId::AccessIamGrants,
            RouteHandlerId::AccessOrgApplyRenew,
            RouteHandlerId::AccessOrgManage,
            RouteHandlerId::PeersSubRouter,
            RouteHandlerId::McpStream,
            RouteHandlerId::HostedControlManagement,
        ];
        let mut seen: HashSet<RouteHandlerId> = HashSet::new();
        let mut previous: Option<RouteHandlerId> = None;
        for route in ROUTES {
            if !seen.insert(route.handler) {
                assert!(
                    shared.contains(&route.handler),
                    "handler {:?} is bound to more than one route but is not \
                     a declared shared-handler group",
                    route.handler,
                );
                assert_eq!(
                    previous,
                    Some(route.handler),
                    "shared handler {:?} rows must be contiguous in ROUTES",
                    route.handler,
                );
            }
            previous = Some(route.handler);
        }
    }

    #[test]
    fn exact_match_honors_boundaries() {
        assert!(match_route("GET", "/api/sessions").is_some());
        // Method tightening: the session list is GET-only; other methods
        // fall to the dispatch-level 405-with-Allow.
        assert!(match_route("POST", "/api/sessions").is_none());
        assert!(match_route("GET", "/api/sessionsfoo").is_none());
        assert!(match_route("GET", "/api/sessions/").is_none());
        assert!(match_route("GET", "/api/sessions/stream").is_some());
        assert!(match_route("GET", "/api/worktrees").is_some());
        assert!(match_route("POST", "/api/worktrees").is_none());
        assert!(match_route("POST", "/api/worktrees/inspect").is_some());
        assert!(match_route("GET", "/api/worktrees/inspect").is_none());
        assert!(match_route("POST", "/api/worktrees/inspect-old").is_none());
    }

    #[test]
    fn method_tightening_rejects_undeclared_methods_with_allow_union() {
        // Tightened read rows: only GET matches; the dispatch layer turns
        // the miss into 405 with this Allow union (previously the
        // method-blind arm served any method, or the request fell through
        // to the SPA shell).
        for path in [
            "/api/sessions",
            "/api/sessions/stream",
            "/api/sessions/search",
            "/api/project-root",
            "/api/api-key-status",
            "/api/external-agents",
            "/api/displays",
            "/api/access/enrollment-requests",
            "/api/access/iam/state",
            "/api/access/overview",
            "/api/dashboard/targets",
        ] {
            assert!(match_route("GET", path).is_some(), "{path}");
            assert!(match_route("DELETE", path).is_none(), "{path}");
            assert_eq!(
                allowed_methods_for_path(path).as_deref(),
                Some("GET, OPTIONS"),
                "{path}"
            );
        }
        // Tightened admin/present rows: POST-only.
        for path in [
            "/api/access/org-grants",
            "/api/access/orgs/revocations/apply",
            "/api/access/org-grants/renew",
            "/api/access/iam/user-client-grants",
            "/api/access/iam/grants/update",
            "/api/access/orgs/trust",
            "/api/access/orgs/revoke",
            "/api/access/org-grants/issue",
            "/api/access/org-grants/revoke-member",
            "/api/access/org-grants/issuers/init",
            "/api/access/org-grants/issuers/delegate",
            "/api/access/org-grants/issuers/install",
            "/api/access/enrollment-requests/decide",
            "/api/coordinator/route",
        ] {
            assert!(match_route("POST", path).is_some(), "{path}");
            assert!(match_route("GET", path).is_none(), "{path}");
            assert_eq!(
                allowed_methods_for_path(path).as_deref(),
                Some("POST, OPTIONS"),
                "{path}"
            );
        }
        // Mixed-method path: the Allow union spans both rows, and an
        // undeclared method (PUT here) matches neither.
        assert_eq!(
            allowed_methods_for_path("/api/settings").as_deref(),
            Some("GET, POST, OPTIONS")
        );
        assert!(match_route("PUT", "/api/settings").is_none());
        // The two deliberate `Any` catch-alls keep matching every method:
        // their handlers route methods per leaf internally and answer
        // garbage with JSON 404s a per-shape row split would forfeit.
        assert!(match_route("DELETE", "/api/peers").is_some());
        assert!(match_route("DELETE", crate::peer::access_request::PUBLIC_REQUEST_PATH).is_some());
        // Undeclared paths still fall through to the legacy chain / SPA
        // shell rather than the 405 arm.
        assert_eq!(allowed_methods_for_path("/api/no-such-endpoint"), None);
    }

    #[test]
    fn body_policies_pin_their_caps() {
        let policy = |method: &str, path: &str| match_route(method, path).unwrap().0.body;
        // Route-specific caps.
        assert_eq!(
            policy("POST", "/api/fs/write"),
            BodyPolicy::Capped(
                crate::web_gateway::UPLOAD_MAX_BYTES + crate::web_gateway::UPLOAD_MAX_BYTES / 2
            )
        );
        assert_eq!(
            policy("POST", "/mcp"),
            BodyPolicy::Capped(MCP_BODY_CAP_BYTES)
        );
        assert_eq!(
            policy("POST", "/api/diagnostics/visual-freshness"),
            BodyPolicy::Capped(DIAGNOSTICS_BODY_CAP_BYTES)
        );
        assert_eq!(
            policy("POST", "/api/claude-auth/start"),
            BodyPolicy::Capped(CLAUDE_AUTH_START_BODY_CAP_BYTES)
        );
        assert_eq!(
            policy("POST", "/api/claude-auth/code"),
            BodyPolicy::Capped(CLAUDE_AUTH_CODE_BODY_CAP_BYTES)
        );
        assert_eq!(policy("POST", "/api/claude-auth/cancel"), BodyPolicy::None);
        assert_eq!(
            policy("POST", "/api/codex-auth/start"),
            BodyPolicy::Capped(CODEX_AUTH_START_BODY_CAP_BYTES)
        );
        assert_eq!(policy("POST", "/api/codex-auth/cancel"), BodyPolicy::None);
        assert_eq!(
            policy("POST", "/api/access/org-grants"),
            BodyPolicy::Capped(crate::access::org::MAX_ORG_GRANT_DOC_BYTES)
        );
        assert_eq!(
            policy("POST", "/api/access/orgs/revocations/apply"),
            BodyPolicy::Capped(crate::access::org::MAX_ORG_ORL_BYTES)
        );
        assert_eq!(
            policy("POST", "/api/access/org-grants/renew"),
            BodyPolicy::Capped(crate::access::org::MAX_ORG_GRANT_DOC_BYTES)
        );
        // Handler-owned streams: uploads spool to a tempfile; the doorbell's
        // cap is runtime config the table cannot carry.
        assert_eq!(
            policy("POST", "/api/session/current/uploads"),
            BodyPolicy::Streaming
        );
        assert_eq!(
            policy("POST", crate::peer::access_request::PUBLIC_REQUEST_PATH),
            BodyPolicy::Streaming
        );
        // The transfer-chunk row spools its raw body under the handler's
        // pinned per-chunk cap (the row is Streaming; the constant pin
        // guards the value the handler enforces).
        assert_eq!(
            policy("POST", "/api/transfers/j1/chunk"),
            BodyPolicy::Streaming
        );
        assert_eq!(
            crate::web_gateway::TRANSFER_HTTP_CHUNK_MAX_BYTES,
            32 * 1024 * 1024,
            "the transfer chunk cap is a designed constant (design §4): \
             it bounds per-request memory/disk while resumability keeps \
             small chunks cheap — change it deliberately, with the docs",
        );
        // Everything else that takes a body rides the shared default cap.
        assert_eq!(policy("POST", "/api/settings"), BodyPolicy::Default);
        assert_eq!(policy("POST", "/api/peers"), BodyPolicy::Default);
        assert_eq!(policy("POST", "/api/transfers"), BodyPolicy::Default);
        assert_eq!(
            policy("POST", "/api/transfers/j1/commit"),
            BodyPolicy::Default
        );
        assert_eq!(policy("GET", "/api/transfers"), BodyPolicy::None);
        assert_eq!(
            policy("GET", "/api/transfers/j1/download"),
            BodyPolicy::None
        );
        assert_eq!(policy("DELETE", "/api/transfers/j1"), BodyPolicy::None);
        assert_eq!(policy("POST", "/api/transfers/j1/delete"), BodyPolicy::None);
    }

    #[test]
    fn under_match_owns_subtree_not_lookalikes() {
        assert!(match_route("GET", "/api/session/current/changes").is_some());
        assert!(match_route("GET", "/api/session/current/changes/src/main.rs").is_some());
        // The changes-specific GET row must win over the generic
        // current-session sub-router rows (declaration order).
        let (route, _) = match_route("GET", "/api/session/current/changes").unwrap();
        assert_eq!(route.handler, RouteHandlerId::SessionCurrentChanges);
        // A changes look-alike is not the changes route, but it IS still
        // under /api/session/current, so the sub-router serves it — same
        // as the legacy catch-all did.
        let (route, _) = match_route("GET", "/api/session/current/changesx").unwrap();
        assert_eq!(route.handler, RouteHandlerId::SessionSubRouter);
        // POST under current goes to the sub-router (method-explicit port
        // of the method-blind legacy catch-all).
        let (route, _) = match_route("POST", "/api/session/current/changes").unwrap();
        assert_eq!(route.handler, RouteHandlerId::SessionSubRouter);
    }

    #[test]
    fn session_family_precedence_matches_legacy_chain() {
        // Deletion shapes: native DELETE, target form, and the WKWebView
        // POST `/delete`-suffix fallbacks all reach the shared handler.
        for (method, path) in [
            ("DELETE", "/api/session/abc123"),
            ("DELETE", "/api/session/abc123/recordings"),
            ("DELETE", "/api/session/abc123/recordings/delete"),
            ("POST", "/api/session/abc123/delete"),
            ("POST", "/api/session/abc123/recordings/delete"),
        ] {
            let (route, _) = match_route(method, path)
                .unwrap_or_else(|| panic!("{method} {path} must match a delete row"));
            assert_eq!(
                route.handler,
                RouteHandlerId::SessionDelete,
                "{method} {path}"
            );
        }
        // The dedicated upload-delete row wins over the generic delete
        // shapes by segment count (3 vs 1–2) — this is the arm the legacy
        // chain's plain-DELETE prefix match unreachably shadowed.
        let (route, captures) = match_route("DELETE", "/api/session/current/uploads/u1").unwrap();
        assert_eq!(route.handler, RouteHandlerId::CurrentUploadDelete);
        assert_eq!(captures, vec!["u1"]);
        // The uploads GET family: list on the exact row, raw fetch on the
        // segments row, anything else on the Under catch-all — all three
        // share the handler (declared shared group), so the carve is
        // routing-neutral; it exists to give list and raw their own
        // datachannel twins.
        let (route, captures) = match_route("GET", "/api/session/current/uploads").unwrap();
        assert_eq!(
            route.pattern,
            PathPattern::Exact("/api/session/current/uploads")
        );
        assert_eq!(route.handler, RouteHandlerId::CurrentUploadsGet);
        assert_eq!(
            route.tunnel.as_ref().map(|spec| spec.name),
            Some("api_session_current_uploads")
        );
        assert!(captures.is_empty());
        let (route, captures) = match_route("GET", "/api/session/current/uploads/u1/raw").unwrap();
        assert_eq!(route.handler, RouteHandlerId::CurrentUploadsGet);
        assert_eq!(
            route.tunnel.as_ref().map(|spec| spec.name),
            Some("api_session_current_upload_raw")
        );
        assert_eq!(captures, vec!["u1"]);
        let (route, _) = match_route("GET", "/api/session/current/uploads/u1/other").unwrap();
        assert_eq!(route.handler, RouteHandlerId::CurrentUploadsGet);
        assert_eq!(
            route.pattern,
            PathPattern::Under("/api/session/current/uploads")
        );
        assert!(route.tunnel.is_none());
        // Agent-output: current-exact row first, then the by-id row.
        let (route, _) = match_route("POST", "/api/session/current/agent-output").unwrap();
        assert_eq!(route.handler, RouteHandlerId::CurrentAgentOutput);
        let (route, captures) = match_route("POST", "/api/session/abc123/agent-output").unwrap();
        assert_eq!(route.handler, RouteHandlerId::SessionAgentOutput);
        assert_eq!(captures, vec!["abc123"]);
        // POST {id}/delete must hit the delete row, not the sub-router.
        let (route, _) = match_route("POST", "/api/session/current/delete").unwrap();
        assert_eq!(route.handler, RouteHandlerId::SessionDelete);
        // Session detail (and unknown tails) go to the sub-router.
        let (route, _) = match_route("GET", "/api/session/abc123").unwrap();
        assert_eq!(route.handler, RouteHandlerId::SessionSubRouter);
        let (route, _) =
            match_route("GET", "/api/session/abc123/recordings/s1/playlist.m3u8").unwrap();
        assert_eq!(route.handler, RouteHandlerId::SessionSubRouter);
        // Methods the legacy chain never served for these families stay
        // unserved (the catch-all was method-blind; the port is not).
        assert!(match_route("PUT", "/api/session/abc123").is_none());
        assert!(match_route("PATCH", "/api/session/current/history").is_none());
    }

    #[test]
    fn segments_match_is_exact_shape() {
        let specs: &[SegmentSpec] = &[
            SegmentSpec::Capture("id"),
            SegmentSpec::OneOf(&["message", "task"]),
        ];
        let mut captures = Vec::new();
        assert!(segments_match("/t/p1/message", "/t", specs, &mut captures));
        assert_eq!(captures, vec!["p1", "message"]);

        for miss in [
            "/t/p1/other",    // OneOf mismatch
            "/t/p1",          // too short
            "/t/p1/task/x",   // too long
            "/t//message",    // empty segment
            "/tx/p1/message", // base boundary
            "/t",             // no segments at all
        ] {
            let mut scratch = Vec::new();
            assert!(
                !segments_match(miss, "/t", specs, &mut scratch),
                "{miss} must not match",
            );
        }
    }

    #[test]
    fn classify_maps_authz_to_operations() {
        use crate::peer::access_policy::PeerOperation;
        match classify("POST", "/api/fs/write") {
            TableClassification::Matched(op) => {
                assert_eq!(op, Some(PeerOperation::FilesystemWrite))
            }
            TableClassification::NoMatch => panic!("POST /api/fs/write must classify via table"),
        }
        assert!(matches!(
            classify("GET", "/api/fs/write"),
            TableClassification::NoMatch
        ));
        assert!(matches!(
            classify("GET", "/api/definitely-not-a-route"),
            TableClassification::NoMatch
        ));
        // The formerly-divergent dashboard twins classify inspect-grade:
        // both are pure session-log reads (agent-output is POST-shaped
        // only because the output ids ride the body). The tunnel lane is
        // pinned equal by dashboard_control's
        // `formerly_divergent_twins_gate_identically_on_both_lanes`;
        // transport unification will replace both pins with derivation.
        for (method, path) in [
            ("POST", "/api/session/abc123/agent-output"),
            ("GET", "/api/session/abc123/context-snapshot"),
        ] {
            match classify(method, path) {
                TableClassification::Matched(op) => assert_eq!(
                    op,
                    Some(PeerOperation::SessionInspect),
                    "{method} {path} is a read and must classify inspect-grade"
                ),
                TableClassification::NoMatch => {
                    panic!("{method} {path} must classify via table")
                }
            }
        }
        // The transfer rows classify exactly per design §4: reads
        // FilesystemRead, every mutation FilesystemWrite — the same
        // classes their datachannel twins have always declared.
        for (method, path, op) in [
            ("GET", "/api/transfers", PeerOperation::FilesystemRead),
            (
                "GET",
                "/api/transfers/j1/download",
                PeerOperation::FilesystemRead,
            ),
            ("POST", "/api/transfers", PeerOperation::FilesystemWrite),
            (
                "POST",
                "/api/transfers/j1/chunk",
                PeerOperation::FilesystemWrite,
            ),
            (
                "POST",
                "/api/transfers/j1/commit",
                PeerOperation::FilesystemWrite,
            ),
            (
                "DELETE",
                "/api/transfers/j1",
                PeerOperation::FilesystemWrite,
            ),
            (
                "POST",
                "/api/transfers/j1/delete",
                PeerOperation::FilesystemWrite,
            ),
        ] {
            match classify(method, path) {
                TableClassification::Matched(matched) => {
                    assert_eq!(matched, Some(op), "{method} {path} must classify {op:?}")
                }
                TableClassification::NoMatch => {
                    panic!("{method} {path} must classify via table")
                }
            }
        }
        // Undeclared methods on the family stay unrouted (405 at
        // dispatch), not silently served.
        assert!(matches!(
            classify("PUT", "/api/transfers"),
            TableClassification::NoMatch
        ));
        assert!(matches!(
            classify("GET", "/api/transfers/j1/chunk"),
            TableClassification::NoMatch
        ));
        // The sign-in ceremonies are credential custody: every leaf —
        // the status reads included — classifies CredentialsManage, the
        // operation no peer profile (peer-root included) and no scoped
        // default carries. A widening here is a custody regression.
        for (method, path) in [
            ("POST", "/api/claude-auth/start"),
            ("GET", "/api/claude-auth/status"),
            ("POST", "/api/claude-auth/code"),
            ("POST", "/api/claude-auth/cancel"),
            ("POST", "/api/codex-auth/start"),
            ("GET", "/api/codex-auth/status"),
            ("POST", "/api/codex-auth/cancel"),
        ] {
            match classify(method, path) {
                TableClassification::Matched(op) => assert_eq!(
                    op,
                    Some(PeerOperation::CredentialsManage),
                    "{method} {path} must gate on credentials.manage"
                ),
                TableClassification::NoMatch => {
                    panic!("{method} {path} must classify via table")
                }
            }
        }
        assert!(matches!(
            classify("GET", "/api/claude-auth/start"),
            TableClassification::NoMatch
        ));
        assert!(matches!(
            classify("GET", "/api/codex-auth/start"),
            TableClassification::NoMatch
        ));
        // The device flow deliberately has no code-submission leaf.
        assert!(matches!(
            classify("POST", "/api/codex-auth/code"),
            TableClassification::NoMatch
        ));
    }

    #[test]
    fn tunnel_specs_are_unique_and_derive_operations_fail_closed() {
        let mut seen: HashSet<&str> = HashSet::new();
        for (route, spec) in tunnel_specs() {
            assert!(
                seen.insert(spec.name),
                "tunnel method {} is declared on more than one row",
                spec.name,
            );
            // The status `<method>_available` derivation keys on the
            // api_ prefix; a differently-named tunnel method would
            // silently lose its availability boolean.
            assert!(
                spec.name.starts_with("api_"),
                "tunnel method {} must be api_-prefixed",
                spec.name,
            );
            // Fail-closed derivation must exist: a Public / McpToken /
            // PeerFederation row may only carry a tunnel twin with an
            // explicit documented op override — otherwise the method
            // would resolve to nothing and (correctly, but uselessly) be
            // denied as unknown.
            assert!(
                route.tunnel_operation().is_some(),
                "tunnel method {} has no derivable IAM operation: \
                 non-Operation rows must declare an op_override",
                spec.name,
            );
        }
    }

    /// The op-override slot is a closed, documented enumeration (design
    /// §2.7 / risk R2): every override names its reason, and this test
    /// pins the exact set so a tunnel/HTTP divergence can only ever be
    /// added deliberately. The occupants: the signed-org doorbell twins
    /// (S6) — Public rows on HTTP (the signed document/list is the
    /// authorization) while the tunnel methods deliberately gate on a
    /// bound session's operation. The coordinator twin's historical
    /// override (tunnel PeerManage vs the ladder's Task) left the set on
    /// the 2026-07-11 owner decision unifying coordinator routing on
    /// PeerUse — both lanes now derive from the federation ladder (see
    /// `peers_family_tunnel_ops_assert_against_the_federation_ladder`).
    #[test]
    fn tunnel_op_overrides_are_a_closed_documented_enumeration() {
        let documented: &[&str] = &[
            "api_access_org_orl",
            "api_access_org_orl_apply",
            "api_access_org_present",
            "api_access_org_renew",
        ];
        let mut actual: Vec<&str> = Vec::new();
        for (_, spec) in tunnel_specs() {
            if let Some((_, reason)) = spec.op_override {
                assert!(
                    !reason.trim().is_empty(),
                    "tunnel op override on {} must state a non-empty reason",
                    spec.name,
                );
                actual.push(spec.name);
            }
        }
        actual.sort_unstable();
        assert_eq!(
            actual, documented,
            "tunnel op overrides drifted from the documented divergence set",
        );
    }

    /// The peers family's federation-op assertions (design §2.2 / S7):
    /// each twinned method's IAM operation is no longer a free-floating
    /// declaration — it derives from `federation_http_operation` on the
    /// row's canonical leaf, and this test asserts, per method, that
    /// (a) the ladder classifies the canonical leaf exactly as the
    /// method's historical tunnel operation, (b) the row derivation
    /// reproduces it, and (c) the canonical leaf really is served by
    /// this row (first match), so the leaf a twin is derived from can
    /// never silently belong to a different row. The coordinator twin
    /// joined the derivation on the 2026-07-11 owner decision (PeerUse
    /// on both lanes) and is asserted in the tail — it lives on its own
    /// handler, so it stays out of the PeersSubRouter loop.
    #[test]
    fn peers_family_tunnel_ops_assert_against_the_federation_ladder() {
        use crate::peer::access_policy::{federation_http_operation, PeerOperation as Op};
        let expected: &[(&str, &str, &str, Op)] = &[
            (
                "api_peer_pairing_invite",
                "POST",
                "/api/peers/pairing/invite",
                Op::AccessManage,
            ),
            (
                "api_peer_pairing_request_access",
                "POST",
                "/api/peers/pairing/request-access",
                Op::PeerManage,
            ),
            (
                "api_peer_pairing_request_access_poll",
                "POST",
                "/api/peers/pairing/request-access/poll",
                Op::PeerManage,
            ),
            (
                "api_peer_pairing_requests",
                "GET",
                "/api/peers/pairing/requests",
                Op::AccessInspect,
            ),
            (
                "api_peer_pairing_identities",
                "GET",
                "/api/peers/pairing/identities",
                Op::AccessInspect,
            ),
            (
                "api_peer_pairing_identity_revoke",
                "POST",
                "/api/peers/pairing/identities/revoke",
                Op::AccessManage,
            ),
            (
                "api_peer_pairing_request_decision",
                "POST",
                "/api/peers/pairing/requests/{code}/{decision}",
                Op::AccessManage,
            ),
            (
                "api_peer_pairing_join",
                "POST",
                "/api/peers/pairing/join",
                Op::PeerManage,
            ),
            ("api_peers", "GET", "/api/peers", Op::PeerInspect),
            ("api_peer_add", "POST", "/api/peers", Op::PeerManage),
            ("api_peer_remove", "DELETE", "/api/peers", Op::PeerManage),
            (
                "api_peer_eligible",
                "GET",
                "/api/peers/eligible",
                Op::PeerInspect,
            ),
            (
                "api_peer_message",
                "POST",
                "/api/peers/{peer_id}/message",
                Op::PeerUse,
            ),
            (
                "api_peer_task",
                "POST",
                "/api/peers/{peer_id}/task",
                Op::PeerUse,
            ),
            (
                "api_peer_approval",
                "POST",
                "/api/peers/{peer_id}/approval",
                Op::PeerUse,
            ),
            (
                "api_peer_webrtc_signal",
                "POST",
                "/api/peers/{peer_id}/webrtc",
                Op::PeerUse,
            ),
            (
                "api_peer_file_transfer_signal",
                "POST",
                "/api/peers/{peer_id}/file-transfer-webrtc",
                Op::PeerUse,
            ),
            (
                "api_peer_dashboard_control_signal",
                "POST",
                "/api/peers/{peer_id}/dashboard-control-webrtc",
                Op::PeerUse,
            ),
        ];
        for (name, verb, leaf, op) in expected {
            // (a) The ladder classifies the canonical leaf as the
            // method's historical operation.
            assert_eq!(
                federation_http_operation(verb, leaf),
                Some(*op),
                "{name}: the federation ladder no longer classifies {verb} {leaf} as {op:?}",
            );
            let (route, _) = tunnel_specs()
                .find(|(_, spec)| spec.name == *name)
                .unwrap_or_else(|| panic!("{name} must be declared as a tunnel column"));
            // (b) The row derivation reproduces the ladder's answer.
            assert_eq!(
                route.tunnel_operation(),
                Some(*op),
                "{name}: row derivation disagrees with the ladder",
            );
            assert_eq!(
                route.canonical_leaf(),
                Some((*verb, (*leaf).to_string())),
                "{name}: the row's canonical leaf drifted",
            );
            // (c) The canonical leaf is served by this very row.
            let (matched, _) = match_route(verb, leaf)
                .unwrap_or_else(|| panic!("{name}: no route matches {verb} {leaf}"));
            assert!(
                std::ptr::eq(matched, route),
                "{name}: {verb} {leaf} is served by a different row than the one \
                 declaring the twin",
            );
            assert_eq!(matched.handler, RouteHandlerId::PeersSubRouter, "{name}");
        }
        // Owner decision 2026-07-11: coordinator routing dispatches a
        // task to a capability-matched connected peer under this daemon's
        // peer identity — the quick-controls action class — so both lanes
        // gate on PeerUse. This supersedes the preserved per-lane split
        // (HTTP: Task, tunnel: PeerManage via a documented override),
        // which let a task-authority peer spend this daemon's identity on
        // a third peer over HTTP. Pinned from both directions, same
        // (a)/(b)/(c) facts as the loop above (the coordinator lives on
        // its own handler, not the peers sub-router).
        assert_eq!(
            federation_http_operation("POST", "/api/coordinator/route"),
            Some(Op::PeerUse),
        );
        let (route, spec) = tunnel_specs()
            .find(|(_, spec)| spec.name == "api_coordinator_route")
            .expect("coordinator twin declared");
        assert!(
            spec.op_override.is_none(),
            "the coordinator twin derives from the ladder — its historical \
             override was retired by the 2026-07-11 owner decision",
        );
        assert_eq!(route.tunnel_operation(), Some(Op::PeerUse));
        assert_eq!(
            route.canonical_leaf(),
            Some(("POST", "/api/coordinator/route".to_string())),
        );
        let (matched, _) = match_route("POST", "/api/coordinator/route")
            .expect("no route matches POST /api/coordinator/route");
        assert!(
            std::ptr::eq(matched, route),
            "the coordinator leaf is served by a different row than the one \
             declaring the twin",
        );
        assert_eq!(matched.handler, RouteHandlerId::CoordinatorRoute);
    }

    /// The docs chapter's generated endpoint table must equal the one
    /// rendered from ROUTES. The chapter may say more around it — never
    /// less or different between the markers. Regenerate by running this
    /// test with `-- --nocapture` and pasting the printed block.
    #[test]
    fn endpoint_docs_match_chapter() {
        const BEGIN: &str = "<!-- gateway-route-table:begin (generated; do not edit by hand) -->";
        const END: &str = "<!-- gateway-route-table:end -->";
        let chapter_path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/src/web-dashboard.md");
        let rendered = render_endpoint_docs();
        let chapter = std::fs::read_to_string(chapter_path)
            .unwrap_or_else(|e| panic!("read {chapter_path}: {e}"))
            // Windows checkouts materialize the chapter with CRLF
            // (autocrlf); the rendered table is always LF.
            .replace("\r\n", "\n");
        let block = chapter
            .split_once(BEGIN)
            .and_then(|(_, rest)| rest.split_once(END))
            .map(|(block, _)| block.trim_matches('\n'));
        let expected = rendered.trim_matches('\n');
        if block != Some(expected) {
            println!(
                "--- paste between the markers in docs/src/web-dashboard.md ---\n\
                 {expected}\n\
                 --- end ---"
            );
            panic!(
                "docs/src/web-dashboard.md endpoint table is out of date \
                 (or the markers are missing); regenerate with \
                 `cargo test --bin intendant endpoint_docs_match_chapter -- --nocapture`"
            );
        }
    }

    #[test]
    fn endpoint_docs_render_every_route() {
        let docs = render_endpoint_docs();
        assert_eq!(
            docs.lines().count(),
            ROUTES.len() + 2, // header + separator
            "every route renders exactly one docs row",
        );
        assert!(docs.contains("`/api/session/current/changes[/…]`"));
        assert!(docs.contains("| POST | `/api/fs/write` |"));
    }
}

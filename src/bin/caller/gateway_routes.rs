//! Declarative route table for the web gateway.
//!
//! One declaration per HTTP API route. The gateway derives from this table
//! everything that used to be hand-synchronized across four places in
//! `web_gateway.rs` (the dispatch chain, the IAM classifier, the OPTIONS
//! preflight, and the docs endpoint table):
//!
//! 1. **Dispatch** — the request loop consults [`match_route`] before the
//!    legacy if/else chain and serves a matching route through its
//!    [`RouteHandlerId`] arm. The legacy chain shrinks as families port.
//! 2. **IAM classification** — `dashboard_http_operation` consults
//!    [`classify`] first and only falls back to its residual hand-written
//!    match for routes that have not been ported yet.
//! 3. **Preflight** — `OPTIONS` answers derive method unions and CORS
//!    posture from the table (phase 2 of the migration).
//! 4. **Docs** — [`render_endpoint_docs`] renders the endpoint table for
//!    `docs/src/web-dashboard.md`; a drift test pins the chapter to it
//!    (phase 3).
//!
//! **Never add an API route by editing the dispatch chain**: declare it
//! here and give it a handler arm in `web_gateway.rs`'s table-dispatch
//! match. Table invariants (no shadowed routes, non-empty docs, pattern
//! hygiene) are enforced by unit tests in this module.
//!
//! During the migration, `BodyPolicy` and `CorsPosture` are declarative:
//! handlers keep reading their own bodies and stamping their own response
//! headers exactly as the legacy chain did (byte-identical behavior), and
//! the enums document the contract that phase 4's response/body
//! consolidation will enforce mechanically.

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
    #[allow(dead_code)] // constructed from phase 2 (peer quick-control routes)
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
    /// The public doorbell class: open by design.
    Public,
}

/// How the request body is consumed. Declarative during the migration
/// (handlers keep their exact legacy reads); phase 4 moves consumption
/// into dispatch so handlers can't forget caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyPolicy {
    /// No body is read.
    None,
    /// Read with the shared bounded reader (`read_request_body` /
    /// `read_post_body`).
    Default,
    /// Read with a route-specific cap (e.g. the fs-write envelope cap).
    Capped,
    /// The handler drives the stream itself (uploads, NDJSON streams).
    Streaming,
}

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
    McAnchors,
    McRecords,
    McFission,
    WorktreesInspect,
    WorktreesRemove,
    WorktreesScan,
    WorktreesList,
    SessionsStream,
    SessionsSearch,
    SessionsList,
    ProjectRoot,
    SettingsPost,
    SettingsGet,
    ApiKeysPost,
    ApiKeyStatus,
    ExternalAgents,
    DiagnosticsVisualFreshness,
    Displays,
    /// The public peer access-request doorbell (create + status poll).
    Doorbell,
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
    DashboardTargets,
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
    }
}

/// The route table. **Match order is declaration order** — first match
/// wins, and the no-shadowing invariant test keeps every row reachable.
/// Keep `Exact`/`Segments` rows of a family before an `Under` row of the
/// same base.
pub(crate) static ROUTES: &[Route] = &[
    // ── Filesystem (scoped by authorize_http_filesystem_access; the GET
    //    trio is additionally pre-gated by peer_filesystem_query_request).
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/fs/stat"),
        PeerOperation::FilesystemRead,
        BodyPolicy::None,
        RouteHandlerId::FsStat,
        "Stat a filesystem path (scope-checked)",
    ),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/fs/list"),
        PeerOperation::FilesystemRead,
        BodyPolicy::None,
        RouteHandlerId::FsList,
        "List a directory (scope-checked)",
    ),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/fs/read"),
        PeerOperation::FilesystemRead,
        BodyPolicy::None,
        RouteHandlerId::FsRead,
        "Read file bytes (scope-checked; supports byte ranges)",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/fs/mkdir"),
        PeerOperation::FilesystemWrite,
        BodyPolicy::Default,
        RouteHandlerId::FsMkdir,
        "Create a directory (scope-checked)",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/fs/write"),
        PeerOperation::FilesystemWrite,
        BodyPolicy::Capped,
        RouteHandlerId::FsWrite,
        "Write file bytes (scope-checked; sha256-guarded overwrite)",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/fs/rename"),
        PeerOperation::FilesystemWrite,
        BodyPolicy::Default,
        RouteHandlerId::FsRename,
        "Move/rename a file or directory (scope-checked)",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/fs/delete"),
        PeerOperation::FilesystemWrite,
        BodyPolicy::Default,
        RouteHandlerId::FsDelete,
        "Delete a file or directory (scope-checked)",
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
    ),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/session/current/history"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::CurrentHistory,
        "Serialized rollback History for the current session",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/session/current/rollback"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::CurrentRollback,
        "Roll the current session back to a round (optionally reverting files)",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/session/current/redo"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::CurrentRedo,
        "Redo the last rolled-back round",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/session/current/prune"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::CurrentPrune,
        "Prune rollback state for the current session",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/session/current/agent-output"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::CurrentAgentOutput,
        "Append agent output to the current session's log",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/session/current/uploads"),
        PeerOperation::SessionManage,
        BodyPolicy::Streaming,
        RouteHandlerId::CurrentUploadsPost,
        "Upload a file attachment (raw streamed body; name/destination in query)",
    ),
    op_route(
        RouteMethod::Get,
        PathPattern::Under("/api/session/current/uploads"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::CurrentUploadsGet,
        "List uploads, or fetch one (subpath {id}/raw)",
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
    ),
    // ── Session deletion. Five accepted wire shapes (native DELETE plus
    //    the WKWebView POST fallback with a literal `delete` suffix); one
    //    handler serves all of them by filtering the `delete` segment.
    op_route(
        RouteMethod::Delete,
        PathPattern::Segments("/api/session", &[SegmentSpec::Capture("id")]),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::SessionDelete,
        "Delete a session's data",
    ),
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
    op_route(
        RouteMethod::Post,
        PathPattern::Segments(
            "/api/session",
            &[
                SegmentSpec::Capture("id"),
                SegmentSpec::Literal("agent-output"),
            ],
        ),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::SessionAgentOutput,
        "Append agent output to a session's log by id",
    ),
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
    op_route(
        RouteMethod::Get,
        PathPattern::Under("/api/session"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionSubRouter,
        "Session detail; context-snapshot, recordings (+segments/playlist), report zip, frames",
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
    ),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/managed-context/records"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::McRecords,
        "Managed-context record index",
    ),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/managed-context/fission"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::McFission,
        "Managed-context fission state",
    ),
    // ── Worktrees.
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/worktrees/inspect"),
        PeerOperation::SessionInspect,
        BodyPolicy::Default,
        RouteHandlerId::WorktreesInspect,
        "Inspect one worktree (branch, ahead/behind, dirty state)",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/worktrees/remove"),
        PeerOperation::SessionManage,
        BodyPolicy::Default,
        RouteHandlerId::WorktreesRemove,
        "Remove a worktree from the inventory",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/worktrees/scan"),
        PeerOperation::SessionManage,
        BodyPolicy::None,
        RouteHandlerId::WorktreesScan,
        "Rescan the worktree inventory (refreshes the cache)",
    ),
    op_route(
        RouteMethod::Get,
        PathPattern::Exact("/api/worktrees"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::WorktreesList,
        "Cached worktree inventory",
    ),
    // ── Session listing. The stream/search rows close a historical gap:
    //    dispatch served them but the hand classifier never gated them
    //    for browser principals (peers were already SessionInspect-gated
    //    by federation_http_operation). Declaring the operation here is
    //    the fail-closed fix; the differential test allowlists it.
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/sessions/stream"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionsStream,
        "NDJSON stream of the session list",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/sessions/search"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionsSearch,
        "Search sessions (q, source, mode, project filters)",
    ),
    // Method-agnostic in the legacy chain (see RouteMethod::Any).
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/sessions"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::SessionsList,
        "List sessions (id filter, limit, usage view; response CORS * for the fleet Stats tab)",
    ),
    // ── Settings / info endpoints. `Any` rows port arms that were
    //    method-blind in the legacy chain; their single declared
    //    operation now classifies every method (fail-closed vs the
    //    residual classifier's method-specific holes — allowlisted in
    //    the differential test).
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/project-root"),
        PeerOperation::Settings,
        BodyPolicy::None,
        RouteHandlerId::ProjectRoot,
        "Project root path this daemon serves",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/settings"),
        PeerOperation::Settings,
        BodyPolicy::Default,
        RouteHandlerId::SettingsPost,
        "Update runtime settings",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/settings"),
        PeerOperation::Settings,
        BodyPolicy::None,
        RouteHandlerId::SettingsGet,
        "Current runtime settings",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/api-keys"),
        PeerOperation::Settings,
        BodyPolicy::Default,
        RouteHandlerId::ApiKeysPost,
        "Store provider API keys in the project .env",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/api-key-status"),
        PeerOperation::Settings,
        BodyPolicy::None,
        RouteHandlerId::ApiKeyStatus,
        "Which provider keys are configured (presence only)",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/external-agents"),
        PeerOperation::SessionInspect,
        BodyPolicy::None,
        RouteHandlerId::ExternalAgents,
        "Detected external coding agents (codex, claude)",
    ),
    op_route(
        RouteMethod::Post,
        PathPattern::Exact("/api/diagnostics/visual-freshness"),
        PeerOperation::DisplayInput,
        BodyPolicy::Default,
        RouteHandlerId::DiagnosticsVisualFreshness,
        "Visual-freshness diagnostics transcript sink (NDJSON body)",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/displays"),
        PeerOperation::DisplayView,
        BodyPolicy::None,
        RouteHandlerId::Displays,
        "Enumerate active displays",
    ),
    // ── Public doorbell + signed org endpoints. The payload's own
    //    signature/shape is the authority; RouteAuthz::Public makes the
    //    no-IAM-gate decision explicit (these paths are also exempted by
    //    the mTLS and origin gates, which match on the same constants).
    public_route(
        RouteMethod::Any,
        PathPattern::Under(crate::peer::access_request::PUBLIC_REQUEST_PATH),
        BodyPolicy::Capped,
        RouteHandlerId::Doorbell,
        "Peer access-request doorbell: knock (POST, size-capped) or poll one request's status (GET subpath)",
    ),
    public_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/org-grants"),
        BodyPolicy::Capped,
        RouteHandlerId::AccessOrgGrantPresent,
        "Present a signed org grant document (verified against locally trusted org keys)",
    ),
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
    ),
    public_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/orgs/revocations/apply"),
        BodyPolicy::Capped,
        RouteHandlerId::AccessOrgApplyRenew,
        "Apply a signed org revocation list",
    ),
    public_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/org-grants/renew"),
        BodyPolicy::Capped,
        RouteHandlerId::AccessOrgApplyRenew,
        "Renew an org grant document (signed payload)",
    ),
    // ── Access administration (fleet-CORS where the anchor page needs
    //    to read responses; own-origin otherwise).
    fleet_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/iam/user-client-grants"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessIamGrants,
        "Upsert a user-client grant",
    ),
    fleet_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/iam/grants/update"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessIamGrants,
        "Update an IAM grant",
    ),
    fleet_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/orgs/trust"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Trust an org root key on this daemon",
    ),
    fleet_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/orgs/revoke"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Withdraw trust in an org root key",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/org-grants/issue"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Issue an org grant (org root/issuer key on this daemon)",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/org-grants/revoke-member"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Revoke an org member (appends to the ORL)",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/org-grants/issuers/init"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Initialize an org issuer key",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/org-grants/issuers/delegate"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Delegate to an org issuer",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/org-grants/issuers/install"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessOrgManage,
        "Install a delegated org issuer key",
    ),
    fleet_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/enrollment-requests/decide"),
        PeerOperation::AccessManage,
        BodyPolicy::Default,
        RouteHandlerId::AccessEnrollmentDecide,
        "Approve or deny a pending enrollment request",
    ),
    fleet_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/enrollment-requests"),
        PeerOperation::AccessInspect,
        BodyPolicy::None,
        RouteHandlerId::AccessEnrollmentRequests,
        "Pending enrollment requests",
    ),
    fleet_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/iam/state"),
        PeerOperation::AccessInspect,
        BodyPolicy::None,
        RouteHandlerId::AccessIamState,
        "Local IAM state (roles, grants, bindings)",
    ),
    fleet_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/access/overview"),
        PeerOperation::AccessInspect,
        BodyPolicy::None,
        RouteHandlerId::AccessOverview,
        "Access overview for the calling principal",
    ),
    op_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/dashboard/targets"),
        PeerOperation::AccessInspect,
        BodyPolicy::None,
        RouteHandlerId::DashboardTargets,
        "Dashboard target list (this daemon + connected peers)",
    ),
    // ── Federation surface: registry, pairing, quick controls,
    //    capability routing. One verbatim sub-router row per family;
    //    IAM delegates to federation_http_operation (see
    //    RouteAuthz::PeerFederation).
    federation_route(
        RouteMethod::Any,
        PathPattern::Under("/api/peers"),
        BodyPolicy::Default,
        RouteHandlerId::PeersSubRouter,
        "Peer registry (list/add/remove), pairing (invite/join/requests/identities), eligibility, and per-peer quick controls + signaling relays",
    ),
    federation_route(
        RouteMethod::Any,
        PathPattern::Exact("/api/coordinator/route"),
        BodyPolicy::Default,
        RouteHandlerId::CoordinatorRoute,
        "Capability-based task routing through the Coordinator",
    ),
    // ── MCP Streamable HTTP (token-bound inside the handler; the
    //    per-tool IAM gate lives in the MCP layer).
    Route {
        method: RouteMethod::Post,
        pattern: PathPattern::Exact("/mcp"),
        authz: RouteAuthz::McpToken,
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::Default,
        handler: RouteHandlerId::McpPost,
        doc: "MCP Streamable HTTP endpoint (JSON-RPC requests + notifications)",
    },
    Route {
        method: RouteMethod::Get,
        pattern: PathPattern::Exact("/mcp"),
        authz: RouteAuthz::McpToken,
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::None,
        handler: RouteHandlerId::McpStream,
        doc: "MCP SSE stream (405: stateless server)",
    },
    Route {
        method: RouteMethod::Delete,
        pattern: PathPattern::Exact("/mcp"),
        authz: RouteAuthz::McpToken,
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::None,
        handler: RouteHandlerId::McpStream,
        doc: "MCP session delete (405: stateless server)",
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
            CorsPosture::Public => "public",
        };
        let body = match route.body {
            BodyPolicy::None => "none",
            BodyPolicy::Default => "bounded",
            BodyPolicy::Capped => "capped",
            BodyPolicy::Streaming => "streaming",
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
            RouteHandlerId::SessionDelete,
            RouteHandlerId::SessionSubRouter,
            RouteHandlerId::AccessIamGrants,
            RouteHandlerId::AccessOrgApplyRenew,
            RouteHandlerId::AccessOrgManage,
            RouteHandlerId::McpStream,
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
        assert!(match_route("POST", "/api/sessions").is_some()); // Any-method legacy arm
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

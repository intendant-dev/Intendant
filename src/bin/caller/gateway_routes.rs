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
#[allow(dead_code)] // constructed from phase 2 (segment-shaped peer routes)
pub(crate) enum SegmentSpec {
    /// Any single non-empty segment; handed to the handler as a capture.
    /// The name is used for docs rendering (`/api/peers/{peer_id}/…`).
    Capture(&'static str),
    /// This exact segment.
    Literal(&'static str),
    /// One of these exact segments; the matched value is also captured.
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
    #[allow(dead_code)] // constructed from phase 2 (segment-shaped peer routes)
    Segments(&'static str, &'static [SegmentSpec]),
}

/// The HTTP method a route answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteMethod {
    Get,
    Post,
    #[allow(dead_code)] // declared from phase 1 (session DELETE routes)
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
    /// doorbell, the org-revocation apply endpoint). Forces a route to
    /// SAY it is public instead of falling through a match.
    #[allow(dead_code)] // declared from phase 2 (doorbell/org-grant routes)
    Public,
    /// `/mcp`: token-bound inside the handler, with the scoped
    /// own/app-origin CORS echo. Classifies as no `PeerOperation` (the
    /// MCP layer enforces per-tool IAM itself).
    #[allow(dead_code)] // declared from phase 2 (/mcp routes)
    McpToken,
}

/// Which CORS/preflight posture the route gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CorsPosture {
    /// Same-origin or the `intendant://` app scheme only (the default
    /// for `/api/*`): preflight echoes an allowed origin, otherwise no
    /// ACAO header.
    OwnOrigin,
    /// The fleet Access APIs: echo only fleet-allowlisted origins.
    #[allow(dead_code)] // declared from phase 2 (Access family)
    FleetAllowlist,
    /// The public doorbell class: open by design.
    #[allow(dead_code)] // declared from phase 2 (doorbell routes)
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
    #[allow(dead_code)] // declared as streaming routes port
    Streaming,
}

/// Links a table row to its dispatch arm in `web_gateway.rs`. The match
/// there is exhaustive, so a declared route without an arm — or an arm
/// whose route was deleted — fails to compile; the uniqueness invariant
/// test catches a handler bound to two rows unintentionally (multi-method
/// rows sharing one handler are declared adjacently and exempted there).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RouteHandlerId {
    FsWrite,
    SessionCurrentChanges,
    WorktreesInspect,
    WorktreesRemove,
    WorktreesScan,
    WorktreesList,
    SessionsList,
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

/// The route table. **Match order is declaration order** — first match
/// wins, and the no-shadowing invariant test keeps every row reachable.
/// Keep `Exact` rows of a family before an `Under` row of the same base.
pub(crate) static ROUTES: &[Route] = &[
    Route {
        method: RouteMethod::Post,
        pattern: PathPattern::Exact("/api/fs/write"),
        authz: RouteAuthz::Operation(PeerOperation::FilesystemWrite),
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::Capped,
        handler: RouteHandlerId::FsWrite,
        doc: "Write file bytes (scope-checked; sha256-guarded overwrite)",
    },
    Route {
        method: RouteMethod::Get,
        pattern: PathPattern::Under("/api/session/current/changes"),
        authz: RouteAuthz::Operation(PeerOperation::SessionManage),
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::None,
        handler: RouteHandlerId::SessionCurrentChanges,
        doc: "List the session's changed files, or the unified diff for one file (subpath)",
    },
    Route {
        method: RouteMethod::Post,
        pattern: PathPattern::Exact("/api/worktrees/inspect"),
        authz: RouteAuthz::Operation(PeerOperation::SessionInspect),
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::Default,
        handler: RouteHandlerId::WorktreesInspect,
        doc: "Inspect one worktree (branch, ahead/behind, dirty state)",
    },
    Route {
        method: RouteMethod::Post,
        pattern: PathPattern::Exact("/api/worktrees/remove"),
        authz: RouteAuthz::Operation(PeerOperation::SessionManage),
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::Default,
        handler: RouteHandlerId::WorktreesRemove,
        doc: "Remove a worktree from the inventory",
    },
    Route {
        method: RouteMethod::Post,
        pattern: PathPattern::Exact("/api/worktrees/scan"),
        authz: RouteAuthz::Operation(PeerOperation::SessionManage),
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::None,
        handler: RouteHandlerId::WorktreesScan,
        doc: "Rescan the worktree inventory (refreshes the cache)",
    },
    Route {
        method: RouteMethod::Get,
        pattern: PathPattern::Exact("/api/worktrees"),
        authz: RouteAuthz::Operation(PeerOperation::SessionInspect),
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::None,
        handler: RouteHandlerId::WorktreesList,
        doc: "Cached worktree inventory",
    },
    Route {
        // Method-agnostic in the legacy chain (see RouteMethod::Any).
        method: RouteMethod::Any,
        pattern: PathPattern::Exact("/api/sessions"),
        authz: RouteAuthz::Operation(PeerOperation::SessionInspect),
        cors: CorsPosture::OwnOrigin,
        body: BodyPolicy::None,
        handler: RouteHandlerId::SessionsList,
        doc:
            "List sessions (id filter, limit, usage view; response CORS * for the fleet Stats tab)",
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
        }),
        None => TableClassification::NoMatch,
    }
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

    fn methods_overlap(a: RouteMethod, b: RouteMethod) -> bool {
        a == RouteMethod::Any || b == RouteMethod::Any || a == b
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
                if !methods_overlap(earlier.method, later.method) {
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
    fn handler_ids_are_unique() {
        let mut seen = HashSet::new();
        for route in ROUTES {
            assert!(
                seen.insert(route.handler),
                "handler {:?} is bound to more than one route; multi-method \
                 routes should be declared as adjacent rows sharing the \
                 handler deliberately (and this test updated to exempt them)",
                route.handler,
            );
        }
    }

    #[test]
    fn exact_match_honors_boundaries() {
        assert!(match_route("GET", "/api/sessions").is_some());
        assert!(match_route("POST", "/api/sessions").is_some()); // Any-method legacy arm
        assert!(match_route("GET", "/api/sessionsfoo").is_none());
        assert!(match_route("GET", "/api/sessions/").is_none());
        assert!(match_route("GET", "/api/sessions/stream").is_none()); // still legacy-served
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
        assert!(match_route("GET", "/api/session/current/changesx").is_none());
        assert!(match_route("POST", "/api/session/current/changes").is_none()); // legacy catch-all
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

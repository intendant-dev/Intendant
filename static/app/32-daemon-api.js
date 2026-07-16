// ── daemonApi — the unified daemon-call facade (transport program, F0) ────
// One call surface for every request/response exchange with a daemon, over
// three transport adapters (docs: the transport-unification design, §3):
//
//   daemonApi.request(method, params, opts)          -> {ok, status, body}
//   daemonApi.bytes(method, params, opts)            -> {bytes, meta:{rangeStart,rangeEnd,totalSize,sha256,filename,contentType,job}}
//   daemonApi.upload(method, params, source, opts)   -> {ok, status, body}   (source: Blob | Uint8Array)
//   daemonApi.stream(method, params, opts, onEvent)  -> completion summary
//   daemonApi.availability(method, target)           -> {ok, reason}
//   // opts: { target?: hostId|{remoteHttp: origin}|null, signal?,
//   //         timeoutMs?, retries?, fallback?: 'auto'|'never' }
//
// `method` is always the tunnel method name; DAEMON_API_HTTP_MAP maps it to
// the HTTP twin when the direct lane serves the call. Adapters:
//   - local tunnel: the `dashboardControlTransport` protocol client
//     (50-control-transport.js) — request/bytes/upload/stream map 1:1;
//   - peer tunnel: `peerDashboardControlConnectionForHost(hostId)` — same
//     wire; peers have NO HTTP lane by design (the browser never
//     authenticates to a peer over HTTP);
//   - HTTP: `authedFetch` + the descriptor table. Connect mode is policy,
//     not an adapter: it forces `fallback:'never'` (hosted validators probe
//     exactly this no-legacy-fallback behavior);
//   - remote-http (F4): `{remoteHttp: origin}` targets name a FLEET
//     daemon's own HTTP origin — direct cross-origin fetch under the
//     fleet-CORS rows, never a tunnel, never a fallback (design §5's F4
//     caveat: fleet fan-out is explicit, not ambiguous).
//
// STATUS: consumed by the F1 family (files IDE fs calls, transfers pump,
// staged uploads), the F2 sessions-family reads, the F3 settings/keys
// family (settings GET/POST, api-keys save, key-status, project-root,
// external-agents, displays), the F4 access dialogs family
// (overview/enrollment/connect/tier + IAM grant writes), the F5
// peers/approvals/coordinator family (peer list + quick controls, the
// pairing dialogs, the coordinator forms, and the peer WebRTC signal
// relays), the F6 credential-custody family (vault leases, the
// custody trail, the daemon vault store + deposit lane, client-egress
// registration/probe — tunnel-only methods whose per-cause availability
// the vault UI surfaces), the F1c transfer-jobs adapter (the
// resumable jobs protocol over the S9 /api/transfers rows, presence
// feature-detected through the probe registry below), and the F7
// families: the control-msg dispatchers (api_control_msg,
// api_session_control_msg, api_dashboard_action_msg — WS-twin residue:
// their HTTP-era twin is the /ws intent stream, so the facade serves the
// tunnel leg only and the call sites keep their own /ws fallback) plus
// the display RPCs — the input-authority trio and the display WebRTC
// signal RPC (WS-twin residue too: /ws display_offer/display_ice/
// display_input are the twins) and the visual-freshness diagnostics
// sink (twinned — the one rawBody entry below). Realtime display frames
// (display_input events, media channels) are NOT facade calls and never
// will be. The F8a flip finished the migration: the last product call
// sites (session delete/recordings/agent-output, the current-session
// rounds family, the media/voice/mcp residue trio) ride the facade, and
// the legacy helpers (`rpcOrHttp`/`jsonFetch` on DashboardTransport,
// `dashboardJsonFetch`) are warn-logging shims — every hit records
// through dashboardTransportShimRecord below and window.qa
// .transportShimHits() answers the soak verdict; F8b deletes the shims
// after a clean soak week. The boot smoke's window.qa.daemonApi() probe
// asserts the facade evaluates.
//
// Fragment placement: this file must evaluate BEFORE every consumer
// fragment's eval-time code (manifest order is program order — the
// cross-fragment TDZ lint in crates/app-html-assembler enforces the direct
// case). It sits right after 31-init-identity-fleet.js, which declares the
// `let` bindings the adapters read (dashboardControlTransport,
// peerDashboardControlConnectionsByHost); every other reference here
// (authedFetch, dashboardConnectModeEnabled, peerDashboardControlConnectionForHost,
// dashboardComposeFetchSignal, dashboardControlRequestTimeoutMs,
// dashboardControlBase64ToBytes, dashboardControlBytesToBase64) is a
// module-hoisted function declaration, and ALL cross-fragment references in
// this file live inside function bodies — nothing here touches another
// fragment's binding at eval time.

// ── Descriptor table: tunnel method -> HTTP twin ──────────────────────────
// Hand-written mirror of gateway_routes::ROUTES for the methods the facade
// can serve over direct HTTP. A daemon-side parity test
// (`daemon_api_http_map_mirrors_gateway_routes` in
// src/bin/caller/dashboard_control/mod.rs) pins every entry's verb + path
// template against the route table and the tunnel method's IAM operation —
// the sanctioned mirror-with-parity-test pattern (CLAUDE.md "Derive, don't
// mirror"). KEEP ONE ENTRY PER LINE: the test parses this literal.
//
// Coverage: the F1 family (filesystem + staged uploads), the F2
// sessions-family reads (managed-context, worktrees, the session list and
// its NDJSON stream, search, detail, report, context snapshots) plus the
// F8a sessions stragglers (recordings list, agent output by id and for
// the current session, session delete, and the current-session rounds
// family — changes, history, rollback, redo, prune), the
// F3 settings/keys family (settings GET/POST, api-keys save, key-status,
// project-root, external-agents, displays), and the F4 access family:
// the dialogs set (overview, IAM state, enrollment reads + decide, IAM
// grant upsert/update, the connect admin quartet, the tier pair,
// fleet-cert request, dashboard targets) plus the org set (trust/revoke,
// issuance, issuer key management, and the signed-org doorbell quartet —
// present, renew, ORL read, ORL apply, whose HTTP rows are Public by
// design: the signed document is the authorization), and the F5 peers /
// coordinator family (the peer registry list/add/remove, eligible,
// quick controls — message/task/approval, the three WebRTC signal
// relays, the pairing set, and the coordinator route; their rows
// delegate IAM to the federation ladder, S7), and the transfer-jobs
// family (task #6 / F1c): the six /api/transfers rows (S9) twinning the
// datachannel `api_transfer_*` methods. Unlike every other family, a
// deployed daemon may predate those rows, so their entries carry
// `probe: 'transfers'` — the availability derivation and the transfers
// pump consult the one-shot GET /api/transfers probe (below) before
// treating the HTTP lane as present (design §4: 404/405 ⇒ old daemon ⇒
// legacy behavior + honest availability). The F6 credential-custody family
// (api_credential_*, api_daemon_vault_*) is deliberately absent too, and
// stays so: custody is tunnel-scoped by design — no HTTP rows exist or
// are planned (docs/src/credential-custody.md; the transport design
// parks custody HTTP rows as an explicit future decision), so those
// methods ride the facade with no fallback lane at all. The F7 display
// residue (api_display_bootstrap, api_display_webrtc_signal, the
// api_display_input_authority_* trio) is absent for the same reason:
// their HTTP-era twin is the /ws signaling socket, not a route (design
// §2.7 pins them residue), so the facade serves their tunnel leg only —
// api_diagnostics_visual_freshness is the family's one twinned entry.
// Entry shape: verb + path template (`{name}` segments are lifted from
// params), `alias` = capture-name -> param-key lift map (the session rows
// capture `id` while the tunnel's canonical param is `session_id` — the
// handlers accept both, so the wire never changes), `query` = param keys
// lifted into the query string (arrays comma-join; empty arrays stay
// absent), `queryJson` = query keys whose array value JSON-encodes instead
// (the search `projects` filter, which the HTTP handler serde-parses),
// `queryRepeat` = query-key -> array-param-key map emitting one
// `key=value` pair per element (the eligible endpoint's repeated
// `?capability=` keys from the tunnel's `capabilities` array; empty
// arrays stay absent),
// `lane` = non-JSON response/request lane ('bytes' | 'upload' | 'stream'),
// `encode` = upload
// body encoding ('raw' streamed body | 'json-b64' JSON envelope with
// content_b64), `rawQuery` = the named param is a pre-encoded query STRING
// the HTTP twin takes verbatim (the managed-context tunnel contract),
// `pathSuffix` = the named param is an OPTIONAL file path appended under
// the base path (the changes row's Under pattern; empty keeps the bare
// base = the list shape — the tunnel twin carries the same value as a
// plain param and rebuilds the sub-path server-side),
// `rawBody` = the named string param is the raw REQUEST BODY the HTTP
// twin takes verbatim, sent as `rawBodyType` (the visual-freshness
// NDJSON sink: the tunnel carries the transcript as a `body` param, the
// HTTP row appends its raw body unparsed — a JSON-encoded body would be
// written into the transcript verbatim). rawBody twins carry all other
// metadata in the query string, like encode:'raw' uploads.
// `probe` = the twin's rows may be absent on a deployed daemon; name the
// DAEMON_API_HTTP_PROBES entry whose result gates the http-only lane.
// `mutation` is DERIVED from the verb (POST/DELETE), never
// stored — that derivation is the fallback policy (§3.7).
const DAEMON_API_HTTP_MAP = Object.freeze({
  api_fs_stat: { verb: 'GET', path: '/api/fs/stat', query: ['path'] },
  api_fs_list: { verb: 'GET', path: '/api/fs/list', query: ['path'] },
  api_fs_read: { verb: 'GET', path: '/api/fs/read', query: ['path'], lane: 'bytes' },
  api_fs_mkdir: { verb: 'POST', path: '/api/fs/mkdir' },
  api_fs_write: { verb: 'POST', path: '/api/fs/write', lane: 'upload', encode: 'json-b64' },
  api_fs_rename: { verb: 'POST', path: '/api/fs/rename' },
  api_fs_delete: { verb: 'POST', path: '/api/fs/delete' },
  api_transfer_jobs: { verb: 'GET', path: '/api/transfers', query: ['id', 'resume_token'], probe: 'transfers' },
  api_transfer_job_create: { verb: 'POST', path: '/api/transfers', probe: 'transfers' },
  api_transfer_upload_chunk: { verb: 'POST', path: '/api/transfers/{id}/chunk', query: ['offset', 'resume_token'], lane: 'upload', encode: 'raw', probe: 'transfers' },
  api_transfer_upload_commit: { verb: 'POST', path: '/api/transfers/{id}/commit', probe: 'transfers' },
  api_transfer_job_delete: { verb: 'DELETE', path: '/api/transfers/{id}', probe: 'transfers' },
  api_transfer_download_read: { verb: 'GET', path: '/api/transfers/{id}/download', lane: 'bytes', probe: 'transfers' },
  api_session_current_uploads: { verb: 'GET', path: '/api/session/current/uploads' },
  api_session_current_upload: { verb: 'POST', path: '/api/session/current/uploads', query: ['name', 'destination'], lane: 'upload', encode: 'raw' },
  api_session_current_upload_raw: { verb: 'GET', path: '/api/session/current/uploads/{id}/raw', lane: 'bytes' },
  api_session_current_upload_delete: { verb: 'DELETE', path: '/api/session/current/uploads/{upload_id}' },
  api_sessions: { verb: 'GET', path: '/api/sessions', query: ['ids', 'limit', 'view'] },
  api_sessions_stream: { verb: 'GET', path: '/api/sessions/stream', query: ['limit'], lane: 'stream' },
  api_sessions_search: { verb: 'GET', path: '/api/sessions/search', query: ['q', 'source', 'mode', 'projects'], queryJson: ['projects'] },
  api_sessions_message_search: { verb: 'GET', path: '/api/sessions/message-search', query: ['q', 'source', 'include_superseded', 'subagents', 'cursor', 'limit'] },
  api_session_detail: { verb: 'GET', path: '/api/session/{id}', alias: { id: 'session_id' }, query: ['source', 'limit', 'before'] },
  api_session_report: { verb: 'GET', path: '/api/session/{id}/report', alias: { id: 'session_id' }, lane: 'bytes' },
  api_session_context_snapshot: { verb: 'GET', path: '/api/session/{id}/context-snapshot', alias: { id: 'session_id' }, query: ['file', 'source', 'request_id', 'request_index', 'ts'] },
  api_session_recordings: { verb: 'GET', path: '/api/session/{id}/recordings', alias: { id: 'session_id' } },
  api_session_agent_output: { verb: 'POST', path: '/api/session/{id}/agent-output', alias: { id: 'session_id' }, query: ['source'] },
  api_session_delete: { verb: 'DELETE', path: '/api/session/{id}/{target}', alias: { id: 'session_id' } },
  api_session_current_changes: { verb: 'GET', path: '/api/session/current/changes', pathSuffix: 'path', rawQuery: 'query' },
  api_session_current_history: { verb: 'GET', path: '/api/session/current/history' },
  api_session_current_rollback: { verb: 'POST', path: '/api/session/current/rollback' },
  api_agenda_list: { verb: 'GET', path: '/api/agenda' },
  api_agenda_op: { verb: 'POST', path: '/api/agenda/op' },
  api_agenda_reminder_policy: { verb: 'POST', path: '/api/agenda/reminders/policy' },
  api_memory_search: { verb: 'GET', path: '/api/memory/search', query: ['q', 'limit', 'candidates'] },
  api_memory_claim: { verb: 'GET', path: '/api/memory/claim', query: ['id'] },
  api_memory_propose: { verb: 'POST', path: '/api/memory/propose' },
  api_session_current_redo: { verb: 'POST', path: '/api/session/current/redo' },
  api_session_current_prune: { verb: 'POST', path: '/api/session/current/prune' },
  api_session_current_agent_output: { verb: 'POST', path: '/api/session/current/agent-output' },
  api_managed_context_records: { verb: 'GET', path: '/api/managed-context/records', rawQuery: 'query' },
  api_managed_context_anchors: { verb: 'GET', path: '/api/managed-context/anchors', rawQuery: 'query' },
  api_managed_context_fission: { verb: 'GET', path: '/api/managed-context/fission', rawQuery: 'query' },
  api_worktrees: { verb: 'GET', path: '/api/worktrees' },
  api_worktrees_inspect: { verb: 'POST', path: '/api/worktrees/inspect' },
  api_worktrees_scan: { verb: 'POST', path: '/api/worktrees/scan' },
  api_worktrees_remove: { verb: 'POST', path: '/api/worktrees/remove' },
  api_worktrees_clean: { verb: 'POST', path: '/api/worktrees/clean' },
  api_worktrees_merge: { verb: 'POST', path: '/api/worktrees/merge' },
  api_settings: { verb: 'GET', path: '/api/settings' },
  api_settings_save: { verb: 'POST', path: '/api/settings' },
  api_api_keys_save: { verb: 'POST', path: '/api/api-keys' },
  api_key_status: { verb: 'GET', path: '/api/api-key-status' },
  api_project_root: { verb: 'GET', path: '/api/project-root' },
  api_external_agents: { verb: 'GET', path: '/api/external-agents' },
  api_displays: { verb: 'GET', path: '/api/displays' },
  api_diagnostics_visual_freshness: { verb: 'POST', path: '/api/diagnostics/visual-freshness', query: ['session_id'], rawBody: 'body', rawBodyType: 'application/x-ndjson' },
  api_access_overview: { verb: 'GET', path: '/api/access/overview' },
  api_access_iam_state: { verb: 'GET', path: '/api/access/iam/state' },
  api_access_enrollment_requests: { verb: 'GET', path: '/api/access/enrollment-requests' },
  api_access_enrollment_decide: { verb: 'POST', path: '/api/access/enrollment-requests/decide' },
  api_access_iam_upsert_user_client_grant: { verb: 'POST', path: '/api/access/iam/user-client-grants' },
  api_access_iam_update_grant: { verb: 'POST', path: '/api/access/iam/grants/update' },
  api_access_connect_status: { verb: 'GET', path: '/api/access/connect/status' },
  api_access_connect_claim_code: { verb: 'GET', path: '/api/access/connect/claim-code' },
  api_access_connect_config: { verb: 'POST', path: '/api/access/connect/config' },
  api_access_connect_unclaim: { verb: 'POST', path: '/api/access/connect/unclaim' },
  api_access_set_tier: { verb: 'POST', path: '/api/access/tier' },
  api_fleet_cert_request: { verb: 'POST', path: '/api/access/fleet-cert/request' },
  api_dashboard_targets: { verb: 'GET', path: '/api/dashboard/targets' },
  api_dashboard_tabs: { verb: 'GET', path: '/api/dashboard/tabs' },
  api_access_org_trust: { verb: 'POST', path: '/api/access/orgs/trust' },
  api_access_org_revoke: { verb: 'POST', path: '/api/access/orgs/revoke' },
  api_access_org_issue: { verb: 'POST', path: '/api/access/org-grants/issue' },
  api_access_org_revoke_member: { verb: 'POST', path: '/api/access/org-grants/revoke-member' },
  api_access_org_issuer_init: { verb: 'POST', path: '/api/access/org-grants/issuers/init' },
  api_access_org_issuer_delegate: { verb: 'POST', path: '/api/access/org-grants/issuers/delegate' },
  api_access_org_issuer_install: { verb: 'POST', path: '/api/access/org-grants/issuers/install' },
  api_access_org_present: { verb: 'POST', path: '/api/access/org-grants' },
  api_access_org_renew: { verb: 'POST', path: '/api/access/org-grants/renew' },
  api_access_org_orl: { verb: 'GET', path: '/api/access/orgs/{org_handle}/revocations', alias: { org_handle: 'handle' } },
  api_access_org_orl_apply: { verb: 'POST', path: '/api/access/orgs/revocations/apply' },
  api_peers: { verb: 'GET', path: '/api/peers' },
  api_peer_add: { verb: 'POST', path: '/api/peers' },
  api_peer_remove: { verb: 'DELETE', path: '/api/peers' },
  api_peer_eligible: { verb: 'GET', path: '/api/peers/eligible', queryRepeat: { capability: 'capabilities' } },
  api_peer_message: { verb: 'POST', path: '/api/peers/{peer_id}/message' },
  api_peer_task: { verb: 'POST', path: '/api/peers/{peer_id}/task' },
  api_peer_approval: { verb: 'POST', path: '/api/peers/{peer_id}/approval' },
  api_peer_webrtc_signal: { verb: 'POST', path: '/api/peers/{peer_id}/webrtc' },
  api_peer_file_transfer_signal: { verb: 'POST', path: '/api/peers/{peer_id}/file-transfer-webrtc' },
  api_peer_dashboard_control_signal: { verb: 'POST', path: '/api/peers/{peer_id}/dashboard-control-webrtc' },
  api_peer_pairing_invite: { verb: 'POST', path: '/api/peers/pairing/invite' },
  api_peer_pairing_join: { verb: 'POST', path: '/api/peers/pairing/join' },
  api_peer_pairing_request_access: { verb: 'POST', path: '/api/peers/pairing/request-access' },
  api_peer_pairing_request_access_poll: { verb: 'POST', path: '/api/peers/pairing/request-access/poll' },
  api_peer_pairing_requests: { verb: 'GET', path: '/api/peers/pairing/requests' },
  api_peer_pairing_request_decision: { verb: 'POST', path: '/api/peers/pairing/requests/{code}/{decision}', alias: { code: 'request_id', decision: 'op' } },
  api_peer_pairing_identities: { verb: 'GET', path: '/api/peers/pairing/identities' },
  api_peer_pairing_identity_revoke: { verb: 'POST', path: '/api/peers/pairing/identities/revoke' },
  // Both coordinator lanes gate on peer.use (owner decision 2026-07-11):
  // routing a task through the coordinator delegates this daemon's peer
  // identity to a capability-matched peer — the same action class as the
  // per-peer task quick control. The unification retired the program's
  // last live per-lane IAM divergence (HTTP: Task via the federation
  // ladder; tunnel: PeerManage via a documented op-override); the tunnel
  // twin now derives from the route row like every other twinned method.
  api_coordinator_route: { verb: 'POST', path: '/api/coordinator/route' },
});

// ── Uniform error shape (§3.5) ────────────────────────────────────────────
// UI code branches on `kind`, never on message strings:
//   'abort'       — the caller's signal fired
//   'timeout'     — the composed timeout fired (tunnel waitFor / fetch)
//   'transport'   — the lane itself failed (channel closed, fetch threw,
//                   tunnel down with no permissible fallback)
//   'denied'      — the daemon's authorizer refused the method
//   'unavailable' — no lane can serve this method here (unknown method on
//                   an old daemon, tunnel-only method with no tunnel)
//   'http'        — the daemon answered with an error result (status set
//                   when the response carried one)
class DaemonApiError extends Error {
  constructor(kind, method, target, message, status = null) {
    super(message);
    this.name = 'DaemonApiError';
    this.kind = kind;
    this.method = method;
    this.target = target || null;
    this.status = Number.isFinite(Number(status)) ? Number(status) : null;
  }
}

// Classify a tunnel rejection. The transport rejects with plain Errors; the
// server's authorizer texts are the stable markers ("… is not allowed: …"
// and "unknown dashboard-control method: …" from
// authorize_dashboard_control_method), and waitFor stamps "timed out".
function daemonApiTunnelError(err, method, target) {
  if (err instanceof DaemonApiError) return err;
  const message = (err && err.message) || String(err);
  if (err && err.name === 'AbortError') return new DaemonApiError('abort', method, target, message);
  if (err && err.name === 'TimeoutError') return new DaemonApiError('timeout', method, target, message);
  if (/timed out/i.test(message)) return new DaemonApiError('timeout', method, target, message);
  if (/is not allowed/.test(message)) return new DaemonApiError('denied', method, target, message);
  if (/unknown dashboard-control method/.test(message)) {
    return new DaemonApiError('unavailable', method, target, message);
  }
  return new DaemonApiError('transport', method, target, message);
}

// Classify a fetch() rejection. With the composed signal (§3.6) a timeout
// aborts the fetch with a TimeoutError reason; a caller abort keeps
// AbortError; everything else is the network lane failing.
function daemonApiHttpError(err, method, target = null) {
  if (err instanceof DaemonApiError) return err;
  const message = (err && err.message) || String(err);
  if (err && err.name === 'TimeoutError') return new DaemonApiError('timeout', method, target, message);
  if (err && err.name === 'AbortError') return new DaemonApiError('abort', method, target, message);
  return new DaemonApiError('transport', method, target, message);
}

// ── Envelopes ─────────────────────────────────────────────────────────────
// {ok, status, body} from a tunnel result payload. The _httpStatus/_httpOk
// sidecar keys are part of the wire contract (design §5); this is the
// facade's own normalizer — the fake-Response shim (responseFromPayload,
// 49-daemons-multihost.js) and its clones retire onto it at family flips.
function daemonApiEnvelopeFromPayload(payload) {
  const rawStatus = Number(payload && payload._httpStatus);
  const status = Number.isFinite(rawStatus) && rawStatus >= 100 && rawStatus <= 599 ? rawStatus : 200;
  const ok = typeof (payload && payload._httpOk) === 'boolean'
    ? payload._httpOk
    : status >= 200 && status < 300;
  let body = payload;
  if (body && typeof body === 'object' && !Array.isArray(body)) {
    body = { ...body };
    delete body._httpStatus;
    delete body._httpOk;
  }
  return { ok, status, body: body ?? {} };
}

// {bytes, meta} from a tunnel byte-stream result. Field names follow the
// byte_stream_end.result sidecar (range_start/range_end are exclusive-end,
// matching the server's dashboard_fs_read_file shape). An ok:false result
// (the daemon answered, with an error) throws 'http' — it is a delivered
// response, so it must never be retried or replayed.
function daemonApiBytesEnvelope(result, method, target) {
  if (!result || result.ok === false || result._httpOk === false) {
    const raw = Number(result && result._httpStatus);
    throw new DaemonApiError(
      'http',
      method,
      target,
      (result && result.error) || `${method} returned an error`,
      Number.isFinite(raw) ? raw : null
    );
  }
  const bytes = result.bytes instanceof Uint8Array
    ? result.bytes
    : (typeof result.data_base64 === 'string' && result.data_base64
      ? dashboardControlBase64ToBytes(result.data_base64)
      : new Uint8Array(0));
  const num = value => (Number.isFinite(Number(value)) ? Number(value) : null);
  return {
    bytes,
    meta: {
      rangeStart: num(result.range_start ?? result.offset),
      rangeEnd: num(result.range_end),
      totalSize: num(result.total_size ?? result.totalSize),
      sha256: typeof result.sha256 === 'string' ? result.sha256 : '',
      filename: result.filename ? String(result.filename) : '',
      contentType: result.content_type ? String(result.content_type) : '',
      job: result.job && typeof result.job === 'object' ? result.job : null,
    },
  };
}

// ── Timeouts & abort (§3.6): the composed-signal rule ─────────────────────
// Every adapter call runs under the caller's signal composed with a hard
// per-method timeout — no facade call may run signal-less (the
// transfers-pump wedge class stays unrepresentable). Family defaults come
// from dashboardControlRequestTimeoutMs (50-control-transport.js).
function daemonApiTimeoutMs(method, opts) {
  const timeout = Number(opts && opts.timeoutMs);
  return Number.isFinite(timeout) && timeout > 0
    ? timeout
    : dashboardControlRequestTimeoutMs(method);
}

function daemonApiHttpSignal(method, opts) {
  return dashboardComposeFetchSignal(opts && opts.signal, daemonApiTimeoutMs(method, opts));
}

// Tunnel options: signal passes through; timeoutMs is honored by the
// bytes/upload/stream verbs. (The transport's `request` verb applies its
// own per-method default and ignores options.timeoutMs today — changing
// that is a transport-file edit that belongs to a family flip, not F0.)
function daemonApiTunnelOptions(method, opts) {
  return {
    signal: opts && opts.signal,
    timeoutMs: daemonApiTimeoutMs(method, opts),
  };
}

// ── Fallback policy as data (§3.7) ────────────────────────────────────────
// Derived from the twin's verb, never judged per call site: reads (GET
// twins) may fall back to HTTP and retry; mutations (POST/DELETE twins)
// must never be replayed over HTTP after a tunnel attempt that MAY have
// reached the daemon (today's fallbackAfterRpcFailure:false semantics, the
// no-replay rule the hosted validators probe). Mutations may still use
// HTTP when no tunnel attempt was made at all. Connect mode never uses
// HTTP for any method.
function daemonApiFallbackPolicy(method) {
  const spec = DAEMON_API_HTTP_MAP[method] || null;
  if (!spec) return { httpTwin: false, mutation: null, replayAfterAttempt: false };
  const mutation = spec.verb !== 'GET';
  return { httpTwin: true, mutation, replayAfterAttempt: !mutation };
}

// Gate the HTTP lane for a local-target call. `tunnelError` is the
// classified failure of a tunnel attempt (null when no attempt was made);
// when fallback is not permitted the original tunnel error propagates so
// callers see the true failure, not a policy artifact.
function daemonApiEnsureHttpFallback(method, opts, tunnelError) {
  const policy = daemonApiFallbackPolicy(method);
  if (dashboardConnectModeEnabled()) {
    throw tunnelError || new DaemonApiError(
      'transport', method, null,
      `dashboard tunnel is not connected for ${method} (Connect mode has no HTTP lane)`
    );
  }
  if (!policy.httpTwin) {
    throw tunnelError || new DaemonApiError(
      'unavailable', method, null,
      `${method} has no HTTP twin and the dashboard tunnel is not connected`
    );
  }
  if (opts && opts.fallback === 'never') {
    throw tunnelError || new DaemonApiError(
      'transport', method, null,
      `dashboard tunnel is not connected for ${method} (fallback disabled)`
    );
  }
  if (tunnelError && !policy.replayAfterAttempt) throw tunnelError;
}

// ── HTTP row-presence probes (F1c, design §4) ─────────────────────────────
// Most descriptor rows shipped with (or before) the facade, so map
// presence == daemon presence. The transfer-jobs rows (task #6) landed
// after deployed daemons existed, so their entries are `probe`-gated:
// ONE GET probe per page load answers "does this daemon serve the rows?"
// — 404/405 ⇒ old daemon (absent ⇒ legacy behavior + honest
// availability); any delivered response, auth walls included, proves the
// rows exist (the real request surfaces its own denial). A transport
// failure leaves the state 'unknown' so a later ensure() re-probes; the
// cached verdict feeds daemonApiAvailability below.
const DAEMON_API_HTTP_PROBES = {
  transfers: { path: '/api/transfers', state: 'unknown', promise: null },
};

function daemonApiHttpProbeState(name) {
  return DAEMON_API_HTTP_PROBES[name]?.state || 'unknown';
}

// Resolve (and cache) a probe: Promise<boolean> — "the rows are present".
// Callers that need a lane DECISION await this; sync availability reads
// the cached state. Never called in connect mode (no HTTP lane there) —
// the transfers pump gates on dashboardConnectModeEnabled() first.
async function daemonApiEnsureHttpProbe(name) {
  const probe = DAEMON_API_HTTP_PROBES[name];
  if (!probe) return false;
  if (probe.state !== 'unknown') return probe.state === 'present';
  if (!probe.promise) {
    probe.promise = (async () => {
      try {
        const resp = await authedFetch(probe.path, {
          method: 'GET',
          cache: 'no-store',
          signal: dashboardComposeFetchSignal(null, 15000),
        });
        probe.state = resp.status === 404 || resp.status === 405 ? 'absent' : 'present';
      } catch (_) {
        probe.state = 'unknown';
      } finally {
        probe.promise = null;
      }
      return probe.state === 'present';
    })();
  }
  return probe.promise;
}

// ── HTTP adapter ──────────────────────────────────────────────────────────
// Builds verb/path/query/body from the descriptor. `{name}` path segments
// lift (and consume) params[name] — or params[alias[name]] when the entry
// declares a capture alias; `query` keys lift into the query
// string; for JSON POSTs the remaining params become the body verbatim.

// pathSuffix values address files. Absolute paths (POSIX '/', Windows
// drive prefix) encode as ONE percent-encoded segment — the leading '/'
// must ride as %2F or the server's slash-trimming eats it; relative
// paths keep '/' separators, encoding each segment (the legacy
// encodeChangePath contract, owned by the adapter since the F8a flip).
function daemonApiEncodePathSuffix(value) {
  const raw = String(value);
  if (raw.startsWith('/') || /^[A-Za-z]:[\\/]/.test(raw)) return encodeURIComponent(raw);
  return raw.split('/').map(encodeURIComponent).join('/');
}
function daemonApiHttpTarget(spec, method, params) {
  const source = params && typeof params === 'object' ? params : {};
  const used = new Set();
  let path = spec.path.replace(/\{([A-Za-z0-9_]+)\}/g, (match, key) => {
    const paramKey = (spec.alias && spec.alias[key]) || key;
    const value = source[paramKey];
    if (value === undefined || value === null || String(value) === '') {
      throw new TypeError(`daemonApi: ${method} requires params.${paramKey} for ${spec.path}`);
    }
    used.add(paramKey);
    return encodeURIComponent(String(value));
  });
  // pathSuffix twins: the named param is an optional file path appended
  // under the base route; empty keeps the bare base (the changes list
  // shape vs the one-file diff shape).
  if (spec.pathSuffix) {
    const raw = source[spec.pathSuffix];
    used.add(spec.pathSuffix);
    if (raw !== undefined && raw !== null && String(raw) !== '') {
      path += `/${daemonApiEncodePathSuffix(raw)}`;
    }
  }
  const query = [];
  for (const key of spec.query || []) {
    const value = source[key];
    if (value === undefined || value === null) continue;
    used.add(key);
    if (Array.isArray(value)) {
      // Empty lists stay absent on every array key — the tunnel
      // vocabulary cannot express HTTP's present-but-empty filter, and no
      // legacy call site sent one.
      if (!value.length) continue;
      // queryJson keys JSON-encode (the search `projects` filter, which
      // the HTTP handler serde-parses); other arrays comma-join
      // (api_sessions ids).
      const encoded = (spec.queryJson || []).includes(key)
        ? JSON.stringify(value)
        : value.join(',');
      query.push(`${encodeURIComponent(key)}=${encodeURIComponent(encoded)}`);
      continue;
    }
    query.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`);
  }
  // queryRepeat twins: the tunnel method takes an array param; the HTTP
  // twin takes one repeated `key=value` pair per element (the eligible
  // endpoint's `?capability=` vocabulary — its server parser rejects
  // comma-joins, so these keys never ride the `query` lift).
  for (const [key, paramKey] of Object.entries(spec.queryRepeat || {})) {
    const value = source[paramKey];
    used.add(paramKey);
    if (!Array.isArray(value)) continue;
    for (const element of value) {
      if (element === undefined || element === null || String(element) === '') continue;
      query.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(element))}`);
    }
  }
  // rawQuery twins: the tunnel method takes one pre-encoded query-string
  // param (the managed-context handlers rebuild a request line from it);
  // the HTTP twin takes that same string as the URL query verbatim.
  if (spec.rawQuery) {
    const raw = source[spec.rawQuery];
    used.add(spec.rawQuery);
    if (raw !== undefined && raw !== null && String(raw) !== '') {
      query.push(String(raw));
    }
  }
  const rest = {};
  for (const [key, value] of Object.entries(source)) {
    if (!used.has(key)) rest[key] = value;
  }
  return { url: query.length ? `${path}?${query.join('&')}` : path, rest };
}

async function daemonApiHttpRequest(method, params, opts) {
  const spec = DAEMON_API_HTTP_MAP[method];
  const { url, rest } = daemonApiHttpTarget(spec, method, params);
  const init = { method: spec.verb, signal: daemonApiHttpSignal(method, opts) };
  // Caller cache posture passes through (session detail/metadata reads
  // send 'no-store', exactly as their pre-facade fetches did).
  if (opts && opts.cache) init.cache = opts.cache;
  // rawBody twins: the named param IS the request body, verbatim (the
  // visual-freshness NDJSON sink appends its body unparsed — JSON-encoding
  // it would corrupt the transcript). Everything else rides the query
  // string; any other leftover param has no lane and must not silently
  // become a body.
  if (spec.rawBody) {
    const raw = rest[spec.rawBody];
    delete rest[spec.rawBody];
    const extras = Object.keys(rest);
    if (extras.length > 0) {
      throw new TypeError(
        `daemonApi: ${method} rawBody twin cannot carry extra params (${extras.join(', ')}) — declare them as query keys`
      );
    }
    init.headers = { 'Content-Type': spec.rawBodyType || 'application/octet-stream' };
    init.body = String(raw ?? '');
  } else if ((spec.verb === 'POST' || spec.verb === 'DELETE') && Object.keys(rest).length > 0) {
    // Body-less POST twins stay body-less (api_worktrees_scan rides a
    // BodyPolicy::None row, and every legacy empty-payload POST call site
    // sent no body/Content-Type either). DELETE twins carry their leftover
    // params the same way: api_peer_remove's HTTP shape has always been a
    // DELETE with a `{peer_id}` JSON body (path-captured DELETE twins
    // consume their params into the path and stay body-less).
    init.headers = { 'Content-Type': 'application/json' };
    init.body = JSON.stringify(rest);
  }
  let resp;
  try {
    resp = await authedFetch(url, init);
  } catch (err) {
    throw daemonApiHttpError(err, method);
  }
  const body = await resp.json().catch(() => ({}));
  return { ok: resp.ok, status: resp.status, body: body ?? {} };
}

async function daemonApiHttpBytes(method, params, opts) {
  const spec = DAEMON_API_HTTP_MAP[method];
  const { url } = daemonApiHttpTarget(spec, method, params);
  const init = { method: spec.verb, signal: daemonApiHttpSignal(method, opts) };
  // The tunnel twin takes offset/length params; the HTTP lane speaks Range
  // headers (handle_fs_read). Content-Range's end is inclusive — normalize
  // back to the tunnel's exclusive rangeEnd below.
  const offset = Number(params && params.offset);
  const length = Number(params && params.length);
  if (Number.isFinite(offset) && offset >= 0 && Number.isFinite(length) && length > 0) {
    init.headers = { Range: `bytes=${offset}-${offset + length - 1}` };
  } else if (Number.isFinite(offset) && offset > 0) {
    init.headers = { Range: `bytes=${offset}-` };
  }
  let resp;
  try {
    resp = await authedFetch(url, init);
  } catch (err) {
    throw daemonApiHttpError(err, method);
  }
  if (!resp.ok) {
    const detail = await resp.json().catch(() => ({}));
    throw new DaemonApiError(
      'http', method, null,
      (detail && detail.error) || `${method} returned HTTP ${resp.status}`,
      resp.status
    );
  }
  const bytes = new Uint8Array(await resp.arrayBuffer());
  const headers = resp.headers;
  const header = name => (headers && typeof headers.get === 'function' ? headers.get(name) || '' : '');
  const rangeMatch = /^bytes\s+(\d+)-(\d+)\/(\d+|\*)$/.exec(header('content-range').trim());
  const partial = resp.status === 206;
  const filenameMatch = /filename="([^"]*)"/.exec(header('content-disposition'));
  return {
    bytes,
    meta: {
      rangeStart: rangeMatch ? Number(rangeMatch[1]) : (partial ? null : 0),
      rangeEnd: rangeMatch ? Number(rangeMatch[2]) + 1 : (partial ? null : bytes.byteLength),
      totalSize: rangeMatch && rangeMatch[3] !== '*'
        ? Number(rangeMatch[3])
        : (partial ? null : bytes.byteLength),
      sha256: header('x-content-sha256'),
      filename: filenameMatch ? filenameMatch[1] : '',
      contentType: header('content-type') || 'application/octet-stream',
      job: null,
    },
  };
}

async function daemonApiHttpUpload(method, params, source, opts) {
  const spec = DAEMON_API_HTTP_MAP[method];
  const { url, rest } = daemonApiHttpTarget(spec, method, params);
  const init = { method: spec.verb, signal: daemonApiHttpSignal(method, opts) };
  if (spec.encode === 'raw') {
    // Raw streamed body (staged uploads): metadata rides the query string;
    // a tunnel-only `mime` param becomes the Content-Type header.
    init.headers = {
      'Content-Type': (opts && opts.contentType) || rest.mime || (source && source.type) || 'application/octet-stream',
    };
    init.body = source;
  } else {
    // json-b64 twins (api_fs_write): JSON envelope with base64 content.
    let bytes;
    if (source instanceof Blob) bytes = new Uint8Array(await source.arrayBuffer());
    else if (source instanceof Uint8Array) bytes = source;
    else bytes = new Uint8Array(source || []);
    init.headers = { 'Content-Type': 'application/json' };
    init.body = JSON.stringify({ ...rest, content_b64: dashboardControlBytesToBase64(bytes) });
  }
  let resp;
  try {
    resp = await authedFetch(url, init);
  } catch (err) {
    throw daemonApiHttpError(err, method);
  }
  const body = await resp.json().catch(() => ({}));
  return { ok: resp.ok, status: resp.status, body: body ?? {} };
}

// Read a newline-delimited JSON body, invoking onLine per non-empty line.
// Shared with call sites that keep an explicit remote NDJSON lane (the
// cross-origin peer session stream) so the reader exists exactly once.
async function daemonApiReadNdjsonBody(response, onLine) {
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  const handleLine = line => {
    const trimmed = line.trim();
    if (trimmed) onLine(trimmed);
  };
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split('\n');
    buffer = lines.pop() || '';
    for (const line of lines) handleLine(line);
  }
  buffer += decoder.decode();
  if (buffer.trim()) handleLine(buffer);
}

// Stream-lane twins over direct HTTP: the daemon's NDJSON endpoints emit
// the same event objects the tunnel's stream_event frames carry (the
// sessions stream predates the tunnel lane), so the adapter reads lines
// and feeds the same callbacks. There is no stream_end result on HTTP —
// resolve with null, exactly what consumers that track state from events
// expect. `end` fires like the tunnel's; `start` has no HTTP equivalent
// frame and is never synthesized.
async function daemonApiHttpStream(method, params, opts, onEvent) {
  const spec = DAEMON_API_HTTP_MAP[method];
  const { url } = daemonApiHttpTarget(spec, method, params);
  let resp;
  try {
    resp = await authedFetch(url, { method: spec.verb, signal: daemonApiHttpSignal(method, opts) });
  } catch (err) {
    throw daemonApiHttpError(err, method);
  }
  if (!resp.ok || !resp.body) {
    throw new DaemonApiError('http', method, null, `${method} returned HTTP ${resp.status}`, resp.status);
  }
  const emit = event => {
    // Same callback-shape tolerance as the tunnel's callStreamCallback.
    try {
      if (typeof onEvent === 'function') onEvent(event);
      else if (onEvent && typeof onEvent.event === 'function') onEvent.event(event);
    } catch (err) {
      console.warn('[daemon-api] stream callback failed', err);
    }
  };
  try {
    await daemonApiReadNdjsonBody(resp, line => emit(JSON.parse(line)));
  } catch (err) {
    throw daemonApiHttpError(err, method);
  }
  if (onEvent && typeof onEvent.end === 'function') {
    try { onEvent.end(null); } catch (err) { console.warn('[daemon-api] stream end callback failed', err); }
  }
  return null;
}

// ── Remote-http adapter (transport F4) ────────────────────────────────────
// Cross-daemon fleet calls: the anchor page applies access changes to
// OTHER daemons at independently verified direct-mTLS URLs — the
// fleet-CORS rows, with the browser's own identity to that origin (mTLS
// certificate) as the authentication. A rendezvous-controlled fleet-SNI
// URL is discovery-only and the daemon rejects this lane there. This
// daemon's `authedFetch` bearer stays out of the
// lane on purpose: nothing minted here carries authority there
// (trust-architecture: authority is only ever minted by the target
// daemon's local IAM). Naming the target names the transport — no tunnel
// is ever attempted, no fallback is ever taken, and a delivered error is
// final (design §5's F4 caveat: explicit remote-http, never fallback
// ambiguity). JSON lane only: every fleet-CORS twin is a JSON verb, so
// the bytes/upload/stream verbs refuse remote-http targets loudly.

// The normalized origin for a `{remoteHttp: origin}` target: '' when the
// target is not remote-http-shaped (null and string peer ids pass
// through to the tunnel adapters); a throw when the shape is right but
// the origin is not a usable absolute http(s) origin — a mis-built
// target must fail loudly, never drift into another lane.
function daemonApiRemoteHttpOrigin(target) {
  if (!target || typeof target !== 'object') return '';
  const raw = String(target.remoteHttp || '').trim();
  let origin = '';
  if (raw) {
    try {
      const url = new URL(raw);
      if (url.protocol === 'https:' || url.protocol === 'http:') origin = url.origin;
    } catch { /* rejected below */ }
  }
  if (!origin) {
    throw new TypeError(
      `daemonApi: object targets must carry an absolute http(s) remoteHttp origin (got ${raw || 'nothing'})`
    );
  }
  return origin;
}

// Fleet links cross networks the local per-method defaults (5 s for the
// access family) were never sized for — floor the composed timeout at
// the peer-dial default unless the caller sets one. Still never
// signal-less (§3.6).
function daemonApiRemoteHttpSignal(method, opts) {
  const timeout = Number(opts && opts.timeoutMs);
  const ms = Number.isFinite(timeout) && timeout > 0
    ? timeout
    : Math.max(daemonApiTimeoutMs(method, opts), 30000);
  return dashboardComposeFetchSignal(opts && opts.signal, ms);
}

async function daemonApiRemoteHttpRequest(origin, method, params, opts) {
  const spec = DAEMON_API_HTTP_MAP[method];
  if (!spec) {
    throw new DaemonApiError(
      'unavailable', method, origin,
      `${method} has no HTTP twin (remote-http targets are direct-HTTP only)`
    );
  }
  const { url, rest } = daemonApiHttpTarget(spec, method, params);
  const init = { method: spec.verb, mode: 'cors', signal: daemonApiRemoteHttpSignal(method, opts) };
  // Same body rule as the local adapter: mutation verbs carry leftover
  // params as the JSON body (no fleet-CORS DELETE twin exists today, but
  // the adapters must agree on the descriptor's semantics).
  if ((spec.verb === 'POST' || spec.verb === 'DELETE') && Object.keys(rest).length > 0) {
    init.headers = { 'Content-Type': 'application/json' };
    init.body = JSON.stringify(rest);
  }
  let resp;
  try {
    resp = await fetch(`${origin}${url}`, init);
  } catch (err) {
    throw daemonApiHttpError(err, method, origin);
  }
  const body = await resp.json().catch(() => ({}));
  return { ok: resp.ok, status: resp.status, body: body ?? {} };
}

// ── Tunnel adapters ───────────────────────────────────────────────────────
// Local and peer speak the same DashboardControlTransport protocol; the
// peer adapter resolves (or dials) the per-host connection first. A peer
// target never falls back to HTTP.
async function daemonApiPeerTransport(target, method) {
  let conn;
  try {
    conn = await peerDashboardControlConnectionForHost(target, { timeoutMs: 30000 });
  } catch (err) {
    throw daemonApiTunnelError(err, method, target);
  }
  if (!conn || !conn.canUseRpc()) {
    throw new DaemonApiError('transport', method, target, 'peer dashboard-control tunnel is unavailable');
  }
  return conn;
}

// Bounded-retry byte read over a tunnel (promoted
// dashboardRequestBytesWithRetry semantics): retries transport-level
// rejections only; a delivered error response ('http', via the envelope)
// and aborts propagate immediately. Reads only — bytes is a GET-lane verb.
async function daemonApiTunnelBytes(transport, method, params, opts, target) {
  const configured = Number(opts && opts.retries);
  const retries = Number.isFinite(configured) ? Math.max(0, configured) : 2;
  let attempt = 0;
  for (;;) {
    if (opts && opts.signal && opts.signal.aborted) {
      throw new DaemonApiError('abort', method, target, 'daemon api request aborted');
    }
    let result;
    try {
      result = await transport.requestBytes(method, params, daemonApiTunnelOptions(method, opts));
    } catch (err) {
      const classified = daemonApiTunnelError(err, method, target);
      if (classified.kind === 'abort' || attempt >= retries) throw classified;
      attempt += 1;
      await new Promise(resolve => setTimeout(resolve, 200 * attempt));
      continue;
    }
    return daemonApiBytesEnvelope(result, method, target);
  }
}

// ── The four verbs ────────────────────────────────────────────────────────
async function daemonApiRequest(method, params = {}, opts = {}) {
  const remoteOrigin = daemonApiRemoteHttpOrigin(opts.target);
  if (remoteOrigin) return daemonApiRemoteHttpRequest(remoteOrigin, method, params, opts);
  const target = opts.target || null;
  if (target) {
    const conn = await daemonApiPeerTransport(target, method);
    try {
      const payload = await conn.request(method, params, daemonApiTunnelOptions(method, opts));
      return daemonApiEnvelopeFromPayload(payload);
    } catch (err) {
      throw daemonApiTunnelError(err, method, target);
    }
  }
  const transport = dashboardControlTransport;
  let tunnelError = null;
  if (transport && transport.canUseRpc()) {
    try {
      const payload = await transport.request(method, params, daemonApiTunnelOptions(method, opts));
      return daemonApiEnvelopeFromPayload(payload);
    } catch (err) {
      tunnelError = daemonApiTunnelError(err, method, null);
      if (tunnelError.kind === 'abort') throw tunnelError;
    }
  }
  daemonApiEnsureHttpFallback(method, opts, tunnelError);
  return daemonApiHttpRequest(method, params, opts);
}

async function daemonApiBytes(method, params = {}, opts = {}) {
  const remoteOrigin = daemonApiRemoteHttpOrigin(opts.target);
  if (remoteOrigin) {
    throw new DaemonApiError(
      'unavailable', method, remoteOrigin,
      `${method}: remote-http targets serve JSON request() calls only (no fleet-CORS byte twins exist)`
    );
  }
  const target = opts.target || null;
  if (target) {
    const conn = await daemonApiPeerTransport(target, method);
    return daemonApiTunnelBytes(conn, method, params, opts, target);
  }
  const transport = dashboardControlTransport;
  let tunnelError = null;
  if (transport && transport.canUseRpc()) {
    try {
      return await daemonApiTunnelBytes(transport, method, params, opts, null);
    } catch (err) {
      const classified = daemonApiTunnelError(err, method, null);
      // Delivered responses ('http') and aborts are final — only a failed
      // lane consults the fallback policy.
      if (classified.kind === 'abort' || classified.kind === 'http') throw classified;
      tunnelError = classified;
    }
  }
  daemonApiEnsureHttpFallback(method, opts, tunnelError);
  return daemonApiHttpBytes(method, params, opts);
}

async function daemonApiUpload(method, params = {}, source, opts = {}) {
  const remoteOrigin = daemonApiRemoteHttpOrigin(opts.target);
  if (remoteOrigin) {
    throw new DaemonApiError(
      'unavailable', method, remoteOrigin,
      `${method}: remote-http targets serve JSON request() calls only (no fleet-CORS upload twins exist)`
    );
  }
  const target = opts.target || null;
  if (target) {
    const conn = await daemonApiPeerTransport(target, method);
    try {
      const payload = await conn.uploadBytes(method, params, source, daemonApiTunnelOptions(method, opts));
      return daemonApiEnvelopeFromPayload(payload);
    } catch (err) {
      throw daemonApiTunnelError(err, method, target);
    }
  }
  const transport = dashboardControlTransport;
  let tunnelError = null;
  if (transport && transport.canUseRpc()) {
    try {
      const payload = await transport.uploadBytes(method, params, source, daemonApiTunnelOptions(method, opts));
      return daemonApiEnvelopeFromPayload(payload);
    } catch (err) {
      tunnelError = daemonApiTunnelError(err, method, null);
      if (tunnelError.kind === 'abort') throw tunnelError;
    }
  }
  // Uploads are POST twins: the policy derivation refuses HTTP after any
  // tunnel attempt; a never-attempted upload may take the HTTP lane.
  daemonApiEnsureHttpFallback(method, opts, tunnelError);
  return daemonApiHttpUpload(method, params, source, opts);
}

async function daemonApiStream(method, params = {}, opts = {}, onEvent = {}) {
  const remoteOrigin = daemonApiRemoteHttpOrigin(opts.target);
  if (remoteOrigin) {
    throw new DaemonApiError(
      'unavailable', method, remoteOrigin,
      `${method}: remote-http targets serve JSON request() calls only (no fleet-CORS stream twins exist)`
    );
  }
  const target = opts.target || null;
  if (target) {
    const conn = await daemonApiPeerTransport(target, method);
    try {
      return await conn.stream(method, params, daemonApiTunnelOptions(method, opts), onEvent);
    } catch (err) {
      throw daemonApiTunnelError(err, method, target);
    }
  }
  const transport = dashboardControlTransport;
  let tunnelError = null;
  if (transport && transport.canUseRpc()) {
    try {
      return await transport.stream(method, params, daemonApiTunnelOptions(method, opts), onEvent);
    } catch (err) {
      tunnelError = daemonApiTunnelError(err, method, null);
      if (tunnelError.kind === 'abort') throw tunnelError;
    }
  }
  // Streams are GET twins: a failed tunnel stream may replay over the
  // NDJSON HTTP lane (consumers merge idempotent event objects; a
  // mid-stream restart re-delivers rows, exactly like the legacy
  // per-call-site fallbacks did). Connect mode never uses HTTP.
  daemonApiEnsureHttpFallback(method, opts, tunnelError);
  return daemonApiHttpStream(method, params, opts, onEvent);
}

// ── Availability (§3.4): one function, derived ────────────────────────────
// Inputs, in order: adapter connection state; the hello `features` list
// (distinguishes "daemon too old" from "denied"); the status
// `<method>_available` booleans (server-derived from its method table —
// false rolls denial and runtime-not-ready into one honest "no");
// connect-mode policy; HTTP-map presence (a direct dashboard with no
// tunnel still reports twinned methods as reachable), qualified by the
// row-presence probes for `probe`-gated entries. The scattered
// canUse*/`*Available` probes become one-line derivations over this at
// their family flips.
function daemonApiTunnelMethodAvailability(transport, method) {
  const spec = DAEMON_API_HTTP_MAP[method] || null;
  const status = transport.lastStatus || null;
  const features = Array.isArray(transport.controlFeatures) ? transport.controlFeatures : [];
  const lane = (spec && spec.lane) || 'json';
  const laneFeature = lane === 'bytes' ? 'byte_streams'
    : (lane === 'upload' ? 'upload_frames'
      : (lane === 'stream' ? 'stream_frames' : ''));
  if (laneFeature) {
    // Not every lane feature has a status boolean (stream_frames is
    // wire-level only), and pre-status daemons carry none — fall back to
    // the hello features list rather than reading absence as denial.
    const laneFlag = status ? status[`${laneFeature}_available`] : undefined;
    const laneOk = typeof laneFlag === 'boolean' ? laneFlag : features.includes(laneFeature);
    if (!laneOk) return { ok: false, reason: 'unsupported' };
  }
  const flag = status ? status[`${method}_available`] : undefined;
  if (typeof flag === 'boolean') {
    return flag ? { ok: true, reason: 'connected' } : { ok: false, reason: 'denied' };
  }
  if (features.length) {
    // Upload-only methods are not named in `features` — they advertise
    // through the upload_frames umbrella (CONTROL_ONLY_METHODS upload_only).
    const advertised = features.includes(method) || (lane === 'upload' && features.includes('upload_frames'));
    return advertised ? { ok: true, reason: 'connected' } : { ok: false, reason: 'unsupported' };
  }
  // Channel open but hello_ack/status not yet digested: optimistic, like
  // every existing call path (the request itself reports the truth).
  return { ok: true, reason: 'connected' };
}

function daemonApiAvailability(method, target = null) {
  const remoteOrigin = daemonApiRemoteHttpOrigin(target);
  if (remoteOrigin) {
    // A REMOTE origin's reachability is unknowable without a request;
    // the honest answer is lane presence — the descriptor names the
    // methods fleet daemons serve over independently verified direct HTTP.
    return DAEMON_API_HTTP_MAP[method]
      ? { ok: true, reason: 'http-only' }
      : { ok: false, reason: 'never' };
  }
  if (target) {
    const conn = peerDashboardControlConnectionsByHost.get(String(target));
    if (conn && conn.canUseRpc()) return daemonApiTunnelMethodAvailability(conn, method);
    // Peers have no HTTP lane by design: dialable-but-unconnected is a
    // down transport; an unavailable signaling lane can never be reached
    // from this dashboard at all.
    if (peerDashboardControlSignalAvailable(target)) return { ok: false, reason: 'transport-down' };
    return { ok: false, reason: 'never' };
  }
  const transport = dashboardControlTransport;
  if (transport && transport.canUseRpc()) return daemonApiTunnelMethodAvailability(transport, method);
  if (dashboardConnectModeEnabled()) return { ok: false, reason: 'transport-down' };
  const spec = DAEMON_API_HTTP_MAP[method];
  if (spec) {
    // Probe-gated twins (the transfer-jobs rows): a probed-absent daemon
    // answers the honest 'unsupported', exactly like a tunnel daemon too
    // old for the method. Un-probed stays optimistic — the same posture
    // as a tunnel whose hello_ack has not landed (the request itself
    // reports the truth), and lane DECISIONS await the probe first.
    if (spec.probe && daemonApiHttpProbeState(spec.probe) === 'absent') {
      return { ok: false, reason: 'unsupported' };
    }
    return { ok: true, reason: 'http-only' };
  }
  return { ok: false, reason: 'transport-down' };
}

// ── Descriptor checksum ───────────────────────────────────────────────────
// FNV-1a over the canonicalized descriptor — a cheap drift beacon the boot
// smoke can log and harnesses can compare across daemon/browser pairs.
function daemonApiDescriptorChecksum() {
  const canonical = Object.keys(DAEMON_API_HTTP_MAP).sort().map(name => {
    const spec = DAEMON_API_HTTP_MAP[name];
    return [name, spec.verb, spec.path, (spec.query || []).join(','), spec.lane || 'json', spec.encode || ''].join('|');
  }).join('\n');
  let hash = 0x811c9dc5;
  for (let i = 0; i < canonical.length; i += 1) {
    hash ^= canonical.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash.toString(16).padStart(8, '0');
}

// The facade. Frozen so a consumer can't monkey-patch a verb out from
// under the others; DAEMON_API_HTTP_MAP rides along read-only for
// harnesses and the F1 transfer adapters.
const daemonApi = Object.freeze({
  request: daemonApiRequest,
  bytes: daemonApiBytes,
  upload: daemonApiUpload,
  stream: daemonApiStream,
  availability: daemonApiAvailability,
  fallbackPolicy: daemonApiFallbackPolicy,
  ensureHttpProbe: daemonApiEnsureHttpProbe,
  httpProbeState: daemonApiHttpProbeState,
  httpMap: DAEMON_API_HTTP_MAP,
  descriptorChecksum: daemonApiDescriptorChecksum,
  Error: DaemonApiError,
});

// ── Legacy-transport shim instrumentation (F8a flip) ─────────────────────
// The three legacy helpers (`rpcOrHttp`/`jsonFetch` on DashboardTransport,
// `dashboardJsonFetch`) have zero in-repo product callers after the flip;
// they survive one soak week as warn-logging shims so any path the caller
// census missed surfaces in live dashboards instead of silently riding
// legacy semantics. Every shim invocation records here: one console.warn
// per distinct `helper:label` key (no spam loops — a polling caller warns
// once), every hit counts. `window.qa.transportShimHits()` is the soak
// verdict the validators and the F8b removal session query — an all-zero
// readback (total: 0) is the green light to delete the shims, the
// fake-Response builder (responseFromPayload), and the canUse* quartet.
// A dashboardJsonFetch call that delegates to DashboardTransport.jsonFetch
// records under BOTH helper names by design: the label names the missed
// method either way, and the pair tells the census which lane it rode.
const dashboardTransportShimState = {
  total: 0,
  byCaller: Object.create(null),
  warned: new Set(),
};

function dashboardTransportShimRecord(helper, label) {
  const key = `${helper}:${String(label || '(unlabeled)')}`;
  dashboardTransportShimState.total += 1;
  dashboardTransportShimState.byCaller[key] =
    (dashboardTransportShimState.byCaller[key] || 0) + 1;
  if (dashboardTransportShimState.warned.has(key)) return;
  dashboardTransportShimState.warned.add(key);
  // The first non-shim stack line names the concrete call site for the
  // census; captured only on the first hit per key, so it stays cheap.
  const stack = (new Error().stack || '').split('\n').slice(2, 4).join(' | ');
  console.warn(
    `[transport-shim] legacy ${helper}('${String(label || '')}') still has a live caller — `
    + `migrate it to daemonApi (transport F8a; the shim is deleted at F8b). ${stack}`
  );
}

// QA readback (window.qa convention): adapter states, an availability
// sample across the three lanes, and the descriptor checksum. Cheap and
// side-effect-free — availability() only reads connection state. The boot
// smoke asserts this probe exists and answers; nothing else consumes the
// facade in F0. transportShimHits is the F8a soak counter (see above).
window.qa = Object.assign(window.qa || {}, {
  transportShimHits: () => ({
    total: dashboardTransportShimState.total,
    byCaller: { ...dashboardTransportShimState.byCaller },
  }),
  daemonApi: () => {
    const transport = dashboardControlTransport;
    const peers = [];
    for (const [hostId, conn] of peerDashboardControlConnectionsByHost) {
      peers.push({ hostId, connected: Boolean(conn && conn.canUseRpc()) });
    }
    return {
      adapters: {
        localTunnel: {
          present: Boolean(transport),
          connected: Boolean(transport && transport.canUseRpc()),
          features: Array.isArray(transport && transport.controlFeatures)
            ? transport.controlFeatures.length
            : 0,
          statusBooleans: transport && transport.lastStatus
            ? Object.keys(transport.lastStatus).filter(key => key.endsWith('_available')).length
            : 0,
          lastErrorKind: (transport && transport.lastErrorKind) || '',
        },
        peerTunnels: peers,
        http: {
          connectMode: Boolean(dashboardConnectModeEnabled()),
          reachable: !dashboardConnectModeEnabled(),
          transfersProbe: daemonApiHttpProbeState('transfers'),
        },
        lane: typeof dashboardEventLaneQa === 'function' ? dashboardEventLaneQa() : null,
      },
      availability: {
        api_fs_stat: daemonApiAvailability('api_fs_stat'),
        api_fs_write: daemonApiAvailability('api_fs_write'),
        api_session_current_upload: daemonApiAvailability('api_session_current_upload'),
        api_sessions: daemonApiAvailability('api_sessions'),
        api_sessions_stream: daemonApiAvailability('api_sessions_stream'),
        // Probe-gated transfers sample (F1c): 'http-only' flips to
        // 'unsupported' once the rows probe answers absent.
        api_transfer_job_create: daemonApiAvailability('api_transfer_job_create'),
        // Tunnel-only custody sample (F6): 'denied' vs 'unsupported' vs
        // 'transport-down' is directly observable here — no HTTP twin
        // ever answers for it.
        api_credential_lease_status: daemonApiAvailability('api_credential_lease_status'),
        // WS-twin residue sample (F7): the display input-authority gate
        // the display slots and the input frame sender derive from.
        api_display_input_authority_request: daemonApiAvailability('api_display_input_authority_request'),
      },
      descriptor: {
        methods: Object.keys(DAEMON_API_HTTP_MAP).length,
        checksum: daemonApiDescriptorChecksum(),
      },
    };
  },
});

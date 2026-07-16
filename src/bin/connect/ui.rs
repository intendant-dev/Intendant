//! The served surface: landing/connect/trust/access pages and their HTML
//! builders, the embedded installers and brand assets, and health probes.

use super::*;

pub(crate) async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

/// The hosted installer, embedded
/// at build time so the service — hosted or self-hosted — serves the
/// installer that matches its own version:
///   curl -fsSL <origin>/install.sh | sh -s --
///
/// Served with this rendezvous' public origin injected as the default
/// `--connect` URL: fetching the installer from a rendezvous IS the opt-in,
/// and a fresh VPS has no other way to learn where to register — without
/// it the daemon comes up unregistered and hosted claiming dead-ends.
/// (A compiled-in default in the daemon would instead make every install
/// phone home to intendant.dev; serve-time injection keeps self-hosting
/// exact.) Explicit `--connect` / `-Connect` still wins over the default.
pub(crate) const INSTALL_SH: &str = include_str!("../../../scripts/install.sh");
pub(crate) const INSTALL_SH_CONNECT_DEFAULT: &str =
    r#"CONNECT_URL="${INTENDANT_CONNECT_RENDEZVOUS_URL:-}""#;

/// Only a plain URL charset may be spliced into the scripts — anything
/// else (quotes, spaces, `$`) could change what the shell parses. A
/// misconfigured origin falls back to serving the script verbatim.
pub(crate) fn connect_default_injectable(origin: &str) -> bool {
    !origin.is_empty()
        && origin
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '/' | '.' | '-' | '_'))
}

pub(crate) fn install_sh_body(public_origin: &str) -> String {
    if !connect_default_injectable(public_origin) {
        return INSTALL_SH.to_string();
    }
    INSTALL_SH.replacen(
        INSTALL_SH_CONNECT_DEFAULT,
        &format!(r#"CONNECT_URL="${{INTENDANT_CONNECT_RENDEZVOUS_URL:-{public_origin}}}""#),
        1,
    )
}

pub(crate) async fn install_sh(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    state.static_pages.install_sh.respond(&headers)
}

/// The Windows counterpart, for PowerShell:
///   & ([scriptblock]::Create((irm <origin>/install.ps1)))
pub(crate) const INSTALL_PS1: &str = include_str!("../../../scripts/install.ps1");
pub(crate) const INSTALL_PS1_CONNECT_DEFAULT: &str = "    [string]$Connect = \"\",";

pub(crate) fn install_ps1_body(public_origin: &str) -> String {
    if !connect_default_injectable(public_origin) {
        return INSTALL_PS1.to_string();
    }
    INSTALL_PS1.replacen(
        INSTALL_PS1_CONNECT_DEFAULT,
        &format!("    [string]$Connect = \"{public_origin}\","),
        1,
    )
}

pub(crate) async fn install_ps1(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    state.static_pages.install_ps1.respond(&headers)
}

/// The canonical Intendant mark, embedded so every page this binary serves
/// gets the real logo without a static root. `static/logo.svg` is the
/// macOS icon vector (macos-app/icon.svg) with the dock margin cropped in
/// viewBox space; the PNG fallback is rendered from it (`rsvg-convert -w 128`).
pub(crate) const LOGO_SVG: &str = include_str!("../../../static/logo.svg");
pub(crate) const BRAND_ICON_PNG: &[u8] = include_bytes!("../../../static/icon-128.png");
/// Connect's push-notification worker. This is the sole shared-static source
/// file the rendezvous exposes, and it is embedded at compile time under one
/// explicit route rather than reachable through a filesystem fallback.
pub(crate) const CONNECT_SERVICE_WORKER_JS: &str = include_str!("../../../static/sw.js");

pub(crate) async fn logo_svg() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        LOGO_SVG,
    )
}

pub(crate) async fn favicon_png() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        BRAND_ICON_PNG,
    )
}

pub(crate) async fn service_worker_js() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/javascript; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert("service-worker-allowed", HeaderValue::from_static("/"));
    (headers, CONNECT_SERVICE_WORKER_JS).into_response()
}

/// Product screenshots for the landing page, embedded like the installer so
/// every deployment serves visuals that match its own UI. Captured from a
/// staged local rig (daemon "atlas", account "@ada") — synthetic content only.
/// A table rather than a match so the artifact-transparency manifest
/// (transparency.rs) enumerates exactly what this route serves.
pub(crate) const LANDING_ASSETS: &[(&str, &[u8])] = &[
    ("hero.webp", include_bytes!("assets/landing-hero.webp")),
    ("video.webp", include_bytes!("assets/landing-video.webp")),
    ("vault.webp", include_bytes!("assets/landing-vault.webp")),
    (
        "station.webp",
        include_bytes!("assets/landing-station.webp"),
    ),
    ("claim.webp", include_bytes!("assets/landing-claim.webp")),
    ("phone.webp", include_bytes!("assets/landing-phone.webp")),
];

pub(crate) fn landing_asset_bytes(name: &str) -> Option<&'static [u8]> {
    LANDING_ASSETS
        .iter()
        .find(|(asset_name, _)| *asset_name == name)
        .map(|(_, bytes)| *bytes)
}

pub(crate) async fn landing_asset(AxumPath(name): AxumPath<String>) -> Response {
    match landing_asset_bytes(&name) {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "image/webp"),
                (header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(crate) async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    // Filesystem probe (and first-run dir creation) off the async workers:
    // a slow disk must degrade this probe, not the whole runtime.
    let parent = state.config.data_file.parent().map(Path::to_path_buf);
    let state_parent_ok = match parent {
        None => false,
        Some(parent) => tokio::task::spawn_blocking(move || {
            parent.exists() || std::fs::create_dir_all(&parent).is_ok()
        })
        .await
        .unwrap_or(false),
    };
    let ok = state_parent_ok;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "ok": ok,
            "state_parent": state_parent_ok,
        })),
    )
        .into_response()
}

const HTML_FRAME_ANCESTORS: &str = "frame-ancestors 'none'";

fn deny_html_framing(headers: &mut HeaderMap) {
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(HTML_FRAME_ANCESTORS),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
}

/// One startup-rendered page: shared bytes plus a strong ETag, served with
/// `Cache-Control: no-cache` so browsers revalidate cheaply (304) instead of
/// re-downloading tens of KB per visit.
pub(crate) struct StaticPage {
    body: axum::body::Bytes,
    etag: HeaderValue,
    content_type: HeaderValue,
    deny_framing: bool,
}

impl StaticPage {
    fn html(body: String) -> Self {
        Self::new(
            body,
            HeaderValue::from_static("text/html; charset=utf-8"),
            true,
        )
    }

    fn script(body: String) -> Self {
        Self::new(
            body,
            HeaderValue::from_static("text/plain; charset=utf-8"),
            false,
        )
    }

    fn new(body: String, content_type: HeaderValue, deny_framing: bool) -> Self {
        let etag = HeaderValue::from_str(&format!("\"{}\"", &sha256_hex(body.as_bytes())[..32]))
            .expect("hex etag is a valid header value");
        StaticPage {
            body: axum::body::Bytes::from(body),
            etag,
            content_type,
            deny_framing,
        }
    }

    fn respond(&self, request_headers: &HeaderMap) -> Response {
        let revalidated = request_headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.split(',').any(|candidate| {
                    let candidate = candidate.trim();
                    // RFC 9110: `If-None-Match: *` matches any current
                    // representation.
                    candidate == "*"
                        || candidate.trim_start_matches("W/")
                            == self.etag.to_str().unwrap_or_default()
                })
            });
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, self.content_type.clone());
        headers.insert(header::ETAG, self.etag.clone());
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        if self.deny_framing {
            deny_html_framing(&mut headers);
        }
        if revalidated {
            (StatusCode::NOT_MODIFIED, headers).into_response()
        } else {
            (headers, self.body.clone()).into_response()
        }
    }
}

/// Every page this binary serves is a pure function of the public origin
/// (pinned by the artifact-transparency determinism tests), so they are
/// rendered exactly once at startup instead of formatting a multi-KB
/// template per hit. The same builders feed the transparency manifest, so
/// served bytes and logged hashes cannot diverge.
pub(crate) struct StaticPages {
    landing: StaticPage,
    connect: StaticPage,
    access: StaticPage,
    trust: StaticPage,
    install_sh: StaticPage,
    install_ps1: StaticPage,
}

impl StaticPages {
    pub(crate) fn render(config: &ServiceConfig) -> Self {
        let origin = config.public_origin.as_str();
        StaticPages {
            landing: StaticPage::html(landing_ui_html(origin)),
            connect: StaticPage::html(connect_page_html(origin)),
            access: StaticPage::html(access_page_html(origin)),
            trust: StaticPage::html(trust_ui_html(origin)),
            install_sh: StaticPage::script(install_sh_body(origin)),
            install_ps1: StaticPage::script(install_ps1_body(origin)),
        }
    }
}

pub(crate) async fn landing_ui(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    state.static_pages.landing.respond(&headers)
}

/// The `/connect` page body — shared by the route handler and the
/// artifact-transparency manifest so both hash/serve identical bytes.
pub(crate) fn connect_page_html(origin: &str) -> String {
    connect_ui_html(origin, "Intendant Connect", "Rendezvous account")
}

/// The `/access` page body (see `connect_page_html`).
pub(crate) fn access_page_html(origin: &str) -> String {
    connect_ui_html(
        origin,
        "Intendant Access",
        "Rendezvous and fleet navigation",
    )
}

pub(crate) async fn connect_ui(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    state.static_pages.connect.respond(&headers)
}

pub(crate) async fn trust_ui(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    state.static_pages.trust.respond(&headers)
}

pub(crate) async fn access_ui(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    state.static_pages.access.respond(&headers)
}

/// The default Connect build is a directory, not a daemon-control client.
/// Keep both historical entry-point spellings as fail-closed redirects so
/// bookmarked or crafted `?connect=1&daemon_id=...` URLs cannot load the
/// dashboard SPA from the hosted origin.
pub(crate) async fn app_html() -> Response {
    Redirect::to("/connect").into_response()
}

/// No filesystem fallback exists in the default Connect binary. This is a
/// security boundary: the repo's static root contains the daemon dashboard
/// and authority-bearing control client, which hosted-origin JS must never
/// serve or activate.
pub(crate) async fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

pub(crate) fn trust_ui_html(origin: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>How trust works — Intendant Connect</title>
  <link rel="icon" type="image/svg+xml" href="/logo.svg">
  <link rel="icon" type="image/png" href="/favicon.png">
  <style>
    :root {{
      color-scheme: dark;
      --bg: #11111b; --top: #181825; --surface: #1e1e2e; --surface-2: #313244;
      --line: rgba(205, 214, 244, 0.09); --line-strong: rgba(205, 214, 244, 0.16);
      --text: #cdd6f4; --muted: #a6adc8; --muted-2: #6c7086;
      --accent: #89b4fa; --accent-hover: #74c7ec; --lavender: #b4befe;
      --ok: #a6e3a1; --warn: #f9e2af; --err: #f38ba8;
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: var(--bg); color: var(--text);
    }}
    * {{ box-sizing: border-box; }}
    body {{ margin: 0; min-height: 100vh; background-color: var(--bg); background-image: radial-gradient(1100px 520px at 50% -160px, rgba(137, 180, 250, .12) 0%, rgba(137, 180, 250, 0) 62%); background-attachment: fixed; }}
    a {{ color: var(--accent); }}
    a:hover {{ color: var(--accent-hover); }}
    code {{ color: var(--muted); font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; overflow-wrap: anywhere; }}
    header {{ border-bottom: 1px solid var(--line); background: rgba(24, 24, 37, .82); }}
    .topbar {{ width: min(760px, calc(100vw - 32px)); margin: 0 auto; min-height: 60px; display: flex; align-items: center; gap: 12px; }}
    .brand-mark {{ width: 30px; height: 30px; display: block; flex: 0 0 auto; }}
    .topbar a {{ color: var(--text); text-decoration: none; font-weight: 700; font-size: 15px; }}
    main {{ width: min(760px, calc(100vw - 32px)); margin: 0 auto; padding: 34px 0 72px; line-height: 1.62; font-size: 15px; }}
    h1 {{ font-size: 28px; letter-spacing: -.015em; line-height: 1.15; margin: 0 0 8px; }}
    .lede {{ color: var(--muted); font-size: 16px; margin: 0 0 26px; }}
    h2 {{ font-size: 18px; margin: 34px 0 8px; letter-spacing: -.01em; }}
    p {{ margin: 10px 0; color: var(--text); }}
    p.dim, li span {{ color: var(--muted); }}
    ol, ul {{ padding-left: 22px; margin: 10px 0; display: grid; gap: 8px; }}
    li strong {{ display: block; }}
    .card {{ border: 1px solid var(--line-strong); background: rgba(24, 24, 37, .6); border-radius: 12px; padding: 16px 18px; margin: 16px 0; }}
    .card.good {{ border-color: rgba(166, 227, 161, .35); }}
    .foot {{ margin-top: 34px; padding-top: 16px; border-top: 1px solid var(--line); color: var(--muted-2); font-size: 13px; }}
  </style>
</head>
<body>
  <header><div class="topbar"><img class="brand-mark" src="/logo.svg" alt=""><a href="/connect">Intendant Connect</a></div></header>
  <main>
    <h1>How trust works here</h1>
    <p class="lede">The short version: this service handles accounts, routes, availability, hosted code, installers, and optional encrypted push delivery. A daemon grant can only be minted by that daemon's local IAM.</p>

    <h2>What this service actually does</h2>
    <p>Connect <em>serves</em> this discovery client and its installers, <em>publishes</em> daemon routes and presence, <em>stores</em> client-signed fleet records, and <em>remembers</em> which routes an account linked. A link creates no daemon principal or grant. This hosted build and its fleet WebPKI names are discovery-only and cannot open a daemon control session; use a trusted local or independently verified direct-mTLS surface for access. No signed/notarized native release exists for this alpha.</p>

    <h2>"But I sign in with a passkey&hellip;"</h2>
    <p>A fair question: doesn&rsquo;t signing in give the server something it could use?</p>
    <p>The authenticator&rsquo;s passkey secret remains non-extractable: this service does not receive the private key and cannot use it at another origin. But this service controls the JavaScript running at this origin. Malicious replacement code could prompt for user verification, request a valid assertion or PRF evaluation, and then use or exfiltrate the resulting assertion, PRF output, or decrypted account state available to that page. The passkey authenticates you only for Connect account, route, and encrypted-metadata operations; none of those results authenticates to a daemon or grants daemon authority.</p>

    <h2>If this service turned malicious</h2>
    <ol>
      <li><strong>It could lie in introductions.</strong><span>It can alter account and routing metadata, substitute the daemon key at first introduction, or deny a route. A daemon signature checked by this page proves consistency with the key Connect linked; it is not an independent key pin, because this service supplies both the first key record and the browser code doing the check. Account assertions never authenticate to a daemon, and this hosted build has an immutable <code>role:none</code> ceiling: it cannot open a control session even if local state is edited to grant its browser key.</span></li>
      <li><strong>It could deny service.</strong><span>Connect controls availability for its account, route, presence, and push services. Denial does not add daemon authority, but it can hide or delay those updates.</span></li>
      <li><strong>It could serve malicious code or installers.</strong><span>Hosted code can misuse Connect account state and any decrypted vault or fleet data made available after a user gesture; a replaced installer can compromise what it installs. There is no hosted or fleet-name daemon-control session in the default product. Use a trusted local or independently verified direct-mTLS root surface; self-host or verify artifacts out of band if you do not trust this deployment. The current native artifact is unsigned development-only.</span></li>
    </ol>

    <div class="card good">
      <strong>The rule the protocol follows:</strong> authority records are minted and enforced only by the target daemon's local access control; the rendezvous API cannot mint one. Connect is still trusted for availability, account and route metadata, and the code and installers it serves. A malicious installer can compromise what it installs, so those are real limits rather than a claim that the service is powerless.
    </div>

    <h2>Notifications</h2>
    <p class="dim">Optional Web Push alerts ("your computer went offline") are composed from the polling presence this service already sees &mdash; no new knowledge &mdash; and each payload is encrypted to your browser&rsquo;s subscription, so the push relays in between carry ciphertext.</p>

    <h2>Names are checkable here</h2>
    <p class="dim">Every name binding this service hands out &mdash; which daemon key Connect recorded for a linked computer, handle creations, revocation lists, verified badges &mdash; is committed to an append-only transparency log. Your browser pins the signed tree head and re-verifies on every visit that history only ever grew. This makes later rewriting detectable; it does not independently validate the first key or the browser code served by this origin. Handles can carry <em>verified identity</em> badges (a DNS record or GitHub gist you control); verification is decoration, never authority. Dormant handles with no computers and no sign-ins are eventually freed &mdash; squatted names don&rsquo;t keep.</p>

    <h2>Organizations</h2>
    <p class="dim">Org membership is a document signed by the organization&rsquo;s own key, verified by each of its computers directly. This service stores at most the org&rsquo;s <em>revocation list</em>, also root-signed. A computer that has recorded a sequence high-water mark rejects rollback below it; a fresh computer can still be shown an older signed list. A malicious board can withhold or serve stale data, but cannot forge a newer valid list.</p>

    <h2>Verify all of this</h2>
    <p class="dim">The component is open and self-hostable: <a href="https://intendant-dev.github.io/Intendant/self-hosted-rendezvous.html" target="_blank" rel="noopener">run your own rendezvous</a>, read the <a href="https://intendant-dev.github.io/Intendant/trust-architecture.html" target="_blank" rel="noopener">full trust architecture</a>, or audit the <a href="https://github.com/intendant-dev/Intendant" target="_blank" rel="noopener">source</a>.</p>

    <div class="foot">This instance: <code>{origin}</code> &mdash; one deployment of an open component, not a chokepoint.</div>
  </main>
</body>
</html>"#
    )
}

pub(crate) const DOCS_URL: &str = "https://intendant-dev.github.io/Intendant/";
pub(crate) const REPO_URL: &str = "https://github.com/intendant-dev/Intendant";

/// The deployment advisor — the lead of the landing install section: four
/// questions -> one command per platform (sh or PowerShell, `--service` where it belongs)
/// plus an honest credential-setup plan for after the claim. A separate const so
/// its CSS/JS braces stay out of the page-level `format!`; it derives
/// the command from `location.origin` at runtime, so a self-hosted
/// rendezvous advertises its own installer here too. The default answers'
/// command is server-rendered into the terminal (via the
/// `__ADVISOR_DEFAULT_CMD__` placeholder) so the page works without JS
/// and the one-command story is visible before any click. Every question
/// is about the agent's machine. A browser needs no separate app install,
/// but daemon control still requires trusted certificate/profile enrollment.
pub(crate) const LANDING_ADVISOR_HTML: &str = r##"<div class="advisor" id="advisor">
        <style>
          .advisor { border: 1px solid var(--line); border-radius: var(--radius); background: rgba(30, 30, 46, .55); }
          .advbody { padding: 16px; display: grid; gap: 14px; }
          .advq { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
          /* Labels take their own line so option rows keep an even rhythm
             at any column width (no ragged orphan buttons). */
          .advq .ql { flex: 0 0 100%; font-size: 13.5px; color: var(--muted-2); }
          .advq button { background: transparent; border: 1px solid var(--line-strong); color: var(--muted); border-radius: 999px; padding: 5px 13px; font-size: 13px; cursor: pointer; }
          .advq button:hover { color: var(--text); border-color: var(--accent); }
          .advq button.on { background: var(--surface-2); color: var(--text); border-color: var(--accent); }
          .advout { border-top: 1px solid var(--line); padding-top: 14px; display: grid; gap: 10px; }
          .advout ul { margin: 0; padding-left: 20px; font-size: 14px; color: var(--muted); display: grid; gap: 6px; }
          .advout ul b { color: var(--text); }
          .advout ul:empty { display: none; }
        </style>
        <div class="advbody">
          <div class="advq" data-q="os">
            <span class="ql">OS on the agent's machine?</span>
            <button data-v="linux" class="on">Linux</button>
            <button data-v="macos">macOS</button>
            <button data-v="windows">Windows</button>
          </div>
          <div class="advq" data-q="box">
            <span class="ql">What kind of machine?</span>
            <button data-v="vps" class="on">A rented VPS</button>
            <button data-v="server">My own always-on machine</button>
            <button data-v="laptop">The machine I'm on now</button>
          </div>
          <div class="advq" data-q="fuel">
            <span class="ql">What will fuel it?</span>
            <button data-v="api" class="on">API keys</button>
            <button data-v="sub">Subscriptions (Codex, Claude Code)</button>
            <button data-v="both">Both</button>
          </div>
          <div class="advq" data-q="solo">
            <span class="ql">Keep working with your browser closed?</span>
            <button data-v="no" class="on">No — while I watch</button>
            <button data-v="yes">Yes — unattended runs</button>
          </div>
          <div class="advout">
            <div class="terminal">
              <div class="tbar">
                <span class="dot r"></span><span class="dot y"></span><span class="dot g"></span>
                <span class="bftitle" id="advtitle">fresh box — sh</span>
                <button onclick="navigator.clipboard&&navigator.clipboard.writeText(document.getElementById('advcmd').textContent)">copy</button>
              </div>
              <pre><span class="ps" id="advps">$ </span><span id="advcmd">__ADVISOR_DEFAULT_CMD__</span></pre>
            </div>
            <ul id="advplan"></ul>
            <p class="installnote" id="advnote"></p>
          </div>
        </div>
        <script>
        (function () {
          var pick = { os: 'linux', box: 'vps', fuel: 'api', solo: 'no' };
          function render() {
            // </> keep raw angle brackets out of the inline
            // script (the page-level invariant the tests pin).
            var svc = pick.box !== 'laptop';
            var cmd = pick.os === 'windows'
              ? '& ([scriptblock]::Create((irm ' + location.origin + '/install.ps1)))' + (svc ? ' -Service' : '')
              : 'curl -fsSL ' + location.origin + '/install.sh | sh -s --' + (svc ? ' --service' : '');
            document.getElementById('advps').textContent = pick.os === 'windows' ? 'PS> ' : '$ ';
            document.getElementById('advtitle').textContent = pick.os === 'windows' ? 'fresh box — PowerShell' : 'fresh box — sh';
            document.getElementById('advcmd').textContent = cmd;
            var plan = [];
            if (pick.box === 'laptop') {
              plan.push('<b>Local credentials work as-is.</b> A .env key remains supported. The daemon-store vault is available from a trusted local or independently verified direct-mTLS client. Connect implements blind account-vault storage, but this directory serves no vault client or delivery bridge.');
            } else {
              var watched = pick.solo === 'no';
              if (pick.fuel !== 'sub') {
                plan.push('<b>API keys:</b> configure .env on the box, or open the daemon through a trusted local/direct-mTLS client and grant a memory-only lease from its daemon-store vault. This hosted Connect tab cannot fuel the daemon or relay its calls.');
              }
              if (pick.fuel !== 'api') {
                plan.push('<b>Subscriptions:</b> establish auth through the signed client or another trusted direct surface. Access-token leases are short-lived; full-credential OAuth mode temporarily materializes a private auth home on the daemon for the lease window.');
                if (!watched) plan.push('<b>Unattended runs:</b> plan for local credential custody or an explicitly authorized trusted client. Closing this hosted tab changes no daemon credential state.');
              }
            }
            document.getElementById('advplan').innerHTML = plan.map(function (item) { return '<li>' + item + '</li>'; }).join('');
            var note = { vps: 'A deliberately keyless box outside a full-credential OAuth lease can avoid durable provider secrets. That is a deployment choice, not a promise made by Connect.',
                         server: 'Choose the daemon’s credential source deliberately: .env is durable; API-key leases are memory-only; full-credential OAuth leases temporarily write private auth files.',
                         laptop: 'Connect account-vault storage has no shipped client or trusted delivery bridge; use the daemon-store vault from a trusted surface.' }[pick.box];
            if (svc) {
              note += pick.os === 'windows'
                ? ' -Service installs a Task Scheduler entry (at boot when elevated, at logon otherwise) supervised by a built-in restart loop; the installer prints the log file the one-time claim code lands in. Run it from PowerShell.'
                : ' --service keeps the daemon alive past this SSH session via the platform’s own supervisor — systemd where present, launchd on macOS, cron plus the built-in supervisor elsewhere — and prints where the one-time claim code lands.';
            }

            document.getElementById('advnote').textContent = note;
          }
          Array.prototype.forEach.call(document.querySelectorAll('#advisor .advq'), function (row) {
            Array.prototype.forEach.call(row.querySelectorAll('button'), function (button) {
              button.addEventListener('click', function () {
                Array.prototype.forEach.call(row.querySelectorAll('button'), function (other) { other.classList.remove('on'); });
                button.classList.add('on');
                pick[row.getAttribute('data-q')] = button.getAttribute('data-v');
                render();
              });
            });
          });
          render();
        })();
        </script>
      </div>"##;

/// The public landing page at `/`. Deliberately static and dependency-free;
/// the install one-liner is origin-aware so a self-hosted rendezvous
/// advertises its own installer.
pub(crate) fn landing_ui_html(origin: &str) -> String {
    let install_cmd = format!("curl -fsSL {origin}/install.sh | sh -s --");
    // r## because the page contains fragment links (`href="#install"`),
    // whose `"#` would terminate a plain r#-string.
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Intendant — an operating environment for autonomous AI agents</title>
  <meta name="description" content="Give an AI agent a full machine — shell, files, display, voice — under layered human oversight. Your keys stay yours.">
  <link rel="icon" type="image/svg+xml" href="/logo.svg">
  <link rel="icon" type="image/png" href="/favicon.png">
  <style>
    :root {{
      color-scheme: dark;
      --bg: #11111b;
      --top: #181825;
      --surface: #1e1e2e;
      --surface-2: #313244;
      --line: rgba(205, 214, 244, 0.09);
      --line-strong: rgba(205, 214, 244, 0.16);
      --text: #cdd6f4;
      --muted: #a6adc8;
      --muted-2: #6c7086;
      --accent: #89b4fa;
      --accent-hover: #74c7ec;
      --accent-ink: #11111b;
      --lavender: #b4befe;
      --ok: #a6e3a1;
      --warn: #f9e2af;
      --radius: 12px;
      --shadow: 0 18px 50px rgba(0, 0, 0, .35);
    }}
    * {{ box-sizing: border-box; }}
    html {{ scroll-behavior: smooth; }}
    @media (prefers-reduced-motion: reduce) {{ html {{ scroll-behavior: auto; }} }}
    body {{
      margin: 0;
      background:
        radial-gradient(1200px 500px at 70% -10%, rgba(137, 180, 250, .10), transparent 60%),
        radial-gradient(900px 420px at 10% 0%, rgba(180, 190, 254, .07), transparent 55%),
        var(--bg);
      color: var(--text);
      font: 16px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    }}
    a {{ color: var(--accent); text-decoration: none; }}
    a:hover {{ color: var(--accent-hover); }}
    .wrap {{ max-width: 1080px; margin: 0 auto; padding: 0 22px; }}
    header {{
      display: flex; align-items: center; justify-content: space-between;
      padding: 18px 0; flex-wrap: wrap; gap: 10px 18px;
    }}
    .mark {{ display: flex; align-items: center; font-weight: 700; letter-spacing: .3px; font-size: 17px; color: var(--text); }}
    .mark img {{ width: 26px; height: 26px; display: block; margin-right: 9px; }}
    .mark span {{ color: var(--accent); }}
    .mark .pill-alpha {{
      margin-left: 10px; padding: 2px 9px; border: 1px solid var(--line-strong);
      border-radius: 999px; font-size: 10.5px; font-weight: 700;
      letter-spacing: .12em; text-transform: uppercase; color: var(--muted-2);
    }}
    nav {{ display: flex; gap: 14px 20px; align-items: center; font-size: 14.5px; flex-wrap: wrap; }}
    nav a {{ color: var(--muted); white-space: nowrap; }}
    nav a:hover {{ color: var(--text); }}
    .btn {{
      display: inline-block; padding: 9px 18px; border-radius: 999px;
      background: var(--accent); color: var(--accent-ink); font-weight: 600;
      border: 1px solid transparent;
    }}
    .btn:hover {{ background: var(--accent-hover); color: var(--accent-ink); }}
    .btn.ghost {{ background: transparent; color: var(--text); border-color: var(--line-strong); }}
    .btn.ghost:hover {{ border-color: var(--accent); color: var(--accent); }}
    .hero {{ padding: 64px 0 10px; text-align: center; }}
    .hero h1 {{
      margin: 0 auto 18px; font-size: clamp(31px, 5.5vw, 49px); line-height: 1.13;
      letter-spacing: -.6px; max-width: 21ch;
    }}
    .hero h1 em {{ font-style: normal; color: var(--lavender); }}
    .hero p {{ margin: 0 auto 28px; font-size: 17.5px; color: var(--muted); max-width: 680px; }}
    .cta {{ display: flex; gap: 12px; flex-wrap: wrap; justify-content: center; }}
    /* Framed product shots */
    .heroshot {{ position: relative; margin: 92px 0 0; }}
    .heroshot::before {{
      content: ""; position: absolute; inset: -60px 0 auto; height: 340px;
      background: radial-gradient(640px 260px at 50% 20%, rgba(137, 180, 250, .16), transparent 70%);
      pointer-events: none;
    }}
    .browserframe {{
      position: relative; background: var(--top); border: 1px solid var(--line-strong);
      border-radius: 14px; box-shadow: var(--shadow); overflow: hidden;
    }}
    .bfbar {{
      display: flex; align-items: center; gap: 7px; padding: 10px 14px;
      border-bottom: 1px solid var(--line);
    }}
    .dot {{ width: 10px; height: 10px; border-radius: 50%; }}
    .dot.r {{ background: rgba(243, 139, 168, .75); }}
    .dot.y {{ background: rgba(249, 226, 175, .75); }}
    .dot.g {{ background: rgba(166, 227, 161, .75); }}
    .bftitle {{
      margin-left: 8px; font: 12.5px/1 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      color: var(--muted-2); letter-spacing: .3px;
    }}
    .browserframe img, .shot img {{ display: block; width: 100%; height: auto; }}
    .shotcaption {{
      margin: 14px auto 0; max-width: 740px; text-align: center;
      font-size: 13.5px; color: var(--muted-2);
    }}
    /* The tour: alternating text/screenshot rows */
    .tour {{ padding: 84px 0 0; }}
    .trow {{
      display: grid; grid-template-columns: minmax(0, .92fr) minmax(0, 1.08fr);
      gap: 48px; align-items: center; padding: 30px 0;
    }}
    .trow.rev .txt {{ order: 2; }}
    .eyebrow {{
      font-size: 12px; font-weight: 700; letter-spacing: .14em;
      text-transform: uppercase; color: var(--accent); margin-bottom: 10px;
    }}
    .trow h3 {{ margin: 0 0 12px; font-size: 23px; letter-spacing: -.3px; }}
    .trow .txt p {{ margin: 0; font-size: 15.5px; color: var(--muted); }}
    .shot {{
      background: var(--top); border: 1px solid var(--line-strong);
      border-radius: 12px; box-shadow: var(--shadow); overflow: hidden;
    }}
    .shotnote {{ margin-top: 10px; font-size: 13px; color: var(--muted-2); }}
    /* Custody: the two fueling modes, told by what travels */
    .fuelmap {{ margin-top: 16px; display: grid; gap: 9px; }}
    .fuelrow {{ display: flex; gap: 10px; align-items: baseline; flex-wrap: wrap; }}
    .fueltag {{
      flex: 0 0 auto; min-width: 96px; text-align: center; padding: 2px 8px;
      border: 1px solid var(--line-strong); border-radius: 6px;
      font: 700 10.5px/1.7 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      letter-spacing: .08em; text-transform: uppercase; color: var(--accent);
    }}
    .fuelrow:last-child .fueltag {{ color: var(--ok); }}
    .fuelflow {{ flex: 1; min-width: 230px; font-size: 13px; color: var(--muted-2); }}
    .fuelflow em {{ font-style: normal; color: var(--muted); }}
    .fuelflow .fx {{ opacity: .65; padding: 0 1px; }}
    /* The phone row: a bezel, not a browser frame */
    .phonepic {{ display: grid; justify-items: center; }}
    .phonepic .shotnote {{ text-align: center; }}
    .phoneframe {{
      width: min(280px, 72vw); padding: 10px; border-radius: 44px;
      background: #0d0d15; border: 1px solid var(--line-strong);
      box-shadow: var(--shadow);
    }}
    .phoneframe img {{ display: block; width: 100%; height: auto; border-radius: 34px; }}
    section h2 {{ font-size: 24px; margin: 0 0 20px; letter-spacing: -.3px; }}
    .sectionlede {{ margin: -10px 0 24px; font-size: 15px; color: var(--muted); max-width: 660px; }}
    section.features {{ padding: 78px 0 0; }}
    .grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(230px, 1fr)); gap: 14px; }}
    .card {{
      border: 1px solid var(--line); border-radius: var(--radius);
      background: var(--surface); padding: 18px 18px 16px;
    }}
    .card h3 {{ margin: 0 0 8px; font-size: 16px; }}
    .card p {{ margin: 0; font-size: 14px; color: var(--muted); }}
    /* Install */
    /* Sits directly under the hero: "how do I use it" is the first answer
       the page gives, so it gets hero-adjacent spacing, not section spacing. */
    .install-section {{ padding: 46px 0 0; }}
    .igrid {{ display: grid; grid-template-columns: minmax(0, 1.15fr) minmax(0, .85fr); gap: 30px; align-items: start; }}
    .terminal {{
      background: var(--top); border: 1px solid var(--line-strong);
      border-radius: var(--radius); box-shadow: var(--shadow); overflow: hidden;
    }}
    .tbar {{
      display: flex; align-items: center; gap: 7px; padding: 9px 14px;
      border-bottom: 1px solid var(--line);
    }}
    .tbar .bftitle {{ flex: 1; }}
    .tbar button {{
      background: transparent; border: 1px solid var(--line-strong); color: var(--muted);
      border-radius: 6px; padding: 3px 10px; font-size: 12px; cursor: pointer;
    }}
    .tbar button:hover {{ color: var(--text); border-color: var(--accent); }}
    .terminal pre {{
      margin: 0; padding: 16px 18px;
      font: 13.5px/1.7 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      color: var(--ok);
      white-space: pre-wrap; overflow-wrap: break-word;
    }}
    .terminal pre .ps {{ color: var(--muted-2); user-select: none; }}
    .installnote {{ margin: 14px 2px 0; font-size: 13px; color: var(--muted-2); }}
    .steps {{ display: grid; gap: 12px; }}
    .step {{
      border: 1px solid var(--line); border-radius: var(--radius);
      background: rgba(30, 30, 46, .55); padding: 14px 16px; font-size: 14px; color: var(--muted);
    }}
    .step b {{ display: block; color: var(--text); margin-bottom: 4px; font-size: 14.5px; }}
    .step .n {{ color: var(--accent); font-weight: 700; margin-right: 6px; }}
    .whyname {{ padding: 78px 0 0; }}
    .whyname p {{ max-width: 65ch; margin: 0; font-size: 15.5px; color: var(--muted); line-height: 1.65; }}
    .whyname p strong {{ color: var(--text); font-weight: 600; }}
    .trustrow {{ padding: 78px 0 20px; }}
    .trustrow .card {{ background: rgba(166, 227, 161, .05); border-color: rgba(166, 227, 161, .18); }}
    footer {{
      margin-top: 64px; padding: 26px 0 40px; border-top: 1px solid var(--line);
      display: flex; justify-content: space-between; gap: 14px; flex-wrap: wrap;
      font-size: 13.5px; color: var(--muted-2);
    }}
    footer nav {{ gap: 16px; }}
    @media (max-width: 920px) {{
      .hero {{ padding-top: 46px; }}
      /* minmax(0, …) everywhere: a bare 1fr keeps the min-content floor and
         lets wide content (the install one-liner) stretch the page. */
      .trow {{ grid-template-columns: minmax(0, 1fr); gap: 16px; padding: 24px 0; }}
      .trow.rev .txt {{ order: 0; }}
      .tour {{ padding-top: 60px; }}
      .igrid {{ grid-template-columns: minmax(0, 1fr); }}
      section.features, .whyname, .trustrow {{ padding-top: 56px; }}
      .install-section {{ padding-top: 36px; }}
      .heroshot {{ margin-top: 64px; }}
    }}
  </style>
</head>
<body>
  <div class="wrap">
    <header>
      <div class="mark"><img src="/logo.svg" alt="">intendant<span>.dev</span><span class="pill-alpha">alpha</span></div>
      <nav>
        <a href="/trust">How trust works</a>
        <a href="{DOCS_URL}">Docs</a>
        <a href="{REPO_URL}">GitHub</a>
        <a href="#install">Install</a>
        <a class="btn ghost" href="/connect">Sign in</a>
      </nav>
    </header>

    <section class="hero">
      <h1>Give an AI agent a full machine — <em>under your oversight</em></h1>
      <p>
        Intendant is an open-source operating environment for autonomous AI
        agents: a shell, files, a display it can see and control, voice, and
        phone calls — with layered human supervision. It runs its own agent
        loop, supervises Codex and Claude Code as managed backends, and is
        portable across OpenAI, Anthropic, and Gemini. The agent's machine
        can run macOS, Linux, or Windows. Route discovery needs only this
        browser tab; control uses local presence or a browser enrolled for
        independently verified direct mTLS by a trusted daemon owner. No
        signed/notarized native release exists for this alpha.
      </p>
      <div class="cta">
        <a class="btn" href="/connect">Open Connect</a>
        <a class="btn ghost" href="#install">Install a daemon</a>
      </div>
    </section>

    <section class="install-section" id="install">
      <h2>Stand up a daemon in about ninety seconds</h2>
      <p class="sectionlede">
        Four answers about the machine the agent will live on, and the exact
        command appears. You can discover it from your phone without a separate
        app; controlling the daemon still requires trusted certificate or
        profile enrollment.
      </p>
      <div class="igrid">
        {advisor}
        <div>
          <div class="steps">
            <div class="step"><b><span class="n">1</span>Install</b>
              One command installs the daemon. Code served here is part of the installer trust boundary.</div>
            <div class="step"><b><span class="n">2</span>Link</b>
              Enter its twelve-word one-time claim code to add the route to your account. This grants no access.</div>
            <div class="step"><b><span class="n">3</span>Establish owner</b>
              Use the machine's local console or independently verified direct mTLS to establish root outside hosted-origin JavaScript. No signed/notarized native release exists for this alpha.</div>
            <div class="step"><b><span class="n">4</span>Fuel</b>
              Configure credentials from a trusted daemon surface. Connect implements blind account-vault storage but serves no vault client or daemon-delivery bridge in this build.</div>
          </div>
          <p class="installnote">
            New here? <a href="/connect">Sign in</a> to link the route for
            discovery. The link does not create a daemon principal or grant;
            establish access from a trusted local or independently verified direct-mTLS
            surface. This hosted build remains discovery-only.
          </p>
        </div>
      </div>
    </section>

    <section class="heroshot">
      <div class="browserframe">
        <div class="bfbar">
          <span class="dot r"></span><span class="dot y"></span><span class="dot g"></span>
          <span class="bftitle">atlas — Intendant dashboard</span>
        </div>
        <img src="/assets/landing/hero.webp" width="2200" height="1192" fetchpriority="high"
             alt="The Intendant dashboard's Activity feed: an agent diagnoses a failing nightly job with an auto-approved tail command, proposes a one-line diff to jobs/rollup.py, waits for an approval-gated backfill run, and reports the verified result.">
      </div>
      <p class="shotcaption">
        The Activity feed on an authorized daemon: autonomy is a dial, approvals
        are explicit, and every command, diff, and decision is logged and
        replayable.
      </p>
    </section>

    <section class="tour">
      <div class="trow">
        <div class="txt">
          <div class="eyebrow">The desktop</div>
          <h3>A real desktop, watched</h3>
          <p>The agent gets a display it can see and drive — a browser, a
          terminal, whatever the task needs — and you watch it stream live
          over WebRTC. Input stays yours to share: take control at any
          moment, annotate what you see, record what happened.</p>
        </div>
        <div class="pic">
          <div class="shot">
            <img loading="lazy" src="/assets/landing/video.webp" width="2000" height="1119"
                 alt="The dashboard's Video tab streaming a live agent desktop over WebRTC: a browser and a terminal scrolling a build, with view-only, annotate, record, and take-control affordances.">
          </div>
          <div class="shotnote">Watching atlas's display, live — view-only until you hand input over.</div>
        </div>
      </div>

      <div class="trow rev">
        <div class="txt">
          <div class="eyebrow">Mission control</div>
          <h3>Every agent, one canvas</h3>
          <p>Station renders the whole machine live — sessions, approvals,
          context budgets, changes, and worktrees orbiting one WebGPU canvas.
          The same daemon is operable from the CLI and MCP, and a glance away
          from your phone in an enrolled browser.</p>
        </div>
        <div class="pic">
          <div class="shot">
            <img loading="lazy" src="/assets/landing/station.webp" width="2000" height="1014"
                 alt="The Station tab: a radar-style WebGPU control room showing live nodes for peers, sessions, activity, context, changes, view, controls, and worktrees.">
          </div>
          <div class="shotnote">Station — the fleet and every session's state, rendered live.</div>
        </div>
      </div>

      <div class="trow">
        <div class="txt">
          <div class="eyebrow">Credential custody</div>
          <h3>Custody machinery, with a boundary</h3>
          <p>Intendant implements sealed vault storage, time-boxed leases,
          and client egress for authorized control sessions. The Connect
          account-vault backend and a daemon's own vault are separate today.
          This directory does not serve the vault client or crypto worker and
          has no daemon-control channel, so it cannot create or unseal account
          entries, grant a lease, or relay a provider call. Use a
          trusted direct client for those mechanisms, or use
          the daemon's local credential configuration.</p>
          <div class="fuelmap">
            <div class="fuelrow"><span class="fueltag">trusted client</span>
              <span class="fuelflow">daemon-store vault <span class="fx">→</span> authorized lease or relay <span class="fx">→</span> daemon workload</span></div>
            <div class="fuelrow"><span class="fueltag">Connect tab</span>
              <span class="fuelflow">account-vault API/storage only <em>(no shipped client or daemon bridge)</em></span></div>
          </div>
        </div>
        <div class="pic">
          <div class="shot">
            <img loading="lazy" src="/assets/landing/vault.webp" width="1800" height="975"
                 alt="The trusted daemon dashboard's credential vault panel, showing masked entries and time-boxed lease controls.">
          </div>
          <div class="shotnote">Shown on an authorized daemon surface; Connect cannot invoke these controls.</div>
        </div>
      </div>

      <div class="trow rev">
        <div class="txt">
          <div class="eyebrow">Arrival</div>
          <h3>Link a machine with twelve words</h3>
          <p>Start the daemon anywhere and it prints a one-time claim code.
          Enter it here to link the route to your account for discovery. The
          link changes no daemon IAM and grants no machine access; ownership
          starts only from a trusted local or independently verified direct-mTLS surface.</p>
        </div>
        <div class="pic">
          <div class="shot">
            <img loading="lazy" src="/assets/landing/claim.webp" width="1800" height="635"
             alt="Intendant Connect: a linked computer named atlas shown online with uptime history, next to the add-a-computer flow that accepts a twelve-word one-time claim code.">
          </div>
          <div class="shotnote">atlas, discoverable seconds after its one-time code was entered.</div>
        </div>
      </div>

      <div class="trow">
        <div class="txt">
          <div class="eyebrow">The client</div>
          <h3>Zero-install discovery; trusted enrollment for control</h3>
          <p>Link and discover a daemon from this browser tab without client
          software. To approve a diff, watch the live desktop, or run mission
          control, use local loopback access or first enroll that browser for
          independently verified direct mTLS from a trusted owner surface.
          After enrollment the dashboard remains browser-based and carries
          only the authority that daemon granted to the authenticated
          principal. Installing a client certificate or profile is a real
          enrollment step, not a zero-install claim.</p>
        </div>
        <div class="pic phonepic">
          <div class="phoneframe">
            <img loading="lazy" src="/assets/landing/phone.webp" width="780" height="1688"
                 alt="The same Intendant session on a phone: the Activity feed showing the agent's diff, an approval-gated backfill command, and the verified result — driven entirely from a mobile browser.">
          </div>
          <div class="shotnote">The rollup fix from above — same session, held in one hand.</div>
        </div>
      </div>
    </section>

    <section class="features">
      <h2>What's in the box</h2>
      <div class="grid">
        <div class="card">
          <h3>Bring your own agent</h3>
          <p>Codex and Claude Code run as managed backends — under the
          same oversight, autonomy dial, and session logging as the
          native agent loop.</p>
        </div>
        <div class="card">
          <h3>Your keys stay yours</h3>
          <p>Sealed vaults, expiring API-key leases, and client egress are
          available on authorized daemon sessions. Connect can store an
          opaque account-vault envelope, but this directory serves neither its
          vault client nor a path that could deliver it to a daemon.
          Local .env credentials remain supported, and full-credential OAuth
          leases temporarily materialize private auth files.</p>
        </div>
        <div class="card">
          <h3>Multiple trusted interfaces</h3>
          <p>Use the web dashboard for visual control, CLI or MCP for
          automation, and live voice or phone for conversation. A remote
          browser enrolled for direct mTLS by a trusted owner runs the web
          client there, phone included, without a separate app install; that
          remote browser still needs certificate or profile enrollment.</p>
        </div>
        <div class="card">
          <h3>A fleet, not a box</h3>
          <p>Daemons federate: shared displays, cross-machine sessions, and
          organization-signed access — all enforced locally by each daemon's
          own IAM, never by this service.</p>
        </div>
      </div>
    </section>

    <section class="whyname">
      <h2>Why “Intendant”</h2>
      <p>In a theater, performers play and conductors orchestrate — the
      <strong>Intendant</strong> runs the house: who gets the stage, which
      productions run, on whose authority, with the books open. Here agents
      perform, orchestrators conduct (Codex and Claude Code as guest
      conductors), and the Intendant runs the house and answers to you —
      houses federate, companies tour on signed contracts, house rules always
      win: a network of agentic networks.</p>
    </section>

    <section class="trustrow">
      <h2>Built to be distrusted</h2>
      <div class="grid">
        <div class="card">
          <h3>The daemon is the authority mint</h3>
          <p>Connect is trusted for the code and installers it serves,
          availability, accounts, routes, fleet metadata, and optional push delivery. Its rendezvous
          protocol cannot mint a daemon grant, but a replaced installer can
          compromise what it installs, and malicious hosted code can misuse
          Connect account or decrypted browser state available after a gesture. You can
          <a href="/trust">read exactly what it can and cannot do</a>,
          or run your own.</p>
        </div>
        <div class="card">
          <h3>The sandbox never holds keys</h3>
          <p>Inside each daemon, the sandboxed process that executes
          commands never sees an API key, and the process that talks to
          model providers never executes commands. A hijacked conversation
          can't steal credentials; a hijacked shell can't phone home
          through the model — by construction, not by policy.</p>
        </div>
      </div>
    </section>

    <footer>
      <div>Intendant — open source, self-hostable, provider-agnostic.</div>
      <nav>
        <a href="/trust">Trust</a>
        <a href="/install.sh">install.sh</a>
        <a href="{DOCS_URL}">Docs</a>
        <a href="{REPO_URL}">GitHub</a>
      </nav>
    </footer>
  </div>
</body>
</html>"##,
        // Server-render the default answers' command (Linux VPS ⇒ --service)
        // so the terminal shows a real, origin-aware one-liner before any
        // JS runs; render() redraws the same text on load.
        advisor = LANDING_ADVISOR_HTML.replace(
            "__ADVISOR_DEFAULT_CMD__",
            &format!("{install_cmd} --service")
        ),
    )
}

pub(crate) fn connect_ui_html(origin: &str, product_title: &str, account_subtitle: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{product_title}</title>
  <link rel="icon" type="image/svg+xml" href="/logo.svg">
  <link rel="icon" type="image/png" href="/favicon.png">
  <style>
    :root {{
      color-scheme: dark;
      --bg: #11111b;
      --top: #181825;
      --surface: #1e1e2e;
      --surface-2: #313244;
      --surface-3: #45475a;
      --line: rgba(205, 214, 244, 0.09);
      --line-strong: rgba(205, 214, 244, 0.16);
      --text: #cdd6f4;
      --muted: #a6adc8;
      --muted-2: #6c7086;
      --accent: #89b4fa;
      --accent-hover: #74c7ec;
      --accent-ink: #11111b;
      --lavender: #b4befe;
      --ok: #a6e3a1;
      --warn: #f9e2af;
      --err: #f38ba8;
      --focus: #f9e2af;
      --shadow: 0 18px 50px rgba(0, 0, 0, .35);
      --radius: 12px;
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: var(--bg);
      color: var(--text);
    }}
    * {{ box-sizing: border-box; }}
    html {{ min-height: 100%; }}
    body {{ margin: 0; min-height: 100vh; background-color: var(--bg); background-image: radial-gradient(1100px 520px at 50% -160px, rgba(137, 180, 250, .14) 0%, rgba(137, 180, 250, 0) 62%), radial-gradient(ellipse at 50% -12%, #1e1e2e 0%, #11111b 72%); background-attachment: fixed; background-repeat: no-repeat; }}
    button, input {{ font: inherit; }}
    button {{ height: 38px; padding: 0 15px; color: var(--accent-ink); background: var(--accent); border: 1px solid transparent; border-radius: 8px; font-weight: 700; cursor: pointer; transition: background .16s ease, border-color .16s ease, color .16s ease, transform .12s ease, box-shadow .16s ease; white-space: nowrap; }}
    button:hover:not(:disabled) {{ background: var(--accent-hover); transform: translateY(-1px); box-shadow: 0 6px 18px rgba(137, 180, 250, .25); }}
    button:focus-visible, input:focus-visible, a:focus-visible, summary:focus-visible {{ outline: 2px solid var(--focus); outline-offset: 2px; border-radius: 6px; }}
    button.secondary {{ color: var(--text); background: var(--surface-2); border-color: var(--line-strong); }}
    button.secondary:hover:not(:disabled) {{ background: var(--surface-3); box-shadow: none; }}
    button.ghost {{ color: var(--muted); background: transparent; border-color: var(--line); }}
    button.ghost:hover:not(:disabled) {{ color: var(--text); background: var(--surface-2); box-shadow: none; }}
    button.danger {{ color: var(--err); background: rgba(243, 139, 168, .08); border-color: rgba(243, 139, 168, .45); }}
    button.danger:hover:not(:disabled) {{ background: rgba(243, 139, 168, .16); box-shadow: none; }}
    button.linklike {{ height: auto; padding: 0; color: var(--accent); background: none; border: 0; font-weight: 700; }}
    button.linklike:hover:not(:disabled) {{ color: var(--accent-hover); transform: none; box-shadow: none; text-decoration: underline; }}
    button:disabled {{ opacity: .58; cursor: default; transform: none; box-shadow: none; }}
    input {{ width: 100%; min-width: 0; height: 42px; padding: 9px 12px; color: var(--text); background: rgba(17, 17, 27, .8); border: 1px solid var(--line-strong); border-radius: 8px; transition: border-color .16s ease; }}
    input:hover {{ border-color: rgba(205, 214, 244, .26); }}
    input::placeholder {{ color: var(--muted-2); }}
    a {{ color: var(--accent); }}
    a:hover {{ color: var(--accent-hover); }}
    code {{ color: var(--muted); font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; overflow-wrap: anywhere; }}

    header {{ border-bottom: 1px solid var(--line); background: rgba(24, 24, 37, .82); backdrop-filter: blur(10px); position: sticky; top: 0; z-index: 5; }}
    .topbar {{ width: min(1180px, calc(100vw - 32px)); margin: 0 auto; min-height: 64px; display: flex; align-items: center; justify-content: space-between; gap: 18px; }}
    .brand {{ display: flex; align-items: center; gap: 12px; min-width: 0; }}
    .brand-mark {{ width: 34px; height: 34px; display: block; flex: 0 0 auto; }}
    .brand h1 {{ font-size: 17px; line-height: 1.15; margin: 0; }}
    .brand-sub {{ color: var(--muted-2); font-size: 12px; margin-top: 2px; }}
    .top-actions {{ display: flex; align-items: center; gap: 9px; }}
    .session-chip {{ display: inline-flex; align-items: center; gap: 8px; min-height: 32px; padding: 0 12px; border: 1px solid var(--line-strong); border-radius: 999px; background: var(--surface); color: var(--text); font-size: 13px; font-weight: 700; }}
    .session-chip .dot {{ width: 7px; height: 7px; border-radius: 50%; background: var(--ok); }}

    main.shell {{ width: min(1180px, calc(100vw - 32px)); margin: 0 auto; padding: 26px 0 56px; display: grid; gap: 18px; animation: rise .35s ease; }}
    @keyframes rise {{ from {{ opacity: 0; transform: translateY(6px); }} to {{ opacity: 1; transform: none; }} }}
    @media (prefers-reduced-motion: reduce) {{ main.shell {{ animation: none; }} button:hover:not(:disabled) {{ transform: none; }} }}

    /* ── Signed out: hero ── */
    body.signed-out main.shell {{ width: min(560px, calc(100vw - 32px)); padding-top: 7vh; }}
    .hero {{ text-align: center; display: grid; gap: 14px; justify-items: center; padding: 8px 0 22px; }}
    .hero-mark {{ width: 58px; height: 58px; display: block; border-radius: 16px; box-shadow: var(--shadow); }}
    .hero-title {{ font-size: 32px; line-height: 1.12; margin: 6px 0 0; letter-spacing: -.015em; }}
    .hero-sub {{ color: var(--muted); font-size: 15px; line-height: 1.55; margin: 0; max-width: 46ch; }}
    .auth-card {{ border: 1px solid var(--line-strong); background: rgba(24, 24, 37, .72); border-radius: var(--radius); box-shadow: var(--shadow); padding: 22px; display: grid; gap: 14px; }}
    .auth-row {{ display: flex; gap: 9px; }}
    .auth-row input {{ flex: 1 1 auto; }}
    .auth-row button {{ height: 42px; flex: 0 0 auto; }}
    .auth-alt {{ color: var(--muted); font-size: 13px; display: flex; gap: 6px; align-items: baseline; }}
    .auth-note {{ font-size: 12.5px; line-height: 1.55; color: var(--muted-2); }}
    .auth-note a {{ color: var(--muted); }}
    .auth-note a:hover {{ color: var(--accent); }}
    .feature-strip {{ list-style: none; margin: 6px 0 0; padding: 0; display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }}
    .feature-strip li {{ border: 1px solid var(--line); border-radius: 10px; background: rgba(24, 24, 37, .5); padding: 12px 13px; display: grid; gap: 4px; }}
    .feature-strip strong {{ font-size: 13px; }}
    .feature-strip span {{ color: var(--muted-2); font-size: 12px; line-height: 1.45; }}
    body.signed-in #auth {{ display: none; }}

    /* ── Signed in: computers ── */
    .section-head {{ display: flex; align-items: baseline; justify-content: space-between; gap: 14px; padding: 4px 2px 0; }}
    .section-head h2 {{ font-size: 20px; margin: 0; letter-spacing: -.01em; }}
    .section-head .sub {{ color: var(--muted-2); font-size: 13px; }}
    .computer-grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 14px; align-items: start; }}
    .computer-grid.empty {{ grid-template-columns: minmax(300px, 460px); justify-content: center; }}
    .computer-card {{ min-width: 0; border: 1px solid var(--line-strong); background: rgba(24, 24, 37, .72); border-radius: var(--radius); box-shadow: var(--shadow); padding: 18px; display: grid; gap: 12px; align-content: start; transition: border-color .16s ease, transform .16s ease; }}
    .computer-card:hover {{ border-color: rgba(205, 214, 244, .24); }}
    .computer-head {{ display: flex; align-items: center; gap: 10px; min-width: 0; }}
    .computer-dot {{ width: 9px; height: 9px; border-radius: 50%; background: var(--muted-2); flex: 0 0 auto; }}
    .computer-dot.ok {{ background: var(--ok); box-shadow: 0 0 8px rgba(166, 227, 161, .6); }}
    .computer-name {{ min-width: 0; display: grid; gap: 2px; }}
    .computer-name strong {{ font-size: 15px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
    .computer-name .sub {{ color: var(--muted-2); font-size: 12px; }}
    .computer-actions {{ display: flex; gap: 8px; flex-wrap: wrap; }}
    .route-only {{ flex: 1 1 180px; align-self: center; color: var(--muted-2); font-size: 12px; line-height: 1.4; }}
    .presence {{ display: grid; gap: 5px; }}
    .presence-bars {{ display: flex; gap: 2px; align-items: flex-end; height: 14px; }}
    .presence-bars span {{ flex: 1 1 auto; min-width: 2px; height: 5px; border-radius: 1px; background: var(--surface-3); }}
    .presence-bars span.on {{ height: 14px; background: var(--ok); opacity: .75; }}
    .presence-label {{ color: var(--muted-2); font-size: 11px; }}
    .computer-card details {{ border-top: 1px solid var(--line); padding-top: 10px; }}
    .computer-card summary {{ color: var(--muted-2); font-size: 12px; font-weight: 700; cursor: pointer; list-style: none; }}
    .computer-card summary::before {{ content: '▸ '; }}
    .computer-card details[open] summary::before {{ content: '▾ '; }}
    .kv {{ display: grid; gap: 8px; margin-top: 10px; }}
    .kv .k {{ color: var(--muted-2); font-size: 11px; font-weight: 800; text-transform: uppercase; letter-spacing: .04em; }}
    .kv code {{ display: block; font-size: 12px; padding: 7px 9px; border: 1px solid var(--line); border-radius: 6px; background: rgba(17, 17, 27, .55); }}
    .kv .danger-row {{ margin-top: 4px; }}
    .add-card {{ border-style: dashed; background: rgba(24, 24, 37, .45); }}
    .add-card h3 {{ margin: 0; font-size: 15px; }}
    .steps {{ margin: 0; padding: 0 0 0 18px; color: var(--muted); font-size: 13px; line-height: 1.55; display: grid; gap: 6px; }}
    .steps code {{ font-size: 12px; }}
    label {{ display: block; color: var(--muted); font-size: 12px; font-weight: 700; margin-bottom: 7px; }}
    .status {{ min-height: 18px; color: var(--muted); font-size: 13px; line-height: 1.4; overflow-wrap: anywhere; }}
    .status.status-ok {{ color: var(--ok); }}
    .status.status-err {{ color: var(--err); }}
    .status.status-warn {{ color: var(--warn); }}
    .empty-hint {{ color: var(--muted-2); font-size: 13px; }}

    /* ── Saved places + advanced ── */
    section.panel {{ min-width: 0; border: 1px solid var(--line-strong); background: rgba(24, 24, 37, .72); border-radius: var(--radius); box-shadow: var(--shadow); }}
    .panel-header {{ padding: 15px 18px; border-bottom: 1px solid var(--line); display: flex; align-items: center; justify-content: space-between; gap: 14px; }}
    .panel-header h2 {{ font-size: 14px; margin: 0; }}
    .panel-header .sub {{ color: var(--muted-2); font-size: 12px; margin-top: 3px; }}
    .panel-body {{ padding: 16px 18px; }}
    .place-row {{ display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 11px 0; border-bottom: 1px solid var(--line); }}
    .place-row:first-child {{ padding-top: 0; }}
    .place-row:last-child {{ border-bottom: 0; padding-bottom: 0; }}
    .place-main {{ min-width: 0; display: grid; gap: 3px; }}
    .place-main strong {{ font-size: 13.5px; }}
    .place-main .sub {{ color: var(--muted-2); font-size: 12px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
    .place-actions {{ display: flex; gap: 8px; flex: 0 0 auto; }}
    .place-actions button {{ height: 32px; padding: 0 12px; font-size: 12.5px; }}
    .pill {{ display: inline-flex; align-items: center; gap: 6px; width: fit-content; min-height: 24px; padding: 0 10px; border-radius: 999px; background: var(--surface-2); color: var(--muted); border: 1px solid var(--line); font-size: 12px; font-weight: 750; }}
    .pill.ok {{ color: var(--ok); border-color: rgba(166, 227, 161, .4); background: rgba(166, 227, 161, .09); }}
    .pill.warn {{ color: var(--warn); border-color: rgba(249, 226, 175, .35); background: rgba(249, 226, 175, .08); }}
    .pill .dot {{ width: 6px; height: 6px; border-radius: 50%; background: currentColor; }}
    details.advanced {{ border: 1px solid var(--line); border-radius: var(--radius); background: rgba(24, 24, 37, .4); }}
    details.advanced > summary {{ list-style: none; cursor: pointer; padding: 14px 18px; color: var(--muted); font-size: 13px; font-weight: 750; display: flex; align-items: center; gap: 8px; }}
    details.advanced > summary::before {{ content: '▸'; color: var(--muted-2); }}
    details.advanced[open] > summary::before {{ content: '▾'; }}
    details.advanced > summary .hint {{ color: var(--muted-2); font-weight: 500; }}
    .advanced-body {{ border-top: 1px solid var(--line); padding: 18px; display: grid; gap: 22px; }}
    .advanced-block {{ display: grid; gap: 10px; }}
    .advanced-block > h3 {{ margin: 0; font-size: 13px; }}
    .advanced-block > .sub {{ color: var(--muted-2); font-size: 12.5px; line-height: 1.5; margin-top: -6px; }}
    .user-id-row {{ display: flex; gap: 8px; align-items: center; }}
    .user-id-row code {{ flex: 1 1 auto; min-width: 0; color: var(--text); font-size: 12px; padding: 7px 9px; border: 1px solid var(--line); border-radius: 6px; background: rgba(17, 17, 27, .55); }}
    .user-id-row button {{ height: 30px; padding: 0 10px; font-size: 12px; flex: 0 0 auto; }}
    .metric-row {{ display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }}
    .org-row {{ display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 10px 0; border-bottom: 1px solid var(--line); }}
    .org-row:first-child {{ padding-top: 0; }}
    .org-row:last-child {{ border-bottom: 0; padding-bottom: 0; }}
    .org-main {{ min-width: 0; display: grid; gap: 3px; }}
    .org-main strong {{ font-size: 13.5px; }}
    .org-main .sub {{ color: var(--muted-2); font-size: 12px; }}
    .org-side {{ display: flex; gap: 8px; align-items: center; flex: 0 0 auto; }}
    .pill.err {{ color: var(--err); border-color: rgba(243, 139, 168, .4); background: rgba(243, 139, 168, .08); }}
    .audit {{ display: grid; }}
    .event {{ padding: 11px 0; border-bottom: 1px solid var(--line); font-size: 13px; }}
    .event:first-child {{ padding-top: 0; }}
    .event:last-child {{ border-bottom: 0; padding-bottom: 0; }}
    .event-line {{ display: flex; justify-content: space-between; gap: 12px; align-items: baseline; }}
    .event-name {{ font-weight: 750; }}
    .event time {{ color: var(--muted); font-size: 12px; white-space: nowrap; }}
    .event code {{ display: inline-block; margin-top: 3px; font-size: 12px; }}
    .hidden {{ display: none !important; }}
    .handle {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-weight: 700; }}

    @media (max-width: 700px) {{
      /* The bar wraps instead of letting the squeezed title paint under
         the session chip: brand keeps the first row, actions take the
         next when space runs out. */
      .topbar {{ min-height: auto; padding: 12px 0; flex-wrap: wrap; row-gap: 8px; }}
      .brand h1 {{ font-size: 16px; white-space: nowrap; }}
      .brand-sub {{ display: none; }}
      .top-actions {{ margin-left: auto; flex-wrap: wrap; justify-content: flex-end; }}
      .feature-strip {{ grid-template-columns: 1fr; }}
      .hero-title {{ font-size: 26px; }}
      .auth-row {{ flex-direction: column; }}
      .place-row {{ flex-direction: column; align-items: stretch; }}
      .place-actions button {{ flex: 1 1 auto; }}
    }}
  </style>
</head>
<body class="signed-out">
  <header>
    <div class="topbar">
      <div class="brand">
        <img class="brand-mark" src="/logo.svg" alt="">
        <div>
        <h1>{product_title}</h1>
          <div class="brand-sub">{account_subtitle}</div>
        </div>
      </div>
      <div class="top-actions">
        <span id="session-chip" class="session-chip hidden"><span class="dot" aria-hidden="true"></span><span id="session-chip-handle"></span></span>
        <button id="refresh" class="ghost hidden">Refresh</button>
        <button id="logout" class="ghost hidden">Sign out</button>
      </div>
    </div>
  </header>
  <main class="shell">
    <!-- ── Signed out: landing ── -->
    <section id="auth">
      <div class="hero">
        <img class="hero-mark" src="/logo.svg" alt="">
        <h2 class="hero-title">Your computers, anywhere.</h2>
        <p class="hero-sub">Sign in with a passkey to find machines linked to your account. Linking is discovery only: daemon control still requires loopback or independently verified direct mTLS and a grant approved through a trusted root surface. No signed/notarized native release exists for this alpha.</p>
      </div>
      <div class="auth-card">
        <div>
          <label for="account">Account handle</label>
          <div class="auth-row">
            <input id="account" autocomplete="username webauthn" autocapitalize="none" spellcheck="false" placeholder="your-handle">
            <button id="login">Sign in</button>
          </div>
        </div>
        <div id="invite-row" class="hidden">
          <label for="invite-code">Invite code</label>
          <input id="invite-code" autocomplete="off" autocapitalize="none" spellcheck="false" placeholder="registration is invite-only during the alpha">
        </div>
        <div id="invite-note" class="auth-note hidden">
          Intendant is in invite-only alpha &mdash; creating an account needs an
          invite right now. No code yet? Follow the project on
          <a href="{REPO_URL}" target="_blank" rel="noopener">GitHub</a>,
          or run your own rendezvous (below) &mdash; self-hosting is never gated.
        </div>
        <div id="auth-actions" class="auth-alt">
          <span>New here?</span>
          <button id="register" class="linklike">Create your account with a passkey</button>
        </div>
        <div id="auth-status" class="status" role="status"></div>
      </div>
      <ul class="feature-strip">
        <li><strong>Passkeys only</strong><span>No passwords. Your devices already sync the key.</span></li>
        <li><strong>Honest limits</strong><span>Connect serves this code and handles accounts, routes, availability, fleet metadata, and optional push delivery; daemons alone mint access. <a href="/trust">See the complete trust boundary</a>.</span></li>
        <li><strong>Self-hostable</strong><span>Run your own rendezvous &mdash; <a href="https://intendant-dev.github.io/Intendant/self-hosted-rendezvous.html" target="_blank" rel="noopener">read how</a>.</span></li>
      </ul>
    </section>

    <!-- ── Signed in: computers ── -->
    <section id="manage" class="hidden">
      <div class="section-head">
        <h2>Your computers</h2>
        <div id="who" class="sub"></div>
      </div>
      <div style="height: 12px"></div>
      <div class="computer-grid">
        <div id="computer-cards" style="display: contents"></div>
        <div class="computer-card add-card">
          <h3>Add a computer</h3>
          <ol class="steps">
            <li>On that machine, start <code>intendant</code> with Connect enabled &mdash; it prints a 12&#8209;word one-time claim code in its log.</li>
            <li>Enter the code here to link its route to this account for discovery.</li>
          </ol>
          <div>
            <label for="claim-code">One-time claim code</label>
            <input id="claim-code" autocomplete="off" spellcheck="false" placeholder="twelve words printed by the daemon">
            <div class="sub">Discovery only: linking grants no machine access and changes no daemon IAM. Single use. We will never ask for a password, API key, recovery phrase, private key, or passkey secret here.</div>
          </div>
          <button id="claim">Connect it</button>
          <div id="claim-status" class="status" role="status"></div>
        </div>
      </div>
    </section>

    <!-- ── Signed in: saved places (only when any) ── -->
    <section id="fleet-section" class="panel hidden">
      <div class="panel-header">
        <div>
          <h2>Saved places</h2>
          <div class="sub">Routes this account remembers across your browsers; target daemons enforce local IAM</div>
        </div>
      </div>
      <div class="panel-body">
        <div id="fleet-rows"></div>
      </div>
    </section>

    <!-- ── Signed in: the power drawer ── -->
    <details id="advanced" class="advanced hidden">
      <summary>Advanced <span class="hint">&mdash; account identity, organizations, sync encryption, audit trail</span></summary>
      <div class="advanced-body">
        <div class="advanced-block" id="session-card">
          <h3>Account</h3>
          <div class="metric-row">
            <span class="pill"><span id="session-handle" class="handle"></span></span>
            <span id="session-passkeys" class="pill"></span>
            <span id="enc-pill" class="pill"></span>
          </div>
          <div class="sub">This user id identifies Connect account, route, and audit metadata. It never authenticates to a daemon. Browser identity keys are metadata-only in this alpha; use loopback or independently verified direct mTLS for control.</div>
          <div class="user-id-row">
            <code id="session-user-id"></code>
            <button id="copy-user-id" class="ghost" type="button">Copy</button>
          </div>
        </div>
        <div class="advanced-block" id="orgs-block">
          <h3>Organizations</h3>
          <div class="sub">Signed membership-document records already stored in this browser origin. Connect does not present them to daemons. Human browser-key subjects are record-only in this alpha; usable access still requires a trusted mTLS or peer identity.</div>
          <div id="org-rows"></div>
        </div>
        <div class="advanced-block">
          <h3>What this account can and cannot do</h3>
          <div class="sub">It is rendezvous and navigation only &mdash; it grants nothing by itself. Every daemon decides access through its own local IAM. Daemon-signed link acknowledgements are checked against the key Connect recorded, which proves consistency with that directory record but is not an independent first-introduction pin: Connect also serves the record and this browser code. Private fields in Saved places sync end&#8209;to&#8209;end encrypted when your passkey supports PRF. <a href="/trust">The full story.</a></div>
        </div>
        <div class="advanced-block" id="identity-block">
          <h3>Verified identity</h3>
          <div class="sub">Optionally prove this handle is yours by publishing a claim you control. Verification is decoration &mdash; keys stay the identity &mdash; and every verified badge is committed to this service&rsquo;s public transparency log. Your claim line: <code id="attest-claim"></code></div>
          <div class="metric-row" id="attest-badges"></div>
          <div class="kv-row">
            <input id="attest-domain" autocomplete="off" spellcheck="false" placeholder="example.com &mdash; needs TXT at _intendant.example.com">
            <button id="attest-dns-btn" class="ghost">Verify domain</button>
          </div>
          <div class="kv-row">
            <input id="attest-gist" autocomplete="off" spellcheck="false" placeholder="https://gist.githubusercontent.com/&lt;you&gt;/&hellip;/raw &mdash; containing the claim line">
            <button id="attest-github-btn" class="ghost">Verify GitHub</button>
          </div>
          <div id="attest-status" class="sub"></div>
        </div>
        <div class="advanced-block" id="log-block">
          <h3>Transparency log</h3>
          <div class="sub">Every name binding this service hands out (which daemon key Connect recorded for a linked computer, handle creations, revocation lists, badges) is committed to an append-only log. Your browser pins the signed tree head and re-verifies consistency on every visit &mdash; later rewriting is detectable, but the first key introduction and browser code are still trusted to this Connect deployment.</div>
          <div class="metric-row"><span id="log-pill" class="pill">checking&hellip;</span><button id="log-reset-trust" class="ghost hidden" title="Discard the pinned tree head and trust the log's current signing key from now on. Only do this if you expected the operator to rotate the key.">Reset trust</button></div>
        </div>
        <div class="advanced-block" id="push-block">
          <h3>Notifications</h3>
          <div class="sub">Get a notification on this browser when one of your computers goes offline or comes back &mdash; and, if you opt in, when an agent is stuck waiting on you (a command approval or a question) with no dashboard open. Request alerts carry only the kind of request plus the computer and session names, never what the agent is doing. All alerts are delivered encrypted to this browser alone.</div>
          <div class="metric-row">
            <span id="push-status" class="pill">checking&hellip;</span>
            <button id="push-enable" class="secondary hidden">Enable on this browser</button>
            <button id="push-disable" class="ghost hidden">Disable</button>
            <button id="push-test" class="ghost hidden">Send a test</button>
          </div>
          <div class="metric-row hidden" id="push-flags">
            <label><input type="checkbox" id="push-presence-flag"> computer offline/online</label>
            <label><input type="checkbox" id="push-requests-flag"> agent waiting on you</label>
          </div>
        </div>
        <div class="advanced-block" id="audit-section">
          <h3>Audit</h3>
          <div class="sub">Recent account activity on this rendezvous.</div>
          <div id="audit" class="audit"></div>
        </div>
        <div class="advanced-block">
          <h3>Self-host</h3>
          <div class="sub">This origin (<code>{origin}</code>) is one instance of an open component. <a href="https://intendant-dev.github.io/Intendant/self-hosted-rendezvous.html" target="_blank" rel="noopener">Run your own</a> and point your daemons at it.</div>
        </div>
      </div>
    </details>
  </main>
<script>
const $ = id => document.getElementById(id);
const state = {{ user: null, daemons: [], fleetTargets: [], csrfToken: '' }};
function setStatus(id, text, kind = '') {{
  const el = $(id);
  el.textContent = text || '';
  el.className = 'status' + (kind ? ' status-' + kind : '');
}}

function setBusy(id, busy) {{
  const el = $(id);
  if (!el) return;
  el.disabled = Boolean(busy);
}}

async function api(path, options = {{}}) {{
  const headers = {{
    'content-type': 'application/json',
    ...(options.headers || {{}}),
  }};
  if (state.csrfToken && !headers['x-intendant-csrf']) {{
    headers['x-intendant-csrf'] = state.csrfToken;
  }}
  const resp = await fetch(path, {{
    ...options,
    headers,
  }});
  const body = await resp.json().catch(() => ({{}}));
  if (!resp.ok || body.ok === false) throw new Error(body.error || `HTTP ${{resp.status}}`);
  return body;
}}

function b64uToBuf(value) {{
  const text = String(value || '').replace(/-/g, '+').replace(/_/g, '/');
  const padded = text.padEnd(Math.ceil(text.length / 4) * 4, '=');
  const bin = atob(padded);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) out[i] = bin.charCodeAt(i);
  return out.buffer;
}}

function bufToB64u(value) {{
  const bytes = new Uint8Array(value || new ArrayBuffer(0));
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '');
}}

function publicKeyOptions(start) {{
  const options = start.options && (start.options.publicKey || start.options);
  if (!options) throw new Error('missing WebAuthn options');
  options.challenge = b64uToBuf(options.challenge);
  if (options.user?.id) options.user.id = b64uToBuf(options.user.id);
  for (const cred of options.excludeCredentials || []) cred.id = b64uToBuf(cred.id);
  for (const cred of options.allowCredentials || []) cred.id = b64uToBuf(cred.id);
  return options;
}}

function registrationCredentialJSON(credential) {{
  return {{
    id: credential.id,
    clientDataJSON: bufToB64u(credential.response.clientDataJSON),
    attestationObject: bufToB64u(credential.response.attestationObject),
    transports: credential.response.getTransports ? credential.response.getTransports() : [],
  }};
}}

function authenticationCredentialJSON(credential) {{
  return {{
    id: credential.id,
    clientDataJSON: bufToB64u(credential.response.clientDataJSON),
    authenticatorData: bufToB64u(credential.response.authenticatorData),
    signature: bufToB64u(credential.response.signature),
    userHandle: credential.response.userHandle ? bufToB64u(credential.response.userHandle) : null,
  }};
}}

// Fleet-sync encryption: evaluate the WebAuthn PRF extension during the
// passkey ceremony and stash the per-tab secret used for private fleet fields.
// The second output reserves compatibility with the account-vault envelope
// format; this directory does not ship that vault client or its crypto worker.
// Separate PRF domains keep the two designs from sharing key material. The
// server never sees either output.
const FLEET_PRF_SALT = new TextEncoder().encode('intendant-fleet-sync-v1');
const VAULT_PRF_SALT = new TextEncoder().encode('intendant-vault-v1');

function prfExtensions() {{
  return {{ prf: {{ eval: {{ first: FLEET_PRF_SALT, second: VAULT_PRF_SALT }} }} }};
}}

function stashPrfSecret(credential) {{
  try {{
    const results = credential.getClientExtensionResults?.();
    const toB64u = buf => {{
      const bytes = new Uint8Array(buf);
      let bin = '';
      for (const b of bytes) bin += String.fromCharCode(b);
      return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    }};
    const first = results?.prf?.results?.first;
    if (!first) return;
    sessionStorage.setItem('intendant_fleet_prf_v1', toB64u(first));
    // Older authenticators may evaluate only one salt; the vault then
    // falls back to its legacy fleet-secret derivation client-side.
    const second = results?.prf?.results?.second;
    if (second) sessionStorage.setItem('intendant_vault_prf_v1', toB64u(second));
  }} catch (err) {{
    console.warn('PRF secret unavailable:', err?.message || err);
  }}
}}

async function createPasskey() {{
  const account = $('account').value.trim();
  if (!account) throw new Error('Account handle is required');
  setBusy('register', true);
  setStatus('auth-status', 'Waiting for passkey', '');
  try {{
    const start = await api('/api/auth/register/start', {{
      method: 'POST',
      body: JSON.stringify({{
        account_name: account,
        invite_code: ($('invite-code')?.value || '').trim(),
      }}),
    }});
    const credential = await navigator.credentials.create({{ publicKey: {{ ...publicKeyOptions(start), extensions: prfExtensions() }} }});
    stashPrfSecret(credential);
    const done = await api('/api/auth/register/finish', {{
      method: 'POST',
      body: JSON.stringify({{
        flow_id: start.flow_id,
        credential: registrationCredentialJSON(credential),
      }}),
    }});
    state.user = done.user;
    state.csrfToken = done.csrf_token || state.csrfToken;
    setStatus('auth-status', 'Signed in', 'ok');
    await refreshAll();
  }} finally {{
    setBusy('register', false);
  }}
}}

async function login() {{
  const account = $('account').value.trim();
  if (!account) throw new Error('Account handle is required');
  setBusy('login', true);
  setStatus('auth-status', 'Waiting for passkey', '');
  try {{
    const start = await api('/api/auth/login/start', {{
      method: 'POST',
      body: JSON.stringify({{ account_name: account }}),
    }});
    const credential = await navigator.credentials.get({{ publicKey: {{ ...publicKeyOptions(start), extensions: prfExtensions() }} }});
    stashPrfSecret(credential);
    const done = await api('/api/auth/login/finish', {{
      method: 'POST',
      body: JSON.stringify({{
        flow_id: start.flow_id,
        credential: authenticationCredentialJSON(credential),
      }}),
    }});
    state.user = done.user;
    state.csrfToken = done.csrf_token || state.csrfToken;
    setStatus('auth-status', 'Signed in', 'ok');
    await refreshAll();
  }} finally {{
    setBusy('login', false);
  }}
}}

/* Mirrors the daemon/service normalize_claim_code: lowercase alphanumeric
   runs joined by '-'. */
function normalizeClaimCode(input) {{
  return String(input || '')
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter(Boolean)
    .join('-');
}}

async function sha256B64uOfText(text) {{
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  return bufToB64u(digest);
}}

async function claimDaemon() {{
  const claimCode = $('claim-code').value.trim();
  if (!claimCode) throw new Error('One-time claim code is required');
  setBusy('claim', true);
  setStatus('claim-status', 'Waiting for daemon route acknowledgment', '');
  try {{
    const normalized = normalizeClaimCode(claimCode);
    if (!normalized) throw new Error('One-time claim code is required');
    // Hash-only submission avoids sending the plaintext code in this API
    // request. The code links Connect route metadata only.
    const start = await api('/api/claims/claim', {{
      method: 'POST',
      body: JSON.stringify({{ claim_code_hash: await sha256B64uOfText(normalized) }}),
    }});
    const deadline = Date.now() + 65000;
    while (Date.now() < deadline) {{
      await new Promise(resolve => setTimeout(resolve, 750));
      const status = await api(`/api/claims/${{encodeURIComponent(start.claim_id)}}`);
      if (status.result?.status === 'approved') {{
        setStatus(
          'claim-status',
          `Linked ${{status.result.daemon_id}} to this account for discovery. No machine access was granted. Establish owner access directly on the machine or through an independently verified direct-mTLS daemon connection.`,
          'ok'
        );
        $('claim-code').value = '';
        await refreshAll();
        return;
      }}
      if (status.result?.status === 'rejected') {{
        throw new Error(status.result.error || 'claim rejected');
      }}
    }}
    throw new Error('claim timed out');
  }} finally {{
    setBusy('claim', false);
  }}
}}

/* Read (never create) this origin's browser identity key fingerprint so
   stored org documents can be badged as bound to this browser or not. */
async function ownIdentityFingerprint() {{
  try {{
    if (!window.indexedDB || !crypto?.subtle) return '';
    const db = await new Promise((resolve, reject) => {{
      const req = indexedDB.open('intendant-client-identity', 1);
      req.onupgradeneeded = () => {{
        if (!req.result.objectStoreNames.contains('keys')) req.result.createObjectStore('keys');
      }};
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    }});
    const record = await new Promise((resolve, reject) => {{
      const tx = db.transaction('keys', 'readonly');
      const req = tx.objectStore('keys').get('v1');
      req.onsuccess = () => resolve(req.result || null);
      req.onerror = () => reject(req.error);
    }});
    db.close();
    if (!record?.publicRaw) return '';
    const digest = await crypto.subtle.digest('SHA-256', record.publicRaw);
    return bufToB64u(digest);
  }} catch {{ return ''; }}
}}

async function renderOrgs() {{
  const rows = $('org-rows');
  rows.innerHTML = '';
  let map = {{}};
  try {{ map = JSON.parse(localStorage.getItem('intendant_org_grants_v1') || '{{}}') || {{}}; }} catch {{}}
  const docs = Object.values(map).filter(doc => doc && typeof doc === 'object' && doc.org?.handle);
  if (!docs.length) {{
    rows.innerHTML = '<div class="empty-hint">None stored in this browser origin. Connect neither fetches nor presents organization documents. Human browser-key documents are record-only in this alpha.</div>';
    return;
  }}
  const ownFp = await ownIdentityFingerprint();
  const now = Date.now();
  for (const doc of docs) {{
    const expires = Number(doc.expires_at_unix_ms || 0);
    const daysLeft = Math.floor((expires - now) / 86400000);
    const expired = expires <= now;
    const role = String(doc.role_id || '').replace(/^role:/, '').replace(/^peer:/, 'daemon: ');
    const subjectFp = String(doc.subject?.peer_fingerprint || doc.subject?.client_key_fingerprint || '');
    const mine = ownFp && subjectFp === ownFp;
    const expiryText = expired
      ? 'expired — ask the org for a renewed document'
      : daysLeft < 1 ? 'expires today'
      : `expires in ${{daysLeft}} day${{daysLeft === 1 ? '' : 's'}}`;
    const row = document.createElement('div');
    row.className = 'org-row';
    row.innerHTML = `
      <div class="org-main">
        <strong>@${{escapeHtml(String(doc.org.handle))}}</strong>
        <span class="sub">${{escapeHtml(role)}} &middot; ${{mine ? 'bound to this browser' : 'bound to ' + escapeHtml(shortId(subjectFp))}} &middot; ${{escapeHtml(expiryText)}}</span>
      </div>
      <div class="org-side">
        <span class="pill ${{expired ? 'err' : (daysLeft < 5 ? 'warn' : 'ok')}}">${{expired ? 'expired' : 'active'}}</span>
        <button class="ghost" data-org-remove="${{escapeAttr(String(doc.org.handle))}}">Remove</button>
      </div>`;
    rows.appendChild(row);
  }}
  rows.querySelectorAll('[data-org-remove]').forEach(button => {{
    button.addEventListener('click', () => {{
      const handle = button.getAttribute('data-org-remove');
      if (!confirm(`Remove the stored @${{handle}} document record from this browser? Existing daemon records are unaffected.`)) return;
      try {{
        const current = JSON.parse(localStorage.getItem('intendant_org_grants_v1') || '{{}}') || {{}};
        delete current[handle];
        localStorage.setItem('intendant_org_grants_v1', JSON.stringify(current));
      }} catch {{}}
      renderOrgs();
    }});
  }});
}}

/* ── Transparency log client: RFC 9162 verification in WebCrypto ── */
const LOG_STH_KEY = 'intendant_log_sth_v1';

async function logSha(bytes) {{
  return new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
}}

async function logNodeHash(left, right) {{
  const buf = new Uint8Array(1 + left.length + right.length);
  buf[0] = 0x01; buf.set(left, 1); buf.set(right, 1 + left.length);
  return logSha(buf);
}}

function bytesEqual(a, b) {{
  if (!a || !b || a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) diff |= a[i] ^ b[i];
  return diff === 0;
}}

async function logVerifyConsistency(oldSize, newSize, oldRoot, newRoot, proof) {{
  if (oldSize === newSize) return bytesEqual(oldRoot, newRoot) && proof.length === 0;
  if (oldSize === 0 || oldSize > newSize) return false;
  const complete = (oldSize & (oldSize - 1)) === 0;
  let i = 0;
  const first = complete ? oldRoot : proof[i++];
  if (!first) return false;
  let fn = oldSize - 1, sn = newSize - 1;
  while (fn % 2 === 1) {{ fn = Math.floor(fn / 2); sn = Math.floor(sn / 2); }}
  let fr = first, sr = first;
  for (; i < proof.length; i += 1) {{
    if (sn === 0) return false;
    const p = proof[i];
    if (fn % 2 === 1 || fn === sn) {{
      fr = await logNodeHash(p, fr);
      sr = await logNodeHash(p, sr);
      if (fn % 2 === 0) while (fn % 2 === 0 && fn !== 0) {{ fn = Math.floor(fn / 2); sn = Math.floor(sn / 2); }}
    }} else {{
      sr = await logNodeHash(sr, p);
    }}
    fn = Math.floor(fn / 2); sn = Math.floor(sn / 2);
  }}
  return bytesEqual(fr, oldRoot) && bytesEqual(sr, newRoot) && sn === 0;
}}

async function logVerifySthSignature(sth) {{
  try {{
    const key = await crypto.subtle.importKey(
      'raw', b64uToBuf(sth.public_key),
      {{ name: 'ECDSA', namedCurve: 'P-256' }}, false, ['verify']);
    const payload = new TextEncoder().encode(
      `intendant-log-sth-v1\n${{sth.size}}\n${{sth.root}}\n${{sth.unix_ms}}`);
    return await crypto.subtle.verify(
      {{ name: 'ECDSA', hash: 'SHA-256' }}, key, b64uToBuf(sth.signature), payload);
  }} catch {{
    return false;
  }}
}}

/* Pin the signed tree head; on every visit verify the log only ever
   appended since last time. A failed check is loud and sticky — including a
   changed log signing key, which would otherwise let the service swap in a
   fresh log and dodge the consistency proof entirely (trust-on-every-use).
   Recovering from a legitimate key rotation is an explicit user action. */
async function transparencyCheck() {{
  const pill = $('log-pill');
  const resetBtn = $('log-reset-trust');
  if (resetBtn) resetBtn.classList.add('hidden');
  try {{
    const sth = await api('/api/log/sth');
    if (!(await logVerifySthSignature(sth))) throw new Error('tree head signature invalid');
    let pinned = null;
    try {{ pinned = JSON.parse(localStorage.getItem(LOG_STH_KEY) || 'null'); }} catch {{}}
    if (pinned && pinned.size > 0) {{
      if (pinned.public_key !== sth.public_key) {{
        if (resetBtn) resetBtn.classList.remove('hidden');
        throw new Error('log signing key changed — history can no longer be verified against your pin');
      }}
      if (sth.size < pinned.size) throw new Error('log shrank — history was rewritten');
      const proof = await api(`/api/log/consistency?old=${{pinned.size}}&new=${{sth.size}}`);
      const asBytes = value => new Uint8Array(b64uToBuf(value));
      const consistent = await logVerifyConsistency(
        pinned.size, sth.size,
        asBytes(pinned.root), asBytes(sth.root),
        (proof.proof || []).map(asBytes));
      if (!consistent) throw new Error('consistency proof failed — history was rewritten');
    }}
    localStorage.setItem(LOG_STH_KEY, JSON.stringify({{
      size: sth.size, root: sth.root, public_key: sth.public_key,
      pinned_unix_ms: pinned?.pinned_unix_ms || Date.now(),
    }}));
    if (pill) {{
      const since = pinned?.pinned_unix_ms ? new Date(pinned.pinned_unix_ms).toLocaleDateString() : 'today';
      pill.textContent = `${{sth.size}} entries · consistent since ${{since}}`;
      pill.className = 'pill ok';
    }}
  }} catch (err) {{
    console.warn('[transparency] check failed:', err);
    if (pill) {{
      pill.textContent = 'VERIFICATION FAILED: ' + err.message;
      pill.className = 'pill err';
    }}
  }}
}}

function renderAttestations() {{
  const claim = $('attest-claim');
  const badges = $('attest-badges');
  if (!claim || !badges || !state.user) return;
  claim.textContent = `intendant-handle=${{state.user.account_name}}@${{location.host}}`;
  const list = state.user.attestations || [];
  badges.innerHTML = list.length
    ? list.map(a => `<span class="pill ok" title="verified ${{new Date(a.verified_unix_ms).toLocaleDateString()}}">&#10003; ${{escapeHtml(a.kind === 'dns' ? a.subject : a.subject.replace('github:', 'github.com/'))}}</span>`).join('')
    : '<span class="sub">no verifications yet</span>';
}}

async function pushSubscriptionState() {{
  if (!('serviceWorker' in navigator) || !('PushManager' in window)) return {{ supported: false }};
  const registration = await navigator.serviceWorker.getRegistration('/');
  const subscription = registration ? await registration.pushManager.getSubscription() : null;
  return {{ supported: true, subscription }};
}}

async function renderPushBlock() {{
  const status = $('push-status');
  if (!status) return;
  const stateNow = await pushSubscriptionState().catch(() => ({{ supported: false }}));
  const enableBtn = $('push-enable');
  const disableBtn = $('push-disable');
  const testBtn = $('push-test');
  if (!stateNow.supported) {{
    status.textContent = 'not supported in this browser';
    status.className = 'pill';
    enableBtn.classList.add('hidden');
    disableBtn.classList.add('hidden');
    testBtn.classList.add('hidden');
    return;
  }}
  const on = Boolean(stateNow.subscription);
  status.textContent = on ? 'on for this browser' : 'off';
  status.className = 'pill' + (on ? ' ok' : '');
  enableBtn.classList.toggle('hidden', on);
  disableBtn.classList.toggle('hidden', !on);
  testBtn.classList.toggle('hidden', !on);
  const flags = $('push-flags');
  flags.classList.toggle('hidden', !on);
  if (on) {{
    try {{
      const {{ subscriptions }} = await api('/api/push/subscriptions');
      const mine = (subscriptions || []).find(s => s.endpoint === stateNow.subscription.endpoint);
      $('push-presence-flag').checked = Boolean(mine?.notify_presence);
      $('push-requests-flag').checked = Boolean(mine?.notify_requests);
    }} catch {{}}
  }}
}}

async function setPushPreference(patch) {{
  const stateNow = await pushSubscriptionState();
  const endpoint = stateNow.subscription?.endpoint || '';
  if (!endpoint) return;
  await api('/api/push/preferences', {{
    method: 'POST',
    body: JSON.stringify({{ endpoint, ...patch }}),
  }});
}}

async function enablePushNotifications() {{
  const permission = await Notification.requestPermission();
  if (permission !== 'granted') throw new Error('notification permission was not granted');
  const {{ public_key }} = await api('/api/push/vapid-public-key');
  const registration = await navigator.serviceWorker.register('/sw.js', {{ scope: '/' }});
  await navigator.serviceWorker.ready;
  const subscription = await registration.pushManager.subscribe({{
    userVisibleOnly: true,
    applicationServerKey: b64uToBuf(public_key),
  }});
  const raw = subscription.toJSON();
  await api('/api/push/subscribe', {{
    method: 'POST',
    body: JSON.stringify({{
      endpoint: raw.endpoint,
      p256dh: raw.keys?.p256dh || '',
      auth: raw.keys?.auth || '',
      label: navigator.userAgent.slice(0, 100),
    }}),
  }});
}}

async function disablePushNotifications() {{
  const stateNow = await pushSubscriptionState();
  const endpoint = stateNow.subscription?.endpoint || '';
  if (stateNow.subscription) await stateNow.subscription.unsubscribe().catch(() => {{}});
  await api('/api/push/unsubscribe', {{ method: 'POST', body: JSON.stringify({{ endpoint }}) }});
}}

let fleetAesKey = null;
async function fleetEncryptionKey() {{
  if (fleetAesKey) return fleetAesKey;
  try {{
    const prf = sessionStorage.getItem('intendant_fleet_prf_v1') || '';
    if (!prf || !crypto?.subtle) return null;
    const hkdf = await crypto.subtle.importKey('raw', b64uToBuf(prf), 'HKDF', false, ['deriveKey']);
    fleetAesKey = await crypto.subtle.deriveKey(
      {{ name: 'HKDF', hash: 'SHA-256', salt: new TextEncoder().encode('intendant-fleet-sync-v1'), info: new TextEncoder().encode('fleet-enc') }},
      hkdf, {{ name: 'AES-GCM', length: 256 }}, false, ['decrypt']
    );
    return fleetAesKey;
  }} catch {{ return null; }}
}}

async function decryptFleetTarget(target) {{
  const enc = String(target?.enc_fields || '');
  if (!enc.startsWith('enc1:')) return target;
  const key = await fleetEncryptionKey();
  if (!key) return {{ ...target, fleet_locked: true }};
  try {{
    const [iv, ct] = enc.slice(5).split(':');
    const plain = await crypto.subtle.decrypt({{ name: 'AES-GCM', iv: b64uToBuf(iv) }}, key, b64uToBuf(ct));
    const secret = JSON.parse(new TextDecoder().decode(plain));
    return {{ ...target, url: String(secret.url || ''), ws_url: String(secret.ws_url || ''), browser_tcp_via_url: String(secret.browser_tcp_via_url || ''), fleet_locked: false }};
  }} catch {{ return {{ ...target, fleet_locked: true }}; }}
}}

async function refreshAll() {{
  setBusy('refresh', true);
  try {{
    const me = await api('/api/me');
    state.csrfToken = me.csrf_token || '';
    state.user = me.authenticated ? me.user : null;
    state.inviteRequired = me.invite_required === true;
    renderAuth();
    if (!state.user) return;
    const [daemons, fleet, audit] = await Promise.all([
      api('/api/daemons'),
      api('/api/fleet/targets'),
      api('/api/audit'),
    ]);
    state.daemons = daemons.daemons || [];
    state.fleetTargets = await Promise.all((fleet.targets || []).map(decryptFleetTarget));
    renderOrgs().catch(() => {{}});
    renderDaemons();
    renderFleetTargets();
    renderAudit(audit.events || []);
  }} finally {{
    setBusy('refresh', false);
  }}
}}

function renderAuth() {{
  const authed = Boolean(state.user);
  $('invite-row').classList.toggle('hidden', authed || !state.inviteRequired);
  $('invite-note').classList.toggle('hidden', authed || !state.inviteRequired);
  document.body.classList.toggle('signed-out', !authed);
  document.body.classList.toggle('signed-in', authed);
  $('manage').classList.toggle('hidden', !authed);
  $('advanced').classList.toggle('hidden', !authed);
  $('logout').classList.toggle('hidden', !authed);
  $('refresh').classList.toggle('hidden', !authed);
  $('session-chip').classList.toggle('hidden', !authed);
  $('auth-actions').classList.toggle('hidden', authed);
  $('account').disabled = authed;
  if (!authed) $('fleet-section').classList.add('hidden');
  if (authed) renderPushBlock().catch(() => {{}});
  if (authed) renderAttestations();
  if (authed) {{
    $('account').value = state.user.account_name || '';
    $('session-chip-handle').textContent = '@' + state.user.account_name;
    $('session-handle').textContent = '@' + state.user.account_name;
    $('session-passkeys').textContent = `${{state.user.passkey_count}} passkey${{state.user.passkey_count === 1 ? '' : 's'}}`;
    $('session-user-id').textContent = state.user.id || '';
    $('who').textContent = '@' + state.user.account_name;
    const encOn = Boolean(sessionStorage.getItem('intendant_fleet_prf_v1'));
    const enc = $('enc-pill');
    enc.textContent = encOn ? 'sync encryption: on' : 'sync encryption: off';
    enc.className = 'pill' + (encOn ? ' ok' : '');
    enc.title = encOn
      ? 'Private fields in Saved places are end-to-end encrypted with a key derived from your passkey (WebAuthn PRF). This service stores only ciphertext.'
      : 'Your passkey or browser did not offer the WebAuthn PRF extension this session, so Saved places sync public fields only.';
  }} else {{
    $('session-chip-handle').textContent = '';
    $('session-handle').textContent = '';
    $('session-passkeys').textContent = '';
    $('session-user-id').textContent = '';
    $('who').textContent = '';
  }}
}}

function renderDaemons() {{
  const grid = $('computer-cards');
  grid.innerHTML = '';
  grid.parentElement.classList.toggle('empty', state.daemons.length === 0);
  $('who').textContent = state.daemons.length
    ? `${{state.daemons.length}} linked to @${{state.user?.account_name || ''}}`
    : '';
  for (const daemon of state.daemons) {{
    const key = String(daemon.daemon_public_key || '');
    const daemonId = String(daemon.daemon_id || '');
    const hasLabel = Boolean(String(daemon.label || '').trim());
    const label = hasLabel ? String(daemon.label) : shortId(daemonId);
    const lastSeen = formatRelative(daemon.last_seen_unix_ms);
    const card = document.createElement('div');
    card.className = 'computer-card';
    card.innerHTML = `
      <div class="computer-head">
        <span class="computer-dot ${{daemon.online ? 'ok' : ''}}" aria-hidden="true"></span>
        <div class="computer-name">
          <strong title="${{escapeAttr(hasLabel ? label : daemonId)}}">${{escapeHtml(label)}}</strong>
          <span class="sub">${{daemon.online ? 'online now' : 'last seen ' + escapeHtml(lastSeen)}}</span>
        </div>
      </div>
      <div class="computer-actions">
        <span class="route-only">Discovery only &mdash; open this daemon from a trusted local or independently verified direct-mTLS client.</span>
        <button class="secondary" data-rename="${{escapeAttr(daemonId)}}">Rename</button>
      </div>
      ${{presenceSparkline(daemon)}}
      <details>
        <summary>Details</summary>
        <div class="kv">
          <div><div class="k">Daemon id</div><code>${{escapeHtml(daemonId)}}</code></div>
          <div><div class="k">Connect-linked daemon key &mdash; signed link metadata is checked for consistency, not independent identity</div><code>${{escapeHtml(key)}}</code></div>
          <div class="danger-row"><button class="danger" data-revoke="${{escapeAttr(daemonId)}}">Disconnect from this account</button></div>
        </div>
      </details>`;
    grid.appendChild(card);
  }}
  grid.querySelectorAll('[data-revoke]').forEach(button => {{
    button.addEventListener('click', async () => {{
      const id = button.getAttribute('data-revoke');
      if (!confirm(`Disconnect ${{id}} from this account? The computer itself is untouched; it just stops being reachable through here until claimed again.`)) return;
      await api(`/api/daemons/${{encodeURIComponent(id)}}/revoke`, {{ method: 'POST', body: '{{}}' }});
      await refreshAll();
    }});
  }});
  grid.querySelectorAll('[data-rename]').forEach(button => {{
    button.addEventListener('click', async () => {{
      const id = button.getAttribute('data-rename');
      const daemon = state.daemons.find(item => item.daemon_id === id) || {{}};
      const next = prompt('Name this computer', daemon.label || daemon.daemon_id || '');
      if (next === null) return;
      await api(`/api/daemons/${{encodeURIComponent(id)}}/label`, {{
        method: 'POST',
        body: JSON.stringify({{ label: next }}),
      }});
      await refreshAll();
    }});
  }});
}}

function renderFleetTargets() {{
  const rows = $('fleet-rows');
  rows.innerHTML = '';
  const claimedIds = new Set(state.daemons.map(d => String(d.daemon_id || '')));
  const places = state.fleetTargets.filter(target => {{
    const cid = String(target.connect_daemon_id || '');
    return !(target.claimed_daemon === true && cid && claimedIds.has(cid));
  }});
  $('fleet-section').classList.toggle('hidden', !state.user || places.length === 0);
  for (const target of places) {{
    const id = String(target.host_id || target.id || '');
    const rawLabel = String(target.label || '').trim();
    const label = (!rawLabel || rawLabel === id) ? (shortId(id) || 'Place') : rawLabel;
    const locked = target.fleet_locked === true;
    const route = locked
      ? 'End-to-end encrypted — passkey sign-in reveals this saved route; opening the daemon still requires trusted mTLS'
      : String(target.route_label || target.route || target.url || 'Remembered route');
    const online = target.online || target.connected;
    // A live claim link is directory metadata and must never grow a control
    // URL. Only a separately remembered/decrypted direct route may navigate
    // away to its daemon's mTLS origin.
    const url = target.claimed_daemon === true ? '' : String(target.url || '');
    const canForget = target.claimed_daemon !== true;
    const row = document.createElement('div');
    row.className = 'place-row';
    row.innerHTML = `
      <div class="place-main">
        <strong>${{escapeHtml(label)}}</strong>
        <span class="sub" title="${{escapeAttr(route)}}">${{escapeHtml(route)}}</span>
      </div>
      <span class="pill ${{online ? 'ok' : ''}}">${{online ? 'online' : (locked ? 'locked' : 'remembered')}}</span>
      <div class="place-actions">
        <button data-fleet-open="${{escapeAttr(url)}}" ${{url ? '' : 'disabled'}}>Open direct route</button>
        <button class="ghost" data-fleet-forget="${{escapeAttr(id)}}" ${{canForget ? '' : 'disabled'}}>Forget</button>
      </div>`;
    rows.appendChild(row);
  }}
  rows.querySelectorAll('[data-fleet-open]').forEach(button => {{
    button.addEventListener('click', () => {{
      const url = button.getAttribute('data-fleet-open');
      if (url) window.location.href = url;
    }});
  }});
  rows.querySelectorAll('[data-fleet-forget]').forEach(button => {{
    button.addEventListener('click', async () => {{
      const id = button.getAttribute('data-fleet-forget');
      if (!id) return;
      await api(`/api/fleet/targets/${{encodeURIComponent(id)}}/forget`, {{ method: 'POST', body: '{{}}' }});
      await refreshAll();
    }});
  }});
}}

function renderAudit(events) {{
  const el = $('audit');
  el.innerHTML = '';
  if (!events.length) {{
    el.innerHTML = '<div class="empty-hint">No account activity yet.</div>';
    return;
  }}
  for (const event of events.slice(0, 30)) {{
    const div = document.createElement('div');
    div.className = 'event';
    const date = formatDate(event.unix_ms);
    const name = String(event.event || '').replaceAll('_', ' ');
    div.innerHTML = `<div class="event-line"><span class="event-name">${{escapeHtml(name)}}</span><time>${{escapeHtml(date)}}</time></div><code>${{escapeHtml(event.daemon_id || '')}}</code>`;
    el.appendChild(div);
  }}
}}

/* Last 72 hours as tiny bars (present = the daemon polled that hour),
   plus a 7-day availability figure. Display of data the rendezvous
   already has from the polling it exists to do. */
function presenceSparkline(daemon) {{
  const hours = Array.isArray(daemon.presence_hours) ? daemon.presence_hours : [];
  if (!hours.length) return '';
  const seen = new Set(hours.map(Number));
  const nowHour = Math.floor(Date.now() / 3600000);
  const span = 72;
  let bars = '';
  for (let i = span - 1; i >= 0; i -= 1) {{
    const hour = nowHour - i;
    const on = seen.has(hour);
    const when = new Date(hour * 3600000);
    bars += `<span class="${{on ? 'on' : ''}}" title="${{escapeAttr(when.toLocaleString([], {{ weekday: 'short', hour: 'numeric' }}))}} — ${{on ? 'online' : 'offline'}}"></span>`;
  }}
  let weekSeen = 0;
  for (let i = 0; i < 168; i += 1) if (seen.has(nowHour - i)) weekSeen += 1;
  const tracked = Math.min(168, Math.max(1, nowHour - Math.min(...seen) + 1));
  const pct = Math.round((weekSeen / Math.min(168, tracked)) * 100);
  return `<div class="presence"><div class="presence-bars" aria-hidden="true">${{bars}}</div><div class="presence-label">last 3 days &middot; up ${{pct}}% of the ${{tracked >= 168 ? 'week' : 'time tracked'}}</div></div>`;
}}

function compactKey(value) {{
  const key = String(value || '');
  if (key.length <= 24) return key;
  return key.slice(0, 12) + '...' + key.slice(-8);
}}

function shortId(value) {{
  const id = String(value || '');
  if (id.length > 24 && !id.includes('.')) return id.slice(0, 8) + '…' + id.slice(-4);
  return id;
}}

function formatDate(unixMs) {{
  const value = Number(unixMs || 0);
  if (!value) return 'unknown';
  return new Date(value).toLocaleString();
}}

function formatRelative(unixMs) {{
  const value = Number(unixMs || 0);
  if (!value) return 'never';
  const seconds = Math.max(0, Math.floor((Date.now() - value) / 1000));
  if (seconds < 10) return 'just now';
  if (seconds < 60) return `${{seconds}}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${{minutes}}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${{hours}}h ago`;
  return `${{Math.floor(hours / 24)}}d ago`;
}}

function escapeHtml(value) {{
  return String(value ?? '').replace(/[&<>"']/g, c => ({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c]));
}}
function escapeAttr(value) {{ return escapeHtml(value); }}

$('attest-dns-btn').addEventListener('click', async () => {{
  const domain = $('attest-domain').value.trim();
  if (!domain) return;
  setStatus('attest-status', 'checking TXT record\u2026', '');
  try {{
    const r = await api('/api/attest/dns', {{ method: 'POST', body: JSON.stringify({{ domain }}) }});
    setStatus('attest-status', `verified ${{r.subject}}`, 'ok');
    await refreshAll();
  }} catch (err) {{ setStatus('attest-status', err.message, 'err'); }}
}});
$('attest-github-btn').addEventListener('click', async () => {{
  const gist_raw_url = $('attest-gist').value.trim();
  if (!gist_raw_url) return;
  setStatus('attest-status', 'fetching gist\u2026', '');
  try {{
    const r = await api('/api/attest/github', {{ method: 'POST', body: JSON.stringify({{ gist_raw_url }}) }});
    setStatus('attest-status', `verified ${{r.subject}}`, 'ok');
    await refreshAll();
  }} catch (err) {{ setStatus('attest-status', err.message, 'err'); }}
}});
transparencyCheck();
$('log-reset-trust').addEventListener('click', () => {{
  localStorage.removeItem(LOG_STH_KEY);
  transparencyCheck();
}});
$('push-enable').addEventListener('click', () => enablePushNotifications().then(renderPushBlock).catch(err => alert('Notifications: ' + err.message)));
$('push-disable').addEventListener('click', () => disablePushNotifications().then(renderPushBlock).catch(() => renderPushBlock()));
$('push-test').addEventListener('click', async () => {{
  try {{ await api('/api/push/test', {{ method: 'POST', body: '{{}}' }}); }} catch (err) {{ alert('Test failed: ' + err.message); }}
}});
$('push-presence-flag').addEventListener('change', event => {{
  setPushPreference({{ notify_presence: event.target.checked }}).catch(() => renderPushBlock());
}});
$('push-requests-flag').addEventListener('change', event => {{
  setPushPreference({{ notify_requests: event.target.checked }}).catch(() => renderPushBlock());
}});
$('register').addEventListener('click', () => createPasskey().catch(err => setStatus('auth-status', err.message, 'err')));
$('login').addEventListener('click', () => login().catch(err => setStatus('auth-status', err.message, 'err')));
$('claim').addEventListener('click', () => claimDaemon().catch(err => setStatus('claim-status', err.message, 'err')));
$('refresh').addEventListener('click', () => refreshAll().catch(err => setStatus('claim-status', err.message, 'err')));
$('logout').addEventListener('click', async () => {{ await api('/api/logout', {{ method: 'POST', body: '{{}}' }}); state.user = null; state.csrfToken = ''; renderAuth(); }});
$('copy-user-id').addEventListener('click', async () => {{
  const id = state.user && state.user.id ? String(state.user.id) : '';
  if (!id) return;
  try {{
    await navigator.clipboard.writeText(id);
    const btn = $('copy-user-id');
    btn.textContent = 'Copied';
    setTimeout(() => {{ btn.textContent = 'Copy'; }}, 1200);
  }} catch (err) {{
    setStatus('auth-status', 'Copy failed: ' + ((err && err.message) || err), 'err');
  }}
}});
$('account').addEventListener('keydown', event => {{ if (event.key === 'Enter') login().catch(err => setStatus('auth-status', err.message, 'err')); }});
$('claim-code').addEventListener('keydown', event => {{ if (event.key === 'Enter') claimDaemon().catch(err => setStatus('claim-status', err.message, 'err')); }});

const params = new URLSearchParams(location.search);
const fragmentParams = new URLSearchParams(location.hash.replace(/^#/, ''));
// Current claim links put the phrase in the fragment, which browsers do not
// send in HTTP requests or referrers. Query-string claim codes are deliberately
// not supported: even reading one after load would normalize an unsafe link
// format that already exposed the phrase to HTTP and proxy logs.
const claimCodeFromUrl = fragmentParams.get('claim_code');
if (claimCodeFromUrl) {{
  $('claim-code').value = claimCodeFromUrl;
  fragmentParams.delete('claim_code');
  const nextQuery = params.toString();
  const nextFragment = fragmentParams.toString();
  history.replaceState(
    null,
    '',
    location.pathname + (nextQuery ? `?${{nextQuery}}` : '') + (nextFragment ? `#${{nextFragment}}` : '')
  );
}}
// Shareable invites: /connect?invite=CODE prefills the invite field.
if (params.get('invite')) $('invite-code').value = params.get('invite');
refreshAll().catch(() => renderAuth());
</script>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_framing_denied(response: &Response) {
        assert_eq!(
            response
                .headers()
                .get("content-security-policy")
                .and_then(|value| value.to_str().ok()),
            Some(HTML_FRAME_ANCESTORS)
        );
        assert_eq!(
            response
                .headers()
                .get("x-frame-options")
                .and_then(|value| value.to_str().ok()),
            Some("DENY")
        );
    }

    #[test]
    fn every_server_rendered_connect_page_denies_framing() {
        for body in [
            landing_ui_html("https://connect.example"),
            connect_page_html("https://connect.example"),
            trust_ui_html("https://connect.example"),
            access_page_html("https://connect.example"),
        ] {
            let response = StaticPage::html(body).respond(&HeaderMap::new());
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("text/html; charset=utf-8")
            );
            assert_framing_denied(&response);
        }
    }

    #[test]
    fn static_pages_serve_etags_and_revalidate_to_304() {
        let page = StaticPage::html("<!doctype html><title>x</title>".to_string());
        let full = page.respond(&HeaderMap::new());
        assert_eq!(full.status(), StatusCode::OK);
        let etag = full.headers().get(header::ETAG).cloned().expect("etag set");
        assert_eq!(
            full.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-cache")
        );
        assert_framing_denied(&full);

        let mut revalidate = HeaderMap::new();
        revalidate.insert(header::IF_NONE_MATCH, etag.clone());
        let not_modified = page.respond(&revalidate);
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            not_modified.headers().get(header::ETAG),
            Some(&etag),
            "304 re-states the validator"
        );

        // A weak-prefixed or list-form validator still matches; a stale one
        // gets the full body again.
        let mut weak = HeaderMap::new();
        weak.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(&format!("W/{}, \"other\"", etag.to_str().unwrap())).unwrap(),
        );
        assert_eq!(page.respond(&weak).status(), StatusCode::NOT_MODIFIED);
        let mut stale = HeaderMap::new();
        stale.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"stale\""));
        assert_eq!(page.respond(&stale).status(), StatusCode::OK);

        // RFC 9110: `*` matches any current representation.
        let mut any = HeaderMap::new();
        any.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert_eq!(page.respond(&any).status(), StatusCode::NOT_MODIFIED);

        // Installers keep their plain-text identity and skip CSP framing.
        let script = StaticPage::script("#!/bin/sh\n".to_string()).respond(&HeaderMap::new());
        assert_eq!(
            script
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/plain; charset=utf-8")
        );
        assert!(script.headers().get("x-frame-options").is_none());
    }

    #[tokio::test]
    async fn retired_app_entry_points_redirect_and_unknown_paths_are_404() {
        // Drive real HTTP requests through the production route table. This
        // catches an accidental wildcard/static fallback or one spelling
        // losing its explicit route; a test-only mini-router would not.
        let root = tempfile::tempdir().unwrap();
        let state = production_router_test_state(root.path(), Store::default());
        let app = connect_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        for historical_path in ["/app", "/app?connect=1&daemon_id=crafted", "/app.html"] {
            let response = client
                .get(format!("http://{address}{historical_path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::SEE_OTHER,
                "{historical_path}"
            );
            assert_eq!(
                response.headers().get(header::LOCATION).unwrap(),
                "/connect",
                "{historical_path}"
            );
        }
        for forbidden_static_path in [
            "/static/app.html",
            "/app.js",
            "/vault-kernel.js",
            "/wasm-web/presence_web.js",
            "/wasm-station/station_web.js",
        ] {
            let response = client
                .get(format!("http://{address}{forbidden_static_path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{forbidden_static_path}"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn production_router_refuses_hosted_control_without_relay_mutation() {
        let root = tempfile::tempdir().unwrap();
        let user_id = Uuid::new_v4();
        let mut store = Store::default();
        store.users.push(UserRecord {
            id: user_id,
            account_name: "alice".to_string(),
            display_name: "Alice".to_string(),
            passkeys: Vec::new(),
            created_unix_ms: 1,
            updated_unix_ms: 1,
            last_login_unix_ms: 1,
            attestations: Vec::new(),
        });
        let state = production_router_test_state(root.path(), store);
        let (session, csrf) = create_session(&state, user_id).await;
        state.event_queues.lock().await.insert(
            "daemon-1".to_string(),
            VecDeque::from([serde_json::from_value(json!({
                "id": "existing-route-event",
                "kind": "claim_challenge",
            }))
            .unwrap()]),
        );
        state.active_sessions.lock().await.insert(
            "legacy-session".to_string(),
            ActiveDashboardSession {
                daemon_id: "daemon-1".to_string(),
                session_id: "legacy-session".to_string(),
                created_unix_ms: now_unix_ms(),
            },
        );

        let origin = state.config.public_origin.clone();
        let app = connect_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();

        for endpoint in [
            "/api/browser/offer",
            "/api/browser/ice",
            "/api/browser/close",
        ] {
            let response = client
                .post(format!("http://{address}{endpoint}"))
                .header(header::COOKIE, format!("{COOKIE_NAME}={session}"))
                .header(CSRF_HEADER, &csrf)
                .header(header::ORIGIN, &origin)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{endpoint}");
            let body = response.text().await.unwrap();
            assert!(
                body.contains("hosted daemon control is unavailable"),
                "{endpoint}: {body}"
            );
        }

        assert!(state.pending_offers.lock().await.is_empty());
        let queues = state.event_queues.lock().await;
        let queue = queues.get("daemon-1").unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(
            serde_json::to_value(queue.front().unwrap()).unwrap()["id"],
            "existing-route-event"
        );
        drop(queues);
        assert!(state
            .active_sessions
            .lock()
            .await
            .contains_key("legacy-session"));
        assert!(state.rate_limits.lock().await.scopes.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn connect_push_worker_is_explicitly_embedded() {
        let response = service_worker_js().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            response.headers().get("service-worker-allowed").unwrap(),
            "/"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), CONNECT_SERVICE_WORKER_JS.as_bytes());
    }

    #[test]
    fn daemon_spa_source_cannot_activate_hosted_connect_mode_from_query() {
        let source = include_str!("../../../static/app/31-init-identity-fleet.js");
        assert!(source.contains("const DASHBOARD_CONNECT_MODE = false;"));
        assert!(source.contains("const DASHBOARD_CONNECT_DAEMON_ID = '';"));
        assert!(source.contains("const DASHBOARD_CONNECT_SIGNALING_BASE = '';"));
        assert!(!source.contains("dashboardUrlParams.get('connect')"));
    }

    #[test]
    fn trust_page_states_the_model() {
        let html = trust_ui_html("https://connect.intendant.dev");
        assert!(html.contains("<title>How trust works"));
        assert!(html.contains("authenticator&rsquo;s passkey secret remains non-extractable"));
        assert!(html.contains("controls the JavaScript"));
        assert!(html.contains("none of those results authenticates to a daemon"));
        assert!(html.contains("not an independent key pin"));
        assert!(html.contains("does not independently validate the first key"));
        assert!(html.contains("run your own rendezvous"));
        assert!(html.contains("<code>https://connect.intendant.dev</code>"));
    }

    #[test]
    fn access_ui_uses_access_branding() {
        let html = connect_ui_html(
            "https://intendant.dev",
            "Intendant Access",
            "Rendezvous and fleet navigation",
        );
        assert!(html.contains("<title>Intendant Access</title>"));
        assert!(html.contains("<h1>Intendant Access</h1>"));
        assert!(html.contains(">Rendezvous and fleet navigation</div>"));
        assert!(html.contains("target daemons enforce local IAM"));
    }

    #[test]
    fn transparency_pin_fails_hard_on_log_key_change() {
        // The documented pin ("rewriting history here is detectable") is only
        // real if a swapped log signing key is a verification failure, not a
        // silent re-pin; recovery must be the explicit user reset.
        let html = connect_ui_html("https://intendant.dev", "Intendant Connect", "sub");
        assert!(html.contains("pinned.public_key !== sth.public_key"));
        assert!(html.contains("log signing key changed"));
        assert!(html.contains(r#"id="log-reset-trust""#));
        assert!(html.contains("localStorage.removeItem(LOG_STH_KEY)"));
    }

    #[test]
    fn landing_page_states_the_product_and_reuses_the_origin() {
        let html = landing_ui_html("https://rendezvous.example");
        assert!(html.contains("<title>Intendant — an operating environment"));
        // The install one-liner advertises the serving origin, but the
        // hosted installer never accepts or mints an owner key.
        assert!(html.contains("curl -fsSL https://rendezvous.example/install.sh"));
        assert!(!html.contains("--owner"));
        assert!(!html.contains("-Owner"));
        // Beginner path and depth are both one click away.
        assert!(html.contains(r#"href="/connect""#));
        assert!(html.contains(r#"href="/trust""#));
        assert!(html.contains(DOCS_URL));
        assert!(html.contains(REPO_URL));
        assert!(html.contains("Built to be distrusted"));
        // The tour shows the product: every embedded screenshot is referenced,
        // with alt text so the page reads without images.
        for asset in [
            "hero.webp",
            "video.webp",
            "station.webp",
            "vault.webp",
            "claim.webp",
            "phone.webp",
        ] {
            assert!(
                html.contains(&format!("/assets/landing/{asset}")),
                "landing page must reference {asset}"
            );
        }
        assert!(html.contains("alt=\"The Intendant dashboard's Activity feed"));
        // Discovery is zero-install; control honestly names its trusted
        // enrollment or native/local anchor instead of inheriting that claim.
        assert!(html.contains("Zero-install discovery; trusted enrollment for control"));
        assert!(html.contains("Installing a client certificate or profile is a real"));
        assert!(html.contains("controlling the daemon still requires trusted certificate or"));
        assert!(html.contains("Multiple trusted interfaces"));
        assert!(html.contains("without a separate app install"));
        assert!(html.contains("remote browser still needs certificate or profile enrollment"));
        assert!(!html.contains("terminal TUI"));
        // "How do I use it" is the page's first answer: the install
        // questionnaire sits directly under the hero, before the shot tour.
        let install_at = html.find(r#"<section class="install-section""#).unwrap();
        let heroshot_at = html.find(r#"<section class="heroshot""#).unwrap();
        let tour_at = html.find(r#"<section class="tour""#).unwrap();
        assert!(
            install_at < heroshot_at && heroshot_at < tour_at,
            "install must lead, then the product tour"
        );
        // The name is the thesis, stated once, quietly, before the trust row.
        assert!(html.contains("Why “Intendant”"));
        assert!(html.contains("a network of agentic networks"));
        // Custody names both stores and makes the missing bridge explicit.
        assert!(html.contains(r#"class="fuelmap""#));
        assert!(html.contains("account-vault API/storage only"));
        assert!(html.contains("no shipped client or daemon bridge"));
        assert!(html.contains("Connect cannot invoke these controls"));
        // The canonical mark, not an ad-hoc monogram: favicon + header logo.
        assert!(html.contains(r#"<link rel="icon" type="image/svg+xml" href="/logo.svg">"#));
        assert!(html.contains(r#"<link rel="icon" type="image/png" href="/favicon.png">"#));
        assert!(html.contains(r#"<img src="/logo.svg""#));
        assert!(!html.contains("data:image/svg"));
        // The deployment advisor LEADS the install section — no fold to
        // find, four questions all about the agent's machine (the browser
        // needs no separate app install, while trusted enrollment is handled
        // on the daemon/client access path), and
        // runtime-origin commands so self-hosted rendezvous advertise their
        // own installers there too — the sh one-liner AND the PowerShell
        // one (Windows is first-class).
        assert!(!html.contains("<details class=\"advisor\""));
        for question in [
            "OS on the agent's machine?",
            "What kind of machine?",
            "What will fuel it?",
            "Keep working with your browser closed?",
        ] {
            assert!(html.contains(question), "advisor must ask: {question}");
        }
        // The default answers' command is server-rendered, so the page
        // shows a working one-liner (Linux VPS ⇒ --service) without JS.
        assert!(
            html.contains("curl -fsSL https://rendezvous.example/install.sh | sh -s -- --service")
        );
        assert!(!html.contains("__ADVISOR_DEFAULT_CMD__"));
        assert!(html.contains("location.origin + '/install.sh"));
        assert!(html.contains("/install.ps1"));
        assert!(html.contains("--service"));
        assert!(html.contains("-Service"));
        // No init system is asserted as a given — the note speaks in
        // native-supervisor terms, not systemd.
        assert!(!html.contains("journalctl"));
        // Honest alpha framing before anyone clicks Sign in.
        assert!(html.contains(r#"<span class="pill-alpha">alpha</span>"#));
    }

    #[test]
    fn connect_page_frames_the_private_alpha() {
        let html = connect_ui_html(
            "https://intendant.dev",
            "Intendant Connect",
            "Rendezvous account",
        );
        // The invite dead-end explains itself and offers the two open paths.
        assert!(html.contains("invite-only alpha"));
        assert!(html.contains("self-hosting is never gated"));
        assert!(html.contains(r#"$('invite-note').classList.toggle"#));
        // Shareable invite links prefill the code.
        assert!(html.contains("params.get('invite')"));
        assert!(html.contains("One-time claim code"));
        assert!(html.contains("linking grants no machine access"));
        assert!(html.contains("No machine access was granted"));
        assert!(html.contains("new URLSearchParams(location.hash"));
        assert!(html.contains("claim_code_hash: await sha256B64uOfText(normalized)"));
        assert!(!html.contains("JSON.stringify({ claim_code:"));
        assert!(!html.contains("params.get('claim_code')"));
        assert!(html.contains("history.replaceState("));
        assert!(html.contains("not an independent first-introduction pin"));
        assert!(html.contains("signed link metadata is checked for consistency"));
        assert!(html.contains("Connect neither fetches nor presents organization documents"));
        assert!(!html.contains("presented automatically on every connection"));
        assert!(html.contains("Discovery only &mdash; open this daemon from a trusted local"));
        assert!(!html.contains("data-open="));
        assert!(html.contains("target.claimed_daemon === true ? ''"));
        assert!(html.contains("Open direct route"));
        assert!(html.contains("passkey sign-in reveals this saved route"));
        assert!(html.contains("opening the daemon still requires trusted mTLS"));
        assert!(!html.contains("/app?connect=1"));
        assert!(!html.contains("sessions verify this end to end"));
        for forbidden in [
            "/arm",
            "client_key_tag",
            "connect-bootstrap",
            "role: root",
            "ensureOwnIdentity",
            "bootstrapTag",
        ] {
            assert!(
                !html.contains(forbidden),
                "claim page must not contain hosted authority behavior: {forbidden}"
            );
        }
    }

    #[test]
    fn every_page_serves_the_canonical_mark() {
        // The embedded mark is the real artwork: SVG vector + PNG fallback
        // (kept in lockstep with static/ by include_str!/include_bytes!).
        assert!(LOGO_SVG.starts_with("<svg"));
        assert!(
            LOGO_SVG.contains(r#"viewBox="16 16 480 480""#),
            "logo.svg must stay the margin-cropped view of the macOS icon"
        );
        assert_eq!(&BRAND_ICON_PNG[0..8], b"\x89PNG\r\n\x1a\n");
        assert!(
            BRAND_ICON_PNG.len() > 2_048,
            "brand icon suspiciously small"
        );
        let svg_link = r#"<link rel="icon" type="image/svg+xml" href="/logo.svg">"#;
        let png_link = r#"<link rel="icon" type="image/png" href="/favicon.png">"#;
        let connect = connect_ui_html(
            "https://x.example",
            "Intendant Connect",
            "Rendezvous account",
        );
        assert!(connect.contains(svg_link) && connect.contains(png_link));
        assert!(connect.contains(r#"class="brand-mark" src="/logo.svg""#));
        assert!(!connect.contains(">IC</div>"));
        let trust = trust_ui_html("https://x.example");
        assert!(trust.contains(svg_link) && trust.contains(png_link));
        assert!(!trust.contains(">IC</div>"));
    }

    #[test]
    fn landing_assets_are_embedded_webp() {
        for asset in [
            "hero.webp",
            "video.webp",
            "station.webp",
            "vault.webp",
            "claim.webp",
            "phone.webp",
        ] {
            let bytes = landing_asset_bytes(asset)
                .unwrap_or_else(|| panic!("missing embedded landing asset {asset}"));
            // RIFF....WEBP container magic.
            assert!(bytes.len() > 8_192, "{asset} suspiciously small");
            assert_eq!(&bytes[0..4], b"RIFF", "{asset} is not a RIFF container");
            assert_eq!(&bytes[8..12], b"WEBP", "{asset} is not WebP");
        }
        assert!(landing_asset_bytes("nope.webp").is_none());
        assert!(landing_asset_bytes("../secrets").is_none());
    }

    #[test]
    fn embedded_installer_never_accepts_hosted_owner_bootstrap() {
        assert!(
            INSTALL_SH.starts_with("#!/bin/sh"),
            "installer must be a sh script"
        );
        assert!(!INSTALL_SH.contains("--owner"));
        assert!(
            INSTALL_SH.contains("cargo build --release"),
            "installer must build release binaries"
        );
        // --service must delegate to the binary's cross-platform service
        // subcommand, never hand-roll a unit (systemd is one backend of
        // four, not a dependency).
        assert!(INSTALL_SH.contains("service install --now --"));
        assert!(!INSTALL_SH.contains("/etc/systemd/system"));

        assert!(
            INSTALL_PS1.starts_with("<#"),
            "ps1 installer must open with comment help"
        );
        // Windows PowerShell 5.1 decodes BOM-less files as ANSI, and a
        // UTF-8 em-dash misdecodes into a cp1252 smart QUOTE — which the
        // parser honors, unbalancing every string after it. The bootstrap
        // script stays pure ASCII so no delivery path can corrupt it.
        assert!(INSTALL_PS1.is_ascii(), "install.ps1 must be pure ASCII");
        for needle in [
            "param(",
            "$Connect",
            "$Service",
            "cargo build --release",
            "\"service\", \"install\"",
        ] {
            assert!(
                INSTALL_PS1.contains(needle),
                "install.ps1 must contain {needle}"
            );
        }
        assert!(!INSTALL_PS1.contains("$Owner"));
    }

    #[test]
    fn served_installers_default_connect_to_the_serving_rendezvous() {
        // The embedded scripts must keep the sentinel lines the handlers
        // splice — if either drifts, injection silently stops and a fresh
        // VPS comes up unregistered (hosted claiming dead-ends).
        assert!(
            INSTALL_SH.contains(INSTALL_SH_CONNECT_DEFAULT),
            "install.sh connect-default sentinel drifted"
        );
        assert!(
            INSTALL_PS1.contains(INSTALL_PS1_CONNECT_DEFAULT),
            "install.ps1 connect-default sentinel drifted"
        );

        let sh = install_sh_body("https://rendezvous.example");
        assert!(sh.contains(
            r#"CONNECT_URL="${INTENDANT_CONNECT_RENDEZVOUS_URL:-https://rendezvous.example}""#
        ));
        assert!(!sh.contains(INSTALL_SH_CONNECT_DEFAULT));

        let ps1 = install_ps1_body("https://rendezvous.example");
        assert!(ps1.contains(r#"[string]$Connect = "https://rendezvous.example","#));
        assert!(!ps1.contains(INSTALL_PS1_CONNECT_DEFAULT));
        // The ANSI-decode trap applies to the served body, not just the
        // embedded file — the injected origin must not break the pin.
        assert!(ps1.is_ascii(), "served install.ps1 must stay pure ASCII");

        // Splice guard: only a plain URL charset reaches the scripts.
        assert!(connect_default_injectable("https://intendant.dev"));
        assert!(connect_default_injectable("http://localhost:9891"));
        assert!(!connect_default_injectable(r#"https://x"; rm -rf ~"#));
        assert!(!connect_default_injectable("https://x y"));
        assert!(!connect_default_injectable(""));
        let verbatim = install_sh_body(r#"https://x" y"#);
        assert_eq!(verbatim, INSTALL_SH, "unsafe origin must serve verbatim");
    }

    /// Windows PowerShell 5.1 executes setup-windows.ps1 straight from the
    /// fresh clone, so the BOM-less ANSI-decode trap pinned for install.ps1
    /// above applies to it identically — a non-ASCII byte that lands in
    /// code (not a comment) can decode into a cp1252 smart quote the parser
    /// honors. Keep every PowerShell file a fresh box runs pure ASCII.
    #[test]
    fn setup_windows_ps1_is_pure_ascii() {
        const SETUP_PS1: &str = include_str!("../../../scripts/setup-windows.ps1");
        assert!(SETUP_PS1.is_ascii(), "setup-windows.ps1 must be pure ASCII");
    }

    /// Real parse coverage for the PowerShell installer, on the platform
    /// that ships PowerShell — a macOS/Linux dev box cannot tokenize it.
    #[cfg(windows)]
    #[test]
    fn embedded_ps1_installer_parses() {
        let dir = std::env::temp_dir().join(format!("intendant-ps1-parse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("install.ps1");
        std::fs::write(&script, INSTALL_PS1).unwrap();
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "$errs = $null; [System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$null, [ref]$errs) | Out-Null; if ($errs.Count) {{ $errs | ForEach-Object {{ Write-Error $_.Message }}; exit 1 }}",
                    script.display()
                ),
            ])
            .output()
            .expect("powershell must exist on Windows");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            output.status.success(),
            "install.ps1 has parse errors: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

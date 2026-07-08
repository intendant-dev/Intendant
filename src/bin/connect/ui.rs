//! The served surface: landing/connect/trust/access pages and their HTML
//! builders, the embedded installers and brand assets, health probes, and
//! the static-asset fallback.

use super::*;

pub(crate) async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

/// The bootstrap installer (credential custody, rollout step 6), embedded
/// at build time so the service — hosted or self-hosted — serves the
/// installer that matches its own version:
///   curl -fsSL <origin>/install.sh | sh -s -- --owner <fingerprint>
///
/// Served with this rendezvous' public origin injected as the default
/// `--connect` URL: fetching the installer from a rendezvous IS the opt-in,
/// and a fresh VPS has no other way to learn where to register — without
/// it the daemon comes up unregistered and hosted claiming dead-ends.
/// (A compiled-in default in the daemon would instead make every install
/// phone home to intendant.dev; serve-time injection keeps self-hosting
/// exact.) Explicit `--connect` / `-Connect` still wins over the default.
pub(crate) const INSTALL_SH: &str = include_str!("../../../scripts/install.sh");
pub(crate) const INSTALL_SH_CONNECT_DEFAULT: &str = r#"CONNECT_URL="${INTENDANT_CONNECT_RENDEZVOUS_URL:-}""#;

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

pub(crate) async fn install_sh(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        install_sh_body(&state.config.public_origin),
    )
}

/// The Windows counterpart, for PowerShell:
///   & ([scriptblock]::Create((irm <origin>/install.ps1))) -Owner <fingerprint>
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

pub(crate) async fn install_ps1(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        install_ps1_body(&state.config.public_origin),
    )
}

/// The canonical Intendant mark, embedded so every page this binary serves
/// gets the real logo without a static root. `static/logo.svg` is the
/// macOS icon vector (macos-app/icon.svg) with the dock margin cropped in
/// viewBox space; the PNG fallback is rendered from it (`rsvg-convert -w 128`).
pub(crate) const LOGO_SVG: &str = include_str!("../../../static/logo.svg");
pub(crate) const BRAND_ICON_PNG: &[u8] = include_bytes!("../../../static/icon-128.png");

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

/// Product screenshots for the landing page, embedded like the installer so
/// every deployment serves visuals that match its own UI. Captured from a
/// staged local rig (daemon "atlas", account "@ada") — synthetic content only.
pub(crate) fn landing_asset_bytes(name: &str) -> Option<&'static [u8]> {
    match name {
        "hero.webp" => Some(include_bytes!("assets/landing-hero.webp")),
        "video.webp" => Some(include_bytes!("assets/landing-video.webp")),
        "vault.webp" => Some(include_bytes!("assets/landing-vault.webp")),
        "station.webp" => Some(include_bytes!("assets/landing-station.webp")),
        "claim.webp" => Some(include_bytes!("assets/landing-claim.webp")),
        "phone.webp" => Some(include_bytes!("assets/landing-phone.webp")),
        _ => None,
    }
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
    let app_html = state.config.static_root.join("app.html");
    let static_ok = app_html.is_file();
    let state_parent_ok = state
        .config
        .data_file
        .parent()
        .map(|parent| parent.exists() || std::fs::create_dir_all(parent).is_ok())
        .unwrap_or(false);
    let ok = static_ok && state_parent_ok;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "ok": ok,
            "static_app": static_ok,
            "state_parent": state_parent_ok,
        })),
    )
        .into_response()
}

pub(crate) async fn landing_ui(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(landing_ui_html(&state.config.public_origin))
}

pub(crate) async fn connect_ui(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(connect_ui_html(
        &state.config.public_origin,
        "Intendant Connect",
        "Rendezvous account",
    ))
}

pub(crate) async fn trust_ui(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(trust_ui_html(&state.config.public_origin))
}

pub(crate) async fn access_ui(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(connect_ui_html(
        &state.config.public_origin,
        "Intendant Access",
        "Rendezvous and fleet navigation",
    ))
}

pub(crate) async fn app_html(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    if !valid_connect_app_query(uri.query()) {
        return Ok(Redirect::to("/connect").into_response());
    }
    let path = state.config.static_root.join("app.html");
    serve_file(&state.config.static_root, &path)
}

pub(crate) fn valid_connect_app_query(query: Option<&str>) -> bool {
    let Some(query) = query else {
        return false;
    };
    let mut connect_mode = false;
    let mut daemon_id = false;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "connect" => connect_mode = value == "1",
            "daemon_id" => daemon_id = !value.trim().is_empty(),
            _ => {}
        }
    }
    connect_mode && daemon_id
}

pub(crate) async fn static_asset(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    let path = safe_static_path(&state.config.static_root, uri.path())
        .ok_or_else(|| ApiError::not_found("not found"))?;
    serve_file(&state.config.static_root, &path)
}

pub(crate) fn safe_static_path(root: &Path, uri_path: &str) -> Option<PathBuf> {
    let trimmed = uri_path.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }
    let rel = Path::new(trimmed);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

pub(crate) fn serve_file(root: &Path, path: &Path) -> ApiResult<Response> {
    if !path.starts_with(root) || !path.is_file() {
        return Err(ApiError::not_found("not found"));
    }
    let body = std::fs::read(path).map_err(|e| ApiError::not_found(format!("not found: {e}")))?;
    let content_type = content_type_for_path(path);
    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        body,
    )
        .into_response())
}

pub(crate) fn content_type_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
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
    <p class="lede">The short version: this service makes introductions and carries ciphertext. Authority over your computers never lives here &mdash; not even when you sign in.</p>

    <h2>What this service actually does</h2>
    <p>Four jobs, all deliberately powerless: it <em>introduces</em> your browser to your computers (signaling), <em>relays</em> encrypted traffic when networks are awkward, <em>stores</em> your fleet list as client-signed records whose private fields are end-to-end encrypted, and <em>remembers</em> which computers your account claimed. Every session that reaches one of your computers is verified twice at the ends: your browser checks a signature made by the computer itself, and the computer checks a signature made by your browser&rsquo;s own key &mdash; a key that never leaves your device.</p>

    <h2>"But I sign in with a passkey&hellip;"</h2>
    <p>A fair question: doesn&rsquo;t signing in give the server something it could use?</p>
    <p>A passkey never hands over a key. Your device signs a one-time challenge, bound to this origin &mdash; the server can&rsquo;t replay it anywhere, can&rsquo;t sign anything with it, and can&rsquo;t derive anything from it. The signature proves you <em>to the rendezvous, for rendezvous-scoped things</em>: your claim list, your encrypted fleet metadata, your signaling session. The encryption key for that metadata is computed inside your authenticator (the WebAuthn PRF extension) and handed only to the page in your browser &mdash; it is not part of what the server receives.</p>

    <h2>If this service turned malicious</h2>
    <ol>
      <li><strong>It could lie in introductions.</strong><span>When relaying, it could claim your account is someone else &mdash; but computers treat account claims as the weakest identity there is: they only matter if the computer&rsquo;s owner already granted that account a role locally, hosted sessions are capped below full control by default, and the strong identity in every offer is your browser&rsquo;s end-to-end signature, which this service cannot forge.</span></li>
      <li><strong>It could deny service.</strong><span>Any relay can. You would notice, and nothing would be exposed.</span></li>
      <li><strong>It could serve this page with malicious code.</strong><span>The honest residual risk of any hosted web app. It is bounded on purpose: sessions from this origin are role-capped by every computer&rsquo;s own policy, your durable identity key is scoped to each origin (code served here can never wield the key your own computer&rsquo;s dashboard holds), and organization membership never flows through accounts. If you don&rsquo;t want to extend even this much trust, don&rsquo;t: browse via your own computer&rsquo;s address, or run your own rendezvous.</span></li>
    </ol>

    <div class="card good">
      <strong>The rule the whole design follows:</strong> privileged code is served by you or by the resource owner; authority is only ever minted by the target computer&rsquo;s local access control; global services carry introductions, ciphertext, and signatures &mdash; nothing else.
    </div>

    <h2>Notifications</h2>
    <p class="dim">Optional Web Push alerts ("your computer went offline") are composed from the polling presence this service already sees &mdash; no new knowledge &mdash; and each payload is encrypted to your browser&rsquo;s subscription, so the push relays in between carry ciphertext.</p>

    <h2>Names are checkable here</h2>
    <p class="dim">Every name binding this service hands out &mdash; which key a computer had when claimed, handle creations, revocation lists, verified badges &mdash; is committed to an append-only transparency log. Your browser pins the signed tree head and re-verifies on every visit that history only ever grew. Handles can carry <em>verified identity</em> badges (a DNS record or GitHub gist you control); verification is decoration, never authority. Dormant handles with no computers and no sign-ins are eventually freed &mdash; squatted names don&rsquo;t keep.</p>

    <h2>Organizations</h2>
    <p class="dim">Org membership is a document signed by the organization&rsquo;s own key, verified by each of its computers directly. This service stores at most the org&rsquo;s <em>revocation list</em> &mdash; also root-signed and rollback-protected, so the worst a malicious board can do is withhold it, never forge it.</p>

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
/// plus an honest fueling plan for after the claim. A separate const so
/// its CSS/JS braces stay out of the page-level `format!`; it derives
/// the command from `location.origin` at runtime, so a self-hosted
/// rendezvous advertises its own installer here too. The default answers'
/// command is server-rendered into the terminal (via the
/// `__ADVISOR_DEFAULT_CMD__` placeholder) so the page works without JS
/// and the one-command story is visible before any click. Every question
/// is about the agent's machine — the client side needs no install and
/// therefore no questions.
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
              ? '& ([scriptblock]::Create((irm ' + location.origin + "/install.ps1))) -Owner '\u003cyour-key\u003e'" + (svc ? ' -Service' : '')
              : 'curl -fsSL ' + location.origin + '/install.sh | sh -s -- --owner \u003cyour-key\u003e' + (svc ? ' --service' : '');
            document.getElementById('advps').textContent = pick.os === 'windows' ? 'PS> ' : '$ ';
            document.getElementById('advtitle').textContent = pick.os === 'windows' ? 'fresh box — PowerShell' : 'fresh box — sh';
            document.getElementById('advcmd').textContent = cmd;
            var plan = [];
            if (pick.box === 'laptop') {
              plan.push('<b>Fueling is optional here.</b> A local .env key works as-is; the vault still adds cross-device sync and one-click revocation.');
            } else {
              var watched = pick.solo === 'no';
              if (pick.fuel !== 'sub') {
                plan.push(watched
                  ? '<b>Anthropic & Gemini: client egress.</b> The box never holds a key — its provider calls detour through this browser, and stop when it closes. OpenAI’s API refuses browser relay, so lease that one with the offline window at “while connected only”.'
                  : '<b>API keys: leases with a 24 h offline window.</b> Borrowed in memory only, never on disk, revocable from any signed-in device.');
              }
              if (pick.fuel !== 'api') {
                plan.push('<b>Subscriptions: access-token OAuth leases</b> (the default) — your browser refreshes the token and leases only the short-lived result. Codex works out of the box.');
                plan.push(watched
                  ? '<b>Claude Code</b> still needs the full-credential opt-in (Anthropic’s token endpoint refuses browser refresh) — decide per box.'
                  : '<b>Unattended subscription runs</b> beyond the token’s life (≈ 1 h) need the full-credential opt-in: the honest trade is durable authority on the box for the lease window. Claude Code always needs it today.');
              }
            }
            document.getElementById('advplan').innerHTML = plan.map(function (item) { return '<li>' + item + '</li>'; }).join('');
            var note = { vps: 'A disposable box should hold nothing durable. With client egress the key was never on it; with access-token leases what lands there dies in minutes. Wipe it — or lose it — and nothing leaks.',
                         server: 'Nothing rests on disk either way — leases only bound what a runtime compromise could spend before you revoke.',
                         laptop: 'Custody buys the least on the machine your browser already runs on.' }[pick.box];
            if (svc) {
              note += pick.os === 'windows'
                ? ' -Service installs a Task Scheduler entry (at boot when elevated, at logon otherwise) supervised by a built-in restart loop; the installer prints the log file the claim phrase lands in. Run it from PowerShell.'
                : ' --service keeps the daemon alive past this SSH session via the platform’s own supervisor — systemd where present, launchd on macOS, cron plus the built-in supervisor elsewhere — and prints where the claim phrase lands.';
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
    // The placeholder must be entity-escaped or the browser eats it as a tag.
    let install_cmd = format!("curl -fsSL {origin}/install.sh | sh -s -- --owner &lt;your-key&gt;");
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
      <div class="mark"><img src="/logo.svg" alt="">intendant<span>.dev</span><span class="pill-alpha">pre-alpha</span></div>
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
        can run macOS, Linux, or Windows; yours just needs a browser —
        nothing to install on your side of the glass.
      </p>
      <div class="cta">
        <a class="btn" href="/connect">Open your dashboard</a>
        <a class="btn ghost" href="#install">Install a daemon</a>
      </div>
    </section>

    <section class="install-section" id="install">
      <h2>Stand up a daemon in about ninety seconds</h2>
      <p class="sectionlede">
        Four answers about the machine the agent will live on, and the exact
        command appears. That machine is the only one that installs anything
        — you can be reading this from your phone.
      </p>
      <div class="igrid">
        {advisor}
        <div>
          <div class="steps">
            <div class="step"><b><span class="n">1</span>Install</b>
              One command on a fresh box pins root authority to your browser's key. Nothing sensitive travels.</div>
            <div class="step"><b><span class="n">2</span>Claim</b>
              The daemon prints a twelve-word phrase; claim it from the browser you're already holding.</div>
            <div class="step"><b><span class="n">3</span>Fuel</b>
              Grant time-boxed credential leases from your encrypted vault — or relay calls through your browser and never hand over a key at all.</div>
          </div>
          <p class="installnote">
            New here? <a href="/connect">Sign in</a> first — your key is in the
            dashboard's Access drawer. Nothing sensitive travels in the command
            or lands on the box: the daemon boots already owned by you, you claim
            it with a twelve-word phrase, and it borrows credentials from your
            vault only while you let it.
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
        The Activity feed on a claimed daemon: autonomy is a dial, approvals
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
          The same state is a keystroke away in the terminal TUI and the CLI,
          and a glance away from your phone.</p>
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
          <h3>Fueling, not surrendering</h3>
          <p>Provider keys and subscription OAuth live end-to-end encrypted
          behind your passkeys, and a machine gets fuel one of two ways. A
          lease is borrowed authority — held in memory, renewed from your
          browser, dead on expiry or the moment you revoke it. Client egress
          goes further: the key never leaves your browser at all — the box's
          provider calls detour through the tab you're signed in on. A
          disposable VPS can be wiped, or seized, with nothing on it worth
          taking.</p>
          <div class="fuelmap">
            <div class="fuelrow"><span class="fueltag">lease</span>
              <span class="fuelflow">the key travels: vault <span class="fx">→</span> daemon memory <em>(expires on its own)</em> <span class="fx">→</span> provider calls from the box</span></div>
            <div class="fuelrow"><span class="fueltag">client egress</span>
              <span class="fuelflow">the calls travel: daemon <span class="fx">→</span> your browser <em>(the key stays here)</em> <span class="fx">→</span> provider</span></div>
          </div>
        </div>
        <div class="pic">
          <div class="shot">
            <img loading="lazy" src="/assets/landing/vault.webp" width="1800" height="975"
                 alt="The credential vault panel: three credentials with masked secrets, two active leases expiring in 15 minutes granted by @ada, re-fuel buttons, and a client-egress relay option.">
          </div>
          <div class="shotnote">Leases expire on their own; Revoke is always one click away.</div>
        </div>
      </div>

      <div class="trow rev">
        <div class="txt">
          <div class="eyebrow">Arrival</div>
          <h3>Claim a machine with twelve words</h3>
          <p>Start the daemon anywhere and it prints a claim phrase. Paste it
          in the browser you're already holding and the box is yours — owned
          by your key from first boot, reachable from every device you sign
          in on, with the powerful knobs one fold away when you want them.</p>
        </div>
        <div class="pic">
          <div class="shot">
            <img loading="lazy" src="/assets/landing/claim.webp" width="1800" height="635"
                 alt="Intendant Connect: a claimed computer named atlas shown online with uptime history, next to the add-a-computer flow that accepts a twelve-word claim phrase.">
          </div>
          <div class="shotnote">atlas, online seconds after its claim phrase was pasted.</div>
        </div>
      </div>

      <div class="trow">
        <div class="txt">
          <div class="eyebrow">The client</div>
          <h3>Nothing to install on your side</h3>
          <p>Most agent environments start by installing software on the
          device in front of you. Intendant never does: the whole client is
          a browser tab. Approve a diff from your phone, watch the live
          desktop from a tablet, run mission control from any laptop — same
          daemon, same authority, zero client software. On intendant.dev
          there is nothing to set up at all; even fully self-hosted, the
          one-time cost is trusting a certificate, never installing an app.</p>
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
          <p>Provider keys and subscription OAuth live end-to-end encrypted
          behind your passkeys. Daemons borrow leases that expire, or relay
          calls through your browser; disks hold nothing worth stealing.</p>
        </div>
        <div class="card">
          <h3>Every interface, any device</h3>
          <p>Web dashboard, terminal TUI, CLI, MCP, live voice, and phone
          calls — every capability reachable from each of them. The web
          client runs in any browser, phone included, with nothing to
          install client-side.</p>
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
          <h3>This service holds no authority</h3>
          <p>The rendezvous stores ciphertext and relays signaling. Your
          daemons mint and enforce their own access; passkeys and a
          transparency log keep the service honest — and you can
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
    .computer-actions .open {{ flex: 1 1 auto; }}
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
        <p class="hero-sub">Sign in with a passkey and open any machine you own, from any browser. This service only makes the introduction &mdash; each computer verifies you itself and decides what you may do, end to end.</p>
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
          Intendant is in private pre-alpha &mdash; creating an account needs an
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
        <li><strong>Holds no power</strong><span>An introducer and relay. Your computers check your identity themselves &mdash; <a href="/trust">how trust works here</a>.</span></li>
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
            <li>On that machine, start <code>intendant</code> with Connect enabled &mdash; it prints a 12&#8209;word claim phrase in its log.</li>
            <li>Paste the phrase here to link it to this account.</li>
          </ol>
          <div>
            <label for="claim-code">Claim phrase</label>
            <input id="claim-code" autocomplete="off" spellcheck="false" placeholder="twelve words from the startup log">
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
          <div class="sub">Give this user id to a daemon owner when they grant your account access under Access &rarr; People &amp; Devices.</div>
          <div class="user-id-row">
            <code id="session-user-id"></code>
            <button id="copy-user-id" class="ghost" type="button">Copy</button>
          </div>
        </div>
        <div class="advanced-block" id="orgs-block">
          <h3>Organizations</h3>
          <div class="sub">Signed membership documents this browser holds on this origin. They never touch this server &mdash; your browser presents them directly to daemons that trust the issuing org.</div>
          <div id="org-rows"></div>
        </div>
        <div class="advanced-block">
          <h3>What this account can and cannot do</h3>
          <div class="sub">It is rendezvous and navigation only &mdash; it grants nothing by itself. Every daemon decides access through its own local IAM, dashboard sessions verify a signature from the daemon itself, and private fields in Saved places sync end&#8209;to&#8209;end encrypted when your passkey supports PRF. <a href="/trust">The full story.</a></div>
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
          <div class="sub">Every name binding this service hands out (which key a computer had when claimed, handle creations, revocation lists, badges) is committed to an append-only log. Your browser pins the signed tree head and re-verifies consistency on every visit &mdash; rewriting history here is detectable, not just forbidden.</div>
          <div class="metric-row"><span id="log-pill" class="pill">checking&hellip;</span><button id="log-reset-trust" class="ghost hidden" title="Discard the pinned tree head and trust the log's current signing key from now on. Only do this if you expected the operator to rotate the key.">Reset trust</button></div>
        </div>
        <div class="advanced-block" id="push-block">
          <h3>Notifications</h3>
          <div class="sub">Get a notification on this browser when one of your computers goes offline or comes back. Alerts are composed from presence the rendezvous already sees, and delivered encrypted to this browser alone.</div>
          <div class="metric-row">
            <span id="push-status" class="pill">checking&hellip;</span>
            <button id="push-enable" class="secondary hidden">Enable on this browser</button>
            <button id="push-disable" class="ghost hidden">Disable</button>
            <button id="push-test" class="ghost hidden">Send a test</button>
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

// Fleet-sync encryption (trust architecture phase 5 follow-on): evaluate
// the WebAuthn PRF extension during the passkey ceremony and stash the
// per-tab secrets; /app derives AES keys from them so private fleet fields
// and the credential vault sync end-to-end encrypted. Two salts, one
// gesture: `first` feeds fleet-sync, `second` feeds the vault — separate
// PRF domains, so the two features never share key material. The server
// never sees either output.
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
function normalizeClaimPhrase(input) {{
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

/* Load or CREATE this origin's browser identity key — the exact record
   the dashboard app uses (IndexedDB intendant-client-identity/keys/v1,
   non-extractable P-256, {{privateKey, publicRaw, createdAtMs}}).
   Creating here is what makes bootstrap one ceremony: the key that
   claims is the key the daemon enrolls, and the dashboard then signs in
   with it. */
async function ensureOwnIdentity() {{
  if (!window.indexedDB || !crypto?.subtle) throw new Error('WebCrypto unavailable');
  const db = await new Promise((resolve, reject) => {{
    const req = indexedDB.open('intendant-client-identity', 1);
    req.onupgradeneeded = () => {{
      if (!req.result.objectStoreNames.contains('keys')) req.result.createObjectStore('keys');
    }};
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  }});
  try {{
    let record = await new Promise((resolve, reject) => {{
      const tx = db.transaction('keys', 'readonly');
      const req = tx.objectStore('keys').get('v1');
      req.onsuccess = () => resolve(req.result || null);
      req.onerror = () => reject(req.error);
    }});
    if (!record?.privateKey || !record?.publicRaw) {{
      const pair = await crypto.subtle.generateKey(
        {{ name: 'ECDSA', namedCurve: 'P-256' }},
        false,
        ['sign']
      );
      const publicRaw = await crypto.subtle.exportKey('raw', pair.publicKey);
      record = {{ privateKey: pair.privateKey, publicRaw, createdAtMs: Date.now() }};
      await new Promise((resolve, reject) => {{
        const tx = db.transaction('keys', 'readwrite');
        tx.objectStore('keys').put(record, 'v1');
        tx.oncomplete = () => resolve();
        tx.onerror = () => reject(tx.error);
      }});
    }}
    return {{ publicRawB64u: bufToB64u(record.publicRaw) }};
  }} finally {{
    db.close();
  }}
}}

/* First-owner bootstrap tag (mirrors connect_rendezvous.rs): HMAC-SHA256
   keyed by SHA-256(normalized phrase) over a payload binding this
   browser's key and account. The service relays it blind — it holds the
   phrase's hash, never the phrase, so only a phrase-holder can endorse a
   key for enrollment. */
async function bootstrapTag(normalizedPhrase, daemonId, daemonPublicKey, clientKeyB64u, userId, accountName) {{
  const phraseDigest = await crypto.subtle.digest(
    'SHA-256',
    new TextEncoder().encode(normalizedPhrase)
  );
  const hmacKey = await crypto.subtle.importKey(
    'raw',
    phraseDigest,
    {{ name: 'HMAC', hash: 'SHA-256' }},
    false,
    ['sign']
  );
  const payload = `intendant-connect-bootstrap-v1\n${{daemonId}}\n${{daemonPublicKey}}\n${{clientKeyB64u}}\n${{userId}}\n${{accountName}}\n`;
  const tag = await crypto.subtle.sign('HMAC', hmacKey, new TextEncoder().encode(payload));
  return bufToB64u(tag);
}}

async function claimDaemon() {{
  const claimCode = $('claim-code').value.trim();
  if (!claimCode) throw new Error('Claim phrase is required');
  setBusy('claim', true);
  setStatus('claim-status', 'Waiting for daemon proof', '');
  try {{
    const normalized = normalizeClaimPhrase(claimCode);
    if (!normalized) throw new Error('Claim phrase is required');
    // Hash-only claim: the service routes by digest and never sees the
    // plaintext phrase (a daemon-minted phrase must stay between the
    // daemon and this browser).
    const start = await api('/api/claims/claim', {{
      method: 'POST',
      body: JSON.stringify({{ claim_code_hash: await sha256B64uOfText(normalized) }}),
    }});
    let bootstrap = false;
    if (start.needs_bootstrap_arm) {{
      bootstrap = true;
      setStatus('claim-status', 'Fresh daemon — enrolling this browser as its first owner', '');
      const identity = await ensureOwnIdentity();
      const tag = await bootstrapTag(
        normalized,
        String(start.daemon_id || ''),
        String(start.daemon_public_key || ''),
        identity.publicRawB64u,
        String(state.user?.id || ''),
        String(state.user?.account_name || '')
      );
      await api(`/api/claims/${{encodeURIComponent(start.claim_id)}}/arm`, {{
        method: 'POST',
        body: JSON.stringify({{ client_key: identity.publicRawB64u, client_key_tag: tag }}),
      }});
    }}
    const deadline = Date.now() + 65000;
    while (Date.now() < deadline) {{
      await new Promise(resolve => setTimeout(resolve, 750));
      const status = await api(`/api/claims/${{encodeURIComponent(start.claim_id)}}`);
      if (status.result?.status === 'approved') {{
        setStatus(
          'claim-status',
          bootstrap
            ? `Claimed ${{status.result.daemon_id}} — and this browser is enrolled as its first owner (role: root, co-signed by the daemon). Open it from your computers list; the dashboard signs in with this browser's key.`
            : `Rendezvous route claimed for ${{status.result.daemon_id}}. Next: open that daemon directly (its https://host:8765 address) as root, go to Access → People & Devices, and grant this account a role — until then the daemon will refuse hosted dashboard control.`,
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
    rows.innerHTML = '<div class="empty-hint">None stored in this browser. Daemon dashboards keep a membership document here when you join with one; it is then presented automatically on every connection.</div>';
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
      if (!confirm(`Remove the stored @${{handle}} document from this browser? Access already granted on daemons is unaffected; automatic presentation stops.`)) return;
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
        <button class="open" data-open="${{escapeAttr(daemonId)}}">Open</button>
        <button class="secondary" data-rename="${{escapeAttr(daemonId)}}">Rename</button>
      </div>
      ${{presenceSparkline(daemon)}}
      <details>
        <summary>Details</summary>
        <div class="kv">
          <div><div class="k">Daemon id</div><code>${{escapeHtml(daemonId)}}</code></div>
          <div><div class="k">Public key &mdash; sessions verify this end to end</div><code>${{escapeHtml(key)}}</code></div>
          <div class="danger-row"><button class="danger" data-revoke="${{escapeAttr(daemonId)}}">Disconnect from this account</button></div>
        </div>
      </details>`;
    grid.appendChild(card);
  }}
  grid.querySelectorAll('[data-open]').forEach(button => {{
    button.addEventListener('click', () => {{
      const id = button.getAttribute('data-open');
      window.location.href = `/app?connect=1&daemon_id=${{encodeURIComponent(id)}}`;
    }});
  }});
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
      ? 'End-to-end encrypted — opens on a device signed in with your passkey'
      : String(target.route_label || target.route || target.url || 'Remembered route');
    const online = target.online || target.connected;
    const url = String(target.url || '');
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
        <button data-fleet-open="${{escapeAttr(url)}}" ${{url ? '' : 'disabled'}}>Open</button>
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
if (params.get('claim_code')) $('claim-code').value = params.get('claim_code');
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

    #[test]
    fn app_route_requires_connect_mode_and_daemon_id() {
        assert!(valid_connect_app_query(Some(
            "connect=1&daemon_id=vortex-deb-x11-intendant"
        )));
        assert!(valid_connect_app_query(Some(
            "daemon_id=vortex-deb-x11-intendant&connect=1"
        )));
        assert!(!valid_connect_app_query(None));
        assert!(!valid_connect_app_query(Some("")));
        assert!(!valid_connect_app_query(Some(
            "daemon_id=vortex-deb-x11-intendant"
        )));
        assert!(!valid_connect_app_query(Some("connect=1")));
        assert!(!valid_connect_app_query(Some("connect=0&daemon_id=daemon")));
        assert!(!valid_connect_app_query(Some("connect=1&daemon_id=%20")));
    }

    #[test]
    fn trust_page_states_the_model() {
        let html = trust_ui_html("https://connect.intendant.dev");
        assert!(html.contains("<title>How trust works"));
        assert!(html.contains("rendezvous-scoped things"));
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
        // The install one-liner advertises the serving origin, so a
        // self-hosted rendezvous shows its own installer — with the
        // placeholder entity-escaped so browsers render it as text.
        assert!(html.contains("curl -fsSL https://rendezvous.example/install.sh"));
        assert!(html.contains("--owner &lt;your-key&gt;"));
        assert!(!html.contains("--owner <your-key>"));
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
        // The differentiator is stated where people will read it: the client
        // installs nothing, on any device — only the agent's machine does.
        assert!(html.contains("Nothing to install on your side"));
        assert!(html.contains("nothing to install on your side of the glass"));
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
        // Custody names the two fueling modes by what travels: the key
        // (lease) vs the calls (client egress — the disposable-box mode).
        assert!(html.contains(r#"class="fuelmap""#));
        assert!(html.contains("the key travels:"));
        assert!(html.contains("the calls travel:"));
        // The canonical mark, not an ad-hoc monogram: favicon + header logo.
        assert!(html.contains(r#"<link rel="icon" type="image/svg+xml" href="/logo.svg">"#));
        assert!(html.contains(r#"<link rel="icon" type="image/png" href="/favicon.png">"#));
        assert!(html.contains(r#"<img src="/logo.svg""#));
        assert!(!html.contains("data:image/svg"));
        // The deployment advisor LEADS the install section — no fold to
        // find, four questions all about the agent's machine (the client
        // side installs nothing, so it gets no questions), and
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
        assert!(html.contains(
            "curl -fsSL https://rendezvous.example/install.sh | sh -s -- --owner &lt;your-key&gt; --service"
        ));
        assert!(!html.contains("__ADVISOR_DEFAULT_CMD__"));
        assert!(html.contains("location.origin + '/install.sh"));
        assert!(html.contains("/install.ps1"));
        assert!(html.contains("--service"));
        assert!(html.contains("-Service"));
        // No init system is asserted as a given — the note speaks in
        // native-supervisor terms, not systemd.
        assert!(!html.contains("journalctl"));
        // Honest pre-alpha framing before anyone clicks Sign in.
        assert!(html.contains(r#"<span class="pill-alpha">pre-alpha</span>"#));
    }

    #[test]
    fn connect_page_frames_the_private_alpha() {
        let html = connect_ui_html(
            "https://intendant.dev",
            "Intendant Connect",
            "Rendezvous account",
        );
        // The invite dead-end explains itself and offers the two open paths.
        assert!(html.contains("private pre-alpha"));
        assert!(html.contains("self-hosting is never gated"));
        assert!(html.contains(r#"$('invite-note').classList.toggle"#));
        // Shareable invite links prefill the code.
        assert!(html.contains("params.get('invite')"));
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
    fn embedded_installer_is_the_bootstrap_script() {
        assert!(
            INSTALL_SH.starts_with("#!/bin/sh"),
            "installer must be a sh script"
        );
        assert!(
            INSTALL_SH.contains("--owner"),
            "installer must support the owner bootstrap"
        );
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
            "$Owner",
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

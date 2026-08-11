//! Embedded dashboard assets and their HTTP serving: the include_str!/
//! include_bytes! payloads (app shell, wasm bundles, icons, vendored JS),
//! version/etag stamping, cache-control policy, and the static-asset
//! response builder used by the gateway dispatch chain.

use super::*;

pub(crate) const APP_HTML: &str = include_str!("../../../../static/app.html");

/// Build stamp of the embedded dashboard bundle — the value
/// app-html-assembler substituted for `__INTENDANT_APP_BUILD__` (first 16
/// hex chars of the sha256 over the raw manifest-ordered fragments).
/// Extracted once from [`APP_HTML`], so the daemon and the SPA it serves
/// cannot disagree by construction; `/config` reports it and a dashboard
/// tab whose own stamp differs knows it predates the served bundle. Empty
/// when the artifact carries no stamp (pre-stamp checkouts) — the SPA
/// treats an absent value as "no signal", never as a mismatch.
pub(crate) fn app_build() -> &'static str {
    static STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STAMP.get_or_init(|| extract_app_build(APP_HTML).unwrap_or_default())
}

fn extract_app_build(html: &str) -> Option<String> {
    let marker = "const INTENDANT_APP_BUILD = '";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];
    let value = &rest[..rest.find('\'')?];
    (value.len() == 16 && value.bytes().all(|b| b.is_ascii_hexdigit())).then(|| value.to_string())
}

// The vault crypto kernel: the small, separately served worker that owns
// the vault's key material. app.html pins its sha256 (VAULT_KERNEL_SHA256,
// minted by crates/app-html-assembler) and the page refuses to instantiate
// a kernel whose bytes hash differently — the embedded pair below is
// therefore always self-consistent, and the parity test in this module
// re-derives the hash to catch a kernel edit that skipped regeneration.
pub(crate) const VAULT_KERNEL_JS: &str = include_str!("../../../../static/vault-kernel.js");

pub(crate) const AUDIO_PROCESSOR_JS: &str = include_str!("../../../../static/audio-processor.js");

pub(crate) const ICON_128_PNG: &[u8] = include_bytes!("../../../../static/icon-128.png");

pub(crate) const ICON_512_PNG: &[u8] = include_bytes!("../../../../static/icon-512.png");

pub(crate) const ICON_512_MASKABLE_PNG: &[u8] =
    include_bytes!("../../../../static/icon-512-maskable.png");

pub(crate) const APPLE_TOUCH_ICON_PNG: &[u8] =
    include_bytes!("../../../../static/apple-touch-icon.png");

pub(crate) const MANIFEST_WEBMANIFEST: &str =
    include_str!("../../../../static/manifest.webmanifest");

pub(crate) const WASM_WEB_JS: &str = include_str!("../../../../static/wasm-web/presence_web.js");

pub(crate) const WASM_WEB_BIN: &[u8] =
    include_bytes!("../../../../static/wasm-web/presence_web_bg.wasm");

pub(crate) const WASM_STATION_JS: &str =
    include_str!("../../../../static/wasm-station/station_web.js");

pub(crate) const WASM_STATION_BIN: &[u8] =
    include_bytes!("../../../../static/wasm-station/station_web_bg.wasm");

pub(crate) const THREE_MODULE_JS: &str = include_str!("../../../../static/three.module.min.js");

pub(crate) const CODEMIRROR_BUNDLE_JS: &str =
    include_str!("../../../../static/codemirror-bundle.js");

pub(crate) const CODEMIRROR_BUNDLE_CSS: &str =
    include_str!("../../../../static/codemirror-bundle.css");

// Vendored xterm.js (MIT). Previously loaded from jsdelivr with SRI
// pins — the one external fetch in the dashboard; embedding it keeps
// the daemon-served terminal working offline and over trusted LAN/mTLS
// dashboard routes. These bytes hash-match the exact SRI digests the CDN
// loader pinned.
pub(crate) const XTERM_JS: &str = include_str!("../../../../static/xterm.min.js");

// D-2 tile-test harness (parked seed): fetched by the dashboard only
// when ?tile-test=1 / localStorage.tileTest is set.
pub(crate) const TILE_TEST_HARNESS_JS: &str =
    include_str!("../../../../static/tile-test-harness.js");

pub(crate) const XTERM_ADDON_FIT_JS: &str =
    include_str!("../../../../static/xterm-addon-fit.min.js");

pub(crate) const XTERM_CSS: &str = include_str!("../../../../static/xterm.css");

// Self-hosted variable fonts (SIL OFL 1.1; license texts ship in
// static/fonts/). Referenced by the @font-face rules in
// static/app/09-styles-fonts.css — the dashboard must stay fully
// self-contained for offline and trusted LAN/mTLS use.
pub(crate) const FONT_HANKEN_LATIN: &[u8] =
    include_bytes!("../../../../static/fonts/hanken-grotesk-latin.woff2");

pub(crate) const FONT_HANKEN_LATIN_EXT: &[u8] =
    include_bytes!("../../../../static/fonts/hanken-grotesk-latin-ext.woff2");

pub(crate) const FONT_JBMONO_LATIN: &[u8] =
    include_bytes!("../../../../static/fonts/jetbrains-mono-latin.woff2");

pub(crate) const FONT_JBMONO_LATIN_EXT: &[u8] =
    include_bytes!("../../../../static/fonts/jetbrains-mono-latin-ext.woff2");

/// Compute a short content hash for cache-busting embedded static assets.
/// When the WASM, JS, or favicon changes (i.e. a new build), the hash changes,
/// the URL changes, and browsers fetch the new version.
pub(crate) fn asset_version_hash() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    WASM_WEB_BIN.hash(&mut hasher);
    WASM_WEB_JS.hash(&mut hasher);
    WASM_STATION_BIN.hash(&mut hasher);
    WASM_STATION_JS.hash(&mut hasher);
    ICON_128_PNG.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Process-wide cached [`asset_version_hash`] — the embedded assets are
/// compile-time constants, so the hash never changes at runtime and there
/// is no point re-hashing ~4 MB per request.
pub(crate) fn asset_version() -> &'static str {
    static ASSET_VERSION: OnceLock<String> = OnceLock::new();
    ASSET_VERSION.get_or_init(asset_version_hash)
}

/// Strong per-asset ETag token (16 hex chars of a content hash). Rendered
/// on the wire as a quoted strong ETag: `ETag: "<token>"`.
pub(crate) fn asset_etag(body: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// One embedded static asset with its lazily computed ETag and (where it
/// pays) a pre-gzipped body, served by the static-asset routing arms.
pub(crate) struct EmbeddedStaticAsset {
    content_type: &'static str,
    body: &'static [u8],
    etag: String,
    /// Pre-gzipped body; `None` when compression doesn't pay (tiny files,
    /// already-compressed PNG).
    gzip: Option<Vec<u8>>,
}

impl EmbeddedStaticAsset {
    pub(crate) fn view(&self) -> StaticAssetView<'_> {
        StaticAssetView {
            content_type: self.content_type,
            body: self.body,
            etag: &self.etag,
            gzip: self.gzip.as_deref(),
            cache_control: None,
        }
    }
}

/// Map from exact request path to embedded asset. Built once, on first
/// static-asset request; gzipping the ~4 MB of embedded assets is paid a
/// single time per process.
pub(crate) fn embedded_static_asset(path: &str) -> Option<&'static EmbeddedStaticAsset> {
    static EMBEDDED_STATIC_ASSETS: OnceLock<HashMap<&'static str, EmbeddedStaticAsset>> =
        OnceLock::new();
    let assets = EMBEDDED_STATIC_ASSETS.get_or_init(|| {
        let mut map = HashMap::new();
        let mut insert =
            |path: &'static str, content_type: &'static str, body: &'static [u8], compressible| {
                let gzip = (compressible && body.len() >= GZIP_MIN_BYTES)
                    .then(|| gzip_compress(body))
                    .filter(|gz| gz.len() < body.len());
                map.insert(
                    path,
                    EmbeddedStaticAsset {
                        content_type,
                        body,
                        etag: asset_etag(body),
                        gzip,
                    },
                );
            };
        insert(
            "/wasm-web/presence_web_bg.wasm",
            "application/wasm",
            WASM_WEB_BIN,
            true,
        );
        insert(
            "/wasm-station/station_web_bg.wasm",
            "application/wasm",
            WASM_STATION_BIN,
            true,
        );
        insert(
            "/wasm-web/presence_web.js",
            "application/javascript",
            WASM_WEB_JS.as_bytes(),
            true,
        );
        insert(
            "/wasm-station/station_web.js",
            "application/javascript",
            WASM_STATION_JS.as_bytes(),
            true,
        );
        insert(
            "/three.module.min.js",
            "application/javascript",
            THREE_MODULE_JS.as_bytes(),
            true,
        );
        insert(
            "/codemirror-bundle.js",
            "application/javascript",
            CODEMIRROR_BUNDLE_JS.as_bytes(),
            true,
        );
        insert(
            "/codemirror-bundle.css",
            "text/css",
            CODEMIRROR_BUNDLE_CSS.as_bytes(),
            true,
        );
        insert(
            "/audio-processor.js",
            "application/javascript",
            AUDIO_PROCESSOR_JS.as_bytes(),
            true,
        );
        insert(
            "/vault-kernel.js",
            "application/javascript",
            VAULT_KERNEL_JS.as_bytes(),
            true,
        );
        insert(
            "/tile-test-harness.js",
            "application/javascript",
            TILE_TEST_HARNESS_JS.as_bytes(),
            true,
        );
        insert(
            "/xterm.min.js",
            "application/javascript",
            XTERM_JS.as_bytes(),
            true,
        );
        insert(
            "/xterm-addon-fit.min.js",
            "application/javascript",
            XTERM_ADDON_FIT_JS.as_bytes(),
            true,
        );
        insert("/xterm.css", "text/css", XTERM_CSS.as_bytes(), true);
        // woff2 is already Brotli-compressed; gzip would only add overhead.
        insert(
            "/fonts/hanken-grotesk-latin.woff2",
            "font/woff2",
            FONT_HANKEN_LATIN,
            false,
        );
        insert(
            "/fonts/hanken-grotesk-latin-ext.woff2",
            "font/woff2",
            FONT_HANKEN_LATIN_EXT,
            false,
        );
        insert(
            "/fonts/jetbrains-mono-latin.woff2",
            "font/woff2",
            FONT_JBMONO_LATIN,
            false,
        );
        insert(
            "/fonts/jetbrains-mono-latin-ext.woff2",
            "font/woff2",
            FONT_JBMONO_LATIN_EXT,
            false,
        );
        // PNG is already deflate-compressed; gzip would only add overhead.
        insert("/icon-128.png", "image/png", ICON_128_PNG, false);
        insert("/favicon.ico", "image/png", ICON_128_PNG, false);
        insert("/icon-512.png", "image/png", ICON_512_PNG, false);
        insert(
            "/icon-512-maskable.png",
            "image/png",
            ICON_512_MASKABLE_PNG,
            false,
        );
        insert(
            "/apple-touch-icon.png",
            "image/png",
            APPLE_TOUCH_ICON_PNG,
            false,
        );
        insert(
            "/manifest.webmanifest",
            "application/manifest+json",
            MANIFEST_WEBMANIFEST.as_bytes(),
            false,
        );
        map
    });
    assets.get(path)
}

/// GET/HEAD + exact-path gate for one static-asset routing arm.
///
/// Returns the embedded asset only when the method is GET or HEAD *and*
/// `path` (the request target with its query string already stripped) is
/// one of `paths`; `None` lets the request fall through to later routing
/// arms. Exact-path matching is what prevents the historical shadowing
/// bug where `request_line.contains(...)` swallowed API requests that
/// merely mentioned an asset path in a query parameter (e.g.
/// `GET /api/fs/stat?path=/wasm-station/station_web_bg.wasm`).
pub(crate) fn static_asset_arm(
    method: &str,
    path: &str,
    paths: &[&str],
) -> Option<&'static EmbeddedStaticAsset> {
    if method != "GET" && method != "HEAD" {
        return None;
    }
    if !paths.contains(&path) {
        return None;
    }
    embedded_static_asset(path)
}

/// Cache-Control policy for the versioned static assets: a request whose
/// query string carries the *current* cache-busting hash (`?v=<hash>`, as
/// rewritten into app.html) may cache forever — a new build changes the
/// hash and thus the URL. Anything else (stale buster, no buster) stays on
/// cheap ETag revalidation.
pub(crate) fn asset_cache_control(query: &str, current_version: &str) -> &'static str {
    let versioned = query
        .split('&')
        .any(|pair| pair.strip_prefix("v=") == Some(current_version));
    if versioned {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache, must-revalidate"
    }
}

/// Borrowed view of one static asset for [`build_static_asset_response`].
pub(crate) struct StaticAssetView<'a> {
    pub(crate) content_type: &'a str,
    pub(crate) body: &'a [u8],
    /// Bare ETag token (no quotes); rendered as a quoted strong ETag.
    pub(crate) etag: &'a str,
    /// Pre-gzipped body, when compression pays for this asset.
    pub(crate) gzip: Option<&'a [u8]>,
    /// `Some(...)` pins Cache-Control (app.html stays `no-cache` — it is
    /// the entry point carrying the rewritten `?v=` busters); `None`
    /// applies [`asset_cache_control`]'s `?v=` policy.
    pub(crate) cache_control: Option<&'static str>,
}

/// Build a complete HTTP/1.1 response (header bytes + body) for a static
/// asset: conditional requests (`If-None-Match` → `304 Not Modified` with
/// an empty body), gzip negotiation via `Accept-Encoding`, HEAD (same
/// headers as GET, no body), CORS, and the `?v=` Cache-Control policy.
///
/// `keep_alive` is the exchange's keep-alive verdict
/// (`DemuxStream::exchange_reusable` at the serving arm): every shape
/// this builder emits is self-framing (`Content-Length` on 200, no body
/// on 304/HEAD), so static assets are prime keep-alive citizens — the
/// whole point of the request loop is not paying a TCP+TLS handshake
/// per asset on a cold dashboard load.
pub(crate) fn build_static_asset_response(
    method: &str,
    header_text: &str,
    query: &str,
    current_version: &str,
    asset: StaticAssetView<'_>,
    keep_alive: bool,
) -> Vec<u8> {
    let cache_control = asset
        .cache_control
        .unwrap_or_else(|| asset_cache_control(query, current_version));
    // Encoding varies by Accept-Encoding for assets with a gzip variant,
    // so caches must key on it.
    let vary = if asset.gzip.is_some() {
        "Vary: Accept-Encoding\r\n"
    } else {
        ""
    };
    if if_none_match_matches(header_text, asset.etag) {
        let response = HttpResponse::new("304 Not Modified")
            .header("ETag", format!("\"{}\"", asset.etag))
            .header("Cache-Control", cache_control)
            .header_segment(vary)
            .header("Access-Control-Allow-Origin", "*");
        let response = if asset.content_type.starts_with("text/html") {
            response.deny_framing()
        } else {
            response
        };
        return response.connection_reuse(keep_alive).into_bytes();
    }
    let gzip_body = asset
        .gzip
        .filter(|_| accept_encoding_allows_gzip(header_text));
    let (payload, content_encoding) = match gzip_body {
        Some(gz) => (gz, "Content-Encoding: gzip\r\n"),
        None => (asset.body, ""),
    };
    let mut response = HttpResponse::new("200 OK")
        .header("Content-Type", asset.content_type)
        .header("Content-Length", payload.len().to_string())
        .header_segment(content_encoding)
        .header("ETag", format!("\"{}\"", asset.etag))
        .header("Cache-Control", cache_control)
        .header_segment(vary)
        .header("Access-Control-Allow-Origin", "*");
    if asset.content_type.starts_with("text/html") {
        response = response.deny_framing();
    }
    let mut response = response.connection_reuse(keep_alive).into_bytes();
    if method != "HEAD" {
        response.extend_from_slice(payload);
    }
    response
}

/// Rewrite every occurrence of `path` in `html` to `path?v={version}`,
/// normalizing any `?v=<token>` already following the path so the result
/// always carries exactly one buster (idempotent; never `?v=a?v=b`, even
/// if the source HTML hardcodes a stale buster like `?v=wgpu29`).
pub(crate) fn rewrite_asset_url_with_version(html: &str, path: &str, version: &str) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    while let Some(idx) = rest.find(path) {
        out.push_str(&rest[..idx]);
        out.push_str(path);
        out.push_str("?v=");
        out.push_str(version);
        let mut tail = &rest[idx + path.len()..];
        if let Some(stripped) = tail.strip_prefix("?v=") {
            let token_len = stripped
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
                .unwrap_or(stripped.len());
            tail = &stripped[token_len..];
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Asset URLs inside app.html that carry `?v=` cache busters. The
/// spawn-time rewrite of the embedded copy and the
/// `INTENDANT_APP_HTML_PATH` per-request override apply the same set.
const APP_HTML_VERSIONED_ASSETS: [&str; 13] = [
    "/xterm.css",
    "/wasm-web/presence_web.js",
    "/wasm-web/presence_web_bg.wasm",
    "/wasm-station/station_web.js",
    "/wasm-station/station_web_bg.wasm",
    "/three.module.min.js",
    "/codemirror-bundle.js",
    "/codemirror-bundle.css",
    "/icon-128.png",
    "/fonts/hanken-grotesk-latin.woff2",
    "/fonts/hanken-grotesk-latin-ext.woff2",
    "/fonts/jetbrains-mono-latin.woff2",
    "/fonts/jetbrains-mono-latin-ext.woff2",
];

/// Rewrite every [`APP_HTML_VERSIONED_ASSETS`] URL in an app.html body to
/// carry the current `?v=` buster.
pub(crate) fn rewrite_app_html_asset_urls(html: String, version: &str) -> String {
    APP_HTML_VERSIONED_ASSETS.iter().fold(html, |html, path| {
        rewrite_asset_url_with_version(&html, path, version)
    })
}

/// The `INTENDANT_APP_HTML_PATH` dev override: serve the dashboard entry
/// point from this disk path instead of the embedded copy, re-reading it
/// on every request — a fragment edit shows up on browser refresh after
/// `cargo run -p app-html-assembler`, with no daemon rebuild or restart.
/// Read once at gateway spawn; a whitespace-only value counts as unset.
pub(crate) fn app_html_override_path() -> Option<std::path::PathBuf> {
    app_html_override_from(std::env::var("INTENDANT_APP_HTML_PATH").ok())
}

fn app_html_override_from(raw: Option<String>) -> Option<std::path::PathBuf> {
    let raw = raw?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(trimmed))
}

/// Serve one dashboard request under the `INTENDANT_APP_HTML_PATH`
/// override: fresh disk read, the same `?v=` rewrite as the embedded
/// copy, a fresh strong ETag (an unchanged file still revalidates to
/// 304), no gzip. A read failure is a loud 500 naming the override —
/// falling back to the embedded copy would silently mask the broken
/// path the developer is trying to iterate on.
pub(crate) fn app_html_override_response(
    method: &str,
    header_text: &str,
    query: &str,
    path: &std::path::Path,
    keep_alive: bool,
) -> Vec<u8> {
    match std::fs::read_to_string(path) {
        Ok(html) => {
            let html = rewrite_app_html_asset_urls(html, asset_version());
            let etag = asset_etag(html.as_bytes());
            build_static_asset_response(
                method,
                header_text,
                query,
                asset_version(),
                StaticAssetView {
                    content_type: "text/html; charset=utf-8",
                    body: html.as_bytes(),
                    etag: &etag,
                    gzip: None,
                    cache_control: Some("no-cache"),
                },
                keep_alive,
            )
        }
        Err(err) => {
            // The configured path and the OS error stay server-side:
            // this response is served certificate-free from the shell
            // lane, so a body echoing the absolute override path would
            // hand any reachable page a local username/project layout
            // whenever the dev override is broken.
            eprintln!(
                "[web_gateway] INTENDANT_APP_HTML_PATH read failed ({}): {err}",
                path.display()
            );
            let body = "INTENDANT_APP_HTML_PATH override is active but its \
                 configured app.html could not be read. Check the daemon log \
                 for the path and error, then fix the path (or unset \
                 INTENDANT_APP_HTML_PATH) and refresh.\n"
                .to_string();
            let mut response = HttpResponse::new("500 Internal Server Error")
                .header("Content-Type", "text/plain; charset=utf-8")
                .header("Content-Length", body.len().to_string())
                .header("Cache-Control", "no-store")
                .header("Access-Control-Allow-Origin", "*")
                .connection_reuse(keep_alive)
                .into_bytes();
            if method != "HEAD" {
                response.extend_from_slice(body.as_bytes());
            }
            response
        }
    }
}

/// Under the `INTENDANT_APP_HTML_PATH` dev override, serve /vault-kernel.js
/// from the override file's sibling `vault-kernel.js` when one exists (fresh
/// disk read per request, like the app.html override itself). The pin inside
/// the overridden app.html was minted from that sibling by the assembler, so
/// serving the embedded — possibly stale — kernel would trip the page's
/// integrity check mid-iteration. `None` (no override dir, no sibling, read
/// error) falls back to the embedded kernel: fail-open here is correct
/// because the page's hash check is the enforcement point either way.
pub(crate) fn vault_kernel_override_response(
    method: &str,
    header_text: &str,
    query: &str,
    app_html_path: &std::path::Path,
    keep_alive: bool,
) -> Option<Vec<u8>> {
    let sibling = app_html_path.parent()?.join("vault-kernel.js");
    let body = std::fs::read(&sibling).ok()?;
    let etag = asset_etag(&body);
    Some(build_static_asset_response(
        method,
        header_text,
        query,
        asset_version(),
        StaticAssetView {
            content_type: "application/javascript",
            body: &body,
            etag: &etag,
            gzip: None,
            cache_control: Some("no-cache"),
        },
        keep_alive,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATION_WASM_ARM_PATHS: &[&str] = &[
        "/wasm-web/presence_web_bg.wasm",
        "/wasm-station/station_web_bg.wasm",
    ];

    /// Lowercase-hex sha256, matching the assembler's pin encoding.
    fn sha256_hex(data: &[u8]) -> String {
        use sha2::Digest as _;
        sha2::Sha256::digest(data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// The build stamp: the embedded app.html must carry a minted (16-hex)
    /// `INTENDANT_APP_BUILD` value, never the raw placeholder — otherwise
    /// every served tab would see a stampless daemon and the stale-tab
    /// reload nudge goes blind.
    #[test]
    fn app_build_stamp_is_minted_in_embedded_artifact() {
        let stamp = app_build();
        assert_eq!(
            stamp.len(),
            16,
            "embedded app.html carries no minted INTENDANT_APP_BUILD stamp — \
             regenerate with `cargo run -p app-html-assembler` and commit"
        );
        assert!(stamp.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(
            !APP_HTML.contains("__INTENDANT_APP_BUILD__"),
            "the raw placeholder must never ship"
        );
    }

    /// The vault-kernel hash pin: the embedded app.html must pin exactly the
    /// sha256 of the embedded kernel bytes. This is the daemon-side parity
    /// gate for the pinned-kernel design (the page refuses to instantiate a
    /// kernel whose hash differs): an edit to static/vault-kernel.js that
    /// skips `cargo run -p app-html-assembler` (any cargo build also
    /// reassembles) fails here instead of shipping a dashboard whose vault
    /// refuses to unlock.
    #[test]
    fn vault_kernel_hash_pin_matches_embedded_kernel() {
        let marker = "const VAULT_KERNEL_SHA256 = '";
        let start = APP_HTML
            .find(marker)
            .expect("app.html must carry the VAULT_KERNEL_SHA256 pin");
        let rest = &APP_HTML[start + marker.len()..];
        let end = rest.find('\'').expect("pin constant must be quoted");
        let pinned = &rest[..end];
        assert_eq!(
            pinned.len(),
            64,
            "pin must be a full lowercase-hex sha256, got {pinned:?} — \
             was app.html assembled without static/vault-kernel.js?"
        );
        assert_eq!(
            pinned,
            sha256_hex(VAULT_KERNEL_JS.as_bytes()),
            "static/app.html pins a different kernel hash than \
             static/vault-kernel.js — regenerate with `cargo run -p \
             app-html-assembler` and commit both files together"
        );
        // The placeholder itself must never ship.
        assert!(
            !APP_HTML.contains("__VAULT_KERNEL_SHA256__"),
            "unsubstituted vault-kernel placeholder in app.html"
        );
        // The kernel is served at the path the page fetches.
        let asset = embedded_static_asset("/vault-kernel.js").expect("kernel must be embedded");
        assert_eq!(asset.content_type, "application/javascript");
        assert_eq!(asset.body, VAULT_KERNEL_JS.as_bytes());
    }

    /// The update-copy honesty pin: the update chip, the release
    /// (availability) chip, the Daemon update panel, and the swap
    /// confirm affordance each state the same plain sentence — installs
    /// alongside, running sessions finish uninterrupted, the old
    /// version may keep running while they do. Pinned against the
    /// served bytes (and the docs chapter) so a rewording that drops
    /// the honest bounds fails here instead of shipping.
    #[test]
    fn update_copy_honesty_sentence_is_pinned() {
        const SENTENCE: &str = "The update installs alongside the current version — running sessions finish uninterrupted, and the old version may keep running until they are done.";
        let served = APP_HTML.matches(SENTENCE).count();
        assert_eq!(
            served, 4,
            "the served dashboard must state the update-copy honesty sentence \
             on exactly its four surfaces (update chip, release chip, Daemon \
             update panel, swap confirm) — found {served}; edit the \
             static/app/ fragments and regenerate with `cargo run -p \
             app-html-assembler`"
        );
        let chapter_path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/src/web-dashboard.md");
        let chapter = std::fs::read_to_string(chapter_path)
            .expect("docs/src/web-dashboard.md is part of the checkout");
        // The chapter hard-wraps prose, so match against the
        // whitespace-normalized text.
        let chapter_flat = chapter.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            chapter_flat.matches(SENTENCE).count(),
            1,
            "docs/src/web-dashboard.md must state the same update-copy \
             honesty sentence exactly once"
        );
    }

    #[test]
    fn vault_kernel_override_serves_disk_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let app_html_path = dir.path().join("app.html");
        // No sibling yet: fall back to the embedded kernel.
        assert!(vault_kernel_override_response("GET", "", "", &app_html_path, false).is_none());
        std::fs::write(
            dir.path().join("vault-kernel.js"),
            b"self.onmessage=null;\n",
        )
        .unwrap();
        let resp = vault_kernel_override_response("GET", "", "", &app_html_path, false)
            .expect("sibling kernel must be served");
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: application/javascript"));
        assert!(text.contains("Cache-Control: no-cache"));
        assert!(text.ends_with("self.onmessage=null;\n"));
    }

    /// The Terminal tab's honest exited lifecycle: when the shell dies
    /// (Ctrl-D, `exit`, crash) the pane must SAY so — with the status —
    /// and offer a visible way back in (the toolbar's New shell button
    /// plus the in-terminal hint); and output racing the lazy xterm load
    /// must be buffered, never dropped (the open is sent before the
    /// scripts finish so shell startup overlaps the fetch). If a needle
    /// disappears, the dashboard has regrown the dead-end or the blank
    /// first paint these pinned away — restore the affordance, don't
    /// widen the test.
    #[test]
    fn terminal_exited_reentry_and_early_open_are_pinned() {
        assert_eq!(
            APP_HTML.matches("id=\"shell-restart-btn\"").count(),
            1,
            "the Terminal toolbar must carry exactly one New shell re-entry button"
        );
        assert!(
            APP_HTML.contains("[shell exited with status ${status}]"),
            "the exited state must name the shell's exit status"
        );
        assert!(
            APP_HTML.contains("[press Enter to start a new shell]"),
            "the exited state must tell the user how to get a shell back"
        );
        assert!(
            APP_HTML.contains("bufferPreInitShellEvent"),
            "shell output arriving before xterm loads must be buffered, not dropped"
        );
    }

    /// Agent-authored question-preview HTML must only ever render inside
    /// a sandboxed iframe with an opaque origin. Dashboard authentication
    /// is ambient (mTLS client cert → IAM principal), so a same-origin
    /// grant — or any unsandboxed embed of agent markup — would let it
    /// drive the gateway with the operator's full authority. If this test
    /// fails, someone widened the sandbox: fix the widening, not the test.
    #[test]
    fn question_preview_iframe_sandbox_is_pinned() {
        assert!(
            APP_HTML.contains("setAttribute('sandbox', 'allow-scripts')"),
            "question-preview iframe lost its sandbox attribute"
        );
        assert_eq!(
            APP_HTML.matches("setAttribute('sandbox'").count(),
            1,
            "every iframe sandbox attribute must be the pinned allow-scripts-only set"
        );
        assert_eq!(
            APP_HTML.matches("allow-same-origin").count(),
            0,
            "allow-same-origin must never appear anywhere in the dashboard bundle"
        );
        assert_eq!(
            APP_HTML.matches(".srcdoc").count(),
            1,
            "srcdoc must have exactly one writer: the sandboxed question-preview renderer"
        );
    }

    /// The digest-visibility mandate: a manifest digest is what an owner
    /// Approve gesture cryptographically covers — it changes on every
    /// re-propose while the item id stays — so every surface carrying
    /// the gesture renders it through the one shared chip (short form
    /// inline, full digest on hover, click copies). If this fails, a
    /// surface dropped the chip or forked the wire: restore the shared
    /// one.
    #[test]
    fn approval_surfaces_show_the_digest_they_sign() {
        let shared = include_str!("../../../../static/app/ui2-agenda.js");
        // The formatter/chip pair and the single copy wire live in the
        // shared fragment; the chip reveals + copies the full digest and
        // truncates only through the formatter.
        assert!(shared.contains("function agendaShortDigest"));
        assert!(shared.contains("function agendaDigestChipHtml"));
        assert!(shared.contains("data-copy-digest"));
        assert!(shared.contains("sha256 ${d}"));
        assert!(shared.contains("agendaShortDigest(d)"));
        assert!(
            include_str!("../../../../static/app/ui2-agenda.css").contains(".ag2-digest-chip"),
            "the chip lost its one shared appearance"
        );
        // Every Approve-gesture surface renders it: the inline card
        // strip (pending Approve + suspended Re-arm) and the Automations
        // row; the inspector effect detail; the one-gesture workflow
        // sheet's node rows.
        for (name, fragment, chips) in [
            (
                "ui2-agenda-cards.js",
                include_str!("../../../../static/app/ui2-agenda-cards.js"),
                3usize,
            ),
            (
                "ui2-agenda-inspector.js",
                include_str!("../../../../static/app/ui2-agenda-inspector.js"),
                1,
            ),
            (
                "ui2-agenda-workflows.js",
                include_str!("../../../../static/app/ui2-agenda-workflows.js"),
                1,
            ),
        ] {
            assert!(
                fragment.matches("agendaDigestChipHtml(").count() >= chips,
                "{name}: an approval surface stopped rendering the digest chip"
            );
            assert!(
                !fragment.contains("function agendaDigestChipHtml"),
                "{name}: the chip helper must have exactly one definition, in ui2-agenda.js"
            );
        }
        // The approved state shows the digest the recorded approval
        // covers, and a re-proposed effect visibly carries its NEW one.
        let inspector = include_str!("../../../../static/app/ui2-agenda-inspector.js");
        assert!(inspector.contains("bound ? e.approval.digest : e.digest"));
        assert!(inspector.contains("Re-proposed since your last approval"));
    }

    /// Track AW, the automate-sheet half of the emission-shape law,
    /// retargeted to the generic served-catalog flow (its
    /// registry-era twin died with mandate_templates.rs at the
    /// cutover; this copy carries the law): the sheet consumes the
    /// served definition catalog and stamps through the daemon's
    /// stamp op (via the shared wrapper whose one transport call the
    /// workflow-fragment pin counts) — it neither proposes nor
    /// approves client-side, so the owner's per-effect digest ceremony
    /// stays the only arming act.
    #[test]
    fn automate_sheet_consumes_the_catalog_and_never_approves() {
        let sheet = include_str!("../../../../static/app/ui2-agenda.js");
        assert!(
            sheet.contains("api_agenda_definitions"),
            "the automate sheet must read the served definition catalog"
        );
        assert!(
            sheet.contains("agendaDefinitionStamp("),
            "the automate sheet must stamp through the daemon's stamp op (the shared wrapper)"
        );
        assert_eq!(
            sheet.matches("api_agenda_stamp").count(),
            0,
            "the stamp transport lives in the shared wrapper — one call site, one fragment"
        );
        // The ban is on EMISSION shapes, by name: the shared op emitter
        // (`agendaSendOp`, same fragment) INSPECTS `params.op ===
        // 'approve_effect'` for the approve-while-blocked confirm, which
        // is reading a surface's op, never minting one — approvals are
        // still emitted only by the surfaces the other pins govern.
        for emission in [
            "op: 'approve_effect'",
            "op: \"approve_effect\"",
            "data-op-btn=\"approve_effect\"",
        ] {
            assert_eq!(
                sheet.matches(emission).count(),
                0,
                "the automate sheet cannot approve — the ceremony stays the owner's \
                 ({emission:?} found)"
            );
        }
        assert_eq!(
            sheet.matches("propose_effect").count(),
            0,
            "the sheet stamps through the daemon — client-side proposing was the registry era"
        );
        // Refusals stay visible: invalid and shadowed entries render
        // disabled with their reason, never hidden.
        assert!(sheet.contains("invalid definition"));
        assert!(sheet.contains("shadowed by a personal definition of the same name"));
    }

    /// Skills/plugins S2 (sealed intake d56f4ebf, §4c): the automate
    /// sheet renders DECLARED parameters served on the catalog node —
    /// v0's prose-sniffing cap heuristic (card 01KZ8PK1FD) is replaced
    /// by declared data. The typed value substitutes into the declared
    /// line template client-side (the machine writes the canonical
    /// bytes), the pre-stamp summary shows the exact substituted line,
    /// the empty-required gate is client convenience beside the daemon's
    /// refusal, and every line rides the stamp op's `annotations` beside
    /// the surviving generic note lane. The v0 copy laws carry over: the
    /// annotation surface is named THREAD, and the executor summary
    /// names the definition's node pins instead of claiming "daemon
    /// defaults" when pins exist.
    #[test]
    fn automate_sheet_renders_declared_parameters() {
        let sheet = include_str!("../../../../static/app/ui2-agenda.js");
        assert!(
            sheet.contains("function agendaStampDeclaredParams"),
            "the sheet must render the parameters the catalog declares"
        );
        assert!(
            !sheet.contains("agendaStampCapDemanded"),
            "the v0 prose-sniffing cap heuristic is replaced by declared data (S2)"
        );
        assert!(
            sheet.contains(".replace('<value>', () => value)"),
            "the typed value must substitute into the declared template verbatim"
        );
        assert!(
            sheet.contains("record ${param.label || param.name} in the item’s THREAD: ${line}"),
            "the pre-stamp summary must show the exact substituted line"
        );
        assert!(
            sheet.contains("Fill the required ${param.label || param.name} field"),
            "an empty required parameter must refuse client-side (convenience; \
             the daemon op is the wall)"
        );
        assert!(
            sheet.contains("overrides.annotations = annotations"),
            "parameter lines and the note must ride the stamp op's annotations field"
        );
        // Copy law: the user-facing name for the annotation surface is
        // THREAD (the UI's label), never the internal inspector name.
        assert!(
            sheet.contains("THREAD section"),
            "note copy must name the THREAD section"
        );
        // Executor honesty (the sheet-summary half of card 01KZ8PK1FD):
        // the definition's node pins are named when no explicit pick is
        // made — the stamp lane's actual fallback.
        assert!(
            sheet.contains("the definition’s node pins, recorded on the manifest"),
            "the pre-stamp summary must show the definition's node pins"
        );
    }

    /// Track AW, the workflow half of the emission-shape law,
    /// retargeted to the generic flow (its registry-era twin,
    /// workflow_approval_sheet_approves_only_in_the_owner_confirm_lane,
    /// died with mandate_templates.rs at the cutover): the workflow and
    /// triggered lanes stamp through the daemon (no client-side graph
    /// assembly survives), the approval sheet renders the sealed bytes
    /// from the content-addressed serving lane, and the approval lane
    /// stays exactly one emitter — one `approve_effect`, inside
    /// `agendaWorkflowEmitApprovals`, iterating exactly the stamped
    /// node set, called once from the owner-confirm handler.
    #[test]
    fn workflow_surfaces_stamp_through_the_daemon_with_one_emitter() {
        let fragment = include_str!("../../../../static/app/ui2-agenda-workflows.js");
        assert!(
            fragment.contains("api_agenda_stamp"),
            "the workflow lanes must stamp through the daemon's stamp op"
        );
        assert!(
            fragment.contains("api_agenda_sealed"),
            "the approval sheet must render the sealed bytes from the serving lane"
        );
        // The daemon owns graph assembly now: no client-side parking,
        // placing, edge-drawing, or proposing survives in this fragment.
        for gone in [
            "add_relies_on",
            "propose_effect",
            "op: 'place'",
            "op: 'add'",
        ] {
            assert_eq!(
                fragment.matches(gone).count(),
                0,
                "client-side graph assembly must not survive the stamp-op rewire: found {gone:?}"
            );
        }
        // The emission shape, unweakened: exactly one approve_effect in
        // the fragment, inside the single pinned emitter, iterating the
        // stamped node set, with exactly one call site that lives in the
        // owner-confirm handler.
        assert_eq!(
            fragment.matches("approve_effect").count(),
            1,
            "the fragment must contain exactly one approval emission site"
        );
        let (_, emitter) = fragment
            .split_once("async function agendaWorkflowEmitApprovals(")
            .expect("the pinned emitter must exist");
        let body = emitter
            .split_once("\n}")
            .expect("the emitter body must close")
            .0;
        assert!(
            body.contains("for (const node of batch.nodes)"),
            "the emitter iterates exactly the stamped node set"
        );
        assert!(
            body.contains("approve_effect"),
            "the one emission lives inside the pinned emitter"
        );
        assert_eq!(
            fragment
                .matches("agendaWorkflowEmitApprovals(stamped)")
                .count(),
            1,
            "exactly one call site invokes the emitter"
        );
        let confirm = fragment
            .find("async function agendaWorkflowApproveConfirm(")
            .expect("the owner-confirm handler must exist");
        let call = fragment
            .find("agendaWorkflowEmitApprovals(stamped)")
            .expect("the emitter call site must exist");
        assert!(
            call > confirm,
            "the emitter is called from the owner-confirm handler, nowhere earlier"
        );
    }

    /// The approval-time manifest editor's lane law: the card's Edit
    /// affordance is an OPENER for the one schedule sheet, whose save is
    /// the manifest-EDIT lane's single `propose_effect` emitter — the
    /// edit UI is a client of re-propose, never a second writer. The
    /// fragment set's other emitters are the seals module's adopt
    /// confirm — the RE-SEAL ceremony (fresh pins from drift review),
    /// pinned here to carry the manifest verbatim including the shape —
    /// and the missed-card RESCHEDULE lane (`agendaRescheduleMissed`,
    /// fireability card): verbatim re-propose with the floor moved to
    /// now + re-approve, one tap. Each lane exactly one emitter. (The
    /// automate and workflow guards above pin their fragments to zero.)
    #[test]
    fn edit_mints_through_the_repropose_lane() {
        let inspector = include_str!("../../../../static/app/ui2-agenda-inspector.js");
        let cards = include_str!("../../../../static/app/ui2-agenda-cards.js");
        let seals = include_str!("../../../../static/app/ui2-agenda-seals.js");
        assert_eq!(
            inspector.matches("op: 'propose_effect'").count(),
            2,
            "exactly two emission sites in the inspector fragment: the \
             schedule sheet's confirm (edit lane) and the missed-card \
             reschedule (one-tap lane)"
        );
        assert_eq!(
            seals.matches("propose_effect").count(),
            1,
            "exactly one re-seal emission site: the adopt confirm"
        );
        assert!(
            seals.contains("if (m.interactive) params.interactive = true;"),
            "the adopt carries the shape verbatim — a re-seal never flips \
             an interactive manifest to a goal run"
        );
        let (_, confirm) = inspector
            .split_once("async function agendaSchedConfirm(")
            .expect("the sheet confirm must exist");
        assert!(
            confirm.contains("op: 'propose_effect'"),
            "the edit-lane emission lives inside the sheet confirm"
        );
        let (_, resched) = inspector
            .split_once("async function agendaRescheduleMissed(")
            .expect("the reschedule lane must exist");
        assert!(
            resched.contains("op: 'propose_effect'"),
            "the reschedule emission lives inside agendaRescheduleMissed"
        );
        assert!(
            resched.contains("if (m.interactive) params.interactive = true;"),
            "the reschedule carries the manifest verbatim — shape included; \
             only the floor moves"
        );
        assert_eq!(
            cards.matches("propose_effect").count(),
            0,
            "the card never proposes — its affordances open the sheet or \
             call the inspector's reschedule lane"
        );
        // The pending strips carry the affordance; the delegation opens
        // the sheet and sends no op.
        assert!(cards.contains("data-edit-sched"));
        assert!(
            cards.contains("agendaOpenSchedSheet(editSched.dataset.editSched"),
            "the card edit handler opens the one editor"
        );
        // No edit lane for ALREADY-approved effects from the card: the
        // affordance renders only in the pending/suspended branches (the
        // inline strip and the automations strip), never beside Revoke.
        // Five sites: the inline strip's Fix-plan closure + its plain
        // Edit…, the automations strip's Fix-plan + its plain Edit…, and
        // the one delegation handler.
        assert_eq!(
            cards.matches("data-edit-sched").count(),
            5,
            "four branch buttons (two Fix-plan, two Edit…) + the one \
             delegation handler"
        );
        // The missed-card one-tap: two strip emissions + the one
        // delegation handler, which routes to the inspector lane.
        assert_eq!(
            cards.matches("data-resched-effect").count(),
            3,
            "two missed-branch buttons + the one delegation handler"
        );
        assert!(
            cards.contains("agendaRescheduleMissed(resched.dataset.reschedEffect"),
            "the card reschedule handler calls the one reschedule lane"
        );
    }

    /// The fireability class law on the render side: the cards NEVER
    /// offer Approve/Re-arm against a served `fireability_refusal` —
    /// both strips branch on the served verdict before emitting the
    /// approve button, and the daemon's approve intake backs the law
    /// (`approve_is_never_armed_on_an_unfireable_manifest` in the
    /// agenda store tests). The SPA maps refusals by the daemon's
    /// pinned grammar (`FIREABILITY_REFUSAL_PREFIX`).
    #[test]
    fn approve_is_gated_on_the_served_fireability_verdict() {
        let cards = include_str!("../../../../static/app/ui2-agenda-cards.js");
        let inspector = include_str!("../../../../static/app/ui2-agenda-inspector.js");
        assert!(
            cards.matches("fireability_refusal").count() >= 2,
            "both card strips read the served verdict"
        );
        // The inline strip's approve emission sits in the refusal-gated
        // ternary; the automations strip's pending branch is reachable
        // only past the refusal branch above it.
        assert!(
            cards.contains("refusal\n        ? fixBtn('Fix plan…')"),
            "the inline strip offers Fix plan INSTEAD of Approve on a refusal"
        );
        assert!(
            cards.contains("if (refusal && (st.kind === 'pending' || st.kind === 'suspended'))"),
            "the automations strip gates Approve/Re-arm on the served refusal"
        );
        assert!(
            inspector.contains("/^unfireable\\((project|executor|floor)\\): /"),
            "the SPA parses exactly the daemon's refusal grammar \
             (FIREABILITY_REFUSAL_PREFIX in agenda/fireability.rs)"
        );
        assert_eq!(
            crate::agenda::FIREABILITY_REFUSAL_PREFIX,
            "unfireable(",
            "the grammar the SPA regex above matches"
        );
    }

    /// The blocked-chip honesty law: the chip is a tappable BUTTON that
    /// names each gate (no hover-only reveal), the per-gate rows join
    /// `relies_on` client-side within the served window, and the
    /// delivered-awaiting-Complete distinction derives from exactly the
    /// served run truth — `last_run.state === 'completed'` AND a
    /// self-reported `achieved` — never from transport state alone.
    /// Advisory throughout: nothing here gates approval or firing.
    /// The safeguards-flagged terminal renders its OWN face — never the
    /// generic interrupted/failed chip — and every surface states the
    /// law: never auto-retried, never a model switch, the remedy is a
    /// fresh-session recast. Daemon side the served key is
    /// `terminal.class == "safeguards_flagged"` (session_catalog row,
    /// lifted verbatim by the grid envelope); these needles hold the
    /// fragments to it.
    #[test]
    fn safeguards_flagged_terminal_is_distinct_and_never_generic() {
        let list = include_str!("../../../../static/app/57a-sessions-list.js");
        let windows = include_str!("../../../../static/app/39-session-windows.js");
        let css = include_str!("../../../../static/app/ui2-sessions.css");
        assert!(
            list.contains("s.terminal.class === 'safeguards_flagged'"),
            "the sessions-list chip derives the distinct face from the served terminal class"
        );
        assert!(
            list.contains("'ui-chip err sc-status safeguards-flagged'"),
            "the distinct chip class replaces the generic status chip for this class"
        );
        assert!(
            list.contains("statusEl.textContent = 'safeguards-flagged'"),
            "the chip says what happened, never a generic status word"
        );
        assert!(
            list.contains("recast the task in a fresh session"),
            "the chip's tooltip states the remedy"
        );
        assert!(
            windows.contains("out.class = terminalClass"),
            "the terminal-facts normalizer forwards the served class key"
        );
        assert!(
            windows.contains("terminal?.class === 'safeguards_flagged'"),
            "the dead-window statement branches on the served class"
        );
        assert!(
            windows.contains("Never auto-retried and never switched to another model"),
            "the terminal statement states the no-retry / no-fallback law"
        );
        assert!(
            css.contains(".ui-chip.sc-status.safeguards-flagged"),
            "the distinct chip face has its own style hook"
        );
    }

    /// Parked-task respawn honesty (the died-with-restart class): the
    /// dashboard derives the attention state from the wire's died
    /// fields, never invents a parked count the registry doesn't vouch
    /// for (the post-respawn unknown-task "Parked · 1 task" latch), and
    /// the one-tap re-run is an OWNER-tapped follow-up through the
    /// existing lane — the copy states that nothing re-runs
    /// automatically. These needles hold the fragments to it.
    #[test]
    fn died_tasks_attention_is_derived_offered_and_never_auto_rerun() {
        let windows = include_str!("../../../../static/app/39-session-windows.js");
        let actions = include_str!("../../../../static/app/41-session-window-actions.js");
        let lifecycle = include_str!("../../../../static/app/54-session-lifecycle.js");
        let handover = include_str!("../../../../static/app/ui2-handover.js");
        assert!(
            windows.contains("diedBackgroundTasks"),
            "the attention state derives from the wire's died fields"
        );
        assert!(
            windows.contains("state: 'died-tasks'"),
            "died tasks derive a display state (like stalled), producers never send it"
        );
        assert!(
            windows.contains("Nothing was re-run automatically"),
            "the explainer states the no-auto-rerun law"
        );
        assert!(
            windows.contains("const n = (act.backgroundTasks || []).length;\n  if (!n) return '';"),
            "a parked claim with no vouched tasks renders NO parked pill (the unknown-task latch)"
        );
        assert!(
            windows.contains("sendDiedTaskRerun(sessionId, v.diedTasks, v.diedCause)"),
            "the one-tap re-run rides the activity chip action"
        );
        assert!(
            lifecycle.contains("function sendDiedTaskRerun"),
            "the tap sends a normal follow-up through the existing start_task lane"
        );
        assert!(
            lifecycle.contains("not re-run automatically"),
            "the composed follow-up tells the model the daemon re-ran nothing"
        );
        assert!(
            actions.contains("sessionDiedTasksPillLabel"),
            "the status pill states the died ending instead of a bare Idle"
        );
        assert!(
            handover.contains("parked on a background task"),
            "a live bg-park holdout says what the idle session waits on"
        );
    }

    #[test]
    fn blocked_chip_explains_every_gate_and_the_delivered_wait() {
        let cards = include_str!("../../../../static/app/ui2-agenda-cards.js");
        let shared = include_str!("../../../../static/app/ui2-agenda.js");
        let inspector = include_str!("../../../../static/app/ui2-agenda-inspector.js");
        assert!(
            cards.contains("data-blocked-toggle="),
            "the blocked chip and its fold affordance are tappable buttons"
        );
        assert!(
            !cards.contains("agendaChipHtml('blocked'"),
            "no surface renders the old say-nothing blocked chip"
        );
        // The delivered judgment reads the SERVED owed-completion
        // classification (summary.rs `item_owed_completion`, served as
        // the target row's `completable` flag and the blocked_on
        // causes' `target_completable`) — the old client re-derivation
        // from run + attestation is deleted (derive-don't-mirror).
        assert!(
            shared.contains("target.completable === true")
                && shared.contains("served.target_completable === true"),
            "delivered-awaiting-Complete reads the served classification"
        );
        assert!(
            !shared.contains("&& run.attestation.outcome === 'achieved'"),
            "no client-side re-derivation of the owed-completion predicate"
        );
        assert!(
            shared.contains("'finished · awaiting Complete'"),
            "the all-delivered chip face names the actionable wait"
        );
        // Both the card rows and the inspector's Blocked-on section read
        // the ONE shared judgment — no second derivation to drift.
        assert!(
            cards.contains("agendaBlockedExplain(item)")
                && inspector.contains("agendaPrereqStates(item)"),
            "cards and inspector render the shared prerequisite judgment"
        );
        // The honest out-of-window degrade: an absent target on an
        // unblocked item is provably done; only a blocked one shows the
        // id-only unknown row.
        assert!(
            shared.contains("'outside this live window'") && shared.contains("'done · archived'"),
            "absent targets degrade honestly instead of rendering 'missing'"
        );
    }

    /// The serving-grain law for the editor (Track AS × the manifest
    /// editor): list rows are summaries — the manifest MINUS `goal` and
    /// the sealed refs — but the edit sheet round-trips the WHOLE
    /// manifest, so its opener prefills only from the FULL item
    /// (`agendaFullItemFor`), parking on a loading state until the
    /// single-flight fetch lands and the arrival hook re-enters. A
    /// summary prefill would blank the goal and silently unseal the
    /// refs on save — the exact bug the round-trip law prevents.
    #[test]
    fn edit_prefills_from_the_full_item_grain() {
        let inspector = include_str!("../../../../static/app/ui2-agenda-inspector.js");
        let shared = include_str!("../../../../static/app/ui2-agenda.js");
        let (_, opener) = inspector
            .split_once("function agendaOpenSchedSheet(")
            .expect("the sheet opener must exist");
        let opener = opener
            .split_once("\nfunction ")
            .map(|(body, _)| body)
            .unwrap_or(opener);
        assert!(
            opener.contains("agendaFullItemFor(itemId)"),
            "the opener prefills from the full item grain, never the summary row"
        );
        assert!(
            opener.contains("'sched-loading'"),
            "an uncached full item parks the sheet on the loading state"
        );
        assert!(
            inspector.contains("Loading the full manifest…"),
            "the loading state renders honestly instead of a degraded form"
        );
        assert!(
            shared.contains("agendaSheetState.kind === 'sched-loading'"),
            "the full-item arrival hook re-enters the waiting opener"
        );
    }

    /// The shape toggle is honest about consequences ON the sheet, in
    /// the scheduler's own semantics: interactive opens-and-waits (it
    /// does not auto-run the goal), goal run is the autonomous one-shot.
    /// The interactive pin rides the propose only for the interactive
    /// shape, so a goal-run edit stays byte-compatible with daemons
    /// that predate the field.
    #[test]
    fn shape_toggle_states_its_consequences() {
        let inspector = include_str!("../../../../static/app/ui2-agenda-inspector.js");
        assert!(
            inspector.contains(
                "Opens with the goal as your message and waits for you — it does not auto-run."
            ),
            "the interactive consequence line must render with the toggle"
        );
        assert!(
            inspector.contains("Autonomous one-shot — runs the goal unattended and writes back."),
            "the goal-run consequence line must render with the toggle"
        );
        assert_eq!(
            inspector.matches(r#"data-sheet-act="sched-shape""#).count(),
            2,
            "one segmented control, two shapes"
        );
        assert!(
            inspector.contains("if (s.shape === 'interactive') params.interactive = true;"),
            "interactive rides the propose only when chosen"
        );
    }

    /// Sealed binding refs render READ-ONLY on the edit sheet — locator
    /// plus pinned hash, no input controls — and the confirm carries the
    /// state verbatim (`binding_refs: s.bindingRefs`); editing sealed
    /// content stays the re-seal ceremony. The daemon-side twin
    /// (`unchanged_binding_refs_carry_forward_past_live_drift`) pins the
    /// intake half: a verbatim carry verifies against the sealed store.
    #[test]
    fn sealed_refs_render_readonly() {
        let inspector = include_str!("../../../../static/app/ui2-agenda-inspector.js");
        let start = inspector
            .find(r#"class="ag2-refs-ro" data-mf-field="binding_refs""#)
            .expect("the read-only refs block must exist on the sheet");
        let end = inspector
            .find("Carried verbatim — this edit cannot change sealed content.")
            .expect("the read-only law is stated where the refs render");
        assert!(end > start, "the law line closes the refs block");
        let block = &inspector[start..end];
        for control in ["<input", "<textarea", "<select"] {
            assert!(
                !block.contains(control),
                "the refs block is read-only — found {control:?}"
            );
        }
        assert!(
            block.contains("agendaDigestChipHtml(r.sha256"),
            "each ref renders its pinned hash"
        );
        assert!(
            inspector.contains("params.binding_refs = s.bindingRefs"),
            "the confirm carries the sealed pins verbatim, never reconstructed"
        );
    }

    /// The approve-while-blocked confirm (confirm-not-gate) rides the
    /// ONE approve emitter — `agendaSendOp`, which every surface
    /// (card strips, inspector, sheet approve-now, missed-reschedule)
    /// funnels through — BEFORE dispatch, derives from the served
    /// `blocked_on` truth, names the actual prerequisite, and scopes
    /// to time-floored manifests (an event-triggered approval is safe
    /// by construction, so the workflow batch sheet stays quiet).
    #[test]
    fn approve_while_blocked_confirm_rides_the_one_emitter() {
        let agenda = include_str!("../../../../static/app/ui2-agenda.js");
        let send = agenda
            .find("async function agendaSendOp")
            .expect("the one approve emitter exists");
        let confirm = agenda
            .find("await agendaApproveBlockedConfirm(params)")
            .expect("the confirm is wired at the emitter");
        let dispatch = agenda
            .find("daemonApi.request('api_agenda_op', params)")
            .expect("the emitter dispatches the op");
        assert!(
            send < confirm && confirm < dispatch,
            "the confirm runs inside agendaSendOp BEFORE the op is sent"
        );
        assert!(
            agenda.contains("params.op === 'approve_effect'"),
            "only approve legs confirm"
        );
        assert!(
            agenda.contains("item.blocked_on"),
            "derived from the SERVED blocked_on truth, never a client-side join"
        );
        assert!(
            agenda.contains("(effect.manifest && effect.manifest.trigger)) return true"),
            "event-triggered manifests skip the confirm — safe by construction"
        );
        for named in [
            "is still open",
            "was retired without completing",
            "is missing from this agenda",
            "is still uncleared",
        ] {
            assert!(
                agenda.contains(named),
                "the confirm names the actual cause lane: {named:?}"
            );
        }
        assert!(
            agenda.contains("— approve anyway?"),
            "confirm-not-gate: the question, never a refusal"
        );
    }

    /// The on-unblock offer (dependents' suggested mode) renders on the
    /// schedule sheet for items with `relies_on` edges, preselects ONLY
    /// on a fresh propose (existing time-floor plans open unchanged),
    /// and mints the EXISTING trigger vocabulary — never a parallel
    /// mechanism.
    #[test]
    fn sched_sheet_offers_on_unblock_for_dependents() {
        let inspector = include_str!("../../../../static/app/ui2-agenda-inspector.js");
        assert!(
            inspector.contains(r#"data-sheet-offer="on_unblock""#),
            "the offer row exists on the sheet"
        );
        assert!(
            inspector.contains(r#"data-sheet="onUnblock""#),
            "the offer is a visible, reversible tick"
        );
        assert!(
            inspector.contains(
                "Fire when prerequisites complete (on_unblock) — suggested for dependents"
            ),
            "the offer says what it does NOW and that it is the suggested mode"
        );
        assert!(
            inspector.contains("params.trigger = { kind: 'on_unblock' }"),
            "the ticked offer mints the existing OnUnblock trigger vocabulary"
        );
        assert!(
            inspector.contains(
                "onUnblock: !m && Array.isArray(item.relies_on) && item.relies_on.length > 0"
            ),
            "preselected only on a FRESH propose for a dependent item"
        );
    }

    /// Derive, don't mirror — the fragment half: every field
    /// `SessionManifest`'s own schema declares appears on the edit sheet
    /// as a `data-mf-field` marker (editor or read-only row), and no
    /// marker names a field the schema doesn't have. With the command
    /// parity pin in `agenda::types`, a tenth manifest field fails the
    /// suite until the propose lane AND this form acknowledge it —
    /// instead of shipping as an edit lane that silently drops it.
    #[test]
    fn editable_fields_derive_from_schema() {
        let inspector = include_str!("../../../../static/app/ui2-agenda-inspector.js");
        let schema_fields = crate::agenda::session_manifest_schema_fields();
        let marker = r#"data-mf-field=""#;
        let mut markers = std::collections::BTreeSet::new();
        for (idx, _) in inspector.match_indices(marker) {
            let name = inspector[idx + marker.len()..]
                .split('"')
                .next()
                .unwrap_or_default()
                .to_string();
            markers.insert(name);
        }
        assert_eq!(
            markers, schema_fields,
            "the edit sheet's field markers must be exactly the manifest schema's fields"
        );
    }

    /// Track AW surfaces (§2.5 rendering): the picker renders what a
    /// definition MEANS — name, provenance chip, shape line,
    /// description — and the preview renders header + node summary +
    /// authored prose. Raw definition bytes never render as the default
    /// view: the old `preview.textContent = entry.text` dump is gone,
    /// and the only raw-bytes rendering lives inside the explicit
    /// exact-bytes expander.
    #[test]
    fn picker_renders_meaning_never_raw_bytes() {
        let sheet = include_str!("../../../../static/app/ui2-agenda.js");
        for rendered in [
            "agendaProvenanceChipEl(",   // provenance chip, picker + preview
            "agsx-def-btn-desc",         // description under each entry
            "agendaDefinitionKindLine(", // the shape line
            "agendaDefinitionProse(",    // prose split, never raw dump
        ] {
            assert!(
                sheet.contains(rendered),
                "the picker/preview lost its meaning-rendering seam: {rendered}"
            );
        }
        assert_eq!(
            sheet.matches("preview.textContent = entry.text").count(),
            0,
            "raw definition bytes must never be the preview's default rendering"
        );
        // Provenance renders from the served field — house vs personal
        // stays visible in every catalog (§2.5).
        assert!(sheet.contains("agsx-prov-personal") || sheet.contains("agsx-prov-${p}"));
    }

    /// Track AW surfaces: the exact sealed (or to-be-sealed) bytes stay
    /// ONE explicit expander away everywhere the pretty rendering
    /// stands in for them — the sheet preview's "exact bytes a stamp
    /// seals" expander, and the card-side sealed view served from the
    /// content-addressed lane. Verification honesty: rendering never
    /// replaces the bytes.
    #[test]
    fn sealed_bytes_stay_one_expander_away() {
        let sheet = include_str!("../../../../static/app/ui2-agenda.js");
        assert!(
            sheet.contains("Exact bytes a stamp seals"),
            "the sheet preview lost its exact-bytes expander"
        );
        let seals = include_str!("../../../../static/app/ui2-agenda-seals.js");
        assert!(
            seals.contains("api_agenda_sealed"),
            "the card-side sealed view must serve from the content-addressed lane"
        );
        assert!(
            seals.contains("ag2-seal-exact"),
            "the sealed view renders as an expander on the refs strip"
        );
        assert!(
            seals.contains("the exact bytes firings execute"),
            "the sealed view names what the bytes ARE"
        );
    }

    /// Track AW surfaces: stamping is an explicit gesture behind a
    /// pre-stamp summary — for EVERY definition kind. Picker clicks
    /// select and preview; the one Stamp button submits. Concretely:
    /// the sheet's only stamp-transport references live inside the
    /// submit handler (defined after the open/render function), the
    /// registry-era stamp-on-click picker hooks are gone from the
    /// workflows fragment, and the shared wrapper carries the single
    /// transport call.
    #[test]
    fn stamping_requires_an_explicit_gesture_with_preview() {
        let sheet = include_str!("../../../../static/app/ui2-agenda.js");
        assert!(
            sheet.contains("agsx-summary"),
            "the pre-stamp summary must render before the gesture"
        );
        let (before_submit, submit) = sheet
            .split_once("async function agendaAutomationSheetSubmit(")
            .expect("the explicit submit handler must exist");
        assert_eq!(
            before_submit.matches("agendaDefinitionStamp(").count(),
            0,
            "nothing before the submit handler may stamp — selection previews, Stamp stamps"
        );
        assert!(
            submit.contains("agendaDefinitionStamp("),
            "the submit handler is where the one stamp gesture fires"
        );
        let workflows = include_str!("../../../../static/app/ui2-agenda-workflows.js");
        assert_eq!(
            workflows.matches("api_agenda_stamp").count(),
            1,
            "one stamp transport call, inside the shared wrapper"
        );
        for gone in [
            "agendaWorkflowRenderPickerButtons",
            "agendaTriggeredMandateRenderButtons",
            "agendaTriggeredMandateStamp",
        ] {
            assert_eq!(
                workflows.matches(gone).count(),
                0,
                "the stamp-on-click picker hooks must stay dead: found {gone}"
            );
        }
    }

    /// Track AW surfaces (Q9 vocabulary): the registry-era "template"
    /// word is gone from the agenda fragments — the surfaces speak
    /// action / workflow / definition / stamp. (The CSS keeps
    /// `grid-template-*` property names; vocabulary lives in the JS.)
    #[test]
    fn template_vocabulary_is_gone() {
        for (name, fragment) in [
            (
                "ui2-agenda.js",
                include_str!("../../../../static/app/ui2-agenda.js"),
            ),
            (
                "ui2-agenda-cards.js",
                include_str!("../../../../static/app/ui2-agenda-cards.js"),
            ),
            (
                "ui2-agenda-inspector.js",
                include_str!("../../../../static/app/ui2-agenda-inspector.js"),
            ),
            (
                "ui2-agenda-workflows.js",
                include_str!("../../../../static/app/ui2-agenda-workflows.js"),
            ),
            (
                "ui2-agenda-seals.js",
                include_str!("../../../../static/app/ui2-agenda-seals.js"),
            ),
        ] {
            assert_eq!(
                fragment.to_ascii_lowercase().matches("template").count(),
                0,
                "{name}: the ratified vocabulary is action/workflow/definition/stamp — \
                 'template' died with the registry"
            );
        }
    }

    /// Track AW §2.4/§2.7 (the deferred card rendering, shipped):
    /// manifests with binding refs grow a refs strip on the inspector's
    /// effect card — locator, pin chip, expand-time drift chip in
    /// sealed-serving language — plus the Review-&-adopt gesture, whose
    /// ONE emission site re-proposes through the existing propose lane
    /// with the refreshed pin, landing on the ordinary approval (the
    /// one-gesture sheet for multi-node). Nothing auto-adopts, and the
    /// Automations row carries only the fetch-free sealed count.
    #[test]
    fn stamped_cards_show_refs_drift_and_adopt() {
        let seals = include_str!("../../../../static/app/ui2-agenda-seals.js");
        assert!(
            seals.contains("api_agenda_ref_drift"),
            "drift judges through the served expand-time lane"
        );
        assert!(
            seals.contains("sealed revision still serves"),
            "the drift chip speaks sealed-serving language — drift is informational"
        );
        assert!(seals.contains("Review &amp; adopt"));
        // One adopt emission through the EXISTING propose lane, inside
        // the confirm handler — refreshing exactly the drifted pin.
        assert_eq!(
            seals.matches("propose_effect").count(),
            1,
            "adopt re-proposes through one emission site"
        );
        let (before_confirm, confirm) = seals
            .split_once("async function agendaSealAdoptConfirm(")
            .expect("the adopt confirm handler must exist");
        assert_eq!(
            before_confirm.matches("propose_effect").count(),
            0,
            "nothing adopts before the explicit confirm"
        );
        assert!(confirm.contains("binding_refs: (m.binding_refs || [])"));
        assert!(
            confirm.contains("agendaWorkflowOpenApprovalSheet("),
            "a multi-node adopt lands on the one-gesture approval sheet"
        );
        assert_eq!(
            seals.matches("approve_effect").count(),
            0,
            "adopt never approves — the workflow fragment's single emitter stays the only lane"
        );
        // The strip renders on the inspector's effect card; the
        // Automations row stays fetch-free (count chip only).
        let inspector = include_str!("../../../../static/app/ui2-agenda-inspector.js");
        assert!(inspector.contains("agendaSealsStripHtml(item)"));
        let cards = include_str!("../../../../static/app/ui2-agenda-cards.js");
        assert!(cards.contains("ag2-auto-sealed"));
        assert_eq!(
            cards.matches("api_agenda_ref_drift").count(),
            0,
            "list render never hashes — drift is the item panel's expand-time judgment"
        );
    }

    /// The ledger search executes SPA-side — `agendaSearchMatch` filters
    /// the served item snapshot in the browser (the daemon serves items
    /// unfiltered) — so the digest lane is pinned at the fragment:
    /// prefixes of >=8 hex chars, case-insensitive, against every digest
    /// the item owns, resolving to the owning item like an id search.
    #[test]
    fn ledger_search_matches_digest_prefixes() {
        let cards = include_str!("../../../../static/app/ui2-agenda-cards.js");
        let after = |marker: &str| {
            let (_, rest) = cards
                .split_once(marker)
                .unwrap_or_else(|| panic!("{marker} must exist in ui2-agenda-cards.js"));
            rest.split_once("\nfunction ")
                .map(|(body, _)| body)
                .unwrap_or(rest)
        };
        let matcher = after("function agendaSearchMatch");
        assert!(
            matcher.contains("[0-9a-f]{8,64}"),
            "the digest lane lost its >=8-hex-char, case-insensitive floor"
        );
        assert!(matcher.contains("agendaItemDigests(item)"));
        assert!(
            matcher.contains(".startsWith(q)"),
            "digest matching must be prefix matching, not substring"
        );
        let collector = after("function agendaItemDigests");
        for family in ["e.digest", "e.approval.digest", "r.digest"] {
            assert!(
                collector.contains(family),
                "the digest collector lost the {family} family"
            );
        }
    }

    /// One truncation, one place: outside the shared formatter no agenda
    /// fragment slices a digest — the per-surface variants (8/10/12/16
    /// chars once coexisted) are exactly how cited digests became
    /// unfindable. Full reveals (the hood's grouped display, tooltips
    /// carrying the whole sha256) are not truncations and stay free.
    #[test]
    fn one_short_digest_formatter_everywhere() {
        for (name, content) in [
            (
                "ui2-agenda.js",
                include_str!("../../../../static/app/ui2-agenda.js"),
            ),
            (
                "ui2-agenda-cards.js",
                include_str!("../../../../static/app/ui2-agenda-cards.js"),
            ),
            (
                "ui2-agenda-inspector.js",
                include_str!("../../../../static/app/ui2-agenda-inspector.js"),
            ),
            (
                "ui2-agenda-graph.js",
                include_str!("../../../../static/app/ui2-agenda-graph.js"),
            ),
            (
                "ui2-agenda-plan.js",
                include_str!("../../../../static/app/ui2-agenda-plan.js"),
            ),
            (
                "ui2-agenda-diary.js",
                include_str!("../../../../static/app/ui2-agenda-diary.js"),
            ),
            (
                "ui2-agenda-hood.js",
                include_str!("../../../../static/app/ui2-agenda-hood.js"),
            ),
            (
                "ui2-agenda-workflows.js",
                include_str!("../../../../static/app/ui2-agenda-workflows.js"),
            ),
            // The session windows' sealed-inputs chip consumes digests
            // too (the grid envelope) — same one-formatter law.
            (
                "39-session-windows.js",
                include_str!("../../../../static/app/39-session-windows.js"),
            ),
        ] {
            for (idx, line) in content.lines().enumerate() {
                let lower = line.to_ascii_lowercase();
                if lower.contains("digest") && lower.contains(".slice(") {
                    assert!(
                        name == "ui2-agenda.js" && line.contains("AGENDA_DIGEST_SHORT_LEN"),
                        "{name}:{}: a digest is truncated outside agendaShortDigest: {line}",
                        idx + 1
                    );
                }
            }
        }
    }

    #[test]
    fn test_app_html_embedded() {
        assert!(!APP_HTML.is_empty());
        assert!(APP_HTML.contains("<!DOCTYPE html>"));
        assert!(APP_HTML.contains("tab-activity"));
        assert!(APP_HTML.contains("tab-stats"));
        assert!(APP_HTML.contains("tab-terminal"));
        assert!(APP_HTML.contains("tab-displays"));
        assert!(APP_HTML.contains("/three.module.min.js"));
        assert!(THREE_MODULE_JS.contains("Three.js Authors"));
    }

    /// External-URL opener contract with the bundled macOS app: the
    /// wrapper routes popups/external navigations to the system default
    /// browser (sign-in needs the user's real profile), returns null from
    /// window.open by design, and marks the capability with an injected
    /// `__intendantAppExternalOpen`. The SPA's shared opener reads that
    /// marker, and external window.open sites go through it so a genuinely
    /// blocked popup renders an honest fallback instead of a silent no-op.
    #[test]
    fn external_url_opener_contract_is_pinned() {
        // The shared helpers exist (foundation fragment) and read the marker.
        assert!(APP_HTML.contains("function openExternalUrl(url)"));
        assert!(APP_HTML.contains("function openExternalUrlOrExplain(url)"));
        assert!(APP_HTML.contains("window.__intendantAppExternalOpen === true"));

        // Agent sign-in: helper-routed open + state-driven honest fallback
        // (state, not DOM — the 2s ceremony poll re-renders the card).
        assert!(APP_HTML.contains("if (openExternalUrl(url)) return;"));
        assert!(APP_HTML.contains("state.openFallback = true;"));
        assert!(APP_HTML.contains("agent-signin-open-fallback"));

        // Passkey enrollment: the gesture-preserving about:blank pre-open
        // is skipped under the wrapper marker (it can never carry the
        // later URL there) and the fetched URL goes through the helper.
        assert!(APP_HTML.contains("!openExternalUrl(invitation.enrollment_url)"));

        // The wrapper really injects the marker the SPA reads and carries
        // both webview exits (popup + main-frame policy) — a rename or a
        // dropped hook on either side fails here, not in a user's hands.
        let wrapper = include_str!("../../../../macos-app/main.swift");
        assert!(wrapper.contains("window.__intendantAppExternalOpen = true;"));
        assert!(wrapper.contains("createWebViewWith configuration: WKWebViewConfiguration"));
        assert!(wrapper.contains("decidePolicyFor navigationAction: WKNavigationAction"));
    }

    #[test]
    fn live_workspace_input_is_released_before_its_surface_is_hidden() {
        fn section(start: &str, end: &str) -> &'static str {
            APP_HTML
                .split_once(start)
                .and_then(|(_, rest)| rest.split_once(end).map(|(body, _)| body))
                .unwrap_or_else(|| panic!("missing app.html section {start:?} .. {end:?}"))
        }

        fn assert_before(body: &str, first: &str, second: &str) {
            let first_at = body
                .find(first)
                .unwrap_or_else(|| panic!("missing {first:?} in app.html section"));
            let second_at = body
                .find(second)
                .unwrap_or_else(|| panic!("missing {second:?} in app.html section"));
            assert!(
                first_at < second_at,
                "{first:?} must precede {second:?} in app.html section"
            );
        }

        // Closing a display must flush held-key keyups while the input gate is
        // still open, then release server-side authority.
        let disconnect = section(
            "  disconnect({ userInitiated = false } = {}) {",
            "\n}\n\nfunction removeDisplaySlot",
        );
        assert_before(
            disconnect,
            "this._exitInteractive(userInitiated);",
            "this._releaseAuthority();",
        );

        // Both ways a live projection can be hidden must release active input,
        // cancel an in-flight Take, and relinquish authority already granted.
        for body in [
            section(
                "  function teardownSelectedSurface(slot) {",
                "\n  function selectLiveDisplay(",
            ),
            section(
                "  window.deactivateLiveDisplayWorkspace = function() {",
                "\n  function reconcileSelectedDisplay(",
            ),
        ] {
            for required in [
                "slot.interactive",
                "slot._takeControlPending",
                "slot.authorityState === 'you'",
                "slot.releaseControl();",
            ] {
                assert!(
                    body.contains(required),
                    "live-surface teardown must contain {required:?}"
                );
            }
        }

        // Tab navigation must deactivate Live while it is still the active
        // workspace, before the pane is hidden.
        let switch_tab = section(
            "function switchTab(tabId) {",
            "\nfunction contextResolveVizTheme(",
        );
        assert_before(
            switch_tab,
            "window.deactivateLiveDisplayWorkspace()",
            "activeTab = tabId;",
        );

        // A shared-view Take originating in Activity or Station must enter the
        // Live workspace before selecting the target and requesting authority.
        let shared_take = section(
            "function takeSharedViewInput() {",
            "\nfunction handleSharedViewEvent(",
        );
        assert_before(
            shared_take,
            "routeTo('displays')",
            "window.selectLiveDisplay(",
        );
        assert_before(
            shared_take,
            "window.selectLiveDisplay(",
            "slot.takeControl();",
        );
    }

    #[test]
    fn dashboard_validator_cachebuster_catalog_matches_the_daemon() {
        const VALIDATOR: &str = include_str!("../../../../scripts/validate-dashboard.cjs");
        let marker = "const APP_HTML_CACHEBUSTED_ASSET_PATHS = [";
        let body = VALIDATOR
            .split_once(marker)
            .and_then(|(_, rest)| rest.split_once("];"))
            .map(|(body, _)| body)
            .expect("validator cachebuster catalog");
        let validator_paths = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| line.trim_end_matches(',').trim_matches('\'').to_string())
            .collect::<Vec<_>>();
        let daemon_paths = APP_HTML_VERSIONED_ASSETS
            .iter()
            .map(|path| (*path).to_string())
            .collect::<Vec<_>>();
        assert_eq!(validator_paths, daemon_paths);
    }

    #[test]
    fn new_session_codex_model_override_is_wired() {
        assert!(APP_HTML.contains(r#"id="new-session-codex-model""#));
        assert!(APP_HTML.contains(r#"id="new-session-codex-model-select""#));
        assert!(APP_HTML.contains(r#"id="new-session-codex-reasoning-effort""#));
        assert!(APP_HTML.contains("codexModelSel.disabled = !appliesToCodex;"));
        assert!(APP_HTML.contains("const model = selection || newSessionCodexGlobalModel;"));
        assert!(APP_HTML.contains("if (model) msg.codex_model = model;"));
        assert!(APP_HTML.contains("msg.codex_reasoning_effort = reasoningEffort;"));
    }

    #[test]
    fn global_external_agent_model_defaults_are_wired_in_settings() {
        for id in [
            "set-codex-model-select",
            "set-codex-model-custom",
            "set-codex-reasoning-effort",
            "set-claude-model-select",
            "set-claude-model-custom",
            "set-claude-effort",
            "set-claude-permission-mode",
            "set-kimi-model-select",
            "set-kimi-model-custom",
            "set-kimi-thinking",
            "set-kimi-permission-mode",
            "set-kimi-plan-mode",
            "set-kimi-swarm-mode",
        ] {
            assert!(APP_HTML.contains(&format!(r#"id="{id}""#)), "missing {id}");
        }
        assert!(APP_HTML.contains("codex_model: selectedCodexModel"));
        assert!(APP_HTML.contains("claude_model: selectedClaudeModel"));
        assert!(APP_HTML.contains("claude_effort: selectedClaudeEffort"));
        assert!(APP_HTML.contains("kimi_model: selectedKimiModel"));
        assert!(APP_HTML.contains("function populateSettingsCodexModel"));
        assert!(APP_HTML.contains("function populateSettingsClaudeModel"));
        assert!(APP_HTML.contains("function populateSettingsClaudeEffort"));
        assert!(APP_HTML.contains("function populateSettingsKimiModel"));
    }

    /// The agenda Start-now sheet's config controls are wired end to end:
    /// the daemon-default provenance hint, the explicit-pick recording, and
    /// the `agent_config` block on the start_now payload (with the backend
    /// pinned alongside any explicit pick, so the reviewed config binds).
    #[test]
    fn agenda_start_sheet_config_controls_are_wired() {
        assert!(APP_HTML.contains("function agendaStartBackendConfig"));
        assert!(APP_HTML.contains(r#"select.id = id;"#));
        assert!(APP_HTML.contains("Daemon default (${defaultValue})"));
        assert!(APP_HTML.contains("'explicit — recorded on the manifest'"));
        assert!(APP_HTML.contains("if (model) agentConfig[spec.modelKey] = model;"));
        assert!(APP_HTML.contains("if (effort) agentConfig[spec.effortKey] = effort;"));
        assert!(APP_HTML.contains("agentConfig.agent = spec.backend;"));
        assert!(APP_HTML.contains("params.agent_config = agentConfig;"));
    }

    #[test]
    fn new_session_kimi_overrides_are_wired() {
        for id in [
            "new-session-kimi-model-select",
            "new-session-kimi-model",
            "new-session-kimi-thinking",
            "new-session-kimi-permission-mode",
            "new-session-kimi-plan-mode",
            "new-session-kimi-swarm-mode",
        ] {
            assert!(APP_HTML.contains(&format!(r#"id="{id}""#)), "missing {id}");
        }
        assert!(APP_HTML.contains("if (model) msg.kimi_model = model;"));
        assert!(APP_HTML.contains("msg.kimi_thinking = thinking;"));
        assert!(APP_HTML.contains("msg.kimi_permission_mode = permissionMode;"));
        assert!(APP_HTML.contains("msg.kimi_plan_mode ="));
        assert!(APP_HTML.contains("msg.kimi_swarm_mode ="));
    }

    #[test]
    fn kimi_historical_fork_points_are_joined_to_transcript_turns() {
        fn section(start: &str, end: &str) -> &'static str {
            APP_HTML
                .split_once(start)
                .and_then(|(_, rest)| rest.split_once(end).map(|(body, _)| body))
                .unwrap_or_else(|| panic!("missing app.html section {start:?} .. {end:?}"))
        }

        // The catalog index, panel-to-row jump, and row-to-point lookup must
        // all use the same whole-turn convention: boundary k is rendered on
        // the following user prompt (user_turn_index k + 1).
        let catalog_index = section(
            "function sessionForkStoreInlineIndex(",
            "\nfunction sessionForkRefreshDetailAffordances(",
        );
        assert!(catalog_index.contains("source === 'codex' || source === 'kimi'"));
        assert!(catalog_index.contains("point.kind === 'turn-boundary'"));

        let row_match = section(
            "function sessionForkRowMatchesPoint(",
            "\nfunction sessionForkJumpToPoint(",
        );
        assert!(row_match.contains("source === 'codex' || source === 'kimi'"));
        assert!(row_match.contains("record.user_turn_index === point.turn + 1"));

        let row_lookup = section(
            "function sessionForkInlinePointForRecord(",
            "\nfunction sessionForkRowHint(",
        );
        assert!(row_lookup.contains("idx.source === 'codex' || idx.source === 'kimi'"));
        assert!(row_lookup.contains("idx.byRow.get(`turn:${turn - 1}`)"));
    }

    #[test]
    fn embedded_codex_picker_fallback_matches_daemon_catalog() {
        fn marker_json(start: &str, end: &str) -> serde_json::Value {
            let json = APP_HTML
                .split_once(start)
                .and_then(|(_, rest)| rest.split_once(end).map(|(json, _)| json.trim()))
                .unwrap_or_else(|| panic!("missing embedded catalog markers {start} / {end}"));
            serde_json::from_str(json).expect("embedded catalog marker body is JSON")
        }

        let actual_models = marker_json(
            "/* codex-model-catalog:start */",
            "/* codex-model-catalog:end */",
        );
        let expected_models = serde_json::Value::Array(
            crate::project::CODEX_MODEL_CATALOG
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "id": entry.id,
                        "display_name": entry.display_name,
                        "default_reasoning_effort": entry.default_reasoning_effort,
                        "reasoning_efforts": entry.reasoning_efforts,
                    })
                })
                .collect(),
        );
        assert_eq!(actual_models, expected_models);

        let actual_efforts = marker_json(
            "/* codex-reasoning-efforts:start */",
            "/* codex-reasoning-efforts:end */",
        );
        let expected_efforts = serde_json::json!(crate::project::CODEX_REASONING_EFFORTS
            .iter()
            .filter(|effort| !effort.is_empty())
            .collect::<Vec<_>>());
        assert_eq!(actual_efforts, expected_efforts);
    }

    /// Card 01KZR0QP9A: the SPA's hardcoded Kimi model vocabulary killed
    /// sessions when the installed backend's real catalog moved (fresh
    /// Kimi 0.34.0 refused `kimi-code/k3` after spawn). The mirrors are
    /// dead: no quoted `kimi-code/…` model-id literal may exist in the
    /// assembled dashboard or the Station pane producer — every picker
    /// derives from the daemon-served catalog through the one shared
    /// helper. (`~/.kimi-code/…` path copy is not a model id and stays.)
    /// The ONE permitted compiled vocabulary is the daemon-side
    /// `backend_model_catalog::KIMI_COMPILED_MODEL_SUGGESTIONS`
    /// declaration (card 01KZR67RHT), which reaches pickers only through
    /// the served `compiled_suggestions` field — never as SPA literals.
    #[test]
    fn kimi_model_vocabulary_mirrors_are_dead() {
        for needle in ["'kimi-code/", "\"kimi-code/", "`kimi-code/"] {
            assert!(
                !APP_HTML.contains(needle),
                "app.html re-grew a hardcoded kimi model-id literal ({needle}…): \
                 derive from backendModelCatalog('kimi') instead"
            );
        }
        let station_panels = include_str!("../../../../crates/station-web/src/hud/panels.rs");
        assert!(
            !station_panels.contains("\"kimi-code/"),
            "station-web panels re-grew a hardcoded kimi model-id literal: \
             render from StationControlsSummary::kimi_model_choices instead"
        );
        // The shared derivation really is wired: the one helper exists and
        // every picker lane consumes it.
        assert!(APP_HTML.contains("function backendModelCatalog("));
        assert!(APP_HTML.contains("function populateKimiModelSelect("));
        assert!(APP_HTML.contains("function populateNewSessionKimiModelSelect("));
        assert!(APP_HTML.contains("function refreshSettingsKimiModelOptions("));
        assert!(APP_HTML.contains("kimiModelChoices:"));
        // Card 01KZR67RHT: the compiled-baseline suggestions flow through
        // the SAME shared source — the served field is consumed, suggested
        // entries render as their own labeled group, and pinned-model →
        // Custom-row mapping recognizes suggestions. A regression here
        // returns fresh installs to Default+Custom-only pickers.
        assert!(APP_HTML.contains("compiled_suggestions"));
        assert!(APP_HTML.contains("function kimiCatalogOffers("));
        assert!(APP_HTML.contains("Suggested models"));
    }

    /// The Claude model-alias vocabulary is a deliberate static mirror
    /// (the CLI resolves the aliases; there is no daemon-learned catalog
    /// for Claude Code yet), so per the repo convention each mirror site
    /// is pinned to the daemon's `project::CLAUDE_MODEL_ALIASES` — an
    /// alias change that forgets a picker fails here instead of shipping
    /// as drift.
    #[test]
    fn claude_model_alias_mirrors_match_daemon_vocabulary() {
        let single_quoted = crate::project::CLAUDE_MODEL_ALIASES
            .iter()
            .map(|alias| format!("'{alias}'"))
            .collect::<Vec<_>>()
            .join(", ");
        for (name, fragment) in [
            (
                "40-session-launch.js",
                include_str!("../../../../static/app/40-session-launch.js"),
            ),
            (
                "53-stats-settings.js",
                include_str!("../../../../static/app/53-stats-settings.js"),
            ),
            (
                "ui2-agenda.js",
                include_str!("../../../../static/app/ui2-agenda.js"),
            ),
            (
                "34-station-panes.js",
                include_str!("../../../../static/app/34-station-panes.js"),
            ),
        ] {
            assert!(
                fragment.contains(&single_quoted),
                "{name}: claude alias list must mirror project::CLAUDE_MODEL_ALIASES \
                 ({single_quoted})"
            );
        }
        let double_quoted = crate::project::CLAUDE_MODEL_ALIASES
            .iter()
            .map(|alias| format!("\"{alias}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let station_panels = include_str!("../../../../crates/station-web/src/hud/panels.rs");
        assert!(
            station_panels.contains(&double_quoted),
            "station-web panels: MODEL_ALIASES must mirror project::CLAUDE_MODEL_ALIASES \
             ({double_quoted})"
        );
    }

    #[test]
    fn api_request_mentioning_asset_path_in_query_is_not_shadowed() {
        // Regression: the old `request_line.contains(...)` routing served
        // the station wasm for *any* request line containing its path —
        // including API calls that merely mention it in a query parameter.
        let request_line = "GET /api/fs/stat?path=/wasm-station/station_web_bg.wasm HTTP/1.1";
        let (method, path, _query) = parse_request_target(request_line);
        assert_eq!(path, "/api/fs/stat");
        assert!(
            static_asset_arm(method, path, STATION_WASM_ARM_PATHS).is_none(),
            "API path embedding an asset path must fall through to the API routes"
        );

        // The exact path (with or without a query string) still serves
        // the asset, for both GET and HEAD.
        let (method, path, query) =
            parse_request_target("GET /wasm-station/station_web_bg.wasm?v=abc HTTP/1.1");
        assert_eq!(query, "v=abc");
        let asset = static_asset_arm(method, path, STATION_WASM_ARM_PATHS)
            .expect("exact wasm path must serve the wasm");
        assert_eq!(asset.content_type, "application/wasm");
        assert_eq!(asset.body, WASM_STATION_BIN);
        assert!(static_asset_arm(
            "HEAD",
            "/wasm-station/station_web_bg.wasm",
            STATION_WASM_ARM_PATHS
        )
        .is_some());

        // Non-GET/HEAD methods and superstring paths fall through.
        assert!(static_asset_arm(
            "POST",
            "/wasm-station/station_web_bg.wasm",
            STATION_WASM_ARM_PATHS
        )
        .is_none());
        assert!(static_asset_arm(
            "GET",
            "/wasm-station/station_web_bg.wasm.map",
            STATION_WASM_ARM_PATHS
        )
        .is_none());
    }

    #[test]
    fn embedded_static_assets_precompress_large_assets() {
        for path in [
            "/wasm-web/presence_web_bg.wasm",
            "/wasm-station/station_web_bg.wasm",
            "/wasm-web/presence_web.js",
            "/wasm-station/station_web.js",
            "/three.module.min.js",
            "/codemirror-bundle.js",
            "/codemirror-bundle.css",
        ] {
            let asset = embedded_static_asset(path).expect(path);
            assert_eq!(asset.etag, asset_etag(asset.body));
            let gzip = asset
                .gzip
                .as_ref()
                .unwrap_or_else(|| panic!("{path} should be pre-gzipped"));
            assert!(gzip.len() < asset.body.len(), "{path} gzip must shrink");
        }
        // PNG is already deflate-compressed: no gzip variant.
        let icon = embedded_static_asset("/icon-128.png").unwrap();
        assert!(icon.gzip.is_none());
        // The favicon alias serves the same PNG.
        assert_eq!(
            embedded_static_asset("/favicon.ico").unwrap().body,
            ICON_128_PNG
        );
        // The PWA surface: manifest + install icons, embedded like the rest.
        let manifest = embedded_static_asset("/manifest.webmanifest").unwrap();
        assert_eq!(manifest.content_type, "application/manifest+json");
        let parsed: serde_json::Value =
            serde_json::from_slice(manifest.body).expect("manifest must be valid JSON");
        assert_eq!(parsed["display"], "standalone");
        for icon in parsed["icons"].as_array().expect("manifest icons") {
            let src = icon["src"].as_str().unwrap();
            assert!(
                embedded_static_asset(src).is_some(),
                "manifest icon {src} must itself be embedded"
            );
        }
        assert!(embedded_static_asset("/apple-touch-icon.png").is_some());
        // The gzip gate is size-based: tiny assets stay identity-only.
        let audio = embedded_static_asset("/audio-processor.js").unwrap();
        assert_eq!(audio.gzip.is_some(), audio.body.len() >= GZIP_MIN_BYTES);
        // Unknown paths are not assets.
        assert!(embedded_static_asset("/api/fs/stat").is_none());
    }

    #[test]
    fn asset_cache_control_immutable_only_for_current_version() {
        let immutable = "public, max-age=31536000, immutable";
        let revalidate = "no-cache, must-revalidate";
        assert_eq!(asset_cache_control("v=abc", "abc"), immutable);
        assert_eq!(asset_cache_control("foo=1&v=abc", "abc"), immutable);
        assert_eq!(asset_cache_control("v=stale", "abc"), revalidate);
        assert_eq!(asset_cache_control("vv=abc", "abc"), revalidate);
        assert_eq!(asset_cache_control("", "abc"), revalidate);
    }

    fn test_asset_view<'a>(body: &'a [u8], gzip: Option<&'a [u8]>) -> StaticAssetView<'a> {
        StaticAssetView {
            content_type: "application/javascript",
            body,
            etag: "feedface00000000",
            gzip,
            cache_control: None,
        }
    }

    fn split_http_response(response: &[u8]) -> (String, &[u8]) {
        let split = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header terminator")
            + 4;
        (
            String::from_utf8(response[..split].to_vec()).unwrap(),
            &response[split..],
        )
    }

    #[test]
    fn static_asset_response_serves_gzip_when_accepted() {
        let body = vec![b'a'; 16384];
        let gz = gzip_compress(&body);
        let response = build_static_asset_response(
            "GET",
            "GET /x.js?v=cur HTTP/1.1\r\nAccept-Encoding: gzip, br\r\n",
            "v=cur",
            "cur",
            test_asset_view(&body, Some(&gz)),
            false,
        );
        let (head, payload) = split_http_response(&response);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains("Content-Encoding: gzip\r\n"));
        assert!(head.contains(&format!("Content-Length: {}\r\n", gz.len())));
        assert!(head.contains("ETag: \"feedface00000000\"\r\n"));
        assert!(head.contains("Cache-Control: public, max-age=31536000, immutable\r\n"));
        assert!(head.contains("Vary: Accept-Encoding\r\n"));
        assert!(head.contains("Access-Control-Allow-Origin: *\r\n"));
        assert_eq!(payload, &gz[..]);
        // The gzip payload round-trips back to the original body.
        use std::io::Read as _;
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(payload)
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn static_asset_response_identity_without_accept_encoding() {
        let body = vec![b'b'; 8192];
        let gz = gzip_compress(&body);
        let response = build_static_asset_response(
            "GET",
            "GET /x.js HTTP/1.1\r\n",
            "",
            "cur",
            test_asset_view(&body, Some(&gz)),
            false,
        );
        let (head, payload) = split_http_response(&response);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(!head.contains("Content-Encoding"));
        assert!(head.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(head.contains("Cache-Control: no-cache, must-revalidate\r\n"));
        assert!(head.contains("Vary: Accept-Encoding\r\n"));
        assert_eq!(payload, &body[..]);
    }

    #[test]
    fn static_asset_response_304_on_etag_match() {
        let body = b"0123456789".repeat(1000);
        let gz = gzip_compress(&body);
        let response = build_static_asset_response(
            "GET",
            "GET /x.js HTTP/1.1\r\nAccept-Encoding: gzip\r\nIf-None-Match: W/\"feedface00000000\"\r\n",
            "",
            "cur",
            test_asset_view(&body, Some(&gz)),
            false,
        );
        let (head, payload) = split_http_response(&response);
        assert!(head.starts_with("HTTP/1.1 304 Not Modified\r\n"));
        assert!(payload.is_empty(), "304 must carry no body");
        assert!(head.contains("ETag: \"feedface00000000\"\r\n"));
        assert!(head.contains("Cache-Control: no-cache, must-revalidate\r\n"));
        assert!(head.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(!head.contains("Content-Encoding"));
        assert!(!head.contains("Content-Length"));
    }

    #[test]
    fn static_asset_response_head_sends_headers_only() {
        let body = vec![b'c'; 8192];
        let gz = gzip_compress(&body);
        let response = build_static_asset_response(
            "HEAD",
            "HEAD /x.js HTTP/1.1\r\nAccept-Encoding: gzip\r\n",
            "",
            "cur",
            test_asset_view(&body, Some(&gz)),
            false,
        );
        let (head, payload) = split_http_response(&response);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(payload.is_empty(), "HEAD must carry no body");
        // Headers (including Content-Length) match what GET would send.
        assert!(head.contains(&format!("Content-Length: {}\r\n", gz.len())));
        assert!(head.contains("Content-Encoding: gzip\r\n"));
    }

    #[test]
    fn asset_url_rewrite_is_idempotent_and_normalizes_stale_busters() {
        let v = "0123456789abcdef";
        // The source HTML hardcodes a stale buster on one asset (the old
        // `?v=wgpu29` station-wasm case) and none on another.
        let html = "<script src=\"/wasm-station/station_web.js?v=wgpu29\"></script>\n\
                    import('/wasm-station/station_web_bg.wasm');";
        let rewritten = rewrite_asset_url_with_version(html, "/wasm-station/station_web.js", v);
        let rewritten =
            rewrite_asset_url_with_version(&rewritten, "/wasm-station/station_web_bg.wasm", v);
        assert!(rewritten.contains("/wasm-station/station_web.js?v=0123456789abcdef\""));
        assert!(rewritten.contains("/wasm-station/station_web_bg.wasm?v=0123456789abcdef'"));
        assert!(
            !rewritten.contains("wgpu29"),
            "stale buster must be replaced"
        );
        assert!(
            !rewritten.contains("?v=0123456789abcdef?v="),
            "never a malformed double query"
        );

        // Idempotent: re-applying the rewrite changes nothing.
        let twice = rewrite_asset_url_with_version(&rewritten, "/wasm-station/station_web.js", v);
        let twice = rewrite_asset_url_with_version(&twice, "/wasm-station/station_web_bg.wasm", v);
        assert_eq!(twice, rewritten);

        // Multiple occurrences are all rewritten.
        let multi = rewrite_asset_url_with_version(
            "/icon-128.png /icon-128.png?v=old /icon-128.png",
            "/icon-128.png",
            v,
        );
        assert_eq!(
            multi,
            "/icon-128.png?v=0123456789abcdef /icon-128.png?v=0123456789abcdef /icon-128.png?v=0123456789abcdef"
        );
    }

    #[test]
    fn app_html_override_blank_values_count_as_unset() {
        assert_eq!(app_html_override_from(None), None);
        assert_eq!(app_html_override_from(Some(String::new())), None);
        assert_eq!(app_html_override_from(Some("   ".into())), None);
        assert_eq!(
            app_html_override_from(Some(" /tmp/app.html ".into())),
            Some(std::path::PathBuf::from("/tmp/app.html"))
        );
    }

    #[test]
    fn dashboard_flushes_keys_and_mouse_buttons_on_input_teardown() {
        assert!(APP_HTML.contains("owner._heldButtons.add(e.button)"));
        assert!(APP_HTML.contains("sendControl({ t: 'mu', x, y, b })"));
        assert!(APP_HTML.contains("handlers.pointercancel = (e) =>"));
        assert!(APP_HTML.contains("owner._flushHeldKeys?.();"));
    }

    #[test]
    fn agent_signin_refresh_is_derived_from_the_provider_catalog() {
        for provider in ["claude", "codex", "kimi"] {
            assert!(
                APP_HTML.contains(&format!("  {provider}: {{")),
                "missing {provider} from AGENT_SIGNIN_PROVIDERS"
            );
        }
        assert!(APP_HTML.contains("for (const provider of Object.keys(AGENT_SIGNIN_PROVIDERS)) {"));
        assert!(APP_HTML.contains("agentSigninRefresh(provider).catch(() => {});"));
    }

    #[test]
    fn dashboard_refreshes_every_declared_external_agent_signin_card() {
        assert!(APP_HTML.contains("const AGENT_SIGNIN_PROVIDERS = {"));
        assert!(APP_HTML.contains("  kimi: {"));
        assert!(APP_HTML.contains("for (const provider of Object.keys(AGENT_SIGNIN_PROVIDERS)) {"));
        assert!(APP_HTML.contains("agentSigninRefresh(provider).catch(() => {});"));
    }

    #[test]
    fn kimi_device_signin_copy_and_countdown_are_provider_aware() {
        assert!(APP_HTML.contains("startLabel: 'Start Kimi sign-in'"));
        assert!(APP_HTML.contains("openLabel: 'Open Kimi sign-in'"));
        assert!(APP_HTML.contains("devicePageName: 'Kimi'"));
        assert!(APP_HTML.contains("spec.devicePageName || spec.label"));
        assert!(
            APP_HTML.contains(".some(provider => agentSigninPhase(provider) === 'awaiting_user')")
        );
        assert!(APP_HTML.contains("paneIsVisible('vault')"));
    }

    #[test]
    fn app_html_override_rereads_per_request_and_revalidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.html");
        std::fs::write(
            &path,
            "<!DOCTYPE html><script src=\"/three.module.min.js\"></script>one",
        )
        .unwrap();
        let first = app_html_override_response("GET", "", "", &path, false);
        let first = String::from_utf8_lossy(&first);
        assert!(first.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(first.contains("Content-Type: text/html"));
        // The disk copy gets the same `?v=` rewrite as the embedded copy.
        assert!(first.contains(&format!("/three.module.min.js?v={}", asset_version())));
        assert!(first.ends_with("one"));

        // An edit is visible on the very next request — nothing caches it.
        std::fs::write(&path, "<!DOCTYPE html>two").unwrap();
        let second = app_html_override_response("GET", "", "", &path, false);
        let second = String::from_utf8_lossy(&second);
        assert!(second.ends_with("two"));

        // Unchanged content still revalidates to a 304 via its fresh ETag.
        let etag = second
            .split("ETag: \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("override response carries an ETag")
            .to_string();
        let third = app_html_override_response(
            "GET",
            &format!("If-None-Match: \"{etag}\"\r\n"),
            "",
            &path,
            false,
        );
        assert!(String::from_utf8_lossy(&third).starts_with("HTTP/1.1 304"));
    }

    #[test]
    fn app_html_override_read_failure_is_a_loud_500() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.html");
        let resp = app_html_override_response("GET", "", "", &path, false);
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 500"));
        assert!(text.contains("INTENDANT_APP_HTML_PATH"));
        // The configured absolute path (and the OS error) must never
        // reach the body — this page is readable certificate-free, and
        // the path leaks a local username/project layout. It is logged
        // server-side instead.
        assert!(!text.contains("missing.html"));
        assert!(!text.contains(&dir.path().display().to_string()));
        // HEAD keeps the status but sends headers only.
        let head = app_html_override_response("HEAD", "", "", &path, false);
        let head = String::from_utf8_lossy(&head);
        assert!(head.starts_with("HTTP/1.1 500"));
        assert!(head.ends_with("\r\n\r\n"));
    }

    /// Track AO — the Q8 labeling law plus R6's negative, asserted where
    /// chip/tooltip copy is generated (the agenda fragments riding the
    /// assembled artifact). Binding: every attestation surface says
    /// self-reported and hovers not-verified; the transport verdict and
    /// the self-report never share a glyph (attested-achieved is sky —
    /// never the transport-success green chip); blocked/abandoned render
    /// amber-class, never rose (rose stays transport-failure);
    /// unattested is a hollow neutral "no self-report" — absence, never
    /// anomaly styling. R6: the transport note stays the transport's
    /// last-words line — never rendered as, beside, or styled like a
    /// self-report, tooltips included.
    #[test]
    fn attestation_chips_follow_the_labeling_law() {
        // The one attestation tone map: achieved = sky, everything else
        // amber — an exhaustive ternary, so no outcome can reach rose or
        // the transport green.
        assert!(APP_HTML.contains("return outcome === 'achieved' ? 'sky' : 'amber';"));
        // Every chip label carries the self-report word, ◆-marked.
        assert!(APP_HTML.contains("`◆ self-reported: ${att.outcome}`"));
        assert!(APP_HTML.contains("◆ self-reported: ${escapeHtml(last.attestation.outcome)}"));
        // The not-verified hover, on every attestation surface.
        assert!(APP_HTML.contains("The session’s own report — not verified."));
        // Unattested: hollow (dashed) neutral, absence copy — never
        // anomaly styling.
        assert!(APP_HTML.contains(
            "agendaChipHtml('◇ no self-report', 'neutral', AGENDA_UNATTESTED_HOVER, true)"
        ));
        assert!(APP_HTML
            .contains("No self-report exists for this run — the session ended without attesting."));
        // The hood's op rows: attest ops are first-class, labeled
        // self-report, achieved sky / the rest amber (never rose).
        assert!(APP_HTML.contains("attested ${op.outcome || '—'} — self-report"));
        assert!(APP_HTML.contains("return op.outcome === 'achieved' ? 'sky' : 'amber';"));
        // The CSS side of the hue law.
        assert!(APP_HTML.contains(".ag2-auto-attest.att-achieved { color: var(--sky); }"));
        assert!(APP_HTML.contains(".ag2-auto-attest.att-abandoned { color: var(--amber); }"));
        // R6's negative: the strip's transport tip stays state + last
        // words only (no self-report wording can enter the template),
        // and the inspector's transport-note tooltip names it the
        // transport record, not a self-report.
        assert!(
            APP_HTML.contains("const tip = `${last.state}${last.note ? ` — ${last.note}` : ''}`;")
        );
        assert!(APP_HTML
            .contains("The session’s final message as the run ended — the transport record, not a self-report"));
        // Regeneration legibility rides the same surfaces: the run line
        // names the bounded auto-retry.
        assert!(APP_HTML.contains("attempt ${e.last_run_attempt} (auto-retry)"));
        // The session grid's chip obeys the same law (Track AO §2.8):
        // the ◆/◇ family marks the self-report axis, and the
        // safe-to-stop copy claims only what the machine knows.
        assert!(APP_HTML.contains("`◆ self-reported: ${v.outcome}` : '◇ no self-report'"));
        assert!(
            APP_HTML.contains("Stopping kills a live agenda run — the occurrence records failed")
        );
        assert!(
            APP_HTML.contains("stopping it kills that agenda run (the occurrence records failed)")
        );
        assert!(APP_HTML
            .contains("Idle · no agenda-owed work — stopping loses only this session’s context"));
    }

    /// The closable-at-a-glance lens (Track AO follow-through) stays a
    /// POSITIVE-only composition of already-ruled claims: the served
    /// stop derivation vetoes by name, settled turns positive only once
    /// the window is quiet, a linked-but-unclaimed occurrence claims
    /// nothing, the chip ships hidden (never "0 closable"), the dim keys
    /// on the html attribute the toggle stamps, an emptied count
    /// disengages the lens instead of stranding a fully-dimmed grid, and
    /// the per-window class derives in the same single pass as the count
    /// so the two can never disagree. The × affordance's ruled copy is
    /// pinned above (`attestation_chips_follow_the_labeling_law`) — the
    /// lens adds a glance layer without rewriting those claims.
    #[test]
    fn closable_lens_is_positive_only() {
        assert!(APP_HTML
            .contains("if (stop === 'kills_live_run' || stop === 'owed_work') return false;"));
        assert!(APP_HTML.contains("if (stop === 'settled') return quiet;"));
        assert!(APP_HTML.contains("if (stop || claim.linked) return false;"));
        assert!(APP_HTML.contains("id=\"ui2-closable-lens-btn\" hidden"));
        assert!(APP_HTML.contains(
            "html[data-ui2-closable-lens=\"on\"] .session-window:not(.session-window-closable)"
        ));
        assert!(
            APP_HTML.contains("if (btn.hidden && ui2ClosableLensOn) ui2SetClosableLens(false);")
        );
        assert!(APP_HTML.contains("win.el.classList.toggle('session-window-closable', closable)"));
    }

    /// The engaged lens states its direction ON-canvas (live specimen
    /// 2026-07-31: a returning viewer read the dim as "closable" — the
    /// exact inversion): the chip's own text flips to name the bright
    /// side, the grid legend restates it keyed on the same html
    /// attribute as the dim (it can never outlive the lens), navigating
    /// away from the Timeline grid disengages the look, and the
    /// agenda-settled × title is phase-guarded — on a non-quiet window
    /// it leads with the live pill label instead of reading as an
    /// all-clear, re-derived on every phase application because the
    /// phase-only fast path skips the wide render.
    #[test]
    fn closable_lens_states_its_direction() {
        assert!(APP_HTML.contains("safe to close · rest dimmed"));
        assert!(APP_HTML.contains("id=\"ui2-closable-lens-legend\""));
        assert!(APP_HTML.contains(
            "html.ui-v2[data-ui2-closable-lens=\"on\"] .ui2-closable-lens-legend { display: flex; }"
        ));
        assert!(APP_HTML.contains(
            "— stopping interrupts it; no agenda-owed work remains (the linked occurrence is settled)"
        ));
        assert!(APP_HTML.contains("function ui2DisengageClosableLensOffSurface()"));
        assert!(APP_HTML.contains("updateSessionWindowCloseTitle(win, sid);"));
    }
}

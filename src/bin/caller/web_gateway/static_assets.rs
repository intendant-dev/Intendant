//! Embedded dashboard assets and their HTTP serving: the include_str!/
//! include_bytes! payloads (app shell, wasm bundles, icons, vendored JS),
//! version/etag stamping, cache-control policy, and the static-asset
//! response builder used by the gateway dispatch chain.

use super::*;

pub(crate) const APP_HTML: &str = include_str!("../../../../static/app.html");

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
// the terminal working offline/LAN and on hosted Connect. The vendored
// bytes hash-match the exact SRI digests the CDN loader pinned.
pub(crate) const XTERM_JS: &str = include_str!("../../../../static/xterm.min.js");

pub(crate) const XTERM_ADDON_FIT_JS: &str =
    include_str!("../../../../static/xterm-addon-fit.min.js");

pub(crate) const XTERM_CSS: &str = include_str!("../../../../static/xterm.css");

// Self-hosted variable fonts (SIL OFL 1.1; license texts ship in
// static/fonts/). Referenced by the @font-face rules in
// static/app/09-styles-fonts.css — the dashboard must stay fully
// self-contained for offline/LAN and hosted-Connect use.
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
pub(crate) fn build_static_asset_response(
    method: &str,
    header_text: &str,
    query: &str,
    current_version: &str,
    asset: StaticAssetView<'_>,
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
        return HttpResponse::new("304 Not Modified")
            .header("ETag", format!("\"{}\"", asset.etag))
            .header("Cache-Control", cache_control)
            .header_segment(vary)
            .header("Access-Control-Allow-Origin", "*")
            .header("Connection", "close")
            .into_bytes();
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
        .header("Access-Control-Allow-Origin", "*")
        .header("Connection", "close")
        .into_bytes();
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
            )
        }
        Err(err) => {
            eprintln!(
                "[web_gateway] INTENDANT_APP_HTML_PATH read failed ({}): {err}",
                path.display()
            );
            let body = format!(
                "INTENDANT_APP_HTML_PATH override is active but unreadable.\n\n\
                 path: {}\nerror: {err}\n\n\
                 Fix the path (or unset INTENDANT_APP_HTML_PATH) and refresh.\n",
                path.display()
            );
            let mut response = HttpResponse::new("500 Internal Server Error")
                .header("Content-Type", "text/plain; charset=utf-8")
                .header("Content-Length", body.len().to_string())
                .header("Cache-Control", "no-store")
                .header("Access-Control-Allow-Origin", "*")
                .header("Connection", "close")
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

    #[test]
    fn vault_kernel_override_serves_disk_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let app_html_path = dir.path().join("app.html");
        // No sibling yet: fall back to the embedded kernel.
        assert!(vault_kernel_override_response("GET", "", "", &app_html_path).is_none());
        std::fs::write(
            dir.path().join("vault-kernel.js"),
            b"self.onmessage=null;\n",
        )
        .unwrap();
        let resp = vault_kernel_override_response("GET", "", "", &app_html_path)
            .expect("sibling kernel must be served");
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: application/javascript"));
        assert!(text.contains("Cache-Control: no-cache"));
        assert!(text.ends_with("self.onmessage=null;\n"));
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
    fn app_html_override_rereads_per_request_and_revalidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.html");
        std::fs::write(
            &path,
            "<!DOCTYPE html><script src=\"/three.module.min.js\"></script>one",
        )
        .unwrap();
        let first = app_html_override_response("GET", "", "", &path);
        let first = String::from_utf8_lossy(&first);
        assert!(first.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(first.contains("Content-Type: text/html"));
        // The disk copy gets the same `?v=` rewrite as the embedded copy.
        assert!(first.contains(&format!("/three.module.min.js?v={}", asset_version())));
        assert!(first.ends_with("one"));

        // An edit is visible on the very next request — nothing caches it.
        std::fs::write(&path, "<!DOCTYPE html>two").unwrap();
        let second = app_html_override_response("GET", "", "", &path);
        let second = String::from_utf8_lossy(&second);
        assert!(second.ends_with("two"));

        // Unchanged content still revalidates to a 304 via its fresh ETag.
        let etag = second
            .split("ETag: \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("override response carries an ETag")
            .to_string();
        let third =
            app_html_override_response("GET", &format!("If-None-Match: \"{etag}\"\r\n"), "", &path);
        assert!(String::from_utf8_lossy(&third).starts_with("HTTP/1.1 304"));
    }

    #[test]
    fn app_html_override_read_failure_is_a_loud_500() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.html");
        let resp = app_html_override_response("GET", "", "", &path);
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 500"));
        assert!(text.contains("INTENDANT_APP_HTML_PATH"));
        assert!(text.contains("missing.html"));
        // HEAD keeps the status but sends headers only.
        let head = app_html_override_response("HEAD", "", "", &path);
        let head = String::from_utf8_lossy(&head);
        assert!(head.starts_with("HTTP/1.1 500"));
        assert!(head.ends_with("\r\n\r\n"));
    }
}

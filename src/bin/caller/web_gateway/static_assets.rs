//! Embedded dashboard assets and their HTTP serving: the include_str!/
//! include_bytes! payloads (app shell, wasm bundles, icons, vendored JS),
//! version/etag stamping, cache-control policy, and the static-asset
//! response builder used by the gateway dispatch chain.

use super::*;

pub(crate) const APP_HTML: &str = include_str!("../../../../static/app.html");

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
        return format!(
            "HTTP/1.1 304 Not Modified\r\n\
             ETag: \"{etag}\"\r\n\
             Cache-Control: {cache_control}\r\n\
             {vary}Access-Control-Allow-Origin: *\r\n\
             Connection: close\r\n\
             \r\n",
            etag = asset.etag,
        )
        .into_bytes();
    }
    let gzip_body = asset
        .gzip
        .filter(|_| accept_encoding_allows_gzip(header_text));
    let (payload, content_encoding) = match gzip_body {
        Some(gz) => (gz, "Content-Encoding: gzip\r\n"),
        None => (asset.body, ""),
    };
    let mut response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         {content_encoding}ETag: \"{etag}\"\r\n\
         Cache-Control: {cache_control}\r\n\
         {vary}Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n",
        content_type = asset.content_type,
        len = payload.len(),
        etag = asset.etag,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    const STATION_WASM_ARM_PATHS: &[&str] = &[
        "/wasm-web/presence_web_bg.wasm",
        "/wasm-station/station_web_bg.wasm",
    ];

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
}

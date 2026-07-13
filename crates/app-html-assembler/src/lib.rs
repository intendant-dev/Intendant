//! Assembles `static/app.html` from the ordered fragments in `static/app/`.
//!
//! The dashboard SPA is served as one self-contained HTML file — that is a
//! product feature (zero-install, strict CSP, embedded in the daemon via
//! `include_str!`, served from disk by intendant-connect). Its *source* lives
//! as typed fragments (`.css` / `.js` / `.html`) under `static/app/`, in the
//! order fixed by `static/app/manifest.txt`, and this crate concatenates them
//! back into the tracked artifact. The transform is concatenation plus a
//! generated-file header, one banner comment per fragment, and exactly two
//! documented substitutions — the vault-kernel hash pin and the build stamp
//! (both below) — nothing
//! else: all JS fragments share the single `<script type="module">` scope
//! exactly as they did in the monolith (the open/close tags live in tiny
//! wrapper fragments), so hoisting and TDZ order are untouched by
//! construction.
//!
//! **The vault-kernel hash pin.** `static/vault-kernel.js` is the vault's
//! crypto kernel: a small, separately served worker that owns the vault key
//! material, so the code the keys depend on stays one auditable file rather
//! than the whole bundle. The page refuses to instantiate a kernel it
//! cannot verify, and the reference it verifies against is minted here: any
//! fragment may carry the placeholder [`VAULT_KERNEL_HASH_TOKEN`], and
//! assembly replaces every occurrence with the lowercase-hex sha256 of the
//! kernel file's exact bytes (deterministic, so the regen gate still
//! settles). Fail-closed: a placeholder with no kernel file to hash is an
//! assembly error. A daemon-side parity test
//! (`web_gateway/static_assets.rs`) recomputes the hash and asserts the
//! assembled artifact pins it, so editing the kernel without regenerating
//! app.html fails the suite.
//!
//! **The build stamp.** Any fragment may carry [`APP_BUILD_TOKEN`], and
//! assembly replaces every occurrence with the first 16 lowercase-hex chars
//! of the sha256 over the *raw* fragment bytes in manifest order (raw =
//! placeholders un-substituted, so the stamp is well-defined and
//! deterministic — the regen gate settles). The daemon extracts the stamp
//! from its embedded artifact and serves it in `/config`; a dashboard tab
//! whose own stamp differs is from an older served bundle and nudges itself
//! to reload (`31-init-identity-fleet.js` declares
//! `const INTENDANT_APP_BUILD = '__INTENDANT_APP_BUILD__'`). A daemon-side
//! parity test (`web_gateway/static_assets.rs`) asserts the shipped artifact
//! carries a minted stamp, never the placeholder.
//!
//! Fail-closed: any mismatch between the manifest and the fragment directory
//! is an error, never a silently dropped fragment — a stale or partial
//! artifact would ship a stale dashboard inside the daemon binary.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

mod eval_order;

/// Fragment directory, relative to the repo root.
pub const FRAGMENT_DIR: &str = "static/app";
/// Assembly-order manifest inside [`FRAGMENT_DIR`].
pub const MANIFEST_NAME: &str = "manifest.txt";
/// The generated artifact, relative to the repo root.
pub const OUTPUT: &str = "static/app.html";
/// The vault crypto kernel, relative to the repo root — a standalone,
/// separately served artifact (NOT a fragment) whose sha256 is pinned into
/// the assembled bundle. See "The vault-kernel hash pin" in the crate docs.
pub const VAULT_KERNEL_PATH: &str = "static/vault-kernel.js";
/// Placeholder fragments carry where the kernel's lowercase-hex sha256 is
/// substituted at assembly time (`32-vault-custody.js` declares
/// `const VAULT_KERNEL_SHA256 = '__VAULT_KERNEL_SHA256__'`).
pub const VAULT_KERNEL_HASH_TOKEN: &str = "__VAULT_KERNEL_SHA256__";
/// Placeholder any fragment may carry where the build stamp — the first 16
/// lowercase-hex chars of the sha256 over the raw manifest-ordered fragment
/// bytes — is substituted at assembly time. See "The build stamp" in the
/// crate docs.
pub const APP_BUILD_TOKEN: &str = "__INTENDANT_APP_BUILD__";

/// Extensions that count as fragments. Anything else in the directory
/// (README.md, the manifest itself) is ignored by the completeness check;
/// the manifest may only list these types.
const FRAGMENT_EXTENSIONS: &[&str] = &["css", "js", "html"];

/// First line of the generated artifact. A comment before `<!DOCTYPE>` is
/// spec-legal and does not trigger quirks mode; nothing in the tree sniffs
/// the artifact's first line (the validate-dashboard served-vs-disk identity
/// check normalizes only cache-bust query strings, which appear in both).
pub const GENERATED_HEADER: &str = "<!-- GENERATED from static/app/ — edit the fragments, \
     not this file. Order: static/app/manifest.txt; any cargo build reassembles; \
     CI enforces the match. -->\n";

/// Banner emitted before a fragment so the assembled file stays navigable
/// and a devtools line number maps back to its fragment via the nearest
/// banner. `None` for the first fragment (the generated header marks it) and
/// for the `*-open.html` / `*-close.html` wrappers: a wrapper's banner would
/// land in the syntax context the wrapper is about to change (e.g. an HTML
/// comment inside the still-open module script, before `</script>`), so the
/// wrappers stay unmarked. With wrappers excluded, extension == context:
/// `.css` banners sit inside `<style>`, `.js` banners inside the script
/// scopes, `.html` banners in markup.
fn fragment_banner(entry: &str, index: usize) -> Option<String> {
    if index == 0 || entry.ends_with("-open.html") || entry.ends_with("-close.html") {
        return None;
    }
    match Path::new(entry).extension().and_then(|e| e.to_str()) {
        Some("css") | Some("js") => Some(format!("/* ── static/app/{entry} ── */\n")),
        Some("html") => Some(format!("<!-- ── static/app/{entry} ── -->\n")),
        _ => None,
    }
}

/// What one assembly run did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No `static/app/` fragments exist (pre-split checkout, or the split
    /// was rolled back) — nothing to do, the tracked artifact is the truth.
    NoFragments,
    /// Assembled output already matched the on-disk artifact byte for byte.
    Unchanged { fragments: usize, bytes: usize },
    /// Artifact rewritten from the fragments.
    Written { fragments: usize, bytes: usize },
}

/// Assemble `static/app.html` under `repo_root`, writing only when the
/// output differs (an unconditional write would bump the artifact's mtime
/// every build and make cargo re-embed — and thus rebuild — the gateway
/// crate each time).
pub fn assemble(repo_root: &Path) -> Result<Outcome, String> {
    let fragment_dir = repo_root.join(FRAGMENT_DIR);
    let manifest_path = fragment_dir.join(MANIFEST_NAME);

    if !manifest_path.is_file() {
        if fragment_files(&fragment_dir).is_empty() {
            return Ok(Outcome::NoFragments);
        }
        return Err(format!(
            "{} exists with fragments but {}/{} is missing — the manifest is \
             the assembly order and must list every fragment",
            FRAGMENT_DIR, FRAGMENT_DIR, MANIFEST_NAME
        ));
    }

    let manifest = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("failed to read {}: {e}", manifest_path.display()))?;
    let entries = parse_manifest(&manifest)?;
    validate_completeness(&entries, &fragment_dir)?;

    // The vault-kernel hash pin (crate docs): computed lazily on the first
    // fragment that carries the placeholder, so fragment sets without a pin
    // (tests, pre-kernel checkouts) never require the kernel file.
    let mut kernel_hash: Option<String> = None;

    // The build stamp (crate docs): sha256 over the raw fragment bytes in
    // manifest order, before any substitution — deterministic across
    // machines and self-consistent (the stamped artifact still hashes the
    // placeholder form). Fragments are pre-read once and reused below.
    let mut fragment_bytes: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
    let mut build_hasher = Sha256::new();
    for entry in &entries {
        let path = fragment_dir.join(entry);
        let bytes = fs::read(&path)
            .map_err(|e| format!("failed to read fragment {}: {e}", path.display()))?;
        build_hasher.update(&bytes);
        fragment_bytes.push(bytes);
    }
    let build_stamp = {
        let mut hex = String::with_capacity(64);
        for byte in build_hasher.finalize() {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex.truncate(16);
        hex
    };

    let mut assembled: Vec<u8> = Vec::new();
    assembled.extend_from_slice(GENERATED_HEADER.as_bytes());
    // All .js fragments share one <script type="module"> scope in manifest
    // order; lint them (below) as the single program they become — with the
    // pin already substituted, exactly as the browser will evaluate it.
    let mut js_fragments: Vec<(String, String)> = Vec::new();
    for (index, (entry, raw)) in entries.iter().zip(fragment_bytes).enumerate() {
        if let Some(banner) = fragment_banner(entry, index) {
            assembled.extend_from_slice(banner.as_bytes());
        }
        let mut bytes = raw;
        if find_subslice(&bytes, APP_BUILD_TOKEN.as_bytes()).is_some() {
            bytes = replace_subslice(&bytes, APP_BUILD_TOKEN.as_bytes(), build_stamp.as_bytes());
        }
        if find_subslice(&bytes, VAULT_KERNEL_HASH_TOKEN.as_bytes()).is_some() {
            if kernel_hash.is_none() {
                let kernel_path = repo_root.join(VAULT_KERNEL_PATH);
                let kernel_bytes = fs::read(&kernel_path).map_err(|e| {
                    format!(
                        "fragment {entry} pins the vault-kernel hash \
                         ({VAULT_KERNEL_HASH_TOKEN}) but {VAULT_KERNEL_PATH} is unreadable: {e} \
                         — the kernel file must exist so the pin can be minted"
                    )
                })?;
                let mut hex = String::with_capacity(64);
                for byte in Sha256::digest(&kernel_bytes) {
                    hex.push_str(&format!("{byte:02x}"));
                }
                kernel_hash = Some(hex);
            }
            bytes = replace_subslice(
                &bytes,
                VAULT_KERNEL_HASH_TOKEN.as_bytes(),
                kernel_hash.as_deref().unwrap_or_default().as_bytes(),
            );
        }
        if entry.ends_with(".js") {
            js_fragments.push((
                format!("{FRAGMENT_DIR}/{entry}"),
                String::from_utf8_lossy(&bytes).into_owned(),
            ));
        }
        assembled.extend_from_slice(&bytes);
    }
    eval_order::check_eval_order(&js_fragments)?;

    let output_path = repo_root.join(OUTPUT);
    let existing = fs::read(&output_path).ok();
    if existing.as_deref() == Some(assembled.as_slice()) {
        return Ok(Outcome::Unchanged {
            fragments: entries.len(),
            bytes: assembled.len(),
        });
    }
    fs::write(&output_path, &assembled)
        .map_err(|e| format!("failed to write {}: {e}", output_path.display()))?;
    Ok(Outcome::Written {
        fragments: entries.len(),
        bytes: assembled.len(),
    })
}

/// First byte offset of `needle` in `haystack`, if any. Byte-level so the
/// substitution never cares about fragment encodings.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Replace every occurrence of `needle` with `replacement`.
fn replace_subslice(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(idx) = find_subslice(rest, needle) {
        out.extend_from_slice(&rest[..idx]);
        out.extend_from_slice(replacement);
        rest = &rest[idx + needle.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// Parse manifest text into ordered fragment paths (relative to the fragment
/// directory). Blank lines and `#` comments are ignored.
fn parse_manifest(text: &str) -> Result<Vec<String>, String> {
    let mut entries = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let lineno = idx + 1;
        if line.starts_with('/') || line.contains('\\') || line.split('/').any(|c| c == "..") {
            return Err(format!(
                "manifest line {lineno}: {line:?} must be a relative path inside {FRAGMENT_DIR} \
                 (forward slashes, no '..')"
            ));
        }
        let is_fragment_type = Path::new(line)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| FRAGMENT_EXTENSIONS.contains(&e));
        if !is_fragment_type {
            return Err(format!(
                "manifest line {lineno}: {line:?} is not a fragment type ({})",
                FRAGMENT_EXTENSIONS.join("/")
            ));
        }
        if entries.iter().any(|e| e == line) {
            return Err(format!("manifest line {lineno}: duplicate entry {line:?}"));
        }
        entries.push(line.to_string());
    }
    if entries.is_empty() {
        return Err(format!("{FRAGMENT_DIR}/{MANIFEST_NAME} lists no fragments"));
    }
    Ok(entries)
}

/// Both directions of the manifest ↔ directory invariant: every listed
/// fragment exists, and every fragment-typed file in the directory is listed.
fn validate_completeness(entries: &[String], fragment_dir: &Path) -> Result<(), String> {
    let mut missing: Vec<&str> = entries
        .iter()
        .filter(|e| !fragment_dir.join(e.as_str()).is_file())
        .map(|e| e.as_str())
        .collect();
    missing.sort_unstable();
    if !missing.is_empty() {
        return Err(format!(
            "manifest lists fragments that do not exist under {FRAGMENT_DIR}: {}",
            missing.join(", ")
        ));
    }

    let mut unlisted: Vec<String> = fragment_files(fragment_dir)
        .into_iter()
        .filter(|f| !entries.iter().any(|e| e == f))
        .collect();
    unlisted.sort_unstable();
    if !unlisted.is_empty() {
        return Err(format!(
            "fragments exist under {FRAGMENT_DIR} but are not listed in {MANIFEST_NAME} \
             (they would be silently dropped from app.html): {}",
            unlisted.join(", ")
        ));
    }
    Ok(())
}

/// All fragment-typed files under `dir`, recursively, as `/`-joined paths
/// relative to `dir`. Empty when the directory doesn't exist.
fn fragment_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect_fragment_files(dir, PathBuf::new(), &mut out);
    out
}

fn collect_fragment_files(dir: &Path, rel: PathBuf, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = rel.join(entry.file_name());
        if path.is_dir() {
            collect_fragment_files(&path, rel, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| FRAGMENT_EXTENSIONS.contains(&e))
        {
            // Manifest entries use forward slashes on every platform.
            let joined = rel
                .iter()
                .map(|c| c.to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push(joined);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn no_fragments_is_a_clean_skip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(assemble(dir.path()), Ok(Outcome::NoFragments));
        // An unrelated file in static/app/ (e.g. a README) still skips.
        write(dir.path(), "static/app/README.md", "docs");
        assert_eq!(assemble(dir.path()), Ok(Outcome::NoFragments));
    }

    #[test]
    fn fragments_without_manifest_fail() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "static/app/00-head.html", "<html>");
        let err = assemble(dir.path()).unwrap_err();
        assert!(err.contains("manifest.txt is missing"), "{err}");
    }

    #[test]
    fn assembles_in_manifest_order_and_settles() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "static/app/00-a.html", "A\n");
        write(dir.path(), "static/app/10-b.css", "B\n");
        write(dir.path(), "static/app/20-c.js", "C\n");
        write(
            dir.path(),
            "static/app/manifest.txt",
            "# order\n00-a.html\n\n20-c.js\n10-b.css\n",
        );
        let expected = format!(
            "{GENERATED_HEADER}A\n\
             /* ── static/app/20-c.js ── */\nC\n\
             /* ── static/app/10-b.css ── */\nB\n"
        );
        let bytes = expected.len();
        assert_eq!(
            assemble(dir.path()),
            Ok(Outcome::Written {
                fragments: 3,
                bytes
            })
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(OUTPUT)).unwrap(),
            expected
        );
        // Second run: byte-identical, no rewrite.
        assert_eq!(
            assemble(dir.path()),
            Ok(Outcome::Unchanged {
                fragments: 3,
                bytes
            })
        );
        // A hand-edit to the artifact is overwritten by fragment truth.
        write(dir.path(), OUTPUT, "hand edit");
        assert_eq!(
            assemble(dir.path()),
            Ok(Outcome::Written {
                fragments: 3,
                bytes
            })
        );
    }

    #[test]
    fn wrappers_and_first_fragment_get_no_banner() {
        // The wrappers hold the <style> / <script type="module"> transitions;
        // a banner on them would land in the syntax context they're closing.
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in [
            ("00-head.html", "<!DOCTYPE html>\n"),
            ("09-style-open.html", "<style>\n"),
            ("10-x.css", ".x{}\n"),
            ("19-style-close.html", "</style>\n"),
            ("20-shell.html", "<body>\n"),
            ("30-module-open.html", "<script type=\"module\">\n"),
            ("31-a.js", "let a;\n"),
            ("59-module-close.html", "</script>\n"),
        ] {
            write(dir.path(), &format!("static/app/{name}"), content);
        }
        write(
            dir.path(),
            "static/app/manifest.txt",
            "00-head.html\n09-style-open.html\n10-x.css\n19-style-close.html\n\
             20-shell.html\n30-module-open.html\n31-a.js\n59-module-close.html\n",
        );
        assemble(dir.path()).unwrap();
        let out = fs::read_to_string(dir.path().join(OUTPUT)).unwrap();
        let expected = format!(
            "{GENERATED_HEADER}<!DOCTYPE html>\n<style>\n\
             /* ── static/app/10-x.css ── */\n.x{{}}\n</style>\n\
             <!-- ── static/app/20-shell.html ── -->\n<body>\n\
             <script type=\"module\">\n\
             /* ── static/app/31-a.js ── */\nlet a;\n</script>\n"
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn unlisted_and_missing_fragments_fail() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "static/app/00-a.html", "A\n");
        write(dir.path(), "static/app/10-b.css", "B\n");
        write(dir.path(), "static/app/manifest.txt", "00-a.html\n");
        let err = assemble(dir.path()).unwrap_err();
        assert!(
            err.contains("not listed") && err.contains("10-b.css"),
            "{err}"
        );

        write(
            dir.path(),
            "static/app/manifest.txt",
            "00-a.html\n10-b.css\n90-gone.js\n",
        );
        let err = assemble(dir.path()).unwrap_err();
        assert!(
            err.contains("do not exist") && err.contains("90-gone.js"),
            "{err}"
        );
    }

    #[test]
    fn build_stamp_substitutes_and_settles() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "static/app/00-a.js",
            "const INTENDANT_APP_BUILD = '__INTENDANT_APP_BUILD__';\n",
        );
        write(dir.path(), "static/app/manifest.txt", "00-a.js\n");
        assemble(dir.path()).unwrap();
        let out = fs::read_to_string(dir.path().join(OUTPUT)).unwrap();
        assert!(
            !out.contains(APP_BUILD_TOKEN),
            "placeholder must be substituted"
        );
        let stamp1 = out.split("INTENDANT_APP_BUILD = '").nth(1).unwrap()[..16].to_string();
        assert!(stamp1.bytes().all(|b| b.is_ascii_hexdigit()));
        // Settles: a rerun is byte-identical.
        assert!(matches!(
            assemble(dir.path()).unwrap(),
            Outcome::Unchanged { .. }
        ));
        // Any fragment edit mints a different stamp.
        write(
            dir.path(),
            "static/app/00-a.js",
            "const INTENDANT_APP_BUILD = '__INTENDANT_APP_BUILD__'; // v2\n",
        );
        assemble(dir.path()).unwrap();
        let out2 = fs::read_to_string(dir.path().join(OUTPUT)).unwrap();
        let stamp2 = out2.split("INTENDANT_APP_BUILD = '").nth(1).unwrap()[..16].to_string();
        assert_ne!(stamp1, stamp2);
    }

    #[test]
    fn assemble_fails_on_cross_fragment_eval_order_hazard() {
        // Integration of the eval-order lint (eval_order.rs, where the
        // scanner-level cases live): a top-level reference to a later
        // fragment's `let` must fail assembly, not ship a dashboard that
        // dies at module evaluation.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "static/app/30-open.html",
            "<script type=\"module\">\n",
        );
        write(
            dir.path(),
            "static/app/40-a.js",
            "if (laterFlag) { console.log(1); }\n",
        );
        write(dir.path(), "static/app/50-b.js", "let laterFlag = true;\n");
        write(dir.path(), "static/app/59-close.html", "</script>\n");
        write(
            dir.path(),
            "static/app/manifest.txt",
            "30-open.html\n40-a.js\n50-b.js\n59-close.html\n",
        );
        let err = assemble(dir.path()).unwrap_err();
        assert!(err.contains("eval-order lint failed"), "{err}");
        assert!(err.contains("laterFlag"), "{err}");
        assert!(err.contains("40-a.js"), "{err}");
        assert!(err.contains("50-b.js"), "{err}");
        // No artifact must be written for a failing fragment set.
        assert!(!dir.path().join(OUTPUT).exists());

        // The incident's fix shape resolves it: declare in the referencing
        // fragment; later fragments keep only ordinary uses.
        write(
            dir.path(),
            "static/app/40-a.js",
            "let laterFlag = true;\nif (laterFlag) { console.log(1); }\n",
        );
        write(
            dir.path(),
            "static/app/50-b.js",
            "console.log(laterFlag);\n",
        );
        assemble(dir.path()).unwrap();
    }

    #[test]
    fn vault_kernel_hash_pin_substitutes_and_settles() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "static/vault-kernel.js",
            "self.onmessage = () => {};\n",
        );
        write(
            dir.path(),
            "static/app/30-a.js",
            "const VAULT_KERNEL_SHA256 = '__VAULT_KERNEL_SHA256__';\n",
        );
        write(dir.path(), "static/app/manifest.txt", "30-a.js\n");
        assemble(dir.path()).unwrap();
        let out = fs::read_to_string(dir.path().join(OUTPUT)).unwrap();
        // sha256("self.onmessage = () => {};\n"), lowercase hex.
        let expected = {
            let mut hex = String::new();
            for byte in Sha256::digest("self.onmessage = () => {};\n".as_bytes()) {
                hex.push_str(&format!("{byte:02x}"));
            }
            hex
        };
        assert!(
            out.contains(&format!("const VAULT_KERNEL_SHA256 = '{expected}';")),
            "assembled artifact must pin the kernel hash: {out}"
        );
        assert!(
            !out.contains(VAULT_KERNEL_HASH_TOKEN),
            "no placeholder may survive assembly"
        );
        // Deterministic: a second run settles (no rewrite churn).
        assert!(matches!(
            assemble(dir.path()),
            Ok(Outcome::Unchanged { .. })
        ));
        // A kernel edit changes the pin on the next assembly.
        write(
            dir.path(),
            "static/vault-kernel.js",
            "self.onmessage = null;\n",
        );
        assert!(matches!(assemble(dir.path()), Ok(Outcome::Written { .. })));
        let out = fs::read_to_string(dir.path().join(OUTPUT)).unwrap();
        assert!(
            !out.contains(&expected),
            "stale pin must not survive a kernel edit"
        );
    }

    #[test]
    fn vault_kernel_pin_without_kernel_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "static/app/30-a.js",
            "const VAULT_KERNEL_SHA256 = '__VAULT_KERNEL_SHA256__';\n",
        );
        write(dir.path(), "static/app/manifest.txt", "30-a.js\n");
        let err = assemble(dir.path()).unwrap_err();
        assert!(err.contains("vault-kernel"), "{err}");
        assert!(err.contains("30-a.js"), "{err}");
        assert!(!dir.path().join(OUTPUT).exists());
    }

    #[test]
    fn replace_subslice_handles_multiple_and_absent_needles() {
        assert_eq!(replace_subslice(b"a__T__b__T__c", b"__T__", b"X"), b"aXbXc");
        assert_eq!(replace_subslice(b"abc", b"__T__", b"X"), b"abc");
        assert_eq!(find_subslice(b"abc", b""), None);
        assert_eq!(find_subslice(b"ab", b"abc"), None);
        assert_eq!(find_subslice(b"xabc", b"abc"), Some(1));
    }

    #[test]
    fn manifest_rejects_traversal_duplicates_and_foreign_types() {
        assert!(parse_manifest("../evil.js\n").is_err());
        assert!(parse_manifest("/abs.js\n").is_err());
        assert!(parse_manifest("a\\b.js\n").is_err());
        assert!(parse_manifest("a.js\na.js\n").is_err());
        assert!(parse_manifest("notes.txt\n").is_err());
        assert!(parse_manifest("# only comments\n\n").is_err());
        assert_eq!(
            parse_manifest("# c\nsub/a.js\nb.css\n").unwrap(),
            vec!["sub/a.js".to_string(), "b.css".to_string()]
        );
    }
}

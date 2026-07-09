//! Assembles `static/app.html` from the ordered fragments in `static/app/`.
//!
//! The dashboard SPA is served as one self-contained HTML file — that is a
//! product feature (zero-install, strict CSP, embedded in the daemon via
//! `include_str!`, served from disk by intendant-connect). Its *source* lives
//! as typed fragments (`.css` / `.js` / `.html`) under `static/app/`, in the
//! order fixed by `static/app/manifest.txt`, and this crate concatenates them
//! back into the tracked artifact. The transform is concatenation plus a
//! generated-file header and one banner comment per fragment — nothing else:
//! all JS fragments share the single `<script type="module">` scope exactly
//! as they did in the monolith (the open/close tags live in tiny wrapper
//! fragments), so hoisting and TDZ order are untouched by construction.
//!
//! Fail-closed: any mismatch between the manifest and the fragment directory
//! is an error, never a silently dropped fragment — a stale or partial
//! artifact would ship a stale dashboard inside the daemon binary.

use std::fs;
use std::path::{Path, PathBuf};

mod eval_order;

/// Fragment directory, relative to the repo root.
pub const FRAGMENT_DIR: &str = "static/app";
/// Assembly-order manifest inside [`FRAGMENT_DIR`].
pub const MANIFEST_NAME: &str = "manifest.txt";
/// The generated artifact, relative to the repo root.
pub const OUTPUT: &str = "static/app.html";

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

    let mut assembled: Vec<u8> = Vec::new();
    assembled.extend_from_slice(GENERATED_HEADER.as_bytes());
    // All .js fragments share one <script type="module"> scope in manifest
    // order; lint them (below) as the single program they become.
    let mut js_fragments: Vec<(String, String)> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(banner) = fragment_banner(entry, index) {
            assembled.extend_from_slice(banner.as_bytes());
        }
        let path = fragment_dir.join(entry);
        let bytes = fs::read(&path)
            .map_err(|e| format!("failed to read fragment {}: {e}", path.display()))?;
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
            Ok(Outcome::Written { fragments: 3, bytes })
        );
        assert_eq!(fs::read_to_string(dir.path().join(OUTPUT)).unwrap(), expected);
        // Second run: byte-identical, no rewrite.
        assert_eq!(
            assemble(dir.path()),
            Ok(Outcome::Unchanged { fragments: 3, bytes })
        );
        // A hand-edit to the artifact is overwritten by fragment truth.
        write(dir.path(), OUTPUT, "hand edit");
        assert_eq!(
            assemble(dir.path()),
            Ok(Outcome::Written { fragments: 3, bytes })
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
        assert!(err.contains("not listed") && err.contains("10-b.css"), "{err}");

        write(
            dir.path(),
            "static/app/manifest.txt",
            "00-a.html\n10-b.css\n90-gone.js\n",
        );
        let err = assemble(dir.path()).unwrap_err();
        assert!(err.contains("do not exist") && err.contains("90-gone.js"), "{err}");
    }

    #[test]
    fn assemble_fails_on_cross_fragment_eval_order_hazard() {
        // Integration of the eval-order lint (eval_order.rs, where the
        // scanner-level cases live): a top-level reference to a later
        // fragment's `let` must fail assembly, not ship a dashboard that
        // dies at module evaluation.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "static/app/30-open.html", "<script type=\"module\">\n");
        write(dir.path(), "static/app/40-a.js", "if (laterFlag) { console.log(1); }\n");
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
        write(dir.path(), "static/app/50-b.js", "console.log(laterFlag);\n");
        assemble(dir.path()).unwrap();
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

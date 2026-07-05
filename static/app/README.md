# static/app/ — dashboard SPA source

`static/app.html` (the file the daemon embeds via `include_str!` and
intendant-connect serves from disk) is **generated**: `build.rs` concatenates
the fragments in this directory, in the order fixed by `manifest.txt`, via
`crates/app-html-assembler`. Any `cargo build`/`check` reassembles it, and
CI (`.github/workflows/app-html.yml`) fails when the committed artifact
doesn't match the committed fragments.

Rules:

- **Edit fragments, never `static/app.html`.** A hand-edit to the artifact is
  overwritten by the next build and rejected by CI.
- The transform is concatenation plus a generated header and one banner
  comment per fragment — nothing else. All `.js` fragments share the single
  `<script type="module">` scope (the tags live in the `*-open.html` /
  `*-close.html` wrappers), so declaration order across fragments matters
  exactly as it did in the monolith: **manifest order is program order**.
- Every `.css`/`.js`/`.html` file here must be listed in `manifest.txt`
  exactly once; the build fails otherwise — nothing is silently dropped.
- Numeric filename prefixes are cosmetic (they keep `ls` readable);
  `manifest.txt` is the only authority on order.
- Keep a fragment under ~3k lines. When one outgrows the budget, split it at
  a natural boundary — a pure move plus a manifest edit.
- **Merge conflicts:** resolve them in the fragments, run
  `cargo run -p app-html-assembler`, then `git add static/app.html`. Never
  hand-reconcile the generated file.
- **Fast iteration** (skip the daemon rebuild): launch the daemon with
  `INTENDANT_APP_HTML_PATH=$PWD/static/app.html` and the gateway re-reads
  that file on every request — edit a fragment, run
  `cargo run -p app-html-assembler`, refresh the browser. WASM and vendored
  assets stay embedded; those still need a normal build.

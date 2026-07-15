# Execution lanes — plan and estimates

Status: **planning document** (repair-tranche item: a concrete plan
with estimates for the §13.2 surfaces that have never executed the
corpus). Today exactly two surfaces execute it — the Rust core
(minting + drift/coverage gates) and the independent Rust reducer
(the differential harness); CI runs both as the advisory
`owner-plane reference artifacts (Rust core + reducer)` job. A
vector's `surfaces` array is a §13.2 applicability annotation and is
never treated as execution (`coverage/outcomes-map.json` and
`coverage/obligations-13-3.json` both state this).

## Lane 1 — Chromium browser execution (`browser`)

**What it proves.** The §13.2 `browser` column: WebCrypto
(Ed25519/P-256/ECDH/HPKE composition, AES-GCM, HKDF/PBKDF2) agrees
with the Rust suites on families 1–5, 8, 12–13's browser-required
rows, and the IndexedDB Txn subset executes the family-13 journal
shapes. The in-schema guard already reflects one real limit: family-3
sign-then-verify cases exclude `browser` (WebCrypto cannot inject
signing randomness), so the lane runs verify-only there.

**Shape.** A `wasm32-unknown-unknown` build of the reducer's lane
code (the crate is `serde`/`ring`-free by construction on the paths
that matter; the crypto calls route through a small trait already
separable from the lane logic), packaged with `wasm-pack`, driven by
the repo's existing CDP harness pattern (`scripts/
validate-dashboard.cjs` precedent: launch headless Chromium, load a
fixture page, stream per-vector verdicts over the console protocol,
compare against the same `all_green` predicate the CLI harness
uses). The fixture page fetches the committed `vectors/` corpus and
the companion, runs every browser-annotated vector, and reports the
identical report rows the Rust harness prints.

**Work items.**
1. Extract the reducer's crypto calls behind a `Crypto` trait with a
   WebCrypto (`web-sys`) implementation — the KAT module is the only
   place raw primitives are invoked (~1 session).
   **DONE (2026-07-15), seam + scaffold:** `reducer/src/crypto.rs`
   carries the maybe-async trait (the §13.2 browser primitive set:
   digest, Ed25519/P-256 verify with the low-S policy, point/scalar
   ops, HPKE open, AES-GCM, HKDF-SHA256, PBKDF2-HMAC-SHA512, Ed25519
   seed→pk) with `NativeCrypto` on the existing crates and
   `block_on_ready` for the sync CLI path; the browser-required KAT
   lanes (families 1–5, 8) are generic over it, and the engine lanes
   stay direct per §13.2's family matrix (their browser cell is the
   IndexedDB Txn subset, not WebCrypto). `browser-lane/` compiles the
   schema-less reducer for wasm32-unknown-unknown (`schema` feature
   default-on; jsonschema→ahash→getrandom-0.3 cannot build there;
   `getrandom/js` unified for the p256/hpke stack) and exposes
   `run_vector` (dep-free structural layers + semantics). The
   SubtleCrypto backend is a DELIBERATE unwired stub — every
   primitive errors, so crypto vectors report FAIL and nothing can
   green-wash before item 2's driver actually runs a browser; the
   backend gets written WITH that driver, which is the only honest
   way to test it.
2. `wasm-pack` packaging + the fixture page + CDP driver reusing the
   validate-dashboard launch/scrape recipe (~1 session) — plus the
   SubtleCrypto backend the scaffold stubs (digest, importKey+verify,
   ECDH `deriveBits` composed into HPKE per RFC 9180, AES-GCM
   encrypt/decrypt, HKDF/PBKDF2 deriveBits; WebCrypto ECDSA does not
   enforce low-S, so the wasm side pre-checks `s ≤ n/2` on the raw
   signature bytes before verifying).
   **DONE (2026-07-15), first green run 56/56:** `browser-lane/src/
   webcrypto.rs` implements the whole backend on `crypto.subtle`
   (labeled HKDF for the RFC 9180 schedule built on `sign(HMAC)`;
   PKCS#8-wrapped scalar/seed imports with JWK-export public-key
   recovery; the low-S pre-check against ⌊n/2⌋; an RFC 8032 TEST-1
   probe distinguishing "no Ed25519 in this browser" = backend `Err`
   from "malformed key" = semantic `false`). `fixture/index.html` +
   `driver.cjs` (launch/WebSocket mechanics after the CI-proven
   scripts/smoke-dashboard-boot.cjs; no npm deps) serve the corpus
   over loopback, run every browser-annotated vector in
   `--headless=new` Chromium over raw CDP, and gate on the all_green
   shape; verified green (56/56 under HeadlessChrome 150 — the
   high-S rejection vector passing proves the low-S pre-check fires)
   AND red (a tampered-corpus negative control via `LANE_VECTORS_DIR`
   exits 1 naming the row). The advisory workflow carries the job as
   `browser execution (Chromium; f13 in-memory, IndexedDB shim
   pending)` — the name keeps item 3's gap visible.
3. IndexedDB Txn-subset shim for the family-13 journal lane
   (transaction boundaries mapped to the Txn frames; L1 truncation
   simulated at the fixture layer) (~1–2 sessions).
   **DONE (2026-07-15):** the fixture's substrate layer mirrors the
   storage lane's shape on the browser substrate — every f13 hex
   input round-trips through a real IndexedDB record with ONE put
   per transaction (the journal's atomic unit mapped onto IDB's);
   streams are stored frame-per-record at the REAL frame boundaries
   the reducer's walker reports (exported to the fixture as
   `frame_spans`), with ordered read-back equality; crash cuts are
   simulated at the fixture layer as row-level truncation (whole
   frames below the cut + the torn tail slice) and the ordered
   read-back must equal the in-memory prefix; the lock matrix runs
   over `navigator.locks` (exclusive + `ifAvailable` denial) with
   Web Workers as the other actors, the denied loser proving the
   STORE stays readable via an IndexedDB read, and the releaser
   awaiting its request promise so a release can never race the next
   acquire. The driver gates substrate rows alongside semantics and
   fails on a zero frames/cuts aggregate (a disengaged mapping
   cannot green-wash); a flipped lock-denial-step negative control
   goes red in BOTH layers independently (the reducer's in-memory
   lane and the real Web Locks denial). Clean run: 56/56 with 16
   substrate vectors (records=37, bytes=30 781, frames=72, cuts=11).

**STATUS: DELIVERED (2026-07-15) — all three work items.** The
advisory job is now plainly `browser execution (Chromium)`; the
Gate-B concerns (IndexedDB failure injection/eviction, engine
matrices, quota) remain out by design, per the section below.

**Estimate.** 2–4 working sessions. **Exit criterion.** The CDP
driver exits nonzero unless every browser-annotated vector reports
`semantics=PASS` under Chromium, wired into the advisory workflow as
a separate accurately-named job (`browser execution (Chromium)`).

## Lane 2 — per-OS portable storage (`storage-macos/linux/windows`)

**What it proves.** The §13.2 storage column for families 13–14: the
zone-log framing, crash-cut truncation (L1), the erase-crash matrix,
and the cross-process lock behave identically over each OS's real
file and locking primitives — the PORTABLE subset (open/write/rename
/flock-equivalents), not production durability.

**Shape.** A small storage harness binary in the reducer crate
(`--bin storage-lane`) that materializes each family-13/14 vector's
stream into a temp file, performs the cuts as real truncations,
replays through the SAME lane code the CLI harness uses, and runs
the lock-matrix vector with two real processes. The fleet already
runs all three OSes as CI runners; the job rides the advisory
workflow with a 3-OS matrix (hosted runners suffice — the corpus is
committed, keyless, displayless).

**Work items.**
1. The `storage-lane` bin: tempdir materialization + truncation +
   the two-process lock rig (~1 session).
2. The 3-OS matrix job + Windows path/locking quirks shakedown
   (~0.5–1 session).

**Estimate.** 1–2 working sessions. **Exit criterion.** All three
legs green on the full family-13/14 subset, job named
`portable storage execution (macOS/Linux/Windows)`.

**STATUS: DELIVERED (2026-07-15).** `reducer --bin storage_lane`
executes all 19 storage-annotated vectors on real files (byte
round-trips, real `set_len` truncations per crash cut, the lock
matrix across two real processes on `std` advisory locks), then
runs the unmodified harness semantics; the advisory workflow
carries the 3-OS matrix job. Chromium (lane 1) remains the open
funded lane.

## Explicitly Gate-B (distinguishable production concerns)

Per the repair-tranche scope ruling these stay OUT of the lanes
above and are named so a green lane is never misread as covering
them: production fsync/durability ordering, OS keystore custody,
IndexedDB failure injection and eviction behavior, Firefox and
Safari engines, and storage-quota pressure. Each needs
fault-injection or engine matrices the reference artifacts cannot
honestly simulate.

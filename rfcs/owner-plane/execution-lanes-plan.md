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
2. `wasm-pack` packaging + the fixture page + CDP driver reusing the
   validate-dashboard launch/scrape recipe (~1 session).
3. IndexedDB Txn-subset shim for the family-13 journal lane
   (transaction boundaries mapped to the Txn frames; L1 truncation
   simulated at the fixture layer) (~1–2 sessions).

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

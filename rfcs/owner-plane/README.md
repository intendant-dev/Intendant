# Owner Plane — D0-A Core + Memory

The Owner Plane specification program: the encrypted, owner-governed
memory/agenda substrate (umbrella RFC) and its first gate document,
**D0-A Core + Memory**, developed 2026-07-11 → 2026-07-13 through
twenty externally-reviewed revisions (decision record D-1..D-200).

## Layout

| Path | What |
|---|---|
| `agenda-owner-plane-rfc.md` | The umbrella RFC, **v3.1 — FROZEN** (changes belong in gate specs, never here) |
| `owner-plane-d0a-spec.md` | **D0-A, v0.5.20 — the terminal cut plus the owner's ratification amendments** (D-201..D-203; SHA-256 `ec3a9a6dda8f8c839b6c6eb7fb3322b439bf3976a8cd8ac0f6297838102dedef`; v0.5.19 archived byte-exact). Behavioral findings enter only with a failing executable trace (D-200); owner rulings enter as decision rows |
| `d0a-vector-cases.v1.json` | The **normative companion schema** (D-91): closed per-family `case_kind` vocabularies + exact per-kind input/result contracts. A vector is valid only if it passes BOTH this and the spec's §13.1 container schema |
| `archive/` | Every as-reviewed draft, byte-exact (v0.1 → v0.5.19) — the red baselines |
| `reviews/` | The full review record: per-revision peer review(s) + adjudicated syntheses |
| `core/` | The reference core (canonical CBOR, hash domains, vector RNG, suite-v1 crypto) — the fixture-minting implementation; the independent reducer must not share its code |
| `vectors/` | Committed vector fixtures (`f{family:02}-{name}.json`), minted by `cargo run --bin mint` in `core/`; a drift-gate test pins these bytes to the builders — edit builders, never these files |
| `reducer/` | The **independent reducer + differential harness** — shares NO code with `core/` (own strict CBOR reader, domains, envelope, fold engine incl. the D-138 control re-fold, journal machine, erase-crash replayer, edge predicate, an independent BIP-39 leg, B.2/B.3 transcription). `cargo run --bin harness` is a real gate: nonzero exit on any structural failure, semantic FAIL, or Unimplemented. The full 157-vector corpus is reproduced; `--bin storage_lane` executes the storage-annotated subset on real files (the delivered per-OS lane) |
| `browser-lane/` | The **browser execution lane** (delivered): the schema-less reducer + a WebCrypto backend compiled to wasm; `driver.cjs` runs every browser-annotated vector in headless Chromium over raw CDP — semantics via `crypto.subtle`, the family-13 substrate over real IndexedDB transactions + Web Locks — and exits nonzero unless all green |
| `gate-a-audit.md` | **The Gate-A discrepancy audit, amended after the repair tranche** (2026-07-14): the differential scoreboard incl. the tranche's findings (a real D-185 engine gap among them), the twelve D-items with per-item status, the conventions, the machine-enforced coverage pointers — and the verdict: **predicate satisfied, awaiting the owner's Gate-A stamp** (amended 2026-07-15 when the browser lane delivered; this document never self-stamps a PASS) |
| `coverage/` | The **machine-enforced coverage inventory**: `outcomes-map.json` (generated §10.4 outcome → vector map; 22/59 uncovered, pinned shrink-only) and `obligations-13-3.json` (the §13.3 obligation ledger — verbatim quote pins + full line coverage of the section; 14 vectored / 25 partial / 43 pending) |
| `decisions-pending.md` | The **D2/D5 decision record — both RULED 2026-07-14** (D-201 no-class/no-vote; D-202 sticky + re-proposal): the alternatives, authority consequences, and the drafts the rulings chose from, now minted |
| `p1-v1-profile.md` | The **P1 v1 profile — RATIFIED as drafted (D-203)**: five implement-before-Gate-A mechanisms; every other unimplemented normative mechanism fail-closed with a named outcome |
| `execution-lanes-plan.md` | The **execution-lanes plan — BOTH lanes DELIVERED** (per-OS portable storage 2026-07-15; Chromium WebCrypto/IndexedDB 2026-07-15); the Gate-B production concerns stay named and excluded |

## Provenance note

These documents were authored at `~/` and moved here byte-identical
on 2026-07-13. Internal path references (`~/owner-plane-d0a-spec.md`,
`~/agenda-rfc-archive/…`, `~/agenda-owner-plane-rfc.md`) are
historical; basenames are unchanged, so they map 1:1 into this
directory (`archive/` for the archive glob). The spec was NOT edited
for the move — its hash is pinned by the companion schema and the
review record, and the byte-exact baseline outranks path cosmetics.

## Status

- **Gate A: FAIL — not stamped** (`gate-a-audit.md` §5). The repair
  tranche closed the artifact-side clauses (strict gate, real
  convergence + the D-185 fix, D1/D4/D6, the verified reopen-kill
  trace, machine-enforced coverage, advisory CI), and the owner's
  2026-07-14 rulings are now recorded as spec v0.5.20's D-201..D-203:
  D2 = no class / no vote (pinned by the two `bare-daemon-*-inert`
  vectors), D5 = sticky rejection + writer re-proposal (the
  late/timely endpoint pair), the D3/D7/D8/D10/D12 prose
  ratifications, D11 recorded-open, the §4.7 wire gap shelved for
  v1, the P1 v1 profile RATIFIED, the coverage scope line RATIFIED
  (cheap §10.4 gaps close pre-Gate-A; the ceremony sagas defer to
  Gate B as recorded decisions), and the execution lanes funded.
  The corpus stands at 157 vectors, every one reproduced by the
  independent reducer.
- **Post-ruling execution (2026-07-15)**: all five §C.1
  implement-before-Gate-A mechanisms are implemented + vectored
  (erase-manifest admission, compromise-mode T4 with derived-lane
  retro-disqualification, rotation_refs linkage, D-93/D-143
  frontier-head validation, the re-fold invariant); the cheap-gap
  batch closed ten §10.4 outcomes (22 → 12, the rest explicit
  test-tied Gate-B deferrals); and the **portable-storage execution
  lane is DELIVERED** — every storage-annotated vector runs on real
  files with real truncations and a real cross-process lock, on a
  3-OS advisory CI matrix.
- **The Chromium browser lane is DELIVERED (2026-07-15)** — every
  browser-annotated vector executes in headless Chromium (WebCrypto
  semantics; the family-13 IndexedDB Txn + Web Locks substrate),
  green locally and on CI with negative controls verified red. With
  it the audit's twelve-clause predicate is fully satisfied: **no
  open artifact item remains, and the Gate-A decision rests with
  the owner** (the freeze-time prose ratifications + the §16 stamp).
  No owner decision besides the gate itself is outstanding.
- **Durable P1 Memory writes stay prohibited** until Gate B plus the
  umbrella's P0.5/tombed-cutover prerequisites (spec header) —
  independent of Gate A.

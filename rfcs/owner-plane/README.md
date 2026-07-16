# Owner Plane — D0-A Core + Memory

The Owner Plane specification program: the encrypted, owner-governed
memory/agenda substrate (umbrella RFC) and its first gate document,
**D0-A Core + Memory**, developed 2026-07-11 → 2026-07-13 through
twenty externally-reviewed revisions (decision record D-1..D-200).

## Layout

| Path | What |
|---|---|
| `agenda-owner-plane-rfc.md` | The umbrella RFC, **v3.1 — FROZEN** (changes belong in gate specs, never here) |
| `owner-plane-d0a-spec.md` | **D0-A, v0.5.22 — the terminal cut plus the owner's ratification amendments** (D-201..D-205; SHA-256 `30c91f941da7ba3458ed4886a5fab5a6be991703b7802668be56a4b8d531f5ef`; v0.5.19 through v0.5.21 archived byte-exact; D-204 narrows D-202's convergence carrier to shared evidence-arrival structure; D-205 — the ff23f1cd round's delegated adjudication — completes it with the self-evidence exception to fork registration). Behavioral findings enter only with a failing executable trace (D-200); owner rulings enter as decision rows |
| `d0a-vector-cases.v1.json` | The **normative companion schema** (D-91, amendments #1–#7; #7 = the machine-readable `evidence_class` on evidence-lifecycle vectors): closed per-family `case_kind` vocabularies + exact per-kind input/result contracts. A vector is valid only if it passes BOTH this and the spec's §13.1 container schema |
| `archive/` | Every as-reviewed draft, byte-exact (v0.1 → v0.5.19) — the red baselines |
| `reviews/` | The full review record: per-revision peer review(s) + adjudicated syntheses |
| `core/` | The reference core (canonical CBOR, hash domains, vector RNG, suite-v1 crypto) — the fixture-minting implementation; the independent reducer must not share its code |
| `vectors/` | Committed vector fixtures (`f{family:02}-{name}.json`), minted by `cargo run --bin mint` in `core/`; a drift-gate test pins these bytes to the builders — edit builders, never these files |
| `reducer/` | The **independent reducer + differential harness** — shares NO code with `core/` (own strict CBOR reader, domains, envelope, fold engine incl. the D-138 control re-fold, journal machine, erase-crash replayer, edge predicate, an independent BIP-39 leg, B.2/B.3 transcription). `cargo run --bin harness` is a real gate: nonzero exit on any structural failure, semantic FAIL, or Unimplemented. The full 168-vector corpus is reproduced; `--bin storage_lane` executes every storage-annotated vector on real files (the delivered per-OS lane: all streams through the durable flush+rename path onto pre-seeded destinations, plus the flush failpoint control) |
| `browser-lane/` | The **browser execution lane** (delivered): the schema-less reducer + a WebCrypto backend compiled to wasm; `driver.cjs` runs every browser-annotated vector in headless Chromium over raw CDP — semantics via `crypto.subtle`, the family-13 substrate over real IndexedDB transactions + Web Locks — and exits nonzero unless all green |
| `gate-a-audit.md` | **The Gate-A discrepancy audit, amended after the repair tranche** (2026-07-14): the differential scoreboard incl. the tranche's findings (a real D-185 engine gap among them), the twelve D-items with per-item status, the conventions, the machine-enforced coverage pointers — and the **FAIL verdict** (re-amended 2026-07-15 on the reconciled verification review — the interim predicate-satisfied claim is withdrawn — and 2026-07-16 after the criterion-12 tranche: D-99 body-before-placement in full, the D-130 exact-reference rule, both D-202 worlds vectored, the storage proof made discriminating) |
| `coverage/` | The **machine-enforced coverage inventory**: `outcomes-map.json` (generated §10.4 outcome → vector map; 12/59 uncovered — explicit Gate-B deferrals, pinned shrink-only) and `obligations-13-3.json` (the §13.3 obligation ledger — verbatim quote pins + full line coverage of the section; 14 vectored / 26 partial / 42 pending / 2 structural). Both declare the six executed surfaces (2 Rust + Chromium + 3 storage OSes) |
| `decisions-pending.md` | The **decision record**: D2/D5 RULED 2026-07-14 (D-201 no-class/no-vote; D-202 sticky + re-proposal) and D6 RULED 2026-07-16 (D-204: the criterion-12 D-202 convergence narrowing, folded into spec v0.5.21) — the alternatives and drafts preserved as chosen-from records |
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
  The corpus stands at 168 vectors, every one reproduced by the
  independent reducer.
- **Post-ruling execution (2026-07-15)**: the §C.1
  implement-before-Gate-A mechanisms are executed — rows one
  through four implemented + vectored (erase-manifest admission,
  compromise-mode T4 with derived-lane retro-disqualification,
  rotation_refs linkage, D-93/D-143 frontier-head validation), row
  five as the internal replay invariant the profile records; the
  cheap-gap
  batch closed ten §10.4 outcomes (22 → 12, the rest explicit
  test-tied Gate-B deferrals); and the **portable-storage execution
  lane is DELIVERED** — every storage-annotated vector runs on real
  files with real truncations and a real cross-process lock, on a
  3-OS advisory CI matrix.
- **The Chromium browser lane is DELIVERED (2026-07-15)** — every
  browser-annotated vector executes in headless Chromium (WebCrypto
  semantics; the family-13 IndexedDB Txn + Web Locks substrate),
  green locally and on CI with negative controls verified red.
- **The 2026-07-15 reconciled verification review returned FAIL**
  (`reviews/2026-07-15-gate-a-verification-reconciled-review.md`):
  the reducer is not order-convergent on legal unlisted delivery
  orders, the D-99 control pipeline order is violated, Journal
  reopen kills accept unauthenticated evidence, audit-partition
  exactness self-references, lane manifests are shrinkable, the
  storage lane omits flush/replacement, and D-202's lifecycle is
  unexecuted. The interim "predicate satisfied" audit claim is
  withdrawn; the **bounded repair tranche is EXECUTED** (owner
  directive 2026-07-15, both optional items built): the canonical
  set-derived fold makes order convergence structural (all eight
  review orders committed as regressions + a metamorphic order
  suite + an arrival-order restoration control), the D-99 pipeline
  order and the D-130 exact-reference rule are implemented (unheld
  named heads pend; full two-variant fork selection stays honestly
  deferred), Journal reopen
  kills verify against authenticated ADMITTED facts, audit-partition
  release derives from an independent read-release event, both
  execution-lane run sets pin to `coverage/lane-manifests.json`, the
  storage lane executes real flush + atomic replacement with
  invocation proof, and the D-202 lifecycle is executable
  (evidence-lifecycle lane; stickiness probed non-vacuous).
- **The 2026-07-15 criterion-12 review round returned FAIL again**
  (two independent reviews + their synthesis, filed in `reviews/`)
  with three executable protocol counterexamples the first round's
  case selection had not reached, and the **criterion-12 tranche is
  EXECUTED** (owner directive 2026-07-15): the COMPLETE control body
  stage — hash, registry row, arm-indexed intrinsic CDDL — now
  precedes replay and placement with the two multi-fault regressions
  committed (F1); `parse_heads` selects only genuinely held
  byte-variants and an unheld named head pends `ref-unresolved`,
  with the f07 fixture re-authored to the pend expectation and full
  fork selection honestly deferred (F2); BOTH ruled D-202 evidence
  worlds are vectored with the cross-world divergence pinned from
  one byte source, and the convergence-promise narrowing is RATIFIED
  (D-204, owner 2026-07-16, folded into spec v0.5.21) (F3); the storage proof now covers every stream, replaces
  pre-seeded destinations, and couples the flush observation to a
  failpoint control that the review's own mutation turns red (F4);
  and this round's truth pass re-verified counts, comments, and
  claims across the program docs (criterion 11). The next verdict is
  a FRESH review's to make — this repo never self-stamps.
- **The ff23f1cd fresh review returned FAIL** (five findings, filed
  as `reviews/2026-07-16-gate-a-ff23f1cd-review.md`) and its repairs
  are EXECUTED under the owner's delegated adjudication of that
  round: the **D-205 self-evidence exception** (spec v0.5.22 — a
  late-class original arriving after its re-proposal self-classifies
  sticky `lease-stale` at the occupied coordinate and never freezes
  its own convergence carrier; the re-proposal-first order rides the
  late-first vector as its third listed delivery, and each
  evidence-lifecycle vector declares a machine-readable
  `evidence_class`, companion amendment #7); **closed CDDL key
  sets** across every dispatched control arm and nested shape
  (App-A-verbatim tables; the extra-field multi-fault regression
  committed); **registry dispatch on all three coordinates** (an
  `operation_version != 1` op rejects `unknown-version` before CDDL,
  replay, and placement; vectored); the **storage proof hardened**
  (counter equality against the corpus-derived stream count,
  read-back-verified pre-seeded replacement, the sync seam's limit
  stated honestly — all three review mutations verified red); and
  the criterion-11 leftovers swept. Verdict unchanged: FAIL until a
  fresh review reports no executable finding.
- **Durable P1 Memory writes stay prohibited** until Gate B plus the
  umbrella's P0.5/tombed-cutover prerequisites (spec header) —
  independent of Gate A.

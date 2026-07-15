# Owner Plane — D0-A Core + Memory

The Owner Plane specification program: the encrypted, owner-governed
memory/agenda substrate (umbrella RFC) and its first gate document,
**D0-A Core + Memory**, developed 2026-07-11 → 2026-07-13 through
twenty externally-reviewed revisions (decision record D-1..D-200).

## Layout

| Path | What |
|---|---|
| `agenda-owner-plane-rfc.md` | The umbrella RFC, **v3.1 — FROZEN** (changes belong in gate specs, never here) |
| `owner-plane-d0a-spec.md` | **D0-A, v0.5.19 — the terminal prose cut** (SHA-256 `410880e04433b629d5d11956e322f59832494d8f25042b3dfcf34d8b694c6748`). From here, behavioral findings enter only with a failing executable trace (D-200) |
| `d0a-vector-cases.v1.json` | The **normative companion schema** (D-91): closed per-family `case_kind` vocabularies + exact per-kind input/result contracts. A vector is valid only if it passes BOTH this and the spec's §13.1 container schema |
| `archive/` | Every as-reviewed draft, byte-exact (v0.1 → v0.5.18) — the red baselines |
| `reviews/` | The full review record: per-revision peer review(s) + adjudicated syntheses |
| `core/` | The reference core (canonical CBOR, hash domains, vector RNG, suite-v1 crypto) — the fixture-minting implementation; the independent reducer must not share its code |
| `vectors/` | Committed vector fixtures (`f{family:02}-{name}.json`), minted by `cargo run --bin mint` in `core/`; a drift-gate test pins these bytes to the builders — edit builders, never these files |
| `reducer/` | The **independent reducer + differential harness** — shares NO code with `core/` (own strict CBOR reader, domains, envelope, fold engine incl. the D-138 control re-fold, journal machine, erase-crash replayer, edge predicate, an independent BIP-39 leg, B.2/B.3 transcription). `cargo run --bin harness` is a real gate: nonzero exit on any structural failure, semantic FAIL, or Unimplemented. The full 143-vector corpus is reproduced |
| `gate-a-audit.md` | **The Gate-A discrepancy audit, amended after the repair tranche** (2026-07-14): the differential scoreboard incl. the tranche's findings (a real D-185 engine gap among them), the twelve D-items with per-item status, the conventions, the machine-enforced coverage pointers — and the **FAIL verdict** with the amended twelve-clause predicate |
| `coverage/` | The **machine-enforced coverage inventory**: `outcomes-map.json` (generated §10.4 outcome → vector map; 22/59 uncovered, pinned shrink-only) and `obligations-13-3.json` (the §13.3 obligation ledger — verbatim quote pins + full line coverage of the section; 14 vectored / 25 partial / 43 pending) |
| `decisions-pending.md` | The **open owner decisions** (D2 bare-writer actor class; D5 late-receipt lifecycle): alternatives, authority consequences, discriminating vector drafts — nothing chosen |
| `p1-v1-profile.md` | The **P1 v1 profile** (recommendations pending ratification): every unimplemented normative mechanism dispositioned implement+vector or fail-closed with a named outcome |
| `execution-lanes-plan.md` | The **execution-lanes plan**: Chromium (WebCrypto/IndexedDB) and per-OS portable-storage lanes with estimates; the Gate-B production concerns named |

## Provenance note

These documents were authored at `~/` and moved here byte-identical
on 2026-07-13. Internal path references (`~/owner-plane-d0a-spec.md`,
`~/agenda-rfc-archive/…`, `~/agenda-owner-plane-rfc.md`) are
historical; basenames are unchanged, so they map 1:1 into this
directory (`archive/` for the archive glob). The spec was NOT edited
for the move — its hash is pinned by the companion schema and the
review record, and the byte-exact baseline outranks path cosmetics.

## Status

- **Gate A: FAIL — not stamped** (`gate-a-audit.md` §5, amended
  after the repair tranche). The tranche closed the artifact-side
  clauses: a strict harness gate that really goes red, real ≥2-order
  convergence (which exposed and fixed a genuine D-185 journal
  reservation gap), the D1 BIP-39 checksum negative, D4
  signature-before-freeze, the D6 de-oracled erase lane (signed
  rotation ops as the control context), the verified reopen-kill
  trace with both arms, the machine-enforced coverage inventory, and
  the advisory CI workflow. The corpus stands at 143 vectors, every
  one reproduced by the independent reducer.
- **Open before any PASS**: the execution lanes (Chromium + per-OS
  storage — planned, never run), ratification of the P1 v1 profile,
  and the owner rulings (D2, D5, the prose items D3/D7/D8/D10/D12,
  D11's recorded acceptance, and the §4.7 fork-discovery wire gap).
  See `decisions-pending.md` and the audit's §5 predicate.
- **Durable P1 Memory writes stay prohibited** until Gate B plus the
  umbrella's P0.5/tombed-cutover prerequisites (spec header) —
  independent of Gate A.

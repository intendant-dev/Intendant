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
| `reducer/` | The **independent reducer + differential harness** — shares NO code with `core/` (own strict CBOR reader, domains, envelope, fold engine, journal machine; `cargo run --bin harness` reports per-vector status). The opening tranche is fully reproduced (8/8; the burn-down already caught and fixed one fixture-minting defect — the erase fixture's flowless release) |

## Provenance note

These documents were authored at `~/` and moved here byte-identical
on 2026-07-13. Internal path references (`~/owner-plane-d0a-spec.md`,
`~/agenda-rfc-archive/…`, `~/agenda-owner-plane-rfc.md`) are
historical; basenames are unchanged, so they map 1:1 into this
directory (`archive/` for the archive glob). The spec was NOT edited
for the move — its hash is pinned by the companion schema and the
review record, and the byte-exact baseline outranks path cosmetics.

## Status

- **Gate A: pending — currently false** (spec §16). The companion,
  the opening tranche (8 vectors, all reproduced), the independent
  reducer, and the differential harness exist; the corpus
  (families 1–13), the family-14 run, the surface runs, and the
  final prose↔CDDL↔companion↔vector discrepancy audit do not yet.
- **Durable P1 Memory writes stay prohibited** until Gate B plus the
  umbrella's P0.5/tombed-cutover prerequisites (spec header).
- Next: the corpus (families 1–13) via `core/`, burned down through
  the same harness; then family 14, surfaces, and the Gate-A audit
  (which also sweeps the interpretation register kept in the
  program ledger).

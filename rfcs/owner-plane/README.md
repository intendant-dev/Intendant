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
| `core/` | The reference core (canonical CBOR, hash domains, vector RNG) — the fixture-minting implementation |

## Provenance note

These documents were authored at `~/` and moved here byte-identical
on 2026-07-13. Internal path references (`~/owner-plane-d0a-spec.md`,
`~/agenda-rfc-archive/…`, `~/agenda-owner-plane-rfc.md`) are
historical; basenames are unchanged, so they map 1:1 into this
directory (`archive/` for the archive glob). The spec was NOT edited
for the move — its hash is pinned by the companion schema and the
review record, and the byte-exact baseline outranks path cosmetics.

## Status

- **Gate A: pending — currently false** (spec §16). The companion
  exists; the corpus, independent reducer, differential harness,
  family-14 run, surface runs, and the final prose↔CDDL↔companion↔
  vector discrepancy audit do not yet.
- **Durable P1 Memory writes stay prohibited** until Gate B plus the
  umbrella's P0.5/tombed-cutover prerequisites (spec header).
- Next: mint the red-fixture opening tranche (listed in the
  companion's `x-informative` annex) via `core/`, then the
  independent reducer and differential harness.

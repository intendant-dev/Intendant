# Task: salestream ETL pipeline

You are working in the `salestream` repository (your current directory). It is
a small ETL pipeline that normalizes raw sales-CSV exports to JSONL, merges and
deduplicates them across regions, and produces a summary report. The repo
compiles and its scaffolding is in place, but the three tools are unimplemented
stubs and their tests fail.

## Your goal

Make the whole repository pass. Concretely:

1. `make test` passes (all three components' tests succeed).
2. `make pipeline RAW=<dir> OUT=<dir>` runs end to end on a directory of CSV
   files and writes `<OUT>/normalized/*.jsonl`, `<OUT>/merged.jsonl`, and
   `<OUT>/report.json`.

## The three components are independent

The repository has three components, each in its own directory with its own
authoritative spec and its own starter tests:

- `normalizer/` — `normalize.py`, a Python CSV→JSONL normalizer. Spec:
  `normalizer/SPEC.md`.
- `dedup/` — a Rust merge/dedupe tool with a documented conflict policy. Spec:
  `dedup/SPEC.md`.
- `report/` — `report.sh`, a bash+jq report generator. Spec: `report/SPEC.md`.

They share only the one-line JSON record schema described in `README.md`. They
do not import or call each other's source; each can be built and tested on its
own. Read each component's `SPEC.md` carefully — the edge-case rules
(amount/date parsing, the dedupe conflict policy, tie-breaks, rounding) are
precise and are what the tests check.

## Rules

- Implement to the specs. Do not edit the test files, the `Makefile`, the
  `SPEC.md` files, or `README.md`; implement the three tools
  (`normalizer/normalize.py`, `dedup/src/main.rs`, `report/report.sh`) and add
  supporting source if you wish.
- Allowed dependencies: Python standard library only for the normalizer; the
  `dedup` crate may use the `serde_json` crate already in its `Cargo.toml`
  (run `cargo fetch` in `dedup/` if the registry is reachable; the
  `Cargo.lock` is committed); `report.sh` may use only `bash` and `jq`.
- No network access is required or expected at runtime.

## How you are evaluated

A held-back grader runs each component against freshly generated inputs and an
independent reference implementation of each spec, then runs the full
`make pipeline` end to end. Partial credit is awarded per component plus a
bonus for the end-to-end pipeline, so a correct component counts even if
another is incomplete. Correctness on the spec's edge cases is the whole game.

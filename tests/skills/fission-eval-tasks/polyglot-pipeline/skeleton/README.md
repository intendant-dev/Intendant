# salestream

A small ETL pipeline for sales-transaction exports. Raw CSV exports from
several regional systems are normalized to JSONL, merged and deduplicated
across regions, and summarized into a report.

## Components

The pipeline is three components. **They are independent**: they share only
the record schema below, never import each other's code, and can be built and
tested in any order. Each directory has its own `SPEC.md` (the authoritative
contract for that component) and its own starter tests (currently failing).

| Directory | Tool | Language |
|---|---|---|
| `normalizer/` | `normalize.py` — CSV → JSONL normalizer | Python 3 (stdlib) |
| `dedup/` | `dedup` — JSONL merge/dedupe with a conflict policy | Rust (serde_json is in Cargo.toml) |
| `report/` | `report.sh` — JSONL → summary report | bash + jq |

## Record schema (shared contract)

One JSON object per line (JSONL). Produced by the normalizer, consumed by
dedup, whose output is consumed by the report.

```json
{"id": "<non-empty string>",
 "name": "<string>",
 "email": "<lowercase string with one @>" ,
 "amount": 1234.5,
 "date": "YYYY-MM-DD",
 "tags": ["<string>", "..."]}
```

`email` may be `null`. `amount` is a JSON number. `tags` is a (possibly
empty) array of strings, sorted ascending with no duplicates. JSON key order
within a line does not matter.

## Make targets (provided — already correct, you should not need to edit them)

```
make build                      # cargo-build the dedup tool (release)
make test                       # run all three components' starter tests
make pipeline RAW=<dir> OUT=<dir>   # full pipeline:
```

`make pipeline` does, in order:

1. For each `$(RAW)/*.csv` (shell glob order, i.e. sorted by filename):
   `python3 normalizer/normalize.py <csv> $(OUT)/normalized/<name>.jsonl`
2. `dedup/target/release/dedup $(OUT)/normalized/*.jsonl > $(OUT)/merged.jsonl`
   (again sorted glob order)
3. `bash report/report.sh $(OUT)/merged.jsonl > $(OUT)/report.json`

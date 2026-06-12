# report — JSONL → summary report

A `bash` + `jq` script. No other interpreters.

## CLI

```
bash report/report.sh MERGED.jsonl     # JSON report on stdout
```

- One argument: a JSONL file of records (the schema in the repo README).
- Empty/whitespace-only lines are ignored.
- Print exactly one JSON object to stdout. Exit 0 on success; exit non-zero
  on a missing/unreadable argument.
- The input may be empty (zero records) — see the empty case below.

## Report shape

```json
{
  "count": <int: number of records>,
  "total_amount": <number: sum of amount, rounded to 2 decimals>,
  "by_tag": { "<tag>": <int count of records carrying that tag>, ... },
  "top_spenders": [ {"id": "<id>", "amount": <number>}, ... ]
}
```

Field rules:

- `count` — total record count.
- `total_amount` — sum of every record's `amount`, rounded to 2 decimal
  places (half-up). A whole number is still emitted as a number (e.g. `42`
  or `42.5`, never `"42"`).
- `by_tag` — for every tag value that appears in any record's `tags` array,
  the number of records whose `tags` contain it. Tags appearing zero times
  are absent. (Object key order does not matter.)
- `top_spenders` — the records with the highest `amount`, as `{id, amount}`
  objects, sorted by `amount` **descending**; break ties by `id` **ascending**
  (byte order). Include at most 3. If there are fewer than 3 records, include
  all of them. Omit nothing else.

### Empty input

Exactly:

```json
{"count": 0, "total_amount": 0, "by_tag": {}, "top_spenders": []}
```

## Starter test

```
bash report/tests/test_report.sh
```

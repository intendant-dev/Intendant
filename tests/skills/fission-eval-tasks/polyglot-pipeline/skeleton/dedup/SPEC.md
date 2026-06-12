# dedup — JSONL merge/dedupe with a conflict policy

A compiled Rust binary (this crate; `serde_json` is already a dependency and
`Cargo.lock` is committed — no other crates are needed).

## CLI

```
dedup FILE1.jsonl [FILE2.jsonl ...]   # merged output on stdout
```

- At least one file argument; with no arguments print usage to stderr and
  exit 2.
- Exit 0 on success. Exit non-zero on an unreadable file or a line that is
  not a JSON object (inputs in normal operation conform to the record schema
  in the repo README — every line has `id`, `name`, `email`, `amount`,
  `date`, `tags`).
- Skip lines that are empty/whitespace-only.

## Merge semantics

Think of the input as one global sequence of records: files in argument
order, lines in file order. Each record's **position** is its index in that
sequence.

1. Group records by exact `id` string equality.
2. **Conflict policy — pick a winner per group:** the record with the newest
   `date` wins (dates are `YYYY-MM-DD`, so plain string comparison orders
   them). If several records tie on the newest date, the one with the
   **largest position** (latest in the global sequence) wins.
3. The output record for a group is the winner's record **except `tags`**,
   which is the union of the `tags` arrays of *all* records in the group
   (winner and losers alike), deduplicated and sorted ascending (byte order).
4. Every other field (`id`, `name`, `email`, `amount`, `date`) comes from the
   winner only.

## Output

One JSON object per line, **groups sorted by `id` ascending** (byte order).
JSON key order within a line does not matter. `amount` must be emitted as a
JSON number equal to the winner's amount.

## Starter test

```
bash dedup/tests/cli_test.sh     # builds (cargo build --release) and runs the binary
```

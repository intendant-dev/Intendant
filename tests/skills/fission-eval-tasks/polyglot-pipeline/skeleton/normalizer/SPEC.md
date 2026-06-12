# normalizer — CSV → JSONL

## CLI

```
python3 normalizer/normalize.py INPUT.csv OUTPUT.jsonl
```

- Exit 0 on success — including when every row was rejected (an empty output
  file is valid output).
- Exit non-zero only for usage errors or an unreadable input file.
- Accepted records are written to `OUTPUT.jsonl`, one JSON object per line,
  **in input order**. Nothing is required on stdout. You may log a summary to
  stderr; it is not checked.

## Input

UTF-8 CSV with standard double-quote quoting (fields may contain commas,
newlines are not used inside fields, `""` escapes a quote inside a quoted
field — i.e. what Python's `csv` module reads and writes by default).

The first row is the header. It contains exactly the six column names `id`,
`name`, `email`, `amount`, `date`, `tags` — **in any order** (match them after
trimming whitespace and lowercasing the header cells). Columns must be mapped
by header name, not position.

## Per-row rules (apply in this order)

1. **Blank rows:** if every field in the row is empty or whitespace-only,
   skip the row silently (it is neither accepted nor a reject).
2. **Trim:** strip leading/trailing whitespace from every field.
3. **id:** must be non-empty after trimming, else **reject** the row.
   Output as-is.
4. **name:** any string (may be empty). Output as-is (post-trim).
5. **email:** if empty → output JSON `null`. Otherwise lowercase it; it must
   then contain exactly one `@` with at least one character on each side,
   else **reject** the row.
6. **amount:** parse with exactly this algorithm — (a) if it starts with `-`,
   note the sign and drop it; (b) if it now starts with `$`, drop it;
   (c) remove every `,`; (d) what remains must match `^[0-9]+(\.[0-9]{1,2})?$`,
   else **reject** the row. Output the (signed) value as a JSON number.
   Examples: `"$1,234.50"` → `1234.5`, `"-$12"` → `-12`, `"7.25"` → `7.25`;
   `"12.345"`, `"$"`, `"1.2.3"`, `""` → reject.
7. **date:** accept exactly two formats — `YYYY-MM-DD` or `MM/DD/YYYY`
   (both zero-padded, 4-digit year). It must be a real calendar date.
   Output normalized to `YYYY-MM-DD`. Anything else (other separators,
   non-padded, impossible dates like `2025-02-30`) → **reject** the row.
8. **tags:** split the field on `;`, trim each piece, drop empty pieces,
   remove duplicates, sort ascending (plain byte/codepoint order). Output as
   a JSON array of strings (`[]` when the field is empty).

Rejected rows are dropped silently. A rejected row never appears in the
output; rejection of one row does not affect any other row.

## Output record shape

Exactly the keys `id`, `name`, `email`, `amount`, `date`, `tags` (JSON key
order within the object does not matter). See the schema in the repo README.

## Starter test

```
python3 normalizer/tests/test_normalize.py
```

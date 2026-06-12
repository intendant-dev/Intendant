# worker — job processor

Python 3 stdlib only. Two responsibilities: a pure **compute** function (the op
semantics) and a **serve** loop that drives jobs through the API.

## CLI

```
python3 worker/worker.py compute OP INPUT_JSON      # pure: print {"status","result"}
python3 worker/worker.py serve API_URL [--once] [--poll SECONDS]
```

### `compute OP INPUT_JSON`

`INPUT_JSON` is the job's `input`, as a JSON string. Print exactly one JSON
object to stdout: `{"status": "done"|"error", "result": <any>}`. Exit 0.
This subcommand performs no I/O beyond reading argv and writing stdout, so it
is independently testable without the API.

### `serve API_URL [--once] [--poll SECONDS]`

Loop: `GET {API_URL}/jobs?status=queued`; for a queued job, `POST .../claim`
(skip on `409` — someone else got it); compute its result; `POST .../result`
with `{"status", "result"}` from compute. With `--once`, process at most one
job and exit; otherwise loop forever, sleeping `--poll` seconds (default `0.2`)
when there is nothing to do. Tolerate transient API errors by retrying.

## Op semantics (compute)

`compute(op, input)` returns `(status, result)`:

| op | valid input | result | 
|---|---|---|
| `sum` | list of numbers | their sum (a number) |
| `max` | **non-empty** list of numbers | the maximum |
| `sort_desc` | list of numbers | a new list, sorted descending |
| `reverse` | string | the string reversed |
| `wordcount` | string | integer = number of whitespace-separated tokens (`len(s.split())`) |
| `uppercase` | string | the string upper-cased |

- Booleans are **not** numbers here (JSON `true`/`false` count as invalid in a
  numeric list).
- On the wrong input type (e.g. `sum` of a string, `max` of `[]`, `reverse` of
  a number), or an **unknown op**, return status `"error"`. The `result` for an
  error is not graded (use `null` or a short message — anything).
- On valid input, return status `"done"` and the result above.

Examples: `compute("sum", [1, 2, 3])` → `("done", 6)`;
`compute("sort_desc", [3, 1, 2])` → `("done", [3, 2, 1])`;
`compute("wordcount", "  a  b c ")` → `("done", 3)`;
`compute("max", [])` → `("error", ...)`;
`compute("frobnicate", 1)` → `("error", ...)`.

## Starter test

```
python3 worker/tests/test_worker.py
```

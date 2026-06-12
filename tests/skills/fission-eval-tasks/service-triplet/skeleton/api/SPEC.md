# api — REST job store

Python 3 stdlib only (e.g. `http.server`, `json`). Stores jobs in memory; it
does **not** compute results (that is the worker's job). It is a queue with a
lifecycle.

## CLI

```
python3 api/server.py --port PORT [--host HOST]
```

- `--port` is required; `--host` defaults to `127.0.0.1`.
- Bind and serve until killed. It is fine (and expected) to print a startup
  line to stderr; stdout is not checked.

## Job

A JSON object with exactly these keys:

```json
{"id": "<non-empty unique string>",
 "op": "<string>",
 "input": <any JSON>,
 "status": "queued" | "running" | "done" | "error",
 "result": <any JSON, null until set>}
```

`id` is assigned by the API (any non-empty unique string — a counter or UUID is
fine). New jobs start `queued` with `result` `null`.

## Endpoints

All request/response bodies are JSON. Respond with
`Content-Type: application/json`.

| Method + path | Request body | Success | Errors |
|---|---|---|---|
| `GET /healthz` | — | `200 {"ok": true}` | — |
| `POST /jobs` | `{"op": <str>, "input": <any>}` | `201` with the created job | `400` if the body is not valid JSON, or `op` is missing/not a string, or `input` is missing |
| `GET /jobs/{id}` | — | `200` with the job | `404` if no such id |
| `GET /jobs` | — | `200 {"jobs": [<job>, ...]}` (every job) | — |
| `GET /jobs?status={s}` | — | `200 {"jobs": [...]}` filtered to jobs whose `status == s` | — |
| `POST /jobs/{id}/claim` | — | `200` with the job, whose `status` is now `running` | `404` unknown id; `409` if the job's status is not `queued` |
| `POST /jobs/{id}/result` | `{"status": "done"\|"error", "result": <any>}` | `200` with the updated job (`status` and `result` set) | `404` unknown id; `400` if body is not JSON or `status` is not `done`/`error` |

Notes:

- `claim` must be **atomic**: if two requests claim the same `queued` job, one
  returns `200` (and flips it to `running`), the other returns `409`. A simple
  lock around the read-modify-write is enough.
- `result` may be set on a `running` job (the normal path); setting it on an
  already-terminal job is allowed (last write wins) but the worker never does
  that.
- The order of jobs in `GET /jobs` is unspecified.
- Unknown routes/methods may return `404`/`405`; only the rows above are
  graded.

## Starter test

```
bash api/tests/test_api.sh
```

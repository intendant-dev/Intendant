# jobline

A tiny job-processing system: a REST **API** that stores jobs, a **worker**
that computes their results, and a **CLI** client that submits and polls jobs.
Everything is Python 3 standard library — no third-party packages, no network
beyond localhost.

## Components

The system is three components. **They are independent**: each lives in its own
directory with its own authoritative spec and its own starter tests (currently
failing), and each can be built and tested on its own. They communicate only
through the shared HTTP protocol and op semantics below — no component imports
another's source.

| Directory | Tool | Role |
|---|---|---|
| `api/` | `server.py` | REST job store (queue + lifecycle). Computes nothing. |
| `worker/` | `worker.py` | Claims queued jobs, computes results, submits them back. Owns the op semantics. |
| `cli/` | `client.py` | Client: submit a job, get a job, wait for completion. |

## Shared contract — HTTP protocol

A **job** is a JSON object: `{"id", "op", "input", "status", "result"}` where
`status` is one of `queued`, `running`, `done`, `error`, and `result` is
`null` until set. `id` is a non-empty unique string the API assigns. The API
serves these endpoints (all bodies are JSON; `Content-Type: application/json`):

| Method + path | Body | Success | Errors |
|---|---|---|---|
| `GET /healthz` | — | `200 {"ok": true}` | — |
| `POST /jobs` | `{"op": str, "input": any}` | `201` job (status `queued`, result `null`) | `400` if not JSON / missing `op` or `input` |
| `GET /jobs/{id}` | — | `200` job | `404` unknown id |
| `GET /jobs?status={s}` | — | `200 {"jobs": [job, ...]}` (filtered by status if given, else all) | — |
| `POST /jobs/{id}/claim` | — | `200` job now `running` | `404` unknown; `409` if not currently `queued` |
| `POST /jobs/{id}/result` | `{"status": "done"\|"error", "result": any}` | `200` updated job | `404` unknown; `400` bad body |

The `claim` step is atomic: two workers racing to claim the same job — exactly
one gets `200`, the other `409`.

## Shared contract — op semantics (defined by the worker)

`op` + `input` → `result`. See `worker/SPEC.md` for the exact rules. Summary:
`sum`/`max`/`sort_desc` take a list of numbers; `reverse`/`wordcount`/
`uppercase` take a string. Invalid input (or an unknown op) yields status
`error`.

## Shared contract — CLI verbs

See `cli/SPEC.md`. `submit <url> <op> <input_json>` prints the new job id;
`get <url> <id>` prints the job JSON; `wait <url> <id>` polls until the job is
`done`/`error` and prints the final job JSON.

## Make targets

```
make test      # run all three components' starter tests
make run-api PORT=8080     # convenience: start the API
make run-worker URL=http://127.0.0.1:8080   # convenience: start the worker loop
```

The end-to-end flow (API + worker + CLI together) is exercised by the grader,
which starts the live trio on random ports and drives it with generated jobs.

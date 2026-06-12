# Task: jobline service trio

You are working in the `jobline` repository (your current directory): a tiny
job-processing system made of a REST API, a worker, and a CLI client, all in
Python 3 (standard library only). The scaffolding and specs are in place, but
the three programs are unimplemented stubs and their tests fail.

## Your goal

Make the whole repository work:

1. `make test` passes (all three components' starter tests succeed).
2. The three programs interoperate over the shared HTTP protocol: the CLI can
   submit a job to the API, the worker picks it up and computes its result, and
   the CLI can wait for and read the finished job.

## The three components are independent

Each component lives in its own directory with its own authoritative spec and
its own starter tests:

- `api/` — `server.py`, a REST job store (queue + lifecycle; it stores jobs but
  computes nothing). Spec: `api/SPEC.md`.
- `worker/` — `worker.py`, which computes job results (it owns the op
  semantics) and has a serve loop that drives jobs through the API. Spec:
  `worker/SPEC.md`.
- `cli/` — `client.py`, a client with `submit` / `get` / `wait` verbs. Spec:
  `cli/SPEC.md`.

They communicate only through the shared HTTP protocol and op semantics
documented in `README.md` and the per-component specs; no component imports
another's source. Each can be implemented and tested on its own — the API
against raw HTTP requests, the worker's `compute` as a pure function, the CLI
against any conforming server.

## Rules

- Implement to the specs. Do not edit the test files, the `Makefile`, the
  `SPEC.md` files, or `README.md`; implement the three programs
  (`api/server.py`, `worker/worker.py`, `cli/client.py`) and add supporting
  source if you wish.
- Python 3 standard library only — no third-party packages. No network access
  beyond binding/connecting to localhost.

## How you are evaluated

A held-back grader checks each component independently against generated
inputs — it drives your API over HTTP and inspects the job lifecycle, runs your
worker's `compute` against an independent oracle, and runs your CLI against a
conforming reference server — then runs the three together: it starts your API
and worker on random ports and uses your CLI to submit generated jobs and wait
for their results, checking each result against the oracle. Partial credit is
awarded per component plus a bonus for the end-to-end flow, so a correct
component counts even if another is incomplete.

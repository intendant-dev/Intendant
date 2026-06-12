# cli — job client

Python 3 stdlib only (`urllib`, `json`, `argparse`). Talks to the API over the
shared HTTP protocol (see the repo README). It implements three verbs.

## CLI

```
python3 cli/client.py submit API_URL OP INPUT_JSON
python3 cli/client.py get    API_URL JOB_ID
python3 cli/client.py wait   API_URL JOB_ID [--timeout SECONDS] [--poll SECONDS]
```

`API_URL` is a base URL like `http://127.0.0.1:8080` (no trailing slash).

### `submit API_URL OP INPUT_JSON`

`POST {API_URL}/jobs` with `{"op": OP, "input": <INPUT_JSON parsed as JSON>}`.
On `201`, print **only the new job's `id`** (followed by a newline) to stdout
and exit 0. On any non-201 response, print an error to stderr and exit
non-zero.

### `get API_URL JOB_ID`

`GET {API_URL}/jobs/{JOB_ID}`. On `200`, print the job as JSON to stdout and
exit 0. On `404`, print an error to stderr and exit non-zero.

### `wait API_URL JOB_ID [--timeout SECONDS] [--poll SECONDS]`

Poll `GET {API_URL}/jobs/{JOB_ID}` every `--poll` seconds (default `0.1`) until
the job's `status` is `done` or `error`, or `--timeout` seconds (default `10`)
elapse. Then:

- status `done`: print the final job JSON to stdout, exit 0.
- status `error`: print the final job JSON to stdout, exit non-zero.
- timeout: print an error to stderr, exit non-zero.

The printed job JSON must be a single JSON object parseable with `json.loads`;
key order and whitespace do not matter.

## Starter test

```
python3 cli/tests/test_cli.py
```

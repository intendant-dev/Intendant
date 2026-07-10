#!/bin/bash
# ACTIONS_RUNNER_HOOK_JOB_STARTED — pre-job janitor for the CI service
# account (safety net for residue a killed previous job left behind).
# Wired via the `.env` file in each runner root; see scripts/ci/README.md,
# "Job hooks". All logic lives in hook-lib.sh; this wrapper only names the
# phase. Always exits 0: a non-zero exit here would fail the job.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/ci/hooks/hook-lib.sh
. "$HERE/hook-lib.sh"
run_hook started

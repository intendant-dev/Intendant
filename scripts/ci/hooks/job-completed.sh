#!/bin/bash
# ACTIONS_RUNNER_HOOK_JOB_COMPLETED — post-job janitor for the CI service
# account (cleans up after the job that just finished; job-started.sh
# catches anything this one couldn't see yet). Wired via the `.env` file in
# each runner root; see scripts/ci/README.md, "Job hooks". All logic lives
# in hook-lib.sh; this wrapper only names the phase.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/ci/hooks/hook-lib.sh
. "$HERE/hook-lib.sh"
run_hook completed

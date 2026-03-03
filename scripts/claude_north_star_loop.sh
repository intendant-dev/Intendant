#!/usr/bin/env bash
set -euo pipefail

ROOT="/home/user/projects/intendant-codex-fork"
LOG_DIR="${ROOT}/.intendant/controller-loop"
HALT_FILE="${LOG_DIR}/request_halt"
HALT_AFTER_CYCLE_FILE="${LOG_DIR}/request_halt_after_cycle"
LOCK_DIR="${LOG_DIR}/active.lock"
LOCK_PID_FILE="${LOCK_DIR}/pid"
LOCK_RUN_FILE="${LOCK_DIR}/run_id"
LOCK_TS_FILE="${LOCK_DIR}/acquired_at"

# Persistent graceful halt gate: when armed, refuse to start a new cycle.
if [[ -f "$HALT_FILE" ]]; then
  exit 0
fi

# Legacy one-shot graceful halt gate.
if [[ -f "$HALT_AFTER_CYCLE_FILE" ]]; then
  rm -f "$HALT_AFTER_CYCLE_FILE"
  exit 0
fi

# Ensure each loop cycle runs in its own detached session. This prevents
# parent/session teardown from sending TERM to newly spawned cycles.
if [[ "${INTENDANT_LOOP_DETACHED:-0}" != "1" ]]; then
  SELF_PATH="$(readlink -f "${BASH_SOURCE[0]:-$0}")"
  if command -v setsid >/dev/null 2>&1; then
    nohup setsid -f env INTENDANT_LOOP_DETACHED=1 bash "$SELF_PATH" "$@" \
      </dev/null >/dev/null 2>&1 &
  else
    nohup env INTENDANT_LOOP_DETACHED=1 bash "$SELF_PATH" "$@" \
      </dev/null >/dev/null 2>&1 &
  fi
  exit 0
fi

RUN_TS="$(date -u +"%Y%m%dT%H%M%SZ")"
RUN_ID="${RUN_TS}-$$"
RUN_DIR="${LOG_DIR}/${RUN_ID}"
OUT_FILE="${RUN_DIR}/claude.json"
STATUS_FILE="${RUN_DIR}/status.json"
SUMMARY_FILE="${RUN_DIR}/summary.json"
HEARTBEAT_FILE="${RUN_DIR}/heartbeat.txt"
LATEST_LINK="${LOG_DIR}/latest"
LATEST_PID_FILE="${LOG_DIR}/latest.pid"
LATEST_OUT_FILE="${LOG_DIR}/latest.json"
LATEST_STATUS_FILE="${LOG_DIR}/latest.status.json"
LATEST_RUN_ID_FILE="${LOG_DIR}/latest.run_id"
CLAUDE_PID_FILE="${RUN_DIR}/claude.pid"
WRAPPER_PID_FILE="${RUN_DIR}/wrapper.pid"
INTERVENTION_LOG="${RUN_DIR}/intervention.log"
STOP_FILE="${LOG_DIR}/request_stop"
ABORT_FILE="${LOG_DIR}/request_abort"

HB_PID=""
CLAUDE_PID=""
CONTROL_PID=""
FINALIZED="0"
STARTED_AT="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

acquire_lock() {
  mkdir -p "$LOG_DIR"
  local owner_pid
  if mkdir "$LOCK_DIR" 2>/dev/null; then
    printf '%s\n' "$$" > "$LOCK_PID_FILE"
    printf '%s\n' "$RUN_ID" > "$LOCK_RUN_FILE"
    date -u +"%Y-%m-%dT%H:%M:%SZ" > "$LOCK_TS_FILE"
    return 0
  fi

  owner_pid="$(cat "$LOCK_PID_FILE" 2>/dev/null || true)"
  if [[ -n "$owner_pid" ]] && ! kill -0 "$owner_pid" >/dev/null 2>&1; then
    rm -rf "$LOCK_DIR"
    if mkdir "$LOCK_DIR" 2>/dev/null; then
      printf '%s\n' "$$" > "$LOCK_PID_FILE"
      printf '%s\n' "$RUN_ID" > "$LOCK_RUN_FILE"
      date -u +"%Y-%m-%dT%H:%M:%SZ" > "$LOCK_TS_FILE"
      return 0
    fi
  fi
  return 1
}

release_lock() {
  local owner_pid
  owner_pid="$(cat "$LOCK_PID_FILE" 2>/dev/null || true)"
  if [[ -n "$owner_pid" && "$owner_pid" == "$$" ]]; then
    rm -rf "$LOCK_DIR"
  fi
}

log_intervention() {
  printf '%s %s\n' "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" "$*" >> "$INTERVENTION_LOG"
}

capture_signal_diagnostics() {
  local sig="$1"
  local self_meta parent_meta
  self_meta="$(ps -o pid=,ppid=,pgid=,sid=,tty=,stat=,etime=,cmd= -p "$$" 2>/dev/null | sed 's/^ *//')"
  parent_meta="$(ps -o pid=,ppid=,pgid=,sid=,tty=,stat=,etime=,cmd= -p "$PPID" 2>/dev/null | sed 's/^ *//')"
  log_intervention "signal_received=$sig self=[$self_meta] parent=[$parent_meta] claude_pid=${CLAUDE_PID:-unset}"
  if [[ -n "${CLAUDE_PID:-}" ]]; then
    local claude_meta
    claude_meta="$(ps -o pid=,ppid=,pgid=,sid=,tty=,stat=,etime=,cmd= -p "$CLAUDE_PID" 2>/dev/null | sed 's/^ *//')"
    log_intervention "signal_context_claude=[$claude_meta]"
  fi
}

write_status() {
  local state="$1"
  local exit_code="$2"
  local reason="${3:-}"
  local finished_at
  local tmp_status tmp_latest tmp_summary
  finished_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  tmp_status="${STATUS_FILE}.tmp.$$"
  tmp_latest="${LATEST_STATUS_FILE}.tmp.$$"
  tmp_summary="${SUMMARY_FILE}.tmp.$$"
  printf '{"run_id":"%s","state":"%s","pid":%s,"claude_pid":"%s","exit_code":%s,"started_at":"%s","finished_at":"%s","reason":"%s","output":"%s"}\n' \
    "$RUN_ID" "$state" "$$" "${CLAUDE_PID:-}" "$exit_code" "$STARTED_AT" "$finished_at" "$reason" "$OUT_FILE" > "$tmp_status"
  mv -f "$tmp_status" "$STATUS_FILE"
  cp "$STATUS_FILE" "$tmp_latest"
  mv -f "$tmp_latest" "$LATEST_STATUS_FILE"
  printf '{"run_id":"%s","state":"%s","exit_code":%s,"finished_at":"%s"}\n' \
    "$RUN_ID" "$state" "$exit_code" "$finished_at" > "$tmp_summary"
  mv -f "$tmp_summary" "$SUMMARY_FILE"
}

cleanup() {
  local state="$1"
  local exit_code="$2"
  local reason="${3:-}"
  if [[ "$FINALIZED" == "1" ]]; then
    return
  fi
  FINALIZED="1"
  log_intervention "cleanup_begin state=$state exit_code=$exit_code reason=${reason:-none} claude_pid=${CLAUDE_PID:-unset}"
  if [[ -n "$HB_PID" ]]; then
    kill "$HB_PID" >/dev/null 2>&1 || true
    wait "$HB_PID" 2>/dev/null || true
  fi
  if [[ -n "$CONTROL_PID" ]]; then
    kill "$CONTROL_PID" >/dev/null 2>&1 || true
    wait "$CONTROL_PID" 2>/dev/null || true
  fi
  if [[ -n "$CLAUDE_PID" ]]; then
    if kill -0 "$CLAUDE_PID" >/dev/null 2>&1; then
      kill -TERM "$CLAUDE_PID" >/dev/null 2>&1 || true
      for _ in 1 2 3 4 5; do
        if ! kill -0 "$CLAUDE_PID" >/dev/null 2>&1; then
          break
        fi
        sleep 1
      done
      if kill -0 "$CLAUDE_PID" >/dev/null 2>&1; then
        kill -KILL "$CLAUDE_PID" >/dev/null 2>&1 || true
      fi
    fi
    wait "$CLAUDE_PID" 2>/dev/null || true
  fi
  write_status "$state" "$exit_code" "$reason"
  log_intervention "cleanup_end state=$state exit_code=$exit_code reason=${reason:-none}"
  release_lock
}

on_signal() {
  local sig="$1"
  capture_signal_diagnostics "$sig"
  cleanup "signaled" 143 "$sig"
  exit 143
}

on_exit() {
  local exit_code="$?"
  if [[ "$FINALIZED" != "1" ]]; then
    local state="failed"
    local reason="unexpected_exit"
    if [[ "$exit_code" -eq 0 ]]; then
      state="exited"
      reason="exit_trap"
    fi
    cleanup "$state" "$exit_code" "$reason"
  fi
}

read -r -d '' PROMPT <<'EOF' || true
North star: recursively improve intendant toward state-of-the-art CLI/TUI/MCP controller behavior.

Execution policy:
- Complete one concrete improvement per cycle.
- Include tests and docs updates for each improvement.
- Keep changes incremental and shippable.
- Run intendant E2E tests each cycle before handoff.
- If E2E or regression tests fail, fix the bugs in the same cycle before scheduling restart.
- The repository may already contain uncommitted changes from prior loop cycles; treat those as expected baseline context, not as unexpected external edits.
- Do not stop only because `git status` is dirty at turn start; continue from current workspace state.
- Do not modify `scripts/claude_north_star_loop.sh` unless the operator explicitly requests loop-infrastructure changes.
- Commit each completed cycle before restart handshake.
- Use one commit per cycle with message format: `loop: <short summary> [run <YYYYMMDDTHHMMSSZ>]`.
- Do not amend prior commits.
- Do not push unless explicitly requested by the user.
- Before restart handshake, ensure there are no staged/unstaged tracked changes left (`git status --porcelain --untracked-files=no` should be empty).

Controller recursion policy:
- Near turn end, call intendant MCP tool schedule_controller_restart with:
  - controller_id: "claude_code"
  - north_star_goal: this same north-star objective
  - restart_after: "turn_end"
  - auto_start_task: false
  - restart_command: "bash /home/user/projects/intendant-codex-fork/scripts/claude_north_star_loop.sh"
- Then call controller_turn_complete as the final controller action.
- Do not use start_task for normal work loops (only explicit E2E testing).
EOF

if ! acquire_lock; then
  exit 0
fi
mkdir -p "$RUN_DIR"
ln -sfn "$RUN_DIR" "$LATEST_LINK"
printf '%s\n' "$$" > "$LATEST_PID_FILE"
printf '%s\n' "$$" > "$WRAPPER_PID_FILE"
printf '%s\n' "$OUT_FILE" > "$LATEST_OUT_FILE"
printf '%s\n' "$RUN_ID" > "$LATEST_RUN_ID_FILE"
# Clear stale operator intervention requests from prior runs.
rm -f "$STOP_FILE" "$ABORT_FILE"
write_status "starting" -1 ""
log_intervention "run_started run_id=$RUN_ID pid=$$ ppid=$PPID"

cd "$ROOT"
(
  while true; do
    date -u +"%Y-%m-%dT%H:%M:%SZ heartbeat pid=$$" > "$HEARTBEAT_FILE"
    sleep 15
  done
) &
HB_PID=$!

(
  while true; do
    current_pid=""
    if [[ -f "$CLAUDE_PID_FILE" ]]; then
      current_pid="$(cat "$CLAUDE_PID_FILE" 2>/dev/null || true)"
    elif [[ -n "$CLAUDE_PID" ]]; then
      current_pid="$CLAUDE_PID"
    fi
    if [[ -n "$current_pid" ]] && kill -0 "$current_pid" >/dev/null 2>&1; then
      if [[ -f "$STOP_FILE" ]]; then
        log_intervention "operator_request=stop claude_pid=$current_pid"
        rm -f "$STOP_FILE"
        kill -TERM "$current_pid" >/dev/null 2>&1 || true
      fi
      if [[ -f "$ABORT_FILE" ]]; then
        log_intervention "operator_request=abort claude_pid=$current_pid"
        rm -f "$ABORT_FILE"
        kill -KILL "$current_pid" >/dev/null 2>&1 || true
      fi
    fi
    sleep 2
  done
) &
CONTROL_PID=$!

trap 'on_signal TERM' TERM
trap 'on_signal INT' INT
trap 'on_signal HUP' HUP
trap 'on_signal QUIT' QUIT
trap 'on_exit' EXIT

set +e
claude -p \
  --dangerously-skip-permissions \
  --output-format json \
  --verbose \
  ${CLAUDE_MODEL:+--model "$CLAUDE_MODEL"} \
  ${CLAUDE_MAX_BUDGET:+--max-budget-usd "$CLAUDE_MAX_BUDGET"} \
  "$PROMPT" >> "$OUT_FILE" 2>&1 &
CLAUDE_PID="$!"
printf '%s\n' "$CLAUDE_PID" > "$CLAUDE_PID_FILE"
log_intervention "claude_started claude_pid=$CLAUDE_PID"
write_status "running" -1 ""
wait "$CLAUDE_PID"
EXIT_CODE=$?
set -e

cleanup "exited" "$EXIT_CODE" ""
exit "$EXIT_CODE"

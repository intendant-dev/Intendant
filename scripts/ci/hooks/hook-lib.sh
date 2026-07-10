#!/bin/bash
# Shared engine for the GitHub Actions runner job hooks (job-started.sh /
# job-completed.sh source this; wiring and doctrine in scripts/ci/README.md,
# "Job hooks").
#
# Runs as the CI service account, inside the job's Runner.Worker, with the
# job's environment (RUNNER_TEMP, RUNNER_NAME, GITHUB_*). Duties per
# invocation:
#
#   1. wipe the per-runner $RUNNER_TEMP residue,
#   2. reap stale per-ACCOUNT temp and test-home residue (age-gated:
#      $TMPDIR and ~/.intendant are shared with the other listener's
#      live jobs — never yank a fresh entry),
#   3. kill leftover processes that no live runner tree owns,
#   4. log exactly one summary line to $LOG (rotated), and
#   5. always exit 0, within HOOK_TIMEOUT_SECS.
#
# Never touched: ~/.cache/intendant-ci (the warm external cargo target
# caches — the fleet watchdog owns those), ~/.cargo, ~/.rustup.
#
# Exit-0 doctrine: a non-zero exit from the job-started hook FAILS the job
# (GitHub semantics), so janitorial trouble must never take down CI — every
# failure path logs what it can and exits 0.

LOG="${INTENDANT_CI_HOOK_LOG:-/var/log/intendant-ci-hooks.log}"
HOOK_TIMEOUT_SECS="${INTENDANT_CI_HOOK_TIMEOUT_SECS:-60}"

log_line() {
    printf '%s %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$*" >> "$LOG" 2>/dev/null || true
}

# Rotation mirrors fleet-watchdog.sh's budget (1MB cap, keep the last 256K)
# — but the hook runs as the CI account, which can write the pre-created log
# FILE yet cannot create siblings in /var/log, so it rotates by
# truncate-in-place instead of tmp+mv. (The two listeners' hooks can race
# here; the worst case is a lost tail line in a janitor log.)
rotate_log() {
    [ -f "$LOG" ] || return 0
    [ "$(wc -c < "$LOG" 2>/dev/null || echo 0)" -gt 1048576 ] || return 0
    local tmp
    tmp=$(mktemp 2>/dev/null) || return 0
    if tail -c 262144 "$LOG" > "$tmp" 2>/dev/null; then
        cat "$tmp" > "$LOG" 2>/dev/null
    fi
    rm -f "$tmp"
}

# $RUNNER_TEMP is per-runner (<runner root>/_work/_temp) — never shared with
# the other listener — and the runner recreates it every job, so a full wipe
# of its CONTENTS is safe. The case guard keeps a mis-set variable from ever
# aiming this at / or $HOME.
wipe_runner_temp() {
    local n=0 entry
    case "${RUNNER_TEMP:-}" in
        */_work/_temp)
            if [ -d "$RUNNER_TEMP" ]; then
                for entry in "$RUNNER_TEMP"/* "$RUNNER_TEMP"/.[!.]* "$RUNNER_TEMP"/..?*; do
                    [ -e "$entry" ] || [ -L "$entry" ] || continue
                    rm -rf "$entry" 2>/dev/null && n=$((n + 1))
                done
            fi
            ;;
    esac
    printf '%s' "$n"
}

# $TMPDIR (the per-account DARWIN_USER_TEMP_DIR under a gui session; plain
# /tmp under a LaunchDaemon, which launchd gives no TMPDIR) is shared by
# BOTH listeners' jobs — a fresh tempdir may belong to the other listener's
# live job. Only reap well-known test/temp prefixes older than 24h (no job
# runs remotely that long); under /tmp the sticky bit additionally limits
# deletion to this account's own entries.
wipe_stale_tmp() {
    local base="${TMPDIR:-/tmp}" n=0 entry
    if [ ! -d "$base" ]; then
        printf '0'
        return 0
    fi
    while IFS= read -r entry; do
        [ -n "$entry" ] || continue
        rm -rf "$entry" 2>/dev/null && n=$((n + 1))
    done < <(find "$base" -mindepth 1 -maxdepth 1 \
        \( -name '.tmp*' -o -name 'tmp.*' -o -name 'intendant-*' \
           -o -name 'rustc*' -o -name 'cargo-install*' \) \
        -mmin +1440 2>/dev/null)
    printf '%s' "$n"
}

# Hermetic tests never write $HOME, but escaped fixtures historically did
# (CLAUDE.md, "Tests are hermetic"). On the dedicated CI account,
# ~/.intendant is only ever such residue — reap it age-gated so an
# unhermetic test still running on the other listener isn't yanked mid-job.
wipe_stale_test_home() {
    local n=0 entry
    if [ ! -d "$HOME/.intendant" ]; then
        printf '0'
        return 0
    fi
    while IFS= read -r entry; do
        [ -n "$entry" ] || continue
        rm -rf "$entry" 2>/dev/null && n=$((n + 1))
    done < <(find "$HOME/.intendant" -mindepth 1 -maxdepth 1 -mmin +1440 2>/dev/null)
    printf '%s' "$n"
}

# Kill leftover processes owned by the CI account that no live runner tree
# owns (daemons leaked by killed test jobs). Both listeners run as this
# account, so "leftover" is decided by ANCESTRY, not age: a process whose
# ancestor chain reaches a runner service process (either listener's
# runsvc.sh / RunnerService.js / Runner.Listener / Runner.Worker) is a live
# job's process and is left alone; one that chains straight to launchd
# without passing a runner process is an orphan. The shared per-account
# sccache server is explicitly protected — killing it mid-compile would fail
# the other listener's rustc invocations.
reap_orphans() {
    local uid snapshot
    uid=$(id -u)
    snapshot=$(ps -axo pid=,ppid=,uid=,command= 2>/dev/null | awk -v u="$uid" '$3 == u')
    if [ -z "$snapshot" ]; then
        printf 'killed=0'
        return 0
    fi

    local protected=" " pid ppid cmd
    while read -r pid ppid _ cmd; do
        [ -n "$pid" ] || continue
        case "$cmd" in
            *runsvc.sh* | *RunnerService.js* | *Runner.Listener* | *Runner.Worker* | *sccache*)
                protected="$protected$pid "
                ;;
        esac
    done <<< "$snapshot"

    # Belt and braces: protect this hook's own ancestor chain (it chains to
    # Runner.Worker anyway). Bounded walk in case of a ppid anomaly.
    local hop=0
    pid=$$
    while [ "$hop" -lt 64 ] && [ -n "$pid" ] && [ "$pid" -gt 1 ] 2>/dev/null; do
        protected="$protected$pid "
        pid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')
        hop=$((hop + 1))
    done

    # Fixed point: every descendant of a protected process is protected
    # (descendants of a live Worker are that job's processes).
    local changed=1
    while [ "$changed" = 1 ]; do
        changed=0
        while read -r pid ppid _ cmd; do
            [ -n "$pid" ] || continue
            case "$protected" in
                *" $pid "*) continue ;;
            esac
            case "$protected" in
                *" $ppid "*)
                    protected="$protected$pid "
                    changed=1
                    ;;
            esac
        done <<< "$snapshot"
    done

    # Everything else is an orphan: TERM, short grace, then KILL.
    local victims="" detail="" n=0 comm
    while read -r pid ppid _ cmd; do
        [ -n "$pid" ] || continue
        case "$protected" in
            *" $pid "*) continue ;;
        esac
        victims="$victims $pid"
        comm=$(basename "${cmd%% *}" 2>/dev/null)
        detail="$detail,$pid:${comm:-?}"
        n=$((n + 1))
    done <<< "$snapshot"

    if [ -n "$victims" ]; then
        # shellcheck disable=SC2086 # victims is a deliberate pid word-list
        kill -TERM $victims 2>/dev/null
        sleep 3
        for pid in $victims; do
            kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
        done
    fi
    printf 'killed=%s%s' "$n" "${detail:+ [${detail#,}]}"
}

# The bounded driver: the real work runs in a child subshell so a hung
# cleanup can be reaped by the timer — GitHub applies no timeout of its own
# to these hooks, and a wedged started-hook would wedge the job.
run_hook() {
    local phase="$1"
    rotate_log
    local t0 result_file work timer outcome detail=""
    t0=$(date +%s)
    result_file=$(mktemp 2>/dev/null) || result_file=""

    (
        rt=$(wipe_runner_temp)
        st=$(wipe_stale_tmp)
        sh_=$(wipe_stale_test_home)
        ko=$(reap_orphans)
        if [ -n "$result_file" ]; then
            printf 'runner_temp=%s stale_tmp=%s stale_home=%s %s' \
                "$rt" "$st" "$sh_" "$ko" > "$result_file"
        fi
    ) &
    work=$!
    ( sleep "$HOOK_TIMEOUT_SECS"; kill -TERM "$work" 2>/dev/null ) &
    timer=$!

    outcome=ok
    wait "$work" 2>/dev/null || outcome=timeout
    # (The reap-to-kill gap here is a theoretical PID-reuse window only when
    # the work finishes at exactly the deadline; blast radius is a stray
    # TERM inside our own account.)
    kill "$timer" 2>/dev/null
    wait "$timer" 2>/dev/null

    if [ -n "$result_file" ]; then
        detail=$(cat "$result_file" 2>/dev/null || true)
        rm -f "$result_file"
    fi
    log_line "$phase runner=${RUNNER_NAME:-?} job=${GITHUB_JOB:-?} run=${GITHUB_RUN_ID:-?} took=$(($(date +%s) - t0))s outcome=$outcome $detail"
    exit 0
}

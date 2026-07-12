#!/bin/bash
# Fleet runner watchdog — one tick per invocation (drive it from a
# LaunchDaemon StartInterval / systemd timer, as root).
#
# Why this exists: the in-job disk preflight fails AFTER a job is
# assigned, so a full disk burns whole speculative merge-queue entries
# (seven macOS validations died at 18G free on 2026-07-10). This
# watchdog acts BEFORE assignment: it pauses the runner listeners when
# free disk drops below the stop floor, reclaims what it owns (stale
# external cargo target caches — see the "External cargo target dir"
# workflow steps), and resumes the listeners only once free space
# clears the resume ceiling (hysteresis, so it never flaps). On macOS
# it thins local APFS snapshots first: purgeable space makes `df`
# swing tens of GB with zero real deletions, and thinning makes the
# number honest before any decision is taken.
#
# It only ever deletes inside the configured cache roots. It never
# touches checkouts, project worktrees, or anything it did not create.
# Cache maintenance is idle-only: an assigned runner job owns its target
# directory until Runner.Worker exits, even when the host is below the normal
# disk stop floor.
#
# Config: /etc/intendant-ci/watchdog.conf (see watchdog.conf.example).
set -u

CONF="${INTENDANT_CI_WATCHDOG_CONF:-/etc/intendant-ci/watchdog.conf}"
# shellcheck disable=SC1090
[ -r "$CONF" ] && . "$CONF"

STOP_GB="${STOP_GB:-50}"            # pause listeners below this (idle listeners)
HARD_STOP_GB="${HARD_STOP_GB:-25}"  # pause even mid-job below this
RESUME_GB="${RESUME_GB:-75}"        # resume listeners at/above this
PRUNE_DAYS="${PRUNE_DAYS:-7}"       # drop cache keys unused this long
CAP_GB="${CAP_GB:-50}"              # total cap per cache root
CACHE_ROOTS="${CACHE_ROOTS:-}"      # space-separated cache roots
VOLUME="${VOLUME:-/}"               # volume free space is measured on
STATE_DIR="${STATE_DIR:-/var/db/intendant-ci}"
LOG="${LOG:-/var/log/intendant-ci-watchdog.log}"
RUNNER_USER="${RUNNER_USER:-}"      # account(s) the listeners run as (space-separated)
RUNNER_UID="${RUNNER_UID:-}"        # LaunchAgent account uid (macOS gui domain)
RUNNER_LABELS="${RUNNER_LABELS:-}"  # macOS: LaunchAgent labels (gui domain)
RUNNER_PLIST_DIR="${RUNNER_PLIST_DIR:-}"  # macOS: dir holding those plists
RUNNER_DAEMON_LABELS="${RUNNER_DAEMON_LABELS:-}"  # macOS: LaunchDaemon labels (system domain)
RUNNER_DAEMON_PLIST_DIR="${RUNNER_DAEMON_PLIST_DIR:-/Library/LaunchDaemons}"
RUNNER_UNITS="${RUNNER_UNITS:-}"    # Linux: systemd units

PAUSED_MARKER="$STATE_DIR/listeners.paused"          # disk-pressure pause
MEM_PAUSED_MARKER="$STATE_DIR/listeners.paused-mem"  # memory-pressure pause
MEM_TICKS_FILE="$STATE_DIR/mem-pressure-ticks"
MEM_NORMAL_FILE="$STATE_DIR/mem-normal-ticks"
# Memory-pressure thresholds. macOS: kern.memorystatus_vm_pressure_level
# (1 normal / 2 warning / 4 critical). Linux: PSI `some avg10` percentage
# from /proc/pressure/memory.
MEM_PRESSURE_LEVEL="${MEM_PRESSURE_LEVEL:-4}"
MEM_PSI_PAUSE="${MEM_PSI_PAUSE:-40}"
MEM_PAUSE_TICKS="${MEM_PAUSE_TICKS:-2}"
MEM_RESUME_TICKS="${MEM_RESUME_TICKS:-2}"
mkdir -p "$STATE_DIR"

log() {
    printf '%s %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$*" >> "$LOG"
}

# Rotate a runaway log (keep it a bounded artifact, not a disk risk).
if [ -f "$LOG" ] && [ "$(wc -c < "$LOG")" -gt 1048576 ]; then
    tail -c 262144 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
fi

free_gb() {
    df -Pk "$VOLUME" | awk 'NR==2 {print int($4 / 1024 / 1024)}'
}

is_macos() { [ "$(uname -s)" = "Darwin" ]; }

job_running() {
    # Runner.Worker only exists while a job executes on a listener.
    # RUNNER_USER may list several accounts (mid-migration a host runs
    # LaunchAgent listeners as the operator and LaunchDaemon listeners as
    # the CI service account at the same time).
    if [ -n "$RUNNER_USER" ]; then
        for u in $RUNNER_USER; do
            pgrep -u "$u" -f 'Runner\.Worker' >/dev/null 2>&1 && return 0
        done
        return 1
    else
        pgrep -f 'Runner\.Worker' >/dev/null 2>&1
    fi
}

stop_listeners() {
    if is_macos; then
        for label in $RUNNER_LABELS; do
            launchctl bootout "gui/$RUNNER_UID/$label" 2>/dev/null \
                && log "stopped listener $label" \
                || log "listener $label was not running"
        done
        for label in $RUNNER_DAEMON_LABELS; do
            launchctl bootout "system/$label" 2>/dev/null \
                && log "stopped daemon listener $label" \
                || log "daemon listener $label was not running"
        done
    else
        for unit in $RUNNER_UNITS; do
            systemctl stop "$unit" 2>/dev/null \
                && log "stopped listener $unit" \
                || log "listener $unit was not running"
        done
    fi
}

start_listeners() {
    if is_macos; then
        for label in $RUNNER_LABELS; do
            launchctl bootstrap "gui/$RUNNER_UID" "$RUNNER_PLIST_DIR/$label.plist" 2>/dev/null \
                && log "resumed listener $label" \
                || log "listener $label already running (or bootstrap failed — check manually)"
        done
        for label in $RUNNER_DAEMON_LABELS; do
            launchctl bootstrap system "$RUNNER_DAEMON_PLIST_DIR/$label.plist" 2>/dev/null \
                && log "resumed daemon listener $label" \
                || log "daemon listener $label already running (or bootstrap failed — check manually)"
        done
    else
        for unit in $RUNNER_UNITS; do
            systemctl start "$unit" 2>/dev/null \
                && log "resumed listener $unit" \
                || log "listener $unit failed to start — check manually"
        done
    fi
}

# Sustained host memory pressure? Fail-open: a missing/unreadable probe
# reads as "not pressured" — the watchdog must never wedge assignment on
# probe quirks. This is an ASSIGNMENT gate only: it cannot and does not
# constrain builds already running (bounding those is the rustc
# governor's job — scripts/ci/README.md "Governor").
mem_pressured() {
    if is_macos; then
        level=$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null || echo 1)
        [ "$level" -ge "$MEM_PRESSURE_LEVEL" ] 2>/dev/null
    else
        [ -r /proc/pressure/memory ] || return 1
        awk -v cap="$MEM_PSI_PAUSE" '/^some/ {
            for (i = 1; i <= NF; i++) if ($i ~ /^avg10=/) {
                sub(/^avg10=/, "", $i); exit !($i + 0 > cap + 0)
            }
            exit 1
        }' /proc/pressure/memory
    fi
}

# Cache keys are the per-listener, per-toolchain dirs the workflows
# create; each carries a .last-used recency marker.
prune_stale_keys() {
    for root in $CACHE_ROOTS; do
        [ -d "$root" ] || continue
        for key in "$root"/*/; do
            [ -d "$key" ] || continue
            marker="$key.last-used"
            if [ ! -f "$marker" ] || [ -n "$(find "$marker" -mtime +"$PRUNE_DAYS" 2>/dev/null)" ]; then
                # A job may have been assigned after main's idle check. Never
                # race that job by deleting a cache key underneath rustc.
                if job_running; then
                    log "job began during cache pruning — deferring maintenance"
                    return
                fi
                log "pruning stale cache key $key"
                rm -rf "$key"
            fi
        done
    done
}

# Oldest-first eviction until the root fits its cap (or, under disk
# pressure, until the volume clears the resume ceiling).
evict_until() {
    target_free="$1"  # 0 = only enforce CAP_GB
    for root in $CACHE_ROOTS; do
        [ -d "$root" ] || continue
        while :; do
            used_kb=$(du -sk "$root" 2>/dev/null | awk '{print $1}')
            used_gb=$((used_kb / 1024 / 1024))
            over_cap=$([ "$used_gb" -gt "$CAP_GB" ] && echo 1 || echo 0)
            need_free=$([ "$target_free" -gt 0 ] && [ "$(free_gb)" -lt "$target_free" ] && echo 1 || echo 0)
            [ "$over_cap" = 0 ] && [ "$need_free" = 0 ] && break
            oldest=$(ls -1td "$root"/*/ 2>/dev/null | tail -1)
            [ -z "$oldest" ] && break
            # Recheck immediately before deletion: an idle listener can accept
            # a job while the watchdog is measuring a large cache root.
            if job_running; then
                log "job began during cache eviction — deferring maintenance"
                return
            fi
            log "evicting cache key $oldest (root ${used_gb}G, cap ${CAP_GB}G, free $(free_gb)G)"
            rm -rf "$oldest"
        done
    done
}

# Policy-ceiling observability: a substantial rustc with neither a
# rustc-governor nor an sccache ancestor means someone opted out of the
# governor (env beats config: RUSTC_WRAPPER="" / RUSTC=… — seen in the
# wild from an agent session). Log-only; the inhabitants are cooperative
# agents and CLAUDE.md carries the doctrine. Known blind spot: a build
# run with RUSTC_WRAPPER=sccache (bypassing the governor but keeping
# sccache) is ancestry-indistinguishable from a governed compile.
detect_ungoverned_rustc() {
    ps -axo pid=,rss=,comm= 2>/dev/null | awk '$3 ~ /rustc$/ && $2 > 512000 {print $1, $2}'     | while read -r rpid rss; do
        p="$rpid" governed=0 hop=0
        while [ "$hop" -lt 10 ] && [ -n "$p" ] && [ "$p" -gt 1 ] 2>/dev/null; do
            case "$(ps -o comm= -p "$p" 2>/dev/null)" in
                *rustc-governor* | *sccache*) governed=1; break ;;
            esac
            p=$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' ')
            hop=$((hop + 1))
        done
        [ "$governed" = 0 ] && log "ungoverned rustc pid=$rpid rss=$((rss / 1024))MB — no governor/sccache ancestor (RUSTC_WRAPPER override?)"
    done
}

main() {
    free=$(free_gb)

    # macOS: purgeable space (local Time Machine snapshots) makes the
    # free number swing tens of GB on its own. Thin snapshots before
    # trusting a low reading, then re-measure.
    if is_macos && [ "$free" -lt "$RESUME_GB" ]; then
        tmutil thinlocalsnapshots "$VOLUME" 999999999999 4 >/dev/null 2>&1
        free=$(free_gb)
    fi

    # An ordinary low-disk event pauses assignment but never destroys a live
    # build. HARD_STOP_GB remains the explicit exception: stop the listeners
    # first, then reclaim once Runner.Worker has exited. The deletion helpers
    # recheck job_running in case shutdown is not instantaneous.
    if job_running; then
        if [ "$free" -lt "$HARD_STOP_GB" ]; then
            log "free ${free}G below hard floor ${HARD_STOP_GB}G — pausing listeners (even mid-job)"
            stop_listeners
            touch "$PAUSED_MARKER"
        else
            if [ "$free" -lt "$STOP_GB" ]; then
                log "free ${free}G below ${STOP_GB}G but a job is running — deferring pause and cache maintenance"
            fi
            return
        fi
    fi

    detect_ungoverned_rustc
    prune_stale_keys
    evict_until 0  # steady-state cap enforcement

    free=$(free_gb)
    if [ "$free" -lt "$STOP_GB" ]; then
        if [ "$free" -lt "$HARD_STOP_GB" ]; then
            log "free ${free}G below hard floor ${HARD_STOP_GB}G — pausing listeners (even mid-job)"
            stop_listeners
        elif job_running; then
            # A job may have arrived during steady-state cache maintenance.
            log "free ${free}G below ${STOP_GB}G but a job is running — deferring pause and cache maintenance"
            return
        else
            log "free ${free}G below ${STOP_GB}G — pausing listeners"
            stop_listeners
            touch "$PAUSED_MARKER"
        fi
        evict_until "$RESUME_GB"
        free=$(free_gb)
    fi

    if [ -f "$PAUSED_MARKER" ] && [ "$free" -ge "$RESUME_GB" ]; then
        log "free ${free}G cleared resume ceiling ${RESUME_GB}G — clearing disk pause"
        rm -f "$PAUSED_MARKER"
        if [ ! -f "$MEM_PAUSED_MARKER" ]; then
            start_listeners
        else
            log "memory pause still active — listeners stay down"
        fi
    fi

    # Memory pressure: pause new-job ASSIGNMENT on sustained pressure,
    # resume on sustained normal. Consecutive-tick counters give the
    # hysteresis; never pause mid-job on memory alone (unlike the disk
    # hard floor) — running compiles are the governor's problem, and a
    # one-job listener assigns its next job only when idle anyway.
    if mem_pressured; then
        mem_ticks=$(( $(cat "$MEM_TICKS_FILE" 2>/dev/null || echo 0) + 1 ))
        echo "$mem_ticks" > "$MEM_TICKS_FILE"
        rm -f "$MEM_NORMAL_FILE"
        if [ "$mem_ticks" -ge "$MEM_PAUSE_TICKS" ] && [ ! -f "$MEM_PAUSED_MARKER" ]; then
            if job_running; then
                log "memory pressure sustained (${mem_ticks} ticks) but a job is running — deferring pause to next tick"
            else
                log "memory pressure sustained (${mem_ticks} ticks) — pausing listeners (assignment gate)"
                stop_listeners
                touch "$MEM_PAUSED_MARKER"
            fi
        fi
    else
        rm -f "$MEM_TICKS_FILE"
        if [ -f "$MEM_PAUSED_MARKER" ]; then
            mem_normal=$(( $(cat "$MEM_NORMAL_FILE" 2>/dev/null || echo 0) + 1 ))
            echo "$mem_normal" > "$MEM_NORMAL_FILE"
            if [ "$mem_normal" -ge "$MEM_RESUME_TICKS" ]; then
                log "memory pressure normal for ${mem_normal} ticks — clearing memory pause"
                rm -f "$MEM_PAUSED_MARKER" "$MEM_NORMAL_FILE"
                if [ ! -f "$PAUSED_MARKER" ]; then
                    start_listeners
                else
                    log "disk pause still active — listeners stay down"
                fi
            fi
        else
            rm -f "$MEM_NORMAL_FILE"
        fi
    fi
}

main

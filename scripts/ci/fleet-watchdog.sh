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

PAUSED_MARKER="$STATE_DIR/listeners.paused"
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
    touch "$PAUSED_MARKER"
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
    rm -f "$PAUSED_MARKER"
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
            log "evicting cache key $oldest (root ${used_gb}G, cap ${CAP_GB}G, free $(free_gb)G)"
            rm -rf "$oldest"
        done
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

    prune_stale_keys
    evict_until 0  # steady-state cap enforcement

    free=$(free_gb)
    if [ "$free" -lt "$STOP_GB" ]; then
        if [ "$free" -lt "$HARD_STOP_GB" ]; then
            log "free ${free}G below hard floor ${HARD_STOP_GB}G — pausing listeners (even mid-job)"
            stop_listeners
        elif job_running; then
            log "free ${free}G below ${STOP_GB}G but a job is running — deferring pause to next tick"
        else
            log "free ${free}G below ${STOP_GB}G — pausing listeners"
            stop_listeners
        fi
        evict_until "$RESUME_GB"
        free=$(free_gb)
    fi

    if [ -f "$PAUSED_MARKER" ] && [ "$free" -ge "$RESUME_GB" ]; then
        log "free ${free}G cleared resume ceiling ${RESUME_GB}G — resuming listeners"
        start_listeners
    fi
}

main

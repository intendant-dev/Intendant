#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--" ]]; then
  shift
fi
if [[ "$#" -eq 0 ]]; then
  echo "usage: run-worker.sh -- <foreground worker command> [args...]" >&2
  exit 2
fi

install_root="${INTENDANT_CLOUD_INSTALL_ROOT:-$HOME/.local}"
nonce_file="$install_root/share/intendant-cloud/boot-nonce"
if [[ -r "$nonce_file" ]]; then
  boot_nonce="$(cat "$nonce_file")"
else
  boot_nonce="uncached"
fi
if [[ ! "$boot_nonce" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "invalid Intendant Cloud boot nonce" >&2
  exit 2
fi

runtime_root="${XDG_RUNTIME_DIR:-/tmp}/intendant-cloud-worker-$UID"
if [[ -L "$runtime_root" ]]; then
  echo "refusing symlinked Intendant Cloud runtime root: $runtime_root" >&2
  exit 2
fi
mkdir -p "$runtime_root"
chmod 0700 "$runtime_root"
runtime_parent="$runtime_root/$boot_nonce"
mkdir -p "$runtime_parent"
chmod 0700 "$runtime_parent"
task_root="$(mktemp -d "$runtime_parent/task.XXXXXXXX")"
chmod 0700 "$task_root"

# All identity-bearing Intendant state is task-local and outside the cached
# home directories. Credentials should be one-time and supplied only to the
# foreground command; this launcher never copies them to disk.
export XDG_CONFIG_HOME="$task_root/config"
export XDG_DATA_HOME="$task_root/data"
export XDG_CACHE_HOME="$task_root/cache"
export INTENDANT_HOME="$task_root/intendant"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$INTENDANT_HOME"

echo "Intendant Cloud worker runtime: $task_root" >&2
exec "$@"

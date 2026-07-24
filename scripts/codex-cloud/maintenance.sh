#!/usr/bin/env bash
set -euo pipefail

# Cached containers must not reuse a previous task's daemon identity, client
# certificate, enrollment token, or process metadata. The per-user suffix and
# symlink check prevent a world-writable /tmp entry from redirecting cleanup.
runtime_root="${XDG_RUNTIME_DIR:-/tmp}/intendant-cloud-worker-$UID"
if [[ -L "$runtime_root" ]]; then
  echo "refusing symlinked Intendant Cloud runtime root: $runtime_root" >&2
  exit 2
fi
if [[ -d "$runtime_root" ]]; then
  find "$runtime_root" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
fi
mkdir -p "$runtime_root"
chmod 0700 "$runtime_root"

repo_root="${CODEX_CLOUD_REPO_ROOT:-$PWD}"
"$repo_root/scripts/codex-cloud/setup.sh"

install_root="${INTENDANT_CLOUD_INSTALL_ROOT:-$HOME/.local}"
state_dir="$install_root/share/intendant-cloud"
mkdir -p "$state_dir"
chmod 0700 "$state_dir"

if [[ -r /proc/sys/kernel/random/uuid ]]; then
  tr -d '\n' < /proc/sys/kernel/random/uuid > "$state_dir/boot-nonce"
else
  python3 -c 'import uuid; print(uuid.uuid4(), end="")' > "$state_dir/boot-nonce"
fi
chmod 0600 "$state_dir/boot-nonce"

echo "Intendant Codex Cloud maintenance complete; task identity will be created at launch"

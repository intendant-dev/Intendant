#!/bin/bash
# Install the machine-wide rustc concurrency governor on a macOS box
# (run with sudo from a repo checkout:
#   sudo scripts/ci/install-governor-macos.sh [path/to/rustc-governor]).
#
# Builds nothing: takes a prebuilt binary (default target/release/rustc-governor,
# from `cargo build --release -p rustc-governor`). Idempotent: re-running
# upgrades the binary and tops up permit files for the *effective* config,
# and never overwrites an existing /usr/local/etc/intendant-governor.toml —
# that file is live operator state (the kill switch lives in it).
#
# Deliberately does NOT touch any account's ~/.cargo/config.toml: the
# governor stays inert until an account's cargo points at it, and the
# operator canaries the CI account before wiring their own. This script
# only prints the lines to add. Doc: scripts/ci/README.md, "Governor".
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "run with sudo" >&2
    exit 1
fi

BIN_SRC="${1:-target/release/rustc-governor}"
if [ ! -f "$BIN_SRC" ]; then
    echo "no governor binary at $BIN_SRC — build one first: cargo build --release -p rustc-governor" >&2
    exit 1
fi

LIB_BIN_DIR="/usr/local/lib/intendant-ci/bin"
CONF="/usr/local/etc/intendant-governor.toml"
PERMIT_DIR_DEFAULT="/usr/local/var/intendant-governor"

install -d -m 0755 "$LIB_BIN_DIR" /usr/local/etc
install -m 0755 "$BIN_SRC" "$LIB_BIN_DIR/rustc-governor"
echo "installed $LIB_BIN_DIR/rustc-governor"

if [ ! -f "$CONF" ]; then
    cat > "$CONF" <<'CONF_EOF'
# Machine-wide rustc concurrency governor (rustc-governor) — live config.
# Every governed rustc invocation re-reads this file, so edits take effect
# immediately: `enabled = false` is the kill switch (no listener restarts;
# in-flight waiters fail open within one poll tick). A missing or
# unparseable file also fails OPEN — builds run ungoverned, never break.
# Doc: scripts/ci/README.md ("Governor") in the Intendant repo.
enabled = true
permit_dir = "/usr/local/var/intendant-governor"
# Per-box sizing: local_reserved + ci_reserved = the machine-wide ceiling
# of concurrent rustc processes. This Mac (24GB, two CI listeners plus
# interactive agents): 1 local + 2 CI.
local_reserved = 1
ci_reserved = 2
ci_users = ["_intendant-ci", "ci"]
# real_rustc = "/absolute/path/to/rustc"   # optional; default resolution is
#                                          # $HOME/.cargo/bin/rustc, else PATH
CONF_EOF
    chmod 0644 "$CONF"
    echo "wrote $CONF"
else
    echo "keeping existing $CONF (live operator state — edit by hand)"
fi

# Pre-create the flock files for the *effective* config (an existing conf
# wins over the defaults above). Non-root accounts can flock(2) a 0644
# root-owned file through a read-only open, but cannot create files in the
# 0755 root dir — the installer owns creation. File names mirror
# crates/rustc-governor/src/permits.rs (interlock: change both together).
permit_dir=$(sed -n 's/^permit_dir[[:space:]]*=[[:space:]]*"\(.*\)".*$/\1/p' "$CONF" | tail -n 1)
local_n=$(sed -n 's/^local_reserved[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*$/\1/p' "$CONF" | tail -n 1)
ci_n=$(sed -n 's/^ci_reserved[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*$/\1/p' "$CONF" | tail -n 1)
permit_dir="${permit_dir:-$PERMIT_DIR_DEFAULT}"
local_n="${local_n:-1}"
ci_n="${ci_n:-2}"

install -d -m 0755 "$permit_dir"
create_lock_file() {
    [ -f "$1" ] || : > "$1"
    chown root:wheel "$1"
    chmod 0644 "$1"
}
i=0
while [ "$i" -lt "$local_n" ]; do
    create_lock_file "$permit_dir/permit-local-$i"
    i=$((i + 1))
done
i=0
while [ "$i" -lt "$ci_n" ]; do
    create_lock_file "$permit_dir/permit-ci-$i"
    i=$((i + 1))
done
create_lock_file "$permit_dir/demand-local"
create_lock_file "$permit_dir/demand-ci"
# Every governed account appends to (and rotates) the log: world-writable.
[ -f "$permit_dir/governor.log" ] || : > "$permit_dir/governor.log"
chown root:wheel "$permit_dir/governor.log"
chmod 0666 "$permit_dir/governor.log"
echo "permit dir ready: $permit_dir (local=$local_n ci=$ci_n)"

cat <<'NEXT_EOF'

The governor is installed but INERT: no account uses it until its cargo
config points at it. For each account to govern, add under the EXISTING
[build] table in that account's ~/.cargo/config.toml (keep the
rustc-wrapper = sccache line exactly as it is):

[build]
rustc = "/usr/local/lib/intendant-ci/bin/rustc-governor"

Rollout order (see scripts/ci/README.md "Governor"): the CI account
first, soak a day of green runs, then the operator account.

Notes:
- sccache hashes the compiler binary (the governor) into its cache keys:
  first enablement — and every governor upgrade — invalidates that
  account's sccache cache once.
- Kill switch: set `enabled = false` in /usr/local/etc/intendant-governor.toml
  (immediate, machine-wide); removing the rustc= line fully unwires an account.
- Watch it: tail -f /usr/local/var/intendant-governor/governor.log
NEXT_EOF

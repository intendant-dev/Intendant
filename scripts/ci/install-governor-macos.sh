#!/bin/bash
# Install the machine-wide rustc concurrency governor on a macOS box
# (run with sudo from a repo checkout:
#   sudo scripts/ci/install-governor-macos.sh [path/to/rustc-governor]).
#
# Builds nothing: takes a prebuilt binary (default target/release/rustc-governor,
# from `cargo build --release -p rustc-governor`). Idempotent: re-running
# upgrades the binary and tops up lock files for the *effective* config,
# and never overwrites an existing /usr/local/etc/intendant-governor.toml —
# that file is live operator state (the kill switch lives in it).
#
# Ordering is deliberate: config and lock assets FIRST, the binary LAST.
# Old binaries ignore unknown config keys and extra lock files, but a new
# binary must never run ahead of the root-minted files it gates on (a
# heavyweight link would degrade to ungated on a box that wanted the
# gate). Upgrades land during a quiescent interval anyway: an
# already-running old governor cannot retroactively acquire a gate it
# never knew about.
#
# Deliberately does NOT touch any account's ~/.cargo/config.toml: the
# governor stays inert until an account's cargo points at it (as
# `[build] rustc-wrapper`; the governor then runs sccache as its governed
# child, per the conf's wrap_with key), and the operator canaries the CI
# account before wiring their own. This script only prints the lines to
# set/remove. Doc: scripts/ci/README.md, "Governor".
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
# Heavyweight final links (bin/--test targets that emit link) additionally
# serialize through this many machine-GLOBAL slots — concurrent multi-GiB
# final links are what melt a small box, not ordinary rustc concurrency.
# 0 disables the link gate; absent, the binary defaults it to 1.
link_slots = 1
ci_users = ["_intendant-ci", "ci"]
# Governed invocations spawn `wrap_with <rustc> <args…>` (the sccache
# client) as a CHILD while the governor itself holds its locks until the
# child exits — the fds are close-on-exec, so no child (crucially, no
# daemonized sccache server) can inherit a flock. Unset, empty, or a
# missing path: the compiler runs directly (correct, just uncached).
wrap_with = "/opt/homebrew/bin/sccache"
CONF_EOF
    chmod 0644 "$CONF"
    echo "wrote $CONF"
else
    echo "keeping existing $CONF (live operator state — edit by hand)"
    if ! grep -q '^[[:space:]]*wrap_with[[:space:]]*=' "$CONF"; then
        cat <<'WRAP_EOF'
  NOTE: this conf predates the wrapper-chain flip and has no wrap_with key.
  The governor is now cargo's rustc-wrapper and runs sccache itself;
  without wrap_with, governed builds still work but run UNCACHED. Add to
  the conf by hand:
    wrap_with = "/opt/homebrew/bin/sccache"
  (Any leftover real_rustc line is inert and can be dropped: the governor
  now receives the compiler path from cargo as argv[1].)
WRAP_EOF
    fi
    if ! grep -q '^[[:space:]]*link_slots[[:space:]]*=' "$CONF"; then
        echo "  NOTE: no link_slots key — the binary defaults to 1 (heavyweight final"
        echo "  links serialize machine-wide). Add 'link_slots = N' to resize, 0 to disable."
    fi
fi

# Pre-create the flock files for the *effective* config (an existing conf
# wins over the defaults above) BEFORE the binary swap — see the ordering
# note up top. Non-root accounts can flock(2) a 0644 root-owned file
# through a read-only open, but cannot create files in the 0755 root dir —
# the installer owns creation. File names mirror
# crates/rustc-governor/src/permits.rs (interlock: change both together).
permit_dir=$(sed -n 's/^permit_dir[[:space:]]*=[[:space:]]*"\(.*\)".*$/\1/p' "$CONF" | tail -n 1)
local_n=$(sed -n 's/^local_reserved[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*$/\1/p' "$CONF" | tail -n 1)
ci_n=$(sed -n 's/^ci_reserved[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*$/\1/p' "$CONF" | tail -n 1)
link_n=$(sed -n 's/^link_slots[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*$/\1/p' "$CONF" | tail -n 1)
permit_dir="${permit_dir:-$PERMIT_DIR_DEFAULT}"
local_n="${local_n:-1}"
ci_n="${ci_n:-2}"
link_n="${link_n:-1}"   # the binary's default when the key is absent

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
i=0
while [ "$i" -lt "$link_n" ]; do
    create_lock_file "$permit_dir/link-$i"
    i=$((i + 1))
done
create_lock_file "$permit_dir/demand-local"
create_lock_file "$permit_dir/demand-ci"
# Every governed account appends to (and rotates) the log: world-writable.
[ -f "$permit_dir/governor.log" ] || : > "$permit_dir/governor.log"
chown root:wheel "$permit_dir/governor.log"
chmod 0666 "$permit_dir/governor.log"
echo "permit dir ready: $permit_dir (local=$local_n ci=$ci_n link=$link_n)"

# Binary last (see the ordering note up top).
install -m 0755 "$BIN_SRC" "$LIB_BIN_DIR/rustc-governor"
echo "installed $LIB_BIN_DIR/rustc-governor"

cat <<'NEXT_EOF'

The governor is installed but INERT: no account uses it until its cargo
config points at it. For each account to govern, set under the EXISTING
[build] table in that account's ~/.cargo/config.toml:

[build]
rustc-wrapper = "/usr/local/lib/intendant-ci/bin/rustc-governor"

replacing any `rustc-wrapper = ".../sccache"` line (the governor runs
sccache itself as a governed child, via the conf's wrap_with key), and
REMOVE any legacy `rustc = ".../rustc-governor"` line — the governor
receives the real compiler path from cargo as argv[1]; if cargo hands it
the governor itself, it refuses with exit 127 rather than exec-looping.

Rollout order (see scripts/ci/README.md "Governor"): the CI account
first, soak a day of green runs, then the operator account.

Notes:
- Cache keys are computed against the real rustc again (sccache is asked
  to run the real compiler, not the governor), so enablement and governor
  upgrades no longer invalidate the account's sccache cache.
- Kill switch: set `enabled = false` in /usr/local/etc/intendant-governor.toml
  (immediate, machine-wide); a disabled governor still execs the wrap_with
  chain, so caching survives the kill switch. Removing the rustc-wrapper=
  line fully unwires an account. `link_slots = 0` turns off only the
  heavyweight-link gate.
- Watch it: tail -f /usr/local/var/intendant-governor/governor.log
  (kind=link lines carry link_wait_ms; kind=link-done carries runtime_ms —
  the link-gate soak data.)
NEXT_EOF

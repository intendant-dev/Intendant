#!/bin/bash
# Create the dedicated macOS CI service account and provision its per-user
# toolchain (run with sudo from a repo checkout:
# sudo scripts/ci/setup-ci-account-macos.sh).
#
# Why: CI jobs executing as the operator's own account can read everything
# the operator can (~/.ssh, .env API keys, gh tokens, session stores) and
# inherit the operator's TCC grants. The Dell and Windows runners already
# run as dedicated non-admin ci users; this brings the Mac to parity.
# Migration of the listeners themselves is a separate, per-listener step
# (migrate-runner-macos.sh) — this script only prepares the account.
#
# Idempotent: re-running upgrades the job hooks and converges the toolchain
# pin; it never recreates an existing account and never overwrites an
# existing ~/.cargo/config.toml.
#
# The repo is public: this script auto-detects every host specific at run
# time and hardcodes none (the one committed name is the generic account
# `_intendant-ci` itself).
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "run with sudo" >&2
    exit 1
fi
if [ "$(uname -s)" != "Darwin" ]; then
    echo "macOS only (Linux hosts already run a dedicated ci user)" >&2
    exit 1
fi

CI_ACCOUNT="${INTENDANT_CI_ACCOUNT:-_intendant-ci}"
# Home placement: role accounts conventionally live OUTSIDE /Users (cf.
# Apple's own daemon accounts under /var/*). A /Users home implies a human
# loginwindow account — Spotlight indexing, user-template contents, showing
# up in sharing/backup surfaces — none of which a headless CI principal
# should have. /var/ci is on the Data volume (same free-space pool the
# watchdog measures), short (kind to unix-socket path limits in tests), and
# clearly machine-scoped.
CI_HOME="${INTENDANT_CI_HOME:-/var/ci}"
LIB_DIR="/usr/local/lib/intendant-ci"
HOOKS_LOG="/var/log/intendant-ci-hooks.log"
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

# Run a command as the CI account with its own HOME and a deterministic
# PATH (the account has no login shell profile to lean on).
ci_run() {
    # --chdir equivalent via sh -c: the invoker's cwd is typically inside
    # the operator's 700 home, where the CI account cannot even getcwd —
    # rustup and cargo abort on that. Run everything from the CI home.
    sudo -u "$CI_ACCOUNT" -H env HOME="$CI_HOME" \
        PATH="$CI_HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
        sh -c 'cd "$HOME" && exec "$@"' ci-run "$@"
}

echo "== account"
if dscl . -read "/Users/$CI_ACCOUNT" UniqueID 2>/dev/null | grep -qE 'UniqueID: [0-9]+'; then
    # Attribute-level check: a partially-created record (a failed prior
    # run) satisfies a bare `dscl -read` while `id` still fails.
    echo "account $CI_ACCOUNT already exists — leaving it as is"
else
    # Free UID in Apple's role-account range: sysadminctl -roleAccount
    # requires the underscore prefix and a UID in 450-499 (enforced by
    # sysadminctl itself — "Role account requires specified UID in 450-499
    # range", verified live on Darwin 25.4; older docs said 200-400). One
    # dscl snapshot, then a pure-shell scan.
    used_uids="$(dscl . -list /Users UniqueID | awk '{print $2}')"
    uid=""
    for candidate in $(seq 450 499); do
        if ! printf '%s\n' "$used_uids" | grep -qx "$candidate"; then
            uid="$candidate"
            break
        fi
    done
    if [ -z "$uid" ]; then
        echo "no free role-account UID in 450-499" >&2
        exit 1
    fi
    # Dedicated primary group (NOT staff, NOT admin); prefer gid == uid.
    used_gids="$(dscl . -list /Groups PrimaryGroupID | awk '{print $2}')"
    gid=""
    for candidate in "$uid" $(seq 450 499); do
        if ! printf '%s\n' "$used_gids" | grep -qx "$candidate"; then
            gid="$candidate"
            break
        fi
    done
    if [ -z "$gid" ]; then
        echo "no free role-account GID in 450-499" >&2
        exit 1
    fi
    if ! dseditgroup -o read "$CI_ACCOUNT" >/dev/null 2>&1; then
        dseditgroup -o create -i "$gid" -r "Intendant CI" "$CI_ACCOUNT"
    fi
    echo "creating role account $CI_ACCOUNT (uid $uid, gid $gid, home $CI_HOME)"
    # -roleAccount: hidden service account semantics. No -password: the
    # account gets no password material, so password login is impossible.
    sysadminctl -addUser "$CI_ACCOUNT" -roleAccount -UID "$uid" -GID "$gid" \
        -fullName "Intendant CI" -home "$CI_HOME" -shell /bin/bash
    # sysadminctl IGNORES -home and -shell for role accounts (prints
    # "Home argument is ignored… /var/empty", live on Darwin 25.4). The
    # LaunchDaemon injects HOME anyway, but the directory-services record
    # must agree with it for getpwuid-based resolution: set both directly.
    dscl . -create "/Users/$CI_ACCOUNT" NFSHomeDirectory "$CI_HOME"
    dscl . -create "/Users/$CI_ACCOUNT" UserShell /bin/bash
    # Belt and braces (role accounts are already hidden by UID range):
    dscl . -create "/Users/$CI_ACCOUNT" IsHidden 1
    # sysadminctl mints real PBKDF2 password material (ShadowHashData) even
    # with no -password argument (verified live on Darwin 25.4). It is
    # inert while the record has no AuthenticationAuthority, but a later
    # tool could add one — delete the hash so password auth has nothing to
    # verify against, ever. NB: deletes take the BARE attribute name; the
    # dsAttrTypeNative: prefix (which reads print) makes the delete a
    # silent no-op.
    dscl . -delete "/Users/$CI_ACCOUNT" ShadowHashData 2>/dev/null || true
    # Darwin 26 sysadminctl also mints a ShadowHash AuthenticationAuthority
    # for role accounts (observed live 2026-07-15; Darwin 25 did not). A
    # role account needs no authority entries at all — delete the attribute
    # so no password path can ever be re-derived.
    dscl . -delete "/Users/$CI_ACCOUNT" AuthenticationAuthority 2>/dev/null || true
fi

CI_GROUP="$(id -gn "$CI_ACCOUNT")"

echo "== home"
# Deliberately NOT createhomedir: a headless account needs none of the user
# template, and an otherwise-empty home keeps the hermeticity canary signal
# clean (anything that appears in it was put there by a job).
for dir in "$CI_HOME" \
    "$CI_HOME/.cache" \
    "$CI_HOME/.cache/intendant-ci" \
    "$CI_HOME/.cache/intendant-ci/target" \
    "$CI_HOME/Library" \
    "$CI_HOME/Library/Logs"; do
    install -d -o "$CI_ACCOUNT" -g "$CI_GROUP" -m 0750 "$dir"
done

echo "== rust toolchain"
# Pin = whatever the invoking host currently builds with. The workflows key
# the external cargo target caches by `rustc -V`, so pinning the same
# toolchain keeps one warm cache lineage per listener across the migration.
PIN="${INTENDANT_CI_RUST_VERSION:-}"
HOST_RUSTC_LINE=""
if [ -z "$PIN" ]; then
    inv="${SUDO_USER:-}"
    if [ -n "$inv" ]; then
        inv_home="$(dscl . -read "/Users/$inv" NFSHomeDirectory 2>/dev/null | awk '{print $2}')"
        for cand in "${inv_home:-/nonexistent}/.cargo/bin/rustc" /opt/homebrew/bin/rustc /usr/local/bin/rustc; do
            if [ -x "$cand" ]; then
                # rustup shims resolve their toolchain from $HOME — run as
                # the invoking user so the shim sees theirs.
                HOST_RUSTC_LINE="$(sudo -u "$inv" -H "$cand" -V 2>/dev/null || true)"
                if [ -n "$HOST_RUSTC_LINE" ]; then
                    break
                fi
            fi
        done
        if [ -z "$HOST_RUSTC_LINE" ]; then
            HOST_RUSTC_LINE="$(sudo -u "$inv" -H bash -lc 'rustc -V' 2>/dev/null || true)"
        fi
    fi
    if [ -z "$HOST_RUSTC_LINE" ]; then
        echo "could not detect the host toolchain (no rustc for ${inv:-<no SUDO_USER>});" >&2
        echo "re-run with INTENDANT_CI_RUST_VERSION=<x.y.z> sudo -E $0" >&2
        exit 1
    fi
    PIN="$(echo "$HOST_RUSTC_LINE" | awk '{print $2}')"
    case "$PIN" in
        *nightly* | *beta* | *dev*)
            echo "host toolchain '$HOST_RUSTC_LINE' is not a plain stable version;" >&2
            echo "re-run with an explicit INTENDANT_CI_RUST_VERSION=<toolchain>" >&2
            exit 1
            ;;
    esac
fi
echo "host toolchain: ${HOST_RUSTC_LINE:-<explicit override>} -> pinning $PIN for $CI_ACCOUNT"

if [ -x "$CI_HOME/.cargo/bin/rustup" ]; then
    echo "rustup already installed for $CI_ACCOUNT"
else
    installer="$CI_HOME/.rustup-init.sh"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o "$installer"
    chmod 0644 "$installer"
    ci_run sh "$installer" -y --no-modify-path --profile default --default-toolchain "$PIN"
    rm -f "$installer"
fi
# Converge on re-runs (the host toolchain may have moved since first setup).
ci_run "$CI_HOME/.cargo/bin/rustup" toolchain install "$PIN" --profile default
ci_run "$CI_HOME/.cargo/bin/rustup" default "$PIN"

echo "== cargo config"
CARGO_CONFIG="$CI_HOME/.cargo/config.toml"
# Detected OUTSIDE the seed-once branch: the supervised-server block below
# needs it on every run, and under `set -u` an idempotent re-run (config
# already present) otherwise dies on the unbound variable (live 2026-07-15).
sccache_bin=""
for cand in /opt/homebrew/bin/sccache /usr/local/bin/sccache; do
    if [ -x "$cand" ]; then
        sccache_bin="$cand"
        break
    fi
done
if [ -f "$CARGO_CONFIG" ]; then
    echo "keeping existing $CARGO_CONFIG"
else
    # Account-level jobs cap (scripts/ci/README.md, "Cargo parallelism
    # cap"): mirror the operator account's cap when one is set so the two
    # accounts obey the same doctrine, else the documented default.
    jobs=""
    if [ -n "${SUDO_USER:-}" ]; then
        inv_home="$(dscl . -read "/Users/${SUDO_USER}" NFSHomeDirectory 2>/dev/null | awk '{print $2}')"
        if [ -n "$inv_home" ] && [ -f "$inv_home/.cargo/config.toml" ]; then
            jobs="$(awk -F= '/^[[:space:]]*jobs[[:space:]]*=/ {gsub(/[^0-9]/, "", $2); print $2; exit}' \
                "$inv_home/.cargo/config.toml" 2>/dev/null || true)"
        fi
    fi
    jobs="${jobs:-6}"
    {
        echo "# Account-level cargo cap — scripts/ci/README.md, \"Cargo parallelism cap\"."
        echo "[build]"
        echo "jobs = $jobs"
        if [ -n "$sccache_bin" ]; then
            echo "rustc-wrapper = \"$sccache_bin\""
            # sccache's client/server rendezvous is one TCP port (default
            # 4226) for the whole machine: the CI client would attach to
            # the OPERATOR's server, which then can't read a 0750 /var/ci
            # toolchain — "failed to execute compile" (live 2026-07-10).
            # Give the CI account its own port and cache dir.
            echo ""
            echo "[env]"
            echo "SCCACHE_SERVER_PORT = \"4227\""
            echo "SCCACHE_DIR = \"$CI_HOME/.cache/sccache\""
        fi
    } > "$CARGO_CONFIG"
    chown "$CI_ACCOUNT:$CI_GROUP" "$CARGO_CONFIG"
    chmod 0644 "$CARGO_CONFIG"
    if [ -n "$sccache_bin" ]; then
        echo "seeded $CARGO_CONFIG (jobs = $jobs, rustc-wrapper = $sccache_bin)"
    else
        echo "seeded $CARGO_CONFIG (jobs = $jobs; no sccache on this host, wrapper omitted)"
    fi
fi

# ---- supervised sccache server (one per account, shared by listeners) ----
# Never rely on in-job server spawning: the cargo [env] port above does
# not reach every in-job sccache invocation (the rustc version probe
# failed on the default port, 2026-07-10), and a client racing a dying
# or job-reaped server reads a truncated response header ("failed to
# fill whole buffer" — cargo exit 101 within seconds). One
# launchd-supervised FOREGROUND server owns the account's port instead
# (SCCACHE_NO_DAEMON: a forked server dies with its launchd process
# group); job clients only ever connect. The per-listener .env mirrors
# the port/dir into job env (migrate-runner-macos.sh).
if [ -n "$sccache_bin" ]; then
    SCCACHE_LABEL="com.intendant.ci.sccache"
    SCCACHE_PLIST="/Library/LaunchDaemons/$SCCACHE_LABEL.plist"
    cat > "$SCCACHE_PLIST.tmp" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$SCCACHE_LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$sccache_bin</string>
  </array>
  <key>UserName</key><string>$CI_ACCOUNT</string>
  <key>GroupName</key><string>$CI_GROUP</string>
  <key>WorkingDirectory</key><string>$CI_HOME</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>$CI_HOME</string>
    <key>SCCACHE_START_SERVER</key><string>1</string>
    <key>SCCACHE_NO_DAEMON</key><string>1</string>
    <key>SCCACHE_SERVER_PORT</key><string>4227</string>
    <key>SCCACHE_DIR</key><string>$CI_HOME/.cache/sccache</string>
    <key>SCCACHE_IDLE_TIMEOUT</key><string>0</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$CI_HOME/Library/Logs/sccache-server.log</string>
  <key>StandardErrorPath</key><string>$CI_HOME/Library/Logs/sccache-server.log</string>
</dict>
</plist>
PLIST_EOF
    mv "$SCCACHE_PLIST.tmp" "$SCCACHE_PLIST"
    chown root:wheel "$SCCACHE_PLIST"
    chmod 0644 "$SCCACHE_PLIST"
    # Idempotent re-run: bootout the old instance first (brief server
    # blip; this script is a maintenance operation, not a hot path).
    launchctl bootout "system/$SCCACHE_LABEL" 2>/dev/null || true
    launchctl bootstrap system "$SCCACHE_PLIST"
    echo "bootstrapped $SCCACHE_LABEL ($sccache_bin, port 4227, foreground under launchd)"
fi

echo "== wasm-pack"
# Same convention as scripts/setup-macos.sh: cargo install pinned by the
# repo's .wasm-pack-version (build.rs skips WASM rebuilds under any other
# version). Non-fatal: CI legs only need wasm-pack when a PR ships stale
# committed WASM artifacts, so a failure here is a canary-visible gap, not
# a setup blocker.
WASM_PIN="$(tr -d '[:space:]' < "$REPO_ROOT/.wasm-pack-version")"
installed="$(ci_run "$CI_HOME/.cargo/bin/wasm-pack" --version 2>/dev/null | cut -d' ' -f2 || true)"
if [ "$installed" = "$WASM_PIN" ]; then
    echo "wasm-pack $installed already pinned"
else
    echo "installing wasm-pack $WASM_PIN as $CI_ACCOUNT (cargo install — takes a few minutes)..."
    force_flag=""
    if [ -n "$installed" ]; then
        force_flag="--force"
    fi
    if ! ci_run "$CI_HOME/.cargo/bin/cargo" install wasm-pack --version "$WASM_PIN" --locked $force_flag; then
        echo "WARNING: wasm-pack install failed — CANARY-VISIBLE GAP: a job that" >&2
        echo "needs a WASM rebuild (stale committed artifacts) will fail on this" >&2
        echo "account until you re-run this script successfully." >&2
    fi
fi

echo "== job hooks"
install -d -m 0755 "$LIB_DIR" "$LIB_DIR/hooks"
install -m 0755 "$HERE/hooks/job-started.sh" "$LIB_DIR/hooks/job-started.sh"
install -m 0755 "$HERE/hooks/job-completed.sh" "$LIB_DIR/hooks/job-completed.sh"
install -m 0644 "$HERE/hooks/hook-lib.sh" "$LIB_DIR/hooks/hook-lib.sh"
# The hooks run as the CI account but log under /var/log — pre-create the
# file so they can append without owning the directory.
touch "$HOOKS_LOG"
chown "$CI_ACCOUNT:$CI_GROUP" "$HOOKS_LOG"
chmod 0644 "$HOOKS_LOG"
echo "hooks installed to $LIB_DIR/hooks (log: $HOOKS_LOG)"

# Idempotent .env wiring (GitHub reads the runner root's .env at listener
# startup): a no-op on first setup — migrate-runner-macos.sh wires each
# runner dir as it moves in — but a re-run after migration re-points
# already-present dirs at upgraded hooks.
set_env_kv() {
    local env_file="$1" key="$2" val="$3"
    touch "$env_file"
    if grep -q "^${key}=" "$env_file"; then
        sed -i '' "s|^${key}=.*|${key}=${val}|" "$env_file"
    else
        printf '%s=%s\n' "$key" "$val" >> "$env_file"
    fi
}
for d in "$CI_HOME"/actions-runner-*/; do
    [ -d "$d" ] || continue
    set_env_kv "${d%/}/.env" ACTIONS_RUNNER_HOOK_JOB_STARTED "$LIB_DIR/hooks/job-started.sh"
    set_env_kv "${d%/}/.env" ACTIONS_RUNNER_HOOK_JOB_COMPLETED "$LIB_DIR/hooks/job-completed.sh"
    chown "$CI_ACCOUNT:$CI_GROUP" "${d%/}/.env"
    echo "wired hooks into ${d%/}/.env (listener restart required to take effect)"
done

echo "== verification"
fail=0
id "$CI_ACCOUNT"

if dseditgroup -o checkmember -m "$CI_ACCOUNT" admin 2>/dev/null | grep -q "^yes"; then
    echo "FAIL: $CI_ACCOUNT is in admin" >&2
    fail=1
else
    echo "ok: not in admin"
fi
# staff(20) membership is COMPUTED for every local account on macOS
# (opendirectoryd implicit membership — /Groups/staff lists only root;
# there is no per-user record to delete, verified live on Darwin 25.4).
# The effective boundary is 700 operator homes + no admin, checked below.
if dseditgroup -o checkmember -m "$CI_ACCOUNT" staff 2>/dev/null | grep -q "^yes"; then
    echo "note: staff membership is macOS-computed for all local accounts (not removable; boundary = home modes + no admin)"
else
    echo "ok: not in staff (primary group: $CI_GROUP)"
fi

# Password material: check the hash data itself, not just the authority
# list — sysadminctl mints ShadowHashData unasked. Idempotent runs
# converge (delete) rather than warn. dscl -read exits 0 for an existing
# RECORD whether or not the queried key exists (absent keys print "No
# such key"), so presence must be parsed from the output — anchored,
# because the No-such-key line also contains the attribute name.
shadow_present() {
    dscl . -read "/Users/$CI_ACCOUNT" dsAttrTypeNative:ShadowHashData 2>/dev/null \
        | grep -q '^dsAttrTypeNative:ShadowHashData:'
}
if shadow_present; then
    dscl . -delete "/Users/$CI_ACCOUNT" ShadowHashData 2>/dev/null || true
fi
auth_auth="$(dscl . -read "/Users/$CI_ACCOUNT" AuthenticationAuthority 2>/dev/null || true)"
# Converge rather than fail: newer Darwin mints this attribute (see the
# creation block); idempotent re-runs on hosts set up before the fix must
# repair it, not report it.
if echo "$auth_auth" | grep -q "ShadowHash"; then
    dscl . -delete "/Users/$CI_ACCOUNT" AuthenticationAuthority 2>/dev/null || true
    auth_auth="$(dscl . -read "/Users/$CI_ACCOUNT" AuthenticationAuthority 2>/dev/null || true)"
fi
if shadow_present; then
    echo "FAIL: $CI_ACCOUNT still has ShadowHashData after delete" >&2
    fail=1
elif echo "$auth_auth" | grep -q "ShadowHash"; then
    echo "FAIL: $CI_ACCOUNT has a ShadowHash AuthenticationAuthority" >&2
    fail=1
else
    echo "ok: no password material (no ShadowHashData, no ShadowHash authority)"
fi

# (logical pwd: /var is a symlink to /private/var on macOS, so a physical
# pwd would report /private/var/ci and false-fail the comparison)
resolved_home="$(ci_run sh -c 'cd "$HOME" && pwd')"
if [ "$resolved_home" = "$CI_HOME" ]; then
    echo "ok: HOME resolves to $resolved_home"
else
    echo "FAIL: HOME resolves to '$resolved_home', expected $CI_HOME" >&2
    fail=1
fi

# The privacy boundary this account exists for: it must not be able to
# traverse any human home. Report only — fixing a too-open home is the
# operator's call (expected mode: 700).
for home in /Users/*; do
    [ -d "$home" ] || continue
    owner_uid="$(stat -f %u "$home" 2>/dev/null || echo 0)"
    [ "$owner_uid" -ge 500 ] || continue
    mode="$(stat -f %Lp "$home" 2>/dev/null || echo '?')"
    if ci_run test -x "$home" 2>/dev/null; then
        echo "WARN: $CI_ACCOUNT can traverse $home (mode $mode) — the privacy boundary wants: chmod 700 $home" >&2
    else
        echo "ok: $home not traversable (mode $mode)"
    fi
done

echo "toolchain as $CI_ACCOUNT: $(ci_run rustc -V 2>/dev/null || echo 'rustc MISSING')"
echo "wasm-pack as $CI_ACCOUNT: $(ci_run wasm-pack --version 2>/dev/null || echo 'MISSING (see gap warning above)')"
echo "cargo config:"
sed 's/^/    /' "$CARGO_CONFIG"

if [ "$fail" -ne 0 ]; then
    echo "verification FAILED — fix the failures above before migrating a listener" >&2
    exit 1
fi
echo "account ready. Next: sudo scripts/ci/migrate-runner-macos.sh <listener-name>"

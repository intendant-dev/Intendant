#!/bin/sh
# Intendant installer.
# The canonical copy of this script is a per-tag GitHub RELEASE ASSET —
# stamped with the release it belongs to, sha256-committed to the public
# transparency log alongside the binaries, and immutable once published:
#   curl -fsSL https://github.com/intendant-dev/Intendant/releases/latest/download/install.sh | sh
# The copy in scripts/ is the unstamped source; a Connect rendezvous
# serves at most a redirect to the release asset, never the script body.
#
# Stands up a daemon and optionally links its route to Connect. The
# one-time claim code grants no daemon access and changes no IAM. Establish
# root separately through the machine's local console or direct mTLS. The
# packaged macOS app only bridges its own bundled local daemon; this
# installer never accepts an owner key.

set -eu

# ── Release identity ──
# Stamped by release.yml when this script is packaged as a release asset
# (empty in the repository copy). A stamped installer announces the
# release it belongs to and installs exactly that released tree: the
# checkout is verified against the stamped commit below and the install
# fails closed on mismatch. Its own bytes are covered by the release
# manifest committed to the transparency log, so the custody chain is
# log -> installer bytes -> pinned commit -> the tree that gets built.
INSTALLER_RELEASE_TAG=""
INSTALLER_RELEASE_COMMIT=""

usage() {
  cat <<'EOF'
Intendant installer

  curl -fsSL https://github.com/intendant-dev/Intendant/releases/latest/download/install.sh | sh -s -- \
    [--service] [--connect <rendezvous-url>] \
    [--daemon-id <id>] [--no-run]

Options:
  --service       Keep the daemon running unattended: installs a boot
                  service via the platform's native supervisor (systemd
                  where present, launchd on macOS, cron @reboot + the
                  built-in supervisor elsewhere) so it survives this SSH
                  session and restarts on failure.
  --connect <url> Rendezvous to register with for discovery. Default: the
                  environment's INTENDANT_CONNECT_RENDEZVOUS_URL, else
                  none (the daemon publishes no discovery route; its
                  local dashboard still works).
  --daemon-id <id>Stable daemon id at the rendezvous.
  --ref <ref>     Pin the fresh clone to a tag, branch, or commit.
                  Default: the release this installer was stamped with
                  (when fetched as a release asset); an unstamped copy
                  falls back to the newest published release tag (vX.Y.Z),
                  and to the default branch head only while no release
                  exists. An explicit ref you choose skips the
                  release-pin verification.
  --no-run        Build and link only; print how to start it.

Environment overrides:
  INTENDANT_REPO         git URL   (default: https://github.com/intendant-dev/Intendant)
  INTENDANT_INSTALL_DIR  checkout  (default: ~/intendant)
  INTENDANT_REF          same as --ref
EOF
}

REPO="${INTENDANT_REPO:-https://github.com/intendant-dev/Intendant}"
INSTALL_DIR="${INTENDANT_INSTALL_DIR:-$HOME/intendant}"
CONNECT_URL="${INTENDANT_CONNECT_RENDEZVOUS_URL:-}"
DAEMON_ID="${INTENDANT_CONNECT_DAEMON_ID:-}"
REF="${INTENDANT_REF:-}"
RUN=1
SERVICE=0

say() { printf '\033[1m[intendant install]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[intendant install]\033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --connect)
      [ $# -ge 2 ] || die "--connect requires a rendezvous URL"
      CONNECT_URL="$2"; shift 2 ;;
    --daemon-id)
      [ $# -ge 2 ] || die "--daemon-id requires a value"
      DAEMON_ID="$2"; shift 2 ;;
    --ref)
      [ $# -ge 2 ] || die "--ref requires a git ref (tag, branch, or commit)"
      REF="$2"; shift 2 ;;
    --service)
      SERVICE=1; shift ;;
    --no-run)
      RUN=0; shift ;;
    -h|--help)
      usage
      exit 0 ;;
    *)
      die "unknown argument: $1" ;;
  esac
done

# ── Identity banner ──
# Say what this copy IS before doing anything else.
if [ -n "$INSTALLER_RELEASE_COMMIT" ]; then
  say "release-pinned installer: $INSTALLER_RELEASE_TAG @ $INSTALLER_RELEASE_COMMIT"
else
  say "unstamped source copy (no release pin) — the canonical, verified installer is the GitHub release asset"
fi

# ── Platform ──
PLATFORM="$(uname -s)"
case "$PLATFORM" in
  Linux|Darwin) ;;
  MINGW*|MSYS*|CYGWIN*)
    die "this installer targets macOS/Linux. On Windows use install.ps1 from PowerShell:
    & ([scriptblock]::Create((irm https://github.com/intendant-dev/Intendant/releases/latest/download/install.ps1)))" ;;
  *)
    say "note: unrecognized platform $PLATFORM — continuing, but dependency setup is on you." ;;
esac
# --service needs no init-system check here: `intendant service install`
# detects the platform's supervisor itself (systemd / launchd / cron).

# ── Toolchain ──
# Only git is needed this early (for the clone). Rust may legitimately be
# missing on a fresh box — scripts/setup-linux.sh installs it below, so
# the hard requirement is enforced after dependency setup, not before it.
# git lives under the same self-sufficiency law, but the repo's setup
# scripts cannot carry it — they arrive BY the clone — and stock cloud
# images (Debian minimal, for one) ship without it. So the pre-flight
# installs git through the platform package manager, honestly: the exact
# command is announced before it runs, sudo is used only where it exists
# (prompting on its own terms), and with no root path the script stops
# and names the command for you — never a silent escalation.
if ! command -v git >/dev/null 2>&1; then
  GIT_NEEDS_ROOT=1
  if command -v apt-get >/dev/null 2>&1; then
    GIT_INSTALL="apt-get update && apt-get install -y git"
  elif command -v dnf >/dev/null 2>&1; then
    GIT_INSTALL="dnf install -y git"
  elif command -v yum >/dev/null 2>&1; then
    GIT_INSTALL="yum install -y git"
  elif command -v zypper >/dev/null 2>&1; then
    GIT_INSTALL="zypper --non-interactive install git"
  elif command -v pacman >/dev/null 2>&1; then
    GIT_INSTALL="pacman -Sy --noconfirm git"
  elif command -v apk >/dev/null 2>&1; then
    GIT_INSTALL="apk add git"
  elif command -v brew >/dev/null 2>&1; then
    # brew refuses to run as root and does its own escalation.
    GIT_INSTALL="brew install git"
    GIT_NEEDS_ROOT=0
  else
    die "git is required, and no supported package manager was found to install it (apt-get, dnf, yum, zypper, pacman, apk, brew) — install git yourself, then re-run this installer"
  fi
  if [ "$GIT_NEEDS_ROOT" = "1" ] && [ "$(id -u)" != "0" ]; then
    command -v sudo >/dev/null 2>&1 || die "git is required, this shell is not root, and there is no sudo — as root, run:
    $GIT_INSTALL
then re-run this installer"
    say "git is missing — installing it now: sudo sh -c '$GIT_INSTALL' (sudo may prompt for your password)"
    sudo sh -c "$GIT_INSTALL" || die "git install failed — run it yourself: sudo sh -c '$GIT_INSTALL' — then re-run this installer"
  else
    say "git is missing — installing it now: $GIT_INSTALL"
    sh -c "$GIT_INSTALL" || die "git install failed — run it yourself: $GIT_INSTALL — then re-run this installer"
  fi
  command -v git >/dev/null 2>&1 || die "the install reported success but git is still not on PATH — install git yourself, then re-run this installer"
fi

# ── Source ──
if [ -d "$INSTALL_DIR/.git" ]; then
  [ -z "$REF" ] || die "--ref pins fresh clones only; $INSTALL_DIR already exists — check out the ref there yourself"
  say "using existing checkout at $INSTALL_DIR (leaving it exactly as-is)"
  [ -z "$INSTALLER_RELEASE_COMMIT" ] || say "note: the stamped release pin ($INSTALLER_RELEASE_TAG) is not enforced on a checkout you already had"
else
  if [ -z "$REF" ]; then
    if [ -n "$INSTALLER_RELEASE_COMMIT" ]; then
      # A stamped release asset installs exactly its own release; the
      # tree is verified against the stamped commit after checkout.
      REF="$INSTALLER_RELEASE_TAG"
      say "installing the stamped release: $REF"
    else
      # Unstamped copy: default fresh installs to the newest published
      # release tag (vX.Y.Z only — pre-releases and peeled refs are
      # filtered) so even this path delivers an immutable, released tree.
      # Falling back to the mutable default-branch head happens only while
      # no release exists, and says so out loud. --ref / INTENDANT_REF
      # override either way.
      REF="$(git ls-remote --tags "$REPO" 'v*' 2>/dev/null \
        | sed -n 's|.*refs/tags/\(v[0-9][0-9]*\.[0-9][0-9.]*\)$|\1|p' \
        | sort -V | tail -n 1 || true)"
      if [ -n "$REF" ]; then
        say "pinning to the latest release tag: $REF (override with --ref)"
      else
        say "note: no release tags published yet — installing the default branch head (mutable; pin with --ref once releases exist)."
      fi
    fi
  elif [ -n "$INSTALLER_RELEASE_COMMIT" ] && [ "$REF" != "$INSTALLER_RELEASE_TAG" ]; then
    say "note: explicit ref $REF overrides the stamped release ($INSTALLER_RELEASE_TAG) — release-pin verification is skipped for a ref you chose"
  fi
  say "cloning $REPO -> $INSTALL_DIR"
  git clone --depth 1 "$REPO" "$INSTALL_DIR"
  if [ -n "$REF" ]; then
    say "pinning checkout to $REF"
    git -C "$INSTALL_DIR" fetch --depth 1 origin "$REF"
    git -C "$INSTALL_DIR" checkout --detach FETCH_HEAD
  fi
fi
cd "$INSTALL_DIR"

# ── Release-pin verification ──
# A stamped installer fails closed unless the tree it just checked out is
# the exact commit its release recorded — a moved tag, a substituted
# remote, or a tampered mirror all land here, BEFORE anything from the
# tree is executed. Everything the installer runs from here on (the
# setup scripts, the build) comes from the verified tree, and
# `cargo build --locked` extends the pinning to dependency hashes.
if [ -n "$INSTALLER_RELEASE_COMMIT" ] && [ "$REF" = "$INSTALLER_RELEASE_TAG" ]; then
  ACTUAL_COMMIT="$(git rev-parse HEAD)"
  if [ "$ACTUAL_COMMIT" != "$INSTALLER_RELEASE_COMMIT" ]; then
    die "RELEASE_PIN_MISMATCH: $INSTALLER_RELEASE_TAG checked out commit $ACTUAL_COMMIT, but this installer was published for $INSTALLER_RELEASE_COMMIT. Refusing to continue. Re-download the installer from the release page and compare the repository's tags before trusting either."
  fi
  say "release pin verified: $REF is commit $ACTUAL_COMMIT"
fi

# ── System dependencies ──
if [ "$PLATFORM" = "Linux" ] && command -v apt-get >/dev/null 2>&1 && [ -x scripts/setup-linux.sh ]; then
  say "installing system dependencies (scripts/setup-linux.sh)"
  ./scripts/setup-linux.sh || die "system dependency setup failed"
elif [ "$PLATFORM" = "Linux" ]; then
  say "note: no apt-get here — if the build fails on a missing native dep, install your distro's equivalents of the APT_PACKAGES list in scripts/setup-linux.sh (pkg-config, libclang, libvpx, libpipewire-0.3, libxcb + shm/randr)."
elif [ "$PLATFORM" = "Darwin" ] && [ -x scripts/setup-macos.sh ]; then
  say "checking system dependencies (scripts/setup-macos.sh)"
  ./scripts/setup-macos.sh || die "system dependency setup failed"
fi

# setup-linux.sh installs rustup when cargo is missing, but into its own
# shell — pick the env up here before insisting.
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
  . "$HOME/.cargo/env"
fi
command -v cargo >/dev/null 2>&1 || die "Rust is required. Install via https://rustup.rs then re-run:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"

# ── Build ──
# --locked: build exactly the committed Cargo.lock — a resolution that
# differs from what CI tested is a failure, not a fallback.
say "building release binaries (this takes a few minutes on a fresh box)"
cargo build --release --locked

BIN_DIR="$HOME/.local/bin"
mkdir -p "$BIN_DIR"
ln -sf "$INSTALL_DIR/target/release/intendant" "$BIN_DIR/intendant"
ln -sf "$INSTALL_DIR/target/release/intendant-runtime" "$BIN_DIR/intendant-runtime"
case ":$PATH:" in
  *":$BIN_DIR:"*) say "linked binaries into $BIN_DIR" ;;
  *) say "linked binaries into $BIN_DIR — not on PATH yet; add: export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

# ── Launch ──
set -- --no-tui
if [ -n "$CONNECT_URL" ]; then
  export INTENDANT_CONNECT_RENDEZVOUS_URL="$CONNECT_URL"
  [ -n "$DAEMON_ID" ] && export INTENDANT_CONNECT_DAEMON_ID="$DAEMON_ID"
  say "rendezvous: $CONNECT_URL"
else
  say "note: no --connect rendezvous URL — the daemon will not publish a discovery route (its local dashboard still works)."
fi

# Auto-open the dashboard once the daemon reports ready (install.ps1
# twin). `ctl dashboard-url` prints this boot's tokened loopback owner
# URL — certless, local-only, token rotating each boot — so the first
# dashboard needs no certificate enrollment. Backgrounded because the
# launches below `exec`; a subshell survives that. Only attempted where
# a GUI opener exists: a headless SSH box skips it silently, and the
# URL is reprintable any time with `intendant ctl dashboard-url`.
spawn_dashboard_opener() {
  OPENER=""
  if command -v open >/dev/null 2>&1; then OPENER="open"
  elif command -v xdg-open >/dev/null 2>&1; then OPENER="xdg-open"
  else return 0; fi
  (
    tries=0
    while [ "$tries" -lt 60 ]; do
      sleep 2
      URL="$("$INSTALL_DIR/target/release/intendant" ctl dashboard-url 2>/dev/null || true)"
      if [ -n "$URL" ]; then
        say "dashboard ready: $URL (token rotates each boot; reprint with: intendant ctl dashboard-url)"
        "$OPENER" "$URL" >/dev/null 2>&1 || true
        exit 0
      fi
      tries=$((tries + 1))
    done
  ) &
}

if [ "$SERVICE" = "1" ]; then
  # A daemon on a rented box must outlive this SSH session and restart
  # on failure. The binary itself picks the platform's supervisor
  # (systemd / launchd / cron @reboot + built-in supervisor) and prints
  # where the one-time claim code lands. The INTENDANT_CONNECT_* exports above
  # are captured into the service definition.
  if [ "$RUN" = "1" ]; then
    spawn_dashboard_opener
    exec "$INSTALL_DIR/target/release/intendant" service install --now -- "$@"
  else
    exec "$INSTALL_DIR/target/release/intendant" service install -- "$@"
  fi
elif [ "$RUN" = "1" ]; then
  say "starting the daemon — its one-time Connect code links discovery only and grants no access. Establish owner through this machine's local console or direct mTLS."
  spawn_dashboard_opener
  exec "$INSTALL_DIR/target/release/intendant" "$@"
else
  say "done. Start it with:"
  say "  $BIN_DIR/intendant $*"
fi

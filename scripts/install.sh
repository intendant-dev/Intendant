#!/bin/sh
# Intendant bootstrap installer (credential custody, rollout step 6).
# Served by every Intendant Connect rendezvous at /install.sh.
#
# Stands up a daemon that is OWNED from first boot and holds no secrets:
#   1. --owner pins root authority to your browser identity key (the
#      fingerprint is public — shown in the dashboard's Access drawer).
#   2. The daemon prints its claim phrase; claim it from the browser you
#      are already holding.
#   3. The first dashboard session fuels it with credential leases from
#      your vault. Nothing sensitive ever appears on this machine's disk,
#      in this command, or on the wire.

set -eu

usage() {
  cat <<'EOF'
Intendant bootstrap installer

  curl -fsSL https://intendant.dev/install.sh | sh -s -- \
    --owner <client-key-fingerprint> [--service] [--connect <rendezvous-url>] \
    [--daemon-id <id>] [--no-run]

Options:
  --owner <fp>    Pin root authority to your browser key from first boot
                  (the fingerprint is public — dashboard Access drawer).
  --service       Keep the daemon running unattended: installs a boot
                  service via the platform's native supervisor (systemd
                  where present, launchd on macOS, cron @reboot + the
                  built-in supervisor elsewhere) so it survives this SSH
                  session and restarts on failure.
  --connect <url> Rendezvous to register with. Default: the environment's
                  INTENDANT_CONNECT_RENDEZVOUS_URL, else the rendezvous
                  this script was fetched from (injected when served).
  --daemon-id <id>Stable daemon id at the rendezvous.
  --ref <ref>     Pin the fresh clone to a tag, branch, or commit.
                  Default: the newest published release tag (vX.Y.Z);
                  only when no release exists yet, the default branch head.
  --no-run        Build and link only; print how to start it.

Environment overrides:
  INTENDANT_REPO         git URL   (default: https://github.com/intendant-dev/Intendant)
  INTENDANT_INSTALL_DIR  checkout  (default: ~/intendant)
  INTENDANT_REF          same as --ref (lets a serving rendezvous pin a release)
EOF
}

REPO="${INTENDANT_REPO:-https://github.com/intendant-dev/Intendant}"
INSTALL_DIR="${INTENDANT_INSTALL_DIR:-$HOME/intendant}"
OWNER=""
CONNECT_URL="${INTENDANT_CONNECT_RENDEZVOUS_URL:-}"
DAEMON_ID="${INTENDANT_CONNECT_DAEMON_ID:-}"
REF="${INTENDANT_REF:-}"
RUN=1
SERVICE=0

say() { printf '\033[1m[intendant install]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[intendant install]\033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --owner)
      [ $# -ge 2 ] || die "--owner requires a client-key fingerprint"
      OWNER="$2"; shift 2 ;;
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

[ -n "$OWNER" ] || say "note: no --owner given — the daemon will start unowned; pass your client-key fingerprint (Access drawer) to own it from first boot."

# ── Platform ──
PLATFORM="$(uname -s)"
case "$PLATFORM" in
  Linux|Darwin) ;;
  MINGW*|MSYS*|CYGWIN*)
    die "this installer targets macOS/Linux. On Windows use install.ps1 from PowerShell:
    & ([scriptblock]::Create((irm https://intendant.dev/install.ps1))) -Owner <your-key>" ;;
  *)
    say "note: unrecognized platform $PLATFORM — continuing, but dependency setup is on you." ;;
esac
# --service needs no init-system check here: `intendant service install`
# detects the platform's supervisor itself (systemd / launchd / cron).

# ── Toolchain ──
# Only git is needed this early (for the clone). Rust may legitimately be
# missing on a fresh box — scripts/setup-linux.sh installs it below, so
# the hard requirement is enforced after dependency setup, not before it.
command -v git >/dev/null 2>&1 || die "git is required (install it and re-run)"

# ── Source ──
if [ -d "$INSTALL_DIR/.git" ]; then
  [ -z "$REF" ] || die "--ref pins fresh clones only; $INSTALL_DIR already exists — check out the ref there yourself"
  say "using existing checkout at $INSTALL_DIR (leaving it exactly as-is)"
else
  if [ -z "$REF" ]; then
    # Default fresh installs to the newest published release tag (vX.Y.Z
    # only — pre-releases and peeled refs are filtered) so the served
    # installer delivers an immutable, released tree. Falling back to the
    # mutable default-branch head happens only while no release exists,
    # and says so out loud. --ref / INTENDANT_REF override either way.
    REF="$(git ls-remote --tags "$REPO" 'v*' 2>/dev/null \
      | sed -n 's|.*refs/tags/\(v[0-9][0-9]*\.[0-9][0-9.]*\)$|\1|p' \
      | sort -V | tail -n 1 || true)"
    if [ -n "$REF" ]; then
      say "pinning to the latest release tag: $REF (override with --ref)"
    else
      say "note: no release tags published yet — installing the default branch head (mutable; pin with --ref once releases exist)."
    fi
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
if [ -n "$OWNER" ]; then
  set -- "$@" --owner "$OWNER"
fi
if [ -n "$CONNECT_URL" ]; then
  export INTENDANT_CONNECT_RENDEZVOUS_URL="$CONNECT_URL"
  [ -n "$DAEMON_ID" ] && export INTENDANT_CONNECT_DAEMON_ID="$DAEMON_ID"
  say "rendezvous: $CONNECT_URL"
else
  say "note: no --connect rendezvous URL — hosted claiming needs one (the daemon still serves its local dashboard)."
fi

if [ "$SERVICE" = "1" ]; then
  # A daemon on a rented box must outlive this SSH session and restart
  # on failure. The binary itself picks the platform's supervisor
  # (systemd / launchd / cron @reboot + built-in supervisor) and prints
  # where the claim phrase lands. The INTENDANT_CONNECT_* exports above
  # are captured into the service definition.
  if [ "$RUN" = "1" ]; then
    exec "$INSTALL_DIR/target/release/intendant" service install --now -- "$@"
  else
    exec "$INSTALL_DIR/target/release/intendant" service install -- "$@"
  fi
elif [ "$RUN" = "1" ]; then
  say "starting the daemon — it will print its claim phrase; claim it from your browser, then fuel it from the vault."
  exec "$INSTALL_DIR/target/release/intendant" "$@"
else
  say "done. Start it with:"
  say "  $BIN_DIR/intendant $*"
fi

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
# On the binary fast path the build is replaced by the same release's
# prebuilt pair, sha256-verified against sidecars published by that
# release and covered by the same logged manifest; --from-source keeps
# the fully source-built path.
INSTALLER_RELEASE_TAG=""
INSTALLER_RELEASE_COMMIT=""

usage() {
  cat <<'EOF'
Intendant installer

  curl -fsSL https://github.com/intendant-dev/Intendant/releases/latest/download/install.sh | sh -s -- \
    [--service] [--connect <rendezvous-url>] \
    [--daemon-id <id>] [--no-run] [--from-source]

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
                  falls back to the newest published release tag by
                  semver precedence (vX.Y.Z or vX.Y.Z-<prerelease> —
                  prerelease releases count), and to the default branch
                  head only while no release exists. An explicit ref you
                  choose skips the release-pin verification.
  --no-run        Build and link only; print how to start it.
  --from-source   Always build from source, skipping the prebuilt-binary
                  fast path (the fully source-verified install). Without
                  it, installing a release downloads the release's
                  sha256-verified binary pair when one is published for
                  this platform, and builds from source otherwise.

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
FROM_SOURCE=0

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
    --from-source)
      FROM_SOURCE=1; shift ;;
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

# ── Release-tag picker ──
# Reads candidate tags (vX.Y.Z or vX.Y.Z-<prerelease>) one per line and
# prints the highest-precedence one under semver 2.0 ordering: version
# core first, then release > prerelease, then prerelease identifiers
# (dot-separated; numeric identifiers compare numerically and rank below
# alphanumeric ones, alphanumeric compare ASCII-lexically, the shorter
# identifier set loses ties). Earlier copies filtered prerelease tags out
# entirely (and `sort -V` cannot order them per semver anyway) — but the
# published alphas ARE the product releases (GitHub marks them Latest),
# so an unstamped copy was silently regressing fresh installs to the
# stale v0.1.0.
semver_max_tag() {
  awk '
    function ident_cmp(x, y,   xn, yn) {
      xn = (x ~ /^[0-9]+$/); yn = (y ~ /^[0-9]+$/)
      if (xn && yn) { if (x + 0 == y + 0) return 0; return (x + 0 < y + 0) ? -1 : 1 }
      if (xn != yn) return xn ? -1 : 1
      if (x "" == y "") return 0
      return (x "" < y "") ? -1 : 1
    }
    function pre_cmp(a, b,   ai, bi, na, nb, i, r) {
      na = split(a, ai, "."); nb = split(b, bi, ".")
      for (i = 1; i <= na && i <= nb; i++) {
        r = ident_cmp(ai[i], bi[i]); if (r != 0) return r
      }
      if (na == nb) return 0
      return (na < nb) ? -1 : 1
    }
    function tag_cmp(a, b,   ac, bc, ap, bp, ai, bi, i, p) {
      ap = ""; bp = ""
      ac = substr(a, 2); bc = substr(b, 2)
      if ((p = index(ac, "-")) > 0) { ap = substr(ac, p + 1); ac = substr(ac, 1, p - 1) }
      if ((p = index(bc, "-")) > 0) { bp = substr(bc, p + 1); bc = substr(bc, 1, p - 1) }
      split(ac, ai, "."); split(bc, bi, ".")
      for (i = 1; i <= 3; i++) {
        if (ai[i] + 0 != bi[i] + 0) return (ai[i] + 0 < bi[i] + 0) ? -1 : 1
      }
      if (ap == "" && bp == "") return 0
      if (ap == "") return 1
      if (bp == "") return -1
      return pre_cmp(ap, bp)
    }
    NR == 1 || tag_cmp($0, best) > 0 { best = $0 }
    END { if (best != "") print best }
  '
}

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
      # release tag by semver precedence so even this path delivers an
      # immutable, released tree. Prerelease tags count (see the picker
      # above — the old vX.Y.Z-only filter regressed fresh installs to
      # v0.1.0); peeled ^{} refs stay excluded by the $-anchored match.
      # Falling back to the mutable default-branch head happens only while
      # no release exists, and says so out loud. --ref / INTENDANT_REF
      # override either way.
      REF="$(git ls-remote --tags "$REPO" 'v*' 2>/dev/null \
        | sed -n 's|.*refs/tags/\(v[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\(-[0-9A-Za-z.-][0-9A-Za-z.-]*\)\{0,1\}\)$|\1|p' \
        | semver_max_tag || true)"
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

# ── Binary fast path (stage) ──
# When the resolved ref IS a release tag, that release may publish a
# prebuilt `intendant` + `intendant-runtime` pair for this platform
# (Linux x86_64/aarch64: Intendant-<version>-linux-<arch>.tar.gz — a
# tar.gz so the executable bits survive): downloading it replaces the
# 12+ minute source compile. The trust story is unchanged — the checkout
# above stays pin-verified to the release commit, and the pair is
# sha256-verified against the .sha256 sidecar published by the same
# release, every asset of which is PGP-signed and committed to the
# public transparency log. --from-source opts out and keeps the fully
# source-verified build. Old releases without the pair, other
# architectures, a non-GitHub INTENDANT_REPO, or any verification
# failure fall back to the source build. macOS stays source-built here:
# no paired unix binary asset is published for it (the packaged app
# bundle carries the macOS binaries).
#
# Staging and smoke are two phases: the pair is downloaded, verified,
# and extracted HERE, but only run AFTER dependency setup below — on a
# fresh box the binaries need the runtime shared libraries setup
# installs (libvpx, pipewire, xcb) before they can execute at all.

# Owner/repo path of a github.com HTTPS remote (the only host the
# release-asset fast path can construct download URLs for); any other
# remote returns non-zero and the caller takes the source build.
github_repo_path() {
  case "$1" in
    https://github.com/*)
      _path="${1#https://github.com/}"
      _path="${_path%.git}"
      _path="${_path%/}"
      case "$_path" in
        */*/*) return 1 ;;
        */*) printf '%s\n' "$_path"; return 0 ;;
        *) return 1 ;;
      esac ;;
    *) return 1 ;;
  esac
}

# Lowercase sha256 of a file, via whichever tool this box has
# (sha256sum on Linux coreutils, shasum on macOS/BSD).
file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}' | tr 'A-F' 'a-f'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}' | tr 'A-F' 'a-f'
  else
    return 1
  fi
}

# First hex field of a `sha256sum`-format sidecar, lowercased.
sidecar_sha256() {
  awk 'NR==1 {print $1}' "$1" | tr 'A-F' 'a-f'
}

fetch_url() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    return 1
  fi
}

# Download <asset> (+ its .sha256 sidecar) from the release <tag> of
# github repo <path>, verify the digest, and leave the verified file at
# $FAST_TMP/<asset>. Non-zero = not available / failed verification; the
# caller takes the source build. A digest MISMATCH is the loud case: the
# download is deleted and the mismatch named.
fetch_release_asset() {
  _repo_path="$1"; _tag="$2"; _asset="$3"
  _base="https://github.com/${_repo_path}/releases/download/${_tag}"
  say "binary fast path: fetching ${_asset} from the ${_tag} release"
  if ! fetch_url "${_base}/${_asset}" "$FAST_TMP/${_asset}"; then
    return 1
  fi
  if ! fetch_url "${_base}/${_asset}.sha256" "$FAST_TMP/${_asset}.sha256"; then
    rm -f "$FAST_TMP/${_asset}"
    return 1
  fi
  _want="$(sidecar_sha256 "$FAST_TMP/${_asset}.sha256")"
  _got="$(file_sha256 "$FAST_TMP/${_asset}")" || _got=""
  if [ -z "$_want" ] || [ -z "$_got" ] || [ "$_want" != "$_got" ]; then
    rm -f "$FAST_TMP/${_asset}" "$FAST_TMP/${_asset}.sha256"
    say "WARNING: ${_asset} did not match its .sha256 sidecar (expected ${_want:-<none>}, got ${_got:-<none>}) — discarding the download and building from source instead"
    return 1
  fi
  say "verified ${_asset} against its .sha256 sidecar"
  return 0
}

stage_release_binaries() {
  [ "$FROM_SOURCE" = "0" ] || return 1
  [ "$PLATFORM" = "Linux" ] || return 1
  case "$REF" in v[0-9]*) ;; *) return 1 ;; esac
  FAST_ARCH="$(uname -m)"
  case "$FAST_ARCH" in x86_64|aarch64) ;; *) return 1 ;; esac
  FAST_REPO_PATH="$(github_repo_path "$REPO")" || return 1
  FAST_ASSET="Intendant-${REF#v}-linux-${FAST_ARCH}.tar.gz"
  FAST_TMP="$(mktemp -d)" || return 1
  if ! fetch_release_asset "$FAST_REPO_PATH" "$REF" "$FAST_ASSET"; then
    rm -rf "$FAST_TMP"
    say "note: no verified prebuilt binary pair for $REF on linux-$FAST_ARCH (releases before v0.2.0-alpha.6 ship none) — building from source"
    return 1
  fi
  mkdir -p "$INSTALL_DIR/target/release"
  if ! tar -xzf "$FAST_TMP/$FAST_ASSET" -C "$INSTALL_DIR/target/release" intendant intendant-runtime; then
    rm -f "$INSTALL_DIR/target/release/intendant" "$INSTALL_DIR/target/release/intendant-runtime"
    rm -rf "$FAST_TMP"
    say "WARNING: could not extract $FAST_ASSET — building from source instead"
    return 1
  fi
  rm -rf "$FAST_TMP"
  # tar preserves the executable bits; chmod defensively anyway.
  chmod +x "$INSTALL_DIR/target/release/intendant" "$INSTALL_DIR/target/release/intendant-runtime" 2>/dev/null || true
  return 0
}

BINARY_STAGED=0
if stage_release_binaries; then
  BINARY_STAGED=1
  say "staged the release's prebuilt binary pair — it is smoke-tested after dependency setup (--from-source forces a source build)"
fi

# ── System dependencies ──
if [ "$PLATFORM" = "Linux" ] && command -v apt-get >/dev/null 2>&1 && [ -x scripts/setup-linux.sh ]; then
  if [ "$BINARY_STAGED" = "1" ]; then
    # The setup run still matters on the binary path — it installs the
    # runtime shared libraries the prebuilt binaries load (libvpx,
    # pipewire, xcb) — but its build phase would waste the fast path, so
    # skip exactly that with the script's own --no-build flag.
    say "installing system dependencies (scripts/setup-linux.sh --no-build)"
    ./scripts/setup-linux.sh --no-build || die "system dependency setup failed"
  else
    say "installing system dependencies (scripts/setup-linux.sh)"
    ./scripts/setup-linux.sh || die "system dependency setup failed"
  fi
elif [ "$PLATFORM" = "Linux" ]; then
  say "note: no apt-get here — if the build fails on a missing native dep, install your distro's equivalents of the APT_PACKAGES list in scripts/setup-linux.sh (pkg-config, libclang, libvpx, libpipewire-0.3, libxcb + shm/randr)."
elif [ "$PLATFORM" = "Darwin" ] && [ -x scripts/setup-macos.sh ]; then
  say "checking system dependencies (scripts/setup-macos.sh)"
  ./scripts/setup-macos.sh || die "system dependency setup failed"
fi

# ── Binary fast path (smoke) ──
# The staged pair must actually run here before it is trusted: an
# old-glibc box ("GLIBC_x.y not found"), a wrong-architecture download,
# or a corrupt file all fail --version, and the honest answer is to say
# so, remove the pair, and build from source.
if [ "$BINARY_STAGED" = "1" ]; then
  if "$INSTALL_DIR/target/release/intendant" --version >/dev/null 2>&1; then
    say "prebuilt binaries verified and working — the source build is skipped"
  else
    say "WARNING: the prebuilt intendant binary does not run on this system (old glibc, wrong architecture, or a corrupt download) — removing it and building from source instead"
    rm -f "$INSTALL_DIR/target/release/intendant" "$INSTALL_DIR/target/release/intendant-runtime"
    BINARY_STAGED=0
  fi
fi

# Only the source build requires the Rust toolchain: the whole block is
# skipped when the verified prebuilt pair is in place.
if [ "$BINARY_STAGED" = "0" ]; then
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
  # Named bins mirror the release lane's proven build shape (release.yml's
  # binary jobs): a daemon install needs `intendant` + `intendant-runtime`
  # only, and naming them skips the wasm workspace members and
  # station-web's native graphics stack (~71 packages) that a bare
  # workspace build would compile — a configuration no CI leg gates —
  # while shrinking the install build. intendant-connect is the hosted
  # rendezvous service and is not part of a daemon install.
  say "building release binaries (this takes a few minutes on a fresh box)"
  cargo build --release --locked --bin intendant --bin intendant-runtime
fi

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

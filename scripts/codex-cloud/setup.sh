#!/usr/bin/env bash
set -euo pipefail

install_root="${INTENDANT_CLOUD_INSTALL_ROOT:-$HOME/.local}"
bin_dir="$install_root/bin"
libexec_dir="$install_root/libexec/intendant-cloud"
repo_root="${CODEX_CLOUD_REPO_ROOT:-$PWD}"
# File scope, not function-local: the EXIT trap that cleans it expands
# after install_downloaded_binary has returned, where a local would be
# unbound under set -u (and the download would leak).
downloaded=""
sccache_tmp=""

mkdir -p "$bin_dir" "$libexec_dir"
export PATH="$bin_dir:$PATH"

cleanup_downloads() {
  rm -f "${downloaded:-}"
  if [[ -n "${sccache_tmp:-}" && -d "$sccache_tmp" ]]; then
    rm -rf -- "$sccache_tmp"
  fi
}
trap cleanup_downloads EXIT

file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo "sha256sum or shasum is required to verify downloaded binaries" >&2
    return 2
  fi
}

install_downloaded_binary() {
  if [[ -z "${INTENDANT_CLOUD_BINARY_SHA256:-}" ]]; then
    echo "INTENDANT_CLOUD_BINARY_SHA256 is required with INTENDANT_CLOUD_BINARY_URL" >&2
    return 2
  fi

  local actual
  downloaded="$(mktemp)"
  curl --fail --silent --show-error --location \
    --proto '=https' --tlsv1.2 \
    "$INTENDANT_CLOUD_BINARY_URL" \
    --output "$downloaded"

  actual="$(file_sha256 "$downloaded")"

  if [[ "$actual" != "$INTENDANT_CLOUD_BINARY_SHA256" ]]; then
    echo "Intendant binary checksum mismatch" >&2
    return 2
  fi
  install -m 0755 "$downloaded" "$bin_dir/intendant"
}

SCCACHE_VERSION="0.15.0"

sccache_is_usable() {
  local binary="$1"
  local version major rest minor
  version="$("$binary" --version 2>/dev/null | awk '{print $2}')"
  version="${version#v}"
  major="${version%%.*}"
  rest="${version#*.}"
  minor="${rest%%.*}"
  [[ "$major" =~ ^[0-9]+$ && "$minor" =~ ^[0-9]+$ ]] \
    && (( major > 0 || minor >= 14 ))
}

install_prebuilt_sccache() {
  local target checksum asset archive actual extracted
  case "$(uname -s):$(uname -m)" in
    Linux:x86_64|Linux:amd64)
      target="x86_64-unknown-linux-musl"
      checksum="782d2b5dd7ae0a55ebe368ab258114d0928d019ac2d949ab85d5d02f3926709e"
      ;;
    Linux:aarch64|Linux:arm64)
      target="aarch64-unknown-linux-musl"
      checksum="3a6a3712b49da3d263bf2d30d702de4302793016019e800bfb81c0c69401d8f8"
      ;;
    *) return 1 ;;
  esac

  asset="sccache-v${SCCACHE_VERSION}-${target}.tar.gz"
  sccache_tmp="$(mktemp -d)" || return 1
  archive="$sccache_tmp/$asset"
  curl --fail --silent --show-error --location \
    --proto '=https' --tlsv1.2 \
    "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/$asset" \
    --output "$archive" || return 1
  actual="$(file_sha256 "$archive")" || return 1
  if [[ "$actual" != "$checksum" ]]; then
    echo "sccache $SCCACHE_VERSION checksum mismatch for $target" >&2
    return 1
  fi
  tar -xzf "$archive" -C "$sccache_tmp" || return 1
  extracted="$sccache_tmp/sccache-v${SCCACHE_VERSION}-${target}/sccache"
  [[ -f "$extracted" ]] || return 1
  install -m 0755 "$extracted" "$bin_dir/sccache" || return 1
  rm -rf -- "$sccache_tmp"
  sccache_tmp=""
  echo "installed checksum-verified sccache $SCCACHE_VERSION prebuilt for $target"
}

install_sccache() {
  local existing
  existing="$(command -v sccache || true)"
  if [[ -n "$existing" ]] && sccache_is_usable "$existing"; then
    echo "worker sccache prerequisite present ($("$existing" --version))"
    return 0
  fi
  if install_prebuilt_sccache; then
    return 0
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    echo "warning: no supported sccache prebuilt and cargo is unavailable" >&2
    return 1
  fi
  echo "warning: prebuilt sccache unavailable; compiling pinned $SCCACHE_VERSION fallback" >&2
  cargo install --locked --version "$SCCACHE_VERSION" --root "$install_root" sccache
}

build_checked_out_binary() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is required; select a Rust version in the Codex Cloud environment" >&2
    return 2
  fi
  if [[ ! -f "$repo_root/Cargo.toml" ]]; then
    echo "no Cargo.toml at CODEX_CLOUD_REPO_ROOT=$repo_root" >&2
    return 2
  fi
  cargo build --locked --release --bin intendant --manifest-path "$repo_root/Cargo.toml"
  install -m 0755 "$repo_root/target/release/intendant" "$bin_dir/intendant"
}

if [[ "${INTENDANT_CLOUD_SKIP_SCCACHE:-0}" != "1" ]]; then
  install_sccache || echo "warning: sccache installation failed; durable_sccache jobs will be unavailable" >&2
fi

if [[ -n "${INTENDANT_CLOUD_BINARY_URL:-}" ]]; then
  install_downloaded_binary
else
  build_checked_out_binary
fi

# Display slice (attach slice 3): the worker serves its virtual display
# over the attachment. Xvfb + xdpyinfo are what vision::launch_display
# requires; best-effort — a worker without them still attaches, and
# display_open answers with a clear "launch Xvfb" error naming the gap.
if command -v Xvfb >/dev/null 2>&1 && command -v xdpyinfo >/dev/null 2>&1; then
  echo "worker display prerequisites present (Xvfb, xdpyinfo)"
elif command -v apt-get >/dev/null 2>&1; then
  if apt-get install -y --no-install-recommends xvfb x11-utils 2>/dev/null \
    || sudo -n apt-get install -y --no-install-recommends xvfb x11-utils 2>/dev/null; then
    echo "installed worker display prerequisites (xvfb, x11-utils)"
  else
    echo "warning: could not install xvfb/x11-utils; worker display will be unavailable" >&2
  fi
else
  echo "warning: Xvfb/xdpyinfo missing and no apt-get; worker display will be unavailable" >&2
fi

script_root="$repo_root/scripts/codex-cloud"
if [[ ! -f "$script_root/run-worker.sh" ]]; then
  echo "missing $script_root/run-worker.sh" >&2
  exit 2
fi
install -m 0755 "$script_root/run-worker.sh" "$libexec_dir/run-worker.sh"

"$bin_dir/intendant" codex-cloud --help >/dev/null
echo "Intendant Codex Cloud bootstrap installed at $bin_dir/intendant"

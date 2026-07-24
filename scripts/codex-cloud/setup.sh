#!/usr/bin/env bash
set -euo pipefail

install_root="${INTENDANT_CLOUD_INSTALL_ROOT:-$HOME/.local}"
bin_dir="$install_root/bin"
libexec_dir="$install_root/libexec/intendant-cloud"
repo_root="${CODEX_CLOUD_REPO_ROOT:-$PWD}"

mkdir -p "$bin_dir" "$libexec_dir"

install_downloaded_binary() {
  if [[ -z "${INTENDANT_CLOUD_BINARY_SHA256:-}" ]]; then
    echo "INTENDANT_CLOUD_BINARY_SHA256 is required with INTENDANT_CLOUD_BINARY_URL" >&2
    return 2
  fi

  local downloaded actual
  downloaded="$(mktemp)"
  # EXIT, not RETURN: a set -e abort inside the function skips RETURN traps
  # and would leak the download.
  trap 'rm -f "$downloaded"' EXIT
  curl --fail --silent --show-error --location \
    --proto '=https' --tlsv1.2 \
    "$INTENDANT_CLOUD_BINARY_URL" \
    --output "$downloaded"

  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$downloaded" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$downloaded" | awk '{print $1}')"
  else
    echo "sha256sum or shasum is required to verify the Intendant binary" >&2
    return 2
  fi

  if [[ "$actual" != "$INTENDANT_CLOUD_BINARY_SHA256" ]]; then
    echo "Intendant binary checksum mismatch" >&2
    return 2
  fi
  install -m 0755 "$downloaded" "$bin_dir/intendant"
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

if [[ -n "${INTENDANT_CLOUD_BINARY_URL:-}" ]]; then
  install_downloaded_binary
else
  build_checked_out_binary
fi

script_root="$repo_root/scripts/codex-cloud"
if [[ ! -f "$script_root/run-worker.sh" ]]; then
  echo "missing $script_root/run-worker.sh" >&2
  exit 2
fi
install -m 0755 "$script_root/run-worker.sh" "$libexec_dir/run-worker.sh"

"$bin_dir/intendant" codex-cloud --help >/dev/null
echo "Intendant Codex Cloud bootstrap installed at $bin_dir/intendant"

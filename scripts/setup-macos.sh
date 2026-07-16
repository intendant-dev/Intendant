#!/usr/bin/env bash
#
# Intendant macOS dependency installer.
#
# Usage:
#   ./setup-macos.sh             # Install all dependencies and build
#   ./setup-macos.sh --check     # Check what's installed without changing anything
#
set -euo pipefail

die()  { echo "error: $*" >&2; exit 1; }
info() { echo ":: $*"; }
warn() { echo "!! $*" >&2; }
ok()   { echo "   ✓ $1"; }
miss() { echo "   ✗ $1 — $2"; }

ACTION="install"
NEEDS_REBOOT=false

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --check) ACTION="check"; shift ;;
            -h|--help) sed -n '3,8p' "$0" | sed 's/^# \?//'; exit 0 ;;
            *)       die "unknown option: $1" ;;
        esac
    done
}

# ── Checks ──────────────────────────────────────────────────────────────────

check_macos() {
    [[ "$(uname)" == "Darwin" ]] || die "this script is for macOS"
}

has_cmd() { command -v "$1" &>/dev/null; }

has_brew_pkg() { brew list --formula "$1" &>/dev/null 2>&1; }

# Check if a named audio device exists in the audio system
has_audio_device() {
    local name="$1"
    system_profiler SPAudioDataType 2>/dev/null | grep -q "$name"
}

# Legacy alias
has_blackhole() { has_audio_device "$1"; }

# ── Dependency definitions ──────────────────────────────────────────────────

# Core deps: needed for basic operation
check_core() {
    local all_ok=true

    echo ""
    echo "Core dependencies:"

    if has_cmd brew; then
        ok "Homebrew"
    else
        miss "Homebrew" "https://brew.sh"
        all_ok=false
    fi

    if has_cmd rustc && has_cmd cargo; then
        ok "Rust toolchain ($(rustc --version 2>/dev/null | cut -d' ' -f2))"
    else
        miss "Rust toolchain" "https://rustup.rs"
        all_ok=false
    fi

    # Xcode Command Line Tools provide the macOS SDK and Metal frameworks used
    # by native GPU crates such as wgpu.
    if xcrun --sdk macosx --show-sdk-path >/dev/null 2>&1; then
        ok "Xcode Command Line Tools"
    else
        miss "Xcode Command Line Tools" "xcode-select --install"
        all_ok=false
    fi

    if has_cmd bash; then
        ok "bash"
    else
        miss "bash" "should be pre-installed on macOS"
        all_ok=false
    fi

    # ripgrep — used by external agents (Codex, Claude Code) for code
    # search. Missing `rg` causes agents to fall back to slower paths.
    if has_cmd rg; then
        ok "ripgrep"
    else
        miss "ripgrep" "brew install ripgrep"
        all_ok=false
    fi

    # -sys crate link deps (parity with setup-linux.sh APT_PACKAGES:
    # pkg-config + libvpx-dev; opus for the audio stack). Without these a
    # plain `cargo build` dies in the crates' build scripts — a fresh CI
    # host proved it (env-libvpx-sys panic, 2026-07-15).
    if has_cmd pkg-config; then
        ok "pkg-config"
    else
        miss "pkg-config" "brew install pkgconf"
        all_ok=false
    fi
    local lib formula
    for lib in vpx:libvpx opus:opus; do
        formula="${lib#*:}"
        lib="${lib%%:*}"
        if pkg-config --exists "$lib" 2>/dev/null; then
            ok "lib$lib ($(pkg-config --modversion "$lib" 2>/dev/null))"
        else
            miss "lib$lib" "brew install $formula"
            all_ok=false
        fi
    done

    $all_ok
}

# Computer-use deps: needed for display interaction
check_computer_use() {
    local all_ok=true

    echo ""
    echo "Computer-use dependencies:"

    if has_cmd screencapture; then
        ok "screencapture (built-in)"
    else
        miss "screencapture" "should be pre-installed on macOS"
        all_ok=false
    fi

    # CU input injection is in-process (CGEvent) — no cliclick needed.

    $all_ok
}

# Audio routing deps: needed for spawn_live_audio (voice calls through apps).
# Browser-based voice (Gemini Live / OpenAI Realtime via WebRTC) works without these.
#
# Two modes:
#   1. Vortex Audio (preferred): HAL plugin with direct shm bridge. No system
#      default-device changes; apps open the Vortex device directly.
#   2. BlackHole (fallback): Virtual loopback via system default switching.
#      Simpler setup but changes system-wide audio defaults during calls.
check_audio() {
    local all_ok=true

    echo ""
    echo "Audio routing dependencies:"

    # Vortex Audio (preferred)
    if has_audio_device "Vortex Audio"; then
        ok "Vortex Audio (HAL plugin, direct shm)"
    else
        miss "Vortex Audio" "install Vortex guest tools (scripts/install-vortex-audio.sh)"

        if has_cmd SwitchAudioSource; then
            ok "SwitchAudioSource (BlackHole fallback)"
        else
            miss "SwitchAudioSource" "brew install switchaudio-osx"
            all_ok=false
        fi

        if has_cmd sox; then
            ok "sox (BlackHole fallback)"
        else
            miss "sox" "brew install sox"
            all_ok=false
        fi

        if has_blackhole "BlackHole 2ch"; then
            ok "BlackHole 2ch (fallback)"
        else
            miss "BlackHole 2ch" "brew install --cask blackhole-2ch (reboot required)"
            all_ok=false
        fi
        if has_blackhole "BlackHole 16ch"; then
            ok "BlackHole 16ch (fallback)"
        else
            miss "BlackHole 16ch" "brew install --cask blackhole-16ch (reboot required)"
            all_ok=false
        fi
    fi

    # TCC mic access
    echo ""
    echo "Audio permissions:"
    echo "   ⚠  macOS requires microphone permission for audio input."
    echo "      Launch Intendant.app from Finder (not SSH) and approve"
    echo "      the mic prompt on first run."

    $all_ok
}

# Recording deps
check_recording() {
    local all_ok=true

    echo ""
    echo "Recording dependencies:"

    if has_cmd ffmpeg; then
        ok "ffmpeg"
    else
        miss "ffmpeg" "brew install ffmpeg"
        all_ok=false
    fi

    $all_ok
}

check_managed_browser() {
    local all_ok=true

    echo ""
    echo "Managed browser workspace dependency:"

    local script_dir repo_root intendant_bin
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    repo_root="$script_dir/.."
    intendant_bin="$repo_root/target/release/intendant"

    if [[ -x "$intendant_bin" ]]; then
        if "$intendant_bin" setup browsers --check --print-path >/dev/null 2>&1; then
            ok "Chrome for Testing / managed Chromium"
        else
            miss "Chrome for Testing / managed Chromium" "$intendant_bin setup browsers"
            all_ok=false
        fi
    else
        local cache_root="${XDG_CACHE_HOME:-$HOME/Library/Caches}/intendant/browser-workspaces"
        if [[ -d "$cache_root" ]] && find "$cache_root" -type f \( -name "Google Chrome for Testing" -o -name "Chromium" -o -name "chrome" \) -print -quit 2>/dev/null | grep -q .; then
            ok "Chrome for Testing / managed Chromium"
        else
            miss "Chrome for Testing / managed Chromium" "build first, then run target/release/intendant setup browsers"
            all_ok=false
        fi
    fi

    $all_ok
}

# WASM build deps (required — build.rs auto-rebuilds WASM when source changes)
check_wasm() {
    echo ""
    echo "WASM build dependencies:"

    # Pinned to the version the committed wasm blobs were built with —
    # single-sourced from .wasm-pack-version (build.rs enforces the same
    # pin and skips rebuilds under any other version, so a mismatched
    # install silently ships stale WASM). `cargo install` resolves
    # outside our Cargo.lock, so the exact version is the pin.
    local script_dir wasm_pack_pin installed
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    wasm_pack_pin="$(tr -d '[:space:]' < "$script_dir/../.wasm-pack-version")"
    installed="$(wasm-pack --version 2>/dev/null | cut -d' ' -f2 || true)"

    if [ "$installed" = "$wasm_pack_pin" ]; then
        ok "wasm-pack $installed (pinned)"
    else
        if [ -n "$installed" ]; then
            miss "wasm-pack $installed != pin $wasm_pack_pin" "cargo install wasm-pack --version $wasm_pack_pin --locked --force"
            info "reinstalling wasm-pack at the pinned version..."
            cargo install wasm-pack --version "$wasm_pack_pin" --locked --force
        else
            miss "wasm-pack" "cargo install wasm-pack --version $wasm_pack_pin --locked"
            info "installing wasm-pack..."
            cargo install wasm-pack --version "$wasm_pack_pin" --locked
        fi
    fi
}

# ── Install ─────────────────────────────────────────────────────────────────

ensure_homebrew() {
    if has_cmd brew; then return; fi
    info "installing Homebrew..."
    /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    # Add to PATH for this session
    if [[ -f /opt/homebrew/bin/brew ]]; then
        eval "$(/opt/homebrew/bin/brew shellenv)"
    fi
}

ensure_rust() {
    if has_cmd rustc && has_cmd cargo; then return; fi
    info "installing Rust toolchain..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
}

brew_install() {
    local pkg="$1"
    if has_brew_pkg "$pkg" || has_cmd "$pkg"; then return; fi
    info "installing $pkg..."
    brew install "$pkg"
}

install_vortex_audio() {
    if has_audio_device "Vortex Audio"; then return; fi

    local script_dir
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    local vortex_tools="$script_dir/../vendor/vortex-guest-tools"
    local pkg="$vortex_tools/VortexGuestTools.pkg"
    local manifest="$vortex_tools/VortexGuestTools.pkg.manifest"

    if [[ ! -f "$pkg" ]]; then
        warn "Vortex guest tools not found at $pkg"
        warn "Audio routing will fall back to BlackHole."
        warn "To install Vortex: run scripts/update-vortex-pkg.sh"
        return
    fi

    # Verify the vendored pkg matches its committed integrity manifest before
    # we hand it to `sudo installer`. The pkg installs a LaunchDaemon and a
    # CoreAudio HAL plugin that run as root — any tamper here is full machine
    # compromise. Manifest is refreshed atomically by update-vortex-pkg.sh.
    if [[ ! -f "$manifest" ]]; then
        die "Vortex pkg present but manifest missing: $manifest"
    fi

    local expected actual
    expected="$(awk -F': ' '$1 == "pkg_sha256" { print $2; exit }' "$manifest")"
    [[ -n "$expected" ]] || die "manifest is missing pkg_sha256: $manifest"

    actual="$(shasum -a 256 "$pkg" | awk '{print $1}')"
    if [[ "$actual" != "$expected" ]]; then
        warn "Vortex pkg sha256 mismatch — refusing to install."
        warn "  expected: $expected"
        warn "  actual:   $actual"
        warn "  manifest: $manifest"
        die "Refresh via scripts/update-vortex-pkg.sh or verify the pkg manually."
    fi
    ok "Vortex pkg integrity verified ($expected)"

    info "installing Vortex guest tools..."
    sudo installer -pkg "$pkg" -target /
    NEEDS_REBOOT=true
}

# Disable screensaver, screen lock, and display/system sleep.
#
# An intendant host operates the desktop autonomously; a screensaver or
# screen lock would interrupt the agent and hide the desktop from the
# captured stream (ScreenCaptureKit keeps working, but shows the lock
# screen, not the content). Display sleep is equally disruptive: after
# the display turns off, macOS starts a separate lock-after-sleep timer
# (`sysadminctl -screenLock`, default 300s), so leaving displaysleep on
# silently re-enables the lock behavior we're trying to disable.
#
# Idempotent: `defaults write` and `pmset` overwrite and never error on
# no-change.
disable_screen_lock() {
    echo ""
    info "disabling screensaver password prompt and idle timer (per-user)..."
    defaults write com.apple.screensaver askForPassword -int 0
    defaults write com.apple.screensaver askForPasswordDelay -int 0
    defaults -currentHost write com.apple.screensaver idleTime -int 0
    ok "per-user screensaver disabled"

    info "disabling display sleep and system sleep (requires sudo)..."
    if sudo -n pmset -a displaysleep 0 sleep 0 >/dev/null 2>&1; then
        ok "displaysleep=0 sleep=0 (system-wide)"
    else
        warn "could not run pmset without a password prompt — run manually:"
        echo "         sudo pmset -a displaysleep 0 sleep 0"
    fi

    ok "to apply screensaver settings immediately: killall cfprefsd"
}

install_blackhole() {
    local need_2ch=false need_16ch=false

    has_blackhole "BlackHole 2ch"  || need_2ch=true
    has_blackhole "BlackHole 16ch" || need_16ch=true

    if ! $need_2ch && ! $need_16ch; then return; fi

    $need_2ch  && { info "installing BlackHole 2ch (virtual mic)...";  brew install --cask blackhole-2ch;  }
    $need_16ch && { info "installing BlackHole 16ch (app capture)..."; brew install --cask blackhole-16ch; }

    NEEDS_REBOOT=true
}

install_managed_browser() {
    local script_dir repo_root intendant_bin
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    repo_root="$script_dir/.."
    intendant_bin="$repo_root/target/release/intendant"

    if [[ ! -x "$intendant_bin" ]]; then
        warn "cannot install managed browser; missing $intendant_bin"
        warn "after building, run: target/release/intendant setup browsers"
        return
    fi

    info "installing managed Chrome for Testing browser..."
    if "$intendant_bin" setup browsers; then
        ok "managed browser ready"
    else
        warn "managed browser install failed; browser workspaces need one of:"
        warn "  target/release/intendant setup browsers"
        warn "  INTENDANT_BROWSER_WORKSPACE_EXECUTABLE=/path/to/chrome"
        warn "  provider=system_cdp for an explicit system-browser launch"
    fi
}

build_intendant() {
    info "building intendant (release)..."
    local script_dir
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    local repo_root="$script_dir/.."

    cd "$repo_root"
    cargo build --release --locked

    local bin_dir="$repo_root/target/release"
    echo ""
    ok "intendant          → $bin_dir/intendant"
    ok "intendant-runtime  → $bin_dir/intendant-runtime"
}

# ── Main ────────────────────────────────────────────────────────────────────

run_check() {
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Intendant macOS Dependency Check"
    echo "════════════════════════════════════════════════════════"

    local core_ok cu_ok audio_ok rec_ok browser_ok
    check_core         && core_ok=true  || core_ok=false
    check_computer_use && cu_ok=true    || cu_ok=false
    check_audio        && audio_ok=true || audio_ok=false
    check_recording    && rec_ok=true   || rec_ok=false
    check_managed_browser && browser_ok=true || browser_ok=false

    check_wasm

    echo ""
    echo "────────────────────────────────────────────────────────"

    if $core_ok && $cu_ok; then
        echo "  Core + computer-use: ready"
    else
        echo "  Core + computer-use: missing dependencies"
    fi

    if $audio_ok; then
        echo "  Audio routing: ready"
    else
        echo "  Audio routing: missing dependencies"
    fi

    if $rec_ok; then
        echo "  Recording: ready"
    else
        echo "  Recording: missing dependencies"
    fi

    if $browser_ok; then
        echo "  Browser workspaces: ready"
    else
        echo "  Browser workspaces: missing managed browser"
    fi

    echo ""
}

run_install() {
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Intendant macOS Setup"
    echo "════════════════════════════════════════════════════════"

    # Phase 1: Core
    info "checking core dependencies..."
    ensure_homebrew
    ensure_rust

    # Phase 2: Homebrew packages
    info "installing Homebrew packages..."
    brew_install pkgconf
    brew_install libvpx
    brew_install opus
    brew_install ffmpeg
    brew_install switchaudio-osx
    brew_install sox

    # Phase 3: Audio routing
    # Try Vortex first (preferred), fall back to BlackHole
    install_vortex_audio
    if ! has_audio_device "Vortex Audio"; then
        install_blackhole
    fi

    # Phase 4: Build
    echo ""
    build_intendant

    # Phase 5: Managed browser for CDP browser workspaces
    install_managed_browser

    # Phase 6: Disable screensaver so the agent isn't interrupted
    disable_screen_lock

    # Phase 7: App bundle
    echo ""
    info "building macOS app bundle..."
    if [ -f scripts/bundle-macos.sh ]; then
        bash scripts/bundle-macos.sh
    fi

    # Phase 8: Final status
    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  Setup complete!"
    echo "════════════════════════════════════════════════════════"
    echo ""

    if $NEEDS_REBOOT; then
        warn "Reboot required before audio routing will work."
        echo "   Audio drivers were installed but need a reboot to load."
        echo "   You may also need to allow the system extension in"
        echo "   System Settings → Privacy & Security."
        echo ""
    fi

    echo "  IMPORTANT: Launch from the macOS GUI for audio to work:"
    echo ""
    echo "    open target/Intendant.app --args --web"
    echo ""
    echo "  macOS requires GUI session for audio input. Do NOT run"
    echo "  from SSH — use the app bundle, Finder, or Terminal.app"
    echo "  inside the VM's display."
    echo ""
    echo "  On first launch, approve the microphone permission prompt."
    echo ""
}

main() {
    parse_args "$@"
    check_macos

    case "$ACTION" in
        check)   run_check ;;
        install) run_install ;;
    esac
}

main "$@"

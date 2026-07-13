#!/bin/bash
# Build intendant as a native macOS desktop app.
#
# **On macOS this is the required build path for anything that uses
# display capture, input injection, microphone, or camera.** Running
# `cargo build --release` and launching `target/release/intendant`
# directly produces an ad-hoc-signed binary; macOS TCC refuses to
# grant Screen Recording / Accessibility / Microphone / Camera to
# ad-hoc binaries (and silently re-invalidates any grant on the next
# rebuild because the cdhash changes), so features gated on those
# capabilities just fail with opaque "permission denied" errors at
# runtime. This script solves that by:
#
#   1. Compiling the Rust binaries (intendant + intendant-runtime).
#   2. Compiling a small Swift wrapper (macos-app/*.swift) that
#      hosts a WKWebView loading the dashboard and spawns the Rust
#      daemon as a child — so TCC grants to the .app flow through
#      to the daemon (in-process CGEvent/AX computer use) and its
#      subprocesses (ffmpeg, screencapture, etc.) via inheritance.
#   3. Code-signing with a stable local identity stored in
#      ~/.intendant/signing.keychain-db. A cert-based Designated
#      Requirement survives rebuilds, so a one-time TCC grant keeps
#      working across `./scripts/bundle-macos.sh` re-runs. (Ad-hoc
#      signing would re-prompt every rebuild.) The identity is escrowed
#      to ~/.intendant/signing-identity.p12: TCC keys every grant to the
#      signing cert, so a lost keychain must re-import the same cert —
#      silently minting a new one voids all grants (see the signing
#      section below and the DR drift check after it).
#   4. Installing the bundle to /Applications/Intendant.app and
#      refreshing LaunchServices. This is the only location a few
#      pieces of the stack (Claude Code's computer-use MCP, Dock
#      quick-launch, Spotlight, `open -b com.intendant.app`)
#      consistently recognise as "installed". Set `INSTALL_APP=0`
#      to skip this step (build-only, for CI-ish runs).
#
# Headless Linux builds that don't need any TCC-gated capability
# can continue using plain `cargo build --release`; this script is
# macOS-specific.
#
# Usage:
#   ./scripts/bundle-macos.sh          # Release build + install
#   ./scripts/bundle-macos.sh debug    # Debug build + install
#   INSTALL_APP=0 ./scripts/bundle-macos.sh   # Build only
#
# Release signing seam (all optional; when none of these are set the script
# produces the local-dev bundle exactly as before):
#
#   INTENDANT_SIGN_IDENTITY    Codesign identity name, e.g.
#                              "Developer ID Application: Jane Doe (TEAMID)".
#                              Setting it activates the distribution path:
#                              non-system dylibs are bundled into
#                              Contents/Frameworks and everything is signed
#                              inside-out with the hardened runtime, a secure
#                              timestamp, and macos-app/entitlements.plist.
#   INTENDANT_SIGN_KEYCHAIN    Optional keychain holding that identity (a CI
#                              throwaway keychain). It must already be
#                              unlocked and in the keychain search list —
#                              codesign does not find identities in
#                              out-of-search-list keychains (verified
#                              empirically; --keychain alone is not enough).
#   INTENDANT_NOTARY_KEY_FILE  App Store Connect API private key (.p8) for
#                              `notarytool`.
#   INTENDANT_NOTARY_KEY_ID    Key ID of that API key.
#   INTENDANT_NOTARY_ISSUER    Issuer ID of that API key. The three notary
#                              variables are all-or-nothing, and require
#                              INTENDANT_SIGN_IDENTITY: partial configuration
#                              is a hard error (a release must never silently
#                              ship less signed than the operator intended).
#   INTENDANT_ARTIFACT_DIR     When set, stage a versioned
#                              Intendant-<version>-macos-<arch>.zip plus a
#                              .sha256 checksum file there. Builds without
#                              INTENDANT_SIGN_IDENTITY are suffixed
#                              "-unsigned-dev" so nobody mistakes a local
#                              bundle for a release.
#   INTENDANT_APP_VERSION      Version stamp override (the release workflow
#                              passes the git tag). Default: `git describe
#                              --tags --match "v*"`, so dev builds get
#                              "0.0.0-<sha>"-style versions.
#
# Output:
#   target/Intendant.app           (always — staged build)
#   /Applications/Intendant.app    (when INSTALL_APP=1, the default)
#
# Launch after: `open -b com.intendant.app`

set -euo pipefail

BUNDLE_ID="com.intendant.app"

PROFILE="${1:-release}"

die() {
    echo "Error: $*" >&2
    exit 1
}

# --- Release-signing configuration (validated before the long build) -------

SIGN_RELEASE_IDENTITY="${INTENDANT_SIGN_IDENTITY:-}"
SIGN_RELEASE_KEYCHAIN="${INTENDANT_SIGN_KEYCHAIN:-}"
NOTARY_KEY_FILE="${INTENDANT_NOTARY_KEY_FILE:-}"
NOTARY_KEY_ID="${INTENDANT_NOTARY_KEY_ID:-}"
NOTARY_ISSUER="${INTENDANT_NOTARY_ISSUER:-}"

# Notary env is all-or-nothing: a partially configured release run must fail
# loudly, not quietly ship an un-notarized artifact.
notary_env_state() {
    local n=0
    [ -n "$NOTARY_KEY_FILE" ] && n=$((n + 1))
    [ -n "$NOTARY_KEY_ID" ] && n=$((n + 1))
    [ -n "$NOTARY_ISSUER" ] && n=$((n + 1))
    case "$n" in
        0) echo "none" ;;
        3) echo "all" ;;
        *) echo "partial" ;;
    esac
}

NOTARY_STATE="$(notary_env_state)"
case "$NOTARY_STATE" in
    partial)
        die "partial notarization config: INTENDANT_NOTARY_KEY_FILE, INTENDANT_NOTARY_KEY_ID, and INTENDANT_NOTARY_ISSUER must all be set together (or none)"
        ;;
    all)
        [ -n "$SIGN_RELEASE_IDENTITY" ] \
            || die "notarization requires INTENDANT_SIGN_IDENTITY (Apple only notarizes Developer ID-signed bundles)"
        [ -f "$NOTARY_KEY_FILE" ] \
            || die "INTENDANT_NOTARY_KEY_FILE does not exist: $NOTARY_KEY_FILE"
        ;;
esac

# --- Version stamp ----------------------------------------------------------
# Release builds get the tag (workflow passes INTENDANT_APP_VERSION=v1.2.3);
# dev builds derive from `git describe` against v* tags only — this repo also
# carries non-version tags (bench-pilot-*) that must not leak into versions.
# With no v* tag in history the describe output is a bare commit hash, which
# gets a "0.0.0-" prefix so CFBundleShortVersionString stays ordered and
# recognizably a dev stamp.
APP_VERSION="${INTENDANT_APP_VERSION:-}"
if [ -z "$APP_VERSION" ]; then
    APP_VERSION="$(git describe --tags --match 'v*' --always --dirty 2>/dev/null || true)"
fi
[ -n "$APP_VERSION" ] || APP_VERSION="0.0.0-dev"
APP_VERSION="${APP_VERSION#v}"
case "$APP_VERSION" in
    # Tag-derived versions always contain a dot; a no-v-tag `git describe
    # --always` yields a bare (dotless) commit hash, optionally "-dirty".
    *.*) : ;;
    *) APP_VERSION="0.0.0-${APP_VERSION}" ;;
esac
echo "App version: $APP_VERSION"

# --- Build ------------------------------------------------------------------

if [ "$PROFILE" = "debug" ]; then
    BINARY="target/debug/intendant"
    RUNTIME="target/debug/intendant-runtime"
    cargo build --bin intendant --bin intendant-runtime
else
    BINARY="target/release/intendant"
    RUNTIME="target/release/intendant-runtime"
    cargo build --release --bin intendant --bin intendant-runtime
fi

APP="target/Intendant.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
FRAMEWORKS="$CONTENTS/Frameworks"

# Where the freshly-built bundle gets installed at the end of this
# script. `/Applications` is the only install location a few tools
# (Claude Code's computer-use MCP, Dock quick-launch, Spotlight)
# consistently recognise — a bundle living only in `target/` gets
# rejected as "not installed" even after LaunchServices sees it.
# Set `INSTALL_APP=0` to skip the install step (build-only).
INSTALLED_APP="/Applications/Intendant.app"
INSTALL_APP="${INSTALL_APP:-1}"

LS=/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

# Unregister any stale Intendant.app bundles from other worktrees or Trash.
# Multiple bundles with the same CFBundleIdentifier cause macOS LaunchServices
# to launch the wrong one (possibly an old worktree build from days ago).
# Only when installing: this cleanup exists to disambiguate *launching*, and a
# build-only run (INSTALL_APP=0 — CI, release packaging) must not delete other
# worktrees' staged bundles on a shared machine.
if [ "$INSTALL_APP" = "1" ]; then
    while IFS= read -r stale_path; do
        # Skip the current target (this build's output) AND the canonical
        # install destination — both are expected to hold an Intendant.app
        # at the end of this script, and the install step below overwrites
        # `/Applications` in place rather than deleting it first.
        if [ "$stale_path" != "$PROJECT_ROOT/$APP" ] && [ "$stale_path" != "$INSTALLED_APP" ]; then
            "$LS" -u "$stale_path" 2>/dev/null || true
            rm -rf "$stale_path" 2>/dev/null || true
        fi
    done < <("$LS" -dump 2>/dev/null | grep -o '/[^ ]*Intendant\.app' | sort -u)
fi

rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES"

# Compile Swift wrapper (main.swift must stay first: with multiple input
# files, swiftc only allows top-level code in a file named main.swift)
echo "Compiling macOS app wrapper..."
swiftc -O -o "$MACOS/Intendant" macos-app/main.swift macos-app/BackendSupervisor.swift \
    macos-app/UpdateChecker.swift \
    -framework Cocoa -framework WebKit

# Copy Rust binaries
cp "$BINARY" "$MACOS/intendant-bin"
cp "$RUNTIME" "$MACOS/intendant-runtime"

# Copy app icon
if [ -f "macos-app/AppIcon.icns" ]; then
    cp "macos-app/AppIcon.icns" "$RESOURCES/AppIcon.icns"
fi

# Info.plist — written before signing, so the signature seals it (the
# pre-release-seam script signed first and wrote the plist after, which left
# the seal broken and the code-signing identifier inferred from the executable
# name instead of the bundle identifier).
cat > "$CONTENTS/Info.plist" << PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>Intendant</string>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleName</key>
    <string>Intendant</string>
    <key>CFBundleDisplayName</key>
    <string>Intendant</string>
    <key>CFBundleVersion</key>
    <string>${APP_VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${APP_VERSION}</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSScreenCaptureUsageDescription</key>
    <string>Intendant records your screen for display capture, computer use, and session replay.</string>
    <key>NSAppleEventsUsageDescription</key>
    <string>Intendant uses AppleScript for keyboard/mouse automation and system control.</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>Intendant uses the microphone for voice conversations with the AI presence layer.</string>
    <key>NSCameraUsageDescription</key>
    <string>Intendant uses the camera for video input to the AI presence layer.</string>
</dict>
</plist>
PLIST

# --- Signing ----------------------------------------------------------------

# codesign wrapper: appends --keychain when INTENDANT_SIGN_KEYCHAIN is set.
# (A function instead of an array: macOS bash 3.2 + `set -u` rejects empty
# array expansion.)
release_codesign() {
    if [ -n "$SIGN_RELEASE_KEYCHAIN" ]; then
        codesign --keychain "$SIGN_RELEASE_KEYCHAIN" "$@"
    else
        codesign "$@"
    fi
}

# Loud re-grant instructions, printed whenever this build's signature no
# longer matches what previous TCC grants were keyed to (a fresh identity
# was minted, or the designated requirement drifted). macOS keeps the
# System Settings toggles visually ON while every grant silently stops
# validating, so without this warning the failure surfaces days later as an
# opaque "declined TCC" capture error at runtime. Printed at most once in
# full; later calls in the same run add a one-line note.
TCC_REGRANT_WARNED=0
warn_tcc_regrant() {
    if [ "$TCC_REGRANT_WARNED" = "1" ]; then
        echo "(TCC re-grant warning above also applies: $1)" >&2
        return
    fi
    TCC_REGRANT_WARNED=1
    cat >&2 << WARN

============================================================================
  WARNING: TCC grants from previous Intendant builds are now VOID
============================================================================

  Why: $1

  macOS keys TCC permissions to the app's code-signing requirement. When it
  changes, every grant made to earlier builds — Screen Recording,
  Accessibility, Apple Events / Automation, Microphone, Camera — silently
  stops validating. System Settings still shows the toggles ON, but capture
  fails at runtime with "The user declined TCCs".

  Re-grant once this build is installed:
    1. System Settings → Privacy & Security → Screen & System Audio
       Recording ("Screen Recording" on older macOS): toggle Intendant
       OFF, then back ON (remove and re-add it if the toggle seems inert).
    2. Repeat for Accessibility, and for Automation (Apple Events),
       Microphone, and Camera if you use those features.
    3. Relaunch Intendant (open -b com.intendant.app) — TCC grants are
       re-read at launch, not live.

============================================================================

WARN
}

# Bundle every non-system dylib the executables reference (today: Homebrew
# libvpx) into Contents/Frameworks and rewrite the load commands to
# @executable_path/../Frameworks/. A distributable app cannot reference
# /opt/homebrew paths — the dylib won't exist on the user's machine, and
# hardened-runtime library validation rejects a dylib signed by another team.
# Bundled copies get re-signed with our identity in the inside-out pass below.
bundle_nonsystem_dylibs() {
    local pass=0 changed=1 bin dep name
    while [ "$changed" = "1" ] && [ "$pass" -lt 5 ]; do
        changed=0
        pass=$((pass + 1))
        for bin in "$MACOS/Intendant" "$MACOS/intendant-bin" "$MACOS/intendant-runtime" "$FRAMEWORKS"/*.dylib; do
            [ -f "$bin" ] || continue
            # otool -L lists "<path> (compatibility ...)" lines after a header.
            for dep in $(otool -L "$bin" | awk 'NR>1 {print $1}'); do
                case "$dep" in
                    /usr/lib/* | /System/* | @rpath/* | @executable_path/* | @loader_path/*) continue ;;
                esac
                name="$(basename "$dep")"
                if [ ! -f "$FRAMEWORKS/$name" ]; then
                    echo "Bundling non-system dylib: $dep"
                    mkdir -p "$FRAMEWORKS"
                    cp "$dep" "$FRAMEWORKS/$name"
                    chmod u+w "$FRAMEWORKS/$name"
                    install_name_tool -id "@executable_path/../Frameworks/$name" "$FRAMEWORKS/$name"
                    changed=1
                fi
                install_name_tool -change "$dep" "@executable_path/../Frameworks/$name" "$bin"
            done
        done
    done
    if [ "$changed" = "1" ]; then
        die "dylib bundling did not converge after $pass passes — circular or deeply nested non-system dylib graph?"
    fi
}

if [ -n "$SIGN_RELEASE_IDENTITY" ]; then
    # Distribution path: hardened runtime + secure timestamp + entitlements,
    # signed inside-out (nested code first, then the bundle — Apple deprecated
    # --deep for a reason: it applies the *bundle's* flags to nested code and
    # misses entitlements). Notarization requires all of this on every Mach-O
    # in the bundle.
    ENTITLEMENTS="macos-app/entitlements.plist"
    [ -f "$ENTITLEMENTS" ] || die "missing $ENTITLEMENTS"
    plutil -lint "$ENTITLEMENTS" > /dev/null

    bundle_nonsystem_dylibs

    echo "Signing app bundle with '$SIGN_RELEASE_IDENTITY' (hardened runtime + timestamp)..."
    if [ -d "$FRAMEWORKS" ]; then
        for dylib in "$FRAMEWORKS"/*.dylib; do
            [ -f "$dylib" ] || continue
            release_codesign --force --options runtime --timestamp \
                --sign "$SIGN_RELEASE_IDENTITY" "$dylib"
        done
    fi
    for exe in "$MACOS/intendant-runtime" "$MACOS/intendant-bin"; do
        release_codesign --force --options runtime --timestamp \
            --entitlements "$ENTITLEMENTS" \
            --sign "$SIGN_RELEASE_IDENTITY" "$exe"
    done
    # Signing the bundle signs Contents/MacOS/Intendant (the CFBundleExecutable)
    # and seals Info.plist + Resources + the already-signed nested code.
    release_codesign --force --options runtime --timestamp \
        --entitlements "$ENTITLEMENTS" \
        --sign "$SIGN_RELEASE_IDENTITY" "$APP"

    codesign --verify --deep --strict --verbose=2 "$APP"
    echo "Signed and verified with '$SIGN_RELEASE_IDENTITY'"
else
    # Local-dev path: sign with a stable self-signed identity so TCC
    # permissions survive recompiles. Uses a dedicated keychain at
    # ~/.intendant/signing.keychain-db (works over SSH, no Apple Developer
    # account needed, no GUI Keychain prompts).
    SIGN_IDENTITY="Intendant Dev"
    SIGN_KEYCHAIN="$HOME/.intendant/signing.keychain-db"
    SIGN_KEYCHAIN_PASS="intendant-dev"
    # PKCS#12 escrow of the signing identity, outside the keychain. The
    # keychain used to hold the ONLY copy of the private key (the temp cert
    # dir is deleted right after import), so a lost or recreated keychain
    # silently minted a brand-new identity — and because macOS TCC keys
    # every grant to the signing cert, all Screen Recording / Accessibility
    # / Apple Events grants died with no visible error (2026-07-13
    # incident). With the escrow, a later run re-imports the SAME identity
    # instead. Passphrase choice: fixed and public ("intendant", the same
    # one this script has always used for the import) — this is a
    # machine-local self-signed cert whose only job is a stable designated
    # requirement, not a trust anchor. The secret is the key *file*, which
    # lives chmod-600 under ~/.intendant and never enters the repo tree.
    SIGN_P12_BACKUP="$HOME/.intendant/signing-identity.p12"
    SIGN_P12_PASS="intendant"

    if ! security find-identity -p codesigning "$SIGN_KEYCHAIN" 2>/dev/null | grep -q "$SIGN_IDENTITY"; then
        mkdir -p "$(dirname "$SIGN_KEYCHAIN")"
        security create-keychain -p "$SIGN_KEYCHAIN_PASS" "$SIGN_KEYCHAIN" 2>/dev/null || true
        security unlock-keychain -p "$SIGN_KEYCHAIN_PASS" "$SIGN_KEYCHAIN"
        security set-keychain-settings "$SIGN_KEYCHAIN"
        if [ -f "$SIGN_P12_BACKUP" ]; then
            # The keychain is gone/empty but the escrow survives: re-import
            # the same cert + key, so the designated requirement — and every
            # TCC grant keyed to it — keeps validating.
            echo "Signing keychain is missing '$SIGN_IDENTITY'; re-importing it from $SIGN_P12_BACKUP (existing TCC grants stay valid)..."
            security import "$SIGN_P12_BACKUP" -k "$SIGN_KEYCHAIN" -P "$SIGN_P12_PASS" -T /usr/bin/codesign -A
        else
            echo "Creating local code signing certificate '$SIGN_IDENTITY'..."
            CERT_DIR=$(mktemp -d)
            cat > "$CERT_DIR/cert.conf" << 'CERTCONF'
[req]
distinguished_name = req_dn
x509_extensions = codesign
prompt = no
[req_dn]
CN = Intendant Dev
[codesign]
keyUsage = digitalSignature
extendedKeyUsage = codeSigning
CERTCONF
            openssl req -x509 -newkey rsa:2048 -nodes \
                -keyout "$CERT_DIR/key.pem" -out "$CERT_DIR/cert.pem" \
                -days 3650 -config "$CERT_DIR/cert.conf" 2>/dev/null
            openssl pkcs12 -export -out "$CERT_DIR/cert.p12" \
                -inkey "$CERT_DIR/key.pem" -in "$CERT_DIR/cert.pem" \
                -passout "pass:$SIGN_P12_PASS" 2>/dev/null
            security import "$CERT_DIR/cert.p12" -k "$SIGN_KEYCHAIN" -P "$SIGN_P12_PASS" -T /usr/bin/codesign -A
            # Escrow the identity BEFORE deleting the temp dir — the
            # keychain must never again hold the only copy of the key.
            cp "$CERT_DIR/cert.p12" "$SIGN_P12_BACKUP"
            chmod 600 "$SIGN_P12_BACKUP"
            rm -rf "$CERT_DIR"
            echo "Certificate created in $SIGN_KEYCHAIN (escrowed to $SIGN_P12_BACKUP)"
            warn_tcc_regrant "a NEW signing identity was created — no '$SIGN_IDENTITY' in $SIGN_KEYCHAIN and no escrow at $SIGN_P12_BACKUP to recover it from"
        fi
        security set-key-partition-list -S apple-tool:,apple: -s -k "$SIGN_KEYCHAIN_PASS" "$SIGN_KEYCHAIN" >/dev/null 2>&1
        # Add to search list so codesign can find it (list-keychains -s
        # replaces the whole list, so re-list the existing entries too;
        # word-splitting the quoted paths is intended).
        # shellcheck disable=SC2046
        security list-keychains -d user -s "$SIGN_KEYCHAIN" $(security list-keychains -d user | tr -d '"')
    elif [ ! -f "$SIGN_P12_BACKUP" ]; then
        # The identity predates the escrow (or the escrow was deleted):
        # backfill it from the keychain so a future keychain loss re-imports
        # instead of minting. Non-fatal — an un-escrowed identity still
        # signs; it just cannot survive keychain loss.
        security unlock-keychain -p "$SIGN_KEYCHAIN_PASS" "$SIGN_KEYCHAIN" 2>/dev/null || true
        if security export -k "$SIGN_KEYCHAIN" -t identities -f pkcs12 -P "$SIGN_P12_PASS" -o "$SIGN_P12_BACKUP" 2>/dev/null; then
            chmod 600 "$SIGN_P12_BACKUP"
            echo "Escrowed signing identity to $SIGN_P12_BACKUP"
        else
            rm -f "$SIGN_P12_BACKUP"
            echo "Warning: could not escrow the signing identity to $SIGN_P12_BACKUP; if $SIGN_KEYCHAIN is ever lost, a new identity will be minted and every TCC grant will be void" >&2
        fi
    fi

    echo "Signing app bundle..."
    security unlock-keychain -p "$SIGN_KEYCHAIN_PASS" "$SIGN_KEYCHAIN" 2>/dev/null
    if security find-identity -p codesigning "$SIGN_KEYCHAIN" 2>/dev/null | grep -q "$SIGN_IDENTITY"; then
        codesign --force --deep --keychain "$SIGN_KEYCHAIN" --sign "$SIGN_IDENTITY" "$APP"
        echo "Signed with '$SIGN_IDENTITY' (TCC grants will persist across recompiles)"
    else
        echo "Warning: '$SIGN_IDENTITY' certificate not found, falling back to ad-hoc signing"
        echo "TCC permissions may be invalidated on each recompile"
        codesign --force --deep --sign - "$APP" 2>/dev/null || true
    fi
fi

# --- TCC drift check: designated requirement vs. the last signed build -------
# macOS TCC keys each grant to the app's designated requirement (DR) as it
# was at grant time. ANY DR change — a re-minted signing cert, a changed
# signing identifier (the 2026-07-10 `Intendant` → `com.intendant.app`
# identifier move voided grants exactly this way), a dev-cert ↔ Developer ID
# switch, or ad-hoc's per-build cdhash — voids every existing grant while
# System Settings keeps showing the toggles ON. Diff this build's DR against
# the previous signed build's and warn loudly when it moved. Reads only the
# bundle plus a cache under ~/.intendant — needs no TCC or Full Disk Access.
DR_CACHE="$HOME/.intendant/last-signed-dr.txt"
# codesign -d -r- prints the requirement itself ("designated => ...") on
# stdout and the Executable= header on stderr; unsigned bundles yield empty.
CURRENT_DR="$(codesign -d -r- "$APP" 2>/dev/null || true)"
if [ -n "$CURRENT_DR" ]; then
    PREVIOUS_DR="$(cat "$DR_CACHE" 2>/dev/null || true)"
    if [ -n "$PREVIOUS_DR" ] && [ "$CURRENT_DR" != "$PREVIOUS_DR" ]; then
        echo "Designated requirement changed since the last signed build:" >&2
        printf '  was: %s\n  now: %s\n' "$PREVIOUS_DR" "$CURRENT_DR" >&2
        warn_tcc_regrant "this build's code-signing designated requirement differs from the last signed build's (cached at $DR_CACHE)"
    fi
    mkdir -p "$(dirname "$DR_CACHE")"
    printf '%s\n' "$CURRENT_DR" > "$DR_CACHE"
fi

# --- Notarization (optional; requires the release identity) ------------------

NOTARIZED=0
if [ "$NOTARY_STATE" = "all" ]; then
    echo "Submitting to the Apple notary service (typically 1-5 minutes)..."
    NOTARIZE_ZIP="$(mktemp -d)/Intendant-notarize.zip"
    ditto -c -k --keepParent "$APP" "$NOTARIZE_ZIP"
    SUBMIT_JSON="$(xcrun notarytool submit "$NOTARIZE_ZIP" \
        --key "$NOTARY_KEY_FILE" \
        --key-id "$NOTARY_KEY_ID" \
        --issuer "$NOTARY_ISSUER" \
        --wait --timeout 30m \
        --output-format json)" || die "notarytool submit failed"
    echo "$SUBMIT_JSON"
    SUBMIT_STATUS="$(printf '%s' "$SUBMIT_JSON" \
        | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))')"
    SUBMIT_ID="$(printf '%s' "$SUBMIT_JSON" \
        | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')"
    rm -f "$NOTARIZE_ZIP"
    if [ "$SUBMIT_STATUS" != "Accepted" ]; then
        echo "Notarization was not accepted (status: ${SUBMIT_STATUS:-unknown}); fetching the notary log:" >&2
        [ -n "$SUBMIT_ID" ] && xcrun notarytool log "$SUBMIT_ID" \
            --key "$NOTARY_KEY_FILE" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER" >&2 || true
        die "notarization failed"
    fi
    # Staple the ticket so Gatekeeper accepts the app offline.
    xcrun stapler staple "$APP"
    xcrun stapler validate "$APP"
    NOTARIZED=1
    # Informational only: spctl consults the local policy database, which can
    # reject for machine-local reasons even on a correctly notarized bundle.
    spctl --assess --type exec -vv "$APP" || echo "Note: spctl assessment failed (see above); stapler validate passed"
    echo "Notarized and stapled."
fi

# --- Versioned artifact (optional) -------------------------------------------

if [ -n "${INTENDANT_ARTIFACT_DIR:-}" ]; then
    mkdir -p "$INTENDANT_ARTIFACT_DIR"
    ARCH="$(uname -m)"
    SUFFIX=""
    if [ -z "$SIGN_RELEASE_IDENTITY" ]; then
        SUFFIX="-unsigned-dev"
    elif [ "$NOTARIZED" != "1" ]; then
        SUFFIX="-signed-unnotarized"
    fi
    ZIP_NAME="Intendant-${APP_VERSION}-macos-${ARCH}${SUFFIX}.zip"
    ditto -c -k --keepParent "$APP" "$INTENDANT_ARTIFACT_DIR/$ZIP_NAME"
    (cd "$INTENDANT_ARTIFACT_DIR" && shasum -a 256 "$ZIP_NAME" > "$ZIP_NAME.sha256")
    echo "Artifact: $INTENDANT_ARTIFACT_DIR/$ZIP_NAME"
    echo "Checksum: $INTENDANT_ARTIFACT_DIR/$ZIP_NAME.sha256"
fi

# --- Install ------------------------------------------------------------------

if [ "$INSTALL_APP" = "1" ]; then
    # Install the freshly-signed bundle to /Applications so everything
    # downstream (LaunchServices, computer-use MCP, Spotlight, Dock)
    # sees a recognised install location. TCC permissions survive
    # this install *because the signing identity is stable* — the
    # cert-based Designated Requirement matches across builds, so
    # Screen Recording / Accessibility / Microphone grants carry
    # over without re-prompting. (Ad-hoc signing would re-prompt
    # every install since the cdhash changes each build; we're on
    # cert-based signing specifically to avoid that.)
    #
    # `ditto` is Apple's recommended tool for app-bundle copies —
    # it preserves extended attributes (including the embedded code
    # signature's `com.apple.cs.CodeRequirements-*` xattrs) that
    # `cp -R` occasionally corrupts. `rm -rf` first so the copy is
    # a fresh slate rather than merged over whatever was there.
    echo "Installing to $INSTALLED_APP..."
    rm -rf "$INSTALLED_APP"
    ditto "$APP" "$INSTALLED_APP"

    # Refresh LaunchServices so the new build is recognised
    # immediately (without the refresh, the Dock / Spotlight /
    # computer-use MCP can take a minute or two to notice).
    "$LS" -f "$INSTALLED_APP"

    # Unregister the `target/` build path. Both paths holding the
    # same CFBundleIdentifier re-introduces the ambiguity the
    # top-of-script cleanup exists to prevent — with two copies
    # registered, `open -b com.intendant.app` can pick either
    # nondeterministically. Leave the files (some devs may `open
    # target/Intendant.app` directly for debugging); just drop the
    # LaunchServices record.
    "$LS" -u "$PROJECT_ROOT/$APP" 2>/dev/null || true

    echo "✅ Built + installed: $INSTALLED_APP (version $APP_VERSION)"
    echo ""
    echo "Launch:"
    echo "  open -b com.intendant.app"
else
    echo "✅ Built: $APP (version $APP_VERSION; skipping install; set INSTALL_APP=1 to install)"
    echo ""
    echo "Launch:"
    echo "  open target/Intendant.app"
fi

# Repeat a one-line pointer at the very end so the full warning can't be
# scrolled away by the install output above.
if [ "$TCC_REGRANT_WARNED" = "1" ]; then
    echo "" >&2
    echo "⚠️  TCC re-grant required (see the warning above): cycle Screen Recording / Accessibility for Intendant in System Settings, then relaunch." >&2
fi

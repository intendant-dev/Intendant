#!/bin/bash
# Build intendant as a native macOS desktop app.
#
# Creates Intendant.app with a Swift wrapper that:
#   1. Launches intendant --web as a child process
#   2. Opens a native window with WKWebView loading the dashboard
#   3. Gets proper TCC permissions (Screen Recording, Accessibility)
#   4. Child processes (ffmpeg, screencapture, cliclick) inherit permissions
#
# Usage:
#   ./scripts/bundle-macos.sh          # Release build
#   ./scripts/bundle-macos.sh debug    # Debug build
#
# Output: target/Intendant.app

set -euo pipefail

BUNDLE_ID="com.intendant.app"

PROFILE="${1:-release}"

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

# Where the freshly-built bundle gets installed at the end of this
# script. `/Applications` is the only install location a few tools
# (Claude Code's computer-use MCP, Dock quick-launch, Spotlight)
# consistently recognise — a bundle living only in `target/` gets
# rejected as "not installed" even after LaunchServices sees it.
# Set `INSTALL_APP=0` to skip the install step (build-only).
INSTALLED_APP="/Applications/Intendant.app"
INSTALL_APP="${INSTALL_APP:-1}"

# Unregister any stale Intendant.app bundles from other worktrees or Trash.
# Multiple bundles with the same CFBundleIdentifier cause macOS LaunchServices
# to launch the wrong one (possibly an old worktree build from days ago).
LS=/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"
while IFS= read -r stale_path; do
    # Skip the current target (this build's output) AND the canonical
    # install destination — both are expected to hold an Intendant.app
    # at the end of this script, and the install step below overwrites
    # `/Applications` in place rather than deleting it first.
    if [ "$stale_path" != "$PROJECT_ROOT/$APP" ] && [ "$stale_path" != "$INSTALLED_APP" ]; then
        $LS -u "$stale_path" 2>/dev/null || true
        rm -rf "$stale_path" 2>/dev/null || true
    fi
done < <($LS -dump 2>/dev/null | grep -o '/[^ ]*Intendant\.app' | sort -u)

rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES"

# Compile Swift wrapper
echo "Compiling macOS app wrapper..."
swiftc -O -o "$MACOS/Intendant" macos-app/main.swift \
    -framework Cocoa -framework WebKit

# Copy Rust binaries
cp "$BINARY" "$MACOS/intendant-bin"
cp "$RUNTIME" "$MACOS/intendant-runtime"

# Code-sign with a stable local identity so TCC permissions survive recompiles.
# Uses a dedicated keychain at ~/.intendant/signing.keychain-db (works over SSH,
# no Apple Developer account needed, no GUI Keychain prompts).
SIGN_IDENTITY="Intendant Dev"
SIGN_KEYCHAIN="$HOME/.intendant/signing.keychain-db"
SIGN_KEYCHAIN_PASS="intendant-dev"

if ! security find-identity -p codesigning "$SIGN_KEYCHAIN" 2>/dev/null | grep -q "$SIGN_IDENTITY"; then
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
        -passout pass:intendant 2>/dev/null
    mkdir -p "$(dirname "$SIGN_KEYCHAIN")"
    security create-keychain -p "$SIGN_KEYCHAIN_PASS" "$SIGN_KEYCHAIN" 2>/dev/null || true
    security unlock-keychain -p "$SIGN_KEYCHAIN_PASS" "$SIGN_KEYCHAIN"
    security set-keychain-settings "$SIGN_KEYCHAIN"
    security import "$CERT_DIR/cert.p12" -k "$SIGN_KEYCHAIN" -P "intendant" -T /usr/bin/codesign -A
    security set-key-partition-list -S apple-tool:,apple: -s -k "$SIGN_KEYCHAIN_PASS" "$SIGN_KEYCHAIN" >/dev/null 2>&1
    # Add to search list so codesign can find it
    security list-keychains -d user -s "$SIGN_KEYCHAIN" $(security list-keychains -d user | tr -d '"')
    rm -rf "$CERT_DIR"
    echo "Certificate created in $SIGN_KEYCHAIN"
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

# Copy app icon
if [ -f "macos-app/AppIcon.icns" ]; then
    cp "macos-app/AppIcon.icns" "$RESOURCES/AppIcon.icns"
fi

# Info.plist
cat > "$CONTENTS/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>Intendant</string>
    <key>CFBundleIdentifier</key>
    <string>com.intendant.app</string>
    <key>CFBundleName</key>
    <string>Intendant</string>
    <key>CFBundleDisplayName</key>
    <string>Intendant</string>
    <key>CFBundleVersion</key>
    <string>1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
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
    $LS -f "$INSTALLED_APP"

    # Unregister the `target/` build path. Both paths holding the
    # same CFBundleIdentifier re-introduces the ambiguity the
    # top-of-script cleanup exists to prevent — with two copies
    # registered, `open -b com.intendant.app` can pick either
    # nondeterministically. Leave the files (some devs may `open
    # target/Intendant.app` directly for debugging); just drop the
    # LaunchServices record.
    $LS -u "$PROJECT_ROOT/$APP" 2>/dev/null || true

    echo "✅ Built + installed: $INSTALLED_APP"
    echo ""
    echo "Launch:"
    echo "  open -b com.intendant.app"
else
    echo "✅ Built: $APP (skipping install; set INSTALL_APP=1 to install)"
    echo ""
    echo "Launch:"
    echo "  open target/Intendant.app"
fi

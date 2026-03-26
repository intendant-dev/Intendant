#!/bin/bash
# Build intendant as a macOS .app bundle.
#
# The .app wrapper gives intendant a proper bundle ID so macOS TCC
# (Transparency, Consent, and Control) can manage permissions:
#   - Screen Recording (ffmpeg avfoundation, screencapture)
#   - Accessibility (cliclick for computer use)
#   - Screen Sharing (VNC)
#
# Usage:
#   ./scripts/bundle-macos.sh          # Release build
#   ./scripts/bundle-macos.sh debug    # Debug build
#
# Output: target/Intendant.app

set -euo pipefail

PROFILE="${1:-release}"
if [ "$PROFILE" = "debug" ]; then
    BINARY="target/debug/intendant"
    RUNTIME="target/debug/intendant-runtime"
    cargo build
else
    BINARY="target/release/intendant"
    RUNTIME="target/release/intendant-runtime"
    cargo build --release
fi

APP="target/Intendant.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES"

# Copy binaries
cp "$BINARY" "$MACOS/intendant"
cp "$RUNTIME" "$MACOS/intendant-runtime"

# Info.plist with TCC usage descriptions
cat > "$CONTENTS/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>intendant</string>
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
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSUIElement</key>
    <true/>
    <key>NSScreenCaptureUsageDescription</key>
    <string>Intendant records your screen for display capture, computer use, and session replay.</string>
    <key>NSAppleEventsUsageDescription</key>
    <string>Intendant uses AppleScript for keyboard/mouse automation and system control.</string>
</dict>
</plist>
PLIST

# Wrapper script that forwards args and sets up PATH
mv "$MACOS/intendant" "$MACOS/intendant-bin"
cat > "$MACOS/intendant" << 'WRAPPER'
#!/bin/bash
# Ensure Homebrew tools are in PATH
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
DIR="$(cd "$(dirname "$0")" && pwd)"
exec "$DIR/intendant-bin" "$@"
WRAPPER
chmod +x "$MACOS/intendant"

echo "✅ Built: $APP"
echo ""
echo "Run from terminal:"
echo "  open target/Intendant.app --args --web"
echo ""
echo "Or launch directly:"
echo "  target/Intendant.app/Contents/MacOS/intendant --web"
echo ""
echo "On first run, macOS will prompt for Screen Recording permission."

#!/bin/sh
# Build target/release/artty.app: a release build wrapped in a macOS app bundle
# with an icon, signed ad hoc so it runs locally. Copy it to /Applications to
# launch artty from Finder, the Dock or Spotlight.
#
#   ARTTY_BUNDLE_ID   bundle identifier (default: dev.artty.terminal)
#   ARTTY_SIGN        signing identity (default: "-", ad hoc)
set -eu
cd "$(dirname "$0")/.."

BUNDLE_ID="${ARTTY_BUNDLE_ID:-dev.artty.terminal}"
SIGN="${ARTTY_SIGN:--}"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/artty-app/Cargo.toml | head -1)

cargo build --release -p artty

APP=target/release/artty.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/artty "$APP/Contents/MacOS/artty"
cp assets/artty.icns "$APP/Contents/Resources/artty.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>artty</string>
    <key>CFBundleDisplayName</key><string>artty</string>
    <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
    <key>CFBundleExecutable</key><string>artty</string>
    <key>CFBundleIconFile</key><string>artty</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

codesign --force --sign "$SIGN" "$APP"
echo "Built $APP ($BUNDLE_ID $VERSION)"

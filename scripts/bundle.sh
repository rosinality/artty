#!/bin/sh
# Build target/release/artty.app: a release build wrapped in a macOS app bundle
# with an icon, signed ad hoc so it runs locally. With zig and cargo-zigbuild
# (and Rust's x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu targets), it
# also carries Linux builds, for the app to install on other machines.
#
#   ./scripts/bundle.sh             build (or rebuild) the bundle
#   ./scripts/bundle.sh --install   also link /Applications/artty.app to it, so
#                                   every later build updates the installed app
#
#   ARTTY_BUNDLE_ID   bundle identifier (default: dev.artty.terminal)
#   ARTTY_SIGN        signing identity (default: "-", ad hoc)
#   ARTTY_APPS        where --install links the app (default: /Applications)
set -eu
cd "$(dirname "$0")/.."

BUNDLE_ID="${ARTTY_BUNDLE_ID:-dev.artty.terminal}"
SIGN="${ARTTY_SIGN:--}"
APPS="${ARTTY_APPS:-/Applications}"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/artty-app/Cargo.toml | head -1)
INSTALL=false
case "${1:-}" in
  --install) INSTALL=true ;;
  "") ;;
  *) echo "usage: $0 [--install]" >&2; exit 2 ;;
esac

cargo build --release -p artty

# Linux builds (against glibc 2.28: any distribution from 2018 on).
LINUX=""
if command -v cargo-zigbuild >/dev/null 2>&1 || [ -x "$HOME/.cargo/bin/cargo-zigbuild" ]; then
  for CPU in x86_64 aarch64; do
    if rustup target list --installed | grep -qx "$CPU-unknown-linux-gnu"; then
      cargo zigbuild --release -p artty --target "$CPU-unknown-linux-gnu.2.28"
      LINUX="$LINUX $CPU"
    else
      echo "(no Linux build for $CPU: rustup target add $CPU-unknown-linux-gnu)"
    fi
  done
else
  echo "(no Linux builds: brew install zig && cargo install cargo-zigbuild)"
fi

# Assemble the new bundle next to the old one, then swap it in with a rename:
# a running artty keeps its (now unlinked) files and is never modified in place.
APP="$PWD/target/release/artty.app"
NEW="$APP.new"
rm -rf "$NEW"
mkdir -p "$NEW/Contents/MacOS" "$NEW/Contents/Resources"
cp target/release/artty "$NEW/Contents/MacOS/artty"
cp assets/artty.icns "$NEW/Contents/Resources/artty.icns"
for CPU in $LINUX; do
  mkdir -p "$NEW/Contents/Resources/linux"
  cp "target/$CPU-unknown-linux-gnu/release/artty" "$NEW/Contents/Resources/linux/artty-$CPU"
done

cat > "$NEW/Contents/Info.plist" <<PLIST
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

codesign --force --sign "$SIGN" "$NEW"
rm -rf "$APP"
mv "$NEW" "$APP"
echo "Built $APP ($BUNDLE_ID $VERSION${LINUX:+; for Linux on$LINUX})"

LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

if $INSTALL; then
  LINK="$APPS/artty.app"
  if [ -L "$LINK" ]; then
    rm "$LINK"
  elif [ -e "$LINK" ]; then
    # An earlier copy: move it to the Trash rather than deleting it.
    TRASHED="$HOME/.Trash/artty.app $(date +%Y-%m-%d\ %H.%M.%S)"
    mv "$LINK" "$TRASHED"
    echo "Moved the old copy to $TRASHED"
  fi
  ln -s "$APP" "$LINK"
  echo "Linked $LINK -> $APP"
fi

# Let Finder, the Dock and Spotlight see the new build (icon, version).
[ -x "$LSREGISTER" ] && "$LSREGISTER" -f "$APP" >/dev/null 2>&1 || true

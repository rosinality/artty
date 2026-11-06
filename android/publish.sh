#!/bin/sh
# Build the release APK and publish it where the app looks for updates
# (Settings, Updates): a folder served over HTTPS.
#
#   android/publish.sh DEST
#
# DEST is a folder here, or `user@host:path` (copied with scp, as your ssh
# config says). It gets artty-<versionCode>.apk, then version.json naming it
# (last, so the app never sees one without its APK).
#
# Signed with signing/artty.keystore, a copy of this Mac's debug key (made from
# ~/.android/debug.keystore the first time): the app installs updates only over
# a build signed with the same key. Keep it.
set -eu
DEST=${1:?usage: android/publish.sh DEST (a folder, or user@host:path)}
cd "$(dirname "$0")"

if [ ! -f signing/artty.keystore ]; then
    mkdir -p signing
    cp "$HOME/.android/debug.keystore" signing/artty.keystore
fi

./gradlew -q assembleRelease releaseVersion
versionCode=$(sed -n 's/^versionCode=//p' app/build/release-version.properties)
versionName=$(sed -n 's/^versionName=//p' app/build/release-version.properties)
APK=app/build/outputs/apk/release/app-release.apk
NAME=artty-$versionCode.apk
SHA=$(shasum -a 256 "$APK" | cut -d' ' -f1)
SIZE=$(wc -c < "$APK" | tr -d ' ')

OUT=app/build/publish
rm -rf "$OUT" && mkdir -p "$OUT"
cp "$APK" "$OUT/$NAME"
cat > "$OUT/version.json" <<JSON
{
  "versionCode": $versionCode,
  "versionName": "$versionName",
  "apk": "$NAME",
  "sha256": "$SHA",
  "size": $SIZE
}
JSON

case "$DEST" in
    *:*)
        scp -q "$OUT/$NAME" "$DEST/"
        scp -q "$OUT/version.json" "$DEST/"
        ;;
    *)
        mkdir -p "$DEST"
        cp "$OUT/$NAME" "$DEST/"
        cp "$OUT/version.json" "$DEST/"
        ;;
esac
echo "Published artty $versionName (build $versionCode) to $DEST"

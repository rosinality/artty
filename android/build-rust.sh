#!/bin/sh
# The app's Rust side: libartty_android.so for each ABI (into jniLibs), and
# its Kotlin bindings (into app/build/generated/uniffi).
set -eu
cd "$(dirname "$0")/.."
: "${ANDROID_HOME:=/opt/homebrew/share/android-commandlinetools}"
: "${ANDROID_NDK_HOME:=$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)}"
export ANDROID_HOME ANDROID_NDK_HOME
PROFILE="${ARTTY_RUST_PROFILE:-release}"
cargo ndk -t arm64-v8a -t x86_64 --platform 28 -o android/app/src/main/jniLibs \
    build -p artty-android --lib --profile "$PROFILE"
DIR=$PROFILE
[ "$PROFILE" = dev ] && DIR=debug
cargo run -q -p artty-android --bin uniffi-bindgen -- generate \
    --library "target/aarch64-linux-android/$DIR/libartty_android.so" \
    --language kotlin --no-format --out-dir android/app/build/generated/uniffi

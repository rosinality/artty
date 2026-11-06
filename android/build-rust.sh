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

# This app's build of artty for Linux (against glibc 2.28), for installing it
# on a server that runs another build: in the APK's assets, as linux/artty-<cpu>.
LINUX=android/app/build/generated/linux-assets/linux
rm -rf "$LINUX" && mkdir -p "$LINUX"
if command -v cargo-zigbuild >/dev/null 2>&1 || [ -x "$HOME/.cargo/bin/cargo-zigbuild" ]; then
    for CPU in x86_64 aarch64; do
        if rustup target list --installed | grep -qx "$CPU-unknown-linux-gnu"; then
            cargo zigbuild -q --release -p artty --target "$CPU-unknown-linux-gnu.2.28"
            cp "target/$CPU-unknown-linux-gnu/release/artty" "$LINUX/artty-$CPU"
        else
            echo "(no Linux build for $CPU: rustup target add $CPU-unknown-linux-gnu)"
        fi
    done
else
    echo "(no Linux builds: brew install zig && cargo install cargo-zigbuild)"
fi

# artty for Android

A client for artty servers: it logs in by ssh (built in: `crates/artty-client`,
through `crates/artty-android` and UniFFI), shows a server's windows, and shares
one with the Mac app — the pane takes the size of whichever device is in use.

## Building

Needs the Android SDK (platform 37, build tools, NDK), JDK 21, the Rust targets
`aarch64-linux-android` and `x86_64-linux-android`, and `cargo-ndk`:

    brew install --cask android-commandlinetools
    brew install openjdk@21
    sdkmanager "platform-tools" "platforms;android-37.0" "build-tools;36.1.0" "ndk;30.0.16248370"
    rustup target add aarch64-linux-android x86_64-linux-android
    cargo install cargo-ndk

Then, with `local.properties` naming the SDK (`sdk.dir=…`):

    JAVA_HOME=/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home ./gradlew assembleDebug

`build-rust.sh` runs first (Gradle's `preBuild`): it builds the Rust library for
each ABI into `app/src/main/jniLibs`, and its Kotlin bindings into
`app/build/generated/uniffi`. `ARTTY_RUST_PROFILE=dev` builds it unoptimized.
It also builds artty for Linux into the APK's assets, for installing on a server
that runs another build, as the Mac app's bundle does: that needs zig and
cargo-zigbuild, and the Rust target for each CPU (`x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`); a CPU without its target is skipped.

## Using it

"This phone's key" shows the key the phone made for itself: add it to
`~/.ssh/authorized_keys` on each server (Tailscale SSH needs nothing); it's kept
in Android Keystore. The server needs this build of artty (the protocol must
match): connecting to one that runs another build offers to install this one
there.

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

## Updating without USB

`android/publish.sh DEST` builds the release APK and puts it, with a
`version.json` naming it, in `DEST`: a folder here, or `user@host:path` (copied
with scp). Serve that folder over HTTPS (any static host; a Cloudflare address
in front of a server works) and give its address in the app's Settings,
Updates. The app looks there when it opens (every few hours at most), offers a
newer build, downloads it, checks its SHA-256 and hands it to the system
installer; the first time, Android asks to let artty install apps.

Android installs an update only over the app signed with the same key. Release
builds are signed with `signing/artty.keystore`, a copy of this Mac's debug key
(`~/.android/debug.keystore`) made the first time, so a release installs over
the debug builds already on a phone. Keep that file: without it, a new build
installs only after uninstalling the app, which loses its data (the phone's key
included). Each build's versionCode is the minutes since 2025, so a newer
build is always newer; its name says the commit. Debug builds may check a plain
`http://` address (for trying it on the computer); releases only `https://`.

## Using it

Settings, Keys, shows the key the phone made for itself: add it to
`~/.ssh/authorized_keys` on each server (Tailscale SSH needs nothing); it's kept
in Android Keystore and can't leave it. Other private keys can be added (from a
file, or pasted), kept encrypted with a Keystore key; a passphrase is asked for
when one is needed, and kept if you say so. The server needs this build of
artty (the protocol must match): connecting to one that runs another build
offers to install this one there.

A server is a destination as `ssh` takes it: `user@host[:port]`, or a host of
the app's ssh config (Settings, SSH config: as `~/.ssh/config`, typed in or
imported), which says how to reach it. Read as OpenSSH reads it (Host and
Match blocks, first value wins), it does HostName, User, Port, IdentityFile (a
key added, by its file name; `~/.ssh/phone` is the phone's), IdentitiesOnly,
ProxyJump (through each jump host in turn), LocalForward, RemoteForward,
DynamicForward (SOCKS), ForwardAgent (each signature asked of you),
ServerAliveInterval and -CountMax, ConnectTimeout, Compression, Ciphers,
KexAlgorithms, HostKeyAlgorithms and MACs (with `+`, `-`, `^`),
PreferredAuthentications and the *Authentication switches,
StrictHostKeyChecking, HostKeyAlias, SetEnv, and, for plain ssh, RemoteCommand
and RequestTTY. Adding a server shows what its destination resolves to, and
the options not done here. A ProxyCommand can't run on a phone (ProxyJump does
the same), but cloudflared's (`cloudflared access ssh --hostname %h`, for a
host behind a Cloudflare Tunnel) is done by the app itself, with no cloudflared:
ssh in a WebSocket to the hostname, with an Access service token if the command
gives one (`--service-token-id`, `--service-token-secret`). The drawer lists the connection's forwards, and adds more.

Settings also has the machines' keys (to forget one that changed for a good
reason), the terminal's (text size, the volume keys, the keys row's
characters, a vibrating bell, a plain tab's history), and a backup of it all to
a file (the added keys only if asked: whoever has the file can read them).

A machine without artty works too, with plain ssh: turn off "artty on it" for
the server (connecting to one where artty isn't found offers to). Its tabs are
shells there, each its own ssh channel, drawn by artty's terminal on the phone,
so finding, selecting and links work as they do on an artty server. But they
end with the connection: reconnecting (after the phone slept, say) opens a new
shell, and there are no files, workspaces or agents.

The keys row's Ctrl and Alt hold for the next key; a long press keeps them on
until tapped again. The volume keys, held, add what a phone keyboard lacks:
volume down is Ctrl, and volume up with W A S D gives the arrows, E Esc, T Tab,
P N pages up and down, H L Home and End, and 1–0 F1–F10 (Termux's keys);
pressed alone, they turn the volume. A hardware keyboard has Ctrl+Shift with T
(new tab), W (close it), Tab or 1–9 (other tabs), C and V (copy, paste), F
(find), and = - 0 (text size); Ctrl+Tab goes to the next tab. A tap on a link
(an OSC 8 link, or a URL in the text) offers to open or copy it; a `file://`
link opens the file from the pane's machine.

The text is JetBrains Mono, under the SIL Open Font License
(`licenses/JetBrainsMono-OFL.txt`).

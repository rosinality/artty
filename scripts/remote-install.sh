#!/bin/sh
# Install artty on another (Linux) machine, for this Mac's app to reach over
# ssh (`artty remote add NAME --ssh DESTINATION`).
#
#   ./scripts/remote-install.sh DESTINATION
#   ./scripts/remote-install.sh --build-there DESTINATION
#
# Builds it here for that machine's CPU, against glibc 2.28 (so it runs on
# any distribution from 2018 on), copies it there and installs it into
# ~/.local/bin. That needs zig and cargo-zigbuild, and Rust's target:
#
#   brew install zig && cargo install cargo-zigbuild
#   rustup target add x86_64-unknown-linux-gnu   # or aarch64-unknown-linux-gnu
#
# --build-there instead copies this source to ~/.cache/artty/src there and
# builds it with that machine's cargo (for an older glibc, or musl).
#
# A server already running there moves to the new build (programs keep
# running).
set -eu
cd "$(dirname "$0")/.."
THERE=
if [ "${1:-}" = --build-there ]; then
  THERE=1
  shift
fi
DEST=${1:?usage: remote-install.sh [--build-there] DESTINATION}

# Move ~/.local/bin/artty.new there into place, and the server onto it.
finish() {
  ssh -T "$DEST" sh -s <<'REMOTE'
set -eu
chmod 755 "$HOME/.local/bin/artty.new"
mv "$HOME/.local/bin/artty.new" "$HOME/.local/bin/artty"
if "$HOME/.local/bin/artty" server status >/dev/null 2>&1; then
  "$HOME/.local/bin/artty" server restart
fi
echo "installed $("$HOME/.local/bin/artty" server --handover-check) at $HOME/.local/bin/artty"
REMOTE
}

if [ -n "$THERE" ]; then
  echo "copying the source to $DEST ..."
  ssh -T "$DEST" mkdir -p .cache/artty/src
  rsync -az --delete --exclude /target --exclude /.git --exclude /.jj --exclude /.bench \
    ./ "$DEST:.cache/artty/src/"
  echo "building on $DEST ..."
  ssh -T "$DEST" sh -s <<'REMOTE'
set -eu
cd ~/.cache/artty/src
CARGO=$(command -v cargo || echo "$HOME/.cargo/bin/cargo")
"$CARGO" build --release -p artty
mkdir -p "$HOME/.local/bin"
cp target/release/artty "$HOME/.local/bin/artty.new"
REMOTE
  finish
  exit
fi

# What it runs: Linux on which CPU, with which C library.
set -- $(ssh -T "$DEST" sh -c "'uname -sm; ldd --version 2>&1 | head -n 1'")
SYSTEM=${1:-} CPU=${2:-}
shift 2 || true
LIBC="$*"
if [ "$SYSTEM" != Linux ]; then
  echo "$DEST runs ${SYSTEM:-something unknown}; try --build-there." >&2
  exit 1
fi
case "$CPU" in
  x86_64 | aarch64) TARGET=$CPU-unknown-linux-gnu ;;
  *)
    echo "$DEST has a $CPU CPU; try --build-there." >&2
    exit 1
    ;;
esac
GLIBC=$(echo "$LIBC" | grep -Eo '[0-9]+\.[0-9]+' | tail -n 1)
case "$LIBC" in *GLIBC* | *"GNU libc"* | *"GNU C Library"*) ;; *) GLIBC= ;; esac
if [ -z "$GLIBC" ] || ! awk "BEGIN { exit !($GLIBC >= 2.28) }"; then
  echo "$DEST's C library isn't glibc 2.28 or later (${LIBC:-unknown}); try --build-there." >&2
  exit 1
fi

if ! command -v cargo-zigbuild >/dev/null 2>&1 && ! [ -x "$HOME/.cargo/bin/cargo-zigbuild" ]; then
  echo "needs cargo-zigbuild: brew install zig && cargo install cargo-zigbuild" >&2
  exit 1
fi
if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "needs Rust's $TARGET: rustup target add $TARGET" >&2
  exit 1
fi

echo "building for $TARGET (glibc 2.28) ..."
cargo zigbuild --release -p artty --target "$TARGET.2.28"
echo "copying it to $DEST ..."
ssh -T -o Compression=yes "$DEST" 'mkdir -p .local/bin && cat > .local/bin/artty.new' \
  <"target/$TARGET/release/artty"
finish

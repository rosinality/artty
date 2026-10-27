#!/bin/sh
# Build artty on another machine and install it there, for this Mac's app
# to reach over ssh (`artty remote add NAME --ssh DESTINATION`).
#
#   ./scripts/remote-install.sh DESTINATION
#
# Copies this source to ~/.cache/artty/src there (rsync), builds it with
# that machine's cargo (on PATH or in ~/.cargo/bin), and installs `artty`
# into ~/.local/bin. A server already running there moves to the new build
# (programs keep running).
set -eu
cd "$(dirname "$0")/.."
DEST=${1:?usage: remote-install.sh DESTINATION}

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
install -m 755 target/release/artty "$HOME/.local/bin/artty.new"
mv "$HOME/.local/bin/artty.new" "$HOME/.local/bin/artty"
if "$HOME/.local/bin/artty" server status >/dev/null 2>&1; then
  "$HOME/.local/bin/artty" server restart
fi
echo "installed $("$HOME/.local/bin/artty" server --handover-check) at $HOME/.local/bin/artty"
REMOTE

#!/bin/sh
# Terminal benchmarks: artty against other terminals on this machine.
#
#   ./scripts/bench.sh [io|fire|mem|all] [TERMINAL...]
#
#   io     `cat` an 11 MB text file, 5 runs (lower is better)
#   fire   `cat` 25 MB of colorful full-screen frames, like a fire animation:
#          every cell's background changes each frame, 3 runs (lower is better)
#   mem    memory a few seconds after launch, idle (artty: app + server)
#
# TERMINALs: artty (default), kitty, alacritty, ghostty — whichever are in
# /Applications. Times are taken inside the terminal, by zsh, so they include
# everything up to the terminal reading the output; grids are matched to
# artty's. The method follows moktavizen/terminal-benchmark, as tty7's
# scripts/bench does (https://github.com/l0ng-ai/tty7, Apache-2.0).
#
# artty runs from target/release (`cargo build --release` first) with its own
# server and settings under $ARTTY_BENCH_DIR (default .bench), never touching
# yours. Windows open and close by themselves; don't type into them or cover
# them while they run (macOS slows down hidden windows).
set -eu
cd "$(dirname "$0")/.."
REPO=$PWD
BENCH=${ARTTY_BENCH_DIR:-$REPO/.bench}
TEST=${1:-all}
[ $# -gt 0 ] && shift
TERMINALS=${*:-artty}
ARTTY=$REPO/target/release/artty
mkdir -p "$BENCH/results"

[ -x "$ARTTY" ] || { echo "build first: cargo build --release" >&2; exit 1; }

# Test data, made once.
if [ ! -f "$BENCH/corpus.txt" ] || [ ! -f "$BENCH/fire.txt" ]; then
  echo "making test data in $BENCH ..."
  python3 - "$BENCH" <<'EOF'
import random, sys
out = sys.argv[1]
random.seed(1)
words = ("the quick brown fox jumps over lazy dog to be or not that is question whether tis "
         "nobler in mind suffer slings arrows outrageous fortune").split()
lines, size = [], 0
while size < 11_000_000:
    line = " ".join(random.choice(words) for _ in range(random.randint(4, 16)))
    lines.append(line)
    size += len(line) + 1
open(f"{out}/corpus.txt", "w").write("\n".join(lines) + "\n")
random.seed(2)
cols, rows = 105, 34
heat = [[0] * cols for _ in range(rows)]
palette = [16, 52, 88, 124, 160, 196, 202, 208, 214, 220, 226, 227, 228, 229, 230, 231]
frames = []
for _ in range(600):
    heat[-1] = [random.randint(10, 15) for _ in range(cols)]
    for y in range(rows - 1):
        for x in range(cols):
            src = heat[y + 1][min(cols - 1, max(0, x + random.randint(-1, 1)))]
            heat[y][x] = max(0, src - random.randint(0, 1))
    frames.append("\x1b[H" + "\x1b[0m\r\n".join("".join(f"\x1b[48;5;{palette[v]}m " for v in row) for row in heat) + "\x1b[0m")
open(f"{out}/fire.txt", "w").write("".join(frames))
EOF
fi

# What runs inside each terminal: time `cat` with zsh's clock, write results.
cat > "$BENCH/inner.zsh" <<'EOF'
zmodload zsh/datetime
file=$1 runs=$2 out=$3
: > $out
print -- "grid: $(stty size)" >> $out
sleep 1
for i in $(seq $runs); do
  t0=$EPOCHREALTIME
  command cat $file
  t1=$EPOCHREALTIME
  printf 'run %d: %.0f\n' $i $(( (t1 - t0) * 1000 )) >> $out
done
print -- done >> $out
sleep 1
EOF

# Start TERMINAL running `zsh inner.zsh ARGS` at ROWS x COLUMNS; prints its pid.
launch() {
  terminal=$1 rows=$2 cols=$3; shift 3
  case $terminal in
    artty)
      rm -rf "$BENCH/artty"; mkdir -p "$BENCH/artty/tmp" "$BENCH/artty/config/artty"
      TMPDIR=$BENCH/artty/tmp/ XDG_STATE_HOME=$BENCH/artty/state XDG_CONFIG_HOME=$BENCH/artty/config \
        "$ARTTY" -e zsh "$@" >/dev/null 2>&1 & ;;
    kitty)
      /Applications/kitty.app/Contents/MacOS/kitty --config NONE -o initial_window_width="${cols}c" \
        -o initial_window_height="${rows}c" -o remember_window_size=no -o confirm_os_window_close=0 \
        zsh "$@" >/dev/null 2>&1 & ;;
    alacritty)
      /Applications/Alacritty.app/Contents/MacOS/alacritty --config-file /dev/null \
        -o "window.dimensions.columns=$cols" -o "window.dimensions.lines=$rows" -e zsh "$@" >/dev/null 2>&1 & ;;
    ghostty)
      /Applications/Ghostty.app/Contents/MacOS/ghostty --config-default-files=false --window-save-state=never \
        --window-width="$cols" --window-height="$rows" -e zsh "$@" >/dev/null 2>&1 & ;;
    *) echo "unknown terminal: $terminal" >&2; return 1 ;;
  esac
  echo $!
}

installed() {
  case $1 in
    artty) true ;;
    kitty) [ -d /Applications/kitty.app ] ;;
    alacritty) [ -d /Applications/Alacritty.app ] ;;
    ghostty) [ -d /Applications/Ghostty.app ] ;;
    *) false ;;
  esac
}

# Run a timed test in TERMINAL; prints "median (runs...)" in ms.
timed() {
  terminal=$1 file=$2 runs=$3
  out=$BENCH/results/$TEST_NAME-$terminal.txt
  rm -f "$out"
  pid=$(launch "$terminal" "$ROWS" "$COLS" "$BENCH/inner.zsh" "$file" "$runs" "$out")
  i=0
  while ! grep -q done "$out" 2>/dev/null && [ $i -lt 240 ]; do sleep 0.5; i=$((i + 1)); done
  sleep 2
  kill "$pid" 2>/dev/null || true
  [ "$terminal" = artty ] && TMPDIR=$BENCH/artty/tmp/ "$ARTTY" server stop >/dev/null 2>&1 || true
  if ! grep -q done "$out" 2>/dev/null; then echo "no result"; return; fi
  grid=$(sed -n 's/^grid: //p' "$out")
  if [ "$terminal" = artty ]; then ROWS=${grid% *} COLS=${grid#* }; fi
  sed -n 's/^run [0-9]*: //p' "$out" | sort -n | awk -v grid="$grid" '
    { v[NR] = $1; all = all (NR > 1 ? " " : "") $1 }
    END { printf "%6d ms   (runs: %s; grid %s)\n", v[int((NR + 1) / 2)], all, grid }'
}

memory() {
  terminal=$1
  # A shell that stays a while, as at a prompt.
  echo 'sleep 20' > "$BENCH/idle.zsh"
  pid=$(launch "$terminal" "$ROWS" "$COLS" "$BENCH/idle.zsh")
  sleep 6
  pids=$pid
  if [ "$terminal" = artty ]; then
    server=$(TMPDIR=$BENCH/artty/tmp/ "$ARTTY" server status 2>/dev/null | sed -n 's/.*pid \([0-9]*\).*/\1/p')
    pids="$pid ${server:-}"
  fi
  total=0
  for p in $pids; do
    kb=$(footprint "$p" 2>/dev/null | awk '/phys_footprint:/ { v = $2; u = $3; if (u == "MB") v *= 1024; if (u == "GB") v *= 1048576; print int(v) }')
    total=$((total + ${kb:-0}))
  done
  kill $pid 2>/dev/null || true
  [ "$terminal" = artty ] && TMPDIR=$BENCH/artty/tmp/ "$ARTTY" server stop >/dev/null 2>&1 || true
  echo "$((total / 1024)) MB"
}

ROWS=35 COLS=105
done_terminals=
for name in io fire mem; do
  case $TEST in all|$name) ;; *) continue ;; esac
  TEST_NAME=$name
  echo "== $name"
  for terminal in artty $TERMINALS; do
    case " $done_terminals " in *" $terminal-$name "*) continue ;; esac
    done_terminals="$done_terminals $terminal-$name"
    installed "$terminal" || { printf '%-10s not installed\n' "$terminal"; continue; }
    printf '%-10s ' "$terminal"
    case $name in
      io) timed "$terminal" "$BENCH/corpus.txt" 5 ;;
      fire) timed "$terminal" "$BENCH/fire.txt" 3 ;;
      mem) memory "$terminal" ;;
    esac
  done
done

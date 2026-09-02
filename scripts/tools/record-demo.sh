#!/usr/bin/env bash
# Record the landing-page demo: drive the real cdh TUI inside tmux, snapshot the
# pane at a fixed rate, rasterize the snapshots, and encode docs/demo.webm,
# docs/demo.mp4 and docs/demo-poster.png.
#
# The demo runs against a throwaway HOME so nothing on the recording machine can
# leak into a published asset, and every captured frame is scanned for the real
# user and host name before anything is encoded.
set -euo pipefail

# The picker only splits list and preview side by side from 108 columns up
# (PREVIEW_SIDE_MIN_WIDTH), and the full footer hint needs 109, so 110 columns is
# the narrowest width that shows the real layout. At 8 CSS px per column that is
# 880 px wide, which fits the landing page's prose column without a horizontal
# scrollbar. Everything is rasterized at 2x device pixels (cell 16x36, font 27.31)
# and displayed at 1x, so text stays sharp on HiDPI screens. The 16:36 cell and
# 27.31:36 font-to-line ratio are the proportions measured in a real terminal.
# The cell height must stay divisible by 4: the ambient cube is drawn with
# Braille, whose dot grid is 2x4 per cell, and an uneven division skews the dots.
COLS=110
ROWS=30
CELL_W=16
CELL_H=36
FONT_SIZE=27.31
# The cube animates on a 50 ms tick, so 12 fps samples it densely enough to read
# as rotation. 232 frames is 19.3 s.
FPS=12
TOTAL_FRAMES=232
# All three assets ship on the landing page's critical path; 700 KiB is the point
# where the page stops feeling instant on a slow connection.
BUDGET_BYTES=716800

# "<frame index>:<tmux key>". Help accepts any key and F2 walks straight from
# help into settings, so the arc never needs a stray keystroke to get between
# overlays. The two trailing Escapes are one gesture in the picker's own terms:
# Esc first hides the preview, and only then clears the query.
SCHEDULE=(
  24:p 30:r 36:o
  56:Down 68:Down
  92:Tab
  116:Tab
  140:F1
  164:F2
  188:Escape
  204:Escape 208:Escape 214:Tab
)

FONT=${CDH_DEMO_FONT:-/mnt/c/Windows/Fonts/CascadiaCode.ttf}
# Cascadia stops at 1483 codepoints and does not map U+FF1F, the full-width
# question mark the help overlay lists as a key. Without a fallback that cell
# rendered as a .notdef box; the renderer picks per codepoint from this chain.
# Colon-separated, because font paths on this platform contain spaces.
IFS=: read -r -a FALLBACK_FONTS <<< "${CDH_DEMO_FALLBACK_FONTS:-/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc}"

# The preview panel prints the selected candidate's absolute path, so the
# throwaway HOME lands on screen -- and its mktemp suffix changes every run,
# which is not something to publish as the page's main visual. The renderer
# rewrites the prefix to this fixed home in cell space, where the substitution
# provably cannot move a column: the rest of the path slides left to stay
# contiguous and the slack is paid back as blanks at the end of the row. Keep it
# no longer than the demo home's own path. The gate below still scans the raw
# captures, so masking cannot be used to launder a real leak.
PUBLIC_HOME=/home/dev

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
socket="cdh-demo-$$"
work=""
demo_home=""

cleanup() {
  tmux -L "$socket" kill-server > /dev/null 2>&1 || true
  if [ -n "$work" ]; then
    rm -rf -- "$work"
  fi
  if [ -n "$demo_home" ]; then
    rm -rf -- "$demo_home"
  fi
}
trap cleanup EXIT

die() {
  echo "record-demo: $*" >&2
  exit 1
}

[ -r "$FONT" ] || die "font not readable: $FONT (override with CDH_DEMO_FONT)"
for fallback in "${FALLBACK_FONTS[@]}"; do
  [ -r "$fallback" ] || die "fallback font not readable: $fallback (override with CDH_DEMO_FALLBACK_FONTS)"
done
for tool in tmux ffmpeg python3 git; do
  command -v "$tool" > /dev/null 2>&1 || die "missing required tool: $tool"
done
python3 -c 'import PIL' > /dev/null 2>&1 || die "missing python module: PIL (Pillow)"

work=$(mktemp -d "${TMPDIR:-/tmp}/cdh-demo-work.XXXXXX")
# Kept short for two reasons: the preview elides a path that outgrows the panel,
# and PUBLIC_HOME has to fit inside it for the mask to stay width-preserving.
demo_home=$(mktemp -d "${TMPDIR:-/tmp}/cdh.XXXX")
[ "${#PUBLIC_HOME}" -le "${#demo_home}" ] \
  || die "PUBLIC_HOME ($PUBLIC_HOME) is longer than the demo home ($demo_home)"
mkdir -p "$work/frames" "$work/png"

echo "record-demo: building the release binary"
cargo build --release --locked --manifest-path "$repo_root/Cargo.toml"
binary="$repo_root/target/release/cdh"
[ -x "$binary" ] || die "release binary not found at $binary"

# --- the throwaway demo home ---------------------------------------------------

# A directory set that looks like a machine somebody actually works on. It has
# to be big enough to fill the visible list, because a half-empty list is what
# the landing page's first frame would otherwise show.
DEMO_DIRS=(
  projects/awesome-app/src
  projects/awesome-app/tests
  projects/awesome-app/docs
  projects/api-server/src
  projects/api-server/migrations
  projects/web-dashboard/src
  projects/cli-tools
  projects/dotfiles
  work/quarterly-report
  work/design-docs
  work/onboarding
  study/rust-book/exercises
  study/wasm-notes
  study/algorithms
)

mkdir -p "$demo_home/.config/cdh" "$demo_home/.local/share/cdh/history"
for relative in "${DEMO_DIRS[@]}"; do
  mkdir -p "$demo_home/$relative"
done

# Enough files that the preview panel has something to list for every directory
# the recording selects. An empty one reads as "Empty directory", which is a poor
# thing for the landing page to spend seven seconds showing.
printf '%s\n' 'name = "awesome-app"' > "$demo_home/projects/awesome-app/Cargo.toml"
printf '%s\n' '# awesome-app' > "$demo_home/projects/awesome-app/README.md"
printf '%s\n' '- wire up the picker' > "$demo_home/projects/awesome-app/TODO.md"
printf '%s\n' 'fn main() {}' > "$demo_home/projects/awesome-app/src/main.rs"
printf '%s\n' 'pub mod picker;' > "$demo_home/projects/awesome-app/src/lib.rs"
printf '%s\n' 'pub fn run() {}' > "$demo_home/projects/awesome-app/src/picker.rs"
printf '%s\n' '#[test]' 'fn smoke() {}' > "$demo_home/projects/awesome-app/tests/smoke.rs"
printf '%s\n' '# Architecture' > "$demo_home/projects/awesome-app/docs/design.md"
printf '%s\n' '# Keybindings' > "$demo_home/projects/awesome-app/docs/keys.md"
printf '%s\n' 'fn main() {}' > "$demo_home/projects/api-server/src/main.rs"
printf '%s\n' '-- create users' > "$demo_home/projects/api-server/migrations/0001_init.sql"
printf '%s\n' 'export const App = () => null;' > "$demo_home/projects/web-dashboard/src/app.tsx"
printf '%s\n' '## Q3' > "$demo_home/work/quarterly-report/summary.md"
printf '%s\n' '# Ownership' > "$demo_home/work/design-docs/rfc-0001.md"
printf '%s\n' 'fn main() {}' > "$demo_home/study/rust-book/exercises/guessing_game.rs"

for repo in awesome-app api-server; do
  root="$demo_home/projects/$repo"
  git -C "$root" -c init.defaultBranch=main init -q
  git -C "$root" add -A
  git -C "$root" \
    -c user.name=demo -c user.email=demo@example.invalid \
    -c commit.gpgsign=false -c core.hooksPath=/dev/null \
    commit -q -m "initial commit"
done
# awesome-app keeps a tracked file dirty so the preview shows "● main · modified".
printf '%s\n' '- wire up the picker' '- record the demo' > "$demo_home/projects/awesome-app/TODO.md"

printf '%s\n' \
  'language = "en"' \
  'theme = "graphite"' \
  'preview = true' \
  'color = true' \
  'mouse = false' \
  > "$demo_home/.config/cdh/tui.toml"

# Frecency history: `<visits> <hours since the last one> <path>`, expanded to one
# `<unix seconds>\t<absolute path>` line per visit. The counts are what put
# awesome-app on top, and the varied ages are what the preview reports as
# "last visit" while the selection moves.
now=$(date +%s)
history_dir="$demo_home/.local/share/cdh/history"
: > "$history_dir/history_raw"
: > "$history_dir/history_uniq"
while read -r visits hours relative; do
  [ -n "$visits" ] || continue
  newest=$((now - hours * 3600 - 600))
  for ((visit = 0; visit < visits; visit++)); do
    printf '%s\t%s\n' "$((newest - visit * 5400))" "$demo_home/$relative" \
      >> "$history_dir/history_raw"
  done
  printf '%s\n' "$demo_home/$relative" >> "$history_dir/history_uniq"
done << 'EOF'
18 0 projects/awesome-app
11 3 projects/api-server
9 1 projects/awesome-app/src
6 27 projects/cli-tools
5 8 projects/web-dashboard
4 52 work/quarterly-report
3 6 study/rust-book
2 96 study/wasm-notes
2 19 projects/dotfiles
1 73 work/design-docs
EOF

# --- drive the TUI and snapshot the pane ---------------------------------------

# `-f` starts from an empty config so the recording never picks up the operator's
# tmux settings, and RGB has to be declared explicitly or tmux quantizes the
# picker's true-color output down to 256 colors.
cat > "$work/tmux.conf" << 'EOF'
set -g default-terminal "tmux-256color"
set -as terminal-features ",*:RGB"
set -g status off
set -g escape-time 10
EOF

target="cdh-demo:0.0"
# Scan roots are pinned to the three work trees instead of letting the default
# `$HOME` BFS run. On a real machine the visible list is frecency-ranked out of
# tens of thousands of discovered directories, so unvisited plumbing never
# surfaces; in a home this small the BFS reaches `~/.config` and
# `~/.local/share/cdh/history` immediately and fills the first frame with the
# demo's own scaffolding.
tmux -f "$work/tmux.conf" -L "$socket" new-session -d -s cdh-demo \
  -x "$COLS" -y "$ROWS" -c "$demo_home" \
  -e "HOME=$demo_home" \
  -e "XDG_CONFIG_HOME=$demo_home/.config" \
  -e "XDG_DATA_HOME=$demo_home/.local/share" \
  -e "XDG_STATE_HOME=$demo_home/.local/state" \
  -e "XDG_CACHE_HOME=$demo_home/.cache" \
  -e "CDH_SCAN_ROOTS=$demo_home/projects:$demo_home/work:$demo_home/study" \
  -e "COLORTERM=truecolor" \
  -e "CDH_CORNER_3D=1" \
  -- "$binary"

# Let the directory scan finish before frame 0, so the poster shows a settled
# list instead of a half-filled one.
sleep 2.5

echo "record-demo: capturing $TOTAL_FRAMES frames at ${FPS}fps"
interval_ns=$((1000000000 / FPS))
start_ns=$(date +%s%N)
for ((index = 0; index < TOTAL_FRAMES; index++)); do
  for entry in "${SCHEDULE[@]}"; do
    if [ "${entry%%:*}" = "$index" ]; then
      tmux -L "$socket" send-keys -t "$target" "${entry#*:}"
    fi
  done
  tmux -L "$socket" capture-pane -e -p -t "$target" \
    > "$(printf '%s/frames/frame-%04d.txt' "$work" "$index")"
  now_ns=$(date +%s%N)
  delay_ms=$(((start_ns + (index + 1) * interval_ns - now_ns) / 1000000))
  if ((delay_ms > 0)); then
    sleep "$(printf '%d.%03d' $((delay_ms / 1000)) $((delay_ms % 1000)))"
  fi
done
elapsed_ms=$((($(date +%s%N) - start_ns) / 1000000))

# A dead pane means a keystroke quit the picker part-way through, which would
# silently turn the tail of the recording into a frozen or blank screen.
dead=$(tmux -L "$socket" list-panes -t "$target" -F '#{pane_dead}')
[ "$dead" = "0" ] || die "the picker exited during recording -- the key schedule quit it early"

# --- refuse to publish anything carrying the recording machine's identity ------

real_user=$(id -un)
real_host=$(uname -n)
for needle in "$real_user" "$real_host" "$(printf '%s' "$real_host" | tr '[:upper:]' '[:lower:]')" "/home/$real_user"; do
  [ -n "$needle" ] || continue
  if grep -rqF -- "$needle" "$work/frames"; then
    die "captured frames contain '$needle' -- refusing to publish"
  fi
done

# --- rasterize and encode ------------------------------------------------------

fallback_args=()
for fallback in "${FALLBACK_FONTS[@]}"; do
  fallback_args+=(--fallback-font "$fallback")
done

# Cross-check the masking: the renderer has to rewrite exactly as many prefixes
# as the raw captures contain. A lower count would mean an occurrence slipped
# through in a form the mask did not recognise, and `/tmp/` is then forbidden
# outright so a prefix elided down to its head cannot survive either.
expected_masks=$(grep -rhoF -- "$demo_home" "$work/frames" | wc -l)
[ "$expected_masks" -gt 0 ] || die "no captured frame shows the demo home -- the mask would be untested"

python3 "$repo_root/scripts/tools/render_demo_frames.py" \
  --frames "$work/frames" --out "$work/png" --font "$FONT" "${fallback_args[@]}" \
  --cols "$COLS" --rows "$ROWS" \
  --cell-width "$CELL_W" --cell-height "$CELL_H" --font-size "$FONT_SIZE" \
  --default-fg '#d8e1f5' --default-bg '#141822' \
  --mask-path "$demo_home=$PUBLIC_HOME" --forbid "${demo_home%/*}/" \
  | tee "$work/render.log"
masked=$(sed -n 's/.*, \([0-9][0-9]*\) paths masked.*/\1/p' "$work/render.log")
[ "${masked:-0}" -ge "$expected_masks" ] \
  || die "masked $masked of $expected_masks demo-home occurrences -- one would have shipped"

# Encode at the rate the frames were actually captured at, so playback runs at
# the speed the cube really spins even if the capture loop drifted.
rate="${TOTAL_FRAMES}000/$elapsed_ms"
echo "record-demo: captured in ${elapsed_ms}ms, encoding at $rate fps"

docs="$repo_root/docs"
ffmpeg -loglevel error -y -framerate "$rate" -i "$work/png/frame-%04d.png" \
  -c:v libvpx-vp9 -pix_fmt yuv444p -b:v 0 -crf 38 -g 240 -row-mt 1 \
  -deadline good -cpu-used 2 -an "$docs/demo.webm"
ffmpeg -loglevel error -y -framerate "$rate" -i "$work/png/frame-%04d.png" \
  -c:v libx264 -pix_fmt yuv420p -crf 30 -preset veryslow -tune stillimage -g 240 \
  -movflags +faststart -an "$docs/demo.mp4"
python3 -c 'import sys; from PIL import Image; Image.open(sys.argv[1]).quantize(colors=256).save(sys.argv[2], optimize=True)' \
  "$work/png/frame-0000.png" "$docs/demo-poster.png"

total=0
for asset in demo.webm demo.mp4 demo-poster.png; do
  bytes=$(wc -c < "$docs/$asset")
  total=$((total + bytes))
  printf 'record-demo: %-16s %7d bytes\n' "$asset" "$bytes"
done
printf 'record-demo: total %d bytes (budget %d)\n' "$total" "$BUDGET_BYTES"
[ "$total" -le "$BUDGET_BYTES" ] || die "assets exceed the $BUDGET_BYTES byte budget"

echo "record-demo: done"

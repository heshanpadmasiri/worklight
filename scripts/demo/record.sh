#!/usr/bin/env bash
# Record docs/demo.gif.
#
#   scripts/demo/record.sh [output.gif]
#
# setup.sh builds a throwaway tmux session; this opens the Worklight panel over
# it and drives it with `tmux send-keys` while an asciinema recorder sits
# attached, then renders the cast with agg. Nothing here touches a real
# database or work session.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/release/worklight"
SESSION="${WORKLIGHT_DEMO_SESSION:-worklight-demo}"
RECORDER="${SESSION}-recorder"
STATE="${WORKLIGHT_DEMO_STATE:-/tmp/worklight-demo}"
DB="$STATE/worklight.db"
CAST="$STATE/demo.cast"
GIF="${1:-$ROOT/docs/demo.gif}"
COLS=120
ROWS=44

for tool in asciinema agg tmux sqlite3; do
  command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }
done

WORKLIGHT_DEMO_COLUMNS=$COLS WORKLIGHT_DEMO_LINES=$ROWS "$ROOT/scripts/demo/setup.sh"

# asciinema needs a tty, and the recorded pty has to be exactly the size of the
# demo window or tmux reflows the panes on attach. A single-pane tmux session
# with no status line gives both.
tmux kill-session -t "$RECORDER" 2>/dev/null || true
rm -f "$CAST"
tmux new-session -d -s "$RECORDER" -x "$COLS" -y "$ROWS" \
  "asciinema rec --overwrite --quiet --command 'TMUX= tmux attach -t $SESSION' '$CAST'"
tmux set-option -t "$RECORDER" status off
sleep 3
START=$(date +%s)

PANEL=""
# The panel is a picker, not a dashboard: it closes itself once it has jumped
# somewhere, so each step of the demo opens a fresh one.
open_panel() {
  # Closing the panel hands its rows back to whichever pane is next to it, so
  # even the layout out again before opening the next one.
  tmux select-layout -t "$SESSION" even-vertical
  PANEL=$(tmux split-window -v -f -t "$SESSION" -l 17 -P -F '#{pane_id}' \
    "WORKLIGHT_DB=$DB $BIN panel")
  sleep 1.5
}
key() { tmux send-keys -t "$PANEL" "$@"; }
pause() { sleep "$1"; }

# The panes as you left them: three agents and three builds, all elsewhere.
pause 2

# Open the panel over them.
open_panel
pause 2

# Walk the selection down the agent table.
key Down; pause 0.6
key Down; pause 0.9
key Up;   pause 0.4
key Up;   pause 1.3

# Enter jumps to the pane the waiting agent is asking from, and closes the panel.
key Enter; pause 3.5

# `a` acknowledges something that finished, dropping it off the list, and `q`
# closes the panel without going anywhere.
open_panel
pause 1.2
key Down; pause 1
key a;    pause 2.5
key q;    pause 2

# The same jump works for a process: here, the build that failed.
open_panel
pause 1.2
key Down; pause 0.3
key Down; pause 0.3
key Down; pause 1.3
key Enter; pause 3.5

# Detach rather than kill, so the recording ends on the panes instead of on
# tmux's "[exited]" notice.
pause 2
CUT=$(( $(date +%s) - START ))
tmux detach-client -s "$SESSION"
for _ in $(seq 40); do tmux has-session -t "$RECORDER" 2>/dev/null || break; sleep 0.25; done
tmux kill-session -t "$RECORDER" 2>/dev/null || true
tmux kill-session -t "$SESSION" 2>/dev/null || true

# Cut tmux's detach teardown off the end of the recording.
python3 "$ROOT/scripts/demo/trim_cast.py" "$CAST" "$CUT"

mkdir -p "$(dirname "$GIF")"
agg --font-family "JetBrainsMono Nerd Font Mono" --font-size 16 --theme asciinema \
  --idle-time-limit 2 "$CAST" "$GIF"
echo "wrote $GIF ($(du -h "$GIF" | cut -f1))"

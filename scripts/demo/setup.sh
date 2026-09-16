#!/usr/bin/env bash
# Build the throwaway tmux session recorded for the README demo.
#
# The session holds only the panes doing work; record.sh opens the Worklight
# panel over them. Everything lives in a scratch database and a dedicated tmux
# session, so the recording never touches a real database or work session.
set -euo pipefail

SESSION="${WORKLIGHT_DEMO_SESSION:-worklight-demo}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/release/worklight"
STATE="${WORKLIGHT_DEMO_STATE:-/tmp/worklight-demo}"
DB="$STATE/worklight.db"
COLS="${WORKLIGHT_DEMO_COLUMNS:-120}"
ROWS="${WORKLIGHT_DEMO_LINES:-44}"

[ -x "$BIN" ] || { echo "build the release binary first: cargo build --release" >&2; exit 1; }

tmux kill-session -t "$SESSION" 2>/dev/null || true
rm -rf "$STATE"
mkdir -p "$STATE"

# Where the demo's agents and builds are pretending to run. Any directory works;
# these are just plausible ones.
WORK1="${WORKLIGHT_DEMO_DIR1:-$HOME/Projects/nutcraker}"
WORK2="${WORKLIGHT_DEMO_DIR2:-$HOME/Projects/ballerina-lang}"
WORK3="$ROOT"
for dir in "$WORK1" "$WORK2"; do
  [ -d "$dir" ] || {
    echo "missing demo directory: $dir (override with WORKLIGHT_DEMO_DIR1/2)" >&2
    exit 1
  }
done

# Each pane runs one of these instead of an interactive shell. Registering from
# inside the pane is what ties the agent and the process to it: Worklight reads
# $TMUX_PANE at registration time, and that pane is where `enter` lands. Holding
# the pane on a `sleep` rather than a prompt also keeps the text put — a shell
# would redraw its prompt every time the panel resizes the pane, scrolling the
# output the demo is there to show.
pane_script() {
  local path="$1" kind="$2" agent_out="$3" command="$4" process_out="$5" text="$6"
  cat > "$path" <<INNER
#!/bin/sh
"$BIN" --database "$DB" agent register "$kind" > "$STATE/$agent_out"
"$BIN" --database "$DB" process start "$command" > "$STATE/$process_out"
clear
printf '%s\n' $text
exec sleep 86400
INNER
  chmod +x "$path"
}

pane_script "$STATE/pane1.sh" claude agent-claude "cargo test --workspace" proc-test \
  "'claude: refactor the parser fixtures' '' 'running cargo test --workspace ...' 'test result: ok. 412 passed'"
pane_script "$STATE/pane2.sh" codex agent-codex "gradle :jballerina-unit-test:test" proc-gradle \
  "'codex: apply patch to compiler/BIRGen.java? [y/N]' '' 'gradle :jballerina-unit-test:test' '> Executing test 214/903'"
pane_script "$STATE/pane3.sh" pi agent-pi "cargo build --release" proc-build \
  "'pi: rebased onto main, build is red' '' '   Compiling worklight v0.1.0' 'error: could not compile worklight (1 error)'"

# Pane ids, not indices: the recording must not depend on the reader's
# base-index or pane-base-index settings.
P1=$(tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS" -c "$WORK1" -P -F '#{pane_id}' "$STATE/pane1.sh")
P2=$(tmux split-window -v -t "$P1" -c "$WORK2" -P -F '#{pane_id}' "$STATE/pane2.sh")
tmux split-window -v -t "$P2" -c "$WORK3" "$STATE/pane3.sh"
tmux select-layout -t "$SESSION" even-vertical

# A recording wants the panes and nothing else.
tmux set-option -t "$SESSION" status off
tmux set-option -t "$SESSION" remain-on-exit off

# Wait for the panes to finish registering before reading the ids back.
for _ in $(seq 60); do
  [ -s "$STATE/proc-build" ] && break
  sleep 0.25
done

wl() { "$BIN" --database "$DB" "$@" >/dev/null; }
id() { tr -d '[:space:]' < "$STATE/$1"; }

# A snapshot worth looking at: one agent waiting on the user, one still
# working, one finished, plus a passing, a failing and a still-running process.
wl agent status "$(id agent-claude)" working
wl agent status "$(id agent-codex)" working
wl agent status "$(id agent-codex)" waiting
wl agent status "$(id agent-pi)" working
wl agent status "$(id agent-pi)" done
wl process finish "$(id proc-test)" 0
wl process finish "$(id proc-build)" 101

# Everything above was registered seconds ago, so every row would read "0s".
# Backdate the timestamps to the kind of elapsed times the panel exists for.
now=$(( $(date +%s) * 1000 ))   # Worklight stores milliseconds.
sqlite3 "$DB" <<SQL
UPDATE agents SET started_at = $now - 1560000 WHERE id = $(id agent-claude);
UPDATE agents SET started_at = $now - 240000  WHERE id = $(id agent-codex);
UPDATE agents SET started_at = $now - 720000  WHERE id = $(id agent-pi);
UPDATE processes SET started_at = $now - 856000, finished_at = $now - 122000 WHERE id = $(id proc-test);
UPDATE processes SET started_at = $now - 460000, finished_at = $now - 431000 WHERE id = $(id proc-build);
UPDATE processes SET started_at = $now - 95000 WHERE id = $(id proc-gradle);
SQL

tmux select-pane -t "$P1"
echo "$SESSION ready (database: $DB)"

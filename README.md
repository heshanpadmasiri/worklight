# Worklight

Manual tracking of user-started commands: record that a command started, record
its exit code when it ends, and see everything in a terminal panel that can
take you back to where each command was started.

Worklight never executes the command you name. A command label is metadata.

## Commands

    worklight start "cargo test"          # prints a tracking id
    worklight finish <id> <exit_code>     # records the result
    worklight get <id>
    worklight list [--active]
    worklight acknowledge <id>
    worklight focus <id> [navigation arguments...]
    worklight panel                       # same as running with no command

Focus passes navigation arguments to the recorded orchestrator. The tmux
orchestrator accepts `--client NAME` to switch only that attached client.

Global options: `--database PATH` and `--dry-run`. `--dry-run` reads the
database but never writes to it; `start --dry-run` prints `0` and issues no
tracking id, which is the signal for a future shell hook not to report a
completion for that run.

State is derived from the exit code, never stored: no exit code means running,
zero means succeeded, anything else means failed (exit 130 shows as
`failed/130`). Acknowledgment is a boolean.

Each command that reports a run prints one tab-separated line:

    id  state  exit  elapsed_ms  acknowledged  orchestrator  cwd  label

Within fields, backslash, tab, carriage return, and newline are escaped as
`\\`, `\t`, `\r`, and `\n`, so every run occupies exactly one eight-column row.

## Panel

`worklight` with no command opens the Ratatui panel. It refreshes every second;
arrows or `j`/`k` select, `Enter` navigates to the selected run, `a`
acknowledges, `q` or `Esc` quits. Opening the panel acknowledges nothing;
successful navigation acknowledges a completed run. The panel calls the same
application functions the CLI does — it runs no Worklight subprocess and issues
no SQL of its own.

## Where a run was started

At start Worklight detects tmux (socket, server instance, pane) and falls back
to a shell record holding the working directory. At finish it detects again and
saves the refreshed destination with the completion, so finishing from another
pane moves where the panel takes you. A shell record cannot be focused; the
panel says so rather than guessing.

## Install and set up

    mise run setup            # shows the planned diffs, then asks
    mise run setup -- -y      # accept ordinary confirmations
    mise run setup -- --dry-run

Setup installs the binary through Cargo's installation directory (honoring
`CARGO_HOME`) and writes `integrations.zsh` and `integrations.tmux` under the
user config directory, loaded from marked blocks in `.zshrc` and the tmux
configuration. The zsh integration only puts the binary directory on `PATH`; no
tracking hooks are installed in this stage. The tmux integration binds
prefix + Space to a popup that opens the panel and passes the initiating client.

Existing prefix + Space bindings, ambiguous configuration paths and duplicated
blocks are reported rather than overwritten. Changed files are backed up,
unrelated content is preserved, and a dry run builds, installs and writes
nothing. Start a new zsh session to pick up shell changes; loading the binding
into an already-running tmux server is opt-in with `--reload-tmux`.

## Development

    mise run build            # cargo build --release
    mise run check            # rustfmt and clippy
    mise run test             # fast native correctness tests
    mise run test:e2e         # CLI and setup end-to-end tests
    mise run test:terminal    # PTY and tmux end-to-end tests
    mise run bench            # benchmarks against isolated SQLite fixtures

Every end-to-end test file is directly runnable (`python3 e2e/test_cli.py`) and
owns temporary HOME, config and database paths, so your own database, shell
configuration and tmux server are never touched. The test suite is fast and
has nothing disabled in it: scale lives in the benchmarks, not behind an
ignored test.

`mise run bench` builds real fixtures up to a million completed runs, reports
latency percentiles and throughput per history size, and checks the query plan
at every size — it exits non-zero if the active list stops using its partial
index or if any operation fails. `bench/baselines/` holds measured baselines;
numeric budgets are not set until baselines exist for both Linux and macOS.

## Layout

    src/storage.rs         the Storage trait, its errors and shared validation
    src/storage/sqlite.rs  SQLite implementation, schema and transactions
    src/orchestrator.rs    tmux/shell detection and navigation
    src/process.rs         the tracked run and its derived state
    src/app.rs             application functions shared by the CLI and panel
    src/cli.rs             argument parsing, dispatch and formatting
    src/tui.rs             the Ratatui panel

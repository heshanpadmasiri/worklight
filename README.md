# Worklight

Worklight automatically tracks selected foreground commands entered in
interactive zsh and tracks supported agent-harness runtimes. Its terminal panel
shows actionable work and can navigate back to the originating shell or tmux
pane.

Worklight never executes or wraps a tracked command. Command labels are
metadata, and harness integrations report their own lifecycle through the agent
CLI.

## Commands

### Manually tracked processes

    worklight process start "cargo test"          # prints a tracking id
    worklight process finish <id> <exit_code>     # records the result
    worklight process get <id>
    worklight process list [--active]
    worklight process acknowledge <id>
    worklight process focus <id> [navigation arguments...]

A process state is derived from its exit code: no exit code means `running`,
zero means `succeeded`, and anything else means `failed` (`130` is shown as
`failed/130`).

`start` accepts commands whose first literal token is `cargo`, `make`, `gmake`,
`go`, `npm`, `npx`, `pnpm`, `pnpx`, `yarn`, `bun`, `gradle`, `gradlew`, `mvn`,
or `mvnw`, as well as `git rebase`. Literal `sudo` followed immediately by one
of those commands is also accepted. Matching is case-sensitive; assignments,
paths, wrappers, quoted/escaped executables, sudo options, other Git
subcommands, and command lines containing a top-level background operator are
rejected. Foreground pipelines and conditional chains are tracked as one run.

An ineligible `start` prints exactly `0`, succeeds, and creates no database.
`--dry-run start` does the same for every command without validation. The zsh
hook retains only a positive ID, so rejected commands receive no completion.

### Agents

    worklight agent register <kind>       # prints a positive tracking id
    worklight agent status <id> <status>
    worklight agent get <id>
    worklight agent list [--active]
    worklight agent acknowledge <id>
    worklight agent focus <id> [navigation arguments...]

The five agent statuses are `idle`, `working`, `waiting`, `done`, and `killed`.
Allowed effective transitions are:

- `idle` to `working` or `killed`;
- `working` to `waiting`, `done`, or `killed`;
- `waiting` to `working` or `killed`;
- `done` to `working` or `killed`;
- none from `killed`, which is terminal.

Reporting the current status again succeeds without changing the row. Every
real status transition clears acknowledgment, so a previously acknowledged
`done` agent becomes actionable again when it starts `working`. Only `done`
agents can be acknowledged; repeated acknowledgment is harmless.

Agent and process IDs come from separate tables and may have the same numeric
value. Always use the `agent` subcommands for agent IDs.

### Output and global options

Global options are `--database PATH` and `--dry-run`. Dry-run reads and
validates without writing or navigating. A dry-run process start or agent
registration prints `0` and creates no tracking row.

Commands that report a process print escaped TSV columns:

    id  state  exit  elapsed_ms  acknowledged  orchestrator  cwd  label

Commands that report an agent print:

    id  status  acknowledged  elapsed_ms  orchestrator  cwd  kind

Within fields, backslash, tab, carriage return, and newline are escaped as
`\\`, `\t`, `\r`, and `\n`, so every item occupies exactly one row.

## Panel

Run `worklight panel`, or simply `worklight`, to open the Ratatui panel. It
synchronizes persisted state every five seconds. Arrows or `j`/`k` select,
`Enter` navigates, `h` toggles acknowledged process history, and `q` or `Esc`
quits.

Agents and processes are displayed in separate tables. Both show command,
state, elapsed time, and working directory; processes additionally show exit
status. Their box titles make a type column unnecessary. Actionable agents are
ordered `waiting`, `idle`,
`working`, then `done`, with newer agents first inside each status.
Acknowledged `done` agents and `killed` agents
are hidden. Hidden acknowledged agents remain synchronized and reappear after
normal synchronization if the same runtime starts working again.

Opening or refreshing the panel acknowledges nothing. Successful navigation
to a completed process acknowledges it. Successful navigation to an agent
conditionally acknowledges it only if its current stored status is still
`done`; failed navigation never acknowledges and opens a confirmation popup
that can delete the stale agent or process from the database. The `h` history
view is process-only and does nothing to agent state. Administrative
acknowledgment remains available through the process and agent CLI commands.

## Where work started

At start or registration Worklight records the current tmux pane and working
directory, or falls back to a shell record holding the working directory. A
process finish updates its destination from the finishing environment. An agent
keeps its registration destination for its runtime. The pane identifier is
sufficient for navigation within the current tmux server; no server or socket
metadata is persisted. The tmux orchestrator accepts `--client NAME` during
focus to switch only that attached client. A plain shell record cannot be
focused; Worklight reports that rather than guessing.

## Pi integration

Setup installs a dependency-free global Pi extension that represents each Pi
extension runtime as one Worklight agent. It maps Pi events as follows:

- session start registers an `idle` agent;
- agent work reports `working`;
- blocking extension UI prompts during active agent work report `waiting`, then
  `working` when closed (prompts from idle extension commands are ignored);
- final settlement reports `working` then `done` (the repeated `working`
  repairs missed updates and closes a waiting span);
- session shutdown reports terminal `killed`.

Pi runtime replacement flows—including `/new`, `/resume`, `/fork`, `/clone`,
and `/reload`—shut down the old runtime and register a new ID. Worklight IDs
are not stored in Pi session files or reused.

Tracking is best effort: a Worklight command failure never fails a Pi lifecycle
event. Shutdown waits at most 12 seconds for queued updates and cleanup. A
crash, forced termination, command failure, or timeout can therefore leave a
stale agent; Worklight performs no heartbeat, PID polling, or stale-row cleanup.

## Claude Code integration

Setup installs a dependency-free hook script and registers it in the Claude Code
settings file, representing each Claude Code session as one Worklight agent. It
maps Claude Code hook events as follows:

- session start registers an `idle` agent;
- a submitted prompt reports `working`;
- a blocking prompt — a permission request or an elicitation dialog — reports
  `waiting`, but only while the agent is `working`, so the idle and quota
  notifications that arrive after a turn are ignored;
- the next completed tool call reports `working` again, because Claude Code
  emits no permission-granted event;
- the end of a turn reports `working` then `done` (the repeated `working`
  repairs missed updates and closes a waiting span that a denied permission
  would otherwise leave open);
- session end reports terminal `killed`.

Compaction continues the same session, so its agent is kept. Every other start —
startup, `/clear`, `--resume`, and `/branch` — retires the agent the session
recorded and registers a new ID. Subagents are part of one main-agent run and
report nothing of their own.

Because each hook is a separate process, the Worklight ID lives in a state file
per session under `${XDG_STATE_HOME}/worklight/claude`, or
`${HOME}/.local/state/worklight/claude`. A status equal to the recorded one is
never sent, so the hook on tool completion usually runs no command at all. The
file is removed at session end, and files older than seven days are pruned at
session start.

Tracking is best effort: the hook always exits zero, never writes to standard
output, and records failures in `hook.log` beside the state files. A status that
fails to report is retried by the next event that wants it. A crash, forced
termination, or command failure can leave a stale agent; Worklight performs no
heartbeat, PID polling, or stale-row cleanup.

## Install and set up

    mise run setup            # show planned diffs, then ask
    mise run setup -- -y      # accept ordinary confirmations
    mise run setup -- --dry-run

Setup installs the binary through Cargo's installation directory (honoring
`CARGO_HOME`) and writes `integrations.zsh` and `integrations.tmux` under the
user config directory, loaded from marked blocks in `.zshrc` and the tmux
configuration. Under the same single setup confirmation, the zsh integration
adds idempotent `preexec`/`precmd` hooks that send the complete unevaluated
command line to the installed binary and finish accepted runs with zsh's final
status. The tmux integration binds prefix + Space to a popup that opens the
panel and passes the initiating client.

The rendered Pi extension is installed at:

    ${PI_CODING_AGENT_DIR}/extensions/worklight/index.ts

when `PI_CODING_AGENT_DIR` is nonempty, or otherwise at:

    ${HOME}/.pi/agent/extensions/worklight/index.ts

It embeds the absolute Cargo-installed Worklight binary path, so Pi does not
depend on a newly changed shell `PATH`. Setup does not edit Pi settings or
reload Pi. New Pi processes auto-discover the extension; run `/reload` in an
existing Pi process after setup.

The rendered Claude Code hook is installed at:

    ${CLAUDE_CONFIG_DIR}/worklight/hook.py

when `CLAUDE_CONFIG_DIR` is nonempty, or otherwise at
`${HOME}/.claude/worklight/hook.py`. It embeds the same absolute binary path and
is run as `/usr/bin/env python3`, so it needs no executable bit. Setup registers
it in `settings.json` beside it, replacing only the hook entries that name this
script and preserving every other setting and hook. JSON has no marked-block
equivalent, so that file is re-serialized rather than edited in place: the full
diff is shown before anything is written, a backup is kept, and a second run
produces identical text. A settings file that is not valid JSON, or whose hook
configuration has an unexpected shape, is reported rather than overwritten.
Start a new Claude Code session after setup.

Every generated file, including the Pi extension, participates in setup's
diff, confirmation, dry-run, backup, idempotency, and partial-failure behavior.
Existing prefix + Space bindings, ambiguous configuration paths, and duplicate
marked blocks are reported rather than overwritten. Unrelated content is
preserved. Start a new zsh session for shell changes; loading the tmux binding
into an existing server is opt-in with `--reload-tmux`.

## Database migration

Current databases use schema version 3. Opening a writable version-2 database
migrates it transactionally by adding agent storage and indexes; existing
process, shell, and tmux rows are not rewritten. A failed migration rolls back
and leaves version 2 intact. Read-only and dry-run access to a version-2
database reports that a writable migration is required and makes no change.
Older, unknown, and nonempty versionless schemas remain incompatible.

## Development

    mise run build            # cargo build --release
    mise run check            # rustfmt and clippy
    mise run test             # fast native correctness tests
    mise run test:e2e         # CLI and setup end-to-end tests
    mise run test:terminal    # PTY and tmux end-to-end tests
    node agents/pi/index.test.mjs  # dependency-free Pi lifecycle harness
    mise run bench            # isolated SQLite benchmarks

Every end-to-end Python file is directly runnable and owns temporary HOME,
configuration, and database paths. The Pi harness renders the setup-time binary
placeholder into a temporary extension and drives it through a fake extension
API. The Claude Code tests render the same placeholder into a temporary hook and
run it once per event, as Claude Code does, against a stub binary that records
the commands it was asked to run.

## Layout

    agents/claude/hook.py    Claude Code lifecycle hook source template
    agents/pi/index.ts       Pi lifecycle extension source template
    scripts/setup.py         binary and integration installer
    src/agent.rs             harness-neutral tracked agent runtime
    src/storage.rs           storage facade and shared validation
    src/storage/sqlite.rs    SQLite schema, migration, and transactions
    src/orchestrator.rs      tmux/shell detection and navigation
    src/process.rs           manually tracked process run
    src/cli.rs               parsing, dispatch, and TSV formatting
    src/tui.rs               combined agent/process panel

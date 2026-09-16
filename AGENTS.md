# AGENTS.md

## Architecture

Worklight is a Rust CLI/TUI that records foreground commands and agent-harness lifecycles in SQLite, then navigates back to their originating shell or tmux pane.

- `src/main.rs` — binary entry point; delegates to the CLI.
- `src/cli.rs` — Clap parsing, command dispatch, dry-run behavior, and TSV output.
- `src/process.rs` — tracked command lifecycle and derived process state.
- `src/agent.rs` — agent lifecycle and status-transition rules.
- `src/storage.rs` — storage facade and validation.
- `src/storage/sqlite.rs` — SQLite schema, migrations, queries, and transactions.
- `src/orchestrator.rs` — captures shell/tmux context and performs navigation.
- `src/tui.rs` — Ratatui panel, synchronization, selection, and actions.
- `src/error.rs` — shared errors.
- `agents/{pi,claude,codex}/` — dependency-light lifecycle integration templates.
- `scripts/setup.py` — installs the binary and renders/configures integrations.
- `src/tests/` — Rust unit/integration-style tests.
- `e2e/` — isolated Python CLI, setup, hook, PTY, and tmux tests.

Process and agent IDs belong to separate tables and may overlap. Integrations are best-effort: hook failures must not disrupt the host agent. Generated integrations embed the installed binary path and should remain dependency-light.

## Useful commands

Use `mise` for the standard development workflow:

```sh
mise run build          # release build
mise run check          # rustfmt check + clippy with warnings denied
mise run test           # fast Rust tests
mise run test:e2e       # CLI/setup/integration tests
mise run test:terminal  # PTY and tmux tests
mise run bench          # release build + SQLite benchmarks
mise run setup -- --dry-run  # preview installation/config changes
```

Targeted checks:

```sh
cargo test <test_name>
node agents/pi/index.test.mjs
python3 e2e/test_codex_integration.py
python3 e2e/test_claude_hook.py
```

Before finishing a change, run `mise run check` and the narrowest relevant test suite. Use temporary HOME/config/database paths in integration tests; never mutate the developer's real configuration.

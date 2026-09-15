---
name: deploy-worklight
description: Deploy or update Worklight from this checkout, including the Rust binary, zsh/tmux integrations, Pi extension, and Claude Code hooks. Use when asked to deploy, install, reinstall, or refresh Worklight or one of its agent-harness integrations.
compatibility: Requires mise, Rust/Cargo, and Python 3. Run from the Worklight repository.
---

# Deploy Worklight

Always deploy from the repository root. The deployment uses the current working tree, including uncommitted changes; report the current branch/commit and dirty status before running it.

## Choose the deployment scope

### Binary only

Use this only when the Rust binary changed and no generated integration needs refreshing:

```bash
mise run install
```

This runs `cargo install --path . --force` and installs `worklight` in Cargo's bin directory (`$CARGO_HOME/bin`, or `~/.cargo/bin`). It does not update any shell, tmux, Pi, or Claude Code files.

Verify it with:

```bash
command -v worklight
worklight --help
```

### Binary and all integrations

This is the normal deployment path, and is required after changing anything under `agents/` or `scripts/setup.py`:

```bash
mise run setup -- --dry-run
mise run setup -- -y
```

The dry run only displays planned file changes; it does not build, install, or write anything. Review its output before applying. `-y` accepts the ordinary setup confirmation.

Setup first builds the release binary and installs it through Cargo. Only after that succeeds does it update generated integrations. Existing changed files are backed up beside the originals as `.worklight-<timestamp>.bak`.

## Integration deployment details

The setup command deploys all integrations together; it has no per-integration selector. Do not copy files from `agents/` directly because harness files contain a `__WORKLIGHT_BINARY__` placeholder that setup replaces with the absolute installed binary path.

### Pi

Source:

```text
agents/pi/index.ts
```

Installed destination:

```text
${PI_CODING_AGENT_DIR}/extensions/worklight/index.ts
```

If `PI_CODING_AGENT_DIR` is empty or unset, the destination is:

```text
${HOME}/.pi/agent/extensions/worklight/index.ts
```

After deployment:

- New Pi processes discover the extension automatically.
- In an existing Pi process, run `/reload`.
- If deploying to a non-default Pi installation, set the override on both preview and apply commands:

```bash
PI_CODING_AGENT_DIR=/path/to/pi-agent mise run setup -- --dry-run
PI_CODING_AGENT_DIR=/path/to/pi-agent mise run setup -- -y
```

### Claude Code

Source:

```text
agents/claude/hook.py
```

Installed hook destination:

```text
${CLAUDE_CONFIG_DIR}/worklight/hook.py
```

If `CLAUDE_CONFIG_DIR` is empty or unset, the destination is:

```text
${HOME}/.claude/worklight/hook.py
```

Setup also updates `settings.json` in that Claude configuration directory. It replaces only hook entries that name Worklight's generated hook, preserves unrelated settings and hooks, and re-serializes the JSON. Review the dry-run diff carefully.

After deployment, start a new Claude Code session; existing sessions do not reload the hooks.

For a non-default Claude configuration, set the override on both commands:

```bash
CLAUDE_CONFIG_DIR=/path/to/claude-config mise run setup -- --dry-run
CLAUDE_CONFIG_DIR=/path/to/claude-config mise run setup -- -y
```

## Shell and tmux integrations

Full setup also deploys:

- zsh command tracking to `${XDG_CONFIG_HOME:-$HOME/.config}/worklight/integrations.zsh`, with a marked loader block in `.zshrc`;
- the tmux popup binding to `${XDG_CONFIG_HOME:-$HOME/.config}/worklight/integrations.tmux`, with a marked loader block in the selected tmux config.

Start a new zsh session after deployment. To load the generated tmux integration into the running default server during deployment, use:

```bash
mise run setup -- -y --reload-tmux
```

For a tmux server on a specific socket:

```bash
mise run setup -- -y --reload-tmux /path/to/socket
```

Setup refuses ambiguous zsh/tmux paths, malformed or duplicate marked blocks, an existing prefix + Space binding, invalid Claude settings JSON, and unexpected Claude hook shapes. Resolve the reported conflict rather than bypassing setup. Use `--zshrc PATH` or `--tmux-conf PATH` when setup asks for an explicit choice.

## Completion report

Report:

1. whether binary-only or full setup was run;
2. the installed binary path;
3. whether configuration changed or was already current;
4. any warnings or failures;
5. required activation steps: `/reload` for existing Pi, a new Claude Code session, a new zsh session, and tmux reload status.

#!/usr/bin/env python3
"""End-to-end tests for `mise run setup`.

Every test owns a temporary HOME and CARGO_HOME, so the developer's own shell
configuration, tmux configuration and installed binaries are never touched.
"""

from __future__ import annotations

import importlib.util
import json
import os
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKOUT = Path(__file__).resolve().parent.parent
SETUP = CHECKOUT / "scripts" / "setup.py"
TIMEOUT = 600

_SETUP_SPEC = importlib.util.spec_from_file_location("worklight_setup", SETUP)
assert _SETUP_SPEC is not None and _SETUP_SPEC.loader is not None
setup_module = importlib.util.module_from_spec(_SETUP_SPEC)
_SETUP_SPEC.loader.exec_module(setup_module)


class SetupTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="worklight-setup-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.home = self.root / "home"
        (self.home / ".config").mkdir(parents=True)
        self.cargo_home = self.root / "cargo"
        self.env = {
            "HOME": str(self.home),
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            "CARGO_HOME": str(self.cargo_home),
            # Reuse the checkout's build artifacts so installing is not a
            # cold build for every test.
            "CARGO_TARGET_DIR": str(CHECKOUT / "target"),
            # rustup keeps its toolchains outside CARGO_HOME; without this the
            # sanitized environment has no compiler at all.
            "RUSTUP_HOME": os.environ.get(
                "RUSTUP_HOME", str(Path.home() / ".rustup")
            ),
            "TERM": "dumb",
        }
        self.config = self.home / ".config" / "worklight"
        self.zshrc = self.home / ".zshrc"
        self.tmux_conf = self.home / ".config" / "tmux" / "tmux.conf"
        self.binary = self.cargo_home / "bin" / "worklight"
        self.pi_agent_dir = self.home / ".pi" / "agent"
        self.pi_extension = (
            self.pi_agent_dir / "extensions" / "worklight" / "index.ts"
        )
        self.claude_dir = self.home / ".claude"
        self.claude_hook = self.claude_dir / "worklight" / "hook.py"
        self.claude_settings = self.claude_dir / "settings.json"
        self.codex_home = self.home / ".codex"
        self.codex_hooks = self.codex_home / "hooks.json"
        self.codex_integration = self.config / "integrations.codex.py"

    def setup(
        self,
        *args: str,
        stdin: str | None = None,
        env: dict[str, str] | None = None,
        checkout: Path | None = None,
    ) -> subprocess.CompletedProcess:
        script = (checkout or CHECKOUT) / "scripts" / "setup.py"
        command = [sys.executable, str(script), *args]
        completed = subprocess.run(
            command,
            input=stdin if stdin is not None else "",
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
            env={**self.env, **(env or {})},
        )
        completed.args = command
        return completed

    def detail(self, result: subprocess.CompletedProcess) -> str:
        return (
            f"command: {' '.join(result.args)}\nexit code: {result.returncode}\n"
            f"stdout: {result.stdout!r}\nstderr: {result.stderr!r}"
        )

    def assertUntouched(self) -> None:
        self.assertFalse(self.config.exists(), "config directory was created")
        self.assertFalse(self.binary.exists(), "binary was installed")
        self.assertFalse(self.pi_agent_dir.exists(), "Pi agent directory was created")
        self.assertFalse(self.claude_dir.exists(), "Claude Code directory was created")
        self.assertFalse(self.codex_home.exists(), "Codex directory was created")


class PreviewTests(SetupTestCase):
    def test_a_dry_run_shows_diffs_and_changes_nothing(self) -> None:
        result = self.setup("--dry-run")

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertIn("integrations.zsh", result.stdout)
        self.assertIn(str(self.pi_extension), result.stdout)
        self.assertIn(str(self.claude_hook), result.stdout)
        self.assertIn(str(self.claude_settings), result.stdout)
        self.assertIn(str(self.codex_hooks), result.stdout)
        self.assertIn(str(self.codex_integration), result.stdout)
        self.assertIn("dry run", result.stdout)
        self.assertUntouched()

    def test_a_dry_run_overrides_yes(self) -> None:
        result = self.setup("-y", "--dry-run")

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertUntouched()

    def test_noninteractive_without_acceptance_fails(self) -> None:
        result = self.setup()

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertIn("without confirmation", result.stderr)
        self.assertUntouched()

    def test_refusal_changes_nothing(self) -> None:
        # Stdin is not a terminal here, which is the strictest case: setup
        # refuses rather than assuming yes.
        result = self.setup(stdin="n\n")

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertUntouched()


class ConfigurationTests(SetupTestCase):
    def test_ambiguous_tmux_configuration_requires_resolution(self) -> None:
        self.tmux_conf.parent.mkdir(parents=True)
        self.tmux_conf.write_text("set -g mouse on\n")
        (self.home / ".tmux.conf").write_text("set -g mouse on\n")

        result = self.setup("--dry-run")

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertIn("--tmux-conf", result.stderr)

    def test_an_existing_binding_is_reported_rather_than_overwritten(self) -> None:
        self.tmux_conf.parent.mkdir(parents=True)
        self.tmux_conf.write_text("bind-key Space copy-mode\n")

        result = self.setup("--dry-run")

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertIn("already binds prefix + Space", result.stderr)
        self.assertEqual(self.tmux_conf.read_text(), "bind-key Space copy-mode\n")

    def test_a_quoted_space_binding_is_reported(self) -> None:
        self.tmux_conf.parent.mkdir(parents=True)
        self.tmux_conf.write_text("bind-key 'Space' copy-mode\n")

        result = self.setup("--dry-run")

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertIn("already binds prefix + Space", result.stderr)

    def test_a_literal_space_binding_is_reported(self) -> None:
        self.tmux_conf.parent.mkdir(parents=True)
        self.tmux_conf.write_text("bind-key ' ' copy-mode\n")

        result = self.setup("--dry-run")

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertIn("already binds prefix + Space", result.stderr)

    def test_a_root_table_space_binding_does_not_conflict(self) -> None:
        self.tmux_conf.parent.mkdir(parents=True)
        self.tmux_conf.write_text("bind-key -T root Space copy-mode\n")

        result = self.setup("--dry-run")

        self.assertEqual(result.returncode, 0, self.detail(result))

    def test_a_duplicated_block_requires_resolution(self) -> None:
        self.zshrc.write_text(
            "# >>> worklight >>>\nsource x\n# <<< worklight <<<\n"
            "# >>> worklight >>>\nsource y\n# <<< worklight <<<\n"
        )

        result = self.setup("--dry-run")

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertIn("more than one worklight block", result.stderr)

    def test_zdotdir_is_respected(self) -> None:
        zdotdir = self.home / "zsh"
        zdotdir.mkdir()
        (zdotdir / ".zshrc").write_text("# zdotdir\n")

        result = self.setup("--dry-run", env={"ZDOTDIR": str(zdotdir)})

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertIn(str(zdotdir / ".zshrc"), result.stdout)
        self.assertNotIn(f"{self.home}/.zshrc", result.stdout)

    def test_generated_hooks_quote_an_absolute_binary_path(self) -> None:
        cargo_home = self.root / "cargo home with 'quote"

        result = self.setup("--dry-run", env={"CARGO_HOME": str(cargo_home)})

        self.assertEqual(result.returncode, 0, self.detail(result))
        expected = (cargo_home / "bin" / "worklight").resolve()
        self.assertIn(shlex.quote(str(expected)), result.stdout)
        self.assertIn("_worklight_hook_preexec", result.stdout)
        self.assertIn("_worklight_hook_precmd", result.stdout)

    def test_relative_cargo_home_generates_an_absolute_hook_path(self) -> None:
        result = self.setup("--dry-run", env={"CARGO_HOME": "relative-cargo"})

        self.assertEqual(result.returncode, 0, self.detail(result))
        expected = (CHECKOUT / "relative-cargo" / "bin" / "worklight").resolve()
        self.assertIn(str(expected), result.stdout)

    def test_custom_paths_are_honored(self) -> None:
        custom_zshrc = self.root / "custom.zshrc"
        custom_tmux = self.root / "custom.tmux.conf"
        custom_zshrc.write_text("# custom\n")
        custom_tmux.write_text("# custom\n")

        result = self.setup(
            "--dry-run", "--zshrc", str(custom_zshrc), "--tmux-conf", str(custom_tmux)
        )

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertIn(str(custom_zshrc), result.stdout)
        self.assertIn(str(custom_tmux), result.stdout)

    def test_pi_agent_directory_override_is_honored(self) -> None:
        custom_pi = self.root / "custom-pi"

        result = self.setup(
            "--dry-run", env={"PI_CODING_AGENT_DIR": str(custom_pi)}
        )

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertIn(
            str(custom_pi / "extensions" / "worklight" / "index.ts"),
            result.stdout,
        )
        self.assertNotIn(str(self.pi_extension), result.stdout)
        self.assertFalse(custom_pi.exists())

    def test_empty_pi_agent_directory_override_uses_home_default(self) -> None:
        result = self.setup("--dry-run", env={"PI_CODING_AGENT_DIR": ""})

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertIn(str(self.pi_extension), result.stdout)

    def test_claude_configuration_directory_override_is_honored(self) -> None:
        custom = self.root / "custom-claude"

        result = self.setup("--dry-run", env={"CLAUDE_CONFIG_DIR": str(custom)})

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertIn(str(custom / "worklight" / "hook.py"), result.stdout)
        self.assertIn(str(custom / "settings.json"), result.stdout)
        self.assertNotIn(str(self.claude_hook), result.stdout)
        self.assertFalse(custom.exists())

    def test_empty_claude_configuration_override_uses_home_default(self) -> None:
        result = self.setup("--dry-run", env={"CLAUDE_CONFIG_DIR": ""})

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertIn(str(self.claude_hook), result.stdout)

    def test_unparsable_claude_settings_are_reported(self) -> None:
        self.claude_settings.parent.mkdir(parents=True)
        self.claude_settings.write_text("{ not json\n")

        result = self.setup("--dry-run")

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertIn("fix it by hand", result.stderr)

    def test_claude_settings_merge_preserves_unrelated_configuration(self) -> None:
        hook = Path("/home/user/.claude/worklight/hook.py")
        settings = Path("/home/user/.claude/settings.json")
        existing = json.dumps(
            {
                "model": "opus",
                "hooks": {
                    "Stop": [
                        {"hooks": [{"type": "command", "command": "notify-send done"}]},
                        {
                            "hooks": [
                                {
                                    "type": "command",
                                    "command": f"/usr/bin/env python3 {hook} Stop",
                                    "timeout": 10,
                                }
                            ]
                        },
                    ],
                    "PreToolUse": [
                        {"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}
                    ],
                },
            }
        )

        merged = setup_module.claude_settings_contents(existing, hook, settings)
        parsed = json.loads(merged)

        self.assertEqual(parsed["model"], "opus")
        self.assertEqual(
            parsed["hooks"]["PreToolUse"],
            [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}],
        )
        # The unrelated Stop hook survives; the stale worklight one is replaced
        # rather than duplicated.
        stop = parsed["hooks"]["Stop"]
        self.assertEqual(len(stop), 2)
        self.assertEqual(stop[0]["hooks"][0]["command"], "notify-send done")
        self.assertIn(str(hook), stop[1]["hooks"][0]["command"])
        # Re-running produces identical text, so setup reports no change.
        self.assertEqual(
            merged, setup_module.claude_settings_contents(merged, hook, settings)
        )

    def test_claude_settings_merge_keeps_text_as_written(self) -> None:
        hook = Path("/home/user/.claude/worklight/hook.py")
        settings = Path("/home/user/.claude/settings.json")
        existing = '{\n  "note": "an em dash \u2014 stays literal",\n  "hooks": {}\n}\n'

        merged = setup_module.claude_settings_contents(existing, hook, settings)

        self.assertIn("an em dash \u2014 stays literal", merged)
        self.assertNotIn("\\u2014", merged)
        # Only the hooks key changes; nothing else is rewritten.
        self.assertIn('"note": "an em dash \u2014 stays literal",', merged)

    def test_claude_hook_binary_path_is_python_safe(self) -> None:
        unusual = self.root / 'cargo "quoted" \\ path\nnext' / "bin" / "worklight"

        contents = setup_module.claude_hook_contents(unusual)
        rendered = json.dumps(str(unusual.resolve()))

        self.assertIn(f"WORKLIGHT_BINARY = {rendered}", contents)
        self.assertNotIn('"__WORKLIGHT_BINARY__"', contents)

    def test_extension_binary_path_is_typescript_safe(self) -> None:
        unusual = self.root / 'cargo "quoted" \\ path\nnext' / "bin" / "worklight"

        contents = setup_module.pi_extension_contents(unusual)
        rendered = json.dumps(str(unusual.resolve()))

        self.assertIn(f"const WORKLIGHT_BINARY = {rendered};", contents)
        self.assertNotIn('"__WORKLIGHT_BINARY__"', contents)

    def test_codex_home_override_is_honored(self) -> None:
        custom_codex = self.root / "custom-codex"

        result = self.setup("--dry-run", env={"CODEX_HOME": str(custom_codex)})

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertIn(str(custom_codex / "hooks.json"), result.stdout)
        self.assertNotIn(str(self.codex_hooks), result.stdout)
        self.assertFalse(custom_codex.exists())

    def test_existing_codex_hooks_are_preserved(self) -> None:
        self.codex_home.mkdir()
        existing = {
            "description": "personal hooks",
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            {"type": "command", "command": "python3 personal.py"}
                        ]
                    }
                ],
                "CustomEvent": [{"hooks": []}],
            },
            "future": {"enabled": True},
        }
        self.codex_hooks.write_text(json.dumps(existing))

        result = self.setup("-y")

        self.assertEqual(result.returncode, 0, self.detail(result))
        installed = json.loads(self.codex_hooks.read_text())
        self.assertEqual(installed["description"], "personal hooks")
        self.assertEqual(installed["future"], {"enabled": True})
        self.assertEqual(installed["hooks"]["CustomEvent"], [{"hooks": []}])
        stop_handlers = [
            handler
            for group in installed["hooks"]["Stop"]
            for handler in group["hooks"]
        ]
        self.assertIn(
            {"type": "command", "command": "python3 personal.py"}, stop_handlers
        )
        self.assertEqual(
            sum(
                setup_module.CODEX_HOOK_MARKER in handler.get("command", "")
                for handler in stop_handlers
            ),
            1,
        )

    def test_malformed_codex_hooks_are_rejected_without_changes(self) -> None:
        self.codex_home.mkdir()
        self.codex_hooks.write_text("{not json\n")

        result = self.setup("--dry-run")

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertIn("cannot parse Codex hooks", result.stderr)
        self.assertEqual(self.codex_hooks.read_text(), "{not json\n")
        self.assertFalse(self.config.exists())

    def test_codex_hook_binary_path_is_python_safe(self) -> None:
        unusual = self.root / 'cargo "quoted" \\ path\nnext' / "bin" / "worklight"

        contents = setup_module.codex_hook_contents(unusual)

        self.assertIn(
            f"WORKLIGHT_BINARY = {json.dumps(str(unusual.resolve()))}",
            contents,
        )
        self.assertNotIn('"__WORKLIGHT_BINARY__"', contents)


class InstallationTests(SetupTestCase):
    def test_setup_installs_the_binary_and_writes_the_integrations(self) -> None:
        self.zshrc.write_text("export EDITOR=vi\n")

        result = self.setup("-y")

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertTrue(self.binary.exists(), self.detail(result))
        self.assertIn("worklight_bin", (self.config / "integrations.zsh").read_text())
        self.assertIn(
            "display-popup", (self.config / "integrations.tmux").read_text()
        )
        zshrc = self.zshrc.read_text()
        # Unrelated content is preserved and the block is marked.
        self.assertIn("export EDITOR=vi", zshrc)
        self.assertIn("# >>> worklight >>>", zshrc)
        self.assertIn(str(self.config / "integrations.zsh"), zshrc)
        self.assertIn("source-file", self.tmux_conf.read_text())
        # The binding uses the installed binary's absolute path and passes the
        # initiating client.
        integration = (self.config / "integrations.tmux").read_text()
        self.assertIn(str(self.binary), integration)
        self.assertIn("run-shell -C", integration)
        self.assertIn("#{q:client_name}", integration)
        zsh_integration = (self.config / "integrations.zsh").read_text()
        self.assertIn("_worklight_hook_preexec", zsh_integration)
        self.assertIn("_worklight_hook_precmd", zsh_integration)
        self.assertIn("typeset -g _worklight_hook_pending_id", zsh_integration)
        self.assertIn(shlex.quote(str(self.binary)), zsh_integration)
        extension = self.pi_extension.read_text()
        self.assertIn(
            f"const WORKLIGHT_BINARY = {json.dumps(str(self.binary.resolve()))};",
            extension,
        )
        self.assertNotIn("__WORKLIGHT_BINARY__", extension)
        self.assertFalse((self.pi_agent_dir / "settings.json").exists())
        hook = self.claude_hook.read_text()
        self.assertIn(f"WORKLIGHT_BINARY = {json.dumps(str(self.binary.resolve()))}", hook)
        self.assertNotIn("__WORKLIGHT_BINARY__", hook)
        settings = json.loads(self.claude_settings.read_text())
        self.assertEqual(
            sorted(settings["hooks"]),
            [
                "Notification",
                "PostToolUse",
                "SessionEnd",
                "SessionStart",
                "Stop",
                "UserPromptSubmit",
            ],
        )
        for groups in settings["hooks"].values():
            entry = groups[-1]["hooks"][0]
            self.assertEqual(entry["type"], "command")
            self.assertEqual(entry["timeout"], 10)
            self.assertIn(shlex.quote(str(self.claude_hook)), entry["command"])
        self.assertIn("new processes load", result.stdout)
        self.assertIn("/reload", result.stdout)
        self.assertIn("Claude Code", result.stdout)
        codex_runner = self.codex_integration.read_text()
        self.assertIn(
            f"WORKLIGHT_BINARY = {json.dumps(str(self.binary.resolve()))}", codex_runner
        )
        hooks = json.loads(self.codex_hooks.read_text())["hooks"]
        self.assertEqual(set(hooks), set(setup_module.CODEX_EVENTS))
        for groups in hooks.values():
            self.assertEqual(len(groups), 1)
            handler = groups[0]["hooks"][0]
            self.assertEqual(handler["type"], "command")
            self.assertEqual(handler["timeout"], 3)
            self.assertIn(setup_module.CODEX_HOOK_MARKER, handler["command"])
            self.assertIn(str(self.codex_integration.resolve()), handler["command"])
        self.assertIn("new processes load", result.stdout)
        self.assertIn("/reload", result.stdout)
        self.assertIn("use /hooks to review and trust Worklight", result.stdout)

        def codex_event(name: str, **extra: object) -> subprocess.CompletedProcess:
            payload = {
                "session_id": "setup-integration-session",
                "hook_event_name": name,
                "cwd": str(self.root),
                **extra,
            }
            return subprocess.run(
                [
                    sys.executable,
                    str(self.codex_integration),
                    setup_module.CODEX_HOOK_MARKER,
                ],
                input=json.dumps(payload),
                capture_output=True,
                text=True,
                cwd=self.root,
                env=self.env,
                timeout=10,
            )

        for event in (
            codex_event("SessionStart", source="startup"),
            codex_event("UserPromptSubmit", prompt="test"),
            codex_event("Stop", last_assistant_message="done"),
        ):
            self.assertEqual(event.returncode, 0, event.stderr)
            self.assertEqual(event.stdout, "")
        database = self.home / ".local" / "share" / "worklight" / "worklight.db"
        with sqlite3.connect(database) as connection:
            agent_id, kind, status = connection.execute(
                "SELECT id,kind,status FROM agents ORDER BY id DESC LIMIT 1"
            ).fetchone()
        self.assertEqual((kind, status), ("codex", 4))

        ended = codex_event("SessionEnd", reason="other")
        self.assertEqual(ended.returncode, 0, ended.stderr)
        completed = subprocess.run(
            [str(self.binary), "agent", "get", str(agent_id)],
            capture_output=True,
            text=True,
            env=self.env,
            timeout=10,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("\tkilled\t", completed.stdout)

    def test_rerunning_changes_nothing_and_writes_no_backup(self) -> None:
        self.assertEqual(self.setup("-y").returncode, 0)
        before = self.zshrc.read_text()

        result = self.setup("-y")

        self.assertEqual(result.returncode, 0, self.detail(result))
        self.assertIn("already up to date", result.stdout)
        self.assertEqual(self.zshrc.read_text(), before)
        self.assertEqual(list(self.home.glob(".zshrc.worklight-*.bak")), [])
        self.assertEqual(
            list(self.pi_extension.parent.glob("index.ts.worklight-*.bak")), []
        )
        self.assertEqual(
            list(self.claude_hook.parent.glob("hook.py.worklight-*.bak")), []
        )
        self.assertEqual(
            list(self.claude_dir.glob("settings.json.worklight-*.bak")), []
        )
        self.assertEqual(
            list(self.codex_home.glob("hooks.json.worklight-*.bak")), []
        )

    def test_an_update_backs_up_and_preserves_unrelated_content(self) -> None:
        self.zshrc.write_text("export EDITOR=vi\n")
        self.assertEqual(self.setup("-y").returncode, 0)
        # Stale files, as an older version would have written.
        self.zshrc.write_text(
            "export EDITOR=vi\n# >>> worklight >>>\nsource /old/path\n# <<< worklight <<<\n"
            "alias ll='ls -l'\n"
        )
        self.pi_extension.write_text("// stale extension\n")
        self.claude_hook.write_text("# stale hook\n")
        stale_settings = json.loads(self.claude_settings.read_text())
        stale_settings["hooks"]["Stop"] = [
            {"hooks": [{"type": "command", "command": f"python3 {self.claude_hook} Stop"}]}
        ]
        stale_settings["editorMode"] = "vim"
        self.claude_settings.write_text(json.dumps(stale_settings, indent=2) + "\n")
        hooks = json.loads(self.codex_hooks.read_text())
        hooks["hooks"]["Stop"].append(
            {"hooks": [{"type": "command", "command": "python3 personal.py"}]}
        )
        self.codex_hooks.write_text(json.dumps(hooks))

        result = self.setup("-y")

        self.assertEqual(result.returncode, 0, self.detail(result))
        zshrc = self.zshrc.read_text()
        self.assertIn("export EDITOR=vi", zshrc)
        self.assertIn("alias ll='ls -l'", zshrc)
        self.assertNotIn("/old/path", zshrc)
        self.assertTrue(list(self.home.glob(".zshrc.worklight-*.bak")))
        extension_backups = list(
            self.pi_extension.parent.glob("index.ts.worklight-*.bak")
        )
        self.assertEqual(len(extension_backups), 1)
        self.assertEqual(extension_backups[0].read_text(), "// stale extension\n")
        self.assertIn(str(self.binary.resolve()), self.pi_extension.read_text())
        self.assertIn(str(self.binary.resolve()), self.claude_hook.read_text())
        self.assertEqual(
            len(list(self.claude_hook.parent.glob("hook.py.worklight-*.bak"))), 1
        )
        settings = json.loads(self.claude_settings.read_text())
        # An unrelated setting survives and the stale hook entry is not doubled.
        self.assertEqual(settings["editorMode"], "vim")
        self.assertEqual(len(settings["hooks"]["Stop"]), 1)
        installed_hooks = json.loads(self.codex_hooks.read_text())
        stop_handlers = [
            handler
            for group in installed_hooks["hooks"]["Stop"]
            for handler in group["hooks"]
        ]
        self.assertIn(
            {"type": "command", "command": "python3 personal.py"}, stop_handlers
        )
        self.assertEqual(
            sum(
                setup_module.CODEX_HOOK_MARKER in handler.get("command", "")
                for handler in stop_handlers
            ),
            1,
        )
        self.assertTrue(list(self.codex_home.glob("hooks.json.worklight-*.bak")))

    def test_a_requested_tmux_reload_failure_returns_nonzero(self) -> None:
        missing_socket = self.root / "missing-tmux.sock"

        result = self.setup("-y", "--reload-tmux", str(missing_socket))

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertIn("requested live tmux reload did not", result.stderr)
        self.assertTrue(self.binary.exists())
        self.assertTrue((self.config / "integrations.tmux").exists())

    def test_a_build_failure_changes_no_configuration(self) -> None:
        broken = self.root / "broken-checkout"
        shutil.copytree(
            CHECKOUT,
            broken,
            ignore=shutil.ignore_patterns("target", ".git", "*.bak"),
        )
        (broken / "src" / "main.rs").write_text("fn main() { this is not rust }\n")

        result = self.setup("-y", checkout=broken)

        self.assertEqual(result.returncode, 1, self.detail(result))
        self.assertIn("no configuration was changed", result.stderr)
        self.assertFalse(self.config.exists())
        self.assertFalse(self.zshrc.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)

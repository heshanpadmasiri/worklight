#!/usr/bin/env python3
"""Real zsh lifecycle tests for the generated Worklight hooks."""

from __future__ import annotations

import importlib.util
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from harness import WorklightTestCase  # noqa: E402
from terminal import PtySession  # noqa: E402

SETUP_PATH = Path(__file__).resolve().parent.parent / "scripts" / "setup.py"
SPEC = importlib.util.spec_from_file_location("worklight_setup", SETUP_PATH)
assert SPEC and SPEC.loader
setup_module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(setup_module)


class ZshIntegrationTests(WorklightTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.zsh = shutil.which("zsh")
        if self.zsh is None:
            self.skipTest("zsh is not installed")
        self.bin_dir = self.root / "installed bin"
        self.bin_dir.mkdir()
        shutil.copy2(self.binary, self.bin_dir / "worklight")
        self.integration = self.root / "integrations.zsh"
        self.integration.write_text(setup_module.zsh_integration(self.bin_dir))
        self.env["WORKLIGHT_DB"] = str(self.db)

    def shell(self) -> PtySession:
        env = {**self.env, "PS1": "WL-PROMPT:%? > ", "PROMPT": "WL-PROMPT:%? > "}
        session = PtySession([self.zsh, "-f"], env)
        self.addCleanup(session.close)
        self.assertTrue(session.wait_for("WL-PROMPT:"), session.detail())
        return session

    def prompt(self, session: PtySession, status: int) -> None:
        before = session.output.count("WL-PROMPT:")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            session.read(0.2)
            if session.output.count("WL-PROMPT:") > before and f"WL-PROMPT:{status} >" in session.screen():
                return
        self.fail(session.detail())

    def test_real_interactive_hooks_track_complete_lines_and_statuses(self) -> None:
        session = self.shell()
        session.send("cargo() { return ${1:-0} }; worklight() { return 99 }; alias worklight=false\n")
        self.prompt(session, 0)
        session.send(f"source {setup_module.shlex.quote(str(self.integration))}\n")
        self.prompt(session, 0)

        session.send("  cargo 0  \n")
        self.prompt(session, 0)
        session.send("cargo 7 | cargo 3\n")
        self.prompt(session, 3)
        session.send("cargo 4 || cargo 5\n")
        self.prompt(session, 5)
        session.send("definitely_missing_worklight_command\n")
        self.prompt(session, 127)
        session.send("cargo 0 &\n")
        self.prompt(session, 0)
        session.send("exit\n")
        self.assertEqual(session.wait(), 0, session.detail())

        with sqlite3.connect(self.db) as conn:
            rows = conn.execute(
                "SELECT label, exit_status FROM processes ORDER BY id"
            ).fetchall()
        self.assertEqual(
            rows,
            [
                ("  cargo 0  ", 0),
                ("cargo 7 | cargo 3", 3),
                ("cargo 4 || cargo 5", 5),
            ],
        )

    def make_spy(self) -> tuple[Path, Path, Path]:
        spy_dir = self.root / "spy bin"
        spy_dir.mkdir(exist_ok=True)
        log = self.root / "spy.jsonl"
        spy = spy_dir / "worklight"
        spy.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "with open(os.environ['SPY_LOG'], 'a') as stream:\n"
            "    stream.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "if sys.argv[1] == 'start':\n"
            "    sys.stdout.write(os.environ.get('SPY_OUTPUT', ''))\n"
            "    sys.stderr.write('start diagnostic\\n')\n"
            "    raise SystemExit(int(os.environ.get('SPY_START_STATUS', '0')))\n"
            "sys.stdout.write('finish output\\n')\n"
            "sys.stderr.write('finish diagnostic\\n')\n"
            "raise SystemExit(int(os.environ.get('SPY_FINISH_STATUS', '0')))\n"
        )
        spy.chmod(0o755)
        integration = self.root / "spy-integration.zsh"
        integration.write_text(setup_module.zsh_integration(spy_dir))
        return spy, log, integration

    def run_zsh_script(
        self, script: str, env: dict[str, str]
    ) -> subprocess.CompletedProcess:
        return subprocess.run(
            [self.zsh, "-f", "-c", script],
            capture_output=True,
            text=True,
            env={**self.env, **env},
        )

    def test_malformed_start_output_clears_stale_state_and_never_finishes(self) -> None:
        _, log, integration = self.make_spy()
        quoted = setup_module.shlex.quote(str(integration))
        rejected = ["", "0\n", "00\n", "+1\n", "-1\n", " 1\n", "1 \n", "1\n2\n", "1x\n"]
        for index, output in enumerate(rejected):
            with self.subTest(output=output):
                if log.exists():
                    log.unlink()
                completed = self.run_zsh_script(
                    f"source {quoted}; typeset -g _worklight_hook_pending_id=99; "
                    "_worklight_hook_preexec 'cargo safe'; "
                    "print -r -- pending:${_worklight_hook_pending_id:-empty}; "
                    "_worklight_hook_precmd >/dev/null 2>&1; true",
                    {"SPY_LOG": str(log), "SPY_OUTPUT": output},
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertEqual(completed.stdout, "pending:empty\n")
                calls = [json.loads(line) for line in log.read_text().splitlines()]
                self.assertEqual(calls, [["start", "cargo safe"]], index)

    def test_failed_and_unavailable_start_leave_no_pending_completion(self) -> None:
        spy, log, integration = self.make_spy()
        quoted = setup_module.shlex.quote(str(integration))
        script = (
            f"source {quoted}; typeset -g _worklight_hook_pending_id=99; "
            "_worklight_hook_preexec 'cargo safe'; "
            "print -r -- pending:${_worklight_hook_pending_id:-empty}"
        )
        failed = self.run_zsh_script(
            script,
            {
                "SPY_LOG": str(log),
                "SPY_OUTPUT": "9\n",
                "SPY_START_STATUS": "1",
            },
        )
        self.assertEqual((failed.returncode, failed.stdout, failed.stderr), (0, "pending:empty\n", ""))
        self.assertEqual(
            [json.loads(line) for line in log.read_text().splitlines()],
            [["start", "cargo safe"]],
        )

        log.unlink()
        spy.unlink()
        unavailable = self.run_zsh_script(
            script, {"SPY_LOG": str(log), "SPY_OUTPUT": "9\n"}
        )
        self.assertEqual(
            (unavailable.returncode, unavailable.stdout, unavailable.stderr),
            (0, "pending:empty\n", ""),
        )
        self.assertFalse(log.exists())

    def test_exact_arguments_status_suppression_and_failed_finish_is_not_retried(self) -> None:
        _, log, integration = self.make_spy()
        command = "cargo a 'b c' *; echo x & quoted"
        completed = self.run_zsh_script(
            f"source {setup_module.shlex.quote(str(integration))}; "
            f"_worklight_hook_preexec {setup_module.shlex.quote(command)}; "
            "status7() { return 7 }; status7; _worklight_hook_precmd; observed=$?; "
            "_worklight_hook_precmd >/dev/null 2>&1; print -r -- status:$observed",
            {
                "SPY_LOG": str(log),
                "SPY_OUTPUT": "42\n",
                "SPY_FINISH_STATUS": "1",
            },
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout, "status:7\n")
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual(calls, [["start", command], ["finish", "42", "7"]])

    def test_sourcing_twice_registers_once_and_preserves_other_hooks(self) -> None:
        script = self.root / "check.zsh"
        script.write_text(
            "autoload -Uz add-zsh-hook\n"
            "other_hook() { return 0 }\n"
            "add-zsh-hook preexec other_hook\n"
            f"source {setup_module.shlex.quote(str(self.integration))}\n"
            f"source {setup_module.shlex.quote(str(self.integration))}\n"
            "print -rl -- $preexec_functions\n"
            "print -r -- ---\n"
            "print -rl -- $precmd_functions\n"
        )
        completed = subprocess.run(
            [self.zsh, "-f", str(script)], capture_output=True, text=True, env=self.env
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        preexec, precmd = completed.stdout.split("---\n", 1)
        self.assertEqual(preexec.splitlines().count("other_hook"), 1)
        self.assertEqual(preexec.splitlines().count("_worklight_hook_preexec"), 1)
        self.assertEqual(precmd.splitlines().count("_worklight_hook_precmd"), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)

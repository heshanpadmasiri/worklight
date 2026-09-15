#!/usr/bin/env python3
"""Lifecycle tests for the generated Codex command hook."""

from __future__ import annotations

import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKOUT = Path(__file__).resolve().parent.parent
SETUP_PATH = CHECKOUT / "scripts" / "setup.py"
SPEC = importlib.util.spec_from_file_location("worklight_setup", SETUP_PATH)
assert SPEC is not None and SPEC.loader is not None
setup_module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(setup_module)


class CodexHookTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="worklight-codex-hook-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.home = self.root / "home"
        self.data = self.root / "data"
        self.home.mkdir()
        self.log = self.root / "calls.jsonl"
        self.counter = self.root / "counter"
        self.binary = self.root / "fake-worklight"
        self.binary.write_text(
            """#!/usr/bin/env python3
import fcntl, json, os, sys
from pathlib import Path
log = Path(os.environ["FAKE_WORKLIGHT_LOG"])
counter = Path(os.environ["FAKE_WORKLIGHT_COUNTER"])
with counter.open("a+") as state:
    fcntl.flock(state, fcntl.LOCK_EX)
    with log.open("a") as output:
        output.write(json.dumps(sys.argv[1:]) + "\\n")
    if sys.argv[1:3] == ["agent", "register"]:
        state.seek(0)
        value = int(state.read() or "0") + 1
        state.seek(0)
        state.truncate()
        state.write(str(value))
        print(os.environ.get("FAKE_REGISTER_OUTPUT", value))
sys.exit(int(os.environ.get("FAKE_WORKLIGHT_EXIT", "0")))
"""
        )
        self.binary.chmod(0o755)
        self.runner = self.root / "codex-hook.py"
        self.runner.write_text(setup_module.codex_hook_contents(self.binary))
        self.env = {
            "HOME": str(self.home),
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "XDG_DATA_HOME": str(self.data),
            "FAKE_WORKLIGHT_LOG": str(self.log),
            "FAKE_WORKLIGHT_COUNTER": str(self.counter),
        }

    def emit(
        self,
        event: str,
        *,
        session: str = "session-a",
        extra: dict[str, object] | None = None,
        env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess:
        payload = {
            "session_id": session,
            "hook_event_name": event,
            "cwd": str(self.root),
            **(extra or {}),
        }
        return subprocess.run(
            [sys.executable, str(self.runner), setup_module.CODEX_HOOK_MARKER],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            cwd=self.root,
            env={**self.env, **(env or {})},
            timeout=10,
        )

    def calls(self) -> list[list[str]]:
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_complete_lifecycle_and_waiting_state(self) -> None:
        self.assertEqual(
            self.emit("SessionStart", extra={"source": "startup"}).returncode, 0
        )
        self.emit("UserPromptSubmit")
        self.emit("PermissionRequest")
        self.emit("PreToolUse")
        self.emit("PreToolUse")
        self.emit("Stop")
        self.emit("UserPromptSubmit")
        self.emit("Interrupt")
        self.emit("SessionEnd", extra={"reason": "other"})

        self.assertEqual(
            self.calls(),
            [
                ["agent", "register", "codex"],
                ["agent", "status", "1", "working"],
                ["agent", "status", "1", "waiting"],
                ["agent", "status", "1", "working"],
                ["agent", "status", "1", "working"],
                ["agent", "status", "1", "done"],
                ["agent", "status", "1", "working"],
                ["agent", "status", "1", "working"],
                ["agent", "status", "1", "done"],
                ["agent", "status", "1", "killed"],
            ],
        )
        states = self.data / "worklight" / "codex-sessions"
        self.assertEqual(list(states.glob("*.json")), [])

    def test_compaction_and_events_without_registration_are_ignored(self) -> None:
        self.emit("SessionStart", extra={"source": "compact"})
        self.emit("UserPromptSubmit")
        self.emit("Stop")

        self.assertEqual(self.calls(), [])

    def test_runtime_replacement_kills_the_previous_agent(self) -> None:
        self.emit("SessionStart", extra={"source": "startup"})
        self.emit("SessionStart", extra={"source": "resume"})
        self.emit("UserPromptSubmit")

        self.assertEqual(
            self.calls(),
            [
                ["agent", "register", "codex"],
                ["agent", "status", "1", "killed"],
                ["agent", "register", "codex"],
                ["agent", "status", "2", "working"],
            ],
        )

    def test_failed_replacement_does_not_reactivate_a_killed_agent(self) -> None:
        self.emit("SessionStart", extra={"source": "startup"})
        self.emit(
            "SessionStart",
            extra={"source": "resume"},
            env={"FAKE_REGISTER_OUTPUT": "not-an-id"},
        )
        self.emit("UserPromptSubmit")

        self.assertEqual(
            self.calls(),
            [
                ["agent", "register", "codex"],
                ["agent", "status", "1", "killed"],
                ["agent", "register", "codex"],
            ],
        )

    def test_sessions_keep_independent_agent_ids(self) -> None:
        self.emit("SessionStart", session="one", extra={"source": "startup"})
        self.emit("SessionStart", session="two", extra={"source": "startup"})
        self.emit("UserPromptSubmit", session="two")
        self.emit("UserPromptSubmit", session="one")

        self.assertEqual(
            self.calls()[-2:],
            [
                ["agent", "status", "2", "working"],
                ["agent", "status", "1", "working"],
            ],
        )

    def test_invalid_registration_and_command_failures_are_best_effort(self) -> None:
        result = self.emit(
            "SessionStart",
            extra={"source": "startup"},
            env={"FAKE_REGISTER_OUTPUT": "not-an-id"},
        )
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("invalid agent id", result.stderr)
        self.emit("UserPromptSubmit")
        self.assertEqual(self.calls(), [["agent", "register", "codex"]])

        failed = self.emit(
            "SessionStart",
            session="failed",
            extra={"source": "startup"},
            env={"FAKE_WORKLIGHT_EXIT": "2"},
        )
        self.assertEqual(failed.returncode, 0)
        self.assertEqual(failed.stdout, "")
        self.assertIn("worklight:", failed.stderr)

    def test_malformed_input_never_fails_the_hook(self) -> None:
        result = subprocess.run(
            [sys.executable, str(self.runner), setup_module.CODEX_HOOK_MARKER],
            input="not json",
            capture_output=True,
            text=True,
            cwd=self.root,
            env=self.env,
            timeout=10,
        )

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("worklight:", result.stderr)
        self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)

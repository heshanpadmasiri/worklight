#!/usr/bin/env python3
"""End-to-end tests for the Claude Code lifecycle hook.

The rendered hook is driven exactly as Claude Code drives it — one fresh
process per event, payload on stdin — against a stub binary that records the
Worklight commands it was asked to run.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

CHECKOUT = Path(__file__).resolve().parent.parent
SOURCE = CHECKOUT / "agents" / "claude" / "hook.py"
PLACEHOLDER = '"__WORKLIGHT_BINARY__"'
TIMEOUT = 60
SESSION = "3a8f21e0-0000-4000-8000-000000000001"

STUB = """#!/bin/sh
printf '%s|%s\\n' "$*" "$(pwd)" >> "$WORKLIGHT_STUB_LOG"
if [ "$1 $2" = "agent register" ]; then
  printf '%s\\n' "${WORKLIGHT_STUB_ID-1}"
fi
exit "${WORKLIGHT_STUB_EXIT-0}"
"""


class HookTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="worklight-claude-hook-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.state = self.root / "state"
        self.project = self.root / "project"
        self.project.mkdir()
        self.log = self.root / "commands.log"
        self.stub = self.root / "worklight"
        self.stub.write_text(STUB)
        self.stub.chmod(0o755)
        self.hook = self.render(self.stub)
        self.env = {
            "HOME": str(self.root / "home"),
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "XDG_STATE_HOME": str(self.state),
            "WORKLIGHT_STUB_LOG": str(self.log),
        }

    def render(self, binary: Path) -> Path:
        source = SOURCE.read_text()
        self.assertEqual(source.count(PLACEHOLDER), 1, "binary placeholder is not unique")
        path = self.root / f"hook-{binary.name}.py"
        path.write_text(source.replace(PLACEHOLDER, json.dumps(str(binary))))
        return path

    def state_dir(self) -> Path:
        return self.state / "worklight" / "claude"

    def state_file(self, session: str = SESSION) -> Path:
        return self.state_dir() / f"{session}.json"

    def emit(
        self,
        event: str,
        *,
        session: str = SESSION,
        cwd: str | None = None,
        stdin: str | None = None,
        hook: Path | None = None,
        env: dict[str, str] | None = None,
        args: list[str] | None = None,
    ) -> subprocess.CompletedProcess:
        payload: dict[str, object] = {
            "session_id": session,
            "cwd": cwd if cwd is not None else str(self.project),
            "transcript_path": str(self.root / "transcript.jsonl"),
            "hook_event_name": event,
        }
        command = [sys.executable, str(hook or self.hook), *(args if args is not None else [event])]
        result = subprocess.run(
            command,
            input=stdin if stdin is not None else json.dumps(payload),
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
            env={**self.env, **(env or {})},
        )
        # A hook that fails or speaks would disturb Claude Code itself.
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")
        return result

    def commands(self) -> list[list[str]]:
        if not self.log.exists():
            return []
        return [line.split("|")[0].split() for line in self.log.read_text().splitlines()]

    def statuses(self) -> list[str]:
        return [command[3] for command in self.commands() if command[1] == "status"]

    def turn(self) -> None:
        self.emit("UserPromptSubmit")
        self.emit("Stop")


class LifecycleTests(HookTestCase):
    def test_a_full_session_reports_every_span(self) -> None:
        self.emit("SessionStart")
        self.emit("UserPromptSubmit")
        self.emit("Notification")
        self.emit("PostToolUse")
        self.emit("Stop")
        self.emit("SessionEnd")

        self.assertEqual(
            self.commands(),
            [
                ["agent", "register", "claude"],
                ["agent", "status", "1", "working"],
                ["agent", "status", "1", "waiting"],
                ["agent", "status", "1", "working"],
                ["agent", "status", "1", "done"],
                ["agent", "status", "1", "killed"],
            ],
        )

    def test_registration_records_the_session_directory(self) -> None:
        self.emit("SessionStart")
        recorded = self.log.read_text().splitlines()[0].split("|")[1]
        self.assertEqual(Path(recorded).resolve(), self.project.resolve())

    def test_a_done_agent_works_again_on_the_next_turn(self) -> None:
        self.emit("SessionStart")
        self.turn()
        self.turn()
        self.assertEqual(
            self.statuses(), ["working", "done", "working", "done"]
        )

    def test_a_denied_permission_still_settles(self) -> None:
        # Denial produces no PostToolUse, so `waiting` has to be repaired before
        # `done`, which it cannot reach directly.
        self.emit("SessionStart")
        self.emit("UserPromptSubmit")
        self.emit("Notification")
        self.emit("Stop")
        self.assertEqual(self.statuses(), ["working", "waiting", "working", "done"])

    def test_notifications_outside_work_report_nothing(self) -> None:
        self.emit("SessionStart")
        self.emit("Notification")
        self.turn()
        self.emit("Notification")
        self.assertEqual(self.statuses(), ["working", "done"])

    def test_a_settled_status_spawns_no_command(self) -> None:
        self.emit("SessionStart")
        self.emit("UserPromptSubmit")
        before = len(self.commands())
        for _ in range(5):
            self.emit("PostToolUse")
        self.emit("UserPromptSubmit")
        self.assertEqual(len(self.commands()), before)

    def test_unmapped_events_report_nothing(self) -> None:
        self.emit("SessionStart")
        self.emit("UserPromptSubmit")
        for event in ("SubagentStop", "PreCompact", "PostCompact", "PreToolUse"):
            self.emit(event)
        self.assertEqual(self.statuses(), ["working"])


class RuntimeReplacementTests(HookTestCase):
    def test_compaction_keeps_the_same_agent(self) -> None:
        self.emit("SessionStart")
        self.emit("UserPromptSubmit")
        self.emit("SessionStart", stdin=json.dumps(
            {"session_id": SESSION, "cwd": str(self.project), "source": "compact"}
        ))
        self.emit("Stop")
        self.assertEqual(self.commands(), [
            ["agent", "register", "claude"],
            ["agent", "status", "1", "working"],
            ["agent", "status", "1", "done"],
        ])

    def test_any_other_start_retires_the_recorded_agent(self) -> None:
        self.emit("SessionStart")
        self.emit("SessionStart", env={"WORKLIGHT_STUB_ID": "2"}, stdin=json.dumps(
            {"session_id": SESSION, "cwd": str(self.project), "source": "resume"}
        ))
        self.emit("UserPromptSubmit", env={"WORKLIGHT_STUB_ID": "2"})
        self.assertEqual(self.commands(), [
            ["agent", "register", "claude"],
            ["agent", "status", "1", "killed"],
            ["agent", "register", "claude"],
            ["agent", "status", "2", "working"],
        ])

    def test_session_end_forgets_the_session(self) -> None:
        self.emit("SessionStart")
        self.assertTrue(self.state_file().exists())
        self.emit("SessionEnd")
        self.assertFalse(self.state_file().exists())
        # Nothing is left to report against.
        self.emit("Stop")
        self.assertEqual(self.statuses(), ["killed"])

    def test_sessions_are_tracked_independently(self) -> None:
        other = "3a8f21e0-0000-4000-8000-000000000002"
        self.emit("SessionStart")
        self.emit("SessionStart", session=other, env={"WORKLIGHT_STUB_ID": "2"})
        self.emit("UserPromptSubmit")
        self.emit("UserPromptSubmit", session=other)
        self.assertEqual(self.statuses(), ["working", "working"])
        self.assertEqual(
            [command[2] for command in self.commands() if command[1] == "status"],
            ["1", "2"],
        )

    def test_stale_state_is_pruned_at_session_start(self) -> None:
        self.emit("SessionStart")
        stale = self.state_dir() / "3a8f21e0-0000-4000-8000-00000000000f.json"
        shutil.copyfile(self.state_file(), stale)
        old = time.time() - 8 * 24 * 60 * 60
        os.utime(stale, (old, old))
        self.emit("SessionStart", session="3a8f21e0-0000-4000-8000-000000000003")
        self.assertFalse(stale.exists())
        self.assertTrue(self.state_file().exists())


class FailureTests(HookTestCase):
    def test_an_unusable_registration_suppresses_every_status(self) -> None:
        for identifier in ("0", "01", "-1", "+1", "1.0", "", "9223372036854775808", "1 2"):
            with self.subTest(identifier=identifier):
                self.setUp()
                self.emit("SessionStart", env={"WORKLIGHT_STUB_ID": identifier})
                self.turn()
                self.emit("SessionEnd")
                self.assertEqual(self.commands(), [["agent", "register", "claude"]])

    def test_a_failing_binary_suppresses_every_status(self) -> None:
        self.emit("SessionStart", env={"WORKLIGHT_STUB_EXIT": "2"})
        self.turn()
        self.assertEqual(self.commands(), [["agent", "register", "claude"]])

    def test_a_failed_status_is_retried_by_the_next_event(self) -> None:
        self.emit("SessionStart")
        self.emit("UserPromptSubmit", env={"WORKLIGHT_STUB_EXIT": "1"})
        self.emit("Stop")
        self.assertEqual(self.statuses(), ["working", "working", "done"])

    def test_a_missing_binary_changes_nothing(self) -> None:
        hook = self.render(self.root / "absent" / "worklight")
        self.emit("SessionStart", hook=hook)
        self.emit("Stop", hook=hook)
        self.assertEqual(self.commands(), [])
        self.assertFalse(self.state_file().exists())

    def test_failures_are_logged_beside_the_state(self) -> None:
        self.emit("SessionStart", env={"WORKLIGHT_STUB_EXIT": "3"})
        self.assertIn("register", (self.state_dir() / "hook.log").read_text())

    def test_unusable_input_does_nothing(self) -> None:
        cases: list[dict[str, object]] = [
            {"stdin": ""},
            {"stdin": "not json"},
            {"stdin": "[]"},
            {"stdin": json.dumps({"cwd": "/tmp"})},
            {"stdin": json.dumps({"session_id": "", "cwd": "/tmp"})},
            {"stdin": json.dumps({"session_id": "../escape", "cwd": "/tmp"})},
            {"stdin": json.dumps({"session_id": 7, "cwd": "/tmp"})},
            {"args": []},
            {"args": ["SessionStart", "extra"]},
        ]
        for case in cases:
            with self.subTest(case=case):
                self.emit("SessionStart", **case)  # type: ignore[arg-type]
        self.assertEqual(self.commands(), [])
        self.assertFalse(self.state_dir().exists())

    def test_a_missing_working_directory_still_registers(self) -> None:
        self.emit("SessionStart", cwd=str(self.root / "gone"))
        self.assertEqual(self.commands()[0], ["agent", "register", "claude"])

    def test_corrupt_state_is_treated_as_no_agent(self) -> None:
        self.emit("SessionStart")
        self.state_file().write_text("{ not json")
        self.emit("Stop")
        self.assertEqual(self.statuses(), [])


if __name__ == "__main__":
    unittest.main()

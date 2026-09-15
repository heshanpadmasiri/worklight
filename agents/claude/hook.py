#!/usr/bin/env python3
"""Report one Claude Code session into Worklight as a tracked agent.

Setup renders this template, replacing the single Worklight binary placeholder
with an absolute path, and registers it as a Claude Code hook command:

    hook.py <EventName>          # the hook payload arrives as JSON on stdin

Unlike the Pi extension, every hook is a fresh process, so the Worklight agent
id lives in a per-session state file instead of memory.

Tracking is best effort. This script always exits 0 and never writes to stdout:
exit code 2 blocks a Claude Code event, and stdout is fed back into the model's
context on SessionStart and UserPromptSubmit.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

WORKLIGHT_BINARY = "__WORKLIGHT_BINARY__"
AGENT_KIND = "claude"
COMMAND_TIMEOUT_SECONDS = 10
# A crash leaves a state file behind; Worklight itself never cleans up rows.
STALE_STATE_SECONDS = 7 * 24 * 60 * 60
MAX_LOG_BYTES = 1 << 20
MAX_I64 = 9223372036854775807

SESSION_ID = re.compile(r"\A[A-Za-z0-9_-]{1,128}\Z")
AGENT_ID = re.compile(r"\A[1-9][0-9]*\Z")


# --- state ---------------------------------------------------------------


def state_dir(env: dict[str, str]) -> Path:
    base = env.get("XDG_STATE_HOME")
    if base:
        return Path(base).expanduser() / "worklight" / "claude"
    return Path(env.get("HOME", "~")).expanduser() / ".local" / "state" / "worklight" / "claude"


def state_path(directory: Path, session_id: str) -> Path:
    return directory / f"{session_id}.json"


def read_state(path: Path) -> dict[str, str] | None:
    """Return the recorded agent, or None when there is nothing usable."""
    try:
        recorded = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(recorded, dict):
        return None
    agent_id = recorded.get("id")
    status = recorded.get("status")
    if not isinstance(agent_id, str) or not valid_agent_id(agent_id):
        return None
    if not isinstance(status, str):
        return None
    return {"id": agent_id, "status": status}


def write_state(path: Path, agent_id: str, status: str) -> None:
    """Replace the state file atomically; concurrent hooks must never read half of it."""
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    try:
        temporary.write_text(json.dumps({"id": agent_id, "status": status}))
        os.replace(temporary, path)
    except OSError:
        temporary.unlink(missing_ok=True)
        raise


def prune(directory: Path) -> None:
    """Drop state files no live session can own any more."""
    deadline = time.time() - STALE_STATE_SECONDS
    try:
        entries = list(directory.glob("*.json"))
    except OSError:
        return
    for entry in entries:
        try:
            if entry.stat().st_mtime < deadline:
                entry.unlink(missing_ok=True)
        except OSError:
            continue


def log(directory: Path, message: str) -> None:
    """Record a failure where it cannot disturb Claude Code."""
    try:
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / "hook.log"
        if path.exists() and path.stat().st_size > MAX_LOG_BYTES:
            path.unlink(missing_ok=True)
        stamp = time.strftime("%Y-%m-%dT%H:%M:%S")
        with path.open("a") as handle:
            handle.write(f"{stamp} {message}\n")
    except OSError:
        pass


# --- worklight -----------------------------------------------------------


def valid_agent_id(value: str) -> bool:
    return bool(AGENT_ID.match(value)) and int(value) <= MAX_I64


class Session:
    """One Claude Code session's view of its Worklight agent."""

    def __init__(self, directory: Path, session_id: str, cwd: str | None) -> None:
        self.directory = directory
        self.path = state_path(directory, session_id)
        self.cwd = cwd
        self.state = read_state(self.path)

    @property
    def status(self) -> str | None:
        return self.state["status"] if self.state else None

    def execute(self, args: list[str]) -> str | None:
        """Run one Worklight command, returning its stdout, or None on any failure."""
        try:
            result = subprocess.run(
                [WORKLIGHT_BINARY, *args],
                capture_output=True,
                text=True,
                timeout=COMMAND_TIMEOUT_SECONDS,
                cwd=self.cwd,
            )
        except (OSError, subprocess.SubprocessError) as error:
            log(self.directory, f"could not run worklight {' '.join(args)}: {error}")
            return None
        if result.returncode != 0:
            detail = result.stderr.strip() or f"exited with status {result.returncode}"
            log(self.directory, f"worklight {' '.join(args)} failed: {detail}")
            return None
        return result.stdout

    def register(self) -> None:
        stdout = self.execute(["agent", "register", AGENT_KIND])
        if stdout is None:
            return
        agent_id = stdout.strip()
        if not valid_agent_id(agent_id):
            log(self.directory, f"invalid agent id in command output: {agent_id!r}")
            return
        try:
            write_state(self.path, agent_id, "idle")
        except OSError as error:
            log(self.directory, f"could not record agent {agent_id}: {error}")
            return
        self.state = {"id": agent_id, "status": "idle"}

    def report(self, status: str) -> None:
        """Report a status, skipping the call when it would change nothing.

        The recorded status only advances once the command succeeds, so a failed
        report is retried by the next event that wants the same status.
        """
        if self.state is None or self.state["status"] == status:
            return
        if self.execute(["agent", "status", self.state["id"], status]) is None:
            return
        try:
            write_state(self.path, self.state["id"], status)
        except OSError as error:
            log(self.directory, f"could not record status {status}: {error}")
            return
        self.state = {"id": self.state["id"], "status": status}

    def forget(self) -> None:
        try:
            self.path.unlink(missing_ok=True)
        except OSError:
            pass
        self.state = None


# --- events --------------------------------------------------------------


def handle(event: str, payload: dict, session: Session) -> None:
    if event == "SessionStart":
        prune(session.directory)
        # Compaction continues the same session, so its agent is kept as is.
        if payload.get("source") == "compact":
            return
        # Any other start replaces the runtime: retire what the session left.
        session.report("killed")
        session.forget()
        session.register()
    elif event == "UserPromptSubmit":
        session.report("working")
    elif event == "Notification":
        # Matchers select the blocking prompts, and an idle or finished agent is
        # not blocked on the user however it was reached.
        if session.status == "working":
            session.report("waiting")
    elif event == "PostToolUse":
        # There is no permission-granted event; a completed tool closes the span.
        if session.status == "waiting":
            session.report("working")
    elif event == "Stop":
        # `waiting` cannot reach `done` directly, and a denied permission leaves
        # the agent waiting, so repair the span first.
        session.report("working")
        session.report("done")
    elif event == "SessionEnd":
        session.report("killed")
        session.forget()


def main(argv: list[str], stdin: str, env: dict[str, str]) -> int:
    if len(argv) != 2:
        return 0
    try:
        payload = json.loads(stdin)
    except ValueError:
        return 0
    if not isinstance(payload, dict):
        return 0

    session_id = payload.get("session_id")
    if not isinstance(session_id, str) or not SESSION_ID.match(session_id):
        return 0

    # Register where the session actually runs, not where the hook was started.
    cwd = payload.get("cwd")
    if not isinstance(cwd, str) or not os.path.isdir(cwd):
        cwd = None

    directory = state_dir(env)
    handle(argv[1], payload, Session(directory, session_id, cwd))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv, sys.stdin.read(), dict(os.environ)))
    except Exception:  # noqa: BLE001 - a hook failure must never reach Claude Code
        sys.exit(0)

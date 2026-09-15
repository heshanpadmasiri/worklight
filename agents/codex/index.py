#!/usr/bin/env python3
"""Best-effort Codex lifecycle tracking for Worklight.

Codex invokes this program as a command hook and writes one JSON event to
stdin. Setup replaces the binary placeholder with the installed Worklight
path. The program deliberately produces no stdout because hook output can
alter Codex behavior for several lifecycle events.
"""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

WORKLIGHT_BINARY = "__WORKLIGHT_BINARY__"
COMMAND_TIMEOUT_SECONDS = 1.0
MAX_I64 = 9_223_372_036_854_775_807
VALID_STATUSES = {"idle", "working", "waiting", "done", "killed"}


def log_failure(action: str, detail: object) -> None:
    print(f"worklight: could not {action}: {detail}", file=sys.stderr)


def absolute(path: Path, cwd: Path) -> Path:
    return path if path.is_absolute() else cwd / path


def data_home(env: dict[str, str], cwd: Path) -> Path:
    configured = env.get("XDG_DATA_HOME")
    if configured:
        return absolute(Path(configured), cwd)
    return absolute(Path(env.get("HOME", ".")) / ".local" / "share", cwd)


def database_path(env: dict[str, str], cwd: Path) -> Path:
    configured = env.get("WORKLIGHT_DB")
    if configured:
        return absolute(Path(configured), cwd)
    return data_home(env, cwd) / "worklight" / "worklight.db"


def session_paths(
    session_id: str, env: dict[str, str], cwd: Path
) -> tuple[Path, Path]:
    identity = f"{database_path(env, cwd)}\0{session_id}".encode()
    name = hashlib.sha256(identity).hexdigest()
    directory = data_home(env, cwd) / "worklight" / "codex-sessions"
    return directory / f"{name}.json", directory / f"{name}.lock"


def valid_agent_id(value: object) -> str | None:
    if not isinstance(value, str) or not value.isascii() or not value.isdigit():
        return None
    if value.startswith("0"):
        return None
    try:
        parsed = int(value)
    except ValueError:
        return None
    if not 0 < parsed <= MAX_I64 or str(parsed) != value:
        return None
    return value


def read_state(path: Path) -> dict[str, str] | None:
    try:
        value = json.loads(path.read_text())
    except FileNotFoundError:
        return None
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        log_failure("read Codex session state", error)
        return None
    if not isinstance(value, dict):
        log_failure("read Codex session state", "state is not an object")
        return None
    agent_id = valid_agent_id(value.get("agent_id"))
    status = value.get("status")
    if agent_id is None or status not in VALID_STATUSES:
        log_failure("read Codex session state", "state has invalid fields")
        return None
    return {"agent_id": agent_id, "status": status}


def write_state(path: Path, state: dict[str, str]) -> None:
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w") as output:
            json.dump(state, output, separators=(",", ":"))
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def run_worklight(args: list[str]) -> str | None:
    try:
        result = subprocess.run(
            [WORKLIGHT_BINARY, *args],
            capture_output=True,
            text=True,
            timeout=COMMAND_TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.SubprocessError) as error:
        log_failure("run Worklight", error)
        return None
    if result.returncode != 0:
        detail = result.stderr.strip() or f"process exited with status {result.returncode}"
        log_failure(f"run {' '.join(args)}", detail)
        return None
    return result.stdout.strip()


def set_status(path: Path, state: dict[str, str], status: str, *, force: bool = False) -> bool:
    if not force and state["status"] == status:
        return True
    if run_worklight(["agent", "status", state["agent_id"], status]) is None:
        return False
    state["status"] = status
    write_state(path, state)
    return True


def register(path: Path, previous: dict[str, str] | None) -> None:
    if previous is not None:
        if set_status(path, previous, "killed"):
            try:
                path.unlink()
            except FileNotFoundError:
                pass
    output = run_worklight(["agent", "register", "codex"])
    agent_id = valid_agent_id(output)
    if agent_id is None:
        if output is not None:
            log_failure("register Codex agent", "invalid agent id in command output")
        return
    write_state(path, {"agent_id": agent_id, "status": "idle"})


def settle(path: Path, state: dict[str, str]) -> None:
    if set_status(path, state, "working", force=True):
        set_status(path, state, "done", force=True)


def dispatch(event: dict[str, Any], state_path: Path) -> None:
    name = event["hook_event_name"]
    state = read_state(state_path)

    if name == "SessionStart":
        if event.get("source") in {"startup", "resume", "clear"}:
            register(state_path, state)
        return
    if state is None:
        return
    if name == "UserPromptSubmit":
        set_status(state_path, state, "working")
    elif name == "PermissionRequest":
        set_status(state_path, state, "waiting")
    elif name == "PreToolUse":
        if state["status"] == "waiting":
            set_status(state_path, state, "working")
    elif name in {"Stop", "Interrupt"}:
        settle(state_path, state)
    elif name == "SessionEnd":
        if set_status(state_path, state, "killed"):
            try:
                state_path.unlink()
            except FileNotFoundError:
                pass


def main() -> int:
    try:
        event = json.load(sys.stdin)
        if not isinstance(event, dict):
            raise ValueError("hook input is not an object")
        session_id = event.get("session_id")
        name = event.get("hook_event_name")
        cwd_value = event.get("cwd")
        if not all(isinstance(value, str) and value for value in (session_id, name, cwd_value)):
            raise ValueError("hook input lacks session_id, hook_event_name, or cwd")
        cwd = Path(cwd_value)
        state_path, lock_path = session_paths(session_id, dict(os.environ), cwd)
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        with lock_path.open("a+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            dispatch(event, state_path)
    except Exception as error:  # Hooks must never interfere with Codex itself.
        log_failure("track Codex lifecycle", error)
    return 0


if __name__ == "__main__":
    sys.exit(main())

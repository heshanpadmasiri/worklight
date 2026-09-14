"""Shared end-to-end helpers.

Every test owns temporary HOME, config and database paths and a sanitized
environment, so the developer's own database, shell configuration and tmux
server are never touched.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

CHECKOUT = Path(__file__).resolve().parent.parent
TIMEOUT = 30


def binary() -> Path:
    override = os.environ.get("WORKLIGHT_BIN")
    if override:
        return Path(override)
    for profile in ("release", "debug"):
        candidate = CHECKOUT / "target" / profile / "worklight"
        if candidate.exists():
            return candidate
    raise unittest.SkipTest("worklight is not built; run `mise run build`")


class Result:
    def __init__(self, command: list[str], completed: subprocess.CompletedProcess) -> None:
        self.command = command
        self.code = completed.returncode
        self.out = completed.stdout
        self.err = completed.stderr

    @property
    def line(self) -> str:
        return self.out.strip()

    @property
    def rows(self) -> list[list[str]]:
        return [line.split("\t") for line in self.out.splitlines() if line]

    def __str__(self) -> str:
        return (
            f"command: {' '.join(self.command)}\n"
            f"exit code: {self.code}\n"
            f"stdout: {self.out!r}\n"
            f"stderr: {self.err!r}"
        )


class WorklightTestCase(unittest.TestCase):
    """A sanitized environment with its own HOME and database."""

    def setUp(self) -> None:
        self.binary = binary()
        self.root = Path(tempfile.mkdtemp(prefix="worklight-e2e-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.home = self.root / "home"
        self.home.mkdir()
        self.db = self.root / "data" / "worklight.db"
        self.env = {
            "HOME": str(self.home),
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            "XDG_DATA_HOME": str(self.home / ".local" / "share"),
            "TERM": "xterm-256color",
            "LANG": "C.UTF-8",
        }

    def run_worklight(self, *args: str, env: dict[str, str] | None = None) -> Result:
        command = [str(self.binary), "--database", str(self.db), *args]
        completed = subprocess.run(
            command,
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
            env={**self.env, **(env or {})},
        )
        return Result(command, completed)

    def ok(self, *args: str, env: dict[str, str] | None = None) -> Result:
        result = self.run_worklight(*args, env=env)
        self.assertEqual(result.code, 0, f"expected success\n{result}")
        return result

    def fails(self, *args: str, env: dict[str, str] | None = None) -> Result:
        result = self.run_worklight(*args, env=env)
        self.assertNotEqual(result.code, 0, f"expected failure\n{result}")
        return result

    def start(self, label: str, env: dict[str, str] | None = None) -> str:
        result = self.ok("start", label, env=env)
        match = re.fullmatch(r"([0-9]+)\n", result.out, flags=re.ASCII)
        self.assertIsNotNone(match, f"expected one positive process ID line\n{result}")
        process_id = match.group(1)
        self.assertGreater(int(process_id), 0, f"expected a positive process ID\n{result}")
        return process_id

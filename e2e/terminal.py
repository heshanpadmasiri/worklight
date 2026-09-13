"""PTY helpers shared by the terminal end-to-end tests."""

from __future__ import annotations

import fcntl
import os
import pty
import re
import select
import signal
import struct
import subprocess
import termios
import time

ESCAPES = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b[()][AB0]|\r")
ENTER_ALTERNATE_SCREEN = "\x1b[?1049h"
LEAVE_ALTERNATE_SCREEN = "\x1b[?1049l"


class PtySession:
    """A child process attached to a pseudo terminal."""

    def __init__(self, command: list[str], env: dict[str, str]) -> None:
        self.command = command
        self.master, slave = pty.openpty()
        # A zero-sized terminal renders nothing at all.
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 200, 0, 0))
        self.process = subprocess.Popen(
            command,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            close_fds=True,
        )
        os.close(slave)
        self.output = ""

    def read(self, timeout: float = 0.5) -> str:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.master], [], [], 0.05)
            if not ready:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                break
            if not chunk:
                break
            self.output += chunk.decode("utf-8", "replace")
        return self.output

    def wait_for(self, text: str, timeout: float = 10.0) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if text in self.screen():
                return True
            self.read(0.2)
        return text in self.screen()

    def screen(self) -> str:
        return ESCAPES.sub("", self.output)

    def send(self, keys: str) -> None:
        os.write(self.master, keys.encode())

    def wait(self, timeout: float = 10.0) -> int:
        """Wait for exit, draining output so the restore sequence is seen."""
        deadline = time.monotonic() + timeout
        while self.process.poll() is None and time.monotonic() < deadline:
            self.read(0.2)
        self.read(0.5)
        try:
            return self.process.wait(timeout=max(0.1, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            self.process.send_signal(signal.SIGKILL)
            raise

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait(timeout=5)
        try:
            os.close(self.master)
        except OSError:
            pass

    def detail(self) -> str:
        return f"command: {' '.join(self.command)}\nscreen:\n{self.screen()}"

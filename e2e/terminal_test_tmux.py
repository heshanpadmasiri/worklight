#!/usr/bin/env python3
"""tmux end-to-end tests.

Each test runs its own tmux server on its own socket and kills it on success
and on failure, so the developer's tmux server is never involved.
"""

from __future__ import annotations

import shlex
import shutil
import subprocess
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from harness import WorklightTestCase  # noqa: E402
from terminal import PtySession  # noqa: E402

ID, STATE, EXIT, ELAPSED, ACK, KIND, CWD, LABEL = range(8)


class TmuxTestCase(WorklightTestCase):
    def setUp(self) -> None:
        super().setUp()
        if shutil.which("tmux") is None:
            self.skipTest("tmux is not installed")
        self.socket = str(self.root / "tmux.sock")
        self.env["TMUX_TMPDIR"] = str(self.root)
        self.addCleanup(self.kill_server)
        self.new_server()

    # --- server ----------------------------------------------------------

    def tmux(self, *args: str, check: bool = True) -> str:
        command = ["tmux", "-S", self.socket, *args]
        result = subprocess.run(
            command, capture_output=True, text=True, timeout=30, env=self.env
        )
        if check and result.returncode != 0:
            self.fail(
                f"command: {' '.join(command)}\nexit code: {result.returncode}\n"
                f"stdout: {result.stdout!r}\nstderr: {result.stderr!r}"
            )
        return result.stdout.strip()

    def new_server(self) -> None:
        self.tmux(
            "-f", "/dev/null", "new-session", "-d", "-s", "main", "-x", "200", "-y", "50"
        )

    def kill_server(self) -> None:
        subprocess.run(
            ["tmux", "-S", self.socket, "kill-server"],
            capture_output=True,
            text=True,
            env=self.env,
        )

    def server_pid(self) -> str:
        return self.tmux("display-message", "-p", "-F", "#{pid}")

    def attach(self, target: str = "main") -> str:
        """Attach a real client and return its name."""
        existing = set(
            self.tmux("list-clients", "-F", "#{client_name}", check=False).splitlines()
        )
        session = PtySession(
            ["tmux", "-S", self.socket, "attach", "-t", target], self.env
        )
        self.addCleanup(session.close)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            clients = set(
                self.tmux(
                    "list-clients", "-F", "#{client_name}", check=False
                ).splitlines()
            )
            added = clients - existing
            if added:
                return added.pop()
            time.sleep(0.1)
        self.fail(f"no tmux client attached\n{session.detail()}")

    # --- runs inside panes ----------------------------------------------

    def in_pane(self, pane: str, *args: str) -> str:
        """Run worklight from inside a pane, as a user would."""
        self.calls = getattr(self, "calls", 0) + 1
        out = self.root / f"pane-{pane.strip('%')}-{self.calls}.txt"
        quoted = " ".join(shlex.quote(argument) for argument in args)
        self.tmux(
            "send-keys",
            "-t",
            pane,
            f"{shlex.quote(str(self.binary))} --database {shlex.quote(str(self.db))} "
            f"{quoted} > {shlex.quote(str(out))} 2>&1",
            "Enter",
        )
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if out.exists() and out.read_text().strip():
                return out.read_text().strip()
            time.sleep(0.1)
        self.fail(f"pane {pane} wrote no output to {out}")

    def start_in_pane(self, pane: str, label: str) -> str:
        return self.in_pane(pane, "start", label)

    def finish_in_pane(self, pane: str, run_id: str, exit_code: str) -> None:
        self.in_pane(pane, "finish", run_id, exit_code)

    def panes(self) -> list[str]:
        return self.tmux("list-panes", "-a", "-F", "#{pane_id}").splitlines()

    def active_pane(self) -> str:
        return self.tmux("display-message", "-p", "-F", "#{pane_id}")

    def client_details(self, client: str) -> tuple[str, str]:
        clients = self.tmux(
            "list-clients",
            "-F",
            "#{client_name}\t#{client_session}\t#{pane_id}",
        )
        for line in clients.splitlines():
            name, session, pane = line.split("\t")
            if name == client:
                return session, pane
        self.fail(f"client {client!r} not found in {clients!r}")

    def client_session(self, client: str) -> str:
        return self.client_details(client)[0]

    def session_pane(self, session: str) -> str:
        return self.tmux(
            "display-message", "-t", session, "-p", "-F", "#{pane_id}"
        )


class CaptureTests(TmuxTestCase):
    def test_a_run_started_in_a_pane_records_that_pane(self) -> None:
        pane = self.panes()[0]

        run_id = self.start_in_pane(pane, "cargo test")

        row = self.ok("get", run_id).rows[0]
        self.assertEqual(row[KIND], "tmux")
        stored = subprocess.run(
            ["sqlite3", str(self.db), "SELECT pane_id, server_instance FROM tmux"],
            capture_output=True,
            text=True,
        )
        if stored.returncode == 0:
            self.assertIn(pane, stored.stdout)
            self.assertIn(self.server_pid(), stored.stdout)

    def test_concurrent_capture_from_several_panes(self) -> None:
        self.tmux("split-window", "-t", "main")
        self.tmux("split-window", "-t", "main")
        panes = self.panes()

        ids = [self.start_in_pane(pane, f"job-{index}") for index, pane in enumerate(panes)]

        self.assertEqual(len(set(ids)), len(panes))
        self.assertEqual(len(self.ok("list").rows), len(panes))

    def test_without_tmux_variables_a_shell_record_is_kept(self) -> None:
        # Running outside tmux inside the same environment falls back to the
        # shell rather than inventing tmux data.
        run_id = self.start("cargo test")

        row = self.ok("get", run_id).rows[0]

        self.assertEqual(row[KIND], "shell")
        self.assertIn("unavailable", self.fails("focus", run_id).err)


class FocusTests(TmuxTestCase):
    def test_focus_switches_only_the_named_client(self) -> None:
        self.tmux("split-window", "-t", "main")
        target, other = self.panes()[0], self.panes()[1]
        run_id = self.start_in_pane(target, "cargo test")
        self.tmux("select-pane", "-t", other)
        self.tmux("new-session", "-d", "-s", "holding")
        self.tmux("new-session", "-d", "-s", "waiting")
        observer = self.attach("holding")
        client = self.attach("waiting")
        self.assertEqual(self.client_session(observer), "holding")
        self.assertEqual(self.client_session(client), "waiting")

        self.ok("focus", run_id, "--client", client)

        self.assertEqual(self.client_session(client), "main")
        self.assertEqual(self.session_pane("main"), target)
        self.assertEqual(self.client_session(observer), "holding")

    def test_successful_navigation_acknowledges_a_completed_run(self) -> None:
        client = self.attach()
        pane = self.panes()[0]
        run_id = self.start_in_pane(pane, "cargo test")
        self.finish_in_pane(pane, run_id, "0")

        self.ok("focus", run_id, "--client", client)

        self.assertEqual(self.ok("get", run_id).rows[0][ACK], "yes")

    def test_navigating_to_a_running_run_acknowledges_nothing(self) -> None:
        client = self.attach()
        pane = self.panes()[0]
        run_id = self.start_in_pane(pane, "sleep 100")

        self.ok("focus", run_id, "--client", client)

        self.assertEqual(self.ok("get", run_id).rows[0][ACK], "no")

    def test_a_renamed_window_still_resolves(self) -> None:
        client = self.attach()
        pane = self.panes()[0]
        run_id = self.start_in_pane(pane, "cargo test")
        self.finish_in_pane(pane, run_id, "0")
        # Pane display names are resolved at navigation time, not stored.
        self.tmux("rename-window", "-t", "main", "renamed")
        self.tmux("rename-session", "-t", "main", "elsewhere")

        result = self.ok("focus", run_id, "--client", client)

        self.assertIn("elsewhere", result.line)

    def test_a_removed_pane_is_reported_without_changing_state(self) -> None:
        client = self.attach()
        self.tmux("split-window", "-t", "main")
        doomed = self.panes()[1]
        run_id = self.start_in_pane(doomed, "cargo test")
        self.finish_in_pane(doomed, run_id, "0")
        self.tmux("kill-pane", "-t", doomed)

        result = self.fails("focus", run_id, "--client", client)

        self.assertIn("no longer available", result.err)
        self.assertEqual(self.ok("get", run_id).rows[0][ACK], "no")

    def test_a_restarted_server_is_distinguished_from_a_reused_pane(self) -> None:
        pane = self.panes()[0]
        run_id = self.start_in_pane(pane, "cargo test")
        self.finish_in_pane(pane, run_id, "0")
        old_pid = self.server_pid()

        self.kill_server()
        self.new_server()
        client = self.attach()
        # The socket path and even the pane id can come back; the server
        # instance is what tells the difference.
        self.assertNotEqual(self.server_pid(), old_pid)

        result = self.fails("focus", run_id, "--client", client)

        self.assertIn("restarted", result.err)
        self.assertEqual(self.ok("get", run_id).rows[0][ACK], "no")


if __name__ == "__main__":
    unittest.main(verbosity=2)

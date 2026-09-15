#!/usr/bin/env python3
"""PTY tests for the Worklight panel."""

from __future__ import annotations

import sqlite3
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from harness import WorklightTestCase  # noqa: E402
from terminal import LEAVE_ALTERNATE_SCREEN, PtySession  # noqa: E402


class PanelTestCase(WorklightTestCase):
    def panel(self, *args: str, env: dict[str, str] | None = None) -> PtySession:
        session = PtySession(
            [str(self.binary), "--database", str(self.db), *args],
            {**self.env, **(env or {})},
        )
        self.addCleanup(session.close)
        return session

    def quit(self, session: PtySession, key: str = "q") -> None:
        session.send(key)
        code = session.wait()
        self.assertEqual(code, 0, session.detail())
        # Terminal settings are restored: the alternate screen is left behind.
        self.assertIn(LEAVE_ALTERNATE_SCREEN, session.output, session.detail())


class LaunchTests(PanelTestCase):
    def test_no_command_opens_the_panel(self) -> None:
        self.start("cargo build")

        session = self.panel()

        self.assertTrue(session.wait_for("cargo build"), session.detail())
        self.quit(session)

    def test_the_panel_command_opens_the_panel(self) -> None:
        self.start("cargo build")

        session = self.panel("panel")

        self.assertTrue(session.wait_for("cargo build"), session.detail())
        self.quit(session)

    def test_the_panel_shows_every_column(self) -> None:
        run_id = self.start("cargo test")
        self.ok("process", "finish", run_id, "130")

        session = self.panel()

        for expected in ("agents", "processes", "command", "state", "exit", "elapsed", "cwd"):
            self.assertTrue(session.wait_for(expected), session.detail())
        for removed in ("type", "where"):
            self.assertNotIn(removed, session.screen())
        self.assertTrue(session.wait_for("failed/130"), session.detail())
        self.quit(session)

    def test_opening_the_panel_acknowledges_nothing(self) -> None:
        run_id = self.start("cargo test")
        self.ok("process", "finish", run_id, "0")

        session = self.panel()
        self.assertTrue(session.wait_for("cargo test"), session.detail())
        self.quit(session)

        self.assertEqual(self.ok("process", "get", run_id).rows[0][4], "no")

    def test_escape_also_quits(self) -> None:
        self.start("cargo build")
        session = self.panel()
        self.assertTrue(session.wait_for("cargo build"), session.detail())

        self.quit(session, "\x1b")


class RefreshTests(PanelTestCase):
    def test_the_panel_refreshes_without_input(self) -> None:
        session = self.panel()
        self.assertTrue(session.wait_for("worklight"), session.detail())

        self.start("cargo appeared later")

        self.assertTrue(session.wait_for("cargo appeared later", timeout=10), session.detail())
        self.quit(session)


class SelectionTests(PanelTestCase):
    def test_selection_moves_and_a_has_no_effect(self) -> None:
        first = self.start("cargo first")
        self.ok("process", "finish", first, "0")
        second = self.start("cargo second")
        self.ok("process", "finish", second, "1")
        session = self.panel()
        self.assertTrue(session.wait_for("first"), session.detail())
        self.assertNotIn("a acknowledge", session.screen())

        # The newest run is selected first; move down to the older one.
        session.send("j")
        session.read(0.5)
        session.send("a")
        session.read(1.0)
        self.quit(session)

        self.assertEqual(self.ok("process", "get", first).rows[0][4], "no")
        self.assertEqual(self.ok("process", "get", second).rows[0][4], "no")

    def test_arrow_keys_select_without_acknowledging(self) -> None:
        first = self.start("cargo first")
        self.ok("process", "finish", first, "0")
        second = self.start("cargo second")
        self.ok("process", "finish", second, "1")
        session = self.panel()
        self.assertTrue(session.wait_for("first"), session.detail())

        session.send("\x1b[B")  # down
        session.read(0.5)
        session.send("\x1b[A")  # up
        session.read(0.5)
        session.send("a")
        session.read(1.0)
        self.quit(session)

        self.assertEqual(self.ok("process", "get", first).rows[0][4], "no")
        self.assertEqual(self.ok("process", "get", second).rows[0][4], "no")


    def test_history_browsing_acknowledges_nothing(self) -> None:
        acknowledged = self.start("cargo history-old")
        self.ok("process", "finish", acknowledged, "0")
        self.ok("process", "acknowledge", acknowledged)
        current = self.start("cargo history-current")
        self.ok("process", "finish", current, "1")
        session = self.panel()
        self.assertTrue(session.wait_for("history-current"), session.detail())

        session.send("h")
        self.assertTrue(session.wait_for("history-old"), session.detail())
        session.send("h")
        session.read(0.5)
        self.quit(session)

        self.assertEqual(self.ok("process", "get", acknowledged).rows[0][4], "yes")
        self.assertEqual(self.ok("process", "get", current).rows[0][4], "no")


class ErrorTests(PanelTestCase):
    def test_dry_run_enter_previews_without_navigating(self) -> None:
        run_id = self.start("cargo test")
        self.ok("process", "finish", run_id, "0")
        session = self.panel("--dry-run")
        self.assertTrue(session.wait_for("cargo test"), session.detail())

        session.send("\r")

        self.assertTrue(session.wait_for("dry run: would navigate"), session.detail())
        self.quit(session)
        self.assertEqual(self.ok("process", "get", run_id).rows[0][4], "no")

    def test_navigation_failures_are_shown_and_acknowledge_nothing(self) -> None:
        run_id = self.start("cargo test")
        self.ok("process", "finish", run_id, "0")
        session = self.panel()
        self.assertTrue(session.wait_for("cargo test"), session.detail())

        session.send("\r")

        self.assertTrue(session.wait_for("unavailable"), session.detail())
        self.quit(session)
        self.assertEqual(self.ok("process", "get", run_id).rows[0][4], "no")

    def test_failed_navigation_can_delete_the_target_from_the_database(self) -> None:
        run_id = self.start("cargo stale target")
        session = self.panel()
        self.assertTrue(session.wait_for("cargo stale target"), session.detail())

        session.send("\r")
        self.assertTrue(session.wait_for("Delete", timeout=5), session.detail())
        session.send("y")
        session.read(1.0)

        with sqlite3.connect(self.db) as conn:
            self.assertEqual(
                conn.execute("SELECT count(*) FROM processes WHERE id=?", (run_id,)).fetchone()[0],
                0,
            )
            self.assertEqual(conn.execute("SELECT count(*) FROM shells").fetchone()[0], 0)
        self.quit(session)

    def test_a_has_no_effect_on_a_running_run(self) -> None:
        run_id = self.start("cargo run -- sleep 100")
        session = self.panel()
        self.assertTrue(session.wait_for("cargo run -- sleep 100"), session.detail())

        session.send("a")
        session.read(1.0)

        self.assertNotIn("still running", session.screen())
        self.quit(session)
        self.assertEqual(self.ok("process", "get", run_id).rows[0][4], "no")

    def test_a_storage_error_is_shown_without_leaving_the_terminal_broken(self) -> None:
        self.start("cargo test")
        session = self.panel()
        self.assertTrue(session.wait_for("cargo test"), session.detail())

        with sqlite3.connect(self.db) as conn:
            conn.execute("PRAGMA user_version = 99")

        self.assertTrue(session.wait_for("error:", timeout=10), session.detail())
        self.quit(session)


class IsolationTests(PanelTestCase):
    def test_the_panel_runs_no_worklight_subprocess(self) -> None:
        # A decoy earlier on PATH records any subprocess call.
        marker = self.root / "subprocess-was-called"
        decoy_dir = self.root / "decoy"
        decoy_dir.mkdir()
        decoy = decoy_dir / "worklight"
        decoy.write_text(f'#!/bin/sh\ntouch "{marker}"\n')
        decoy.chmod(0o755)

        run_id = self.start("cargo test")
        self.ok("process", "finish", run_id, "0")
        session = self.panel(env={"PATH": f"{decoy_dir}:{self.env['PATH']}"})
        self.assertTrue(session.wait_for("cargo test"), session.detail())
        session.send("a")
        session.read(1.0)
        session.send("\r")
        session.read(1.0)
        self.quit(session)

        self.assertFalse(marker.exists(), "the panel shelled out to worklight")
        self.assertEqual(self.ok("process", "get", run_id).rows[0][4], "no")


if __name__ == "__main__":
    unittest.main(verbosity=2)

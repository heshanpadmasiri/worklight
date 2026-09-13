#!/usr/bin/env python3
"""End-to-end tests for the Worklight command line, against real SQLite."""

from __future__ import annotations

import concurrent.futures
import sqlite3
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from harness import WorklightTestCase  # noqa: E402

ID, STATE, EXIT, ELAPSED, ACK, KIND, CWD, LABEL = range(8)


def stable(rows: list[list[str]]) -> list[list[str]]:
    """Rows without the elapsed column, which keeps moving for running runs."""
    return [row[:ELAPSED] + row[ELAPSED + 1 :] for row in rows]


class StartTests(WorklightTestCase):
    def test_each_start_returns_a_new_id_for_the_same_label(self) -> None:
        first = self.start("cargo test")
        second = self.start("cargo test")

        self.assertNotEqual(first, second)
        self.assertEqual(len(self.ok("list").rows), 2)

    def test_a_started_run_is_running_with_no_exit_code(self) -> None:
        run_id = self.start("sleep 100")

        row = self.ok("get", run_id).rows[0]

        self.assertEqual(row[STATE], "running")
        self.assertEqual(row[EXIT], "-")
        self.assertEqual(row[ACK], "no")
        self.assertEqual(row[LABEL], "sleep 100")

    def test_the_command_label_is_never_executed(self) -> None:
        marker = self.root / "should-not-exist"

        self.start(f"touch {marker}")

        self.assertFalse(marker.exists())

    def test_records_persist_across_invocations(self) -> None:
        run_id = self.start("cargo build")

        self.assertEqual(self.ok("get", run_id).rows[0][LABEL], "cargo build")

    def test_control_characters_are_escaped_in_tabular_output(self) -> None:
        run_id = self.start("line one\tcolumn\\name\nline two\rline three")

        result = self.ok("get", run_id)

        self.assertEqual(len(result.rows), 1)
        self.assertEqual(len(result.rows[0]), 8)
        self.assertEqual(
            result.rows[0][LABEL],
            r"line one\tcolumn\\name\nline two\rline three",
        )


class FinishTests(WorklightTestCase):
    def test_state_is_derived_from_the_exit_code(self) -> None:
        for code, state in (("0", "succeeded"), ("1", "failed/1"), ("130", "failed/130")):
            with self.subTest(code=code):
                run_id = self.start(f"exit {code}")
                row = self.ok("finish", run_id, code).rows[0]
                self.assertEqual(row[STATE], state)
                self.assertEqual(row[EXIT], code)

    def test_finishing_an_unknown_id_fails(self) -> None:
        result = self.fails("finish", "not-an-id", "0")

        self.assertIn("no process with id not-an-id", result.err)

    def test_invalid_input_fails(self) -> None:
        run_id = self.start("cargo test")

        self.assertIn("256", self.fails("finish", run_id, "256").err)
        self.fails("finish", run_id)
        self.fails("finish")
        self.assertEqual(self.ok("get", run_id).rows[0][STATE], "running")

    def test_an_identical_repeated_completion_preserves_the_record(self) -> None:
        run_id = self.start("cargo test")
        first = self.ok("finish", run_id, "3").rows[0]

        repeated = self.ok("finish", run_id, "3").rows[0]

        self.assertEqual(first[:ELAPSED], repeated[:ELAPSED])
        self.assertEqual(first[ACK:], repeated[ACK:])

    def test_a_conflicting_completion_does_not_overwrite_the_record(self) -> None:
        run_id = self.start("cargo test")
        self.ok("finish", run_id, "3")

        result = self.fails("finish", run_id, "0")

        self.assertIn("already finished", result.err)
        self.assertEqual(self.ok("get", run_id).rows[0][EXIT], "3")


class AcknowledgeTests(WorklightTestCase):
    def test_acknowledgment_is_a_boolean_that_preserves_completion(self) -> None:
        run_id = self.start("cargo test")
        finished = self.ok("finish", run_id, "1").rows[0]

        acknowledged = self.ok("acknowledge", run_id).rows[0]
        repeated = self.ok("acknowledge", run_id).rows[0]

        self.assertEqual(acknowledged[ACK], "yes")
        self.assertEqual(repeated[ACK], "yes")
        self.assertEqual(acknowledged[EXIT], finished[EXIT])
        self.assertEqual(acknowledged[CWD], finished[CWD])

    def test_a_running_run_cannot_be_acknowledged(self) -> None:
        run_id = self.start("sleep 100")

        self.fails("acknowledge", run_id)

        self.assertEqual(self.ok("get", run_id).rows[0][ACK], "no")


class ListTests(WorklightTestCase):
    def test_list_all_and_list_active(self) -> None:
        running = self.start("sleep 100")
        finished = self.start("cargo test")
        self.ok("finish", finished, "0")

        listed = [row[ID] for row in self.ok("list").rows]
        active = [row[ID] for row in self.ok("list", "--active").rows]

        self.assertEqual(sorted(listed), sorted([running, finished]))
        self.assertEqual(active, [running])

    def test_lists_carry_the_saved_navigation_information(self) -> None:
        tmux_env = {"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%9"}
        self.start("cargo test", env=tmux_env)

        row = self.ok("list").rows[0]

        self.assertEqual(row[KIND], "tmux")
        self.assertTrue(row[CWD])

    def test_lists_are_in_a_deterministic_order(self) -> None:
        for index in range(5):
            self.start(f"job {index}")

        first = [row[ID] for row in self.ok("list").rows]
        second = [row[ID] for row in self.ok("list").rows]

        self.assertEqual(first, second)


class OrchestratorTests(WorklightTestCase):
    def test_tmux_is_recorded_when_the_environment_says_so(self) -> None:
        run_id = self.start(
            "cargo test", env={"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%3"}
        )

        row = self.ok("get", run_id).rows[0]

        self.assertEqual(row[KIND], "tmux")
        with sqlite3.connect(self.db) as conn:
            pane, socket, instance = conn.execute(
                "SELECT pane_id, socket, server_instance FROM tmux"
            ).fetchone()
        self.assertEqual((pane, socket, instance), ("%3", "/tmp/worklight-socket", "4242"))

    def test_outside_tmux_a_shell_record_is_stored(self) -> None:
        run_id = self.start("cargo test")

        row = self.ok("get", run_id).rows[0]

        self.assertEqual(row[KIND], "shell")
        with sqlite3.connect(self.db) as conn:
            shells = conn.execute("SELECT count(*) FROM shells").fetchone()[0]
            panes = conn.execute("SELECT count(*) FROM tmux").fetchone()[0]
        # A shell record, not absent tmux data.
        self.assertEqual((shells, panes), (1, 0))

    def test_finishing_elsewhere_moves_the_saved_destination(self) -> None:
        run_id = self.start(
            "cargo test", env={"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%3"}
        )

        self.ok(
            "finish",
            run_id,
            "0",
            env={"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%8"},
        )

        with sqlite3.connect(self.db) as conn:
            panes = [row[0] for row in conn.execute("SELECT pane_id FROM tmux")]
        self.assertEqual(panes, ["%8"])

    def test_focusing_a_shell_record_reports_that_it_is_unavailable(self) -> None:
        run_id = self.start("cargo test")
        self.ok("finish", run_id, "0")

        result = self.fails("focus", run_id)

        self.assertIn("unavailable", result.err)
        # A missing destination does not change process state.
        self.assertEqual(self.ok("get", run_id).rows[0][ACK], "no")


class DryRunTests(WorklightTestCase):
    def test_a_dry_run_start_prints_zero_and_saves_nothing(self) -> None:
        result = self.ok("--dry-run", "start", "cargo build")

        self.assertEqual(result.line, "0")
        self.assertFalse(self.db.exists())
        self.assertEqual(self.ok("list").rows, [])

    def test_a_dry_run_does_not_write_to_an_existing_database(self) -> None:
        run_id = self.start("cargo test")
        before = stable(self.ok("list").rows)

        self.ok("--dry-run", "start", "another")
        self.ok("--dry-run", "finish", run_id, "0")
        self.ok("--dry-run", "list")

        self.assertEqual(stable(self.ok("list").rows), before)
        self.assertEqual(self.ok("get", run_id).rows[0][STATE], "running")

    def test_a_dry_run_focus_reports_the_destination_without_navigating(self) -> None:
        run_id = self.start(
            "cargo test", env={"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%3"}
        )
        self.ok("finish", run_id, "0", env={"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%3"})

        # The socket does not exist, so a real focus would fail; a preview
        # never reaches tmux at all.
        result = self.ok("--dry-run", "focus", run_id)

        self.assertIn("%3", result.line)
        self.assertEqual(self.ok("get", run_id).rows[0][ACK], "no")

    def test_a_dry_run_still_reports_conflicts(self) -> None:
        run_id = self.start("cargo test")
        self.ok("finish", run_id, "1")

        result = self.fails("--dry-run", "finish", run_id, "0")

        self.assertIn("already finished", result.err)


class DatabaseTests(WorklightTestCase):
    def test_reading_a_missing_database_creates_nothing(self) -> None:
        result = self.ok("list")

        self.assertEqual(result.rows, [])
        self.assertFalse(self.db.exists())
        self.assertFalse(self.db.parent.exists())

    def test_an_unknown_id_is_an_error_not_an_empty_success(self) -> None:
        self.start("cargo test")

        self.assertIn("no process with id", self.fails("get", "missing").err)

    def test_an_incompatible_database_is_not_replaced(self) -> None:
        self.start("cargo test")
        with sqlite3.connect(self.db) as conn:
            conn.execute("PRAGMA user_version = 99")

        result = self.fails("list")

        self.assertIn("incompatible", result.err)
        with sqlite3.connect(self.db) as conn:
            self.assertEqual(
                conn.execute("SELECT count(*) FROM processes").fetchone()[0], 1
            )

    def test_a_corrupt_database_is_not_replaced(self) -> None:
        self.db.parent.mkdir(parents=True)
        self.db.write_bytes(b"definitely not a database")

        self.fails("list")

        self.assertEqual(self.db.read_bytes(), b"definitely not a database")


class ConcurrencyTests(WorklightTestCase):
    def test_concurrent_first_starts_initialize_a_new_database(self) -> None:
        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            results = list(
                pool.map(
                    lambda index: self.run_worklight("start", f"job {index}"),
                    range(16),
                )
            )

        for result in results:
            self.assertEqual(result.code, 0, str(result))
        self.assertEqual(len(self.ok("list").rows), 16)

    def test_concurrent_callers_finish_their_own_runs(self) -> None:
        ids = [self.start(f"job {index}") for index in range(8)]

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            results = list(
                pool.map(
                    lambda pair: self.run_worklight("finish", pair[1], str(pair[0])),
                    enumerate(ids),
                )
            )

        for result in results:
            self.assertEqual(result.code, 0, str(result))
        for index, run_id in enumerate(ids):
            self.assertEqual(self.ok("get", run_id).rows[0][EXIT], str(index))
        self.assertEqual(self.ok("list", "--active").rows, [])

    def test_only_one_of_two_conflicting_completions_wins(self) -> None:
        run_id = self.start("cargo test")

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(
                pool.map(
                    lambda code: self.run_worklight("finish", run_id, code), ["0", "1"]
                )
            )

        codes = sorted(result.code for result in results)
        self.assertEqual(codes, [0, 1], "\n\n".join(str(r) for r in results))
        self.assertIn(self.ok("get", run_id).rows[0][EXIT], {"0", "1"})


if __name__ == "__main__":
    unittest.main(verbosity=2)

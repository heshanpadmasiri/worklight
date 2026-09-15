#!/usr/bin/env python3
"""End-to-end tests for the Worklight command line, against real SQLite."""

from __future__ import annotations

import concurrent.futures
import os
import sqlite3
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from harness import WorklightTestCase  # noqa: E402

ID, STATE, EXIT, ELAPSED, ACK, KIND, CWD, LABEL = range(8)
AGENT_ID, AGENT_STATUS, AGENT_ACK, AGENT_ELAPSED, AGENT_ORCHESTRATOR, AGENT_CWD, AGENT_KIND = range(7)


def stable(rows: list[list[str]]) -> list[list[str]]:
    """Rows without the elapsed column, which keeps moving for running runs."""
    return [row[:ELAPSED] + row[ELAPSED + 1 :] for row in rows]


def conn_count(database: Path, table: str) -> int:
    with sqlite3.connect(database) as conn:
        return conn.execute(f"SELECT count(*) FROM {table}").fetchone()[0]


class StartTests(WorklightTestCase):
    def test_each_start_returns_a_new_id_for_the_same_label(self) -> None:
        first = self.start("cargo test")
        second = self.start("cargo test")

        self.assertNotEqual(first, second)
        self.assertEqual(len(self.ok("process", "list").rows), 2)

    def test_a_started_run_is_running_with_no_exit_code(self) -> None:
        run_id = self.start("cargo run -- sleep 100")

        row = self.ok("process", "get", run_id).rows[0]

        self.assertEqual(row[STATE], "running")
        self.assertEqual(row[EXIT], "-")
        self.assertEqual(row[ACK], "no")
        self.assertEqual(row[LABEL], "cargo run -- sleep 100")

    def test_the_command_label_is_never_executed(self) -> None:
        marker = self.root / "should-not-exist"

        self.start(f"cargo test; touch {marker}")

        self.assertFalse(marker.exists())

    def test_records_persist_across_invocations(self) -> None:
        run_id = self.start("cargo build")

        self.assertEqual(self.ok("process", "get", run_id).rows[0][LABEL], "cargo build")

    def test_control_characters_are_escaped_in_tabular_output(self) -> None:
        run_id = self.start("cargo line one\tcolumn\\name\nline two\rline three")

        result = self.ok("process", "get", run_id)

        self.assertEqual(len(result.rows), 1)
        self.assertEqual(len(result.rows[0]), 8)
        self.assertEqual(
            result.rows[0][LABEL],
            r"cargo line one\tcolumn\\name\nline two\rline three",
        )

    def test_eligible_start_preserves_the_complete_label_in_storage(self) -> None:
        label = "  cargo test | tee 'a & b'; printf done  "
        run_id = self.start(label)

        with sqlite3.connect(self.db) as conn:
            stored = conn.execute(
                "SELECT label FROM processes WHERE id = ?", (run_id,)
            ).fetchone()[0]
        self.assertEqual(stored, label)
        self.assertEqual(
            conn_count(self.db, "processes"),
            1,
        )

    def test_ineligible_start_returns_zero_without_creating_storage(self) -> None:
        result = self.ok("process", "start", "python test.py")

        self.assertEqual(result.out, "0\n")
        self.assertEqual(result.err, "")
        self.assertFalse(self.db.exists())
        self.assertFalse(self.db.parent.exists())

    def test_rejection_happens_before_storage_and_environment_validation(self) -> None:
        blocked = self.root / "not-a-directory"
        blocked.write_text("blocked")
        impossible_db = blocked / "worklight.db"
        command = [
            str(self.binary),
            "--database",
            str(impossible_db),
            "process",
            "start",
            "python test.py",
        ]
        result = subprocess.run(
            command,
            capture_output=True,
            text=True,
            env={**self.env, "TMUX": "malformed", "TMUX_PANE": ""},
        )
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "0\n", ""))

        deleted_cwd = self.root / "deleted-cwd"
        deleted_cwd.mkdir()
        result = subprocess.run(
            command,
            cwd=deleted_cwd,
            preexec_fn=lambda: os.rmdir(deleted_cwd),
            capture_output=True,
            text=True,
            env=self.env,
        )
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "0\n", ""))


class FinishTests(WorklightTestCase):
    def test_state_is_derived_from_the_exit_code(self) -> None:
        for code, state in (("0", "succeeded"), ("1", "failed/1"), ("130", "failed/130")):
            with self.subTest(code=code):
                run_id = self.start(f"cargo run -- exit {code}")
                row = self.ok("process", "finish", run_id, code).rows[0]
                self.assertEqual(row[STATE], state)
                self.assertEqual(row[EXIT], code)

    def test_finishing_an_unknown_id_fails(self) -> None:
        result = self.fails("process", "finish", "9999", "0")

        self.assertIn("no process with id 9999", result.err)

    def test_invalid_input_fails(self) -> None:
        run_id = self.start("cargo test")

        self.assertIn("256", self.fails("process", "finish", run_id, "256").err)
        self.fails("process", "finish", run_id)
        self.fails("process", "finish")
        self.assertEqual(self.ok("process", "get", run_id).rows[0][STATE], "running")

    def test_an_identical_repeated_completion_is_rejected_without_changes(self) -> None:
        run_id = self.start("cargo test")
        first = self.ok("process", "finish", run_id, "3").rows[0]

        repeated = self.fails("process", "finish", run_id, "3")

        self.assertIn("already finished", repeated.err)
        current = self.ok("process", "get", run_id).rows[0]
        self.assertEqual(first[:ELAPSED], current[:ELAPSED])
        self.assertEqual(first[ACK:], current[ACK:])

    def test_a_conflicting_completion_does_not_overwrite_the_record(self) -> None:
        run_id = self.start("cargo test")
        self.ok("process", "finish", run_id, "3")

        result = self.fails("process", "finish", run_id, "0")

        self.assertIn("already finished", result.err)
        self.assertEqual(self.ok("process", "get", run_id).rows[0][EXIT], "3")


class AcknowledgeTests(WorklightTestCase):
    def test_acknowledgment_is_a_boolean_that_preserves_completion(self) -> None:
        run_id = self.start("cargo test")
        finished = self.ok("process", "finish", run_id, "1").rows[0]

        acknowledged = self.ok("process", "acknowledge", run_id).rows[0]
        repeated = self.ok("process", "acknowledge", run_id).rows[0]

        self.assertEqual(acknowledged[ACK], "yes")
        self.assertEqual(repeated[ACK], "yes")
        self.assertEqual(acknowledged[EXIT], finished[EXIT])
        self.assertEqual(acknowledged[CWD], finished[CWD])

    def test_a_running_run_cannot_be_acknowledged(self) -> None:
        run_id = self.start("cargo run -- sleep 100")

        self.fails("process", "acknowledge", run_id)

        self.assertEqual(self.ok("process", "get", run_id).rows[0][ACK], "no")


class ListTests(WorklightTestCase):
    def test_list_all_and_list_active(self) -> None:
        running = self.start("cargo run -- sleep 100")
        finished = self.start("cargo test")
        self.ok("process", "finish", finished, "0")

        listed = [row[ID] for row in self.ok("process", "list").rows]
        active = [row[ID] for row in self.ok("process", "list", "--active").rows]

        self.assertEqual(sorted(listed), sorted([running, finished]))
        self.assertEqual(active, [running])

    def test_lists_carry_the_saved_navigation_information(self) -> None:
        tmux_env = {"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%9"}
        self.start("cargo test", env=tmux_env)

        row = self.ok("process", "list").rows[0]

        self.assertEqual(row[KIND], "tmux")
        self.assertTrue(row[CWD])

    def test_lists_are_in_a_deterministic_order(self) -> None:
        for index in range(5):
            self.start(f"cargo job {index}")

        first = [row[ID] for row in self.ok("process", "list").rows]
        second = [row[ID] for row in self.ok("process", "list").rows]

        self.assertEqual(first, second)


class OrchestratorTests(WorklightTestCase):
    def test_tmux_is_recorded_when_the_environment_says_so(self) -> None:
        run_id = self.start(
            "cargo test", env={"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%3"}
        )

        row = self.ok("process", "get", run_id).rows[0]

        self.assertEqual(row[KIND], "tmux")
        with sqlite3.connect(self.db) as conn:
            pane, cwd = conn.execute("SELECT pane, cwd FROM tmux").fetchone()
        self.assertEqual(pane, "%3")
        self.assertTrue(cwd)

    def test_outside_tmux_a_shell_record_is_stored(self) -> None:
        run_id = self.start("cargo test")

        row = self.ok("process", "get", run_id).rows[0]

        self.assertEqual(row[KIND], "shell")
        with sqlite3.connect(self.db) as conn:
            shells = conn.execute("SELECT count(*) FROM shells").fetchone()[0]
            panes = conn.execute("SELECT count(*) FROM tmux").fetchone()[0]
        # A shell record, not absent tmux data.
        self.assertEqual((shells, panes), (1, 0))

    def test_finishing_elsewhere_preserves_the_saved_destination(self) -> None:
        run_id = self.start(
            "cargo test", env={"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%3"}
        )

        self.ok(
            "process",
            "finish",
            run_id,
            "0",
            env={"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%8"},
        )

        with sqlite3.connect(self.db) as conn:
            panes = [row[0] for row in conn.execute("SELECT pane FROM tmux")]
        self.assertEqual(panes, ["%3"])

    def test_focusing_a_shell_record_reports_that_it_is_unavailable(self) -> None:
        run_id = self.start("cargo test")
        self.ok("process", "finish", run_id, "0")

        result = self.fails("process", "focus", run_id)

        self.assertIn("unavailable", result.err)
        # A missing destination does not change process state.
        self.assertEqual(self.ok("process", "get", run_id).rows[0][ACK], "no")


class DryRunTests(WorklightTestCase):
    def test_a_dry_run_start_prints_zero_and_saves_nothing(self) -> None:
        result = self.ok("--dry-run", "process", "start", "cargo build")

        self.assertEqual(result.out, "0\n")
        self.assertEqual(result.err, "")
        self.assertFalse(self.db.exists())
        self.assertEqual(self.ok("process", "list").rows, [])

    def test_dry_run_start_skips_all_validation_for_any_label(self) -> None:
        blocked = self.root / "not-a-directory"
        blocked.write_text("blocked")
        for label in ("cargo test", "python test.py"):
            completed = subprocess.run(
                [
                    str(self.binary),
                    "--database",
                    str(blocked / "worklight.db"),
                    "--dry-run",
                    "process",
                    "start",
                    label,
                ],
                capture_output=True,
                text=True,
                env={**self.env, "TMUX": "malformed", "TMUX_PANE": ""},
            )
            self.assertEqual(
                (completed.returncode, completed.stdout, completed.stderr),
                (0, "0\n", ""),
            )

    def test_a_dry_run_does_not_write_to_an_existing_database(self) -> None:
        run_id = self.start("cargo test")
        before = stable(self.ok("process", "list").rows)

        self.ok("--dry-run", "process", "start", "another")
        self.ok("--dry-run", "process", "finish", run_id, "0")
        self.ok("--dry-run", "process", "list")

        self.assertEqual(stable(self.ok("process", "list").rows), before)
        self.assertEqual(self.ok("process", "get", run_id).rows[0][STATE], "running")

    def test_a_dry_run_focus_reports_the_destination_without_navigating(self) -> None:
        run_id = self.start(
            "cargo test", env={"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%3"}
        )
        self.ok("process", "finish", run_id, "0", env={"TMUX": "/tmp/worklight-socket,4242,0", "TMUX_PANE": "%3"})

        # The socket does not exist, so a real focus would fail; a preview
        # never reaches tmux at all.
        result = self.ok("--dry-run", "process", "focus", run_id)

        self.assertIn("%3", result.line)
        self.assertEqual(self.ok("process", "get", run_id).rows[0][ACK], "no")

    def test_a_dry_run_still_reports_conflicts(self) -> None:
        run_id = self.start("cargo test")
        self.ok("process", "finish", run_id, "1")

        result = self.fails("--dry-run", "process", "finish", run_id, "0")

        self.assertIn("already finished", result.err)


class AgentCliTests(WorklightTestCase):
    def register(self, kind: str = "pi", env: dict[str, str] | None = None) -> str:
        result = self.ok("agent", "register", kind, env=env)
        self.assertRegex(result.out, r"^[1-9][0-9]*\n$")
        return result.line

    def status(self, agent_id: str, status: str) -> list[str]:
        return self.ok("agent", "status", agent_id, status).rows[0]

    def test_help_and_all_subcommands_parse(self) -> None:
        self.assertIn("register", self.ok("agent", "--help").out)
        for command in ("register", "status", "get", "list", "acknowledge", "focus"):
            result = self.run_worklight("agent", command, "--help")
            self.assertEqual(result.code, 0, str(result))

    def test_registration_get_and_escaped_tsv(self) -> None:
        agent_id = self.register("pi\\kind\tline\nnext")
        row = self.ok("agent", "get", agent_id).rows[0]

        self.assertEqual(len(row), 7)
        self.assertEqual(row[AGENT_ID], agent_id)
        self.assertEqual(row[AGENT_STATUS], "idle")
        self.assertEqual(row[AGENT_ACK], "no")
        self.assertEqual(row[AGENT_ORCHESTRATOR], "shell")
        self.assertEqual(row[AGENT_KIND], r"pi\\kind\tline\nnext")

    def test_status_acknowledgment_and_active_listing(self) -> None:
        active = self.register("active")
        done = self.register("done")
        killed = self.register("killed")

        self.status(done, "working")
        self.status(done, "done")
        acknowledged = self.ok("agent", "acknowledge", done).rows[0]
        self.assertEqual(acknowledged[AGENT_ACK], "yes")
        self.status(killed, "killed")

        listed = [row[AGENT_ID] for row in self.ok("agent", "list").rows]
        actionable = [row[AGENT_ID] for row in self.ok("agent", "list", "--active").rows]
        self.assertEqual(set(listed), {active, done, killed})
        self.assertEqual(actionable, [active])

        working = self.status(done, "working")
        self.assertEqual((working[AGENT_STATUS], working[AGENT_ACK]), ("working", "no"))

    def test_invalid_ids_statuses_and_transitions_do_not_write(self) -> None:
        agent_id = self.register()
        for invalid in ("0", "-1"):
            self.assertIn("must be positive", self.fails("agent", "get", invalid).err)
        self.assertIn("unknown agent status", self.fails("agent", "status", agent_id, "IDLE").err)
        self.assertIn("cannot transition", self.fails("agent", "status", agent_id, "done").err)
        self.assertEqual(self.ok("agent", "get", agent_id).rows[0][AGENT_STATUS], "idle")
        self.assertIn("not done", self.fails("agent", "acknowledge", agent_id).err)

    def test_dry_run_registration_creates_nothing(self) -> None:
        result = self.ok("--dry-run", "agent", "register", "pi")
        self.assertEqual(result.out, "0\n")
        self.assertFalse(self.db.exists())
        self.assertFalse(self.db.parent.exists())

    def test_dry_run_mutations_validate_and_write_nothing(self) -> None:
        agent_id = self.register()
        before = self.ok("agent", "get", agent_id).rows[0]

        preview = self.ok("--dry-run", "agent", "status", agent_id, "working").rows[0]
        self.assertEqual(preview[AGENT_STATUS], "idle")
        self.assertEqual(self.ok("agent", "get", agent_id).rows[0][AGENT_STATUS], "idle")
        self.assertIn(
            "cannot transition",
            self.fails("--dry-run", "agent", "status", agent_id, "done").err,
        )
        self.assertIn("not done", self.fails("--dry-run", "agent", "acknowledge", agent_id).err)
        focused = self.ok("--dry-run", "agent", "focus", agent_id)
        self.assertIn("shell", focused.rows[0][0])
        after = self.ok("agent", "get", agent_id).rows[0]
        self.assertEqual(before[:AGENT_ELAPSED], after[:AGENT_ELAPSED])
        self.assertEqual(before[AGENT_ELAPSED + 1 :], after[AGENT_ELAPSED + 1 :])

    def test_focus_failure_does_not_acknowledge_done(self) -> None:
        agent_id = self.register()
        self.status(agent_id, "working")
        self.status(agent_id, "done")
        self.assertIn("unavailable", self.fails("agent", "focus", agent_id).err)
        self.assertEqual(self.ok("agent", "get", agent_id).rows[0][AGENT_ACK], "no")

    def test_successful_focus_conditionally_acknowledges_done(self) -> None:
        fake_bin = self.root / "fake-bin"
        fake_bin.mkdir()
        tmux = fake_bin / "tmux"
        tmux.write_text(
            "#!/bin/sh\n"
            "if [ \"$1\" = list-panes ]; then printf '%s\\t%s\\n' '%42' 'work:1.0'; fi\n"
        )
        tmux.chmod(0o755)
        env = {
            "PATH": f"{fake_bin}:{self.env['PATH']}",
            "TMUX": "fake,1,0",
            "TMUX_PANE": "%42",
        }
        agent_id = self.register(env=env)
        self.status(agent_id, "working")
        self.status(agent_id, "done")

        focused = self.ok("agent", "focus", agent_id, env=env)

        self.assertEqual(focused.rows[0][0], "work:1.0")
        self.assertEqual(focused.rows[0][1 + AGENT_STATUS], "done")
        self.assertEqual(focused.rows[0][1 + AGENT_ACK], "yes")
        self.assertEqual(self.ok("agent", "get", agent_id).rows[0][AGENT_ACK], "yes")


class DatabaseTests(WorklightTestCase):
    def test_reading_a_missing_database_creates_nothing(self) -> None:
        result = self.ok("process", "list")

        self.assertEqual(result.rows, [])
        self.assertFalse(self.db.exists())
        self.assertFalse(self.db.parent.exists())

    def test_an_unknown_id_is_an_error_not_an_empty_success(self) -> None:
        self.start("cargo test")

        self.assertIn("no process with id", self.fails("process", "get", "9999").err)

    def test_an_incompatible_database_is_not_replaced(self) -> None:
        self.start("cargo test")
        with sqlite3.connect(self.db) as conn:
            conn.execute("PRAGMA user_version = 99")

        result = self.fails("process", "list")

        self.assertIn("incompatible", result.err)
        with sqlite3.connect(self.db) as conn:
            self.assertEqual(
                conn.execute("SELECT count(*) FROM processes").fetchone()[0], 1
            )

    def test_a_corrupt_database_is_not_replaced(self) -> None:
        self.db.parent.mkdir(parents=True)
        self.db.write_bytes(b"definitely not a database")

        self.fails("process", "list")

        self.assertEqual(self.db.read_bytes(), b"definitely not a database")


class ConcurrencyTests(WorklightTestCase):
    def test_concurrent_first_starts_initialize_a_new_database(self) -> None:
        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            results = list(
                pool.map(
                    lambda index: self.run_worklight("process", "start", f"cargo job {index}"),
                    range(16),
                )
            )

        for result in results:
            self.assertEqual(result.code, 0, str(result))
        self.assertEqual(len(self.ok("process", "list").rows), 16)

    def test_concurrent_callers_finish_their_own_runs(self) -> None:
        ids = [self.start(f"cargo job {index}") for index in range(8)]

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            results = list(
                pool.map(
                    lambda pair: self.run_worklight("process", "finish", pair[1], str(pair[0])),
                    enumerate(ids),
                )
            )

        for result in results:
            self.assertEqual(result.code, 0, str(result))
        for index, run_id in enumerate(ids):
            self.assertEqual(self.ok("process", "get", run_id).rows[0][EXIT], str(index))
        self.assertEqual(self.ok("process", "list", "--active").rows, [])

    def test_only_one_of_two_conflicting_completions_wins(self) -> None:
        run_id = self.start("cargo test")

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(
                pool.map(
                    lambda code: self.run_worklight("process", "finish", run_id, code), ["0", "1"]
                )
            )

        codes = sorted(result.code for result in results)
        self.assertEqual(codes, [0, 1], "\n\n".join(str(r) for r in results))
        self.assertIn(self.ok("process", "get", run_id).rows[0][EXIT], {"0", "1"})


if __name__ == "__main__":
    unittest.main(verbosity=2)

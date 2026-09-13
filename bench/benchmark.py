#!/usr/bin/env python3
"""Benchmark driver for Worklight, against real isolated SQLite fixtures.

Run through mise: `mise run bench [-- --quick]`. Benchmarks are separate from
the fast correctness tests and are never pointed at a real database.

Every measurement is one invocation of the release binary, so the reported
latency is what a shell hook would actually pay, process start included.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import platform
import random
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

CHECKOUT = Path(__file__).resolve().parent.parent
BINARY = CHECKOUT / "target" / "release" / "worklight"

ACTIVE_COUNTS = [0, 100, 1_000]
COMPLETED_COUNTS = [0, 10_000, 100_000, 1_000_000]
QUICK_ACTIVE = [0, 100]
QUICK_COMPLETED = [0, 10_000]


# --- fixtures ------------------------------------------------------------


class Fixture:
    """An isolated database with a known number of active and completed runs.

    Rows are inserted directly, which is reserved for benchmarks and schema
    edge cases; the schema itself still comes from the binary.
    """

    def __init__(self, root: Path, active: int, completed: int) -> None:
        self.path = root / f"bench-{active}-{completed}.db"
        self.active = active
        self.completed = completed

    def build(self, env: dict[str, str]) -> None:
        if self.path.exists():
            self.path.unlink()
        # Let the binary create the schema, so the fixture cannot drift.
        seed = run(env, "--database", str(self.path), "start", "schema seed").strip()
        run(env, "--database", str(self.path), "finish", seed, "0")
        conn = sqlite3.connect(self.path)
        conn.execute("DELETE FROM processes")
        conn.execute("DELETE FROM shells")
        conn.execute("DELETE FROM tmux")
        self._insert(conn, self.completed, finished=True)
        self._insert(conn, self.active, finished=False)
        conn.commit()
        conn.close()

    def _insert(self, conn: sqlite3.Connection, count: int, finished: bool) -> None:
        kind = "done" if finished else "live"
        rows = []
        shells = []
        panes = []
        base = 1_600_000_000_000
        for index in range(count):
            key = f"{kind}{index}"
            # Timestamps vary, and every fourth run is recorded in tmux.
            started = base + index * 997
            if index % 4 == 0:
                panes.append((key, "/work", "/tmp/tmux-bench", str(4000 + index % 7), f"%{index}"))
                shell_id, tmux_id = None, key
            else:
                shells.append((key, f"/work/{index % 32}"))
                shell_id, tmux_id = key, None
            if finished:
                # Mixed success and failure, and only some acknowledged.
                exit_code = 0 if index % 3 else (1 if index % 2 else 130)
                rows.append(
                    (
                        key,
                        f"job {index}",
                        started,
                        started + 5_000,
                        exit_code,
                        1 if index % 5 == 0 else 0,
                        shell_id,
                        tmux_id,
                    )
                )
            else:
                rows.append((key, f"job {index}", started, None, None, 0, shell_id, tmux_id))
        conn.executemany("INSERT INTO shells (id, cwd) VALUES (?, ?)", shells)
        conn.executemany(
            "INSERT INTO tmux (id, cwd, socket, server_instance, pane_id) VALUES (?, ?, ?, ?, ?)",
            panes,
        )
        conn.executemany(
            "INSERT INTO processes (id, label, started_at, finished_at, exit_code,"
            " acknowledged, shell_id, tmux_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rows,
        )

    def validate(self) -> dict[str, int]:
        conn = sqlite3.connect(self.path)
        active = conn.execute(
            "SELECT count(*) FROM processes WHERE exit_code IS NULL"
        ).fetchone()[0]
        completed = conn.execute(
            "SELECT count(*) FROM processes WHERE exit_code IS NOT NULL"
        ).fetchone()[0]
        conn.close()
        if active != self.active or completed != self.completed:
            raise SystemExit(
                f"fixture {self.path} holds {active}/{completed}, expected "
                f"{self.active}/{self.completed}"
            )
        return {
            "active": active,
            "completed": completed,
            "bytes": self.path.stat().st_size,
        }

    def plans(self) -> dict[str, dict[str, str | bool]]:
        """Query plans at this fixture's size.

        The optimizer's choice can change once the tables are large, so the
        plan is inspected at every size rather than assumed from a small one.
        """
        conn = sqlite3.connect(self.path)
        plans: dict[str, dict[str, str | bool]] = {}
        for name, sql in (
            ("list_all", "SELECT id FROM processes ORDER BY started_at DESC, id DESC"),
            (
                "list_active",
                "SELECT id FROM processes WHERE exit_code IS NULL"
                " ORDER BY started_at DESC, id DESC",
            ),
        ):
            plan = " | ".join(row[3] for row in conn.execute(f"EXPLAIN QUERY PLAN {sql}"))
            plans[name] = {"plan": plan, "ok": self._plan_ok(name, plan)}
        conn.close()
        return plans

    def _plan_ok(self, name: str, plan: str) -> bool:
        if name != "list_active":
            # ListAll necessarily reads all history; any plan is acceptable.
            return True
        # The partial index holds only the running records, so the active list
        # must not fall back to reading the completed ones.
        return "processes_active" in plan

    def ids(self, count: int) -> list[str]:
        conn = sqlite3.connect(self.path)
        rows = [
            row[0]
            for row in conn.execute("SELECT id FROM processes LIMIT ?", (count * 4,))
        ]
        conn.close()
        if not rows:
            return []
        random.seed(17)
        return [random.choice(rows) for _ in range(count)]


# --- measurement ---------------------------------------------------------


def run(env: dict[str, str], *args: str) -> str:
    result = subprocess.run(
        [str(BINARY), *args], capture_output=True, text=True, env=env, timeout=300
    )
    if result.returncode != 0:
        raise RuntimeError(f"{' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


class Measurement:
    def __init__(self, name: str) -> None:
        self.name = name
        self.samples: list[float] = []
        self.failures = 0
        self.wall = 0.0

    def record(self, seconds: float) -> None:
        self.samples.append(seconds * 1000)

    def summary(self) -> dict[str, float | int]:
        if not self.samples:
            return {"operation": self.name, "samples": 0, "failures": self.failures}
        ordered = sorted(self.samples)
        return {
            "operation": self.name,
            "samples": len(ordered),
            "failures": self.failures,
            "p50_ms": round(statistics.median(ordered), 3),
            "p95_ms": round(percentile(ordered, 0.95), 3),
            "p99_ms": round(percentile(ordered, 0.99), 3),
            "max_ms": round(ordered[-1], 3),
            "throughput_per_s": round(len(ordered) / self.wall, 1) if self.wall else 0,
        }


def percentile(ordered: list[float], fraction: float) -> float:
    index = min(len(ordered) - 1, int(round(fraction * (len(ordered) - 1))))
    return ordered[index]


def timed(measurement: Measurement, call) -> None:
    start = time.perf_counter()
    try:
        call()
    except RuntimeError:
        measurement.failures += 1
        return
    finally:
        elapsed = time.perf_counter() - start
    measurement.record(elapsed)
    measurement.wall += elapsed


def measure_fixture(fixture: Fixture, env: dict[str, str], iterations: int) -> list[dict]:
    db = str(fixture.path)
    results = []

    # Real starts, each issuing a new tracking id.
    starts = Measurement("start")
    started_ids: list[str] = []
    for _ in range(iterations):
        holder: list[str] = []
        timed(starts, lambda: holder.append(run(env, "--database", db, "start", "bench").strip()))
        started_ids.extend(holder)
    results.append(starts.summary())

    # Real completions: each id is finished exactly once, so no repeated
    # no-op is counted as a finish.
    finishes = Measurement("finish")
    for index, run_id in enumerate(started_ids):
        timed(
            finishes,
            lambda run_id=run_id, index=index: run(
                env, "--database", db, "finish", run_id, str(index % 256)
            ),
        )
    results.append(finishes.summary())

    gets = Measurement("get")
    for run_id in fixture.ids(iterations) or started_ids[:iterations]:
        timed(gets, lambda run_id=run_id: run(env, "--database", db, "get", run_id))
    results.append(gets.summary())

    # ListAll necessarily costs proportionally to the history it returns, so
    # it is measured separately and with fewer iterations.
    list_all = Measurement("list_all")
    for _ in range(max(3, iterations // 10)):
        timed(list_all, lambda: run(env, "--database", db, "list"))
    results.append(list_all.summary())

    list_active = Measurement("list_active")
    for _ in range(iterations):
        timed(list_active, lambda: run(env, "--database", db, "list", "--active"))
    results.append(list_active.summary())

    results.append(measure_concurrency(fixture, env, iterations))
    return results


def measure_concurrency(fixture: Fixture, env: dict[str, str], iterations: int) -> dict:
    """Concurrent writers and readers against the same database."""
    db = str(fixture.path)
    writers, readers = 4, 4
    per_worker = max(2, iterations // 10)
    measurement = Measurement("concurrent")

    def writer() -> None:
        for _ in range(per_worker):
            start = time.perf_counter()
            try:
                run_id = run(env, "--database", db, "start", "concurrent").strip()
                run(env, "--database", db, "finish", run_id, "0")
            except RuntimeError:
                measurement.failures += 1
                continue
            measurement.record(time.perf_counter() - start)

    def reader() -> None:
        for _ in range(per_worker):
            start = time.perf_counter()
            try:
                run(env, "--database", db, "list", "--active")
            except RuntimeError:
                measurement.failures += 1
                continue
            measurement.record(time.perf_counter() - start)

    wall = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=writers + readers) as pool:
        futures = [pool.submit(writer) for _ in range(writers)]
        futures += [pool.submit(reader) for _ in range(readers)]
        for future in futures:
            future.result()
    measurement.wall = time.perf_counter() - wall
    return measurement.summary()


# --- reporting -----------------------------------------------------------


def environment_metadata() -> dict:
    def version(*command: str) -> str:
        try:
            return subprocess.run(
                command, capture_output=True, text=True, timeout=30
            ).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return "unknown"

    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpus": os.cpu_count(),
        "python": sys.version.split()[0],
        "sqlite": sqlite3.sqlite_version,
        "rustc": version("rustc", "--version"),
        "binary": str(BINARY),
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }


def print_report(report: dict) -> None:
    print("worklight benchmarks")
    for key, value in report["environment"].items():
        print(f"  {key}: {value}")
    for case in report["cases"]:
        size = case["fixture"]
        print(
            f"\nactive={size['active']} completed={size['completed']} "
            f"({size['bytes'] / 1e6:.1f} MB)"
        )
        for name, plan in case["plans"].items():
            flag = "" if plan["ok"] else "  <-- WRONG INDEX"
            print(f"  plan {name}: {plan['plan']}{flag}")
        header = f"  {'operation':<12}{'n':>7}{'fail':>6}{'p50':>10}{'p95':>10}{'p99':>10}{'max':>10}{'ops/s':>10}"
        print(header)
        for measurement in case["measurements"]:
            if not measurement.get("samples"):
                print(f"  {measurement['operation']:<12}{0:>7}{measurement['failures']:>6}")
                continue
            print(
                f"  {measurement['operation']:<12}{measurement['samples']:>7}"
                f"{measurement['failures']:>6}{measurement['p50_ms']:>10.2f}"
                f"{measurement['p95_ms']:>10.2f}{measurement['p99_ms']:>10.2f}"
                f"{measurement['max_ms']:>10.2f}{measurement['throughput_per_s']:>10.1f}"
            )


def degradation(report: dict) -> None:
    """Compare the same operation across history sizes on this machine."""
    print("\ndegradation across completed history (p50 ms)")
    operations = ["start", "finish", "get", "list_active", "list_all"]
    sizes = sorted({case["fixture"]["completed"] for case in report["cases"]})
    print("  " + "operation".ljust(12) + "".join(f"{size:>12}" for size in sizes))
    for operation in operations:
        cells = []
        for size in sizes:
            values = [
                measurement["p50_ms"]
                for case in report["cases"]
                if case["fixture"]["completed"] == size
                for measurement in case["measurements"]
                if measurement["operation"] == operation and measurement.get("samples")
            ]
            cells.append(f"{statistics.median(values):>12.2f}" if values else f"{'-':>12}")
        print("  " + operation.ljust(12) + "".join(cells))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--quick", action="store_true", help="a small matrix, for smoke runs")
    parser.add_argument("--iterations", type=int, default=30)
    parser.add_argument("--json", type=Path, help="also write the full report here")
    args = parser.parse_args(argv)

    if not BINARY.exists():
        print(f"benchmark: {BINARY} is missing; run `mise run build`", file=sys.stderr)
        return 1

    active_counts = QUICK_ACTIVE if args.quick else ACTIVE_COUNTS
    completed_counts = QUICK_COMPLETED if args.quick else COMPLETED_COUNTS
    iterations = args.iterations

    root = Path(tempfile.mkdtemp(prefix="worklight-bench-"))
    env = {
        "HOME": str(root),
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "XDG_DATA_HOME": str(root / "data"),
        "TERM": "dumb",
    }
    report = {"environment": environment_metadata(), "cases": []}
    try:
        for completed in completed_counts:
            for active in active_counts:
                fixture = Fixture(root, active, completed)
                # Fixture setup, reset and validation stay outside the timed
                # intervals.
                fixture.build(env)
                size = fixture.validate()
                plans = fixture.plans()
                measurements = measure_fixture(fixture, env, iterations)
                report["cases"].append(
                    {"fixture": size, "plans": plans, "measurements": measurements}
                )
                fixture.path.unlink(missing_ok=True)
    finally:
        shutil.rmtree(root, ignore_errors=True)

    print_report(report)
    degradation(report)
    if args.json:
        args.json.write_text(json.dumps(report, indent=2))
        print(f"\nwrote {args.json}")

    regressions = [
        (case["fixture"], name, plan["plan"])
        for case in report["cases"]
        for name, plan in case["plans"].items()
        if not plan["ok"]
    ]
    failures = sum(
        measurement.get("failures", 0)
        for case in report["cases"]
        for measurement in case["measurements"]
    )
    for size, name, plan in regressions:
        print(
            f"\nbenchmark: {name} stopped using its index at active="
            f"{size['active']} completed={size['completed']}: {plan}",
            file=sys.stderr,
        )
    if failures:
        print(f"\nbenchmark: {failures} operations failed", file=sys.stderr)
    return 1 if regressions or failures else 0


if __name__ == "__main__":
    sys.exit(main())

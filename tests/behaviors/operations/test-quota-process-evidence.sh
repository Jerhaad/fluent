#!/bin/bash
# Validate the bounded host observer without inspecting the operator's processes.
set -euo pipefail
repository=$(git rev-parse --show-toplevel)
export LOG_DIR="$repository/tests/output/test-quota-process-evidence"
source "$repository/tests/lib/run_test.sh"

test_quota_process_observer() {
python3 - "$repository/tests/fixtures/collect-quota-process-evidence.py" <<'PY'
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("quota_process_evidence", sys.argv[1])
observer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(observer)

def row(pid, parent, command="fixture", start="10:00:00"):
    return f"{pid} {parent} 10 Wed Sep 16 {start} 2026 {command}\n"

def result(command, text="", code=0, error=""):
    return subprocess.CompletedProcess(command, code, text, error)

class ObserverTests(unittest.TestCase):
    def test_reparented_children_remain_owned_but_reused_pids_do_not(self):
        tracker = observer.ProcessTracker(10)
        initial = observer.parse_processes(row(10, 1) + row(11, 10) + row(12, 11)
                                          + row(20, 1, "unrelated private arguments"))
        self.assertEqual([p["pid"] for p in tracker.observe(initial)], [10, 11, 12])
        later = observer.parse_processes(row(11, 1) + row(12, 11) + row(13, 11)
                                        + row(10, 1, start="10:01:00") + row(21, 10))
        self.assertEqual([p["pid"] for p in tracker.observe(later)], [11, 12, 13])
        self.assertEqual(tracker.observe(observer.parse_processes(row(20, 1))), [])

    def test_missing_root_and_invalid_inventory_are_not_clean_settlement(self):
        for text in ["", "not a process table", "10 1 10 Wed Sep 16 10:00:00 2026"]:
            with self.assertRaises(ValueError):
                observer.parse_processes(text)
        with self.assertRaises(ValueError):
            observer.ProcessTracker(10).observe(observer.parse_processes(row(20, 1)))

    def test_capture_survives_parent_exit_and_keeps_descriptor_scope(self):
        inventories = iter([row(10, 1) + row(11, 10) + row(20, 1, "private-unrelated"),
                            row(11, 1) + row(20, 1, "private-unrelated"), row(20, 1)])
        commands = []
        def probe(command, **kwargs):
            commands.append(command)
            self.assertLessEqual(kwargs["timeout"], 3)
            if command[0] == "/bin/ps":
                return result(command, next(inventories))
            self.assertEqual(command[0], "/usr/sbin/lsof")
            self.assertIn("-FpcgfatDin", command)
            pids = command[command.index("-p") + 1].split(",")
            return result(command, "".join(f"p{pid}\nf1\naw\ntPIPE\nnpipe-address\n" for pid in pids))
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "evidence"
            with patch.object(observer.subprocess, "run", side_effect=probe), \
                 patch.object(observer.time, "sleep"):
                self.assertEqual(observer.collect(10, output, 2), 0)
            events = [json.loads(s) for s in (output / "processes.jsonl").read_text().splitlines()]
            self.assertEqual(events[-1]["event"], "observed-exit")
            descriptors = [e for e in events if e["event"] == "descriptors"]
            self.assertEqual([e["pids"] for e in descriptors], [[10, 11], [11]])
            self.assertIn("pipe-address", descriptors[0]["stdout"])
            self.assertIn("\naw\n", descriptors[0]["stdout"])
            self.assertNotIn("private-unrelated", (output / "processes.jsonl").read_text())
            with self.assertRaises(FileExistsError):
                observer.collect(10, output, 2)

    def test_denied_inventory_preserves_failure_and_starts_nothing(self):
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "evidence"
            with patch.object(observer.subprocess, "run", return_value=result([], code=1, error="Operation not permitted")) as probe:
                self.assertEqual(observer.collect(10, output, 2), 2)
            self.assertEqual(probe.call_count, 1)
            text = (output / "processes.jsonl").read_text()
            self.assertIn("Operation not permitted", text)
            self.assertNotIn("observed-exit", text)

    def test_deadline_reports_live_processes_without_cleaning_them_up(self):
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "evidence"
            def probe(command, **kwargs):
                return result(command, row(10, 1) if command[0] == "/bin/ps" else "p10\nf1\ntPIPE\n")
            with patch.object(observer.subprocess, "run", side_effect=probe), \
                 patch.object(observer.time, "monotonic", side_effect=[0, 0, 3]), \
                 patch.object(observer.time, "sleep"):
                self.assertEqual(observer.collect(10, output, 2), 2)
            events = [json.loads(s) for s in (output / "processes.jsonl").read_text().splitlines()]
            self.assertEqual(events[-1]["event"], "deadline")
            self.assertEqual(events[-1]["last_observed_pids"], [10])

    def test_descriptor_failure_is_retained_even_if_processes_exit(self):
        inventories = iter([row(10, 1), row(20, 1)])
        def probe(command, **kwargs):
            if command[0] == "/bin/ps":
                return result(command, next(inventories))
            return result(command, code=1, error="permission denied")
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "evidence"
            with patch.object(observer.subprocess, "run", side_effect=probe), \
                 patch.object(observer.time, "sleep"):
                self.assertEqual(observer.collect(10, output, 2), 2)
            events = [json.loads(s) for s in (output / "processes.jsonl").read_text().splitlines()]
            self.assertEqual(events[-1]["event"], "observed-exit")
            self.assertTrue(events[-1]["descriptor_gaps"])

    def test_empty_descriptor_result_is_not_complete_evidence(self):
        inventories = iter([row(10, 1), row(20, 1)])
        def probe(command, **kwargs):
            return result(command, next(inventories) if command[0] == "/bin/ps" else "")
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "evidence"
            with patch.object(observer.subprocess, "run", side_effect=probe), \
                 patch.object(observer.time, "sleep"):
                self.assertEqual(observer.collect(10, output, 2), 2)

    def test_cli_rejects_unbounded_or_invalid_selection_before_observing(self):
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "evidence"
            for pid, seconds in [("0", "90"), ("10", "0"), ("10", "121")]:
                refused = subprocess.run([sys.executable, str(spec.origin), "--pid", pid,
                                          "--seconds", seconds, "--output", str(output)],
                                         capture_output=True, text=True, timeout=3)
                self.assertEqual(refused.returncode, 2)
                self.assertIn("--seconds must be between", refused.stderr)
                self.assertFalse(output.exists())

unittest.main(argv=[sys.argv[0]])
PY
}

run_test "quota process observer preserves scoped evidence" test_quota_process_observer
summarize_and_exit

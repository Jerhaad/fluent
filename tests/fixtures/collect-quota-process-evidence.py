#!/usr/bin/env python3
"""Observe one focused quota-test process tree on an authorized macOS host.

This read-only observer does not launch tests, alter their concurrency, signal
processes, or grant verification credit. Keep its output with native test logs.
Sampling can miss a child that exits or reparents between observations; an empty
observed tree is not proof that every possible descendant has exited.
"""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import time


def parse_processes(text):
    """Parse the C-locale PID, parent, group, start time, and command columns."""
    processes = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        fields = line.split(None, 8)
        if len(fields) != 9:
            raise ValueError("malformed process inventory row")
        pid, parent, group = map(int, fields[:3])
        if pid <= 0 or parent < 0 or group < 0 or pid in processes:
            raise ValueError("invalid or duplicate process identity")
        processes[pid] = {
            "pid": pid,
            "ppid": parent,
            "pgid": group,
            "started": " ".join(fields[3:8]),
            "command": fields[8],
        }
    if not processes:
        raise ValueError("process inventory is empty")
    return processes


class ProcessTracker:
    """Retain observed descendants across reparenting without adopting reused PIDs."""

    def __init__(self, root_pid):
        self.root_pid = root_pid
        self.identities = {}

    def observe(self, processes):
        if not self.identities:
            if self.root_pid not in processes:
                raise ValueError("the selected root exited before its identity was observed")
            self.identities[self.root_pid] = processes[self.root_pid]["started"]
        live = {
            pid for pid, started in self.identities.items()
            if pid in processes and processes[pid]["started"] == started
        }
        while True:
            children = {
                pid for pid, process in processes.items()
                if pid not in self.identities and process["ppid"] in live
            }
            if not children:
                break
            for pid in children:
                self.identities[pid] = processes[pid]["started"]
            live.update(children)
        return [processes[pid] for pid in sorted(live)]


def collect(root_pid, output, seconds):
    """Record scoped observations before cleanup, stopping at exit or a deadline."""
    output.mkdir(parents=True, exist_ok=False)
    tracker = ProcessTracker(root_pid)
    deadline = time.monotonic() + seconds
    descriptor_gaps = False
    last_pids = []
    with (output / "processes.jsonl").open("x") as log:
        def emit(event, **data):
            log.write(json.dumps({
                "event": event,
                "observed_at": datetime.now(timezone.utc).isoformat(),
                **data,
            }) + "\n")
            log.flush()

        emit("observer", root_pid=root_pid, seconds=seconds,
             identity_resolution="PID and ps lstart (one-second resolution)",
             limitation="Unobserved descendants and descriptor changes between samples remain unknown")
        try:
            while time.monotonic() < deadline:
                command = ["/bin/ps", "-axo", "pid=,ppid=,pgid=,lstart=,command="]
                result = subprocess.run(command, capture_output=True, text=True,
                                        timeout=3, env={**os.environ, "LC_ALL": "C"})
                if result.returncode != 0:
                    emit("inventory-error", command=command, returncode=result.returncode,
                         stderr=result.stderr)
                    return 2
                # Never persist unrelated process-table rows or their arguments.
                live = tracker.observe(parse_processes(result.stdout))
                last_pids = [process["pid"] for process in live]
                emit("processes", processes=live)
                if not live:
                    emit("observed-exit", descriptor_gaps=descriptor_gaps)
                    return 2 if descriptor_gaps else 0
                command = ["/usr/sbin/lsof", "-nP", "-a", "-p",
                           ",".join(map(str, last_pids)), "-FpcgfatDin"]
                result = subprocess.run(command, capture_output=True, text=True, timeout=3)
                emit("descriptors", command=command, pids=last_pids,
                     returncode=result.returncode, stdout=result.stdout, stderr=result.stderr)
                # A disappearing process may race lsof. Preserve the gap rather
                # than claiming an empty output proves descriptor settlement.
                captured_pids = {
                    int(line[1:]) for line in result.stdout.splitlines()
                    if line.startswith("p") and line[1:].isdigit()
                }
                descriptor_gaps |= (result.returncode != 0 or bool(result.stderr)
                                    or captured_pids != set(last_pids))
                time.sleep(0.1)
            emit("deadline", last_observed_pids=last_pids, descriptor_gaps=descriptor_gaps)
        except (OSError, ValueError, subprocess.TimeoutExpired) as error:
            emit("observer-error", error=str(error), last_observed_pids=last_pids)
    return 2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True,
                        help="observed PID of the focused nextest launch")
    parser.add_argument("--output", type=Path, required=True,
                        help="new directory to preserve process evidence")
    parser.add_argument("--seconds", type=int, default=90,
                        help="observation deadline, at most 120 seconds")
    args = parser.parse_args()
    if args.pid <= 0 or not 1 <= args.seconds <= 120:
        parser.error("--pid must be positive and --seconds must be between 1 and 120")
    return collect(args.pid, args.output, args.seconds)


if __name__ == "__main__":
    raise SystemExit(main())

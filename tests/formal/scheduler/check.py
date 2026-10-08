#!/usr/bin/env python3
"""Run candidate and defect-control TLC checks with the pinned official JAR."""

import argparse
import hashlib
import re
import subprocess
import sys
from pathlib import Path

JAR_SHA256 = "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
CASES = (
    ("QueueHandoff", "queue-candidate.cfg", None),
    ("QueueHandoff", "queue-historical-6875b409.cfg", "Recoverable"),
    ("QueueHandoff", "queue-negative-duplicate.cfg", "OneAttempt"),
    ("GenerationFence", "generation-candidate.cfg", None),
    ("GenerationFence", "generation-historical-6875b409.cfg", "StaleOperationRejected"),
    ("GenerationFence", "generation-negative-identity.cfg", "StaleOperationRejected"),
    ("ServiceIsolation", "isolation-candidate.cfg", None),
    ("ServiceIsolation", "isolation-historical-6875b409.cfg", "HealthyProgress"),
    ("ServiceIsolation", "isolation-negative-spin.cfg", "PollBound"),
    ("ServiceIsolation", "isolation-negative-drop.cfg", "WorkerRetained"),
)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", choices=["recovery"], required=True)
    parser.add_argument("--jar", type=Path, required=True)
    args = parser.parse_args()
    jar = args.jar.resolve()
    if not jar.is_file():
        parser.error(f"verified TLC JAR is missing: {jar}")
    digest = hashlib.sha256(jar.read_bytes()).hexdigest()
    if digest != JAR_SHA256:
        parser.error(f"TLC JAR SHA-256 mismatch: {digest}")
    try:
        version = subprocess.run(["java", "-version"], capture_output=True, text=True, check=True)
    except (OSError, subprocess.CalledProcessError) as error:
        parser.error(f"Java 21 is required: {error}")
    if not re.search(r'(?:version "21\.|openjdk 21\.)', version.stderr + version.stdout):
        parser.error(f"Java 21 is required: {version.stderr.strip()}")

    source = Path(__file__).resolve().parent
    logs = Path("target/formal/scheduler").resolve()
    logs.mkdir(parents=True, exist_ok=True)
    failed = False
    for module, config, expected_invariant in CASES:
        command = ["java", "-cp", str(jar), "tlc2.TLC", "-workers", "1", "-deadlock", "-config", config, module]
        try:
            result = subprocess.run(command, cwd=source, capture_output=True, text=True, timeout=120)
            output = result.stdout + result.stderr
        except subprocess.TimeoutExpired as error:
            output = f"TLC timed out after 120 seconds: {error}\n"
            result = None
        log = logs / f"{config}.log"
        log.write_text("$ " + " ".join(command) + "\n" + output)
        if expected_invariant is None:
            passed = result is not None and result.returncode == 0 and "Model checking completed" in output
        else:
            passed = (result is not None and result.returncode != 0
                      and re.search(rf"Invariant {re.escape(expected_invariant)} is violated", output) is not None)
        label = "PASS" if passed else "FAIL"
        print(f"{label} {module} {config} -> {log}")
        failed |= not passed
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Run candidate and defect-control TLC checks with the pinned official JAR."""

import argparse
import hashlib
import re
import subprocess
import sys
import tempfile
from pathlib import Path

JAR_SHA256 = "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
RECOVERY_CASES = (
    ("QueueHandoff", "queue-current.cfg", "Temporal properties were violated"),
    ("QueueHandoff", "queue-proposed.cfg", None),
    ("QueueHandoff", "queue-mutant-no-lease-check.cfg", "Invariant OwnerTokenCurrent is violated"),
    ("QueueHandoff", "queue-mutant-new-attempt.cfg", "Invariant OneAttempt is violated"),
    ("GenerationFence", "generation-current.cfg", None),
    ("GenerationFence", "generation-mutant-no-fence.cfg", "Invariant NoStaleCommit is violated"),
    ("ServiceIsolation", "isolation-current.cfg", "Temporal properties were violated"),
    ("ServiceIsolation", "isolation-proposed.cfg", None),
)
LIFECYCLE_CASES = (
    ("ServiceLifecycle", "lifecycle-proposed.cfg", None),
    ("ServiceLifecycle", "lifecycle-legacy.cfg", None),
    ("ServiceLifecycle", "lifecycle-negative-build.cfg", "Invariant NoMismatchedReuse is violated"),
    ("ServiceLifecycle", "lifecycle-negative-instance.cfg", "Invariant NoStaleStop is violated"),
    ("ServiceLifecycle", "lifecycle-negative-admission.cfg", "Invariant NoAdmissionAfterDrain is violated"),
    ("ServiceLifecycle", "lifecycle-negative-replay.cfg", "Invariant ReplayStable is violated"),
    ("ServiceLifecycle", "lifecycle-negative-retention.cfg", "Invariant RetainedResult is violated"),
)


def is_java_21_version(output: str) -> bool:
    return bool(re.search(r'(?:\bversion "21(?:\.|"))|(?:\bopenjdk 21(?:\.|\s|$))', output))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", choices=["recovery", "lifecycle"], required=True)
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
    if not is_java_21_version(version.stderr + version.stdout):
        parser.error(f"Java 21 is required: {version.stderr.strip()}")

    source = Path(__file__).resolve().parent
    logs = Path("target/formal/scheduler").resolve()
    logs.mkdir(parents=True, exist_ok=True)
    failed = False
    cases = RECOVERY_CASES if args.suite == "recovery" else LIFECYCLE_CASES
    for module, config, expected_invariant in cases:
        with tempfile.TemporaryDirectory(prefix="tlc-", dir=logs) as metadir:
            command = ["java", "-XX:+UseParallelGC", "-Xmx256m", "-cp", str(jar),
                       "tlc2.TLC", "-workers", "1", "-fp", "0", "-seed", "1",
                       "-metadir", metadir, "-config", config, module + ".tla"]
            try:
                result = subprocess.run(command, cwd=source, capture_output=True, text=True, timeout=180)
                output = result.stdout + result.stderr
            except subprocess.TimeoutExpired as error:
                output = f"TLC timed out after 180 seconds: {error}\n"
                result = None
        log = logs / f"{config}.log"
        log.write_text("$ " + " ".join(command) + "\n" + output)
        if expected_invariant is None:
            passed = result is not None and result.returncode == 0 and "Model checking completed" in output
        else:
            passed = (result is not None and result.returncode != 0
                      and expected_invariant in output)
        label = "PASS" if passed else "FAIL"
        print(f"{label} {module} {config} -> {log}")
        failed |= not passed
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())

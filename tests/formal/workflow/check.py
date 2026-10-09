#!/usr/bin/env python3
"""Check the foundation model, defect controls, and a fresh public-command trace."""

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

JAR_SHA256 = "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
CONTROLS = {
    "deliverinputs": "Invariant InputSafe is violated",
    "checkevidence": "Invariant EvidenceSafe is violated",
    "fenceresume": "Invariant ResumeSafe is violated",
    "guardeffect": "Invariant OneEffect is violated",
    "truthfulobservation": "Invariant ObservationSafe is violated",
    "guardcleanup": "Invariant CleanupSafe is violated",
    "guardversion": "Invariant VersionSafe is violated",
}


def run_case(source, output_dir, jar, config, expected):
    with tempfile.TemporaryDirectory(prefix="tlc-", dir=output_dir) as metadir:
        command = ["java", "-XX:+UseParallelGC", "-Xmx512m", "-cp", str(jar),
                   "tlc2.TLC", "-deadlock", "-workers", "1", "-fp", "0",
                   "-seed", "1", "-metadir", metadir, "-config", config,
                   "Workflow.tla"]
        try:
            result = subprocess.run(command, cwd=source, capture_output=True,
                                    text=True, timeout=240)
            log = result.stdout + result.stderr
        except subprocess.TimeoutExpired as error:
            result = None
            log = f"TLC timed out: {error}\n"
    (output_dir / f"{config}.log").write_text("$ " + " ".join(command) + "\n" + log)
    if expected:
        ok = result is not None and result.returncode != 0 and expected in log
    else:
        ok = result is not None and result.returncode == 0 and "Model checking completed" in log
    return {"case": config, "pass": ok, "expected": expected, "log": f"{config}.log"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", required=True)
    parser.add_argument("--jar", type=Path, required=True)
    args = parser.parse_args()
    if args.suite != "foundation":
        parser.error(f"unknown or incomplete suite: {args.suite}")
    jar = args.jar.resolve()
    if not jar.is_file() or hashlib.sha256(jar.read_bytes()).hexdigest() != JAR_SHA256:
        parser.error("pinned TLC 1.7.4 JAR is missing or has the wrong SHA-256")
    try:
        version = subprocess.run(["java", "-version"], capture_output=True,
                                 text=True, check=True)
    except (OSError, subprocess.CalledProcessError) as error:
        parser.error(f"Java 21 is required: {error}")
    if not re.search(r'(?:version "21(?:\.|"))|(?:openjdk 21(?:\.|\s|$))',
                     version.stdout + version.stderr):
        parser.error("Java 21 is required")
    source = Path(__file__).resolve().parent
    root = source.parents[2]
    output = root / "target/formal/workflow/foundation"
    output.mkdir(parents=True, exist_ok=True)
    results = []
    for config, expected in [("foundation.cfg", None),
                             ("repeated-recovery.cfg", None),
                             *[(f"negative-{name}.cfg", violation)
                               for name, violation in CONTROLS.items()]]:
        results.append(run_case(source, output, jar, config, expected))
        print(("PASS" if results[-1]["pass"] else "FAIL") + " " + config)
    command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
               "--test", "workflow_assurance", "--no-tests", "fail",
               "workflow_foundation_public_trace"]
    trace_path = output / "production-trace.json"
    for name in ("production-trace.json", "intake.json", "planned.json",
                 "canceled.json", "refused.json"):
        (output / name).unlink(missing_ok=True)
    fixture = subprocess.run(command, cwd=root, capture_output=True, text=True)
    (output / "production-fixture.log").write_text("$ " + " ".join(command) + "\n"
                                                    + fixture.stdout + fixture.stderr)
    fixture_ok = fixture.returncode == 0 and trace_path.is_file()
    results.append({"case": "fresh-production-fixture", "pass": fixture_ok,
                    "log": "production-fixture.log"})
    if fixture_ok:
        trace = json.loads(trace_path.read_text())
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                              capture_output=True, text=True, check=True).stdout.strip()
        binary = root / "target/debug/fluent"
        bound = trace["fluent-commit"] == head and binary.is_file() and \
            trace["fluent-binary-sha256"] == hashlib.sha256(binary.read_bytes()).hexdigest()
        projection = subprocess.run(["python3", str(source / "project.py"), str(trace_path)],
                                    capture_output=True, text=True)
        (output / "projection.log").write_text(projection.stdout + projection.stderr)
        valid = bound and projection.returncode == 0
        results.append({"case": "production-projection", "pass": valid,
                        "log": "projection.log"})
        corrupt = json.loads(json.dumps(trace))
        corrupt["events"][2]["raw-facts"]["work"]["id"] = "wrong-work"
        corrupt_path = output / "negative-trace.json"
        corrupt_path.write_text(json.dumps(corrupt))
        negative = subprocess.run(["python3", str(source / "project.py"), str(corrupt_path)],
                                  capture_output=True, text=True)
        (output / "negative-trace.log").write_text(negative.stdout + negative.stderr)
        results.append({"case": "negative-trace", "pass": negative.returncode != 0 and
                        "raw snapshot differs from artifact" in negative.stderr,
                        "log": "negative-trace.log"})
    result = {"schema-version": 1, "suite": args.suite, "cases": results,
              "pass": all(row["pass"] for row in results)}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return 0 if result["pass"] else 1


if __name__ == "__main__":
    sys.exit(main())

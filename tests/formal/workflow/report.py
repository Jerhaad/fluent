#!/usr/bin/env python3
"""Summarize candidate-bound workflow checks without crediting missing evidence."""

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

SUITES = ("foundation", "writer-inputs", "evidence-status",
          "recovery-observation", "release-transition", "integrated-production")
REQUIRED_CASES = {
    "foundation": ("foundation.cfg", "combined-one-failure.cfg",
                   "repeated-recovery.cfg", "review-only.cfg", "service-competition.cfg",
                   "negative-deliverinputs.cfg", "negative-checkevidence.cfg",
                   "negative-reviewroute.cfg", "negative-guardversion.cfg",
                   "fresh-production-fixture", "production-projection",
                   "production-model-path", "negative-trace-projection",
                   "negative-model-path"),
    "writer-inputs": ("negative-deliverinputs.cfg", "fresh-writer-fixture",
                      "writer-production-projection", "omitted-instruction-control",
                      "missing-delivery-boundary-control", "writer-production-path.cfg"),
    "evidence-status": ("negative-checkevidence.cfg", "negative-landing-new.cfg",
                        "fresh-evidence-fixture", "public-carried-review-fixture",
                        "public-stale-carried-review-landing",
                        "evidence-production-projection", "evidence-production-path.cfg",
                        "missing-observation"),
    "recovery-observation": ("repeated-recovery.cfg", "joint-settlement.cfg",
                             "negative-joint-settlement.cfg", "fresh-public-recovery",
                             "recovery-projection", "recovery-production.cfg"),
    "release-transition": ("released-fixture-1", "released-fixture-2",
                           "released-fixture-3", "current-authority-projection",
                           "pending-publication-projection", "service-projection",
                           "service-competition.cfg", "negative-guardversion.cfg"),
    "integrated-production": ("fresh-queue-cancel", "queue-cancel-projection",
                              "cross-work-negative"),
}
BOUNDARIES = (
    "Intake and authorization scope", "Foreground execution", "Queued claim and lease",
    "Recovered dispatch", "Generation fence", "Shared service ownership",
    "Service drain and child settlement", "Project isolation", "Child launch and stop",
    "Required Writer input", "Writer completion and file failures",
    "Ordinary and targeted review", "Canonical Tester", "Baseline and canonical outcome",
    "Capture and no-expertise Learning", "Ready candidate and human acceptance",
    "Landing intent and Git effect", "Landing result and replay", "Follow-up publication",
    "Review-only Work", "Status observation", "Writer completion recovery guidance",
    "Public continuation requirement identity", "Cleanup eligibility", "Version transitions",
)
MODELS = ("Workflow.tla", "Trace.tla", "WriterTrace.tla", "EvidenceTrace.tla",
          "RecoveryTrace.tla", "JointSettlement.tla")
SCHEDULER_MODELS = ("QueueHandoff.tla", "GenerationFence.tla", "ServiceIsolation.tla",
                    "ChildLaunchIdentity.tla", "ServiceLifecycle.tla",
                    "LandingPublication.tla")
EXTERNAL_GATES = {
    "plan:step-6": "python3 tests/formal/workflow/check.py --suite=integrated --jar=target/fluent-tools/tla2tools-1.7.4.jar",
    "plan:step-7": "FLUENT_WORKFLOW_REQUIRE_HOST=1 cargo nextest run --offline --features test-support --test workflow_assurance --no-tests fail workflow_release_build_",
    "plan:step-8": "native Architecture, Behaviors, Documentation, Skills and Tests reviews",
    "plan:step-9": "canonical final Tester from .fluent/tester.yaml",
    "plan:step-10": "capture-mode Learner",
}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def connection_credit(evidence, passing_cases):
    """Credit only a named, checked model-to-production connection."""
    if not evidence.startswith("Verified: "):
        return False
    parts = evidence.removeprefix("Verified: ").split("; ")
    if len(parts) != 4:
        return False
    references = {}
    for key, part in zip(("model", "assumption", "production", "control"), parts):
        if not part.startswith(key + "="):
            return False
        match = re.fullmatch(r"([a-z][a-z-]*)/([A-Za-z0-9_.-]+)",
                             part[len(key) + 1:])
        if match is None:
            return False
        suite, case = match.groups()
        if case not in passing_cases.get(suite, set()):
            return False
        references[key] = (suite, case)
    return len({references["model"], references["production"],
                references["control"]}) == 3


def connections(source, passing_cases):
    rows = []
    for line in (source / "connections.md").read_text().splitlines():
        if not line.startswith("| ") or line.startswith("| ---"):
            continue
        cells = [cell.strip() for cell in line.strip("| ").split("|")]
        if len(cells) != 5 or cells[0] == "Boundary and property":
            continue
        rows.append({"boundary": cells[0], "model-mapping": cells[1],
                     "assumption": cells[2], "scenario-control-owner": cells[3],
                     "evidence": cells[4],
                     "pass": connection_credit(cells[4], passing_cases)})
    return rows


def report(results, candidate, binary, source, jar=None, java=None):
    suites = {}
    missing = []
    for name in SUITES:
        directory = results / ("integrated" if name == "integrated-production" else name)
        filename = "production-result.json" if name == "integrated-production" else "result.json"
        path = directory / filename
        if not path.is_file():
            missing.append(f"{name}: {filename}")
            suites[name] = {"pass": False, "reason": "missing result"}
            continue
        try:
            value = json.loads(path.read_text())
            assert value["schema-version"] == 1 and value["suite"] == name
            cases = value["cases"]
            assert isinstance(cases, list) and cases
            logs = {}
            for case in cases:
                assert isinstance(case["case"], str) and isinstance(case["pass"], bool)
                if "log" in case:
                    log = case["log"]
                    assert isinstance(log, str) and Path(log).name == log
                    log_path = directory / log
                    assert log_path.is_file()
                    logs[log] = digest(log_path)
            observed = [case["case"] for case in cases]
            required = set(REQUIRED_CASES[name])
            complete = len(observed) == len(set(observed)) and required <= set(observed)
            bound = value.get("candidate-commit") == candidate and \
                value.get("fluent-binary-sha256") == digest(binary)
            passed = bool(value["pass"]) and bound and complete and \
                all(case["pass"] for case in cases)
            suites[name] = {"pass": passed, "candidate-bound": bound,
                            "required-cases-present": complete,
                            "result-sha256": digest(path), "log-sha256": logs,
                            "cases": cases}
            if not passed:
                missing.append(f"{name}: failed or unbound result")
        except (AssertionError, KeyError, TypeError, ValueError, OSError) as error:
            missing.append(f"{name}: malformed result or missing log")
            suites[name] = {"pass": False, "reason": str(error)}
    passing_cases = {name: {case["case"] for case in value.get("cases", [])
                            if case.get("pass") is True}
                     for name, value in suites.items() if value.get("pass") is True}
    mappings = connections(source, passing_cases)
    if [row["boundary"] for row in mappings] != list(BOUNDARIES):
        missing.append("connection inventory changed or incomplete")
    missing.extend(f"connection: {row['boundary']}" for row in mappings if not row["pass"])
    missing.extend(f"model: {name}" for name in MODELS if not (source / name).is_file())
    scheduler = source.parent / "scheduler"
    missing.extend(f"scheduler model: {name}" for name in SCHEDULER_MODELS
                   if not (scheduler / name).is_file())
    tools = {"tlc-jar": jar, "java": java}
    missing.extend(f"tool: {name}" for name, path in tools.items()
                   if path is None or not path.is_file())
    return {"schema-version": 1, "candidate-commit": candidate,
            "fluent-binary-sha256": digest(binary),
            "model-sha256": {name: digest(source / name) for name in MODELS
                             if (source / name).is_file()} |
                            {name: digest(scheduler / name) for name in SCHEDULER_MODELS
                             if (scheduler / name).is_file()},
            "tool-sha256": {name: digest(path) for name, path in tools.items()
                            if path is not None and path.is_file()},
            "suite-source-sha256": {"workflow": digest(source / "check.py"),
                                    "scheduler": digest(scheduler / "check.py"),
                                    "report": digest(source / "report.py"),
                                    "integrated-projector": digest(source / "project_integrated.py")},
            "connection-inventory-sha256": digest(source / "connections.md"),
            "suites": suites, "connections": mappings,
            "external-gates": {name: {"state": "pending", "command": command}
                               for name, command in EXTERNAL_GATES.items()},
            "missing": missing,
            "pass": not missing}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--jar", type=Path)
    parser.add_argument("--java", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    source = Path(__file__).resolve().parent
    result = report(args.results, args.candidate, args.binary, source,
                    args.jar, args.java)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    return 0 if result["pass"] else 1


if __name__ == "__main__":
    sys.exit(main())

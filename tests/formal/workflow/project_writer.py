#!/usr/bin/env python3
"""Project exact classified Writer input observations onto the workflow model."""

import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require


NAMES = {"classified-work.json", "writer-work.json", "classification.json",
         "provider-observation.txt"}


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and
            trace["scenario"] == "writer-inputs-classified-source-correction" and
            trace["model-profile"] == "writer-inputs" and
            trace["collection-complete"] is True, "wrong or incomplete writer trace")
    for key in ("fluent-commit", "fluent-binary-sha256"):
        require(type(trace[key]) is str and re.fullmatch(r"[0-9a-f]{40,64}", trace[key]),
                f"invalid {key}")
    artifacts = trace["artifacts"]
    require(type(artifacts) is list and len(artifacts) == len(NAMES),
            "missing writer artifacts")
    seen = set()
    for artifact in artifacts:
        exact_keys(artifact, ("path", "sha256"), "artifact")
        name = artifact["path"]
        require(name in NAMES and name not in seen, "invalid writer artifact path")
        seen.add(name)
        require(hashlib.sha256((root / name).read_bytes()).hexdigest() == artifact["sha256"],
                f"artifact digest differs: {name}")
    events = trace["events"]
    require(type(events) is list and len(events) == 2, "missing writer events")
    classified = load_json((root / "classified-work.json").read_text())
    completed = load_json((root / "writer-work.json").read_text())
    recovery = classified["attempts"][0]["evidence_recoveries"][0]
    authority = recovery["code_change_classification"]
    audit = load_json((root / "classification.json").read_text())
    observation = (root / "provider-observation.txt").read_text()
    require(audit["operation_id"] == authority["operation_id"] and
            audit["writer_task_id"] == authority["writer_task_id"] and
            audit["candidate_commit"] == recovery["candidate_commit"] and
            audit["authorization"] == authority["authorization"] and
            audit["authorization_digest"] == authority["authorization_digest"] and
            audit["review_digest"] == authority["review_digest"] and
            audit["evidence"]["digest"] == recovery["attachment"]["digest"],
            "classification audit binding differs")
    require("sha256:" + hashlib.sha256(audit["authorization"].encode()).hexdigest() ==
            audit["authorization_digest"], "authorization digest differs")
    require(classified["id"] == completed["id"] == audit["work_item_id"] and
            audit["attempt_id"] == "attempt-1", "writer identity differs")
    writer = next(task for task in completed["attempts"][0]["tasks"]
                  if task["id"] == authority["writer_task_id"])
    require(writer["status"] == "complete" and writer["output"] is not None,
            "writer did not launch and complete")
    require(any(ref["path"] == authority["audit_artifact"]["path"] and
                ref["producer_id"] == authority["operation_id"]
                for ref in writer["input_artifacts"]), "writer lost audit input")
    require("Current assigned action: source correction" in observation and
            audit["authorization"] in observation and
            authority["audit_artifact"]["path"] in observation and
            observation.endswith("\nread\n"), "provider did not observe current instructions")
    for index, (actor, boundary, operation, work) in enumerate((
            ("operator", "classify-source-correction", authority["operation_id"], classified),
            ("worker", "writer-launch", authority["writer_task_id"], completed))):
        event = events[index]
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == actor and
                event["operation-id"] == operation and event["generation"] is None and
                event["boundary"] == boundary and
                event["causal-predecessors"] == ([] if index == 0 else [1]),
                "writer event identity or order differs")
        exact_keys(event["raw-facts"], ("exit-code", "work"), "raw facts")
        require(event["raw-facts"]["exit-code"] == 0 and
                event["raw-facts"]["work"] == work, "writer event facts differ")
    return {"states": ["intake", "planned", "authorized", "authorized",
                       "dispatched", "delivered", "written"],
            "work-id": classified["id"], "attempt-id": "attempt-1"}


def main():
    try:
        path = Path(sys.argv[1]).resolve()
        require(len(sys.argv) == 2, "invalid arguments")
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, KeyError, IndexError, StopIteration) as error:
        print(f"writer trace violation: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

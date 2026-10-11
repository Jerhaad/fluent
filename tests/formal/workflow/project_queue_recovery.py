#!/usr/bin/env python3
"""Check repeated public queue recovery and rejected stale settlements."""
import hashlib
import json
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require
from project_queue_dispatch import project as project_dispatch


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "queue-recovery"
            and trace["model-profile"] == "queue-recovery"
            and trace["collection-complete"] is True, "incomplete recovery trace")
    require(type(trace["events"]) is list and len(trace["events"]) == 3,
            "missing recovery observations")
    require(type(trace["artifacts"]) is list and len(trace["artifacts"]) == 3,
            "missing recovery artifacts")
    facts = []
    for index, (event, artifact) in enumerate(zip(trace["events"], trace["artifacts"])):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "operator"
                and event["generation"] is None
                and event["boundary"] == ("process-crash" if index < 2 else "continued-and-fenced")
                and event["operation-id"] == ("kill-scheduler" if index < 2 else "scheduler-run")
                and event["causal-predecessors"] == ([] if index == 0 else [index]),
                "ambiguous recovery order")
        exact_keys(artifact, ("path", "sha256"), "artifact")
        require(artifact["path"] == f"recovery-{index}.json", "invalid recovery path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "recovery digest differs")
        require(load_json(raw) == event["raw-facts"], "recovery artifact differs from observation")
        facts.append(event["raw-facts"])
    exact_keys(facts[2], ("continued", "stale-settlements"), "continuation")
    continued = facts[2]["continued"]
    require(all(trace[key] == continued[key] for key in
                ("fluent-commit", "fluent-binary-sha256")), "recovery candidate differs")
    mapping = project_dispatch(continued, root, recoveries=2)
    final = continued["events"][1]["raw-facts"]["first"]
    initial = continued["events"][0]["raw-facts"]["first"]
    stale = facts[2]["stale-settlements"]
    require(type(stale) is list and len(stale) == 2, "missing stale settlement attempts")
    for index, crash in enumerate(facts[:2]):
        exact_keys(crash, ("dispatch", "work", "lease-before-crash", "lease-after-crash"), "crash")
        dispatch, work = crash["dispatch"], crash["work"]
        require(crash["lease-before-crash"] is True and crash["lease-after-crash"] is False,
                "crash did not release the Attempt lease")
        require(dispatch["status"] == "running" and
                dispatch["work_item_id"] == work["id"] == final["work_item_id"] and
                dispatch["dispatch_id"] == final["dispatch_id"], "recovery changed dispatch identity")
        require(type(work["attempts"]) is list and len(work["attempts"]) == 1 and
                work["attempts"][0]["id"] == dispatch["bound_attempt_id"] == final["bound_attempt_id"],
                "recovery changed bound Attempt")
        require(dispatch["generation"] == initial["generation"] + 2 + index * 3,
                "recovery did not fence the previous generation")
        exact_keys(stale[index], ("generation", "error", "before", "after"), "stale settlement")
        require(stale[index]["generation"] == dispatch["generation"] and
                "generation" in stale[index]["error"] and
                stale[index]["before"] == stale[index]["after"] == final,
                "stale settlement changed the current dispatch")
    return {**mapping, "crashes": 2, "states": ["running", "crashed", "recovered",
            "running", "crashed", "recovered", "running", "stale-settlement-refused"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"queue recovery projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

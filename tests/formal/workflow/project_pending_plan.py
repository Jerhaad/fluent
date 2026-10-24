#!/usr/bin/env python3
"""Project a failed Plan proof through an observed no-change Writer."""

import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, require


NAMES = ("pending-plan.json", "after-no-change.json", "after-proof.json",
         "ready.json")
ACTORS = ("attempt-run", "attempt-run", "evidence-attach", "attempt-run")
OPERATIONS = ("initial-writer", "writer-no-change", "plan-proof", "resume")
BOUNDARIES = ("pending-plan", "writer-complete", "proof-attached", "ready")


def project(trace, directory):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile",
                       "collection-complete", "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and
            trace["scenario"] == "integrated-no-change-pending-plan" and
            trace["model-profile"] == "pending-plan-recovery" and
            trace["collection-complete"] is True, "trace identity or completeness differs")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]),
            "candidate binding is invalid")
    artifacts = trace["artifacts"]
    require(type(artifacts) is list and len(artifacts) == 4,
            "Plan snapshots are incomplete")
    works = []
    for entry, name in zip(artifacts, NAMES):
        exact_keys(entry, ("path", "sha256"), "artifact")
        require(entry["path"] == name, "unexpected artifact path or order")
        data = (directory / name).read_bytes()
        require(hashlib.sha256(data).hexdigest() == entry["sha256"],
                f"artifact digest differs: {name}")
        works.append(json.loads(data))
    events = trace["events"]
    require(type(events) is list and len(events) == 4,
            "missing or extra Plan events")
    for index, event in enumerate(events):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and
                event["actor"] == ACTORS[index] and
                event["operation-id"] == OPERATIONS[index] and
                event["generation"] is None and
                event["boundary"] == BOUNDARIES[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]),
                "Plan event order or owner differs")
        exact_keys(event["raw-facts"], ("work",), "event facts")
        require(event["raw-facts"]["work"] == works[index],
                "event differs from retained Work snapshot")
    require(len({work["id"] for work in works}) == 1 and
            all(len(work["attempts"]) == 1 for work in works),
            "Work or Attempt identity changed")
    attempts = [work["attempts"][0] for work in works]
    require(len({attempt["id"] for attempt in attempts}) == 1,
            "Attempt identity changed")
    writers = [[task for task in attempt["tasks"] if task["kind"] == "write"]
               for attempt in attempts]
    require(len(writers[0]) == 1 and all(len(rows) == 2 for rows in writers[1:]),
            "no-change Writer was not executed")
    candidate = writers[0][0]["output"]["commit"]
    correction = writers[1][1]
    require(correction["status"] == "complete" and
            correction["output"]["commit"] == candidate and
            correction["output"]["no_change"] is not None and
            all(rows[1]["id"] == correction["id"] for rows in writers[1:]),
            "Writer changed candidate or lost no-change declaration")
    require(all(work["merge_candidates"] == [] for work in works[:3]) and
            works[3]["merge_candidates"] and
            works[3]["merge_candidates"][0]["candidate_commit"] == candidate and
            works[3]["merge_candidates"][0]["ready_at"] is not None,
            "Plan proof did not gate readiness")
    contract = attempts[0]["writer_completion_contract"]["requirements"]
    require(attempts[0]["status"] == "needs-user" and
            any(row["id"] == "plan:step-2" and row["responsibility"] == "host"
                for row in contract) and
            "plan:step-2" in attempts[0]["pause_record"]["next-operation"]["arguments"],
            "initial Plan check was not pending")
    recoveries = [attempt.get("evidence_recoveries", []) for attempt in attempts]
    require(len(recoveries[0]) == 0 and len(recoveries[1]) >= 1 and
            len(recoveries[2]) > len(recoveries[1]) and
            recoveries[3][:-1] == recoveries[2][:-1] and
            recoveries[3][-1]["state"] == "complete" and
            recoveries[3][-1]["candidate_commit"] == candidate,
            "pending Plan proof or recovery history disappeared")
    require(not any(task["kind"] == "tester" for attempt in attempts[:3]
                    for task in attempt["tasks"]) and
            len([task for task in attempts[3]["tasks"] if task["kind"] == "tester"]) == 1,
            "canonical Tester ran before Plan proof or did not run")
    states = []
    for index, (work, attempt) in enumerate(zip(works, attempts)):
        states.append({"work": work["id"], "attempt": attempt["id"],
                       "candidate": writers[index][0]["output"]["commit"],
                       "writer": writers[index][-1]["id"],
                       "outputCommit": writers[index][-1]["output"]["commit"],
                       "noChange": index > 0,
                       "proofCount": len(recoveries[index]),
                       "proofComplete": bool(recoveries[index] and
                                             recoveries[index][-1]["state"] == "complete"),
                       "testerSeen": any(task["kind"] == "tester"
                                         for task in attempt["tasks"]),
                       "ready": bool(work["merge_candidates"])})
    return {"states": states, "candidate": candidate,
            "writer": correction["id"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(json.loads(path.read_text()), path.parent),
                         sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"pending Plan projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

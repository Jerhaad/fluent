#!/usr/bin/env python3
"""Project a no-change Writer and canonical-check continuation."""

import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, require


NAMES = ("after-failed-check.json", "after-no-change.json",
         "after-plan-attachment.json", "ready.json")
ACTORS = ("attempt-run", "attempt-run", "evidence-attach", "scheduler-dispatch")
OPERATIONS = ("canonical-failure", "writer-no-change",
              "canonical-attachment", "queue-resume")
BOUNDARIES = ("pending-canonical", "writer-complete", "canonical-proof", "ready")


def project(trace, directory):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile",
                       "collection-complete", "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and
            trace["scenario"] == "integrated-no-change-pending-canonical" and
            trace["model-profile"] == "no-change-recovery" and
            trace["collection-complete"] is True, "trace identity or completeness differs")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]),
            "candidate binding is invalid")
    artifacts = trace["artifacts"]
    require(type(artifacts) is list and len(artifacts) == len(NAMES),
            "snapshot artifacts are incomplete")
    raw = []
    for entry, name in zip(artifacts, NAMES):
        exact_keys(entry, ("path", "sha256"), "artifact")
        require(entry["path"] == name, "unexpected artifact path or order")
        data = (directory / name).read_bytes()
        require(hashlib.sha256(data).hexdigest() == entry["sha256"],
                f"artifact digest differs: {name}")
        raw.append(json.loads(data))
    events = trace["events"]
    require(type(events) is list and len(events) == len(NAMES),
            "missing or extra events")
    for index, event in enumerate(events):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and
                event["actor"] == ACTORS[index] and
                event["operation-id"] == OPERATIONS[index] and
                event["generation"] is None and
                event["boundary"] == BOUNDARIES[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]),
                "event order or owner differs")
        exact_keys(event["raw-facts"], ("work",), "event facts")
        require(event["raw-facts"]["work"] == raw[index],
                "event differs from retained Work snapshot")
    require(len({item["id"] for item in raw}) == 1 and
            all(len(item["attempts"]) == 1 for item in raw),
            "Work or Attempt identity changed")
    attempts = [item["attempts"][0] for item in raw]
    require(len({attempt["id"] for attempt in attempts}) == 1,
            "Attempt identity changed")
    writers = [[task for task in attempt["tasks"] if task["kind"] == "write"]
               for attempt in attempts]
    testers = [[task for task in attempt["tasks"] if task["kind"] == "tester"]
               for attempt in attempts]
    require(len(writers[0]) == 1 and all(len(rows) == 2 for rows in writers[1:]) and
            all(len(rows) == 1 for rows in testers),
            "Writer or Tester history changed")
    original = writers[0][0]["output"]["commit"]
    correction = writers[1][1]
    require(correction["output"]["commit"] == original and
            correction["output"]["no_change"] is not None and
            all(rows[1]["id"] == correction["id"] for rows in writers[1:]),
            "no-change Writer did not retain the candidate")
    require(all(rows[0]["id"] == testers[0][0]["id"] for rows in testers),
            "canonical Tester was replaced")
    require(raw[0]["merge_candidates"] == [] and
            raw[1]["merge_candidates"] == [] and
            raw[2]["merge_candidates"] == [],
            "candidate became ready before canonical proof")
    require(raw[3]["merge_candidates"] and
            raw[3]["merge_candidates"][0]["candidate_commit"] == original and
            raw[3]["merge_candidates"][0]["ready_at"] is not None,
            "ready candidate lacks retained identity")
    require(len(attempts[0]["evidence_recoveries"]) >= 1 and
            len(attempts[3]["evidence_recoveries"]) >
            len(attempts[0]["evidence_recoveries"]) and
            attempts[3]["evidence_recoveries"][0] ==
            attempts[0]["evidence_recoveries"][0],
            "canonical proof or completed recovery history disappeared")
    states = []
    for index, attempt in enumerate(attempts):
        states.append({"work": raw[index]["id"], "attempt": attempt["id"],
                       "candidate": writers[index][0]["output"]["commit"],
                       "writer": writers[index][-1]["id"],
                       "outputCommit": writers[index][-1]["output"]["commit"],
                       "noChange": index > 0,
                       "proofCount": len(attempt["evidence_recoveries"]),
                       "tester": testers[index][0]["id"],
                       "ready": bool(raw[index]["merge_candidates"])})
    return {"states": states, "candidate": original,
            "attempt": attempts[0]["id"], "writer": correction["id"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(json.loads(path.read_text()), path.parent),
                         sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"no-change projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

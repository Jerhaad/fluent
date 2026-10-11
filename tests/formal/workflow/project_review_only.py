#!/usr/bin/env python3
"""Project a public review-only run from retained Work and Git facts."""

import hashlib
import json
import re
import sys
from datetime import datetime
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

ARTIFACTS = ("planned.json", "complete.json", "head-before.txt", "head-after.txt",
             "merge-candidates.txt")


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and
            trace["scenario"] == "review-only-order" and
            trace["model-profile"] == "review-only" and
            trace["collection-complete"] is True,
            "wrong or incomplete review-only trace")
    require(type(trace["fluent-commit"]) is str and
            re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            type(trace["fluent-binary-sha256"]) is str and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]),
            "invalid candidate binding")
    entries = trace["artifacts"]
    require(type(entries) is list and len(entries) == len(ARTIFACTS),
            "missing artifacts")
    observed = {}
    for entry in entries:
        exact_keys(entry, ("path", "sha256"), "artifact")
        name = entry["path"]
        require(type(name) is str and name in ARTIFACTS and name not in observed,
                "invalid artifact path")
        raw = (root / name).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == entry["sha256"],
                f"artifact digest differs: {name}")
        observed[name] = raw
    require(set(observed) == set(ARTIFACTS), "missing artifacts")
    events = trace["events"]
    require(type(events) is list and len(events) == 2, "missing review-only events")
    for index, (boundary, operation) in enumerate((
            ("review-only-planned", "review-codebase"),
            ("review-only-complete", "attempt-run"))):
        event = events[index]
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(type(event["sequence"]) is int and event["sequence"] == index + 1 and
                event["actor"] == "operator" and event["operation-id"] == operation and
                event["generation"] is None and event["boundary"] == boundary and
                event["causal-predecessors"] == ([] if index == 0 else [1]),
                "missing or ambiguous review-only order")
        facts = event["raw-facts"]
        exact_keys(facts, ("exit-code", "work"), "review-only facts")
        require(type(facts["exit-code"]) is int and facts["exit-code"] == 0 and
                facts["work"] == load_json(observed[
                    "planned.json" if index == 0 else "complete.json"]),
                "public result differs from retained Work")
    planned = events[0]["raw-facts"]["work"]
    complete = events[1]["raw-facts"]["work"]
    require(planned.get("id") == complete.get("id") == "review" and
            len(planned.get("attempts", [])) == len(complete.get("attempts", [])) == 1,
            "review Work identity changed")
    before, after = planned["attempts"][0], complete["attempts"][0]
    require(before.get("id") == after.get("id") == "attempt-review" and
            before.get("kind") == after.get("kind") == "review-only" and
            before.get("work_item_id") == after.get("work_item_id") == "review",
            "review Attempt identity changed")
    require(before.get("status") == "reviewing" and
            after.get("status") == "complete" and
            after.get("review_state") == "passed",
            "review-only completion differs")
    initial = before.get("tasks")
    final = after.get("tasks")
    require(type(initial) is list and type(final) is list and
            len(initial) == len(final) == 6 and
            [task.get("id") for task in initial] ==
            [task.get("id") for task in final],
            "review task identity changed")
    tester = final[0]
    require(tester.get("id") == "attempt-review-tester" and
            tester.get("kind") == "tester" and tester.get("status") == "complete",
            "review-only Tester did not complete")
    try:
        tester_done = datetime.fromisoformat(tester["completed_at"])
        review_starts = [datetime.fromisoformat(task["started_at"])
                         for task in final[1:]]
    except (ValueError, KeyError, TypeError) as error:
        raise TraceError(f"review-only task time missing or malformed: {error}") from error
    require(all(task.get("kind") == "review" and
                task.get("depends_on") == tester["id"] and
                task.get("status") == "complete" for task in final[1:]) and
            all(tester_done <= started for started in review_starts),
            "review started before Tester completed")
    require(all(task.get("kind") != "write" for task in final) and
            not complete.get("merge_candidates") and
            observed["merge-candidates.txt"] == b"No Merge Candidates found\n" and
            observed["head-before.txt"] == observed["head-after.txt"] and
            re.fullmatch(rb"[0-9a-f]{40}\n", observed["head-before.txt"]),
            "review-only route changed source or created a Writer or Merge Candidate")
    return {"states": ["review-only-planned", "tester-complete",
                       "reviews-complete"], "work": complete["id"],
            "attempt": after["id"], "review-count": len(final) - 1}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent),
                         sort_keys=True))
    except (TraceError, OSError, ValueError, KeyError, IndexError, TypeError) as error:
        print(f"review-only projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

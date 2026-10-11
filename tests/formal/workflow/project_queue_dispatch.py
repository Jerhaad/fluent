#!/usr/bin/env python3
"""Map public queued execution facts onto the joint dispatch boundary."""

import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

ARTIFACTS = ("first-queued.json", "second-queued.json", "first-running.json",
             "second-running.json", "first-work.json", "second-work.json")


def project(trace, root, recoveries=0):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and
            trace["scenario"] == "queue-dispatch-connection" and
            trace["model-profile"] == "queue-dispatch-connection" and
            trace["collection-complete"] is True,
            "wrong or incomplete queue dispatch trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]),
            "invalid candidate binding")
    require(type(trace["artifacts"]) is list and
            len(trace["artifacts"]) == len(ARTIFACTS), "missing artifacts")
    observed = {}
    for entry in trace["artifacts"]:
        exact_keys(entry, ("path", "sha256"), "artifact")
        name = entry["path"]
        require(name in ARTIFACTS and name not in observed, "invalid artifact path")
        raw = (root / name).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == entry["sha256"],
                f"artifact digest differs: {name}")
        observed[name] = load_json(raw)
    require(set(observed) == set(ARTIFACTS), "missing artifacts")
    events = trace["events"]
    require(type(events) is list and len(events) == 2, "missing queue events")
    for index, event in enumerate(events):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and
                event["causal-predecessors"] == ([] if index == 0 else [1]),
                "missing or ambiguous queue order")
    queued, running = [event["raw-facts"] for event in events]
    require(events[0]["actor"] == "operator" and
            events[0]["operation-id"] == "queue-add" and
            events[0]["boundary"] == "two-queued" and
            events[0]["generation"] == 0 and
            events[1]["actor"] == "scheduler" and
            events[1]["operation-id"] == "scheduler-run" and
            events[1]["boundary"] == "claim-lease-run",
            "queue boundary differs")
    exact_keys(queued, ("first", "second"), "queued facts")
    exact_keys(running, ("first", "second", "first-work", "second-work",
                         "lease-live", "coordinator-live"), "running facts")
    for name in ("first", "second"):
        require(queued[name] == observed[f"{name}-queued.json"] and
                running[name] == observed[f"{name}-running.json"] and
                running[f"{name}-work"] == observed[f"{name}-work.json"],
                "raw queue facts differ from command output")
    first, second = running["first"], running["second"]
    first_work, second_work = running["first-work"], running["second-work"]
    require(queued["first"]["status"] == "queued" and
            queued["second"]["status"] == "queued" and
            first["status"] == "running" and second["status"] == "queued",
            "competing dispatch state differs")
    require(type(running["lease-live"]) is bool and running["lease-live"],
            "running dispatch lacks a live Attempt lease")
    require(type(running["coordinator-live"]) is bool and
            running["coordinator-live"],
            "claim lacks a live project coordinator")
    require(first["dispatch_id"] == queued["first"]["dispatch_id"] and
            first["work_item_id"] == queued["first"]["work_item_id"] ==
            first_work["id"] and
            second["dispatch_id"] == queued["second"]["dispatch_id"] and
            second["work_item_id"] == queued["second"]["work_item_id"] ==
            second_work["id"] and first_work["id"] != second_work["id"],
            "Work or dispatch identity changed")
    require(type(first["generation"]) is int and
            first["generation"] == queued["first"]["generation"] + 2 + 3 * recoveries and
            events[1]["generation"] == first["generation"] and
            second["generation"] == queued["second"]["generation"],
            "claim and running generations do not match")
    attempts = first_work["attempts"]
    require(type(attempts) is list and len(attempts) == 1 and
            type(first["bound_attempt_id"]) is str and
            attempts[0]["id"] == first["bound_attempt_id"] and
            second.get("bound_attempt_id") is None and
            second_work["attempts"] == [] and
            type(first.get("claimed_at")) is str,
            "bound Attempt identity or claim history differs")
    return {"states": ["two-queued", "claim-lease-run"],
            "first-work": first_work["id"], "second-work": second_work["id"],
            "bound-attempt": first["bound_attempt_id"],
            "dispatch": first["dispatch_id"], "generation": first["generation"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent),
                         sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"queue dispatch projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

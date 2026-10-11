#!/usr/bin/env python3
"""Map cancellation timestamps and dispatch identity to authority epochs."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

BOUNDARIES = ("first-authority", "first-resumed", "later-stop", "stale-refused", "current-resumed")
OPERATIONS = ("queue-resume", "scheduler-run", "attempt-cancel", "stale-attempt-run", "queue-resume-and-run")


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "generation-authority"
            and trace["model-profile"] == "generation-connection"
            and trace["collection-complete"] is True, "incomplete authority trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]), "invalid candidate")
    require(type(trace["events"]) is list and len(trace["events"]) == 5 and
            type(trace["artifacts"]) is list and len(trace["artifacts"]) == 5,
            "missing authority observations")
    facts = []
    for index, (event, artifact) in enumerate(zip(trace["events"], trace["artifacts"])):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "operator" and
                event["generation"] is None and event["boundary"] == BOUNDARIES[index] and
                event["operation-id"] == OPERATIONS[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]),
                "ambiguous authority order")
        exact_keys(artifact, ("path", "sha256"), "artifact")
        require(artifact["path"] == f"generation-{index}.json", "invalid authority path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "authority digest differs")
        require(load_json(raw) == event["raw-facts"], "authority observation differs")
        facts.append(event["raw-facts"])
    attempts = [fact["work"]["attempts"] for fact in facts]
    require(all(len(items) == 1 for items in attempts), "authority changed Attempt count")
    require(all(fact["work"]["id"] == facts[0]["work"]["id"] for fact in facts) and
            all(items[0]["id"] == attempts[0][0]["id"] for items in attempts), "authority changed identity")
    stops = [items[0]["cancellation"] for items in attempts]
    first, current = stops[0]["requested_at"], stops[2]["requested_at"]
    require(type(first) is str and type(current) is str and first != current and
            stops[1]["requested_at"] == first and
            stops[3]["requested_at"] == stops[4]["requested_at"] == current,
            "replacement did not establish a distinct cancellation epoch")
    require(all(stops[i].get("resumed_at") is None for i in (0, 2, 3)) and
            all(type(stops[i].get("resumed_at")) is str for i in (1, 4)),
            "stale authority cleared the later stop")
    require(facts[3]["work"] == facts[2]["work"] and
            type(facts[3]["exit"]) is int and facts[3]["exit"] != 0 and
            "scheduler launch does not match the running dispatch and Attempt" in facts[3]["stderr"],
            "stale dispatch changed Work or lacked the expected refusal")
    old, new = facts[0]["dispatch"], facts[4]["dispatch"]
    for dispatch, stop in ((old, first), (new, current)):
        require(dispatch["work_item_id"] == facts[0]["work"]["id"] and
                dispatch["bound_attempt_id"] == attempts[0][0]["id"] and
                dispatch["intent"] == "resume-attempt" and
                dispatch["resume_cancellation_requested_at"] == stop,
                "resume dispatch lacks current cancellation authority")
    require(old["dispatch_id"] != new["dispatch_id"] and
            new["resume_of_dispatch_id"] == old["dispatch_id"], "replacement dispatch lacks provenance")
    # Queue generation resets for a new dispatch. Its full identity includes
    # dispatch_id; cancellation authority instead uses the requested_at epoch.
    return {"work": facts[0]["work"]["id"], "attempt": attempts[0][0]["id"],
            "epochs": [first, current], "dispatches": [old["dispatch_id"], new["dispatch_id"]],
            "states": ["authorized", "resumed", "replaced", "stale-refused", "current-resumed"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"generation projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

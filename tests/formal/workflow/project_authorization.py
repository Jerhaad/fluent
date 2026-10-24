#!/usr/bin/env python3
"""Check human authorization preserves the proposed Work scope and queue intent."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

BOUNDARIES = ("proposed", "refused", "authorized", "repeated")
OPERATIONS = ("land", "queue-add", "work-item-authorize", "work-item-authorize")


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "authorization"
            and trace["model-profile"] == "authorization-connection"
            and trace["collection-complete"] is True, "incomplete authorization trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]), "invalid candidate")
    require(type(trace["events"]) is list and len(trace["events"]) == 4 and
            type(trace["artifacts"]) is list and len(trace["artifacts"]) == 4,
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
        require(artifact["path"] == f"authorization-{index}.json", "invalid authority path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "authority digest differs")
        require(load_json(raw) == event["raw-facts"], "authority observation differs")
        facts.append(event["raw-facts"])
    proposed, refused, authorized, repeated = facts
    original, current = proposed["work"], authorized["work"]
    require(original["authorization"]["state"] == "proposed" and not original["attempts"] and
            refused["work"] == original and type(refused["exit"]) is int and refused["exit"] != 0 and
            "is proposed; human authorization is required" in refused["stderr"],
            "proposed Work was admitted before authorization")
    require(current["id"] == original["id"] and current["authorization"] ==
            {"state": "execution-ready", "authority": "human"} and not current["attempts"],
            "queue admission lacks current human authority")
    require(all(current[key] == original[key] for key in
                ("origin", "corrective_context", "corrective_audit", "pending_enqueue")) and
            current["corrective_context"]["included_scope"] and current["corrective_context"]["excluded_scope"] and
            current["lineage"]["root_id"] == original["lineage"]["root_id"] and current["lineage"]["charged"] is True,
            "authorization changed proposed scope or origin")
    queue = authorized["queue"]
    require(queue["work_item_id"] == current["id"] and len(queue["dispatches"]) == 1,
            "authorization duplicated queue admission")
    dispatch = queue["dispatches"][0]
    require(dispatch["work_item_id"] == current["id"] and dispatch["status"] == "queued" and
            dispatch["generation"] == 0 and not dispatch.get("bound_attempt_id") and
            dispatch["origin_operation_id"] == current["pending_enqueue"]["origin_operation_id"] and
            dispatch["priority"] == current["pending_enqueue"]["priority"], "queue lost authorized enqueue intent")
    require(authorized == repeated, "repeated authorization changed retained identity")
    return {"work": current["id"], "states": ["intake", "intake", "authorized", "authorized"],
            "dispatch": dispatch["dispatch_id"], "scope": current["corrective_context"]["included_scope"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"authorization projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Check exact landing intent and current cancellation authority before Git."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

BOUNDARIES = ("ready", "intent", "canceled", "refused")
OPERATIONS = ("attempt-run", "land-interrupted", "attempt-cancel", "land-retry")


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "approval"
            and trace["model-profile"] == "approval-connection"
            and trace["collection-complete"] is True, "incomplete approval trace")
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
        require(artifact["path"] == f"approval-{index}.json", "invalid authority path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "authority digest differs")
        require(load_json(raw) == event["raw-facts"], "authority observation differs")
        facts.append(event["raw-facts"])
    works = [f["work"] for f in facts]
    candidates = [w["merge_candidates"][0] for w in works]
    attempts = [w["attempts"][0] for w in works]
    require(all(w["id"] == works[0]["id"] for w in works) and
            all(c["id"] == candidates[0]["id"] and c["attempt_id"] == attempts[0]["id"] for c in candidates),
            "approval changed candidate identity")
    require(candidates[0].get("ready_at") and candidates[0]["merge_state"]["status"] == "pending" and
            "injected interruption before landing Git effect" in facts[1]["stderr"],
            "missing pre-effect approval boundary")
    intent = candidates[1]["merge_state"]["landing_intent"]
    require(intent["work_item_id"] == works[0]["id"] and intent["attempt_id"] == attempts[0]["id"] and
            intent["merge_candidate_id"] == candidates[0]["id"] and
            intent["verified_commit"] == candidates[1]["candidate_commit"] and
            intent["target_before"] == facts[0]["head"] and
            intent["operation_id"] == f'landing:{works[0]["id"]}:{candidates[0]["id"]}:{intent["verified_commit"]}' and
            intent["authorization_digest"] and intent["accepted_model_digest"], "approval intent scope differs")
    require(all(f["head"] == facts[0]["head"] for f in facts) and
            all(c["merge_state"]["status"] == "landing-pending" and not c["merge_state"].get("merged_commit")
                and c["merge_state"].get("landing_intent") == intent for c in candidates[1:]),
            "canceled landing applied an effect")
    cancellation = attempts[2]["cancellation"]
    require(cancellation == attempts[3]["cancellation"] and cancellation.get("requested_at") and
            not cancellation.get("resumed_at") and not attempts[1].get("cancellation") and
            type(facts[3]["exit"]) is int and facts[3]["exit"] != 0 and
            facts[3]["stderr"] == "Error: Attempt has a durable cancellation request; explicitly resume after settlement\n",
            "landing retry ignored current cancellation")
    return {"candidate": intent["verified_commit"], "operation": intent["operation_id"],
            "states": ["verified", "intent", "canceled", "canceled"], "effects": 0}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"approval projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

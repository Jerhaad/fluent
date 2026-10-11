#!/usr/bin/env python3
"""Check retained landing intent, replay, and follow-up publication."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

BOUNDARIES = ("ready", "git-effect", "follow-up-pending", "replayed", "independent", "settled")
OPERATIONS = ("attempt-run", "land-interrupted", "land-obstructed", "land-retry", "independent-write", "land-retry")


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "landing-recovery"
            and trace["model-profile"] == "landing-connection"
            and trace["collection-complete"] is True, "incomplete landing trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]), "invalid candidate")
    require(type(trace["events"]) is list and len(trace["events"]) == 6 and
            type(trace["artifacts"]) is list and len(trace["artifacts"]) == 6,
            "missing landing observations")
    facts = []
    for index, (event, artifact) in enumerate(zip(trace["events"], trace["artifacts"])):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "operator" and
                event["generation"] is None and event["boundary"] == BOUNDARIES[index] and
                event["operation-id"] == OPERATIONS[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]),
                "ambiguous landing order")
        exact_keys(artifact, ("path", "sha256"), "artifact")
        require(artifact["path"] == f"landing-{index}.json", "invalid landing path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "landing digest differs")
        require(load_json(raw) == event["raw-facts"], "landing observation differs")
        facts.append(event["raw-facts"])
    works = [f["work"] for f in facts]
    require(all(w["id"] == works[0]["id"] and len(w["merge_candidates"]) == 1 for w in works),
            "landing changed Work identity")
    candidates = [w["merge_candidates"][0] for w in works]
    require(all(c["id"] == candidates[0]["id"] and c["attempt_id"] == candidates[0]["attempt_id"]
                for c in candidates), "landing changed candidate identity")
    states = [c["merge_state"] for c in candidates]
    require(states[0]["status"] == "pending" and candidates[0].get("ready_at") and
            states[1]["status"] == "landing-pending" and not states[1].get("merged_commit"),
            "missing interrupted result boundary")
    intent = states[1]["landing_intent"]
    require(intent["schema_version"] == 1 and intent["work_item_id"] == works[0]["id"] and
            intent["attempt_id"] == candidates[0]["attempt_id"] and
            intent["merge_candidate_id"] == candidates[0]["id"] and
            intent["verified_commit"] == facts[1]["head"] == candidates[1]["candidate_commit"] and
            intent["target_before"] == facts[0]["head"] != facts[1]["head"] and
            intent["target_branch"] == candidates[1]["target_branch"] and
            candidates[1]["target_workspace"]["path"] == "." and
            Path(intent["repository"]).parent == Path(intent["target_workspace"]) and
            intent["operation_id"] == f'landing:{works[0]["id"]}:{candidates[0]["id"]}:{facts[1]["head"]}',
            "Git effect lacks exact retained intent")
    require(all(re.fullmatch(r"[0-9a-f]{64}", intent[key]) for key in
                ("authorization_digest", "evidence_digest", "accepted_model_digest")) and
            intent["check_artifacts"], "intent lacks verification and approval binding")
    require(all(state.get("landing_intent") == intent for state in states[1:]),
            "retry changed landing authority")
    require(all(state["status"] == "merged" and state["merged_commit"] == facts[1]["head"]
                for state in states[2:]) and facts[2]["head"] == facts[3]["head"] == facts[1]["head"],
            "replay applied another Git effect")
    require(facts[2] == facts[3] and all(states[i]["follow_up_failure"]["stage"] == "observation"
                for i in (2,3,4)) and facts[2]["obstruction"] == "block publication\n",
            "failed publication lost its recoverable boundary")
    require(facts[4]["head"] != facts[1]["head"] and facts[5]["head"] == facts[4]["head"] and
            facts[4]["independent"] == facts[5]["independent"] and
            facts[4]["independent"]["id"] != works[0]["id"] and
            facts[4]["file"] == facts[5]["file"] == "later work\n", "replay overwrote independent state")
    observations = facts[5]["observations"]
    require(not states[5].get("follow_up_failure") and len(observations) == 1 and
            f'work-item-id: {works[0]["id"]}\n' in observations[0]["content"] and
            f'merge-candidate-id: {candidates[0]["id"]}\n' in observations[0]["content"] and
            f'merged-commit: {facts[1]["head"]}\n' in observations[0]["content"],
            "follow-up publication lacks retained origin")
    return {"work": works[0]["id"], "intent": intent["operation_id"],
            "effect": facts[1]["head"], "independent-head": facts[4]["head"],
            "states": list(BOUNDARIES)}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"landing projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

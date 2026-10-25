#!/usr/bin/env python3
"""Check cleanup retention and read-only status across pending publication."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

BOUNDARIES = ("pending", "cleanup-refused", "settled")
OPERATIONS = ("observe", "cleanup", "land-retry")


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "cleanup"
            and trace["model-profile"] == "cleanup-connection"
            and trace["collection-complete"] is True, "incomplete cleanup trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]), "invalid candidate")
    require(type(trace["events"]) is list and len(trace["events"]) == 3 and
            type(trace["artifacts"]) is list and len(trace["artifacts"]) == 3,
            "missing cleanup observations")
    facts = []
    for index, (event, artifact) in enumerate(zip(trace["events"], trace["artifacts"])):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "operator" and
                event["generation"] is None and event["boundary"] == BOUNDARIES[index] and
                event["operation-id"] == OPERATIONS[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]),
                "ambiguous cleanup order")
        exact_keys(artifact, ("path", "sha256"), "artifact")
        require(artifact["path"] == f"cleanup-{index}.json", "invalid cleanup path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "cleanup digest differs")
        require(load_json(raw) == event["raw-facts"], "cleanup observation differs")
        facts.append(event["raw-facts"])
    before, retained, settled = facts
    require(before["work"] == retained["work"] and before["retained"] == retained["retained"]
            and retained["artifacts-present"] is True, "cleanup removed pending recovery state")
    require(type(before["retained"]) is dict and before["retained"] and
            any("/artifacts/" in path for path in before["retained"]), "missing retained evidence")
    pending = before["work"]["merge_candidates"][0]
    final = settled["work"]["merge_candidates"][0]
    require(pending["merge_state"]["status"] == final["merge_state"]["status"] == "merged" and
            pending["merge_state"]["follow_up_failure"]["stage"] == "observation" and
            not final["merge_state"].get("follow_up_failure") and
            pending["merge_state"]["landing_intent"] == final["merge_state"]["landing_intent"] and
            pending["merge_state"]["merged_commit"] == final["merge_state"]["merged_commit"],
            "cleanup retry lost landing identity")
    for fact in (before, settled):
        work = fact["work"]
        shown = fact["shown"]
        require(shown["id"] == work["id"] and shown["attempts"] == work["attempts"] and
                shown["merge_candidates"] == work["merge_candidates"] and
                shown["evidence-history"]["merged-commit"] == final["merge_state"]["merged_commit"] and
                any("[merged]" in line for line in fact["status"]),
                "status differs from retained state")
    require(len(settled["observations"]) == 1 and
            f'merged-commit: {final["merge_state"]["merged_commit"]}\n' in settled["observations"][0],
            "cleanup recovery did not publish retained origin")
    return {"states": ["follow-up-pending", "follow-up-pending", "follow-up"],
            "observed-merge": "merged", "work": before["work"]["id"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"cleanup projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

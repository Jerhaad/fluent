#!/usr/bin/env python3
"""Check Learner retry mode, reviewed identity, and durable handoff."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

BOUNDARIES = ("failed", "retried")
OPERATIONS = ("attempt-run", "attempt-run")


def project(trace, root):
    name = trace["scenario"]
    require(name in ("learning_capture", "learning_no_expertise"), "invalid learning scenario")
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == name
            and trace["model-profile"] == name + "-connection"
            and trace["collection-complete"] is True, "incomplete learning trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]), "invalid candidate")
    require(type(trace["events"]) is list and len(trace["events"]) == 2 and
            type(trace["artifacts"]) is list and len(trace["artifacts"]) == 2,
            "missing learning observations")
    facts = []
    for index, (event, artifact) in enumerate(zip(trace["events"], trace["artifacts"])):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "operator" and
                event["generation"] is None and event["boundary"] == BOUNDARIES[index] and
                event["operation-id"] == OPERATIONS[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]),
                "ambiguous learning order")
        exact_keys(artifact, ("path", "sha256"), "artifact")
        require(artifact["path"] == f"{name}-{index}.json", "invalid learning path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "learning digest differs")
        require(load_json(raw) == event["raw-facts"], "learning observation differs")
        facts.append(event["raw-facts"])
    before, after = facts
    original, continued = before["work"], after["work"]
    mode = "capture" if name == "learning_capture" else "no-expertise"
    require(original["id"] == continued["id"] and original["learner_mode"] == continued["learner_mode"] == mode,
            "retry changed learner mode or Work identity")
    old, new = original["attempts"][0], continued["attempts"][0]
    require(old["id"] == new["id"] and old["learning"]["status"] == "failed" and
            old["learning"]["runs"] == 1 and new["learning"]["runs"] == 2,
            "retry did not retain the learning record")
    require([t["id"] for t in old["tasks"]] == [t["id"] for t in new["tasks"]] and
            all(t["status"] == "complete" for t in new["tasks"]), "retry reran completed Tasks")
    require([t for t in old["tasks"] if t["kind"] in ("review", "tester")] ==
            [t for t in new["tasks"] if t["kind"] in ("review", "tester")], "retry changed reviewed evidence")
    a, b = original["merge_candidates"][0], continued["merge_candidates"][0]
    require(a["id"] == b["id"] and a["attempt_id"] == b["attempt_id"] == new["id"] and
            after["head"] == b["candidate_commit"] and not a.get("ready_at"), "candidate binding differs")
    if mode == "capture":
        require(new["learning"]["status"] == "succeeded" and b.get("ready_at") and
                after["changed"].splitlines() == [".fluent/expertise/retry.md"] and
                a["candidate_commit"] != b["candidate_commit"], "capture did not retain expertise change")
    else:
        require(a["candidate_commit"] == b["candidate_commit"] and after["changed"] == "" and
                old["tasks"] == new["tasks"], "no-expertise retry changed reviewed candidate")
        require(after["confinement"] in ("available", "unavailable"), "missing confinement observation")
        if after["confinement"] == "unavailable":
            require(new["learning"]["status"] == "failed" and not b.get("ready_at") and
                    after["handoff"] is None and after["handoff-bytes"] is None,
                    "unsupported confinement received readiness credit")
        else:
            require(new["learning"]["status"] == "succeeded" and b.get("ready_at"),
                    "confined retry did not complete")
    if new["learning"]["status"] == "succeeded":
        handoff = after["handoff"]
        require(load_json(after["handoff-bytes"]) == handoff and
                "sha256:" + hashlib.sha256(after["handoff-bytes"].encode()).hexdigest() == new["learning"]["handoff"]["digest"] and
                handoff["source_work_item_id"] == original["id"] and handoff["source_attempt_id"] == new["id"] and
                handoff["source_merge_candidate_id"] == b["id"], "handoff differs from retained learning")
        if mode == "capture":
            require([e["path"] for e in handoff["learning"]["expertise"]] == [".fluent/expertise/retry.md"],
                    "handoff lost expertise origin")
    return {"mode": mode, "outcome": new["learning"]["status"],
            "confinement": after["confinement"], "states": ["tested", "ready" if b.get("ready_at") else "tested"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"learning projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

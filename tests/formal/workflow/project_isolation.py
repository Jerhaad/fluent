#!/usr/bin/env python3
"""Check project-local failures without granting worker execution evidence."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

BOUNDARIES = ("configuration", "corrupt-work", "coordinator")
OPERATIONS = ("poll-projects",) * 3


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "project-isolation"
            and trace["model-profile"] == "isolation-connection"
            and trace["collection-complete"] is True, "incomplete isolation trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]), "invalid candidate")
    require(type(trace["events"]) is list and len(trace["events"]) == 3 and
            type(trace["artifacts"]) is list and len(trace["artifacts"]) == 3,
            "missing isolation observations")
    facts = []
    for index, (event, artifact) in enumerate(zip(trace["events"], trace["artifacts"])):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "service" and
                event["generation"] is None and event["boundary"] == BOUNDARIES[index] and
                event["operation-id"] == OPERATIONS[index] and
                event["causal-predecessors"] == [],
                "ambiguous isolation order")
        exact_keys(artifact, ("path", "sha256"), "artifact")
        require(artifact["path"] == f"isolation-{index}.json", "invalid isolation path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "isolation digest differs")
        require(load_json(raw) == event["raw-facts"], "isolation observation differs")
        facts.append(event["raw-facts"])
    for index, fact in enumerate(facts):
        require(fact["fault"] == BOUNDARIES[index], "wrong fault boundary")
        before = fact["before"]
        require(fact["registry"] == before["registry"] and len(fact["registry"]) == 2 and
                len(set(fact["registry"].values())) == 2, "project registration changed")
        bad_path = next(path for path in fact["registry"] if path.endswith("/00-bad"))
        require(any(path.endswith("/10-good") for path in fact["registry"]), "healthy project missing")
        good, bad = fact["good"], fact["bad"]
        require(before["good"]["status"] == before["bad"]["status"] == "queued" and
                good["work_item_id"] == before["good"]["work_item_id"] == "wi-good" and
                bad["work_item_id"] == before["bad"]["work_item_id"] == "wi-bad" and
                good["dispatch_id"] == before["good"]["dispatch_id"] and
                bad["dispatch_id"] == before["bad"]["dispatch_id"], "poll changed dispatch identity")
        require(fact["calls"] == ["wi-good"] and good["status"] == "needs-user" and
                good["generation"] > before["good"]["generation"], "healthy project was not dispatched")
        work = fact["good-work"]
        require(work["id"] == good["work_item_id"] and len(work["attempts"]) == 1 and
                work["attempts"][0]["id"] == good["bound_attempt_id"] and
                work["attempts"][0]["status"] == "needs-user", "healthy Attempt binding differs")
        require(not bad.get("bound_attempt_id"), "unreadable project acquired an Attempt")
        if fact["fault"] == "corrupt-work":
            require(fact["bad-work"] == "invalid json" and bad["status"] == "blocked",
                    "corrupt project was rewritten or dropped")
        else:
            require(bad == before["bad"] and any(bad_path in line for line in fact["diagnostics"]),
                    "project-local failure was lost")
    return {"faults": list(BOUNDARIES), "states": ["authorized", "healthy-dispatched"],
            "external-runner": "scripted-needs-user"}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"isolation projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

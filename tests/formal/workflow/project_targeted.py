#!/usr/bin/env python3
"""Check targeted review against exact canonical command and candidate identity."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

BOUNDARIES = ("failed-check", "attached", "ready")
OPERATIONS = ("task-run", "evidence-attach", "attempt-run")


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "targeted"
            and trace["model-profile"] == "targeted-connection"
            and trace["collection-complete"] is True, "incomplete targeted trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]), "invalid candidate")
    require(type(trace["events"]) is list and len(trace["events"]) == 3 and
            type(trace["artifacts"]) is list and len(trace["artifacts"]) == 3,
            "missing targeted observations")
    facts = []
    for index, (event, artifact) in enumerate(zip(trace["events"], trace["artifacts"])):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "operator" and
                event["generation"] is None and event["boundary"] == BOUNDARIES[index] and
                event["operation-id"] == OPERATIONS[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]),
                "ambiguous targeted order")
        exact_keys(artifact, ("path", "sha256"), "artifact")
        require(artifact["path"] == f"targeted-{index}.json", "invalid targeted path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "targeted digest differs")
        require(load_json(raw) == event["raw-facts"], "targeted observation differs")
        facts.append(event["raw-facts"])
    failed, attached, ready = facts
    works = [f["work"] for f in facts]
    attempts = [w["attempts"][0] for w in works]
    require(all(w["id"] == works[0]["id"] for w in works) and
            all(a["id"] == attempts[0]["id"] for a in attempts), "review changed Work or Attempt identity")
    original = attempts[0]["tasks"]
    for attempt in attempts[1:]:
        require(all(next(t for t in attempt["tasks"] if t["id"] == prior["id"]) == prior for prior in original),
                "targeted review replaced ordinary evidence")
    require(failed["results"] == ready["results"] and ready["passing-runs"] == "pass\n",
            "targeted review reran or rewrote canonical results")
    results = load_json(failed["results"])
    candidate = results["candidate_commit"]
    require(results["error"] is None and len(results["commands"]) == 2 and
            results["commands"][0]["exit_code"] == 0 and results["commands"][1]["exit_code"] != 0 and
            results["commands"][1]["command"] == "false", "original canonical outcome differs")
    pending = attempts[1]["evidence_recoveries"][0]
    complete = attempts[2]["evidence_recoveries"][0]
    require(len(attempts[1]["evidence_recoveries"]) == len(attempts[2]["evidence_recoveries"]) == 1 and
            pending["state"] == "reviewing" and complete["state"] == "complete", "targeted credit lacks completed review")
    check = pending["required_check"]
    selector = "tester:" + hashlib.sha256(failed["config"].encode()).hexdigest() + ":1"
    require(attached["selector"] == check["id"] == selector and check == complete["required_check"] and
            check["candidate_commit"] == pending["candidate_commit"] == complete["candidate_commit"] == candidate and
            check["work_item_id"] == works[0]["id"] and check["attempt_id"] == attempts[0]["id"] and
            check["source"]["command_index"] == 1 and check["source"]["kind"] == "canonical-tester" and
            check["command"] == "false" and check["working_directory"] == "." and
            check["source"]["results"]["digest"] == "sha256:" + hashlib.sha256(failed["results"].encode()).hexdigest(),
            "targeted credit escaped exact command or candidate")
    proof = load_json(attached["proof"])
    require(proof["result"] == "pass" and proof["check"] == check["command"] and
            proof["working_directory"] == check["working_directory"] and
            pending["attachment"] == complete["attachment"] and
            pending["attachment"]["digest"] == "sha256:" + hashlib.sha256(attached["proof"].encode()).hexdigest(),
            "replacement proof binding differs")
    targets = [t for t in attempts[2]["tasks"] if t.get("evidence_review_context")]
    require(len(targets) == 1 and len(complete["targets"]) == 1 and targets[0]["role"] == "tests" and
            targets[0]["status"] == "complete" and targets[0]["evidence_review_context"]["candidate_commit"] == candidate and
            targets[0]["evidence_review_context"]["attachment"] == complete["attachment"] and
            complete["targets"][0]["review_task_id"] == targets[0]["id"] and
            complete["targets"][0]["review_digest"] == "sha256:" + hashlib.sha256(ready["review"].encode()).hexdigest() and
            ready["review"].startswith("Verdict: pass\n"), "targeted review lacks scoped verdict")
    candidate_record = works[2]["merge_candidates"][0]
    require(candidate_record.get("ready_at") and candidate_record["candidate_commit"] == candidate == ready["head"],
            "ready candidate differs from reviewed check")
    return {"candidate": candidate, "required-check": selector,
            "states": ["reviewed", "reviewed", "tested"], "scope": "tests"}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"targeted projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

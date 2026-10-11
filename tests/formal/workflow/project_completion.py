#!/usr/bin/env python3
"""Check interrupted context-retry publication against retained raw Work files."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

BOUNDARIES = ("audit", "journal", "children", "attempt", "task")
OPERATIONS = ("repair-writer-completion",) * 5


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "completion"
            and trace["model-profile"] == "completion-connection"
            and trace["collection-complete"] is True, "incomplete completion trace")
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
        require(artifact["path"] == f"completion-{index}.json", "invalid authority path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "authority digest differs")
        require(load_json(raw) == event["raw-facts"], "authority observation differs")
        facts.append(event["raw-facts"])
    for index, fact in enumerate(facts):
        require(fact["interrupted-exit"] == 86, "publication interruption was not observed")
        original, recovered = fact["original"], fact["recovered"]
        require(recovered == fact["repeated"] and original["id"] == recovered["id"],
                "publication replay changed retained identity")
        old, new = original["attempts"][0], recovered["attempts"][0]
        require(old["id"] == new["id"] and [t["id"] for t in old["tasks"]] == [t["id"] for t in new["tasks"]],
                "publication created another Writer")
        source = next(t for t in old["tasks"] if t["id"] == "attempt-1-write-5")
        retry = next(t for t in new["tasks"] if t["id"] == source["id"])
        require(source["status"] == "failed" and retry.get("status", "planned") == "planned" and
                not source.get("output") and not retry.get("output"), "retry credited output-free completion")
        for key in ("id", "work_item_id", "attempt_id", "instructions", "workspace_access", "writer_correction_assignment"):
            require(source[key] == retry[key], "retry replaced the assigned Writer context")
        for prior in old["tasks"]:
            if prior["id"] not in (source["id"], "attempt-1-write-6"):
                require(next(t for t in new["tasks"] if t["id"] == prior["id"]) == prior,
                        "publication rewrote completed history")
        successor = next(t for t in new["tasks"] if t["id"] == "attempt-1-write-6")
        require(successor["status"] == "superseded" and new["status"] == "needs-user" and
                new["pause_kind"] == "writer-completion", "retry left competing Writer authority")
        operations = [op for op in new["recovery_operations"] if op["operation-kind"] == "writer-completion-context-retry"]
        require(len(operations) == 1, "publication duplicated the recovery operation")
        operation = operations[0]
        require(operation["work-item-id"] == original["id"] and operation["attempt-id"] == old["id"] and
                operation["task-id"] == source["id"] and operation["result"] == "applied" and
                operation["candidate-commit"] == source["writer_correction_assignment"]["source_candidate_commit"] and
                operation["authorization-digest"] == "sha256:" + hashlib.sha256(operation["authorization"].encode()).hexdigest(),
                "publication lost exact recovery authority")
        trees = []
        for key in ("interrupted", "published"):
            entries = fact[key]
            require(len({entry["path"] for entry in entries}) == len(entries), "duplicate retained path")
            trees.append({e["path"]: bytes(e["bytes"]) for e in entries if e["kind"] == "file"})
        interrupted, published = trees
        journal = f'transactions/{original["id"]}.json'
        require(journal not in published and (journal in interrupted) == (index > 0),
                "publication lost its recoverable journal boundary")
        if index > 0:
            pending = load_json(interrupted[journal])
            require(pending["id"] == original["id"] and
                    pending["context_retry_publications"] == [operation["operation-id"]],
                    "journal names another recovery operation")
        audit = f'artifacts/{original["id"]}/{old["id"]}/writer-completion-context-retries/{source["id"]}/recovery.json'
        require(audit in interrupted and audit in published and interrupted[audit] == published[audit],
                "publication replaced its retained audit")
        saved = load_json(published[audit])
        require(saved["before"] == old and saved["task_id"] == source["id"] and
                saved["resolution"] == operation["authorization"], "audit lost the source history")
    return {"prefixes": list(BOUNDARIES), "states": ["interrupted", "authorized"],
            "retry-task": "attempt-1-write-5", "new-task-count": 0}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"completion projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

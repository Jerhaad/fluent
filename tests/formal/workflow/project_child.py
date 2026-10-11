#!/usr/bin/env python3
"""Check child-owned receipt identity through parent loss and public cancellation."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "child-launch-stop"
            and trace["model-profile"] == "child-connection"
            and trace["collection-complete"] is True, "incomplete child trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]), "invalid candidate")
    require(type(trace["events"]) is list and len(trace["events"]) == 4 and
            type(trace["artifacts"]) is list and len(trace["artifacts"]) == 4,
            "missing child observations")
    facts = []
    for index, (event, artifact) in enumerate(zip(trace["events"], trace["artifacts"])):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "operator" and
                event["generation"] is None and
                event["boundary"] == ("receipt", "parent-crash", "execution", "settled")[index] and
                event["operation-id"] == ("task-run", "kill-parent", "release-exec", "attempt-cancel")[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]), "ambiguous child order")
        exact_keys(artifact, ("path", "sha256"), "artifact")
        require(artifact["path"] == f"child-{index}.json", "invalid child path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "child digest differs")
        require(load_json(raw) == event["raw-facts"], "child observation differs")
        facts.append(event["raw-facts"])
    receipt = facts[0]["receipt"]
    require(type(receipt) is str and re.fullmatch(r"2 [0-9a-f]{32} [0-9]{10}\n", receipt),
            "missing or malformed child receipt")
    _, generation, pid = receipt.split()
    pid = int(pid)
    require(pid > 0 and facts[1]["receipt"] == receipt, "child receipt changed after parent crash")
    record = facts[0]["record"]
    owner = record["attempt_owner"]
    for fact in facts:
        current = fact["record"]
        require(current["schema_version"] == 2 and current["launch_generation"] == generation,
                "receipt generation does not bind execution")
        require(current["attempt_owner"] == owner and
                current["owner_pid"] == facts[0]["owner-pid"], "execution owner changed")
    require(facts[1]["parent-reaped"] is True and
            facts[1]["record"] == record and record["state"] == "launching" and
            record["leader_pid"] == record["process_group_id"] == 0,
            "parent loss did not precede PID publication")
    require(facts[2]["payload-pid"] == pid and facts[2]["lease-live"] is True,
            "execution lacks the receipted child or held ownership")
    for index in (2, 3):
        work = facts[index]["work"]
        require(work["id"] == owner["work_item_id"] and len(work["attempts"]) == 1 and
                work["attempts"][0]["id"] == owner["attempt_id"], "receipt has the wrong Work owner")
        tasks = work["attempts"][0]["tasks"]
        require(len(tasks) == 1 and tasks[0]["work_item_id"] == work["id"] and
                tasks[0]["attempt_id"] == owner["attempt_id"] and tasks[0]["kind"] == "write",
                "receipt has the wrong Task owner")
    before = facts[2]["work"]["attempts"][0]
    after = facts[3]["work"]["attempts"][0]
    require(before["tasks"][0]["id"] == after["tasks"][0]["id"] and
            before["tasks"][0]["status"] == "executing" and
            after["tasks"][0]["status"] == "needs-user", "cancellation changed Task identity")
    require(facts[3]["exit"] == 0 and facts[3]["lease-live"] is False and
            facts[3]["process-gone"] is True and facts[3]["record"]["state"] == "canceled" and
            type(after["cancellation"]["requested_at"]) is str and
            type(after["cancellation"]["settled_at"]) is str and
            after["cancellation"].get("resumed_at") is None, "cancellation did not settle owned execution")
    return {"work": owner["work_item_id"], "attempt": owner["attempt_id"],
            "task": before["tasks"][0]["id"], "child": pid, "launch-generation": generation,
            "states": ["receipted", "parent-crashed", "executing", "stopped-and-settled"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"child projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

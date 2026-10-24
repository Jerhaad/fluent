#!/usr/bin/env python3
"""Project public recovery of a retained no-change Writer with pending Plan work."""
import copy
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, require

SNAPSHOTS = ("pending.json", "authorized.json", "executed.json", "replayed.json")
EXTRAS = ("source-matrix.json", "contract.json", "source-output.json",
          "source-assignment.json", "source-transcript.jsonl", "authority.json",
          "context.json", "provider-context.json", "successor-matrix.json")
ACTORS = ("work-show", "repair-writer-completion", "attempt-run", "repair-writer-completion")
OPERATIONS = ("observe-pending", "authorize-successor", "execute-successor", "replay-authorization")
BOUNDARIES = ("pending", "authorized", "executed", "replayed")


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def project(trace, directory):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and
            trace["scenario"] == "integrated-no-change-writer-plan" and
            trace["model-profile"] == "writer-plan-recovery" and
            trace["collection-complete"] is True, "incomplete Writer Plan trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]), "invalid candidate binding")
    artifacts = trace["artifacts"]
    require(len(artifacts) == len(SNAPSHOTS + EXTRAS), "missing Writer Plan artifacts")
    raw = {}
    for entry, name in zip(artifacts, SNAPSHOTS + EXTRAS):
        exact_keys(entry, ("path", "sha256"), "artifact")
        require(entry["path"] == name, "unexpected artifact path or order")
        raw[name] = (directory / name).read_bytes()
        require(hashlib.sha256(raw[name]).hexdigest() == entry["sha256"], "artifact digest differs")
    data = {name: json.loads(value) for name, value in raw.items() if name.endswith(".json")}
    works = [data[name] for name in SNAPSHOTS]
    require(len(trace["events"]) == 4, "missing or extra events")
    for index, event in enumerate(trace["events"]):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation", "boundary",
                           "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == ACTORS[index] and
                event["operation-id"] == OPERATIONS[index] and event["generation"] is None and
                event["boundary"] == BOUNDARIES[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]) and
                event["raw-facts"] == {"work": works[index]}, "event differs from retained Work")
    require(len({w["id"] for w in works}) == 1 and all(len(w["attempts"]) == 1 for w in works),
            "Work or Attempt identity changed")
    attempts = [w["attempts"][0] for w in works]
    require(len({a["id"] for a in attempts}) == 1, "Attempt identity changed")
    require([len(a["tasks"]) for a in attempts] == [1, 2, 2, 2], "expected one authorized successor")
    source = attempts[0]["tasks"][0]
    require(source["kind"] == "write" and source["status"] == "complete" and
            source["output"]["no_change"] and
            source["output"]["base_commit"] == source["output"]["commit"] and
            all(a["tasks"][0] == source for a in attempts), "completed no-change history changed")
    candidate = source["output"]["commit"]
    planned = attempts[1]["tasks"][1]
    executed = attempts[2]["tasks"][1]
    require(planned["kind"] == executed["kind"] == "write" and
            planned["id"] == executed["id"] and planned.get("status", "planned") == "planned" and
            planned.get("output") is None and executed["status"] == "complete" and
            executed["output"]["base_commit"] == candidate and
            executed["output"]["commit"] != candidate and
            attempts[3]["tasks"] == attempts[2]["tasks"], "successor did not execute exactly once")
    contract = data["contract.json"]
    require(all(a["writer_completion_contract"] == contract for a in attempts),
            "requirement contract changed")
    require(data["source-matrix.json"]["task-id"] == source["id"],
            "source matrix belongs to another Writer")
    rows = data["source-matrix.json"]["rows"]
    require([row["id"] for row in rows] == [row["id"] for row in contract["requirements"]],
            "source matrix replaced contract requirements")
    pending = [row["id"] for row in rows if row["kind"] == "plan" and
               row.get("responsibility", "writer") == "writer" and row["state"] == "pending"]
    require(pending == ["plan:step-1"] and
            any(row["id"] == pending[0] and row["responsibility"] == "writer"
                for row in contract["requirements"]), "pending Writer Plan identity disappeared")
    context = data["context.json"]
    require(context == data["provider-context.json"] and
            context["candidate-commit"] == candidate and
            any(row["id"] == pending[0] and row["state"] == "pending"
                for row in context["pending-writer-rows"]) and
            not any(row["id"] == "plan:step-2" for row in context["pending-writer-rows"]),
            "provider lost pending Writer Plan input")
    final = data["successor-matrix.json"]
    require(final["task-id"] == executed["id"] and
            [row["id"] for row in final["rows"]] == [row["id"] for row in rows],
            "successor replaced requirements")
    for old, new in zip(rows, final["rows"]):
        if old.get("responsibility", "writer") == "writer":
            require(new["state"] == "complete" and new["implementation-evidence"] and
                    new["verification"], "successor did not complete Writer requirements")
        else:
            require(old == new and new["state"] == "pending" and
                    not new["implementation-evidence"] and not new["verification"],
                    "successor credited external requirements")
    authority = data["authority.json"]
    require(authority["work-item-id"] == works[0]["id"] and
            authority["attempt-id"] == attempts[0]["id"] and
            authority["source-writer-task-id"] == source["id"] and
            authority["task-id"] == planned["id"] and
            authority["candidate-commit"] == candidate and
            authority["resolution"] and
            authority["resolution-sha256"] == digest(authority["resolution"].encode()),
            "recovery authority differs from current successor")
    audit_path = (f'.fluent/work/artifacts/{works[0]["id"]}/{attempts[0]["id"]}/'
                  f'writer-completion-recoveries/{planned["id"]}/recovery.json')
    require(all(any(ref["producer_id"] == "writer-completion-recovery" and
                        ref["path"] == audit_path for ref in task["input_artifacts"])
                for task in (planned, executed)), "successor lost its authority input")
    for field, name in (("source-matrix-sha256", "source-matrix.json"),
                        ("requirement-contract-sha256", "contract.json"),
                        ("source-output-sha256", "source-output.json"),
                        ("source-assignment-sha256", "source-assignment.json"),
                        ("source-transcript-sha256", "source-transcript.jsonl")):
        require(authority[field] == digest(raw[name]), "recovery source binding differs")
    require(data["source-output.json"] == source["output"] and
            data["source-assignment.json"] == [source["workspace_access"], source.get("writer_correction_assignment")],
            "retained source evidence differs")
    operations = attempts[1]["recovery_operations"]
    require(len(operations) == len(attempts[0].get("recovery_operations", [])) + 1 and
            operations[:-1] == attempts[0].get("recovery_operations", []) and
            all(a["recovery_operations"] == operations for a in attempts[1:]),
            "recovery replay changed authorization history")
    operation = operations[-1]
    require(operation["work-item-id"] == works[0]["id"] and
            operation["attempt-id"] == attempts[0]["id"] and
            operation["candidate-commit"] == candidate and
            operation["task-id"] == source["id"] and
            operation["authorization"] == authority["resolution"] and
            operation["authorization-digest"] == authority["resolution-sha256"] and
            operation["result"] == "applied", "operation lost current authorization")
    require(all(not w["merge_candidates"] for w in works) and
            attempts[-1]["status"] == "needs-user", "pending host requirement did not gate readiness")
    return {"states": [{"work": w["id"], "attempt": a["id"], "candidate": candidate,
                        "source": source["id"], "successor": "none" if i == 0 else planned["id"],
                        "pending": i < 2, "authorized": i > 0, "executed": i > 1,
                        "outputCommit": candidate if i < 2 else executed["output"]["commit"],
                        "hostPending": True, "ready": bool(w["merge_candidates"])}
                       for i, (w, a) in enumerate(zip(works, attempts))]}


# Rehash each changed artifact and its event copy, so these controls test facts,
# not merely the trace envelope's digest protection.
CONTROLS = {
    "pending": ("context.json", "provider lost pending Writer Plan input"),
    "authority": ("authority.json", "recovery authority differs from current successor"),
    "history": ("executed.json", "completed no-change history changed"),
    "duplicate": ("authorized.json", "expected one authorized successor"),
    "execution": ("executed.json", "successor did not execute exactly once"),
}


def negative_trace(trace, directory, target, defect):
    target.mkdir(parents=True, exist_ok=True)
    changed = copy.deepcopy(trace)
    for entry in changed["artifacts"]:
        (target / entry["path"]).write_bytes((directory / entry["path"]).read_bytes())
    name, expected = CONTROLS[defect]
    value = json.loads((target / name).read_text())
    if defect == "pending":
        value["pending-writer-rows"] = []
    elif defect == "authority":
        value["task-id"] = "wrong-successor"
    elif defect == "history":
        value["attempts"][0]["tasks"][0]["output"]["commit"] = "0" * 40
    elif defect == "duplicate":
        value["attempts"][0]["tasks"].append(copy.deepcopy(value["attempts"][0]["tasks"][-1]))
    elif defect == "execution":
        value["attempts"][0]["tasks"][1]["status"] = "planned"
    names = [name, "provider-context.json"] if defect == "pending" else [name]
    for name in names:
        path = target / name
        path.write_text(json.dumps(value))
        next(e for e in changed["artifacts"] if e["path"] == name)["sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
        if name in SNAPSHOTS:
            changed["events"][SNAPSHOTS.index(name)]["raw-facts"]["work"] = value
    (target / "no-change-trace.json").write_text(json.dumps(changed))
    return changed, expected


def main():
    try:
        require(len(sys.argv) in (2, 3), "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        trace = json.loads(path.read_text())
        result = project(trace, path.parent)
        if len(sys.argv) == 3:
            require(sys.argv[2] == "--check-controls", "invalid option")
            for defect in CONTROLS:
                directory = path.parent / ("negative-" + defect)
                changed, expected = negative_trace(trace, path.parent, directory, defect)
                try:
                    project(changed, directory)
                except TraceError as error:
                    require(str(error) == expected, f"wrong {defect} rejection: {error}")
                else:
                    raise TraceError(f"accepted {defect} control")
        print(json.dumps(result, sort_keys=True))
    except (TraceError, OSError, ValueError, KeyError, IndexError, TypeError) as error:
        print(f"Writer Plan projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Project a completed Writer repair from retained public-command facts."""

import hashlib
import json
import re
import sys
from pathlib import Path


class TraceError(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise TraceError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key: {key}")
        result[key] = value
    return result


def read_json(path):
    return json.loads(path.read_text(), object_pairs_hook=unique_object)


def project(path):
    trace = read_json(path)
    require(set(trace) == {"schema-version", "scenario", "fluent-commit",
                           "fluent-binary-sha256", "model-profile",
                           "collection-complete", "events", "artifacts"},
            "trace envelope differs")
    require(trace["schema-version"] == 1 and
            trace["scenario"] == "malformed-completion-public-recovery" and
            trace["model-profile"] == "recovery-observation" and
            trace["collection-complete"] is True, "trace is incomplete or misidentified")
    for name in ("fluent-commit", "fluent-binary-sha256"):
        require(isinstance(trace[name], str) and
                re.fullmatch(r"[0-9a-f]{40,64}", trace[name]) is not None,
                f"invalid {name}")
    names = ("paused-work.json", "authorized-work.json", "continued-work.json",
             "source-matrix.json")
    artifacts = trace["artifacts"]
    require(isinstance(artifacts, list) and len(artifacts) == len(names),
            "missing recovery artifacts")
    for artifact, name in zip(artifacts, names):
        require(set(artifact) == {"path", "sha256"} and artifact["path"] == name,
                "artifact identity differs")
        data = (path.parent / name).read_bytes()
        require(hashlib.sha256(data).hexdigest() == artifact["sha256"],
                f"artifact digest differs: {name}")
    matrix = read_json(path.parent / "source-matrix.json")
    require(matrix.get("rows", [{}])[0].get("verification", [{}])[0].get("result") ==
            "pass; extra text", "malformed source matrix was not retained")
    events = trace["events"]
    require(isinstance(events, list) and len(events) == 3, "missing recovery events")
    stages = ("paused-observation", "repair", "resume")
    operations = ("work-item-show", "repair-writer-completion", "attempt-run")
    works = []
    for index, (event, stage, operation) in enumerate(zip(events, stages, operations)):
        require(set(event) == {"sequence", "actor", "operation-id", "generation",
                               "boundary", "causal-predecessors", "raw-facts"},
                "event fields differ")
        require(event["sequence"] == index + 1 and event["actor"] == "operator" and
                event["operation-id"] == operation and event["generation"] is None and
                event["boundary"] == stage and
                event["causal-predecessors"] == ([] if index == 0 else [index]),
                "recovery event order differs")
        facts = event["raw-facts"]
        require(set(facts) == {"work", "exit-code", "stdout", "stderr"} and
                type(facts["exit-code"]) is int and facts["exit-code"] == 0 and
                isinstance(facts["stdout"], str) and isinstance(facts["stderr"], str),
                "recovery command did not succeed")
        work = read_json(path.parent / names[index])
        require(facts["work"] == work, "raw Work differs from public snapshot")
        works.append(work)
    paused, authorized, continued = works
    work_id = paused.get("id")
    require(isinstance(work_id, str) and work_id and
            all(work.get("id") == work_id for work in works),
            "Work identity changed")
    before, repaired, after = [work["attempts"][0] for work in works]
    require(all(attempt["id"] == "attempt-1" for attempt in (before, repaired, after)),
            "Attempt identity changed")
    source = before["tasks"][0]
    require(before["status"] == "needs-user" and
            before.get("pause_kind") == "writer-completion" and
            len(before["tasks"]) == 1 and source["status"] == "complete" and
            "repair-writer-completion" in paused.get("next-action", ""),
            "malformed completion was not publicly recoverable")
    require(repaired["status"] == "planned" and len(repaired["tasks"]) == 2 and
            repaired["tasks"][0] == source and
            repaired["tasks"][1].get("status", "planned") == "planned" and
            len(repaired.get("recovery_operations", [])) == 1,
            "repair did not reserve exactly one correction")
    require(len(after["tasks"]) == 2 and after["tasks"][0] == source and
            after["tasks"][1]["id"] == repaired["tasks"][1]["id"] and
            after["tasks"][1]["status"] == "complete" and
            len(after.get("recovery_operations", [])) == 1 and
            "evidence attach" in continued.get("next-action", ""),
            "recovery did not execute the next permitted transition")
    correction = repaired["tasks"][1]
    operation = repaired["recovery_operations"][0]
    authority_path = (f".fluent/work/artifacts/{work_id}/attempt-1/"
                      f"writer-completion-recoveries/{correction['id']}/recovery.json")
    require(operation == after["recovery_operations"][0] and
            operation["schema-version"] == 1 and
            operation["operation-kind"] == "writer-completion-recovery" and
            operation["result"] == "applied" and
            operation["authorization-digest"] == "sha256:" +
            hashlib.sha256(operation["authorization"].encode()).hexdigest() and
            isinstance(operation["operation-id"], str) and
            operation["operation-id"].startswith("recovery-") and
            operation["work-item-id"] == work_id and
            operation["attempt-id"] == "attempt-1" and
            operation["task-id"] == source["id"] and
            operation["candidate-commit"] == source["output"]["commit"] and
            operation["candidate-commit"] == before["pause_record"]["candidate-commit"] and
            operation["workspace-path"] == source["output"]["workspace_path"] and
            correction["work_item_id"] == work_id and
            correction["attempt_id"] == "attempt-1" and
            correction["kind"] == "write" and
            correction["role"] == "author" and
            {(entry["path"], entry["producer_id"])
             for entry in correction["input_artifacts"]} ==
            {(authority_path, "writer-completion-recovery"),
             (source["artifact_area"]["path"] + "/writer-completion.json", source["id"])} and
            authority_path in correction["instructions"] and
            operation["authorization"] in correction["instructions"],
            "recovery authorization is not bound to source Work, Task, candidate, and authority")
    states = []
    for phase, work, task_status in (("paused", paused, "absent"),
                                     ("authorized", authorized, "planned"),
                                     ("continued", continued, "complete")):
        attempt = work["attempts"][0]
        states.append({"phase": phase, "work": work_id,
                       "attempt": attempt["id"], "source": source["id"],
                       "candidate": source["output"]["commit"],
                       "correction": correction["id"] if len(attempt["tasks"]) > 1 else "absent",
                       "correctionStatus": task_status,
                       "operation": operation["operation-id"] if attempt.get("recovery_operations") else "absent",
                       "authority": authority_path if attempt.get("recovery_operations") else "absent",
                       "published": phase == "continued"})
    return {"states": states,
            "work-id": work_id, "attempt-id": "attempt-1"}


def main():
    try:
        require(len(sys.argv) == 2, "expected one trace path")
        print(json.dumps(project(Path(sys.argv[1]).resolve()), sort_keys=True))
    except (TraceError, OSError, KeyError, IndexError, TypeError, ValueError) as error:
        print(f"recovery trace violation: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

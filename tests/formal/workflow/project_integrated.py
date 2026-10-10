#!/usr/bin/env python3
"""Project a public queued/canceled Work sequence from raw command facts."""

import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

ARTIFACTS = ("queued-work.json", "foreground-work.json", "queue.txt",
             "status.txt", "refused.txt")
BOUNDARIES = ("queue-admitted", "other-canceled", "observed", "run-refused")
OPERATIONS = ("queue-add", "attempt-cancel", "status", "task-run")


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and
            trace["scenario"] == "integrated-queue-cancel" and
            trace["model-profile"] == "service-competition",
            "wrong trace schema, scenario or profile")
    require(trace["collection-complete"] is True, "collection incomplete")
    require(type(trace["fluent-commit"]) is str and
            re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]),
            "invalid source commit")
    require(type(trace["fluent-binary-sha256"]) is str and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]),
            "invalid binary digest")
    artifacts = trace["artifacts"]
    require(type(artifacts) is list and len(artifacts) == len(ARTIFACTS),
            "missing artifacts")
    observed = {}
    for entry in artifacts:
        exact_keys(entry, ("path", "sha256"), "artifact")
        name = entry["path"]
        require(type(name) is str and name in ARTIFACTS and name not in observed,
                "invalid artifact path")
        raw = (root / name).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == entry["sha256"],
                f"artifact digest differs: {name}")
        observed[name] = raw
    require(set(observed) == set(ARTIFACTS), "missing artifacts")
    events = trace["events"]
    require(type(events) is list and len(events) == len(BOUNDARIES),
            "missing or extra events")
    for index, event in enumerate(events):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(type(event["sequence"]) is int and event["sequence"] == index + 1 and
                event["actor"] == "operator" and event["generation"] is None and
                event["operation-id"] == OPERATIONS[index] and
                event["boundary"] == BOUNDARIES[index],
                "event identity or boundary differs")
        require(event["causal-predecessors"] == ([] if index == 0 else [index]),
                "missing or ambiguous causal predecessor")
    queued, canceled, inspection, refusal = [event["raw-facts"] for event in events]
    exact_keys(queued, ("exit-code", "work"), "queue facts")
    exact_keys(canceled, ("exit-code", "work"), "cancel facts")
    exact_keys(inspection, ("exit-code", "queue", "status", "work"),
               "inspection facts")
    exact_keys(refusal, ("exit-code", "stderr", "work"), "refusal facts")
    require(all(type(facts["exit-code"]) is int and facts["exit-code"] == 0
                for facts in (queued, canceled, inspection)) and
            type(refusal["exit-code"]) is int and refusal["exit-code"] != 0,
            "public command outcome differs")
    require(queued["work"] == load_json(observed["queued-work.json"]) and
            canceled["work"] == load_json(observed["foreground-work.json"]) and
            inspection["work"] == queued["work"] and
            refusal["work"] == canceled["work"],
            "raw Work facts differ from retained snapshots")
    require(queued["work"].get("id") == "queued" and
            canceled["work"].get("id") == "foreground", "Work identity changed")
    require(type(queued["work"].get("attempts")) is list and
            queued["work"]["attempts"] == [], "queued Work gained an Attempt")
    attempts = canceled["work"].get("attempts")
    require(type(attempts) is list and len(attempts) == 1 and
            attempts[0].get("id") == "attempt-1" and
            attempts[0].get("status") == "needs-user" and
            type(attempts[0].get("cancellation")) is dict and
            attempts[0]["cancellation"].get("requested_at") is not None,
            "cancellation did not retain the foreground Attempt")
    require(inspection["queue"] == observed["queue.txt"].decode() and
            inspection["status"] == observed["status.txt"].decode() and
            type(inspection["status"]) is str and inspection["status"],
            "observation differs from retained output")
    require(any(line.split()[0] == "0" and line.split()[2:] == ["queued", "queued"]
                for line in inspection["queue"].splitlines() if len(line.split()) == 4),
            "queued Work disappeared or changed status")
    require(refusal["stderr"] == observed["refused.txt"].decode() and
            refusal["stderr"] ==
            "Error: Attempt has a durable cancellation request; explicitly resume after settlement\n",
            "cancellation refusal not observed")
    return {"states": ["queued", "other-canceled", "observed", "refused"],
            "queued-work": "queued", "canceled-work": "foreground"}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent),
                         sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"integrated projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

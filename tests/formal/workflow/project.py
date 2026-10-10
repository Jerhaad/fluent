#!/usr/bin/env python3
"""Project version-1 public observations without trusting command verdict text."""

import hashlib
import json
import re
import sys
from pathlib import Path


class TraceError(ValueError):
    pass


def require(ok, message):
    if not ok:
        raise TraceError(message)


def exact_keys(value, keys, label):
    require(type(value) is dict and set(value) == set(keys), f"{label}: invalid fields")


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        require(key not in value, f"duplicate JSON key: {key}")
        value[key] = item
    return value


def load_json(data):
    return json.loads(data, object_pairs_hook=unique_object)


def attempt(work):
    require(type(work) is dict and type(work.get("attempts")) is list,
            "work snapshot lacks attempts")
    return work["attempts"]


def project(trace, root, model_input=False):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(type(trace["schema-version"]) is int and trace["schema-version"] == 1,
            "unsupported trace schema")
    require(trace["scenario"] == "foundation-public-cancel" and
            trace["model-profile"] == "foundation", "wrong scenario or profile")
    require(type(trace["collection-complete"]) is bool and trace["collection-complete"],
            "collection incomplete")
    for key in ("fluent-commit", "fluent-binary-sha256"):
        require(type(trace[key]) is str and re.fullmatch(r"[0-9a-f]{40,64}", trace[key]),
                f"invalid {key}")
    artifacts = trace["artifacts"]
    require(type(artifacts) is list and len(artifacts) == 4, "missing artifacts")
    artifact_names = set()
    for artifact in artifacts:
        exact_keys(artifact, ("path", "sha256"), "artifact")
        name = artifact["path"]
        require(type(name) is str and name in
                {"intake.json", "planned.json", "canceled.json", "refused.json"}
                and name not in artifact_names, "invalid artifact path")
        artifact_names.add(name)
        data = (root / name).read_bytes()
        require(hashlib.sha256(data).hexdigest() == artifact["sha256"],
                f"artifact digest differs: {name}")
    events = trace["events"]
    require(type(events) is list and len(events) == 4, "missing or extra events")
    stages = ("intake", "attempt-create", "cancel", "run-refused")
    operations = ("create-work", "create-attempt", "cancel-attempt", "run-attempt")
    snapshots = []
    for index, event in enumerate(events):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(type(event["sequence"]) is int and event["sequence"] == index + 1,
                "event sequence differs")
        require(event["actor"] == "operator" and event["operation-id"] == operations[index]
                and event["generation"] is None and event["boundary"] == stages[index],
                "event identity or boundary differs")
        require(event["causal-predecessors"] == ([] if index == 0 else [index]),
                "missing or ambiguous causal predecessor")
        facts = event["raw-facts"]
        exact_keys(facts, ("exit-code", "work") if index < 3 else
                   ("exit-code", "stdout", "stderr", "work"), "raw facts")
        require(type(facts["exit-code"]) is int, "missing exit code")
        if index < 3:
            require(facts["exit-code"] == 0, "public command failed")
        else:
            require(facts["exit-code"] != 0 and type(facts["stderr"]) is str
                    and type(facts["stdout"]) is str, "run refusal not observed")
            require(facts["stderr"] ==
                    "Error: Attempt has a durable cancellation request; explicitly resume after settlement\n"
                    and facts["stdout"] == "", "cancellation refusal not observed")
        name = ("intake.json", "planned.json", "canceled.json", "refused.json")[index]
        require(type(facts["work"]) is dict and
                facts["work"] == load_json((root / name).read_text()),
                "raw snapshot differs from artifact")
        snapshots.append(facts["work"])
    work_id = snapshots[0].get("id")
    require(type(work_id) is str and work_id and all(s.get("id") == work_id for s in snapshots),
            "work identity changed")
    require(attempt(snapshots[0]) == [], "intake already has attempt")
    first = attempt(snapshots[1])
    require(len(first) == 1 and type(first[0]) is dict
            and first[0].get("id") == "attempt-1"
            and first[0].get("status") == "planned", "attempt creation did not plan once")
    canceled = attempt(snapshots[2])
    require(len(canceled) == 1 and type(canceled[0]) is dict
            and type(canceled[0].get("cancellation")) is dict
            and canceled[0].get("id") == "attempt-1"
            and canceled[0].get("status") == "needs-user"
            and canceled[0].get("cancellation", {}).get("requested_at") is not None,
            "cancellation did not suspend current attempt")
    if not model_input:
        require(snapshots[3] == snapshots[2], "run advanced canceled state")
    final_state = "canceled" if snapshots[3] == snapshots[2] else "advanced"
    return {"states": ["intake", "planned", "canceled", final_state],
            "work-id": work_id, "attempt-id": "attempt-1"}


def main():
    try:
        path = Path(sys.argv[1]).resolve()
        require(len(sys.argv) == 2 or
                (len(sys.argv) == 3 and sys.argv[2] == "--model-input"),
                "invalid arguments")
        print(json.dumps(project(load_json(path.read_text()), path.parent,
                                 len(sys.argv) == 3), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError) as error:
        print(f"trace violation: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

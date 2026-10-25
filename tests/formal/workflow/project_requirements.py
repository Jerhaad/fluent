#!/usr/bin/env python3
"""Check section-qualified requirements in the matrix the Writer actually read."""
import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require

BOUNDARIES = ("delivered",)
OPERATIONS = ("task-run",)


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "requirements"
            and trace["model-profile"] == "requirements-connection"
            and trace["collection-complete"] is True, "incomplete requirement trace")
    require(re.fullmatch(r"[0-9a-f]{40}", trace["fluent-commit"]) and
            re.fullmatch(r"[0-9a-f]{64}", trace["fluent-binary-sha256"]), "invalid candidate")
    require(type(trace["events"]) is list and len(trace["events"]) == 1 and
            type(trace["artifacts"]) is list and len(trace["artifacts"]) == 1,
            "missing requirement observations")
    facts = []
    for index, (event, artifact) in enumerate(zip(trace["events"], trace["artifacts"])):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "operator" and
                event["generation"] is None and event["boundary"] == BOUNDARIES[index] and
                event["operation-id"] == OPERATIONS[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]),
                "ambiguous requirement order")
        exact_keys(artifact, ("path", "sha256"), "artifact")
        require(artifact["path"] == f"requirements-{index}.json", "invalid requirement path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "requirement digest differs")
        require(load_json(raw) == event["raw-facts"], "requirement observation differs")
        facts.append(event["raw-facts"])
    fact = facts[0]
    require(fact["observed"] == fact["matrix"], "Writer did not read the current matrix")
    matrix = load_json(fact["matrix"])
    work = fact["work"]
    task = work["attempts"][0]["tasks"][0]
    require(matrix["schema-version"] == 2 and matrix["task-id"] == task["id"] and
            task["work_item_id"] == work["id"] and task["attempt_id"] == work["attempts"][0]["id"],
            "matrix Task binding differs")
    rows = matrix["rows"]
    identities = {row["id"] for row in rows}
    required = {f"behavior:tester-task:b{number}" for number in range(20,34)} | {
        "behavior:public-workflow-continuation:b20"}
    require(required <= identities and len(identities) == len(rows), "section-qualified identities were lost")
    document = fact["document"]
    require("## Tester Task\n" in document and "## Public workflow continuation\n" in document,
            "missing authoritative section headings")
    for identity in sorted(required):
        row = next(row for row in rows if row["id"] == identity)
        section, number = identity.split(":")[1:]
        title = "Tester Task" if section == "tester-task" else "Public workflow continuation"
        body = document.split("## " + title + "\n", 1)[1].split("\n## ", 1)[0]
        body = body.split("### " + number.upper() + "\n", 1)[1].split("\n### ", 1)[0]
        normalized = " ".join(body.split())
        statement = " ".join(" ".join(line for line in body.splitlines() if not line.startswith("Test:")).split())
        require(row["source"] == "behaviors.md#" + section + ":" + number and
                row["requirement"] and row["verification-refs"] and
                " ".join(row["requirement"].split()) == statement and
                all(reference in normalized for reference in row["verification-refs"]),
                "requirement statement or proof reference changed section")
    return {"identities": sorted(required), "work": work["id"], "task": task["id"], "states": ["delivered"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"requirements projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

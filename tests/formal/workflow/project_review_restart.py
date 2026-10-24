#!/usr/bin/env python3
"""Check review-only restart against retained Tester and review identities."""
import hashlib
import json
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require
from project_review_only import project as project_review


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and trace["scenario"] == "review-restart"
            and trace["model-profile"] == "review-restart"
            and trace["collection-complete"] is True, "incomplete review restart")
    require(type(trace["events"]) is list and len(trace["events"]) == 3 and
            type(trace["artifacts"]) is list and len(trace["artifacts"]) == 3,
            "missing restart observations")
    facts = []
    for index, (event, artifact) in enumerate(zip(trace["events"], trace["artifacts"])):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation",
                           "boundary", "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "operator" and
                event["generation"] is None and
                event["boundary"] == ("crashed", "canceled", "continued")[index] and
                event["operation-id"] == ("kill-parent", "attempt-cancel", "attempt-run")[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]), "ambiguous restart order")
        exact_keys(artifact, ("path", "sha256"), "artifact")
        require(artifact["path"] == f"review-restart-{index}.json", "invalid restart path")
        raw = (root / artifact["path"]).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == artifact["sha256"], "restart digest differs")
        require(load_json(raw) == event["raw-facts"], "restart observation differs")
        facts.append(event["raw-facts"])
    continued = facts[2]["continued"]
    require(all(trace[key] == continued[key] for key in ("fluent-commit", "fluent-binary-sha256")),
            "restart candidate differs")
    mapping = project_review(continued, root)
    final = continued["events"][1]["raw-facts"]["work"]["attempts"][0]
    attempts = []
    for fact in facts[:2]:
        work = fact["work"]
        require(work["id"] == mapping["work"] and len(work["attempts"]) == 1,
                "restart changed Work identity")
        attempt = work["attempts"][0]
        require(attempt["id"] == mapping["attempt"] and attempt["kind"] == "review-only" and
                [task["id"] for task in attempt["tasks"]] == [task["id"] for task in final["tasks"]],
                "restart changed Task identities")
        require(attempt["tasks"][0] == final["tasks"][0], "restart replaced completed Tester history")
        attempts.append(attempt)
    require(any(task["status"] == "executing" for task in attempts[0]["tasks"][1:]),
            "crash did not interrupt a reviewer")
    cancellation = attempts[1]["cancellation"]
    require(type(cancellation["requested_at"]) is str and type(cancellation["settled_at"]) is str and
            cancellation.get("resumed_at") is None and
            final["cancellation"]["requested_at"] == cancellation["requested_at"] and
            type(final["cancellation"]["resumed_at"]) is str, "restart lacks current resume authority")
    return {**mapping, "states": ["tester-complete", "review-crashed", "canceled", "reviews-complete"]}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, IndexError, KeyError, TypeError) as error:
        print(f"review restart projection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

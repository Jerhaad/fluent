#!/usr/bin/env python3
"""Check observed release refusal against protected file facts."""

import hashlib
import json
import re
import sys
from pathlib import Path

RELEASE_SHA = "8bb76717be942e4a0c97c297c318c0a1331e92105180edb386354dddbae12974"


def project(path: Path, candidate: Path, commit: str):
    trace = json.loads(path.read_text())
    assert set(trace) == {"schema-version", "scenario", "fluent-commit",
                          "fluent-binary-sha256", "model-profile",
                          "collection-complete", "events", "artifacts"}
    assert trace["schema-version"] == 1
    assert trace["scenario"] in ("current-authority", "pending-publication",
                                 "released-shared-service-transition")
    assert trace["model-profile"] == "release-transition"
    assert trace["collection-complete"] is True
    assert trace["fluent-commit"] == commit
    assert trace["fluent-binary-sha256"] == hashlib.sha256(candidate.read_bytes()).hexdigest()
    assert trace["artifacts"] == []
    if trace["scenario"] == "released-shared-service-transition":
        return project_service(trace)
    before, refused = trace["events"]
    assert before["sequence"] == 1 and refused["sequence"] == 2
    assert before["boundary"] == "before-refusal" and refused["boundary"] == "refused"
    assert before["causal-predecessors"] == []
    assert refused["causal-predecessors"] == [1]
    assert before["operation-id"] == "snapshot"
    assert refused["operation-id"] == "work-item-abandon"
    assert before["raw-facts"]["predecessor-sha256"] == RELEASE_SHA
    facts = refused["raw-facts"]
    assert type(facts["exit-code"]) is int and facts["exit-code"] != 0
    assert isinstance(facts["stderr"], str) and facts["stderr"]
    assert isinstance(facts["stdout"], str)
    protected = before["raw-facts"]["protected"]
    assert isinstance(protected, list) and protected
    assert facts["protected"] == protected
    names = set()
    for entry in protected:
        assert set(entry) == {"path", "kind", "sha256"}
        name = entry["path"]
        assert isinstance(name, str) and name and name not in names
        assert not Path(name).is_absolute() and ".." not in Path(name).parts
        names.add(name)
        assert entry["kind"] in ("file", "directory", "symlink")
        assert re.fullmatch("[0-9a-f]{64}", entry["sha256"])
    assert any(name.startswith("items/") for name in names)
    if trace["scenario"] == "pending-publication":
        assert any(name.startswith("transactions/") for name in names)
    return {"scenario": trace["scenario"], "protected-entries": len(protected)}


def project_service(trace):
    old, stopped, new = trace["events"]
    for index, (event, boundary, operation) in enumerate(zip(
            (old, stopped, new),
            ("old-owner", "old-owner-settled", "new-owner"),
            ("scheduler-run", "scheduler-stop-all", "scheduler-run")), 1):
        assert event["sequence"] == index
        assert event["boundary"] == boundary and event["operation-id"] == operation
        assert event["causal-predecessors"] == ([] if index == 1 else [index - 1])
    first = old["raw-facts"]
    settled = stopped["raw-facts"]
    replacement = new["raw-facts"]
    assert first["predecessor-sha256"] == RELEASE_SHA
    assert first["registered-projects"] == replacement["registered-projects"] == 2
    old_health, new_health = first["health"], replacement["health"]
    assert old_health["instance_id"] != new_health["instance_id"]
    assert old_health["build"] != new_health["build"]
    assert old_health["generation"] == new_health["generation"]
    assert settled["stop-exit-code"] == 0
    assert "Stopped shared scheduler" in settled["stop-stdout"]
    assert settled["owned-worker-started"] is True
    active = settled["active-work"]
    terminal = {"candidate-ready", "failed", "needs-user", "canceled", "blocked"}
    assert len(active) == 2
    assert all(work["status"] in terminal | {"queued"} for work in active)
    assert any(work["status"] in terminal for work in active)
    assert replacement["stale-stop-refused"] is True
    assert len(settled["queues"]) == len(replacement["queues"]) == 2
    assert all(queue["status"] == "queued" for queue in settled["queues"])
    assert all(queue["work_item_id"] == prior["work_item_id"]
               for queue, prior in zip(replacement["queues"], settled["queues"]))
    return {"scenario": trace["scenario"], "service-transfer": "settled"}


if __name__ == "__main__":
    try:
        print(json.dumps(project(Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3])))
    except (AssertionError, OSError, ValueError, IndexError, KeyError) as error:
        print(f"release projection failed: {error}", file=sys.stderr)
        sys.exit(1)

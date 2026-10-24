"""Bind release results to the proof bytes checked before later named tests."""

import hashlib
import json
from pathlib import Path


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def retain_release_evidence(output, collection, cases):
    output = output.resolve()
    paths = {path for path in collection.resolve().rglob("*") if path.is_file()}
    paths.update(output / case["log"] for case in cases if "log" in case)
    return [{"path": str(path.relative_to(output)), "sha256": digest(path)}
            for path in sorted(paths)]


def verify_release_evidence(directory, result):
    retained = result["retained-artifacts"]
    assert isinstance(retained, list) and retained, "missing retained release proof"
    evidence = {}
    for entry in retained:
        assert set(entry) == {"path", "sha256"}
        name = entry["path"]
        assert isinstance(name, str) and name and name not in evidence
        path = Path(name)
        assert not path.is_absolute() and ".." not in path.parts
        assert digest(directory / path) == entry["sha256"], f"changed release proof: {name}"
        evidence[name] = entry["sha256"]
    for case in result["cases"]:
        if "log" in case:
            assert case["log"] in evidence, "unbound release log"
    for scenario in ("current-authority", "pending-publication", "service-ownership"):
        traces = [Path(name) for name in evidence if Path(name).name == f"{scenario}.json"]
        assert len(traces) == 1, f"missing or ambiguous release trace: {scenario}"
        trace = json.loads((directory / traces[0]).read_text())
        assert trace["artifacts"], "missing release artifacts"
        for artifact in trace["artifacts"]:
            relative = Path(artifact["path"])
            assert not relative.is_absolute() and ".." not in relative.parts
            name = str(traces[0].parent / relative)
            assert evidence.get(name) == artifact["sha256"], "unbound release artifact"

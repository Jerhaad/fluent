#!/usr/bin/env python3
"""Project retained landing artifacts and public inspection onto evidence states."""

import hashlib
import json
import re
import sys
from pathlib import Path

from project import TraceError, exact_keys, load_json, require


NAMES = {"ready-work.json", "landed-work.json", "landing-results.json",
         "landing-manifest.json", "target-baseline.json", "attempt-results.json",
         "status.txt", "shown-work.json", "tester-config.yaml", "extractor",
         "writer-deltas.json"}


def affected_roles(paths):
    """Apply the ordinary review domain policy to retained Writer paths."""
    roles = set()
    for path in paths:
        require(isinstance(path, str) and path and not path.startswith("/") and
                all(part not in ("", ".", "..") for part in path.split("/")),
                "invalid Writer delta path")
        if path.startswith(".fluent/expertise/"):
            changed = set()
        elif path in ("documentation/architecture.md", "expertise/architecture.md"):
            changed = {"documentation", "architecture"}
        elif path in ("documentation/behaviors.md", "expertise/behaviors.md"):
            changed = {"documentation", "behaviors", "tests"}
        elif path.startswith("documentation/") or path in ("README.md", "LICENSE") or path.startswith("CHANGELOG"):
            changed = {"documentation"}
        elif path == "expertise/tests.md":
            changed = {"documentation", "tests"}
        elif path == "expertise/skills.md":
            changed = {"documentation", "skills"}
        elif path in ("expertise/documentation.md", "expertise/README.md"):
            changed = {"documentation"}
        elif path.startswith("expertise/"):
            changed = {"documentation", "behaviors", "architecture", "skills", "tests"}
        elif path.startswith("skills/"):
            changed = {"documentation", "skills"}
        elif path.startswith("skill-migrations/"):
            changed = {"documentation", "skills", "tests"}
        elif path.startswith("prompts/"):
            changed = {"documentation", "behaviors", "skills", "tests"}
        elif path.startswith("tests/"):
            changed = {"tests"}
        elif (path.startswith(("src/", "scripts/", "tools/", "infrastructure/", "sandboxes/", ".fluent/hooks/")) or
              path in (".fluent/tester.yaml", ".fluent/extract-tester-results", "Cargo.toml", "Cargo.lock", "build.rs")):
            changed = {"documentation", "architecture", "tests"}
        else:
            changed = {"documentation", "behaviors", "architecture", "skills", "tests"}
        roles.update(changed)
    return roles


def digest(data):
    return hashlib.sha256(data).hexdigest()


def artifact(work, producer):
    matches = [ref["path"] for ref in work["merge_candidates"][0]["merge_state"]
               ["check_artifacts"] if ref["producer_id"] == producer]
    require(len(matches) == 1, f"missing or ambiguous {producer} artifact")
    return matches[0]


def project(trace, root):
    exact_keys(trace, ("schema-version", "scenario", "fluent-commit",
                       "fluent-binary-sha256", "model-profile", "collection-complete",
                       "events", "artifacts"), "trace")
    require(trace["schema-version"] == 1 and
            trace["scenario"] == "evidence-status-rebased-landing" and
            trace["model-profile"] == "evidence-status" and
            trace["collection-complete"] is True, "wrong or incomplete evidence trace")
    for field in ("fluent-commit", "fluent-binary-sha256"):
        require(type(trace[field]) is str and re.fullmatch(r"[0-9a-f]{40,64}", trace[field]),
                f"invalid {field}")
    artifacts = trace["artifacts"]
    require(type(artifacts) is list, "missing artifacts")
    seen = set()
    for entry in artifacts:
        exact_keys(entry, ("path", "sha256"), "artifact")
        name = entry["path"]
        require(type(name) is str and name not in seen and
                (name in NAMES or re.fullmatch(r"landing-\d+-(stdout|stderr)\.log", name)),
                "invalid artifact path")
        seen.add(name)
        require(digest((root / name).read_bytes()) == entry["sha256"],
                f"artifact digest differs: {name}")
    require(NAMES <= seen, "missing required evidence artifact")
    ready = load_json((root / "ready-work.json").read_text())
    landed = load_json((root / "landed-work.json").read_text())
    shown = load_json((root / "shown-work.json").read_text())
    results = load_json((root / "landing-results.json").read_text())
    manifest = load_json((root / "landing-manifest.json").read_text())
    baseline = load_json((root / "target-baseline.json").read_text())
    attempt_results = load_json((root / "attempt-results.json").read_text())
    events = trace["events"]
    require(type(events) is list and len(events) == 4, "missing evidence events")
    boundaries = ("ready", "target-advanced", "landed", "observed")
    operations = ("attempt-run", "advance-target", "land-candidate", "inspect-landed")
    for index, event in enumerate(events):
        exact_keys(event, ("sequence", "actor", "operation-id", "generation", "boundary",
                           "causal-predecessors", "raw-facts"), "event")
        require(event["sequence"] == index + 1 and event["actor"] == "operator" and
                event["operation-id"] == operations[index] and event["generation"] is None and
                event["boundary"] == boundaries[index] and
                event["causal-predecessors"] == ([] if index == 0 else [index]),
                "evidence event identity or order differs")
    require(events[0]["raw-facts"] == {"work": ready} and
            events[2]["raw-facts"].get("work") == landed and
            events[3]["raw-facts"] == {"status": (root / "status.txt").read_text(),
                                       "show": shown}, "event facts differ from raw artifacts")
    target = events[1]["raw-facts"].get("target-commit")
    merged = events[2]["raw-facts"].get("merged-commit")
    require(type(target) is str and re.fullmatch(r"[0-9a-f]{40}", target) and
            type(merged) is str and re.fullmatch(r"[0-9a-f]{40}", merged) and target != merged,
            "target advance or merged identity is missing")
    require(ready["id"] == landed["id"] == shown["id"] and
            ready["attempts"][0]["id"] == landed["attempts"][0]["id"],
            "Work or Attempt identity changed")
    ready_attempt = ready["attempts"][0]
    require(ready_attempt["status"] == "complete" and
            ready_attempt["review_state"] == "passed" and
            ready_attempt["learning"]["status"] == "succeeded", "ready gates are missing")
    ready_candidate = ready["merge_candidates"][0]
    require(ready["authorization"] == {"state": "execution-ready", "authority": "human"},
            "ready authority differs")
    writes = [task for task in ready_attempt["tasks"] if task["kind"] == "write" and
              task["status"] == "complete" and task.get("output")]
    require(writes and writes[-1]["output"]["commit"] == ready_candidate["candidate_commit"],
            "ready Writer binding differs")
    expected_context = {"candidate_commit": ready_candidate["candidate_commit"],
                        "candidate_workspace_id": ready_candidate["source_workspace"]["id"],
                        "candidate_workspace_path": ready_candidate["source_workspace"]["path"],
                        "source_branch": ready_candidate["source_branch"]}
    deltas = load_json((root / "writer-deltas.json").read_text())
    require(type(deltas) is list and len(deltas) == len(writes) - 1,
            "Writer delta evidence is incomplete")
    invalidated = {}
    for index, delta in enumerate(deltas, 1):
        require(delta.get("from") == writes[index - 1]["output"]["commit"] and
                delta.get("to") == writes[index]["output"]["commit"] and
                type(delta.get("paths")) is list,
                "Writer delta binding differs")
        invalidated[writes[index]["id"]] = affected_roles(delta["paths"])
    reviews = [task for task in ready_attempt["tasks"] if task["kind"] == "review" and
               task["status"] == "complete" and task.get("evidence_review_context") is None]
    credited = set()
    for role in ("architecture", "behaviors", "documentation", "skills", "tests"):
        review = next((task for task in reversed(reviews) if task["role"] == role), None)
        if review is None:
            continue
        context = review.get("review_context") or {}
        bound = all(context.get(key) == value for key, value in expected_context.items())
        if not bound:
            # Each later Writer must leave this Review's domain unchanged.
            review_index = ready_attempt["tasks"].index(review)
            later = [write for write in writes[1:] if
                     ready_attempt["tasks"].index(write) > review_index]
            carried = (context.get("candidate_commit") in
                       {write["output"]["commit"] for write in writes} and
                       bool(later) and all(role not in invalidated[write["id"]]
                                           for write in later))
            require(carried and all(context.get(key) == expected_context[key] for key in
                                    ("candidate_workspace_id", "candidate_workspace_path",
                                     "source_branch")),
                    "ordinary review binding differs")
        require({"id": context["candidate_workspace_id"],
                 "path": context["candidate_workspace_path"]} in
                review["workspace_access"]["reads"], "ordinary review authority differs")
        credited.add(role)
    require(credited >= {"architecture", "behaviors", "documentation", "skills", "tests"},
            "ordinary review coverage is incomplete")
    tester = [task for task in ready_attempt["tasks"] if task["kind"] == "tester" and
              task["status"] == "complete"]
    require(len(tester) == 1, "canonical Attempt Tester is missing")
    require(all(tester[0]["review_context"].get(key) == value for key, value in
                expected_context.items()), "Attempt Tester context differs")
    require(attempt_results["candidate_commit"] ==
            tester[0]["review_context"]["candidate_commit"] and
            attempt_results["candidate_commit"] != merged, "Attempt Tester identity is wrong")
    candidate = landed["merge_candidates"][0]
    require(candidate["merge_state"]["status"] == "merged" and
            candidate["merge_state"]["merged_commit"] == merged and
            candidate["candidate_commit"] == merged, "final candidate is not merged")
    final_path = artifact(landed, "merge-tester")
    baseline_path = artifact(landed, "merge-target-baseline")
    require(final_path.endswith("/tester/tester-results.json") and
            baseline_path == final_path.replace("/tester/tester-results.json",
                                                "/target-baseline/tester-results.json"),
            "landing proof path is unbound")
    require(results["candidate_commit"] == merged and baseline["candidate_commit"] == target,
            "landing or baseline candidate binding differs")
    require(manifest["schema_version"] == 1 and manifest["full_run"] is True and
            manifest["config_digest"] == "sha256:" + digest((root / "tester-config.yaml").read_bytes()) and
            manifest["extractor_digest"] == "sha256:" + digest((root / "extractor").read_bytes()),
            "landing manifest configuration differs")
    require(results["error"] is None and baseline["error"] is None and
            len(results["commands"]) == len(manifest["commands"]) > 0,
            "landing Tester is incomplete")
    for index, (command, declared) in enumerate(zip(results["commands"], manifest["commands"])):
        require(command["command"] == declared["command"] and
                command["test_harness"] == declared["test_harness"],
                f"landing command {index} differs from its occurrence")
        for stream in ("stdout", "stderr"):
            require(command[f"{stream}_log"] == f"commands/{index}-{stream}.log" and
                    f"landing-{index}-{stream}.log" in seen,
                    f"landing command {index} log is missing")
        require(not (command.get("process_settlement") or {}).get("leaks"),
                f"landing command {index} leaked a process")
    require(results["summary"]["total"] == len(results["tests"]) and
            results["summary"]["fail"] == sum(test["status"] == "fail" for test in results["tests"]),
            "landing summary differs")
    known = {test["id"] for test in baseline["tests"] if test["status"] == "fail"}
    require({test["id"] for test in results["tests"] if test["status"] == "fail"} <= known,
            "landing has a new failing test")
    for index, command in enumerate(results["commands"]):
        if command["exit_code"] == 0:
            continue
        prior = baseline["commands"][index]
        harness = command["test_harness"]
        current_failures = {test["id"] for test in results["tests"] if
                            test["status"] == "fail" and test["test_harness"] == harness}
        prior_failures = {test["id"] for test in baseline["tests"] if
                          test["status"] == "fail" and test["test_harness"] == harness}
        require(command["command"] == prior["command"] and
                command["test_harness"] == prior["test_harness"] and
                command["exit_code"] == prior["exit_code"] and
                len([entry for entry in results["commands"] if entry["exit_code"] != 0 and
                     entry["test_harness"] == harness]) == 1 and
                len([entry for entry in baseline["commands"] if entry["exit_code"] != 0 and
                     entry["test_harness"] == harness]) == 1 and
                current_failures and current_failures <= prior_failures,
                f"landing command {index} has an unexplained failure")
    require(shown["evidence-history"]["merged-commit"] == merged and
            shown["evidence-history"]["tester-results"] == final_path and
            shown["evidence-history"]["diagnostic"] is None and
            "merged" in (root / "status.txt").read_text() and
            final_path in (root / "status.txt").read_text() and
            "next-action" not in shown, "display did not retain final accepted evidence")
    return {"states": ["ready", "target-advanced", "landed", "observed"],
            "merged-commit": merged, "landing-proof": final_path}


def main():
    try:
        require(len(sys.argv) == 2, "invalid arguments")
        path = Path(sys.argv[1]).resolve()
        print(json.dumps(project(load_json(path.read_text()), path.parent), sort_keys=True))
    except (TraceError, OSError, ValueError, KeyError, IndexError, TypeError) as error:
        print(f"evidence trace violation: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Check the foundation model, defect controls, and a fresh public-command trace."""

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

JAR_SHA256 = "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
CONTROLS = {
    "deliverinputs": "Invariant InputSafe is violated",
    "checkevidence": "Invariant EvidenceSafe is violated",
    "fenceresume": "Invariant ResumeSafe is violated",
    "guardeffect": "Invariant OneEffect is violated",
    "truthfulobservation": "Invariant ObservationSafe is violated",
    "guardcleanup": "Invariant CleanupSafe is violated",
    "guardversion": "Invariant VersionSafe is violated",
    "guardbaseline": "Invariant AcceptedBaselineSafe is violated",
    "guardlandingbaseline": "Invariant LandingAcceptanceSafe is violated",
    "canonical-incomplete": "Invariant AcceptedBaselineSafe is violated",
    "canonical-harness": "Invariant AcceptedBaselineSafe is violated",
    "canonical-leak": "Invariant AcceptedBaselineSafe is violated",
    "canonical-unexplained": "Invariant AcceptedBaselineSafe is violated",
    "canonical-misbound": "Invariant AcceptedBaselineSafe is violated",
    "canonical-reference-fail": "Invariant AcceptedBaselineSafe is violated",
    "prewritebinding": "Invariant AcceptedBaselineSafe is violated",
    "reviewroute": "Invariant ReviewOnlyNoGit is violated",
    "landing-new": "Invariant LandingAcceptanceSafe is violated",
    "landing-incomplete": "Invariant LandingAcceptanceSafe is violated",
    "landing-harness": "Invariant LandingAcceptanceSafe is violated",
    "landing-leak": "Invariant LandingAcceptanceSafe is violated",
    "landing-unexplained": "Invariant LandingAcceptanceSafe is violated",
    "landing-misbound": "Invariant LandingAcceptanceSafe is violated",
    "landing-reference-fail": "Invariant LandingAcceptanceSafe is violated",
    "target-untrusted": "Invariant LandingAcceptanceSafe is violated",
    "target-misbound": "Invariant LandingAcceptanceSafe is violated",
}
PROFILE_ACTIONS = {
    "foundation.cfg": ("ChooseForeground", "Enqueue", "Dispatch", "Writer",
                       "Review", "TargetedReview", "RecheckSameCandidate",
                       "CarryForward", "CorrectCandidate", "Tester",
                       "LearnCapture", "LearnNoExpertise", "Ready",
                       "CanonicalResult", "LandingResult", "TargetBaseline",
                       "LandingVerify",
                       "GitEffect", "PublishResult"),
    "repeated-recovery.cfg": ("Crash", "Recover", "FailWrite", "RecoverIO",
                              "ReplaceGeneration", "GrantResumeAuthority"),
    "review-only.cfg": ("ReviewOnlyStart", "ReviewOnlyReview", "ReviewOnlyReady",
                        "ReviewOnlyFailure", "Cancel", "GrantResumeAuthority",
                        "Observe", "VersionTransition"),
    "service-competition.cfg": ("ChooseForeground", "Enqueue", "Dispatch",
                                "Writer", "Crash", "Recover", "ReplaceGeneration"),
    "combined-one-failure.cfg": ("Crash", "Recover", "FailWrite", "RecoverIO",
                                  "CorrectCandidate",
                                  "CarryForward", "Observe", "VersionTransition",
                                  "TargetBaseline", "GitEffect", "PublishResult"),
}

GUARDED_CASES = {
    "canonical": ("new", "incomplete", "harness", "leak", "unexplained",
                  "misbound", "reference-fail"),
    "prewrite": ("untrusted", "misbound"),
    "landing": ("new", "incomplete", "harness", "leak", "unexplained",
                "misbound", "reference-fail"),
    "target": ("cleared", "untrusted", "misbound"),
}


def run_case(source, output_dir, jar, java, config, expected, module="Workflow"):
    name = Path(config).name
    with tempfile.TemporaryDirectory(prefix="tlc-", dir=output_dir) as metadir:
        command = [java, "-XX:+UseParallelGC", "-Xmx512m", "-cp", str(jar),
                   "tlc2.TLC", "-deadlock", "-coverage", "1", "-workers", "1", "-fp", "0",
                   "-seed", "1", "-metadir", metadir, "-config", config,
                   f"{module}.tla"]
        try:
            result = subprocess.run(command, cwd=source, capture_output=True,
                                    text=True, timeout=240)
            log = result.stdout + result.stderr
        except subprocess.TimeoutExpired as error:
            result = None
            log = f"TLC timed out: {error}\n"
    (output_dir / f"{name}.log").write_text("$ " + " ".join(command) + "\n" + log)
    missing_actions = []
    if expected:
        ok = result is not None and result.returncode != 0 and expected in log
    else:
        ok = result is not None and result.returncode == 0 and "Model checking completed" in log
        for action in PROFILE_ACTIONS.get(name, ()):
            counts = re.findall(rf"<{action} line [^>]+>: (\d+):(\d+)", log)
            if not any(int(generated) > 0 for _, generated in counts):
                missing_actions.append(action)
        ok = ok and not missing_actions
    return {"case": name, "pass": ok, "expected": expected,
            "missing-actions": missing_actions, "log": f"{name}.log"}


def guarded_cases(source, output, jar, java, only=None):
    """Complete each guarded boundary search and witness its rejected input."""
    base = (source / "foundation.cfg").read_text()
    results = []
    for boundary, kinds in GUARDED_CASES.items():
        for kind in kinds:
            if only is not None and (boundary, kind) != only:
                continue
            config = base.replace("CONSTRAINT FoundationBound",
                                  "CONSTRAINT GuardedCanonicalBound" if boundary in
                                  ("canonical", "prewrite") else
                                  "CONSTRAINT GuardedLandingBound")
            replacements = {
                "canonical": ("CanonicalKinds = {\"pass\"}",
                              f'CanonicalKinds = {{"{kind}"}}'),
                "prewrite": ("CorruptPrewrite = FALSE", "CorruptPrewrite = TRUE"),
                "landing": ("LandingKinds = {\"pass\"}",
                            f'LandingKinds = {{"{kind}"}}'),
                "target": ("TargetKinds = {\"unchanged\"}",
                           f'TargetKinds = {{"{kind}"}}'),
            }
            old, new = replacements[boundary]
            if boundary == "prewrite":
                config = config.replace('CanonicalKinds = {"pass"}',
                                        'CanonicalKinds = {"known"}')
            if boundary == "target" and kind == "cleared":
                config = config.replace('LandingKinds = {"pass"}',
                                        'LandingKinds = {"known"}')
            assert config.count(old) == 1
            config = config.replace(old, new)
            stem = f"guarded-{boundary}-{kind}"
            positive = output / f"{stem}.cfg"
            positive.write_text(config)
            results.append(run_case(source, output, jar, java, str(positive), None))
            witness = output / f"{stem}-witness.cfg"
            invariant = ("CanonicalRejectionUnreachable" if boundary == "canonical"
                         else f"{kind.capitalize()}PrewriteRejectionUnreachable"
                         if boundary == "prewrite" else
                         "LandingRejectionUnreachable")
            witness.write_text(config + f"INVARIANT {invariant}\n")
            results.append(run_case(source, output, jar, java, str(witness),
                                    f"Invariant {invariant} is violated"))
    return results


def run_trace_case(source, output, jar, java, name, states, expect_pass):
    config = output / f"{name}.cfg"
    config.write_text("SPECIFICATION TraceSpec\nCONSTANTS\n"
                      "  Works = {w1}\n  TraceWork = w1\n  ReviewOnly = {}\n  MaxFailures = 0\n"
                      + '  CanonicalKinds = {"pass"}\n'
                      + '  LandingKinds = {"pass"}\n'
                      + '  CorruptPrewrite = FALSE\n'
                      + '  TargetKinds = {"unchanged"}\n'
                      + "".join(f"  {control} = TRUE\n" for control in
                                ("DeliverInputs", "CheckEvidence", "FenceResume",
                                 "GuardEffect", "TruthfulObservation", "GuardCleanup",
                                 "GuardVersion", "GuardBaseline",
                                 "GuardLandingBaseline", "GuardReviewRoute"))
                      + "".join(f"  TraceState{i} = {json.dumps(state)}\n"
                                for i, state in enumerate(states, 1))
                      + "PROPERTY TraceComplete\n")
    with tempfile.TemporaryDirectory(prefix="tlc-trace-", dir=output) as metadir:
        command = [java, "-XX:+UseParallelGC", "-Xmx512m", "-cp", str(jar),
                   "tlc2.TLC", "-deadlock", "-workers", "1", "-fp", "0",
                   "-seed", "1", "-metadir", metadir, "-config", str(config),
                   "Trace.tla"]
        try:
            result = subprocess.run(command, cwd=source, capture_output=True,
                                    text=True, timeout=60)
            log = result.stdout + result.stderr
        except subprocess.TimeoutExpired as error:
            result = None
            log = f"TLC timed out: {error}\n"
    (output / f"{name}.log").write_text("$ " + " ".join(command) + "\n" + log)
    complete = result is not None and "Model checking completed" in log
    ok = (result.returncode == 0 and complete) if expect_pass else (
        result is not None and result.returncode != 0 and
        "Temporal properties were violated" in log and
        "traceIndex = 3" in log and bool(re.search(r"State \d+: Stuttering", log)))
    return {"case": name, "pass": ok, "log": f"{name}.log"}


def java_21(root):
    candidates = ["java", *map(str, sorted((root / "target/fluent-tools/jdk21").glob(
        "*/Contents/Home/bin/java")))]
    for executable in candidates:
        try:
            version = subprocess.run([executable, "-version"], capture_output=True,
                                     text=True, check=True)
        except (OSError, subprocess.CalledProcessError):
            continue
        if re.search(r'(?:version "21(?:\.|"))|(?:openjdk 21(?:\.|\s|$))',
                     version.stdout + version.stderr):
            return executable
    return None


def writer_inputs_suite(source, root, output, jar, java):
    results = [run_case(source, output, jar, java, "negative-deliverinputs.cfg",
                        CONTROLS["deliverinputs"])]
    command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
               "--test", "binary", "--no-tests", "fail",
               "classified_writer_receives_current_authorized_instructions"]
    trace_path = output / "production-trace.json"
    for name in ("production-trace.json", "classified-work.json", "writer-work.json",
                 "classification.json", "provider-invocation.txt", "provider-observation.txt"):
        (output / name).unlink(missing_ok=True)
    fixture = subprocess.run(command, cwd=root, capture_output=True, text=True)
    (output / "production-fixture.log").write_text("$ " + " ".join(command) + "\n"
                                                    + fixture.stdout + fixture.stderr)
    fixture_ok = fixture.returncode == 0 and trace_path.is_file()
    results.append({"case": "fresh-writer-fixture", "pass": fixture_ok,
                    "log": "production-fixture.log"})
    if fixture_ok:
        trace = json.loads(trace_path.read_text())
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                              capture_output=True, text=True, check=True).stdout.strip()
        binary = root / "target/debug/fluent"
        bound = trace["fluent-commit"] == head and binary.is_file() and \
            trace["fluent-binary-sha256"] == hashlib.sha256(binary.read_bytes()).hexdigest()
        projection = subprocess.run(["python3", str(source / "project_writer.py"),
                                     str(trace_path)], capture_output=True, text=True)
        (output / "projection.log").write_text(projection.stdout + projection.stderr)
        valid = bound and projection.returncode == 0
        results.append({"case": "writer-production-projection", "pass": valid,
                        "log": "projection.log"})
        omitted = json.loads(json.dumps(trace))
        negative_dir = output / "omitted-instruction"
        negative_dir.mkdir(exist_ok=True)
        for name in ("classified-work.json", "writer-work.json", "classification.json",
                     "provider-invocation.txt"):
            (negative_dir / name).write_bytes((output / name).read_bytes())
        observation = (output / "provider-observation.txt").read_text()
        authorization = json.loads((output / "classification.json").read_text())["authorization"]
        require_omission = authorization in observation
        changed = observation.replace(authorization, "[instruction omitted]")
        (negative_dir / "provider-observation.txt").write_text(changed)
        for artifact in omitted["artifacts"]:
            if artifact["path"] == "provider-observation.txt":
                artifact["sha256"] = hashlib.sha256(changed.encode()).hexdigest()
        omitted_path = negative_dir / "production-trace.json"
        omitted_path.write_text(json.dumps(omitted))
        negative = subprocess.run(["python3", str(source / "project_writer.py"),
                                   str(omitted_path)], capture_output=True, text=True)
        (output / "omitted-instruction.log").write_text(negative.stdout + negative.stderr)
        results.append({"case": "omitted-instruction-control",
                        "pass": require_omission and negative.returncode != 0 and
                                "provider did not observe current instructions" in negative.stderr,
                        "log": "omitted-instruction.log"})
        missing = json.loads(json.dumps(trace))
        missing["events"] = [event for event in missing["events"]
                             if event["boundary"] != "read-required-input"]
        missing_path = output / "missing-delivery-trace.json"
        missing_path.write_text(json.dumps(missing))
        missing_result = subprocess.run(
            ["python3", str(source / "project_writer.py"), str(missing_path)],
            capture_output=True, text=True)
        (output / "missing-delivery.log").write_text(
            missing_result.stdout + missing_result.stderr)
        results.append({"case": "missing-delivery-boundary-control",
                        "pass": missing_result.returncode != 0 and
                                "missing writer events" in missing_result.stderr,
                        "log": "missing-delivery.log"})
        if valid:
            states = json.loads(projection.stdout)["states"]
            config = output / "writer-production-path.cfg"
            config.write_text("SPECIFICATION WriterSpec\nCONSTANTS\n"
                              "  Works = {w1}\n  WriterWork = w1\n  ReviewOnly = {}\n"
                              "  MaxFailures = 0\n"
                              + '  CanonicalKinds = {"pass"}\n'
                              + '  LandingKinds = {"pass"}\n'
                              + '  TargetKinds = {"unchanged"}\n'
                              + '  CorruptPrewrite = FALSE\n'
                              + "".join(f"  {control} = TRUE\n" for control in
                                         ("DeliverInputs", "CheckEvidence", "FenceResume",
                                          "GuardEffect", "TruthfulObservation", "GuardCleanup",
                                          "GuardVersion", "GuardBaseline",
                                          "GuardLandingBaseline", "GuardReviewRoute"))
                              + "".join(f"  WriterState{i} = {json.dumps(state)}\n"
                                        for i, state in enumerate(states, 1))
                              + "INVARIANT InputSafe\nPROPERTY WriterComplete\n")
            results.append(run_case(source, output, jar, java, str(config), None,
                                    module="WriterTrace"))
    result = {"schema-version": 1, "suite": "writer-inputs", "cases": results,
              "pass": all(row["pass"] for row in results)}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    for row in results:
        print(("PASS" if row["pass"] else "FAIL") + " " + row["case"])
    return 0 if result["pass"] else 1


def evidence_status_suite(source, root, output, jar, java):
    results = []
    for name in ("negative-checkevidence", "negative-landing-misbound",
                 "negative-landing-new", "negative-truthfulobservation",
                 "known-landing-accepted"):
        expected = ("Invariant KnownLandingUnreachable is violated" if name ==
                    "known-landing-accepted" else CONTROLS[name.removeprefix("negative-")])
        results.append(run_case(source, output, jar, java, name + ".cfg", expected))
    command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
               "--test", "binary", "--no-tests", "fail",
               "landed_status_uses_final_landing_tester_evidence"]
    trace_path = output / "production-trace.json"
    for path in output.glob("landing-*-*.log"):
        path.unlink()
    for name in ("production-trace.json", "ready-work.json", "landed-work.json",
                 "landing-results.json", "landing-manifest.json", "target-baseline.json",
                 "attempt-results.json", "status.txt", "shown-work.json",
                 "tester-config.yaml", "extractor", "writer-deltas.json"):
        (output / name).unlink(missing_ok=True)
    fixture = subprocess.run(command, cwd=root, capture_output=True, text=True)
    (output / "production-fixture.log").write_text("$ " + " ".join(command) + "\n"
                                                     + fixture.stdout + fixture.stderr)
    fixture_ok = fixture.returncode == 0 and trace_path.is_file()
    results.append({"case": "fresh-evidence-fixture", "pass": fixture_ok,
                    "log": "production-fixture.log"})
    carried_command = command[:-1] + ["review_coverage_preserves_valid_ordinary_carry_forward"]
    carried_path = output / "carried-production/production-trace.json"
    carried_path.unlink(missing_ok=True)
    carried_fixture = subprocess.run(carried_command, cwd=root, capture_output=True, text=True)
    (output / "carried-production-fixture.log").write_text(
        "$ " + " ".join(carried_command) + "\n" +
        carried_fixture.stdout + carried_fixture.stderr)
    carried_ok = carried_fixture.returncode == 0 and carried_path.is_file()
    results.append({"case": "public-carried-review-fixture", "pass": carried_ok,
                    "log": "carried-production-fixture.log"})
    stale_command = command[:-1] + ["workflow_landing_rejects_stale_carried_review"]
    stale_fixture = subprocess.run(stale_command, cwd=root, capture_output=True, text=True)
    (output / "stale-carried-review-fixture.log").write_text(
        "$ " + " ".join(stale_command) + "\n" +
        stale_fixture.stdout + stale_fixture.stderr)
    results.append({"case": "public-stale-carried-review-landing",
                    "pass": stale_fixture.returncode == 0,
                    "log": "stale-carried-review-fixture.log"})
    if carried_ok:
        carried = json.loads(carried_path.read_text())
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                              capture_output=True, text=True, check=True).stdout.strip()
        binary = root / "target/debug/fluent"
        bound = carried["fluent-commit"] == head and binary.is_file() and \
            carried["fluent-binary-sha256"] == hashlib.sha256(binary.read_bytes()).hexdigest()
        projected = subprocess.run(["python3", str(source / "project_evidence.py"),
                                    str(carried_path)], capture_output=True, text=True)
        (output / "carried-production-projection.log").write_text(
            projected.stdout + projected.stderr)
        results.append({"case": "public-carried-review-projection",
                        "pass": bound and projected.returncode == 0,
                        "log": "carried-production-projection.log"})
        future_dir = output / "real-review-binds-later-writer"
        future_dir.mkdir(exist_ok=True)
        changed = json.loads(json.dumps(carried))
        for entry in carried["artifacts"]:
            (future_dir / entry["path"]).write_bytes(
                (carried_path.parent / entry["path"]).read_bytes())
        work = json.loads((future_dir / "ready-work.json").read_text())
        tasks = work["attempts"][0]["tasks"]
        first_write = next(task for task in tasks if task["kind"] == "write")
        later_write = next(task for task in tasks if task["kind"] == "write" and
                           task["id"] != first_write["id"])
        review = next(task for task in tasks if task["kind"] == "review" and
                      task["role"] == "tests" and
                      task["review_context"]["candidate_commit"] ==
                      first_write["output"]["commit"])
        review["review_context"]["candidate_commit"] = later_write["output"]["commit"]
        data = json.dumps(work).encode()
        (future_dir / "ready-work.json").write_bytes(data)
        next(entry for entry in changed["artifacts"] if entry["path"] ==
             "ready-work.json")["sha256"] = hashlib.sha256(data).hexdigest()
        changed["events"][0]["raw-facts"]["work"] = work
        future_path = future_dir / "trace.json"
        future_path.write_text(json.dumps(changed))
        rejected = subprocess.run(["python3", str(source / "project_evidence.py"),
                                   str(future_path)], capture_output=True, text=True)
        (output / "real-review-binds-later-writer.log").write_text(
            rejected.stdout + rejected.stderr)
        results.append({"case": "real-review-binds-later-writer",
                        "pass": rejected.returncode != 0 and
                        "ordinary review producing Writer differs" in rejected.stderr,
                        "log": "real-review-binds-later-writer.log"})
    if fixture_ok:
        trace = json.loads(trace_path.read_text())
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                              capture_output=True, text=True, check=True).stdout.strip()
        binary = root / "target/debug/fluent"
        bound = trace["fluent-commit"] == head and binary.is_file() and \
            trace["fluent-binary-sha256"] == hashlib.sha256(binary.read_bytes()).hexdigest()
        projection = subprocess.run(["python3", str(source / "project_evidence.py"),
                                     str(trace_path)], capture_output=True, text=True)
        (output / "projection.log").write_text(projection.stdout + projection.stderr)
        valid = bound and projection.returncode == 0
        results.append({"case": "evidence-production-projection", "pass": valid,
                        "log": "projection.log"})
        controls = (
            ("wrong-review-candidate", "ready-work.json", lambda value: next(
                task for task in value["attempts"][0]["tasks"] if task["kind"] == "review" and
                task["role"] == "tests")["review_context"].update(candidate_commit="0" * 40),
             "ordinary review producing Writer differs"),
            ("wrong-review-context", "ready-work.json", lambda value: next(
                task for task in value["attempts"][0]["tasks"] if task["kind"] == "review" and
                task["role"] == "tests")["review_context"].update(source_branch="other"),
             "ordinary review binding differs"),
            ("wrong-review-authority", "ready-work.json", lambda value: next(
                task for task in value["attempts"][0]["tasks"] if task["kind"] == "review" and
                task["role"] == "tests")["workspace_access"]["reads"][0].update(path="../other"),
             "ordinary review authority differs"),
            ("failed-review-verdict", "review-attempt-1-review-tests.md",
             lambda value: value.replace("Verdict: pass", "Verdict: fail"),
             "ordinary review verdict is missing or not passing"),
            ("wrong-candidate", "landing-results.json", lambda value: value.update(
                candidate_commit="0" * 40), "candidate binding differs"),
            ("wrong-command", "landing-results.json", lambda value: value["commands"][0]
             .update(command="forged command"), "differs from its occurrence"),
            ("old-display", "shown-work.json", lambda value: value["evidence-history"]
             .update(**{"tester-results": None, "diagnostic": "old Attempt only"}),
             "display did not retain"),
        )
        for label, artifact, mutate, diagnostic in controls:
            negative_dir = output / label
            negative_dir.mkdir(exist_ok=True)
            changed = json.loads(json.dumps(trace))
            for entry in trace["artifacts"]:
                (negative_dir / entry["path"]).write_bytes((output / entry["path"]).read_bytes())
            value = ((negative_dir / artifact).read_text() if label == "failed-review-verdict"
                     else json.loads((negative_dir / artifact).read_text()))
            changed_value = mutate(value)
            data = (changed_value.encode() if label == "failed-review-verdict"
                    else json.dumps(value).encode())
            (negative_dir / artifact).write_bytes(data)
            for entry in changed["artifacts"]:
                if entry["path"] == artifact:
                    entry["sha256"] = hashlib.sha256(data).hexdigest()
            if label == "old-display":
                changed["events"][3]["raw-facts"]["show"] = value
            if artifact == "ready-work.json":
                changed["events"][0]["raw-facts"]["work"] = value
            control_path = negative_dir / "trace.json"
            control_path.write_text(json.dumps(changed))
            negative = subprocess.run(["python3", str(source / "project_evidence.py"),
                                       str(control_path)], capture_output=True, text=True)
            (output / f"{label}.log").write_text(negative.stdout + negative.stderr)
            results.append({"case": label, "pass": negative.returncode != 0 and
                            diagnostic in negative.stderr, "log": f"{label}.log"})
        for label, paths, expected_pass in (
            ("carried-review-valid", [".fluent/expertise/learned.md"], True),
            ("carried-review-stale", ["tests/corrected.rs"], False),
            ("review-binds-later-writer", [".fluent/expertise/learned.md"], False),
        ):
            control_dir = output / label
            control_dir.mkdir(exist_ok=True)
            carried = json.loads(json.dumps(trace))
            for entry in trace["artifacts"]:
                (control_dir / entry["path"]).write_bytes((output / entry["path"]).read_bytes())
            work = json.loads((control_dir / "ready-work.json").read_text())
            tasks = work["attempts"][0]["tasks"]
            previous = "1" * 40
            tasks[0]["output"]["commit"] = previous
            for task in tasks:
                if task["kind"] == "review" and task.get("evidence_review_context") is None:
                    task["review_context"]["candidate_commit"] = previous
            correction = json.loads(json.dumps(tasks[0]))
            correction["id"] = "attempt-1-write-carried-control"
            correction["output"]["commit"] = work["merge_candidates"][0]["candidate_commit"]
            tasks.insert(next(i for i, task in enumerate(tasks) if task["kind"] == "tester"), correction)
            if label == "review-binds-later-writer":
                next(task for task in tasks if task["kind"] == "review" and
                     task["role"] == "tests")["review_context"]["candidate_commit"] = correction["output"]["commit"]
            delta = [{"from": previous, "to": correction["output"]["commit"], "paths": paths}]
            for name, value in (("ready-work.json", work), ("writer-deltas.json", delta)):
                data = json.dumps(value).encode()
                (control_dir / name).write_bytes(data)
                next(entry for entry in carried["artifacts"] if entry["path"] == name)["sha256"] = hashlib.sha256(data).hexdigest()
            diff_bytes = ("\n".join(paths) + "\n").encode()
            (control_dir / "writer-diff-1.txt").write_bytes(diff_bytes)
            carried["artifacts"].append({"path": "writer-diff-1.txt",
                                         "sha256": hashlib.sha256(diff_bytes).hexdigest()})
            carried["events"][0]["raw-facts"]["work"] = work
            control_path = control_dir / "trace.json"
            control_path.write_text(json.dumps(carried))
            checked = subprocess.run(["python3", str(source / "project_evidence.py"),
                                      str(control_path)], capture_output=True, text=True)
            (output / f"{label}.log").write_text(checked.stdout + checked.stderr)
            results.append({"case": label, "pass": (checked.returncode == 0) == expected_pass and
                            (expected_pass or ("ordinary review producing Writer differs" if
                             label == "review-binds-later-writer" else
                             "ordinary review binding differs") in checked.stderr),
                            "log": f"{label}.log"})
        missing = json.loads(json.dumps(trace))
        missing["events"] = missing["events"][:-1]
        missing_path = output / "missing-observation.json"
        missing_path.write_text(json.dumps(missing))
        negative = subprocess.run(["python3", str(source / "project_evidence.py"),
                                   str(missing_path)], capture_output=True, text=True)
        (output / "missing-observation.log").write_text(negative.stdout + negative.stderr)
        results.append({"case": "missing-observation", "pass": negative.returncode != 0 and
                        "missing evidence events" in negative.stderr,
                        "log": "missing-observation.log"})
        if valid:
            states = json.loads(projection.stdout)["states"]
            config = output / "evidence-production-path.cfg"
            config.write_text("SPECIFICATION EvidenceSpec\nCONSTANTS\n"
                              "  Works = {w1}\n  EvidenceWork = w1\n  ReviewOnly = {}\n"
                              "  MaxFailures = 0\n"
                              + '  CanonicalKinds = {"pass"}\n'
                              + '  LandingKinds = {"pass"}\n'
                              + '  TargetKinds = {"unchanged"}\n'
                              + '  CorruptPrewrite = FALSE\n'
                              + "".join(f"  {control} = TRUE\n" for control in
                                        ("DeliverInputs", "CheckEvidence", "FenceResume",
                                         "GuardEffect", "TruthfulObservation", "GuardCleanup",
                                         "GuardVersion", "GuardBaseline",
                                         "GuardLandingBaseline", "GuardReviewRoute"))
                              + "".join(f"  EvidenceState{i} = {json.dumps(state)}\n"
                                        for i, state in enumerate(states, 1))
                              + "INVARIANT EvidenceSafe\n"
                              + "INVARIANT LandingAcceptanceSafe\n"
                              + "INVARIANT PublicationSafe\n"
                              + "INVARIANT ObservationSafe\n"
                              + "PROPERTY EvidenceComplete\n")
            results.append(run_case(source, output, jar, java, str(config), None,
                                    module="EvidenceTrace"))
    result = {"schema-version": 1, "suite": "evidence-status", "cases": results,
              "pass": all(row["pass"] for row in results)}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    for row in results:
        print(("PASS" if row["pass"] else "FAIL") + " " + row["case"])
    return 0 if result["pass"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", required=True)
    parser.add_argument("--jar", type=Path, required=True)
    parser.add_argument("--focus", choices=("combined-one-failure", "guarded"))
    args = parser.parse_args()
    if args.suite not in ("foundation", "writer-inputs", "evidence-status"):
        parser.error(f"unknown or incomplete suite: {args.suite}")
    jar = args.jar.resolve()
    if not jar.is_file() or hashlib.sha256(jar.read_bytes()).hexdigest() != JAR_SHA256:
        parser.error("pinned TLC 1.7.4 JAR is missing or has the wrong SHA-256")
    source = Path(__file__).resolve().parent
    root = source.parents[2]
    java = java_21(root)
    if java is None:
        parser.error("Java 21 is required")
    if args.suite == "writer-inputs":
        if args.focus:
            parser.error("--focus is only supported for foundation")
        output = root / "target/formal/workflow/writer-inputs"
        output.mkdir(parents=True, exist_ok=True)
        return writer_inputs_suite(source, root, output, jar, java)
    if args.suite == "evidence-status":
        if args.focus:
            parser.error("--focus is only supported for foundation")
        output = root / "target/formal/workflow/evidence-status"
        output.mkdir(parents=True, exist_ok=True)
        return evidence_status_suite(source, root, output, jar, java)
    output = root / "target/formal/workflow/foundation"
    output.mkdir(parents=True, exist_ok=True)
    if args.focus:
        focused = ([run_case(source, output, jar, java,
                             "combined-one-failure.cfg", None),
                    run_case(source, output, jar, java,
                             "combined-one-failure-witness.cfg",
                             "Invariant RecoveredPublicationUnreachable is violated"),
                    run_case(source, output, jar, java,
                             "combined-recovered-correction-witness.cfg",
                             "Invariant RecoveredCorrectionUnreachable is violated"),
                    run_case(source, output, jar, java,
                             "combined-pending-observation-witness.cfg",
                             "Invariant InterruptedPendingObservationUnreachable is violated"),
                    run_case(source, output, jar, java,
                             "combined-observed-recovery-witness.cfg",
                             "Invariant ObservedRecoveryPublicationUnreachable is violated"),
                    run_case(source, output, jar, java,
                             "combined-observed-recovery-excluded.cfg", None)]
                   if args.focus == "combined-one-failure" else
                   guarded_cases(source, output, jar, java))
        for row in focused:
            print(("PASS" if row["pass"] else "FAIL") + " " + row["case"])
        return 0 if all(row["pass"] for row in focused) else 1
    results = []
    for config, expected in [("foundation.cfg", None),
                             ("combined-one-failure.cfg", None),
                             ("combined-one-failure-witness.cfg",
                              "Invariant RecoveredPublicationUnreachable is violated"),
                             ("combined-recovered-correction-witness.cfg",
                              "Invariant RecoveredCorrectionUnreachable is violated"),
                             ("combined-pending-observation-witness.cfg",
                              "Invariant InterruptedPendingObservationUnreachable is violated"),
                             ("combined-observed-recovery-witness.cfg",
                              "Invariant ObservedRecoveryPublicationUnreachable is violated"),
                             ("combined-observed-recovery-excluded.cfg", None),
                             ("repeated-recovery.cfg", None),
                             ("review-only.cfg", None),
                             ("service-competition.cfg", None),
                             ("known-failure-accepted.cfg",
                              "Invariant KnownFailureUnreachable is violated"),
                             ("known-landing-accepted.cfg",
                              "Invariant KnownLandingUnreachable is violated"),
                             *[(f"negative-{name}.cfg", violation)
                               for name, violation in CONTROLS.items()]]:
        results.append(run_case(source, output, jar, java, config, expected))
        print(("PASS" if results[-1]["pass"] else "FAIL") + " " + config)
    for row in guarded_cases(source, output, jar, java):
        results.append(row)
        print(("PASS" if row["pass"] else "FAIL") + " " + row["case"])
    command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
               "--test", "workflow_assurance", "--no-tests", "fail",
               "workflow_foundation_public_trace"]
    trace_path = output / "production-trace.json"
    for name in ("production-trace.json", "intake.json", "planned.json",
                 "canceled.json", "refused.json"):
        (output / name).unlink(missing_ok=True)
    fixture = subprocess.run(command, cwd=root, capture_output=True, text=True)
    (output / "production-fixture.log").write_text("$ " + " ".join(command) + "\n"
                                                    + fixture.stdout + fixture.stderr)
    fixture_ok = fixture.returncode == 0 and trace_path.is_file()
    results.append({"case": "fresh-production-fixture", "pass": fixture_ok,
                    "log": "production-fixture.log"})
    if fixture_ok:
        trace = json.loads(trace_path.read_text())
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                              capture_output=True, text=True, check=True).stdout.strip()
        binary = root / "target/debug/fluent"
        bound = trace["fluent-commit"] == head and binary.is_file() and \
            trace["fluent-binary-sha256"] == hashlib.sha256(binary.read_bytes()).hexdigest()
        projection = subprocess.run(["python3", str(source / "project.py"), str(trace_path)],
                                    capture_output=True, text=True)
        (output / "projection.log").write_text(projection.stdout + projection.stderr)
        valid = bound and projection.returncode == 0
        results.append({"case": "production-projection", "pass": valid,
                        "log": "projection.log"})
        if valid:
            states = json.loads(projection.stdout)["states"]
            results.append(run_trace_case(source, output, jar, java,
                                          "production-model-path", states, True))
        corrupt = json.loads(json.dumps(trace))
        corrupt["events"][3]["raw-facts"]["work"]["attempts"][0]["status"] = "complete"
        negative_dir = output / "negative-trace"
        negative_dir.mkdir(exist_ok=True)
        for name in ("intake.json", "planned.json", "canceled.json"):
            (negative_dir / name).write_bytes((output / name).read_bytes())
        refused = json.dumps(corrupt["events"][3]["raw-facts"]["work"]).encode()
        (negative_dir / "refused.json").write_bytes(refused)
        corrupt["artifacts"][3]["sha256"] = hashlib.sha256(refused).hexdigest()
        corrupt_path = negative_dir / "negative-trace.json"
        corrupt_path.write_text(json.dumps(corrupt))
        negative = subprocess.run(["python3", str(source / "project.py"), str(corrupt_path),
                                   "--model-input"],
                                  capture_output=True, text=True)
        (output / "negative-trace.log").write_text(negative.stdout + negative.stderr)
        results.append({"case": "negative-trace-projection", "pass": negative.returncode == 0,
                        "log": "negative-trace.log"})
        if negative.returncode == 0:
            results.append(run_trace_case(source, output, jar, java,
                                          "negative-model-path",
                                          json.loads(negative.stdout)["states"], False))
    result = {"schema-version": 1, "suite": args.suite, "cases": results,
              "pass": all(row["pass"] for row in results)}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return 0 if result["pass"] else 1


if __name__ == "__main__":
    sys.exit(main())

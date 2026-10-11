#!/usr/bin/env python3
"""Check workflow models, defect controls, and fresh public-command traces."""

import argparse
import hashlib
import json
import os
import re
import shutil
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
    "approval-connection.cfg": ("Intent", "Cancel", "Effect"),
    "approval-assumptions.cfg": ("Intent", "Cancel", "Effect"),
    "version-connection.cfg": ("Refuse",),
    "version-assumptions.cfg": ("Refuse",),
    "release-ownership-connection.cfg": ("RequestLegacyStop", "SettleA", "SettleB", "ReleaseLegacy", "StartCurrent", "ReuseCurrent", "StaleStop"),
    "release-ownership-assumptions.cfg": ("SettleA", "SettleB", "ReleaseLegacy", "StartCurrent"),
    "completion-connection.cfg": ("WritePart", "Crash", "Retry", "Publish", "Replay"),
    "completion-assumptions.cfg": ("WritePart", "Crash", "Retry", "Publish"),
    "requirements-connection.cfg": ("Deliver",),
    "requirements-assumptions.cfg": ("Deliver",),
    "authorization-connection.cfg": ("Intake", "Authorize", "Enqueue", "Repeat"),
    "authorization-assumptions.cfg": ("Intake", "Authorize", "Enqueue"),
    "evidence-connection.cfg": ("Attach", "Review", "Credit", "Result", "Tester"),
    "evidence-assumptions.cfg": ("Attach", "Review", "Credit", "Tester"),
    "learning-connection.cfg": ("Retry", "Refuse", "Capture", "NoExpertise", "Ready"),
    "learning-assumptions.cfg": ("Retry", "Refuse", "Capture", "NoExpertise"),
    "isolation-connection.cfg": ("Poll", "NextPoll"),
    "isolation-assumptions.cfg": ("Poll", "NextPoll"),
    "landing-connection.cfg": ("Intent", "Effect", "Publish", "Crash", "Recover", "FailWrite", "RecoverIO", "Replay", "FollowUp", "Cleanup", "Observe"),
    "landing-assumptions.cfg": ("Intent", "Effect", "Publish", "Advance", "IndependentWrite", "Replay"),
    "service-connection.cfg": ("Start", "Reuse", "Claim", "Receipt", "Execute", "Drain", "Settle", "Publish", "Release", "Crash", "Replay"),
    "service-assumptions.cfg": ("Start", "Claim", "Receipt", "Execute", "Publish", "Crash"),
    "review-connection.cfg": ("Test", "StartReview", "FinishReview", "Credit", "Ready", "Crash", "Cancel", "Recover", "Resume"),
    "review-assumptions.cfg": ("Test", "StartReview", "Crash", "Resume"),
    "child-connection.cfg": ("Reserve", "Receipt", "Execute", "Crash", "Stop",
                             "Signal", "RecoverChild", "Replace", "Resume"),
    "child-assumptions.cfg": ("Reserve", "Receipt", "Execute", "Crash", "Stop", "Signal"),
    "generation-connection.cfg": ("Stop", "Read", "Replace", "Resume"),
    "generation-assumptions.cfg": ("Stop", "Read", "Replace", "Resume"),
    "queue-recovery-connection.cfg": ("Crash", "Restart", "Recover", "Dispatch"),
    "queue-recovery-assumptions.cfg": ("Crash", "Restart", "Recover", "Dispatch"),
    "queue-dispatch-connection.cfg": ("Elect", "Claim", "Acquire", "Bind",
                                      "Dispatch", "Finish", "Settle"),
    "queue-dispatch-assumptions.cfg": ("Elect", "Claim", "Acquire", "Bind",
                                       "Dispatch"),
    "no-change-production.cfg": ("NoChange", "Attach", "Resume"),
    "pending-plan-production.cfg": ("NoChange", "Attach", "Resume"),
    "joint-settlement.cfg": ("Launch", "RequestStop", "Crash", "Restart",
                             "Settle", "Publish"),
    "recovery-production.cfg": ("Repair", "Run"),
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


def candidate_commit(root):
    return subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                          check=True, capture_output=True, text=True).stdout.strip()


def candidate_binary_digest(root):
    return hashlib.sha256((root / "target/debug/fluent").read_bytes()).hexdigest()


def integrated_production_suite(source, root, output, jar, java):
    """Refresh and independently check combined public workflow traces."""
    directory = output / "production"
    directory.mkdir(parents=True, exist_ok=True)
    for name in ("production-trace.json", "queued-work.json", "foreground-work.json",
                 "queue.txt", "status.txt", "refused.txt", "head-before.txt",
                 "head-after.txt", "queue-after-git.txt", "queued-after-git.json",
                 "foreground-after-git.json"):
        (directory / name).unlink(missing_ok=True)
    command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
               "--test", "workflow_assurance", "--no-tests", "fail",
               "workflow_integrated_queue_survives_other_attempt_cancellation"]
    fixture = subprocess.run(command, cwd=root, capture_output=True, text=True)
    (output / "production-fixture.log").write_text(
        "$ " + " ".join(command) + "\n" + fixture.stdout + fixture.stderr)
    trace_path = directory / "production-trace.json"
    produced = fixture.returncode == 0 and trace_path.is_file()
    rows = [{"case": "fresh-queue-cancel", "pass": produced,
             "log": "production-fixture.log"}]
    review_directory = directory / "review-only"
    review_directory.mkdir(exist_ok=True)
    for name in ("production-trace.json", "planned.json", "complete.json",
                 "head-before.txt", "head-after.txt", "merge-candidates.txt"):
        (review_directory / name).unlink(missing_ok=True)
    review_command = ["cargo", "nextest", "run", "--offline", "--features",
                      "test-support", "--test", "workflow_assurance", "--no-tests",
                      "fail", "workflow_integrated_review_only_orders_tester_before_reviews"]
    review_fixture = subprocess.run(review_command, cwd=root, capture_output=True, text=True)
    (output / "review-only-fixture.log").write_text(
        "$ " + " ".join(review_command) + "\n" +
        review_fixture.stdout + review_fixture.stderr)
    review_path = review_directory / "production-trace.json"
    review_produced = review_fixture.returncode == 0 and review_path.is_file()
    rows.append({"case": "fresh-review-only", "pass": review_produced,
                 "log": "review-only-fixture.log"})
    if review_produced:
        try:
            review_trace = json.loads(review_path.read_text())
            review_bound = review_trace["fluent-commit"] == candidate_commit(root) and \
                review_trace["fluent-binary-sha256"] == candidate_binary_digest(root)
        except (ValueError, KeyError, OSError):
            review_bound = False
        command = ["python3", str(source / "project_review_only.py"), str(review_path)]
        projected = subprocess.run(command, cwd=root, capture_output=True, text=True)
        (output / "review-only-projection.log").write_text(
            "$ " + " ".join(command) + "\n" + projected.stdout + projected.stderr)
        rows.append({"case": "review-only-projection",
                     "pass": review_bound and projected.returncode == 0,
                     "log": "review-only-projection.log"})
        if review_bound and projected.returncode == 0:
            negative_dir = review_directory / "order-negative"
            negative_dir.mkdir(exist_ok=True)
            for entry in review_trace["artifacts"]:
                shutil.copyfile(review_directory / entry["path"],
                                negative_dir / entry["path"])
            changed = json.loads(json.dumps(review_trace))
            complete = changed["events"][1]["raw-facts"]["work"]
            complete["attempts"][0]["tasks"][1]["started_at"] = \
                "2000-01-01T00:00:00+00:00"
            raw = json.dumps(complete).encode()
            (negative_dir / "complete.json").write_bytes(raw)
            for entry in changed["artifacts"]:
                if entry["path"] == "complete.json":
                    entry["sha256"] = hashlib.sha256(raw).hexdigest()
            control = negative_dir / "production-trace.json"
            control.write_text(json.dumps(changed))
            command = ["python3", str(source / "project_review_only.py"), str(control)]
            rejected = subprocess.run(command, cwd=root, capture_output=True, text=True)
            (output / "review-only-order-negative.log").write_text(
                "$ " + " ".join(command) + "\n" +
                rejected.stdout + rejected.stderr)
            rows.append({"case": "review-only-order-negative",
                         "pass": rejected.returncode != 0 and
                         "review started before Tester completed" in rejected.stderr,
                         "log": "review-only-order-negative.log"})
    for config, violation in (("queue-dispatch-connection.cfg", None),
                              ("queue-dispatch-assumptions.cfg", None),
                              ("queue-dispatch-early.cfg",
                               "Invariant MappedDispatch is violated"),
                              ("queue-dispatch-no-coordinator.cfg",
                               "Invariant ClaimRequiresCoordinator is violated")):
        rows.append(run_case(source, output, jar, java, config, violation,
                             module="QueueDispatchConnection"))
    queue_directory = directory / "queue-dispatch"
    queue_directory.mkdir(exist_ok=True)
    for name in ("production-trace.json", "first-queued.json", "second-queued.json",
                 "first-running.json", "second-running.json", "first-work.json",
                 "second-work.json"):
        (queue_directory / name).unlink(missing_ok=True)
    queue_command = ["cargo", "nextest", "run", "--offline", "--features",
                     "test-support", "--test", "workflow_assurance", "--no-tests",
                     "fail", "workflow_integrated_queue_claim_holds_bound_attempt_lease"]
    queue_fixture = subprocess.run(queue_command, cwd=root, capture_output=True, text=True)
    (output / "queue-dispatch-fixture.log").write_text(
        "$ " + " ".join(queue_command) + "\n" + queue_fixture.stdout + queue_fixture.stderr)
    queue_trace_path = queue_directory / "production-trace.json"
    queue_produced = queue_fixture.returncode == 0 and queue_trace_path.is_file()
    rows.append({"case": "fresh-queue-dispatch", "pass": queue_produced,
                 "log": "queue-dispatch-fixture.log"})
    if queue_produced:
        try:
            queue_trace = json.loads(queue_trace_path.read_text())
            queue_bound = queue_trace["fluent-commit"] == candidate_commit(root) and \
                queue_trace["fluent-binary-sha256"] == candidate_binary_digest(root)
        except (ValueError, KeyError, OSError):
            queue_bound = False
        command = ["python3", str(source / "project_queue_dispatch.py"),
                   str(queue_trace_path)]
        projected = subprocess.run(command, cwd=root, capture_output=True, text=True)
        (output / "queue-dispatch-projection.log").write_text(
            "$ " + " ".join(command) + "\n" + projected.stdout + projected.stderr)
        rows.append({"case": "queue-dispatch-projection",
                     "pass": queue_bound and projected.returncode == 0,
                     "log": "queue-dispatch-projection.log"})
        if queue_bound and projected.returncode == 0:
            for name, mutate, diagnostic in (
                ("queue-dispatch-bound-negative",
                 lambda value: value["events"][1]["raw-facts"]["first"].update(
                     {"bound_attempt_id": "wrong-attempt"}),
                 "bound Attempt identity or claim history differs"),
                ("queue-dispatch-lease-negative",
                 lambda value: value["events"][1]["raw-facts"].update(
                     {"lease-live": False}),
                 "running dispatch lacks a live Attempt lease"),
                ("queue-dispatch-coordinator-negative",
                 lambda value: value["events"][1]["raw-facts"].update(
                     {"coordinator-live": False}),
                 "claim lacks a live project coordinator")):
                changed = json.loads(json.dumps(queue_trace))
                mutate(changed)
                control_dir = queue_directory / name
                control_dir.mkdir(exist_ok=True)
                for entry in changed["artifacts"]:
                    shutil.copyfile(queue_directory / entry["path"],
                                    control_dir / entry["path"])
                if name == "queue-dispatch-bound-negative":
                    raw = json.dumps(changed["events"][1]["raw-facts"]["first"]).encode()
                    (control_dir / "first-running.json").write_bytes(raw)
                    for entry in changed["artifacts"]:
                        if entry["path"] == "first-running.json":
                            entry["sha256"] = hashlib.sha256(raw).hexdigest()
                control = control_dir / "production-trace.json"
                control.write_text(json.dumps(changed))
                command = ["python3", str(source / "project_queue_dispatch.py"),
                           str(control)]
                rejected = subprocess.run(command, cwd=root, capture_output=True,
                                          text=True)
                log = output / f"{name}.log"
                log.write_text("$ " + " ".join(command) + "\n" +
                               rejected.stdout + rejected.stderr)
                rows.append({"case": name,
                             "pass": rejected.returncode != 0 and
                             diagnostic in rejected.stderr, "log": log.name})
    rows.extend(queue_recovery_suite(source, root, output, jar, java))
    rows.extend(generation_connection_suite(source, root, output, jar, java))
    rows.extend(child_connection_suite(source, root, output, jar, java))
    rows.extend(review_connection_suite(source, root, output, jar, java))
    rows.extend(landing_connection_suite(source, root, output, jar, java))
    rows.extend(service_connection_suite(source, output, jar, java))
    rows.extend(release_ownership_connection_suite(source, output, jar, java))
    rows.extend(version_connection_suite(source, output, jar, java))
    rows.extend(isolation_connection_suite(source, root, output, jar, java))
    rows.extend(cleanup_connection_suite(source, root, output, jar, java))
    rows.extend(learning_connection_suite(source, root, output, jar, java))
    rows.extend(targeted_connection_suite(source, root, output, jar, java))
    rows.extend(authorization_connection_suite(source, root, output, jar, java))
    rows.extend(requirements_connection_suite(source, root, output, jar, java))
    rows.extend(completion_connection_suite(source, root, output, jar, java))
    rows.extend(approval_connection_suite(source, root, output, jar, java))
    if produced:
        try:
            trace = json.loads(trace_path.read_text())
            bound = trace["fluent-commit"] == candidate_commit(root) and \
                trace["fluent-binary-sha256"] == candidate_binary_digest(root)
        except (ValueError, KeyError, OSError):
            bound = False
        command = ["python3", str(source / "project_integrated.py"), str(trace_path)]
        projected = subprocess.run(command, cwd=root, capture_output=True, text=True)
        (output / "production-projection.log").write_text(
            "$ " + " ".join(command) + "\n" + projected.stdout + projected.stderr)
        valid = bound and projected.returncode == 0
        rows.append({"case": "queue-cancel-projection", "pass": valid,
                     "log": "production-projection.log"})
        if valid:
            negative_dir = output / "cross-work-negative"
            negative_dir.mkdir(exist_ok=True)
            for entry in trace["artifacts"]:
                shutil.copyfile(directory / entry["path"], negative_dir / entry["path"])
            changed = json.loads(json.dumps(trace))
            for index in (1, 3):
                changed["events"][index]["raw-facts"]["work"]["id"] = "queued"
            bytes_changed = json.dumps(
                changed["events"][1]["raw-facts"]["work"]).encode()
            (negative_dir / "foreground-work.json").write_bytes(bytes_changed)
            for entry in changed["artifacts"]:
                if entry["path"] == "foreground-work.json":
                    entry["sha256"] = hashlib.sha256(bytes_changed).hexdigest()
            negative_path = negative_dir / "production-trace.json"
            negative_path.write_text(json.dumps(changed))
            command = ["python3", str(source / "project_integrated.py"),
                       str(negative_path)]
            rejected = subprocess.run(command, cwd=root, capture_output=True, text=True)
            (output / "cross-work-negative.log").write_text(
                "$ " + " ".join(command) + "\n" + rejected.stdout + rejected.stderr)
            rows.append({"case": "cross-work-negative",
                         "pass": rejected.returncode != 0 and
                         "Work identity changed" in rejected.stderr,
                         "log": "cross-work-negative.log"})
            ambiguous = json.loads(json.dumps(trace))
            ambiguous["events"][2]["causal-predecessors"] = [1]
            ambiguous_path = directory / "ambiguous-order-trace.json"
            ambiguous_path.write_text(json.dumps(ambiguous))
            command = ["python3", str(source / "project_integrated.py"),
                       str(ambiguous_path)]
            rejected = subprocess.run(command, cwd=root, capture_output=True, text=True)
            (output / "ambiguous-order-negative.log").write_text(
                "$ " + " ".join(command) + "\n" + rejected.stdout + rejected.stderr)
            rows.append({"case": "ambiguous-order-negative",
                         "pass": rejected.returncode != 0 and
                         "missing or ambiguous causal predecessor" in rejected.stderr,
                         "log": "ambiguous-order-negative.log"})
            changed_work_dir = output / "post-git-work-negative"
            changed_work_dir.mkdir(exist_ok=True)
            for entry in trace["artifacts"]:
                shutil.copyfile(directory / entry["path"],
                                changed_work_dir / entry["path"])
            changed = json.loads(json.dumps(trace))
            work = changed["events"][4]["raw-facts"]["queued-work"]
            work["title"] = "changed"
            work_bytes = json.dumps(work).encode()
            (changed_work_dir / "queued-after-git.json").write_bytes(work_bytes)
            for entry in changed["artifacts"]:
                if entry["path"] == "queued-after-git.json":
                    entry["sha256"] = hashlib.sha256(work_bytes).hexdigest()
            changed_path = changed_work_dir / "production-trace.json"
            changed_path.write_text(json.dumps(changed))
            command = ["python3", str(source / "project_integrated.py"),
                       str(changed_path)]
            rejected = subprocess.run(command, cwd=root, capture_output=True, text=True)
            (output / "post-git-work-negative.log").write_text(
                "$ " + " ".join(command) + "\n" + rejected.stdout + rejected.stderr)
            rows.append({"case": "post-git-work-negative",
                         "pass": rejected.returncode != 0 and
                         "independent Git change altered retained Work or queue" in rejected.stderr,
                         "log": "post-git-work-negative.log"})
            changed_queue_dir = output / "post-git-queue-negative"
            changed_queue_dir.mkdir(exist_ok=True)
            for entry in trace["artifacts"]:
                shutil.copyfile(directory / entry["path"],
                                changed_queue_dir / entry["path"])
            changed = json.loads(json.dumps(trace))
            queue_bytes = b"0 changed queued changed\n"
            (changed_queue_dir / "queue-after-git.txt").write_bytes(queue_bytes)
            changed["events"][4]["raw-facts"]["queue"] = queue_bytes.decode()
            for entry in changed["artifacts"]:
                if entry["path"] == "queue-after-git.txt":
                    entry["sha256"] = hashlib.sha256(queue_bytes).hexdigest()
            changed_path = changed_queue_dir / "production-trace.json"
            changed_path.write_text(json.dumps(changed))
            command = ["python3", str(source / "project_integrated.py"),
                       str(changed_path)]
            rejected = subprocess.run(command, cwd=root, capture_output=True, text=True)
            (output / "post-git-queue-negative.log").write_text(
                "$ " + " ".join(command) + "\n" + rejected.stdout + rejected.stderr)
            rows.append({"case": "post-git-queue-negative",
                         "pass": rejected.returncode != 0 and
                         "independent Git change altered retained Work or queue" in rejected.stderr,
                         "log": "post-git-queue-negative.log"})
    no_change = directory / "no-change"
    no_change.mkdir(exist_ok=True)
    for path in no_change.iterdir():
        if path.is_file():
            path.unlink()
    command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
               "--test", "binary", "--no-tests", "fail", "-E",
               "test(=combined_evidence_recovery_resumes_to_ready_candidate)"]
    environment = dict(os.environ)
    environment["FLUENT_INTEGRATED_NO_CHANGE_TRACE"] = str(no_change)
    fixture = subprocess.run(command, cwd=root, env=environment,
                             capture_output=True, text=True)
    (output / "no-change-fixture.log").write_text(
        "$ " + " ".join(command) + "\n" + fixture.stdout + fixture.stderr)
    path = no_change / "no-change-trace.json"
    produced = fixture.returncode == 0 and path.is_file()
    rows.append({"case": "fresh-no-change-pending-canonical", "pass": produced,
                 "log": "no-change-fixture.log"})
    if produced:
        try:
            trace = json.loads(path.read_text())
            bound = trace["fluent-commit"] == candidate_commit(root) and \
                trace["fluent-binary-sha256"] == candidate_binary_digest(root)
        except (ValueError, KeyError, OSError):
            bound = False
        command = ["python3", str(source / "project_no_change.py"), str(path)]
        projected = subprocess.run(command, cwd=root, capture_output=True, text=True)
        (output / "no-change-projection.log").write_text(
            "$ " + " ".join(command) + "\n" + projected.stdout + projected.stderr)
        valid = bound and projected.returncode == 0
        rows.append({"case": "no-change-projection", "pass": valid,
                     "log": "no-change-projection.log"})
        if valid:
            states = json.loads(projected.stdout)["states"]
            config = output / "no-change-production.cfg"
            config.write_text("SPECIFICATION Spec\nCONSTANTS\n" +
                              "".join(f"  Trace{i}{key} = " +
                                      ("TRUE" if value is True else "FALSE" if value is False
                                       else json.dumps(value)) + "\n"
                                      for i, state in enumerate(states, 1)
                                      for key, value in state.items()) +
                              "INVARIANT TraceSafe\nPROPERTY TraceComplete\n")
            rows.append(run_case(source, output, jar, java, str(config), None,
                                 "NoChangeTrace"))
            wrong_candidate = output / "no-change-wrong-candidate.cfg"
            binding = f'Trace4candidate = "{states[3]["candidate"]}"'
            assert config.read_text().count(binding) == 1
            wrong_candidate.write_text(config.read_text().replace(
                binding, 'Trace4candidate = "wrong"'))
            rows.append(run_case(source, output, jar, java,
                                 str(wrong_candidate),
                                 "Invariant TraceSafe is violated",
                                 "NoChangeTrace"))
            negative_dir = directory / "no-change-negative"
            negative_dir.mkdir(exist_ok=True)
            for entry in trace["artifacts"]:
                shutil.copyfile(no_change / entry["path"],
                                negative_dir / entry["path"])
            changed = json.loads(json.dumps(trace))
            ready = changed["events"][-1]["raw-facts"]["work"]
            ready["attempts"][0]["evidence_recoveries"] = []
            artifact = negative_dir / "ready.json"
            artifact.write_text(json.dumps(ready))
            changed["artifacts"][-1]["sha256"] = hashlib.sha256(
                artifact.read_bytes()).hexdigest()
            negative_path = negative_dir / "no-change-trace.json"
            negative_path.write_text(json.dumps(changed))
            command = ["python3", str(source / "project_no_change.py"),
                       str(negative_path)]
            rejected = subprocess.run(command, cwd=root, capture_output=True, text=True)
            (output / "no-change-negative.log").write_text(
                "$ " + " ".join(command) + "\n" + rejected.stdout + rejected.stderr)
            rows.append({"case": "no-change-missing-proof-negative",
                         "pass": rejected.returncode != 0 and
                         "canonical proof or completed recovery history disappeared" in rejected.stderr,
                         "log": "no-change-negative.log"})
    pending_plan = directory / "pending-plan"
    pending_plan.mkdir(exist_ok=True)
    for path in pending_plan.iterdir():
        if path.is_file():
            path.unlink()
    command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
               "--test", "binary", "--no-tests", "fail", "-E",
               "test(=integrated_no_change_writer_continues_pending_plan_proof)"]
    environment = dict(os.environ)
    environment["FLUENT_INTEGRATED_PENDING_PLAN_TRACE"] = str(pending_plan)
    fixture = subprocess.run(command, cwd=root, env=environment,
                             capture_output=True, text=True)
    (output / "pending-plan-fixture.log").write_text(
        "$ " + " ".join(command) + "\n" + fixture.stdout + fixture.stderr)
    path = pending_plan / "no-change-trace.json"
    produced = fixture.returncode == 0 and path.is_file()
    rows.append({"case": "fresh-no-change-pending-plan", "pass": produced,
                 "log": "pending-plan-fixture.log"})
    if produced:
        try:
            trace = json.loads(path.read_text())
            bound = trace["fluent-commit"] == candidate_commit(root) and \
                trace["fluent-binary-sha256"] == candidate_binary_digest(root)
        except (ValueError, KeyError, OSError):
            bound = False
        command = ["python3", str(source / "project_pending_plan.py"), str(path)]
        projected = subprocess.run(command, cwd=root, capture_output=True, text=True)
        (output / "pending-plan-projection.log").write_text(
            "$ " + " ".join(command) + "\n" + projected.stdout + projected.stderr)
        valid = bound and projected.returncode == 0
        rows.append({"case": "pending-plan-projection", "pass": valid,
                     "log": "pending-plan-projection.log"})
        if valid:
            states = json.loads(projected.stdout)["states"]
            config = output / "pending-plan-production.cfg"
            config.write_text("SPECIFICATION Spec\nCONSTANTS\n" +
                              "".join(f"  Trace{i}{key} = " +
                                      ("TRUE" if value is True else "FALSE" if value is False
                                       else json.dumps(value)) + "\n"
                                      for i, state in enumerate(states, 1)
                                      for key, value in state.items()) +
                              "INVARIANT TraceSafe\nPROPERTY TraceComplete\n")
            rows.append(run_case(source, output, jar, java, str(config), None,
                                 "PendingPlanTrace"))
            wrong_proof = output / "pending-plan-wrong-proof.cfg"
            assert config.read_text().count("Trace4proofComplete = TRUE") == 1
            wrong_proof.write_text(config.read_text().replace(
                "Trace4proofComplete = TRUE", "Trace4proofComplete = FALSE"))
            rows.append(run_case(source, output, jar, java, str(wrong_proof),
                                 "Invariant TraceSafe is violated",
                                 "PendingPlanTrace"))
            negative_dir = directory / "pending-plan-negative"
            negative_dir.mkdir(exist_ok=True)
            for entry in trace["artifacts"]:
                shutil.copyfile(pending_plan / entry["path"],
                                negative_dir / entry["path"])
            changed = json.loads(json.dumps(trace))
            ready = changed["events"][-1]["raw-facts"]["work"]
            ready["attempts"][0]["evidence_recoveries"] = []
            artifact = negative_dir / "ready.json"
            artifact.write_text(json.dumps(ready))
            changed["artifacts"][-1]["sha256"] = hashlib.sha256(
                artifact.read_bytes()).hexdigest()
            negative_path = negative_dir / "no-change-trace.json"
            negative_path.write_text(json.dumps(changed))
            command = ["python3", str(source / "project_pending_plan.py"),
                       str(negative_path)]
            rejected = subprocess.run(command, cwd=root, capture_output=True, text=True)
            (output / "pending-plan-negative.log").write_text(
                "$ " + " ".join(command) + "\n" + rejected.stdout + rejected.stderr)
            rows.append({"case": "pending-plan-missing-proof-negative",
                         "pass": rejected.returncode != 0 and
                         "pending Plan proof or recovery history disappeared" in rejected.stderr,
                         "log": "pending-plan-negative.log"})
    result = {"schema-version": 1, "suite": "integrated-production",
              "candidate-commit": candidate_commit(root),
              "fluent-binary-sha256": candidate_binary_digest(root), "cases": rows,
              "pass": all(row["pass"] for row in rows)}
    (output / "production-result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def named_test_passed(run):
    """Credit exactly one executed named test from its nextest result."""
    if run is None or run.returncode != 0:
        return False
    summaries = re.findall(r"^\s*Summary[^\n]*", run.stdout + "\n" + run.stderr,
                           re.MULTILINE)
    if len(summaries) != 1:
        return False
    # Accept nextest's singular and legacy plural forms, but require one pass
    # and no contradictory outcomes. A successful exit alone is insufficient.
    return re.fullmatch(
        r"\s*Summary\s+\[[^\]\n]+\]\s+1 tests? run: 1 passed(?:, \d+ skipped)?\s*",
        summaries[0]) is not None



def integrated_suite(source, root, output, jar, java):
    """Run each maintained suite, then retain a fail-closed candidate report."""
    results = []
    for name in ("foundation", "writer-inputs", "evidence-status",
                 "recovery-observation", "release-transition"):
        command = ["python3", str(source / "check.py"), "--suite=" + name,
                   "--jar=" + str(jar)]
        result = subprocess.run(command, cwd=root, capture_output=True, text=True)
        log = output / (name + ".log")
        log.write_text("$ " + " ".join(command) + "\n" + result.stdout + result.stderr)
        results.append({"case": name, "pass": result.returncode == 0,
                        "log": log.name})
    production = integrated_production_suite(source, root, output, jar, java)
    results.append({"case": "integrated-production", "pass": production["pass"],
                    "log": "production-fixture.log"})
    scheduler = source.parent / "scheduler/check.py"
    for name in ("recovery", "lifecycle", "launch", "landing"):
        (root / "target/formal/scheduler" / f"{name}-result.json").unlink(missing_ok=True)
        command = ["python3", str(scheduler), "--suite=" + name,
                   "--jar=" + str(jar)]
        environment = dict(os.environ)
        java_path = Path(shutil.which(java) or java).resolve()
        environment["PATH"] = str(java_path.parent) + os.pathsep + environment.get("PATH", "")
        result = subprocess.run(command, cwd=root, env=environment,
                                capture_output=True, text=True)
        log = output / ("scheduler-" + name + ".log")
        log.write_text("$ " + " ".join(command) + "\n" + result.stdout + result.stderr)
        results.append({"case": "scheduler-" + name, "pass": result.returncode == 0,
                        "log": log.name})
    contracts = json.loads((source / "approved-contracts.json").read_text())
    named = [test for row in contracts["behaviors"] for test in row["tests"]]
    named_rows = []
    for binary_name in ("binary", "scheduler_service"):
        selected = [test for test in named if test["binary"] == "fluent::" + binary_name]
        expression = " | ".join(f'test(={test["name"]})' for test in selected)
        base = ["cargo", "nextest"]
        list_command = base + ["list", "--offline", "--features", "test-support", "--test",
                               binary_name, "-E", expression, "--message-format", "json"]
        listed = subprocess.run(list_command, cwd=root, capture_output=True, text=True)
        discovered = set()
        if listed.returncode == 0:
            try:
                listing = json.loads(listed.stdout)
                cases = listing["rust-suites"]["fluent::" + binary_name]["testcases"]
                discovered = {name for name, case in cases.items()
                              if case["filter-match"]["status"] == "matches"}
            except (KeyError, TypeError, ValueError):
                pass
        complete = discovered == {test["name"] for test in selected}
        discovery_log = output / ("named-discovery-" + binary_name + ".log")
        discovery_log.write_text("$ " + " ".join(list_command) + "\n" +
                                 listed.stdout + listed.stderr)
        environment = dict(os.environ)
        environment["FLUENT_WORKFLOW_REQUIRE_HOST"] = "1"
        for test in selected:
            command = base + ["run", "--offline", "--features", "test-support", "--test",
                              binary_name, "--no-tests", "fail", "-E",
                              f'test(={test["name"]})']
            run = subprocess.run(command, cwd=root, env=environment, capture_output=True,
                                 text=True) if complete else None
            log = output / ("named-" + test["name"] + ".log")
            log.write_text("$ " + " ".join(command) + "\n" + run.stdout + run.stderr
                           if run is not None else "named test discovery incomplete\n")
            passed = named_test_passed(run)
            named_rows.append({"binary": test["binary"], "name": test["name"],
                               "pass": passed, "log": log.name,
                               "log-sha256": hashlib.sha256(log.read_bytes()).hexdigest()})
    named_receipt = {"schema-version": 1, "candidate-commit": candidate_commit(root),
                     "fluent-binary-sha256": candidate_binary_digest(root),
                     "cases": named_rows, "pass": all(row["pass"] for row in named_rows)}
    (output / "named-tests-result.json").write_text(json.dumps(named_receipt, indent=2) + "\n")
    results.append({"case": "approved-named-tests", "pass": named_receipt["pass"],
                    "log": "named-tests-result.json"})
    binary = root / "target/debug/fluent"
    if binary.is_file():
        command = ["python3", str(source / "report.py"), "--results",
                   str(output.parent), "--candidate", candidate_commit(root),
                   "--binary", str(binary), "--jar", str(jar),
                   "--java", str(Path(shutil.which(java) or java).resolve()),
                   "--output", str(output / "report.json")]
        report = subprocess.run(command, cwd=root, capture_output=True, text=True)
        (output / "report.log").write_text("$ " + " ".join(command) + "\n" +
                                           report.stdout + report.stderr)
        results.append({"case": "candidate-report", "pass": report.returncode == 0,
                        "log": "report.log"})
    else:
        results.append({"case": "candidate-report", "pass": False,
                        "reason": "ordinary candidate binary is missing"})
    summary = {"schema-version": 1, "suite": "integrated",
               "candidate-commit": candidate_commit(root), "cases": results,
               "pass": all(row["pass"] for row in results)}
    (output / "result.json").write_text(json.dumps(summary, indent=2) + "\n")
    return 0 if summary["pass"] else 1


def review_connection_suite(source, root, output, jar, java):
    return observed_connection_suite(
        source, root, output, jar, java, "review-restart", "ReviewConnection",
        (("review-connection.cfg", None), ("review-assumptions.cfg", None),
         ("review-early.cfg", "Invariant TesterFirst is violated"),
         ("review-retest.cfg", "Invariant TesterRetained is violated")),
        "workflow_integrated_review_only_resumes_after_interruption", None,
        (("replaced-tester", 0, lambda f: f["work"]["attempts"][0]["tasks"][0].update(
            {"completed_at": "2000-01-01T00:00:00Z"}), "restart replaced completed Tester history"),
         ("stale-resume", 1, lambda f: f["work"]["attempts"][0]["cancellation"].update(
            {"resumed_at": "unauthorized"}), "restart lacks current resume authority")),
        binary="workflow_assurance", trace_file="restart-trace.json")


def child_connection_suite(source, root, output, jar, java):
    return observed_connection_suite(
        source, root, output, jar, java, "child", "ChildConnection",
        (("child-connection.cfg", None), ("child-assumptions.cfg", None),
         ("child-no-receipt.cfg", "Invariant NoUnrecordedExecution is violated"),
         ("child-wrong-generation.cfg", "Invariant NoWrongSignal is violated"),
         ("child-wrong-owner.cfg", "Invariant NoWrongOwnerSignal is violated")),
        "workflow_integrated_child_receipt_survives_parent_crash", "FLUENT_INTEGRATED_CHILD_TRACE",
        (("missing-receipt", 0, lambda f: f.update({"receipt": ""}),
          "missing or malformed child receipt"),
         ("wrong-generation", 3, lambda f: f["record"].update({"launch_generation": "0" * 32}),
          "receipt generation does not bind execution"),
         ("wrong-owner", 3, lambda f: f["record"]["attempt_owner"].update({"work_item_id": "other"}),
          "execution owner changed"),
         ("unsettled", 3, lambda f: f.update({"process-gone": False}),
          "cancellation did not settle owned execution")))


def generation_connection_suite(source, root, output, jar, java):
    return observed_connection_suite(
        source, root, output, jar, java, "generation", "GenerationConnection",
        (("generation-connection.cfg", None), ("generation-assumptions.cfg", None),
         ("generation-stale.cfg", "Invariant NoStaleResume is violated")),
        "workflow_recovery_requires_current_resume_authority", "FLUENT_INTEGRATED_GENERATION_TRACE",
        (("stale-stop", 3, lambda f: f["work"]["attempts"][0]["cancellation"].update(
            {"resumed_at": "unauthorized"}), "stale authority cleared the later stop"),
         ("wrong-authority", 4, lambda f: f["dispatch"].update(
            {"resume_cancellation_requested_at": "old-stop"}),
          "resume dispatch lacks current cancellation authority"),
         ("wrong-refusal", 3, lambda f: f.update({"stderr": "unrelated failure"}),
          "stale dispatch changed Work or lacked the expected refusal")))


def approval_connection_suite(source, root, output, jar, java):
    return observed_connection_suite(
        source, root, output, jar, java, "approval", "ApprovalConnection",
        (("approval-connection.cfg", None), ("approval-assumptions.cfg", None),
         ("approval-candidate.cfg", "Invariant CurrentApproval is violated"),
         ("approval-stop.cfg", "Invariant CurrentStop is violated")),
        "workflow_integrated_landing_rechecks_current_approval", "FLUENT_INTEGRATED_APPROVAL_TRACE",
        (("scope", 1, lambda f: f["work"]["merge_candidates"][0]["merge_state"]["landing_intent"].update(
            {"merge_candidate_id": "other"}), "approval intent scope differs"),
         ("effect", 3, lambda f: f.update({"head": "0" * 40}), "canceled landing applied an effect"),
         ("authority", 3, lambda f: f["work"]["attempts"][0]["cancellation"].update({"resumed_at": "old-authority"}),
          "landing retry ignored current cancellation")))


def completion_connection_suite(source, root, output, jar, java):
    return observed_connection_suite(
        source, root, output, jar, java, "completion", "CompletionConnection",
        (("completion-connection.cfg", None), ("completion-assumptions.cfg", None),
         ("completion-early.cfg", "Invariant NoEarlyAuthority is violated"),
         ("completion-history.cfg", "Invariant RetainedHistory is violated"),
         ("completion-replay.cfg", "Invariant OneReservation is violated")),
        "writer_completion_context_repaired_retry_replays_publication", "FLUENT_INTEGRATED_COMPLETION_TRACE",
        (("identity", 0, lambda f: f["repeated"].update({"id": "other"}), "publication replay changed retained identity"),
         ("history", 1, lambda f: f["original"]["attempts"][0]["tasks"][0].update({"instructions": "changed"}),
          "publication rewrote completed history")))


def requirements_connection_suite(source, root, output, jar, java):
    def collide(fact):
        matrix = json.loads(fact["matrix"])
        matrix["rows"][0]["id"] = "behavior:public-workflow-continuation:b20"
        fact["matrix"] = fact["observed"] = json.dumps(matrix)
    return observed_connection_suite(
        source, root, output, jar, java, "requirements", "RequirementConnection",
        (("requirements-connection.cfg", None), ("requirements-assumptions.cfg", None),
         ("requirements-collision.cfg", "Invariant AllIdentities is violated")),
        "workflow_integrated_continuation_requirements_reach_writer", "FLUENT_INTEGRATED_REQUIREMENTS_TRACE",
        (("collision", 0, collide, "section-qualified identities were lost"),
         ("delivery", 0, lambda f: f.update({"observed": "{}"}), "Writer did not read the current matrix")))


def authorization_connection_suite(source, root, output, jar, java):
    return observed_connection_suite(
        source, root, output, jar, java, "authorization", "AuthorizationConnection",
        (("authorization-connection.cfg", None), ("authorization-assumptions.cfg", None),
         ("authorization-forged.cfg", "Invariant AuthorizedAdmission is violated")),
        "human_authorization_transitions_same_work_item", "FLUENT_INTEGRATED_AUTHORIZATION_TRACE",
        (("forged", 2, lambda f: f["work"]["authorization"].update({"authority": "automatic"}),
          "queue admission lacks current human authority"),
         ("scope", 2, lambda f: f["work"]["corrective_context"].update({"included_scope": "other"}),
          "authorization changed proposed scope or origin")))


def targeted_connection_suite(source, root, output, jar, java):
    return observed_connection_suite(
        source, root, output, jar, java, "targeted", "EvidenceConnection",
        (("evidence-connection.cfg", None), ("evidence-assumptions.cfg", None),
         ("evidence-scope.cfg", "Invariant ScopedCredit is violated"),
         ("evidence-stale.cfg", "Invariant CurrentCredit is violated")),
        "canonical_tester_recovery_reuses_other_passing_results", "FLUENT_INTEGRATED_TARGETED_TRACE",
        (("scope", 1, lambda f: f["work"]["attempts"][0]["evidence_recoveries"][0]["required_check"]["source"].update(
            {"command_index": 0}), "targeted credit escaped exact command or candidate"),
         ("stale", 2, lambda f: f["work"]["merge_candidates"][0].update({"candidate_commit": "0" * 40}),
          "ready candidate differs from reviewed check"),
         ("verdict", 2, lambda f: f.update({"review": "Verdict: fail\n"}), "targeted review lacks scoped verdict")))


def learning_connection_suite(source, root, output, jar, java):
    rows = observed_connection_suite(
        source, root, output, jar, java, "learning_capture", "LearningConnection",
        (("learning-connection.cfg", None), ("learning-assumptions.cfg", None),
         ("learning-candidate.cfg", "Invariant CandidateSafe is violated"),
         ("learning-confinement.cfg", "Invariant ConfinementSafe is violated"),
         ("learning-mode.cfg", "Action property ModeStable is violated")),
        "learner_retry_completes_existing_record_idempotently", "FLUENT_INTEGRATED_LEARNING_CAPTURE_TRACE",
        (("mode", 1, lambda f: f["work"].update({"learner_mode": "no-expertise"}), "retry changed learner mode or Work identity"),
         ("handoff", 1, lambda f: f["handoff"].update({"source_attempt_id": "other"}), "handoff differs from retained learning")))
    rows.extend(observed_connection_suite(
        source, root, output, jar, java, "learning_no_expertise", "LearningConnection", (),
        "pre_land_no_expertise_retry_runs_only_learner_and_preserves_mode", "FLUENT_INTEGRATED_LEARNING_NO_EXPERTISE_TRACE",
        (("mutation", 1, lambda f: f.update({"changed": "src/unauthorized.rs"}), "no-expertise retry changed reviewed candidate"),)))
    path = output / "production/learning_no_expertise/production-trace.json"
    try:
        final = json.loads(path.read_text())["events"][-1]["raw-facts"]
        confined = final["confinement"] == "available" and final["work"]["attempts"][0]["learning"]["status"] == "succeeded"
    except (OSError, ValueError, KeyError, IndexError, TypeError):
        confined = False
    log = output / "learning-confined-success.log"
    log.write_text("Confined no-expertise retry succeeded.\n" if confined else
                   "Required successful no-expertise retry unavailable; fail-closed observations do not replace this host check.\n")
    rows.append({"case": "learning-no-expertise-confined-success", "pass": confined, "log": log.name})
    return rows


def cleanup_connection_suite(source, root, output, jar, java):
    return observed_connection_suite(
        source, root, output, jar, java, "cleanup", "LandingConnection",
        (("landing-cleanup.cfg", "Invariant CleanupSafe is violated"),
         ("landing-observation.cfg", "Invariant ObservationSafe is violated")),
        "workflow_cleanup_retains_pending_recovery", "FLUENT_INTEGRATED_CLEANUP_TRACE",
        (("lost-evidence", 1, lambda f: f.update({"artifacts-present": False}), "cleanup removed pending recovery state"),
         ("status", 0, lambda f: f["shown"]["merge_candidates"][0]["merge_state"].pop("follow_up_failure"),
          "status differs from retained state"),
         ("publication", 2, lambda f: f.update({"observations": []}), "cleanup recovery did not publish retained origin")))


def isolation_connection_suite(source, root, output, jar, java):
    return observed_connection_suite(
        source, root, output, jar, java, "isolation", "IsolationConnection",
        (("isolation-connection.cfg", None), ("isolation-assumptions.cfg", None),
         ("isolation-dropped.cfg", "Temporal properties were violated")),
        "scheduler_isolation_continues_healthy_projects", "FLUENT_INTEGRATED_ISOLATION_TRACE",
        (("dropped", 0, lambda f: f.update({"calls": []}), "healthy project was not dispatched"),
         ("corrupt", 1, lambda f: f.update({"bad-work": "rewritten"}), "corrupt project was rewritten or dropped"),
         ("identity", 2, lambda f: f["good"].update({"bound_attempt_id": "other"}), "healthy Attempt binding differs")),
        binary="scheduler_service")


def version_connection_suite(source, output, jar, java):
    return [run_case(source, output, jar, java, config, expected, "VersionConnection")
            for config, expected in (
                ("version-connection.cfg", None), ("version-assumptions.cfg", None),
                ("version-write.cfg", "Invariant NoUnsupportedMutation is violated"),
                ("version-state.cfg", "Invariant RetainedState is violated"))]


def version_trace_case(source, output, jar, java, scenario, projection):
    mapping = projection["mapping"]
    config = output / f"{scenario}-connection.cfg"
    config.write_text('SPECIFICATION Spec\nCONSTANTS\n  CheckVersion = TRUE\n' +
                      '  Refused = ' + ('TRUE' if mapping["refused"] else 'FALSE') + '\n' +
                      '  BeforeDigest = ' + json.dumps(mapping["before-digest"]) + '\n' +
                      '  AfterDigest = ' + json.dumps(mapping["after-digest"]) + '\n' +
                      '  PendingPublication = ' + ('TRUE' if mapping["pending-publication"] else 'FALSE') + '\n' +
                      'INVARIANTS NoUnsupportedMutation RetainedState\nPROPERTY ObservedRefusal\nCHECK_DEADLOCK FALSE\n')
    return run_case(source, output, jar, java, str(config), None, "VersionConnection")


def release_ownership_trace_case(source, output, jar, java, projection):
    mapping = projection["mapping"]
    def booleans(name, values):
        assert len(values) == 2, "release mapping requires both projects"
        return "".join("  " + name + suffix + " = " + ("TRUE" if value else "FALSE") + "\n"
                       for suffix, value in zip(("A", "B"), values))
    config = output / "service-ownership-production.cfg"
    config.write_text("SPECIFICATION TraceSpec\nCONSTANTS\n" +
                      "  SettleLegacyChildren = TRUE\n  CheckNewBuild = TRUE\n  CheckNewStop = TRUE\n" +
                      booleans("BeforeHeld", mapping["children-held-before"]) +
                      booleans("AfterHeld", mapping["children-held-after"]) +
                      booleans("Completed", mapping["children-completed"]) +
                      "  NewOwner = " + ("TRUE" if mapping["current-owner"] else "FALSE") + "\n" +
                      "  StaleRefused = " + ("TRUE" if mapping["stale-stop-refused"] else "FALSE") + "\n" +
                      "INVARIANTS Observed NoOverlap CurrentIdentity SettledBeforeStart\n" +
                      "PROPERTY TraceComplete\nCHECK_DEADLOCK FALSE\n")
    return run_case(source, output, jar, java, str(config), None, "ReleaseOwnershipTrace")


def release_ownership_connection_suite(source, output, jar, java):
    rows = [run_case(source, output, jar, java, config, expected, "ReleaseOwnershipConnection")
            for config, expected in (
                ("release-ownership-connection.cfg", None), ("release-ownership-assumptions.cfg", None),
                ("release-ownership-unsettled.cfg", "Invariant NoOverlap is violated"),
                ("release-ownership-build.cfg", "Invariant CurrentIdentity is violated"),
                ("release-ownership-stop.cfg", "Invariant CurrentIdentity is violated"))]
    # These exercise the adapter, not the physical released-service fixture.
    rows.extend(run_case(source, output, jar, java, config, expected, "ReleaseOwnershipTrace")
                for config, expected in (("release-ownership-trace.cfg", None),
                    ("release-ownership-trace-held.cfg", "Invariant Observed is violated")))
    return rows


def service_connection_suite(source, output, jar, java):
    # The released-service production evidence remains owned by the separate
    # host release suite; this writer suite supplies only the joint mapping.
    return [run_case(source, output, jar, java, config, expected, "ServiceConnection")
            for config, expected in (
                ("service-connection.cfg", None), ("service-assumptions.cfg", None),
                ("service-unsettled.cfg", "Invariant ChildBeforePublication is violated"),
                ("service-stale-stop.cfg", "Invariant OwnerBound is violated"),
                ("service-wrong-build.cfg", "Invariant OwnerBound is violated"))]


def landing_connection_suite(source, root, output, jar, java):
    return observed_connection_suite(
        source, root, output, jar, java, "landing", "LandingConnection",
        (("landing-connection.cfg", None), ("landing-assumptions.cfg", None),
         ("landing-unbound.cfg", "Invariant NoUnboundEffect is violated"),
         ("landing-replay.cfg", "Invariant OneEffect is violated"),
         ("landing-independent.cfg", "Invariant RetainIndependent is violated")),
        "workflow_recovery_preserves_intervening_changes", "FLUENT_INTEGRATED_LANDING_TRACE",
        (("unbound", 1, lambda f: f["work"]["merge_candidates"][0]["merge_state"]["landing_intent"].update(
            {"verified_commit": "0" * 40}), "Git effect lacks exact retained intent"),
         ("replay", 3, lambda f: f.update({"head": "0" * 40}), "replay applied another Git effect"),
         ("independent", 5, lambda f: f.update({"file": "overwritten"}), "replay overwrote independent state"),
         ("follow-up", 5, lambda f: f.update({"observations": []}), "follow-up publication lacks retained origin")))


def observed_connection_suite(source, root, output, jar, java, name, module,
                              configs, test, variable, controls,
                              binary="binary", trace_file="production-trace.json"):
    """Run a public fixture, bind its observations, and rehash defect controls."""
    rows = [run_case(source, output, jar, java, config, expected, module)
            for config, expected in configs]
    directory = output / "production" / name
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / trace_file
    path.unlink(missing_ok=True)
    command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
               "--test", binary, "--no-tests", "fail", "-E", f"test(={test})"]
    environment = dict(os.environ)
    if variable is not None:
        environment[variable] = str(directory)
    fixture = subprocess.run(command, cwd=root, env=environment, capture_output=True, text=True)
    log = output / f"{name}-fixture.log"
    log.write_text("$ " + " ".join(command) + "\n" + fixture.stdout + fixture.stderr)
    produced = fixture.returncode == 0 and path.is_file()
    rows.append({"case": f"fresh-{name}", "pass": produced, "log": log.name})
    if not produced:
        return rows
    try:
        trace = json.loads(path.read_text())
        bound = trace["fluent-commit"] == candidate_commit(root) and \
            trace["fluent-binary-sha256"] == candidate_binary_digest(root)
    except (ValueError, KeyError, OSError):
        bound = False
    command = ["python3", str(source / ("project_" + name.replace("-", "_") + ".py")), str(path)]
    projected = subprocess.run(command, cwd=root, capture_output=True, text=True)
    log = output / f"{name}-projection.log"
    log.write_text("$ " + " ".join(command) + "\n" + projected.stdout + projected.stderr)
    rows.append({"case": f"{name}-projection",
                 "pass": bound and projected.returncode == 0, "log": log.name})
    if not bound or projected.returncode != 0:
        return rows
    for label, index, mutate, diagnostic in controls:
        changed = json.loads(json.dumps(trace))
        mutate(changed["events"][index]["raw-facts"])
        control_dir = directory / label
        control_dir.mkdir(exist_ok=True)
        for artifact in (*trace["artifacts"], *trace["events"][-1]["raw-facts"].get("continued", {}).get("artifacts", [])):
            shutil.copyfile(directory / artifact["path"], control_dir / artifact["path"])
        artifact = changed["artifacts"][index]
        raw = json.dumps(changed["events"][index]["raw-facts"]).encode()
        (control_dir / artifact["path"]).write_bytes(raw)
        artifact["sha256"] = hashlib.sha256(raw).hexdigest()
        control = control_dir / "production-trace.json"
        control.write_text(json.dumps(changed))
        command = ["python3", str(source / ("project_" + name.replace("-", "_") + ".py")), str(control)]
        rejected = subprocess.run(command, cwd=root, capture_output=True, text=True)
        log = output / f"{name}-{label}-negative.log"
        log.write_text("$ " + " ".join(command) + "\n" + rejected.stdout + rejected.stderr)
        rows.append({"case": f"{name}-{label}-negative", "pass": rejected.returncode != 0 and
                     diagnostic in rejected.stderr, "log": log.name})
    return rows


def queue_recovery_suite(source, root, output, jar, java):
    """Check retained identity and fences across repeated scheduler crashes."""
    rows = []
    for config, expected in (
            ("queue-recovery-connection.cfg", None),
            ("queue-recovery-assumptions.cfg", None),
            ("queue-recovery-duplicate.cfg", "Invariant SameAttempt is violated"),
            ("queue-recovery-live-lease.cfg", "Invariant CurrentLease is violated")):
        rows.append(run_case(source, output, jar, java, config, expected,
                             "QueueDispatchConnection"))
    directory = output / "production/queue-recovery"
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / "recovery-trace.json"
    path.unlink(missing_ok=True)
    command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
               "--test", "workflow_assurance", "--no-tests", "fail",
               "workflow_integrated_queue_recovery_fences_old_generation"]
    fixture = subprocess.run(command, cwd=root, capture_output=True, text=True)
    log = output / "queue-recovery-fixture.log"
    log.write_text("$ " + " ".join(command) + "\n" + fixture.stdout + fixture.stderr)
    produced = fixture.returncode == 0 and path.is_file()
    rows.append({"case": "fresh-queue-recovery", "pass": produced, "log": log.name})
    if not produced:
        return rows
    try:
        trace = json.loads(path.read_text())
        bound = trace["fluent-commit"] == candidate_commit(root) and \
            trace["fluent-binary-sha256"] == candidate_binary_digest(root)
    except (ValueError, KeyError, OSError):
        bound = False
    command = ["python3", str(source / "project_queue_recovery.py"), str(path)]
    projected = subprocess.run(command, cwd=root, capture_output=True, text=True)
    log = output / "queue-recovery-projection.log"
    log.write_text("$ " + " ".join(command) + "\n" + projected.stdout + projected.stderr)
    rows.append({"case": "queue-recovery-projection",
                 "pass": bound and projected.returncode == 0, "log": log.name})
    if not bound or projected.returncode != 0:
        return rows
    for name, index, mutate, diagnostic in (
            ("queue-recovery-attempt-negative", 1,
             lambda f: f["work"]["attempts"][0].update({"id": "duplicate-attempt"}),
             "recovery changed bound Attempt"),
            ("queue-recovery-lease-negative", 0,
             lambda f: f.update({"lease-after-crash": True}),
             "crash did not release the Attempt lease"),
            ("queue-recovery-generation-negative", 1,
             lambda f: f["dispatch"].update({"generation": 2}),
             "recovery did not fence the previous generation"),
            ("queue-recovery-stale-negative", 2,
             lambda f: f["stale-settlements"][0]["after"].update({"status": "failed"}),
             "stale settlement changed the current dispatch")):
        changed = json.loads(json.dumps(trace))
        mutate(changed["events"][index]["raw-facts"])
        control_dir = directory / name
        control_dir.mkdir(exist_ok=True)
        for artifact in (*trace["artifacts"],
                         *trace["events"][2]["raw-facts"]["continued"]["artifacts"]):
            shutil.copyfile(directory / artifact["path"], control_dir / artifact["path"])
        artifact = changed["artifacts"][index]
        raw = json.dumps(changed["events"][index]["raw-facts"]).encode()
        (control_dir / artifact["path"]).write_bytes(raw)
        artifact["sha256"] = hashlib.sha256(raw).hexdigest()
        control = control_dir / "recovery-trace.json"
        control.write_text(json.dumps(changed))
        command = ["python3", str(source / "project_queue_recovery.py"), str(control)]
        rejected = subprocess.run(command, cwd=root, capture_output=True, text=True)
        log = output / f"{name}.log"
        log.write_text("$ " + " ".join(command) + "\n" + rejected.stdout + rejected.stderr)
        rows.append({"case": name, "pass": rejected.returncode != 0 and
                     diagnostic in rejected.stderr, "log": log.name})
    return rows


def run_case(source, output_dir, jar, java, config, expected, module="Workflow"):
    name = Path(config).name
    with tempfile.TemporaryDirectory(prefix="tlc-", dir=output_dir) as metadir:
        command = [java, "-DTLA-Library=" + str(source.parent / "scheduler"),
                   "-XX:+UseParallelGC", "-Xmx512m", "-cp", str(jar),
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
        changed = json.loads(json.dumps(trace))
        directory = output / "wrong-task"
        directory.mkdir(exist_ok=True)
        for artifact in trace["artifacts"]:
            shutil.copyfile(output / artifact["path"], directory / artifact["path"])
        work = json.loads((directory / "writer-work.json").read_text())
        identity = changed["events"][-1]["operation-id"]
        task = next(task for task in work["attempts"][0]["tasks"] if task["id"] == identity)
        task["attempt_id"] = "wrong-attempt"
        raw = json.dumps(work).encode()
        (directory / "writer-work.json").write_bytes(raw)
        changed["events"][-1]["raw-facts"]["work"] = work
        next(a for a in changed["artifacts"] if a["path"] == "writer-work.json")["sha256"] = hashlib.sha256(raw).hexdigest()
        path = directory / "production-trace.json"
        path.write_text(json.dumps(changed))
        rejected = subprocess.run(["python3", str(source / "project_writer.py"), str(path)], capture_output=True, text=True)
        (output / "wrong-task.log").write_text(rejected.stdout + rejected.stderr)
        results.append({"case": "wrong-task-control", "pass": rejected.returncode != 0 and
                        "foreground Task identity or workspace differs" in rejected.stderr,
                        "log": "wrong-task.log"})
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
    result = {"schema-version": 1, "suite": "writer-inputs",
              "candidate-commit": candidate_commit(root),
              "fluent-binary-sha256": candidate_binary_digest(root), "cases": results,
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
            ("attempt-tester-error", "attempt-results.json", lambda value: value.update(
                error="harness failed"), "Attempt Tester has a harness error"),
            ("attempt-tester-command", "attempt-results.json", lambda value: value["commands"][0]
             .update(command="forged command"), "Attempt Tester command 0 differs"),
            ("attempt-tester-failure", "attempt-results.json", lambda value: value["commands"][0]
             .update(exit_code=1), "Attempt Tester command 0 has an unexplained failure"),
            ("attempt-tester-new-failure", "attempt-results.json", lambda value: (
                value["tests"][0].update(status="fail"),
                value["summary"].update(**{"pass": 1, "fail": 1})),
             "Attempt Tester has a new failing test"),
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
        paired_dir = output / "paired-wrong-landing-command"
        paired_dir.mkdir(exist_ok=True)
        paired = json.loads(json.dumps(trace))
        for entry in trace["artifacts"]:
            (paired_dir / entry["path"]).write_bytes((output / entry["path"]).read_bytes())
        for name in ("landing-results.json", "landing-manifest.json"):
            value = json.loads((paired_dir / name).read_text())
            value["commands"][0]["command"] = "forged command"
            data = json.dumps(value).encode()
            (paired_dir / name).write_bytes(data)
            next(entry for entry in paired["artifacts"] if entry["path"] == name)["sha256"] = \
                hashlib.sha256(data).hexdigest()
        paired_path = paired_dir / "trace.json"
        paired_path.write_text(json.dumps(paired))
        rejected = subprocess.run(["python3", str(source / "project_evidence.py"),
                                   str(paired_path)], capture_output=True, text=True)
        (output / "paired-wrong-landing-command.log").write_text(rejected.stdout + rejected.stderr)
        results.append({"case": "paired-wrong-landing-command",
                        "pass": rejected.returncode != 0 and
                        "landing command 0 differs from declared Tester configuration" in rejected.stderr,
                        "log": "paired-wrong-landing-command.log"})
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
    result = {"schema-version": 1, "suite": "evidence-status",
              "candidate-commit": candidate_commit(root),
              "fluent-binary-sha256": candidate_binary_digest(root), "cases": results,
              "pass": all(row["pass"] for row in results)}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    for row in results:
        print(("PASS" if row["pass"] else "FAIL") + " " + row["case"])
    return 0 if result["pass"] else 1


def recovery_observation_suite(source, root, output, jar, java):
    """Check bounded recovery, child settlement, and fresh public continuation."""
    results = []
    cases = [
        ("repeated-recovery.cfg", None, "Workflow"),
        ("combined-one-failure.cfg", None, "Workflow"),
        ("service-competition.cfg", None, "Workflow"),
        ("negative-fenceresume.cfg", CONTROLS["fenceresume"], "Workflow"),
        ("negative-guardeffect.cfg", CONTROLS["guardeffect"], "Workflow"),
        ("negative-truthfulobservation.cfg", CONTROLS["truthfulobservation"], "Workflow"),
        ("negative-guardcleanup.cfg", CONTROLS["guardcleanup"], "Workflow"),
        ("joint-settlement.cfg", None, "JointSettlement"),
        ("joint-settlement-two-crashes.cfg",
         "Invariant RepeatedCrashUnreachable is violated", "JointSettlement"),
        ("negative-joint-settlement.cfg",
         "Invariant NoEarlyPublication is violated", "JointSettlement"),
    ]
    for config, expected, module in cases:
        row = run_case(source, output, jar, java, config, expected, module)
        results.append(row)
        print(("PASS" if row["pass"] else "FAIL") + " " + row["case"])

    trace_path = output / "production-trace.json"
    for name in ("production-trace.json", "paused-work.json", "authorized-work.json",
                 "continued-work.json", "source-matrix.json"):
        (output / name).unlink(missing_ok=True)
    command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
               "--test", "binary", "--no-tests", "fail",
               "workflow_reachable_failures_have_public_continuation"]
    fixture = subprocess.run(command, cwd=root, capture_output=True, text=True)
    (output / "production-fixture.log").write_text(
        "$ " + " ".join(command) + "\n" + fixture.stdout + fixture.stderr)
    produced = fixture.returncode == 0 and trace_path.is_file()
    results.append({"case": "fresh-public-recovery", "pass": produced,
                    "log": "production-fixture.log"})
    if produced:
        trace = json.loads(trace_path.read_text())
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root,
                              capture_output=True, text=True, check=True).stdout.strip()
        binary = root / "target/debug/fluent"
        bound = trace["fluent-commit"] == head and binary.is_file() and \
            trace["fluent-binary-sha256"] == hashlib.sha256(binary.read_bytes()).hexdigest()
        projection = subprocess.run(["python3", str(source / "project_recovery.py"),
                                     str(trace_path)], capture_output=True, text=True)
        (output / "projection.log").write_text(projection.stdout + projection.stderr)
        valid = bound and projection.returncode == 0
        results.append({"case": "recovery-projection", "pass": valid,
                        "log": "projection.log"})
        if valid:
            states = json.loads(projection.stdout)["states"]
            config = output / "recovery-production.cfg"
            config.write_text("SPECIFICATION Spec\nCONSTANTS\n" +
                              "".join(f"  Trace{i}{key} = " +
                                      ("TRUE" if value is True else "FALSE" if value is False
                                       else json.dumps(value)) + "\n"
                                      for i, state in enumerate(states, 1)
                                      for key, value in state.items()) +
                              "INVARIANT TraceSafe\nPROPERTY TraceComplete\n")
            results.append(run_case(source, output, jar, java, str(config), None,
                                    "RecoveryTrace"))
            # Change only the projected completion commit. The projector has
            # already accepted the source trace, so TLC must reject this
            # impossible Run transition on its own.
            wrong_commit = output / "recovery-wrong-output.cfg"
            correct_binding = f'Trace3outputCommit = "{states[2]["outputCommit"]}"'
            assert config.read_text().count(correct_binding) == 1
            wrong_commit.write_text(config.read_text().replace(
                correct_binding, 'Trace3outputCommit = "wrong"'))
            results.append(run_case(source, output, jar, java, str(wrong_commit),
                                    "Invariant TraceSafe is violated",
                                    "RecoveryTrace"))
        for name, edit in (
            ("stale-action", lambda work: work.__setitem__("next-action", "queue resume")),
            ("missing-transition", lambda work: work["attempts"][0]["tasks"][1]
             .__setitem__("status", "planned")),
            ("misbound-authorization", lambda work: work["attempts"][0]
             ["recovery_operations"][0].__setitem__(
                 "candidate-commit", work["attempts"][0]["tasks"][0]
                 ["output"]["base_commit"])),
        ):
            negative_dir = output / name
            negative_dir.mkdir(exist_ok=True)
            for entry in trace["artifacts"]:
                (negative_dir / entry["path"]).write_bytes(
                    (output / entry["path"]).read_bytes())
            modified = json.loads(json.dumps(trace))
            index = {"stale-action": 0, "missing-transition": 2,
                     "misbound-authorization": 1}[name]
            artifact = modified["artifacts"][index]
            work = json.loads((negative_dir / artifact["path"]).read_text())
            edit(work)
            data = json.dumps(work, sort_keys=True).encode()
            (negative_dir / artifact["path"]).write_bytes(data)
            artifact["sha256"] = hashlib.sha256(data).hexdigest()
            modified["events"][index]["raw-facts"]["work"] = work
            if name == "misbound-authorization":
                later = modified["artifacts"][2]
                later_work = json.loads((negative_dir / later["path"]).read_text())
                edit(later_work)
                later_data = json.dumps(later_work, sort_keys=True).encode()
                (negative_dir / later["path"]).write_bytes(later_data)
                later["sha256"] = hashlib.sha256(later_data).hexdigest()
                modified["events"][2]["raw-facts"]["work"] = later_work
            negative_path = negative_dir / "production-trace.json"
            negative_path.write_text(json.dumps(modified))
            rejected = subprocess.run(["python3", str(source / "project_recovery.py"),
                                       str(negative_path)], capture_output=True, text=True)
            (output / f"{name}.log").write_text(rejected.stdout + rejected.stderr)
            results.append({"case": name, "pass": rejected.returncode != 0,
                            "log": f"{name}.log"})
    result = {"schema-version": 1, "suite": "recovery-observation",
              "candidate-commit": candidate_commit(root),
              "fluent-binary-sha256": candidate_binary_digest(root), "cases": results,
              "pass": all(row["pass"] for row in results)}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return 0 if result["pass"] else 1


def release_transition_suite(source, root, output, jar, java):
    """Recheck fresh release facts and the bounded version guard."""
    results = []
    cases = (
        ("binary", ("released_upgrade_preserves_workflow_state",
                    "released_upgrade_recovers_interrupted_publication",
                    "released_upgrade_recovers_predecessor_interrupted_work",
                    "released_upgrade_preserves_running_before_stop",
                    "released_upgrade_preserves_landed_work")),
        ("binary", ("released_predecessor_refuses_current_authority",
                    "released_predecessor_refuses_pending_current_publication")),
        ("scheduler_service", ("scheduler_released_upgrade_preserves_shared_ownership",)),
    )
    for index, (binary, names) in enumerate(cases, 1):
        command = ["cargo", "nextest", "run", "--offline", "--features", "test-support",
                   "--test", binary, "--no-tests", "fail", *names]
        environment = dict(os.environ)
        if binary == "scheduler_service":
            environment["FLUENT_WORKFLOW_REQUIRE_HOST"] = "1"
        result = subprocess.run(command, cwd=root, env=environment, capture_output=True,
                                text=True)
        name = f"released-fixture-{index}"
        (output / f"{name}.log").write_text("$ " + " ".join(command) + "\n" +
                                             result.stdout + result.stderr)
        results.append({"case": name, "pass": result.returncode == 0,
                        "log": f"{name}.log"})
    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, check=True,
                            capture_output=True, text=True).stdout.strip()
    candidate = root / "target/debug/fluent"
    for scenario in ("current-authority", "pending-publication"):
        path = output / f"{scenario}.json"
        command = ["python3", str(source / "project_release.py"), str(path),
                   str(candidate), commit]
        projection = subprocess.run(command, cwd=root, capture_output=True, text=True)
        (output / f"{scenario}-projection.log").write_text(
            "$ " + " ".join(command) + "\n" + projection.stdout + projection.stderr)
        results.append({"case": f"{scenario}-projection", "pass": projection.returncode == 0,
                        "log": f"{scenario}-projection.log"})
        if projection.returncode == 0:
            results.append(version_trace_case(source, output, jar, java, scenario, json.loads(projection.stdout)))
            corrupted = json.loads(path.read_text())
            corrupted["events"][1]["raw-facts"]["protected"] = []
            negative_path = output / f"{scenario}-negative.json"
            negative_path.write_text(json.dumps(corrupted))
            negative = subprocess.run([*command[:2], str(negative_path), *command[3:]],
                                      cwd=root, capture_output=True, text=True)
            (output / f"{scenario}-negative.log").write_text(
                negative.stdout + negative.stderr)
            results.append({"case": f"{scenario}-negative", "pass": negative.returncode != 0 and
                            "protected snapshot changed" in negative.stderr,
                            "log": f"{scenario}-negative.log"})
            corrupted = json.loads(path.read_text())
            corrupted["artifacts"][0]["sha256"] = "0" * 64
            negative_path = output / f"{scenario}-artifact-negative.json"
            negative_path.write_text(json.dumps(corrupted))
            negative = subprocess.run([*command[:2], str(negative_path), *command[3:]],
                                      cwd=root, capture_output=True, text=True)
            (output / f"{scenario}-artifact-negative.log").write_text(
                negative.stdout + negative.stderr)
            results.append({"case": f"{scenario}-artifact-negative",
                            "pass": negative.returncode != 0 and
                            "artifact digest mismatch" in negative.stderr,
                            "log": f"{scenario}-artifact-negative.log"})
            for label, mutate, diagnostic in (
                    ("actor-negative", lambda trace: trace["events"][1].update(actor="current-reader"),
                     "refusal actor mismatch"),
                    ("operation-negative", lambda trace: trace["events"][1].update({"operation-id": "queue-add"}),
                     "refusal operation mismatch"),
                    ("diagnostic-negative", lambda trace: trace["events"][1]["raw-facts"].update(
                        stderr="unrelated file permission error"), "compatibility diagnostic mismatch")):
                corrupted = json.loads(path.read_text())
                mutate(corrupted)
                negative_path = output / f"{scenario}-{label}.json"
                negative_path.write_text(json.dumps(corrupted))
                negative = subprocess.run([*command[:2], str(negative_path), *command[3:]],
                                          cwd=root, capture_output=True, text=True)
                (output / f"{scenario}-{label}.log").write_text(
                    negative.stdout + negative.stderr)
                results.append({"case": f"{scenario}-{label}",
                                "pass": negative.returncode != 0 and
                                diagnostic in negative.stderr,
                                "log": f"{scenario}-{label}.log"})
    service_trace = output / "service-ownership.json"
    service_command = ["python3", str(source / "project_release.py"), str(service_trace),
                       str(candidate), commit]
    service_projection = subprocess.run(service_command, cwd=root, capture_output=True, text=True)
    (output / "service-projection.log").write_text(
        "$ " + " ".join(service_command) + "\n" +
        service_projection.stdout + service_projection.stderr)
    results.append({"case": "service-projection", "pass": service_projection.returncode == 0,
                    "log": "service-projection.log"})
    if service_projection.returncode == 0:
        results.append(release_ownership_trace_case(source, output, jar, java, json.loads(service_projection.stdout)))
        for name, mutate in (
                ("service-incompatible-negative", lambda trace: trace["events"][0]["raw-facts"]["incompatible-start"].update(exit=0)),
                ("service-project-negative", lambda trace: trace["events"][1]["raw-facts"]["owned-workers"][1].update(work_item_id="first")),
                ("service-identity-negative", lambda trace: trace["events"][2]["raw-facts"]["health"].update(
                    instance_id=trace["events"][0]["raw-facts"]["health"]["instance_id"])),
                ("service-worker-negative", lambda trace: trace["events"][1]["raw-facts"]["owned-workers"][0].update(
                    dispatch_id="unobserved-dispatch")),
                ("service-launch-negative", lambda trace: trace["events"][1]["raw-facts"]["owned-workers"][0]["launches"][0].update(
                    {"process-group-absent": False})),
                ("service-receipt-negative", lambda trace: trace["events"][1]["raw-facts"]["owned-workers"][0]["initial-receipt"].update(
                    {"state": "settled"})),
                ("service-artifact-negative", lambda trace: trace["artifacts"][0].update(
                    {"sha256": "0" * 64})),
                ("service-stale-stop-negative", lambda trace: trace["events"][2]["raw-facts"]["stale-stop"].update(
                    {"requested-coordinator": "unobserved-coordinator"}))):
            corrupted = json.loads(service_trace.read_text())
            mutate(corrupted)
            negative_path = output / f"{name}.json"
            negative_path.write_text(json.dumps(corrupted))
            negative = subprocess.run([*service_command[:2], str(negative_path),
                                       *service_command[3:]], cwd=root,
                                      capture_output=True, text=True)
            (output / f"{name}.log").write_text(negative.stdout + negative.stderr)
            diagnostic = {
                "service-incompatible-negative": "incompatible start replaced legacy ownership",
                "service-project-negative": "legacy settlement requires both registered workers",
                "service-identity-negative": "service identity mismatch",
                "service-worker-negative": "owned dispatch mismatch",
                "service-launch-negative": "worker process group remains",
                "service-receipt-negative": "worker receipt mismatch",
                "service-artifact-negative": "artifact digest mismatch",
                "service-stale-stop-negative": "stale stop authority mismatch",
            }[name]
            results.append({"case": name, "pass": negative.returncode != 0 and
                            diagnostic in negative.stderr,
                            "log": f"{name}.log"})
    results.append(run_case(source, output, jar, java, "service-competition.cfg", None))
    results.append(run_case(source, output, jar, java, "negative-guardversion.cfg",
                            CONTROLS["guardversion"]))
    summary = {"schema-version": 1, "suite": "release-transition",
               "candidate-commit": candidate_commit(root),
               "fluent-binary-sha256": candidate_binary_digest(root), "cases": results,
               "pass": all(row["pass"] for row in results)}
    (output / "result.json").write_text(json.dumps(summary, indent=2) + "\n")
    return 0 if summary["pass"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", required=True)
    parser.add_argument("--jar", type=Path, required=True)
    parser.add_argument("--focus", choices=("combined-one-failure", "guarded"))
    args = parser.parse_args()
    if args.suite not in ("foundation", "writer-inputs", "evidence-status",
                          "recovery-observation", "release-transition", "integrated"):
        parser.error(f"unknown or incomplete suite: {args.suite}")
    jar = args.jar.resolve()
    if not jar.is_file() or hashlib.sha256(jar.read_bytes()).hexdigest() != JAR_SHA256:
        parser.error("pinned TLC 1.7.4 JAR is missing or has the wrong SHA-256")
    source = Path(__file__).resolve().parent
    root = source.parents[2]
    java = java_21(root)
    if java is None:
        parser.error("Java 21 is required")
    if args.suite == "integrated":
        if args.focus:
            parser.error("--focus is only supported for foundation")
        output = root / "target/formal/workflow/integrated"
        output.mkdir(parents=True, exist_ok=True)
        return integrated_suite(source, root, output, jar, java)
    if args.suite == "release-transition":
        if args.focus:
            parser.error("--focus is only supported for foundation")
        output = root / "target/formal/workflow/release-transition"
        output.mkdir(parents=True, exist_ok=True)
        return release_transition_suite(source, root, output, jar, java)
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
    if args.suite == "recovery-observation":
        if args.focus:
            parser.error("--focus is only supported for foundation")
        output = root / "target/formal/workflow/recovery-observation"
        output.mkdir(parents=True, exist_ok=True)
        return recovery_observation_suite(source, root, output, jar, java)
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
    result = {"schema-version": 1, "suite": args.suite,
              "candidate-commit": candidate_commit(root),
              "fluent-binary-sha256": candidate_binary_digest(root), "cases": results,
              "pass": all(row["pass"] for row in results)}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return 0 if result["pass"] else 1


if __name__ == "__main__":
    sys.exit(main())

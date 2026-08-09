---
name: scheduler-resume-requires-explicit-dispatch-intent
description: Scheduler recovery of NeedsUser work requires a new explicit resume dispatch; an Attempt binding alone is not user authorization
metadata:
  type: convention
---

A queue dispatch's `bound_attempt_id` identifies what a worker owns; it does not
prove that a user authorized Fluent to cross a `NeedsUser` pause. Preserve that
distinction in the durable ledger:

1. Keep the dispatch that reached `needs-user` as terminal history.
2. Require an explicit command naming the Work Item and exact Attempt.
3. Append a new dispatch with typed resume intent, the binding already set, and
   a link to the prior dispatch.
4. At claim time, validate the persisted intent and binding against the current
   Work-model aggregate using the same pause-readiness predicate as direct
   Attempt execution.
5. If the resumed run pauses again, terminalize only the new dispatch and
   require another explicit resume.

This shape makes command retries idempotent while the matching dispatch is
active, survives coordinator restart, and prevents ordinary queue replay or a
stale bound id from impersonating human authorization. Tests must prove the
public command, preserved history, exact binding, no extra Attempt, restart,
another `needs-user` outcome, and fail-closed mismatch cases.

Apply the same boundary to a `candidate-gate` pause. When gate evidence says
the candidate was rejected, one explicit resume may plan one corrective Writer
on the same workspace and provider session. When the gate was blocked by the
host environment, resume rechecks the unchanged candidate instead. If either
path reaches `needs-user` again, require another explicit resume dispatch.

Candidate-gate diagnostics must appear as authoritative `current-findings` in
the corrective Writer's generated execution context. Do not rely on the Writer
listing a prior artifact directory: its sandbox may admit one artifact without
admitting parent-directory enumeration, and long absolute paths may be bounded
in the context index.

An older binary may hard-fail the corrective Writer after it returns without a
valid no-change declaration. Accept a resume after a failed dispatch only for
the exact legacy shape: the final Task is a failed Writer with no output, its
input names gate evidence produced by an earlier completed Writer, and that
same evidence remains attached to the Attempt. Replan that Task in place. Keep
every other failed Attempt and failed dispatch non-resumable.

Related: [[needs-user-not-terminal-for-cleanup]],
[[next-action-guidance-is-an-executable-interface]],
[[backward-compatible-serde-fields]], [[route-tests-drive-real-launch-wiring]]

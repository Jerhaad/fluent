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

Related: [[needs-user-not-terminal-for-cleanup]],
[[next-action-guidance-is-an-executable-interface]],
[[backward-compatible-serde-fields]], [[route-tests-drive-real-launch-wiring]]

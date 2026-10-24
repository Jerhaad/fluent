---
name: scheduler-resume-requires-explicit-dispatch-intent
description: Scheduler recovery requires explicit resume intent bound to the exact Attempt and cancellation observed at admission
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

A canceled dispatch is terminal history, not a resumable suspension and not an
active conflict. A later explicit resume may append a fresh dispatch when the
current exact Attempt independently passes the recovery predicate. Do not link
the new dispatch to the canceled entry or require its binding to match; it may
belong to an older Attempt. Preserve the canceled record unchanged, and bind
the new resume before claim.

An Attempt cancellation has its own identity. Queue resume records the exact
`requested_at` it authorizes without clearing the stop, and the scheduler child
carries its dispatch ID to recover that persisted authority. The Attempt loop
compares it with the current cancellation under the Work model lock and requires
settlement before recording resume. A generic resume intent cannot authorize a
later cancellation, and replaying an active dispatch cannot refresh its authority.
Ordinary and legacy dispatches with no cancellation authority cannot clear an
active stop.
Foreground execution captures its authority before initialization for the same
reason: a launch that began before cancellation is not a fresh resume.

This shape makes command retries idempotent while the matching dispatch is
active, survives coordinator restart, and prevents ordinary queue replay or a
stale bound id from impersonating human authorization. Tests must prove the
public command, preserved history, exact binding, no extra Attempt, restart,
another `needs-user` outcome, and fail-closed mismatch cases.

A current pre-land Learner failure uses the same explicit authorization even
though the Attempt is complete and the scheduler recorded `blocked` rather than
`needs-user`. Admit that recovery only when reviews passed, every Task is
complete, the failed Learning disposition is relaunchable, exactly one pending
unreviewed candidate remains clean at its recorded commit, and no Learner lease
is live. Repeat the read-only inspection at claim, then let the Attempt loop
repeat its reservation-time checks and retry only the Learner. Preserve the
prior blocked dispatch, Tasks, review evidence, Tester evidence, and candidate.
Reject a moved, dirty, reviewed, landed, or ambiguous candidate, or one with a
live Learner run, without changing the queue.

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

One older lifecycle wrote a generic Learner failure after its retained Learner
gate found missing final Tester references, then incorrectly projected the
complete Attempt as `candidate-ready`. Accept an explicit resume from that
historical `candidate-ready` state, or from its corrected `blocked` projection,
only when the latest Learner run gate, final Tester results, completion contract,
pending unreviewed candidate, and generic failure all match that exact defect.
Use the durable Learning run counter to locate the gate: capture mode may
preallocate later empty run directories, but any later directory containing
evidence makes the compatibility state ambiguous and non-resumable.
The same read-only compatibility predicate must gate both enqueue and claim.
Execution then normalizes the evidence to the post-Tester gate and changes the
Work model in one fresh lock-held mutation. Every other complete Attempt remains
non-resumable through the queue.

Related: [[needs-user-not-terminal-for-cleanup]],
[[next-action-guidance-is-an-executable-interface]],
[[backward-compatible-serde-fields]], [[route-tests-drive-real-launch-wiring]]

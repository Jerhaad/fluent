---
name: superseded-history-requires-recovery-provenance
description: Admit retained Superseded Tasks to Learner retry only when recovery evidence accounts for every one
metadata:
  type: architecture
---

A Superseded Task can remain in a completed Attempt after supported Writer
recovery. Its terminal status and ordinary Work-model shape alone do not prove
that recovery created it. Before queue admission, claim, or foreground Learner
retry treats the Attempt as complete, require the shared recovery contract to
match every retained Superseded Task to a recorded Writer-completion authority
artifact, a matching recovery operation, and a completed replacement Writer.
The authorized Task set must equal the retained Superseded set; an unmatched or
twice-authorized Task fails closed.

Exercise this rule through the public recovery and retry commands. Build the
positive fixture with supported recovery, then check that retries preserve the
stored Task evidence and Merge Candidate identity. A synthetic Superseded Task
proves only that structural validation accepts its shape, not that the route
admits legitimate recovery history.

Related: [[recovery-contracts-form-neutral-boundary]],
[[test-fixtures-use-production-state]], [[route-tests-drive-real-launch-wiring]].

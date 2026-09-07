---
name: test-fixtures-use-production-state
description: Reproduce production states and persisted aggregates in lifecycle fixtures; use explicit historical evidence for compatibility tests
metadata:
  type: testing
---

The Work model has multiple interacting lifecycle state enums (`AttemptReviewState`, `AttemptStatus`, `MergeReviewState`, `MergeCandidateMergeStatus`). Test helper functions like `make_work_item_with_candidate` accept these as parameters, making it easy to construct states that production code never produces.

When testing a function that reads state (like `find_ready_candidate`), fixtures must use the state values that production code would have set at that point in the lifecycle. For example, a newly created `MergeCandidate` has `MergeReviewState::Pending` — the `Passed` value is only set after a successful merge. A readiness test that passes `Passed` into the fixture is testing an unreachable scenario and can mask real bugs.

The test reviewer checks that fixture state values match production creation paths and will flag unreachable combinations.

Match the persisted aggregate as well as its enum values. Work model mutations
reload stored records before validating relationships between Attempts, Tasks,
and Merge Candidates. A candidate created only in the caller's in-memory copy
can disappear from that validation boundary, making a lifecycle test pass on a
different state from production. For pre-land Learner tests, persist the pending
reviewed candidate before starting the Learner; marking its Attempt Complete
alone does not establish that case. Check preserved candidate bytes and completed
Task history when exercising cancellation and resume.

Compatibility tests deliberately exercise state that an older Fluent version
produced. When candidate admission retires a rejection rule, construct its
historical rejected gate explicitly instead of expecting the current gate to
emit that failure. Assert current admission independently, then persist the
historical status and exact diagnostic bound to the fixture's real candidate
commit. Continue through the real downstream gate and recovery code, retaining
the assertions that protect ancestry, trees, authors, and unchanged state on
rejection.

Check shared recovery fixtures across the candidate gate, ancestry recovery,
and Attempt loop when admission policy changes. A stale setup assertion can
stop every test using that fixture before it reaches the recovery behavior,
even while tests focused on current admission pass.

Related: [[test-names-match-assertions]],
[[legacy-evidence-compatibility-stays-isolated]],
[[semantic-validation-tests-reach-target-branch]]

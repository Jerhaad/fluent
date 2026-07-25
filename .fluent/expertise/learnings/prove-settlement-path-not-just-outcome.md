---
name: prove-settlement-path-not-just-outcome
description: A resolved caller or a green suite proves an outcome, not which path produced it; to prove the intended settlement path, join the raw worker handle before coordinator `Drop`/post-join reconciliation and assert the path-distinguishing error fields, because the worker-panic fallback reconciles before the raw join returns and only `worker_error` distinguishes it
metadata:
  type: testing
---

In the status pump, several paths can resolve the same blocked submitter — the
ordinary wake-disconnect path, the worker panic catch, and the `finish`/`Drop`
fallback reconciler. A test that merely asserts the submitter was resolved (or that
the whole suite is green) proves an *outcome*, not *which path* delivered it, so a
fallback settlement can silently stand in for the path the behavior actually
claims.

To prove the intended path settled the caller, join the *raw* worker handle before
coordinator `Drop`/post-join reconciliation can run, and assert the fields that
distinguish the paths. Joining the raw handle does not exclude the worker-panic
fallback: that fallback reconciles *inside the worker closure, before the raw
`join()` returns*, so the join always observes a resolved caller either way. What
separates the ordinary path from the panic fallback is the settlement's error
fields, not the timing of the observation:

- Join the *raw* worker handle directly, before `Drop`/post-join reconciliation, so
  the assertion sees the result the worker path produced rather than a later
  coordinator-reconciled one.
- Assert the path-distinguishing state — e.g. all four error fields absent for the
  clean wake-disconnect path, `worker_error` in particular — so a panic-catch
  fallback (which returns `worker_error = Some(..)` before the raw join returns)
  fails the assertion instead of passing silently.
- Drive the boundary deterministically with a pre-installed idle barrier, not
  scheduling luck: install the test-only settlement probes *before* the worker
  thread starts (via the hooks-preinstalled spawn), so the worker can never emit a
  boundary probe before the hook exists, and let each probe run with both mutexes
  released.

Related: [[status-pump-abandonment-reconciliation]],
[[production-lock-test-hooks]], [[declared-behavior-tests-must-exist-before-land]],
[[test-names-match-assertions]].

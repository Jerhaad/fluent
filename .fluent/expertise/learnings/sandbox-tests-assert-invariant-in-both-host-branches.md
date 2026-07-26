---
name: sandbox-tests-assert-invariant-in-both-host-branches
description: A confinement test asserts its security invariant unconditionally, then branches on sandbox availability only to add the stronger observation each host permits — never skips when the sandbox is unavailable
metadata:
  type: testing
---

Seatbelt sandbox availability varies by host (it is unavailable in some CI and
runtime environments), so a binary/integration test for a confined mode must not skip
or pass vacuously when the sandbox is absent. The reviewer-approved pattern splits on
host capability without ever weakening the core guarantee:

- Assert the core invariant *unconditionally* in both branches — e.g. the stored
  policy, the candidate commit, the live candidate `HEAD`, and the absence of an
  expertise commit are all preserved whether or not the sandbox is usable.
- Branch only to add the *stronger* observation each host permits: where the sandbox
  is usable, additionally assert the coder's out-of-bounds write was actively denied
  (read the outcome from the transcript surface — e.g. `CANDIDATE_WRITE_DENIED`,
  never `CANDIDATE_WRITE_ALLOWED`); on an unsupported host, additionally assert the
  mode fails closed rather than downgrading to unconfined execution.

The host-conditional split is a robustness pattern, not a vacuous escape hatch: the
security invariant holds in every branch and the branch only tightens the assertion.
A test that asserts confinement *only* inside `if sandbox_usable {}` and does nothing
otherwise is a reviewer finding, because it is green-by-absence on the very hosts
where confinement could silently regress. Drive the real `attempt run` binary path
with a mock coder rather than a unit-level denial helper, so the end-to-end
guarantee — not just an internal function — is exercised. Related:
[[declared-behavior-tests-must-exist-before-land]].

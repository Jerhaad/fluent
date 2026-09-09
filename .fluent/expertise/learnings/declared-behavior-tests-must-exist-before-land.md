---
name: declared-behavior-tests-must-exist-before-land
description: Every Test: reference declared in behaviors.md must resolve to a real, passing test before landing — a green suite does not substitute for a missing production-boundary regression, and behaviors/tests/architecture reviewers all block on the gap
metadata:
  type: testing
---

A green test suite is not evidence that a Work Item is complete. The plan's
completion rule is that no behavior may lack a production-boundary regression, so
every `Test:` reference declared in `documentation/behaviors.md` must resolve to
a test that exists in the tree *and* passes in the tester results — independent of
how many other tests pass.

Fluent now enforces this traceability at two host-owned candidate gates. Before
review, the deterministic gate resolves every approved reference against its
committed project-relative path and native test identifier. The behaviors
reviewer then checks that the named test directly exercises the behavior; code
reviewers run before the final Tester and treat Writer-focused verification as
advisory. After the final Tester, the host gate requires every reference to match
a passing structured Rust-test, Python-test, or shell-script identity for the
exact candidate.
A declared reference with no backing definition therefore blocks before review,
and a defined reference without passing final evidence blocks before Learning,
even when many unrelated tests are green.

When an established failure returns to a corrective Writer and a missing test
identity coincides with a nonzero Tester command, include the command failure
alongside the missing selector. Attach the rejected gate, the exact
`tester-results.json`, and only that command's deterministic
`commands/<index>-stdout.log` and `commands/<index>-stderr.log` files. Grant
those files individually as read-only sandbox inputs and bind all of their bytes
to any round-cap approval. Never trust a persisted result to choose a sandbox
path or grant its parent Tester directory. Availability is not inspection: if a
Writer wants to preserve the candidate after failed Tester evidence, require its
fresh no-change declaration to bind every attached gate, structured result, and
command log by exact path and SHA-256 digest. Validate that set before launching
reviewers, and direct the Writer to inspect candidate-owned `.fluent/tester.yaml`
path and cache choices rather than assuming every sandbox failure is external.
When a final Tester failure stops before the candidate gate, a round-cap
approval still needs the same immutable boundary: use only the latest completed
final Tester after the latest Writer, require its result to name that Writer's
exact candidate commit, and bind the result plus the exact failed-command logs.
Never fall back to this ungated evidence when a candidate gate exists; the gate
owns that frontier. Reject failed-command rechecks and passing, stale, missing,
malformed, or candidate-mismatched Tester evidence rather than approving a
mutable or unrelated correction.
That digest proves inspection, not recovery. In the failed-command recheck
route, if the Writer preserves the candidate, rerun only the previously nonzero
commands through the host-owned Tester boundary before reviewers. Bind the
subset to the source result, current
candidate, and unchanged command indexes, strings, and harnesses. Persist the
recheck manifest, result, and logs. Established defects retain correction;
verified unmet prerequisites need environment recovery, and unexplained exits
pause for diagnosis. A passing subset may admit review but never substitutes for
the complete final Tester. Separately, exact host proof accepted by targeted
Tests Review can satisfy a canonical command when the preserved complete run
and all applicable replacements satisfy every requirement. See
[[tester-failure-routing-requires-proven-cause]] and
[[required-checks-outlive-individual-results]].

When you declare a `Test:` reference, either land the backing test passing in the
same candidate, or — if the test is intentionally dropped — remove/replace the
reference and add an `Untestable:` justification. Do not leave a dangling
reference expecting the green suite to cover for it. This is the same
traceability chain [[behaviors-test-citation-sync]] protects against stale
renames; here the reference never had a backing test at all. Related:
[[route-tests-drive-real-launch-wiring]], [[test-names-match-assertions]].

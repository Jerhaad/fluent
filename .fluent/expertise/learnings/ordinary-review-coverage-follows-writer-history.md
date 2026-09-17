---
name: ordinary-review-coverage-follows-writer-history
description: Derive ordinary review coverage from scoped Task history and invalidate passes across every intervening Writer delta
metadata:
  type: architecture
---

For code-producing Attempts, `work_attempt_loop` derives ordinary review
coverage from completed Tasks, their candidate contexts, and saved verdicts.
Every built-in role needs a passing ordinary review before final Tester or
Learning. A targeted evidence review credits only its declared evidence scope;
it cannot supply missing ordinary coverage or replace a failed ordinary verdict.
After targeted acceptance, schedule any missing ordinary role through native
review planning while retaining the targeted Task and its evidence.

Carry a passing ordinary review forward only while its context is valid and
every subsequent completed Writer delta leaves its domain unchanged. Comparing
only the latest pair of Writers can lose an earlier invalidation; comparing
only the original and final trees can hide a change followed by a revert.
Unknown paths or unverifiable comparisons invalidate every domain. A replacement
review names its actual candidate without rewriting historical Task contexts.

Apply the same coverage assessment during planning, review interpretation,
saved final-Tester admission, and unfinished pre-Learning resume. Repair missing
coverage without manufacturing another Writer. Preserve already accepted
Learning when repeated resume observes its expertise commit.

Lifecycle fixtures that begin at Tester or Learning must establish ordinary
review prerequisites. Recovery regressions should let public resume schedule
the missing roles; inserting those roles by hand would conceal a planning gap.

Related: [[pointer-identity-gate-verifies-reviewed-sha]],
[[host-evidence-settles-against-bound-writer]],
[[test-fixtures-use-production-state]], [[route-tests-drive-real-launch-wiring]].

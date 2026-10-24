---
name: tester-failure-routing-requires-proven-cause
description: Distinguish established defects, verified unmet prerequisites, and unexplained Tester exits when choosing recovery
metadata:
  type: architecture
---

Fluent preserves a Tester's result separately from the evidence establishing its
cause. A nonzero exit or a log phrase naming a dependency does not establish a
source defect or an unavailable environment. An unexplained command or recheck
failure pauses for diagnosis without automatically creating another Writer.

The executor can establish an unmet prerequisite when an observed `os` or
`architecture` value conflicts with its declared prerequisite and prevents
command launch. It records the required and observed values, retains the failed
result, and continues other commands.
The recovery assessment revalidates that observation against the manifest before
offering environment recovery. Unobserved environment facts remain missing
proof; they cannot establish why a command failed.

Keep established structured-test failures and candidate-owned configuration or
extractor errors distinct from host infrastructure failures. Missing or unreadable
results, inconsistent summaries, and unverifiable candidate bindings require
diagnosis; a top-level Tester error alone cannot authorize a corrective Writer.
The shared assessment retains established correction or harness-repair routes
and Writer-budget gates. An early return for every
unsatisfied check can accidentally intercept these routes whenever applicability
declarations or prior Plan evidence enable the assessment. Host proof for one
command cannot erase independent aggregate defects.

Preserve the assessed cause when translating references into gate checks and
admitting saved Writers. A missing verification marker or unsupported reference
meaning stays an unresolved verification requirement, including before the first
Tester. A missing committed test source establishes a different cause and may
retain correction. Diagnostic handoffs name the candidate, Tester Task, expected
result path, specific failure, and available command logs so the operator can
act on the pause.

Test routing through the production Attempt path with committed checker inputs,
a canonical manifest, and applicability that activates the assessment. Include
an established assertion failure and a harness error alongside prerequisite and
ambiguous-exit controls. Verify the corrective Writer's exact read-only evidence
inputs where correction applies; a classifier unit test cannot prove the earlier
lifecycle branch reaches that Writer.

Related: [[tester-readiness-shares-production-boundary]],
[[required-checks-outlive-individual-results]],
[[route-tests-drive-real-launch-wiring]].

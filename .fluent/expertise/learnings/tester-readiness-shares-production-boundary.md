---
name: tester-readiness-shares-production-boundary
description: Tester readiness validates structure and executes through the production sandbox, extraction, and normalization boundary; production boundary failures pause only the Tester for repair and retry
metadata:
  type: architecture
---

Treat Tester readiness as an execution-boundary check, not as a separate
approximation of Tester behavior. Structural validation may run before declared
commands and must aggregate independent configuration and extractor problems
without creating Work state. The subsequent readiness run and a production
Tester Task must share the same sandboxed command execution, result extraction,
and normalization operation; otherwise a preflight can accept evidence that the
production Task rejects.

A standalone readiness failure uses temporary artifacts and creates no Attempt:
repair the project-owned Tester harness and rerun the check. In contrast, a
failure from the production Tester boundary — including configuration,
execution, extraction, normalization, or result persistence — means the
evidence is untrustworthy. Pause that same Tester Task and Attempt with a
resumable harness state. After an environmental repair that leaves the reviewed
candidate unchanged, resume only that Tester so completed Writer and review
work remains complete. A candidate-owned Tester contract repair must return
through the explicit `fluent attempt repair-tester` transition rather than
changing the frozen worktree. That transition preserves the failed Tester,
pins one corrective Writer to the exact clean reviewed commit and operator
decision, and confines the accepted net change to `.fluent/tester.yaml` and
`.fluent/extract-tester-results`. The repaired commit becomes the source for
fresh review and final Tester contexts. Keep environment-only retry and tracked
project repair as separate recoveries; an ambiguous harness failure may offer
both, but neither route may silently move the candidate. For code-producing
Work this final Tester runs after reviewers and blocks Learning; review-only
Work retains its Tester-first boundary before reviewers.

Failed Tester results are diagnostic inputs to this scoped Writer, not a set of
product correction requirements. Do not import their ordinary failed commands
into the repair Writer's completion matrix. If a repair Writer commits outside
the two-file boundary, record the rejected commit and restore the exact clean
approved candidate before pausing so public resume can retry. Perform legacy
normalization only after durable Task reservation, and only for a clean linear
descendant; dirty state and unrelated drift remain human-owned and fail closed.

Persist all command records accumulated before a harness failure in the Tester
artifact. An empty command list is accurate only for failures that occurred
before command execution; discarding executed command outcomes removes the
diagnostic evidence needed to repair the harness. Related:
[[reserved-phase-terminal-finalizer]],
[[route-tests-drive-real-launch-wiring]],
[[declared-behavior-tests-must-exist-before-land]].

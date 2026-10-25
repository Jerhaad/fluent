---
name: trace-controls-corrupt-observed-identities
description: Derive trace identity corruption from observed values so negative controls remain effective across worker orderings
metadata:
  type: testing
---

Fluent's production service traces can record concurrent workers in either
order. A negative control that assigns a fixed fixture identity to a positional
worker can leave a valid trace unchanged when that identity is already there.
Do not rely on collection order to create the intended contradiction.

Derive the mutation from observed facts. For a control that violates distinct
project ownership, copy one observed worker identity onto the other. Check that
the original trace passes, the mutated trace differs, and the real projector
rejects it with the intended ownership diagnostic for both worker orders.

Keep semantic controls distinct from artifact-integrity controls. When testing
a semantic relation in digest-bound artifacts, update the corresponding event
facts and artifact digests consistently so a stale digest cannot satisfy the
negative test before it reaches the claimed relation.

Related: [[production-traces-bind-public-cause-to-model]],
[[semantic-validation-tests-reach-target-branch]],
[[scheduler-formal-checks-use-defect-controls]].

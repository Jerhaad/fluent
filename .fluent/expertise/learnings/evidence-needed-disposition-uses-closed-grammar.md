---
name: evidence-needed-disposition-uses-closed-grammar
description: Review classification accepts evidence-needed only from an exact actionable disposition grammar and fails closed on ambiguity
metadata:
  type: convention
---

Review disposition is a protocol with a closed grammar, not a best-effort
interpretation of prose. A current failed review is actionable
`evidence-needed` only when it has exactly one disposition and exactly one
non-empty `External-Condition` plus exactly one non-empty `Additional-Proof`.
An evidence-needed marker with missing, empty, or malformed reasons is not
actionable in the current format and follows the ordinary `CodeChange` path.

Missing, duplicate, unknown, or contradictory disposition markers remain
uncertain; never select the first matching marker or infer a safe route from a
partial match. A passing evidence snapshot cannot authorize source
correction, and the narrow unqualified evidence-needed form is valid only in
the isolated legacy recovery route described by
[[legacy-evidence-compatibility-stays-isolated]].

Keep the prompt, documentation, parser, behavior statements, and tests in
lockstep. Test the cross-product of disposition count, reason presence, reason
cardinality, and result status so malformed current input cannot accidentally
pause without a Writer or bypass the required evidence precondition. Related:
[[ears-statements-are-atomic-and-triggered]],
[[negative-test-cross-product-coverage]],
[[semantic-validation-tests-reach-target-branch]].

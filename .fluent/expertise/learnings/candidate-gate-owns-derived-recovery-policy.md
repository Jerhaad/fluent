---
name: candidate-gate-owns-derived-recovery-policy
description: Keep recovery eligibility classifiers at the host-owned candidate gate and keep recovery modules responsible for orchestration
metadata:
  type: architecture
---

The candidate gate is the host-owned enforcement boundary for recovery
exceptions that depend on gate or project-hook evidence. Put the shared policy
classifier in `candidate_gate` (or a lower-level policy module) and have the
recovery path call it; the gate must not depend on recovery and Task
orchestration details.

Keep a derived project-check exception conjunctive and narrow: `commit-metadata`
must fail, `project-check` may be the only additional failed check, and its
canonical hook evidence must match the exact allowlisted wording and the same
commit and metadata reason. A project-check-only failure, unrelated hook
diagnostic, or any other failed check must remain ineligible before Fluent
mutates Work state or schedules a corrective Writer. Related:
[[corrective-writers-preserve-candidate-lineage]],
[[exception-must-not-widen-conjunctive-invariant]].

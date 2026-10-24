---
name: candidate-gate-source-binding-and-index
description: A corrective Writer's candidate-gate input is its source binding, while the Attempt artifact entry is a matching aggregate projection
metadata:
  type: architecture
---

A rejected candidate gate has two intentional Work-model projections with
different roles. The corrective Writer's exact `input_artifacts` reference binds
the correction to the immediately preceding source Writer. The matching
`Attempt.artifacts` reference makes the same evidence discoverable in the
Attempt-wide audit index. New rejection transitions must persist that canonical
reference exactly once in the Attempt index before they plan the corrective
Writer with it as an input.

Do not reconstruct missing Task provenance from the aggregate index or from a
gate-shaped file on disk. A legacy compatibility path may accept an omitted
aggregate entry only when the correction Task still carries the exact canonical
source binding and every other recovery condition and evidence digest validates.
Add the missing aggregate reference only inside the audited Work-model mutation
that records the recovery; preserve the original gate and hook evidence.

Treat contradictions as ambiguity, not as another legacy omission. Reject
duplicate gate inputs or aggregate entries, a canonical path assigned to another
producer, and any gate-shaped path assigned to the source Writer that differs
from the expected canonical reference. Tests should also prove that legitimate
gates from other Writer rounds can coexist. Related:
[[legacy-evidence-compatibility-stays-isolated]],
[[canonical-recovery-hook-evidence]],
[[audited-planning-recovery-binds-exact-frontier]].

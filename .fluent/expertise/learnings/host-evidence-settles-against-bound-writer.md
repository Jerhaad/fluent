---
name: host-evidence-settles-against-bound-writer
description: Settle host-evidence reviews against their recorded Writer candidate while requiring fresh evidence for a later candidate
metadata:
  type: architecture
---

A Plan host-evidence receipt can remain `Reviewing` after its targeted Tests
Review passes, including in saved history that already contains a later
corrective Writer. Validate that receipt against its recorded completed Writer,
candidate, source matrix, Plan row, command, evidence digest, and targeted Review.
Resolving its source through the latest Writer instead can reject valid history
and prevent fresh evidence from replacing the still-active receipt.

Keep historical settlement separate from current-candidate admission. Once all
targeted Plan reviews pass, the Attempt loop revalidates inside a fresh Work-model
mutation and settles the receipt. Queue admission checks the same facts without
rewriting history; attachment revalidates within its transaction. Preserve the
original evidence and Tasks, and reject changed bound bytes even when a saved
review says pass. A corrected candidate still requires its own host evidence.

Recovery tests must complete the corrective Writer, reach the fresh host handoff,
attach evidence, and resume. Stopping at Writer materialization misses the active
receipt's interaction with later admission.

Related: [[audited-planning-recovery-binds-exact-frontier]],
[[recovery-contracts-form-neutral-boundary]],
[[fresh-field-level-finalizer-preserves-concurrent-state]].

---
name: publish-writer-matrix-repairs-before-correction
description: Publish accepted Writer matrix repairs as authoritative Attempt artifacts before preparing a corrective Writer
metadata:
  type: architecture
---

Correction setup reads the prior Writer's completion matrix through
`recovery_contracts::authoritative_writer_completion_matrix`. A recovery gate
that accepts a repaired matrix only in memory can schedule a correction that
fails during materialization because the authoritative reader still sees the
original bytes.

When a Writer completion or candidate gate accepts format or claim repairs,
write the normalized matrix and repair evidence, attach their references to
the Attempt, and only then create the corrective Writer. Keep the source
matrix and rejected gate evidence intact. Test the path through correction
materialization so a successful in-memory check cannot hide an unreadable
persisted source.

Related: [[recovery-contracts-form-neutral-boundary]],
[[candidate-gate-source-binding-and-index]],
[[route-tests-drive-real-launch-wiring]].

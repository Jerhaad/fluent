---
name: committed-writer-matrix-recovery-authority
description: Distinguish committed-matrix recovery authority from older Writer-completion audits by its source matrix digest and operation Task binding
metadata:
  type: architecture
---

Writer-completion recovery keeps two compatible authority shapes under the
persisted schema. Older audits omit `source_matrix_sha256` and bind the recovery
operation to the Task named by `task_id`. An audit for a completed Writer's
malformed matrix includes `source_matrix_sha256`: `task_id` names the new
corrective Writer, while the operation binds `source_writer_task_id`, the
completed source Writer. The optional digest selects this interpretation; do
not infer it from Task order or change the binding for older audits.

The committed-matrix authority also binds the immutable requirement contract,
source assignment, transcript, output, candidate, and original matrix bytes.
The source matrix remains in the completed Task's artifact area; the audit
stores its digest rather than a copy. Revalidate the bound evidence when
authorizing or replaying the recovery and before the corrective Writer starts.
Keep the completed source Task and its artifacts intact, and give the corrective
Writer a fresh matrix from the contract. Approval schedules a Writer; a
separate resume executes it.

Related: [[recovery-contracts-form-neutral-boundary]],
[[reused-recovery-authority-revalidates-publication]],
[[corrective-writers-preserve-candidate-lineage]].

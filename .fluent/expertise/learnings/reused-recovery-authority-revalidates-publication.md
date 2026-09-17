---
name: reused-recovery-authority-revalidates-publication
description: Revalidate reused recovery authority before journal publication whenever an existing Task reopens
metadata:
  type: architecture
---

Work-model journals can finish publication through maintenance reads and explicit
upgrades after the original resume process has stopped. When recovery reopens an
existing Task under an already published approval and preparation, retain that
preparation operation in the journal's required validation set. Compare the prior
and target lifecycle states to identify this obligation; selecting only newly
added operation identities misses a reopening that reuses existing authority.

Revalidate the approval's bound inputs through the shared read-only contract
before publishing any split Work record. Validation during the original resume
does not cover inputs that change before journal recovery. A later launch check
can prevent a worker from starting, but cannot satisfy a promise to reject before
Work mutation once the journal has already published the reopened state.

Test this interval by interrupting after journal creation, changing an approved
input, and recovering through both a maintenance read and explicit upgrade.
Assert the specific rejection and unchanged Work and source bytes. Restore the
approved input and verify that the same journal recovers the same Task without
another approval or Writer round.

Related: [[publish-managed-inputs-with-model-record]],
[[recovery-contracts-form-neutral-boundary]], [[atomic-task-start-reservation]].

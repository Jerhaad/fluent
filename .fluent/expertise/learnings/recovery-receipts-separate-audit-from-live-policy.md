---
name: recovery-receipts-separate-audit-from-live-policy
description: Replay recovery receipts against their recorded policy commit while revalidating the current descendant policy separately
metadata:
  type: architecture
---

A recovery receipt's recorded policy commit is immutable audit evidence, while
the project branch can advance. Rebuilding the entire expected receipt from the
current project `HEAD` and comparing it for equality strands a valid recovery
after an unrelated fast-forward. The failure is especially subtle when the
process wrote the receipt but stopped before binding it into the Work model.

On replay, validate the policy at the exact recorded commit, require that commit
to remain an ancestor of the current project commit, and retain the original
receipt bytes and recorded identity. Separately validate that the current
checkout still exposes the required policy marker and invokes the expected
hook. Reject a non-descendant advance or changed current policy instead of
silently treating new authority as equivalent to the recorded decision.

Exercise both already-bound and prewritten-but-unbound receipts across an
unrelated project-branch advance. The latter covers the interruption window
between publishing durable evidence and recording the corresponding model
transition. Related: [[approval-records-bind-current-authority]],
[[durable-external-effects-use-reconcilable-identities]],
[[source-correction-recovery-binds-full-frontier]].

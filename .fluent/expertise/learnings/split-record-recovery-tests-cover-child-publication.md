---
name: split-record-recovery-tests-cover-child-publication
description: Interrupt Work-model recovery after child writes to verify replay and read-only inspection of partially published aggregates
metadata:
  type: testing
---

`WorkModelStore` persists a transaction journal, writes Attempt and Task records,
and publishes the Work Item root last. Interrupting immediately after journal
creation leaves the old children intact. That case cannot expose replay guards
that mistake a partially updated aggregate for the original authorized state,
or recovery commands that require an observation snapshot before reconciliation.

Exercise interruptions after the Attempt record, after the affected Task record,
and after all children but before the root publishes. Use the real storage path
with fault injection restricted to test builds. Repeat the exact public recovery
command before calling a maintenance read: maintenance can finish the journal and
hide a broken public retry. Test maintenance reconciliation separately.

Verify that replay retains the operation identity, protected context and evidence,
completed Tasks, and any later stop covered by the recovery contract. At the same
interruption points, exercise status and each show route with byte snapshots of
the protected tree. Observation must leave publication untouched and withhold an
aggregate whose children no longer match the published root. Also reject changed
requests, ownership, or audit evidence without completing the pending transaction.

The [complete-audit publication pattern](../persistence/patterns/bind-complete-recovery-audit.md)
describes how context recovery retains its authorization across these states.
Related: [[test-fixtures-use-production-state]],
[[rejection-tests-snapshot-owned-trees]].

---
name: audited-planning-recovery-binds-exact-frontier
description: Recover approved planning only from an exact candidate-bound review frontier, with auditable idempotence and typed lifecycle supersession
metadata:
  type: architecture
---

An authorized planning revision can recover a paused Attempt only when the
host verifies the exact persisted frontier: the current candidate, the settled
review that requires the revision, its recorded verdict, and any narrowly
recognized launch-failure shape. Revalidate the same identity under the
Work-model mutation lock before changing planning state. A recovery must not
turn a general failed-review shape into a resumable path.

Record the prior and replacement planning bytes, their identities, the human
resolution, candidate, and source-review identity in a deterministic audit.
Validate every immutable input before writing it, and make a repeated identical
request reuse that audit and its corrective Writer rather than creating another
round. When a replacement changes a planning stage, rebuild the host-owned
completion contract and reject it if it changes requirements outside that
stage. If a combined planning representation is authoritative, revise it
consistently or reject the recovery.

Carry every actionable artifact from the settled review round into the
corrective Writer and schedule the affected reviews for its corrected
candidate. If recovery preserves a failed Task as history, model its
supersession as a typed relation bound to the audit, source review, candidate,
and corrective Writer. Each completion, validation, round-cap, and active
frontier predicate must ignore only that validated relation; never broadly
ignore failed Tasks.

Related: [[planning-approval-binds-exact-revision]],
[[corrective-writers-preserve-candidate-lineage]],
[[fresh-field-level-finalizer-preserves-concurrent-state]].

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
round. Apply the approved stage delta to the Attempt's current completion
contract and reject changes outside that stage. Retained planning text cannot
reconstruct ownership splits adopted after intake. Preserve the other stages'
row identities, responsibility, commands, progress, and completion evidence. If
a combined planning representation is authoritative, revise it consistently or
reject the recovery.

Compose saved approvals in their recorded order through `recovery_contracts`.
Decode Approach and Test-reference audits with their own schemas and validate
their source contracts and matrix digests before source selection or exact
replay. Contract history and replacement-matrix history have different edges:
Approach approvals revise requirements, while Test-reference approvals produce
replacement matrices. Follow recorded matrix predecessors, which may name an
ordinary Writer matrix, rather than requiring an ownership-adoption artifact.
An unchanged host obligation may retain accepted evidence only when its exact
candidate, row, command, source bytes, and targeted review still agree.

Apply each audit variant's own frontier invariant during historical replay as
well as initial admission and exact retry. A round-cap Approach audit must keep
proving its extension against the actual pre-extension Writer frontier after
the corrective Writer completes; comparing historical state only with the
latest composed contract can accept altered authorization or strand a later
revision.

Verify recovery composition through public commands, including interleaved
Approach and Test-reference revisions, both intake and adopted ownership, and
a successful corrective Writer. Continue through evidence consumption and the
later Tester and Learner gates; contract replay or Writer materialization alone
does not establish that the next transition remains usable.

Carry every actionable artifact from the settled review round into the
corrective Writer and schedule the affected reviews for its corrected
candidate. If recovery preserves a failed Task as history, model its
supersession as a typed relation bound to the audit, source review, candidate,
and corrective Writer. Each completion, validation, round-cap, and active
frontier predicate must ignore only that validated relation; never broadly
ignore failed Tasks.

Related: [[planning-approval-binds-exact-revision]],
[[corrective-writers-preserve-candidate-lineage]],
[[fresh-field-level-finalizer-preserves-concurrent-state]],
[[host-evidence-settles-against-bound-writer]].

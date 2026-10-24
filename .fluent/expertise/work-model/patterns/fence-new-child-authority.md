# Fence new authority in a split child record

## Context

Use this when a new optional field in a Merge Candidate or Task authorizes an
effect that an older Fluent writer must not silently discard. The Work root's
schema fence alone does not protect a child if a legacy root envelope is
restored during recovery.

## Mechanism

Bump the Work record, writer envelope, and transaction versions. Give the
child's active authority a status value that the immediate predecessor cannot
deserialize, and require that status and authority field to appear together in
current model validation. Keep legacy completed records readable. Test the
pinned predecessor against both the ordinary new record and a fixture that
restores only its old root and Attempt envelopes; compare stored bytes after
both refused mutations.

## Example

`MergeCandidateMergeStatus::LandingPending` accompanies `landing_intent`.
The predecessor refuses the unknown status even after a test restores root
schema 11 and writer envelope 9, while current Fluent validates that a
pending landing always retains its intent.

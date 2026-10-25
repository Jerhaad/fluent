---
name: cleanup-protects-cross-record-eligibility
description: Recheck cleanup eligibility under the locks for both the origin and its descendant recovery dependencies, then hold them through removal
metadata:
  type: architecture
---

Work cleanup selects candidates optimistically, but selection does not authorize
removal. Before deleting a Work Item's worktrees, branch, artifacts, or record,
cleanup must recheck its current state and required follow-up recovery. The
origin's revision alone cannot reveal a descendant authorization that changes
whether the origin must be retained.

The removal boundary takes the follow-up cleanup dependency lock before the
land lock and the origin Work model lock, then holds them through destructive
actions. Descendant authorization takes the same dependency lock before its
lineage and Work locks. Check post-land recovery before taking the land or
origin model lock because that verification takes an operation lock and reads
Work Items. For orphan artifact roots, refresh Work IDs and hold the matching
model lock while checking record absence and removing the root; a list captured
before cleanup began cannot authorize later deletion.

Related: [[lock-ordering-across-subsystems]],
[[follow-up-journal-schema-boundary]], [[production-lock-test-hooks]].

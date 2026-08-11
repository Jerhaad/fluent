# Work Model

The Work model (`src/work_model.rs`) holds the core data structures — WorkItem,
Attempt, Task, MergeCandidate — and their JSON-file storage. WorkItems are
stored split: top-level fields serialize through `WorkItemRecord`, while
attempts, tasks, and merge candidates persist as separate records.

## Patterns

- [extend-work-item-backward-compatibly](work-model/patterns/extend-work-item-backward-compatibly.md) — read when adding a field to WorkItem or another split-stored model type
- [serialize-aggregate-work-writes](work-model/patterns/serialize-aggregate-work-writes.md) — read when adding code that rewrites a WorkItem and its split child records

## Resume transitions

Keep filesystem-aware resume recognition in `lifecycle_recovery` when both queue
admission and the Attempt loop need the same contract. Queue admission performs
the read-only check before it records authorization. Execution repeats the check
inside `mutate_work_item` before changing lifecycle state, so a stale queue
decision cannot reopen a different candidate frontier. Return a small immutable
frontier value and compare it across the transition; do not infer recovery from
the pause label alone.

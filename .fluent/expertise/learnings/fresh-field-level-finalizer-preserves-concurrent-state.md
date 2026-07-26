---
name: fresh-field-level-finalizer-preserves-concurrent-state
description: A phase that must not overwrite pointers settles each terminal transition through a fresh, lock-held mutate_work_item that re-reads the aggregate, accepts only its own exact frontier, and changes one field — never a stale pre-run snapshot write
metadata:
  type: architecture
---

A long-running reserved phase (the Learner) reserves durable state, launches a
coder that may run for minutes, then settles a terminal outcome. If it settles by
writing back the whole `item` snapshot it captured *before* the coder ran, a
concurrent Work-model change that landed during the run is either lost (a naive
overwrite) or turns the write into a `StaleWorkItem` error that strands the phase's
reserved `InProgress` record — the exact hole the release gate found in the
no-expertise finalizer.

The correction: when a phase must preserve pointers and unrelated concurrent state,
settle **each** terminal transition through its own fresh, lock-held
`store.mutate_work_item(id, |fresh| …)`. The reducer:

- **re-reads the current aggregate under the model lock** (`mutate_work_item` does
  this), so the write starts from live state, not a stale snapshot;
- **accepts only this runner's exact frontier** — re-find the Attempt by id, then
  require the Learning record to be exactly the expected `status` *and* `runs`
  (`learning_frontier_is`). A peer that already advanced to a harder or later
  transition is honored (a durable no-op), never revived or overwritten;
- **changes only its own field** (the `learning` record), leaving every pointer and
  every unrelated concurrent field exactly as the current aggregate holds them;
- **evaluates any gate against that same `fresh` aggregate** and commits the
  pass/fail result in the same mutation, so the decision and the write are atomic
  (`prepare_no_expertise_handoff` runs the pointer-identity postflight against
  `fresh` and sets `HandoffPending` or `Failed` in one step). Deciding in one
  transaction and persisting in another reopens the race.

Chain the transitions as separate fresh mutations
(`prepare_no_expertise_handoff` for `InProgress → HandoffPending`/`Failed`, then
`publish_no_expertise_handoff` for `HandoffPending → Succeeded`/`Failed`), and
after settlement refresh the caller's in-memory snapshot for observation only —
**do not** re-write it. Remove the post-phase whole-aggregate write at the Attempt
call sites for this mode; a refreshed snapshot written back is just another
stale-write window.

Prove it with injected concurrency, not just the happy path: mutate durable Work
state inside the coder callback and assert (1) a contradiction in a checked identity
class becomes relaunchable `Failed/Generic` with the contradictory value preserved
and no handoff, (2) an unrelated valid field survives through `HandoffPending` and
`Succeeded`, (3) a concurrent harder Learning transition is never overwritten, and
(4) no `StaleWorkItem` escapes. This is a field-level complement to the
whole-aggregate finalizer in [[reserved-phase-terminal-finalizer]]; capture mode
still uses that finalizer because it intentionally moves pointers. Related:
[[pointer-identity-gate-verifies-reviewed-sha]], [[atomic-task-start-reservation]],
[[prove-settlement-path-not-just-outcome]], [[backward-compatible-serde-fields]].

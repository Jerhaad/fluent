---
name: model-owned-lifecycle-transition-ledger
description: Record lifecycle transitions inside the Work model transaction, preserve typed owner attribution and object-specific reasons, and separate in-memory dashboard views from durable delivery cursors
metadata:
  type: architecture
---

The Work model owns the authoritative lifecycle transition ledger for Work
Items, Attempts, Tasks, and Merge Candidates. Derive and append entries while
the per-Work-Item model lock holds the actual pre- and post-mutation aggregate;
do not reconstruct history in Slack, the dashboard, or another projection.
Reject caller attempts to rewrite or remove prior entries.

Mutation owners pass a closed `DurableTransitionOrigin` through
`*_with_transition` APIs. Use caller-supplied reasons only when the caller owns
the explanation, such as plan approval or scheduler binding. Executor wrappers
should normally supply the typed origin with no generic reason so the ledger
retains the object's actionable pause or failure reason. Bound reason and
evidence text at the model boundary so the append-only aggregate does not become
a transcript store.

An empty legacy ledger does not mean the aggregate has no history. On its first
supported mutation, snapshot every existing durable object state with origin
`Unknown`, then append the current attributed change in the same aggregate
transaction. New Work creation records its initial state in the creation
transaction.

Consumers that deliver durable effects, such as Slack Slice projection, keep
independent durable per-Work cursors over the model-owned ledger. For an external
delivery, schedule the stable transition identity through the idempotent outbox
before advancing that consumer's cursor. Consumers never share acknowledgement
state.

Dashboard observation derives its latest-transition view in memory from verified
published Work snapshots. Refresh and restart reconstruct the view from the
ledger without persisting a cursor, acknowledgement, or projection. Preserve the
[dashboard ownership decision](../decisions.md#dashboard-observation-owns-no-durable-transition-cursor)
when adding consumers or changing how they project transitions.

Related: [[recovery-cursors-advance-after-durable-ingest]],
[[durable-external-effects-use-reconcilable-identities]],
[[backward-compatible-serde-fields]].

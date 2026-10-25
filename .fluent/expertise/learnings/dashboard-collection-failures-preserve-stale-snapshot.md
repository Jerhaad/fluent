---
name: dashboard-collection-failures-preserve-stale-snapshot
description: Preserve dashboard rows as stale when Work enumeration fails and treat item-local diagnostics as a successful partial refresh
metadata:
  type: architecture
---

The Work snapshot boundary distinguishes failure to enumerate the Work Item
collection from failure to read one aggregate. Propagate an enumeration error
through the status and dashboard loaders so dashboard refresh retains its last
successful snapshot and marks it stale. Converting that error into diagnostics
with an empty row list would erase the displayed Work and clear staleness.

An unreadable individual Work Item, split record, or pending journal instead
produces bounded diagnostics alongside every aggregate that can be verified.
That result is a successful partial refresh: it replaces the displayed snapshot
and clears an earlier stale marker while retaining the item diagnostics.
Legitimately absent Work storage remains a successful empty load.

Verify the distinction through the real dashboard loader and refresh path.
Start with visible Work, obstruct collection enumeration, assert preserved rows
and staleness, then restore enumeration with one malformed item and assert a
successful partial refresh. Calling the app's failure handler directly cannot
prove that storage errors reach it.

Related: [[dashboard-layout-contract-is-acyclic]],
[[model-owned-lifecycle-transition-ledger]].

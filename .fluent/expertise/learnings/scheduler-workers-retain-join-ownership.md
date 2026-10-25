---
name: scheduler-workers-retain-join-ownership
description: Keep every claimed scheduler worker owned until join, even when another worker fails or panics
metadata:
  type: architecture
---

The scheduler service starts a tracked worker immediately after each claim and
keeps its join handle with the dispatch identity. Reaping joins only finished
workers and retains unfinished handles. Joining for shutdown visits every
worker, collects failures with project, Work Item, dispatch, generation, and
Attempt context, then returns the collected errors; other service exit paths
log failures while dropping the service. A failed worker must not detach a
healthy worker or imply an Attempt outcome that the durable model did not
record.

Tests for this ownership rule must hold a healthy worker at a barrier while a
failed worker is joined, show that the join remains pending, then release and
join the healthy worker. Check reaping separately with a failed finished worker
and an unfinished healthy worker. Merely observing both dispatches eventually
settle does not prove the service retained their handles.

Related: [[scheduler-service-contains-project-failures]],
[[prove-settlement-path-not-just-outcome]].

---
name: landing-effect-lock-protects-cancellation
description: Serialize Attempt cancellation with landing intent publication and the target Git effect under a project-wide effect lock
metadata:
  type: architecture
---

Landing and Attempt cancellation both change whether a target fast-forward is
authorized. `merge_candidate` holds the land lock for the full landing route,
then acquires the project-wide landing effect lock before it publishes a
`LandingPending` intent. It keeps the effect lock through the final state and
cleanliness checks and the checked Git fast-forward. `cancel_local_attempt`
acquires the same effect lock before it records cancellation. Both paths may
acquire the Work-model lock while holding the effect lock; preserve this order
when changing either path. Landing releases the effect lock after Git advances,
then records the merged result separately.

This short boundary ensures cancellation cannot be recorded between the final
authorization check and the Git effect. A retry after the effect uses the
retained intent to reconcile publication. Related:
[[lock-ordering-across-subsystems]],
[[post-land-effects-are-idempotent-and-land-safe]].

---
name: landed-status-separates-history-from-recovery
description: Show merged Work from retained evidence history while keeping current recovery and verification for unmerged candidates
metadata:
  type: architecture
---

Once a Merge Candidate records `Merged` and its merged commit, status,
Work Item inspection, and dashboard should describe the landed result from the
persisted candidate and retained Tester artifacts. The source checkout may
already be retired. Bind the Tester result to the candidate and its managed
artifact identity, and report missing or inconsistent retained proof as a
history diagnostic. Historical Writer requirements must not become current
work or produce a pre-land resume command after landing.

Keep history diagnostics in the history projection and active recovery blockers
in the recovery projection so consumers can interpret each field without
inferring its meaning from another field. Until a candidate is merged, current
verification and recovery remain active even if its Attempt is Complete.

Related: [[verification-requirements-ignore-diagnostic-wording]],
[[split-record-recovery-tests-cover-child-publication]].

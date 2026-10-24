---
name: record-divergence-in-decisions-md
description: Record accepted Approach deviations in decisions.md; editing expertise alone cannot resolve conflicting requirements
metadata:
  type: convention
---

When an implementation deliberately diverges from an interface or shape specified in `approach.md` — for example collapsing `record_post_land_operation(origin, merged_commit, batch_ref, disposition)` into `record_post_land_operation(project_root, batch, review_request)` so the normalized batch is the single source of truth — record the divergence in `decisions.md`, not only in the round's `progress.md`.

`decisions.md` is the durable record a future contributor consults; `progress.md` notes are round-scoped and stop being read once the round closes. A divergence recorded only in progress notes later reads as accidental drift from the approach. The architecture reviewer flags an unrecorded, otherwise-reasonable divergence as a minor finding for exactly this reason.

Recording a decision does not itself authorize changing a mandatory Approach or
declared behavior. If a proposed structural decision conflicts with those
requirements, surface the conflict as `uncertain` and resolve it through Fluent's
planning-revision path. Updating expertise to match an implementation leaves the
conflict unresolved when the supplied Approach still requires another invariant.
After resolution, keep the implementation, decisions, and canonical behaviors
consistent, including older statements that describe the same transition.

Related: [[keep-architecture-doc-in-sync]]

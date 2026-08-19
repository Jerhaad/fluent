---
name: recovered-approvals-preserve-delay-provenance
description: Mark every reconnect replay source as recovered so delayed consequential approvals require fresh confirmation
metadata:
  type: architecture
---

Reconnect can replay events from more than remote history. The collaboration
runner also drains its durable pending inbox after a restart. Preserve recovered
provenance for both sources; otherwise a delayed approval from the local inbox
can bypass safeguards applied to remote history.

Planning approval may remain valid after reconnect only when current authority
still admits the actor and the event binds to the currently presented revision.
A recovered plain-text reply without control identity must be at or after the
pending action's presentation timestamp. Recovered execution, resume, and
landing approvals never advance directly: retain the pending action and request
fresh confirmation.

Exercise recovery with a newly constructed runner. Cover current, stale, and
revoked planning approvals plus every consequential action for both supplied
history and a pre-populated pending inbox. Related:
[[recovery-cursors-advance-after-durable-ingest]],
[[durable-external-effects-use-reconcilable-identities]].

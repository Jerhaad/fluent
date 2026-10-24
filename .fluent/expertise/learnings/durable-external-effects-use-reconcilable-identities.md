---
name: durable-external-effects-use-reconcilable-identities
description: Persist a stable action identity before an external effect and reconcile that identity before retrying after an interruption
metadata:
  type: architecture
---

Collaboration turns and outbound messages cross external boundaries where a
process can stop after the remote side accepts an effect but before local state
records completion. A local pending flag alone cannot make that window
exactly-once: blindly replaying it can submit a second provider turn or Slack
message.

Before invoking the provider, persist a pending turn with a stable action
identity. The provider protocol must carry that identity and expose recovery
that distinguishes missing, accepted, and completed turns. After an
interruption, reconcile the identity first; submit only a missing turn, retain
an accepted turn for later recovery, and persist a recovered completed reply
before creating its outbound delivery.

Outbound records follow the same rule. Treat a delivery identity and its
payload as immutable, carry a deterministic retry identity through the
transport, and save progress immediately after each accepted delivery rather
than after a whole batch. A revised status is a new immutable revision or an
explicit update of a recorded remote message; never rewrite an existing
outbox identity with new text.

Test the acceptance and completion crash points independently, including the
remote-visible effect count. Related: [[explicit-state-allowlist-before-nonidempotent]],
[[non-idempotent-stage-checkpointing]].

---
name: planning-approval-binds-exact-revision
description: Advance a Work Item planning stage only from the exact digest-bound revision that an authorized approver reviewed
metadata:
  type: architecture
---

Planning review has two distinct records: the conversation presents a named,
immutable revision, while the Work Item stores the planning bytes that may
advance its lifecycle. Do not treat a generic approval or the current mutable
draft as sufficient authority. Construct the revision identity from its stage
and content digest, persist it as the pending revision before delivery, and
accept approval only when it names that exact pending identity.

The Work model remains the lifecycle authority. It validates the permitted
stage order and records the approved revision ID and digest alongside the
approved bytes. Revalidate that identity against the stored bytes when loading
the Work Item, so a stale or altered conversation record cannot advance
planning. An approver is a narrower project role than a contributor and must
also be an allowed participant.

Related: [[bound-conversation-state-follows-project-authority]],
[[durable-external-effects-use-reconcilable-identities]],
[[backward-compatible-serde-fields]].

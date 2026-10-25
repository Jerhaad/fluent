---
name: external-review-feedback-binds-source-candidate
description: Persist immutable review-thread identities and an explicit source commit so selected feedback routes to the candidate it actually reviewed
metadata:
  type: architecture
---

External review feedback is not identified by body text or by whichever
candidate a pull request currently shows. Persist the provider's stable review,
thread, and comment identities, stable URL, resolution state, and explicit
reviewed commit. Reject feedback whose source commit cannot be proven instead
of assigning it to the current candidate.

When polling again, treat thread/comment identities, URL, and source commit as
immutable. A provider response that changes one of those fields is an identity
conflict and must fail closed. Content, review kind, and resolution state may be
reconciled as mutable state under the same identity.

Keep candidate history when one pull request advances. An approver's selected
feedback must resolve its source commit back to that historical candidate and
enter rework against that exact candidate. Reject mixed selections that span
candidate commits, unknown commits, or resolved feedback. Related:
[[durable-external-effects-use-reconcilable-identities]],
[[pointer-identity-gate-verifies-reviewed-sha]].

---
name: approval-records-bind-current-authority
description: Evaluate approvals against freshly loaded project authority and persist a self-contained record of the exact action and revision
metadata:
  type: architecture
---

Keep lifecycle approval policy pure. The coordinator loads and validates the
current project authority, passes that snapshot and the pending action into the
domain policy, persists the returned aggregate, and emits any Slack intent. The
policy must not load configuration, access the filesystem, or perform the
authorized planning, execution, resume, or landing effect.

An accepted approval becomes a self-contained durable authorization record. It
binds the project and thread, pending action id and kind, artifact revision and
presentation time, approving actor, acceptance time, and inbound event identity.
Clear the pending action only after creating this record. Effect consumers use
the immutable authorization record rather than reconstructing authority from
mutable thread state or current configuration.

Test the complete record, not only that an approval count increased or the
pending action disappeared. Assert every binding field and replay the same event
identity to prove the decision is idempotent. Related:
[[pure-domain-keeps-filesystem-authority-at-edges]],
[[token-bound-to-persisted-record]].

---
name: recoverable-parent-child-materialization
description: Stage the exact parent intent before publishing multiple child records, resume matching partial publication, and gate downstream execution until materialization completes
metadata:
  type: architecture
---

An operation that publishes several durable child records and then binds them
from a parent record cannot write the parent last. A crash or later child
failure would leave already-created children orphaned, while retrying would fail
on their existing identities.

Persist a recoverable parent record first in a `materializing` state. Freeze the
complete requested child set and the authority that approved it in that record.
On retry, require the request to match the frozen intent, validate any existing
children against the exact expected content and attribution, publish only the
missing children, and mark the parent complete after all children exist. Reject
a conflicting retry or an unrelated pre-existing child before publishing more
state.

Treat the staged parent as recoverable visibility, not execution authority.
Consumers may report children already published by the transaction, but every
downstream execution or submission gate must reject the parent until
materialization reaches `complete`. Tests should inject a middle-child failure,
verify the partial state remains reportable but non-executable, then retry the
same request and prove it completes without duplicate children or effects.

Related: [[non-idempotent-stage-checkpointing]],
[[publish-managed-inputs-with-model-record]],
[[explicit-state-allowlist-before-nonidempotent]].

---
name: publish-managed-inputs-with-model-record
description: Publish managed files and the Work-model record that references them under one per-item lock, and retain files whenever a record or recovery journal became durable
metadata:
  type: architecture
---

When a Work-model record durably references files outside the split model
records, treat the file installation and record creation as one ownership
transaction. Validate and read approved sources before taking the per-item
`model.lock`, then hold that lock while recovering prior transactions, checking
identity availability, installing the managed files, and authoring the
recoverable Work-model transaction. Otherwise, two creators for the same id can
both install a shared tree and the losing creator can delete the winner's files.

Make failure cleanup depend on durable publication, not merely on whether the
store call returned an error. Remove a new managed tree only when neither the
top-level record nor its recovery journal exists. If either became durable,
retain the files so recovery cannot publish references to missing inputs.

A managed recovery sidecar has no authority from its path or bytes alone.
Journal its exact bytes, publish the aggregate reference with them, and require
resume to find exactly one registered file whose bytes match the recorded
recovery operation. A matching pre-existing sidecar may be reused by the exact
transaction; a parseable conflict remains inert at its old path while a new
full-frontier identity is selected, and an unparseable sidecar at the selected
path fails closed. Pending transaction bytes are recoverable visibility, not
permission to execute.

Cover both boundaries: race two same-id creators and assert the winner retains
its exact files, then inject a post-publication failure at the real persistence
boundary and assert recovery retains and resolves those files. Keep such fault
injection test-only and scoped as described by [[production-lock-test-hooks]].

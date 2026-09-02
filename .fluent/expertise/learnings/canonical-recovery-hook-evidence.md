---
name: canonical-recovery-hook-evidence
description: Bind recovery eligibility to the canonical stored hook log and its digest, not to a serialized diagnostic summary
metadata:
  type: architecture
---

When project-hook output can extend a candidate-gate recovery exception, the
canonical stored `check-pre-merge.log` is the evidence authority. Require the
exact path beside the relevant gate, a bounded regular UTF-8 file, and the
allowlisted finding in its bytes; a `project-check` diagnostic field may
corroborate the log but cannot authorize recovery when the log is missing or
does not contain the finding.

Persist the canonical relative path and SHA-256 in the recovery authority,
attach the same exact artifact to the corrective Task, and revalidate both
identity and digest before resume. Missing, changed, alternate, or unbound hook
evidence must fail closed before Work-model mutation or coder launch. Related:
[[audited-planning-recovery-binds-exact-frontier]],
[[approval-records-bind-current-authority]],
[[provider-outage-evidence-fails-closed]].

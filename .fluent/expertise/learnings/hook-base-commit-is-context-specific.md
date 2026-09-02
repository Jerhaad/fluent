---
name: hook-base-commit-is-context-specific
description: Treat FLUENT_BASE_COMMIT as optional hook context supplied by candidate-gate checks, not as a universal hook variable
metadata:
  type: gotcha
---

`HookContext` fields are optional because Fluent invokes hooks at different
lifecycle boundaries. The candidate-gate `check-pre-merge` invocation supplies
`FLUENT_BASE_COMMIT` for the effective candidate base; merge-time checks and
other hook invocations may leave it unset. Hook implementations and their
documentation must scope their assumptions to the invocation that supplies the
field rather than treating it as universally available. Related:
[[candidate-gate-owns-derived-recovery-policy]],
[[expertise-claims-match-enforced-mechanism]].

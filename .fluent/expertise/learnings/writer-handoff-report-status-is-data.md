---
name: writer-handoff-report-status-is-data
description: Writers must read the handoff report's overall status and per-check diagnostics instead of relying on command exit status
metadata:
  type: gotcha
---

`fluent task check-handoff` prints a JSON report for the current candidate and can
exit successfully when the report's `overall` status is `failed` or
`not-verifiable`. A successful CLI exit therefore says the report was produced,
not that handoff passed. Writer guidance and callers should inspect `overall`
and each check's diagnostics. Preflight is a momentary assessment; Fluent runs
the handoff again after the Writer Task ends. Related:
[[writer-handoff-publishes-assessed-gate]].

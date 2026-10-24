---
name: writer-handoff-publishes-assessed-gate
description: Final Writer handoff publishes the candidate gate result it assessed instead of rerunning a project hook
metadata:
  type: architecture
---

Active Writer preflight and completed Writer handoff use the same ordered
assessment. Preflight keeps its hook logs private and does not publish canonical
candidate-gate evidence. At final handoff, the host publishes the gate result and
hook log captured by that assessment, then decides from those same checks. Do not
run `check-pre-merge` again while publishing: hooks can be stateful, and a second
result could disagree with the report that drove the decision. The final handoff
assesses the current candidate again rather than trusting an earlier preflight.

Tests should check both the report and the published gate evidence, including a
hook whose result would change on a second invocation. Related:
[[canonical-recovery-hook-evidence]], [[route-tests-drive-real-launch-wiring]].

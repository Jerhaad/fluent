---
name: legacy-recovery-rechecks-active-blockers
description: Historical hook output can authorize retired findings only; recheck active blockers across the complete candidate range
metadata:
  type: architecture
---

A stored project-hook log proves what the historical hook reported, not what it
omitted. The candidate can contain an older or weakened policy script, so an
allowlisted legacy finding can coexist with a violation that the current policy
still blocks. Absence from the historical log is not evidence that the active
violation is absent, especially when recovery will skip the ordinary candidate
gate.

Use the closed legacy classifier only to recognize the retired findings. Then
independently recheck every still-active blocker against the exact recovered
range from the protected base through the candidate. Inspect every commit, not
only the tip: an earlier commit can contain a prohibited attribution trailer
while the tip supplies the presentation-only finding stored in the hook log.

Reject any mixed or unlogged active violation before mutation and preserve the
Work Item, Task history, candidate, gate, and hook evidence unchanged. Tests
should place the active violation and retired finding in different commits so a
tip-only scan cannot pass. Related:
[[legacy-evidence-compatibility-stays-isolated]],
[[canonical-recovery-hook-evidence]],
[[candidate-gate-owns-derived-recovery-policy]].

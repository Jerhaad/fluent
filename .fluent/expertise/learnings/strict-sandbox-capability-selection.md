---
name: strict-sandbox-capability-selection
description: Select shared-temp-free sandbox profiles only for launches that receive an explicit private replacement boundary; preserve standard grants for other coders
metadata:
  type: architecture
---

The strict Seatbelt renderer removes the broad shared temporary-directory write
grants. Select it only for a launch that has an explicit private replacement
boundary, such as a prepared Codex worker home granted to that invocation.

Do not apply that strict rendering merely because code passes through a shared
sandbox builder: non-Codex rebase coders rely on the standard shared temporary
grants unless a separately designed replacement boundary is supplied. Keep the
selection at the capability boundary (`Option<&CodexWorkerEnvironment>`), and
test both branches: strict Codex routes must lack the broad grants, while the
normal rebase route must retain them.

Related: [[sandbox-denials-track-template-grants]],
[[canonicalize-confinement-paths]].

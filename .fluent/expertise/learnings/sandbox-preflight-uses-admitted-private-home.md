---
name: sandbox-preflight-uses-admitted-private-home
description: Run strict Codex readiness from its admitted private home and test its ancestor grants independently of workspace grants
metadata:
  type: gotcha
---

Strict Codex readiness runs the prepared production launcher's `login status`
from the worker's private home. The shared temporary directory is not an admitted
working directory merely because the private home lives beneath it. Include the
private home among the profile renderer's explicit roots so path canonicalization
receives the necessary ancestor metadata grants. Grant ancestors only the
metadata needed for traversal; do not grant their contents or writes to make
readiness succeed.

Keep the confinement test's workspace outside the worker temporary tree. When
both live under the same temporary ancestors, grants derived from the workspace
can hide missing grants for the private home. Exercise the installed launcher
and production sandboxed readiness with synthetic provider and readiness
overrides removed, then verify denied reads of unrelated temporary and
operator-home content. A successful shell probe alone does not prove that the
provider can resolve its private home and check authentication.

Related: [[strict-sandbox-capability-selection]],
[[canonicalize-confinement-paths]],
[[scoped-writers-separate-tools-from-mutable-state]].

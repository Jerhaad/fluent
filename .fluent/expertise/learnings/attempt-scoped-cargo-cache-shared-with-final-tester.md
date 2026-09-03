---
name: attempt-scoped-cargo-cache-shared-with-final-tester
description: Share one Fluent-owned Cargo dependency cache across Writers and the final Tester within an exact Attempt while excluding the operator Cargo home
metadata:
  type: architecture
---

Autonomous Writer rounds and the final Tester in one Work Item Attempt share
one rebuildable Fluent-owned Cargo cache keyed by the exact Work Item and
Attempt. Another Attempt must receive a different cache, and Cargo build
output remains in the candidate workspace so the existing warm-output path is
preserved.

Seed only the admitted dependency stores, such as Cargo `registry/` and `git/`,
from the canonical operator Cargo home. Never copy or grant the operator's
credentials, configuration, binaries, or other Cargo-home entries. Set the
private `CARGO_HOME` for both the Tester commands and its result extractor,
grant the private Attempt cache at the rendered sandbox boundary, and hold
the cache lock while reclaiming stale caches and preparing a launch. The cache
is execution state, not durable evidence.

Test both environment forwarding and the rendered sandbox branch. A
no-sandbox test can prove that a command sees `CARGO_HOME` while missing a
broken profile grant; a rendered-profile fixture must execute through the
same launcher seam, verify the exact private cache is writable, and verify the
operator Cargo store is not granted.

Related: [[tester-readiness-shares-production-boundary]],
[[strict-sandbox-capability-selection]],
[[sandbox-tests-assert-invariant-in-both-host-branches]].

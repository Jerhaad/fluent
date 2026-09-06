---
name: attempt-scoped-cargo-cache-shared-with-final-tester
description: Share an exact Attempt's confined Cargo dependency cache with Writers, final Tester, and landing while keeping landing preparation independent of reclamation
metadata:
  type: architecture
---

Autonomous Writer rounds, the final Tester, and landing verification for one
Work Item Attempt share one rebuildable Fluent-owned Cargo cache keyed by the
exact Work Item and Attempt. Another Attempt must receive a different cache,
and Cargo build output remains in the candidate workspace so the existing
warm-output path is preserved.

Seed only the admitted dependency stores, such as Cargo `registry/` and `git/`,
from the canonical operator Cargo home. Never copy or grant the operator's
credentials, configuration, binaries, or other Cargo-home entries. Set the
private `CARGO_HOME` for both the Tester commands and its result extractor,
and grant the private Attempt cache at the rendered sandbox boundary. Task
launches hold the project cache lock while reclaiming stale caches and preparing
the cache. The cache is execution state, not durable evidence.

Landing uses a separate preparation entry point under the same project cache
lock. It seeds only its exact Attempt cache and leaves reclamation to Task
launches and cleanup. Reusing launch preparation here would scan unrelated Work
Items and allow a malformed follow-up record to block landing before post-land
processing can report its own error. Preserve this boundary when consolidating
cache helpers; verify it with malformed unrelated Work and another cache that
must survive preparation.

Test both environment forwarding and the rendered sandbox branch. A
no-sandbox test can prove that a command sees `CARGO_HOME` while missing a
broken profile grant; a rendered-profile fixture must execute through the
same launcher seam, verify the exact private cache is writable, and verify the
operator Cargo store is not granted.

Related: [[tester-readiness-shares-production-boundary]],
[[capture-landing-owns-exact-tester-evidence]],
[[strict-sandbox-capability-selection]],
[[sandbox-tests-assert-invariant-in-both-host-branches]].

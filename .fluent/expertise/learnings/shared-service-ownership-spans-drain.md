---
name: shared-service-ownership-spans-drain
description: Verify the shared scheduler lease and instance through reuse, drain publication, replay, and ownership release
metadata:
  type: architecture
---

The shared scheduler service has one owner across registered projects. Reuse
requires a compatible socket health response, matching version and executable
hash, and metadata that identifies the current lease holder. Check both direct
and managed startup paths, including the health response after a managed start.
An unverified reachable socket stays in place for inspection.

The service instance remains the identity for a global drain. Persist its stop
request, stop claims and new submissions across projects, settle owned workers,
then publish that instance's result before releasing its socket and lease.
Replay a completed request from the retained result; an earlier instance's
request cannot drain its replacement. A published result alone does not prove
release: identity and lease read errors must propagate instead of becoming a
successful stop. Ordinary project stop remains scoped to one coordinator.

Related: [[scheduler-service-contains-project-failures]],
[[lease-acquire-types-contention-vs-infrastructure]].

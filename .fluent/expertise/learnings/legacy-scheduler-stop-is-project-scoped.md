---
name: legacy-scheduler-stop-is-project-scoped
description: The v0.5.0 scheduler stop command drains one project coordinator but leaves its shared service running
metadata:
  type: gotcha
---

When handing a shared scheduler from v0.5.0 to a newer Fluent binary, run the
released `fluent scheduler stop` in every registered project to settle each
coordinator and its active Work. The released command does not accept `--all`
and does not stop the shared service. Stop the original supervised service
process separately, then verify that it exited and its health socket no longer
responds before starting the replacement. The v0.5.0 health response has no
service instance ID or state; use the stored project coordinator identity when
checking a stale stop request across this version boundary.

Exercise these steps against the pinned released executable. Current CLI syntax
and current health fields cannot establish what the predecessor supports.

Related: [[shared-service-ownership-spans-drain]],
[[old-reader-fences-use-recognized-wire-fields]].

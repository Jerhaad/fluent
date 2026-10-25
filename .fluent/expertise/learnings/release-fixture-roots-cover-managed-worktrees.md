---
name: release-fixture-roots-cover-managed-worktrees
description: Isolate release fixture projects with their sibling managed worktrees while keeping the scheduler socket path short
metadata:
  type: testing
---

Fluent creates managed Attempt worktrees beside a project, not inside the
project directory. A release test that allocates repeatable Work IDs in separate
temporary project roots can leave sibling worktrees behind when those project
directories are removed. Put the service home, registered projects, and their
managed worktrees under one private fixture parent, and remove that parent only
after the owned service and provider processes have settled. Test a second
allocation to catch stale worktree reuse.

The scheduler's Unix socket path also includes the service home below that
parent. On macOS, a deeply nested temporary root can exceed `sun_path` and
prevent the released service from starting. Check the computed socket path
against the platform limit before launch. Canonicalize the parent and observed
child paths before deriving fixture-relative artifact paths: `/tmp` and
`/private/tmp` can name the same directory.

Related: [[release-fixture-process-group-ownership]],
[[production-traces-bind-public-cause-to-model]].

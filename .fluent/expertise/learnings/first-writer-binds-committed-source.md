---
name: first-writer-binds-committed-source
description: A missing initial Writer workspace binds only from committed source; non-Fluent source dirt rejects before reservation, Git setup, baseline persistence, or coder launch
metadata:
  type: architecture
---

The first Writer candidate is created from committed Git state, not from the
source checkout's working tree. Before binding a missing initial Writer
workspace, `preflight_write_worktree` reads staged, unstaged, and untracked
status outside `.fluent/`. Any such change rejects the run while the Attempt and
Task remain planned, so the user can commit or revert the paths and retry the
same command.

This cleanliness check is part of the read-only preflight and must run before
Task reservation, branch or worktree creation, baseline persistence, and coder
launch. Keep Git's porcelain diagnostic intact so the rejection names all dirt
classes without re-parsing quoted path syntax. Tests must include a modified
tracked file as well as staged and untracked paths; two untracked files with one
staged do not cover the tracked-unstaged class.

The gate has two deliberate boundaries:

- Changes only under `.fluent/` retain their exception and do not block setup.
- Once the candidate workspace exists, recovery uses that authoritative
  workspace and does not reapply source-checkout cleanliness.

Related: [[atomic-task-start-reservation]],
[[host-owned-git-transaction-over-untrusted-coder]].

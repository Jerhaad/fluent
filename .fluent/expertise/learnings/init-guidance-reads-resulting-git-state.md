---
name: init-guidance-reads-resulting-git-state
description: After instruction seeding, fluent init derives commit-or-revert guidance from resulting Git state even when a later managed-file write failed
metadata:
  type: gotcha
---

`fluent init` can update one managed instruction file and then fail while
reading or writing a later `AGENTS.md` or `CLAUDE.md`. The successful earlier
write remains in the source checkout, so guidance cannot depend on the seeding
operation returning `Ok`.

After the seeding attempt, init independently inspects the resulting Git state
for each managed instruction path. It reports every changed file and explains
that the user must commit or revert it because candidate worktrees use committed
Git state. A later seeding warning and the Git-resolution guidance can therefore
appear together. Outside a Git worktree, this inspection produces no resolution
guidance.

Avoid manufacturing changes during this check: instruction seeding skips
`fs::write` when the rendered block is byte-identical to the existing file.
Re-running init with current managed blocks must preserve those files
byte-for-byte and must not claim that a new Git resolution is required.

The regression shape needs a Git-backed partial failure: let the earlier managed
file update successfully, obstruct the later file, and assert both the warning
and guidance derived from the actual remaining change. A fully successful seed
does not cover this path. Related: [[first-writer-binds-committed-source]].

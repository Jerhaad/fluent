---
name: git-subprocesses-use-central-wrapper
description: Production Git subprocesses must use src/git.rs rather than constructing git commands directly
metadata:
  type: convention
---

Route every production Git CLI invocation through the helpers in `src/git.rs`.
Do not add `Command::new("git")` elsewhere under `src/`, even for a small
identity or availability check. The central wrapper is the project boundary
for constructing Git subprocesses consistently.

The repository has a source-scan regression that rejects direct Git command
construction outside `src/git.rs`. Tests may construct Git processes to build
fixtures, but production modules must extend or reuse the wrapper.

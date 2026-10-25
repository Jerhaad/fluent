---
name: tester-tracked-mutations-restore-after-evidence
description: Work Task Testers retain command evidence while a host guard restores only tracked candidate side effects after every launch
metadata:
  type: architecture
---

Treat project test commands as deterministic evidence producers, not as trusted
custodians of the candidate worktree. Package managers and generators can update
tracked lockfiles even when a test fails. If those changes reach the
post-Tester candidate gate, the useful failure evidence becomes stranded behind
a dirty-worktree pause instead of reaching the corrective Writer.

Wrap every Work Task Tester launch, including retries and Tester-only host
retries, in one host-owned tracked-state transaction. Before launch, require the
exact candidate commit and a clean tracked index and worktree. After launch,
persist the command results and logs, record the changed tracked paths, restore
the index and worktree to the same commit, and verify both invariants before
interpreting or retrying the result. A restoration failure is a Tester harness
failure because subsequent evidence cannot be trusted against a frozen
candidate.

Do not use `git clean` in this transaction. Untracked and ignored caches are
often intentional test outputs and remain subject to project ignore rules and
the ordinary candidate gate. Also do not restore a candidate that was already
dirty or whose `HEAD` moved: those states cannot safely be attributed to the
Tester. Related: [[tester-readiness-shares-production-boundary]],
[[reserved-phase-terminal-finalizer]],
[[host-owned-git-transaction-over-untrusted-coder]].

For a candidate already stranded by an older build, recovery belongs only on
the explicit candidate-gate resume boundary. Require the persisted gate's exact
final-Tester phase, blocked disposition, dirty-status diagnostic, completed
Tester, expected commit, and current tracked status to agree, and reject any
untracked entry. That is a bounded migration of known Tester residue, not a
general license to clean dirty worktrees.

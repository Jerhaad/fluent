---
name: queue-admission-rechecks-after-evidence-preflight
description: Queue admission runs evidence checks outside the queue lock and rechecks the ledger before creating or reusing a dispatch
metadata:
  type: architecture
---

Queue resume and foreground execution must use the same recovery admission
contract. Revision audits, completion matrices, worktrees, and downstream
evidence are read-only but can require filesystem and Git work, so queue
admission validates them before taking the project-wide queue lock. That
preflight ledger read is only a hint: after acquiring the lock, reread the
ledger and compare it with the snapshot used for validation.

If the ledger changed while evidence was being inspected—especially when a
matching active dispatch was removed or terminalized—release the lock and rerun
the complete eligibility checks before creating a replacement dispatch. Claim
uses the same pattern for queued resume. Never let a stale "already queued"
branch bypass candidate, tester, ownership, cancellation, or planning checks.

Related: [[lock-ordering-across-subsystems]],
[[recovery-contracts-form-neutral-boundary]],
[[scheduler-resume-requires-explicit-dispatch-intent]].

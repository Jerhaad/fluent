---
name: recovery-contracts-form-neutral-boundary
description: Keep shared recovery admission, evidence parsing, and Writer completion checks in a caller-independent crate-private module
metadata:
  type: architecture
---

Shared recovery contracts belong in the neutral `recovery_contracts` module.
That module owns the persisted evidence schema and parser, managed-artifact
validation, candidate/review/evidence binding, replay predicates, and Writer
completion checks. `host_evidence`, the queue, and direct Task execution
consume those checks at their respective boundaries instead of maintaining
near-duplicate validators.

Keep the dependency one-way: the neutral contract must not depend on the
host-evidence producer or the Task executor that call it. Keep the surface
crate-private unless an external API is genuinely required. When a new route
can admit or launch a recovery-bound Writer, extend the shared contract and
invoke it from that route rather than adding a caller-specific exception.

Related: [[extract-logic-to-avoid-test-duplication]],
[[pure-domain-keeps-filesystem-authority-at-edges]],
[[audited-planning-recovery-binds-exact-frontier]].

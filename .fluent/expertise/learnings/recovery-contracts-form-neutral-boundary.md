---
name: recovery-contracts-form-neutral-boundary
description: Keep shared recovery admission, evidence parsing, and Writer completion checks in a caller-independent crate-private module
metadata:
  type: architecture
---

Shared recovery contracts belong in the neutral `recovery_contracts` module.
That module owns evidence parsing, managed-artifact validation,
candidate/review/evidence binding, replay predicates, and authoritative Writer
completion selection and checks. Shared persisted evidence and planning-audit
types live in `work_model`. `host_evidence`, the queue, and direct Task execution
consume the neutral checks instead of maintaining near-duplicate validators
or selecting different source matrices.

Keep the dependency one-way: the neutral contract must not depend on the
host-evidence producer, Task executor, or lifecycle callers. Lifecycle
recognition passes the recognized Task into neutral validation; calling back
into lifecycle recovery can create a transitive dependency on the executor.
Keep the surface crate-private unless an external API is required. When a new route
can admit or launch a recovery-bound Writer, extend the shared contract and
invoke it from that route rather than adding a caller-specific exception.

Recognize the specialized recovery before imposing its additional prerequisites.
A Plan containing host work does not make every interrupted Writer a host-only
recovery: an initial Writer or an unfinished continuation retains ordinary
foreground and queued retry. Test these ordinary controls alongside the narrow
host-only path so stricter evidence admission cannot strand unrelated work.

Related: [[extract-logic-to-avoid-test-duplication]],
[[pure-domain-keeps-filesystem-authority-at-edges]],
[[audited-planning-recovery-binds-exact-frontier]].

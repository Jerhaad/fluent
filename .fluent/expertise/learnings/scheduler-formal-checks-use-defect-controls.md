---
name: scheduler-formal-checks-use-defect-controls
description: Run the pinned scheduler TLC suite with passing protocols and expected defect controls, then test the real service boundary
metadata:
  type: testing
---

`tests/formal/scheduler/` holds bounded TLA+ models for queue handoff,
generation fencing, and cross-project service isolation. Its checker pins the
official TLC JAR by digest, requires Java 21, and runs from the repository root
with a caller-supplied JAR path. It treats a proposed protocol or a known
passing control as successful only when model checking completes. A historical
defect or mutant succeeds as a *test case* only when TLC reports its specific
expected violation. Do not collapse all nonzero TLC exits into proof that a
negative control worked.

The models cover abstract state transitions. Rust service tests must still
drive the real poll path, lease ownership, generation changes during recovery,
and child-process crash and restart. The real socket fixture needs a host that
can bind its Unix socket; a transportless fixture alone does not verify that
entry path.

Related: [[route-tests-drive-real-launch-wiring]],
[[passing-reviews-need-candidate-bound-inspection]].

---
name: formal-workflow-bounds-need-witnesses
description: Require reachable witnesses and successor coverage for bounded workflow safety searches
metadata:
  type: testing
---

The workflow TLA+ checks in `tests/formal/workflow/` use finite profiles and
state constraints. A completed safety search establishes its invariant only for
states inside that profile. When a profile claims to cover a route, rejection,
correction, or recovery interaction, require a witness or action coverage that
reaches the relevant state under the intended guard. Check rejection with the
guard enabled as well as a control that exposes the defect when it is disabled.

Inspect each state constraint across the whole claimed sequence. A bound can
admit a failed or pending state yet prune the successor after recovery clears a
failure flag, leaving publication or correction untested. Pair a positive
continuation witness with an exclusion control that loses that witness when the
successor is deliberately removed. Keep the README's coverage claims aligned
with the actual configured choices.

Related: [[scheduler-formal-checks-use-defect-controls]],
[[production-traces-bind-public-cause-to-model]].

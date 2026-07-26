---
name: persisted-policy-vs-resolved-execution-mode
description: Keep the persisted policy enum separate from the crate-private resolved execution mode, and add a third mode without widening a public Boolean surface — the new mode stays reachable only through a crate-private adapter, so external callers cannot construct a contradictory state
metadata:
  type: architecture
---

When a subsystem gains a third operating mode, the persisted *policy* and the
effective *execution mode* are two different concerns and belong in two different
types at two different visibilities:

- The **persisted policy** is public and durable: `work_model::LearnerMode` owns
  what the CLI stores (serde `default` = `capture`, omitted from the split record
  via `is_capture`, with `Display`/`FromStr` + a typed `ParseLearnerModeError` and
  round-trip + legacy-default tests). It records *what the user chose*.
- The **effective execution mode** is crate-private and derived:
  `work_task_executor::LearnerExecutionMode` is resolved from
  `(learner_mode, candidate_merged)` and is the *only* place capture / pre-land
  no-expertise / post-land handoff-only are distinguished. It decides *what
  confinement baseline runs*. Reviewers call this the right seam — persisting a
  choice is one concern, deriving the runtime baseline another.

Do **not** widen an existing public Boolean surface into a tri-state to admit the
new mode. `LearnerRunInputs { handoff_only: bool }` still maps only the two
original modes (capture = `false`, post-land = `true`); `run_learner_captured`
derives its mode from that Boolean, and production reaches the third mode
(`PreLandNoExpertise`) only through a crate-private adapter
(`run_learner_captured_in_mode`). Because the third mode is unreachable through
the public surface, an external caller cannot construct a contradictory
Boolean/enum state — the invariant holds *by construction*, not by validation.
The architecture reviewer verifies this compatibility contract explicitly for any
mode addition fronted by a Boolean.

When the new mode carries its own confinement/security policy, resolve that policy
off the private execution-mode enum too (e.g. `forces_sandbox`,
`expertise_writable`, `bundle_source`), reusing the existing confinement
primitives rather than adding a second security-sensitive path. Reject-not-normalize
ledgers and the pointer-identity gate for such a mode live in
[[host-owned-git-transaction-over-untrusted-coder]] and
[[pointer-identity-gate-verifies-reviewed-sha]]; persisted-field compatibility for
the policy enum follows [[backward-compatible-serde-fields]]; a test must not
re-derive the private gating expression by hand — assert through the production
decision function, per [[route-tests-drive-real-launch-wiring]].

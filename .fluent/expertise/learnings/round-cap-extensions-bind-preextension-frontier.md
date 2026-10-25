---
name: round-cap-extensions-bind-preextension-frontier
description: Round-cap extension records must bind the actual pre-extension Writer frontier and remain valid through replay
metadata:
  type: architecture
---

A `RoundCapExtension` is an authorization record, not a recalculated cap. When
Fluent creates one, it records the configured base cap, materialized Writer
count, and evidence-backed allowances observed before the extension. Validate
the first extension's old cap against that independent frontier, then validate
the bounded increment, derived identity, evidence accounting, and continuity
with every preceding extension. Run the pure chain check at `Attempt` model
validation so unrelated Work-model writes cannot preserve malformed authority.

The failed-review Approach-revision path and historical planning replay need the
stricter bound-frontier check. Legacy extensions without the newer fields may
remain readable for compatibility, but they cannot authorize this recovery.
Do not trust a coherently edited old-cap, new-cap, identity, base-cap, and
pre-extension tuple; a public rejection test must tamper those fields together
and prove that neither the aggregate nor its artifact tree changes.

Related: [[audited-planning-recovery-binds-exact-frontier]],
[[backward-compatible-serde-fields]],
[[semantic-validation-tests-reach-target-branch]].

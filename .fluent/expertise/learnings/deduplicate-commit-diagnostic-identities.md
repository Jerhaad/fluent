---
name: deduplicate-commit-diagnostic-identities
description: Resolve abbreviated Git IDs against unique full commits when several gate findings describe one commit
metadata:
  type: gotcha
---

One commit can produce several commit-metadata findings, such as a malformed
message and an invalid identity. When a later diagnostic names that commit by
an abbreviated hash, collect matching full commit identities and deduplicate
them before deciding whether the abbreviation is unique. Deduplicate only the
identity candidates; retain distinct `(commit, reason)` findings so the gate
still validates every reason and its multiplicity.

Include a fixture with multiple metadata findings for one commit. Otherwise a
resolver can mistake repeated reasons for ambiguous Git identity and reject a
valid derived recovery. Related:
[[negative-test-cross-product-coverage]],
[[candidate-gate-owns-derived-recovery-policy]].

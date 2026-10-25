---
name: baseline-attribution-does-not-satisfy-references
description: Exclude proven pre-existing suite failures from Writer defects while still requiring passing evidence for every explicit reference
metadata:
  type: architecture
---

Fluent's retained pre-write baseline explains suite health. The shared
verification assessment excludes matching pre-existing native failures from
candidate defects, so an unrelated review finding cannot turn them into new
Writer obligations. Gate decisions, status, Writer requirements and context,
and Attempt advancement must consume the same attribution.

Explaining a command's nonzero exit requires matching command position, text,
harness, clean process settlement, and unambiguous native failures covered by
the baseline. An equal exit code alone does not establish a pre-existing cause.
Keep the baseline artifact binding with the assessment rather than rewriting
the current result or reclassifying it in each consumer.

Baseline attribution does not provide passing evidence for an explicit `Test:`
reference. A referenced native test must still pass, and a referenced command
needs its exact passing occurrence or applicable accepted replacement proof.
Missing references, integrity errors, and introduced defects retain their
respective blockers even when other failures match the baseline.

Exercise baseline and current runs through the production Tester, then compare
the real consumers with an unrelated corrective review and a missing or failing
explicit reference. Include introduced-failure and absent-baseline controls.

Related: [[tester-failure-routing-requires-proven-cause]],
[[required-checks-outlive-individual-results]],
[[capture-landing-owns-exact-tester-evidence]].

---
name: landing-recovery-tests-check-publication
description: Landing recovery tests must compare the retained intent with the published merge and its downstream effects
metadata:
  type: testing
---

An intact `LandingIntent` does not prove that recovery published its accepted
result. Tests of interrupted landing should drive the public land command and
compare the merged candidate's check and review artifact references with the
intent. Read the referenced files before and after retry to ensure the accepted
evidence bytes remain unchanged. In capture mode, use an admitted hook change
that creates a distinct verified tip, then assert that the intent, merged
result, candidate pointer, and Write Task provenance all name that tip.

When the test also claims idempotent post-land processing, inspect a
materialized follow-up and compare its identity and bytes across another land
retry. These assertions distinguish retained state from the actual publication
and downstream effects. Related: [[capture-landing-owns-exact-tester-evidence]],
[[post-land-effects-are-idempotent-and-land-safe]],
[[test-fixtures-use-production-state]].

---
name: capture-landing-owns-exact-tester-evidence
description: Capture landing owns final-candidate verification against the exact current target and promotes that evidence only after fast-forward
metadata:
  type: architecture
---

Capture-mode landing compares the final candidate after rebase and any
`fix-pre-merge` commit with trusted Tester evidence for the target commit recorded
when landing starts. The Attempt-start baseline cannot substitute for that target:
newer target failures may be accepted even when the original baseline passed.
Resolve an exact cached target baseline or capture one in a detached checkout of
that commit, leaving the target checkout unchanged.

Reuse final Tester evidence only when its commit, Tester configuration,
extractor, Fluent version, host platform, sandbox mode, and artifact digests
validate. Otherwise run the final candidate through the existing Tester sandbox
and tracked-state guard. Harness errors, process leaks, newly failing test IDs,
and unexplained failed commands block fast-forward. Recheck candidate identity
and cleanliness and the target head before moving the target to the verified SHA.

The Merge Candidate owns this verification through `merge_state.check_artifacts`.
Landing creates no additional Attempt Tester or Reviewer Tasks and preserves the
completed Attempt's review, Tester, and Learning records. Give each verification
invocation a separate retained artifact directory so a retry preserves earlier
failure evidence. Record a failed verification and repair guidance on the Merge
Candidate while retaining its workspace.

Validate reusable candidate evidence before land without publishing it as a
landed baseline. After durable fast-forward, promote that exact artifact instead
of immediately repeating the successful verification. Post-land fallback capture
remains available when exact evidence is unavailable. Keep these capture-mode
rules separate from the identity-preserving `no-expertise` landing route in
[[pointer-identity-gate-verifies-reviewed-sha]]. Related:
[[tester-tracked-mutations-restore-after-evidence]],
[[attempt-scoped-cargo-cache-shared-with-final-tester]],
[[post-land-effects-are-idempotent-and-land-safe]].

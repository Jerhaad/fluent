---
name: expired-evidence-renews-through-explicit-attachment
description: Renew expired proof while preserving history, excluding inactive finding Reviews from current decisions, and retaining stop authority
metadata:
  type: architecture
---

Live-state proof can expire while its targeted Tests Review is planned or after
that Review completes but before Fluent consumes it. Rejecting expired proof
without allowing replacement strands the Attempt: resume cannot consume it and
the active recovery prevents a second attachment. Apply the same renewal
transition to canonical, Plan, reviewer-requested, and carried-finding evidence.

Recognize expiry through the shared read-only recovery contract. Validate all
other bindings at the evidence's recorded execution time so expiry cannot excuse
changed source bytes, a changed candidate, or moved comparison inputs. Report
the original attachment selector only when the candidate and settled ownership
permit it; status must not retire records or publish replacement artifacts.

Fresh explicit attachment validates the new proof and rechecks ownership under
the land and Work-model locks. Preserve old snapshots and completed Reviews,
supersede only the expired recovery's idle Review reservations, and require a
new targeted Tests Review. An identical retry reuses the new recovery's identity.
Cancellation still requires settled owners and explicit current resume authority;
expiry alone grants no permission to restart execution.

For carried findings, a retained failed Review can still be a completed Task
after fresh proof replaces its recovery. Exclude Reviews belonging to inactive
carried-finding recoveries consistently from ordinary review detection, review
planning, and latest-verdict selection. Checking only for accepted recoveries
misses failed or expired recoveries whose Reviews remain in history. Use the
typed recovery relation; retain the active targeted Review and unrelated ordinary
Reviews in their existing decision paths. Verify replacement through passing
targeted Review and public Attempt execution, requiring ordinary reviews, Tester,
and Learning without another Writer while preserving the old Tasks and bytes.

Use the evidence evaluation clock to test the exact expiry boundary separately
from lifecycle timestamps. Drive public attachment, Review, and resume for each
source, covering expiry both before Review launch and after Review completion,
and continue to same-Attempt readiness. Initial attachment rejection alone does
not test renewal.

Related: [[required-checks-outlive-individual-results]],
[[recovery-contracts-form-neutral-boundary]],
[[scheduler-resume-requires-explicit-dispatch-intent]].

---
name: expired-evidence-renews-through-explicit-attachment
description: Renew expired proof across evidence sources while retaining original history and stop authority
metadata:
  type: architecture
---

Live-state proof can expire while its targeted Tests Review is planned or after
that Review completes but before Fluent consumes it. Rejecting expired proof
without allowing replacement strands the Attempt: resume cannot consume it and
the active recovery prevents a second attachment. Apply the same renewal
transition to canonical, Plan, and reviewer-requested evidence.

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

Use the evidence evaluation clock to test the exact expiry boundary separately
from lifecycle timestamps. Drive public attachment, Review, and resume for each
source, covering expiry both before Review launch and after Review completion,
and continue to same-Attempt readiness. Initial attachment rejection alone does
not test renewal.

Related: [[required-checks-outlive-individual-results]],
[[recovery-contracts-form-neutral-boundary]],
[[scheduler-resume-requires-explicit-dispatch-intent]].

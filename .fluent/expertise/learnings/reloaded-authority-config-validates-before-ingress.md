---
name: reloaded-authority-config-validates-before-ingress
description: Validate every live authority configuration reload before persisting ingress or evaluating an approval
metadata:
  type: gotcha
---

Validating runner configuration only at construction leaves a lifetime-dependent
authority gap. Operators can change the on-disk participant, approver, provider,
or project policy while a runner remains alive; using that fresh file without
revalidation can authorize an invalid policy.

Whenever ingress or an approval reloads authority configuration, validate the
fresh value before persisting the inbox event, selecting a participant policy,
or evaluating the action. Replace cached configuration only with that validated
value and fail closed on malformed or incomplete authority. A later valid reload
must also replace an earlier invalid constructor snapshot instead of leaving a
stale cached error in force.

Test both directions with one long-lived runner: valid-to-invalid must reject
before inbox mutation, while invalid-to-valid must recover using the current
disk configuration. Related: [[config-fails-open-only-for-diagnostics]].

# Validate an interrupted recovery before publishing it

## Context

Use this when an external recovery command must reject changed input without
recovering an unrelated pending Work transaction.

## Mechanism

Inspect the transaction's validated target without publishing its split records.
Require the exact saved operation identity and immutable artifact bindings, then
repeat that check under the model lock before native transaction recovery.
Continue through the ordinary reducer after recovery; its exact replay path must
return the saved identities without another write. A read-only aggregate snapshot
may be unavailable while child records belong to the pending target.

## Example

`host_evidence::attach_finding` reads `read_work_item_recovery_target` and uses
`mutate_work_item_from_external_with_recovery_check`. The public CLI test
`carried_finding_evidence_retry_recovers_interrupted_publication` interrupts real
split publication after child writes, rejects changed proof without touching the
journal, and retries matching proof through the native transaction path.

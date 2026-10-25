---
name: local-execution-identity-requires-receipt
description: Resolve versioned local execution ownership from a generation-bound child receipt and inherited lock, not record state alone
metadata:
  type: architecture
---

For version 2 local executions, `execution.json` reserves a fresh launch
generation before child creation. The child publishes `execution-receipt` with
that generation and its PID before entering the requested program, after
checking that its process group equals its PID. Recovery derives the group from
the matching receipt and requires the inherited identity lock before signaling.
The child also retains the Work model lock through exec so a durable stop cannot
overtake the final launch boundary if the parent dies.

Treat the receipt as a separate authority from the record's lifecycle state.
Zero leader and group fields do not prove that launch never reached a child: a
partial, stale, or malformed receipt still blocks an unlaunched recovery path.
Conversely, a settled record does not prove that descendants released the
identity lock. Reject replacement while that lock is held, then use the
validated receipt to stop the owned group. A genuinely absent receipt permits
only the narrowly proven unlaunched recovery paths.

Version 1 records retain their established ownership rules. A version 2 record
without a matching receipt cannot authorize a signal. See
[[old-reader-fences-use-recognized-wire-fields]] for the separate Work storage
compatibility fence and the
[child launch testing pattern](../testing/patterns/test-child-owned-launch-receipts.md)
for process-level race coverage.

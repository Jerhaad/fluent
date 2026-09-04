# Recovery replay corpus

This small, provider-free corpus records the recurring Fluent recovery failures
that motivated the shared pause records, recovery-operation records, and
explicit model upgrades. It intentionally contains synthetic identifiers and
relative placeholders rather than project checkouts, host paths, credentials,
or provider transcripts.

`src/recovery_replay.rs` loads the versioned JSON and replays every Attempt case
through the same candidate-bound operation validator used by public recovery
commands. Each replay verifies that:

- the saved command names the Work Item and Attempt;
- the current candidate is accepted;
- repeating the exact operation returns the original record without duplication;
- a stale candidate is rejected before mutation; and
- the original Task history and operation record survive serialization.

Workflow cases document public reconciliation commands whose state lives outside
an Attempt, such as an already-landed Merge Candidate. Their command and history
are still checked for safe, machine-independent shape.

Add a new case when a recovery failure reaches a user-facing handoff. Keep the
description short, use synthetic commit IDs, and update the schema only when
the replay meaning changes. This corpus is not a replacement for the focused
module tests that exercise each recovery's Git and artifact checks; it is the
stable cross-recovery regression set that catches missing or stale next actions.

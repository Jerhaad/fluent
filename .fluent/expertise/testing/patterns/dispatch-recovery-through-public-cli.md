# Dispatch recovery through the public CLI

## Context

Use this pattern when a queue recovery regression must exercise claim, Attempt
advancement and readiness in an environment that cannot apply nested Seatbelt.
Keep real confinement assertions in the existing profile-enforcement tests.

## Mechanism

Implement the existing `scheduler::AttemptRunner` test seam with a driver that
reads the active fixture dispatch and runs the real `fluent attempt run` binary
with its exact `--dispatch-id`, explicit `--no-sandbox`, and isolated provider
doubles. Return `scheduler::classify_attempt_outcome` from the resulting model.
Use production `pick_next_queued` and `dispatch_one` to claim and settle it.
This replaces the outer launcher, while production code still owns admission,
Task execution, evidence validation, Review acceptance, Learning and readiness.

Create the dispatch through public queue commands. Scope test-support registry
state to the fixture and remove provider credentials through `fluent_cmd`.
Assert that repeated resume keeps the active dispatch ID, and that a later stop
requires fresh authority. Check process and Task-lease settlement before fixture
cleanup, which would otherwise hide leaked ownership.

## Example

`EvidenceCliRunner` in `tests/binary.rs` runs the public Attempt CLI.
`evidence_recovery_retries_preserve_task_and_dispatch_identity` and
`combined_evidence_recovery_resumes_to_ready_candidate` exercise the production
queue transition, retained artifact bytes, settled owners and final readiness.
The saved combined-history test imports historical state only before recovery;
after that point, public commands must resolve each blocker.

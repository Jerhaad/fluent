# Snapshot split Work state for trace checks

## Context

Use this when a production trace needs both a Work Item and its Merge
Candidates. Fluent persists these in separate records.

## Mechanism

Read the aggregate through `WorkModelStore::read_work_item`. Serialize the
Work Item and add its `merge_candidates` field to the trace snapshot explicitly.
Store that combined snapshot as a digest-bound artifact and compare every event
fact with the artifact in an independent projector. Keep candidate readiness
checks based on the Merge Candidate fields, not an assertion result from the
test driver.

## Example

`tests/binary.rs::record_integrated_work_trace` adds the Merge Candidates from
the loaded aggregate to each captured Work snapshot. The projectors under
`tests/formal/workflow/` then check that readiness follows the required proof.

# Scheduler protocol models

`check.py --suite=recovery --jar=target/fluent-tools/tla2tools-1.7.4.jar`
checks the candidate configurations and confirms that each historical or
negative control violates its named invariant. It requires the official TLC
1.7.4 JAR with SHA-256
`936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`
and Java 21. The checker never downloads a tool. It writes complete TLC output
under `target/formal/scheduler/`.

The `*-historical-6875b409.cfg` configurations represent the missing recovery,
generation fence, and per-project isolation at audit commit `6875b409`. The
negative configurations independently allow duplicate Attempt creation, stale
identity writes, immediate retries, or lost worker handles. The candidate
configurations enable recovery and fences while retaining all workers.

These bounded sources reconstruct the audit scenarios from the approved Brief
and Approach. The original pilot model files were not accessible in this
Writer workspace; compare these sources with the pilot before treating them
as a verbatim promotion.

| Model action | Production action |
| --- | --- |
| `QueueHandoff.Claim`, `Acquire` | `scheduler::claim_next` claims then acquires the dispatch lease |
| `CreateAttempt`, `Run` | `scheduler::run_claimed_dispatch` ensures the bound Attempt and marks it running |
| `DurableFinish`, `Publish` | The Attempt runner persists its outcome; `queue::reconcile` publishes it |
| `Crash`, `Recover` | Process loss releases the lease; `SchedulerService::poll_project` invokes `recover_and_reconcile` |
| `GenerationFence.AdvanceGeneration`, `ReplaceIdentity` | A newer queue mutation changes the dispatch generation or identity |
| `StaleRecover` | `queue::*_if_current` rejects an old dispatch ID or generation |
| `ServiceIsolation.Poll` | `SchedulerService::poll_once` visits every registered project |
| `Spin` | An unsupported immediate retry between configured polls |
| `SettleBad`, `SettleGood` | `reap_finished`, `join_workers`, and `settle_worker` join owned handles |

The queue model uses one logical dispatch, one possible crash, and generations
0–8. The isolation model uses two projects, two polls, and two workers. These
models check the listed invariants for their finite state spaces. Rust service
tests supply the production-boundary conformance checks; the models do not
establish behavior under arbitrary filesystem failures or power loss.

# Scheduler protocol checks

`QueueHandoff.tla`, `GenerationFence.tla`, `ServiceIsolation.tla`, and the eight
top-level configurations are exact copies of the approved state-transition
audit inputs. From the repository root, run
`python3 tests/formal/scheduler/check.py --suite=recovery --jar=target/fluent-tools/tla2tools-1.7.4.jar`.
This checks each expected result. It requires the official TLC
1.7.4 JAR (SHA-256
`936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`)
and Java 21. It downloads nothing and stores full logs under
`target/formal/scheduler/`. A missing tool fails the selected host check.

The audit at commit `6875b409` found missing claim recovery and project error
containment. `queue-current.cfg` and `isolation-current.cfg` reproduce those
failures; the corresponding `*-proposed.cfg` checks the proposed protocols.
Generation fencing already worked at that commit: `generation-current.cfg` is
a passing control, while `generation-mutant-no-fence.cfg` deliberately removes
the fence. The queue mutants independently disable the lease check and bound
Attempt reuse. The audit's selected TLC results and hashes were supplied as
local execution input for this Attempt at
`.fluent/drafts/20261007-state-transition-assurance/model/approved-execution-inputs.txt`.

| Model action | Production boundary |
| --- | --- |
| `QueueHandoff.Claim`, `AcquireLease` | `scheduler::claim_next` publishes the claim and acquires the dispatch lease in the coordinator frame |
| `EnsureAttempt`, `MarkRunning` | `scheduler::run_claimed_dispatch` binds the existing or newly created Attempt and publishes running state |
| `CompleteAttempt`, `Settle` | The Attempt runner stores its outcome; the worker settles the dispatch |
| `Crash`, `CrashAll`, `Restart`, `Recover` | A lost process releases its lease; `SchedulerService::poll_project` calls `recover_and_reconcile` before fresh claims |
| `GenerationFence.Read`, `Commit` | Recovery reads a dispatch generation, then `queue::*_if_current` accepts only the current dispatch and generation |
| `ServiceIsolation.PollProject`, `NextPoll` | `SchedulerService::poll_once` visits registered projects once per configured poll and contains project-local failures |
| `Repair`, `Restart` | A repaired project becomes eligible on a later poll; a restarted service continues polling |

`extensions/` preserves the Writer's earlier reconstructed models and their
additional controls separately. They are exploratory extensions, not the audit
sources or historical counterexamples, and the approved host command does not
run them. The Rust service tests check the production entry path, worker
ownership, and dispatch fencing that the abstract models cannot establish.
The bounded models do not prove arbitrary filesystem errors, power loss, or
unbounded concurrency safe.

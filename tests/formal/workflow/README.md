# Workflow foundation checks

From the repository root, run the focused public-command and projection tests:

```sh
cargo nextest run --offline --features test-support --test workflow_assurance --no-tests fail workflow_foundation_public_trace
cargo nextest run --offline --features test-support --test workflow_assurance --no-tests fail workflow_foundation_projection_
```

The host foundation check uses a fresh run of the first command and then checks the bounded model and negative controls:

```sh
python3 tests/formal/workflow/check.py --suite=foundation --jar=target/fluent-tools/tla2tools-1.7.4.jar
```

The checker requires Java 21 and the official TLC 1.7.4 JAR with SHA-256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`. It accepts `java` on `PATH` or a Java 21 installation extracted under `target/fluent-tools/jdk21/`. It downloads nothing. A missing suite, tool, complete TLC search, fixture, or required trace fails. It writes one log per model configuration, the fresh fixture log, projection log, negative trace log, and `result.json` under `target/formal/workflow/foundation/`. The Rust tests are discovered by the existing `cargo nextest` command in `.fluent/tester.yaml`; the final Tester runs that declared suite after review.

## Model and bounds

`Workflow.tla` names Work, Attempt, Task, Workspace, Merge Candidate, dispatch, service, worker-generation, and recovery-operation identities as typed tuples. Its state distinguishes required and delivered inputs, raw review evidence and credited evidence, cancellation and resume authority, pending and published result records, Git effect and result publication, stored stage and displayed observation, and cleanup eligibility. Actions cover authorized intake, foreground or queued dispatch, Writer, ordinary and targeted review, Tester, both Learner modes, final landing verification, landing, follow-up, review-only Work, observation, cleanup, crash/recovery, write failure/retry, and version transition.

`foundation.cfg` has two competing Work identities, one of which takes the review-only branch. A service replacement has generations 1 and 2. At most one reported crash or write failure occurs across those Works. `repeated-recovery.cfg` has one Work and permits two failures. The seven `negative-*.cfg` files each use one Work and independently remove input delivery, evidence credit, resume fencing, one-effect guard, truthful observation, cleanup guard, and version guard. `check.py` requires the named invariant violation for each control. The profiles use finite ordinals and do not restrict away canceled, stale-generation, uncredited, partially published, or failed-write states.

`FairSpec` applies weak fairness to enabled recovery, I/O recovery, and result publication. `RecoveryProgress` therefore says an observed failed state eventually clears if the process keeps taking enabled recovery actions. End-to-end progress also assumes authorized continuation, access, valid evidence, human decisions, and an end to persistent faults; this foundation model does not claim that those external preconditions always arrive. The finite model checks safety under the bounded interleavings. It abstracts repository bytes, Git ancestry, worker process trees, sandbox enforcement, provider text, and persisted record serialization into state fields and actions. No machine-checked Rust refinement proof is claimed. Sudden power loss, OS crashes, remote execution, installation, and release publication are outside this local scope.

## Version-1 trace

`workflow_foundation_public_trace` creates an isolated Git project and runs public `work-item create`, `work-item show`, `attempt create`, `attempt cancel`, `task run`, and another `work-item show`. It captures four raw JSON snapshots, exit codes, and command output. The attempted Task run must be refused while cancellation remains active. `project.py` validates each event against its snapshot, checks causal order and identity, and derives the state sequence `intake → planned → canceled → canceled` without using a production acceptance boolean. It refuses incomplete, malformed, ambiguous, or contradictory observations. The negative Rust test rebinds changed snapshots to valid digests before testing identity and cancellation invariants.

The JSON envelope has exactly these fields:

| Field | Type and meaning |
| --- | --- |
| `schema-version` | Integer `1` |
| `scenario` | String `foundation-public-cancel` |
| `fluent-commit` | Source commit SHA from the candidate checkout |
| `fluent-binary-sha256` | Hex SHA-256 of the executed Fluent binary |
| `model-profile` | String `foundation` |
| `collection-complete` | Boolean; all required observations were collected, independent of workflow completion |
| `events` | Ordered array of event records |
| `artifacts` | Array of `{path, sha256}` records; paths are fixture-relative, limited to the four named snapshots |

Each event has integer `sequence`, string `actor`, string `operation-id`, nullable `generation`, string `boundary`, integer-array `causal-predecessors`, and object `raw-facts`. The four boundaries are `intake`, `attempt-create`, `cancel`, and `run-refused`. Every boundary records integer `exit-code` and the full parsed `work` snapshot from `work-item show`; `run-refused` also records string `stdout` and `stderr`. The fixture uses controlled command order as its barrier. Missing, duplicate, unordered, or artifact-inconsistent facts fail projection. The checker compares the trace commit and binary digest with the candidate it just built, so a retained trace from another candidate cannot satisfy the host check.

The checker stores model results, projection result, raw production trace, and corrupted-trace result separately. Its corrupted trace changes a bound Work identity and must fail with the exact artifact mismatch; the Rust test also checks an invented advancement after cancellation. [The connection inventory](connections.md) maps every workflow boundary to the six existing detailed protocol models, explicit mapping checks, production coverage owners, and pending evidence. Those older models are passing checks of their own protocols; they are not composed whole-workflow assurance. The foundation proves a narrow real trace and records the rest as obligations for the subsequent Work Items.

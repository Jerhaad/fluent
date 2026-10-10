# Workflow foundation checks

## Recovery observation

The `recovery-observation` suite checks repeated failure and recovery profiles,
the existing service competition profile, cancellation, repeated-effect,
observation and cleanup negative controls, and a joint service/child settlement
boundary. `JointSettlement.tla` keeps a child's ownership receipt across two
service crashes. It permits publication of a completed stop only after the
child settles; its negative configuration allows early publication and must
violate the invariant. This closes the atomic worker-clear assumption in the
separate `ServiceLifecycle` model at the modeled handoff. The Rust cancellation
test remains the production check for stale authority, while a live child
crash trace remains a separate connection obligation.

The fresh public continuation trace starts with a completed Writer whose
matrix has the malformed verification result `pass; extra text`. The public
`work-item show` response names `attempt repair-writer-completion` as the next
action. The trace then records that repair and a public `attempt run` which
completes one corrective Writer while preserving the original matrix and
candidate. The next action names evidence attachment for the remaining host
row. `project_recovery.py` derives `paused → authorized → continued` from
the Work snapshots and commands. Rebound stale guidance and a missing Writer
transition fail its negative controls. `RecoveryTrace.tla` checks the derived
transition order. The trace proves this recovery case; the other public
failure routes remain covered by their focused Rust tests.

Run the focused projector on an already captured trace with:

```sh
python3 tests/formal/workflow/project_recovery.py target/formal/workflow/recovery-observation/production-trace.json
```

The required host suite is:

```sh
python3 tests/formal/workflow/check.py --suite=recovery-observation --jar=target/fluent-tools/tla2tools-1.7.4.jar
```

It refreshes the public trace, checks that the source commit and binary match
the candidate, and writes model logs, negative-control logs and `result.json`
under `target/formal/workflow/recovery-observation/`. A missing JAR, Java 21,
fresh trace, completed model search or required negative control fails the
suite. The operator can resume a malformed Writer completion with the exact
`repair-writer-completion` command shown by `work-item show`; after the Writer
completes, use its newly displayed `evidence attach` command for any remaining
host-owned obligation. Cleanup retains an origin while required follow-up
publication is unfinished and rechecks eligibility before removing it.

## Evidence status

The `evidence-status` suite captures a public Attempt, advances the target,
lands the rebased Merge Candidate, and reads `status` and `work-item show` after
the source checkout is retired. `project_evidence.py` checks the raw Work
snapshots, Tester result and manifest, target baseline, command logs, and public
display. It derives `ready → target-advanced → landed → observed` from those
artifacts. `EvidenceTrace.tla` checks the corresponding `Workflow` target
baseline, landing result, verification, intent, Git effect, publication, and
observation actions. A changed Tester candidate, command occurrence, or display
result and a missing observation each fail projection after their artifact
digests are updated. Model controls independently reject bad evidence credit,
misbound or new landing results, and an untruthful observation; the known
landing failure witness keeps accepted baseline treatment reachable.

The Writer's focused trace check is:

```sh
python3 tests/formal/workflow/project_evidence.py target/formal/workflow/evidence-status/production-trace.json
```

The host runs the complete bounded suite against the committed candidate with
`python3 tests/formal/workflow/check.py --suite=evidence-status
--jar=target/fluent-tools/tla2tools-1.7.4.jar`. Its logs and `result.json` stay
under `target/formal/workflow/evidence-status/`. The projector checks the
observed final commit separately from Fluent's status decision; a success label
cannot make a mismatched artifact pass.

## Classified Writer inputs

The `writer-inputs` suite runs the exact classified Writer launch test, checks
its captured classification, prompt, and Work snapshots with
`project_writer.py`, and maps the observed delivery and completion boundaries
through `WriterTrace.tla` to the `Workflow` actions. The classified Work snapshot
and provider invocation establish the dispatch precondition; fixture setup
before classification is outside the checked path. The projector checks the
instruction bytes and the audit's candidate and evidence bindings before it
marks delivery. A missing delivery event or a digest consistent trace with the
instruction removed must fail projection; the model's
`negative-deliverinputs.cfg` independently checks that an omitted delivery
violates `InputSafe`. Run the suite with:

```sh
python3 tests/formal/workflow/check.py --suite=writer-inputs --jar=target/fluent-tools/tla2tools-1.7.4.jar
```

The suite writes its trace, artifacts, model logs, and structured result under
`target/formal/workflow/writer-inputs/`. Its ordinary provider probe records
actual instruction reads. The separate required host command sets
`FLUENT_WORKFLOW_REQUIRE_HOST=1` and applies Seatbelt to test exact-file access
and denial of an unrelated sibling.

From the repository root, run the focused public-command and projection tests:

```sh
cargo nextest run --offline --features test-support --test workflow_assurance --no-tests fail workflow_foundation_public_trace
cargo nextest run --offline --features test-support --test workflow_assurance --no-tests fail workflow_foundation_projection_
```

The host foundation check uses a fresh run of the first command and then checks the bounded model and negative controls:

```sh
python3 tests/formal/workflow/check.py --suite=foundation --jar=target/fluent-tools/tla2tools-1.7.4.jar
```

To repeat just the one-failure combined search or the guarded rejection searches, add `--focus=combined-one-failure` or `--focus=guarded` to that command.

The checker requires Java 21 and the official TLC 1.7.4 JAR with SHA-256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`. It accepts `java` on `PATH` or a Java 21 installation extracted under `target/fluent-tools/jdk21/`. It downloads nothing. A missing suite, tool, complete TLC search, fixture, or required trace fails. It writes one log per model configuration, the fresh fixture log, projection log, negative trace log, and `result.json` under `target/formal/workflow/foundation/`. The Rust tests are discovered by the existing `cargo nextest` command in `.fluent/tester.yaml`; the final Tester runs that declared suite after review.

## Model and bounds

`Workflow.tla` stores Work, Attempt, Task, Workspace, Merge Candidate, dispatch, service, worker-generation, and recovery-operation identities as typed tuples. Its state distinguishes required and delivered input bindings, raw review evidence and credited evidence bound to generation, candidate, requirement, context, and review scope, cancellation and resume authority, pending and published result records, Git effect and result publication, stored stage and displayed observation, and cleanup eligibility. Actions cover authorized intake, foreground selection or queue admission and shared service dispatch, Writer, ordinary and targeted review, same-candidate review carry-forward with its own recorded review kind, a changed candidate with new requirement and context, Tester acceptance of the current candidate, capture and no-expertise Learning with distinct expertise effects, final landing verification, landing, follow-up, review-only Work, observation, cleanup, crash/recovery, write failure/retry, and version transition. Stale generation, candidate, requirement, context, scope, and unrelated evidence remain uncredited; replacement generations return credited Work to review. After a generation replacement, a separate authority grant enables a fenced resume. Paused unfinished Work permits a version transition. The review-only route tests before reviewing, can fail, cancel, resume, observe, and transition version, and completes without Writer, Learner, Merge Candidate, or Git effects.

`foundation.cfg` checks one ordinary Work through both execution modes, correction, review carry-forward, Tester, Learner, landing, cancellation, observation, and version changes. `combined-one-failure.cfg` covers the foreground route with one injected failure, correction, carry-forward, landing, observation, and version change. It requires action coverage for crash, write failure, both recovery paths, correction, carry-forward, target baseline rebinding with unchanged failures, Git effect, and result publication. Separate witnesses reach publication after recovery before the Git effect, a reviewed correction after an ownership crash and recovery, and a truthful observation of pending publication during a failed result write. The observed-recovery witness requires that same observed failed write to recover and publish; its exclusion control checks that the witness disappears when the bound removes the recovery successor. `repeated-recovery.cfg` exercises two failures. Canonical result choices include passing, an unchanged known failing test, a new failing test, incomplete output, a harness error, a process leak, an unexplained failed command, bad candidate binding, and a nonpassing explicit reference. The pre-write baseline and landing target baseline have separate bindings and failure sets; the target can change at landing. The Tester guard checks the retained pre-write baseline and explicit-reference pass. A distinct landing result binds the exact candidate after rebase and checks the trusted target baseline. `review-only.cfg` checks the separate route through Tester, review, and completion, including cancellation, generation replacement, failure, observation, and version states. `negative-reviewroute.cfg` disables route separation and must expose a review-only Git effect. `service-competition.cfg` checks two ordinary Works competing for one service slot through dispatch and Writer completion; its state constraint stops at Writer completion because the full later stages are covered by `foundation.cfg`. It retains cancellation, replacement generation, and one failure in the competing state space. `repeated-recovery.cfg` has one Work and permits two failures; it fixes the foreground route and excludes correction, review carry-forward, observation, version change, cleanup, and target change. Other profiles check those independent choices. The `negative-*.cfg` files each use one Work and independently remove input delivery, evidence credit, resume fencing, one-effect guard, truthful observation, cleanup guard, version guard, Tester baseline guard, or landing target baseline guard. Dedicated canonical and landing controls exercise new failures, incomplete output, harness error, process leak, unexplained command failure, bad candidate binding, nonpassing reference, and untrusted or misbound baseline evidence. `known-failure-accepted.cfg` and `known-landing-accepted.cfg` witness acceptance of an unchanged failing test at the two boundaries. The checker generates bounded, guard-enabled configurations for every invalid canonical and landing outcome, corrupt pre-write binding, and invalid landing target baseline. It requires a complete safety search and a reachable rejection witness for each case; generated configurations and TLC logs are saved under `target/formal/workflow/foundation/`. `check.py` requires the named invariant violation for each control. The profiles use finite ordinals and retain canceled, stale-generation, uncredited, partially published, and failed-write states on the ordinary route.

The checker also requires positive TLC action coverage for the distinct route, correction, evidence reuse, Learner, recovery, and publication actions. A passing invariant over a profile that never reaches one of those actions does not satisfy the suite.

`FairSpec` applies weak fairness to enabled recovery, I/O recovery, and result publication. `RecoveryProgress` therefore says an observed failed state eventually clears if the process keeps taking enabled recovery actions. End-to-end progress also assumes authorized continuation, access, valid evidence, human decisions, and an end to persistent faults; this foundation model does not claim that those external preconditions always arrive. The finite model checks safety under the bounded interleavings. It abstracts repository bytes, Git ancestry, worker process trees, sandbox enforcement, provider text, and persisted record serialization into state fields and actions. No machine-checked Rust refinement proof is claimed. Sudden power loss, OS crashes, remote execution, installation, and release publication are outside this local scope.

## Version-1 trace

`workflow_foundation_public_trace` creates an isolated Git project and runs public `work-item create`, `work-item show`, `attempt create`, `attempt cancel`, `task run`, and another `work-item show`. It captures four raw JSON snapshots, exit codes, and command output. The attempted Task run must return the public durable-cancellation diagnostic while cancellation remains active. `project.py` validates that diagnostic and each event against its snapshot, checks causal order and identity, and derives the state sequence `intake → planned → canceled → canceled` without using a production acceptance boolean. It refuses incomplete, malformed, ambiguous, or contradictory observations. The negative Rust test substitutes an unrelated failure and rebinds changed snapshots to valid digests before testing identity and cancellation invariants.

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

The checker stores model results, projection result, raw production trace, and corrupted-trace result separately. `Trace.tla` checks the projected sequence against the named intake, attempt creation, cancellation, and refusal actions in `Workflow.tla`. The checker also changes the refused snapshot to claim advancement, rebinds its artifact digest, projects that observation for model input, and requires TLC to violate `TraceComplete` at the refusal action. Ordinary projection still rejects that contradictory observation; the Rust test also checks it. [The connection inventory](connections.md) maps every workflow boundary to the six existing detailed protocol models, explicit mapping checks, production coverage owners, and pending evidence. Those older models are passing checks of their own protocols; they are not composed whole-workflow assurance. The foundation proves a narrow real trace and records the rest as obligations for the subsequent Work Items.

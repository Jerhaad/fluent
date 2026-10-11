# Workflow foundation checks

## Integrated candidate check

Run `python3 tests/formal/workflow/check.py --suite=integrated
--jar=target/fluent-tools/tla2tools-1.7.4.jar` from the candidate checkout.
The checker runs the five maintained workflow suites, fresh public
queue/cancellation and no-change Writer sequences, and the scheduler recovery,
lifecycle, launch, and landing suites. The queue/cancellation trace records two
Work identities, the retained queue, the canceled foreground Attempt, read-only
status, a refused Task run, and a later independent Git commit.
The queued dispatch connection also runs two public queue admissions through
`scheduler run` at project capacity one. While the first Writer is held open,
it records both dispatches, both Work Items, the elected coordinator lease,
and the first dispatch's whole-Attempt lease. `project_queue_dispatch.py`
checks their digest-bound identities, bound Attempt, and generation increments.
Rehashed changes to the bound Attempt or either live lease must fail. The
`QueueDispatchConnection.tla` model composes the actual `Workflow` and
`QueueHandoff` actions: claim, lease acquisition, and Attempt creation stutter
in the workflow until `MarkRunning` and `Dispatch` occur together. Two Jobs,
one actor, one coordinator, no crashes, and at most four queue generations
bound its complete safety search. The early-dispatch and missing-coordinator
controls must violate their named invariants. Progress across persistent
provider failure or a process crash is outside this connection; the scheduler
recovery suites check those paths separately.
`project_integrated.py` checks the digest-bound raw facts and causal order
independently of the command assertions. Rehashed traces that swap the canceled
Work identity or change retained Work after the Git commit must fail projection.
The checker writes logs under
`target/formal/workflow/integrated/` and a candidate-bound `report.json` with
suite results, log digests, model hashes, and every connection row from
`connections.md`. The scheduler checker also writes candidate-bound results
for its recovery, lifecycle, launch, and landing cases under
`target/formal/scheduler/`. The report checks every named configuration and
retained log digest before crediting a `scheduler-SUITE/CASE` connection reference.
The integrated result fails if any required suite, log,
binary binding, required case, or connection is missing. An inventory row needs
the structured `Verified: model=SUITE/CASE; assumption=SUITE/CASE;
production=SUITE/CASE; control=SUITE/CASE` evidence described in
`connections.md` before the report may credit it. Existing descriptive
evidence is retained as an explicit gap until those checks pass.

The canonical no-change sequence runs the production Attempt, evidence
attachment, and queue dispatch owners after a failing Tester check. It records
the retained Work and Merge Candidate at four ordered boundaries, and
`project_no_change.py` checks the snapshot digests, candidate binding,
original Tester, retained recovery history, and absence of early readiness.
`NoChangeTrace.tla` checks the projected states under weak fairness. A
rehashed missing canonical proof must fail projection, and a wrong final
candidate must violate the model invariant.

The pending Plan sequence starts with a host Plan row awaiting evidence,
attaches a failing result, executes a no-change Writer after a code-change
review, attaches passing Plan evidence, and continues through the public
Attempt owner to a ready candidate. `project_pending_plan.py` checks the
candidate, Writer, Plan row, Tester order, recovery history, and readiness
against digest-bound Work snapshots. `PendingPlanTrace.tla` checks the joint
Writer and host-proof transition under weak fairness. Rehashed missing proof
and a model state with a false final proof are required negative controls.
These sequences cover their observed paths; the other pending connection rows
in `connections.md` remain obligations.

`approved-contracts.json` records the approved parent statements by section,
statement digest, and named test identity. The integrated checker discovers
each exact test and executes all 22 with `test-support`; its candidate-bound
`named-tests-result.json` retains per-test outcome and command-log digests.
`report.py` refuses to credit missing or changed receipts and lists each
approved behavior with its test state. The inventory covers the six changed
statements and A1–A9. Its `source-sha256` identifies the approved parent
Behavior draft used to prepare this Work Item.

The report is produced by `report.py` after the suite commands. Host results
and final lifecycle gates remain separate candidate-bound records. A green
focused Writer test does not complete those external gates.

## Release transition

The `release-transition` suite runs the installed, SHA-pinned v0.5.0 binary in
isolated projects. Its Work tests exercise planned, queued, paused, running
before cancellation, interrupted, and landed records. The strict host service
test registers two projects, holds owned workers while each legacy coordinator
drains, stops the supervised legacy service, then starts the replacement.
The interrupted Work case terminates a running v0.5.0 Attempt and continues it
through the current public cancellation command.

`project_release.py` checks protected-state equality for both predecessor
refusals and binds the service transition to the observed legacy coordinator
identities, replacement service identity, builds, execution receipts, stop
results, and queues. Mutating the protected state or the observed identities
fails its negative controls. The trace retains digest-bound protected-state
snapshots, initial and final execution receipts, and service logs under the
suite output; the projector checks retained bytes against the event facts. The suite
also checks `service-competition.cfg` and `negative-guardversion.cfg` through
TLC. A joint production trace across all workflow boundaries remains for the
later integrated assurance work.

Run the required host check with:

```sh
python3 tests/formal/workflow/check.py --suite=release-transition --jar=target/fluent-tools/tla2tools-1.7.4.jar
```

The suite requires the pinned JAR, Java 21, and permission to bind an isolated
Unix socket. It writes command logs, traces, projections, controls, and
`result.json` under `target/formal/workflow/release-transition/`.

## Recovery observation

The `recovery-observation` suite checks repeated failure and recovery profiles,
the existing service competition profile, cancellation, repeated-effect,
observation and cleanup negative controls, and a joint service/child settlement
boundary. `JointSettlement.tla` maps the service's worker count to its owned
child and keeps the child's ownership receipt across two service crashes.
Its `Crash` action clears the worker count as `ServiceLifecycle.Crash` does,
while its publication action also requires the child to settle. It permits publication of a completed stop only after the
child settles; its negative configuration allows early publication and must
violate the invariant. This is a bounded abstract control: it does not connect
the full `ServiceLifecycle` state and production child records in one trace.
The project decision is to retain that broader live connection for the later
integrated assurance work. The local worker-clear to child-settlement boundary
is checked here.
The Rust cancellation
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
transition and a misbound recovery authorization fail its negative controls.
`RecoveryTrace.tla` checks the projected Work, Task, candidate, authority, and
publication state across the repair and run actions. The trace proves this recovery case; the other public
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

The integrated production suite also runs a review-only Work Item through
`review codebase` and `attempt run` in an isolated project. Its trace records
the planned and completed Work snapshots and Git HEAD before and after the
Attempt. `project_review_only.py` checks that the Tester completed before every
review started, each review depends on the same Tester Task, no Writer Task or
Merge Candidate appeared, and the source commit stayed fixed. A control changes
one review start time and rehashes the completed snapshot; projection must reject
it. This observed route supplements the bounded `review-only.cfg` search.
Restart and a checked detailed-model connection remain open in the connection
inventory.

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

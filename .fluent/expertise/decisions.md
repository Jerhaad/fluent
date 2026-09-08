# Decisions

Architectural and design decisions that are intentional and should not be flagged by reviewers.

---

## capture-brief Phase 3 keeps cognitive science inline

The capture-brief skill includes cognitive science principles (anchoring bias, framing effects, etc.) directly in the skill content rather than referencing an external expertise file. This is intentional: agents are more likely to read and apply material that appears inline within the skill they are following than to follow a reference to a separate file.

---

## Skills are bundled in the binary and materialized on demand

Skills live under `skills/`; a `<name>.full/` directory overrides a same-named shim when the binary is bundled. Review-skill references are symlinks to authoritative expertise where practical, while the full interactive Fluent bundle carries its stage procedures and selected references directly. At build time, `build.rs` walks the selected trees, dereferences symlinks, and generates `BUNDLED_SKILL_FILES`. At runtime, `materialize_skill()` writes review bundles under `.fluent/work/skills/`, and `fluent skills add` installs provenance-bound agent-facing bundles at supported Claude or Codex roots. Skill instructions reference `references/X.md`, never source-side `expertise/X.md` paths directly.

---

## Releases are ad-hoc signed only — no Developer ID signing or notarization

Release binaries carry only an ad-hoc signature (`codesign --sign -`), applied in `scripts/release.sh` before the checksum. This is deliberate, not an oversight: fluent ships over curl and npx, which do not set the macOS quarantine attribute, so Gatekeeper never runs on the installed binary and Developer ID signing plus notarization would be enforced by nothing. This matches how community CLIs distributed via curl/Homebrew ship (rustup, bun, deno, ripgrep). Ad-hoc signing is the actual macOS requirement — Apple Silicon refuses to execute an unsigned binary. Download safety comes from HTTPS plus the published SHA-256 checksum that `fluent update` verifies. Avoiding Developer ID signing also avoids managing signing secrets in CI. Revisit only if a browser-downloadable artifact (a quarantined `.pkg`/`.dmg`/`.zip`) is ever offered; until then, do not flag the absence of Developer ID signing or notarization.

---

## Learner run evidence is a non-writable sibling of coder staging, not a denied subpath

Host-owned Learner run evidence (transcript, submitted-draft snapshot, error, normalizations) lives under `.fluent/work/artifacts/<work>/<attempt>/learner/runs/run-<N>/`, while the coder writes only to a separate `staging` sibling inside that same run directory. It is deliberately *not* implemented by granting the whole `learner/` directory writable and then denying `learner/runs`. Seatbelt SBPL is last-match-wins, and the rendered profile places `(deny …)` rules ahead of the per-root `(allow file-write* (subpath …))` rules, so an allow on an ancestor subtree overrides a deny on a descendant. Host evidence must therefore live *outside* every granted writable subtree — a sibling of `staging`, not a denied child of a granted root. The run index is allocated from on-disk state (scan existing `run-<N>`, exclusive-create the next), never from the in-memory Learning record, so a lost or omitted record cannot reuse a run identity. Do not "simplify" this into a granted-parent-with-denied-child layout; it would silently let the sandboxed coder write its own run evidence. Related: [[sandbox-denials-track-template-grants]].

---

## The transcript pump's console sink is synchronous and terminal-only; config is per-capture and status has one coordinator

The `transcript_pump` module renders console previews through a single
process-wide sink (`console_preview_sink`, a reference to a plain zero-sized
`static ConsoleSink`). Its operator thresholds
are **not** process-global: they are resolved once per launch (`resolve_config`)
into an immutable `coder::TranscriptCapture` value that is threaded through
`Coder::run_captured` and retained across a launch's auth/rate-limit retry phases.
This replaced the earlier process-wide installed config (a `Mutex` of
`install_config` / `active_config`), under which a concurrent launch could
overwrite another capture's thresholds between resolution and pump spawn. The
public `TranscriptCapture::new(transcript_path, project_root)` constructor resolves
the config internally, so an external `Coder` never names the private config type.

Every persisted `transcript-pump.json` write for one capture is owned by a single
`StatusCoordinator` over an injectable `StatusStore`. It coalesces best-effort
periodic snapshots through a latest-only slot behind a capacity-one wake, processes
required Running and terminal statuses FIFO with acknowledgement (so a terminal
acknowledgement can never be followed by a persisted Running state), balances
every submission across written/coalesced/dropped/disconnected/write-failed
categories, and falls back from an unpersistable Complete to a Failed status. This
replaced the earlier split of a background `StatusWriter` plus a synchronous
`persist_status_sync`; do not reintroduce a second writer or a synchronous
side-channel write. The capture path and the status worker publish the immutable
first fault to a per-pump latch before terminal settlement, so a blocked or slow
status store can never hide a fault from coder supervision.

A coder launch's per-launch supervision diagnostic — most importantly a reaped
leader whose process group could not be *verifiably* swept (`killpg` returned
`EPERM`, or another non-`ESRCH` error) — is surfaced out of the supervisor through
the additive `Coder::run_captured_reported -> CoderRunCompletion`, which pairs the
terminal `Result<i32>` with a serializable `CoderSupervisionReport`. Built-in
coders override it; external coders and mocks use the default that wraps the legacy
`run_captured` with an empty report. Each role artifact boundary
(Writer/Reviewer/Learner/rebase) calls `finish_supervised_coder_run`, which
atomically persists a non-clean report as `coder-supervision.json` beside the
transcript and composes a sidecar-write obstruction as a typed, non-retryable
`SupervisionSidecarError` secondary without relaunching the coder. This is the
durable, non-blocking supervision channel: a group-sweep diagnostic is never
written to a possibly-saturated stderr and never dropped with the `ManagedChild`,
and `Drop` still never publishes a supervision outcome.

Preview delivery is **synchronous and best-effort**, deliberately not a
background renderer over a bounded queue. `PreviewSink::deliver` decides the fate
of the preview on the pump's own thread and returns whether it was delivered, so
drop accounting is exact at every status write (there is no in-flight queue to
settle before `Complete`).

For this landing the production sink **declines every preview** and counts it as
dropped (`dropped_console == records`). Live previews are deferred, not merely
disabled for redirected output:

- Mirroring previews into a redirected (non-terminal) stderr is the flood that
  first stalled Fluent, so a pipe or file sink is never written to.
- Writing to the terminal is no safer here. Even a nonblocking write to an
  independent `/dev/tty` consumes the terminal's remaining queue capacity, so the
  very next *blocking* control-plane write to fd 2 could stall on the space the
  preview just took; an independent file description does not reserve capacity for
  fd 2. Until every Fluent-owned stderr write moves behind one independently
  nonblocking console bus, declining is the safe contract.
- Never `dup(2)` and write blocking: the duplicate shares the same kernel pipe,
  so a later ordinary `eprintln!` would still block in the kernel even with no
  Rust stderr mutex held. Never set `O_NONBLOCK` on a dup of fd 2 either —
  file-status flags are shared.

Declining touches no descriptor and no Rust process-global stderr lock, so
capture is never backpressured and control-plane output never stalls behind the
console. The canonical transcript already holds every byte.

The pump may derive bounded provider-wait diagnostics from complete streamed
records, but those diagnostics never gate capture or execution. Claude Code's
`system/api_retry` record opens a wait interval capped by its `retry_delay_ms`;
the next record or terminal pump settlement closes it. Persist the retry count,
elapsed wait, and active expected resume time in the existing pump status so a
quiet provider is visible without scanning the transcript. Fluent-owned outer
rate-limit sleeps use the sibling role-owned `provider-timing.json` file. Work
metrics index both bounded documents at Task settlement. Before a relaunch
replaces pump status, Fluent adds that phase's completed pump timing to the
role-owned sidecar and records the absorbed phase number to prevent double
counting after resume. Read-only status never walks transcript trees.

Do not "fix" the declining sink by mirroring previews to any stderr or by
reintroducing a background renderer thread. Re-enabling live previews is a separate
change that must first move all Fluent-owned stderr writes behind one independently
nonblocking console bus. (Per-launch config now travels with each capture through
`run_captured`; that is the shipped design, not a thing to undo.)

---

## Transcript age and pump-status timestamps are diagnostics, never authority

`transcript-pump.json` records state, timestamps, and byte/record/drop counters
next to each transcript so an operator can tell a quiet coder from a blocked
console, a failed pump, or completed capture. It is explicitly not a liveness
lease or heartbeat. Executing-Task recovery decides liveness solely from the
process-held flock lease (`executing_task_is_live`), never from how old a
transcript or its status file is. Do not add a transcript-age watchdog or use
pump-status timestamps to reclaim or signal a Task; durable Task ownership is a
separate, dependent Work Item that consumes the pump's terminal signal.

---

## The Learner schema-repair prompt is built inline, not bundled

The bounded schema-repair prompt (`schema_repair_prompt` in `work_task_executor`) is constructed inline rather than added as a file under `prompts/`. It is a short, host-authored instruction that embeds the rejected draft and exact validation error, and it is never resolved through the project→user→bundled content layers the way `learner-user.md` is. Keeping it inline avoids expanding the `prompts/` bundling surface and its naming-guardrail allowlist for a prompt that has no per-project override story. Do not flag the absence of a `prompts/learner-schema-repair.md`.

---

## Advancement requires a succeeded Learner; the post-land handoff-only retry is recovery-only

One shared predicate — `Attempt::learning_advancement_readiness()`, surfaced through `WorkItem::attempt_learning_advancement()` and `MergeCandidate::validate_advancement()` — gates every advancement boundary: Attempt readiness (`MergeCandidateReady`), the scheduler's `candidate-ready` projection, Merge Candidate validation, and the land route. A candidate may advance only once its Attempt's Learner has SUCCEEDED; any other state (absent, `InProgress`, `HandoffPending`, or failed whether relaunchable or not) blocks with one durable reason, `WorkModelError::AttemptLearningNotSucceeded`. This deliberately replaces the earlier "land, then retry the Learner post-land" behavior, which let failed or prepared learning reach `MergeCandidateReady` and land. Do not re-add a boundary that advances over a non-succeeded Learner, and do not fold the readiness check into the structural `MergeCandidate::validate` — it is kept separate so a candidate persisted *before* its Learner runs stays valid (the candidate is created before `run_learner_step`).

Because landing now requires a succeeded Learner, the post-land handoff-only Learner *retry* path (`work_attempt_loop`, guarded by `learner_is_handoff_only` on a Merged candidate) is a recovery/legacy path, not the normal flow: a candidate reaches Merged only with a succeeded Learner, so a Merged candidate with a pending Learner comes from a legacy landing or an interrupted post-land handoff, not fresh advancement. `validate_advancement` therefore exempts Merged/Failed candidates so idempotent post-land follow-up processing still resumes. A pre-land Learner block is surfaced as the dedicated `WorkAttemptRunOutcome::LearnerNotReady`, not `FollowUpRecoveryPending` (whose CLI text hard-codes "is merged" and only fits a genuinely landed candidate). Do not flag the two outcomes as redundant.

### Post-land handoff-only recovery is distinct from selectable pre-land no-expertise

There are two confinement modes that both deny expertise and candidate Git writes, and they must not be conflated. Post-land **handoff-only** recovery is a fixed property of a *merged* candidate: it runs against the persisted merged commit and exists only to resume a landed Attempt's follow-up. Pre-land **no-expertise** is a *durable Work Item policy* (`WorkItem::learner_mode`, default `capture`) selected before execution — for release gates that require the latest reviewed Writer SHA to survive the final Tester, every review executed for that candidate, and Learner unchanged. A passing reviewer from an earlier candidate may remain effective only through Fluent's host-owned changed-domain invalidation proof; it is carried-forward review authority, not a false claim that its Task context names the later SHA. The no-expertise identity gate checks only the final passing round's newly executed Tester and reviewer contexts, while review admission separately proves that every required role is either current or validly carried forward. Both modes reuse the same isolated-snapshot and Seatbelt confinement primitives (`HandoffOnlyWorkspace`, forced sandbox, denied live roots), but `work_task_executor::LearnerExecutionMode` keeps them separate: `PostLandHandoffOnly` baselines on the merged commit, while `PreLandNoExpertise` baselines on the reviewed Writer SHA in the candidate worktree and rejects — never discards-and-records — any candidate Git mutation. The crate-private enum is resolved from `(learner_mode, candidate_merged)`; a merged candidate always runs post-land regardless of the stored policy. The public `LearnerRunInputs { handoff_only: bool }` stays source-compatible (`false` = capture, `true` = post-land) and never expresses no-expertise, which is reachable only through the production adapter. A no-expertise Fargate launch is refused on the host before any side effect, since Linux has no trusted Fluent write boundary and a stale image could ignore the stored field. Prompt text alone is not authority: the host enforces the boundary. Do not "unify" the two modes into one Boolean or run pre-land no-expertise in the live candidate workspace.

---

## The no-expertise pointer-identity postflight settles atomically and per-field, and its concurrency test contradicts the reviewed-SHA pair together

The pre-land no-expertise Learner settles every terminal transition through a fresh, lock-held `mutate_work_item` (`settle_no_expertise_learner` and its helpers `prepare_no_expertise_handoff`, `publish_no_expertise_handoff`, `settle_no_expertise_failure`), not through capture's whole-aggregate `finalize_learning`. Each mutation re-reads the current aggregate, accepts only this runner's exact Learning frontier (`learning_frontier_is`), evaluates the postflight against that `fresh` state, and changes only the Learning record. The Attempt call sites (`interpret_reviews`, the resume path) deliberately skip their post-Learner whole-aggregate `write_work_item` for this mode; re-writing a refreshed snapshot would reopen the stale-write window. This is intentional and must not be "simplified" back into a single whole-aggregate finalizer for no-expertise. See [[fresh-field-level-finalizer-preserves-concurrent-state]].

The aggregate invariant `MergeCandidate.candidate_commit == latest completed Write output.commit` (`MergeCandidate::validate`) couples those two identity pointers, so no valid persisted aggregate can hold them apart. The concurrency test (`pre_land_no_expertise_concurrent_pointer_contradiction_fails_closed`) therefore contradicts the reviewed SHA by moving the Write output and Merge Candidate together off the live candidate HEAD, and contradicts a final review context independently. Do not flag the test for "not contradicting the Merge Candidate on its own" — an independent Merge Candidate contradiction is unreachable through a valid concurrent mutation, and the gate's Merge Candidate check is defended by that validation invariant.

---

## Keep the host-sandbox-only candidate exception during cancellation

`MergeCandidate::validate` requires a candidate's Attempt to be `Complete` with
reviews `Passed`; a `Failed` merge remains separately exempt. Its only recovery
exception permits `NeedsUser` / `HostSandbox`, because the Learner's sandbox
preflight runs after Fluent persists the reviewed candidate. Do not broaden
this structural exception to `Interrupted` or other pauses.

When canceling an Attempt with a persisted pending Merge Candidate, retain
`Complete`, Passed reviews, the candidate, and its original readiness timestamp.
Write an active typed `AttemptCancellation` (`resumed_at` is absent) before
signaling the registered Learner execution. That record is the scheduling pause;
do not attach an `Interrupted` pause kind to this structurally complete Attempt.
Other local Attempts continue to use `NeedsUser` / `Interrupted` with their stop.

Structural validity remains separate from advancement. Both readiness stamping
and `WorkItem::attempt_learning_advancement` reject active cancellation even if
Learning previously succeeded. The storage guard permits the Complete-state stop
only when the existing pending candidate remains unchanged, while rejecting
new Task launches, Learning advancement, and stale candidate-readiness writes.

Explicit resume must match the current cancellation authority and wait for
settlement. It consumes the stop without reopening completed review or Writer
work or changing readiness history, then may relaunch unfinished Learning.
After resume the shared Learner gate still requires succeeded Learning. Keep
`Attempt::learning_advancement_readiness` as the proof of prior Learning when
validating an existing readiness timestamp; a stop must not make that history
unreadable. See [[advancement-requires-a-succeeded-learner]].

---

## Recovery contracts form a neutral dependency boundary

Candidate-bound recovery admission and Writer completion checks live in the
neutral `recovery_contracts` module. Host-evidence classification, queue
admission, and Writer reservation may depend on that boundary, while the
boundary does not depend on any of those callers. This keeps the immutable
candidate, workspace, review, and host-evidence binding contract shared without
introducing a reciprocal executor-to-host-evidence dependency.

Lifecycle recognition feeds candidate and evidence facts into this boundary.
The neutral module must not invoke `lifecycle_recovery`, which can itself depend
on executor services. Authoritative completion selection and ownership-proof
validation are shared here so host attachment and later Writer materialization
cannot choose different source matrices.

## Saved ownership approvals preserve exact obligations and identities

An ownership proposal partitions each original required Plan row into ordered
parts whose text joins back to the original requirement byte for byte. Child
identities append `:part-N` to the original row ID, and every part retains the
source-row provenance and verification references. This intentionally requires
explicit text boundaries: adopting responsibility cannot silently summarize,
drop, optionalize, or reinterpret approved work. Changing the partition or its
identities changes durable approval and completion identity and needs a fresh
approval; it is not a serialization cleanup.

`completion_from` may reuse concrete passing completion only from an identical
Writer obligation. Host, review, Tester, and Learner parts always remain pending
in Writer matrices and use their separate evidence gates. Matching text alone
does not grant completion without the saved approved source and verification.
Future Writer admission consumes the approved mapping while preserving original
matrices and completed Task history.

Work record schema 2 is the mutation guard for readers predating responsibility
assignments. A package version still labeled 0.3.0 cannot provide that guard:
an older reader could otherwise ignore the new fields and overwrite approved
meaning. Retain actual pinned-older-executable refusal coverage when changing
this schema. Reading legacy records or upgrading their syntax does not infer or
approve ownership; legacy rows retain Writer responsibility until an explicit
saved-ownership approval adopts a split.

## Source-correction recovery fails closed on Writer lifecycle

Source-correction classification requires an immutable candidate-bound evidence
snapshot whose `result` is `fail`. Passing evidence follows the ordinary
host-only attachment path and cannot authorize a source correction. Exact
classification replay is idempotent only while the saved classified Writer
remains resumable. The supported resumable states are `Planned`, `Executing`,
or `NeedsUser` with no output, and `Complete` with output. A `Failed` or
`Superseded` classified Writer is not replayable. In either state, repeating
`classify` is rejected without mutation. For a `Complete` Writer with output,
replay retains the original source, review, and evidence bindings but validates
that saved output's workspace as the current clean candidate before returning
the stored result. For a `Failed` Writer, no public resume transition retries
it: `fluent queue resume`, `fluent attempt run`, and repeated `classify` are
rejected, so the operator must inspect the failed Task and escalate for a
separately validated recovery. For a `Superseded` Writer, continue with its
replacement and do not repeat the classification.

This fail-closed policy keeps replay and queue admission aligned: a hard-failed
Writer has no implicit retry and cannot create a second corrective Writer.
No ordinary resume command makes that recovery eligible again. At classified
recovery terminal completion, the host may supersede exactly one historical
failed output-free Writer that the classification replaced; it preserves that
Writer as immutable history and never grants a second Writer through replay.

---

## Dashboard observation owns no durable transition cursor

Status and dashboard must leave the Work tree unchanged. Dashboard refresh
therefore derives its latest-transition view in memory from the same verified
published Work snapshot as its rows. The Work transition ledger owns the durable
history; dashboard observation owns no cursor, acknowledgement, or persisted
projection. Restarting the dashboard reconstructs this view from that ledger.
Do not restore writes to `transition-projection.json` during observation. Other
consumers that deliver durable effects, such as Slack, keep their own cursor
ownership independently of the dashboard.

---

## Verify pending Work snapshots before observation

New Work transaction journals persist `base_children_digest` to prove that stored
Attempts, Tasks, and Merge Candidates still belong to the published base root.
Read-only observation checks stable root and journal values around child assembly.
While a journal remains, the stored children must match that base digest or, once
the target root publishes, the target children. An unreadable journal or child
collection cannot establish absence or consistency.

Keep the digest optional when decoding legacy journals so lifecycle mutations
can still recover them under the model lock. Observation withholds a legacy
pending aggregate when it lacks the required proof, reports a bounded diagnostic,
and leaves recovery to its owner. It must neither recover the journal nor use its
contents as a replacement snapshot. Preserve this compatibility distinction when
changing journal writers or readers; see
[Serialize aggregate Work writes](work-model/patterns/serialize-aggregate-work-writes.md).

---

## Share verification policy below admission and execution

`verification_policy` owns checker digests, sandbox-content identity, and the
bounded-log limit shared by `host_evidence` admission and `verification` workers.
It depends only on content resolution and hashing. Neither admission nor worker
execution belongs in this neutral contract. `verification` may reuse confined
execution and attachment primitives in `host_evidence`; `host_evidence` must not
depend back on `verification`. Both continue to use the shared `execution`
supervisor for process ownership and cancellation. This keeps policy consistent
without making admission and worker orchestration depend on each other.

---

## Keep nested commands under their Task's execution owner

`work_task_executor::run_task` installs the scoped `execution::TaskOwner` guard
for the Task's managed transcript. The guard stores that path in thread-local
state, restores the previous scope on drop, and makes nested synchronous
commands use the Task's execution record even when they supply another
transcript path. Baseline checks, Tester commands, and extractors therefore
retain the reserved Attempt owner and cannot escape its durable stop between
children. Heartbeats and command transitions preserve that owner binding.

Synchronous execution on the owning thread is part of this contract. A newly
spawned thread does **not** inherit `TaskOwner`. Any future move of nested command
execution to another thread must explicitly propagate the owning transcript and
install its scope there before launch. Keep each guard on the thread where it
was entered; retain the single-record launch and settlement discipline rather
than assigning concurrent children to the same record without an ownership
protocol. Passing a different transcript without that scope loses attribution.

Acquire the Work model lock **before** the execution lock. Check the shared
Attempt cancellation while holding the model lock, then retain both locks
through child creation and identity publication. Release both before waiting
for the child or cleaning up a failed launch. This order serializes reservation,
launch, and cancellation without making cancellation wait for worker completion;
a reserved peer observes the shared stop even before cancellation visits its
own execution record. Preserve it when adding nested or parallel work.

---

## Resolve Python source selectors without execution

The candidate gate intentionally uses a limited Rust text scanner to resolve
class-qualified Python selectors from committed declarations. It does not import
candidate Python or resolve inherited or dynamically assigned methods. Keep
this cheap source check separate from the final Tester's required passing
evidence. See [[python-test-selectors-use-static-class-ownership]] for the
supported declaration boundary and
[the architecture documentation](../../documentation/architecture.md)
for source and evidence matching.

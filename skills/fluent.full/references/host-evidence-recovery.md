# Recover a host-evidence finding

Use exactly one recovery path. Attach host-only evidence when the candidate is
correct and a concrete external condition needs another host run. Classify only
a paused candidate-bound recovery when the operator authorizes a source
correction: a new recovery must have an actionable `evidence-needed` review,
while a legacy paused recovery may contain an unqualified marker. For a new
review, a single malformed `Disposition: evidence-needed` marker follows the
ordinary `code-change` Writer path; missing, duplicate, or unknown disposition
markers remain an uncertain pause. The legacy paused-recovery exception keeps
an unqualified marker eligible for its saved recovery. An older Plan-row
recovery may also retain `review_state: not-reviewed` after its failed targeted
Tests Review; Fluent admits that shape only after rechecking its immutable
Plan-row binding and records the failed review before it schedules the
corrective Writer. These paths are not
interchangeable: attachment keeps the candidate waiting, while classification
schedules a Writer on the existing Attempt.

## Contents

- [Attach host-only evidence](#attach-host-only-evidence)
- [Run one focused Tests check](#run-one-focused-tests-check)
- [Classify a candidate-bound source correction](#classify-a-candidate-bound-source-correction)
- [Approve responsibility for a saved mixed Plan row](#approve-responsibility-for-a-saved-mixed-plan-row)
- [Recover an interrupted Writer awaiting a host check](#recover-an-interrupted-writer-awaiting-a-host-check)

## Attach host-only evidence

For an exact failed canonical Tester command, use the required-check selector:

```sh
fluent attempt evidence attach <work-item> <attempt> \
  --candidate <exact-commit> --required-check <check-id> \
  --evidence-file <host-result.json>
```

The check ID is `tester:<tester.yaml-sha256>:<zero-based-command-index>`, using
the digest recorded in that Tester's `canonical-run.json` without its `sha256:`
prefix. The evidence must name the exact command and candidate-relative working
directory (`.` for canonical commands). This attachment requires no failed
ordinary Review, failed-command recheck, or new Writer. Resume the Attempt to
run its targeted Tests Review. An attached result grants no pass until review
accepts it; accepted proof credits that occurrence only. Fluent retains the
failed result and reuses other valid results from the complete canonical run.
A diagnostic command cannot replace the required command. Creating this result
still requires the existing authorization to run on its host.

Use this path when the candidate does not need a source change and either a
failed review asks for proof that a trusted host can run or the final candidate
gate asks for one command-shaped `Test:` reference that canonical Tester did not
run. Create one JSON file on that host, for example:

```json
{"schema_version":1,"producer":"release-host","check":"fluent tester check","working_directory":"/repo","result":"pass","run_at":"2026-08-03T17:59:47Z","output":"...captured output..."}
```

This passing snapshot belongs to host-only attachment. Passing evidence
follows the ordinary host-only attachment path and cannot authorize a source
correction.

Inspect `fluent work-item show <work-item-id>` for the current Attempt,
candidate SHA, and failed `review.md` artifact paths. Then attach the exact
proof and each finding it addresses:

```sh
fluent attempt evidence attach <work-item-id> <attempt-id> \
  --candidate <candidate-sha> --evidence-file host-evidence.json \
  --review-artifact .fluent/work/artifacts/.../review.md
```

For a candidate-gate pause, run the exact command in `needs-user.md` against
the unchanged candidate and pass the rejected `candidate-gate.json` path as the
single `--review-artifact`. Set the evidence document's `check` to that exact
command and `working_directory` to `.`. Fluent accepts this only when that gate
has one missing complete command and no unrelated failure.

If Writer completion pauses because a required Plan row is an authorized-host
check, use the `--plan-row plan:step-N` attachment route named in the handoff:

```sh
fluent attempt evidence attach <work-item-id> <attempt-id> \
  --candidate <candidate-sha> --evidence-file host-evidence.json \
  --plan-row plan:step-N
```

The evidence `check` must exactly equal the one command Fluent resolves from the
Plan row. Its `working_directory` must exactly match the stored value shown in
the handoff. For example, for a Plan command `bash tests/host.sh` with
`working_directory` set to `.`, run the check from the candidate checkout root
and record that relative value, not an absolute host checkout path:

```json
{"schema_version":1,"producer":"release-host","check":"bash tests/host.sh","working_directory":".","result":"pass","run_at":"2026-08-03T17:59:47Z","output":"...captured output..."}
```

The row may contain a direct command reference, an inline code command, or one
command embedded in its Deliverable or Verification prose.
Fluent keeps the approved planning text unchanged; if the row contains no
command or more than one command-shaped reference, it remains a human-recovery
pause instead of guessing. Fluent binds the result to the unchanged Writer
candidate and source completion matrix, records an audited binding, and
schedules one targeted Tests Review. Passing evidence can complete that
host-owned row after the targeted Review and continue to the canonical gates;
failing evidence remains a reviewed correction and does not make the row
complete. Retrying the same command is idempotent. This route does not ask the
Writer to create a source commit for host-only work.

Fluent snapshots the exact document, binds it to that candidate, and reruns only
the named reviewers. Do not edit the candidate or invent a commit. Inspect the
Work Item again for the next action.

## Run one focused Tests check

Use this public path when a failed Tests Review asks for one exact
`Evidence-Command` and no host evidence document exists yet:

```sh
fluent attempt evidence run <work-item-id> <attempt-id> \
  --review-artifact .fluent/work/artifacts/.../review.md
```

This invocation authorizes the exact command and persists one candidate-bound
verification Task. It returns before execution. Follow its printed commands:

```sh
fluent task run <work-item-id> <attempt-id> <verification-task-id>
fluent attempt run <work-item-id> <attempt-id>
```

The worker reads the candidate, writes only private scratch, and keeps bounded
`stdout.log` and `stderr.log` in its Task artifact directory. It exclusively
publishes `receipt.json`, binding the candidate, command, working directory,
checker digest, result, timestamps, exit status, and log digests. The Attempt
loop validates the receipt and uses the ordinary evidence attachment path to
start only the targeted Tests Reviewer. It does not hold a project-wide lock
while the check runs.

If validation fails, inspect the named fixture, receipt, or digest mismatch and
restore the exact authorized inputs or worker-produced bytes before repeating
`fluent attempt run`. A changed candidate or checker requires a fresh
authorization against the current failed review. Do not edit a result to claim
a pass. A canceled verification requires explicit Attempt resume and writes its
retry artifacts separately. Automatic receipt ingestion cannot clear a durable
stop. Validated manual `attach` remains an explicit recovery action for a
settled interrupted Attempt; an invalid attachment leaves it stopped.

Existing schema-version-1 manual evidence documents remain accepted. Historical
Tasks omit the optional verification context; historical Attempts omit the
optional cancellation record. For a legacy review without structured request
lines, also provide its exact `--command` and `--working-directory`. Use `run`
when Fluent must perform the focused check; use `attach` when an authorized host
has already produced the JSON evidence document. Use `classify` only after a
failed snapshot and targeted review establish that the operator authorizes a
source correction.

## Classify a candidate-bound source correction

For a new evidence-targeted review, `evidence-needed` is valid only when the
review names exactly one non-empty `External-Condition` and exactly one
non-empty `Additional-Proof` that would resolve it. A single
`Disposition: evidence-needed` marker with missing, empty, or otherwise
malformed qualification follows the ordinary `code-change` Writer path. Missing,
duplicate, or unknown `Disposition` markers remain an uncertain pause. A legacy
paused recovery with an unqualified marker preserves the only documented
exception to that new-review qualification rule.
When the saved targeted review names the actionable condition above and the
operator authorizes a source correction, classify that correction on the
existing paused Attempt instead of attaching the same proof:

```sh
fluent attempt evidence classify <work-item-id> <attempt-id> \
  --candidate <candidate-sha> \
  --review-artifact .fluent/work/artifacts/.../review.md \
  --evidence-digest <attached-evidence-sha256> \
  --resolution 'The candidate-bound failure requires a source correction.'
```

`--evidence-digest` remains required for every `classify` invocation, including
legacy recovery. Legacy compatibility may derive only the missing persisted
target review digest from the exact bound Review bytes; it never derives the
supplied evidence digest or permits that argument to be omitted.

Source-correction classification requires an immutable candidate-bound evidence
snapshot whose `result` is `fail`. A passing snapshot belongs to the host-only
attachment path above and cannot authorize code change.

Use the failed targeted Tests Review path and evidence digest shown by
`fluent work-item show`. Fluent accepts this command only for the saved,
complete actionable `evidence-needed` review or a legacy paused recovery with
the unqualified marker; a new unqualified review is already on the ordinary
Writer path. The candidate, clean worktree, and matching Attempt frontier must
remain unchanged. A new classification must target the current paused
recovery; a later `Reviewing` or `NeedsEvidence` recovery rejects an older
classification and its Writer continuation. Exact replay returns the stored
result and safely reconciles its audit. Exact classification replay is
idempotent only while the saved classified Writer remains resumable. The
supported resumable states are `Planned`, `Executing`, or `NeedsUser` with no
output, and `Complete` with output. A `Failed` or `Superseded` classified Writer
is not replayable. In either state, repeating `classify` is rejected without
mutation. For a `Complete` Writer with output, replay retains the original
source, review, and evidence bindings but validates that saved output's
workspace is the current clean candidate before returning the stored result. For
a `Failed` Writer, no public resume transition retries it: `fluent queue resume`,
`fluent attempt run`, and repeated `classify` are rejected, so inspect the
failed Task and escalate for a separately validated recovery. For a `Superseded`
Writer, continue with its replacement and do not repeat the classification.
Fluent records the authorization,
explicit `code-change` disposition, and audit artifact, then schedules one
corrective Writer with the failed review, immutable evidence snapshot, and
audit input; resume it with `fluent queue resume <work-item-id> --attempt
<attempt-id>`. While the classified Writer remains resumable, repeating the
exact command returns the same Writer and audit identifiers without creating
another record; stale or different inputs are rejected without mutation. A
passing targeted review still requires all exact-commit evidence and Learning
before landing.

## Approve responsibility for a saved mixed Plan row

Reading status or running `work-item upgrade` does not adopt ownership. Prepare
an explicit JSON proposal for the original required row. Its parts must join
back to the original State reached text, byte for byte, in order. For an original
row `plan:step-2` saying `Fix source. Check host. Review. Test. Learn.`, use:

```json
{"splits":[{"row_id":"plan:step-2","parts":[
  {"requirement":"Fix source.","responsibility":"writer","completion_from":"plan:step-1"},
  {"requirement":" Check host.","responsibility":"host"},
  {"requirement":" Review.","responsibility":"review"},
  {"requirement":" Test.","responsibility":"tester"},
  {"requirement":" Learn.","responsibility":"learner"}
]}]}
```

`completion_from` is optional. Use it only when that source row already completed
an identical Writer obligation with concrete passing verification. Omit it to
leave the Writer part pending. Host and later-stage parts always start pending.
Every part retains the original source-row link and verification references;
the approval audit preserves the complete original contract and matrix digest.

```sh
fluent attempt ownership preview work-id attempt-1 --proposal-file split.json > preview.json
fluent attempt ownership approve work-id attempt-1 --preview-file preview.json --resolution 'Approve the displayed responsibility split'
```

The first command is read-only. Inspect its exact candidate, source evidence,
and proposed parts before approving. Approval rechecks the current Attempt,
clean candidate, planning inputs, source matrix bytes, and stopped Task owners
under the mutation lock. A stale preview requires a fresh preview and decision.
An identical eligible approval returns its original audit without duplicate
Tasks or evidence. Older readers reject the new record schema before mutation.

Approval assigns responsibility; it does not authorize the host command, retry
a hard-failed Writer, or declare any host check passed. The existing Test-reference
and Approach revision commands retain their separate meanings.

## Recover an interrupted Writer awaiting a host check

After approval, `fluent work-item show work-id` names the exact candidate and
host Plan row. An output-free canceled continuation can yield to that check
only when its Task lease and execution owner have stopped, its input names the
preserved source matrix, all Writer obligations are complete, and the managed
candidate remains clean at the saved commit. Fluent supersedes only that
continuation; earlier completed Tasks and failed evidence remain history.

After the operator authorizes and runs the displayed command, attach its fresh
result and resume the same Attempt:

```sh
fluent attempt evidence attach work-id attempt-1 --candidate <SHA> --plan-row plan:step-2:part-2 --evidence-file host-result.json
fluent queue resume work-id --attempt attempt-1
fluent scheduler run
```

For foreground execution, use `fluent attempt run work-id attempt-1` after
attachment. Resuming before attachment returns the exact host handoff. A failed
check never completes the host obligation. Passing evidence must pass targeted
Tests Review before ordinary review, the canonical final Tester, and Learning
can establish readiness.

An identical attachment reuses its pending targeted review. An identical active
queue resume reuses its dispatch. A foreground retry while its reviewer is live
refuses to advance the same Task. All paths recheck the bound candidate and
evidence; a changed attachment, source matrix, or dirty candidate requires
inspection before recovery can continue. Scheduler claim and Review launch
repeat those checks, including when a queue entry predates the changed evidence.

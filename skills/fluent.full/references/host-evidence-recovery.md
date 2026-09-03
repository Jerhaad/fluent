# Recover a host-evidence finding

Use exactly one recovery path. Attach host-only evidence when the candidate is
correct and a concrete external condition needs another host run. Classify only
a paused candidate-bound recovery when the operator authorizes a source
correction: a new recovery must have an actionable `evidence-needed` review,
while a legacy paused recovery may contain an unqualified marker. A single
malformed `Disposition: evidence-needed` marker follows the ordinary
`code-change` Writer path; missing, duplicate, or unknown disposition markers
remain an uncertain pause. These paths are not interchangeable: attachment
keeps the candidate waiting, while classification schedules a Writer on the
existing Attempt.

## Attach host-only evidence

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
Plan row. The row may contain a direct command reference, an inline code
command, or one command embedded in its Deliverable or Verification prose.
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

## Classify a candidate-bound source correction

For a new evidence-targeted review, `evidence-needed` is valid only when the
review names exactly one non-empty `External-Condition` and exactly one
non-empty `Additional-Proof` that would resolve it. A single
`Disposition: evidence-needed` marker with missing, empty, or otherwise
malformed qualification follows the ordinary `code-change` Writer path. Missing,
duplicate, or unknown `Disposition` markers remain an uncertain pause. A legacy
paused recovery with an unqualified marker is the only documented exception.
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
mutation. For a `Failed` Writer, use an explicit validated retry or replan
transition before queue resume; for a `Superseded` Writer, continue with its
replacement and do not repeat the classification. Fluent records the authorization,
explicit `code-change` disposition, and audit artifact, then schedules one
corrective Writer with the failed review, immutable evidence snapshot, and
audit input; resume it with `fluent queue resume <work-item-id> --attempt
<attempt-id>`. While the classified Writer remains resumable, repeating the
exact command returns the same Writer and audit identifiers without creating
another record; stale or different inputs are rejected without mutation. A
passing targeted review still requires all exact-commit evidence and Learning
before landing.

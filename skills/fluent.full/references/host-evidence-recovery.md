# Recover a host-evidence finding

Use this uncommon host-only recovery path when the candidate does not need a
source change and either a failed review asks for proof that a trusted host can
run or the final candidate gate asks for one command-shaped `Test:` reference
that canonical Tester did not run. Create one JSON file on that host, for
example:

```json
{"schema_version":1,"producer":"release-host","check":"fluent tester check","working_directory":"/repo","result":"pass","run_at":"2026-08-03T17:59:47Z","output":"...captured output..."}
```

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

Fluent snapshots the exact document, binds it to that candidate, and reruns
only the named reviewers. Do not edit the candidate or invent a commit. Inspect
the Work Item again for the next action. `evidence-needed` means attach new host
proof only when the review names exactly one non-empty `External-Condition` and
exactly one non-empty `Additional-Proof` that would resolve it. A single
`Disposition: evidence-needed` marker with missing, empty, or otherwise
malformed qualification follows the ordinary `code-change` Writer path.
Missing, duplicate, or unknown `Disposition` markers remain an uncertain pause;
they do not authorize either recovery route. If the exact command already ran
against the exact candidate and the failed review did not name such a
condition, authorize the source correction on the existing paused Attempt
instead of attaching the same proof:

```sh
fluent attempt evidence classify <work-item-id> <attempt-id> \
  --candidate <candidate-sha> \
  --review-artifact .fluent/work/artifacts/.../review.md \
  --evidence-digest <attached-evidence-sha256> \
  --resolution 'The candidate-bound failure requires a source correction.'
```

Use the failed targeted Tests Review path and evidence digest shown by
`fluent work-item show`. Fluent accepts this command only for the saved,
complete actionable `evidence-needed` review or a legacy paused recovery
containing the unqualified marker; a new unqualified review is already on the
ordinary Writer path. The candidate, clean worktree, and matching Attempt
frontier must remain unchanged. A new classification must target the current
paused recovery; a later `Reviewing` or `NeedsEvidence` recovery rejects an
older classification and its Writer continuation. Exact replay still returns
the stored result and safely reconciles its audit. Fluent records the
authorization, explicit `code-change` disposition, and audit artifact, then
schedules one corrective Writer with the failed review, immutable evidence
snapshot, and audit input;
resume it with `fluent queue resume <work-item-id> --attempt <attempt-id>`.
Repeating the exact command returns the same Writer and audit identifiers
without creating another record, while stale or different inputs are rejected
without mutation. A passing targeted review still requires all exact-commit
evidence and Learning before landing.

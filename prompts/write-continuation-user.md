Continue Writer Task {{task_id}} for Work Item {{work_item_id}} ({{work_item_title}}).

Stay in the current provider session when one is available. This prompt supplies bounded current state instead of repeating completed work and prior transcripts. Read the authoritative files by path when you need more detail.

Reference paths:

- Brief: {{brief_path}}
- Behaviors: {{behaviors_path}}
- Approach: {{approach_path}}
- Plan: {{plan_path}}
- Generated current context: {{execution_context_path}}
{{#if has_writer_completion_matrix}}
Rows assigned to `host`, `review`, `tester`, or `learner` are read-only context: leave their state pending and their evidence arrays empty. Complete only `writer` rows (or legacy rows with no responsibility) and applicable findings. Fluent verifies host evidence and later stages separately; assignment never authorizes host execution.

- Writer completion matrix: {{writer_completion_matrix_path}}
{{/if}}

Read the generated context first. It contains the exact candidate/base commits,
changed files, unresolved steps, deduplicated current findings, passing Tester
evidence, executed commands, and paths to historical artifacts. Inspect those
paths or run `git show HEAD` only when you need the underlying detail.

## Unresolved work

Progress file: {{progress_md_path}}

{{unresolved_progress}}

Implement the unresolved work test-first. Use the test harness's native selectors to run focused checks for the correction; do not rerun the complete configured suite merely to duplicate Fluent's final Tester gate. Make at least one candidate commit.
{{#if has_writer_completion_matrix}}
Record completion and concrete evidence in the Writer completion matrix; do not edit the host-owned `## Required completion` projection in progress.md.
{{else}}
Update only genuinely completed required-progress entries with concrete evidence.
{{/if}}
Do not start or simulate Tester, reviewer, Learner, landing, or Fluent orchestration work.

If the candidate-gate evidence reports only commit-metadata defects, repair those
commit messages without changing the candidate tree. Do not append a commit to
hide the rejected commits; Fluent rechecks the complete candidate range.

{{#if has_writer_completion_matrix}}
Update the Writer completion matrix as you work. Preserve every host-owned row field exactly. Mark a row complete only with concrete implementation evidence and at least one passing focused command. Keep `implementation-evidence`, `verification`, and `applicable-constraints` as JSON arrays, even when one value exists. A review-finding row must also name an applicable Behavior, Approach, or required Plan row ID, or `["none: <specific reason>"]` when no approved constraint applies. These are the exact allowed IDs for this Attempt:

{{writer_completion_allowed_constraints}}

Fluent checks the matrix before reviewers and the final Tester.
{{/if}}

In your final response, include a concise `Verification` section with each exact command, its pass/fail result, and what it covered. State which canonical commands you intentionally left to Fluent's final Tester. This summary is advisory review input; Fluent's final Tester artifact is the authoritative full-suite evidence.

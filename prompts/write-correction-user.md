Correct Writer Task {{task_id}} for Work Item {{work_item_id}} ({{work_item_title}}).

Continue in the prior Writer session when Fluent provides it. The generated current context is
the authoritative index for this correction:

- Current context: {{execution_context_path}}
- Progress: {{progress_md_path}}
{{#if has_writer_completion_matrix}}
- Writer completion matrix: {{writer_completion_matrix_path}}
{{/if}}
- Full command output directory: {{artifact_dir}}/commands
{{#if has_corrective_authority}}
- Immutable corrective authority: {{corrective_authority_path}}
{{/if}}

Current correction artifacts (complete, untruncated paths):
{{correction_artifacts_list}}

Read the current context first. It contains the exact candidate and base commits, changed files,
every unresolved progress entry, every current finding, concise passing evidence, and
managed paths to the full historical artifacts. For review findings, read a specific historical
artifact or a focused diff only when its summary does not provide enough detail. Do not bulk-print planning files,
expertise, source trees, prior transcripts, or complete test output into the model conversation.

{{#if has_failed_tester_inspection}}
Failed final-Tester evidence is attached. Read every artifact below before deciding that no
candidate change is needed:
{{failed_tester_inspection_list}}

Inspect the candidate's `.fluent/tester.yaml` and focused path/configuration variants alongside
the failed command logs. A command, cache path, package path, or harness configuration committed
in `.fluent/tester.yaml` is candidate-owned; do not classify its failure as external merely
because the command ran in a sandbox.
{{/if}}

When you run a command that can produce substantial output, save its complete stdout and stderr
under {{artifact_dir}}/commands and inspect only its concise summary or relevant failure excerpt
in the conversation. Keep the complete files for audit.

Apply every current finding test-first while preserving the approved constraints. The original
planning files remain available for a specific question:

- Brief: {{brief_path}}
- Behaviors: {{behaviors_path}}
- Approach: {{approach_path}}
- Plan: {{plan_path}}

Run focused harness-native checks. Reuse Fluent's private `CARGO_HOME`; do not access the
operator's Cargo home or rerun the complete configured suite. Fluent owns the one final Tester.

Create a candidate commit for each real correction and leave the workspace clean. If focused
verification proves no candidate change is needed, write
{{no_change_declaration_path}} with this exact shape:

```json
{{#if has_failed_tester_inspection}}
{
  "schema_version": 1,
  "reason": "Why no correction was necessary after inspecting the failed Tester evidence",
  "verification": [
    {"command": "the focused command you ran", "result": "pass"}
  ],
  "inspected_artifacts": [
    {
      "path": "one exact project-relative path listed above",
      "digest": "sha256:<digest you computed from that file's bytes>"
    }
  ]
}
{{else}}
{
  "schema_version": 1,
  "reason": "Why no correction was necessary",
  "verification": [
    {"command": "the command you ran", "result": "pass"}
  ]
}
{{/if}}
```

Keep `reason` and every `command` non-empty, and include at least one passing
`verification` entry.
{{#if has_failed_tester_inspection}}
Include exactly one `inspected_artifacts` entry for every failed Tester evidence path listed
above, with no extras or duplicates. Compute each digest from the artifact itself; Fluent rejects
missing, stale, or mismatched evidence before it launches reviewers or reruns the final Tester.
{{/if}}
Do not use that declaration after changing the candidate.
{{#if has_writer_completion_matrix}}
Update mutable progress sections and the completion matrix only with concrete passing evidence.
Preserve every host-owned matrix row. Keep `implementation-evidence`, `verification`, and
`applicable-constraints` as JSON arrays, even when one value exists. Map each completed finding to
its applicable Behavior, Approach, or required Plan constraint, or to `["none: <specific reason>"]`.
These are the exact allowed IDs for this Attempt:

{{writer_completion_allowed_constraints}}

Do not edit the host-owned `## Required completion` projection in progress.md.
{{else}}
Update progress only with concrete passing evidence.
{{/if}}

Finish with a concise verification summary containing each exact focused command, result, and
coverage. Name the complete suite left to Fluent's final Tester.

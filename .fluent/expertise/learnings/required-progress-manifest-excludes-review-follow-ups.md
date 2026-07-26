---
name: required-progress-manifest-excludes-review-follow-ups
description: Keep review follow-ups outside the closed required-progress manifest, and tolerate only checked legacy review rows when reconciling old Attempts
metadata:
  type: architecture
---

For an Attempt with a `ProgressContract`, `progress.md` serves two different
purposes that must stay structurally separate:

- `## Required completion` is a closed, host-materialized manifest. Its stable
  ids, requirement text, and order come from required Plan rows. Writers may
  only check those rows and append concrete `Evidence:`; they must not use this
  section as a general work queue.
- `## Review follow-ups` is the mutable queue for findings from later review
  rounds. Follow-up Writers address its unchecked entries before returning to
  required or ordinary plan work.

When a follow-up Writer encounters an old `progress.md` whose required section
contains `Address review finding:` rows, move each row together with its nested
evidence and commit notes into `## Review follow-ups`. Preserve its checked
state and wording. Do not reconstruct, delete, reorder, or reword any required
manifest row while migrating review history.

The advancement gate keeps backward compatibility deliberately narrow. It may
ignore an already-checked top-level row with the exact legacy
`- [x] Address review finding:` prefix inside `## Required completion`, because
that row is settled historical metadata. An unchecked legacy review row must
still block advancement, and every other extra or malformed top-level row must
fail closed through normal manifest reconciliation. Broadly ignoring unknown
rows would let arbitrary content bypass the manifest's exact-set guarantee.

Prompt changes around review rounds must distinguish Attempts that carry a
required-progress contract from legacy Attempts that do not. Required-manifest
terminology and selection rules belong only to the contract-aware branch; both
branches may maintain a separate review-follow-up queue.

Related: [[backward-compatible-serde-fields]],
[[mode-specific-prompts-replace-conflicting-base-instructions]].

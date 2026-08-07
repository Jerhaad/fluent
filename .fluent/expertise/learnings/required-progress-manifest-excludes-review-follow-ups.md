---
name: required-progress-manifest-excludes-review-follow-ups
description: Keep review follow-ups outside the closed required-progress manifest, and tolerate only checked legacy review rows when reconciling old Attempts
metadata:
  type: architecture
---

For an Attempt with a `ProgressContract`, `progress.md` serves two different
purposes that must stay structurally separate:

- `## Required completion` is a closed, host-owned projection. Its stable ids,
  requirement text, and order come from required Plan rows. For a matrix-backed
  Attempt, Writers do not edit this section. They report completion in the
  task-owned matrix, and the host replaces the section with exact Plan text and
  links to validated matrix rows before review.
- `## Review follow-ups` is the mutable queue for findings from later review
  rounds. Follow-up Writers address its unchecked entries before returning to
  required or ordinary plan work.

The host replaces only the required section and preserves every separate
level-two section. Current open findings also appear as matrix finding rows, so
the Writer does not need to move review records through the host-owned section.
Checked legacy rows and nested notes inside that section may disappear when the
host projects current matrix evidence; they are historical round notes, not
advancement authority.

The advancement gate keeps backward compatibility deliberately narrow. An
Attempt without a Writer completion contract still uses its Writer-owned
required-progress section. That legacy gate may ignore an already-checked
top-level row with the exact `- [x] Address review finding:` prefix because the
row is settled historical metadata. An unchecked legacy review row still
blocks advancement, and every other extra or malformed top-level row fails
closed through normal manifest reconciliation.

Prompt changes around review rounds must distinguish matrix-backed Attempts
from legacy Attempts that carry only a required-progress contract. The former
must not tell Writers to toggle, annotate, migrate, or repair the host-owned
section. Both branches may maintain a separate review-follow-up queue.

Related: [[backward-compatible-serde-fields]],
[[mode-specific-prompts-replace-conflicting-base-instructions]].

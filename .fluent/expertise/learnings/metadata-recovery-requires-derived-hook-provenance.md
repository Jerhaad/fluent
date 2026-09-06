---
name: metadata-recovery-requires-derived-hook-provenance
description: Admit metadata recovery with a project-check only from canonical, complete hook evidence bound to the rejected candidate gate
metadata:
  type: architecture
---

The active candidate gate accepts literal `\n` text. Factory's project hook
may report it as presentation advice, but that advice cannot authorize a
rewrite, corrective Writer, or pause. The literal-line-breaks classifier below
exists only to recover failed evidence persisted by older Fluent versions;
keep its exact grammar and receipts separate from current admission.

Metadata-only candidate recovery is an audited exception to the normal
append-only correction path. Admit it only from the exact rejected candidate
gate whose failed checks are either `commit-metadata` alone or
`commit-metadata` plus a derived `project-check`.

The derived form is a closed evidence contract. The gate's project-check
diagnostics must contain exactly one nonzero `check-pre-merge exited N` entry
and no other structured diagnostic. Its `log_path` must be the canonical,
managed `hooks/check-pre-merge.log` sibling of that gate. The log must be a
bounded, valid-UTF-8 regular file, not a symlink, must remain inside the
project, and must contain at least one nonblank line. Every nonblank line must
normalize to a complete commit-metadata finding from that same gate, with
commit IDs resolved uniquely. A generic body-line length diagnostic is
admissible only after the exact Git message line proves the historical literal
`\n` finding; ambiguous, uncovered, unrelated, unparseable, alternate-path, or
changed evidence remains rejected.

Persist the exact hook artifact path and digest in the recovery authority and
as a corrective Writer input, then revalidate both before resuming. Keep the
path and diagnostic classifier in the low-level candidate-gate layer; the
recovery module records the decision and schedules work from its result.
Prompt text may make the artifact readable to the Writer, but it cannot serve
as the authority for recovery eligibility.

Related: [[corrective-writers-preserve-candidate-lineage]],
[[canonicalize-confinement-paths]], [[negative-test-cross-product-coverage]],
[[approval-records-bind-current-authority]].

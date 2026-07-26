---
name: next-action-guidance-is-an-executable-interface
description: A next-action line must match the real CLI signature with authoritative identifiers, and tests must assert and execute the emitted command shape
metadata:
  type: testing
---

Fluent's `→ Next:` output is an operator interface, not illustrative prose. When
the current state already provides authoritative identifiers, render the exact
CLI signature instead of placeholders or a partial prefix. Merge-ready guidance
needs both the Work Item ID and Merge Candidate ID for `merge-candidate show`
and `merge-candidate land`; post-land cleanup is the argument-free global
`fluent cleanup` command and keeps its default dry run.

A prefix assertion is insufficient. Output such as a valid command followed by
an extra positional argument still contains the expected prefix but is not
executable. Reconstructing a separate hard-coded command in the test also leaves
the emitted line unverified.

At the public binary boundary, assert the complete next-action line, including
identifiers, delimiters, and the absence of placeholders or forbidden
arguments. Then invoke the same emitted argument shape through the real CLI.
Source identifiers from the state object or outcome that owns them rather than
re-deriving them from display text. Related:
[[route-tests-drive-real-launch-wiring]], [[test-names-match-assertions]].

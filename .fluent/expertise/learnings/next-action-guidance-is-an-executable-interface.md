---
name: next-action-guidance-is-an-executable-interface
description: Render exact eligible CLI commands and preserve recovery blockers through status and show without generic resume fallbacks
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

Preserve a recovery validator's rejection alongside the absence of an eligible
action. An absent command alone lets outer guidance substitute a generic resume
hint for a blocked recovery. Derive both the blocker and action from the same
shared contract, and carry them through status, JSON show, and stderr hints.
Public-command regressions must check all rendered surfaces after cancellation
and candidate drift, and verify that observation leaves saved Work unchanged.

At the public binary boundary, assert the complete next-action line, including
identifiers, delimiters, and the absence of placeholders or forbidden
arguments. Then invoke the same emitted argument shape through the real CLI.
Source identifiers from the state object or outcome that owns them rather than
re-deriving them from display text. Related:
[[route-tests-drive-real-launch-wiring]], [[test-names-match-assertions]].

---
name: shell-test-evidence-uses-script-path
description: Match shell Test references by exact committed script path while treating parentheticals and harness labels as descriptive text
metadata:
  type: convention
---

For a `.sh` behavior reference, the script's exact committed project-relative
path identifies the test. Its parenthetical describes coverage and need not
appear in either the script source or the structured result. The result's
`test_harness` label may be project-defined; requiring a particular display
label would reject evidence with the correct script identity.

The pre-review gate checks that the script exists in the candidate commit.
The final evidence gate requires a result whose ID equals that exact path and
whose status is passing; conflicting non-passing rows for the same path leave
the reference unsatisfied. A basename, alternate path spelling, or descriptive
label does not substitute for the committed path. Candidate binding and
structured-evidence validation still apply.

Applicable reviewers assess whether the script exercises the stated behavior.
Path admission and a passing script result establish traceability, while the
coverage description guides that semantic review. Related:
[[declared-behavior-tests-must-exist-before-land]],
[[rust-test-evidence-preserves-native-qualifiers]].

---
name: behavior-ids-must-be-numeric
description: Use numeric B headings in behaviors.md because completion-contract extraction ignores suffixed IDs
metadata:
  type: convention
---

`parse_behavior_completion_requirements` in `src/work_model.rs` recognizes
`documentation/behaviors.md` headings only when the text after `B` contains
ASCII digits. A heading such as `B6a` can look valid to a reader but silently
omits its statement and `Test:` references from the Writer completion contract.
Add a new numeric ID without renumbering existing behaviors, and assert that
contract extraction includes the new statement and its exact test references.

Related: [[ears-statements-are-atomic-and-triggered]],
[[declared-behavior-tests-must-exist-before-land]].

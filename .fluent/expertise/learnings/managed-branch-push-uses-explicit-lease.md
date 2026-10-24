---
name: managed-branch-push-uses-explicit-lease
description: Observe a managed remote branch directly and pass its exact head to force-with-lease instead of relying on a local tracking ref
metadata:
  type: gotcha
---

Fluent-generated publication branches may have no local remote-tracking ref.
Plain `--force-with-lease` derives its expectation from local tracking state, so
an initial publication can succeed while a later advancement fails because Git
still expects the remote branch to be absent.

Read the exact managed ref from the same validated push URL immediately before
publishing. Accept only no row or one row containing the requested ref and a
valid commit, then pass an explicit
`--force-with-lease=<ref>:<observed-commit>`; use an empty expected value only
when the branch was observed absent. Push the reviewed commit to that exact ref.
This preserves concurrent-update protection without depending on a fetch or a
local tracking branch.

Exercise the production adapter with an initial publication followed by an
advance after removing any local tracking ref. Related:
[[git-publication-validates-push-destination]],
[[external-adapter-contract-tests-drive-real-protocol]].

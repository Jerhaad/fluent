---
name: assert-persisted-message-equals-source
description: A regression that reads a persisted artifact must compare it for exact equality with the production source to prove write-path fidelity, and for exact equality with the declared approved wording to prove the source-policy invariant; a finite denylist cannot prove the absence of every process-oriented synonym
metadata:
  type: testing
---

The generated fix-pre-merge commit is assembled from a helper. In the defect,
`autofix_commit_message()` returned a `(subject, body)` tuple, the test bound the
subject and discarded `_body`, and the write path persisted *both* fields with
`git commit -m subject -m body`. A helper-level test that inspects only the
subject therefore cannot observe what the commit actually persists.

A later regression tried to close this by reading the persisted `%B`, but it only
checked a narrow denylist and that the intended subject was *contained somewhere*
in the result. Both survive an appended, unlisted body — for example a line like
`Run the formatting check before merge` — because a substring-containment check
accepts extra text and a denylist never anticipates the next process phrase. The
assertions all pass while the persisted message violates the declared
subject-only, content-oriented policy.

Close the gap by comparing the complete persisted artifact for **exact equality**
with the production message source, read at its real Git boundary
(`git log -1 --format=%B`, trimmed). Exact equality fails for any appended or
altered field a denylist would not foresee, and reading `%B` still exercises the
production assembly and Git write path rather than the helper's in-memory value.
This proves persistence fidelity: the write path holds exactly what the source
emitted.

Persistence fidelity is not the same as semantic policy. Equality with the
production source cannot show the source itself still emits an approved subject,
and a finite denylist cannot prove the absence of every process-oriented
synonym. Prove the source-policy invariant separately, and prove it exactly:
because this maintenance commit has one approved fixed subject, assert the
complete persisted message for exact equality with the declared approved wording
(`Conform code to project standards`). Keep the shape assertions — a single
subject and no body — but drop the denylist, which overclaims a policy it cannot
enforce.

The claim "the regression catches any divergence from the helper" is valid only
because the test makes the exact-equality assertion; a denylist-plus-containment
regression does not earn that claim. Related:
[[declared-behavior-tests-must-exist-before-land]],
[[test-names-match-assertions]], [[route-tests-drive-real-launch-wiring]].

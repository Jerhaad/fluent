# Hold Git HEAD while restoring a branch

## Context

A recovery command must replace an exact branch tip while preventing the
worktree's symbolic `HEAD` from changing. Git rejects a single transaction that
both verifies `HEAD` and updates its referent because the branch update also
tries to update HEAD's reflog. A separate, unlocked HEAD check leaves a race.

## Mechanism

Use Git's own prepared transaction to verify and hold the symbolic HEAD:

```text
start
option no-deref
symref-verify HEAD refs/heads/candidate
prepare
```

Read both acknowledgments before proceeding and keep the child's input open.
Prepare the checked branch transaction from the common repository, whose HEAD
names a distinct source branch. Use `option no-deref` before each saved source
and retained ref verification and before the candidate update with its expected
old commit. Keep the prepared transaction open while querying each ref with
`git symbolic-ref --quiet --no-recurse`; require exit status 1 (direct ref).
Only then send `commit`. The held locks prevent substitution after this check.
`no-deref` alone prevents changing an unrelated referent but still permits
replacing the symbolic ref itself, so it cannot replace the type check.

Close each transaction's input and reap its child when finished. EOF aborts
the prepared verification and releases Git's locks even if the coordinator exits
before cleanup. Do not invent a persistent `HEAD.lock` ownership protocol. Reject
Git versions without `symref-verify` support before writing recovery intent.

This lock protects HEAD identity, not the entire recovery. Keep the native land,
Task, execution, dispatch, and aggregate locks, immutable intent, retained commit,
and exact evidence checks. Test interruption around each durable publication and
assert that Git and execution owners have settled before fixture cleanup.

## Example

`candidate_ancestry_recovery::interrupted::PreparedRefs::hold_head` holds HEAD
while `checked_refs` prepares, checks direct ref types, and commits a separate
transaction to restore the exact saved candidate. The public binary test
`interrupted_writer_candidate_restore_replays_exactly` attempts to switch HEAD
while this lock is held, changes the candidate branch independently, and checks
that restoration rejects the changed expected tip without overwriting it.

`interrupted_writer_candidate_restore_rejects_symbolic_branch_substitution`
compares an eligible public restoration with the same fixture after its candidate
branch becomes symbolic to an unrelated branch at the same commit. The latter
must fail, preserve both refs and all Work state, publish no recovery operation,
and release every Git and execution owner before cleanup.

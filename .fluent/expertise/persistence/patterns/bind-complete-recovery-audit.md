# Bind an operation to its complete recovery audit

## Context

Use this pattern when a recovery must keep its original request identity after
publication, while binding an audit that includes the pre-recovery state.

## Mechanism

Derive the audit path from stable operator request fields. Publish the exact
before/after audit once at that path. Bind the complete audit bytes through the
existing recovery operation's `input_digest`, so changing even an audit-only
field invalidates the recorded operation.

If only one authorization may exist for a Task, choose the audit path from that
Task identity alone and bind the resolution inside the immutable audit. A path
that includes the resolution would allow conflicting decisions to occupy two
files after a crash between audit publication and model publication. Reject a
second audit with different bytes before touching the saved Work.

On retry, load the original audit through its request path before deriving the
operation. A partially published journal may prevent an observation snapshot.
Retain the authorized base in that journal, bind its children digest, and check
the original audited change against that base. Under the storage lock, require
each stored child to match its base or target record before completing replay.
Fence the journal schema so older writers cannot discard the base proof.
Reconcile only a matching explicit request before attempting an observation;
also recognize an already-recorded operation inside the locked reducer. Replay
returns that operation without replacing a later lifecycle state.

When an immutable evidence receipt retains the pre-recovery assignment, keep
that receipt unchanged. Share the exact audit validator between publication
and the reader that follows the before/after chain. Require the full saved
operation, audit digest, original request path, exact substitution, and snapshot
bytes for every link. This can preserve existing acceptance but cannot create
new finding credit or general assignment-edit authority.

## Example

`assignment_context_recovery::recover` addresses its audit by candidate, Task,
original assignment digest, input path, original-byte digest, and resolution.
The operation then hashes the whole audit, including its Attempt digest. The
binary regressions reject changed audit bytes and preserve a later cancellation
when the operator repeats a successfully applied request.

`context_retry::audit_ref` uses the original failed Task as its stable address.
The publication regression interrupts after writing that audit, refuses a changed
resolution, and permits only the exact original authorization to finish.

If a journal identifies new operations with a separate list, derive that list
again from its bound base and target. Retain the base in subsequent journals
carrying the authority too; otherwise deleting the list could turn an approval
publication into an ordinary write that skips its proof checks. Permit missing
metadata only for predecessor state that contains no such authority.

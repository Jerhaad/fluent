---
name: recovery-recognition-precedes-evidence-validation
description: Identify a lifecycle recovery family before validating its required evidence so malformed matches cannot fall through to generic resume
metadata:
  type: gotcha
---

Lifecycle recovery inspectors often return an optional frontier: no match lets
the caller try an ordinary resume path, while an error blocks an invalid
recovery. Establish whether the paused Attempt belongs to the special recovery
family from durable structural discriminators before requiring its artifact
references or reading its evidence. Useful discriminators include the Task
ordering, kinds and statuses, the immediately preceding Writer's candidate
output, and the historical policy at its recorded base.

Before the special shape is established, no match means the pause is genuinely
unrelated. After it is established, a missing, malformed, unreadable, or
noncanonical required input is an ineligible recovery and must return an error.
Returning no match at that point lets an outer fallback reopen the stopped Task
or authorize queued dispatch, silently turning corrupt recovery state into an
ordinary retry.

Use the same read-only inspector at foreground and queue admission before any
Work-model or dispatch-ledger mutation. A regression must drive both public
routes and remove both the persisted input reference and any discoverable file;
include non-regular evidence such as a directory. Snapshot the Work aggregate,
queue ledger, candidate Git identity and cleanliness, and evidence bytes to
prove rejection leaves the full owned boundary unchanged. A direct unit test of
the inspector cannot detect a caller's generic fallback. Related:
[[legacy-evidence-compatibility-stays-isolated]],
[[scheduler-resume-requires-explicit-dispatch-intent]],
[[route-tests-drive-real-launch-wiring]].

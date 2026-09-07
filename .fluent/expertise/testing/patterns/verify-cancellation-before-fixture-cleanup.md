# Verify cancellation before fixture cleanup

## Context

Use this pattern when a CLI test must prove that cancellation stops an owned
worker tree. A terminal `execution.json` state does not prove process exit, and
an owned fixture guard can accidentally hide a leak by stopping it during
`wait` or `Drop`.

## Mechanism

Have the worker publish its PID and a long-lived descendant PID before declaring
itself ready. Capture the execution record's leader and process group too, and
check that all captured processes are live in that group before cancellation.
Give both cancellation and the Attempt runner one deadline substantially shorter
than the fixture's natural lifetime. Poll `try_wait` to observe and reap their
exit without invoking fixture cleanup.

Before dropping any guard, require `kill(pid, 0)` to report `ESRCH` for every
captured PID and acquire the existing `execution-owner.lock` through a read-only
open and a nonblocking exclusive flock. Retry the release check within a bounded
interval because macOS can briefly delay flock release visibility.

Keep a separate failure-cleanup guard for the worker group: workers leave the
CLI owner's group. Setup and baseline commands can replace the same Task's
execution record, so refresh the guard's group until the target worker reaches
its ready barrier. For CLI fixture cleanup, accept macOS `EPERM` only after
observing that the owned child exited; live descendants still require a sweep.

To verify intent ordering, let the worker's first SIGTERM handler snapshot the
persisted cancellation record, then wait at a release barrier. Assert the
snapshot before releasing the worker. Mutation-check these assertions by moving
signaling before intent or writing terminal metadata without signaling, restore
the source, then rerun the ordinary focused command.

## Example

`tests/binary.rs::attempt_cancel_stops_each_stage_without_downstream_launch`
captures worker and descendant identities for Writer, verification, Tester,
review, and Learner. Its eight-second cancellation deadline is shorter than the
fixtures' 120-second lifetime, and it checks OS process exit and inherited
ownership before either fixture guard can clean up.

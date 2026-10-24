---
name: release-fixture-process-group-ownership
description: Own every process group a long-lived release fixture launches and verify cleanup during unwinding before fallback teardown
metadata:
  type: testing
---

A release fixture that starts a long-lived Fluent or provider command must own
the command's process group, not only its leader PID. The owner must remain
responsible after a normal wait or output collection, and its unconditional
cleanup path must terminate, escalate when necessary, and verify reaping of the
whole group. This cleanup must also run during assertion failure, timeout, and
unwinding.

Use the shared fixture owner for Rust launches rather than raw `Command::spawn`
calls followed by test-body cleanup. In shell behavior tests, install the cleanup
trap in the same subshell that starts the background child; a parent-scope trap
cannot own a child PID stored only in the case subshell.

Fluent workers can create a process group separate from their CLI owner. Arm a
separate execution guard before readiness or barrier assertions. During teardown,
attempt cooperative barrier release with a bounded wait, then stop and reap the
CLI before reading the final `execution.json` to identify the worker group.
Setup commands can replace the same Task's execution record while the CLI runs;
reading it too early can capture an obsolete group. Keep the temporary project
alive until both guards finish, and check inherited ownership through the
existing `execution-owner.lock` before signaling the recorded worker group.

Test both a leader with a surviving descendant and a real launch-capable fixture
path. To verify fixture cleanup itself, use the same fixture inside
`catch_unwind` and assert the exact intentional panic payload. Cover both normal
barrier release and a failed release write. A directory obstructing a file
barrier works only if the worker requires a regular file to proceed; a mere
existence check would open the barrier and miss forced cleanup.

Start the cleanup deadline before unwinding and keep it shorter than the
workers' natural lifetime. After the tested fixture drops, retain the temporary
project and any fallback guard while checking captured worker and descendant
PIDs for `ESRCH`, the existing ownership lock for release through a read-only
nonblocking flock probe, and the CLI for `ECHILD` to prove it was already reaped.
Fallback cleanup runs only after those observations, so it cannot hide a defect
in the fixture being tested.

Related: [[capture-pump-terminate-descendants-before-eof]],
[[verified-cleanup-for-bounded-host-probes]],
[[prove-settlement-path-not-just-outcome]].

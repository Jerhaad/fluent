---
name: process-inventory-distinguishes-exit-from-unavailable
description: Omit transient process rows only after a PID-specific probe confirms absence or zombie state; unresolved live processes and probe failures leave inventory unavailable
metadata:
  type: gotcha
---

Fluent's host process inventory snapshots the process table before resolving
each selected process's working directory. A process can exit or become a
zombie between those probes. A failed cwd lookup alone cannot establish that
the process is gone, and `kill -0` does not distinguish a zombie from a live
process.

For the per-PID `lsof` and `ps` probes in `src/process_settlement.rs`, only
exit status 1 with both stdout and stderr empty qualifies as no selection.
After that result from the cwd probe, query the same PID's process state.
Omit a row without a cwd only when that query confirms absence or returns a
validated zombie state. Retry a live process's missing cwd within a fixed
probe limit; a still-unresolved live process leaves the inventory unavailable.
Permission errors, warnings, partial records, malformed states, and invalid
UTF-8 are unavailable evidence, not proof of termination. A successful cwd
record must match the queried PID and contain exactly one absolute directory.

Test these lifecycle sequences through the production collector with scripted
command outputs, including exact arguments and exhausted probe sequences.
Also drive the real Tester path with transient rows in both the initial and
post-command inventory. Verify that usable baseline command exits, logs, and
normalized test failures survive, that an unchanged candidate failure still
matches the baseline, and that a new failure still counts as introduced. This
protects baseline comparison while preserving the process-leak guard.

Related: [[release-suite-scoped-process-guard]],
[[inject-side-effects-for-testability]],
[[capture-landing-owns-exact-tester-evidence]].

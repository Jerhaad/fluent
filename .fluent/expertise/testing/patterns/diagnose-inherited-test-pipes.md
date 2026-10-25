# Diagnose inherited test capture pipes

## Context

Use this when nextest reports LEAK after a test exits, especially when warned
test names change across concurrent runs despite verified worker settlement.

## Mechanism

Preserve native output, the runner version, candidate identity and dirty state.
Observe the live runner tree from the host without changing worker confinement.
Within one descriptor sample, compare a warned test's stdout/stderr pipe peer
with extra descriptors in sibling test leaders. Bind each PID to its recorded
parent and start time. Never compare pipe addresses across unrelated samples or
iterations: the kernel can reuse them. Retain failed and partial observations.
Request lsof's `a` field to record read/write access; do not infer that field was
measured in older records that omitted it.

To isolate inheritance from application cleanup, run a small diagnostic fixture
through the exact runner. Before opening files or starting work, inspect its
descriptors with `F_GETFD`, `F_GETFL` and `fstat`. Report pipe descriptors above
stderr, their close-on-exec flags and access modes. The fixture must create no
pipes or children, close no inherited descriptors, and retain concurrent test
execution. Compare it with a direct, concurrent libtest run as a positive control.
This diagnostic is separate from the project's canonical Tester command.

Extra writable pipes present before application work establish a runner launch
defect. Check its exact source and runtime rather than patching application
cleanup. Rust's macOS pipe implementation uses separate pipe creation and
close-on-exec calls; concurrent spawning can inherit a descriptor between them.
An inferred syscall race is distinct from directly measured descriptor flags.
Repair the tool boundary without serializing tests, extending leak timeouts,
closing unknown descriptors in test startup, or suppressing native failures.
Compare patched and unpatched runners built from the same source revision with
the same compiler and build mode. A private launch lock must cover capture-pipe
creation through spawning, then release while the test bodies run concurrently.
Keep any diagnostic runner separate from installed tools; the host must assess
the tool repair before selecting a runtime for the canonical Tester.

## Example

The quota-recovery investigation reproduced extra pipe descriptors with
`FD_CLOEXEC` clear, including write-only handles, in a fixture with no Fluent
dependency or child launches under nextest 0.9.137. Direct concurrent libtest
execution of that same fixture supplied the control. The production observer is
`tests/fixtures/collect-quota-process-evidence.py`; its hermetic regression is
`tests/behaviors/operations/test-quota-process-evidence.sh`.
The bounded correction is preserved as
`tests/fixtures/nextest-macos-pipe-inheritance.patch` for the pinned nextest
source below. Its private patched build passed the concurrent startup diagnostic;
the matching unpatched build reproduced inherited descriptors and native LEAKs.
Applying that patch does not install a runner or change the project's canonical
commands. Preserve the exact binaries, patch, fixture and command logs with the
diagnosis rather than interpreting a later clean run as a fix by itself.

Inspect the pinned [nextest capture implementation](https://github.com/nextest-rs/nextest/blob/75ddba7e911b44c5c0700dac0415d824403de9bd/nextest-runner/src/test_command/imp.rs)
and [Rust's macOS pipe path](https://github.com/rust-lang/rust/blob/1.91.1/library/std/src/sys/pal/unix/pipe.rs).

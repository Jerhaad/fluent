---
name: composed-recovery-instructions-bind-each-preparation
description: Validate composed Writer instructions by reversing exact audited retry preparations while preserving the full instructions delivered to the worker
metadata:
  type: architecture
---

A Writer can retain completion-recovery instructions through later approved
quota retries. Each recovery owner validates its own contribution. The quota
contract reconstructs the original instructions by walking that Task's recorded
approvals backward. For each matching artifact area, require the recorded
preparation and exact Task identity, instructions, and input artifacts before
following the audit's saved failed Task to the preceding state.

The completion contract validates the reconstructed original instructions
against completion authority, then returns the full current instructions for
delivery to the Writer. Do not strip a recognizable suffix or marker from
arbitrary text: matching prose does not prove that an operator approved the
addition. Keep reconstruction in the quota contract and completion validation
in the shared completion contract so queue, executor, and prompt consumers
cannot invent different composition rules.

Public-route regressions cover completion-authorized Writers that subsequently
hit quota limits, including repeated failures requiring fresh quota approval.
Exercise both foreground and queued resume, reject changed instructions and
approval bytes, and retain the original assignment and failure evidence.

Related: [[recovery-recognition-precedes-evidence-validation]],
[[recovery-contracts-form-neutral-boundary]],
[[route-tests-drive-real-launch-wiring]].

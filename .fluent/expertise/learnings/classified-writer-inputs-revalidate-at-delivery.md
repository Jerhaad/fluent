---
name: classified-writer-inputs-revalidate-at-delivery
description: Deliver a classified Writer's current authorization and exact audit through its actual launch context, including resumed provider sessions
metadata:
  type: architecture
---

An approved source correction changes the Writer's current assignment even
when an earlier request for host evidence remains in its history. At launch,
identify source correction as the current action and deliver the complete
authorization through the prompt the provider receives. Give the Writer a
readable reference to the exact candidate-bound classification audit; validate
its bytes again when building the prompt after Task reservation, and grant only
that file in the sandbox. Missing, changed, or oversized required input blocks
launch instead of expanding access or relying on historical metadata.

For a resumed provider session, the saved session must come from an earlier
Fluent-managed provider invocation. Test that the first invocation predates
classification and that the resumed invocation receives the newly authorized
instructions and reads the audit. A fabricated session ID or a persisted Task
assertion alone cannot establish delivery through the resume route.

Related: [[source-correction-recovery-binds-full-frontier]],
[[scoped-writers-separate-tools-from-mutable-state]],
[[route-tests-drive-real-launch-wiring]].

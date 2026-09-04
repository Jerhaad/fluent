---
name: source-correction-recovery-binds-full-frontier
description: Source-correction recovery must bind the complete review, evidence, candidate, and Writer frontier before mutation or replay
metadata:
  type: architecture
---

Source-correction recovery is a candidate-bound workflow, not a general
failed-review retry. Before its first model mutation, and again at every queue,
direct-Writer, or replay entry point, the host must validate the complete
immutable frontier:

- the current Attempt and recovery, original source candidate, and a clean
  workspace in the same repository and a registered managed worktree;
- exactly one complete Tests Review and its candidate-bound Review Task,
  review artifact path and digest, expected host-evidence input, and review
  context;
- a failed immutable EvidenceDocument with its exact digest and the accepted
  disposition, plus the authorization and audit identities; and
- no unrelated live or unresolved Task and no Merge Candidate for the same
  candidate.

Candidate and evidence digests alone are not a sufficient identity: the same
evidence can occur on different review frontiers. Bind the operation to the
review and Task identities as well, and reject a new classification or replay
when a later recovery has entered Reviewing or NeedsEvidence.

Use one shared contract for initial classification, legacy digest migration,
queue admission, direct Writer reservation, and replay. Persist the model’s
classification and Writer before publishing the deterministic audit artifact;
retries reconcile a missing audit and reject conflicting bytes rather than
publishing an audit that the model does not reference.

Replay follows the Writer lifecycle. A planned, executing, or user-paused
Writer may replay only while it has no output. A completed Writer may replay
only after its corrected output rebinds the current clean registered
workspace. Failed or superseded Writers are not automatic retry states. When
the corrected Attempt reaches terminal success, supersede exactly the one
failed, output-free source Writer that the correction replaced.

Related: [[audited-planning-recovery-binds-exact-frontier]],
[[corrective-writers-preserve-candidate-lineage]],
[[legacy-evidence-compatibility-stays-isolated]],
[[reserved-phase-terminal-finalizer]],
[[durable-external-effects-use-reconcilable-identities]].

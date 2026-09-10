---
name: recovery-contracts-form-neutral-boundary
description: Keep recovery admission and Writer completion above shared read-only verification facts, independent of lifecycle callers
metadata:
  type: architecture
---

Shared recovery admission, replay predicates, and authoritative Writer completion
selection belong in `recovery_contracts`. Shared persisted evidence and
planning-audit types live in `work_model`. `host_evidence`, the queue, and direct
Task execution consume these checks rather than selecting different source
matrices or maintaining separate admission rules.

Keep read-only verification facts below recovery admission.
`verification_reference` parses approved reference meaning and native identities;
`verification_assessment` owns evidence parsing, managed-artifact validation,
current requirements, canonical command occurrences, and accepted-proof adapters.
The gate, Writer requirements and context, status, and recovery consume those
facts. Legacy proof adaptation belongs at this shared boundary too; lifecycle
consumers cannot override its binding errors with command text or raw exit codes.

Keep the dependency one-way: the neutral contract must not depend on the
host-evidence producer, Task executor, or lifecycle callers. Lifecycle
recognition passes the recognized Task into neutral validation; calling back
into lifecycle recovery can create a transitive dependency on the executor.
Keep the surface crate-private unless an external API is required. When a new route
can admit or launch a recovery-bound Writer, extend the shared contract and
invoke it from that route rather than adding a caller-specific exception.

Tester produces contracts that recovery consumes. Resolve their shared declared
comparison inputs in `check_applicability`, below both Tester and recovery,
rather than making Tester call back into `recovery_contracts`. Keep shared value
types in `work_model` and include both consumers in dependency-boundary checks.

Recognize the specialized recovery before imposing its additional prerequisites.
A Plan containing host work does not make every interrupted Writer a host-only
recovery: an initial Writer or an unfinished continuation retains ordinary
foreground and queued retry. Test these ordinary controls alongside the narrow
host-only path so stricter evidence admission cannot strand unrelated work.

Related: [[extract-logic-to-avoid-test-duplication]],
[[pure-domain-keeps-filesystem-authority-at-edges]],
[[audited-planning-recovery-binds-exact-frontier]],
[[verification-requirements-ignore-diagnostic-wording]].

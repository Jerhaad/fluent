---
name: rust-test-evidence-preserves-native-qualifiers
description: Resolve Rust test evidence with every supplied path, binary, and module qualifier before requiring passing results
metadata:
  type: architecture
---

The candidate gate resolves Rust test identity across bare and qualified names,
source-path wrappers, and nextest binary/test representations. Preserve every
supplied source path, binary ID, and module qualification. A bare name may match
a qualified name with the same final identifier, but two supplied qualifications
must agree in full; suffix matching cannot establish identity.

Parse nextest binary IDs separately from Rust module names. The supported binary
forms are `package`, `package::target`, and `package::kind/target`; the target
kind and slash remain part of the identity. Reusing a Rust-identifier parser
can discard valid binary targets and hide competing evidence.

Build the identity view from all parseable results before checking status.
Failed and skipped identities still participate in ambiguity resolution. Every
matching representation must agree with the others and report a pass. A result
that omits qualifiers cannot bridge conflicting identities, even when the
reference itself is qualified. Explicit references may disambiguate evidence
only when the supplied result qualifiers support that distinction.

Keep native parsing and normalization in `verification_reference` and current
matching in `verification_assessment`, which the candidate gate consumes.
Preserve approved references and the producer's raw result ID and status.
Regression fixtures should drive the real gate with
committed source, include competing non-passing binary targets, reverse result
order, and retain passing controls for explicit disambiguation. Related:
[[declared-behavior-tests-must-exist-before-land]],
[[shell-test-evidence-uses-script-path]].

A producer-supplied structured identity must retain every qualifier already
present in its raw ID. Compatibility that allows absent qualifiers during
reference matching is too weak to validate this producer boundary: replacing a
qualified raw ID with a partial structured identity would broaden its meaning.
Reject omitted or conflicting qualifiers and recognized native identities of a
different kind. Apply the same rule to Python path, module, and class qualifiers.
Keep unqualified native names and free-form display labels supported when they
do not contradict the supplied identity. Test both production Tester
normalization and gate matching, including contradictions across native kinds.

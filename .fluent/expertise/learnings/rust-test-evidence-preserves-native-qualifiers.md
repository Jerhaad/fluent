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

Keep this interpretation inside the candidate gate without rewriting references
or Tester artifacts. Regression fixtures should drive the real gate with
committed source, include competing non-passing binary targets, reverse result
order, and retain passing controls for explicit disambiguation. Related:
[[declared-behavior-tests-must-exist-before-land]],
[[shell-test-evidence-uses-script-path]].

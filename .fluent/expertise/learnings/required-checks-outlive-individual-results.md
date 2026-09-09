---
name: required-checks-outlive-individual-results
description: Retain required check identities independently of reusable results and Task ordering
metadata:
  type: architecture
---

Fluent must retain a required check even when its previously accepted proof
becomes invalid. Canonical commands, required Plan rows, and reviewer-requested
commands remain separate obligations. In particular, a reviewer-requested
command absent from the canonical Tester must still block readiness after its
proof expires or changes. Filtering invalid results out of a list of accepted
canonical replacements would silently lose that independent requirement.

Evaluate reuse against the exact candidate, command, working directory, checker
configuration, declared environment and comparison inputs, source artifacts,
and targeted Review. A later no-change Writer or unrelated Task does not itself
invalidate those bindings. Preserve the original producer and Review as
provenance, and reject reuse when a newer conflicting result remains unresolved.
Candidate-code evidence has no age-only expiry; live-state evidence requires an
explicit positive lifetime. Older-candidate evidence cannot satisfy a changed
candidate, but it must also not prevent that candidate's initial reviews and
complete Tester from producing fresh proof.

Credit a canonical replacement only to its bound command occurrence. A
structured failure is covered only by its exact command-index, command, and
harness attribution. Missing command results, unrelated structured failures,
extractor errors, and unresolved process leaks remain blockers. Evaluate the
original complete run plus applicable replacements without rewriting its bytes.

Regression tests must invalidate proof after targeted acceptance while another
requirement remains outstanding, then try to finish that other requirement.
Include an independent reviewer command absent from canonical configuration;
using a canonical command for every fixture can hide a lost obligation.

Related: [[expired-evidence-renews-through-explicit-attachment]],
[[host-evidence-settles-against-bound-writer]],
[[no-change-output-binds-preserved-commit]],
[[negative-test-cross-product-coverage]].

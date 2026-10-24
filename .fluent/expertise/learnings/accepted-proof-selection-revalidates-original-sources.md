---
name: accepted-proof-selection-revalidates-original-sources
description: Pin ordinary Tests Review proof inputs without granting acceptance or reassigning historical findings
metadata:
  type: architecture
---

Ordinary Tests Review receives accepted host proof through a content-addressed
summary and exact source artifacts in its existing Task inputs. The saved
summary fixes the selected inputs; acceptance still comes from fresh validation
of the original evidence, bindings, and targeted Review. Keep the Plan,
reviewer-requested, carried-finding, and canonical-command validators distinct
inside the shared recovery contract.

Revalidate the saved selection against current acceptance before direct or
queued reviewer launch, including the final provider boundary. A changed
selection, source, candidate, or applicability must block that launch; a saved
summary cannot preserve acceptance after its proof expires or disappears.
Grant the selected sources through the existing exact regular-file resolver.
Publishing a summary does not justify broader directory reads or evidence writes.

Distinguish artifacts supplied as proof provenance from artifacts that assign
current findings. A historical request included to explain accepted proof must
not reassign its findings to the ordinary Review. Keep the original Writer
matrix unchanged and present current host acceptance separately in generated
context. Targeted acceptance retains its check or finding scope; ordinary review
coverage, final Tester, and Learning keep their own gates.

See [host evidence recovery](../../../documentation/architecture.md#host-evidence-recovery).
Related: [[recovery-contracts-form-neutral-boundary]],
[[required-checks-outlive-individual-results]],
[[ordinary-review-coverage-follows-writer-history]].

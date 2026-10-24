---
name: old-reader-fences-use-recognized-wire-fields
description: Fence every incompatible predecessor through recognized Work storage fields and verify both upgrade and refusal
metadata:
  type: architecture
---

Published Fluent Work Item readers can ignore unknown top-level JSON fields.
Adding a schema marker that such a reader never decodes cannot stop its mutating
commands from rewriting newer split records. Establish the compatibility fence
at a field the older root decoder already recognizes, using a representation
that it rejects before transaction recovery or child-record writes.

Keep the fence in the private storage representation. Fluent's writer envelope
preserves the public in-memory package-version value, and the current reader
also accepts legacy strings. Observation of accepted legacy records leaves
their stored bytes intact; adopting the current representation belongs to a
mutating boundary.

Check the older executable's actual decoder and mutation ordering rather than
inferring protection from a schema field in current source. Use the
[pinned older-reader pattern](../testing/patterns/pinned-older-reader.md) to
verify refusal through the older public command, including pending recovery.
That pattern also preserves separate coverage for a schema check behind an
earlier envelope refusal.

Treat compatibility as an explicit allowlist rather than a numeric range. A
temporary writer format can make supported envelope or transaction schemas
non-contiguous; reject an unsupported intermediate format before journal
recovery or child publication. Exercise each relevant pinned reader against
both published and pending state so an older mutator cannot rewrite a newer
authority through a less-visible path.

Independently developed Fluent recovery features can use the same record schema
number while preserving different authority. When combining them, inspect every
predecessor's decoder and mutation path; equal schema numbers do not establish
that any predecessor can preserve the combined Work aggregate. Advance the root,
Attempt, writer-envelope, and transaction fences beyond the incompatible
predecessors' supported limits while retaining supported historical reads.

Pin each distinct predecessor executable and verify both directions. Let it
author its own feature's published records and interrupted public transaction,
then require the current executable to preserve that authority through upgrade
and exact recovery replay. Separately require each predecessor to refuse
combined records and pending transactions without changing protected bytes.

An envelope upgrade also changes the current executable's storage-test contract.
Fluent pins deterministic JSON separately in `tests/work_model_external.rs` and
`tests/work_model_storage_review_external.rs`; its malformed-envelope tests also
use an explicit unsupported version. When advancing the envelope, inspect both
accepted-output fixtures and rejected-version fixtures. A version previously
used to prove refusal may now be valid. Update those expectations together,
retain malformed-input and unchanged-tree assertions, and run the affected
storage selections alongside the pinned older-reader checks.
Work storage versions its root and Attempt records, writer envelope, and pending
transaction separately. When advancing these schemas, update exact JSON
expectations in external storage tests, existing older-reader tests, and shell
behavior checks together. Keep historical fixture versions pinned to the reader
they exercise. For a current-reader rejection test, derive an unsupported future
version from the current serialized value instead of a literal that a later
upgrade may make valid. Focused tests for the newly added field do not cover all
these existing compatibility assertions.

Related: [[backward-compatible-serde-fields]],
[[semantic-validation-tests-reach-target-branch]].

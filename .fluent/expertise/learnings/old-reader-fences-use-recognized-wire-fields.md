---
name: old-reader-fences-use-recognized-wire-fields
description: Fence older Work writers through fields their published decoder recognizes while retaining current read compatibility with legacy records
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

Related: [[backward-compatible-serde-fields]],
[[semantic-validation-tests-reach-target-branch]].

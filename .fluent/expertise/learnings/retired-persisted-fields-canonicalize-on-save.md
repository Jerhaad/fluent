---
name: retired-persisted-fields-canonicalize-on-save
description: Retired persisted JSON fields must remain readable and disappear when a record is next saved
metadata:
  type: convention
---

When retiring a field from a persisted JSON record, remove it from the Rust
storage type while leaving deserialization compatible with older records. The
next ordinary mutation and save must write the canonical schema without the
retired key; do not retain a legacy shadow field solely to preserve obsolete
data.

Add a regression that seeds a legacy record with the retired field, performs a
real production mutation that rewrites the record, and asserts both that the
current behavior still works and that the rewritten JSON omits the legacy key.
This proves compatibility is read-only and prevents a retired secondary source
of truth from returning in new durable state.

Related: [[backward-compatible-serde-fields]],
[[model-owned-lifecycle-transition-ledger]],
[[assert-persisted-message-equals-source]].

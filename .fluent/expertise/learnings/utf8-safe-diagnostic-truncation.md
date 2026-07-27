---
name: utf8-safe-diagnostic-truncation
description: Byte-capped diagnostic excerpts must realign truncation to a UTF-8 character boundary before slicing
metadata:
  type: gotcha
---

Fluent caps diagnostic excerpts by byte length. When retaining a tail from a
`String`, the computed byte offset can fall inside a multi-byte UTF-8 character;
slicing at that offset panics. Advance the start offset to the next character
boundary before slicing. The resulting excerpt may be shorter than the nominal
byte budget, but it remains valid text and preserves the tail safely.

Apply this rule to shared excerpt helpers so every artifact path that reports
bounded diagnostics inherits the same Unicode-safe behavior.

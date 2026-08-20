---
name: candidate-commits-require-separated-body
description: Candidate commits need a non-whitespace body separated from the subject by a blank line before review or testing
metadata:
  type: convention
---

Candidate admission validates every commit message, not only its subject. A
candidate commit must include a blank separator after its subject and at least
one non-whitespace body line; a subject-only message or a whitespace-only body
is rejected as commit metadata.

The admission gate runs after a Writer completes and before Review or Tester
tasks are created. Fixtures that create incidental commits must therefore
provide conforming messages, while targeted gate tests should cover both forms
of missing body and a valid separated body. Related:
[[commit-body-bullet-point-convention]].

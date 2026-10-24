---
name: candidate-commits-should-use-separated-body
description: Candidate commits should use a non-whitespace body separated from the subject by a blank line
metadata:
  type: convention
---

Factory advises when a candidate commit lacks a blank separator and at least
one non-whitespace body line. This is presentation guidance: a subject-only
message or whitespace-only body does not block candidate admission by itself.

Writers should still add a separated body when practical, and fixtures should
use one unless they intentionally exercise the advisory case. History,
identity, message encoding, and attribution restrictions remain blocking.
Targeted policy tests should cover both forms of missing body and a valid
separated body. Related:
[[commit-body-bullet-point-convention]].

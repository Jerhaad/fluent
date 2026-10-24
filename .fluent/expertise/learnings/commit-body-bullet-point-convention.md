---
name: commit-body-bullet-point-convention
description: Commit bodies should use "- prefix" bullets for multiple changes when practical
metadata:
  type: convention
---

CLAUDE.md specifies that commit bodies use `- prefix` bullet points when listing
multiple changes. This is presentation advice: prose paragraphs do not block
completion or justify a failing review by themselves when they are accurate
and clear.

Each bullet should correspond to one logical change; the body is not a narrative
of what was wrong but a structured list of what was modified or added. Combined
with the 50-character subject-line cap, this keeps commits scannable and
reviewable. Continue to enforce history, identity, message encoding, and
attribution requirements independently.

Related: [[commit-subject-line-fifty-char-limit]]

---
name: commit-subject-line-fifty-char-limit
description: Commit subject lines should stay within 50 characters when practical
metadata:
  type: convention
---

The project's stated commit-message convention caps subject lines at 50
characters. Descriptive module names and test lists easily push subjects past
this limit. Treat the cap as presentation advice: follow it when practical, but
do not create corrective work or a failing review solely for a long subject.

Apply it during drafting: a subject like "Add scheduler service
foundation module with identity and registry" (62 chars) can be shortened to
"Add scheduler service foundation module" or similar when that remains clear.
History, identity, message encoding, and attribution restrictions still block
admission.

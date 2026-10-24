---
name: authorization-language-uses-bounded-affirmatives
description: Treat only exact affirmative forms as lifecycle authorization; contextual, uncertain, and negated action mentions remain contributions
metadata:
  type: gotcha
---

Do not authorize a lifecycle action because a participant message contains its
verb. Substring checks for words such as plan, execute, resume, or land also
match refusals, questions, uncertainty, and ordinary discussion. That can turn
"do not land this yet" into authorization.

Recognize a bounded affirmative grammar tied to the one pending action: exact
yes forms, the current action id or configured label, a matching revision-bound
interaction control, or an explicitly enumerated imperative for that action
kind. Route every other message through the ordinary contribution path.

Pair positive cases for every accepted form with negative cases that mention
the same action in negated, contextual, and uncertain sentences. Assert both
that the pending action remains and that the message reaches the normal
conversation path.

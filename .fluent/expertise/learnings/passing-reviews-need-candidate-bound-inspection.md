# Passing reviews need candidate-bound inspection

A model-written `Verdict: pass` proves only that the model produced the expected
words. Provider tool launches can fail while the provider process still exits
successfully, and those failures are not always present in the canonical
transcript.

For every new passing Review Task, require two independent pieces of evidence:

1. The reviewer declares its exact role, candidate commit, and at least one
   canonical tracked file with a short explanation of what it checked.
2. The host finds one successful provider tool result that identifies both the
   exact candidate and one declared file. A joined absolute path works, as do
   candidate-workspace plus repository-relative path arguments or exact-commit
   plus repository-relative path arguments in the same event. The latter
   covers Git `commit:path` reads through the repository's shared object
   database. Do not combine partial evidence from different events.

Validate declared paths through Git at the reviewed commit rather than through
the mutable working tree. Accept an earlier failed tool call when a later tool
result supplies the required proof. A fail or uncertain verdict already blocks
advancement and does not need this passing-review evidence.

# Passing reviews need candidate-bound inspection

A model-written `Verdict: pass` proves only that the model produced the expected
words. Provider tool launches can fail while the provider process still exits
successfully, and those failures are not always present in the canonical
transcript.

For every new passing Review Task, require two independent pieces of evidence:

1. The reviewer declares its exact role, candidate commit, and at least one
   canonical tracked file with a short explanation of what it checked.
2. The host finds a successful provider tool result that names the absolute
   candidate path to one declared file.

Validate declared paths through Git at the reviewed commit rather than through
the mutable working tree. Accept an earlier failed tool call when a later tool
result supplies the required proof. A fail or uncertain verdict already blocks
advancement and does not need this passing-review evidence.

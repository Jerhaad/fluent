---
name: expertise-claims-match-enforced-mechanism
description: Expertise and learning prose must scope every guarantee to its real precondition and describe a mechanism at the strength the code actually enforces; reviewers check these claims against source, and a claim that only survives under negation is a corrected overclaim
metadata:
  type: convention
---

Learner-written expertise is reviewed for factual accuracy against the source it
describes, not just for readability. Two overclaim shapes recur and each one gets
flagged and corrected:

- **Overstating how strongly a mechanism is enforced.** A module-private zero-sized
  token with a private unit field (`_private: ()`) and a module-private constructor
  is *not* unforgeable or field-less — any code in the same module, including the
  unit tests, can construct one. What such a token buys is a **convention** that
  restricts *production* construction sites to boundaries that have established the
  needed precondition, not a type the compiler makes unforgeable. Describe the
  guard as the convention it is. See [[status-pump-abandonment-reconciliation]] for
  the concrete `AbandonmentAuthority` case.
- **Stating a liveness or safety guarantee unconditionally.** "The submitter never
  hangs" is false when a live in-flight frame can still block indefinitely because
  no boundary has yet established abandonment. Bound the guarantee to its actual
  precondition ("once a termination boundary establishes abandonment, the waiter is
  resolved rather than left blocked") and name the residual case explicitly.

A reliable tell that a claim was an overclaim: it now survives in the prose only
under explicit negation — "this does *not* make the token unforgeable", "this is
*not* an unconditional guarantee". Keep that negating context; it is what makes the
corrected claim accurate, and dropping it reintroduces the overclaim.

When correcting or writing such prose, verify each load-bearing claim against the
cited source symbols (construct-ability, lock discipline, which field distinguishes
a path) rather than restating the intended design. This is the same accuracy
discipline as [[declared-behavior-tests-must-exist-before-land]] and
[[test-names-match-assertions]], applied to explanatory prose instead of tests.
Related: [[prove-settlement-path-not-just-outcome]].

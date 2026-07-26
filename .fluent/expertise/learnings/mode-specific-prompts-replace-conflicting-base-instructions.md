---
name: mode-specific-prompts-replace-conflicting-base-instructions
description: A mode-specific prohibition must replace the contradictory shared instruction, not merely append after it; shared prompt prose stays capability-neutral and the per-mode render owns the allowed action
metadata:
  type: convention
---

When one prompt template serves several execution modes, a late mode-specific
prohibition does **not** neutralize an earlier contradictory command in the shared
body. A `no-expertise` or handoff-only Learner render that says "Write one file per
learning" and "Maintain the index" up top, then appends "do not write
`.fluent/expertise/`" at the end, is not a coherent, testable execution contract:
the coder is told to do and not do the same thing. Reviewers block this even though
the sandbox would deny the write anyway, because a contradictory prompt causes
avoidable denied operations, failed runs, and incorrect reasoning.

The rule:

- **Shared prompt prose stays capability-neutral.** It may tell every mode to
  inspect the change, reviews, tests, existing expertise, and the follow-up schema —
  analysis only. It must not tell every mode to refine, merge, write, index, or
  commit expertise.
- **The mode-specific render owns the allowed action.** Gate the actual
  write/index/commit instructions on the mode (a `{{#if expertise_writable}}` branch
  in the template, or a per-mode `&'static str` directive). `capture` renders the
  positive "write one file per learning, maintain the index, commit `Update
  expertise`" instructions; the denied modes render "leave expertise unchanged;
  propose missing knowledge as a non-corrective follow-up" instead.
- **Every render that reuses the template agrees.** The initial user prompt, the
  initial system prompt, and the schema-repair system prompt must carry the same
  mode-accurate action, since a schema repair reuses the rendered system prompt.

Test the *rendered production prompt through the production construction path*, not
an isolated directive token or a test-only re-render. Both the launch path
(`run_learner_with_coder`) and the tests build prompts through one helper
(`build_learner_prompts` returning `LearnerPrompts`), so a passing unit test cannot
drift from what the coder receives. Cover it at two levels, because a helper-only
test does not verify the route ([[route-tests-drive-real-launch-wiring]]):

- **Helper level.** Call `build_learner_prompts` for every mode and for both the
  initial and schema-repair inputs and assert the mode-accurate content. Prove
  `capture` retains the concrete write commands (e.g. "Write one file per learning",
  "Maintain `<index>`") and prove both denied modes omit those exact commands — not
  merely the phrase "Update expertise". Asserting only the absence of a slogan lets a
  contradictory body slip through.
- **Launch-route level.** Drive the real `run_learner_with_coder` for every mode ×
  {initial, schema-repair} with an injected recording coder and assert the exact
  prompts it receives (a forcing mode reaches the coder once its host preparation is
  satisfied through the injected `HostPreparation` seam). A separate fail-closed launch-route test injects a
  system-prompt and a user-prompt fault independently and asserts the route returns
  the prompt error with the coder factory and launch counters both at zero. Claim
  route coverage only once both route tests exist; a helper test alone does not prove
  the launched prompts.

**Fail closed on prompt construction, do not fall back to a raw template.** The
helper returns `Result` and propagates every content-resolution and template-render
error; there is no raw-template or empty-system fallback. It also scans each
template-rendered prompt for a surviving `{{`/`}}` token (an unresolved variable or a
doubled-brace escape that rendered to literal braces) and errors before any coder is
constructed. A schema-repair user prompt embeds verbatim JSON whose braces are
legitimate, so scan only the template-rendered prompts. Test both failure paths: an
unrenderable template and a placeholder-leaking one.

**Order irreversible side-effecting steps after the decision they depend on.** The
same coherence rule governs *procedure* order, not just prose within one render. A
skill or script that instructs an irreversible default action (e.g. `fluent
work-item create`, which fixes the Learner mode) *before* the step that decides its
mode is incoherent: a later conditional cannot undo a default command already run.
Put the decision first and present the mutually exclusive commands only inside
explicit per-outcome branches, and add an ordering assertion (the decision text
precedes the first side-effecting command) so the sequence cannot silently regress.

See [[persisted-policy-vs-resolved-execution-mode]] for keeping the mode enum
crate-private, [[extract-logic-to-avoid-test-duplication]] and
[[route-tests-drive-real-launch-wiring]] for sharing the production path with tests,
and [[expertise-claims-match-enforced-mechanism]] for matching documented guarantees
to the enforced mechanism.

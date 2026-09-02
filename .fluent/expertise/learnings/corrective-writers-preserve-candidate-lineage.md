---
name: corrective-writers-preserve-candidate-lineage
description: Give every corrective Writer the exact prior candidate and a host-owned lineage mode; append by default and expose the identical-tree metadata exception only from validated gate evidence
metadata:
  type: architecture
---

A corrective Writer can satisfy a content request while accidentally making its
candidate inadmissible. For example, using rebase to remove an unwanted file also
removes the reviewed candidate from reachable history. The later candidate gate
correctly rejects that history, but only after a model call has been wasted.

Prevent that avoidable loop by making correction lineage explicit before launch:

- Record the exact current candidate in the generated execution context as the
  prior candidate for the correction.
- Select the reviewed-correction prompt whenever correction evidence is
  attached. A preceding `continue` outcome may coexist with a later
  candidate-gate rejection and must not select the generic continuation prompt.
- Use that same correction-evidence classification when recording the durable
  Writer run. Inferring the run kind only from preceding review Tasks misses
  candidate-gate corrections that occur before the first review wave and
  undercounts corrective work.
- Select a host-owned lineage mode from durable gate evidence, not from the
  Writer's interpretation of prose or Git state.
- Default to append-only. Content removal, restoration, and other ordinary
  corrections produce a new commit, and the named prior candidate remains an
  ancestor.
- Expose the tree-identical metadata-rewrite mode only when one validated
  rejected gate names the current candidate, establishes candidate identity,
  and fails either solely on commit metadata or on exactly that failure plus
  the approved derived project check. The derived case requires the canonical
  stored `check-pre-merge.log` to contain `commit <id> body line <positive
  integer> exceeds 72 characters` for a commit whose gate finding is
  `literal-line-breaks`; a project-check-only, mixed, or unrelated failure
  remains outside the exception.
- Keep the deterministic candidate gate authoritative. Prompt guidance prevents
  a predictable mistake; it does not replace ancestry, original-base, or tree
  identity checks.

Test both the generated context and the real coder launch route. Assert the
substituted mode and commit identity rather than freezing explanatory prompt
wording. Exercise the metadata exception separately so a malformed, mismatched,
mixed-failure, or ambiguous gate cannot broaden the default.

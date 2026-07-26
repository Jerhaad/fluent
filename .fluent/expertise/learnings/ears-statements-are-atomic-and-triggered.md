---
name: ears-statements-are-atomic-and-triggered
description: Each EARS statement in behaviors.md must name one independently verifiable effect and include the operation or event that triggers it
metadata:
  type: convention
---

Behavior statements in `documentation/behaviors.md` are independently
verifiable contracts. Do not combine rejection timing, prohibited side effects,
diagnostics, persisted state, source binding, candidate contents, and eventual
outcomes into one `WHEN`/`IF` statement. Split distinct effects into separate
numbered statements and retain a passing `Test:` reference on every resulting
statement.

A statement must also name the operation or event that observes its condition.
For example, `WHEN every managed instruction file already contains...` only
describes state; `WHEN fluent init finds that every managed instruction file
already contains...` identifies the trigger and gives the test a clear boundary
to drive.

When a behavior change expands an existing primary-flow contract, do not fold
the new command-shape guarantee into an older statement about an unrelated
outcome. Give the new effect its own statement so a regression maps to one
contract. Related: [[behaviors-test-citation-sync]],
[[declared-behavior-tests-must-exist-before-land]].

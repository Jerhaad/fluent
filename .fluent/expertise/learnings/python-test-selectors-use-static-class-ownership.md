---
name: python-test-selectors-use-static-class-ownership
description: Resolve class-qualified Python selectors from committed declarations without importing candidate code
metadata:
  type: architecture
---

Fluent's pre-review candidate gate resolves class-qualified Python `Test:`
selectors with a local Rust text scanner. It checks the committed file for a
direct `def` or `async def` in the named top-level class and requires a unique
matching class and method declaration. Selector components use ASCII identifier
syntax. The scanner masks comments and strings, joins logical statements, and
tracks indentation so text mentions and methods in unrelated scopes cannot
establish class ownership.

Keep the supported boundary explicit when extending this resolver. It does not
resolve inheritance, dynamically assigned methods, or nested or conditional
class definitions. A source selector names a class and method within its file;
the gate does not resolve import-module aliases or import or execute candidate
Python. This is a limited declaration check, not a full Python parser or proof
that the test runs.

The final Tester must independently supply passing evidence for the candidate.
Python evidence retains every supplied path, module, and class qualifier across
native unittest names and source-path wrappers. Its ambiguity rules follow the
same principle as [[rust-test-evidence-preserves-native-qualifiers]]: resolve
identities across all results before requiring a pass, so short names cannot
bridge conflicting qualifications.

See the [source-resolver decision](../decisions.md#resolve-python-source-selectors-without-execution)
and [[declared-behavior-tests-must-exist-before-land]].

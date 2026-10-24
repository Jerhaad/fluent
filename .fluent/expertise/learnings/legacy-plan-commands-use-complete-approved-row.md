---
name: legacy-plan-commands-use-complete-approved-row
description: Recover missing host commands from the complete approved Plan row while preserving stored command metadata and rejecting ambiguity
metadata:
  type: architecture
---

Saved Plan completion rows can predate structured command metadata. When
ownership adoption divides such a row among responsible roles, recover a
missing host command through the same parser used at Plan intake. Supply the
entire original requirement and all its verification references. Parsing only
the proposed host part can hide a competing command in another part of the
approved row. A missing or ambiguous command leaves the proposal invalid;
partitioning responsibility cannot resolve that ambiguity.

Existing structured command metadata takes precedence over prose extraction.
Preserve its exact invocation, working directory, and environment on the split
rows, rebinding only the command's source-row identity. The compatibility
fallback fills missing metadata only for host parts; it does not infer commands
for other roles or replace an already stored command.

Regression cases should place competing commands in both another proposed
part and the original verification references. These cases distinguish checking
the whole approved row from checking only the selected host text.

Related: [[audited-planning-recovery-binds-exact-frontier]],
[[test-fixtures-use-production-state]].

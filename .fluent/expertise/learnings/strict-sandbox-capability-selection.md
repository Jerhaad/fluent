---
name: strict-sandbox-capability-selection
description: Select shared-temp-free sandbox profiles only for launches that receive an explicit private replacement boundary; preserve standard grants for other coders
metadata:
  type: architecture
---

The strict Seatbelt renderer removes the broad shared temporary-directory write
grants. Select it only for a launch that has an explicit private replacement
boundary, such as a prepared Codex worker home granted to that invocation or a
Learner staging directory with a launch-owned temporary directory.

Do not assume `TMPDIR` accounts for every provider runtime path. Claude 2.1.220's
Bash tool creates session state under
`/private/tmp/claude-<uid>/<workspace-key>/` even when the launch exports a
private `TMPDIR`. A strict Claude Learner therefore needs that one workspace
namespace as an additional writable root. Derive it from the effective uid and
exact workspace path. Claude maps every workspace-path character other than an
ASCII letter, digit, or hyphen to a hyphen when it names this namespace. Include
a dotted path in the encoder regression rather than deriving both the expected
and granted paths with the same helper. Never restore the shared `/private/tmp`
grant or admit a sibling workspace namespace. Prove both the allowed root and a
denied sibling with a real Seatbelt test.

Do not apply that strict rendering merely because code passes through a shared
sandbox builder: non-Codex rebase coders rely on the standard shared temporary
grants unless a separately designed replacement boundary is supplied. Keep the
selection at the capability boundary (`Option<&CodexWorkerEnvironment>`), and
test both branches: strict Codex routes must lack the broad grants, while the
normal rebase route must retain them.

Related: [[sandbox-denials-track-template-grants]],
[[canonicalize-confinement-paths]].

---
name: canonicalize-confinement-paths
description: Canonicalize existing paths before passing them between the environment and Seatbelt policy, while retaining the lifetime guard that owns temporary-directory cleanup
metadata:
  type: architecture
---

The same filesystem object can have multiple spellings through symlinks. When a
Codex worker's `CODEX_HOME` is granted by a Seatbelt profile while the interactive
source home is denied, using aliases in either the environment or the policy can
make the boundary inconsistent. Canonicalize paths that already exist before
passing them to authentication staging, `CODEX_HOME`, preflight, or profile
rendering, so every participant uses the same spelling.

`tempfile::TempDir` remains the cleanup owner, but it is not the path value that
the worker exposes: retain the guard for its lifetime and store the canonical
`PathBuf` separately. A configured source-home path that does not yet exist must
remain unchanged because it cannot be canonicalized; this preserves the
configured spelling and avoids turning an absent-path fallback into an error.

Exercise this at the public launch boundary with aliased worker-temp and
source-home fixtures, asserting the environment, worker write grant, and source
denials use the resolved paths. Related: [[sandbox-denials-track-template-grants]],
[[route-tests-drive-real-launch-wiring]].

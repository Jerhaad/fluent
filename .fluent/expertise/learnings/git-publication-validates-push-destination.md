---
name: git-publication-validates-push-destination
description: Validate the effective Git push URL independently from the fetch URL and publish only through that validated destination
metadata:
  type: architecture
---

Git permits a remote's fetch URL and push URL to name different hosts or
repositories. Validating only `remote.<name>.url` does not establish where a
later `git push <name>` will write.

Before publishing a reviewed commit, read both the configured fetch URL and the
effective push URL. Require an unambiguous single value for each, normalize
each to the supported repository identity, and verify both match the
host-configured integration. Use the validated push URL itself for the remote
head observation and the push so a later Git lookup cannot select a different
destination.

Production-adapter tests must include a matching fetch URL with a mismatched
push host or repository and assert rejection before any remote mutation.
Related: [[git-subprocesses-use-central-wrapper]],
[[pointer-identity-gate-verifies-reviewed-sha]].

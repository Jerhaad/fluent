---
name: provider-argument-failures-classify-at-launch
description: Classify configured provider argument rejection at every launcher boundary from captured parser evidence, not from settlement error text
metadata:
  type: architecture
---

Provider command-line argument failures are recoverable only when the launcher
can prove their process-level origin. Each supported provider launcher must
classify a nonzero captured launch as `ProviderArgumentError` only when its
stderr contains that provider's usage banner, a recognized argument-parser
diagnostic, and an exact configured pass-through option token. Missing capture,
unreadable stderr, unrelated wording, or a token that merely prefixes another
option remains a generic coder failure.

Keep this classification at the provider boundary, before reviewer or Writer
settlement. Settlement may use the typed error to select a narrowly recoverable
pause, but it must not infer recoverability from arbitrary error text. Preserve
the provider, exit status, and bounded diagnostic and invocation context so the
handoff remains auditable. Test every launcher and one real phase route with a
hermetic provider double; helper-only classification tests do not prove the
typed error reaches settlement. See [[terminal-coder-errors-bypass-retry-budget]]
and [[route-tests-drive-real-launch-wiring]].

---
name: provider-readiness-crosses-reservation
description: Shared provider admission remains a no-inference boundary whose prepared proof crosses reservation; configured init may add a separate pre-mutation live canary
metadata:
  type: architecture
---

Provider admission has one shared `CoderKind`-keyed boundary
(`provider_readiness::ProviderReadiness::prepare`) for configured setup and
autonomous Writer, Reviewer, Learner, and Rebase launches. Provider adapters at
this boundary must verify only local command and authentication readiness: they
must not start an interactive session or request model inference. When a
provider returns structured authentication status, parse and validate the
declared state rather than treating a successful process exit as proof of
authentication.

Configured setup may add a separate check before mutation when first-use
confidence requires more than local authentication. Claude configured init does
this with one bounded, safe-mode, tool-free response from every distinct
selected model. Keep that canary outside `prepare`: it proves only that the
chosen model answers now, is not retained as a launch capability, and must not
make autonomous phase admission consume model quota before reservation.

Do not assume a provider's isolated `HOME` can locate host credentials. On
macOS, the `security` command derives its default Keychain search list from
`HOME`; a clean worker home can therefore make a valid login Keychain appear
empty. A host credential bridge should address the effective Unix account's
login Keychain explicitly, then pass only the supported credential into the
prepared provider environment. Never copy the Keychain or credential into the
worker home.

Run readiness before a phase records its durable in-progress reservation. If
readiness prepares a launch capability, such as the private Codex worker
environment, retain the complete readiness object through the reserved launch
rather than reconstructing a provider-specific subset. Rechecking after the
reservation can turn a preflight failure into an already-reserved phase and
creates divergent readiness semantics between providers.

Launch-route regressions must prove both properties: a readiness failure leaves
the phase unreserved and does not invoke the coder, while a successful reserved
run invokes the provider readiness check exactly once. See also
[[atomic-task-start-reservation]] and [[route-tests-drive-real-launch-wiring]].

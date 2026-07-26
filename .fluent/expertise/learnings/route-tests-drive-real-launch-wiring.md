---
name: route-tests-drive-real-launch-wiring
description: A launch-route regression must drive the real phase launch path and fail if it drops or re-resolves the threaded value — a helper test that only asserts config/resolver layering does not verify the wiring and the tests reviewer blocks on it
metadata:
  type: testing
---

When a behavior requires that a phase's launch route threads a resolved value
(a `TranscriptCapture`, a resolved pump config) all the way into the coder, a
test that calls the resolver helper directly (e.g. `resolve_config_from(...)`)
and asserts config layering is *not* sufficient. A regression that dropped or
re-resolved the value on the actual route would still pass that helper test.

The tests reviewer draws an explicit resolver-vs-route distinction and blocks
when only the helper shape exists. A conforming route regression:

- drives the **real** launch route (`run_learner_with_coder`,
  `rebase_candidate_with_coder`), not a helper;
- injects a recording coder that captures what reached `run_captured`;
- asserts the resolved capture's transcript path *and* a distinctive resolved
  threshold (the tests use a sentinel project `console-preview-limit: 7777`)
  arrive verbatim;
- **fails if the route drops or re-resolves** the value — the failure
  sensitivity is the point.

When the route only reaches the coder after a host gate (e.g. a forcing Learner
mode that resolves the trusted Seatbelt boundary runs the fixed
`os::check_prerequisites_for` → `credential::inject_credentials` →
`credential::setup_git_signing` sequence, which reads `PATH`, Keychain, AWS, and
Git-signing state), do **not** satisfy the gate by mutating process-global state.
Writing fake `sandbox-exec`/coder executables and prepending `PATH` under
`#[serial_test::serial]` makes the test non-hermetic and serial, and still touches
live credential/signing state. Instead give the *route itself* one narrow,
test-only injection seam for exactly that host-preparation operation: a private
`HostPreparation` enum whose `Production` variant runs the real fixed sequence and
whose `#[cfg(test)]`-only `Injected` variant defers to a recording closure. The
production adapter always passes `Production`; a non-test build has no injectable
variant (keep the `'a` lifetime live with a `#[cfg(not(test))]` `PhantomData`
marker so the lib still builds). The hermetic route test then drives the real
`run_learner_with_coder`, injects a recording no-op preparation, and asserts
preparation runs **zero** times for an unsandboxed mode and **once** for each
forcing mode — never mutating `PATH` or reading live credentials. A separate test
injects a sentinel preparation failure for a forcing mode and asserts the coder
factory and launch counters stay at zero (preparation failure returns before coder
construction). This is still the real route — only the leaf host-preparation
dependency is injected — so the resolver-vs-route gap this lesson closes stays
closed. The prompt-fault variant needs no host seam at all: a build error before
coder construction short-circuits ahead of the host gate, so its fail-closed test
asserts the counters stay at zero directly.

This is the same "test the real path, not a copy of it" principle as
[[extract-logic-to-avoid-test-duplication]], applied to launch wiring: a helper
test verifies the resolver, only a route test verifies the route.
Related: [[public-api-surface-test]], [[declared-behavior-tests-must-exist-before-land]],
[[mode-specific-prompts-replace-conflicting-base-instructions]].

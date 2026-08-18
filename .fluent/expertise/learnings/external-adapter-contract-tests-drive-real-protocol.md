---
name: external-adapter-contract-tests-drive-real-protocol
description: Test production protocol adapters through bounded local servers and child fixtures, not only in-memory traits or extracted helpers
metadata:
  type: testing
---

An in-memory transport or provider proves coordinator behavior but cannot prove
the production adapter sends credentials, identities, destinations, protocol
fields, acknowledgements, or child environments correctly. Likewise, directly
testing a decoder or environment helper does not prove the live connection or
process-launch route calls it.

Use deterministic local fixtures at each production edge:

- a local HTTP server for Web API authorization, scopes, request bodies,
  same-thread delivery, and stable retry identities;
- a local WebSocket server for Socket Mode connection, envelope decoding, and
  acknowledgement over the live socket;
- a framed or line-oriented child fixture for provider initialization,
  session/turn recovery, and the environment seen by the spawned process;
- fake host executables for Keychain and LaunchAgent command arguments.

Drive the real adapter method or configured runner route and assert both the
request and externally visible result. Keep coordinator tests with in-memory
doubles as a separate layer. Related: [[route-tests-drive-real-launch-wiring]],
[[extract-logic-to-avoid-test-duplication]], [[inject-side-effects-for-testability]].

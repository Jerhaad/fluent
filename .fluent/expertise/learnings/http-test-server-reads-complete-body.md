---
name: http-test-server-reads-complete-body
description: Local HTTP test servers must read headers and the complete Content-Length body before asserting a production request
metadata:
  type: testing
---

TCP reads do not preserve HTTP request boundaries. A local test server that calls
`read` once can receive complete headers but only part of the request body, even
for a small loopback request. Assertions against that partial buffer then fail
nondeterministically and can misdiagnose the production adapter.

Fixture servers must accumulate bytes until they find the header terminator,
parse `Content-Length`, and continue reading until the buffer contains the full
declared body. Exercise the helper with deliberately small read buffers so the
test proves fragmented delivery works instead of passing only when the kernel
coalesces the request. Then assert the production adapter's complete request.

This complements [[external-adapter-contract-tests-drive-real-protocol]]: a
real local protocol fixture is useful evidence only when the fixture itself
implements the protocol's message boundary correctly.

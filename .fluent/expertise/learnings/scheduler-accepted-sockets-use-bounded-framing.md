---
name: scheduler-accepted-sockets-use-bounded-framing
description: Put accepted scheduler Unix streams in blocking mode with timeouts before reading framed requests
metadata:
  type: gotcha
---

The scheduler service keeps its Unix listener nonblocking so its loop can poll
workers. On the supported macOS host, accepted streams can inherit
`O_NONBLOCK`. A valid length-prefixed request may arrive as a header followed
later by its payload; `read_exact` on that stream can return `WouldBlock` and
close the exchange before the payload arrives.

Configure each accepted stream for blocking reads and writes with bounded
timeouts before serving a frame. Keep a real-socket regression that sends the
header and payload separately and checks the response. A retry in one client
path does not establish the contract for other request types.

Related: [[framed-protocol-frame-size-cap]],
[[external-adapter-contract-tests-drive-real-protocol]].

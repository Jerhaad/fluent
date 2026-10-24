---
name: scheduler-service-owns-execution-submission
description: Submit execution through the scheduler's versioned service API; only that service validates projects and builds, persists requests, admits queues, owns capacity, and reports status
metadata:
  type: architecture
---

Slack collaboration and other front ends are clients of the scheduler, not
alternate scheduler implementations. They submit a stable, versioned execution
request through the scheduler service boundary and query status through that
same boundary. They must not write scheduler request files, dispatch records, or
synthetic queue tokens directly.

The scheduler service owns the complete admission path: validate the protocol
generation, running build, registered canonical checkout, and checkout identity;
persist the idempotent request; create or reuse the real queue dispatch; enforce
per-project capacity; claim and launch Attempt workers; and report status from
the bound durable dispatch. A persisted client-side request is intent, not proof
of delivery. After a failed or lost acknowledgement, retry the same stable
identity and let the scheduler reconcile idempotently.

Starting a socket listener alone does not establish this boundary. The
production scheduler lifecycle must serve the protocol and poll every registered
project, and setup must register each configured checkout before clients accept
execution approvals. Contract tests should cross the framed service API and
cover protocol rejection, missing status, replay, stale build or checkout,
capacity, restart, and a real terminal dispatch outcome.

Related: [[token-bound-to-persisted-record]],
[[external-adapter-contract-tests-drive-real-protocol]],
[[lock-ordering-across-subsystems]].

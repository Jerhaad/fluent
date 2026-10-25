---
name: scheduler-service-contains-project-failures
description: Recover each registered checkout before claiming work, contain project failures to that poll, and keep failed stop checks out of admission
metadata:
  type: architecture
---

The shared scheduler service polls registered checkouts in order. For each
checkout it owns, it reconciles durable dispatches before claiming fresh work.
A configuration, queue, coordinator, or worker failure in one checkout must
retain its project and available dispatch identity in diagnostics while later
checkouts still get polled. Registry failures stop the poll because the service
cannot establish which projects it should visit. A typed host sandbox preflight
failure stops the service before it claims or launches work because that gate is
service wide.

A failed stop check or draining acknowledgement must prevent new claims for
that checkout in the current poll. Retry the failed check on the next
configured poll, without logging or retrying it on every service-loop tick.
When acknowledging a stop requires a metadata write, retain the previous
in-memory coordinator state until that write succeeds so the next poll can
retry. Tests should put a faulty checkout before a healthy one, exercise a
failure first observed on the due poll tick, and leave queued work behind a
failed stop acknowledgement.

Related: [[scheduler-service-owns-execution-submission]],
[[lock-ordering-across-subsystems]], [[lease-acquire-types-contention-vs-infrastructure]].

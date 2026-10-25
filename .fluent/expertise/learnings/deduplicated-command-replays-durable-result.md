---
name: deduplicated-command-replays-durable-result
description: A duplicate consequential command suppresses the external effect but replays its persisted result through a deterministic outbox identity
metadata:
  type: architecture
---

The collaboration inbox uses the inbound event identity to prevent repeated
processing, but a generic `Processed => return` is not sufficient for commands
whose response is part of the interface. A Slack redelivery that already
created an external resource must not create it again, and it must still return
the durable resource identity to the thread.

On the processed-event path, recognize the consequential command, load its
persisted result, verify that the result belongs to the command's requested
object (including historical identities when the object can advance), and
schedule the response through a deterministic outbox identity. Do not call the
external adapter again. The outbox then makes repeated replays converge without
duplicating either the external effect or the visible response.

Test this with the exact same inbound event identity across a runner restart.
Assert one external invocation, a response containing the persisted result, and
no additional delivery when that same event is replayed again. Related:
[[durable-external-effects-use-reconcilable-identities]],
[[recovery-cursors-advance-after-durable-ingest]].

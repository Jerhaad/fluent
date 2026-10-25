---
name: recovery-cursors-advance-after-durable-ingest
description: Recover paginated event history from a durable high-water mark and advance it only after each event is durably processed
metadata:
  type: architecture
---

A reconnect loop cannot recover reliably by requesting one fixed-size history
page. Persist a per-stream high-water mark, pass it to the external history
API, follow every pagination cursor, sort recovered events in source order, and
route them through the same durable inbox as live ingress.

Advance the high-water mark only after the event has completed durable ingest
and processing. If processing fails, retain the last safe cursor so the next
reconnect includes the event again. Live delivery and recovered history may
overlap; the inbox's stable event identity must deduplicate that overlap rather
than relying on the cursor alone.

Tests need more than one page, a failure between two recovered events, and a
live/history overlap. Assert that restart begins from the last safe cursor and
that every contribution has one visible effect. Related:
[[durable-external-effects-use-reconcilable-identities]].

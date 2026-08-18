---
name: bound-conversation-state-follows-project-authority
description: Keep bound conversation payloads and active state in the project runtime; the runner-owner root retains only routing, deduplication, and unbound ingress
metadata:
  type: architecture
---

The local Slack runner has two persistence authorities. Before a thread binds,
the runner-owner root may retain the full ingress needed to offer and select a
project. After binding, contribution text, provider sessions, pending turns,
planning state, and other active conversation data belong under the bound
checkout's ignored `.fluent/work/slack/` runtime tree. The owner root retains
only the private project mapping, a redacted routing envelope, and an opaque
deduplication identity.

Do not treat a stripped owner-side index record as a fallback snapshot. If the
authoritative bound snapshot is missing or unreadable, fail closed instead of
loading incomplete state and replaying the turn as unbound work.

Publishing the project snapshot and owner-side index spans two filesystem
locations, so atomic replacement of each file is insufficient. Write a durable
commit record that contains the previous index state, next index state, and
snapshot digest before changing either representation. Recovery promotes the
next index only when the project snapshot matches the recorded digest;
otherwise it restores the previous index. Exercise crashes after either write
order.

Related: [[atomic-write-replace-through-utility]],
[[registry-read-modify-write-needs-flock]].

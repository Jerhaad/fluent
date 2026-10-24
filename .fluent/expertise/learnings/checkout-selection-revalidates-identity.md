---
name: checkout-selection-revalidates-identity
description: Revalidate a discovered checkout's canonical Git identity and current authority before every setup, commit, and publication effect
metadata:
  type: architecture
---

A path-free checkout offer is only a point-in-time selection, not a durable
filesystem capability. Discovery may expose a safe label and an opaque selection
id, but the private discovery entry must retain the canonical path and the exact
Git checkout identity that produced the offer.

Before every consequential boundary, rediscover under current owner policy,
confirm that the actor still has the required authority, and compare the live
checkout identity with the selected identity. Apply this independently before
initialization, before creating an approved commit, and immediately before
publishing the checkout into runner configuration or binding a conversation.
An earlier check does not cover replacement of the Git administrative directory
between effects. Reject an identity mismatch without mutating configuration,
thread state, or the replacement checkout.

Keep host paths out of Slack-visible state. Resolve the opaque selection through
the private canonical entry at the effect boundary instead of persisting or
accepting a participant-supplied path. Tests should replace the Git
administrative directory between selection and each effect and assert that the
effect fails closed. Related: [[approval-records-bind-current-authority]],
[[pure-domain-keeps-filesystem-authority-at-edges]].

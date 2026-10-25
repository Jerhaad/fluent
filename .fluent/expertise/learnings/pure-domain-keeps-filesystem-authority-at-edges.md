---
name: pure-domain-keeps-filesystem-authority-at-edges
description: Keep collaboration domain state path-free and resolve filesystem authority in interfaces and adapters
metadata:
  type: architecture
---

The pure collaboration domain models protocol-neutral identities, thread state,
pending turns, and deliveries. It does not import or persist `PathBuf`, sandbox
profiles, checkout paths, or draft directories. Filesystem authority belongs
at the interfaces and adapter boundary, where configured checkout identities
resolve into a scoped provider capability.

This separation prevents Slack-derived aggregate state from becoming an
ambient host-filesystem capability and lets policy tests run without host path
semantics. When a provider turn needs access, construct an explicit scope at
the coordinator edge and grant only the bound checkout for reads plus the
current Slice draft for writes. Keep runner-owner homes, credentials, sibling
checkouts, shared temporary roots, and mounted volumes outside that scope.

Protect the boundary with a source-level test that rejects filesystem payload
types in the pure domain, plus a production launch contract that inspects the
rendered sandbox and child environment. Related:
[[strict-sandbox-capability-selection]], [[canonicalize-confinement-paths]].

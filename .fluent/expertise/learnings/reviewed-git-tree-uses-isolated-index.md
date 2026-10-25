---
name: reviewed-git-tree-uses-isolated-index
description: Build an exact approved Git tree through a temporary index so unrelated staged, unstaged, and untracked work remains untouched
metadata:
  type: architecture
---

When Fluent commits a reviewed subset of generated checkout changes, the normal
Git index and working tree may already contain unrelated user work. Never stage
the generated paths into that index or derive the commit from the checkout's
current staging state.

Bind the review to the expected `HEAD`, a digest of the ordinary index, the
pre-existing changed paths, and each generated entry's before/after bytes and
Git mode. The mode is part of the reviewed content: preserve regular versus
executable files and symlink entries, including the symlink target bytes.
Immediately before commit, revalidate `HEAD`, the ordinary-index digest, the
review revision, every reviewed after-entry, and the absence of overlap with
pre-existing changed paths.

Construct the tree with a temporary `GIT_INDEX_FILE`: seed it from the reviewed
parent, write blobs from the reviewed bytes, apply the reviewed cache-entry
modes, and write the tree. Create the commit from that tree and advance the
branch with compare-and-swap against the reviewed parent. This leaves the
ordinary index, staged changes, unstaged changes, and untracked files untouched
while ensuring the commit contains exactly the approved entries. Route every
Git invocation through [[git-subprocesses-use-central-wrapper]]. Related:
[[planning-approval-binds-exact-revision]],
[[host-owned-git-transaction-over-untrusted-coder]].

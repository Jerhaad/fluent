---
name: rebase-preserves-review-invalidation-provenance
description: Preserve the pre-rebase Writer tip so landing can reconstruct ordinary review invalidation
metadata:
  type: architecture
---

Capture landing may rebase a Merge Candidate after its ordinary Reviews pass.
The rebase can rewrite the final Writer output and make a diff against the new
target hide paths that Writer changed. Record the original candidate and the
settled rebase tip in the Rebase Task output, then reconstruct review coverage
using the authenticated pre-rebase Writer tip. Equal contents on the target do
not restore a Review invalidated by that Writer's earlier change.

Before landing, bind each retained ordinary Review to the Writer that produced
its candidate and require every currently applicable role. A Review bound to
an unrelated existing commit must not gain credit merely because that commit
exists. On retry, accept a changed source HEAD only when it equals the exact
recorded rebase result; an additional source commit needs fresh verification.

Related: [[ordinary-review-coverage-follows-writer-history]],
[[capture-landing-owns-exact-tester-evidence]].

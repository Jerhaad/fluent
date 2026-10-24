---
name: lock-ordering-across-subsystems
description: Release the queue lock before mutating the Work model; the codebase has a lock hierarchy across queue, Work model, lineage, candidate, and follow-up locks that must not be inverted
metadata:
  type: architecture
---

The codebase holds several independent file locks — the queue lock plus the Work model, lineage, candidate, and follow-up locks. Code that touches more than one must acquire them in a consistent order to avoid deadlock from lock inversion.

The concrete discipline the architecture reviewer checks: **never acquire a Work-model lock while holding the queue lock, and release the queue lock before mutating the Work model.** Queue admission (`add`, `resume`, automatic dispatch, and claim) may acquire one Work Item's model lock first and then the project queue lock. It retains both through the ledger mutation so a lifecycle change cannot race eligibility, but it does not mutate the Work Item in that critical section. For corrective follow-up mutations, acquire locks in this order: follow-up operation (when present), root lineage, a specific Work Item, then queue reconciliation. Replay retains its outer operation lock through `ensure_dispatch` so no same-operation processor observes a partially completed journal. For example, the scheduler releases the queue lock before `ensure_bound_attempt` mutates the Work model, while corrective promotion releases its lineage lock before queue reconciliation.

A related invariant makes some ledger mutators safe without a generation check: recovery transitions (`requeue_active`, `reconcile_active`, `block_active`, `cancel_active`) mutate the latest dispatch under the queue lock without a token-based generation check, unlike the owning worker's `with_matching_active`. This is only safe because recovery runs solely in the single elected coordinator's loop and reconciliation bails out while a bound-Attempt lease is live — so no worker and recovery ever write the same dispatch concurrently. A future contributor who introduces a second concurrent writer of the ledger must restore the generation check; the single-coordinator + live-lease invariant, not a generation check, is what currently keeps recovery correct.

Related: [[backward-compatible-serde-fields]]

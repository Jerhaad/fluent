---
name: status-pump-abandonment-reconciliation
description: The status pump reconciles abandoned work only under a module-private AbandonmentAuthority token whose production construction sites are intentionally restricted, and resolves the idempotent one-shot acknowledgement with the coordinator lock released — its two in-process mutexes are never held together
metadata:
  type: architecture
---

The transcript status pump (`src/transcript_pump.rs`) settles a required status
write through a shared coordinator. Ordinary settlement is *not* abandonment
reconciliation: once `store.write` returns, the worker publishes the raw result as
`Observed` and calls `settle_active(None)` — no authority, because a real result
already exists. Abandonment reconciliation is the separate path that fixes a bounded
"worker unwound" result for a `Pending` frame that can never return, and it is
reached only at a termination boundary: the worker's own panic catch, the worker's
wake-disconnect loop exit, and — after each *joins the worker first* —
`finish` and `Drop`. Two invariants make that shared coordinator safe against
concurrent reconciliation, and a future writer touching it must preserve both.

**Synthesizing an unwound result requires an owned proof.** An accepted required
write sits in the active record as `Pending` until its `store.write` returns. A
reconciler may only fix a bounded worker-unwind result for a `Pending` frame if it
holds an `AbandonmentAuthority` — a module-private zero-sized token with a private
unit field (`_private: ()`) and an ordinary module-private constructor
(`assume_worker_abandoned`). The private field and module-private constructor do
*not* make the token unforgeable or field-less: any code in this module — including
the unit tests, which construct it directly — can build one. What the convention
buys is that *production* construction sites are intentionally restricted to
boundaries that have established that no live store frame can still return: the
worker's own panic catch after the stack unwound, the worker observing a
disconnected wake outside any store call and committed to exit, or `finish`/`Drop`
after the worker join completes. A direct or concurrent reconciler without the token
leaves `Pending` owned and unresolved rather than preempting a real store result
that is still in flight. Do not widen the production constructor's reach or hand the
token to an arbitrary production reconciler; that convention — not an unforgeable
type — is the guard against publishing a fabricated result over a live one.

**The coordinator mutex and the acknowledgement-cell mutex are never held
together.** The submitter blocks on an idempotent one-shot (`AckCell` = a
`Mutex<Option<Result>>` plus a `Condvar`). The resolver authority — carried by the
queued command and then the active record — fixes exactly one immutable result via
`resolve_once` and wakes the waiter; a second or concurrent resolution observes the
stored result and neither replaces nor re-delivers it. Every `resolve_once`, every
`is_observable` read, and every settlement probe runs with the coordinator lock
*released*. State that could be a "second truth" is derived, not stored: a retired
queued command is changed in place to `Resolving` inside the single shared deque
(there is no second container), and both its and the active record's
`Resolved/Retiring` state are derived from resolver observability. Because every
termination boundary that establishes abandonment resolves the same one-shot,
reconciliation does not strand the waiter — once such a boundary is reached the
submitter is resolved rather than left blocked. This is not an unconditional "the
submitter never hangs" guarantee: a live `Pending` store frame that never returns
can still block its submitter indefinitely, because no boundary has yet established
abandonment. And because resolution is idempotent, repeated and concurrent
reconciliation converges.

Related: [[lock-ordering-across-subsystems]] (the host's *file*-lock hierarchy — a
different domain from these in-process coordinator mutexes),
[[compose-typed-failure-precedence]], [[production-lock-test-hooks]],
[[prove-settlement-path-not-just-outcome]].

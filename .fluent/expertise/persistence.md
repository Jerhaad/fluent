# Persistence

Fluent persists operations, journals, Work Items, and scheduler state under
`.fluent/work/`. Treat both their serialized fields and their deterministic
identifiers as durable formats.

## Patterns

- [reconcile-deterministic-identity-changes](persistence/patterns/reconcile-deterministic-identity-changes.md) — read when changing how persisted operation or effect ids are derived

- [bind-complete-recovery-audit](persistence/patterns/bind-complete-recovery-audit.md) — read when an idempotent recovery records a before/after audit that must remain bound after lifecycle changes
- [hold-git-head-during-restoration](persistence/patterns/hold-git-head-during-restoration.md) — read when a checked branch update must also preserve a worktree's symbolic HEAD

- [validate-interrupted-recovery](persistence/patterns/validate-interrupted-recovery.md) — read when external recovery must validate input before replaying a pending transaction

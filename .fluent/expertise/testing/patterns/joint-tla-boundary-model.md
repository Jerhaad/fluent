# Compose existing TLA actions at a workflow boundary

## Context

Use this when a detailed protocol model and `Workflow.tla` each pass alone,
but a Work Item requires a checked connection between them.

## Mechanism

Instantiate both modules with separate variables in a small joint module.
Advance preparation actions in one instance while the other stutters. Couple
the actions at the observable boundary, and check the mapped identities and
state under bounded interleavings. Add separate invariants for each assumption
the coupling needs, and a defect configuration that violates the mapping or an
assumption by name. A production trace must then observe the identities and
state used by the mapping; a green component model alone cannot credit the
connection.

TLC finds an imported scheduler module when the checker supplies
`-DTLA-Library=<scheduler-model-directory>` to Java. Keep the original
protocol model and its configurations intact.

## Example

`tests/formal/workflow/QueueDispatchConnection.tla` instantiates `Workflow`
and `QueueHandoff`. Queue claim, lease acquisition, and Attempt creation
stutter in `Workflow`; `QueueHandoff.MarkRunning` and `Workflow.Dispatch`
advance together. `MappedDispatch` checks Work, Attempt, dispatch, lease,
generation, and owner identities. A public `scheduler run` trace supplies
the raw facts, while `queue-dispatch-early.cfg` must violate that mapping.

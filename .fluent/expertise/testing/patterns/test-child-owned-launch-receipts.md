# Test child-owned launch receipts

## Context

Use this pattern when a parent records process ownership after creating a child,
and recovery must still identify the child if the parent dies during launch.

## Mechanism

Reserve a fresh generation under the execution lock. Make the child acquire its
inherited identity lock and write a fixed receipt containing its PID before exec.
Hold the parent model and execution lock descriptors across fork, so the child
keeps them until exec even if the parent dies. Test the transition with separate
processes: stop the child after receipt publication, kill the parent, request a
durable stop in another process or thread, and verify that stop waits until the
child crosses exec. Test malformed receipts while the identity lock stays held;
neither cancellation nor a replacement launch may infer a group from them.

## Example

`tests/execution_handoff.rs::launch_handoff_cancel_serializes_with_payload_start`
holds a test-only pre-exec barrier after receipt publication. It kills the
launch parent, observes the Work model lock blocking a stop mutation, releases
the child, then requires cancellation to settle before the delayed payload can
write its marker.

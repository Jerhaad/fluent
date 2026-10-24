---
name: production-traces-bind-public-cause-to-model
description: Bind fresh public-command traces to exact artifacts, causal facts, and formal actions
metadata:
  type: testing
---

The workflow foundation joins a public CLI sequence to `Workflow.tla` through
`tests/formal/workflow/project.py` and `Trace.tla`. A projected state sequence
must come from fresh public-command observations with ordered causal events,
exact snapshot artifacts and digests, and the candidate commit and executable
identity. The model check must then match those states to named workflow
actions; validating JSON shape alone does not establish that connection.

For a refusal, record the public fact that identifies its cause as well as the
nonzero exit and unchanged Work state. Otherwise an unrelated command failure
can satisfy the same projected state. Exercise both projector rejection and an
artifact-consistent contradictory trace that the model refuses at the claimed
action. This keeps the production check distinct from a self-reported command
verdict or a stale fixture.

Related: [[formal-workflow-bounds-need-witnesses]],
[[semantic-validation-tests-reach-target-branch]].

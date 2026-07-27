---
name: attempt-coder-mapping-is-runtime-authority
description: Resolve coder mappings from config and environment only when creating an Attempt; existing Attempt and Task runs use the persisted mapping, with explicit CLI fields applied as a fresh atomic overlay
metadata:
  type: architecture
---

An Attempt's persisted coder mapping becomes the authority for every later
Attempt run, direct Task run, and Learner-only recovery. Resolve configuration,
environment variables, and coder defaults when Fluent creates the Attempt. Do
not resolve them again when Fluent runs or resumes an existing Attempt: a
flagless run reads the stored mapping and performs no mapping write, even if the
current environment or project configuration has changed.

Run-time coder, model, and effort flags are sparse overrides, not a request to
rebuild the mapping. Construct `CoderMappingInputs` from explicit CLI fields
only, overlay only the named fields, and preserve every other role and field.
Apply that overlay through `overlay_attempt_coder_mapping`, which derives and
persists the effective mapping from the Attempt freshly read inside
`WorkModelStore::mutate_work_item`. Computing a complete replacement before the
lock can restore stale values from fields the command did not override. This is
the same fresh field-level mutation principle as
[[fresh-field-level-finalizer-preserves-concurrent-state]].

Keep remote launch selection separate from execution policy. The Fargate
entrypoint may use `FLUENT_CODER` to validate and prepare provider credentials,
but it must not translate that value into an `attempt run --coder` override.
Persist any user-supplied run overrides before archiving the workspace, then let
the remote run consume the uploaded Attempt mapping.

Tests for this contract must observe both sides: the complete persisted mapping
and the coder, model, and effort that reach the real launch route. See
[[route-tests-drive-real-launch-wiring]].

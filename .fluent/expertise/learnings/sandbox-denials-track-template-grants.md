---
name: sandbox-denials-track-template-grants
description: Sandbox denial tests must account for common write grants, and handoff-only confinement must strip the shared-temp grants
metadata:
  type: gotcha
---

`os::render_profile_for_access_for_coder_with_denied_writes` adds explicit deny roots, then removes the broad `/private/var/folders` and `/private/tmp` write grants by replacing their exact rendered Seatbelt text. If `sandboxes/common.sb` or profile generation changes the spelling or layout of either grant, the replacement silently becomes a no-op and weakens handoff-only Git confinement.

When changing common sandbox grants or the renderer, update this stripping logic and keep an integration test that proves writes to the denied shared-temp paths actually fail. If refactoring the profile builder, express the exclusion during profile construction so confinement does not depend on post-render string surgery.

For an ordinary sandbox denial test, choose the unrelated write target outside
every path allowed by the rendered profile. A temporary directory under
`/private/var/folders` is writable under the common profile, so a denial
assertion there cannot prove confinement. Have the sandboxed worker read and
validate the delivered input contents, not only check that their paths exist.

Post-land Learner recovery also runs against a no-hardlink disposable clone and a sibling temporary handoff surface. Keep this isolation even for `--no-sandbox`: it prevents a disabled or unavailable Seatbelt profile from exposing the live target checkout or common Git directory. Import only a regular draft file after the isolated run succeeds; never restore a live checkout from a broad snapshot after running untrusted code.

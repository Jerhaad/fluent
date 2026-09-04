---
name: skill-reference-files-keep-main-bundle-small
description: Keep the full skill under its line budget and move low-frequency procedures into explicitly discoverable reference files
metadata:
  type: convention
---

The materialized full skill is a compact operational entry point. Keep its
main file below the project’s 500-line budget, put low-frequency procedures
and recovery detail in `references/` files, and state the exact condition that
tells the reader when to load each reference. Keep mutually exclusive routes
separate so the main skill does not make a host-only path sound like a
classification or execution path.

Treat bundled skill content as a tested interface. When a reference has
several major sections, give it a `## Contents` section; when adding or moving
detail, update the materialized-content tests for the line budget, reference
paths, and route wording. This keeps progressive disclosure useful without
letting the compact entry point drift from the procedures it dispatches to.

Related: [[mode-specific-prompts-replace-conflicting-base-instructions]],
[[managed-skill-installation-ownership]],
[[expertise-testing-patterns-are-copyable-templates]].

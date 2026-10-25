# Security

Fluent accepts paths from model output and durable artifacts. Validate both
their meaning and their exact lexical spelling before using them as authority,
scope, or filesystem inputs.

## Patterns

- [evaluate-conditional-git-includes](security/patterns/evaluate-conditional-git-includes.md) — read when granting file access from Git configuration includes

- [require-canonical-relative-path-spelling](security/patterns/require-canonical-relative-path-spelling.md) — read when an untrusted path participates in an authorization or identity decision

## Declared sandbox input symlinks

Writer and Reviewer prompts may advertise exact host-owned files. Build their
Seatbelt roots with `declared_file_readable_roots`: retain the declared path,
canonicalize it, require a regular-file target under the canonical project
root, and add only that exact target when the spellings differ. Do not replace
this with a parent-directory grant. A missing target, symlink cycle, directory,
or out-of-project target is an authorization failure before Task reservation.

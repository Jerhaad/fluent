# Evaluate conditional Git includes before granting access

## Context

Use this when a confined worker needs exact Git configuration file reads.
Git's include query returns inactive `includeIf` declarations, while its origin
query omits active files that contain no settings. Neither alone identifies
every required file.

## Mechanism

Run Git in the retained worker worktree. Before granting a conditional target,
ask Git to evaluate the same condition against a temporary marker configuration
in the already-prepared private worker home. Query only the marker key and its
origin; accept only the exact marker origin. Remove the temporary files when
the query completes. Do not materialize a future cache during preflight.

Keep Git responsible for matching branches, Git directories and remote URLs.
Moving a `gitdir:./` condition requires preserving its declaring file's real
parent directory as a literal prefix, including escaping glob characters.
Quote configuration subsection strings and keep include paths separate from
conditions so names containing `=` remain valid. Unknown conditions remain
inactive according to Git's own evaluator.

After evaluating the condition, admit only the existing regular target and its
lexical spelling. Preserve active empty includes and nested/symlinked files;
grant no parent directory and no configuration writes.

## Example

`writer_git_include_is_active` and `installed_writer_git_config_roots` in
`src/work_task_executor.rs` implement this boundary. The
`usage_limit_git_config_roots_filter_conditional_includes` unit regression covers
condition matching and empty files, while
`usage_limit_writer_recovery_profile_resolves_fixture_paths` checks inactive
read denial in the production profile. The required host confinement probe
also attempts the denied read with the actual sandbox.

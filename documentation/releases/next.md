# Next release: local Work and service transition

## Move unfinished Work forward

Stop the old shared scheduler before the new binary writes Work state. With
v0.5.0, run `fluent scheduler stop` in each registered project to drain its
coordinator and settle its active Work. This command does not stop the shared
service. Stop the original supervised service process after every project has
settled, then verify that the process has exited and its health socket no
longer answers. The `--all` flag belongs to the newer binary and is not
accepted by v0.5.0. Start the replacement only after the old owner exits.

Inspect Work with `fluent work-item show <id>` and run
`fluent work-item upgrade <id>` for a legacy split record. The command retains
the Work and Attempt identities and is safe to repeat after an interrupted
publication. Continue with the next public command shown by `work-item show`
or `fluent status`; a canceled Attempt needs its normal recovery authority.
Read-only inspection leaves the stored format unchanged.

The release-transition checks use the published v0.5.0 executable at commit
`6875b4091c88d0012181181bc641739a74fb2588` (SHA-256
`8bb76717be942e4a0c97c297c318c0a1331e92105180edb386354dddbae12974`)
in isolated projects. The focused tests verify planned Work migration,
interrupted publication recovery, and refusal by the predecessor against a
current record and a pending current transaction. The strict host service
check exercises two registered projects, queue retention, service stop, and
replacement; its result is recorded by the release-transition host suite.

## Rollback boundary

After the newer binary writes a Work record or publication transaction,
reinstalling v0.5.0 alone cannot roll it back. The predecessor must reject
mutation of that state. Continue forward with the compatible binary and the
public recovery command. This release does not offer a supported backup
restoration procedure: no complete restore set has been rehearsed, and
restoring an older backup would discard later Work. There is no automatic
lossy downgrade.

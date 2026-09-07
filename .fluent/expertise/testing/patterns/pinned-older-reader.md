# Verify mutation refusal with a pinned older reader

## Context

Read this when a persisted schema introduces fields that an older Fluent build
must refuse to ignore during mutation. Reimplementing the old deserializer in a
test does not exercise the older executable's command routing and storage path.

## Mechanism

Build the identified older source before the measured fixture, using the
Attempt's private Cargo cache offline and a separate target directory. Record
both its full source commit and executable SHA-256. Pass the resulting path to
the test through an explicit environment variable. The test checks that identity
before running the older public command against an isolated copy of the new
record, then compares the stored model after refusal. Use byte comparisons for
artifacts whose exact bytes or digests form part of the contract.

Keep this preparation in the declared Tester command so the test cannot silently
skip compatibility verification when the executable is absent. Never substitute
an installed binary or the integration branch's target directory.

When a new record envelope rejects the reader before a previously tested schema
fence, keep the current-format refusal case and add a fixture that restores only
the older envelope representation. Preserve the protected schema fields so that
fixture still proves the inner fence rejects mutation. Compare stored bytes and
directory entries after both refusals.

## Example

`tests/fixtures/build-ownership-old-reader.sh` prepares the source pinned in that
script and emits the executable path. The canonical command exports it as
`FLUENT_OWNERSHIP_OLD_BIN` before nextest. The independently selected binary test
`legacy_ownership_old_reader_refuses_mutation` checks the identity and invokes
that executable's `work-item upgrade` command against a new-schema fixture.

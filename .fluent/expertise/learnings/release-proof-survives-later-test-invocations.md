---
name: release-proof-survives-later-test-invocations
description: Isolate release trace collections and revalidate their retained bytes when assembling the final assurance report
metadata:
  type: testing
---

Fluent's integrated assurance checker runs formal release collection before
approved named tests that can invoke the same release fixtures again. A shared
output directory lets a later passing test replace traces and receipts already
checked by a projector. The earlier pass then describes different evidence from
the bytes retained for the final report.

Give formal collection and each later invocation separate retained directories.
Release fixtures accept the collection directory through
`FLUENT_RELEASE_TRACE_DIR`; standalone invocations allocate their own directory.
Bind the checked traces, referenced raw artifacts, and case logs by digest when
the release suite completes. Final report assembly must revalidate those
bindings and reject changed, missing, or unbound proof before granting credit.

Exercise repeated and concurrent fixture invocations to prove directory
isolation. Separately alter or remove retained traces, raw artifacts, and logs
to prove that final accounting rejects them even when the saved suite result
still says it passed.

Related: [[production-traces-bind-public-cause-to-model]],
[[host-evidence-writes-use-exclusive-create]].

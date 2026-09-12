# Recover a failed-review round-cap pause

## Contents

- [Authorize the bounded extension](#authorize-the-bounded-extension)
- [Record the replacement Approach](#record-the-replacement-approach)
- [Resume and retry](#resume-and-retry)
- [Retain later gates](#retain-later-gates)

## Authorize the bounded extension

Inspect the reviewed candidate and the exact failed-review artifacts. Run:

```text
fluent attempt extend <work-item-id> <attempt-id> --additional-write-rounds N --resolution '<authorization>'
```

The extension must bind the same candidate and failed-review bytes while
leaving an unused Writer round. It authorizes only the bounded correction; it
does not revise planning or prepare a Writer.

## Record the replacement Approach

Save the complete replacement Approach, then run:

```text
fluent attempt revise-approach <work-item-id> <attempt-id> --approach-file <path> --resolution '<authorization>'
```

Use the matching authorization. Fluent journals the immutable audit bytes as a
managed file in one recoverable Work-model transaction with planning, the
completion contract, and one corrective Writer.
Before that transaction is applied, no audit sidecar exists. A pending
transaction may expose the managed file while recovery applies it, but the
aggregate reference and exact bytes together establish authority. The audit
retains the source candidate, exact supporting reviews, failed-review bytes, and
extension history.

## Resume and retry

After revision, explicitly continue through one of these routes:

```text
fluent queue resume <work-item-id> --attempt <attempt-id>
fluent scheduler run
```

or run the foreground route:

```text
fluent attempt run <work-item-id> <attempt-id>
```

Exact retries reuse the same revision and Writer while the bound frontier
remains unchanged. Changed candidate, review bytes, authorization,
cancellation, or unsupported state is rejected without publishing a new
transaction.

## Retain later gates

The corrective Writer retains actionable findings and the later host, Tester, review, and Learning gates. A recorded revision is not a passing verification
or landing authority; complete every later gate before treating the candidate
as ready.

#!/usr/bin/env bash
# Verify Factory's commit body and trailer rules remain project-owned.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CHECK="$ROOT/scripts/check-commit-message-policy.sh"
FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT

git -C "$FIXTURE" init -q
git -C "$FIXTURE" config user.name "Fixture User"
git -C "$FIXTURE" config user.email "fixture@example.com"
printf 'seed\n' > "$FIXTURE/value.txt"
git -C "$FIXTURE" add value.txt
git -C "$FIXTURE" commit -q -m $'Seed fixture\n\n- Establish the policy base'
BASE="$(git -C "$FIXTURE" rev-parse HEAD)"

printf 'candidate\n' >> "$FIXTURE/value.txt"
git -C "$FIXTURE" commit -qam $'Add candidate\n\n- Explain the candidate'
(
  cd "$FIXTURE"
  FLUENT_BASE_COMMIT="$BASE" "$CHECK"
)

assert_rejected() {
  expected="$1"
  if output="$(cd "$FIXTURE" && FLUENT_BASE_COMMIT="$BASE" "$CHECK" 2>&1)"; then
    echo "commit-message policy unexpectedly passed" >&2
    exit 1
  fi
  if ! grep -Fq "$expected" <<<"$output"; then
    echo "commit-message policy did not report $expected" >&2
    echo "$output" >&2
    exit 1
  fi
}

git -C "$FIXTURE" commit --amend -q -m "Add candidate"
assert_rejected "has no body"

LONG_LINE="- $(printf '%073d' 0)"
git -C "$FIXTURE" commit --amend -q -m $'Add candidate\n\n'"$LONG_LINE"
assert_rejected "exceeds 72 characters"

git -C "$FIXTURE" commit --amend -q -m $'Add candidate\n\n- Explain the candidate\n\nCo-Authored-By: Bot <bot@example.com>'
assert_rejected "prohibited Co-Authored-By trailer"

echo "commit-message policy behavior passed"

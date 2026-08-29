#!/bin/sh
# Enforce Factory's project-owned commit body and trailer conventions.
set -eu

candidate=HEAD
base=${FLUENT_BASE_COMMIT:-}

if [ -z "$base" ]; then
  if ! git rev-parse --verify --quiet refs/heads/main >/dev/null; then
    echo "cannot determine commit-message policy base: refs/heads/main is missing" >&2
    exit 2
  fi
  base=$(git merge-base refs/heads/main "$candidate")
fi

if ! git merge-base --is-ancestor "$base" "$candidate"; then
  echo "commit-message policy base $base is not an ancestor of $candidate" >&2
  exit 2
fi

policy_start=$(
  git log --format='%H' --diff-filter=A "$candidate" -- \
    scripts/check-commit-message-policy.sh | tail -n 1
)
if [ -n "$policy_start" ] && git merge-base --is-ancestor "$base" "$policy_start"; then
  base=$(git rev-parse "$policy_start^")
fi

metadata=$(git log --format='%H%x1f%B%x1e' "$base..$candidate")
printf '%s\n' "$metadata" | awk '
  BEGIN { RS = "\036"; FS = "\037"; failed = 0 }
  NF >= 2 {
    commit = $1
    sub(/^\n+/, "", commit)
    message = $2
    sub(/^\n+/, "", message)
    count = split(message, lines, "\n")

    has_body = 0
    if (count >= 3 && lines[2] ~ /^[[:space:]]*$/) {
      for (i = 3; i <= count; i++) {
        if (lines[i] !~ /^[[:space:]]*$/) has_body = 1
      }
    }
    if (!has_body) {
      printf "commit %s has no body separated from its subject by a blank line\n", commit > "/dev/stderr"
      failed = 1
    }

    for (i = 2; i <= count; i++) {
      if (length(lines[i]) > 72) {
        printf "commit %s body line %d exceeds 72 characters\n", commit, i > "/dev/stderr"
        failed = 1
      }
      if (tolower(lines[i]) ~ /^[[:space:]]*co-authored-by:/) {
        printf "commit %s contains a prohibited Co-Authored-By trailer\n", commit > "/dev/stderr"
        failed = 1
      }
    }
  }
  END { exit failed }
'

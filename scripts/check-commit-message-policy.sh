#!/bin/sh
# Advise on Factory's commit presentation and enforce trailer provenance.
# fluent-policy: commit-message-presentation-advisory-v1
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
  BEGIN { RS = "\036"; FS = "\037"; blocked = 0 }
  NF >= 2 {
    commit = $1
    sub(/^\n+/, "", commit)
    message = $2
    sub(/^\n+/, "", message)
    count = split(message, lines, "\n")

    if (index(message, "\\n")) {
      printf "advice: commit-message contains literal \\n text (commit %s)\n", commit > "/dev/stderr"
    }

    has_body = 0
    if (count >= 3 && lines[2] ~ /^[[:space:]]*$/) {
      for (i = 3; i <= count; i++) {
        if (lines[i] !~ /^[[:space:]]*$/) has_body = 1
      }
    }
    if (!has_body) {
      printf "advice: commit-message has no body separated from its subject by a blank line (commit %s)\n", commit > "/dev/stderr"
    }

    for (i = 2; i <= count; i++) {
      if (length(lines[i]) > 72) {
        printf "advice: commit-message body line exceeds 72 characters (commit %s, line %d)\n", commit, i > "/dev/stderr"
      }
      if (tolower(lines[i]) ~ /^[[:space:]]*co-authored-by:/) {
        printf "commit %s contains a prohibited Co-Authored-By trailer\n", commit > "/dev/stderr"
        blocked = 1
      }
    }
  }
  END { exit blocked }
'

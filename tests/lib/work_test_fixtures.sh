#!/usr/bin/env bash
# Shared fixture helpers for work-model behavior tests.

# Seed minimal review-skill stubs for all five review roles.
# Usage: seed_review_skill_stubs <workspace_path>
seed_review_skill_stubs() {
  local workspace="$1"
  for role in documentation behaviors architecture skills tests; do
    mkdir -p "${workspace}/skills/review-${role}"
    printf 'stub\n' > "${workspace}/skills/review-${role}/SKILL.md"
  done
  local hook_path
  hook_path="$(git -C "$workspace" rev-parse --git-path hooks/commit-msg)"
  mkdir -p "$(dirname "$hook_path")"
  cat > "$hook_path" <<'HOOK'
#!/bin/sh
if ! sed -n '3,$p' "$1" | grep -q '[^[:space:]]'; then
  printf '\n- Describe the fixture change\n' >> "$1"
fi
HOOK
  chmod +x "$hook_path"
}

# Seed a minimal tester config so the tester runs clean (no error).
# Usage: seed_tester_config <workspace_path>
seed_tester_config() {
  local workspace="$1"
  mkdir -p "${workspace}/.fluent"
  printf 'commands:\n  - command: "true"\n    test_harness: shell-harness\n' \
    > "${workspace}/.fluent/tester.yaml"
  printf '#!/bin/sh\necho '"'"'[]'"'"'\n' > "${workspace}/.fluent/extract-tester-results"
  chmod +x "${workspace}/.fluent/extract-tester-results"
}

# Seed a minimal tester-results.json artifact so review tasks that
# depend on the tester can find their input artifact.
# Usage: seed_tester_results <project_root> <work_item_id> <attempt_id>
seed_tester_results() {
  local project_root="$1"
  local work_item_id="$2"
  local attempt_id="$3"
  local tester_id="${attempt_id}-tester"
  local dir="${project_root}/.fluent/work/artifacts/${work_item_id}/${attempt_id}/${tester_id}"
  mkdir -p "$dir"
  printf '[]\n' > "${dir}/tester-results.json"
}

# Install a provider-neutral helper for reviewer doubles. A passing mock review
# calls this from an EXIT trap so it emits the same candidate-bound artifact and
# provider transcript event that a real reviewer must produce.
# Usage: install_review_inspection_provider_helper <mock_bin_dir>
install_review_inspection_provider_helper() {
  local bin_dir="$1"
  cat > "${bin_dir}/fluent-test-review-inspection" <<'HELPER'
#!/usr/bin/env bash
set -euo pipefail

provider="$1"
original_status="$2"
shift 2

prompt=''
for arg in "$@"; do
  case "$arg" in
    *'"candidate-commit"'*) prompt="$arg" ;;
  esac
done
if [ -z "$prompt" ] || [ "$original_status" -ne 0 ] || [ ! -f review.md ] \
  || ! grep -qi '^Verdict: pass' review.md \
  || [ -f review-inspection.json ]; then
  exit 0
fi
candidate="$(printf '%s\n' "$prompt" | sed -n 's/^- Workspace: //p' | head -1)"
if [ -z "$candidate" ]; then
  candidate="$(printf '%s\n' "$prompt" | sed -n 's/^Review the codebase at \(.*\)\.$/\1/p' | head -1)"
fi
commit="$(printf '%s\n' "$prompt" | sed -n 's/^[[:space:]]*"candidate-commit": "\([^"]*\)",$/\1/p' | head -1)"
role="$(printf '%s\n' "$prompt" | sed -n 's/^[[:space:]]*"role": "\([^"]*\)",$/\1/p' | head -1)"
if ! inspected="$(git -C "$candidate" ls-tree -r --name-only "$commit" | head -1)"; then
  printf 'review fixture could not inspect candidate=%q commit=%q role=%q\n' \
    "$candidate" "$commit" "$role" >&2
  exit 65
fi
absolute="$candidate/$inspected"

jq -n \
  --arg commit "$commit" \
  --arg role "$role" \
  --arg path "$inspected" \
  '{"schema-version": 1, "candidate-commit": $commit, role: $role, inspections: [{path: $path, summary: "Synthetic reviewer inspected this tracked candidate file."}]}' \
  > review-inspection.json

case "$provider" in
  claude)
    jq -cn \
      --arg absolute "$absolute" \
      '{type: "assistant", message: {content: [{type: "tool_use", id: "mock-review-read", name: "Read", input: {file_path: $absolute}}]}}'
    jq -cn \
      '{type: "user", message: {content: [{type: "tool_result", tool_use_id: "mock-review-read", content: "synthetic source"}]}}'
    ;;
  codex)
    jq -cn \
      --arg absolute "$absolute" \
      '{type: "item.completed", item: {type: "command_execution", status: "completed", exit_code: 0, command: ("sed -n 1,40p " + $absolute)}}'
    ;;
  pi)
    jq -cn \
      --arg absolute "$absolute" \
      '{type: "tool_execution_start", toolCallId: "mock-review-read", toolName: "read", args: {path: $absolute}}'
    jq -cn \
      '{type: "tool_execution_end", toolCallId: "mock-review-read", toolName: "read", result: {}, isError: false}'
    ;;
  *)
    printf 'unknown review fixture provider: %s\n' "$provider" >&2
    exit 64
    ;;
esac
HELPER
  chmod +x "${bin_dir}/fluent-test-review-inspection"
}

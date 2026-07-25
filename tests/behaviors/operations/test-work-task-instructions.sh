#!/usr/bin/env bash
# test-work-task-instructions - Verify durable Work Task instructions.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$(dirname "$(dirname "$SCRIPT_DIR")")")"
FLUENT_BIN="${FLUENT_BIN_OVERRIDE:-${PROJECT_DIR}/target/debug/fluent}"

source "${PROJECT_DIR}/tests/lib/run_test.sh"
source "${PROJECT_DIR}/tests/lib/work_test_fixtures.sh"
LOG_DIR="${PROJECT_DIR}/tests/output/$(basename "$0" .sh)"

setup_test_project() {
  TEST_DIR="$(mktemp -d -t fluent-work-task-instructions-XXXXXX)"
  mkdir -p "$TEST_DIR/project" "$TEST_DIR/bin"
  cd "$TEST_DIR/project"
  git init -b main > /dev/null 2>&1
  git config commit.gpgsign false
  git config user.email "test@test"
  git config user.name "test"
  printf 'test\n' > README.md
  seed_review_skill_stubs "."
  seed_tester_config "."
  git add . && git commit -m "init" > /dev/null 2>&1
}

cleanup_test_project() {
  cd /
  if [ -d "${TEST_DIR}/project/.git" ]; then
    git -C "${TEST_DIR}/project" worktree list --porcelain 2>/dev/null | \
      awk '/^worktree / { print $2 }' | \
      grep -v "^${TEST_DIR}/project$" | while read -r wt; do
        git -C "${TEST_DIR}/project" worktree remove --force "$wt" 2>/dev/null || true
      done || true
  fi
  rm -rf "$TEST_DIR"
}

write_instructions_file() {
  cat > "$TEST_DIR/instructions.md" <<'INSTRUCTIONS'
Brief: build durable Work Task instructions.

Behaviors:
- Preserve coder flags as args.
- Keep prompt content in Work state.
INSTRUCTIONS
}

write_mock_codex() {
  cat > "${TEST_DIR}/bin/codex" <<'MOCK_SCRIPT'
#!/usr/bin/env bash
printf 'ARGV_BEGIN\n' >> "$CODER_ARGS_LOG"
for arg in "$@"; do
  printf 'ARG:%s\n' "$arg" >> "$CODER_ARGS_LOG"
  case "$arg" in
    *"Work Item:"*|*"Execute this Fluent review Task"*)
      printf '%s\n' "$arg" >> "$CODER_PROMPT_LOG"
      ;;
  esac
done

case "$PWD" in
  */work-6-work-1-attempt-1)
    count_file="${TASK_OUTPUT_COUNT_FILE}"
    count="$(cat "$count_file" 2>/dev/null || printf '0')"
    count="$((count + 1))"
    printf '%s\n' "$count" > "$count_file"
    printf 'task output %s\n' "$count" > "task-output-${count}.txt"
    git add "task-output-${count}.txt"
    git commit -m "Add task output ${count}" > /dev/null 2>&1
    ;;
  *)
    printf 'Verdict: pass\n\nMock review passed.\n' > review.md
    ;;
esac

printf '%s\n' '{"type":"result","subtype":"success","result":"done","session_id":"mock"}'
MOCK_SCRIPT
  chmod +x "${TEST_DIR}/bin/codex"
}

create_work_item_with_instructions() {
  write_instructions_file
  "$FLUENT_BIN" work-item create work-1 \
    --title "Instruction propagation" \
    --instructions-file "$TEST_DIR/instructions.md" > /dev/null
}

create_attempt() {
  "$FLUENT_BIN" attempt create work-1 attempt-1 > /dev/null
}

json_value() {
  jq -r "$1" .fluent/work/items/work-1.json
}

assert_contains() {
  # Match in-process so no producer-consumer pipeline exists to fail under
  # pipefail; quote the needle so it stays a literal while the surrounding
  # * remain Bash wildcards.
  if [[ "$1" != *"$2"* ]]; then
    printf '    FAIL: output does not contain "%s"\n' "$2"
    printf '    Output:\n%s\n' "$1"
    return 1
  fi
}

assert_not_contains() {
  if [[ "$1" == *"$2"* ]]; then
    printf '    FAIL: output unexpectedly contains "%s"\n' "$2"
    printf '    Output:\n%s\n' "$1"
    return 1
  fi
}

run_work_task_with_extra_args() {
  PATH="${TEST_DIR}/bin:$PATH" \
    CODER_ARGS_LOG="${TEST_DIR}/coder-args.log" \
    CODER_PROMPT_LOG="${TEST_DIR}/coder-prompt.log" \
    TASK_OUTPUT_COUNT_FILE="${TEST_DIR}/task-output-count" \
    "$FLUENT_BIN" task run --no-sandbox --coder codex \
      work-1 attempt-1 attempt-1-write-1 -- \
      --model test-model EXTRA_ARG_PROMPT_SENTINEL
}

run_work_attempt_with_extra_args() {
  PATH="${TEST_DIR}/bin:$PATH" \
    CODER_ARGS_LOG="${TEST_DIR}/coder-args.log" \
    CODER_PROMPT_LOG="${TEST_DIR}/coder-prompt.log" \
    TASK_OUTPUT_COUNT_FILE="${TEST_DIR}/task-output-count" \
    "$FLUENT_BIN" attempt run --no-sandbox --coder codex \
      work-1 attempt-1 -- --model test-model EXTRA_ARG_PROMPT_SENTINEL
}

test_create_persists_instructions_and_show_displays_them() {
  setup_test_project
  create_work_item_with_instructions

  RESULT=0
  "$FLUENT_BIN" work-item show work-1 > "$TEST_DIR/show.json"
  [ "$(jq -r '.instructions' "$TEST_DIR/show.json")" = "$(cat "$TEST_DIR/instructions.md")" ] || RESULT=1

  cleanup_test_project
  return $RESULT
}

test_attempt_copies_instructions_to_initial_write_task() {
  setup_test_project
  create_work_item_with_instructions
  create_attempt

  RESULT=0
  [ "$("$FLUENT_BIN" work-item show work-1 | jq -r '.attempts[0].tasks[0].instructions')" = "$(cat "$TEST_DIR/instructions.md")" ] || RESULT=1
  [ "$("$FLUENT_BIN" work-item show work-1 | jq -r '.attempts[0].tasks[0].id')" = "attempt-1-write-1" ] || RESULT=1

  cleanup_test_project
  return $RESULT
}

test_task_run_keeps_extra_args_out_of_prompt() {
  setup_test_project
  create_work_item_with_instructions
  create_attempt
  write_mock_codex

  RESULT=0
  run_work_task_with_extra_args > "$TEST_DIR/stdout" 2> "$TEST_DIR/stderr" || RESULT=1

  ARGS="$(cat "$TEST_DIR/coder-args.log")"
  PROMPT="$(cat "$TEST_DIR/coder-prompt.log")"
  assert_contains "$ARGS" "ARG:--model" || RESULT=1
  assert_contains "$ARGS" "ARG:test-model" || RESULT=1
  assert_contains "$ARGS" "ARG:EXTRA_ARG_PROMPT_SENTINEL" || RESULT=1
  assert_not_contains "$PROMPT" "EXTRA_ARG_PROMPT_SENTINEL" || RESULT=1

  cleanup_test_project
  return $RESULT
}

test_attempt_run_keeps_extra_args_out_of_prompt() {
  setup_test_project
  create_work_item_with_instructions
  create_attempt
  write_mock_codex

  RESULT=0
  run_work_attempt_with_extra_args > "$TEST_DIR/stdout" 2> "$TEST_DIR/stderr" || RESULT=1

  ARGS="$(cat "$TEST_DIR/coder-args.log")"
  PROMPTS="$(cat "$TEST_DIR/coder-prompt.log")"
  assert_contains "$ARGS" "ARG:--model" || RESULT=1
  assert_contains "$ARGS" "ARG:test-model" || RESULT=1
  assert_contains "$ARGS" "ARG:EXTRA_ARG_PROMPT_SENTINEL" || RESULT=1
  assert_not_contains "$PROMPTS" "EXTRA_ARG_PROMPT_SENTINEL" || RESULT=1

  cleanup_test_project
  return $RESULT
}

test_minimal_work_item_keeps_minimal_prompt() {
  setup_test_project
  "$FLUENT_BIN" work-item create work-1 --title "Minimal prompt" > /dev/null
  create_attempt
  write_mock_codex

  RESULT=0
  PATH="${TEST_DIR}/bin:$PATH" \
    CODER_ARGS_LOG="${TEST_DIR}/coder-args.log" \
    CODER_PROMPT_LOG="${TEST_DIR}/coder-prompt.log" \
    TASK_OUTPUT_COUNT_FILE="${TEST_DIR}/task-output-count" \
    "$FLUENT_BIN" task run --no-sandbox --coder codex \
      work-1 attempt-1 attempt-1-write-1 > "$TEST_DIR/stdout" 2> "$TEST_DIR/stderr" || RESULT=1

  PROMPT="$(cat "$TEST_DIR/coder-prompt.log")"
  assert_contains "$PROMPT" "Work Item: work-1 - Minimal prompt" || RESULT=1
  assert_contains "$PROMPT" "Fluent Writer" || RESULT=1
  assert_not_contains "$PROMPT" "Task instructions:" || RESULT=1
  [ "$(json_value '.instructions // "missing"')" = "missing" ] || RESULT=1
  [ "$(json_value '.attempts[0].tasks[0].instructions // "missing"')" = "missing" ] || RESULT=1

  cleanup_test_project
  return $RESULT
}

test_literal_containment_helpers_handle_large_multiline_output() {
  needle="EARLY_LINE_LITERAL_SENTINEL"
  # First complete line carries the unique literal; the trailing content is
  # deterministic and exceeds 1 MiB. With the old grep pipeline the matcher
  # exits on the early line and printf then dies with SIGPIPE (PIPESTATUS
  # 141 0) under pipefail, so containment must not depend on that pipeline.
  filler=$'trailing deterministic content line\n'
  while [ "${#filler}" -lt 1048576 ]; do
    filler="${filler}${filler}"
  done
  haystack="prefix ${needle} suffix"$'\n'"${filler}"

  RESULT=0

  # The literal is present on an early line, so containment must succeed.
  assert_contains "$haystack" "$needle" || RESULT=1

  # The same present literal must make the inverse helper fail. Suppress the
  # expected diagnostic so it does not dump the multi-megabyte haystack.
  if assert_not_contains "$haystack" "$needle" > /dev/null 2>&1; then
    printf '    FAIL: assert_not_contains missed a present literal in long output\n'
    RESULT=1
  fi

  # A literal that never appears must let the inverse helper succeed.
  assert_not_contains "$haystack" "ABSENT_LITERAL_NEVER_PRESENT" || RESULT=1

  return $RESULT
}

test_literal_containment_helpers_treat_pattern_metacharacters_literally() {
  # a*b is a Bash pattern only if the needle expansion is unquoted; quoted, it
  # is the literal three-character string, which never occurs in the haystack.
  haystack="prefix aZZb suffix"

  RESULT=0

  # The literal a*b is absent, so containment must fail. Suppress the expected
  # diagnostic from the positive helper.
  if assert_contains "$haystack" "a*b" > /dev/null 2>&1; then
    printf '    FAIL: assert_contains treated "a*b" as a wildcard pattern\n'
    RESULT=1
  fi

  # The inverse helper must succeed for the same absent literal.
  assert_not_contains "$haystack" "a*b" || RESULT=1

  return $RESULT
}

printf 'test-work-task-instructions\n\n'

run_test "work create persists and shows instructions" \
  test_create_persists_instructions_and_show_displays_them
run_test "attempt copies instructions to initial write Task" \
  test_attempt_copies_instructions_to_initial_write_task
run_test "task run keeps extra args out of prompt" \
  test_task_run_keeps_extra_args_out_of_prompt
run_test "attempt run keeps extra args out of prompt" \
  test_attempt_run_keeps_extra_args_out_of_prompt
run_test "minimal Work Item keeps minimal prompt" \
  test_minimal_work_item_keeps_minimal_prompt
run_test "literal containment helpers handle large multiline output" \
  test_literal_containment_helpers_handle_large_multiline_output
run_test "literal containment helpers treat pattern metacharacters literally" \
  test_literal_containment_helpers_treat_pattern_metacharacters_literally

summarize_and_exit

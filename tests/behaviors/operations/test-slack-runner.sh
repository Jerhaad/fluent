#!/usr/bin/env bash
# Verify the local Slack runner's controllable service lifecycle and generated
# LaunchAgent command.
#
# Covers: enable installs controllable login service.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$(dirname "$(dirname "$SCRIPT_DIR")")")"
FLUENT_BIN="${FLUENT_BIN_OVERRIDE:-${PROJECT_DIR}/target/debug/fluent}"
export FLUENT_NO_UPDATE_CHECK=1

source "${PROJECT_DIR}/tests/lib/run_test.sh"
LOG_DIR="${PROJECT_DIR}/tests/output/$(basename "$0" .sh)"

test_runner_owner_controls_login_service() {
  local test_dir home fake_bin launchctl_log output
  test_dir="$(mktemp -d -t fluent-slack-runner-XXXXXX)"
  home="$test_dir/home"
  fake_bin="$test_dir/bin"
  launchctl_log="$test_dir/launchctl.log"
  mkdir -p "$home/.config/fluent/slack" "$fake_bin"
  trap 'rm -rf "$test_dir"' RETURN

  cat > "$fake_bin/launchctl" <<'SCRIPT'
#!/bin/sh
printf '%s\n' "$*" >> "$FLUENT_TEST_LAUNCHCTL_LOG"
SCRIPT
  chmod +x "$fake_bin/launchctl"
  printf '%s\n' '{"workspace":"test-workspace","projects":{},"channel_defaults":{}}' \
    > "$home/.config/fluent/slack/config.json"
  printf '%s\n' '{"connected":true,"last_successful_connection":"2026-08-18T12:00:00Z","failure":null}' \
    > "$home/.config/fluent/slack/status.json"

  HOME="$home" PATH="$fake_bin:$PATH" FLUENT_TEST_LAUNCHCTL_LOG="$launchctl_log" \
    "$FLUENT_BIN" slack enable
  output="$(HOME="$home" "$FLUENT_BIN" slack status)"
  printf '%s' "$output" | grep -F "workspace: test-workspace"
  printf '%s' "$output" | grep -F "connection: connected"
  HOME="$home" PATH="$fake_bin:$PATH" FLUENT_TEST_LAUNCHCTL_LOG="$launchctl_log" \
    "$FLUENT_BIN" slack restart
  HOME="$home" PATH="$fake_bin:$PATH" FLUENT_TEST_LAUNCHCTL_LOG="$launchctl_log" \
    "$FLUENT_BIN" slack stop

  test -f "$home/Library/LaunchAgents/dev.fluent.slack-runner.plist"
  grep -F "bootstrap " "$launchctl_log"
  grep -F "bootout " "$launchctl_log"
  grep -F "$FLUENT_BIN" "$home/Library/LaunchAgents/dev.fluent.slack-runner.plist"
  grep -F '<key>RunAtLoad</key><true/>' \
    "$home/Library/LaunchAgents/dev.fluent.slack-runner.plist"
}

run_test "runner owner controls login service" test_runner_owner_controls_login_service
summarize_and_exit

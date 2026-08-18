#!/usr/bin/env bash
# Verify the local Slack runner's controllable service lifecycle and generated
# LaunchAgent command.
#
# Covers: enable installs controllable login service.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$(dirname "$(dirname "$SCRIPT_DIR")")")"

source "${PROJECT_DIR}/tests/lib/run_test.sh"
LOG_DIR="${PROJECT_DIR}/tests/output/$(basename "$0" .sh)"

test_enable_installs_controllable_login_service() {
  cd "$PROJECT_DIR"
  cargo test --lib slack_collaboration::tests::enable_installs_controllable_login_service
  cargo test --lib slack_collaboration::tests::launch_agent_installs_the_runner_command
  cargo run --quiet -- slack run --help
}

run_test "enable installs controllable login service" test_enable_installs_controllable_login_service

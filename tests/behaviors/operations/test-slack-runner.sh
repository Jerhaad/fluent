#!/usr/bin/env bash
# Verify the local Slack runner's controllable service lifecycle through its
# deterministic ServiceManager boundary.
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
}

run_test "enable installs controllable login service" test_enable_installs_controllable_login_service

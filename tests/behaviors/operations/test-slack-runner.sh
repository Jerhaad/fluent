#!/usr/bin/env bash
# Verify the local Slack runner's controllable service lifecycle through its
# deterministic ServiceManager boundary.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$(dirname "$(dirname "$SCRIPT_DIR")")")"

cd "$PROJECT_DIR"
cargo test --lib slack_collaboration::tests::enable_installs_controllable_login_service

#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$(dirname "$(dirname "$SCRIPT_DIR")")")"
SOURCE_DIR="${PROJECT_DIR}/documentation/readme-diagrams/sources"
ASSET_DIR="${PROJECT_DIR}/.github/assets"
RENDERER="${PROJECT_DIR}/scripts/render-readme-diagrams.py"
README="${PROJECT_DIR}/README.md"

source "${PROJECT_DIR}/tests/lib/run_test.sh"
LOG_DIR="${PROJECT_DIR}/tests/output/$(basename "$0" .sh)"

DIAGRAMS="
fluent-at-a-glance
fluent-overall-flow
how-you-tell-fluent
how-fluent-builds
how-fluent-improves
how-fluent-learns
"

test_tracks_each_light_and_dark_source() {
  local diagram
  local theme
  local source

  for diagram in $DIAGRAMS; do
    for theme in light dark; do
      source="${SOURCE_DIR}/${diagram}-${theme}.svg"
      if [ ! -s "$source" ]; then
        printf '    FAIL: tracked source is missing or empty: %s\n' "$source"
        return 1
      fi
    done
  done

  local source_count
  source_count="$(find "$SOURCE_DIR" -maxdepth 1 -type f -name '*.svg' | wc -l | tr -d ' ')"
  if [ "$source_count" -ne 12 ]; then
    printf '    FAIL: expected six light/dark source pairs, found %s SVG files\n' "$source_count"
    return 1
  fi
}

test_tracks_each_output_and_readme_reference() {
  if [ ! -x "$RENDERER" ]; then
    printf '    FAIL: renderer is not executable: %s\n' "$RENDERER"
    return 1
  fi

  local diagram
  local extension
  local theme
  local asset

  for diagram in $DIAGRAMS; do
    extension="png"
    if [ "$diagram" = "fluent-at-a-glance" ]; then
      extension="gif"
    fi

    for theme in light dark; do
      asset=".github/assets/${diagram}-${theme}.${extension}"
      if [ ! -s "${PROJECT_DIR}/${asset}" ]; then
        printf '    FAIL: generated README asset is missing or empty: %s\n' "$asset"
        return 1
      fi
      if ! grep -Fq "$asset" "$README"; then
        printf '    FAIL: README does not reference generated asset: %s\n' "$asset"
        return 1
      fi
    done
  done

  for theme in light dark; do
    asset="${ASSET_DIR}/fluent-at-a-glance-${theme}.png"
    if [ ! -s "$asset" ]; then
      printf '    FAIL: generated static hero is missing or empty: %s\n' "$asset"
      return 1
    fi
  done
}

test_rejects_animation_tracks_that_break_the_loop() {
  python3 - "$RENDERER" <<'PY'
import importlib.util
import sys

renderer_path = sys.argv[1]
spec = importlib.util.spec_from_file_location("readme_diagram_renderer", renderer_path)
renderer = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = renderer
spec.loader.exec_module(renderer)

points = ((0.0, 0.0), (1.0, 1.0))
for cycles in (0, -1, 1.5, True):
    try:
        renderer.validate_tracks((renderer.Track(points, cycles=cycles),))
    except SystemExit as error:
        if "positive whole number" not in str(error):
            raise AssertionError(f"unexpected validation error: {error}") from error
    else:
        raise AssertionError(f"accepted invalid animation cycle count: {cycles!r}")

renderer.validate_tracks((renderer.Track(points, cycles=2),))
PY
}

test_generated_assets_match_sources() {
  local missing_tools=""
  local tool

  for tool in python3 rsvg-convert ffmpeg gifsicle fc-match; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      missing_tools="${missing_tools} ${tool}"
    fi
  done

  if [ -n "$missing_tools" ]; then
    printf '    SKIP: render check requires:%s\n' "$missing_tools"
    return 0
  fi

  "$RENDERER" --check
}

printf 'test-readme-diagrams\n\n'

run_test "tracks each light and dark source" test_tracks_each_light_and_dark_source
run_test "tracks each output and README reference" test_tracks_each_output_and_readme_reference
run_test "rejects animation tracks that break the loop" test_rejects_animation_tracks_that_break_the_loop
run_test "generated assets match their sources" test_generated_assets_match_sources

summarize_and_exit

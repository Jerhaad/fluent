#!/usr/bin/env bash
# Build the v0.3.0 release source with the Attempt's admitted dependencies.
set -euo pipefail
repository="$(git rev-parse --show-toplevel)"
# Release preparation 47394c84 followed by version-fixture corrections only.
# src/, Cargo.toml, and Cargo.lock are identical at these two commits.
source_commit=2e88c3cbc811a83d733106ded22d29d716cf7184
reader_root="$repository/.scratch/work-model-v0.3.0-reader"
mkdir -p "$reader_root/source"
git -C "$repository" archive "$source_commit" | tar -x -C "$reader_root/source"
# Do not inherit this candidate's Git identity from the archive's parent.
GIT_CEILING_DIRECTORIES="$repository" cargo build --offline --locked \
    --manifest-path "$reader_root/source/Cargo.toml" --target-dir "$reader_root/target" >&2
python3 - "$reader_root" "$source_commit" <<'IDENTITY'
import hashlib, json, sys
from pathlib import Path
root = Path(sys.argv[1])
binary = root / 'target/debug/fluent'
source = root / 'source/src/work_model.rs'
identity = {
    'source_commit': sys.argv[2],
    'work_model_sha256': hashlib.sha256(source.read_bytes()).hexdigest(),
    'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
}
(root / 'identity.json').write_text(json.dumps(identity, indent=2) + '\n')
print(binary)
IDENTITY

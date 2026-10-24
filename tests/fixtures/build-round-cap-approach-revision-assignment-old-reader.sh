#!/usr/bin/env bash
# Build the temporary assignment-aware coordinator before this support change.
set -euo pipefail

repository="$(git rev-parse --show-toplevel)"
source_commit=5c670fdd9eec7eda22e93aee4ea619f1c60f7a70
reader_root="$repository/.scratch/round-cap-approach-revision-assignment-reader/$source_commit"
mkdir -p "$reader_root/source"
git -C "$repository" archive "$source_commit" | tar -x -C "$reader_root/source"

GIT_CEILING_DIRECTORIES="$repository" cargo build --locked --offline \
    --manifest-path "$reader_root/source/Cargo.toml" \
    --target-dir "$reader_root/target" >&2

python3 - "$reader_root" "$source_commit" <<'IDENTITY'
import hashlib
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
binary = root / "target/debug/fluent"
source = root / "source/src/work_model.rs"
identity = {
    "source_commit": sys.argv[2],
    "work_model_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
    "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
}
(root / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
print(binary)
IDENTITY

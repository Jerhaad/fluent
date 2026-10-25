#!/usr/bin/env bash
# Build stable main before round-cap Approach revision support.
set -euo pipefail

repository="$(git rev-parse --show-toplevel)"
source_commit=bd59cf35308bf30fa5cbabc05ff5cf69cebfff5a
reader_root="$repository/.scratch/round-cap-approach-revision-stable-reader/$source_commit"
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

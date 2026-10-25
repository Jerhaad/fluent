#!/bin/bash
# Build the exact pre-assignment Fluent reader from the approved main revision.
set -euo pipefail

repository="$(git rev-parse --show-toplevel)"
source_commit=bd59cf35308bf30fa5cbabc05ff5cf69cebfff5a
reader_root="$repository/.scratch/writer-assignment-main-reader-$source_commit"
mkdir -p "$reader_root"
if [[ ! -f "$reader_root/source-commit" ]]; then
    git -C "$repository" archive "$source_commit" | tar -x -C "$reader_root"
    printf '%s\n' "$source_commit" > "$reader_root/source-commit"
fi

GIT_CEILING_DIRECTORIES="$repository" cargo build --locked \
    --manifest-path "$reader_root/Cargo.toml" \
    --target-dir "$reader_root/target" >&2

python3 - "$reader_root" "$source_commit" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
source_commit = sys.argv[2]
binary = root / "target/debug/fluent"
work_model = root / "src/work_model.rs"
(root / "identity.json").write_text(json.dumps({
    "source_commit": source_commit,
    "work_model_sha256": hashlib.sha256(work_model.read_bytes()).hexdigest(),
    "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
}, indent=2) + "\n")
print(binary)
PY

#!/bin/bash
# Build the pre-handoff Fluent reader from the approved revision.
set -euo pipefail

repository="$(git rev-parse --show-toplevel)"
source_commit=b5d8bf02b016a3977a08fc713e47f525b895aed0
reader_root="$repository/.scratch/carried-finding-old-reader-$source_commit"
mkdir -p "$reader_root"
if [[ ! -f "$reader_root/source-commit" ]]; then
    git -C "$repository" archive "$source_commit" | tar -x -C "$reader_root"
    printf '%s\n' "$source_commit" > "$reader_root/source-commit"
fi

test "$(cat "$reader_root/source-commit")" = "$source_commit"
for source_file in Cargo.lock Cargo.toml src/work_model.rs; do
    git -C "$repository" show "$source_commit:$source_file" | cmp - "$reader_root/$source_file"
done

GIT_CEILING_DIRECTORIES="$repository" cargo build --offline --locked \
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

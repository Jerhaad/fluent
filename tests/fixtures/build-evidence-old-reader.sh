#!/bin/bash
# Build the exact reader preceding canonical required-check authority.
set -euo pipefail
repository=$(git rev-parse --show-toplevel)
source_commit=720368bb855dd6d10fffd221ed0e4badd0b6104e
reader_root="$repository/.scratch/evidence-old-reader"
mkdir -p "$reader_root"
if [[ ! -f "$reader_root/source-commit" ]]; then
    git -C "$repository" archive "$source_commit" | tar -x -C "$reader_root"
    echo "$source_commit" > "$reader_root/source-commit"
fi
[[ "$(cat "$reader_root/source-commit")" == "$source_commit" ]]
cargo build --offline --locked --manifest-path "$reader_root/Cargo.toml" --target-dir "$reader_root/target" >&2
python3 - "$reader_root" "$source_commit" <<'PY'
import hashlib, json, sys
from pathlib import Path
root = Path(sys.argv[1])
binary = root / 'target/debug/fluent'
(root / 'identity.json').write_text(json.dumps({'source_commit': sys.argv[2], 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}))
print(binary)
PY

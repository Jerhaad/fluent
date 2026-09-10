#!/bin/bash
# Build the reader immediately before structured verification meaning.
set -euo pipefail
repository=$(git rev-parse --show-toplevel)
source_commit=0b769cb4681aade324f0cee351832a94f8e72687
reader_root="$repository/.scratch/verification-old-reader/$source_commit"
mkdir -p "$reader_root"
if [[ ! -f "$reader_root/source-commit" ]]; then
    git -C "$repository" archive "$source_commit" | tar -x -C "$reader_root"
    echo "$source_commit" > "$reader_root/source-commit"
fi
if ! cargo build --locked --offline --manifest-path "$reader_root/Cargo.toml" --target-dir "$reader_root/target" > "$reader_root/build.log" 2>&1; then
    cat "$reader_root/build.log" >&2
    exit 1
fi
python3 - "$reader_root" "$source_commit" <<'PY'
import hashlib, json, sys
from pathlib import Path
root = Path(sys.argv[1])
binary = root / 'target/debug/fluent'
(root / 'identity.json').write_text(json.dumps({
    'source_commit': sys.argv[2],
    'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
}))
print(binary)
PY

#!/bin/bash
# Build the last reader before responsibility fields from its exact source.
set -euo pipefail
repository=$(git rev-parse --show-toplevel)
source_commit=f71d20bb11f241c9ab9ba38b401f22e692d9e19f
reader_root="$repository/.scratch/ownership-old-reader"
mkdir -p "$reader_root"
if [[ ! -f "$reader_root/source-commit" ]] || [[ "$(cat "$reader_root/source-commit")" != "$source_commit" ]]; then
    git -C "$repository" archive "$source_commit" | tar -x -C "$reader_root"
    echo "$source_commit" > "$reader_root/source-commit"
fi
cargo build --offline --manifest-path "$reader_root/Cargo.toml" >&2
python3 - "$reader_root" "$source_commit" <<'PY'
import hashlib,json,sys
from pathlib import Path
root=Path(sys.argv[1]); binary=root/'target/debug/fluent'
(root/'identity.json').write_text(json.dumps({'source_commit':sys.argv[2], 'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()}))
print(binary)
PY

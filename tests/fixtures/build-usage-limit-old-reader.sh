#!/bin/bash
# Prepare the exact coordinator source that predates usage-limit authority.
set -euo pipefail
repository="$(git rev-parse --show-toplevel)"
source_commit=5c670fdd9eec7eda22e93aee4ea619f1c60f7a70
reader_root="$repository/.scratch/usage-limit-reader-$source_commit"
mkdir -p "$reader_root"
source_dir="$(mktemp -d "$reader_root/source.XXXXXX")"
trap 'rm -rf "$source_dir"' EXIT
git -C "$repository" archive "$source_commit" > "$reader_root/source.tar"
tar -xf "$reader_root/source.tar" -C "$source_dir"
GIT_CEILING_DIRECTORIES="$repository" cargo build --offline --locked \
    --manifest-path "$source_dir/Cargo.toml" --target-dir "$reader_root/target" >&2
python3 - "$reader_root" "$source_commit" "$source_dir" <<'PY'
import hashlib, json, sys
from pathlib import Path
root, source_commit, source = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
binary = root / 'target/debug/fluent'
(root / 'identity.json').write_text(json.dumps({
    'source_commit': source_commit,
    'source_archive_sha256': hashlib.sha256((root / 'source.tar').read_bytes()).hexdigest(),
    'work_model_sha256': hashlib.sha256((source / 'src/work_model.rs').read_bytes()).hexdigest(),
    'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
}, indent=2) + '\n')
print(binary)
PY

#!/bin/bash
set -euo pipefail
repository="$(git rev-parse --show-toplevel)"
source_commit=dfa960cd3bb7ba2087ab8562c728755e75d37f04
reader_root="$repository/.scratch/landing-recovery-old-reader-$source_commit"
mkdir -p "$reader_root"
source_dir="$reader_root/source"
git -C "$repository" cat-file -e "$source_commit^{commit}"
git -C "$repository" archive "$source_commit" > "$reader_root/source.tar"
rm -rf -- "$source_dir"
mkdir -p "$source_dir"
tar -xf "$reader_root/source.tar" -C "$source_dir"
GIT_CEILING_DIRECTORIES="$repository" cargo build --locked \
    --manifest-path "$source_dir/Cargo.toml" --target-dir "$reader_root/stable-target" >&2
python3 - "$reader_root" "$source_commit" "$source_dir" <<'PY'
import hashlib, json, sys
from pathlib import Path
root, source_commit, source = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
binary = root / 'stable-target/debug/fluent'
digest = hashlib.sha256(binary.read_bytes()).hexdigest()
(root / 'identity.json').write_text(json.dumps({
    'source_commit': source_commit,
    'source_archive_sha256': hashlib.sha256((root / 'source.tar').read_bytes()).hexdigest(),
    'work_model_sha256': hashlib.sha256((source / 'src/work_model.rs').read_bytes()).hexdigest(),
    'binary_sha256': digest,
}, indent=2) + '\n')
print(binary)
PY

#!/bin/bash
set -euo pipefail
repository="$(git rev-parse --show-toplevel)"
source_commit=dfa960cd3bb7ba2087ab8562c728755e75d37f04
reader_root="$repository/.scratch/landing-recovery-old-reader-$source_commit"
mkdir -p "$reader_root"
source_dir="$reader_root/source"
if [ ! -f "$source_dir/Cargo.toml" ]; then
    mkdir -p "$source_dir"
    git -C "$repository" archive "$source_commit" > "$reader_root/source.tar"
    tar -xf "$reader_root/source.tar" -C "$source_dir"
fi
GIT_CEILING_DIRECTORIES="$repository" cargo build --locked \
    --manifest-path "$source_dir/Cargo.toml" --target-dir "$reader_root/stable-target" >&2
python3 - "$reader_root" "$source_commit" <<'PY'
import hashlib, json, sys
from pathlib import Path
root, source_commit = Path(sys.argv[1]), sys.argv[2]
binary = root / 'stable-target/debug/fluent'
digest = hashlib.sha256(binary.read_bytes()).hexdigest()
expected = '909fd3f12d6ff6ed1cc26b31c55c320022ab8725cc54fbaf5dac8f306ae18cd3'
if digest != expected:
    raise SystemExit(f'pinned landing predecessor executable changed: {digest}')
(root / 'identity.json').write_text(json.dumps({
    'source_commit': source_commit,
    'binary_sha256': digest,
}, indent=2) + '\n')
print(binary)
PY

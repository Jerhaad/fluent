#!/bin/bash
# Prove that the pre-change executable cannot discard required-check authority.
set -euo pipefail
repository=$(git rev-parse --show-toplevel)
current_bin="${FLUENT_BIN_OVERRIDE:-$repository/target/debug/fluent}"
old_bin=$(bash "$repository/tests/fixtures/build-evidence-old-reader.sh")
export LOG_DIR="$repository/tests/output/test-evidence-recovery-old-reader"
source "$repository/tests/lib/run_test.sh"
test_evidence_fence() {
python3 - "$current_bin" "$old_bin" <<'PY'
import hashlib, json, shutil, subprocess, sys, tempfile
from pathlib import Path
current, old = map(Path, sys.argv[1:])
identity = json.loads((old.parents[2] / 'identity.json').read_text())
assert identity['source_commit'] == '720368bb855dd6d10fffd221ed0e4badd0b6104e'
assert identity['binary_sha256'] == hashlib.sha256(old.read_bytes()).hexdigest()
print(json.dumps(identity, sort_keys=True))
def run(binary, root, *args):
    return subprocess.run([binary, *args], cwd=root, capture_output=True, text=True, timeout=30)
def success(result):
    assert result.returncode == 0, result.stdout + result.stderr
def snapshot(root):
    return [(str(p.relative_to(root)), 'directory' if p.is_dir() else hashlib.sha256(p.read_bytes()).hexdigest()) for p in sorted(root.rglob('*'))]
with tempfile.TemporaryDirectory(prefix='fluent-evidence-fence-') as temporary:
    root = Path(temporary) / 'project'
    root.mkdir()
    success(run(current, root, 'work-item', 'create', 'evidence-fence', '--title', 'Preserve evidence'))
    success(run(current, root, 'attempt', 'create', 'evidence-fence', 'attempt-1'))
    work = root / '.fluent/work'
    item_path = work / 'items/evidence-fence.json'
    attempt_path = work / 'attempts/evidence-fence/attempt-1.json'
    item = json.loads(item_path.read_text())
    assert item['schema_version'] == 3
    # Prove the pinned executable can execute this mutation on its own format.
    control = Path(temporary) / 'control'
    control.mkdir()
    success(run(old, control, 'work-item', 'create', 'legacy', '--title', 'Control'))
    success(run(old, control, 'work-item', 'abandon', 'legacy', '--reason', 'positive control'))
    for pending in (False, True):
        if pending:
            # Preserve the public aggregate as a pending transaction fixture.
            # The actual old executable must refuse its version before recovery.
            shown = run(current, root, 'work-item', 'show', 'evidence-fence')
            success(shown)
            aggregate = json.loads(shown.stdout)
            aggregate['title'] = 'Pending evidence transition'
            journal = work / 'transactions/evidence-fence.json'
            journal.parent.mkdir(exist_ok=True)
            journal.write_text(json.dumps({
                'schema_version': 2, 'id': 'evidence-fence',
                'base_revision': item['storage_revision'],
                'target_revision': item['storage_revision'] + 1,
                'work_item': aggregate,
            }))
        for command in [('work-item', 'abandon', 'evidence-fence', '--reason', 'must refuse'), ('work-item', 'upgrade', 'evidence-fence')]:
            before = snapshot(work)
            refused = run(old, root, *command)
            assert refused.returncode != 0, refused.stdout + refused.stderr
            assert 'schema' in refused.stderr or 'transaction' in refused.stderr, refused.stderr
            assert snapshot(work) == before, f'older mutation changed evidence state (pending={pending})'
        print(f'pending={pending}: old public mutations refused with unchanged bytes and entries')
PY
}
run_test 'Pre-change reader refuses current evidence authority and journals' test_evidence_fence
summarize_and_exit

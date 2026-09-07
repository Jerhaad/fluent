#!/usr/bin/env bash
# Verify the v0.3.0 executable refuses fenced Work before changing stored state.
set -euo pipefail
repository="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$repository"
current_bin="${FLUENT_BIN_OVERRIDE:-$repository/target/debug/fluent}"
old_bin="$(bash tests/fixtures/build-work-model-old-reader.sh)"
export LOG_DIR="$repository/tests/output/test-work-model-old-reader-fence"
# shellcheck source=tests/lib/run_test.sh
source "$repository/tests/lib/run_test.sh"

test_old_reader_fence() {
python3 - "$repository" "$current_bin" "$old_bin" <<'PY'
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

repository, current, old = map(Path, sys.argv[1:])
source_commit = '2e88c3cbc811a83d733106ded22d29d716cf7184'
reader_root = old.parent.parent.parent
identity = json.loads((reader_root / 'identity.json').read_text())
source = subprocess.check_output(['git', 'show', f'{source_commit}:src/work_model.rs'], cwd=repository)
assert identity['source_commit'] == source_commit
assert identity['work_model_sha256'] == hashlib.sha256(source).hexdigest()
assert identity['binary_sha256'] == hashlib.sha256(old.read_bytes()).hexdigest()
# Pin the real release deserializer, including its required id/title and optional
# string writer field, rather than substituting a test-only rejecting decoder.
record = source.decode().split('struct WorkItemRecord {', 1)[1].split('\n}', 1)[0]
assert 'id: String,' in record and 'title: String,' in record
assert 'model_writer_version: Option<String>,' in record
assert 'schema_version:' not in record
assert subprocess.check_output([old, 'version'], text=True).startswith('fluent 0.3.0')

def run(binary, root, *args):
    result = subprocess.run([binary, *args], cwd=root, text=True, capture_output=True, timeout=30)
    return result

def require_success(result):
    assert result.returncode == 0, result.stdout + result.stderr

def digest(root):
    entries = []
    for path in sorted(root.rglob('*')):
        relative = str(path.relative_to(root))
        if path.is_symlink():
            entries.append((relative, 'symlink', str(path.readlink())))
        elif path.is_dir():
            entries.append((relative, 'directory'))
        else:
            entries.append((relative, 'file', hashlib.sha256(path.read_bytes()).hexdigest()))
    return hashlib.sha256(json.dumps(entries).encode()).hexdigest()

with tempfile.TemporaryDirectory(prefix='fluent-old-reader-fence-') as temporary:
    root = Path(temporary) / 'project'
    root.mkdir()
    subprocess.run(['git', 'init', '-b', 'main'], cwd=root, check=True, capture_output=True)
    require_success(run(current, root, 'work-item', 'create', 'wire-fence', '--title', 'Preserve current Work'))
    require_success(run(current, root, 'attempt', 'create', 'wire-fence', 'attempt-1'))
    work = root / '.fluent/work'
    item_path = work / 'items/wire-fence.json'
    item = json.loads(item_path.read_text())
    attempt_path = work / 'attempts/wire-fence/attempt-1.json'
    attempt = json.loads(attempt_path.read_text())
    task_path = work / 'tasks/wire-fence/attempt-1/attempt-1-write-1.json'
    task = json.loads(task_path.read_text())
    workspace = task['workspace_access']['writes'][0]
    task.update(status='complete', output={
        'workspace_id': workspace['id'], 'workspace_path': workspace['path'],
        'source_branch': 'main', 'commit': 'abc123',
    })
    attempt.update(status='complete', review_state='passed')
    task_path.write_text(json.dumps(task, indent=2))
    attempt_path.write_text(json.dumps(attempt, indent=2))
    candidate = {
        'id': 'attempt-1-merge-candidate', 'attempt_id': 'attempt-1',
        'source_workspace': workspace, 'target_workspace': {'id': 'target', 'path': '.'},
        'source_branch': 'main', 'target_branch': 'main', 'candidate_commit': 'abc123',
        'merge_review_state': 'pending',
    }
    candidate_path = work / 'merge-candidates/wire-fence/attempt-1-merge-candidate.json'
    candidate_path.parent.mkdir(parents=True, exist_ok=True)
    candidate_path.write_text(json.dumps(candidate, indent=2))
    artifact = root / task['artifact_area']['path'] / 'transcript.jsonl'
    artifact.parent.mkdir(parents=True, exist_ok=True)
    artifact.write_bytes(b'preserve exact artifact bytes\n')
    require_success(run(current, root, 'work-item', 'upgrade', 'wire-fence'))

    # A positive control proves this executable can reach and perform the same
    # mutation on the legacy wire representation of this complete fixture.
    legacy = Path(temporary) / 'legacy'
    shutil.copytree(root, legacy)
    legacy_item_path = legacy / '.fluent/work/items/wire-fence.json'
    legacy_item = json.loads(legacy_item_path.read_text())
    legacy_item['model_writer_version'] = '0.3.0'
    legacy_item_path.write_text(json.dumps(legacy_item, indent=2))
    before = digest(legacy / '.fluent/work')
    require_success(run(old, legacy, 'work-item', 'abandon', 'wire-fence', '--reason', 'legacy control'))
    assert digest(legacy / '.fluent/work') != before

    for pending in (False, True):
        if pending:
            aggregate = dict(item)
            aggregate['model_writer_version'] = '0.3.0'
            aggregate['title'] = 'Unpublished journal title'
            aggregate['attempts'] = [dict(attempt, tasks=[task])]
            aggregate['merge_candidates'] = [candidate]
            journal = {
                'schema_version': 1, 'id': 'wire-fence',
                'base_revision': item.get('storage_revision', 0),
                'target_revision': item.get('storage_revision', 0) + 1,
                'work_item': aggregate,
            }
            journal_path = work / 'transactions/wire-fence.json'
            journal_path.parent.mkdir(exist_ok=True)
            journal_path.write_text(json.dumps(journal, indent=2))
        before = digest(work)
        result = run(old, root, 'work-item', 'abandon', 'wire-fence', '--reason', 'must refuse')
        assert result.returncode != 0, 'v0.3.0 unexpectedly accepted the fenced record'
        assert 'expected a string' in result.stderr, result.stdout + result.stderr
        after = digest(work)
        assert after == before, f'old reader changed Work (pending={pending}): {before} != {after}'
        print(f'pending={pending}: before={before} after={after}; rejected writer object')
PY
}

run_test "v0.3.0 refuses current Work with unchanged filesystem digests" test_old_reader_fence
summarize_and_exit

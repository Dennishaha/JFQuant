#!/usr/bin/env python3
"""Benchmark explicit action evidence in a NEW disposable workspace; source stays read-only.

Only actions.json and its declared raw members are copied. Composite evidence trees need
the migration integration fixtures instead. All migration writes target the new fixture.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    binary, source, root = (p.resolve() for p in (args.binary, args.source, args.output))
    assert not root.exists(), 'output must be a new directory'
    book = json.loads((source / 'actions.json').read_bytes())
    names = list(book['raw_sha256'])
    assert names and 'source-evidence-sha256.json' not in names, 'use a simple declared raw package'
    assert all(Path(n).name == n and n not in ['.', '..'] for n in names)
    baseline = {p: digest(source / p) for p in [Path('actions.json'), *(Path('raw') / n for n in names)]}

    def call(*argv):
        result = subprocess.run([str(binary), '--workspace', str(root), *argv], capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
        return json.loads(result.stdout)

    def job(*argv):
        task = call(*argv)['task_id']
        return call('jobs', 'wait', task)['results']

    call('workspace', 'create', str(root))
    (root / '.storage-benchmark-fixture').write_text('disposable evidence fixture\n')
    for i in range(2):
        package = root / 'data' / f'copy-{i}'
        (package / 'raw').mkdir(parents=True)
        for name in names:
            shutil.copyfile(source / 'raw' / name, package / 'raw' / name)
        value = dict(book, downloaded_at=f'2026-10-20T00:00:0{i}+00:00')
        (package / 'actions.json').write_text(json.dumps(value, ensure_ascii=False))
    endpoint = Path(f'/tmp/jfquant-{os.getuid()}/{hashlib.sha256(os.fsencode(root)).hexdigest()[:24]}.sock')
    service_log = root / 'benchmark-service.log'
    with service_log.open('w') as log:
        service = subprocess.Popen(['/usr/bin/time', '-l', str(binary), '--workspace', str(root), 'service', 'serve'], stdout=log, stderr=log)
        try:
            for _ in range(300):
                if endpoint.is_socket():
                    break
                assert service.poll() is None, 'service exited'
                time.sleep(.1)
            else:
                raise TimeoutError('service startup')
            versions = [job('data', 'register', str(root / 'data' / f'copy-{i}' / 'actions.json'))['data']['id'] for i in range(2)]
            instrument = next(iter(book.get('queried_symbols') or book.get('failures') or ['SH600000']))
            timings, expected = {}, None

            def inspect(phase):
                nonlocal expected
                for temperature in ['first', 'warm']:
                    started = time.monotonic()
                    result = job('data', 'inspect-actions', '--actions', versions[0], '--instrument', instrument)
                    timings[f'{phase}_{temperature}_seconds'] = time.monotonic() - started
                    if expected is None:
                        expected = result
                    assert result == expected

            inspect('original')
            started = time.monotonic()
            preview = job('storage', 'migrate', 'plan')
            timings['plan_seconds'] = time.monotonic() - started
            assert not preview['blockers'], preview['blockers']
            started = time.monotonic()
            result = job('storage', 'migrate', 'apply', '--plan', preview['id'], '--confirm-rewrite')
            timings['apply_seconds'] = time.monotonic() - started
            assert result['completed'] and result['committed']
            inspect('objects')
            job('storage', 'export', '--resource', preview['packages'][0]['resources'][0], '--output', str(root / 'exported'))
            for name in names:
                assert digest(root / 'exported' / 'raw' / name) == baseline[Path('raw') / name]
            objects = list((root / '.jfquant/objects').rglob('*.obj'))
            assert len(objects) == preview['unique_objects']
            summary = dict(timings, original_files=2 * len(names), original_bytes=preview['original_file_usage']['logical_bytes'], original_allocated_bytes=preview['original_file_usage']['allocated_bytes'], object_files=len(objects), object_bytes=sum(p.stat().st_size for p in objects), object_allocated_bytes=sum(p.stat().st_blocks * 512 for p in objects), business_results_equal=True, exported_bytes_equal=True, available_before=result['available_before'], available_after=result['available_after'])
        finally:
            call('service', 'stop')
            service.wait(timeout=30)
    match = re.search(r'(\d+)\s+maximum resident set size', service_log.read_text())
    summary['service_peak_rss_bytes'] = int(match[1]) if match else None
    assert all(digest(source / p) == value for p, value in baseline.items())
    summary['source_unchanged'] = True
    (root / 'benchmark-result.json').write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary), flush=True)


if __name__ == '__main__':
    main()

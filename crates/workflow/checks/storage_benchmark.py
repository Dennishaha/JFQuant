#!/usr/bin/env python3
"""Reproducible storage benchmark. Only operates on a newly generated disposable fixture."""
import argparse
import concurrent.futures
import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import time


def create_fixture(root, count):
    root.mkdir(parents=True, exist_ok=False)
    (root / '.storage-benchmark-fixture').write_text('disposable fixture\n')
    payload = json.dumps({'request': {'strings': ['fixture-reference'] * 2048}}).encode()

    def bucket(index):
        directory = root / 'data' / f'bucket-{index:04d}'
        directory.mkdir(parents=True)
        for item in range(min(1000, count - index * 1000)):
            (directory / f'{item:04d}.bin').touch()
        (directory / 'metadata.json').write_bytes(payload)

    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        list(executor.map(bucket, range((count + 999) // 1000)))


def call(binary, root, *args):
    result = subprocess.run([str(binary), '--workspace', str(root), *args], capture_output=True, text=True, check=True)
    return json.loads(result.stdout)


def run(binary, root, operation, threads, label):
    # The marker and newly created root ensure this never removes an existing workspace.
    assert (root / '.storage-benchmark-fixture').read_text() == 'disposable fixture\n'
    shutil.rmtree(root / '.jfquant', ignore_errors=True)
    log = root.parent / f'{root.name}-{label}.log'
    with log.open('w') as output:
        service = subprocess.Popen(['/usr/bin/time', '-l', str(binary), '--workspace', str(root), 'service', 'serve'], stdout=output, stderr=output)
        try:
            endpoint = pathlib.Path(f'/tmp/jfquant-{os.getuid()}/{hashlib.sha256(os.fsencode(root.resolve())).hexdigest()[:24]}.sock')
            for _ in range(300):
                time.sleep(.1)
                # `service status` can auto-start a daemon; wait for our own listener first.
                if endpoint.is_socket():
                    call(binary, root, 'service', 'status')
                    break
                if service.poll() is not None:
                    raise RuntimeError(log.read_text())
            else:
                raise TimeoutError('benchmark service did not publish its listener')
            started = time.monotonic()
            if operation == 'baseline':
                command = ('storage', 'scan')
            elif operation == 'verify':
                command = ('storage', 'plan', '--resource', 'benchmark-unselected')
            else:
                command = ('storage', 'scan', '--level', operation, '--threads', str(threads))
            task = call(binary, root, *command)['task_id']
            result = call(binary, root, 'jobs', 'wait', task)
            elapsed = time.monotonic() - started
            status = call(binary, root, 'jobs', 'status', task)['status']
            query_started = time.monotonic()
            if operation != 'baseline':
                snapshot = call(binary, root, 'storage', 'list').get('snapshot')
                complete = snapshot.get('complete') if snapshot else None
            else:
                complete = result.get('results', {}).get('complete')
            query_seconds = time.monotonic() - query_started
            cache_files = [root / '.jfquant/storage/files.json', root / '.jfquant/storage/index.sqlite']
            print(json.dumps({'label': label, 'operation': operation, 'threads': threads, 'elapsed_seconds': elapsed, 'cached_query_seconds': query_seconds, 'index_bytes': sum(p.stat().st_size for p in cache_files if p.is_file()), 'status': status, 'complete': complete}), flush=True)
        finally:
            subprocess.run([str(binary), '--workspace', str(root), 'service', 'stop'], capture_output=True)
            service.wait(timeout=30)
    match = re.search(r'(\d+)\s+maximum resident set size', log.read_text())
    print(json.dumps({'label': label, 'peak_rss_bytes': int(match[1]) if match else None, 'service_log': str(log)}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=pathlib.Path, required=True, help='New disposable fixture directory; must not exist')
    parser.add_argument('--files', type=int, default=1_000_000)
    parser.add_argument('--baseline', type=pathlib.Path, required=True, help='Previous release CLI')
    parser.add_argument('--current', type=pathlib.Path, required=True, help='Current release CLI')
    parser.add_argument('--reuse-fixture', action='store_true', help='Reuse only an existing fixture with the exact disposable marker')
    parser.add_argument('--tiered-baseline', action='store_true', help='Compare protocol45 resource scan and verification against the SQLite implementation')
    args = parser.parse_args()
    root = args.root.resolve()
    if args.reuse_fixture:
        assert (root / '.storage-benchmark-fixture').read_text() == 'disposable fixture\n'
    else:
        create_fixture(root, args.files)
    cases = [
        ('baseline-first', args.baseline, 'baseline', 1),
        ('baseline-warm', args.baseline, 'baseline', 1),
        ('overview-one', args.current, 'overview', 1),
        ('overview-four-first', args.current, 'overview', 4),
        ('overview-four-warm', args.current, 'overview', 4),
        ('verify-first', args.current, 'verify', 4),
        ('verify-warm', args.current, 'verify', 4),
    ]
    if args.tiered_baseline:
        cases = [
            ('baseline-resources-first', args.baseline, 'resources', 4),
            ('baseline-resources-warm', args.baseline, 'resources', 4),
            ('baseline-verify-first', args.baseline, 'verify', 4),
            ('baseline-verify-warm', args.baseline, 'verify', 4),
            ('sqlite-resources-first', args.current, 'resources', 4),
            ('sqlite-resources-warm', args.current, 'resources', 4),
            ('sqlite-overview-one', args.current, 'overview', 1),
            ('sqlite-overview-four', args.current, 'overview', 4),
            ('sqlite-verify-first', args.current, 'verify', 4),
            ('sqlite-verify-warm', args.current, 'verify', 4),
        ]
    for label, binary, operation, threads in cases:
        run(binary.resolve(), root, operation, threads, label)
    print('First passes are NOT guaranteed cold: this script does not purge OS filesystem caches.', flush=True)


if __name__ == '__main__':
    main()

#!/usr/bin/env python3
"""Verify and restore one tail stream generation. Never overwrite the output file."""
import argparse
import gzip
import json
import os
import pathlib
import shutil
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('archive_root', type=pathlib.Path)
parser.add_argument('--stream', required=True)
parser.add_argument('--generation', required=True, type=int)
parser.add_argument('--output', required=True, type=pathlib.Path)
parser.add_argument('--binary', default='logshipper', help='Executable used to verify each segment')
args = parser.parse_args()
segments = []
for path in args.archive_root.rglob('receipt.json'):
    with path.open() as file:
        receipt = json.load(file)
    stream = receipt.get('stream')
    if stream and stream['stream_id'] == args.stream and stream['generation'] == args.generation:
        segments.append((stream['offset'], path, receipt))
if not segments:
    raise SystemExit('No matching stream segments found')
segments.sort(key=lambda item: item[0])
with tempfile.TemporaryDirectory(prefix='.logshipper-restore-', dir=args.output.parent) as tmp:
    temporary = pathlib.Path(tmp) / 'restored'
    offset = 0
    with temporary.open('xb') as output:
        for position, path, receipt in segments:
            if position != offset:
                raise SystemExit(f'Missing or overlapping segment: expected offset {offset}, found {position}')
            subprocess.run([args.binary, '--verify', str(path)], check=True)
            codec = receipt['compression']
            if codec == 'none':
                with (path.parent / 'data').open('rb') as data:
                    shutil.copyfileobj(data, output, 256 * 1024)
            elif codec == 'gzip':
                with gzip.open(path.parent / 'data.gz', 'rb') as data:
                    shutil.copyfileobj(data, output, 256 * 1024)
            elif codec == 'zstd':
                output.flush()
                subprocess.run(['zstd', '-dc', str(path.parent / 'data.zst')], stdout=output, check=True)
            else:
                raise SystemExit(f'Unsupported codec: {codec}')
            offset += receipt['stream']['bytes']
            if output.tell() != offset:
                raise SystemExit('Decoded length differs from the segment receipt')
        output.flush()
        os.fsync(output.fileno())
    # Hard-link publication fails if the requested destination already exists.
    os.link(temporary, args.output)
    fd = os.open(args.output.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)
print(f'Restored {offset} contiguous bytes to {args.output}; completeness beyond the latest receipt depends on source retention/delivery')

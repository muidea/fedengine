#!/usr/bin/env python3
"""Download only the two signed DuckDB 2.0.0 remote-source extensions over HTTPS."""
import argparse
import gzip
import hashlib
import io
import json
import pathlib
import platform
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--dest', default='data/extensions')
parser.add_argument('--refresh', action='store_true', help='Download again even if the local manifest matches')
args = parser.parse_args()
platforms = {('Linux', 'x86_64'): 'linux_amd64', ('Linux', 'aarch64'): 'linux_arm64',
             ('Darwin', 'arm64'): 'osx_arm64', ('Darwin', 'x86_64'): 'osx_amd64'}
target = platforms.get((platform.system(), platform.machine()))
if target is None:
    raise SystemExit('Unsupported extension platform; supply a separately verified bundle')
root = pathlib.Path(args.dest).resolve()
folder = root / 'v2.0.0' / target
folder.mkdir(parents=True, exist_ok=True)
records = {}
if not args.refresh:
    try:
        cached = json.loads((root / 'manifest.json').read_text())
        valid = cached['duckdb_version'] == 'v2.0.0' and cached['platform'] == target
        for logical in ('postgres', 'mysql'):
            entry = cached['extensions'][logical]
            path = (root / entry['path']).resolve()
            valid = valid and path.is_relative_to(root) and path.is_file()
            valid = valid and hashlib.sha256(path.read_bytes()).hexdigest() == entry['sha256']
        if valid:
            print(json.dumps(cached, indent=2))
            print(f'extension_directory = "{root}"')
            raise SystemExit(0)
    except (OSError, KeyError, ValueError):
        pass
for logical in ('postgres', 'mysql'):
    name = logical + '_scanner'
    url = f'https://extensions.duckdb.org/v2.0.0/{target}/{name}.duckdb_extension.gz'
    with tempfile.TemporaryFile() as compressed:
        subprocess.run(["curl", "--fail", "--location", "--retry", "3", "--retry-all-errors", "--max-time", "60", "--silent", "--show-error", url], stdout=compressed, check=True)
        compressed.seek(0)
        data = compressed.read(128 * 1024 * 1024 + 1)
        if len(data) > 128 * 1024 * 1024:
            raise SystemExit('Compressed extension exceeds size limit')
    with gzip.GzipFile(fileobj=io.BytesIO(data)) as stream:
        data = stream.read(256 * 1024 * 1024 + 1)
    if len(data) > 256 * 1024 * 1024:
        raise SystemExit('Extension exceeds size limit')
    path = folder / (name + '.duckdb_extension')
    path.write_bytes(data)
    records[logical] = {'path': str(path.relative_to(root)), 'sha256': hashlib.sha256(data).hexdigest(), 'url': url}
manifest = {'duckdb_version': 'v2.0.0', 'platform': target, 'extensions': records}
(root / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
print(json.dumps(manifest, indent=2))
print(f'extension_directory = "{root}"')

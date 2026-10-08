#!/usr/bin/env bash
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$script_dir/.."
cargo build --release --locked
python3 - <<'PY'
import hashlib, json, pathlib, platform, shutil, subprocess, tarfile, tempfile
root = pathlib.Path.cwd()
version = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--offline', '--format-version', '1'], text=True))['packages'][0]['version']
name = f'logshipper-{version}-linux-{platform.machine()}'
out = root / 'dist'
out.mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix='logshipper-release-') as tmp:
    stage = pathlib.Path(tmp) / name
    stage.mkdir()
    for item in ['target/release/logshipper', 'config.example.toml', 'README.md', 'DEPLOYMENT.md', 'Cargo.lock',
                 'packaging/logshipper.service', 'packaging/alerts.yml', 'scripts/install.sh', 'scripts/restore-stream.py', 'validation/large-file-local.json']:
        source = root / item
        destination = stage / (source.name if item.startswith(('target/', 'scripts/')) else item)
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
    (stage / 'logshipper').chmod(0o755)
    (stage / 'install.sh').chmod(0o755)
    metadata = {
        'version': version, 'architecture': platform.machine(),
        'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
        'libc': platform.libc_ver(),
        'cargo_lock_sha256': hashlib.sha256((root / 'Cargo.lock').read_bytes()).hexdigest(),
        'binary_sha256': hashlib.sha256((stage / 'logshipper').read_bytes()).hexdigest(),
    }
    (stage / 'build-info.json').write_text(json.dumps(metadata, indent=2) + '\n')
    manifest = ''.join(f'{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.relative_to(stage)}\n' for p in sorted(stage.rglob('*')) if p.is_file())
    (stage / 'SHA256SUMS').write_text(manifest)
    archive = out / (name + '.tar.gz')
    with tarfile.open(archive, 'w:gz') as tar:
        tar.add(stage, arcname=name)
    (out / (archive.name + '.sha256')).write_text(f'{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n')
    print(archive)
PY

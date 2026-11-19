#!/usr/bin/env python3
"""Package a native release build and run the extracted executable."""

import argparse
import hashlib
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import zipfile


def archive_metadata(info):
    info.uid = info.gid = 0
    info.uname = info.gname = ''
    return info


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('target', choices=[
        'aarch64-apple-darwin', 'x86_64-apple-darwin',
        'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu',
        'aarch64-pc-windows-msvc', 'x86_64-pc-windows-msvc',
    ])
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    version = re.search(r'^version = "([^"]+)"', (root / 'Cargo.toml').read_text(), re.M).group(1)
    binary_name = 'gitforge-mcp.exe' if 'windows' in args.target else 'gitforge-mcp'
    binary = root / 'target' / args.target / 'release' / binary_name
    name = 'gitforge-mcp-' + version + '-' + args.target
    output = root / 'dist'
    output.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='gitforge package ') as temporary:
        package = Path(temporary) / name
        package.mkdir()
        for source in [binary, root / 'LICENSE-MIT', root / 'LICENSE-APACHE', root / 'README.md']:
            shutil.copy2(source, package / source.name)
        if 'linux' in args.target:
            archive = output / (name + '.tar.gz')
            with tarfile.open(archive, 'w:gz') as bundle:
                bundle.add(package, arcname=name, filter=archive_metadata)
            extracted = Path(temporary) / 'extracted'
            with tarfile.open(archive) as bundle:
                bundle.extractall(extracted)
        else:
            archive = output / (name + '.zip')
            with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as bundle:
                for item in package.iterdir():
                    bundle.write(item, name + '/' + item.name)
            extracted = Path(temporary) / 'extracted'
            with zipfile.ZipFile(archive) as bundle:
                bundle.extractall(extracted)
        executable = extracted / name / binary_name
        executable.chmod(0o755)
        result = subprocess.check_output([str(executable), '--version'], text=True).strip()
        if result != 'gitforge-mcp ' + version:
            raise SystemExit('Packaged executable reported an unexpected version: ' + result)
        subprocess.run([str(executable), '--help'], check=True, stdout=subprocess.DEVNULL)
    print(hashlib.sha256(archive.read_bytes()).hexdigest() + '  ' + archive.name)


if __name__ == '__main__':
    main()

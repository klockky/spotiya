#!/usr/bin/env python3
"""Prepare a private ARM32 Soloist installation. Never publish the output."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import struct
import tarfile
import tempfile
import urllib.request

URL = 'https://soloist-builds.spotifycdn.com/soloist_release_arm32.tar.gz'
DEVICE_DIR = '/data/opt/solo'


def interpreter(binary: bytes, path: str) -> bytes:
    data = bytearray(binary)
    if data[:6] != b'\x7fELF\x01\x01' or struct.unpack_from('<H', data, 18)[0] != 40:
        raise ValueError('Expected the official ARM32 little-endian ELF')
    offset = struct.unpack_from('<I', data, 28)[0]
    size, count = struct.unpack_from('<HH', data, 42)
    for index in range(count):
        entry = struct.unpack_from('<8I', data, offset + index * size)
        if entry[0] != 3:
            continue
        value = path.encode() + b'\0'
        if len(value) > entry[4]:
            raise ValueError('Interpreter path does not fit the ELF header')
        data[entry[1]:entry[1] + entry[4]] = value.ljust(entry[4], b'\0')
        return bytes(data)
    raise ValueError('ELF interpreter not found')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--compat-dir', required=True, type=Path,
                        help='Existing tested ARM32 runtime containing ld.so and lib/')
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--archive', type=Path, help='Previously downloaded official archive')
    args = parser.parse_args()
    if args.output.exists():
        parser.error('Output must not exist; stage a new directory for each version')
    if not (args.compat_dir / 'ld.so').is_file() or not (args.compat_dir / 'lib').is_dir():
        parser.error('Missing compatibility loader or library directory')
    with tempfile.TemporaryDirectory(prefix='spotiya-soloist-') as temp:
        archive = args.archive or Path(temp) / 'soloist.tar.gz'
        if not args.archive:
            with urllib.request.urlopen(URL, timeout=60) as response, archive.open('wb') as out:
                remaining = 64 * 1024 * 1024
                while block := response.read(min(65536, remaining + 1)):
                    remaining -= len(block)
                    if remaining < 0:
                        raise ValueError('Archive exceeds 64 MiB')
                    out.write(block)
        stage = Path(temp) / 'runtime'
        stage.mkdir(mode=0o700)
        with tarfile.open(archive, 'r:gz') as tar:
            member = tar.getmember('soloist')
            if not member.isfile() or not 0 < member.size <= 64 * 1024 * 1024:
                raise ValueError('Invalid Soloist executable')
            binary = tar.extractfile(member).read()
            # Only ELF PT_INTERP changes. Instructions, API keys and authentication
            # remain untouched; launching ld.so directly breaks Crashpad self-exec.
            patched = interpreter(binary, DEVICE_DIR + '/ld.so')
            (stage / 'soloist').write_bytes(patched)
            (stage / 'soloist').chmod(0o755)
            for name in ['CHANGELOG.md', 'THIRD_PARTY_LICENSES.txt']:
                member = tar.getmember(name)
                if not member.isfile() or member.size > 4 * 1024 * 1024:
                    raise ValueError('Invalid license or changelog file')
                (stage / name).write_bytes(tar.extractfile(member).read())
        shutil.copy2(args.compat_dir / 'ld.so', stage / 'ld.so')
        shutil.copytree(args.compat_dir / 'lib', stage / 'lib', symlinks=True)
        for name in ['run.sh', 'set-key.sh']:
            shutil.copy2(Path(__file__).parent / name, stage / name)
            (stage / name).chmod(0o700)
        (stage / 'source.json').write_text(json.dumps({
            'source': URL,
            'archive_sha256': hashlib.sha256(archive.read_bytes()).hexdigest(),
            'original_sha256': hashlib.sha256(binary).hexdigest(),
            'installed_sha256': hashlib.sha256(patched).hexdigest(),
            'change': 'ELF interpreter path only',
            'redistribute': False,
        }, indent=2) + '\n')
        shutil.copytree(stage, args.output, symlinks=True)
    total = sum(f.stat().st_size for f in args.output.rglob('*') if f.is_file())
    print(f'Private runtime: {args.output} ({total / 1024 / 1024:.2f} MiB)')
    print(f'Install at {DEVICE_DIR}; do not upload to release assets.')


if __name__ == '__main__':
    main()

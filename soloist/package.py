"""Package only the open compatibility runtime; never accept a Soloist executable."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import tarfile
import tempfile
import tomllib

RECIPE = Path(__file__).resolve().parent
PROJECT = RECIPE.parent
FILES = ['ld.so', *['lib/' + name for name in (
    'libc.so.6', 'libm.so.6', 'libdl.so.2', 'libpthread.so.0', 'librt.so.1',
    'libresolv.so.2', 'libnss_dns.so.2', 'libnss_files.so.2', 'libatomic.so.1', 'libpulse.so.0')],
    'run.sh', 'set-key.sh']


def package(runtime, dist):
    runtime, dist = Path(runtime), Path(dist)
    if (runtime / 'soloist').exists():
        raise ValueError('Refusing a directory containing proprietary Soloist')
    dist.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='soloist-package-', dir=dist) as stage:
        root = Path(stage) / 'spotiya-soloist-compat'
        root.mkdir()
        hashes = {}
        for name in FILES:
            source = RECIPE / name if name.endswith('.sh') else runtime / name
            if not source.is_file():
                raise ValueError(f'Missing runtime file: {name}')
            target = root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
            target.chmod(0o755)
            hashes[name] = hashlib.sha256(target.read_bytes()).hexdigest()
        if not (runtime / 'licenses').is_dir():
            raise ValueError('Missing runtime licenses')
        shutil.copytree(runtime / 'licenses', root / 'licenses')
        shutil.copyfile(RECIPE / 'sources.json', root / 'sources.json')
        version = tomllib.loads((PROJECT / 'Cargo.toml').read_text())['package']['version']
        (root / 'manifest.json').write_text(json.dumps(dict(format=1,
            target='armv7-unknown-linux-gnueabihf', spotiya_version=version, files=hashes), indent=2)+'\n')
        archive = dist / 'spotiya-soloist-compat-armv7.tar.gz'
        with tarfile.open(archive, 'w:gz', compresslevel=9) as tar:
            tar.add(root, arcname=root.name)
        (dist / 'SOLOIST_SHA256SUMS').write_text(hashlib.sha256(archive.read_bytes()).hexdigest()+'  '+archive.name+'\n')
        print(f'{archive}: {archive.stat().st_size} bytes (no Spotify client or credentials)')
    return archive


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('runtime', type=Path)
    parser.add_argument('--dist', type=Path, default=PROJECT / 'dist')
    args = parser.parse_args()
    package(args.runtime, args.dist)

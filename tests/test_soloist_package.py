import importlib.util
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('soloist_package', Path(__file__).resolve().parents[1]/'soloist/package.py')
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


class SoloistPackage(unittest.TestCase):
    def test_package_contains_only_compatibility_files_and_matching_hashes(self):
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            runtime = root/'runtime'
            for name in package.FILES:
                if name.endswith('.sh'): continue
                target = runtime/name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(b'fixture')
            (runtime/'licenses').mkdir()
            (runtime/'licenses/license').write_text('test')
            # An unrelated private file is not included by recursive copying.
            (runtime/'api-key').write_text('not-a-real-key')
            archive = package.package(runtime, root/'dist')
            with tarfile.open(archive) as tar:
                names = tar.getnames()
                self.assertFalse(any(n.endswith('/api-key') or n.endswith('/soloist') for n in names))
                manifest = json.load(tar.extractfile('spotiya-soloist-compat/manifest.json'))
                self.assertEqual(set(manifest['files']), set(package.FILES))
                for name, digest in manifest['files'].items():
                    self.assertEqual(package.hashlib.sha256(tar.extractfile('spotiya-soloist-compat/'+name).read()).hexdigest(), digest)
            (runtime/'soloist').write_bytes(b'proprietary')
            with self.assertRaisesRegex(ValueError, 'proprietary'):
                package.package(runtime, root/'rejected')

#!/usr/bin/env python3
"""Small offline checks for the smoke fixtures; no daemon, browser or display."""
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
import zipfile

sys.path.insert(0, str(Path(__file__).parent / 'lib'))
from fixture_extension import create


class FixtureArchives(unittest.TestCase):
    def test_two_distinct_deterministic_local_mv3_archives(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            hashes = []
            for name, worker in [('alpha', 'worker.js'), ('beta', 'background/observer.js')]:
                path = root / (name + '.zip')
                approval = create(path, name, worker)
                first = path.read_bytes()
                self.assertEqual(create(path, name, worker), approval)
                self.assertEqual(path.read_bytes(), first)
                self.assertEqual(hashlib.sha256(first).hexdigest(), approval['archive_sha256'])
                self.assertEqual(len(first), approval['archive_byte_length'])
                with zipfile.ZipFile(path) as archive:
                    manifest = json.loads(archive.read('manifest.json'))
                    self.assertEqual(manifest['manifest_version'], 3)
                    self.assertEqual(manifest['version'], approval['version'])
                    self.assertEqual(manifest['background']['service_worker'], worker)
                    self.assertEqual(manifest['permissions'], ['offscreen'])
                    self.assertNotIn('host_permissions', manifest)
                    self.assertIn(b'chrome.offscreen.createDocument', archive.read(worker))
                    self.assertIn(b'chrome.tabs.create', archive.read(worker))
                    for entry in ['onboarding.html', 'offscreen.html']:
                        self.assertIn(b'view.js', archive.read(entry))
                    self.assertIn(b'chrome.runtime.sendMessage', archive.read('view.js'))
                    self.assertIn('theme+base.css', archive.namelist())
                    self.assertTrue(all('http://' not in archive.read(n).decode() and 'https://' not in archive.read(n).decode() for n in archive.namelist()))
                hashes.append(approval['archive_sha256'])
            self.assertNotEqual(*hashes)


if __name__ == '__main__':
    unittest.main()

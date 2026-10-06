# SPDX-License-Identifier: GPL-3.0-or-later
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

import publish


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.assets = self.root / 'assets'
        self.assets.mkdir()
        self.body = b'test installer bytes'
        self.name = 'gouhuo-setup-0.4.0.exe'
        (self.assets / self.name).write_bytes(self.body)
        self.output = self.root / 'output'
        self.base = 'https://downloads.gouhuo.minerei.dev'
        self.release = dict(tag_name='v0.4.0', draft=False, prerelease=False,
                            body='<script>alert(1)</script>', published_at='2026-10-06T00:00:00Z',
                            assets=[dict(name=self.name, size=len(self.body))])

    def generate(self):
        return publish.generate(self.release, self.assets, self.output, self.base)

    def test_manifest_matches_package_and_page_escapes_notes(self):
        manifest = self.generate()
        item = manifest['files']['windows-x64']
        self.assertEqual(item['sha256'], hashlib.sha256(self.body).hexdigest())
        self.assertEqual(item['url'], f'{self.base}/releases/0.4.0/{self.name}')
        page = (self.output / 'index.html').read_text(encoding='utf-8')
        self.assertIn('&lt;script&gt;', page)
        self.assertNotIn('<script>', page)

    def test_rejects_drafts_prereleases_bad_assets_and_urls(self):
        for field, value in [('draft', True), ('prerelease', True), ('tag_name', 'v0.4.0-rc.1')]:
            with self.subTest(field=field):
                previous = self.release[field]
                self.release[field] = value
                with self.assertRaises(ValueError):
                    self.generate()
                self.release[field] = previous
        self.release['assets'][0]['name'] = '../escape.exe'
        with self.assertRaises(ValueError):
            self.generate()
        for base in ['http://example.com', 'https://user:pass@example.com', 'https://example.com/path']:
            with self.assertRaises(ValueError):
                publish.validate_base(base)

    def test_promotes_only_after_public_download_verification(self):
        manifest = self.generate()
        remote, operations = {}, []
        def upload(path, key, cache, immutable=False):
            operations.append(('upload', key))
            remote[f'{self.base}/{key}'] = path.read_bytes()
        def read(url):
            operations.append(('read', url))
            return remote.get(url)
        publish.publish(manifest, self.assets, self.output, self.base, upload, read)
        uploads = [key for operation, key in operations if operation == 'upload']
        self.assertEqual(uploads[-1], 'stable.json')
        self.assertLess(operations.index(('read', manifest['files']['windows-x64']['url'])),
                        operations.index(('upload', 'stable.json')))
        self.assertEqual(json.loads(remote[f'{self.base}/stable.json']), manifest)

    def test_failed_download_does_not_promote(self):
        manifest = self.generate()
        uploaded = []
        def upload(path, key, cache, immutable=False):
            uploaded.append(key)
        with self.assertRaises(ValueError):
            publish.publish(manifest, self.assets, self.output, self.base, upload, lambda url: None)
        self.assertNotIn('stable.json', uploaded)

    def test_refuses_downgrade(self):
        manifest = self.generate()
        uploaded = []
        def upload(*args):
            uploaded.append(args)
        with self.assertRaises(ValueError):
            publish.publish(manifest, self.assets, self.output, self.base, upload,
                            lambda url: b'{"schema":1,"version":"0.5.0"}')
        self.assertEqual(uploaded, [])


if __name__ == '__main__':
    unittest.main()

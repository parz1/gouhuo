#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Build and optionally publish an immutable release, promoting stable.json last."""
import argparse
import hashlib
import html
import json
import os
from pathlib import Path
import re
import subprocess
import urllib.error
import urllib.parse
import urllib.request

MAX_MANIFEST = 64 * 1024


def validate_base(value):
    parsed = urllib.parse.urlsplit(value)
    if (any(character.isspace() for character in value)
            or parsed.scheme != 'https' or not parsed.hostname or parsed.username
            or parsed.password or parsed.query or parsed.fragment or parsed.path not in ('', '/')):
        raise ValueError('UPDATE_BASE_URL must be an HTTPS origin without path, credentials or query')
    return value.rstrip('/')


def version_tuple(value):
    match = re.fullmatch(r'v?(\d+)\.(\d+)\.(\d+)', value)
    if not match or len(value) > 64:
        raise ValueError('Only stable vMAJOR.MINOR.PATCH releases can be published')
    version = tuple(map(int, match.groups()))
    if any(part > 2**64 - 1 for part in version):
        raise ValueError('Version exceeds the client numeric range')
    return version


def generate(release, assets, output, base):
    base = validate_base(base)
    if release.get('draft') or release.get('prerelease'):
        raise ValueError('Drafts and prereleases cannot update the stable channel')
    version_tuple(release['tag_name'])
    version = release['tag_name'].removeprefix('v')
    notes = (release.get('body') or '本次发布暂无更新说明。').encode('utf-8')[:16 * 1024].decode('utf-8', errors='ignore')
    files = {}
    for asset in release['assets']:
        name = asset['name']
        if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]{0,180}', name):
            raise ValueError('Unexpected release asset name')
        path = assets / name
        if (type(asset['size']) is not int or not path.is_file()
                or path.stat().st_size != asset['size'] or not 0 < asset['size'] <= 512 * 1024 * 1024):
            raise ValueError(f'Missing or incomplete release asset: {name}')
        sha = hashlib.sha256(path.read_bytes()).hexdigest()
        platform = 'windows-x64' if name == f'gouhuo-setup-{version}.exe' else name
        if platform in files:
            raise ValueError('Duplicate release asset')
        files[platform] = dict(name=name, url=f'{base}/releases/{version}/{name}',
                               size=asset['size'], sha256=sha)
    if 'windows-x64' not in files:
        raise ValueError('The Windows installer is required')
    manifest = dict(schema=1, version=version, published_at=release.get('published_at'),
                    release_url=f'{base}/releases/{version}/index.html', notes=notes, files=files)
    encoded = (json.dumps(manifest, ensure_ascii=False, indent=2) + '\n').encode('utf-8')
    if len(encoded) > MAX_MANIFEST:
        raise ValueError('Manifest exceeds client response limit')
    output.mkdir(parents=True, exist_ok=True)
    (output / 'stable.json').write_bytes(encoded)
    links = ''.join(f'<li><a href="{html.escape(item["url"], quote=True)}">{html.escape(item["name"])}</a>'
                    f'<small>SHA-256: {item["sha256"]}</small></li>' for item in files.values())
    page = f'''<!doctype html><html lang="zh-CN"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>篝火 {version} · 更新说明</title>
<style>body{{background:#16171b;color:#e6e8ec;font:16px/1.7 system-ui,sans-serif;max-width:760px;margin:48px auto;padding:0 24px}}a{{color:#97baff}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;font:inherit}}small{{display:block;color:#868c96;overflow-wrap:anywhere}}li{{margin:18px 0}}</style>
<h1>篝火 v{version}</h1><h2>下载安装包</h2><ul>{links}</ul>
<p>安装前请结束通话并退出篝火，运行安装包完成升级。</p>
<h2>更新说明</h2><pre>{html.escape(notes)}</pre></html>'''
    (output / 'index.html').write_text(page, encoding='utf-8')
    return manifest


def public_read(url):
    request = urllib.request.Request(url, headers={'User-Agent': 'gouhuo-release', 'Cache-Control': 'no-cache'})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            if response.geturl() != url:
                raise ValueError('Public update URLs must not redirect')
            body = response.read(512 * 1024 * 1024 + 1)
            if len(body) > 512 * 1024 * 1024:
                raise ValueError('Public object too large')
            return body
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def make_uploader(bucket, account):
    if not re.fullmatch(r'[a-fA-F0-9]{32}', account) or not re.fullmatch(r'[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]', bucket):
        raise ValueError('Invalid R2 account ID or bucket name')
    endpoint = f'https://{account}.r2.cloudflarestorage.com'
    def upload(path, key, cache, immutable=False):
        if immutable:
            head = subprocess.run(['aws', 's3api', 'head-object', '--bucket', bucket, '--key', key,
                                   '--endpoint-url', endpoint], capture_output=True, text=True)
            if head.returncode == 0:
                existing = json.loads(head.stdout)
                digest = hashlib.sha256(path.read_bytes()).hexdigest()
                if existing.get('Metadata', {}).get('sha256') != digest or existing['ContentLength'] != path.stat().st_size:
                    raise ValueError(f'Refusing to overwrite a different immutable package: {key}')
                return
            if not any(marker in head.stderr for marker in ('404', 'Not Found', 'NoSuchKey')):
                raise RuntimeError('R2 object lookup failed; check credentials and bucket configuration')
        content_type = 'application/json; charset=utf-8' if key.endswith('.json') else 'text/html; charset=utf-8' if key.endswith('.html') else 'application/octet-stream'
        command = ['aws', 's3', 'cp', str(path), f's3://{bucket}/{key}', '--endpoint-url', endpoint,
                   '--cache-control', cache, '--content-type', content_type, '--only-show-errors',
                   '--metadata', 'sha256=' + hashlib.sha256(path.read_bytes()).hexdigest()]
        if key.endswith('.exe'):
            command += ['--content-disposition', f'attachment; filename="{path.name}"']
        subprocess.run(command, check=True)
    return upload


def publish(manifest, assets, output, base, upload, read=public_read):
    # Refuse rollbacks, including reruns of older releases.
    previous = read(f'{base}/stable.json')
    if previous is not None:
        if len(previous) > MAX_MANIFEST:
            raise ValueError('Existing update manifest is too large')
        previous = json.loads(previous)
        if previous.get('schema') != 1 or version_tuple(previous['version']) > version_tuple(manifest['version']):
            raise ValueError('Refusing to replace an invalid or newer stable channel')
    prefix = f'releases/{manifest["version"]}/'
    for item in manifest['files'].values():
        upload(assets / item['name'], prefix + item['name'], 'public, max-age=31536000, immutable', True)
        body = read(item['url'])
        if body is None or len(body) != item['size'] or hashlib.sha256(body).hexdigest() != item['sha256']:
            raise ValueError(f'Public download verification failed: {item["name"]}')
    upload(output / 'index.html', prefix + 'index.html', 'public, max-age=300')
    if read(manifest['release_url']) is None:
        raise ValueError('Public release page is unavailable')
    upload(output / 'stable.json', 'stable.json', 'no-store, no-cache, max-age=0, must-revalidate')
    if read(f'{base}/stable.json') != (output / 'stable.json').read_bytes():
        raise ValueError('Stable manifest is stale; check the Cloudflare cache bypass rule')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--release-json', type=Path, required=True)
    parser.add_argument('--assets', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--base-url', required=True)
    parser.add_argument('--upload', action='store_true')
    args = parser.parse_args()
    base = validate_base(args.base_url)
    manifest = generate(json.loads(args.release_json.read_text(encoding='utf-8')), args.assets, args.output, base)
    if args.upload:
        publish(manifest, args.assets, args.output, base,
                make_uploader(os.environ['R2_BUCKET'], os.environ['R2_ACCOUNT_ID']))
    print(f'Prepared v{manifest["version"]}' + (' and verified R2 publication' if args.upload else ' locally (no upload)'))


if __name__ == '__main__':
    main()

#!/usr/bin/env python3
"""Publish client data to a versioned Release before deploying Pages metadata/ops."""
import argparse
import gzip
import hashlib
import json
import re
import shutil
import sqlite3
import subprocess
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from urllib.request import Request, urlopen

from package_official_inputs import restore, verified_archive

PAGES_BASE = 'https://zynthium.github.io/future-meta/'
MARKER = '<!-- future-meta:data-release -->'
TAG_PATTERN = r'data-\d{8}T\d{6}Z-[0-9a-f]{12}'
USER_AGENT = 'future-meta-publisher (+https://github.com/zynthium/future-meta)'


def gh(*args, check=True):
    result = subprocess.run(['gh', *args], capture_output=True, text=True)
    if check and result.returncode:
        raise RuntimeError(f'GitHub command failed ({" ".join(args[:3])}): {result.stderr.strip()}')
    return result


def read_pages(path):
    with urlopen(Request(PAGES_BASE+path, headers={'User-Agent': USER_AGENT, 'Cache-Control': 'no-cache'}), timeout=30) as response:
        return json.load(response)


def download_asset(url, limit):
    with urlopen(Request(url, headers={'User-Agent': USER_AGENT}), timeout=60) as response:
        return response.read(limit)


def release_tag(manifest):
    generated = datetime.fromisoformat(manifest['generated_at'])
    if generated.tzinfo is None:
        raise ValueError('Archive generation time requires a timezone')
    stamp = generated.astimezone(timezone.utc).strftime('%Y%m%dT%H%M%SZ')
    return f'data-{stamp}-{manifest["sha256"][:12]}'


def asset_url(repository, tag, artifact):
    return f'https://github.com/{repository}/releases/download/{tag}/{artifact}'


def live_tag(manifest, repository):
    pattern = rf'https://github\.com/{re.escape(repository)}/releases/download/({TAG_PATTERN})/[^/?#]+'
    tags = {match.group(1) for url in manifest.get('mirrors', []) if (match := re.fullmatch(pattern, url))}
    if len(tags) > 1:
        raise ValueError('Production manifest references multiple data releases')
    return next(iter(tags), None)


def validate_source(source):
    manifest = json.loads((source/'manifest.json').read_text())
    name = manifest['artifact']
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]*\.fmeta\.zst', name):
        raise ValueError('Unsafe client artifact filename')
    artifact = source/name
    if not re.fullmatch(r'[0-9a-f]{64}', manifest['sha256']):
        raise ValueError('Invalid client artifact SHA-256')
    if artifact.stat().st_size != manifest['size'] or hashlib.sha256(artifact.read_bytes()).hexdigest() != manifest['sha256']:
        raise ValueError('Client archive checksum mismatch')
    coverage = json.loads((source/'coverage.json').read_text())
    if coverage['integrity'] != 'ok':
        raise ValueError('Source coverage integrity failed')
    inputs = source/'ops/official-inputs.json'
    transport, archive = verified_archive(inputs)
    with tempfile.TemporaryDirectory(prefix='future-meta-publication-') as temp:
        root = Path(temp)
        restore(inputs, root/'evidence')
        db = root/'seed.sqlite'
        with gzip.open(source/'ops/future-meta.sqlite.gz', 'rb') as handle, db.open('wb') as target:
            shutil.copyfileobj(handle, target)
        with sqlite3.connect(f'file:{db}?mode=ro', uri=True) as connection:
            if connection.execute('pragma integrity_check').fetchone()[0] != 'ok':
                raise ValueError('Seed integrity failed')
            reports = connection.execute('select report_json from rebuild_summary').fetchall()
        if len(reports) != 1:
            raise ValueError('Expected one collected seed report')
        report = json.loads(reports[0][0])
        if report['source'] != 'fresh_collector_outputs_no_seed':
            raise ValueError('Seed is not officially collected')
        if not (report['through'] == transport['through'] == coverage['observed_through']):
            raise ValueError('Seed, bootstrap and coverage dates differ')
        if report['observations'] != coverage['observations']:
            raise ValueError('Seed and coverage observation counts differ')
    return manifest, artifact, archive


def release_info(repository, tag):
    result = gh('release', 'view', tag, '--repo', repository, '--json', 'tagName,isDraft,body', check=False)
    if result.returncode:
        if 'release not found' in result.stderr.lower():
            return None
        raise RuntimeError(result.stderr.strip())
    return json.loads(result.stdout)


def ensure_release(repository, target, tag, artifact, manifest):
    release = release_info(repository, tag)
    if release and not (release.get('body') or '').startswith(MARKER):
        raise ValueError('Refusing to modify an unmanaged Release')
    if release and release['isDraft']:
        # Only an unpublished, owned draft can be replaced after failed upload.
        gh('release', 'delete', tag, '--repo', repository, '--cleanup-tag', '--yes')
        release = None
    if release is None:
        with tempfile.TemporaryDirectory(prefix='future-meta-release-') as temp:
            notes = Path(temp)/'notes.md'
            notes.write_text(f'{MARKER}\nClient data {manifest["data_version"]}\n\nSHA-256: {manifest["sha256"]}\n')
            gh('release', 'create', tag, str(artifact), '--repo', repository, '--target', target,
               '--draft', '--latest=false', '--title', f'Fee data {manifest["data_version"]}', '--notes-file', str(notes))
        gh('release', 'edit', tag, '--repo', repository, '--draft=false', '--latest=false')
    url = asset_url(repository, tag, manifest['artifact'])
    body = download_asset(url, manifest['size']+1)
    if len(body) != manifest['size'] or hashlib.sha256(body).hexdigest() != manifest['sha256']:
        raise ValueError('Published Release asset checksum mismatch')
    return url


def prepare(source, pages, repository, target):
    source, pages = Path(source), Path(pages)
    if pages.exists() and any(pages.iterdir()):
        raise FileExistsError('Pages destination must be empty')
    manifest, artifact, archive = validate_source(source)
    tag = release_tag(manifest)
    previous = live_tag(read_pages('manifest.json'), repository)
    if previous == tag:
        # A retry after deployment preserves the actual replaced version,
        # including when a newer orphan Release was never deployed.
        ledger = read_pages('ops/publication.json')
        if ledger['tag'] != tag:
            raise ValueError('Production publication ledger does not match its manifest')
        previous = ledger['previous_tag']
    if previous and (not re.fullmatch(TAG_PATTERN, previous) or previous == tag):
        raise ValueError('Invalid previous data release tag')
    url = ensure_release(repository, target, tag, artifact, manifest)
    manifest['mirrors'] = [url]
    state = {'schema': 1, 'repository': repository, 'tag': tag, 'previous_tag': previous,
             'artifact_url': url, 'sha256': manifest['sha256'], 'data_version': manifest['data_version']}
    pages.mkdir(parents=True, exist_ok=True)
    (pages/'ops').mkdir()
    shutil.copyfile(source/'coverage.json', pages/'coverage.json')
    for path in [source/'ops/future-meta.sqlite.gz', source/'ops/official-inputs.json', archive]:
        shutil.copyfile(path, pages/'ops'/path.name)
    (pages/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    (pages/'ops/publication.json').write_text(json.dumps(state, indent=2)+'\n')
    return state


def cleanup(pages, repository):
    pages = Path(pages)
    manifest = json.loads((pages/'manifest.json').read_text())
    state = json.loads((pages/'ops/publication.json').read_text())
    if state['repository'] != repository or not re.fullmatch(TAG_PATTERN, state['tag']):
        raise ValueError('Invalid publication owner or data tag')
    url = asset_url(repository, state['tag'], manifest['artifact'])
    if (manifest['mirrors'] != [url] or state['artifact_url'] != url
            or state['sha256'] != manifest['sha256'] or state['data_version'] != manifest['data_version']
            or (state['previous_tag'] and (not re.fullmatch(TAG_PATTERN, state['previous_tag']) or state['previous_tag'] == state['tag']))):
        raise ValueError('Inconsistent publication manifest and ledger')
    if read_pages('manifest.json') != manifest:
        raise ValueError('Canonical production manifest does not match this deployment; retaining Releases')
    if read_pages('ops/publication.json') != state:
        raise ValueError('Canonical production ledger does not match this deployment; retaining Releases')
    keep = {state['tag'], state['previous_tag']}
    releases = json.loads(gh('api', '--paginate', '--slurp', f'repos/{repository}/releases?per_page=100').stdout)
    deleted = []
    for release in (item for page in releases for item in page):
        tag = release['tag_name']
        if re.fullmatch(TAG_PATTERN, tag) and (release.get('body') or '').startswith(MARKER) and tag not in keep:
            gh('release', 'delete', tag, '--repo', repository, '--cleanup-tag', '--yes')
            deleted.append(tag)
    return {'kept': sorted(tag for tag in keep if tag), 'deleted': deleted}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    build = commands.add_parser('prepare')
    build.add_argument('--source', type=Path, required=True)
    build.add_argument('--pages-out', type=Path, required=True)
    build.add_argument('--target', required=True)
    build.add_argument('--github-output', type=Path)
    prune = commands.add_parser('cleanup')
    prune.add_argument('--pages-dir', type=Path, required=True)
    for command in [build, prune]:
        command.add_argument('--repo', required=True)
    args = parser.parse_args()
    if args.command == 'prepare':
        result = prepare(args.source, args.pages_out, args.repo, args.target)
        if args.github_output:
            with args.github_output.open('a') as output:
                output.write('ready=true\n')
    else:
        result = cleanup(args.pages_dir, args.repo)
    print(json.dumps(result))


if __name__ == '__main__':
    main()

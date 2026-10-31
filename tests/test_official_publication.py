import gzip
import hashlib
import io
import json
import sqlite3
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import publish_data as publish

REPO = 'zynthium/future-meta'


def fixture(root):
    source = root/'source'
    ops = source/'ops'
    ops.mkdir(parents=True)
    artifact = b'client archive'
    manifest = {'schema_version': 4, 'generated_at': '2026-10-31T08:00:00Z',
                'data_version': '2026-10-31T08:00:00Z', 'history_end': '2026-10-30T00:00:00+08:00',
                'artifact': 'latest.fmeta.zst', 'size': len(artifact),
                'sha256': hashlib.sha256(artifact).hexdigest(), 'mirrors': []}
    (source/'manifest.json').write_text(json.dumps(manifest))
    (source/'latest.fmeta.zst').write_bytes(artifact)
    (source/'artifacts').mkdir()
    (source/'artifacts/old.fmeta.zst').write_bytes(b'never deploy')
    (source/'coverage.json').write_text(json.dumps({'integrity': 'ok', 'observed_through': '2026-10-30', 'observations': 3}))
    db = root/'seed.sqlite'
    with sqlite3.connect(db) as connection:
        connection.execute('create table rebuild_summary(report_json text)')
        connection.execute('insert into rebuild_summary values(?)', (json.dumps({
            'source': 'fresh_collector_outputs_no_seed', 'through': '2026-10-30', 'observations': 3}),))
    with db.open('rb') as handle, gzip.open(ops/'future-meta.sqlite.gz', 'wb') as target:
        target.write(handle.read())
    original = b'official original'
    inventory = {'schema': 1, 'through': '2026-10-30', 'files': {'original': hashlib.sha256(original).hexdigest()}}
    archive = ops/'official-inputs.tar.gz'
    with tarfile.open(archive, 'w:gz') as handle:
        for name, body in [('bootstrap.json', json.dumps(inventory).encode()), ('original', original)]:
            entry = tarfile.TarInfo(name)
            entry.size = len(body)
            handle.addfile(entry, io.BytesIO(body))
    (ops/'official-inputs.json').write_text(json.dumps({'schema': 2, 'through': '2026-10-30',
        'file': archive.name, 'size': archive.stat().st_size, 'sha256': hashlib.sha256(archive.read_bytes()).hexdigest()}))
    return source, manifest, artifact


class GitHub:
    def __init__(self):
        self.releases = {}
        self.events = []

    def run(self, *args, check=True):
        command = list(args)
        if command[:2] == ['release', 'view']:
            release = self.releases.get(command[2])
            return subprocess.CompletedProcess(command, 0 if release else 1,
                                               json.dumps(release) if release else '', '' if release else 'release not found')
        if command[:2] == ['release', 'create']:
            self.events.append('create')
            assert '--draft' in command and '--latest=false' in command
            body = Path(command[command.index('--notes-file')+1]).read_text()
            self.releases[command[2]] = {'tagName': command[2], 'isDraft': True, 'body': body}
        elif command[:2] == ['release', 'edit']:
            self.events.append('publish')
            assert '--draft=false' in command and '--latest=false' in command
            self.releases[command[2]]['isDraft'] = False
        elif command[:2] == ['release', 'delete']:
            self.events.append(('delete', command[2]))
            assert '--cleanup-tag' in command and '--yes' in command
            del self.releases[command[2]]
        elif command[0] == 'api':
            return subprocess.CompletedProcess(command, 0, json.dumps([[{
                'tag_name': tag, 'body': release['body'], 'draft': release['isDraft']}
                for tag, release in self.releases.items()]]), '')
        else:
            raise AssertionError(command)
        return subprocess.CompletedProcess(command, 0, '', '')


class OfficialPublicationTests(unittest.TestCase):
    def test_release_verified_before_pages_manifest_and_only_internal_files_deploy(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, original_manifest, artifact = fixture(root)
            pages = root/'pages'
            github = GitHub()

            def download(url, limit):
                self.assertEqual(github.events, ['create', 'publish'])
                self.assertFalse(pages.exists())
                github.events.append('verified_download')
                return artifact

            with patch.object(publish, 'gh', side_effect=github.run), \
                    patch.object(publish, 'read_pages', return_value={'mirrors': [publish.PAGES_BASE+'latest.fmeta.zst']}), \
                    patch.object(publish, 'download_asset', side_effect=download):
                state = publish.prepare(source, pages, REPO, 'main')
            manifest = json.loads((pages/'manifest.json').read_text())
            self.assertEqual(manifest['mirrors'], [f'https://github.com/{REPO}/releases/download/{state["tag"]}/latest.fmeta.zst'])
            self.assertEqual(json.loads((source/'manifest.json').read_text()), original_manifest)
            self.assertTrue((source/'latest.fmeta.zst').exists())
            self.assertEqual({str(p.relative_to(pages)) for p in pages.rglob('*') if p.is_file()}, {
                'manifest.json', 'coverage.json', 'ops/future-meta.sqlite.gz', 'ops/official-inputs.json',
                'ops/official-inputs.tar.gz', 'ops/publication.json'})

    def test_bad_release_download_never_prepares_pages(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, _, _ = fixture(root)
            github = GitHub()
            with patch.object(publish, 'gh', side_effect=github.run), patch.object(publish, 'read_pages', return_value={'mirrors': []}), \
                    patch.object(publish, 'download_asset', return_value=b'corrupt'):
                with self.assertRaisesRegex(ValueError, 'checksum'):
                    publish.prepare(source, root/'pages', REPO, 'main')
            self.assertFalse((root/'pages').exists())

    def test_mismatched_seed_date_stops_before_any_release_mutation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, _, _ = fixture(root)
            coverage = source/'coverage.json'
            report = json.loads(coverage.read_text())
            report['observed_through'] = '2026-10-29'
            coverage.write_text(json.dumps(report))
            with patch.object(publish, 'gh') as gh:
                with self.assertRaisesRegex(ValueError, 'dates'):
                    publish.prepare(source, root/'pages', REPO, 'main')
                gh.assert_not_called()

    def test_existing_release_reused_and_replay_preserves_actual_previous_tag(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, manifest, artifact = fixture(root)
            tag = publish.release_tag(manifest)
            previous = 'data-20261030T080000Z-'+'1'*12
            github = GitHub()
            github.releases[tag] = {'tagName': tag, 'isDraft': False, 'body': publish.MARKER}
            live_manifest = {**manifest, 'mirrors': [publish.asset_url(REPO, tag, manifest['artifact'])]}
            ledger = {'tag': tag, 'previous_tag': previous}
            with patch.object(publish, 'gh', side_effect=github.run), \
                    patch.object(publish, 'read_pages', side_effect=lambda path: ledger if path.startswith('ops/') else live_manifest), \
                    patch.object(publish, 'download_asset', return_value=artifact):
                state = publish.prepare(source, root/'pages', REPO, 'main')
            self.assertEqual(state['previous_tag'], previous)
            self.assertEqual(github.events, [])

    def test_cleanup_keeps_exact_current_and_previous_and_never_sdk_releases(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, _, artifact = fixture(root)
            github = GitHub()
            previous = 'data-20261030T080000Z-'+'1'*12
            stale = 'data-20261029T080000Z-'+'2'*12
            orphan = 'data-20261031T090000Z-'+'3'*12
            with patch.object(publish, 'gh', side_effect=github.run), patch.object(publish, 'read_pages', return_value={
                    'mirrors': [publish.asset_url(REPO, previous, 'latest.fmeta.zst')]}), \
                    patch.object(publish, 'download_asset', return_value=artifact):
                state = publish.prepare(source, root/'pages', REPO, 'main')
            for tag in [previous, stale, orphan, 'v0.6.0']:
                github.releases[tag] = {'isDraft': False, 'body': publish.MARKER}
            github.events.clear()
            live = json.loads((root/'pages/manifest.json').read_text())
            with patch.object(publish, 'gh', side_effect=github.run), patch.object(publish, 'read_pages', side_effect=lambda path: state if path.startswith('ops/') else live):
                publish.cleanup(root/'pages', REPO)
            self.assertEqual(set(github.releases), {state['tag'], previous, 'v0.6.0'})

    def test_cleanup_refuses_to_delete_when_pages_does_not_match_publication(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, _, artifact = fixture(root)
            github = GitHub()
            with patch.object(publish, 'gh', side_effect=github.run), patch.object(publish, 'read_pages', return_value={'mirrors': []}), \
                    patch.object(publish, 'download_asset', return_value=artifact):
                publish.prepare(source, root/'pages', REPO, 'main')
            with patch.object(publish, 'read_pages', return_value={'mirrors': []}), patch.object(publish, 'gh') as gh:
                with self.assertRaisesRegex(ValueError, 'production'):
                    publish.cleanup(root/'pages', REPO)
                gh.assert_not_called()

    def test_owned_incomplete_draft_can_be_retried_without_overwriting_a_public_asset(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, manifest, artifact = fixture(root)
            tag = publish.release_tag(manifest)
            github = GitHub()
            github.releases[tag] = {'tagName': tag, 'isDraft': True, 'body': publish.MARKER}
            with patch.object(publish, 'gh', side_effect=github.run), patch.object(publish, 'read_pages', return_value={'mirrors': []}), \
                    patch.object(publish, 'download_asset', return_value=artifact):
                publish.prepare(source, root/'pages', REPO, 'main')
            self.assertEqual(github.events, [('delete', tag), 'create', 'publish'])

    def test_mixed_manifest_and_ledger_cannot_delete_live_release(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, _, artifact = fixture(root)
            github = GitHub()
            with patch.object(publish, 'gh', side_effect=github.run), patch.object(publish, 'read_pages', return_value={'mirrors': []}), \
                    patch.object(publish, 'download_asset', return_value=artifact):
                state = publish.prepare(source, root/'pages', REPO, 'main')
            state['tag'] = 'data-20261029T080000Z-'+'2'*12
            (root/'pages/ops/publication.json').write_text(json.dumps(state))
            live = json.loads((root/'pages/manifest.json').read_text())
            with patch.object(publish, 'read_pages', side_effect=lambda path: state if path.startswith('ops/') else live), \
                    patch.object(publish, 'gh') as gh:
                with self.assertRaisesRegex(ValueError, 'publication'):
                    publish.cleanup(root/'pages', REPO)
                gh.assert_not_called()

    def test_unknown_github_state_is_not_treated_as_missing_release(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, _, _ = fixture(root)
            response = subprocess.CompletedProcess(['gh'], 1, '', 'TLS handshake timeout')
            with patch.object(publish, 'read_pages', return_value={'mirrors': []}), \
                    patch.object(publish, 'gh', return_value=response) as gh:
                with self.assertRaisesRegex(RuntimeError, 'TLS handshake timeout'):
                    publish.prepare(source, root/'pages', REPO, 'main')
            self.assertEqual(gh.call_count, 1)
            self.assertEqual(gh.call_args.args[:2], ('release', 'view'))
            self.assertFalse((root/'pages').exists())

    def test_corrupt_local_client_archive_never_reaches_github(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, _, _ = fixture(root)
            (source/'latest.fmeta.zst').write_bytes(b'corrupt local archive')
            with patch.object(publish, 'gh') as gh:
                with self.assertRaisesRegex(ValueError, 'checksum'):
                    publish.prepare(source, root/'pages', REPO, 'main')
                gh.assert_not_called()


if __name__ == '__main__':
    unittest.main()

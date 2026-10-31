import hashlib
import io
import json
import sqlite3
import sys
import tarfile
import tempfile
import unittest
from datetime import date, datetime
from pathlib import Path
from unittest.mock import patch
from zoneinfo import ZoneInfo

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import package_official_inputs as bundle
import update_official as update


class OfficialUpdateTests(unittest.TestCase):
    def test_fetch_verifies_one_archive_from_github_pages(self):
        body = b'official archive bytes'
        sha = hashlib.sha256(body).hexdigest()
        transport = {'schema': 2, 'through': '2026-10-30', 'file': 'official-inputs.tar.gz',
                     'size': len(body), 'sha256': sha}
        base = 'https://zynthium.github.io/future-meta/ops/'
        requested = []

        def pages(request, timeout):
            self.assertEqual(request.get_header('User-agent'), bundle.USER_AGENT)
            requested.append(request.full_url)
            return io.BytesIO(json.dumps(transport).encode() if request.full_url.endswith('official-inputs.json') else body)

        with tempfile.TemporaryDirectory() as temp, patch.object(bundle, 'urlopen', side_effect=pages):
            output = Path(temp) / 'official-inputs.json'
            self.assertEqual(bundle.fetch(base + output.name, output), transport)
            self.assertEqual((output.parent / 'official-inputs.tar.gz').read_bytes(), body)
        self.assertEqual(requested, [base + 'official-inputs.json', base + 'official-inputs.tar.gz'])

    def test_completed_china_day_before_and_after_daily_publication(self):
        tz = ZoneInfo('Asia/Shanghai')
        self.assertEqual(update.completed_china_date(datetime(2026, 10, 30, 18, 29, tzinfo=tz)), date(2026, 10, 29))
        self.assertEqual(update.completed_china_date(datetime(2026, 10, 30, 18, 45, tzinfo=tz)), date(2026, 10, 30))

    def test_seed_requires_collected_marker(self):
        with tempfile.TemporaryDirectory() as temp:
            db = Path(temp) / 'seed.sqlite'
            with sqlite3.connect(db) as conn:
                conn.execute('create table rebuild_summary(report_json text)')
                conn.execute('insert into rebuild_summary values(?)', (json.dumps({'source': 'old', 'through': '2026-10-30'}),))
            with self.assertRaisesRegex(ValueError, 'not the official'):
                update.seed_through(db)
            with sqlite3.connect(db) as conn:
                conn.execute('update rebuild_summary set report_json=?', (json.dumps({'source': 'fresh_collector_outputs_no_seed', 'through': '2026-10-30'}),))
            self.assertEqual(update.seed_through(db), date(2026, 10, 30))

    def test_weekend_noop_does_not_merge_or_publish(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            args = type('Args', (), dict(db=root/'seed', inputs=root/'bundle', work_dir=root/'work', public_dir=root/'public', through=date(2026, 10, 31), github_output=root/'output'))()
            with patch.object(update, 'seed_through', return_value=date(2026, 10, 30)), patch.object(update, 'restore', return_value={'through': '2026-10-30'}), patch.object(update, 'refresh_calendar', return_value=[]), patch.object(update, 'completed_china_date', return_value=date(2026, 10, 31)), patch.object(update, 'pending_announcements'), patch.object(update, 'daemon') as daemon:
                result = update.update(args)
            self.assertFalse(result['publish'])
            daemon.assert_not_called()
            self.assertEqual(args.github_output.read_text(), 'publish=false\n')

    def test_calendar_source_failure_cannot_become_noop(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            args = type('Args', (), dict(db=root/'seed', inputs=root/'bundle', work_dir=root/'work', public_dir=root/'public', through=date(2026, 10, 31), github_output=root/'output'))()
            with patch.object(update, 'seed_through', return_value=date(2026, 10, 30)), patch.object(update, 'restore', return_value={'through': '2026-10-30'}), patch.object(update, 'pending_announcements'), patch.object(update, 'refresh_calendar', side_effect=RuntimeError('calendar unavailable')), patch.object(update, 'completed_china_date', return_value=date(2026, 10, 31)):
                with self.assertRaisesRegex(RuntimeError, 'unavailable'):
                    update.update(args)
            self.assertFalse(args.github_output.exists())

    def test_restore_rejects_path_traversal_and_tampering(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for name in ('../outside', 'raw.xml'):
                archive = root / 'bad.tar.gz'
                with tarfile.open(archive, 'w:gz') as target:
                    for key, body in [('bootstrap.json', json.dumps({'schema': 1, 'files': {name: '0'*64}}).encode()), (name, b'bad')]:
                        entry = tarfile.TarInfo(key)
                        entry.size = len(body)
                        target.addfile(entry, io.BytesIO(body))
                body = archive.read_bytes()
                index = root/'inputs.json'
                index.write_text(json.dumps({'schema': 2, 'through': '2026-10-30', 'file': archive.name,
                                             'size': len(body), 'sha256': hashlib.sha256(body).hexdigest()}))
                with self.assertRaises(ValueError):
                    bundle.restore(index, root/'out')
                self.assertFalse((root/'out').exists())

    def test_package_roundtrip_rewrites_absolute_raw_paths(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            evidence = root/'evidence'
            evidence.mkdir()
            body = b'official raw'
            sha = hashlib.sha256(body).hexdigest()
            raw = evidence/(sha+'.xml')
            raw.write_bytes(body)
            (evidence/'cffex-product-metadata.tsv').write_text('product\tsha256\traw_path\nIF\t'+sha+'\t'+str(raw)+'\n')
            # Test the extra statutory calendar without embedding a production original.
            calendar = evidence/(sha+'.json')
            calendar.write_bytes(body)
            with patch.object(bundle, 'GFEX_CALENDAR', calendar.name):
                report = bundle.package(evidence, None, '2026-10-30', root/'bundle.json', True)
            restored = root/'restored'
            self.assertEqual(bundle.restore(root/'bundle.json', restored), report)
            self.assertTrue(report['lifecycle_options']['assume_2027_holidays'])
            self.assertNotIn(str(root), (restored/'cffex-product-metadata.tsv').read_text())
            self.assertEqual((restored/raw.name).read_bytes(), body)
            self.assertEqual(json.loads((root/'bundle.json').read_text())['schema'], 2)
            self.assertTrue((root/'bundle.tar.gz').is_file())
            self.assertEqual(list(root.glob('*.part')), [])

    def test_transport_rejects_remote_and_traversal_archive_names(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'inputs.json'
            for name in ('../secret', 'https://other/part', '%2e%2e%2fsecret', '/part'):
                path.write_text(json.dumps({'schema': 2, 'through': '2026-10-30', 'file': name, 'size': 1, 'sha256': '0'*64}))
                with self.assertRaises(ValueError):
                    bundle.transport_manifest(path)

    def test_restore_detects_archive_tampering_before_extracting(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root/'inputs.tar.gz').write_bytes(b'tampered')
            manifest = {'schema': 2, 'through': '2026-10-30', 'file': 'inputs.tar.gz',
                        'size': 8, 'sha256': hashlib.sha256(b'original').hexdigest()}
            (root/'inputs.json').write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                bundle.restore(root/'inputs.json', root/'out')
            self.assertFalse((root/'out').exists())

    def test_old_split_transport_is_rejected_and_large_archive_metadata_is_valid(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)/'inputs.json'
            path.write_text(json.dumps({'schema': 1, 'parts': [{'file': 'part-000', 'size': 1, 'sha256': '0'*64}]}))
            with self.assertRaises(ValueError):
                bundle.transport_manifest(path)
            current = {'schema': 2, 'through': '2026-10-30', 'file': 'inputs.tar.gz', 'size': 40*1024*1024, 'sha256': '0'*64}
            path.write_text(json.dumps(current))
            self.assertEqual(bundle.transport_manifest(path), current)

    def test_regeneration_preserves_reviewed_lifecycles_and_adds_new_symbols(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            path = root/'dce-lifecycle-test.tsv'
            header = 'exchange\tsymbol\texpiry_date\treport_date_kind\n'
            path.write_text(header+'DCE\tm2611\t2026-11-13\tofficial_report_date\n')
            before = update.lifecycle_snapshot(root)
            path.write_text(header+'DCE\tm2611\t2026-11-14\tderived_rule\nDCE\tm2701\t2027-01-15\tuser_assumed_holiday_calendar\n')
            update.preserve_known_lifecycles(root, before)
            self.assertIn('2026-11-13\tofficial_report_date', path.read_text())
            self.assertNotIn('2026-11-14', path.read_text())
            self.assertIn('m2701', path.read_text())


if __name__ == '__main__':
    unittest.main()

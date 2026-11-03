import json
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime
from pathlib import Path
from unittest.mock import patch
from zoneinfo import ZoneInfo

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import update_official as update
import update_schedule as schedule


class OfficialScheduleTests(unittest.TestCase):
    def plan(self, at):
        return schedule.decide(datetime.fromisoformat(at).replace(tzinfo=ZoneInfo('Asia/Shanghai')))

    def test_weekends_and_makeup_workdays_do_not_open_network_window(self):
        for at in ('2026-10-31T18:45', '2026-11-01T08:05', '2026-02-15T18:45'):
            with self.subTest(at=at):
                self.assertFalse(self.plan(at)['should_update'])

    def test_friday_night_runs_and_normal_monday_morning_skips(self):
        self.assertEqual(self.plan('2026-10-30T18:45')['slot'], 'evening')
        self.assertFalse(self.plan('2026-11-02T08:05')['should_update'])

    def test_holidays_and_last_evening_before_holiday_skip(self):
        for at in ('2026-10-01T18:45', '2026-09-30T18:45', '2026-09-24T18:45'):
            with self.subTest(at=at):
                self.assertFalse(self.plan(at)['should_update'])

    def test_first_morning_after_weekday_holiday_runs(self):
        for at in ('2026-10-08T08:05', '2026-09-28T08:05', '2026-02-24T08:05'):
            with self.subTest(at=at):
                self.assertEqual(self.plan(at)['slot'], 'holiday_morning')
                self.assertTrue(self.plan(at)['should_update'])
        self.assertTrue(self.plan('2026-10-08T18:45')['should_update'])

    def test_delayed_runs_outside_admission_windows_skip(self):
        for at in ('2026-11-02T20:00', '2026-10-08T08:20', '2026-11-02T12:00'):
            with self.subTest(at=at):
                self.assertFalse(self.plan(at)['should_update'])
        self.assertTrue(self.plan('2026-10-30T18:30')['should_update'])

    def test_utc_time_is_converted_and_naive_time_rejected(self):
        self.assertTrue(schedule.decide(datetime.fromisoformat('2026-10-30T10:45:00+00:00'))['should_update'])
        with self.assertRaisesRegex(ValueError, 'timezone'):
            schedule.decide(datetime(2026, 10, 30, 18, 45))

    def test_shared_deadline_matches_each_admitted_window(self):
        for at, expected in (
            ('2026-11-02T18:45', '2026-11-02T20:00:00+08:00'),
            ('2026-10-08T08:05', '2026-10-08T08:20:00+08:00'),
        ):
            with self.subTest(at=at):
                self.assertEqual(schedule.access_deadline(self.plan(at)), expected)
        with self.assertRaisesRegex(ValueError, 'No admitted'):
            schedule.access_deadline(self.plan('2026-11-01T18:45'))

    def test_missing_calendar_fails_without_guessing_holidays(self):
        with tempfile.TemporaryDirectory() as temp:
            calendar = Path(temp)/'calendar.json'
            calendar.write_text(json.dumps({'schema': 1, 'years': {}}))
            with patch.object(schedule, 'CALENDAR', calendar), self.assertRaisesRegex(ValueError, '2027'):
                self.plan('2027-01-04T18:45')

    def test_current_weekend_cli_has_no_network_or_collector_dependency(self):
        result = subprocess.run([sys.executable, 'scripts/update_schedule.py', '--at', '2026-10-31T10:45:00+00:00'],
                                check=True, capture_output=True, text=True)
        self.assertFalse(json.loads(result.stdout)['should_update'])

    def test_completed_slot_waits_for_close_and_publication_grace(self):
        for at, expected in (
            ('2026-11-03T20:09:59+08:00', '2026-11-02'),
            ('2026-11-03T20:10:00+08:00', '2026-11-03'),
            ('2026-11-03T12:10:00+00:00', '2026-11-03'),
        ):
            with self.subTest(at=at):
                self.assertEqual(schedule.latest_completed_slot(datetime.fromisoformat(at))['target_through'], expected)

    def test_holiday_eve_and_break_do_not_invent_due_evening_updates(self):
        for at in ('2026-09-30T20:17', '2026-10-01T20:17', '2026-10-07T20:17', '2026-10-08T08:29'):
            with self.subTest(at=at):
                completed = schedule.latest_completed_slot(datetime.fromisoformat(at).replace(tzinfo=schedule.CHINA))
                self.assertEqual(completed['target_through'], '2026-09-29')
        completed = schedule.latest_completed_slot(datetime.fromisoformat('2026-10-08T08:30+08:00'))
        self.assertEqual(completed['target_through'], '2026-09-30')
        self.assertEqual(completed['slot'], '2026-10-08:holiday_morning')

    def test_holiday_morning_collects_pre_break_day_and_evening_collects_today(self):
        for at, target in (
            ('2026-10-08T08:07+08:00', '2026-09-30'),
            ('2026-09-28T08:07+08:00', '2026-09-24'),
            ('2026-10-08T18:35+08:00', '2026-10-08'),
        ):
            with self.subTest(at=at):
                self.assertEqual(schedule.collection_target(datetime.fromisoformat(at)), target)

    def test_health_checks_also_fail_closed_on_missing_calendar_year(self):
        with self.assertRaisesRegex(ValueError, '2027'):
            schedule.latest_completed_slot(datetime.fromisoformat('2027-01-04T20:17+08:00'))

    def test_runtime_recheck_skips_before_reading_branch_or_collecting(self):
        args=type('Args',(),{'republish_current':False})()
        with patch.object(update,'scheduled_decision',return_value={'should_update':False,'reason':'weekend'}), \
                patch.object(update.branch,'read') as read, patch.object(update,'restore_archive') as restore:
            result=update.update(args)
        self.assertFalse(result['publish'])
        read.assert_not_called()
        restore.assert_not_called()


if __name__ == '__main__':
    unittest.main()

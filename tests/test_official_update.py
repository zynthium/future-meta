import copy
import sys
import tempfile
import unittest
from datetime import datetime
from pathlib import Path
from unittest.mock import patch
from zoneinfo import ZoneInfo

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import update_official as update
import data_branch as branch
from test_data_branch import snapshot


class OfficialUpdateTests(unittest.TestCase):
    def test_completed_china_day_before_and_after_publication(self):
        tz = ZoneInfo("Asia/Shanghai")
        self.assertEqual(
            update.completed_china_date(
                datetime(2026, 11, 2, 8, 5, tzinfo=tz)
            ).isoformat(),
            "2026-11-01",
        )
        self.assertEqual(
            update.completed_china_date(
                datetime(2026, 11, 2, 18, 45, tzinfo=tz)
            ).isoformat(),
            "2026-11-02",
        )

    def test_weekend_rejects_before_reading_state_or_importing_collector(self):
        args = type("Args", (), {"republish_current": False})()
        with (
            patch.object(
                update,
                "scheduled_decision",
                return_value={"should_update": False, "reason": "weekend"},
            ),
            patch.object(branch, "read") as read,
        ):
            result = update.update(args)
            self.assertEqual(result["reason"], "weekend")
            read.assert_not_called()

    def test_duplicate_slot_does_not_collect_or_write_state(self):
        manifest, state = snapshot()
        state = branch.begin_attempt(
            state, "2026-11-02:evening", "2026-11-02T10:45:00Z", "url", "1"
        )
        args = type("Args", (), {"republish_current": False, "repo": "repo"})()
        gate = {
            "should_update": True,
            "slot": "evening",
            "evaluated_at": "2026-11-02T18:45:00+08:00",
        }
        with (
            patch.object(update, "scheduled_decision", return_value=gate),
            patch.object(branch, "read", return_value=("parent", manifest, state)),
            patch.object(branch, "commit") as commit,
        ):
            self.assertEqual(update.update(args)["reason"], "slot_already_attempted")
            commit.assert_not_called()

    def test_source_failure_keeps_old_success_checkpoint_and_records_failed_attempt(
        self,
    ):
        manifest, state = snapshot()
        gate = {
            "should_update": True,
            "slot": "evening",
            "evaluated_at": "2026-11-02T18:45:00+08:00",
        }
        with tempfile.TemporaryDirectory() as temp:
            args = type(
                "Args",
                (),
                {
                    "republish_current": False,
                    "repo": "repo",
                    "work_dir": Path(temp) / "work",
                    "rules": Path("config/official-rules.json"),
                },
            )()
            commits = []

            def commit(repo, parent, doc, checkpoint, message):
                commits.append(copy.deepcopy(checkpoint))
                return "started"

            with (
                patch.object(update, "scheduled_decision", return_value=gate),
                patch.object(branch, "read", return_value=("parent", manifest, state)),
                patch.object(branch, "commit", side_effect=commit),
                patch.object(
                    update,
                    "restore_archive",
                    side_effect=RuntimeError("source unavailable"),
                ),
            ):
                with self.assertRaisesRegex(RuntimeError, "source unavailable"):
                    update.update(args)
            self.assertEqual(
                [s["last_attempt"]["status"] for s in commits], ["running", "failed"]
            )
            self.assertEqual(commits[-1]["checked_through"], state["checked_through"])
            self.assertIsNone(commits[-1]["last_success_at"])


if __name__ == "__main__":
    unittest.main()


class PublicationVerificationTests(unittest.TestCase):
    def test_republish_exercises_write_permission_without_advancing_official_state(
        self,
    ):
        manifest, state = snapshot()
        with tempfile.TemporaryDirectory() as temp:
            args = type(
                "Args",
                (),
                {
                    "republish_current": True,
                    "repo": "repo",
                    "work_dir": Path(temp) / "work",
                },
            )()
            with (
                patch.object(branch, "read", return_value=("parent", manifest, state)),
                patch.object(branch, "commit", return_value="probe") as commit,
                patch.object(update, "restore_archive"),
                patch.object(update.publish_data, "verify_live"),
                patch.object(update, "cleanup_publication", return_value="probe"),
            ):
                self.assertEqual(update.update(args)["commit"], "probe")
            self.assertEqual(commit.call_args.args[2], manifest)
            self.assertEqual(commit.call_args.args[3], state)
            self.assertIsNone(state["last_success_at"])

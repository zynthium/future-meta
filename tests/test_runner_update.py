import copy
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
import shutil
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import data_branch as branch
import runner_update as runner
import update_official as update
from test_data_branch import snapshot


class RunnerUpdateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name) / "work"
        self.rules = Path(self.temp.name) / "rules.json"
        self.rules.write_text('{"schema":1}')
        self.args = SimpleNamespace(repo="repo", work_dir=self.work, rules=self.rules,
                                    offline_smoke=False, runner="ubuntu-latest", phase="prepare")
        self.env = patch.dict(os.environ, {"GITHUB_RUN_ID": "20", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_SHA": "code"})
        self.env.start()
        self.addCleanup(self.env.stop)

    def test_github_outputs_never_include_multiline_transport_errors(self):
        output = Path(self.temp.name) / "github-output"
        with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}):
            runner.emit({"needs_backup": True, "error": "navigation failed\ncomplete=true"})
        self.assertEqual(output.read_text(), "needs_backup=true\n")

    def test_offline_smoke_cannot_fall_through_to_legacy_online_cli(self):
        for flags in (("--offline-smoke",), ("--republish-current", "--offline-smoke")):
            with self.subTest(flags=flags), patch.object(sys, "argv", ["update_official.py", "--work-dir", str(self.work), *flags]), patch.object(update, "update") as online:
                with self.assertRaises(SystemExit):
                    update.main()
                online.assert_not_called()

    def test_offline_flag_is_also_rejected_by_direct_legacy_entrypoint(self):
        self.args.offline_smoke = True
        self.args.republish_current = False
        with patch.object(branch, "read") as read:
            with self.assertRaisesRegex(ValueError, "offline"):
                update.update(self.args)
        read.assert_not_called()

    def test_outside_window_does_not_read_or_write_production(self):
        with patch.object(update, "scheduled_decision", return_value={"should_update": False, "reason": "outside_access_window"}), patch.object(branch, "read") as read, patch.object(branch, "commit") as commit:
            result = runner.prepare(self.args)
        self.assertFalse(result["prepared"])
        read.assert_not_called()
        commit.assert_not_called()

    def test_duplicate_slot_never_downloads_or_starts_a_second_round(self):
        manifest, state = snapshot()
        state = branch.begin_attempt(state, "2026-11-02:evening", "2026-11-02T10:45:00Z", "url", "old")
        with patch.object(update, "scheduled_decision", return_value={"should_update": True, "slot": "evening", "evaluated_at": "2026-11-02T18:45:00+08:00"}), patch.object(branch, "read", return_value=("parent", manifest, state)), patch.object(branch, "commit") as commit, patch.object(update.publish_data, "download_asset") as download:
            result = runner.prepare(self.args)
        self.assertFalse(result["prepared"])
        commit.assert_not_called()
        download.assert_not_called()

    def test_orphaned_owned_round_is_failed_without_advancing_checkpoint(self):
        manifest, state = snapshot()
        before = copy.deepcopy(state)
        state = branch.begin_attempt(state, "slot", "2026-11-02T10:45:00Z", "url", "20")
        state["last_attempt"].update(workflow_attempt="1", source_commit="code")
        with patch.object(branch, "read", return_value=("parent", manifest, state)), patch.object(branch, "commit") as commit:
            with self.assertRaisesRegex(RuntimeError, "handoff"):
                runner.finalize(self.args)
        failed = commit.call_args.args[3]
        self.assertEqual(failed["last_attempt"]["status"], "failed")
        self.assertEqual(failed["checked_through"], before["checked_through"])
        self.assertEqual(failed["last_success_at"], before["last_success_at"])

    def test_orphan_finalizer_cannot_modify_another_attempt(self):
        manifest, state = snapshot()
        state = branch.begin_attempt(state, "slot", "2026-11-02T10:45:00Z", "url", "another")
        with patch.object(branch, "read", return_value=("parent", manifest, state)), patch.object(branch, "commit") as commit:
            with self.assertRaises(RuntimeError):
                runner.finalize(self.args)
        commit.assert_not_called()

    def smoke_snapshot(self):
        manifest, state = snapshot()
        body = b"smoke baseline archive, not parsed or published"
        manifest.update(size=len(body), sha256=hashlib.sha256(body).hexdigest())
        state["archive_sha256"] = manifest["sha256"]
        self.args.offline_smoke = True
        return manifest, state, body

    def test_offline_two_runner_handoff_reuses_success_and_preserves_global_budget(self):
        manifest, state, body = self.smoke_snapshot()
        with patch.object(branch, "read", return_value=("parent", manifest, state)), patch.object(branch, "commit") as commit, patch.object(update.publish_data, "download_asset", return_value=body), patch.object(update.publish_data, "publish") as publish:
            self.assertTrue(runner.prepare(self.args)["prepared"])
            self.args.phase = "primary"
            primary = runner.collect(self.args)
            self.assertTrue(primary["needs_backup"])
            self.assertFalse(primary["complete"])
            backup_dir = Path(self.temp.name) / "another-runner"
            shutil.copytree(self.work, backup_dir)
            self.args.work_dir = backup_dir
            self.args.phase, self.args.runner = "backup", "macos-15"
            backup = runner.collect(self.args)
            self.assertTrue(backup["complete"], backup)
            result = runner.finalize(self.args)
        self.assertEqual(result["actual_exchange_requests"], 0)
        self.assertEqual(result["cached_file_attempts"], 1)
        self.assertEqual(result["recovered_file_attempts"], 3)
        commit.assert_not_called()
        publish.assert_not_called()

    def test_handoff_rejects_changed_code_attempt_rules_or_archive(self):
        manifest, state, body = self.smoke_snapshot()
        with patch.object(branch, "read", return_value=("parent", manifest, state)), patch.object(update.publish_data, "download_asset", return_value=body):
            runner.prepare(self.args)
        for field, value in (("GITHUB_SHA", "different"), ("GITHUB_RUN_ID", "different"), ("GITHUB_RUN_ATTEMPT", "2")):
            with self.subTest(field=field), patch.dict(os.environ, {field: value}), self.assertRaisesRegex(ValueError, "handoff"):
                runner.load(self.args)
        self.rules.write_text("different rules")
        with self.assertRaisesRegex(ValueError, "handoff"):
            runner.load(self.args)
        self.rules.write_text('{"schema":1}')
        (self.work / "baseline.fmeta.zst").write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "baseline"):
            runner.load(self.args)

    def test_expired_continuation_and_stage_replay_cannot_request_more_files(self):
        manifest, state, body = self.smoke_snapshot()
        with patch.object(branch, "read", return_value=("parent", manifest, state)), patch.object(update.publish_data, "download_asset", return_value=body):
            runner.prepare(self.args)
        self.args.phase = "primary"
        with patch.object(runner, "now", return_value=datetime.now(timezone.utc) + timedelta(days=1)):
            result = runner.collect(self.args)
        self.assertFalse(result["needs_backup"])
        self.assertIn("window closed", result["error"])
        self.assertFalse((self.work / "transport.json").exists())
        with self.assertRaisesRegex(ValueError, "replay"):
            runner.collect(self.args)

    def test_missing_primary_handoff_cannot_refund_unknown_request_budgets(self):
        manifest, state, body = self.smoke_snapshot()
        with patch.object(branch, "read", return_value=("parent", manifest, state)), patch.object(update.publish_data, "download_asset", return_value=body):
            runner.prepare(self.args)
        self.args.phase, self.args.runner = "backup", "macos-15"
        result = runner.collect(self.args)
        self.assertFalse(result["complete"])
        self.assertIn("primary handoff", result["error"].lower())
        self.assertFalse((self.work / "transport.json").exists())

    def test_explicit_setup_failure_proof_allows_a_fresh_backup(self):
        manifest, state, body = self.smoke_snapshot()
        with patch.object(branch, "read", return_value=("parent", manifest, state)), patch.object(update.publish_data, "download_asset", return_value=body):
            runner.prepare(self.args)
        self.args.phase, self.args.runner = "backup", "macos-15"
        with patch.dict(os.environ, {"SAFE_SETUP_RECOVERY": "true"}), patch("runner_smoke.collect", return_value={"offline_smoke": True, "files": 2}):
            result = runner.collect(self.args)
        self.assertTrue(result["complete"], result)

    def test_entered_primary_without_ledger_cannot_continue_on_a_new_runner(self):
        manifest, state, body = self.smoke_snapshot()
        with patch.object(branch, "read", return_value=("parent", manifest, state)), patch.object(update.publish_data, "download_asset", return_value=body):
            runner.prepare(self.args)
        runner.write(self.work / "stage-primary.json", {"needs_backup": True, "complete": False})
        self.args.phase, self.args.runner = "backup", "macos-15"
        result = runner.collect(self.args)
        self.assertFalse(result["complete"])
        self.assertIn("ledger", result["error"].lower())
        self.assertFalse((self.work / "transport.json").exists())

    def production_handoff(self):
        manifest, state, body = self.smoke_snapshot()
        self.args.offline_smoke = False
        started = branch.begin_attempt(state, "2026-11-02:evening", "2026-11-02T10:45:00Z", "url", "20")
        started["last_attempt"].update(workflow_attempt="1", source_commit="code")
        session = {
            "schema": 1, "repo": "repo", "run_id": "20", "run_attempt": "1",
            "source_commit": "code", "offline_smoke": False,
            "deadline": "2026-11-02T20:00:00+08:00",
            "rules_sha256": runner.digest(self.rules.read_bytes()),
            "parent": "started", "manifest": manifest, "started": started,
            "days": ["2026-11-02"], "previous_through": "2026-10-30",
            "primary_runner": "ubuntu-latest", "backup_runner": "macos-15",
        }
        self.work.mkdir()
        runner.write(self.work / "session.json", session)
        (self.work / "baseline.fmeta.zst").write_bytes(body)
        runner.write(self.work / "collection.json", {
            "session_sha256": runner.session_digest(session),
            "snapshots": [{"exchange": ex, "trade_date": "2026-11-02", "rows": []} for ex in branch.EXCHANGES],
            "source_versions": {"verified": True}, "exceptions": {},
        })
        return manifest, started

    def test_final_validator_failure_cannot_publish_or_advance_any_exchange(self):
        manifest, started = self.production_handoff()
        with patch.object(branch, "read", return_value=("started", manifest, started)), patch.object(branch, "commit") as commit, patch.object(update, "daemon", side_effect=ValueError("incomplete six-exchange batch")), patch.object(update.publish_data, "publish") as publish:
            with self.assertRaisesRegex(ValueError, "six-exchange"):
                runner.finalize(self.args)
        publish.assert_not_called()
        failed = commit.call_args.args[3]
        self.assertEqual(failed["last_attempt"]["status"], "failed")
        self.assertEqual(failed["checked_through"], started["checked_through"])
        self.assertEqual(failed["archive_sha256"], manifest["sha256"])

    def test_success_is_published_once_only_after_the_complete_batch_validator(self):
        manifest, started = self.production_handoff()
        current = ["started", manifest, started]
        events = []

        def validate(*args):
            batch = json.loads((self.work / "batch.json").read_text())
            self.assertEqual({snapshot["exchange"] for snapshot in batch["snapshots"]}, set(branch.EXCHANGES))
            events.append("validated")
            runner.write(self.work / "publication" / "update-report.json", {"changed": False})

        def publish(source, repo, sha, parent, doc, old, finished):
            self.assertIsNone(source)
            self.assertEqual(events, ["validated"])
            self.assertEqual(set(finished["checked_through"].values()), {"2026-11-02"})
            self.assertEqual(doc, manifest)
            events.append("published")
            current[:] = ["published", manifest, finished]
            return "published"

        with patch.object(branch, "read", side_effect=lambda repo: tuple(current)), patch.object(branch, "commit") as commit, patch.object(update, "daemon", side_effect=validate), patch.object(update.publish_data, "publish", side_effect=publish) as publication, patch.object(update, "cleanup_publication", return_value="published"):
            result = runner.finalize(self.args)
        self.assertFalse(result["publish"])
        self.assertEqual(events, ["validated", "published"])
        self.assertEqual(current[2]["last_attempt"]["status"], "unchanged")
        publication.assert_called_once()
        commit.assert_not_called()


if __name__ == "__main__":
    unittest.main()

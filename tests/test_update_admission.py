import contextlib
import copy
from datetime import datetime
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import update_admission as admission
import update_official
from test_data_branch import snapshot


def at(value):
    return datetime.fromisoformat(value + "+08:00")


class AdmissionTests(unittest.TestCase):
    def setUp(self):
        self.manifest, self.state = snapshot()
        self.read = patch.object(
            admission.branch, "read", return_value=("commit", self.manifest, self.state)
        ).start()
        self.ci = patch.object(
            admission, "verified_ci", return_value="https://github.com/full-ci"
        ).start()
        self.write = patch.object(
            admission.branch,
            "commit",
            side_effect=AssertionError("admission is read-only"),
        ).start()
        self.addCleanup(patch.stopall)

    def test_weekend_skips_without_github_or_calendar_access(self):
        with patch.object(
            admission.schedule,
            "load_calendar",
            side_effect=AssertionError("calendar read"),
        ):
            result = admission.assess("repo", at("2027-01-03T18:35"))
        self.assertTrue(result["ok"])
        self.assertEqual(result["reason"], "weekend")
        self.read.assert_not_called()
        self.ci.assert_not_called()

    def test_first_candidate_admits_one_catchup_batch(self):
        result = admission.assess("repo", at("2026-11-03T18:35"))
        self.assertTrue(result["should_update"])
        self.assertTrue(result["ok"])
        self.assertEqual(result["slot"], "2026-11-03:evening")
        self.assertEqual(result["target_through"], "2026-11-03")
        self.ci.assert_called_once_with("repo")
        self.write.assert_not_called()

    def test_no_change_success_skips_later_wakes_before_ci_or_installation(self):
        self.state["checked_through"] = {
            e: "2026-11-03" for e in admission.branch.EXCHANGES
        }
        self.state["last_attempt"] = {
            "slot": "2026-11-03:evening",
            "status": "unchanged",
        }
        before = copy.deepcopy(self.state)
        result = admission.assess("repo", at("2026-11-03T19:20"))
        self.assertEqual(result["reason"], "already_checked")
        self.assertTrue(result["ok"])
        self.assertFalse(result["should_update"])
        self.ci.assert_not_called()
        self.assertEqual(self.state, before)

    def test_failed_or_interrupted_slot_is_red_without_recollection(self):
        for status in ("failed", "running"):
            with self.subTest(status=status):
                self.state["last_attempt"] = {
                    "slot": "2026-11-03:evening",
                    "status": status,
                }
                result = admission.assess("repo", at("2026-11-03T19:05"))
                self.assertFalse(result["should_update"])
                self.assertFalse(result["ok"])
                self.assertEqual(result["reason"], "slot_already_attempted")
        self.ci.assert_not_called()

    def test_missing_full_ci_blocks_collection_without_reserving_slot(self):
        self.ci.return_value = None
        result = admission.assess("repo", at("2026-11-03T18:35"))
        self.assertFalse(result["ok"])
        self.assertFalse(result["should_update"])
        self.assertEqual(result["reason"], "ci_not_verified")
        self.assertIsNone(self.state["last_attempt"])
        self.write.assert_not_called()

    def test_observed_delayed_wakes_detect_missed_previous_evening(self):
        for value, target in (
            ("2026-11-02T12:47", "2026-10-30"),
            ("2026-11-03T04:30", "2026-11-02"),
            ("2026-11-03T13:38", "2026-11-02"),
            ("2026-11-03T20:17", "2026-11-03"),
        ):
            with self.subTest(value=value):
                result = admission.assess("repo", at(value))
                self.assertFalse(result["should_update"])
                self.assertEqual(result["target_through"], target)
                self.assertEqual(result["ok"], target == "2026-10-30")
                if not result["ok"]:
                    self.assertEqual(result["reason"], "missed_update")
        self.ci.assert_not_called()

    def test_ordinary_monday_morning_needs_only_friday_checkpoint(self):
        result = admission.assess("repo", at("2026-11-02T08:07"))
        self.assertTrue(result["ok"])
        self.assertFalse(result["should_update"])
        self.assertEqual(result["target_through"], "2026-10-30")
        self.ci.assert_not_called()

    def test_health_wake_delayed_into_valid_window_stays_read_only(self):
        result = admission.assess("repo", at("2026-11-03T18:35"), health_only=True)
        self.assertFalse(result["should_update"])
        self.assertFalse(result["ok"])
        self.assertEqual(result["target_through"], "2026-11-02")
        self.ci.assert_not_called()

    def test_explicit_postcheck_catches_setup_crossing_window(self):
        args = type("Args", (), {"republish_current": False})()
        with patch.object(
            update_official,
            "scheduled_decision",
            return_value={"should_update": False, "reason": "outside_access_window"},
        ):
            skipped = update_official.update(args)
        self.assertFalse(skipped["publish"])
        result = admission.verify_through("repo", "2026-11-03")
        self.assertFalse(result["ok"])
        self.assertEqual(result["reason"], "missed_update")
        self.ci.assert_not_called()

    def test_successful_postcheck_does_not_require_changed_archive_or_timestamp(self):
        self.state["checked_through"] = {
            e: "2026-11-03" for e in admission.branch.EXCHANGES
        }
        result = admission.verify_through("repo", "2026-11-03")
        self.assertTrue(result["ok"])
        self.ci.assert_not_called()
        self.write.assert_not_called()

    def test_invalid_postcheck_target_is_rejected_before_state_read(self):
        for value in ("", "2026-13-01", "20261103"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                admission.verify_through("repo", value)
        self.read.assert_not_called()

    def test_failed_check_exits_nonzero_and_emits_reviewable_outputs(self):
        result = admission.assess("repo", at("2026-11-03T20:17"))
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "output"
            summary = Path(temp) / "summary"
            with (
                patch.object(
                    sys, "argv", ["admission", "--github-output", str(output)]
                ),
                patch.object(admission, "assess", return_value=result),
                patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}),
                contextlib.redirect_stdout(io.StringIO()) as stream,
                self.assertRaises(SystemExit) as error,
            ):
                admission.main()
            self.assertEqual(error.exception.code, 1)
            self.assertIn("should_update=false", output.read_text())
            self.assertIn("target_through=2026-11-03", output.read_text())
            self.assertIn("missed_update", summary.read_text())
            self.assertIn("::error::", stream.getvalue())

    def test_explicit_republish_does_not_claim_freshness_or_read_state(self):
        with (
            patch.object(sys, "argv", ["admission", "--republish-current"]),
            patch.dict(os.environ, {}, clear=True),
            contextlib.redirect_stdout(io.StringIO()) as stream,
        ):
            admission.main()
        self.assertEqual(
            json.loads(stream.getvalue())["reason"], "explicit_publication_verification"
        )
        self.read.assert_not_called()
        self.ci.assert_not_called()


class FullCiTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "user.name", "test")
        self.addCleanup(patch.stopall)
        patch.object(admission, "ROOT", self.root).start()

    def git(self, *args):
        return subprocess.run(
            ["git", "-C", str(self.root), *args],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    def commit(self, path, content):
        (self.root / path).write_text(content)
        self.git("add", path)
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def verify(self, runs, jobs):
        def api(repo, endpoint, method="GET", body=None):
            self.assertEqual(repo, "repo")
            self.assertEqual(method, "GET")
            self.assertIsNone(body)
            if endpoint.startswith("actions/workflows/ci.yml/runs?"):
                self.assertIn("event=push", endpoint)
                self.assertIn("branch=main", endpoint)
                self.assertIn("status=success", endpoint)
                return {"workflow_runs": runs}
            return {"jobs": jobs[int(endpoint.split("/")[2])]}

        with patch.object(admission.branch, "api", side_effect=api):
            return admission.verified_ci("repo")

    @staticmethod
    def run_entry(sha, run_id):
        return {
            "head_sha": sha,
            "id": run_id,
            "html_url": f"https://github.com/ci/{run_id}",
        }

    def test_successful_code_ci_with_identical_head(self):
        sha = self.commit("runtime.py", "approved")
        self.assertEqual(
            self.verify(
                [self.run_entry(sha, 1)],
                {1: [{"name": "code", "conclusion": "success"}]},
            ),
            "https://github.com/ci/1",
        )

    def test_docs_only_push_reuses_previous_full_code_ci(self):
        code = self.commit("runtime.py", "approved")
        docs = self.commit("README.md", "documentation")
        runs = [self.run_entry(docs, 2), self.run_entry(code, 1)]
        jobs = {
            2: [{"name": "code", "conclusion": "skipped"}],
            1: [{"name": "code", "conclusion": "success"}],
        }
        self.assertEqual(self.verify(runs, jobs), "https://github.com/ci/1")

    def test_green_docs_ci_cannot_hide_unverified_code(self):
        code = self.commit("runtime.py", "approved")
        self.commit("runtime.py", "unchecked change")
        docs = self.commit("README.md", "documentation")
        runs = [self.run_entry(docs, 2), self.run_entry(code, 1)]
        jobs = {
            2: [{"name": "code", "conclusion": "skipped"}],
            1: [{"name": "code", "conclusion": "success"}],
        }
        self.assertIsNone(self.verify(runs, jobs))

    def test_unrelated_ci_commit_is_not_trusted(self):
        self.commit("runtime.py", "approved")
        tree = self.git("rev-parse", "HEAD^{tree}")
        unrelated = self.git("commit-tree", tree, "-m", "unrelated root")
        self.assertIsNone(self.verify([self.run_entry(unrelated, 1)], {}))

    def test_failed_or_missing_code_job_is_not_trusted(self):
        sha = self.commit("runtime.py", "approved")
        for jobs in (
            [],
            [{"name": "code", "conclusion": "failure"}],
            [{"name": "documentation", "conclusion": "success"}],
        ):
            with self.subTest(jobs=jobs):
                self.assertIsNone(self.verify([self.run_entry(sha, 1)], {1: jobs}))


if __name__ == "__main__":
    unittest.main()

import base64
import copy
import json
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import data_branch as branch


def snapshot():
    manifest = {
        "schema_version": 4,
        "sha256": "a" * 64,
        "size": 123,
        "artifact": "latest.fmeta.zst",
        "data_version": "2026-10-30T11:00:00Z",
        "generated_at": "2026-10-30T11:00:00Z",
        "history_start": "2018-01-02T00:00:00+08:00",
        "history_end": "2026-10-30T00:00:00+08:00",
        "mirrors": [
            "https://github.com/zynthium/future-meta/releases/download/data-20261030T110000Z-aaaaaaaaaaaa/latest.fmeta.zst"
        ],
    }
    state = {
        "schema": 1,
        "archive_sha256": manifest["sha256"],
        "checked_through": {e: "2026-10-30" for e in branch.EXCHANGES},
        "last_attempt": None,
        "last_success_at": None,
        "current_release": "data-20261030T110000Z-aaaaaaaaaaaa",
        "previous_release": None,
        "exceptions": {},
        "source_versions": {},
    }
    return manifest, state


class BranchTests(unittest.TestCase):
    def test_attempt_records_start_and_prevents_same_slot_duplicate(self):
        _, state = snapshot()
        started = branch.begin_attempt(
            state,
            "2026-11-02:evening",
            "2026-11-02T10:45:00Z",
            "https://github.com/runs/1",
            "1",
        )
        self.assertEqual(started["last_attempt"]["status"], "running")
        self.assertIsNone(started["last_success_at"])
        self.assertEqual(state["last_attempt"], None)
        with self.assertRaisesRegex(ValueError, "already attempted"):
            branch.begin_attempt(
                started, "2026-11-02:evening", "2026-11-02T10:50:00Z", "url", "2"
            )

    def test_failure_preserves_archive_and_successful_checkpoint(self):
        manifest, state = snapshot()
        state = branch.begin_attempt(
            state, "2026-11-02:evening", "2026-11-02T10:45:00Z", "url", "1"
        )
        failed = branch.finish_attempt(
            state, "failed", "2026-11-02T10:46:00Z", error="source unavailable"
        )
        self.assertEqual(failed["checked_through"], state["checked_through"])
        self.assertEqual(failed["archive_sha256"], manifest["sha256"])
        self.assertIsNone(failed["last_success_at"])
        self.assertEqual(failed["last_attempt"]["status"], "failed")

    def test_no_change_advances_check_time_but_not_manifest(self):
        manifest, state = snapshot()
        before = copy.deepcopy(manifest)
        state = branch.begin_attempt(
            state, "2026-11-02:evening", "2026-11-02T10:45:00Z", "url", "1"
        )
        finished = branch.finish_attempt(
            state, "unchanged", "2026-11-02T10:46:00Z", through="2026-11-02"
        )
        branch.validate(manifest, finished)
        self.assertEqual(finished["last_success_at"], "2026-11-02T10:46:00Z")
        self.assertEqual(set(finished["checked_through"].values()), {"2026-11-02"})
        self.assertEqual(manifest, before)

    def test_commit_contains_only_two_small_files_and_uses_nonforce_ref_update(self):
        manifest, state = snapshot()
        calls = []

        def api(repo, endpoint, method="GET", body=None):
            calls.append((endpoint, method, body))
            if endpoint == "git/ref/heads/data":
                return {"object": {"sha": "parent"}}
            if endpoint == "git/trees":
                return {"sha": "tree"}
            if endpoint == "git/commits":
                return {"sha": "commit"}
            if endpoint == "git/refs/heads/data":
                return {"object": {"sha": "commit"}}
            raise AssertionError(endpoint)

        with patch.object(branch, "api", side_effect=api):
            self.assertEqual(
                branch.commit(
                    "zynthium/future-meta", "parent", manifest, state, "checked"
                ),
                "commit",
            )
        tree = next(body for endpoint, _, body in calls if endpoint == "git/trees")
        self.assertEqual(
            {entry["path"] for entry in tree["tree"]},
            {"manifest.json", "update-state.json"},
        )
        self.assertNotIn("base_tree", tree)
        ref = calls[-1]
        self.assertEqual(ref[2], {"sha": "commit", "force": False})

    def test_stale_writer_does_not_write_any_git_objects(self):
        manifest, state = snapshot()
        with patch.object(
            branch, "api", return_value={"object": {"sha": "newer"}}
        ) as api:
            with self.assertRaisesRegex(ValueError, "changed"):
                branch.commit("zynthium/future-meta", "old", manifest, state, "checked")
            self.assertEqual(api.call_count, 1)

    def test_read_pins_both_files_to_same_commit_and_checks_archive_binding(self):
        manifest, state = snapshot()

        def api(repo, endpoint, method="GET", body=None):
            if endpoint == "git/ref/heads/data":
                return {"object": {"sha": "pinned"}}
            self.assertTrue(endpoint.endswith("?ref=pinned"))
            doc = manifest if "/manifest.json?" in endpoint else state
            return {
                "encoding": "base64",
                "content": base64.b64encode(json.dumps(doc).encode()).decode(),
            }

        with patch.object(branch, "api", side_effect=api):
            actual = branch.read("repo")
        self.assertEqual(actual, ("pinned", manifest, state))
        state["archive_sha256"] = "b" * 64
        with self.assertRaisesRegex(ValueError, "SHA"):
            branch.validate(manifest, state)


if __name__ == "__main__":
    unittest.main()


class ActionsIdentityTests(unittest.TestCase):
    def test_app_token_commit_has_explicit_bot_identity(self):
        manifest, state = snapshot()
        bodies = []

        def api(repo, endpoint, method="GET", body=None):
            if endpoint == "git/ref/heads/data":
                return {"object": {"sha": "parent"}}
            if endpoint == "git/trees":
                return {"sha": "tree"}
            if endpoint == "git/commits":
                bodies.append(body)
                return {"sha": "commit"}
            return {"object": {"sha": "commit"}}

        with (
            patch.dict("os.environ", {"GITHUB_ACTIONS": "true"}),
            patch.object(branch, "api", side_effect=api),
        ):
            branch.commit("repo", "parent", manifest, state, "verify")
        self.assertEqual(bodies[0]["author"]["name"], "github-actions[bot]")
        self.assertEqual(bodies[0]["committer"], bodies[0]["author"])

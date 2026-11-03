import copy
import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import publish_data as publish
import data_branch as branch
from test_data_branch import snapshot


class PublicationTests(unittest.TestCase):
    def test_raw_cdn_can_propagate_after_the_former_six_poll_limit(self):
        manifest, state = snapshot()
        stale = dict(manifest, sha256="0" * 64)
        responses = [json.dumps(doc).encode() for doc in (
            *[doc for _ in range(7) for doc in (stale, state)], manifest, state
        )]
        with (
            patch.object(branch, "read", return_value=("commit", manifest, state)),
            patch.object(publish, "download_asset", side_effect=responses),
            patch.object(publish.time, "sleep") as sleep,
        ):
            publish.verify_live("repo", "commit", manifest, state)
        self.assertGreater(sum(call.args[0] for call in sleep.call_args_list), 25)

    def test_permanently_stale_raw_metadata_still_fails_with_bounded_waits(self):
        manifest, state = snapshot()
        with (
            patch.object(branch, "read", return_value=("commit", manifest, state)),
            patch.object(publish, "download_asset", return_value=b"{}") as download,
            patch.object(publish.time, "sleep") as sleep,
        ):
            with self.assertRaisesRegex(ValueError, "has not propagated"):
                publish.verify_live("repo", "commit", manifest, state)
        self.assertLessEqual(download.call_count, 18)
        self.assertLessEqual(sum(call.args[0] for call in sleep.call_args_list), 305)
        self.assertLessEqual(max(call.args[0] for call in sleep.call_args_list), 60)

    def test_no_change_never_creates_release_or_replaces_manifest(self):
        manifest, state = snapshot()
        old = copy.deepcopy(manifest)
        state = branch.begin_attempt(
            state, "2026-11-02:evening", "2026-11-02T10:45:00Z", "url", "1"
        )
        finished = branch.finish_attempt(
            state, "unchanged", "2026-11-02T10:46:00Z", through="2026-11-02"
        )
        with (
            patch.object(publish, "ensure_release") as release,
            patch.object(branch, "commit", return_value="new") as commit,
        ):
            self.assertEqual(
                publish.publish(
                    None, "repo", "main", "parent", manifest, state, finished
                ),
                "new",
            )
        release.assert_not_called()
        self.assertEqual(commit.call_args.args[2], old)
        self.assertEqual(commit.call_args.args[3]["archive_sha256"], manifest["sha256"])

    def test_changed_release_verified_before_checkpoint_commit(self):
        manifest, state = snapshot()
        state = branch.begin_attempt(state, "slot", "2026-11-02T10:45:00Z", "url", "1")
        finished = branch.finish_attempt(
            state, "changed", "2026-11-02T10:46:00Z", through="2026-11-02"
        )
        events = []
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            body = b"new compressed archive"
            new = dict(
                manifest,
                generated_at="2026-11-02T10:45:00Z",
                data_version="2026-11-02T10:45:00Z",
                sha256=hashlib.sha256(body).hexdigest(),
                size=len(body),
                mirrors=[],
            )
            (root / "manifest.json").write_text(json.dumps(new))
            (root / "latest.fmeta.zst").write_bytes(body)

            def release(repo, target, tag, artifact, doc):
                events.append("release")
                return f"https://github.com/{repo}/releases/download/{tag}/latest.fmeta.zst"

            def commit(repo, parent, doc, checkpoint, message):
                events.append("commit")
                self.assertEqual(checkpoint["archive_sha256"], new["sha256"])
                self.assertEqual(
                    checkpoint["previous_release"], state["current_release"]
                )
                self.assertEqual(
                    doc["mirrors"][0].split("/")[-2], checkpoint["current_release"]
                )
                return "new"

            with (
                patch.object(publish, "ensure_release", side_effect=release),
                patch.object(branch, "commit", side_effect=commit),
            ):
                publish.publish(
                    root, "repo", "main", "parent", manifest, state, finished
                )
        self.assertEqual(events, ["release", "commit"])

    def test_upload_failure_does_not_advance_branch(self):
        manifest, state = snapshot()
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            body = b"archive"
            new = dict(
                manifest, sha256=hashlib.sha256(body).hexdigest(), size=len(body)
            )
            (root / "manifest.json").write_text(json.dumps(new))
            (root / "latest.fmeta.zst").write_bytes(body)
            with (
                patch.object(
                    publish, "ensure_release", side_effect=RuntimeError("upload failed")
                ),
                patch.object(branch, "commit") as commit,
            ):
                with self.assertRaisesRegex(RuntimeError, "upload failed"):
                    publish.publish(
                        root, "repo", "main", "parent", manifest, state, state
                    )
            commit.assert_not_called()

    def test_tampered_staged_archive_rejected_before_release(self):
        manifest, state = snapshot()
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "manifest.json").write_text(json.dumps(manifest))
            (root / "latest.fmeta.zst").write_bytes(b"tampered")
            with patch.object(publish, "ensure_release") as release:
                with self.assertRaisesRegex(ValueError, "checksum"):
                    publish.publish(
                        root, "repo", "main", "parent", manifest, state, state
                    )
                release.assert_not_called()

    def test_cleanup_requires_canonical_readback_and_never_deletes_sdk(self):
        manifest, state = snapshot()
        managed = {
            "tag_name": "data-20261029T110000Z-bbbbbbbbbbbb",
            "body": publish.MARKER,
        }
        sdk = {"tag_name": "v0.6.0", "body": publish.MARKER}
        orphan = {
            "tag_name": "data-20261102T110000Z-cccccccccccc",
            "body": publish.MARKER,
        }
        state["retired_releases"] = [managed["tag_name"]]
        keep = {"tag_name": state["current_release"], "body": publish.MARKER}
        with (
            patch.object(publish, "verify_live", return_value=None) as verify,
            patch.object(publish, "gh") as gh,
        ):
            gh.return_value.stdout = json.dumps([[managed, sdk, keep, orphan]])
            publish.cleanup("repo", "commit", manifest, state)
            verify.assert_called_once()
            deletions = [
                c.args for c in gh.call_args_list if c.args[:2] == ("release", "delete")
            ]
            self.assertEqual(len(deletions), 1)
            self.assertEqual(deletions[0][2], managed["tag_name"])
        with (
            patch.object(publish, "verify_live", side_effect=ValueError("stale")),
            patch.object(publish, "gh") as gh,
        ):
            with self.assertRaisesRegex(ValueError, "stale"):
                publish.cleanup("repo", "commit", manifest, state)
            gh.assert_not_called()


if __name__ == "__main__":
    unittest.main()

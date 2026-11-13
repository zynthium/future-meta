import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import ci_scope
import check_sdk_release as release


class ReleaseRulesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-b", "main")
        (self.repo / "crates/future-meta/src").mkdir(parents=True)
        (self.repo / "crates/future-meta/src/lib.rs").write_text("pub fn original() {}\n")
        self.write_version("0.8.0")
        self.commit()
        self.git("tag", "v0.8.0")

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.repo), "-c", "user.name=Test", "-c", "user.email=test@example.invalid", *args],
            text=True,
            stderr=subprocess.PIPE,
        ).strip()

    def write_version(self, version):
        (self.repo / "crates/future-meta/Cargo.toml").write_text(
            f'[package]\nname = "future-meta"\nversion = "{version}"\n'
        )
        (self.repo / "Cargo.lock").write_text(
            f'[[package]]\nname = "future-meta"\nversion = "{version}"\n'
        )

    def commit(self, update_main=True):
        self.git("add", "--all")
        self.git("commit", "-m", "fixture")
        if update_main:
            self.git("update-ref", "refs/remotes/origin/main", "HEAD")

    def new_release(self, version, update_main=True):
        self.write_version(version)
        self.commit(update_main)
        self.git("tag", "v" + version)

    def test_documentation_only_does_not_request_full_code_checks(self):
        self.assertTrue(ci_scope.documentation_only([
            "README.md", "AGENTS.md", "docs/releasing.md", "scripts/README.md",
            "skills/future-meta/SKILL.md", "skills/future-meta/agents/openai.yaml",
        ]))

    def test_mixed_runtime_workflow_or_unknown_changes_require_full_checks(self):
        for path in (
            "crates/future-meta/src/download.rs", "Cargo.lock", "config/official-rules.json",
            "scripts/update_official.py", ".github/workflows/ci.yml",
            "skills/future-meta/scripts/build.py", "skills/future-meta/config.yaml",
        ):
            with self.subTest(path=path):
                self.assertFalse(ci_scope.documentation_only(["docs/releasing.md", path]))

    def test_code_renamed_to_documentation_still_runs_code_checks(self):
        base = self.git("rev-parse", "HEAD")
        (self.repo / "docs").mkdir()
        self.git("mv", "crates/future-meta/src/lib.rs", "docs/retired.md")
        self.commit()
        paths = ci_scope.changed_paths(self.repo, base)
        self.assertIn("crates/future-meta/src/lib.rs", paths)
        self.assertFalse(ci_scope.documentation_only(paths))

    def test_multi_commit_push_cannot_hide_code_behind_last_documentation_commit(self):
        base = self.git("rev-parse", "HEAD")
        (self.repo / "crates/future-meta/src/lib.rs").write_text("pub fn changed() {}\n")
        self.commit()
        (self.repo / "README.md").write_text("Documentation\n")
        self.commit()
        self.assertFalse(ci_scope.documentation_only(ci_scope.changed_paths(self.repo, base)))

    def test_initial_push_checks_all_files(self):
        paths = ci_scope.changed_paths(self.repo, "0" * 40)
        self.assertIn("Cargo.lock", paths)
        self.assertFalse(ci_scope.documentation_only(paths))

    def test_missing_base_fails_instead_of_skipping_checks(self):
        with self.assertRaises(subprocess.CalledProcessError):
            ci_scope.changed_paths(self.repo, "not-a-commit")

    def test_release_notes_start_at_previous_sdk_not_data_tag(self):
        self.git("tag", "data-20990101T000000Z-aaaaaaaaaaaa")
        self.git("tag", "v-preview")
        self.new_release("0.8.1")
        self.assertEqual(
            release.validate_release(self.repo, "v0.8.1"),
            {"version": "0.8.1", "previous_tag": "v0.8.0"},
        )

    def test_sdk_versions_are_ordered_numerically(self):
        self.new_release("0.8.9")
        self.new_release("0.8.10")
        self.new_release("0.8.11")
        self.assertEqual(release.validate_release(self.repo, "v0.8.11")["previous_tag"], "v0.8.10")

    def test_first_sdk_release_has_no_previous_tag(self):
        self.git("tag", "-d", "v0.8.0")
        self.new_release("0.8.1")
        self.assertEqual(release.validate_release(self.repo, "v0.8.1")["previous_tag"], "")

    def test_unmerged_future_sdk_tag_is_not_used_for_release_notes(self):
        self.git("checkout", "-b", "future")
        self.new_release("0.9.0", update_main=False)
        self.git("checkout", "main")
        self.new_release("0.8.1")
        self.assertEqual(release.validate_release(self.repo, "v0.8.1")["previous_tag"], "v0.8.0")

    def test_sdk_tag_requires_matching_package_and_lock_versions(self):
        with self.assertRaisesRegex(ValueError, "Cargo.toml"):
            release.validate_release(self.repo, "v0.8.1")
        self.new_release("0.8.1")
        (self.repo / "Cargo.lock").write_text('[[package]]\nname="future-meta"\nversion="0.8.0"\n')
        with self.assertRaisesRegex(ValueError, "Cargo.lock"):
            release.validate_release(self.repo, "v0.8.1")

    def test_sdk_tag_requires_matching_python_binding_versions_when_present(self):
        (self.repo / "crates/future-meta-py").mkdir(parents=True)
        (self.repo / "crates/future-meta-py/Cargo.toml").write_text(
            '[package]\nname = "future-meta-py"\nversion = "0.8.0"\n'
        )
        self.write_version("0.8.1")
        self.commit()
        self.git("tag", "v0.8.1")
        with self.assertRaisesRegex(ValueError, "crates/future-meta-py/Cargo.toml"):
            release.validate_release(self.repo, "v0.8.1")
        (self.repo / "crates/future-meta-py/Cargo.toml").write_text(
            '[package]\nname = "future-meta-py"\nversion = "0.8.1"\n'
        )
        self.commit()
        self.git("tag", "-f", "v0.8.1")
        with self.assertRaisesRegex(ValueError, "future-meta-py in Cargo.lock"):
            release.validate_release(self.repo, "v0.8.1")
        (self.repo / "Cargo.lock").write_text(
            '[[package]]\nname = "future-meta"\nversion = "0.8.1"\n\n'
            '[[package]]\nname = "future-meta-py"\nversion = "0.8.1"\n'
        )
        self.commit()
        self.git("tag", "-f", "v0.8.1")
        self.assertEqual(release.validate_release(self.repo, "v0.8.1")["version"], "0.8.1")

    def test_tag_cannot_publish_a_commit_outside_main(self):
        self.new_release("0.8.1", update_main=False)
        with self.assertRaisesRegex(ValueError, "belong to main"):
            release.validate_release(self.repo, "v0.8.1")

    def test_tag_must_point_to_the_validated_checkout(self):
        self.new_release("0.8.1")
        (self.repo / "README.md").write_text("Later commit\n")
        self.commit()
        with self.assertRaisesRegex(ValueError, "checked-out commit"):
            release.validate_release(self.repo, "v0.8.1")

    def test_invalid_or_nonincreasing_sdk_versions_fail(self):
        for tag in ("0.8.0", "v00.8.0", "v0.8.1-rc.1", "data-20260101", "v-docs"):
            with self.subTest(tag=tag), self.assertRaisesRegex(ValueError, "stable vX.Y.Z"):
                release.validate_release(self.repo, tag)
        self.git("tag", "v0.9.0")
        with self.assertRaisesRegex(ValueError, "must increase"):
            release.validate_release(self.repo, "v0.8.0")


if __name__ == "__main__":
    unittest.main()

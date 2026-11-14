import unittest
from pathlib import Path


class UpdateFeeWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = Path(".github/workflows/update-fee-data.yml").read_text()

    def test_archive_increment_and_serialized_publication(self):
        self.assertIn("scripts/update_official.py", self.workflow)
        self.assertIn("cancel-in-progress: false", self.workflow)
        for obsolete in (
            "SEED_URL",
            "INPUTS_URL",
            "pages: write",
            "id-token: write",
            "github-pages",
            "upload-pages-artifact",
            "deploy-pages",
            "package_official_inputs",
            "update-collected",
        ):
            self.assertNotIn(obsolete, self.workflow)
        self.assertIn("contents: write", self.workflow)
        self.assertIn("persist-credentials: false", self.workflow)

    def test_lightweight_admission_and_runtime_collector(self):
        for expected in (
            'cron: "17 12 * * 1-5"',
            "health_only:",
            "needs: admission",
            "scripts/update_admission.py",
            '--github-output "$GITHUB_OUTPUT"',
            "actions: read",
            "fetch-depth: 0",
            "xvfb-run -a python",
            "playwright install --with-deps chrome",
        ):
            self.assertIn(expected, self.workflow)
        self.assertIn(
            "needs.admission.outputs.prepared == 'true'", self.workflow
        )
        self.assertNotIn("cloudflare", self.workflow.lower())

    def test_update_does_not_repeat_ci_and_postcheck_requires_explicit_target(self):
        self.assertNotIn('cargo test', self.workflow)
        self.assertNotIn('unittest discover', self.workflow)
        for expected in (
            'needs: [admission, update]',
            'needs.update.result != \'skipped\'',
            'inputs.republish_current != true',
            'TARGET_THROUGH: ${{ needs.admission.outputs.target_through }}',
            '--verify-through "$TARGET_THROUGH"',
        ):
            self.assertIn(expected, self.workflow)

    def test_health_crons_cannot_collect_even_if_delayed_into_access_window(self):
        self.assertIn("github.event.schedule == '17 12 * * 1-5'", self.workflow)
        self.assertIn("inputs.health_only", self.workflow)
        self.assertIn('args+=(--health-only)', self.workflow)

    def test_republish_is_explicit_and_uses_same_verified_entrypoint(self):
        self.assertIn("republish_current:", self.workflow)
        self.assertIn("--republish-current", self.workflow)
        self.assertIn('--repo "$GITHUB_REPOSITORY"', self.workflow)

    def test_diagnostics_exclude_originals_and_maintenance_packages(self):
        self.assertIn("if: always()", self.workflow)
        self.assertIn("retention-days: 3", self.workflow)
        self.assertIn("set -euo pipefail", self.workflow)
        diagnostics = self.workflow.split("- name: Retain summary diagnostics only", 1)[1].split("  verify:", 1)[0]
        for forbidden in (
            "data/official-update/raw",
            "collected/raw",
            "official-inputs",
            "sqlite.gz",
            "settlement-evidence/",
        ):
            self.assertNotIn(forbidden, diagnostics)

    def test_temporary_handoff_is_run_and_attempt_bound_and_deleted(self):
        self.assertIn("future-meta-handoff-${{ github.run_id }}-${{ github.run_attempt }}", self.workflow)
        self.assertEqual(self.workflow.count("retention-days: 1"), 3)
        self.assertIn("cleanup-handoff:", self.workflow)
        self.assertIn("permissions: {actions: write}", self.workflow)
        self.assertIn("artifact['name'] in names", self.workflow)
        self.assertNotIn("curl_cffi", self.workflow.split("  admission:", 1)[0])

    def test_alternate_runner_continues_with_read_only_collection_permissions(self):
        self.assertIn("runs-on: ${{ needs.admission.outputs.primary_runner }}", self.workflow)
        self.assertIn("runs-on: ${{ needs.admission.outputs.backup_runner }}", self.workflow)
        self.assertIn("needs.primary.outputs.needs_backup == 'true'", self.workflow)
        self.assertIn("needs.primary.outputs.safe_prepare_resume == 'true'", self.workflow)
        self.assertIn("if: always() && steps.collect.outcome == 'skipped'", self.workflow)
        self.assertNotIn("needs.primary.outputs.collection_started != 'true'", self.workflow)
        self.assertIn("--phase primary", self.workflow)
        self.assertIn("--phase backup", self.workflow)
        self.assertIn("--phase finalize", self.workflow)
        for name in ("primary", "backup"):
            block = self.workflow.split(f"  {name}:\n", 1)[1].split("\n  ", 1)[0]
            self.assertNotIn("contents: write", block)

    def test_offline_smoke_is_explicit_and_does_not_enter_checkpoint_verifier(self):
        self.assertIn("offline_smoke:", self.workflow)
        self.assertIn("--offline-smoke", self.workflow)
        self.assertIn("inputs.offline_smoke != true", self.workflow)

    def test_cancelled_runs_cannot_start_backup_collection_or_publication(self):
        for job in ("backup", "update", "verify"):
            block = self.workflow.split(f"  {job}:\n", 1)[1].split("    runs-on:", 1)[0]
            self.assertIn("!cancelled()", block)


if __name__ == "__main__":
    unittest.main()

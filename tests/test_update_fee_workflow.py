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

    def test_offline_admission_and_runtime_collector(self):
        for expected in (
            'cron: "45 10 * * 1-5"',
            'cron: "5 0 * * 1-5"',
            "needs: admission",
            "scripts/update_schedule.py --github-output",
            "xvfb-run -a python",
            "playwright install --with-deps chrome",
        ):
            self.assertIn(expected, self.workflow)
        self.assertIn(
            "if: needs.admission.outputs.should_update == 'true'", self.workflow
        )
        self.assertNotIn("cloudflare", self.workflow.lower())

    def test_republish_is_explicit_and_uses_same_verified_entrypoint(self):
        self.assertIn("republish_current:", self.workflow)
        self.assertIn("--republish-current", self.workflow)
        self.assertIn('--repo "$GITHUB_REPOSITORY"', self.workflow)

    def test_diagnostics_exclude_originals_and_maintenance_packages(self):
        self.assertIn("if: always()", self.workflow)
        self.assertIn("retention-days: 3", self.workflow)
        self.assertIn("set -euo pipefail", self.workflow)
        for forbidden in (
            "data/official-update/raw",
            "official-inputs",
            "sqlite.gz",
            "settlement-evidence/",
        ):
            self.assertNotIn(forbidden, self.workflow)


if __name__ == "__main__":
    unittest.main()

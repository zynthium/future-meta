import unittest
from pathlib import Path


class UpdateFeeWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = Path('.github/workflows/update-fee-data.yml').read_text()

    def test_official_increment_only_and_serialized_publication(self):
        self.assertIn('scripts/update_official.py', self.workflow)
        self.assertIn('cancel-in-progress: false', self.workflow)
        self.assertIn('ops/official-inputs.json', self.workflow)
        self.assertNotIn('update-latest', self.workflow)
        self.assertNotIn('validate-jin10', self.workflow)
        self.assertNotIn('seed-history', self.workflow)

    def test_verified_update_gates_deploy_and_holidays_skip(self):
        self.assertRegex(self.workflow, r"- name: Deploy to Cloudflare Pages\s+if: steps\.refresh\.outputs\.publish == 'true'")
        self.assertIn('verify-collected --db "$DB_PATH"', self.workflow)
        self.assertIn('xvfb-run -a python', self.workflow)
        self.assertIn('playwright install --with-deps chrome', self.workflow)
        self.assertIn('cron: "45 10 * * 1-5"', self.workflow)
        self.assertIn('cron: "5 0 * * 1-5"', self.workflow)
        self.assertIn('needs: admission', self.workflow)
        self.assertIn("if: needs.admission.outputs.should_update == 'true'", self.workflow)
        self.assertIn('scripts/update_schedule.py --github-output', self.workflow)
        self.assertIn('--scheduled', self.workflow)

    def test_failures_preserve_diagnostics(self):
        self.assertIn('set -euo pipefail', self.workflow)
        self.assertRegex(self.workflow, r'- name: Retain official update diagnostics\s+if: always\(\)')
        self.assertIn('actions/upload-artifact@v4', self.workflow)


if __name__ == '__main__':
    unittest.main()

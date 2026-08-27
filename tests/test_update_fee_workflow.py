import re
import unittest
from pathlib import Path


WORKFLOW = Path(".github/workflows/update-fee-data.yml")


class UpdateFeeWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")

    def test_deferred_fee_refresh_still_marks_seed_for_publication(self):
        self.assertIn('echo "fee_publish=false" >> "$GITHUB_OUTPUT"', self.workflow)
        self.assertIn('echo "seed_publish=true" >> "$GITHUB_OUTPUT"', self.workflow)
        self.assertRegex(
            self.workflow,
            r'if \[ "\$refresh_status" -eq 0 \]; then\s+echo "fee_publish=true"',
        )

    def test_client_fee_artifacts_are_gated_only_by_fee_publish(self):
        self.assertRegex(
            self.workflow,
            r'- name: Export Pages artifact\s+if: steps\.refresh\.outputs\.fee_publish == \'true\'',
        )
        self.assertRegex(
            self.workflow,
            r'- name: Compute artifact hash\s+if: steps\.refresh\.outputs\.fee_publish == \'true\'',
        )

    def test_seed_only_deployment_preserves_and_verifies_current_client_artifact(self):
        self.assertRegex(
            self.workflow,
            r'- name: Prepare frozen Pages artifacts\s+if: steps\.refresh\.outputs\.fee_publish != \'true\'',
        )
        self.assertIn('https://future-meta.pages.dev/manifest.json', self.workflow)
        self.assertIn('sha256sum public/latest.fmeta.zst', self.workflow)
        self.assertRegex(
            self.workflow,
            r'- name: Publish updated daemon seed\s+if: steps\.refresh\.outputs\.seed_publish == \'true\'',
        )
        self.assertRegex(
            self.workflow,
            r'- name: Deploy to Cloudflare Pages\s+if: steps\.refresh\.outputs\.seed_publish == \'true\'',
        )


if __name__ == "__main__":
    unittest.main()

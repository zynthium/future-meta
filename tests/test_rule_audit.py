import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import audit_official_rules as audit
import catalog_guard


class RuleAuditTests(unittest.TestCase):
    def test_weekend_skips_without_reading_files_or_accessing_exchange(self):
        args = SimpleNamespace(rules=Path("missing"))
        with (
            patch.object(
                audit,
                "decide",
                return_value={"should_update": False, "reason": "weekend"},
            ),
            patch.object(audit.official_access, "request") as request,
        ):
            self.assertEqual(
                audit.audit(args), {"status": "skipped", "reason": "weekend"}
            )
        request.assert_not_called()

    def run_audit(self, *, failure=False):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            rules = root / "rules.json"
            rules.write_text(
                json.dumps({"schema": 1, "sources": [{"url": "official"}]})
            )
            previous = root / "previous.json"
            previous.write_text(
                json.dumps(
                    {"status": "success", "rules": {"old": {}}, "catalogs": {"old": {}}}
                )
            )
            args = SimpleNamespace(rules=rules, previous=previous)
            gate = {
                "should_update": True,
                "slot": "evening",
                "evaluated_at": "2026-11-02T18:45:00+08:00",
            }

            def documents(*_unused):
                self.assertEqual(
                    os.environ["FUTURE_META_ACCESS_DEADLINE"],
                    "2026-11-02T20:00:00+08:00",
                )
                # A second invocation while the first is still running must
                # lose the same atomic slot claim before any official request.
                self.assertEqual(
                    audit.audit(args),
                    {"status": "skipped", "reason": "slot_already_attempted"},
                )
                return {"verified": {}}

            with (
                patch.object(audit, "decide", return_value=gate),
                patch.object(audit, "AUDITS", root / "audits"),
                patch.object(
                    audit, "check_rule_sources", side_effect=documents
                ) as sources,
                patch.object(
                    catalog_guard,
                    "check",
                    side_effect=ValueError("changed") if failure else None,
                    return_value={"current": {}},
                ) as catalogs,
                patch.dict(os.environ, {"FUTURE_META_ACCESS_DEADLINE": "prior"}),
            ):
                if failure:
                    with self.assertRaisesRegex(ValueError, "changed"):
                        audit.audit(args)
                else:
                    audit.audit(args)
                self.assertEqual(os.environ["FUTURE_META_ACCESS_DEADLINE"], "prior")
                self.assertEqual(
                    audit.audit(args),
                    {"status": "skipped", "reason": "slot_already_attempted"},
                )
            sources.assert_called_once()
            self.assertEqual(sources.call_args.args[1], {"old": {}})
            self.assertEqual(catalogs.call_args.args[2], {"old": {}})
            return json.loads(
                (root / "audits/2026-11-02-evening/rule-audit.json").read_text()
            )

    def test_success_records_separate_rule_check_time_and_versions(self):
        report = self.run_audit()
        self.assertEqual(report["status"], "success")
        self.assertTrue(report["finished_at"])
        self.assertEqual(len(report["rules_sha256"]), 64)
        self.assertEqual(report["catalogs"], {"current": {}})
        self.assertNotIn("checked_through", report)

    def test_changed_catalog_records_failure_without_success_versions(self):
        report = self.run_audit(failure=True)
        self.assertEqual(report["status"], "failed")
        self.assertIn("changed", report["error"])
        self.assertNotIn("rules", report)
        self.assertNotIn("catalogs", report)


if __name__ == "__main__":
    unittest.main()

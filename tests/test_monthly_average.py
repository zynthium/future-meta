import sys
import unittest
import csv
import json
import tempfile
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import collect_contract_specs as specs
import collect_settle_params as collector
import incremental_inputs as inputs


class MonthlyAverageTests(unittest.TestCase):
    def rules(self):
        import json

        rules = json.loads(Path("config/official-rules.json").read_text())
        rules["monthly_average_lifecycles"] = {
            "l-F": {
                "launch_trade_date": "2025-10-29",
                "first_delivery_month": "2026-02",
                "listing_months_before": 6,
            }
        }
        rules["specifications"].append(
            {
                "exchange": "DCE",
                "product": "l-F",
                "valid_from": "2025-10-29",
                "lot_size": 5.0,
                "tick_size": 1.0,
                "valid_from_phase": "date",
            }
        )
        return rules

    def test_future_filter_keeps_monthly_average_parameters(self):
        def row(symbol):
            return {
                "exchange": "DCE",
                "date": "2026-10-30",
                "symbol": symbol,
                "fee_type": "cny_per_lot",
                "long_margin_ratio": 20,
                "short_margin_ratio": 20,
                "hedge_long_margin_ratio": 15,
                "hedge_short_margin_ratio": 15,
                "open_fee": 1,
                "offset_fee": 1,
                "close_today_fee": 1,
                "intraday_open_fee": 1,
            }

        records = [row("l2611"), row("l2611F")]
        self.assertEqual(
            len(collector.validate_records(records, "DCE", "2026-10-30", True)), 2
        )
        with self.assertRaisesRegex(ValueError, "Unsupported monthly-average"):
            collector.validate_records([row("m2611F")], "DCE", "2026-10-30", True)

    def test_one_dce_raw_response_supplies_regular_and_monthly_average_rows(self):
        records = [
            {
                "contractId": symbol,
                "varietyOrder": product,
                "style": "绝对值",
                "openFee": 1,
                "offsetFee": 1,
                "shortOpenFee": 1,
                "shortOffsetFee": 1,
                "specBuyRate": 20,
                "specSellRate": 20,
                "hedgeBuyRate": 15,
                "hedgeSellRate": 15,
            }
            for symbol, product in [("l2611", "l"), ("l2611F", "l-F")]
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            store = collector.RawStore(root / "raw", None, False)
            store.save(
                "DCE",
                "2026-10-30",
                "http://www.dce.com.cn/dcereport/publicweb/tradepara/futAndOptSettle",
                json.dumps({"success": True, "data": records}).encode(),
            )
            output = root / "dce.csv"
            collector.prepare_csv(output)
            with (
                patch("playwright.sync_api.sync_playwright") as browser,
                patch.object(collector, "fetch_with_retry") as request,
            ):
                collector.collect_dce(["2026-10-30"], str(output), store, True)
            browser.assert_not_called()
            request.assert_not_called()
            with output.open() as handle:
                self.assertEqual(
                    {r["symbol"] for r in csv.DictReader(handle)}, {"l2611", "l2611F"}
                )

    def test_monthly_average_specs_do_not_borrow_the_regular_product(self):
        rules = self.rules()
        self.assertEqual(specs.symbol_parts("DCE", "l2611F"), ("DCE", "l2611F", "l-F"))
        self.assertEqual(
            inputs.specification(rules, "DCE.l2611F", "2026-10-30")["product"], "l-F"
        )
        rules["specifications"] = [
            r for r in rules["specifications"] if r["product"] != "l-F"
        ]
        with self.assertRaisesRegex(ValueError, "approved specification"):
            inputs.specification(rules, "DCE.l2611F", "2026-10-30")

    def test_listing_cycle_launch_and_previous_month_expiry(self):
        rules = self.rules()
        for symbol, observed, listing, expiry in [
            ("DCE.l2602F", "2025-10-29", "2025-10-29", "2026-01-30"),
            ("DCE.l2605F", "2025-11-03", "2025-11-03", "2026-04-30"),
            ("DCE.l2611F", "2026-05-06", "2026-05-06", "2026-10-30"),
            ("DCE.l2701F", "2026-07-01", "2026-07-01", "2026-12-31"),
        ]:
            with self.subTest(symbol=symbol):
                self.assertEqual(
                    inputs.new_lifecycle(rules, symbol, observed),
                    (listing, expiry, None),
                )
        listing, expiry, assumption = inputs.new_lifecycle(
            rules, "DCE.l2704F", "2026-10-30"
        )
        self.assertEqual((listing, expiry), ("2026-10-08", "2027-03-31"))
        self.assertTrue(assumption)
        with self.assertRaisesRegex(ValueError, "predates launch"):
            inputs.new_lifecycle(rules, "DCE.l2601F", "2025-10-29")
        with self.assertRaisesRegex(ValueError, "conflicts with observation"):
            inputs.new_lifecycle(rules, "DCE.l2611F", "2026-11-02")
        rules.pop("monthly_average_lifecycles")
        with self.assertRaisesRegex(ValueError, "Missing approved"):
            inputs.new_lifecycle(rules, "DCE.l2611F", "2026-05-06")

    def test_official_table_code_keeps_monthly_average_identity(self):
        body = "<table><tr><td>交易代码</td><td>L合约月份F</td></tr><tr><td>报价单位</td><td>元/吨</td></tr></table>".encode()
        self.assertEqual(
            specs.document_table_evidence(body, "l-F"), ("元/吨", "html_contract_code")
        )
        with self.assertRaisesRegex(ValueError, "do not support product"):
            specs.document_table_evidence(body, "l")
        with self.assertRaisesRegex(ValueError, "do not support product"):
            specs.document_table_evidence(
                body.replace("L合约月份F".encode(), b"L"), "l-F"
            )

    def test_month_scope_supports_suffix_without_changing_delivery_month(self):
        scope = {"delivery_month_from": "2026-02", "delivery_month_to": "2026-03"}
        self.assertTrue(specs.applies_to_contract(scope, "l2602F"))
        self.assertFalse(specs.applies_to_contract(scope, "l2604F"))


if __name__ == "__main__":
    unittest.main()

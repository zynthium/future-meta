import sys
import unittest
from datetime import datetime
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import incremental_inputs as inputs
import official_access as access
import audit_official_rules as rule_audit


class InputTests(unittest.TestCase):
    def test_official_calendar_skips_weekend_holiday_and_rejects_unknown_year(self):
        self.assertEqual(
            inputs.trading_dates("2026-10-29", "2026-11-02"),
            ["2026-10-29", "2026-10-30", "2026-11-02"],
        )
        self.assertEqual(
            inputs.trading_dates("2026-10-01", "2026-10-08"), ["2026-10-08"]
        )
        with self.assertRaisesRegex(ValueError, "2027"):
            inputs.trading_dates("2027-01-01", "2027-01-04")

    def test_deadline_blocks_requests_before_transport_even_on_retry(self):
        with patch.dict(
            "os.environ", {"FUTURE_META_ACCESS_DEADLINE": "2026-11-02T12:00:00+00:00"}
        ):
            with patch.object(
                access,
                "now",
                return_value=datetime.fromisoformat("2026-11-02T12:00:01+00:00"),
            ):
                with patch.object(access.requests, "request") as request:
                    with self.assertRaisesRegex(access.AccessWindowClosed, "closed"):
                        access.request("GET", "https://www.dce.com.cn/test")
                    request.assert_not_called()

    def test_timeout_clamped_to_access_deadline(self):
        with patch.dict(
            "os.environ", {"FUTURE_META_ACCESS_DEADLINE": "2026-11-02T12:00:00+00:00"}
        ):
            with patch.object(
                access,
                "now",
                return_value=datetime.fromisoformat("2026-11-02T11:59:57+00:00"),
            ):
                with patch.object(
                    access.requests, "request", return_value="ok"
                ) as request:
                    self.assertEqual(
                        access.request(
                            "GET", "https://www.dce.com.cn/test", timeout=30
                        ),
                        "ok",
                    )
                    self.assertEqual(request.call_args.kwargs["timeout"], 3.0)

    def test_spec_scope_is_explicit_and_unknown_product_fails(self):
        rules = {
            "specifications": [
                {
                    "exchange": "DCE",
                    "product": "m",
                    "full_symbol": "",
                    "valid_from": "2020-01-01",
                    "valid_to": "",
                    "lot_size": 10.0,
                    "tick_size": 1.0,
                    "delivery_month_from": "2026-12",
                    "delivery_month_to": "",
                    "valid_from_phase": "date",
                    "valid_from_trade_date": "",
                }
            ]
        }
        self.assertEqual(
            inputs.specification(rules, "DCE.m2701", "2026-11-02")["lot_size"], 10.0
        )
        with self.assertRaisesRegex(ValueError, "specification"):
            inputs.specification(rules, "DCE.m2611", "2026-11-02")
        with self.assertRaisesRegex(ValueError, "specification"):
            inputs.specification(rules, "DCE.new2701", "2026-11-02")

    def test_source_document_change_cannot_silently_keep_approved_rule(self):
        response = type(
            "Response",
            (),
            {
                "status_code": 200,
                "content": b"changed official document",
                "headers": {},
            },
        )()
        source = {"url": "https://www.shfe.com.cn/rule.pdf", "sha256": "a" * 64}
        with patch.object(rule_audit, "fetch_source", return_value=response):
            with self.assertRaisesRegex(ValueError, "changed"):
                rule_audit.check_rule_sources({"sources": [source]}, {})

    def test_cffex_carry_requires_verified_catalog_and_respects_expiry(self):
        baseline = [
            {
                "date": "2026-10-30",
                "exchange": "CFFEX",
                "symbol": "IF2611",
                "open_fee": 1,
            }
        ]
        with self.assertRaisesRegex(ValueError, "catalog"):
            inputs.cffex_daily(baseline, [], ["2026-11-02"], catalog_verified=False)
        rows = inputs.cffex_daily(baseline, [], ["2026-11-02"], catalog_verified=True)
        self.assertEqual(rows[0]["date"], "2026-11-02")
        self.assertEqual(baseline[0]["date"], "2026-10-30")


if __name__ == "__main__":
    unittest.main()


class NormalizedRecordTests(unittest.TestCase):
    def test_csv_margins_become_numbers_and_missing_values_fail(self):
        rules = {
            "specifications": [
                {
                    "exchange": "DCE",
                    "product": "m",
                    "full_symbol": "",
                    "valid_from": "2020-01-01",
                    "valid_to": "",
                    "lot_size": 10.0,
                    "tick_size": 1.0,
                    "valid_from_phase": "date",
                }
            ]
        }
        archive = {
            "contracts": [
                {
                    "symbol": "DCE.m2701",
                    "listing_date": "20260101",
                    "expiry_date": "20270115",
                    "lot_size": 10.0,
                    "tick_size": 1.0,
                }
            ]
        }
        record = {
            "date": "2026-11-02",
            "exchange": "DCE",
            "symbol": "m2701",
            "fee_type": "cny_per_lot",
            "long_margin_ratio": "10.0",
            "short_margin_ratio": "11.0",
            "open_fee": "1",
            "offset_fee": "1",
            "close_today_fee": "0",
            "intraday_open_fee": "",
        }
        row = inputs.normalized_rows([record], archive, rules, {}, {})[0]
        self.assertIsInstance(row["buy_margin_rate"], float)
        self.assertEqual(row["buy_margin_rate"], 10.0)
        record["long_margin_ratio"] = ""
        with self.assertRaisesRegex(ValueError, "margin"):
            inputs.normalized_rows([record], archive, rules, {}, {})


class FreshnessTests(unittest.TestCase):
    def test_redirect_checks_deadline_before_each_hop(self):
        response = type(
            "Response",
            (),
            {
                "status_code": 302,
                "headers": {"Location": "https://www.dce.com.cn/new"},
                "close": lambda self: None,
            },
        )()
        with (
            patch.dict(
                "os.environ",
                {"FUTURE_META_ACCESS_DEADLINE": "2026-11-02T12:00:00+00:00"},
            ),
            patch.object(
                access,
                "now",
                side_effect=[
                    datetime.fromisoformat("2026-11-02T11:59:59+00:00"),
                    datetime.fromisoformat("2026-11-02T12:00:01+00:00"),
                ],
            ),
            patch.object(access.requests, "request", return_value=response) as request,
        ):
            with self.assertRaises(access.AccessWindowClosed):
                access.request("GET", "https://www.dce.com.cn/old")
            self.assertEqual(request.call_count, 1)

    def test_same_date_cffex_revision_is_rejected_before_carry(self):
        import hashlib

        prior = {
            "date": "2026-10-19",
            "url": "http://www.cffex.com.cn/old.csv",
            "sha256": hashlib.sha256(b"approved").hexdigest(),
        }
        store = type("Store", (), {"get": lambda self, *a, **k: b"revised"})()
        with self.assertRaisesRegex(ValueError, "revised"):
            inputs.verify_cffex_anchor(
                store, prior, [("2026-10-19", prior["url"])], "2026-10-30"
            )


class DatedMetadataCompletenessTests(unittest.TestCase):
    def test_known_shfe_contract_still_requires_its_current_dated_cbi(self):
        rules = {
            "specifications": [
                {
                    "exchange": "SHFE",
                    "product": "cu",
                    "full_symbol": "",
                    "valid_from": "2020-01-01",
                    "valid_to": "",
                    "lot_size": 5.0,
                    "tick_size": 10.0,
                    "valid_from_phase": "date",
                }
            ]
        }
        archive = {
            "contracts": [
                {
                    "symbol": "SHFE.cu2701",
                    "listing_date": "20260101",
                    "expiry_date": "20270115",
                    "lot_size": 5.0,
                    "tick_size": 10.0,
                }
            ]
        }
        record = {
            "date": "2026-11-02",
            "exchange": "SHFE",
            "symbol": "cu2701",
            "fee_type": "rate_per_10k",
            "long_margin_ratio": 10.0,
            "short_margin_ratio": 11.0,
            "open_fee": 0.5,
            "offset_fee": 0.5,
            "close_today_fee": 0.5,
        }
        with self.assertRaisesRegex(ValueError, "dated official CBI"):
            inputs.normalized_rows([record], archive, rules, {}, {})


class CbiConflictTests(unittest.TestCase):
    def test_conflicting_dated_rows_are_not_resolved_by_last_write(self):
        body = {
            "report_date": "20261102",
            "ContractBaseInfo": [
                {
                    "TRADINGDAY": "20261102",
                    "INSTRUMENTID": "cu2701",
                    "OPENDATE": "20260101",
                    "EXPIREDATE": "20270115",
                },
                {
                    "TRADINGDAY": "20261102",
                    "INSTRUMENTID": "cu2701",
                    "OPENDATE": "20260101",
                    "EXPIREDATE": "20270116",
                },
            ],
        }
        with self.assertRaisesRegex(ValueError, "Conflicting"):
            inputs.cbi_lifecycles(body, "SHFE", "2026-11-02")


class FeeNormalizationTests(unittest.TestCase):
    def test_zero_preserves_the_official_numeric_fee_unit(self):
        self.assertEqual(inputs.fee("cny_per_lot", 0)["kind"], "CnyPerLot")
        self.assertEqual(
            inputs.fee("rate_per_10k", 0)["kind"], "TurnoverRatePerTenThousand"
        )
        self.assertIsNone(inputs.fee("cny_per_lot", None, optional=True))

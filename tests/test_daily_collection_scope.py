import copy
import csv
import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import audit_official_rules as audit
import catalog_guard
import collect_contract_specs as specs
import collect_settle_params as collector
import incremental_inputs as inputs
import official_access


class DailyCollectionScopeTests(unittest.TestCase):
    def test_missing_days_use_daily_sources_without_rule_audit(self):
        self.run_collection(False)

    def test_complete_shfe_cbi_avoids_duplicate_ine_request(self):
        self.run_collection(True)

    def run_collection(self, combined_cbi):
        days = ["2026-10-29", "2026-10-30"]
        symbols = {
            "SHFE": "cu2701",
            "INE": "sc2701",
            "CZCE": "SR701",
            "GFEX": "si2701",
            "DCE": "m2701",
            "CFFEX": "IF2611",
        }
        archive = {
            "contracts": [
                {
                    "symbol": f"{ex}.{symbol}",
                    "listing_date": "20260101",
                    "expiry_date": "20270115",
                    "lot_size": 10.0,
                    "tick_size": 1.0,
                }
                for ex, symbol in symbols.items()
            ]
        }
        rules = {
            "approved_through": "2026-10-28",
            "specifications": [
                {
                    "exchange": ex,
                    "product": symbol.rstrip("0123456789"),
                    "valid_from": "2020-01-01",
                    "lot_size": 10.0,
                    "tick_size": 1.0,
                }
                for ex, symbol in symbols.items()
            ],
        }
        anchor = {
            "date": "2026-10-19",
            "url": "http://www.cffex.com.cn/old.csv",
            "sha256": hashlib.sha256(b"approved").hexdigest(),
        }
        state = {
            "checked_through": {ex: "2026-10-28" for ex in symbols},
            "source_versions": {
                "cffex_snapshot": anchor,
                "rules": {"stale": {}},
                "catalogs": {"stale": {}},
            },
            "exceptions": {},
        }
        original = copy.deepcopy(state)
        calls = []

        def record(ex, day):
            return {
                "exchange": ex,
                "date": day,
                "symbol": symbols[ex],
                "variety": symbols[ex].rstrip("0123456789"),
                "fee_type": "cny_per_lot",
                "long_margin_ratio": 10,
                "short_margin_ratio": 11,
                "hedge_long_margin_ratio": 10,
                "hedge_short_margin_ratio": 11,
                "open_fee": 1,
                "offset_fee": 1,
                "close_today_fee": 0,
                "intraday_open_fee": 1,
            }

        def fetch(ex, day):
            calls.append((ex, day))
            return [record(ex, day)]

        def cbi(url):
            calls.append(("CBI", url))
            ex = "INE" if "www.ine.cn" in url else "SHFE"
            day = url[-12:-4]
            return json.dumps(
                {
                    "report_date": day,
                    "ContractBaseInfo": [
                        {
                            "INSTRUMENTID": symbol,
                            "TRADINGDAY": day,
                            "OPENDATE": "20260101",
                            "EXPIREDATE": "20270115",
                        }
                        for symbol in (
                            [symbols["SHFE"], symbols["INE"]]
                            if combined_cbi
                            else [symbols[ex]]
                        )
                    ],
                }
            ).encode()

        def dce(selected_days, path, store, futures_only):
            self.assertEqual(selected_days, days)
            self.assertTrue(futures_only)
            for day in selected_days:
                rows = fetch("DCE", day)
                with open(path, "a", newline="") as handle:
                    writer = csv.DictWriter(handle, fieldnames=collector.CSV_COLUMNS)
                    writer.writerow(
                        {k: rows[0].get(k, "") for k in collector.CSV_COLUMNS}
                    )

        with (
            tempfile.TemporaryDirectory() as temp,
            patch.object(
                audit, "check_rule_sources", side_effect=AssertionError("daily audit")
            ),
            patch.object(
                catalog_guard, "check", side_effect=AssertionError("daily catalog")
            ),
            patch.object(
                official_access,
                "request",
                side_effect=AssertionError("unexpected HTTP"),
            ),
            patch.object(
                collector,
                "fetch_shfe_ine",
                side_effect=lambda day, ex, store: fetch(ex, day),
            ),
            patch.object(
                collector,
                "fetch_czce",
                side_effect=lambda day, store: fetch("CZCE", day),
            ),
            patch.object(
                collector,
                "fetch_gfex_day",
                side_effect=lambda day, store: fetch("GFEX", day),
            ),
            patch.object(specs, "fetch_cbi_body", side_effect=cbi),
            patch.object(collector, "collect_dce", side_effect=dce),
            patch.object(
                collector,
                "get_cffex_catalog",
                return_value=[(anchor["date"], anchor["url"])],
            ) as catalog,
            patch.object(
                collector.RawStore, "get", return_value=b"approved"
            ) as anchor_get,
            patch.object(
                inputs, "cffex_baseline", return_value=[record("CFFEX", "2026-10-28")]
            ),
        ):
            snapshots, versions, _ = inputs.collect_interval(
                archive, state, rules, days, temp
            )
        self.assertEqual(len(snapshots), 12)
        self.assertTrue(all(len(s["rows"]) == 1 for s in snapshots))
        self.assertEqual(
            [s["trade_date"] for s in snapshots], [d for d in days for _ in symbols]
        )
        for ex in ("SHFE", "INE", "CZCE", "GFEX", "DCE"):
            self.assertEqual([day for exchange, day in calls if exchange == ex], days)
        cbi_calls = [c for c in calls if c[0] == "CBI"]
        self.assertEqual(len(cbi_calls), 2 if combined_cbi else 4)
        if combined_cbi:
            self.assertTrue(all("www.shfe.com.cn" in c[1] for c in cbi_calls))
        catalog.assert_called_once()
        self.assertEqual(catalog.call_args.kwargs, {"min_date": "2026-10-28"})
        anchor_get.assert_called_once_with("CFFEX", anchor["date"], anchor["url"])
        self.assertEqual(state, original)
        self.assertNotIn("rules", versions)
        self.assertNotIn("catalogs", versions)
        self.assertEqual(versions["specifications"]["approved_through"], "2026-10-28")
        self.assertFalse(versions["specifications"]["live_check_performed"])
        self.assertEqual(versions["cffex_snapshot"], anchor)


if __name__ == "__main__":
    unittest.main()

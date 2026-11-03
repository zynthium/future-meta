import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import collect_settle_params as collector
import official_access as access

URL = "https://www.shfe.com.cn/data/busiparamdata/future/Settlement20261103.dat"


def settlement(day="20261103"):
    return json.dumps(
        {
            "report_date": day,
            "update_date": f"{day} 16:10:00",
            "Settlement": [
                {
                    "INSTRUMENTID": "cu2701",
                    "PRODUCTID": "cu_f",
                    "TRADINGDAY": day,
                    "SPEC_LONGMARGINRATIO": "0.12",
                    "SPEC_SHORTMARGINRATIO": "0.12",
                    "LONGMARGINRATIO": "0.10",
                    "SHORTMARGINRATIO": "0.10",
                    "TRADEFEEUNIT": "5",
                    "TRADEFEERATION": "0",
                    "DISCOUNTRATE": "0",
                    "COMMODITYDELIVERYFEEUNIT": "0",
                }
            ],
        }
    ).encode()


class OfficialBrowserFallbackTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.store = collector.RawStore(Path(self.temp.name), None, False)
        self.playwright = Mock()
        self.browser = self.playwright.chromium.launch.return_value
        self.page = self.browser.new_page.return_value
        self.response = self.page.goto.return_value
        self.response.status = 200
        self.response.body.return_value = settlement()
        manager = Mock()
        manager.__enter__ = Mock(return_value=self.playwright)
        manager.__exit__ = Mock(return_value=False)
        self.manager = patch(
            "playwright.sync_api.sync_playwright", return_value=manager
        ).start()
        patch.object(collector, "get_chrome_path", return_value=None).start()
        patch.object(collector.time, "sleep").start()
        self.guard = patch.object(access, "guard_browser").start()
        self.remaining = patch.object(access, "remaining", return_value=None).start()
        self.http = patch.object(
            access,
            "request",
            side_effect=ConnectionResetError("Connection reset by peer"),
        ).start()
        self.addCleanup(patch.stopall)

    def fetch(self):
        return collector.fetch_shfe_ine("2026-11-03", "SHFE", self.store)

    def test_two_resets_then_one_chrome_navigation_preserves_bytes_and_cache(self):
        rows = self.fetch()
        self.assertEqual(rows[0]["symbol"], "cu2701")
        self.assertEqual(rows[0]["open_fee"], 5)
        self.assertEqual(rows[0]["close_today_fee"], 0)
        self.assertEqual(self.fetch(), rows)
        self.assertEqual(self.http.call_count, 2)
        self.page.goto.assert_called_once_with(URL, wait_until="load", timeout=15000)
        self.guard.assert_called_once_with(self.page)
        self.browser.close.assert_called_once()
        wrapper = json.loads(
            (Path(self.temp.name) / "shfe/2026-11-03.json").read_text()
        )
        self.assertEqual(collector.RawStore.unpack(wrapper), settlement())

    def test_http_success_does_not_start_chrome(self):
        self.http.side_effect = None
        response = Mock(status_code=200, content=settlement())
        self.http.return_value = response
        self.assertEqual(self.fetch()[0]["open_fee"], 5)
        self.http.assert_called_once()
        self.manager.assert_not_called()
        response.close.assert_called_once()

    def test_ine_reuses_same_settlement_without_another_http_or_browser_attempt(self):
        doc = json.loads(settlement())
        fee = dict(doc["Settlement"][0], INSTRUMENTID="sc2701", PRODUCTID="sc_f")
        doc["Settlement"].append(fee)
        self.response.body.return_value = json.dumps(doc).encode()
        margin_url = "https://www.ine.cn/data/tradedata/future/dailydata/js20261103.dat"
        margin = {
            "report_date": "20261103",
            "o_cursor": [
                {
                    "INSTRUMENTID": "sc2701",
                    "SPECLONGMARGINRATIO": "0.15",
                    "SPECSHORTMARGINRATIO": "0.15",
                    "HEDGLONGMARGINRATIO": "0.10",
                    "HEDGSHORTMARGINRATIO": "0.10",
                }
            ],
        }
        self.store.save("INE", "2026-11-03", margin_url, json.dumps(margin).encode())
        self.fetch()
        rows = collector.fetch_shfe_ine("2026-11-03", "INE", self.store)
        collector.validate_records(rows, "INE", "2026-11-03", True)
        self.assertEqual(rows[0]["symbol"], "sc2701")
        self.assertEqual(rows[0]["open_fee"], 5)
        self.assertEqual(self.http.call_count, 2)
        self.page.goto.assert_called_once()

    def test_unapproved_url_is_rejected_before_any_transport(self):
        with self.assertRaises(ValueError):
            collector.fetch_with_browser_fallback("https://example.com/data.dat")
        self.http.assert_not_called()
        self.manager.assert_not_called()

    def test_legacy_ine_daily_fee_without_row_date_uses_validated_report_date(self):
        day = "2018-03-26"
        fee_doc = json.loads(settlement("20180326"))
        self.store.save(
            "SHFE",
            day,
            URL.replace("20261103", "20180326"),
            json.dumps(fee_doc).encode(),
        )
        daily_row = {
            "INSTRUMENTID": "sc1809",
            "TRADEFEEUNIT": "20",
            "SPECLONGMARGINRATIO": "0.07",
            "SPECSHORTMARGINRATIO": "0.07",
            "HEDGLONGMARGINRATIO": "0.07",
            "HEDGSHORTMARGINRATIO": "0.07",
        }
        url = "https://www.ine.cn/data/tradedata/future/dailydata/js20180326.dat"
        daily = {"report_date": "20180326", "o_cursor": [daily_row]}
        self.store.save("INE", day, url, json.dumps(daily).encode())
        rows = collector.fetch_shfe_ine(day, "INE", self.store)
        collector.validate_records(rows, "INE", day, True)
        self.assertEqual(rows[0]["open_fee"], 20)
        self.assertEqual(rows[0]["close_today_fee"], 0)
        self.http.assert_not_called()
        daily_row["TRADINGDAY"] = "20180323"
        self.store.save("INE", day, url, json.dumps(daily).encode())
        with self.assertRaisesRegex(ValueError, "date"):
            collector.fetch_shfe_ine(day, "INE", self.store)

    def test_browser_http_error_stops_after_third_attempt_without_caching(self):
        self.response.status = 503
        with self.assertRaisesRegex(RuntimeError, "503") as error:
            self.fetch()
        self.assertIn("ConnectionResetError", str(error.exception))
        self.assertEqual(self.http.call_count, 2)
        self.page.goto.assert_called_once()
        self.response.body.assert_not_called()
        self.browser.close.assert_called_once()
        self.assertFalse((Path(self.temp.name) / "shfe/2026-11-03.json").exists())

    def test_browser_old_date_or_challenge_page_cannot_supply_current_parameters(self):
        for body in (
            settlement("20261102"),
            b"<html>challenge</html>",
            b'{"report_date":"20261103","Settlement":[]}',
        ):
            with self.subTest(body=body):
                store = collector.RawStore(
                    Path(self.temp.name) / str(len(body)), None, False
                )
                self.response.body.return_value = body
                with self.assertRaises(ValueError):
                    rows = collector.fetch_shfe_ine("2026-11-03", "SHFE", store)
                    collector.validate_records(rows, "SHFE", "2026-11-03", True)

    def test_browser_row_date_mismatch_fails_even_if_report_date_is_current(self):
        doc = json.loads(settlement())
        doc["Settlement"][0]["TRADINGDAY"] = "20261102"
        self.response.body.return_value = json.dumps(doc).encode()
        with self.assertRaisesRegex(ValueError, "date"):
            self.fetch()

    def test_window_closure_during_http_is_not_retried_or_sent_to_chrome(self):
        self.http.side_effect = access.AccessWindowClosed("closed")
        with self.assertRaises(access.AccessWindowClosed):
            self.fetch()
        self.http.assert_called_once()
        self.manager.assert_not_called()

    def test_window_closure_after_two_http_attempts_prevents_browser_start(self):
        self.remaining.side_effect = access.AccessWindowClosed("closed")
        with self.assertRaises(access.AccessWindowClosed):
            self.fetch()
        self.assertEqual(self.http.call_count, 2)
        self.manager.assert_not_called()

    def test_window_closure_during_browser_setup_prevents_navigation_and_closes_browser(
        self,
    ):
        self.remaining.side_effect = [None, None, access.AccessWindowClosed("closed")]
        with self.assertRaises(access.AccessWindowClosed):
            self.fetch()
        self.page.goto.assert_not_called()
        self.browser.close.assert_called_once()

    def test_navigation_timeout_is_limited_by_remaining_window(self):
        self.remaining.return_value = 0.5
        self.fetch()
        self.page.goto.assert_called_once_with(URL, wait_until="load", timeout=500)

    def test_window_closure_during_driver_startup_prevents_chrome_launch(self):
        self.remaining.side_effect = [None, access.AccessWindowClosed("closed")]
        with self.assertRaises(access.AccessWindowClosed):
            self.fetch()
        self.playwright.chromium.launch.assert_not_called()
        self.page.goto.assert_not_called()


if __name__ == "__main__":
    unittest.main()

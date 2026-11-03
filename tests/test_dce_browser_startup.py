import csv
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import collect_settle_params as collector
import official_access as access
import collection_transport as transport
from playwright.sync_api import TimeoutError as BrowserTimeout


def parameters():
    row = {
        "contractId": "l2611",
        "varietyOrder": "l",
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
    return json.dumps(
        {
            "success": True,
            "data": [row, dict(row, contractId="l2701F", varietyOrder="l-F")],
        }
    )


class DceStartupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.store = collector.RawStore(self.root / "raw", None, False)
        self.csv = self.root / "dce.csv"
        collector.prepare_csv(self.csv)
        self.playwright = Mock()
        self.first, self.second = Mock(), Mock()
        for browser in (self.first, self.second):
            browser.new_page.return_value.goto.return_value = Mock(status=200)
            browser.new_page.return_value.evaluate.return_value = parameters()
        self.playwright.chromium.launch.side_effect = [self.first, self.second]
        manager = Mock()
        manager.__enter__ = Mock(return_value=self.playwright)
        manager.__exit__ = Mock(return_value=False)
        patch("playwright.sync_api.sync_playwright", return_value=manager).start()
        patch.object(collector, "get_chrome_path", return_value=None).start()
        self.guard = patch.object(access, "guard_browser").start()
        self.remaining = patch.object(access, "remaining", return_value=None).start()
        self.sleep = patch.object(collector.time, "sleep").start()
        self.addCleanup(patch.stopall)

    def collect(self, days=None):
        collector.collect_dce(days or ["2026-11-03"], str(self.csv), self.store, True)

    def test_home_timeout_closes_failed_browser_before_one_fresh_retry(self):
        self.first.new_page.return_value.goto.side_effect = BrowserTimeout(
            "home stalled"
        )

        def launch(**options):
            if self.playwright.chromium.launch.call_count == 1:
                return self.first
            self.first.close.assert_called_once()
            return self.second

        self.playwright.chromium.launch.side_effect = launch
        self.collect(["2026-11-02", "2026-11-03"])
        self.assertEqual(self.playwright.chromium.launch.call_count, 2)
        self.first.new_page.return_value.goto.assert_called_once()
        self.first.new_page.return_value.evaluate.assert_not_called()
        self.assertEqual(self.second.new_page.return_value.goto.call_count, 2)
        self.assertEqual(self.second.new_page.return_value.evaluate.call_count, 2)
        self.second.close.assert_called_once()
        with self.csv.open() as handle:
            rows = list(csv.DictReader(handle))
        self.assertEqual(len(rows), 4)
        self.assertEqual({r["date"] for r in rows}, {"2026-11-02", "2026-11-03"})
        self.assertEqual({r["symbol"] for r in rows}, {"l2611", "l2701F"})

    def test_report_timeout_also_replaces_session(self):
        self.first.new_page.return_value.goto.side_effect = [
            Mock(status=412),
            BrowserTimeout("report stalled"),
        ]
        self.collect()
        self.assertEqual(self.playwright.chromium.launch.call_count, 2)
        self.first.close.assert_called_once()
        self.second.close.assert_called_once()

    def test_both_startup_failures_stop_without_post_or_cached_business_rows(self):
        for browser in (self.first, self.second):
            browser.new_page.return_value.goto.side_effect = BrowserTimeout("stalled")
        with self.assertRaisesRegex(RuntimeError, "initialization"):
            self.collect()
        self.assertEqual(self.playwright.chromium.launch.call_count, 2)
        for browser in (self.first, self.second):
            browser.close.assert_called_once()
            browser.new_page.return_value.evaluate.assert_not_called()
        self.assertFalse((self.root / "raw/dce/2026-11-03.json").exists())

    def test_bad_parameters_fail_without_restarting_or_repeating_the_batch(self):
        self.first.new_page.return_value.evaluate.return_value = (
            "<html>challenge</html>"
        )
        with self.assertRaises(ValueError):
            self.collect()
        self.assertEqual(self.playwright.chromium.launch.call_count, 1)
        self.first.close.assert_called_once()
        self.second.new_page.assert_not_called()

    def test_deadline_before_start_prevents_chrome_launch(self):
        self.remaining.side_effect = access.AccessWindowClosed("closed")
        with self.assertRaises(access.AccessWindowClosed):
            self.collect()
        self.playwright.chromium.launch.assert_not_called()

    def test_deadline_after_failed_session_prevents_retry(self):
        closed = []
        self.first.close.side_effect = lambda: closed.append(True)
        self.first.new_page.return_value.goto.side_effect = BrowserTimeout("stalled")

        def remaining():
            if closed:
                raise access.AccessWindowClosed("closed")
            return None

        self.remaining.side_effect = remaining
        with self.assertRaises(access.AccessWindowClosed):
            self.collect()
        self.assertEqual(self.playwright.chromium.launch.call_count, 1)
        self.second.new_page.assert_not_called()

    def test_navigation_and_bootstrap_waits_are_bounded_by_remaining_window(self):
        self.remaining.return_value = 1.25
        self.collect()
        page = self.first.new_page.return_value
        self.assertTrue(
            all(call.kwargs["timeout"] == 1250 for call in page.goto.call_args_list)
        )
        self.assertEqual(
            [call.args[0] for call in page.wait_for_timeout.call_args_list[:2]],
            [1250, 1250],
        )


    def test_active_primary_initialization_failure_hands_off_without_second_local_session(self):
        self.first.new_page.return_value.goto.side_effect = BrowserTimeout("home stalled")
        path = self.root / "transport.json"
        with transport.activate(path, "primary", "ubuntu-latest"):
            with self.assertRaises(transport.TransportUnavailable):
                self.collect()
        self.assertEqual(self.playwright.chromium.launch.call_count, 1)
        self.first.close.assert_called_once()
        health = transport.read_health(path)
        self.assertEqual(sum(item["failure"] for item in health["buckets"]), 1)
        self.second.new_page.return_value.evaluate.return_value = {
            "status": 200, "body": list(parameters().encode())}
        with transport.activate(path, "backup", "macos-15-intel"):
            self.collect()
        self.assertEqual(self.playwright.chromium.launch.call_count, 2)
        self.assertEqual(len(json.loads(path.read_text())["dce_sessions"]), 2)

    def test_active_api_failure_preserves_completed_day_for_backup(self):
        raw = {"status": 200, "body": list(parameters().encode())}
        self.first.new_page.return_value.evaluate.side_effect = [raw, BrowserTimeout("API stalled")]
        path = self.root / "transport.json"
        with transport.activate(path, "primary", "ubuntu-latest"):
            with self.assertRaises(transport.TransportUnavailable):
                self.collect(["2026-11-02", "2026-11-03"])
        self.assertTrue((self.root / "raw/dce/2026-11-02.json").exists())
        self.assertFalse((self.root / "raw/dce/2026-11-03.json").exists())
        self.second.new_page.return_value.evaluate.return_value = raw
        with transport.activate(path, "backup", "macos-15-intel"):
            self.collect(["2026-11-02", "2026-11-03"])
        self.second.new_page.return_value.evaluate.assert_called_once()
        self.assertEqual(self.second.new_page.return_value.evaluate.call_args.args[1]["params"]["tradeDate"], "20261103")

    def test_active_challenge_body_is_hard_failure_and_does_not_restart_session(self):
        self.first.new_page.return_value.evaluate.return_value = {
            "status": 200, "body": list(b"<html>challenge</html>")}
        with transport.activate(self.root / "transport.json", "primary", "ubuntu-latest"):
            with self.assertRaises(ValueError):
                self.collect()
        self.assertEqual(self.playwright.chromium.launch.call_count, 1)

    def test_active_api_http_error_is_recoverable_but_programming_error_is_not(self):
        from playwright.sync_api import Error
        for result, expected in [({"status": 503, "body": []}, transport.TransportUnavailable),
                                 (Error("ReferenceError: missingVariable is not defined"), Error)]:
            with self.subTest(result=result):
                self.playwright.chromium.launch.side_effect = None
                self.playwright.chromium.launch.return_value = self.first
                if isinstance(result, Exception):
                    self.first.new_page.return_value.evaluate.side_effect = result
                else:
                    self.first.new_page.return_value.evaluate.side_effect = None
                    self.first.new_page.return_value.evaluate.return_value = result
                path = self.root / (expected.__name__ + ".json")
                with transport.activate(path, "primary", "ubuntu-latest"):
                    with self.assertRaises(expected):
                        self.collect()


    def test_active_api_deadline_and_abort_do_not_trigger_another_session(self):
        from playwright.sync_api import Error
        self.first.new_page.return_value.evaluate.side_effect = Error("TimeoutError: signal timed out")
        path = self.root / "abort.json"
        with transport.activate(path, "primary", "ubuntu-latest"):
            with self.assertRaises(transport.TransportUnavailable):
                self.collect()
        self.assertEqual(self.playwright.chromium.launch.call_count, 1)


if __name__ == "__main__":
    unittest.main()

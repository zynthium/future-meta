import base64
import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import collect_settle_params as collector
import collect_contract_specs as specs
import update_official as update


class OfficialPostTransportTests(unittest.TestCase):
    def test_official_calendar_avoids_any_http_request(self):
        from incremental_inputs import trading_dates
        with patch.object(collector.requests,'request',side_effect=AssertionError('unexpected exchange access')) as request:
            self.assertEqual(trading_dates('2026-11-02','2026-11-02'), ['2026-11-02'])
        request.assert_not_called()

    def test_cbi_browser_after_http_exhaustion_retains_original_bytes(self):
        body = b'{"report_date":"20261030","original":"\\u94dc"}\n'
        playwright = Mock()
        browser = playwright.chromium.launch.return_value
        response = browser.new_page.return_value.goto.return_value
        response.status, response.body.return_value = 200, body
        manager = Mock()
        manager.__enter__ = Mock(return_value=playwright)
        manager.__exit__ = Mock(return_value=False)
        url = 'https://www.shfe.com.cn/data/busiparamdata/future/ContractBaseInfo20261030.dat'
        with patch.object(collector, 'fetch_with_retry', side_effect=RuntimeError('HTTP exhausted')), \
                patch.object(collector, 'get_chrome_path', return_value=None), \
                patch('playwright.sync_api.sync_playwright', return_value=manager):
            self.assertEqual(specs.fetch_cbi_body(url), body)
        browser.new_page.return_value.goto.assert_called_once_with(url, wait_until='domcontentloaded', timeout=30000)
        browser.close.assert_called_once()

    def test_cbi_browser_failure_closes_browser_and_does_not_return_error_body(self):
        playwright = Mock()
        browser = playwright.chromium.launch.return_value
        response = browser.new_page.return_value.goto.return_value
        response.status = 503
        manager = Mock()
        manager.__enter__ = Mock(return_value=playwright)
        manager.__exit__ = Mock(return_value=False)
        with patch.object(collector, 'fetch_with_retry', side_effect=RuntimeError('HTTP exhausted')), \
                patch.object(collector, 'get_chrome_path', return_value=None), \
                patch('playwright.sync_api.sync_playwright', return_value=manager):
            with self.assertRaisesRegex(RuntimeError, 'browser request failed'):
                specs.fetch_cbi_body('https://www.shfe.com.cn/data/busiparamdata/future/ContractBaseInfo20261030.dat')
        response.body.assert_not_called()
        browser.close.assert_called_once()

    def test_contract_base_info_uses_same_retried_transport_and_retains_bytes(self):
        day = '2026-10-30'
        bodies = [json.dumps({'report_date': day, 'ContractBaseInfo': [
            {'TRADINGDAY': day, 'INSTRUMENTID': symbol, 'OPENDATE': '2025-11-18',
             'EXPIREDATE': '2026-11-16', 'ENDDELIVDATE': '2026-11-18'}]}).encode()
                  for symbol in ('cu2611', 'sc2611')]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            evidence = specs.Evidence(root)
            with patch.object(collector, 'fetch_with_retry', side_effect=[Mock(content=b) for b in bodies]) as fetch:
                rows = specs.load_lifecycles(evidence, [], [], [day], root/'cache')
            self.assertEqual(set(rows), {'SHFE.cu2611', 'INE.sc2611'})
            self.assertEqual(fetch.call_count, 2)
            for call in fetch.call_args_list:
                self.assertIn('ContractBaseInfo20261030.dat', call.args[0])
                self.assertEqual(call.kwargs['timeout'], 30)
            for body in bodies:
                sha = hashlib.sha256(body).hexdigest()
                self.assertEqual((root/'cache'/f'{sha}.dat').read_bytes(), body)
                self.assertIn(sha, evidence.entries)

    def test_origin_timeout_retries_same_post_and_retains_successful_original(self):
        url = 'http://www.gfex.com.cn/u/interfacesWebTiFutAndOptSettle/loadList'
        form = {'trade_date': '20261030', 'variety': ''}
        body = b'{"code":"0","param":{"trade_date":["20261030"]},"data":[]}'
        responses = [Mock(status_code=524), Mock(status_code=200, content=body)]
        with tempfile.TemporaryDirectory() as temp:
            store = collector.RawStore(Path(temp), None, False)
            with patch.object(collector.requests, 'request', side_effect=responses) as request, patch.object(collector.time, 'sleep') as sleep:
                self.assertEqual(store.get('GFEX', '2026-10-30', url, form=form), body)
            self.assertEqual(request.call_count, 2)
            sleep.assert_called_once_with(2.0)
            for call in request.call_args_list:
                self.assertEqual(call.args, ('POST', url))
                self.assertEqual(call.kwargs['data'], form)
                self.assertEqual(call.kwargs['timeout'], 30)
            retained = json.loads((Path(temp)/'gfex/2026-10-30.json').read_text())
            self.assertEqual(retained['request'], {'method': 'POST', 'form': form})
            self.assertEqual(base64.b64decode(retained['body_base64']), body)
            self.assertEqual(retained['sha256'], hashlib.sha256(body).hexdigest())

    def test_exhausted_post_does_not_create_a_cached_original(self):
        url = 'http://www.gfex.com.cn/u/interfacesWebTiFutAndOptSettle/loadList'
        with tempfile.TemporaryDirectory() as temp:
            store = collector.RawStore(Path(temp), None, False)
            with patch.object(collector.requests, 'request', return_value=Mock(status_code=524)) as request, patch.object(collector.time, 'sleep'):
                with self.assertRaisesRegex(RuntimeError, 'after 3 attempts'):
                    store.get('GFEX', '2026-10-30', url, form={'trade_date': '20261030'})
            self.assertEqual(request.call_count, 3)
            self.assertFalse((Path(temp)/'gfex/2026-10-30.json').exists())


if __name__ == '__main__':
    unittest.main()

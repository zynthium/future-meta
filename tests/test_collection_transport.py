import importlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

import requests

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import official_access as access
import collect_settle_params as collector

URL = 'https://www.shfe.com.cn/data/example.dat'


class CollectionTransportTests(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(importlib.util.find_spec('collection_transport'), 'production transport context is missing')
        self.transport = importlib.import_module('collection_transport')
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'transport.json'
        self.remaining = patch.object(access, 'remaining', return_value=None).start()
        self.addCleanup(patch.stopall)

    def activate(self, stage='primary', runner='ubuntu-latest', **kwargs):
        return self.transport.activate(self.path, stage, runner, **kwargs)

    def test_primary_reserves_two_different_clients_before_network_and_backup_only_chrome(self):
        clients = []

        def fail(method, url, **kwargs):
            ledger = json.loads(self.path.read_text())
            attempts = next(iter(ledger['requests'].values()))['attempts']
            self.assertEqual(len(attempts), len(clients) + 1)
            self.assertEqual(attempts[-1]['outcome'], 'running')
            clients.append(kwargs['client'])
            raise requests.ConnectionError('reset')

        with patch.object(access, 'request', side_effect=fail), self.activate() as context:
            with self.assertRaises(self.transport.TransportUnavailable):
                collector.fetch_with_retry(URL)
        self.assertEqual(clients, ['requests', 'curl_cffi'])
        response = requests.Response()
        response.status_code = 200
        response._content = b'\x00\xfffixture'
        with patch.object(context.__class__, '_chrome', return_value=response) as chrome, patch.object(access, 'request') as http, self.activate('backup', 'macos-15-intel') as backup:
            self.assertEqual(collector.fetch_with_retry(URL).content, b'\x00\xfffixture')
            chrome.assert_called_once()
            http.assert_not_called()
            self.assertEqual(sum(item['success'] for item in backup.health()['buckets']), 1)
        ledger = json.loads(self.path.read_text())
        self.assertEqual(len(next(iter(ledger['requests'].values()))['attempts']), 3)
        with patch.object(context.__class__, '_chrome') as chrome, self.activate('backup', 'macos-15-intel'):
            with self.assertRaises(self.transport.TransportUnavailable):
                collector.fetch_with_retry(URL)
            chrome.assert_not_called()

    def test_new_backup_request_uses_only_three_total_attempts(self):
        with patch.object(access, 'request', side_effect=requests.ConnectionError('reset')) as http, patch.object(self.transport.Context, '_chrome', side_effect=requests.ConnectionError('chrome reset')) as chrome, self.activate('backup', 'macos-15-intel'):
            with self.assertRaises(self.transport.TransportUnavailable):
                collector.fetch_with_retry(URL, max_retries=99)
        self.assertEqual(http.call_count, 2)
        chrome.assert_called_once()

    def test_deadline_and_source_errors_are_hard_failures(self):
        for error in (access.AccessWindowClosed('closed'), ValueError('source rejected')):
            with self.subTest(error=error), patch.object(access, 'request', side_effect=error) as http, self.activate():
                with self.assertRaises(type(error)):
                    collector.fetch_with_retry(URL)
                http.assert_called_once()

    def test_raw_cache_hit_does_not_spend_transport_budget(self):
        store = collector.RawStore(Path(self.temp.name) / 'raw', None, False)
        store.save('SHFE', '2026-11-03', URL, b'cached')
        with patch.object(access, 'request') as http, self.activate():
            self.assertEqual(store.get('SHFE', '2026-11-03', URL), b'cached')
            http.assert_not_called()
        self.assertEqual(json.loads(self.path.read_text())['requests'], {})

    def test_request_identity_canonicalizes_query_and_form_but_keeps_trade_date(self):
        key = self.transport.request_key
        first = key('POST', URL + '?b=2&a=1#fragment', data={'trade_date': '20261103', 'v': ''})
        self.assertEqual(first, key('post', URL + '?a=1&b=2', data={'v': '', 'trade_date': '20261103'}))
        self.assertNotEqual(first, key('POST', URL + '?a=1&b=2', data={'trade_date': '20261104', 'v': ''}))
        self.assertNotEqual(first, key('POST', URL + '?a=1&b=2', json={'trade_date': '20261103', 'v': ''}))

    def test_history_aggregates_by_fixed_dimensions_without_request_urls(self):
        response = Mock(status_code=200)
        with patch.object(access, 'request', return_value=response), self.activate() as context:
            for number in range(10):
                context.fetch('GET', URL + '?day=' + str(number))
            health = context.health()
        self.assertEqual(len(health['buckets']), 1)
        self.assertEqual(health['buckets'][0]['success'], 10)
        self.assertNotIn('example.dat', json.dumps(health))
        with self.activate('backup', 'macos-15-intel', history=health) as context:
            self.assertEqual(context.health(), health)

    def test_runner_preference_requires_observations_then_compares_actual_success_rate(self):
        preference = self.transport.preferred_runners
        self.assertEqual(preference(None), ('ubuntu-latest', 'macos-15-intel'))
        history = {'version': 1, 'buckets': [
            {'host': 'shfe.com.cn', 'runner': 'ubuntu-latest', 'client': 'requests', 'success': 1, 'failure': 4, 'duration_seconds': 3},
            {'host': 'shfe.com.cn', 'runner': 'macos-15-intel', 'client': 'requests', 'success': 4, 'failure': 1, 'duration_seconds': 10},
        ]}
        self.assertEqual(preference(history), ('macos-15-intel', 'ubuntu-latest'))

    def test_dce_session_budget_survives_runner_handoff(self):
        with self.activate() as context:
            context.reserve_dce_session()
            with self.assertRaises(self.transport.TransportUnavailable):
                context.reserve_dce_session()
        with self.activate('backup', 'macos-15-intel') as context:
            context.reserve_dce_session()
            with self.assertRaises(self.transport.TransportUnavailable):
                context.reserve_dce_session()
        self.assertEqual(len(json.loads(self.path.read_text())['dce_sessions']), 2)

    def test_persistence_failure_is_not_misclassified_as_network_failure(self):
        with self.activate() as context:
            with patch.object(context, '_save', side_effect=OSError('disk failed')), patch.object(access, 'request') as http:
                with self.assertRaises(OSError):
                    collector.fetch_with_retry(URL)
                http.assert_not_called()

    def test_backup_refresh_reuses_validated_raw_cache(self):
        store = collector.RawStore(Path(self.temp.name) / 'raw', None, False)
        store.save('SHFE', '2026-11-03', URL, b'cached')
        with patch.object(access, 'request') as http, self.activate('backup', 'macos-15-intel'):
            self.assertEqual(store.get('SHFE', '2026-11-03', URL, refresh=True), b'cached')
            http.assert_not_called()

    def test_read_health_counts_killed_attempt_without_requiring_activation(self):
        with self.activate() as context:
            ledger = context.ledger
        ledger['requests']['fixture'] = {'host': 'shfe.com.cn', 'attempts': [
            {'runner': 'ubuntu-latest', 'client': 'requests', 'outcome': 'running'}]}
        self.path.write_text(json.dumps(ledger))
        for _ in range(2):
            health = self.transport.read_health(self.path)
            self.assertEqual(sum(item['failure'] for item in health['buckets']), 1)
        self.assertEqual(json.loads(self.path.read_text()), ledger)

    def test_killed_attempt_keeps_its_budget_and_is_counted_as_failure_once(self):
        with self.activate() as context:
            ledger = context.ledger
        ledger['requests']['fixture'] = {'host': 'shfe.com.cn', 'attempts': [
            {'runner': 'ubuntu-latest', 'client': 'requests', 'outcome': 'running'}]}
        self.path.write_text(json.dumps(ledger))
        for _ in range(2):
            with self.activate('backup', 'macos-15-intel') as context:
                self.assertEqual(sum(item['failure'] for item in context.health()['buckets']), 1)
                self.assertEqual(len(context.ledger['requests']['fixture']['attempts']), 1)


if __name__ == '__main__':
    unittest.main()

class CurlClientTests(unittest.TestCase):
    def test_curl_uses_chrome_profile_and_checks_each_redirect_deadline(self):
        import types
        curl = types.SimpleNamespace(request=Mock(), RequestsError=RuntimeError)
        first = Mock(status_code=307, headers={'Location': '/second'})
        final = Mock(status_code=200)
        curl.request.side_effect = [first, final]
        with patch.dict(sys.modules, {'curl_cffi': types.SimpleNamespace(requests=curl)}), patch.object(access, 'remaining', side_effect=[10, 4]) as remaining, patch.object(access.requests, 'request') as default:
            actual = access.request('POST', URL, client='curl_cffi', data={'trade_date': '20261103'}, timeout=30,
                                    headers={'User-Agent': collector.USER_AGENT, 'clientid': 'web'})
        self.assertIs(actual, final)
        self.assertEqual(remaining.call_count, 2)
        self.assertEqual([call.kwargs['timeout'] for call in curl.request.call_args_list], [10, 4])
        self.assertTrue(all(call.kwargs['impersonate'] == 'chrome' for call in curl.request.call_args_list))
        self.assertTrue(all('User-Agent' not in call.kwargs['headers'] for call in curl.request.call_args_list))
        self.assertTrue(all(call.kwargs['headers']['clientid'] == 'web' for call in curl.request.call_args_list))
        default.assert_not_called()

    def test_curl_external_redirect_and_expired_window_never_send_next_hop(self):
        import types
        for destination, error in [('https://example.com/escape', ValueError), ('/second', access.AccessWindowClosed)]:
            with self.subTest(destination=destination):
                curl = types.SimpleNamespace(request=Mock(return_value=Mock(status_code=302, headers={'Location': destination})), RequestsError=RuntimeError)
                with patch.dict(sys.modules, {'curl_cffi': types.SimpleNamespace(requests=curl)}), patch.object(access, 'remaining', side_effect=[10, access.AccessWindowClosed('closed')]):
                    with self.assertRaises(error):
                        access.request('GET', URL, client='curl_cffi')
                curl.request.assert_called_once()

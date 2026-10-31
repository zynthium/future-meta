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


class OfficialPostTransportTests(unittest.TestCase):
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
            sleep.assert_called_once_with(1.0)
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
                with self.assertRaisesRegex(RuntimeError, 'after 5 attempts'):
                    store.get('GFEX', '2026-10-30', url, form={'trade_date': '20261030'})
            self.assertEqual(request.call_count, 5)
            self.assertFalse((Path(temp)/'gfex/2026-10-30.json').exists())


if __name__ == '__main__':
    unittest.main()

"""One workflow's bounded transport ledger and small historical health summary.

The ledger is a temporary handoff, never a replacement for the approved archive.
Only aggregate health (six hosts, two runners, three clients) belongs in state.
"""

from contextlib import contextmanager
from contextvars import ContextVar
import hashlib
import json as jsonlib
import math
import os
from pathlib import Path
import tempfile
import time
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit

import requests

import official_access

RUNNERS = ('ubuntu-latest', 'macos-15-intel')
CLIENTS = ('requests', 'curl_cffi', 'chrome')
HOSTS = ('shfe.com.cn', 'ine.cn', 'dce.com.cn', 'czce.com.cn', 'gfex.com.cn', 'cffex.com.cn')
_ACTIVE = ContextVar('official_collection_transport', default=None)


class TransportUnavailable(RuntimeError):
    """Network/HTTP attempts are exhausted on this runner; no business verdict."""


def current():
    return _ACTIVE.get()


def canonical_url(url):
    parsed = urlsplit(url)
    host = (parsed.hostname or '').lower()
    port = parsed.port
    netloc = host if port in (None, 80 if parsed.scheme == 'http' else 443) else f'{host}:{port}'
    return urlunsplit((parsed.scheme.lower(), netloc, parsed.path or '/',
                       urlencode(sorted(parse_qsl(parsed.query, keep_blank_values=True))), ''))


def request_key(method, url, *, data=None, json=None):
    # Distinguish a JSON object from a URL-encoded form, including its trade date.
    identity = [method.upper(), canonical_url(url), data, json]
    encoded = jsonlib.dumps(identity, sort_keys=True, separators=(',', ':'), ensure_ascii=False)
    return hashlib.sha256(encoded.encode()).hexdigest()


def _host(url):
    official_access.validate_url(url)
    hostname = urlsplit(url).hostname
    if hostname == 'shfe.cn' or hostname.endswith('.shfe.cn'):
        return 'shfe.com.cn'
    return next(host for host in HOSTS if hostname == host or hostname.endswith('.' + host))


def _health(value=None):
    buckets = {}
    for item in (value or {}).get('buckets', []):
        key = (item.get('host'), item.get('runner'), item.get('client'))
        if key[0] not in HOSTS or key[1] not in RUNNERS or key[2] not in CLIENTS:
            continue
        success, failure = item.get('success', 0), item.get('failure', 0)
        duration = item.get('duration_seconds', 0)
        if (type(success) is not int or type(failure) is not int or min(success, failure) < 0
                or not isinstance(duration, (int, float)) or not math.isfinite(duration) or duration < 0):
            raise ValueError('Invalid transport health bucket')
        # Bounded counters retain recent performance instead of an ever-growing log.
        while success + failure > 200:
            success, failure, duration = success // 2, failure // 2, duration / 2
        buckets[key] = dict(host=key[0], runner=key[1], client=key[2], success=success,
                            failure=failure, duration_seconds=round(duration, 6))
    return {'version': 1, 'buckets': [buckets[key] for key in sorted(buckets)]}


def preferred_runners(history):
    totals = {runner: [0, 0, 0.0] for runner in RUNNERS}
    for item in _health(history)['buckets']:
        total = totals[item['runner']]
        total[0] += item['success']
        total[1] += item['success'] + item['failure']
        total[2] += item['duration_seconds']
    if any(total[1] < 3 for total in totals.values()):
        return RUNNERS
    return tuple(sorted(RUNNERS, key=lambda runner: (
        -totals[runner][0] / totals[runner][1],
        totals[runner][2] / totals[runner][1], RUNNERS.index(runner))))


def _record_metric(health, host, attempt):
    health = _health(health)
    key = (host, attempt['runner'], attempt['client'])
    bucket = next((item for item in health['buckets'] if
                   (item['host'], item['runner'], item['client']) == key), None)
    if bucket is None:
        bucket = dict(host=host, runner=key[1], client=key[2], success=0, failure=0, duration_seconds=0)
        health['buckets'].append(bucket)
    bucket['success' if attempt['outcome'] == 'success' else 'failure'] += 1
    bucket['duration_seconds'] += attempt.get('duration_seconds', 0)
    return _health(health)


def read_health(path, history=None):
    path = Path(path)
    if not path.exists():
        return _health(history)
    ledger = jsonlib.loads(path.read_text())
    health = _health(ledger['health'])
    # A finalizer can summarize a killed primary without activating collection.
    for item in ledger['requests'].values():
        for attempt in item['attempts']:
            if attempt['outcome'] == 'running':
                health = _record_metric(health, item['host'], attempt)
    return health


class Context:
    def __init__(self, path, stage, runner, history=None):
        if stage not in ('primary', 'backup') or runner not in RUNNERS:
            raise ValueError('Unknown collection stage or runner')
        self.path, self.stage, self.runner = Path(path), stage, runner
        if self.path.exists():
            self.ledger = jsonlib.loads(self.path.read_text())
            if self.ledger.get('version') != 1:
                raise ValueError('Unsupported transport ledger')
        else:
            self.ledger = {'version': 1, 'requests': {}, 'dce_sessions': [], 'health': _health(history)}
        # A killed runner may leave its consumed attempt unfinished. Its failure
        # still counts and, crucially, never refunds the request budget.
        for item in self.ledger['requests'].values():
            for attempt in item['attempts']:
                if attempt['outcome'] == 'running':
                    attempt['outcome'] = 'failure'
                    attempt['duration_seconds'] = 0
                    self._metric(item['host'], attempt)
        self._save()

    def _save(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile('w', encoding='utf-8', dir=self.path.parent,
                                         prefix=self.path.name + '.', delete=False) as handle:
            temporary = Path(handle.name)
            jsonlib.dump(self.ledger, handle, sort_keys=True, separators=(',', ':'))
            handle.flush()
            os.fsync(handle.fileno())
        try:
            os.replace(temporary, self.path)
        finally:
            temporary.unlink(missing_ok=True)

    def health(self):
        return _health(self.ledger['health'])

    def _metric(self, host, attempt):
        self.ledger['health'] = _record_metric(self.ledger['health'], host, attempt)

    def reserve_dce_session(self):
        official_access.remaining()
        sessions = self.ledger['dce_sessions']
        if len(sessions) >= 2 or (self.stage == 'primary' and any(
                item['stage'] == 'primary' for item in sessions)):
            raise TransportUnavailable('DCE browser session budget exhausted on this runner')
        sessions.append({'stage': self.stage, 'runner': self.runner})
        self._save()

    @contextmanager
    def attempt(self, method, url, client, *, data=None, json=None):
        host = _host(url)
        official_access.remaining()
        key = request_key(method, url, data=data, json=json)
        item = self.ledger['requests'].setdefault(key, {'host': host, 'attempts': []})
        if client not in CLIENTS or len(item['attempts']) >= 3:
            raise TransportUnavailable('Official logical request budget exhausted')
        attempt = {'runner': self.runner, 'client': client, 'outcome': 'running'}
        item['attempts'].append(attempt)
        self._save()  # Spend the attempt durably before starting the network.
        started = time.monotonic()
        try:
            yield
        except BaseException:
            attempt['outcome'] = 'failure'
            raise
        else:
            attempt['outcome'] = 'success'
        finally:
            attempt['duration_seconds'] = round(time.monotonic() - started, 6)
            self._metric(host, attempt)
            self._save()

    def _http_clients(self, host):
        buckets = {item['client']: item for item in self.health()['buckets']
                   if item['host'] == host and item['runner'] == self.runner}
        if any(client not in buckets or buckets[client]['success'] + buckets[client]['failure'] < 2
               for client in CLIENTS[:2]):
            return list(CLIENTS[:2])
        return sorted(CLIENTS[:2], key=lambda client: (
            -buckets[client]['success'] / (buckets[client]['success'] + buckets[client]['failure']),
            buckets[client]['duration_seconds'] / (buckets[client]['success'] + buckets[client]['failure']),
            CLIENTS.index(client)))

    def _chrome(self, method, url, **kwargs):
        from collect_settle_params import fetch_chrome
        response = requests.Response()
        response.status_code = 200
        response.url = url
        response._content = fetch_chrome(url, method=method, **kwargs)
        return response

    def fetch(self, method, url, **kwargs):
        host = _host(url)
        key = request_key(method, url, data=kwargs.get('data'), json=kwargs.get('json'))
        attempts = self.ledger['requests'].get(key, {}).get('attempts', [])
        if self.stage == 'primary':
            used = {item['client'] for item in attempts}
            clients = [client for client in self._http_clients(host) if client not in used]
        else:
            clients = ['chrome'] if attempts else self._http_clients(host) + ['chrome']
        last_error = 'request budget already consumed'
        for client in clients:
            if len(attempts) >= 3:
                break
            try:
                with self.attempt(method, url, client, data=kwargs.get('data'), json=kwargs.get('json')):
                    response = (self._chrome(method, url, **kwargs) if client == 'chrome' else
                                official_access.request(method, url, client=client, **kwargs))
                    if response.status_code != 200:
                        status = response.status_code
                        response.close()
                        raise requests.HTTPError(f'Official HTTP {status}')
                    return response
            except (requests.RequestException, ConnectionError, TimeoutError) as error:
                last_error = f'{type(error).__name__}: {error}'
            attempts = self.ledger['requests'].get(key, {}).get('attempts', [])
        raise TransportUnavailable(f'Official transport unavailable on {self.runner}: {url}; {last_error}')


@contextmanager
def activate(path, stage, runner, history=None):
    context = Context(path, stage, runner, history)
    token = _ACTIVE.set(context)
    try:
        yield context
    finally:
        _ACTIVE.reset(token)
        context._save()

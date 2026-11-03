"""Offline workflow exercise: real ledger/cache, synthetic responses, no publishing."""

import json
from unittest.mock import patch

import requests

import collection_transport as transport
import collect_settle_params as collector
import data_branch as branch
import official_access

BASE = "https://www.shfe.com.cn/__future_meta_offline_smoke__/"
FIRST, RECOVERY = BASE + "cached.dat", BASE + "recover.dat"
BODY = b'{"offline_smoke":true,"value":7}'


def response():
    result = requests.Response()
    result.status_code = 200
    result._content = BODY
    return result


def collect(args, session):
    """The primary caches one file, then uses its two failing HTTP choices."""
    store = collector.RawStore(args.work_dir / "collected" / "raw", None, False)
    day = session["days"][0]

    def http(method, url, **kwargs):
        if args.phase == "primary" and url == FIRST:
            return response()
        if args.phase == "primary" and url == RECOVERY:
            raise requests.ConnectionError("intentional offline primary transport failure")
        raise AssertionError("Offline smoke tried an unexpected or already cached HTTP request")

    def chrome(context, method, url, **kwargs):
        if args.phase == "backup" and url == RECOVERY:
            return response()
        raise AssertionError("Offline smoke tried an unexpected Chrome request")

    with (
        patch.object(official_access, "request", side_effect=http),
        patch.object(transport.Context, "_chrome", chrome),
        patch.object(official_access, "guard_browser", side_effect=AssertionError("Offline smoke cannot navigate an exchange")),
    ):
        for key, url in (("first", FIRST), ("recovery", RECOVERY)):
            actual = store.get("SHFE", day, url, key=key)
            if actual != BODY:
                raise ValueError("Offline fixture response changed")
    return {"offline_smoke": True, "files": 2}


def verify(args, session, result):
    before = branch.read(args.repo)
    if before != (session["parent"], session["manifest"], session["original_state"]):
        raise ValueError("Production state changed during offline smoke")
    ledger = json.loads((args.work_dir / "transport.json").read_text())
    first = ledger["requests"][transport.request_key("GET", FIRST)]["attempts"]
    recovered = ledger["requests"][transport.request_key("GET", RECOVERY)]["attempts"]
    if len(first) != 1 or len(recovered) != 3:
        raise ValueError("Offline cross-runner request budget was not preserved")
    if [entry["client"] for entry in recovered] != ["requests", "curl_cffi", "chrome"]:
        raise ValueError("Offline workflow did not exercise all three clients")
    if recovered[-1]["runner"] != session["backup_runner"]:
        raise ValueError("Offline recovery did not switch runners")
    if not result.get("offline_smoke") or result.get("files") != 2:
        raise ValueError("Incomplete offline runner handoff")
    return {"offline_smoke": True, "ok": True, "actual_exchange_requests": 0,
            "cached_file_attempts": len(first), "recovered_file_attempts": len(recovered),
            "production_state_unchanged": True}

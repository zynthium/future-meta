from __future__ import annotations

import http.server
import json
from pathlib import Path
import socket
import socketserver
import sys
import tempfile
import threading
import unittest

sys.path.insert(
    0, str(Path(__file__).resolve().parents[1] / "crates/future-meta-py/python")
)
import future_meta


def _sample_archive_payload(open_fee_value: float = 0.5) -> dict:
    return {
        "schema_version": future_meta.SCHEMA_VERSION,
        "generated_at": "2026-06-15T12:00:00+08:00",
        "history_start": "2026-06-01T00:00:00+08:00",
        "history_end": "2026-06-15T12:00:00+08:00",
        "contracts": [
            {
                "id": 1,
                "symbol": "SHFE.cu2607",
                "listing_date": "20250716",
                "expiry_date": "20260715",
                "lot_size": 5.0,
                "tick_size": 5.0,
                "active": True,
            },
            {
                "id": 2,
                "symbol": "DCE.l2611",
                "listing_date": "20251101",
                "expiry_date": "20261115",
                "lot_size": 5.0,
                "tick_size": 1.0,
                "active": True,
            },
            {
                "id": 3,
                "symbol": "DCE.l2611F",
                "listing_date": "20251101",
                "expiry_date": "20261115",
                "lot_size": 5.0,
                "tick_size": 1.0,
                "active": True,
            },
        ],
        "contract_spec_versions": [
            {
                "contract_id": 1,
                "lot_size": 5.0,
                "tick_size": 10.0,
                "valid_from": "2025-07-16T00:00:00+08:00",
                "valid_to": "2026-06-10T00:00:00+08:00",
            },
            {
                "contract_id": 1,
                "lot_size": 5.0,
                "tick_size": 5.0,
                "valid_from": "2026-06-10T00:00:00+08:00",
                "valid_to": None,
            },
            {
                "contract_id": 2,
                "lot_size": 5.0,
                "tick_size": 1.0,
                "valid_from": "2025-11-01T00:00:00+08:00",
                "valid_to": None,
            },
            {
                "contract_id": 3,
                "lot_size": 5.0,
                "tick_size": 1.0,
                "valid_from": "2025-11-01T00:00:00+08:00",
                "valid_to": None,
            },
        ],
        "fee_versions": [
            {
                "contract_id": 1,
                "rule_hash": "cu-v1",
                "buy_margin_rate": 0.12,
                "sell_margin_rate": 0.12,
                "open_fee": {
                    "kind": "TurnoverRatePerTenThousand",
                    "value": open_fee_value,
                    "raw_text": f"万分之{open_fee_value}",
                },
                "close_yesterday_fee": {
                    "kind": "TurnoverRatePerTenThousand",
                    "value": open_fee_value,
                    "raw_text": f"万分之{open_fee_value}",
                },
                "close_today_fee": {
                    "kind": "Zero",
                    "value": 0.0,
                    "raw_text": "0",
                },
                "intraday_open_fee": {
                    "kind": "TurnoverRatePerTenThousand",
                    "value": 1.0,
                    "raw_text": "万分之1",
                },
                "trading_status": "Trading",
                "is_main_contract": True,
                "source_updated_at": "2026-06-04",
                "valid_from": "2026-06-01T00:00:00+08:00",
                "valid_to": "2026-06-10T00:00:00+08:00",
            },
            {
                "contract_id": 1,
                "rule_hash": "cu-v2",
                "buy_margin_rate": 0.14,
                "sell_margin_rate": 0.14,
                "open_fee": {
                    "kind": "CnyPerLot",
                    "value": 15.0,
                    "raw_text": "15元/手",
                },
                "close_yesterday_fee": {
                    "kind": "CnyPerLot",
                    "value": 15.0,
                    "raw_text": "15元/手",
                },
                "close_today_fee": {
                    "kind": "CnyPerLot",
                    "value": 30.0,
                    "raw_text": "30元/手",
                },
                "intraday_open_fee": None,
                "trading_status": "Trading",
                "is_main_contract": True,
                "source_updated_at": "2026-06-10",
                "valid_from": "2026-06-10T00:00:00+08:00",
                "valid_to": None,
            },
            {
                "contract_id": 2,
                "rule_hash": "l-regular",
                "buy_margin_rate": 0.08,
                "sell_margin_rate": 0.08,
                "open_fee": {"kind": "CnyPerLot", "value": 1.0, "raw_text": "1元/手"},
                "close_yesterday_fee": {
                    "kind": "CnyPerLot",
                    "value": 1.0,
                    "raw_text": "1元/手",
                },
                "close_today_fee": {
                    "kind": "CnyPerLot",
                    "value": 1.0,
                    "raw_text": "1元/手",
                },
                "intraday_open_fee": None,
                "trading_status": "Trading",
                "is_main_contract": False,
                "source_updated_at": None,
                "valid_from": "2026-06-01T00:00:00+08:00",
                "valid_to": None,
            },
            {
                "contract_id": 3,
                "rule_hash": "l-monthly-average",
                "buy_margin_rate": 0.09,
                "sell_margin_rate": 0.09,
                "open_fee": {"kind": "CnyPerLot", "value": 2.0, "raw_text": "2元/手"},
                "close_yesterday_fee": {
                    "kind": "CnyPerLot",
                    "value": 2.0,
                    "raw_text": "2元/手",
                },
                "close_today_fee": {
                    "kind": "CnyPerLot",
                    "value": 0.5,
                    "raw_text": "0.5元/手",
                },
                "intraday_open_fee": None,
                "trading_status": "Trading",
                "is_main_contract": False,
                "source_updated_at": None,
                "valid_from": "2026-06-01T00:00:00+08:00",
                "valid_to": None,
            },
        ],
    }


def _sample_archive_bytes(open_fee_value: float = 0.5) -> bytes:
    return future_meta.encode_archive_json(
        json.dumps(_sample_archive_payload(open_fee_value))
    )


class PythonBindingsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.archive_bytes = _sample_archive_bytes()
        self.meta = future_meta.FutureMeta.from_compressed_bytes(self.archive_bytes)

    def test_symbol_parsing_and_normalization(self) -> None:
        parsed = future_meta.parse_symbol("SHFE.cu2607")
        self.assertEqual(parsed.kind, "Futures")
        self.assertEqual(parsed.exchange, "SHFE")
        self.assertEqual(parsed.local, "cu2607")
        self.assertEqual(parsed.underlying_symbol, "SHFE.cu")
        self.assertEqual(
            parsed.to_dict(),
            {
                "raw": "SHFE.cu2607",
                "kind": "Futures",
                "exchange": "SHFE",
                "local": "cu2607",
                "underlying_symbol": "SHFE.cu",
            },
        )

        monthly = future_meta.parse_symbol("DCE.l2611F")
        self.assertEqual(monthly.kind, "Futures")
        self.assertEqual(monthly.underlying_symbol, "DCE.l")
        self.assertEqual(
            future_meta.normalize_futures_symbol("DCE", "l2611F"),
            "DCE.l2611F",
        )
        self.assertEqual(
            future_meta.normalize_futures_symbol("CZCE", "SR2609"),
            "CZCE.SR609",
        )
        self.assertEqual(
            future_meta.derive_underlying_symbol("KQ.m@SHFE.cu"),
            "SHFE.cu",
        )
        with self.assertRaises(future_meta.FutureMetaError):
            future_meta.derive_underlying_symbol("KQ.i@SHFE.bu")
        with self.assertRaises(future_meta.FutureMetaError):
            future_meta.parse_symbol("INVALID")

    def test_contract_spec_and_midway_adjustment_queries(self) -> None:
        contract = self.meta.contract("SHFE.cu2607")
        self.assertEqual(contract.id, 1)
        self.assertEqual(contract.symbol, "SHFE.cu2607")
        self.assertEqual(contract.lot_size, 5.0)
        self.assertEqual(contract.tick_size, 5.0)
        self.assertEqual(contract.tick_value(), 25.0)
        self.assertEqual(len(self.meta.contracts()), 3)

        spec_before = self.meta.contract_spec_asof(
            "SHFE.cu2607", "2026-06-05T10:00:00+08:00"
        )
        self.assertEqual(spec_before.tick_size, 10.0)
        self.assertEqual(spec_before.tick_value(), 50.0)

        spec_after = self.meta.contract_spec_on("SHFE.cu2607", "2026-06-10")
        self.assertEqual(spec_after.tick_size, 5.0)
        self.assertEqual(spec_after.tick_value(), 25.0)
        self.assertEqual(spec_after.to_dict()["tick_value"], 25.0)

        handle = self.meta.resolve_contract("SHFE.cu2607")
        self.assertEqual(handle.contract_id, 1)
        self.assertEqual(
            self.meta.contract_spec_for_handle_on(handle, "2026-06-05"),
            spec_before,
        )

    def test_fee_queries_and_monthly_average_isolation(self) -> None:
        fee_v1 = self.meta.contract_fee_asof(
            "SHFE.cu2607", "2026-06-04T10:00:00+08:00"
        )
        self.assertEqual(fee_v1.rule_hash, "cu-v1")
        self.assertEqual(fee_v1.open_fee.kind, "TurnoverRatePerTenThousand")
        self.assertEqual(fee_v1.open_fee.value, 0.5)
        self.assertEqual(fee_v1.close_today_fee.kind, "Zero")
        self.assertIsNotNone(fee_v1.intraday_open_fee)
        assert fee_v1.intraday_open_fee is not None
        self.assertEqual(fee_v1.intraday_open_fee.value, 1.0)

        fee_v2 = self.meta.contract_fee_on("SHFE.cu2607", "2026-06-10")
        self.assertEqual(fee_v2.rule_hash, "cu-v2")
        self.assertEqual(fee_v2.open_fee.kind, "CnyPerLot")
        self.assertEqual(fee_v2.open_fee.value, 15.0)
        self.assertIsNone(fee_v2.intraday_open_fee)

        reg_fee = self.meta.contract_fee_on("DCE.l2611", "2026-06-04")
        f_fee = self.meta.contract_fee_on("DCE.l2611F", "2026-06-04")
        self.assertEqual(reg_fee.open_fee.value, 1.0)
        self.assertEqual(f_fee.open_fee.value, 2.0)

        underlying_fees = self.meta.underlying_fees_on("DCE.l", "2026-06-04")
        self.assertEqual(
            [f.rule_hash for f in underlying_fees],
            ["l-regular", "l-monthly-average"],
        )

        with self.assertRaises(future_meta.FutureMetaError):
            self.meta.contract_fee_asof(
                "KQ.m@SHFE.cu", "2026-06-04T10:00:00+08:00"
            )

    def test_trading_day_meta_and_prepared_fee_cursors(self) -> None:
        day = self.meta.for_trading_day("2026-06-04")
        self.assertEqual(day.trading_date(), "2026-06-04")
        prepared = day.prepare_fee_by_symbol("SHFE.cu2607")
        self.assertEqual(prepared.lot_size, 5.0)
        # price = 80000, lots = 2, lot_size = 5, rate = 0.5 / 10000 -> 2 * 5 * 80000 * 0.00005 = 40.0
        self.assertAlmostEqual(prepared.open_amount(80000.0, 2.0), 40.0)
        self.assertAlmostEqual(
            prepared.intraday_open_amount(80000.0, 2.0) or 0.0, 80.0
        )
        self.assertAlmostEqual(prepared.close_today_amount(80000.0, 2.0), 0.0)

        cu_handle = self.meta.resolve_contract("SHFE.cu2607")
        l_handle = self.meta.resolve_contract("DCE.l2611F")
        book = day.prepare_fee_book([cu_handle, l_handle])
        self.assertEqual(len(book), 2)
        self.assertAlmostEqual(book[1].open_amount(8000.0, 3.0), 6.0)

        # 2026-06-04T09:00:00+08:00 in unix nanos
        t1_nanos = 1780534800_000_000_000
        cursors = self.meta.prepare_fee_cursors(
            [cu_handle, l_handle], "2026-06-04", t1_nanos
        )
        self.assertEqual(cursors.current_trading_date(), "2026-06-04")
        self.assertAlmostEqual(cursors.current(0).open_amount(80000.0, 2.0), 40.0)

        # Advance to 2026-06-10T09:00:00+08:00 (fixed 15 CNY/lot)
        t2_nanos = 1781053200_000_000_000
        fee_after = cursors.advance_and_get("2026-06-10", 0, t2_nanos)
        self.assertEqual(cursors.current_trading_date(), "2026-06-10")
        self.assertAlmostEqual(fee_after.open_amount(80000.0, 2.0), 30.0)
        self.assertIsNone(fee_after.intraday_open_amount(80000.0, 2.0))

        # Backward timestamp must fail
        with self.assertRaises(future_meta.FutureMetaError):
            cursors.advance_to("2026-06-10", t2_nanos - 1)

    def test_file_loading(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            archive_path = tmp_path / "sample.fmeta.zst"
            archive_path.write_bytes(self.archive_bytes)
            from_disk = future_meta.FutureMeta.from_file(archive_path)
            self.assertEqual(from_disk.contract("DCE.l2611F").lot_size, 5.0)
            loaded = future_meta.FutureMeta.load_file(archive_path)
            self.assertEqual(loaded.contract("SHFE.cu2607").tick_size, 5.0)

    def test_download_client(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            archive_v1 = self.archive_bytes
            archive_v2 = _sample_archive_bytes(open_fee_value=0.8)
            state = {
                "etag": '"v1"',
                "archive": archive_v1,
            }

            class Handler(http.server.BaseHTTPRequestHandler):
                def do_GET(self):
                    if self.path == "/manifest.json":
                        if self.headers.get("If-None-Match") == state["etag"]:
                            self.send_response(304)
                            self.send_header("ETag", state["etag"])
                            self.end_headers()
                            return
                        body = json.dumps(
                            {
                                "schema_version": future_meta.SCHEMA_VERSION,
                                "data_version": state["etag"].strip('"'),
                                "generated_at": "2026-06-15T12:00:00+08:00",
                                "history_start": "2026-06-01T00:00:00+08:00",
                                "history_end": "2026-06-15T12:00:00+08:00",
                                "fee_effective_from": "2026-06-10T00:00:00+08:00",
                                "artifact": "latest.fmeta.zst",
                                "sha256": future_meta.sha256_hex(state["archive"]),
                                "size": len(state["archive"]),
                                "mirrors": [
                                    f"http://127.0.0.1:{self.server.server_address[1]}/latest.fmeta.zst"
                                ],
                            }
                        ).encode("utf-8")
                        self.send_response(200)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(body)))
                        self.send_header("ETag", state["etag"])
                        self.end_headers()
                        self.wfile.write(body)
                    elif self.path == "/latest.fmeta.zst":
                        body = state["archive"]
                        self.send_response(200)
                        self.send_header("Content-Type", "application/octet-stream")
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                    else:
                        self.send_response(404)
                        self.end_headers()

                def log_message(self, format, *args):
                    return

            try:
                server = socketserver.TCPServer(("127.0.0.1", 0), Handler)
            except PermissionError:
                self.skipTest("local socket bind not permitted in sandbox")
            with server:
                port = server.server_address[1]
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                try:
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=1.0):
                            pass
                    except OSError:
                        self.skipTest("loopback socket connect not permitted in sandbox")
                    cfg = future_meta.DownloadConfig(
                        manifest_url=f"http://127.0.0.1:{port}/manifest.json",
                        cache_dir=tmp_path / "cache",
                    )
                    client = future_meta.DownloadClient.load(cfg)
                    self.assertEqual(
                        client.meta()
                        .contract_fee_on("SHFE.cu2607", "2026-06-04")
                        .open_fee.value,
                        0.5,
                    )
                    self.assertFalse(client.refresh())

                    state["etag"] = '"v2"'
                    state["archive"] = archive_v2
                    self.assertTrue(client.refresh())
                    self.assertEqual(
                        client.meta()
                        .contract_fee_on("SHFE.cu2607", "2026-06-04")
                        .open_fee.value,
                        0.8,
                    )
                finally:
                    server.shutdown()


if __name__ == "__main__":
    unittest.main()

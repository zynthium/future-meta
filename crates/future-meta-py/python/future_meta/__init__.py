"""Chinese futures fee, margin, and contract specification as-of query client."""

from __future__ import annotations


def _load_native():
    try:
        from . import _native as mod

        return mod
    except ImportError:
        import importlib.machinery
        import importlib.util
        from pathlib import Path
        import subprocess
        import sys

        root = Path(__file__).resolve().parents[4]
        src_rs = root / "crates/future-meta-py/src/lib.rs"
        candidates = [
            root / "target" / profile / name
            for profile in ("debug", "release")
            for name in (
                "libfuture_meta_py.dylib",
                "libfuture_meta_py.so",
                "future_meta_py.dll",
            )
        ]
        existing = [p for p in candidates if p.is_file()]
        needs_build = src_rs.is_file() and (
            not existing
            or max(p.stat().st_mtime for p in existing) < src_rs.stat().st_mtime
        )
        if needs_build:
            subprocess.run(
                ["cargo", "build", "-p", "future-meta-py", "--offline"],
                cwd=root,
                check=True,
            )
            existing = [p for p in candidates if p.is_file()]
        if not existing:
            raise
        lib_path = max(existing, key=lambda p: p.stat().st_mtime)
        loader = importlib.machinery.ExtensionFileLoader(
            "future_meta._native", str(lib_path)
        )
        spec = importlib.util.spec_from_loader("future_meta._native", loader)
        if spec is None:
            raise
        mod = importlib.util.module_from_spec(spec)
        sys.modules["future_meta._native"] = mod
        loader.exec_module(mod)
        return mod


_native = _load_native()

SCHEMA_VERSION = _native.SCHEMA_VERSION
Contract = _native.Contract
ContractFee = _native.ContractFee
ContractHandle = _native.ContractHandle
ContractSpecVersion = _native.ContractSpecVersion
DownloadClient = _native.DownloadClient
DownloadConfig = _native.DownloadConfig
FeeSpec = _native.FeeSpec
FutureMeta = _native.FutureMeta
FutureMetaError = _native.FutureMetaError
ParsedSymbol = _native.ParsedSymbol
PreparedFee = _native.PreparedFee
PreparedFeeBook = _native.PreparedFeeBook
PreparedFeeCursors = _native.PreparedFeeCursors
TradingDayMeta = _native.TradingDayMeta
derive_underlying_symbol = _native.derive_underlying_symbol
encode_archive_json = _native.encode_archive_json
load_or_fetch = _native.load_or_fetch
normalize_futures_symbol = _native.normalize_futures_symbol
parse_symbol = _native.parse_symbol
sha256_hex = _native.sha256_hex

__all__ = [
    "SCHEMA_VERSION",
    "Contract",
    "ContractFee",
    "ContractHandle",
    "ContractSpecVersion",
    "DownloadClient",
    "DownloadConfig",
    "FeeSpec",
    "FutureMeta",
    "FutureMetaError",
    "ParsedSymbol",
    "PreparedFee",
    "PreparedFeeBook",
    "PreparedFeeCursors",
    "TradingDayMeta",
    "derive_underlying_symbol",
    "encode_archive_json",
    "load_or_fetch",
    "normalize_futures_symbol",
    "parse_symbol",
    "sha256_hex",
]

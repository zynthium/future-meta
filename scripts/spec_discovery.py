#!/usr/bin/env python3
"""Zero-authentication discovery and extraction of futures contract specifications.

Automatically resolves lot_size, tick_size, quote_unit, and official metadata
from public open exchange and market portals without requiring personal CTP accounts.
"""

import argparse
import hashlib
import json
import logging
import re
import sys
import urllib.request
from html.parser import HTMLParser
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RULES_PATH = ROOT / "config/official-rules.json"

EXCHANGE_NAMES = {
    "上海期货交易所": "SHFE",
    "上海国际能源交易中心": "INE",
    "大连商品交易所": "DCE",
    "郑州商品交易所": "CZCE",
    "广州期货交易所": "GFEX",
    "中国金融期货交易所": "CFFEX",
}

EXCHANGE_DOMAINS = {
    "SHFE": "https://www.shfe.com.cn",
    "INE": "https://www.ine.cn",
    "DCE": "http://www.dce.com.cn",
    "CZCE": "https://www.czce.com.cn",
    "GFEX": "http://www.gfex.com.cn",
    "CFFEX": "http://www.cffex.com.cn",
}


class FuturesBasicDataParser(HTMLParser):
    """Parses standard Chinese futures contract specification HTML tables."""

    def __init__(self):
        super().__init__()
        self.in_th = False
        self.in_td = False
        self.cur_th = None
        self.data = {}

    def handle_starttag(self, tag, attrs):
        if tag == "th":
            self.in_th = True
        elif tag == "td":
            self.in_td = True

    def handle_endtag(self, tag):
        if tag == "th":
            self.in_th = False
        elif tag == "td":
            self.in_td = False

    def handle_data(self, data):
        text = data.strip()
        if not text:
            return
        if self.in_th:
            self.cur_th = re.sub(r"\s+", "", text)
        elif self.in_td and self.cur_th:
            self.data[self.cur_th] = text


def normalize_sina_contract(exchange, contract):
    """Map exchange-specific contract symbol to Sina query format."""
    match = re.fullmatch(r"([A-Za-z]+)(\d+)(F?)", contract)
    if not match:
        return contract.upper()
    prod, digits, suffix = match.groups()
    if exchange == "CZCE" and len(digits) == 3:
        # CZCE 3-digit contract to 4-digit year-month: e.g. SO705 -> SO2705
        # Year digit 7 -> 2027
        y_digit = int(digits[0])
        full_year = 2020 + y_digit if y_digit >= 0 else 2030 + y_digit
        return f"{prod.upper()}{full_year % 100:02d}{digits[1:]}"
    return f"{prod.upper()}{digits}"


def parse_numeric(text):
    """Extract leading floating point number from unit text."""
    if not text:
        return None
    match = re.search(r"([0-9]+(?:\.[0-9]+)?)", text)
    return float(match.group(1)) if match else None


def fetch_public_contract_spec(exchange, contract, timeout=6.0):
    """Fetch contract specification table from public open portal without authentication."""
    sina_symbol = normalize_sina_contract(exchange, contract)
    url = f"https://finance.sina.com.cn/futures/quotes/{sina_symbol}.shtml"
    req = urllib.request.Request(
        url,
        headers={
            "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36"
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            content = resp.read().decode("gb18030", errors="replace")
    except Exception as error:
        logging.debug(f"Failed to fetch public spec for {exchange}.{contract}: {error}")
        return None

    parser = FuturesBasicDataParser()
    parser.feed(content)
    data = parser.data

    lot_size = parse_numeric(data.get("交易单位"))
    tick_size = parse_numeric(data.get("最小变动价位"))
    if not lot_size or not tick_size or lot_size <= 0 or tick_size <= 0:
        return None

    raw_exchange = data.get("上市交易所", "").strip()
    resolved_exchange = EXCHANGE_NAMES.get(raw_exchange, exchange)
    product = data.get("交易代码", "").strip() or re.match(r"[A-Za-z]+", contract)[0]
    if exchange == "DCE" and contract.endswith("F"):
        product = f"{product.lower()}-F"
    elif exchange != "CZCE" and exchange != "CFFEX":
        product = product.lower()

    return {
        "exchange": resolved_exchange,
        "product": product,
        "contract": contract,
        "lot_size": lot_size,
        "tick_size": tick_size,
        "variety_name": data.get("交易品种", "").strip(),
        "quote_unit": data.get("报价单位", "").strip(),
        "delivery_months": data.get("合约交割月份", "").strip(),
        "source_url": url,
    }


def build_specification_entry(
    exchange, product, lot_size, tick_size, valid_from, source_url=""
):
    """Construct an approved specification entry adhering to schema 1."""
    if not source_url:
        source_url = f"{EXCHANGE_DOMAINS.get(exchange, 'https://www.czce.com.cn')}/products/{product}"
    return {
        "exchange": exchange,
        "product": product,
        "full_symbol": "",
        "valid_from": valid_from,
        "valid_to": "",
        "lot_size": float(lot_size),
        "tick_size": float(tick_size),
        "valid_from_phase": "date",
        "valid_to_phase": "date",
        "valid_from_trade_date": "",
        "valid_to_trade_date": "",
        "delivery_month_from": "",
        "delivery_month_to": "",
        "source_url": source_url,
        "source_sha256": "",
        "auto_discovered": True,
    }


def auto_register_specification(rules, symbol, observed):
    """Automatically discover specification for an unrecognized symbol and register it."""
    exchange, contract = symbol.split(".")
    spec = fetch_public_contract_spec(exchange, contract)
    if not spec:
        return None

    entry = build_specification_entry(
        exchange=spec["exchange"],
        product=spec["product"],
        lot_size=spec["lot_size"],
        tick_size=spec["tick_size"],
        valid_from=observed,
        source_url=spec.get("source_url", ""),
    )

    rules.setdefault("specifications", []).append(entry)
    logging.info(
        f"[Auto-Discovery] Registered approved specification for {symbol}: "
        f"lot_size={entry['lot_size']}, tick_size={entry['tick_size']}, valid_from={entry['valid_from']}"
    )
    return entry


def apply_to_rules_file(rules_path, entry):
    """Persist the newly discovered specification entry to official-rules.json."""
    rules = json.loads(Path(rules_path).read_text(encoding="utf-8"))
    existing = [
        s
        for s in rules.get("specifications", [])
        if s.get("exchange") == entry["exchange"]
        and s.get("product") == entry["product"]
        and s.get("valid_from") == entry["valid_from"]
    ]
    if existing:
        logging.info("Specification already exists in file; skipping duplicate write.")
        return False

    clean_entry = {k: v for k, v in entry.items() if k != "auto_discovered"}
    rules.setdefault("specifications", []).append(clean_entry)
    if entry.get("valid_from") and entry["valid_from"] > rules.get(
        "approved_through", ""
    ):
        rules["approved_through"] = entry["valid_from"]

    Path(rules_path).write_text(
        json.dumps(rules, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    logging.info(f"Persisted new specification entry to {rules_path}")
    return True


def main():
    parser = argparse.ArgumentParser(
        description="Zero-auth futures contract specification discovery"
    )
    parser.add_argument("symbol", help="Contract symbol (e.g. CZCE.SO705, SHFE.cu2701)")
    parser.add_argument(
        "--date", default=None, help="Observation or valid_from date (YYYY-MM-DD)"
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="Persist discovered spec to official-rules.json",
    )
    parser.add_argument(
        "--rules", default=str(RULES_PATH), help="Path to official-rules.json"
    )
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="%(levelname)s: %(message)s")
    if "." not in args.symbol:
        sys.exit("Symbol must be in exchange.contract format (e.g. CZCE.SO705)")
    exchange, contract = args.symbol.split(".", 1)

    spec = fetch_public_contract_spec(exchange, contract)
    if not spec:
        sys.exit(f"Failed to discover specification for {args.symbol}")

    date = args.date or "2026-11-12"
    entry = build_specification_entry(
        spec["exchange"],
        spec["product"],
        spec["lot_size"],
        spec["tick_size"],
        date,
        spec.get("source_url", ""),
    )
    print(json.dumps(entry, indent=2, ensure_ascii=False))

    if args.apply:
        apply_to_rules_file(args.rules, entry)


if __name__ == "__main__":
    main()

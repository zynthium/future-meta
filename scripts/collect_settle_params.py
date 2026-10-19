#!/usr/bin/env python3
"""
collect_settle_params.py - Unified Chinese Futures Settlement Parameters Collector.

Collects official daily/snapshot settlement parameters (margins, fees, and settle prices)
across all 6 Chinese futures exchanges:
- SHFE (Shanghai Futures Exchange)
- INE (Shanghai International Energy Exchange)
- CZCE (Zhengzhou Commodity Exchange)
- DCE (Dalian Commodity Exchange - with automated Ruishu 6 bypass)
- CFFEX (China Financial Futures Exchange)
- GFEX (Guangzhou Futures Exchange)

Output format (15 columns):
  date, exchange, symbol, variety,
  long_margin_ratio, short_margin_ratio, hedge_long_margin_ratio, hedge_short_margin_ratio,
  settle_price,
  fee_type, open_fee, offset_fee, close_today_fee, delivery_fee, intraday_open_fee
"""

import argparse
import base64
import csv
import hashlib
import json
import logging
import math
import os
import re
import sys
import tempfile
import time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor
from datetime import date, timedelta
from html.parser import HTMLParser
from pathlib import Path
from typing import List, Optional, Set, Tuple
from xml.etree import ElementTree

import requests

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
    handlers=[logging.StreamHandler(sys.stdout)],
)
logger = logging.getLogger("settle_collector")

CSV_COLUMNS = [
    "date",
    "exchange",
    "symbol",
    "variety",
    "long_margin_ratio",
    "short_margin_ratio",
    "hedge_long_margin_ratio",
    "hedge_short_margin_ratio",
    "settle_price",
    "fee_type",
    "open_fee",
    "offset_fee",
    "close_today_fee",
    "delivery_fee",
    "intraday_open_fee",
]

INE_PRODUCTS = frozenset({"sc", "nr", "lu", "bc", "ec"})

USER_AGENT = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"


def get_chrome_path() -> Optional[str]:
    if os.environ.get("CHROME_PATH") and os.path.exists(os.environ["CHROME_PATH"]):
        return os.environ["CHROME_PATH"]
    candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
        "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    ]
    for c in candidates:
        if os.path.exists(c):
            return c
    return None


def clean_num(val: any) -> Optional[float]:
    if val is None:
        return None
    s = str(val).strip().replace(",", "").replace("%", "")
    if not s or s in ("—", "-", "--"):
        return None
    try:
        number = float(s)
    except ValueError:
        return None
    if not math.isfinite(number):
        raise ValueError(f"Non-finite official number: {val!r}")
    return number


def fetch_with_retry(
    url: str,
    method: str = "GET",
    headers: Optional[dict] = None,
    timeout: int = 15,
    max_retries: int = 5,
    backoff: float = 1.0,
) -> requests.Response:
    hdr = {"User-Agent": USER_AGENT}
    if headers:
        hdr.update(headers)

    for attempt in range(1, max_retries + 1):
        try:
            r = requests.request(method, url, headers=hdr, timeout=timeout)
            if r.status_code == 200:
                return r
            else:
                logger.warning(f"HTTP {r.status_code} on {url} (attempt {attempt}/{max_retries})")
        except Exception as e:
            logger.warning(f"Request error on {url}: {e} (attempt {attempt}/{max_retries})")

        if attempt < max_retries:
            time.sleep(backoff * (2 ** (attempt - 1)))
    raise RuntimeError(f"Official request failed after {max_retries} attempts: {url}")


# ----------------------------------------------------------------------
# Trading Calendar & Helpers
# ----------------------------------------------------------------------
def get_existing_dates_in_csv(filepath: str) -> Set[str]:
    dates = set()
    if os.path.exists(filepath) and os.path.getsize(filepath) > 0:
        try:
            with open(filepath, "r", encoding="utf-8") as f:
                reader = csv.DictReader(f)
                for r in reader:
                    if r.get("date"):
                        dates.add(r["date"])
        except Exception as e:
            logger.warning(f"Error reading existing dates from {filepath}: {e}")
    return dates


def append_records_to_csv(filepath: str, records: List[dict]):
    if not records:
        return
    file_exists = os.path.exists(filepath) and os.path.getsize(filepath) > 0
    with open(filepath, "a", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=CSV_COLUMNS)
        if not file_exists:
            writer.writeheader()
        writer.writerows(records)


def deduplicate_and_sort_csv(filepath: str):
    if not os.path.exists(filepath):
        return
    records = []
    with open(filepath, "r", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        for r in reader:
            records.append(r)

    dedup = {}
    for r in records:
        key = (r["date"], r["symbol"])
        dedup[key] = r

    sorted_records = sorted(dedup.values(), key=lambda x: (x["date"], x["symbol"]))
    with open(filepath, "w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=CSV_COLUMNS)
        writer.writeheader()
        writer.writerows(sorted_records)


class RawStore:
    """Read official response bytes, never business values from derived CSVs."""

    HOSTS = {"www.shfe.com.cn", "www.ine.cn", "www.czce.com.cn",
             "www.dce.com.cn", "www.gfex.com.cn", "www.cffex.com.cn"}

    def __init__(self, root: Path, evidence_dir: Optional[Path], offline: bool):
        self.root = root
        self.offline = offline
        self.index = {}
        self.provenance_dir = None
        self.sources = {}
        if evidence_dir and evidence_dir.exists():
            patterns = ("ine-dailydata-*.tsv", "czce-daily-params-*.tsv",
                        "cffex-calendar-*.tsv")
            for pattern in patterns:
                for manifest in sorted(evidence_dir.glob(pattern)):
                    with manifest.open(encoding="utf-8", newline="") as handle:
                        for row in csv.DictReader(handle, delimiter="\t"):
                            sha = row.get("sha256", "")
                            url = row.get("canonical_url") or row.get("url", "")
                            if row.get("status", "ok") != "ok" or not sha or not url:
                                continue
                            self.check_url(url)
                            if not re.fullmatch(r"[a-f0-9]{64}", sha):
                                raise ValueError(f"Invalid official evidence SHA: {manifest}")
                            suffix = Path(urllib.parse.urlparse(url).path).suffix
                            source = evidence_dir / f"{sha}{suffix}"
                            if not source.exists():
                                raise FileNotFoundError(f"Missing indexed official raw: {source}")
                            # Prefer the latest retained source update, not manifest order.
                            rank = (row.get("report_date", ""), row.get("update_date", ""))
                            previous = self.index.get(url)
                            if previous is None or rank > previous[2]:
                                self.index[url] = (source, sha, rank)

    @classmethod
    def check_url(cls, url: str):
        parsed = urllib.parse.urlparse(url)
        if parsed.scheme not in ("http", "https") or parsed.hostname not in cls.HOSTS:
            raise ValueError(f"Not an official exchange URL: {url}")

    @staticmethod
    def unpack(wrapper: dict, margin: bool = False) -> bytes:
        prefix = "margin_" if margin and ("margin_body" in wrapper or "margin_body_base64" in wrapper) else ""
        if prefix + "body_base64" in wrapper:
            body = base64.b64decode(wrapper[prefix + "body_base64"], validate=True)
        elif prefix + "body" in wrapper:
            body = wrapper[prefix + "body"].encode("utf-8")
        else:
            raise ValueError("Official raw wrapper has no response bytes")
        sha = wrapper.get(prefix + "sha256")
        if not sha or hashlib.sha256(body).hexdigest() != sha:
            raise ValueError("Official raw SHA-256 mismatch")
        return body

    def retain_source(self, exchange, day, url, body, margin=False):
        if self.provenance_dir is None:
            return
        digest = hashlib.sha256(body).hexdigest()
        folder = self.provenance_dir / "raw"
        folder.mkdir(parents=True, exist_ok=True)
        path = folder / digest
        if not path.exists():
            with tempfile.NamedTemporaryFile(dir=folder, delete=False) as handle:
                temporary = Path(handle.name)
                handle.write(body)
            os.replace(temporary, path)
        self.sources[(exchange, day or "", url, margin, digest)] = {
            "exchange": exchange, "date": day, "canonical_url": url,
            "sha256": digest, "raw_path": str(path.resolve()), "margin": margin,
        }

    def cached(self, exchange: str, day: Optional[str], url: str,
               key: Optional[str] = None, margin: bool = False) -> Optional[bytes]:
        self.check_url(url)
        path = self.root / exchange.lower() / f"{key or day}.json"
        margin_path = self.root / exchange.lower() / f"margin-{day}.json"
        if margin and margin_path.exists():
            path = margin_path
        if path.exists():
            wrapper = json.loads(path.read_text(encoding="utf-8"))
            if wrapper.get("exchange") != exchange or (day and wrapper.get("date") != day):
                raise ValueError(f"Official raw exchange/date mismatch: {path}")
            stored_url = (wrapper.get("margin_canonical_url") or wrapper.get("canonical_url")) if margin else wrapper.get("canonical_url")
            if stored_url and stored_url != url:
                raise ValueError(f"Official raw URL mismatch: {path}: {stored_url}")
            body = self.unpack(wrapper, margin)
            # Legacy INE wrappers contain either its daily JSON or both sources.
            if margin and "o_cursor" not in json.loads(body):
                raise ValueError(f"INE margin raw does not contain its daily response: {path}")
            self.retain_source(exchange, day, url, body, margin)
            return body
        indexed = self.index.get(url)
        if indexed:
            body = indexed[0].read_bytes()
            if hashlib.sha256(body).hexdigest() != indexed[1]:
                raise ValueError(f"Official evidence SHA-256 mismatch: {indexed[0]}")
            self.retain_source(exchange, day, url, body, margin)
            return body
        return None

    def save(self, exchange: str, day: Optional[str], url: str, body: bytes,
             key: Optional[str] = None, request: Optional[dict] = None):
        self.check_url(url)
        self.retain_source(exchange, day, url, body)
        folder = self.root / exchange.lower()
        folder.mkdir(parents=True, exist_ok=True)
        path = folder / f"{key or day}.json"
        wrapper = {
            "exchange": exchange, "date": day or date.today().isoformat(),
            "canonical_url": url, "body_base64": base64.b64encode(body).decode("ascii"),
            "sha256": hashlib.sha256(body).hexdigest(),
            "request": request or {"method": "GET"},
        }
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=folder,
                                         prefix=path.name + ".", delete=False) as handle:
            temporary = Path(handle.name)
            json.dump(wrapper, handle, ensure_ascii=False)
        try:
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)

    def get(self, exchange: str, day: Optional[str], url: str,
            key: Optional[str] = None, margin: bool = False,
            refresh: bool = False, form: Optional[dict] = None) -> bytes:
        if not refresh or self.offline:
            body = self.cached(exchange, day, url, key, margin)
            if body is not None:
                return body
        if self.offline:
            raise FileNotFoundError(f"Offline official raw missing: {exchange} {day or key}: {url}")
        self.check_url(url)
        if form is None:
            response = fetch_with_retry(url)
        else:
            # Requests data, including trade_date, is retained with the exact response.
            response = requests.post(url, data=form, headers={"User-Agent": USER_AGENT}, timeout=30)
            response.raise_for_status()
        body = response.content
        save_key = key
        # Do not overwrite the legacy combined INE wrapper while adding its daily raw.
        if margin and (self.root / exchange.lower() / f"{day}.json").exists():
            save_key = f"margin-{day}"
        self.save(exchange, day, url, body, save_key,
                  {"method": "POST", "form": form} if form is not None else None)
        return body


def trading_days(start: str, end: str, store: RawStore) -> List[str]:
    """Weekdays minus explicit exchange holidays, with every month evidenced."""
    first, last = date.fromisoformat(start), date.fromisoformat(end)
    if first > last:
        raise ValueError("--start-date must not be after --end-date")
    month = first.replace(day=1)
    holidays = set()
    while month <= last:
        period = month.strftime("%Y-%m")
        url = f"http://www.cffex.com.cn/sj/jyrl/{month:%Y%m}/index_6782.xml"
        body = store.get("CALENDAR", None, url, key=period)
        root = ElementTree.fromstring(body)
        if root.tag != "docs":
            raise ValueError(f"Invalid official trading calendar: {period}")
        events = [(doc.findtext("pubdate", "").strip(), doc.findtext("title", "").strip())
                  for doc in root.findall("doc")]
        if not any(day.startswith(period + "-") for day, _ in events):
            raise ValueError(f"Official calendar does not cover requested month: {period}")
        # These monthly documents contain adjacent-month events too. Keep their
        # explicit holidays; distinct event dates are not a trading-day calendar.
        for day, title in events:
            parsed = date.fromisoformat(day)
            if title in ("假日休市", "休市"):
                holidays.add(parsed)
        month = (month.replace(day=28) + timedelta(days=4)).replace(day=1)
    result = []
    day = first
    while day <= last:
        if day.weekday() < 5 and day not in holidays:
            result.append(day.isoformat())
        day += timedelta(days=1)
    return result


def validate_records(records: List[dict], exchange: str, day: str,
                     futures_only: bool = False) -> List[dict]:
    if not records:
        raise ValueError(f"Empty official parameters: {exchange} {day}")
    result, symbols = [], set()
    for record in records:
        symbol = record["symbol"]
        if (exchange == "DCE" and symbol.endswith("F")) or (futures_only and exchange == "CFFEX" and record["variety"] in {"IO", "MO", "HO"}):
            continue
        if symbol in symbols:
            raise ValueError(f"Duplicate official contract: {exchange} {day} {symbol}")
        symbols.add(symbol)
        required = ["long_margin_ratio", "short_margin_ratio", "hedge_long_margin_ratio",
                    "hedge_short_margin_ratio", "open_fee", "offset_fee", "close_today_fee"]
        if exchange in {"DCE", "GFEX"}:
            required.append("intraday_open_fee")
        for field in required:
            value = record.get(field)
            if value is None or not math.isfinite(float(value)) or float(value) < 0:
                raise ValueError(f"Invalid official field: {exchange} {day} {symbol} {field}={value}")
        if record["fee_type"] not in {"cny_per_lot", "rate_per_10k"}:
            raise ValueError(f"Invalid official fee mode: {exchange} {day} {symbol}")
        if record["date"] != day or record["exchange"] != exchange:
            raise ValueError(f"Parsed date/exchange mismatch: {exchange} {day} {symbol}")
        result.append(record)
    if not result:
        raise ValueError(f"No requested futures parameters: {exchange} {day}")
    return sorted(result, key=lambda record: record["symbol"])


# ----------------------------------------------------------------------
# 1. SHFE & INE Fetcher
# ----------------------------------------------------------------------
def fetch_shfe_ine(date_str: str, exchange: str, store: RawStore) -> List[dict]:
    clean_date = date_str.replace("-", "")
    domain = "www.shfe.com.cn" if exchange == "SHFE" else "www.ine.cn"
    is_shfe = exchange == "SHFE"
    if is_shfe:
        url = f"https://{domain}/data/busiparamdata/future/Settlement{clean_date}.dat"
    else:
        url = f"https://{domain}/data/tradedata/future/dailydata/js{clean_date}.dat"

    fees_url = f"https://www.shfe.com.cn/data/busiparamdata/future/Settlement{clean_date}.dat"
    fee_data = json.loads(store.get("SHFE", date_str, fees_url))
    data = fee_data if is_shfe else json.loads(store.get("INE", date_str, url, margin=True))
    if str(data.get("report_date", "")).strip() != clean_date:
        raise ValueError(f"Official margin report_date mismatch: {exchange} {date_str}")
    if str(fee_data.get("report_date", "")).strip() != clean_date:
        raise ValueError(f"Official fee report_date mismatch: {exchange} {date_str}")
    fees_by_symbol = {item["INSTRUMENTID"].strip(): item for item in fee_data.get("Settlement", [])}
    cursor = data.get("Settlement" if is_shfe else "o_cursor", [])
    records = []
    for item in cursor:
        raw_symbol = item.get("INSTRUMENTID", "").strip()
        if not raw_symbol:
            continue

        raw_prod = item.get("PRODUCTID", "").strip().lower().replace("_f", "")
        variety_match = re.match(r"([A-Za-z]+)", raw_symbol)
        variety = variety_match.group(1).lower() if variety_match else raw_prod

        # The SHFE parameter files also contain all INE products.
        if (variety in INE_PRODUCTS) != (exchange == "INE"):
            continue
        fee_item = fees_by_symbol.get(raw_symbol)
        if fee_item is None and exchange == "INE" and ("TRADEFEEUNIT" in item or "TRADEFEERATIO" in item):
            fee_item = item
        if fee_item is None or (fee_item is not item and str(fee_item.get("TRADINGDAY", "")).strip() != clean_date):
            raise ValueError(f"Missing or mismatched official fee row: {date_str} {raw_symbol}")

        spec_long = clean_num(item.get("SPEC_LONGMARGINRATIO" if is_shfe else "SPECLONGMARGINRATIO"))
        spec_short = clean_num(item.get("SPEC_SHORTMARGINRATIO" if is_shfe else "SPECSHORTMARGINRATIO"))
        hedge_long = clean_num(item.get("LONGMARGINRATIO" if is_shfe else "HEDGLONGMARGINRATIO"))
        hedge_short = clean_num(item.get("SHORTMARGINRATIO" if is_shfe else "HEDGSHORTMARGINRATIO"))
        settle = clean_num(item.get("SETTLEMENTPRICE"))

        long_ratio = round(spec_long * 100, 4) if spec_long is not None else None
        short_ratio = round(spec_short * 100, 4) if spec_short is not None else None
        h_long_ratio = round(hedge_long * 100, 4) if hedge_long is not None else None
        h_short_ratio = round(hedge_short * 100, 4) if hedge_short is not None else None

        fee_ratio = clean_num(fee_item.get("TRADEFEERATION") or fee_item.get("TRADEFEERATIO"))
        fee_unit = clean_num(fee_item.get("TRADEFEEUNIT"))
        deliv_unit = clean_num(fee_item.get("COMMODITYDELIVERYFEEUNIT") or fee_item.get("COMMODITYDELIVFEEUNIT"))
        deliv_ratio = clean_num(fee_item.get("COMMODITYDELIVERYFEERATION") or fee_item.get("COMMODITYDELIVFEERATIO"))
        ratio_scale = 10000

        if fee_unit is not None and fee_unit > 0:
            fee_type = "cny_per_lot"
            open_fee = fee_unit
        elif fee_ratio is not None and fee_ratio > 0:
            fee_type = "rate_per_10k"
            open_fee = round(fee_ratio * ratio_scale, 4)
        elif fee_unit is not None or fee_ratio is not None:
            fee_type = "cny_per_lot"
            open_fee = 0.0
        else:
            fee_type = ""
            open_fee = None

        offset_fee = open_fee
        # Official prmsummary displays DISCOUNTRATE * 100 as a percentage.
        # The raw value is the multiplier, including 0 and values above 1.
        discount = clean_num(fee_item.get("DISCOUNTRATE"))
        if discount is None:
            # Bound specifically to official launch fee exemption notice:
            # 上能发〔2018〕13号《关于原油期货上市有关事项的通知》
            # Applicable scope: INE crude oil (sc) from launch (2018-03-26) until parameter file integration (2018-10-24).
            # "原油期货交易手续费为人民币20元/手，平今仓交易免收交易手续费。"
            # Canonical URL: https://www.ine.cn/publicnotice/notice/201803/t20180312_811177.html
            # SHA-256: d3aa2ae4ae1814e9c1e2743fd9e91b56cf9e3fb78c0b62a66375b4eb8b55ca6a
            if exchange == "INE" and variety == "sc" and "2018-03-26" <= date_str < "2018-10-24":
                discount = 0.0
            else:
                raise ValueError(f"Missing official DISCOUNTRATE: {date_str} {raw_symbol}")
        if not 0 <= discount < float("inf") or open_fee is None:
            raise ValueError(f"Missing or invalid official fee fields: {date_str} {raw_symbol}")
        close_today_fee = round(open_fee * discount, 10)

        if deliv_unit is not None and deliv_unit > 0:
            deliv_fee = deliv_unit
        elif deliv_ratio is not None and deliv_ratio > 0:
            deliv_fee = round(deliv_ratio * ratio_scale, 4)
        elif (deliv_unit is not None or deliv_ratio is not None) and all(
            value is None or value == 0 for value in (deliv_unit, deliv_ratio)
        ):
            deliv_fee = 0.0
        else:
            deliv_fee = None

        records.append({
            "date": date_str,
            "exchange": exchange,
            "symbol": raw_symbol,
            "variety": variety,
            "long_margin_ratio": long_ratio,
            "short_margin_ratio": short_ratio,
            "hedge_long_margin_ratio": h_long_ratio,
            "hedge_short_margin_ratio": h_short_ratio,
            "settle_price": settle,
            "fee_type": fee_type,
            "open_fee": open_fee,
            "offset_fee": offset_fee,
            "close_today_fee": close_today_fee,
            "delivery_fee": deliv_fee,
        })
    return records


# ----------------------------------------------------------------------
# 2. CZCE Fetcher
# ----------------------------------------------------------------------
class CzceTableParser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.active = False
        self.cell = None
        self.row = []
        self.rows = []

    def handle_starttag(self, tag, attrs):
        if tag == "table" and dict(attrs).get("id") == "tab1":
            self.active = True
        if self.active and tag == "tr":
            # Historical official pages omit </tr> between data rows.
            if self.row:
                self.rows.append(self.row)
            self.row = []
        if self.active and tag in {"td", "th"}:
            self.cell = []

    def handle_data(self, data):
        if self.cell is not None:
            self.cell.append(data)

    def handle_endtag(self, tag):
        if self.active and tag in {"td", "th"} and self.cell is not None:
            self.row.append("".join(self.cell).strip())
            self.cell = None
        if self.active and tag == "tr" and self.row:
            self.rows.append(self.row)
            self.row = []
        if self.active and tag == "table":
            if self.row:
                self.rows.append(self.row)
                self.row = []
            self.active = False


def fetch_czce(date_str: str, store: RawStore) -> List[dict]:
    clean_date = date_str.replace("-", "")
    year = clean_date[:4]
    url = f"https://www.czce.com.cn/cn/DFSStaticFiles/Future/{year}/{clean_date}/FutureDataClearParams.txt"

    html_url = url.removesuffix(".txt") + ".htm"
    # Existing evidence indexes retain the official HTML version of the same
    # table. Parse it directly instead of substituting a previously derived CSV.
    if html_url in store.index:
        url = html_url
    text = store.get("CZCE", date_str, url).decode("utf-8-sig")
    source_date = re.search(r"\b(\d{4}-\d{2}-\d{2})\b", text)
    if not source_date or source_date.group(1) != date_str:
        raise ValueError(f"CZCE official table date mismatch: {date_str}")
    if "<table" in text.lower():
        parser = CzceTableParser()
        parser.feed(text)
        lines = ["|".join(row) for row in parser.rows]
    else:
        lines = [line.strip() for line in text.splitlines() if line.strip()]
    if len(lines) < 2:
        return []

    header_idx = -1
    for idx, line in enumerate(lines[:5]):
        if "合约代码" in line:
            header_idx = idx
            break

    if header_idx == -1:
        return []

    headers = [col.strip() for col in lines[header_idx].split("|")]
    col_map = {col: i for i, col in enumerate(headers)}

    def find_col(candidates):
        for c in candidates:
            for k, idx in col_map.items():
                if c in k:
                    return idx
        return None

    symbol_col = find_col(["合约代码", "品种合约"])
    settle_col = find_col(["当日结算价", "结算价"])
    margin_col = find_col(["交易保证金率", "保证金率"])
    trade_fee_col = find_col(["交易手续费"])
    fee_style_col = find_col(["手续费收取方式"])
    close_today_col = find_col(["日内平今仓交易手续费", "平今手续费", "平今仓"])
    deliv_col = find_col(["交割手续费"])

    if any(column is None for column in (symbol_col, margin_col, trade_fee_col, close_today_col)):
        raise ValueError(f"CZCE required official columns missing: {date_str}")

    records = []
    for line in lines[header_idx + 1 :]:
        if "小计" in line or "合计" in line or "总计" in line:
            continue
        parts = [p.strip() for p in line.split("|")]
        if len(parts) <= max(symbol_col, margin_col):
            continue

        symbol = parts[symbol_col]
        if not symbol or not re.match(r"^[A-Za-z0-9]+$", symbol):
            continue

        variety_match = re.match(r"^([A-Za-z]+)", symbol)
        variety = variety_match.group(1).upper() if variety_match else ""

        margin_val = clean_num(parts[margin_col])
        settle_val = clean_num(parts[settle_col]) if settle_col is not None else None

        style_val = parts[fee_style_col] if fee_style_col is not None and len(parts) > fee_style_col else ""
        # Legacy official tables omit the mode column and quote CNY per lot.
        if not style_val.strip() or "绝对值" in style_val:
            fee_type = "cny_per_lot"
        elif "比例值" in style_val:
            fee_type = "rate_per_10k"
        else:
            raise ValueError(f"Unknown CZCE fee mode: {style_val!r} on {date_str}")
        open_fee = clean_num(parts[trade_fee_col]) if trade_fee_col is not None and len(parts) > trade_fee_col else None
        offset_fee = open_fee
        close_today_fee = clean_num(parts[close_today_col]) if close_today_col is not None and len(parts) > close_today_col else None
        deliv_fee = clean_num(parts[deliv_col]) if deliv_col is not None and len(parts) > deliv_col else None

        records.append({
            "date": date_str,
            "exchange": "CZCE",
            "symbol": symbol,
            "variety": variety,
            "long_margin_ratio": margin_val,
            "short_margin_ratio": margin_val,
            "hedge_long_margin_ratio": margin_val,
            "hedge_short_margin_ratio": margin_val,
            "settle_price": settle_val,
            "fee_type": fee_type,
            "open_fee": open_fee,
            "offset_fee": offset_fee,
            "close_today_fee": close_today_fee,
            "delivery_fee": deliv_fee,
        })
    return records


# ----------------------------------------------------------------------
# 3. CFFEX Fetcher
# ----------------------------------------------------------------------
def get_cffex_catalog(
    store: RawStore,
    min_date: Optional[str] = None,
) -> List[Tuple[str, str]]:
    catalog = []
    base_url = "http://www.cffex.com.cn/cn/jscs.html"
    first_page = store.get("CFFEX", None, base_url, key="catalog-0001", refresh=True).decode("utf-8")

    match = re.search(r"jump\('(\d+)'\)", first_page)
    if not match:
        raise ValueError("Official CFFEX catalog pagination is missing")
    total_pages = int(match.group(1))
    if total_pages < 1:
        raise ValueError("Official CFFEX catalog has no pages")

    logger.info(f"Scanning CFFEX catalog (up to {total_pages} pages)...")
    visited_urls = set()
    for page in range(1, total_pages + 1):
        page_url = base_url if page == 1 else f"http://www.cffex.com.cn/cn/jscs_{page}.html"
        page_text = first_page if page == 1 else store.get(
            "CFFEX", None, page_url, key=f"catalog-{page:04d}", refresh=True,
        ).decode("utf-8")
        page_dates = []
        for href in re.findall(r'href=[\"\']([^\"\']*\.csv)[\"\']', page_text):
            if href not in visited_urls:
                visited_urls.add(href)
                date_match = re.search(r"(\d{8})_1\.csv", href)
                if date_match:
                    d_raw = date_match.group(1)
                    d_fmt = f"{d_raw[:4]}-{d_raw[4:6]}-{d_raw[6:8]}"
                    full_url = "http://www.cffex.com.cn" + href if href.startswith("/") else href
                    catalog.append((d_fmt, full_url))
                    page_dates.append(d_fmt)
        time.sleep(0.05)
        if not page_dates:
            raise ValueError(f"Official CFFEX catalog page {page} contains no parameter snapshots")
        if min_date and page_dates and all(d < min_date for d in page_dates):
            break

    cffex_raw = store.root / "cffex"
    if cffex_raw.exists():
        for path in cffex_raw.glob("????-??-??.json"):
            d_fmt = path.stem
            try:
                meta = json.loads(path.read_text(encoding="utf-8"))
                url = meta.get("canonical_url")
                if url and (d_fmt, url) not in catalog:
                    catalog.append((d_fmt, url))
            except Exception:
                pass

    catalog = sorted(list(dict.fromkeys(catalog)), key=lambda x: x[0])
    return catalog


def fetch_cffex_file(date_str: str, file_url: str, store: RawStore) -> List[dict]:
    body = store.get("CFFEX", date_str, file_url)
    lines = [line.strip() for line in body.decode("gbk").splitlines() if line.strip()]
    if not lines or date_str.replace("-", "") not in " ".join(lines[:2]):
        raise ValueError(f"CFFEX official snapshot title/date mismatch: {date_str}")
    if len(lines) < 2:
        return []

    header_idx = -1
    for idx, line in enumerate(lines[:5]):
        if "合约多头保证金标准" in line:
            header_idx = idx
            break

    if header_idx == -1:
        return []

    reader = csv.reader(lines[header_idx:])
    try:
        headers = next(reader)
    except StopIteration:
        return []

    header_map = {h.strip(): i for i, h in enumerate(headers)}
    symbol_idx = header_map.get("期货合约") or 0
    long_idx = header_map.get("合约多头保证金标准") or 1
    short_idx = header_map.get("合约空头保证金标准") or 2
    fee_idx = header_map.get("交易手续费标准")
    deliv_idx = header_map.get("交割手续费标准")
    close_today_idx = header_map.get("平今仓收取率")
    if fee_idx is None or close_today_idx is None:
        raise ValueError(f"CFFEX required official fee columns missing: {date_str}")

    records = []
    for row in reader:
        if not row or len(row) <= max(symbol_idx, long_idx, short_idx):
            continue
        symbol = row[symbol_idx].strip()
        if not re.match(r"^[A-Z0-9]+$", symbol):
            continue

        variety_match = re.match(r"^([A-Z]+)", symbol)
        variety = variety_match.group(1) if variety_match else ""

        long_val = clean_num(row[long_idx])
        short_val = clean_num(row[short_idx])

        fee_str = row[fee_idx].strip() if fee_idx is not None and len(row) > fee_idx else ""
        deliv_str = row[deliv_idx].strip() if deliv_idx is not None and len(row) > deliv_idx else ""
        close_ratio_str = row[close_today_idx].strip() if close_today_idx is not None and len(row) > close_today_idx else ""

        if "万分之" in fee_str:
            fee_type = "rate_per_10k"
            open_fee = clean_num(fee_str.replace("万分之", ""))
        elif "元/手" in fee_str:
            fee_type = "cny_per_lot"
            open_fee = clean_num(fee_str.replace("元/手", ""))
        else:
            fee_type = "rate_per_10k"
            open_fee = clean_num(fee_str)

        offset_fee = open_fee
        ratio_pct = clean_num(close_ratio_str.replace("%", ""))
        if ratio_pct is not None and open_fee is not None:
            close_today_fee = 0.0 if ratio_pct == 0.0 else round(open_fee * (ratio_pct / 100.0), 4)
        else:
            close_today_fee = None

        deliv_fee = clean_num(deliv_str.replace("万分之", "").replace("元/手", "")) if deliv_str else None

        records.append({
            "date": date_str,
            "exchange": "CFFEX",
            "symbol": symbol,
            "variety": variety,
            "long_margin_ratio": long_val,
            "short_margin_ratio": short_val,
            "hedge_long_margin_ratio": long_val,
            "hedge_short_margin_ratio": short_val,
            "settle_price": None,
            "fee_type": fee_type,
            "open_fee": open_fee,
            "offset_fee": offset_fee,
            "close_today_fee": close_today_fee,
            "delivery_fee": deliv_fee,
        })
    return records


def forward_fill_cffex_daily(snapshot_csv: str, output_csv: str, trading_days: List[str]):
    """
    Forward-fill CFFEX snapshot parameters across all trading days.
    """
    if not os.path.exists(snapshot_csv) or not trading_days:
        raise ValueError("CFFEX forward-fill requires verified snapshots and trading days")
    snapshots = {}
    with open(snapshot_csv, "r", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        for r in reader:
            d = r.get("date")
            if d:
                snapshots.setdefault(d, []).append(r)

    snapshot_dates = sorted(snapshots.keys())
    if not snapshot_dates:
        raise ValueError("CFFEX forward-fill has no verified snapshots")

    daily_records = []
    for day in trading_days:
        valid_snapshots = [s for s in snapshot_dates if s <= day]
        if not valid_snapshots:
            raise ValueError(f"No official CFFEX baseline snapshot for {day}")
        latest_s = valid_snapshots[-1]
        for row in snapshots[latest_s]:
            r_copy = dict(row)
            r_copy["date"] = day
            daily_records.append(r_copy)

    with open(output_csv, "w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=CSV_COLUMNS)
        writer.writeheader()
        writer.writerows(daily_records)
    logger.info(f"[CFFEX] Generated forward-filled daily table: {output_csv} ({len(daily_records)} rows across {len(trading_days)} days)")


# ----------------------------------------------------------------------
# 4. GFEX Fetcher
# ----------------------------------------------------------------------
def parse_dce_gfex(data: dict, day: str, exchange: str) -> List[dict]:
    if exchange == "DCE":
        if data.get("success") is not True:
            raise ValueError(f"Official DCE response failed: {day}: {data.get('msg')}")
    elif str(data.get("code")) != "0":
        raise ValueError(f"Official GFEX response failed: {day}: {data.get('msg')}")
    if exchange == "GFEX" and data.get("param", {}).get("trade_date") != [day.replace("-", "")]:
        raise ValueError(f"GFEX official trade_date mismatch: {day}")
    records = []
    for item in data.get("data", []):
        symbol = item.get("contractId", "").strip()
        if not symbol:
            raise ValueError(f"Official {exchange} row has no contract: {day}")
        match = re.match(r"^([a-zA-Z]+)", symbol)
        variety = item.get("varietyOrder", "").strip() if exchange == "DCE" else (match.group(1).lower() if match else "")
        style = item.get("style", "").strip()
        if style not in {"绝对值", "比例值"}:
            raise ValueError(f"Unknown official {exchange} fee style: {day} {symbol}: {style!r}")
        record = {
            "date": day, "exchange": exchange, "symbol": symbol, "variety": variety,
            "settle_price": clean_num(item.get("clearPrice")),
            "fee_type": "cny_per_lot" if style == "绝对值" else "rate_per_10k",
            "open_fee": clean_num(item.get("openFee")),
            "offset_fee": clean_num(item.get("offsetFee")),
            "close_today_fee": clean_num(item.get("shortOffsetFee")),
            "intraday_open_fee": clean_num(item.get("shortOpenFee")),
            "delivery_fee": None,
        }
        for field, source in [("long_margin_ratio", "specBuyRate"),
                              ("short_margin_ratio", "specSellRate"),
                              ("hedge_long_margin_ratio", "hedgeBuyRate"),
                              ("hedge_short_margin_ratio", "hedgeSellRate")]:
            value = clean_num(item.get(source))
            record[field] = round(value * 100, 4) if value is not None else None
        records.append(record)
    return records


def fetch_gfex_day(day: str, store: RawStore) -> List[dict]:
    url = "http://www.gfex.com.cn/u/interfacesWebTiFutAndOptSettle/loadList"
    body = store.get("GFEX", day, url, form={"trade_date": day.replace("-", ""), "variety": ""})
    return parse_dce_gfex(json.loads(body), day, "GFEX")


def collect_dce(days: List[str], output_csv: str, store: RawStore, futures_only: bool = False):
    url = "http://www.dce.com.cn/dcereport/publicweb/tradepara/futAndOptSettle"
    pending = []
    for day in days:
        body = store.cached("DCE", day, url)
        if body is None:
            if store.offline:
                raise FileNotFoundError(f"Offline official DCE raw missing: {day}")
            pending.append(day)
        else:
            records = validate_records(parse_dce_gfex(json.loads(body), day, "DCE"), "DCE", day, futures_only)
            append_records_to_csv(output_csv, records)
    if not pending:
        return

    from playwright.sync_api import sync_playwright

    with sync_playwright() as playwright:
        chrome = get_chrome_path()
        options = {"headless": False, "args": ["--disable-blink-features=AutomationControlled", "--no-sandbox"]}
        if chrome:
            options["executable_path"] = chrome
        else:
            options["channel"] = "chrome"
        browser = playwright.chromium.launch(**options)
        try:
            page = browser.new_page()
            page.goto("http://www.dce.com.cn/", timeout=20000)
            page.wait_for_timeout(3500)
            page.goto("http://www.dce.com.cn/frontend/dcereport/#/zh/queryFutAndOptSettle?variety=all&tradeType=1", timeout=20000)
            page.wait_for_timeout(4000)
            for i, day in enumerate(pending, 1):
                if i % 20 == 1 or i == len(pending):
                    logger.info(f"[DCE] Collecting day {i}/{len(pending)}: {day}")
                params = {"varietyId": "all", "tradeDate": day.replace("-", ""), "tradeType": "1", "lang": "zh"}
                result = page.evaluate("""async (params) => {
                    const response = await fetch('/dcereport/publicweb/tradepara/futAndOptSettle', {
                        method: 'POST', headers: {'Content-Type': 'application/json', 'clientid': 'web',
                          'access-control-allow-origin-type': '*'}, body: JSON.stringify(params)
                    });
                    if (!response.ok) throw new Error('Official DCE HTTP ' + response.status);
                    return await response.text();
                }""", params)
                body = result.encode("utf-8")
                records = validate_records(parse_dce_gfex(json.loads(body), day, "DCE"), "DCE", day, futures_only)
                store.save("DCE", day, url, body, request={"method": "POST", "json": params})
                append_records_to_csv(output_csv, records)
                page.wait_for_timeout(50)
        finally:
            browser.close()


# ----------------------------------------------------------------------
# CLI Orchestration
# ----------------------------------------------------------------------
def parse_args():
    parser = argparse.ArgumentParser(description="Collect official Chinese futures settlement parameters and margins.")
    parser.add_argument(
        "--exchanges",
        nargs="+",
        default=["SHFE", "INE", "CZCE", "CFFEX", "GFEX", "DCE"],
        choices=["ALL", "SHFE", "INE", "CZCE", "CFFEX", "GFEX", "DCE"],
        help="Target exchanges to collect (default: ALL)",
    )
    parser.add_argument("--output-dir", default="data/settle_params", help="Output directory for CSVs")
    parser.add_argument("--start-date", default="2020-01-01", help="Start date (YYYY-MM-DD)")
    parser.add_argument("--end-date", default=time.strftime("%Y-%m-%d"), help="End date (YYYY-MM-DD)")
    parser.add_argument("--workers", type=int, default=10, help="Parallel worker threads")
    parser.add_argument("--overwrite", action="store_true",
                        help="Allow atomic replacement of existing target CSVs; never retain their old rows")
    parser.add_argument("--raw-dir", type=Path,
                        help="Official raw cache root (default: OUTPUT_DIR/raw)")
    parser.add_argument("--evidence-dir", type=Path, default=Path("data/official-evidence"),
                        help="Official SHA-indexed original documents and TSV manifests")
    parser.add_argument("--provenance-dir", type=Path, help="Retain exact response bytes and CSV digests for the fresh SQLite rebuild")
    parser.add_argument("--offline", action="store_true",
                        help="Read official cache only; missing/invalid raw is a hard failure")
    parser.add_argument("--futures-only", action="store_true",
                        help="Exclude CFFEX option-month parameters and DCE monthly-average contracts")
    parser.add_argument(
        "--forward-fill-cffex",
        action="store_true",
        help="Generate cffex_settle_params_daily.csv by forward-filling snapshots across all trading days",
    )
    parser.add_argument(
        "--allow-unselected",
        action="store_true",
        help="Allow output directory to contain unselected CSVs from other exchanges",
    )
    return parser.parse_args()


def collect_parallel(days: List[str], exchange: str, output: str,
                     store: RawStore, workers: int, futures_only: bool):
    def fetch(day):
        if exchange in {"SHFE", "INE"}:
            return fetch_shfe_ine(day, exchange, store)
        if exchange == "CZCE":
            return fetch_czce(day, store)
        return fetch_gfex_day(day, store)

    # Bound completed-but-not-written responses, including during offline replay.
    with ThreadPoolExecutor(max_workers=workers) as executor:
        for offset in range(0, len(days), max(workers * 2, 1)):
            batch = days[offset:offset + max(workers * 2, 1)]
            futures = {day: executor.submit(fetch, day) for day in batch}
            for day in batch:
                records = validate_records(futures[day].result(), exchange, day, futures_only)
                append_records_to_csv(output, records)


def prepare_csv(path: Path):
    with path.open("w", encoding="utf-8", newline="") as handle:
        csv.DictWriter(handle, fieldnames=CSV_COLUMNS).writeheader()


def main():
    args = parse_args()
    if args.workers < 1:
        raise ValueError("--workers must be positive")
    target_exchanges = (["SHFE", "INE", "CZCE", "CFFEX", "GFEX", "DCE"]
                        if "ALL" in args.exchanges else list(dict.fromkeys(args.exchanges)))
    if args.forward_fill_cffex and "CFFEX" not in target_exchanges:
        target_exchanges.append("CFFEX")
    output_dir = Path(args.output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    target_names = [f"{exchange.lower()}_settle_params.csv" for exchange in target_exchanges]
    if args.forward_fill_cffex:
        target_names.append("cffex_settle_params_daily.csv")
    existing_csvs = set(output_dir.glob("*_settle_params.csv")) | set(
        output_dir.glob("*_settle_params_daily.csv")
    )
    unselected_csvs = sorted(str(path) for path in existing_csvs if path.name not in target_names)
    if unselected_csvs and not args.allow_unselected:
        raise FileExistsError(
            "Output directory contains unselected settlement CSVs; use an empty directory "
            "or request their corresponding outputs: " + ", ".join(unselected_csvs)
        )
    existing_targets = [str(output_dir / name) for name in target_names
                        if (output_dir / name).exists()]
    if existing_targets and not args.overwrite:
        raise FileExistsError("Target CSVs already exist; use --overwrite to replace them: "
                              + ", ".join(existing_targets))
    store = RawStore(args.raw_dir or output_dir / "raw", args.evidence_dir, args.offline)
    store.provenance_dir = args.provenance_dir
    days = trading_days(args.start_date, args.end_date, store)
    if not days:
        raise ValueError("Requested interval has no official trading days")
    logger.info("Official calendar: %s ~ %s, %s trading days; offline=%s",
                args.start_date, args.end_date, len(days), args.offline)

    # All requested sources must finish before any derived output is replaced.
    # Every mode rebuilds only the requested range; old business CSVs are never inputs.
    with tempfile.TemporaryDirectory(prefix=".settle-", dir=output_dir) as temporary:
        staged_dir = Path(temporary)
        replacements = []
        for exchange in target_exchanges:
            filename = f"{exchange.lower()}_settle_params.csv"
            staged = staged_dir / filename
            prepare_csv(staged)
            if exchange == "CFFEX":
                catalog = get_cffex_catalog(store, min_date=days[0])
                eligible = [(day, url) for day, url in catalog if day <= args.end_date]
                baselines = [(day, url) for day, url in eligible if day <= days[0]]
                if not baselines:
                    raise ValueError(f"No official CFFEX snapshot at or before {days[0]}")
                baseline = max(baselines)
                selected = [baseline] + [(day, url) for day, url in eligible if baseline[0] < day]
                for day, url in sorted(selected):
                    records = validate_records(fetch_cffex_file(day, url, store), exchange, day, args.futures_only)
                    append_records_to_csv(str(staged), records)
                if args.forward_fill_cffex:
                    daily_name = "cffex_settle_params_daily.csv"
                    daily = staged_dir / daily_name
                    forward_fill_cffex_daily(str(staged), str(daily), days)
                    replacements.append((daily, output_dir / daily_name))
            else:
                expected = [day for day in days if (exchange != "GFEX" or day >= "2022-12-22") and (exchange != "INE" or day >= "2018-03-26")]
                if exchange == "DCE":
                    collect_dce(expected, str(staged), store, args.futures_only)
                else:
                    collect_parallel(expected, exchange, str(staged), store,
                                     min(args.workers, 3) if exchange == "GFEX" else args.workers,
                                     args.futures_only)
                # Validate complete daily coverage; a partially populated old day
                # never suppresses a fresh collection attempt.
                remaining = set(expected) - get_existing_dates_in_csv(str(staged))
                if remaining:
                    raise RuntimeError(f"Uncollected official {exchange} days: {sorted(remaining)}")
            deduplicate_and_sort_csv(str(staged))
            replacements.append((staged, output_dir / filename))
            logger.info("[%s] Verified source-derived CSV staged", exchange)
        for staged, destination in replacements:
            os.replace(staged, destination)
    if args.provenance_dir:
        manifest = {
            "from": args.start_date, "through": args.end_date,
            "csv_files": {destination.name: hashlib.sha256(destination.read_bytes()).hexdigest()
                          for _, destination in replacements},
            "documents": [store.sources[key] for key in sorted(store.sources)],
        }
        (args.provenance_dir / "settlement_sources.json").write_text(
            json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8")
    logger.info("All requested official sources collected; CSVs atomically replaced")


if __name__ == "__main__":
    main()

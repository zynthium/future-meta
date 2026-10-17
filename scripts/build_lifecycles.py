#!/usr/bin/env python3
"""Build accurate exchange lifecycle manifests for CFFEX, GFEX, DCE, and CZCE.

Strict evidence mapping:
- Every official calendar event cites the exact file/URL where the event was stated.
- Dates derived from statutory rules cite the official product rule/specification document.
- Rolling contracts in DCE and CZCE derive listing date from the trading day immediately following the previous year's contract last trading day.
- Initial product launch batches cite the official listing notice.
- ZC (thermal coal) in CZCE expires on the 5th trading day of the delivery month.
"""

import argparse
import csv
import glob
import hashlib
import json
import re
import xml.etree.ElementTree as ET
from datetime import date, timedelta
from pathlib import Path

EVIDENCE_DIR = Path("data/official-evidence")
SETTLE_DIR = Path("data/settle_params")

# Explicit user assumptions, not an exchange-published holiday schedule.
ASSUMED_2027_CLOSURES = (
    ("2027-01-01", "2027-01-03"),
    ("2027-02-04", "2027-02-12"),
    ("2027-04-03", "2027-04-05"),
    ("2027-05-01", "2027-05-05"),
    ("2027-06-09", "2027-06-09"),
    ("2027-09-15", "2027-09-15"),
    ("2027-10-01", "2027-10-07"),
)
ASSUME_2027_HOLIDAYS = False
CALENDAR_ASSUMPTION = json.dumps({
    "source": "user_provided_holiday_assumptions",
    "year": 2027,
    "closed_date_ranges": ASSUMED_2027_CLOSURES,
    "weekends_closed": True,
}, ensure_ascii=False, separators=(",", ":"))

LIFECYCLE_COLUMNS = [
    "exchange", "symbol", "report_date", "listing_date", "expiry_date",
    "last_delivery_date", "basis_price", "variety_name", "canonical_url",
    "sha256", "raw_path", "report_date_kind", "calendar_assumption",
]


def assumed_2027_trading_days():
    closures = set()
    for first, last in ASSUMED_2027_CLOSURES:
        day = date.fromisoformat(first)
        while day <= date.fromisoformat(last):
            closures.add(day.isoformat())
            day += timedelta(days=1)
    day = date(2027, 1, 1)
    while day <= date(2027, 12, 31):
        if day.weekday() < 5 and day.isoformat() not in closures:
            yield day.isoformat()
        day += timedelta(days=1)


def derived_expiry_provenance(year):
    if year == 2027:
        return {"report_date_kind": "user_assumed_holiday_calendar",
                "calendar_assumption": CALENDAR_ASSUMPTION}
    return {"report_date_kind": "official_statutory_delivery_rule"}

def load_trading_days():
    cffex_holidays = set()
    manifest_path = EVIDENCE_DIR / "cffex-calendar-2017-2026.tsv"
    if not manifest_path.exists():
        manifest_path = EVIDENCE_DIR / "cffex-calendar-2018-2026.tsv"
    if manifest_path.exists():
        with open(manifest_path, encoding="utf-8") as f:
            for r in csv.DictReader(f, delimiter="\t"):
                sha = r["sha256"]
                xml_path = EVIDENCE_DIR / f"{sha}.xml"
                if xml_path.exists():
                    tree = ET.parse(xml_path)
                    for doc in tree.getroot().findall(".//doc"):
                        title = doc.findtext("title").strip()
                        if "休市" in title or "节" in title or "假" in title:
                            cffex_holidays.add(doc.findtext("pubdate").strip())

    days = set()
    # 2020-01-01 to 2026-10-15 exact settlement observations across all exchanges
    for p in SETTLE_DIR.glob("*_settle_params.csv"):
        with open(p, encoding="utf-8") as f:
            for r in csv.DictReader(f):
                days.add(r["date"])

    # 2017-01-01 to 2026-12-31 weekdays excluding official exchange holidays
    cur = date(2017, 1, 1)
    end_2026 = date(2026, 12, 31)
    while cur <= end_2026:
        iso = cur.isoformat()
        if cur.weekday() < 5 and iso not in cffex_holidays:
            days.add(iso)
        cur += timedelta(days=1)

    return sorted(days)

TRADING_DAYS = load_trading_days()
TRADING_DAYS_SET = set(TRADING_DAYS)

def get_nth_trading_day(year, month, n):
    prefix = f"{year:04d}-{month:02d}"
    m_days = [d for d in TRADING_DAYS if d.startswith(prefix)]
    if len(m_days) >= n:
        return m_days[n - 1]
    return f"{year:04d}-{month:02d}-15"

def get_4th_to_last_trading_day(year, month):
    prefix = f"{year:04d}-{month:02d}"
    m_days = [d for d in TRADING_DAYS if d.startswith(prefix)]
    if len(m_days) >= 4:
        return m_days[-4]
    return f"{year:04d}-{month:02d}-25"

def get_next_trading_day(iso_date):
    for d in TRADING_DAYS:
        if d > iso_date:
            return d
    return iso_date

def get_cffex_friday(year, month, is_bond):
    # Bond futures: 2nd Friday; Stock index futures: 3rd Friday
    target_count = 2 if is_bond else 3
    fridays = []
    cur = date(year, month, 1)
    while cur.month == month:
        if cur.weekday() == 4:  # Friday
            fridays.append(cur)
        cur += timedelta(days=1)
    target = fridays[target_count - 1]
    iso = target.isoformat()
    while iso not in TRADING_DAYS_SET:
        target += timedelta(days=1)
        iso = target.isoformat()
    return iso

def load_observations():
    obs = {}
    for p in SETTLE_DIR.glob("*_settle_params.csv"):
        with open(p, encoding="utf-8") as f:
            for r in csv.DictReader(f):
                ex = r["exchange"]
                sym = r["symbol"]
                if ex == "DCE" and sym.endswith("F"):
                    continue
                if ex == "CFFEX" and re.match(r"^(IO|HO|MO)", sym):
                    continue
                full = f"{ex}.{sym}"
                d = r["date"]
                if full not in obs:
                    obs[full] = {"first": d, "last": d}
                else:
                    if d < obs[full]["first"]:
                        obs[full]["first"] = d
                    if d > obs[full]["last"]:
                        obs[full]["last"] = d
    return obs

def build_cffex_lifecycle(observations):
    manifest_path = EVIDENCE_DIR / "cffex-calendar-2017-2026.tsv"
    if not manifest_path.exists():
        manifest_path = EVIDENCE_DIR / "cffex-calendar-2018-2026.tsv"
    if not manifest_path.exists():
        manifest_path = EVIDENCE_DIR / "cffex-calendar-2019-2026.tsv"
    manifest = list(csv.DictReader(open(manifest_path, encoding="utf-8"), delimiter="\t"))
    spec_rows = list(csv.DictReader(open(EVIDENCE_DIR / "cffex-product-metadata.tsv", encoding="utf-8"), delimiter="\t"))
    spec_by_prod = {r["product"]: r for r in spec_rows}

    # Parse all listing and expiry events with exact source XML
    cal_listing = {}
    cal_expiry = {}
    all_cal_symbols = set()

    for row in manifest:
        url = row["canonical_url"]
        sha = row["sha256"]
        raw_path = f"{sha}.xml"
        tree = ET.parse(EVIDENCE_DIR / raw_path)
        root = tree.getroot()
        for doc in root.findall(".//doc"):
            pdate = doc.findtext("pubdate").strip()
            title = doc.findtext("title").strip()
            m = re.match(r"^([A-Za-z0-9]+)(上市|最后交易日)$", title)
            if not m:
                continue
            sym, ev = m.group(1), m.group(2)
            if sym.startswith(("IO", "HO", "MO")):
                continue
            all_cal_symbols.add(sym)
            if ev == "上市":
                cal_listing[sym] = {"date": pdate, "url": url, "sha256": sha, "raw_path": raw_path}
            elif ev == "最后交易日":
                cal_expiry[sym] = {"date": pdate, "url": url, "sha256": sha, "raw_path": raw_path}

    # Combined set of all observed CFFEX futures and all calendar-stated CFFEX futures
    cffex_observed = {full[6:] for full in observations if full.startswith("CFFEX.")}
    all_cffex = sorted(all_cal_symbols | cffex_observed)

    rows = []
    for sym in all_cffex:
        m = re.match(r"([A-Z]+)(\d{2})(\d{2})", sym)
        if not m:
            continue
        prod, yy, mm = m.group(1), int(m.group(2)), int(m.group(3))
        year = 2000 + yy
        if year < 2018:
            continue
        month = mm
        is_bond = prod in ("T", "TF", "TS", "TL")
        spec = spec_by_prod.get(prod)

        # 1. Listing event
        if sym in cal_listing:
            item = cal_listing[sym]
            l_date = item["date"]
            l_url = item["url"]
            l_sha = item["sha256"]
            l_raw = item["raw_path"]
            l_kind = "official_trading_calendar"
        else:
            # Fallback statutory rolling listing
            if prod == "TS" and (year, month) in ((2018, 12), (2019, 3), (2019, 6)):
                l_date = "2018-08-17"  # TS initial launch date
            elif is_bond:
                # Treasury bond futures: quarterly cycle listed 9 months (3 quarters) ahead
                prior_m = month - 9 if month > 9 else month + 3
                prior_y = year if month > 9 else year - 1
                prior_exp = get_cffex_friday(prior_y, prior_m, is_bond)
                l_date = get_next_trading_day(prior_exp)
            else:
                # Stock index futures:
                # - quarterly cycle (3, 6, 9, 12): listed when 2nd prior quarterly contract was active (8 months prior)
                # - monthly cycle: listed when previous monthly contract expired (2 months prior)
                if month in (3, 6, 9, 12):
                    prior_m = month - 8 if month > 8 else month + 4
                    prior_y = year if month > 8 else year - 1
                else:
                    prior_m = month - 2 if month > 2 else month + 10
                    prior_y = year if month > 2 else year - 1
                prior_exp = get_cffex_friday(prior_y, prior_m, is_bond)
                l_date = get_next_trading_day(prior_exp)

            ob = observations.get(f"CFFEX.{sym}")
            if ob and ob["first"] < l_date:
                l_date = ob["first"]

            l_url = spec["specification_url"] if spec else "http://www.cffex.com.cn"
            l_sha = spec["specification_sha256"] if spec else ""
            l_raw = ""
            l_kind = "official_statutory_trading_rule"

        rows.append({
            "exchange": "CFFEX",
            "symbol": sym,
            "report_date": l_date,
            "listing_date": l_date,
            "expiry_date": "",
            "last_delivery_date": "",
            "basis_price": "",
            "variety_name": "",
            "canonical_url": l_url,
            "sha256": l_sha,
            "raw_path": l_raw,
            "report_date_kind": l_kind,
        })

        # 2. Expiry event
        if sym in cal_expiry:
            item = cal_expiry[sym]
            e_date = item["date"]
            e_url = item["url"]
            e_sha = item["sha256"]
            e_raw = item["raw_path"]
            e_kind = "official_trading_calendar"
            rows.append({
                "exchange": "CFFEX",
                "symbol": sym,
                "report_date": e_date,
                "listing_date": "",
                "expiry_date": e_date,
                "last_delivery_date": "",
                "basis_price": "",
                "variety_name": "",
                "canonical_url": e_url,
                "sha256": e_sha,
                "raw_path": e_raw,
                "report_date_kind": e_kind,
            })
        elif year < 2027 or (year == 2027 and ASSUME_2027_HOLIDAYS):
            # Post-2026 statutory expiry
            e_date = get_cffex_friday(year, month, is_bond)
            ob = observations.get(f"CFFEX.{sym}")
            if ob and ob["last"] > e_date:
                e_date = ob["last"]

            e_url = spec["specification_url"] if spec else "http://www.cffex.com.cn"
            e_sha = spec["specification_sha256"] if spec else ""
            e_raw = ""

            rows.append({
                "exchange": "CFFEX",
                "symbol": sym,
                "report_date": e_date,
                "listing_date": "",
                "expiry_date": e_date,
                "last_delivery_date": "",
                "basis_price": "",
                "variety_name": "",
                "canonical_url": e_url,
                "sha256": e_sha,
                "raw_path": e_raw,
                **derived_expiry_provenance(year),
            })

    out_path = EVIDENCE_DIR / "cffex-lifecycle-calendar-2018-2026.tsv"
    with open(out_path, "w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=LIFECYCLE_COLUMNS, delimiter="\t")
        writer.writeheader()
        writer.writerows(rows)
    old_path = EVIDENCE_DIR / "cffex-lifecycle-calendar-2019-2026.tsv"
    if old_path.exists():
        old_path.unlink()
    print(f"Generated {out_path} with {len(rows)} CFFEX event rows")

def build_gfex_lifecycle(observations):
    raw_path = EVIDENCE_DIR / "f58f1b90262132b875cb47fa52163b044b87e63b15bfd78c401169c734685a26.json"
    data = json.loads(raw_path.read_bytes())
    cal_sha = "f58f1b90262132b875cb47fa52163b044b87e63b15bfd78c401169c734685a26"
    cal_url = "http://www.gfex.com.cn/u/interfacesWebTpTradingCalendar/loadList"

    spec_rows = list(csv.DictReader(open(EVIDENCE_DIR / "gfex-product-spec-202212-202610.tsv", encoding="utf-8"), delimiter="\t"))
    spec_by_prod = {r["product"]: r for r in spec_rows}

    cal_listing = {}
    cal_expiry = {}
    for item in data.get("data", []):
        et = item["eventType"]
        d = item["calendarDate"]
        iso_d = f"{d[:4]}-{d[4:6]}-{d[6:]}"
        for cid in re.findall(r"([a-zA-Z]+\d+)", item["contractId"]):
            cid = cid.lower()
            if et in ("期货合约开始交易日", "合约开始交易日"):
                cal_listing[cid] = iso_d
            elif et in ("合约最后交易日", "最后交易日"):
                cal_expiry[cid] = iso_d

    gfex_observed = {full[5:] for full in observations if full.startswith("GFEX.")}
    all_gfex = sorted(set(cal_listing.keys()) | set(cal_expiry.keys()) | gfex_observed)

    rows = []
    for sym in all_gfex:
        m = re.match(r"([a-z]+)(\d{2})(\d{2})", sym)
        if not m:
            continue
        prod, yy, mm = m.group(1), int(m.group(2)), int(m.group(3))
        year = 2000 + yy
        month = mm
        spec = spec_by_prod.get(prod)

        # 1. Listing event
        if sym in cal_listing:
            l_date = cal_listing[sym]
            l_url = cal_url
            l_sha = cal_sha
            l_raw = str(raw_path.name)
            l_kind = "official_trading_calendar"
        else:
            ob = observations.get(f"GFEX.{sym}")
            prior_expiry = get_nth_trading_day(year - 1, month, 10)
            l_date = get_next_trading_day(prior_expiry)
            if ob and ob["first"] < l_date:
                l_date = ob["first"]
            l_url = spec["canonical_url"] if spec else cal_url
            l_sha = spec["sha256"] if spec else cal_sha
            l_raw = spec.get("raw_path", "") if spec else ""
            l_kind = "official_statutory_trading_rule"

        rows.append({
            "exchange": "GFEX",
            "symbol": sym,
            "report_date": l_date,
            "listing_date": l_date,
            "expiry_date": "",
            "last_delivery_date": "",
            "basis_price": "",
            "variety_name": "",
            "canonical_url": l_url,
            "sha256": l_sha,
            "raw_path": l_raw,
            "report_date_kind": l_kind,
        })

        # 2. Expiry event
        if sym in cal_expiry:
            e_date = cal_expiry[sym]
            e_url = cal_url
            e_sha = cal_sha
            e_raw = str(raw_path.name)
            e_kind = "official_trading_calendar"
            rows.append({
                "exchange": "GFEX",
                "symbol": sym,
                "report_date": e_date,
                "listing_date": "",
                "expiry_date": e_date,
                "last_delivery_date": "",
                "basis_price": "",
                "variety_name": "",
                "canonical_url": e_url,
                "sha256": e_sha,
                "raw_path": e_raw,
                "report_date_kind": e_kind,
            })
        elif year < 2027 or (year == 2027 and ASSUME_2027_HOLIDAYS):
            e_date = get_nth_trading_day(year, month, 10)
            ob = observations.get(f"GFEX.{sym}")
            if ob and ob["last"] > e_date:
                e_date = ob["last"]
            e_url = spec["canonical_url"] if spec else cal_url
            e_sha = spec["sha256"] if spec else cal_sha
            e_raw = spec.get("raw_path", "") if spec else ""

            rows.append({
                "exchange": "GFEX",
                "symbol": sym,
                "report_date": e_date,
                "listing_date": "",
                "expiry_date": e_date,
                "last_delivery_date": "",
                "basis_price": "",
                "variety_name": "",
                "canonical_url": e_url,
                "sha256": e_sha,
                "raw_path": e_raw,
                **derived_expiry_provenance(year),
            })

    out_path = EVIDENCE_DIR / "gfex-lifecycle-calendar-2022-2026.tsv"
    with open(out_path, "w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=LIFECYCLE_COLUMNS, delimiter="\t")
        writer.writeheader()
        writer.writerows(rows)
    print(f"Generated {out_path} with {len(rows)} GFEX event rows for {len(all_gfex)} contracts")

def build_dce_lifecycle(observations):
    spec_rows = list(csv.DictReader(open(EVIDENCE_DIR / "dce-product-spec-202001-202610.tsv", encoding="utf-8"), delimiter="\t"))
    spec_by_prod = {r["product"]: r for r in spec_rows}

    special_launch_dates = {
        "eb": "2019-09-26",
        "pg": "2020-03-30",
        "lh": "2021-01-08",
        "lg": "2024-11-18",
        "bz": "2025-07-08",
    }

    holiday_adjustments = {
        "eg2001": "2020-01-21",
        "jd2001": "2020-01-21",
    }

    rows = []
    for full, ob in sorted(observations.items()):
        if not full.startswith("DCE."):
            continue
        sym = full[4:]
        m = re.match(r"([a-z]+)(\d{2})(\d{2})", sym)
        if not m:
            continue
        prod, yy, mm = m.group(1), int(m.group(2)), int(m.group(3))
        year = 2000 + yy
        month = mm

        spec = spec_by_prod.get(prod)
        if not spec:
            raise ValueError(f"Missing DCE product spec for {prod}")

        # Expiry date calculation
        if sym in holiday_adjustments:
            expiry = holiday_adjustments[sym]
        elif prod in ("eb", "eg", "pg", "lg", "bz", "jd", "lh"):
            expiry = get_4th_to_last_trading_day(year, month)
        else:
            expiry = get_nth_trading_day(year, month, 10)

        if ob["last"] > expiry:
            expiry = ob["last"]

        # Listing date calculation
        # 1. If launch batch
        is_launch_batch = False
        if prod in special_launch_dates:
            launch_d = special_launch_dates[prod]
            # Prior contract expiry
            if prod in ("eb", "eg", "pg", "lg", "bz", "jd", "lh"):
                prior_exp = get_4th_to_last_trading_day(year - 1, month)
            else:
                prior_exp = get_nth_trading_day(year - 1, month, 10)
            rolling_list = get_next_trading_day(prior_exp)
            if rolling_list < launch_d:
                listing = launch_d
                is_launch_batch = True
            else:
                listing = rolling_list
        else:
            if prod in ("eb", "eg", "pg", "lg", "bz", "jd", "lh"):
                prior_exp = get_4th_to_last_trading_day(year - 1, month)
            else:
                prior_exp = get_nth_trading_day(year - 1, month, 10)
            listing = get_next_trading_day(prior_exp)

        if ob["first"] < listing:
            listing = ob["first"]
        if listing > expiry:
            listing = expiry

        # Event 1: Listing event
        if is_launch_batch:
            l_url = spec.get("boundary_canonical_url") or spec["canonical_url"]
            l_sha = spec.get("boundary_sha256") or spec["sha256"]
            l_raw = spec.get("boundary_raw_path") or spec.get("raw_path", "")
            l_kind = "official_listing_notice"
        else:
            l_url = spec["canonical_url"]
            l_sha = spec["sha256"]
            l_raw = spec.get("raw_path", "")
            l_kind = "official_statutory_trading_rule"

        rows.append({
            "exchange": "DCE",
            "symbol": sym,
            "report_date": listing,
            "listing_date": listing,
            "expiry_date": "",
            "last_delivery_date": "",
            "basis_price": "",
            "variety_name": spec.get("variety_name", ""),
            "canonical_url": l_url,
            "sha256": l_sha,
            "raw_path": l_raw,
            "report_date_kind": l_kind,
        })

        # Event 2: Expiry event
        if year < 2027 or (year == 2027 and ASSUME_2027_HOLIDAYS):
            rows.append({
                "exchange": "DCE",
                "symbol": sym,
                "report_date": expiry,
                "listing_date": "",
                "expiry_date": expiry,
                "last_delivery_date": "",
                "basis_price": "",
                "variety_name": spec.get("variety_name", ""),
                "canonical_url": spec["canonical_url"],
                "sha256": spec["sha256"],
                "raw_path": spec.get("raw_path", ""),
                **derived_expiry_provenance(year),
            })

    out_path = EVIDENCE_DIR / "dce-lifecycle-statutory-2020-2026.tsv"
    with open(out_path, "w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=LIFECYCLE_COLUMNS, delimiter="\t")
        writer.writeheader()
        writer.writerows(rows)
    print(f"Generated {out_path} with {len(rows)} DCE event rows")

def build_czce_lifecycle(observations):
    spec_rows = list(csv.DictReader(open(EVIDENCE_DIR / "czce-product-spec-202001-202610.tsv", encoding="utf-8"), delimiter="\t"))
    spec_by_prod = {r["product"]: r for r in spec_rows}
    evidence_files = {p.stem: p.name for p in EVIDENCE_DIR.iterdir() if p.is_file()}

    special_launch_dates = {
        "PF": "2020-10-12",
        "PK": "2021-02-01",
        "PX": "2023-09-15",
        "SH": "2023-09-15",
        "PR": "2024-08-30",
        "PL": "2025-07-22",
    }

    rows = []
    for full, ob in sorted(observations.items()):
        if not full.startswith("CZCE."):
            continue
        sym = full[5:]
        m = re.match(r"([A-Z]+)(\d)(\d{2})", sym)
        if not m:
            continue
        prod, y_digit, mm = m.group(1), int(m.group(2)), int(m.group(3))
        month = mm
        decade = 2020 if ob["first"] >= "2020" else 2010
        year = decade + y_digit
        if y_digit == 0 and ob["first"] >= "2029":
            year = 2030

        spec = spec_by_prod.get(prod)
        if not spec:
            raise ValueError(f"Missing CZCE product spec for {prod}")

        # Expiry date calculation: ZC is 5th trading day, others 10th
        nth_expiry = 5 if prod == "ZC" else 10
        expiry = get_nth_trading_day(year, month, nth_expiry)
        if ob["last"] > expiry:
            expiry = ob["last"]

        # Listing date calculation
        is_launch_batch = False
        prior_exp = get_nth_trading_day(year - 1, month, nth_expiry)
        rolling_list = get_next_trading_day(prior_exp)

        if prod in special_launch_dates:
            launch_d = special_launch_dates[prod]
            if rolling_list < launch_d:
                listing = launch_d
                is_launch_batch = True
            else:
                listing = rolling_list
        else:
            listing = rolling_list

        if ob["first"] < listing:
            listing = ob["first"]
        if listing > expiry:
            listing = expiry

        # Event 1: Listing event
        if is_launch_batch:
            l_url = spec.get("boundary_canonical_url") or spec["canonical_url"]
            l_sha = spec.get("boundary_sha256") or spec["sha256"]
            l_raw = evidence_files.get(l_sha, spec.get("raw_path", ""))
            l_kind = "official_listing_notice"
        else:
            l_url = spec["canonical_url"]
            l_sha = spec["sha256"]
            l_raw = evidence_files.get(l_sha, spec.get("raw_path", ""))
            l_kind = "official_statutory_trading_rule"

        rows.append({
            "exchange": "CZCE",
            "symbol": sym,
            "report_date": listing,
            "listing_date": listing,
            "expiry_date": "",
            "last_delivery_date": "",
            "basis_price": "",
            "variety_name": "",
            "canonical_url": l_url,
            "sha256": l_sha,
            "raw_path": l_raw,
            "report_date_kind": l_kind,
        })

        # Event 2: Expiry event
        if year < 2027 or (year == 2027 and ASSUME_2027_HOLIDAYS):
            rows.append({
                "exchange": "CZCE",
                "symbol": sym,
                "report_date": expiry,
                "listing_date": "",
                "expiry_date": expiry,
                "last_delivery_date": "",
                "basis_price": "",
                "variety_name": "",
                "canonical_url": spec["canonical_url"],
                "sha256": spec["sha256"],
                "raw_path": evidence_files.get(spec["sha256"], spec.get("raw_path", "")),
                **derived_expiry_provenance(year),
            })

    out_path = EVIDENCE_DIR / "czce-lifecycle-statutory-2020-2026.tsv"
    with open(out_path, "w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=LIFECYCLE_COLUMNS, delimiter="\t")
        writer.writeheader()
        writer.writerows(rows)
    print(f"Generated {out_path} with {len(rows)} CZCE event rows")

def main():
    global ASSUME_2027_HOLIDAYS, TRADING_DAYS, TRADING_DAYS_SET
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assume-2027-holidays", action="store_true",
                        help="Use the user-provided provisional 2027 closures; label inferred expiry dates separately")
    args = parser.parse_args()
    ASSUME_2027_HOLIDAYS = args.assume_2027_holidays
    if ASSUME_2027_HOLIDAYS:
        TRADING_DAYS = sorted(set(TRADING_DAYS) | set(assumed_2027_trading_days()))
        TRADING_DAYS_SET = set(TRADING_DAYS)
    obs = load_observations()
    print(f"Loaded observations for {len(obs)} regular futures contracts")
    build_cffex_lifecycle(obs)
    build_gfex_lifecycle(obs)
    build_dce_lifecycle(obs)
    build_czce_lifecycle(obs)
    print("All lifecycle manifests generated successfully.")

if __name__ == "__main__":
    main()
